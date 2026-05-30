#!/usr/bin/env python3
# coding: utf-8
"""
本模块集中放本地 Qwen3-ASR 侧的公共能力。

为什么要单独抽这一层：

1. 现在既有“单次脚本模式”，又要新增“常驻 sidecar 模式”。
2. 两者的真正核心逻辑其实完全一致：
   - 校验路径
   - 导入 CapsWriter
   - 初始化 Qwen3-ASR 引擎
   - 显式 `load_audio()`
   - 调用 `engine.asr()`
   - 输出稳定 JSON
3. 如果两份脚本各自复制一套实现，后续修 bug 时极容易出现“一边修了，另一边忘了”的问题。

因此这里保持“公共逻辑在一个文件，入口脚本只负责各自的运行方式”。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path
from typing import Any


def build_common_arg_parser(description: str) -> argparse.ArgumentParser:
    """
    构造本地 ASR 脚本共用的命令行参数解析器。

    这里把“模型、音频、语言、分段参数”等真正和推理相关的输入集中起来，
    让“单次脚本”和“常驻 sidecar”都共用同一份参数语义。
    """

    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--capswriter-root", required=True, help="CapsWriter-Offline 根目录")
    parser.add_argument("--model-dir", required=True, help="Qwen3-ASR 模型目录")
    parser.add_argument("--aligner-dir", required=True, help="Qwen3-ForcedAligner 模型目录")
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
    parser.add_argument(
        "--threads",
        type=int,
        default=0,
        help="llama.cpp 解码线程数；0 表示沿用上游默认策略",
    )
    parser.add_argument(
        "--threads-batch",
        type=int,
        default=0,
        help="llama.cpp batch 线程数；0 表示沿用上游默认策略",
    )
    return parser


def ensure_common_paths(capswriter_root: str, model_dir: str, aligner_dir: str) -> None:
    """
    对 sidecar 启动前就必须存在的关键目录做显式校验。

    这里不检查音频路径，因为 sidecar 启动后会处理多次请求，每次请求的音频不同。
    """

    required_paths = {
        "capswriter_root": Path(capswriter_root),
        "model_dir": Path(model_dir),
        "aligner_dir": Path(aligner_dir),
    }

    for label, path in required_paths.items():
        if not path.exists():
            raise FileNotFoundError(f"{label} 不存在: {path}")


def ensure_audio_path(audio_path: str) -> None:
    """
    对单次转写请求的音频路径做显式校验。
    """

    path = Path(audio_path)
    if not path.exists():
        raise FileNotFoundError(f"audio 不存在: {path}")


def ensure_output_parent(output_path: str) -> None:
    """
    确保输出 JSON 的父目录存在。
    """

    Path(output_path).parent.mkdir(parents=True, exist_ok=True)


def install_capswriter_import_path(capswriter_root: Path) -> None:
    """
    把 CapsWriter 根目录加入 `sys.path`，并补上 llama 共享库搜索路径。

    Linux 上必须显式处理 `LD_LIBRARY_PATH`，否则上游用 `ctypes.CDLL`
    加载 `libllama.so / libggml.so` 时会直接失败。
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
    将上游结果对象规整成 Rust 与 sidecar 都容易消费的稳定 JSON。
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
    """

    path.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )


def build_engine_from_args(args: argparse.Namespace) -> tuple[Any, Any, float]:
    """
    基于命令行参数初始化 Qwen3-ASR 引擎。

    返回值：

    1. `engine`
    2. `load_audio` 函数
    3. 模型加载耗时秒数
    """

    capswriter_root = Path(args.capswriter_root).resolve()
    install_capswriter_import_path(capswriter_root)

    from core.server.engines.qwen_asr_gguf.inference.asr import QwenASREngine
    from core.server.engines.qwen_asr_gguf.inference.audio import load_audio
    from core.server.engines.qwen_asr_gguf.inference import llama
    from core.server.engines.qwen_asr_gguf.inference.schema import (
        ASREngineConfig,
        AlignerConfig,
    )

    load_started_at = time.time()
    requested_threads = args.threads if args.threads > 0 else None
    requested_threads_batch = args.threads_batch if args.threads_batch > 0 else None

    if requested_threads is not None or requested_threads_batch is not None:
        original_init = llama.LlamaContext.__init__

        def patched_init(self, model, n_ctx=2048, n_batch=2048, n_ubatch=512, n_seq_max=1,
                         embeddings=False, pooling_type=0, flash_attn=True,
                         offload_kqv=True, no_perf=True, n_threads=None, n_threads_batch=None):
            """
            运行时覆盖 LlamaContext 线程参数。

            这里不直接改上游源码，而是在 ClassFlow 的 sidecar 进程内临时注入：
            1. 便于快速验证“2 核 vs 4 核”的真实收益；
            2. 不会把服务器上的 CapsWriter 运行目录改成长期分叉版本；
            3. 当环境变量保持 0 时，完全回到当前上游默认行为。
            """

            original_init(
                self,
                model,
                n_ctx=n_ctx,
                n_batch=n_batch,
                n_ubatch=n_ubatch,
                n_seq_max=n_seq_max,
                embeddings=embeddings,
                pooling_type=pooling_type,
                flash_attn=flash_attn,
                offload_kqv=offload_kqv,
                no_perf=no_perf,
                n_threads=requested_threads if requested_threads is not None else n_threads,
                n_threads_batch=(
                    requested_threads_batch
                    if requested_threads_batch is not None
                    else n_threads_batch
                ),
            )

        llama.LlamaContext.__init__ = patched_init

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
    return engine, load_audio, load_seconds


def transcribe_audio_with_loaded_engine(
    *,
    engine: Any,
    load_audio_fn: Any,
    audio_path: str,
    language: str,
    context: str,
    chunk_size: float,
    memory_num: int,
    load_seconds: float,
) -> dict[str, Any]:
    """
    使用已加载好的引擎执行一次真实转写。

    这里沿用已经验证过的规避策略：

    1. 先显式 `load_audio(..., duration=None)`；
    2. 再调用 `engine.asr(...)`；
    3. 不直接走上游默认 `transcribe(audio_file)` 包装。
    """

    ensure_audio_path(audio_path)

    transcribe_started_at = time.time()
    audio = load_audio_fn(audio_path, sample_rate=16000, start_second=None, duration=None)
    result = engine.asr(
        audio=audio,
        context=context,
        language=language,
        chunk_size_sec=chunk_size,
        memory_chunks=memory_num,
        temperature=0.4,
        rollback_num=5,
    )
    transcribe_seconds = time.time() - transcribe_started_at

    return normalize_result(result, load_seconds, transcribe_seconds)
