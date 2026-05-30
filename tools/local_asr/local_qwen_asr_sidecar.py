#!/usr/bin/env python3
# coding: utf-8
"""
本脚本为 ClassFlow 提供“常驻本地 ASR sidecar”。

设计边界很明确：

1. 启动时一次性加载模型到内存。
2. 后续通过本地 HTTP 接口接收转写请求，复用同一个已加载模型。
3. sidecar 本身不做复杂调度；“一次只跑一个任务”的串行保证仍由 Rust 侧控制。
4. sidecar 只负责三类接口：
   - `GET /health`
   - `POST /transcribe`
   - `POST /shutdown`

为什么不用更复杂的框架：

1. 当前目标不是做一个通用的推理网关，而是把“每任务重新加载模型”的浪费先消掉。
2. Python 标准库 `ThreadingHTTPServer` 足够承载本地回环 HTTP。
3. 这样额外依赖最少，部署和排障都更直接。
"""

from __future__ import annotations

import argparse
import json
import os
import threading
import time
import traceback
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

from local_qwen_asr_common import (
    build_common_arg_parser,
    build_engine_from_args,
    ensure_audio_path,
    ensure_common_paths,
    transcribe_audio_with_loaded_engine,
)


class SidecarState:
    """
    保存 sidecar 进程级状态。

    这里的状态全部是“模型级”或“生命周期级”的内容：

    1. 已加载引擎；
    2. `load_audio` 函数引用；
    3. 模型加载耗时；
    4. 最近一次成功使用时间；
    5. 用于在请求级串行化的锁。
    """

    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.started_at = time.time()
        self.last_used_at = self.started_at
        self.engine, self.load_audio_fn, self.load_seconds = build_engine_from_args(args)
        self.transcribe_lock = threading.Lock()

    def transcribe(self, payload: dict[str, Any]) -> dict[str, Any]:
        """
        使用已加载模型执行一次转写。

        这里再加一层进程内锁，是为了防止未来 Rust 侧出现意外并发请求时，
        sidecar 至少还能自我保护，不会把上游引擎打进未定义状态。
        """

        audio_path = str(payload.get("audio_path", "")).strip()
        language = str(payload.get("language") or "Chinese").strip() or "Chinese"
        context = str(payload.get("context") or "")

        ensure_audio_path(audio_path)

        with self.transcribe_lock:
            result = transcribe_audio_with_loaded_engine(
                engine=self.engine,
                load_audio_fn=self.load_audio_fn,
                audio_path=audio_path,
                language=language,
                context=context,
                chunk_size=self.args.chunk_size,
                memory_num=self.args.memory_num,
                load_seconds=self.load_seconds,
            )
            self.last_used_at = time.time()
            return result


def build_arg_parser() -> argparse.ArgumentParser:
    """
    构造 sidecar 专用参数。

    与单次脚本相比，sidecar 不再接收 `--audio / --output`，
    而是增加监听地址参数。
    """

    parser = build_common_arg_parser("启动本地 Qwen3-ASR 常驻 sidecar。")
    parser.add_argument("--host", default="127.0.0.1", help="监听地址，默认 127.0.0.1")
    parser.add_argument("--port", type=int, required=True, help="监听端口")
    return parser


def build_failure_payload(error: Exception) -> dict[str, Any]:
    """
    统一构造失败 JSON。
    """

    return {
        "ok": False,
        "provider": "local_qwen",
        "error": {
            "kind": error.__class__.__name__,
            "message": str(error),
            "traceback": traceback.format_exc(),
        },
    }


def write_json_response(handler: BaseHTTPRequestHandler, status: int, payload: dict[str, Any]) -> None:
    """
    把 Python 字典写成标准 JSON HTTP 响应。
    """

    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json; charset=utf-8")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


def log(message: str) -> None:
    """
    输出统一 sidecar 日志。

    这里故意直接打到 stdout，并立即 flush：

    1. 手工启动时可以直接在终端看。
    2. 后端以 `Stdio::inherit()` 拉起时，会自动进入 `journalctl`。
    """

    print(f"[local-qwen-sidecar pid={os.getpid()}] {message}", flush=True)


def build_handler(state: SidecarState):
    """
    生成绑定进程状态的 HTTP handler 类。
    """

    class SidecarHandler(BaseHTTPRequestHandler):
        """
        sidecar HTTP 处理器。
        """

        server_version = "ClassFlowLocalQwenSidecar/1.0"

        def log_message(self, format: str, *args: Any) -> None:  # noqa: A003
            """
            压掉标准库默认 access log，避免把 stdout 弄得过于嘈杂。
            """

            return

        def do_GET(self) -> None:  # noqa: N802
            if self.path != "/health":
                write_json_response(
                    self,
                    HTTPStatus.NOT_FOUND,
                    {"ok": False, "error": "not_found"},
                )
                return

            uptime_seconds = time.time() - state.started_at
            idle_seconds = time.time() - state.last_used_at
            log(
                f"health ready uptime={uptime_seconds:.2f}s idle={idle_seconds:.2f}s "
                f"load={state.load_seconds:.2f}s"
            )
            write_json_response(
                self,
                HTTPStatus.OK,
                {
                    "ok": True,
                    "provider": "local_qwen",
                    "status": "ready",
                    "uptime_seconds": uptime_seconds,
                    "idle_seconds": idle_seconds,
                    "load_seconds": state.load_seconds,
                },
            )

        def do_POST(self) -> None:  # noqa: N802
            if self.path == "/transcribe":
                self.handle_transcribe()
                return
            if self.path == "/shutdown":
                self.handle_shutdown()
                return

            write_json_response(
                self,
                HTTPStatus.NOT_FOUND,
                {"ok": False, "error": "not_found"},
            )

        def handle_transcribe(self) -> None:
            """
            处理一次转写请求。
            """

            try:
                content_length = int(self.headers.get("Content-Length", "0"))
                raw_body = self.rfile.read(content_length)
                payload = json.loads(raw_body.decode("utf-8"))
                audio_path = str(payload.get("audio_path", ""))
                log(f"transcribe start audio={audio_path}")
                result = state.transcribe(payload)
                log(
                    "transcribe done "
                    f"audio={audio_path} text_len={len(result.get('text') or '')} "
                    f"rtf_payload={json.dumps(result.get('performance') or {}, ensure_ascii=False)}"
                )
                write_json_response(self, HTTPStatus.OK, result)
            except Exception as error:  # noqa: BLE001
                log(f"transcribe failed error={error}")
                write_json_response(
                    self,
                    HTTPStatus.INTERNAL_SERVER_ERROR,
                    build_failure_payload(error),
                )

        def handle_shutdown(self) -> None:
            """
            处理主动关闭请求。

            这里先响应成功，再异步调用 `server.shutdown()`，避免当前请求自己把自己卡死。
            """

            write_json_response(
                self,
                HTTPStatus.OK,
                {"ok": True, "provider": "local_qwen", "status": "shutting_down"},
            )
            log("shutdown requested")

            def shutdown_server() -> None:
                self.server.shutdown()

            threading.Thread(target=shutdown_server, daemon=True).start()

    return SidecarHandler


def main() -> int:
    """
    sidecar 主入口。
    """

    parser = build_arg_parser()
    args = parser.parse_args()

    try:
        ensure_common_paths(
            capswriter_root=args.capswriter_root,
            model_dir=args.model_dir,
            aligner_dir=args.aligner_dir,
        )
        state = SidecarState(args)
        log(
            "sidecar ready "
            f"host={args.host} port={args.port} load_seconds={state.load_seconds:.2f}"
        )
        handler = build_handler(state)
        server = ThreadingHTTPServer((args.host, args.port), handler)
        server.serve_forever()
        return 0
    except KeyboardInterrupt:
        return 0
    except Exception as error:  # noqa: BLE001
        failure = build_failure_payload(error)
        print(json.dumps(failure, ensure_ascii=False), flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
