# 项目状态快照

## 当前结论（必须最新）
- 现状：主仓库代码、自动化测试、真实 DashScope 冒烟、后端常驻服务、Cloudflare Worker 外网代理均已打通；当前公网后端走 Quick Tunnel 临时方案，正式自定义域名仍缺 DNS 写入权限。
- 已完成：方案已定稿；已确认本机具备 Rust、Node.js 与 ffmpeg；确认 `CapsWriter-Offline` 的百炼实现位于 `feat/bailian-cloud-migration` 分支；已清理 `cargo new` 误生成的子仓库元数据。
- 已完成：Rust 后端第一轮 `cargo check` 与单元测试已通过，核心骨架可编译。
- 已完成：Rust 后端接口级测试已通过，验证了鉴权、任务执行、课程聚合、失败重试。
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
- 已完成：后端以 `systemd --user` 方式常驻运行，监听 `127.0.0.1:8787`，健康检查与鉴权检查已通过。
- 已完成：使用本机生成的测试 MP4 跑通了真实流水线：`下载 -> ffmpeg 抽音频 -> DashScope 上传/轮询 -> 文本落盘 -> 课程总稿合并 -> 临时目录清理`。
- 已完成：Cloudflare Worker 已发布到 `https://classflow-web.zhangjiahe0830.workers.dev`，并已通过 `/api/v1/health`、`/api/v1/courses`、`/artifacts/course.md` 外网验证。
- 正在做：整理最终交付说明，并记录当前临时出网方案与正式化缺口。
- 下一步：补齐 Cloudflare `classflow-backend.zjhstudio.com` 的 DNS 记录与正式 Bearer/R2 环境变量，把 Worker 后端地址从 Quick Tunnel 切换到固定域名。

## 关键决策与理由（防止“吃书”）
- 决策A：采用单仓结构承载后端与前端。（原因：当前仓库为空，最利于统一测试、部署与文档。）
- 决策B：前端通过 Cloudflare Worker 代理后端。（原因：避免在浏览器中暴露后端 Bearer Token。）
- 决策C：使用 SQLite 保存任务与课程聚合状态。（原因：部署最轻、恢复简单、适合校园机常驻服务。）
- 决策D：当前先使用 `systemd --user + local artifact store + Quick Tunnel(http2)` 完成真实可用部署。（原因：本机已具备 DashScope，但尚未发现可直接写入的 R2 凭据，且当前账号无法经 API 创建正式 Tunnel DNS 记录。）

## 常见坑 / 复现方法
- 坑1：`CapsWriter-Offline` 默认分支看不到云转写实现；需要切到 `feat/bailian-cloud-migration` 分支参考 `dashscope_rest_client.py` 与 `file_upload_resolver.py`。
- 坑2：旧的 `tsc -p tsconfig.worker.json` 曾把 `worker/*.js` 直接输出到源码目录，Vitest 会把这些残留文件当成重复测试执行；现已改成 `noEmit`，但拉起测试前仍要避免目录里残留旧产物。
- 坑3：Tampermonkey 若只写 `@connect 127.0.0.1`，切到 Cloudflare Tunnel 域名后会直接跨域失败；双模式版本必须放宽到可访问后端实际域名。
- 坑4：这台校园机发起 Quick Tunnel 时默认 `quic` 会超时，外部表现为 `530`；需要显式切到 `http2`。
- 坑5：当前 Cloudflare 账号可更新 Tunnel ingress、可发布 Worker，但直接创建 `classflow-backend.zjhstudio.com` DNS 记录会返回认证错误，因此正式固定域名还未完成。
