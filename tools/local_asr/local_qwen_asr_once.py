#!/usr/bin/env python3
# coding: utf-8
"""
本脚本用于给 ClassFlow 提供“单次调用、本地文件转写”的最小 Python 接口。

当前它已经不是唯一入口了：

1. 早期版本只有这个脚本，Rust 每次任务直接起一个 Python 进程。
2. 后续为了支持“常驻 sidecar + 模型复用”，公共推理逻辑已经抽到
   `local_qwen_asr_common.py`。
3. 这个脚本现在主要承担两件事：
   - 保留单次调用模式，方便独立排障与回归；
   - 作为 sidecar 的“公共逻辑同源基准”，确保两种模式结果一致。
"""

from __future__ import annotations

import argparse
import traceback
from pathlib import Path
from local_qwen_asr_common import (
    build_common_arg_parser,
    build_engine_from_args,
    ensure_audio_path,
    ensure_common_paths,
    ensure_output_parent,
    transcribe_audio_with_loaded_engine,
    write_json,
)


def build_arg_parser() -> argparse.ArgumentParser:
    """
    构造单次脚本专用参数解析器。

    这里在公共参数的基础上，只额外补“本次转写请求自己的音频和输出路径”。
    """

    parser = build_common_arg_parser(
        "使用 CapsWriter 的 Qwen3-ASR 引擎，对单个音频文件执行本地转写。"
    )
    parser.add_argument("--audio", required=True, help="待转写音频路径")
    parser.add_argument("--output", required=True, help="输出 JSON 文件路径")
    parser.add_argument(
        "--language",
        default="Chinese",
        help="CapsWriter 识别语言名称，默认 Chinese",
    )
    parser.add_argument(
        "--context",
        default="",
        help="提供给模型的上下文提示，默认空字符串",
    )
    return parser


def ensure_paths(args: argparse.Namespace) -> None:
    """
    对单次脚本运行所需的所有路径做显式校验。
    """

    ensure_common_paths(
        capswriter_root=args.capswriter_root,
        model_dir=args.model_dir,
        aligner_dir=args.aligner_dir,
    )
    ensure_audio_path(args.audio)
    ensure_output_parent(args.output)


def run(args: argparse.Namespace) -> dict[str, object]:
    """
    执行一次真实本地转写。
    """

    engine, load_audio_fn, load_seconds = build_engine_from_args(args)
    return transcribe_audio_with_loaded_engine(
        engine=engine,
        load_audio_fn=load_audio_fn,
        audio_path=args.audio,
        language=args.language,
        context=args.context,
        chunk_size=args.chunk_size,
        memory_num=args.memory_num,
        load_seconds=load_seconds,
    )


def main() -> int:
    """
    脚本主入口。

    退出码策略：

    1. 成功返回 `0`；
    2. 失败返回 `1`；
    3. 同时总是尽量写出 `output.json`，方便 Rust 侧读取。
    """

    parser = build_arg_parser()
    args = parser.parse_args()
    output_path = Path(args.output).resolve()

    try:
        ensure_paths(args)
        payload = run(args)
        if not payload.get("text", "").strip():
            raise RuntimeError("本地 ASR 返回空文本")
        write_json(output_path, payload)
        return 0
    except Exception as error:  # noqa: BLE001 - 这里需要兜底收敛所有异常
        failure_payload = {
            "ok": False,
            "provider": "local_qwen",
            "error": {
                "kind": error.__class__.__name__,
                "message": str(error),
                "traceback": traceback.format_exc(),
            },
        }
        write_json(output_path, failure_payload)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
