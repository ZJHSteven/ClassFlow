# 项目状态快照

## 当前结论（必须最新）
- 现状：主仓库代码、自动化测试、真实 DashScope 冒烟、后端常驻服务、Cloudflare Worker 外网代理均已打通；Worker 已切到正式后端域名 `classflow-backend.zjhstudio.com`，前端页面已取消定时轮询。
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
- 已完成：Cloudflare Worker 已重新发布到 `https://classflow-web.zhangjiahe0830.workers.dev`，并已改为转发到 `https://classflow-backend.zjhstudio.com`。
- 已完成：前端“任务台 / 课程库”已取消 `5s / 8s` 定时轮询，改为“首次加载 + 手动刷新 + 窗口回到前台时同步一次”，从而避免界面闪烁与 Worker 持续计费。
- 已完成：已把后端运行环境与 Worker secret 的共享 Bearer Token 统一旋转到新值，并验证“后端直连无 token 为 401、带新 token 为 200；Worker 无需前端手带 token 即可访问受保护接口”。
- 已完成：`smartclass-downloader` 已调整为“指向 Worker 域名时可留空 Bearer Token”，并通过 `node --check` 与 `node --test`。
- 已完成：前端已补齐课程总稿/manifest 下载按钮，修复“总稿未生成时仍保留旧预览”的误导展示，并为任务/课程列表增加 hover、选中、按下反馈；新版 Worker 已重新发布。
- 已确认：`https://classflow.zjhstudio.com` 当前会被 Cloudflare Access 重定向到登录页；如果要公开访问，需要你在 Cloudflare Zero Trust 里调整 Access 策略。
- 正在做：按新确认的方向重构存储与前端交互，具体包括“Worker 绑定 R2、后端经 Worker 私有接口写读删产物、失败任务保留 7 天、本地成功即删、前端补齐动画/下载/删除入口”。
- 已完成：已把 Worker 的 R2 绑定骨架与后端 `worker` 产物模式代码接上；当前代码已支持“后端通过 Worker 私有接口写/读/删产物”，为后续 R2 生命周期、任务删除和前端下载入口打下基础。
- 已完成：后端已补上“任务级产物下载接口、失败任务彻底删除接口、课程产物重建函数，以及 SQLite 事件日志按天数/每任务条数裁剪”的代码与测试。
- 下一步：先改 Worker + 后端产物链路与接口，再改前端课程库/任务台交互，最后做完整测试并部署。

## 关键决策与理由（防止“吃书”）
- 决策A：采用单仓结构承载后端与前端。（原因：当前仓库为空，最利于统一测试、部署与文档。）
- 决策B：前端通过 Cloudflare Worker 代理后端。（原因：避免在浏览器中暴露后端 Bearer Token。）
- 决策C：使用 SQLite 保存任务与课程聚合状态。（原因：部署最轻、恢复简单、适合校园机常驻服务。）
- 决策D：后端公网入口已切换到 `classflow-backend.zjhstudio.com`；Worker secret 已不再依赖 Quick Tunnel。（原因：正式 Tunnel 域名现已可访问，固定域名比临时隧道更稳定。）
- 决策E：前端默认关闭定时轮询，只保留显式刷新和焦点回到页面时的同步。（原因：减少视觉闪烁、避免 Worker 按请求计费被无意义消耗。）
- 决策F：userscript 优先访问 Worker，而不是直连后端 Tunnel。（原因：这样脚本端可不暴露后端 Bearer Token，配置更简单，也更不容易填错。）
- 决策G：在 `classflow.zjhstudio.com` 仍受 Cloudflare Access 保护期间，公共脚本与自动请求默认仍以 `workers.dev` 或明确可访问域名为准；自定义域名主要作为你登录后的浏览器入口使用。（原因：未登录访问该域名当前仍会 302 到 Access 登录页。）
- 决策H：新一轮存储改造采用“Worker 绑定 R2 + 后端调用 Worker 私有产物接口”方案，不继续使用后端直连 R2 作为最终形态。（原因：用户明确要求隐藏 R2 对外访问面，并统一从 Worker 下载。）
- 决策I：本地长期产物不保留；成功任务在产物入库后立即清掉本地工作目录，失败任务仅保留 7 天。（原因：控制本地空间占用，同时保留必要的失败排查窗口。）

## 常见坑 / 复现方法
- 坑1：`CapsWriter-Offline` 默认分支看不到云转写实现；需要切到 `feat/bailian-cloud-migration` 分支参考 `dashscope_rest_client.py` 与 `file_upload_resolver.py`。
- 坑2：旧的 `tsc -p tsconfig.worker.json` 曾把 `worker/*.js` 直接输出到源码目录，Vitest 会把这些残留文件当成重复测试执行；现已改成 `noEmit`，但拉起测试前仍要避免目录里残留旧产物。
- 坑3：Tampermonkey 若只写 `@connect 127.0.0.1`，切到 Cloudflare Tunnel 域名后会直接跨域失败；双模式版本必须放宽到可访问后端实际域名。
- 坑4：这台校园机发起 Quick Tunnel 时默认 `quic` 会超时，外部表现为 `530`；需要显式切到 `http2`。
- 坑5：`classflow.zjhstudio.com` 当前受 Cloudflare Access 保护，未登录会直接 `302` 到 Access 登录页；这不是前端挂了，而是访问策略生效。
- 坑6：如果 userscript 指向的是 Worker 域名，却仍强制要求手填后端 token，实际上会把本来可以隐藏的密钥再次暴露给脚本侧；推荐脚本走 Worker 时允许 token 留空。
- 坑7：课程列表里可能存在“已收片段但尚无总稿”的课程；这时后端返回 `merged_markdown_path = null` 是正常状态，前端若继续盲拉 `course.md` 就会出现 404 误导。
