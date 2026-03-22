# 项目状态快照

## 当前结论（必须最新）
- 现状：主仓库已完成执行文档初始化，并已生成 `apps/backend` Rust 工程骨架。
- 已完成：方案已定稿；已确认本机具备 Rust 与 Node.js；确认 `create-cloudflare` 在 Node 18.19.1 上会因 `File is not defined` 失败；确认 `CapsWriter-Offline` 的百炼实现位于 `feat/bailian-cloud-migration` 分支；已清理 `cargo new` 误生成的子仓库元数据。
- 已完成：Rust 后端第一轮 `cargo check` 与单元测试已通过，核心骨架可编译。
- 已完成：Rust 后端接口级测试已通过，验证了鉴权、任务执行、课程聚合、失败重试。
- 已完成：已用兼容 Node 18 的 `create-vite@5.4.0` 生成 `apps/web` React + TypeScript 前端模板。
- 已完成：已在用户目录安装 Node.js `v24.14.0`，并让 `node/npm/npx/corepack` 默认指向新版环境。
- 已完成：前端测试夹具已改成按 URL 分发假数据，消除了轮询与详情请求导致的顺序式 mock 串线问题。
- 已完成：前端旧编译残留 `worker/*.js` 已清理，`npm test` 已通过。
- 已完成：前端 `npm run lint`、`npm test`、`npm run build` 已全部通过，Worker 代理与 React 页面已形成可交付基础版本。
- 已完成：`smartclass-downloader` 已完成 `Gopeed / ClassFlow` 双模式投递改造；新增后端地址、Bearer Token、默认学期配置，并通过 `node --check` 与 `node --test`。
- 已完成：已补齐 `docs/api-contract.md`、`docs/deployment.md`、`apps/backend/env.example`、`apps/web/.dev.vars.example`、systemd 模板、cloudflared ingress 示例与临时目录清理脚本。
- 已完成：最终自动化验收已通过：
  - 后端：`cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test`
  - 前端：`npm run lint`、`npm test`、`npm run build`
  - 脚本：`node --check smartclass-downloader.user.js`、`node --test smartclass-downloader.test.cjs`
- 正在做：整理最终交付说明，并标记真实环境仍需补装的系统依赖。
- 下一步：在真实密钥、真实 R2、真实 DashScope、真实 Tunnel 下做一次外部联调冒烟。

## 关键决策与理由（防止“吃书”）
- 决策A：采用单仓结构承载后端与前端。（原因：当前仓库为空，最利于统一测试、部署与文档。）
- 决策B：前端通过 Cloudflare Worker 代理后端。（原因：避免在浏览器中暴露后端 Bearer Token。）
- 决策C：使用 SQLite 保存任务与课程聚合状态。（原因：部署最轻、恢复简单、适合校园机常驻服务。）

## 常见坑 / 复现方法
- 坑1：本机已确认**未安装** `ffmpeg/ffprobe`，真实流水线在抽音频步骤前会失败。
- 坑2：Cloudflare 官方 `create-cloudflare` 在当前 Node 18.19.1 环境中直接崩溃，需要改为手工搭建兼容的 Worker + Vite 结构，或后续升级 Node 版本后再切回官方脚手架。
- 坑3：`CapsWriter-Offline` 默认分支看不到云转写实现；需要切到 `feat/bailian-cloud-migration` 分支参考 `dashscope_rest_client.py` 与 `file_upload_resolver.py`。
- 坑4：旧的 `tsc -p tsconfig.worker.json` 曾把 `worker/*.js` 直接输出到源码目录，Vitest 会把这些残留文件当成重复测试执行；现已改成 `noEmit`，但拉起测试前仍要避免目录里残留旧产物。
- 坑5：Tampermonkey 若只写 `@connect 127.0.0.1`，切到 Cloudflare Tunnel 域名后会直接跨域失败；双模式版本必须放宽到可访问后端实际域名。
