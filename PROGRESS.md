# 项目状态快照

## 当前结论（必须最新）
- 现状：主仓库已完成执行文档初始化，并已生成 `apps/backend` Rust 工程骨架。
- 已完成：方案已定稿；已确认本机具备 Rust 与 Node.js；确认 `create-cloudflare` 在 Node 18.19.1 上会因 `File is not defined` 失败；确认 `CapsWriter-Offline` 的百炼实现位于 `feat/bailian-cloud-migration` 分支。
- 正在做：修正 Rust 后端第一轮 `cargo check` 暴露的编译问题，并继续补齐后台 worker 与对象存储实现。
- 下一步：让后端完成可编译状态后，补全单元/集成测试，再搭建前端与 Worker 代理。

## 关键决策与理由（防止“吃书”）
- 决策A：采用单仓结构承载后端与前端。（原因：当前仓库为空，最利于统一测试、部署与文档。）
- 决策B：前端通过 Cloudflare Worker 代理后端。（原因：避免在浏览器中暴露后端 Bearer Token。）
- 决策C：使用 SQLite 保存任务与课程聚合状态。（原因：部署最轻、恢复简单、适合校园机常驻服务。）

## 常见坑 / 复现方法
- 坑1：本机暂未确认安装 `ffmpeg/ffprobe`，真实流水线在抽音频步骤前会失败。
- 坑2：Cloudflare 官方 `create-cloudflare` 在当前 Node 18.19.1 环境中直接崩溃，需要改为手工搭建兼容的 Worker + Vite 结构，或后续升级 Node 版本后再切回官方脚手架。
- 坑3：`CapsWriter-Offline` 默认分支看不到云转写实现；需要切到 `feat/bailian-cloud-migration` 分支参考 `dashscope_rest_client.py` 与 `file_upload_resolver.py`。
