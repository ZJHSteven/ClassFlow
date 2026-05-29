#!/usr/bin/env python3
# coding: utf-8
"""
本脚本用于给 ClassFlow 提供“单次调用、本地文件转写”的最小 Python 接口。

设计目标很明确：

1. Rust 后端只需要把音频文件路径、模型目录、输出路径传进来。
2. 脚本内部负责加载 CapsWriter 的 Qwen3-ASR 引擎，并把结果落成稳定 JSON。
3. 避免把 CapsWriter 上游的一些实现细节和坑直接泄漏到 Rust 侧。

这里特别规避了一个已经在 Linux 冒烟中验证过的上游坑：

`QwenASREngine.transcribe()` 默认参数里会把 `duration=0.0` 继续传给 `load_audio()`，
而当前 `load_audio_*()` 实现会把 `duration=0.0` 当成“读取 0 帧”，最终出现：

1. 模型成功加载；
2. 但音频实际为空；
3. 最后结果文本为空字符串。

因此本脚本不直接依赖那个默认调用，而是显式：

1. 先 `load_audio()`；
2. 再手动调用 `engine.asr()`。

这样可以稳定避开这个坑。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import traceback
from pathlib import Path
from typing import Any, Optional


def build_arg_parser() -> argparse.ArgumentParser:
    """
    构造命令行参数解析器。

    输入尽量保持朴素，原因是后续主要由 Rust 侧调用：

    1. 参数名要直观，方便日志排查。
    2. 默认值要尽量少，避免“看似成功，实则用错路径”。
    3. 错误要尽量早暴露，在 argparse 层就挡掉明显错误。
    """

    parser = argparse.ArgumentParser(
        description="使用 CapsWriter 的 Qwen3-ASR 引擎，对单个音频文件执行本地转写。"
    )
    parser.add_argument("--capswriter-root", required=True, help="CapsWriter-Offline 根目录")
    parser.add_argument("--audio", required=True, help="待转写音频路径")
    parser.add_argument("--model-dir", required=True, help="Qwen3-ASR 模型目录")
    parser.add_argument("--aligner-dir", required=True, help="Qwen3-ForcedAligner 模型目录")
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
    parser.add_argument(
        "--chunk-size",
        type=float,
        default=40.0,
        help="分段时长（秒），默认 40.0",
    )
    parser.add_argument(
        "--memory-num",
        type=int,
        default=1,
        help="跨段记忆片段数，默认 1",
    )
    parser.add_argument(
        "--onnx-provider",
        default="CPU",
        help="ONNX 推理后端，默认 CPU",
    )
    return parser


def ensure_paths(args: argparse.Namespace) -> None:
    """
    对关键路径做显式校验。

    这里不等底层模型初始化时再报错，原因是：

    1. 路径类错误越早报越容易看懂。
    2. 模型加载本身就比较慢，不应该让明显错误白白耗时。
    """

    required_paths = {
        "capswriter_root": Path(args.capswriter_root),
        "audio": Path(args.audio),
        "model_dir": Path(args.model_dir),
        "aligner_dir": Path(args.aligner_dir),
    }

    for label, path in required_paths.items():
        if not path.exists():
            raise FileNotFoundError(f"{label} 不存在: {path}")

    output_parent = Path(args.output).parent
    output_parent.mkdir(parents=True, exist_ok=True)


def install_capswriter_import_path(capswriter_root: Path) -> None:
    """
    把 CapsWriter 根目录加入 `sys.path`，并补上 llama 共享库搜索路径。

    Linux 上需要显式处理共享库路径，原因是上游代码内部使用 `ctypes.CDLL`
    从 `qwen_asr_gguf/inference/bin/` 目录加载 `libllama.so` 等文件。
    """

    sys.path.insert(0, capswriter_root.as_posix())

    inference_bin = (
        capswriter_root
        / "core"
        / "server"
        / "engines"
        / "qwen_asr_gguf"
        / "inference"
        / "bin"
    )
    existing = os.environ.get("LD_LIBRARY_PATH", "")
    os.environ["LD_LIBRARY_PATH"] = (
        f"{inference_bin}{os.pathsep}{existing}" if existing else str(inference_bin)
    )


def convert_alignment(alignment: Any) -> list[dict[str, Any]]:
    """
    把 CapsWriter 的对齐结构转换成稳定 JSON 数组。

    输出只保留 ClassFlow 真正需要的字段：

    1. `text`
    2. `start_time`
    3. `end_time`
    """

    if alignment is None:
        return []

    return [
        {
            "text": item.text,
            "start_time": item.start_time,
            "end_time": item.end_time,
        }
        for item in alignment.items
    ]


def normalize_result(result: Any, load_seconds: float, transcribe_seconds: float) -> dict[str, Any]:
    """
    将上游结果对象规整成 Rust 容易消费的 JSON。

    这里特意同时保留：

    1. `text` 作为主文本；
    2. `timestamps` 作为字/片段时间戳数组；
    3. `raw` 作为保底原始结构快照。

    这样后续 Rust 侧既可以先做最小接入，也方便以后扩展更细的对齐信息。
    """

    timestamps = convert_alignment(result.alignment)
    return {
        "ok": True,
        "provider": "local_qwen",
        "text": result.text,
        "tokens": [item["text"] for item in timestamps],
        "timestamps": timestamps,
        "duration_seconds": sum(
            max(0.0, item["end_time"] - item["start_time"]) for item in timestamps
        ),
        "performance": {
            "load_seconds": load_seconds,
            "transcribe_seconds": transcribe_seconds,
            **(result.performance or {}),
        },
        "raw": {
            "text": result.text,
            "alignment": timestamps,
            "performance": result.performance,
        },
    }


def write_json(path: Path, payload: dict[str, Any]) -> None:
    """
    统一写 JSON 文件。

    无论成功还是失败，都保证 Rust 侧能拿到结构化结果，而不是只能读 stderr。
    """

    path.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )


def run(args: argparse.Namespace) -> dict[str, Any]:
    """
    执行一次真实本地转写。

    关键调用顺序：

    1. 导入 CapsWriter 模块；
    2. 初始化配置和模型；
    3. 显式 `load_audio()`；
    4. 再调用 `engine.asr()`；
    5. 最终组装输出 JSON。
    """

    capswriter_root = Path(args.capswriter_root).resolve()
    install_capswriter_import_path(capswriter_root)

    from core.server.engines.qwen_asr_gguf.inference.asr import QwenASREngine
    from core.server.engines.qwen_asr_gguf.inference.audio import load_audio
    from core.server.engines.qwen_asr_gguf.inference.schema import (
        ASREngineConfig,
        AlignerConfig,
    )

    load_started_at = time.time()
    config = ASREngineConfig(
        model_dir=args.model_dir,
        encoder_frontend_fn="qwen3_asr_encoder_frontend.onnx",
        encoder_backend_fn="qwen3_asr_encoder_backend.onnx",
        llm_fn="qwen3_asr_llm.gguf",
        onnx_provider=args.onnx_provider,
        llm_use_gpu=False,
        chunk_size=args.chunk_size,
        memory_num=args.memory_num,
        verbose=False,
        enable_aligner=True,
        align_config=AlignerConfig(
            model_dir=args.aligner_dir,
            encoder_frontend_fn="qwen3_aligner_encoder_frontend.int4.onnx",
            encoder_backend_fn="qwen3_aligner_encoder_backend.int4.onnx",
            llm_fn="qwen3_aligner_llm.q5_k.gguf",
            onnx_provider=args.onnx_provider,
            llm_use_gpu=False,
            dml_pad_to=max(1, int(args.chunk_size)),
        ),
    )
    engine = QwenASREngine(config)
    load_seconds = time.time() - load_started_at

    transcribe_started_at = time.time()
    audio = load_audio(args.audio, sample_rate=16000, start_second=None, duration=None)
    result = engine.asr(
        audio=audio,
        context=args.context,
        language=args.language,
        chunk_size_sec=args.chunk_size,
        memory_chunks=args.memory_num,
        temperature=0.4,
        rollback_num=5,
    )
    transcribe_seconds = time.time() - transcribe_started_at

    return normalize_result(result, load_seconds, transcribe_seconds)


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
