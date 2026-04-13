# ExecPlan

## 目标
- 在 `ClassFlow` 仓库中实现第一阶段完整方案：Rust 后端、React + Cloudflare Worker 前端、以及与 `smartclass-downloader` 的联动改造。

## 执行步骤
1. 已完成：初始化主仓库基础文件，建立 `PLANS.md`、`PROGRESS.md`、`.gitignore` 等执行基线。
2. 已完成：搭建 Rust 后端工作区与核心模块，打通配置、数据模型、API、SQLite 持久层、任务执行骨架。
3. 已完成：为后端补全单元测试、集成测试与 mock 流程测试。
4. 已完成：搭建 React + Cloudflare Worker 前端，完成任务台、课程库、代理 API，并通过 `lint / test / build`。
5. 已完成：改造 `smartclass-downloader`，增加 `Gopeed / ClassFlow` 双模式投递，并补上 `node:test` 脚本测试。
6. 已完成：已补齐部署文档、systemd 示例、cloudflared 示例，并完成当前环境下可自动执行的最终验收测试。
7. 已完成：已在真实环境中启动 `systemd --user` 后端服务、跑通 DashScope 真转写冒烟、发布 `workers.dev` 前端，并验证公网 API 与课程产物读取。
8. 已完成：已移除前端定时轮询，改为“手动刷新 + 页面重新聚焦同步”；并把 Worker 后端地址切换到 `classflow-backend.zjhstudio.com`。
9. 已完成：统一整理后端 / Worker / userscript 的鉴权说明，完成 userscript“走 Worker 时可不手填 token”的改造，并完成共享 token 旋转与重新部署验证。
10. 已完成：在不重启当前 release 后端的前提下，完成前端安全改动：总稿 404 误导修复、下载按钮补齐、交互反馈增强，并重新发布 Worker；后端下载/上传并发与进度采集改造留待后续启用。
11. 已完成：后端已补齐下载/上传鲁棒性主线：`aria2c` 下载、下载重试与低速退出、上传与转写并发拆分、DashScope 请求重试、上传重试、上传/转写检查点持久化，以及“失败后从已完成阶段继续”的 API 集成测试。
12. 已完成：补齐“用户肉眼可感知”的交互与可观测性闭环，包括前端按钮/切换动效显著化、长错误文本自动换行、受控下载反馈，以及后端下载/上传阶段进度与速率暴露；同时把 `aria2c` 低速退出改成“默认关闭、显式配置才启用”。
13. 已完成：按上线前最终口径收口前端与部署：已彻底移除 `useReducedMotion` 降级逻辑、确保“任务运行中的下载/上传进度与速率”能在前端持续可见，并已同步部署前后端。
14. 已完成：修复后端下载进度采集的时序错误，避免 `aria2c` 进度只在进程退出后才一次性写库；真实任务现已验证“下载中持续更新进度/速率”与“切到上传阶段后继续更新进度/速率”。
15. 已完成：把任务台从“运行中 1.2 秒短轮询”改成“后端 SSE 推送任务摘要 + 前端手动刷新详情兜底”，优先压缩 Worker 请求次数，并保留速率/进度可见性；后端 `cargo check / test`、前端 `lint / test / build` 均已通过。

## 当前决策
- 后端使用 `axum + tokio + sqlx(sqlite) + reqwest + aria2c`。
- 前端使用 Cloudflare 官方 React Workers 脚手架方向，浏览器只访问 Worker，不直接访问后端。
- 课程归组键固定为 `学期 + 日期 + 课程名 + 老师`。
- 第一版只报告已收片段数，不启发式推断课程理论节数。
- 后端公网出口使用 `classflow-backend.zjhstudio.com` 固定域名；Quick Tunnel 仅保留为应急调试手段。
- 前端默认关闭自动轮询，优先降低闪烁与 Worker 请求次数。
- 新一轮改造改为“Worker 绑定 R2，后端通过 Worker 私有接口读写最终产物”，而不是后端直连 R2。（原因：用户明确要求不暴露 R2 外链，且最终产物以文本/JSON 为主，适合由 Worker 做统一下载出口。）
- 成功任务的本地媒体临时文件在产物成功入库后立即删除；失败任务本地工作目录保留 `7` 天，若之后重试成功则立即删除。（原因：既要节省本地空间，也要给失败任务保留复查与重试窗口。）
- 长期保留的课程总稿、manifest、原始结果统一放在 R2；本地尽量不保留长期产物。（原因：避免校园机磁盘逐渐被历史产物占满，同时让前端所有下载都走 Worker。）
- 任务与课程界面需要提供明确的 hover / tap / 选中动画、任务放弃删除入口，以及“凡仍存在于 R2 的产物，都能在前端直接下载”。（原因：当前可点击反馈弱、下载入口不全，已经影响实际使用。）
- 下载链路必须切换到 `aria2c`，并显式落地：断点续传、自动重试、连接超时、低速判定、下载失败分类。（原因：当前 `reqwest + response.bytes()` 已经在真实环境里频繁出现下载中断，不能继续当成可交付实现。）
- 上传链路至少要补齐：请求超时、自动重试、上传并发与转写并发拆分、阶段级断点续跑；不能再只靠“整任务重试”。（原因：校园网弱上行环境下，上传失败是必然要面对的主路径，而不是边角异常。）
- 新一轮前端交互要以“可感知反馈优先”收口：即使用户系统偏好减少动态效果，也要保留轻量 hover / press / 渐变反馈，而不是把按钮完全退化成近乎静态文本。（原因：当前实现把 `useReducedMotion()` 直接用作总开关，导致部分环境里几乎看不到任何交互反馈。）
- `aria2c` 的低速退出默认值不再强制启用；只有显式配置正数阈值时才启用“低于某速率自动失败”。（原因：校园网/教学网场景天然存在长时间低速，默认强杀会把本可完成的慢下载误判成失败。）
- 前端最终交付不再保留 `reduced motion` 条件分支，统一输出明确的 hover / tap / 切页动画。（原因：本项目是操作台，不是内容站；这里更重要的是操作确认感，而不是因环境偏好把交互反馈整体削弱。）
- “进度条与速率”验收标准以“任务运行中后端阶段可见”为准，而不是浏览器下载产物时可见。（原因：用户明确关心的是后端任务正在下载视频、正在上传音频时的实时状态。）
- 下载进度链路必须采用“子进程运行中并发读 stdout/stderr 并立刻写库”的实现，不能先 `wait()` 再统一解析输出。（原因：后一种写法会让数据库在任务执行期间长期停留在空值，前端轮询看不到任何实时进度。）
- 任务台的实时状态同步优先采用 `SSE`，不继续依赖高频 HTTP 轮询；任务详情日志保留“选中加载 + 手动刷新”模式。（原因：用户明确要求降低 Cloudflare Worker 按请求计费消耗，而本场景只需要后端单向推送进度，不需要 WebSocket 的双向能力。）

## 2026-04-13 任务台 SSE 请求风暴修复 ExecPlan

### 目标
- 修复任务台进入页面后持续显示“正在加载任务列表...”并反复请求 `/api/v1/tasks` 的前端状态循环。
- 保持任务状态刷新以 SSE 为主：服务端有任务变化时推送任务摘要快照，前端不再因为普通状态更新重复触发阻塞式 HTTP 列表加载。
- 保留 HTTP 作为必要兜底：首屏 SSE 不可用或超时时才请求任务列表；重试、删除、下载、详情按需查看仍继续走 HTTP。
- 增加回归测试，明确断言任务台稳定后不会持续创建新的普通任务列表请求，也不会重复重建 SSE 连接。

### 执行步骤
1. 已完成：定位到 `TaskPanel` 中 `tasks -> loadTaskDetail -> loadTasks -> useEffect` 的依赖链会导致列表更新后重新触发阻塞加载。
2. 已完成：重构 `TaskPanel` 的任务列表状态读取方式，用 `ref` 保存最新任务摘要，避免详情缓存判断依赖 `tasks` 状态本身。
3. 已完成：让任务页首屏优先等待 SSE 首帧；只有 SSE 不可用、解析失败或首帧超时才退回一次 HTTP 列表加载。
4. 已完成：调整手动刷新/回到前台同步逻辑，避免在 SSE 已连接时无意义调用 `/api/v1/tasks`；按钮改为“重连任务流”。
5. 已完成：补充前端测试，覆盖稳定首屏不会持续重复请求任务列表、SSE 首帧可填充任务列表、SSE 失败可退回 HTTP。
6. 已完成：前端验证已通过 `npm run lint`、`npm test`（19 项）、`npm run build`；本轮只改前端 SSE 消费与测试，未改后端 SSE 契约。
7. 已完成：前端 Worker 已重新构建并通过 `npx wrangler deploy` 发布，当前版本号为 `1659f30b-44ae-4842-90d5-f5c3010ff388`；匿名线上探针返回 `302` 到 Cloudflare Access 登录页，符合当前入口受 Access 保护的预期。

## 2026-04-13 临时 OSS 重试与 HGP404 写产物排查 ExecPlan

### 目标
- 修复“历史上传检查点只看数据库 `uploaded_source_url`，不考虑 DashScope 临时 `oss://` 文件已过期”的重试缺陷，避免失败任务多天后重试仍跳过上传并继续失败。
- 排查最近毛泽东思想课程三节课在 `storing_artifacts` 阶段失败、日志出现 `HGP404` 的真实原因，区分是 Worker 私有产物接口、Cloudflare Access / 代理链路、R2 对象路径，还是后端写入策略问题。
- 补齐自动化测试，覆盖“临时 OSS 过期后必须重新上传”和“写产物 404 错误信息必须足够可诊断”这两条回归边界。
- 如代码修复通过验证，再构建并重启系统级后端服务，必要时对失败任务执行受控重试验证。

### 执行步骤
1. 已完成：审查 `worker.rs`、`pipeline.rs`、`artifacts.rs`、SQLite 仓储与现有 API 测试，定位上传检查点、转写检查点、产物写入错误传播的当前实现。
2. 已完成：读取真实数据库与系统级后端日志，按课程名、日期、阶段、错误文本筛出“医学人文英语”和最近三节“毛泽东思想”失败任务，确认失败发生时间、任务 ID、检查点字段和最新错误。
3. 已完成：修改重试策略：如果失败阶段在转写及之前，且保存的是 `oss://` 临时 URL，就不能跨多天盲跳上传；超过 47 小时安全复用窗口或缺少保存时间时会重新上传音频。
4. 已完成：增强 Worker 产物写入 404 / Access 重定向的错误上下文，包含产物路径、HTTP 状态、响应体、请求 URL 与 `Location`，避免只显示无法定位来源的短码。
5. 已完成：新增后端测试，覆盖临时 OSS 过期重试、未过期重试仍可复用上传检查点、产物写入遇到 Cloudflare Access 重定向时不自动跟随到空体 404。
6. 正在做：后端格式化、编译与测试已通过；下一步继续跑 Clippy、更新真实后端环境变量、构建 release、重启系统级 `classflow-backend.service` 与真实任务重试验收。

### 当前状态
- 已完成：医学人文综合英语失败任务 `498d9a22-0968-4929-89ed-b8336c8d1999` 的旧 `oss://` 上传路径来自 `2026-04-04`，在 `2026-04-13` 重试时百炼返回 `BadRequest.ResourceNotExist`，根因是临时 OSS 对象已过期但后端仍跳过上传。
- 已完成：三条毛泽东思想失败任务 `bbcd053e-e4a5-4de9-880b-09a8f2c74a77`、`c81a5111-e0b3-4a22-9a5a-8b39404f1fb0`、`fa6c16b3-7125-4fae-8ac8-78f119cd7d99` 都已有转写检查点，失败点在写 Worker 私有产物；真实探针证明后端请求先被 Cloudflare Access 拦成 `302`，旧 `reqwest` 自动跟随重定向后最终表现为空体 `404`。
- 已完成：本地后端验证已通过 `cargo fmt --check --manifest-path apps/backend/Cargo.toml`、`cargo check --manifest-path apps/backend/Cargo.toml`、`cargo test --manifest-path apps/backend/Cargo.toml`。

## 2026-03-25 本轮 ExecPlan

### 目标
- 收紧前端布局密度，确保主操作按钮在常见笔记本浏览器视口内更容易一次看全。
- 为任务台、课程库的左侧列表和右侧详情/日志区域补上明确高度上限与内部滚动，避免页面整体被历史数据无限拉长。
- 调整任务与课程排序口径，改为“日期越新越靠前”，优先展示最近课程。
- 优化课程总稿读取链路，尽量减少“预览一次、下载一次就重复命中多层 Worker”的额外消耗。

### 执行步骤
1. 审查任务台、课程库、下载链路、Worker 代理与仓储排序代码，明确哪些请求是当前重复发生的。
2. 调整前端页面骨架与样式：压缩字号、间距、卡片内边距、表格行高，并为左右面板设定更稳定的视口高度。
3. 为任务台左侧列表、课程库左侧列表、任务详情日志区、错误提示区、课程总稿预览区增加内部滚动与长度上限，确保页面主按钮不被长内容挤出首屏。
4. 调整任务列表与课程列表排序规则，统一改为按课程日期倒序展示，并保持同日内排序稳定。
5. 优化课程总稿读取与下载逻辑，优先复用已拿到的总稿内容，减少重复请求；同时评估并收敛 Worker 产物读取是否可以避免“Worker 再请求自己”的链路。
6. 更新或新增前端/后端测试，覆盖排序、滚动容器关键结构、课程总稿复用逻辑与现有 SSE 行为不回退。
7. 运行 `npm run lint`、`npm test`、`npm run build`，并补跑相关后端测试；完成后更新 `PROGRESS.md` 并提交。

### 当前状态
- 已完成：步骤 1 到 7 均已执行完毕。
- 已完成：前端布局已收紧，左右面板改为固定高度配合内部滚动，长日志/长报错/长课程列表不再把整页无限撑长。
- 已完成：课程总稿链路已优化为“公开 Worker 直读 R2 + 前端预览复用到下载”，减少重复 Worker 命中。
- 已完成：任务列表与课程列表均已改成最新日期优先，相关前后端测试、前端构建与后端测试均已通过。

## 2026-03-27 DashScope 转写模型修正 ExecPlan

### 目标
- 核对阿里云百炼录音文件转写当前官方模型名，确认 `fun-asr` 与 `fun-asr-mtl` 的适用差异。
- 在不破坏现有上传凭证、异步转写、任务轮询链路的前提下，把后端默认模型从旧值调整为更符合当前免费额度方案的模型名。
- 把模型配置入口明确暴露到环境变量与部署文档，避免后续再靠源码猜默认值。
- 通过自动化测试与真实 DashScope API 冒烟共同确认改动有效。

### 执行步骤
1. 审查后端配置、上传凭证请求、转写任务提交、测试夹具与部署文档，定位模型名的所有读写入口。
2. 查阅阿里云官方文档，确认录音文件转写可用模型名，以及临时 `oss://` 上传与模型名必须一致的约束。
3. 用当前环境的真实 `DASHSCOPE_API_KEY` 做最小化 API 冒烟，至少验证 `getPolicy` 能接受目标模型名；若条件允许，再补一次最小音频上传与异步转写提交验证。
4. 修改后端默认配置、示例环境变量、测试默认值与部署说明，把模型名暴露为可直接修改的显式配置项。
5. 运行后端格式化、静态检查与测试；若真实 API 冒烟可执行，则再次验证改后的目标模型链路。
6. 更新 `PROGRESS.md` 记录本轮结论、关键决策与部署注意事项，并提交。

### 当前状态
- 已完成：已定位当前默认模型为 `fun-asr`，且上传凭证与转写提交共用同一配置值。
- 已完成：已核对阿里云官方文档，并完成真实 API 冒烟，确认 `fun-asr` 与 `fun-asr-mtl` 当前都可正常提交并成功转写官方示例音频。
- 已完成：已把默认模型调整为 `fun-asr-mtl`，并把配置入口在示例环境变量与部署文档中进一步写清楚。
- 已完成：后端验证已通过 `cargo fmt --check`、`cargo check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test`。

## 2026-03-28 课程总稿下载命名微调 ExecPlan

### 目标
- 把前端“下载课程总稿”的默认文件名改成你既有的使用习惯：`月.日-课程名-老师.md`。
- 保持课程原始数据里的完整日期不变，只调整浏览器下载时展示给用户的文件名。
- 确认这次改动只影响前端课程级下载，不误伤后端存储键、课程归组键与已有产物路径。

### 执行步骤
1. 审查课程库面板里课程总稿与课程清单的文件名拼接逻辑，确认当前规则与受影响按钮范围。
2. 把下载文件名改为“日期只保留月.日，顺序为日期-课程名-老师”，并为缺失或异常日期准备安全兜底，避免生成空文件名。
3. 新增或更新前端测试，覆盖“课程总稿下载命名符合旧规则”以及“已预览总稿时下载仍不重复请求”的现有行为不回退。
4. 运行前端 `npm test`、`npm run lint`、`npm run build` 完整验证。
5. 更新 `PROGRESS.md` 记录本轮命名规则变更、测试结果与重新部署状态，并完成前端重新部署。

### 当前状态
- 已完成：已把课程总稿下载文件名改为 `月.日-课程名-老师.md`，并保持后端课程日期、课程归组键、产物路径不变。
- 已完成：前端新增 `courseDownloadFilename` 命名工具与对应单元测试，覆盖标准日期、兼容日期、非法字符清洗、缺省兜底命名。
- 已完成：前端验证已通过 `npm test`（15 项测试）、`npm run lint`、`npm run build`。
- 已完成：Cloudflare Worker 已重新部署到 `https://classflow-web.zhangjiahe0830.workers.dev`，版本号 `81191b71-ac36-4b71-ba66-5b2591d49fc4`；线上抽样验收的首页与 `/api/v1/health` 均返回正常。

## 2026-04-03 系统级托管与 Access 收口 ExecPlan

### 目标
- 把当前依赖 `systemd --user` 的后端常驻方式迁移为真正的系统级 `systemd` 服务，彻底消除“SSH / VS Code 会话断开后后端消失”的风险。
- 迁移时继续沿用现有真实数据目录与环境变量文件，避免改动数据库、缓存、临时目录与产物目录造成额外风险。
- 梳理当前前端、Worker、后端 Tunnel、自定义域名与 `workers.dev` 的访问关系，为后续接入 Cloudflare Access Service Token 做准备。
- 把当前机器上的真实部署路径、systemd 单元位置、数据目录位置与 Access 配置步骤补进文档，方便后续维护与自动化测试。

### 执行步骤
1. 审查当前用户级 `classflow-backend.service`、真实 `backend.env`、数据库路径、临时目录、产物目录与 `cloudflared` 状态，确认迁移边界。
2. 在不迁移数据目录的前提下，生成并安装系统级 `classflow-backend.service`，让它以当前账号 `zjhsteven` 身份运行，但由系统级 `systemd` 在开机阶段托管。
3. 停用用户级 `classflow-backend.service`，启动系统级服务，并验证本机 `127.0.0.1:8787`、Tunnel 外网健康检查与当前进程归属。
4. 梳理仓库与现网中所有依赖 `workers.dev`、后端 Tunnel 域名、`BACKEND_BASE_URL`、`BACKEND_TOKEN` 的位置，明确后续 Access 收口时必须同步修改的点。
5. 更新 `docs/deployment.md`、`PLANS.md`、`PROGRESS.md`，记录真实部署路径、目录清单、系统级托管方案，以及 Cloudflare Access / Service Token 的配置步骤。
6. 运行必要验证命令，确认迁移后服务可持续运行；完成后提交本轮修改。

### 当前状态
- 已完成：已确认当前线上后端此前实际运行于 `~/.config/systemd/user/classflow-backend.service`，并且用户未开启 `linger`，这正是“VS Code 断开后后端跟着消失”的直接原因。
- 已完成：已确认真实环境变量文件原始来源位于 `/home/zjhsteven/.config/classflow/backend.env`，并已复制到系统级路径 `/etc/classflow/backend.env`；真实数据库位于 `/home/zjhsteven/.local/state/classflow/data/classflow.db`，临时目录位于 `/home/zjhsteven/.local/state/classflow/tmp`，本地产物目录位于 `/home/zjhsteven/.local/state/classflow/artifacts`。
- 已完成：已确认 `cloudflared.service` 当前由系统级 `systemd` 正常托管并已启用。
- 已完成：系统级 `/etc/systemd/system/classflow-backend.service` 已安装并 `enable --now`，当前以后端二进制 `/home/zjhsteven/ClassFlow/target/release/backend` 运行于 `/system.slice/classflow-backend.service`。
- 已完成：旧的用户级 `classflow-backend.service` 已 `disable --now`，当前 `http://127.0.0.1:8787/api/v1/health` 与 `https://classflow-backend.zjhstudio.com/api/v1/health` 验证均通过。
- 已完成：已在 `docs/deployment.md` 中补充当前校园机的真实部署路径、系统级托管方式、以及 Cloudflare Access / Service Token 的配置步骤。

## 2026-04-09 Worker 回源与前端卡顿排查 ExecPlan

### 目标
- 先验证 `classflow-web.zhangjiahe0830.workers.dev` 走代理节点是否是当前 `storing_artifacts` 失败的直接诱因。
- 在线上环境最小化改动 `mihomo` 规则，把该域名单独切到 `DIRECT`，然后立刻重试当前 `11` 个失败任务，确认任务是否能从“只差最后写产物”恢复成功。
- 若代理切换后故障仍存在，再继续收缩到后端 `WorkerArtifactStore` 传输策略与前端首屏 / 详情面板卡顿问题。
- 同步记录“系统级 service 但仍按 `uid=1000` 进入 TUN”的现网事实，避免后续继续把问题归因到错误方向。

### 执行步骤
1. 审查并备份当前 `/etc/mihomo/config.yaml`，确认 `tun.include-uid`、`redir-host`、规则顺序与 `classflow-web.zhangjiahe0830.workers.dev` 当前命中策略组。
2. 在 `rules:` 顶部附近新增对 `classflow-web.zhangjiahe0830.workers.dev` 的精确 `DIRECT` 规则，重载 `mihomo`，并确认新连接命中 `DIRECT` 而非 `Kuromis`。
3. 统计当前失败任务列表，在线上后端逐个调用重试接口，观察是否仍卡死在 `storing_artifacts`，并同步检查 `mihomo` / `journalctl` / 数据库状态。
4. 如果重试成功，继续补做后端最小健壮性修复：Worker 产物请求的超时、重试、错误展开与 URL 双斜杠清理。
5. 独立排查前端体验问题：首屏 `502`、`SSE` 首连易断、详情切换慢、课程面板串行请求重；先用现网探针复现，再决定是先改前端请求编排还是先改后端查询结构。
6. 更新 `PROGRESS.md` 记录本轮验证结果与最终结论，并按仓库要求提交。

### 当前状态
- 已完成：已确认 `classflow-backend.service` 虽由系统级 `systemd` 托管，但服务实际用户仍是 `zjhsteven`，而 `/etc/mihomo/config.yaml` 使用 `tun.include-uid: [1000]`，因此后端网络流量仍会进入 TUN，不会因“system 级托管”自动绕过代理。
- 已完成：已确认当前 `mihomo` DNS 模式为 `redir-host`，并非 `fake-ip`，因此现阶段不再把 `fake-ip` 作为主嫌疑。
- 已完成：已从 `mihomo` 日志中抓到 `2026-04-04 23:02:25 +0800`、`23:09:51 +0800`、`23:20:10 +0800` 对 `classflow-web.zhangjiahe0830.workers.dev:443` 的 `dial Kuromis ... i/o timeout` 直接证据。
- 已完成：已复现前端层面的一个近似问题：并发触发 `tasks` / `courses` / `tasks/stream` 时，普通列表请求常可返回 `200`，但 `SSE` 首连会间歇性在 TLS 层失败，这与“首屏首次加载常报 502、刷新后恢复”的现象一致。
- 已完成：已再次核对线上真实环境变量文件，确认 `/etc/classflow/backend.env` 仍保留旧值 `CLASSFLOW_DASHSCOPE_MODEL=fun-asr`，这与仓库里已切到 `fun-asr-mtl` 的默认值不一致，必须在本轮一并纠正，避免继续按旧模型计费。
- 已完成：已把线上 `/etc/classflow/backend.env` 的 `CLASSFLOW_DASHSCOPE_MODEL` 改为 `fun-asr-mtl`，并在无运行任务窗口重启系统级后端；随后已验证真实进程环境变量确实变为 `CLASSFLOW_DASHSCOPE_MODEL=fun-asr-mtl`。
- 已完成：已实测 `classflow-web.zhangjiahe0830.workers.dev -> DIRECT` 在这台机子上不可用；`curl` 直接打 Worker 会触发 TLS 失败，而 `mihomo` 记录到 `dial tcp 199.96.63.163:443: i/o timeout`。
- 已完成：已临时把 `classflow-web.zhangjiahe0830.workers.dev` 切到 `Exflux` 做对照实验；代表任务 `3a0da4ba-12c3-4b3e-8df3-21b5e179848f` 以及其余 `8` 条 `storing_artifacts` 失败任务均已重试成功，说明“最后一步写 Worker 产物失败”与链路选择高度相关。
- 已完成：按用户要求，线上 `mihomo` 现已把 `classflow-web.zhangjiahe0830.workers.dev` 的精确规则改回 `Kuromis` 主组，不再继续使用 `Exflux` 作为长期规则。
- 已完成：仓库代码已补强 `WorkerArtifactStore`：新增独立连接超时/总超时/有限重试/更完整错误上下文，并新增 URL 规范化与临时 `5xx` 自动重试测试；`cargo test -p backend` 已全绿。
- 已完成：已量化前端慢点来源。任务台首屏默认会并发触发 `GET /api/v1/tasks` 与 `GET /api/v1/tasks/stream`，随后再补 `GET /api/v1/tasks/{id}`；课程库则是 `GET /api/v1/courses` 之后串行触发 `GET /api/v1/courses/{key}` 与 `GET /api/v1/courses/{key}/artifacts/course.md`。
- 已完成：已定位“点任务详情特别慢”的硬根因之一：后端原先把 `transcript_json` / `transcript_text` 整包内联进 `/api/v1/tasks/{id}`，代表任务详情从后端返回时高达 `238466` 字节，其中 `transcript_json` 单独就占 `206697` 字节，而前端其实完全没用这两项。
- 已完成：已把任务详情接口改为“前端瘦身响应 + task.json 下载仍保留完整快照”，并重新部署系统级后端。重详情样本现已从 `238466 B / 2.18s` 降到 `5795 B / 0.52s`。
- 已完成：已对前端 `TaskPanel` 做两项直接缓解修复并重新部署 Worker：首次阻塞加载增加短重试，且首屏拿到首个列表成功前不再抢先建立 SSE；另外新增按 `updated_at` 命中的任务详情本地缓存，减少来回切换任务时的重复等待。
- 正在做：保留 `3` 条 `uploading_audio` 失败任务与 `1` 条 `ASR_RESPONSE_HAVE_NO_WORDS` 失败任务作为线上样本，继续把“DashScope 上传慢 / 易失败”和“前端首屏 502 是否已被缓解、SSE 首连是否仍偶发失败”作为下一阶段排查重点。

## 2026-04-09 Cloudflare Access 收口复核 ExecPlan

### 目标
- 把当前 `ClassFlow` 实际对公网暴露的前端、Worker 默认域名、后端 Tunnel 域名逐一核实清楚，避免只盯着自定义域名而漏掉旁路入口。
- 用“仓库代码 + 现网探针”双重证据确认：哪些入口已经被 Cloudflare Access 挡住，哪些入口仍能匿名访问，哪些入口虽然需要应用层 Bearer Token，但最外层仍未接入 Access。
- 输出一套可直接照着执行的收口顺序，优先先堵住真正能匿名读业务数据的入口，再处理后端 Tunnel 域名与自动化 Token。
- 将本轮结论固化到仓库记忆，避免后续再次误判“真正裸奔的到底是哪一个域名”。

### 执行步骤
1. 审查仓库内 `wrangler.toml`、Worker 代理代码、后端鉴权中间件与部署文档，定位所有公开访问入口与鉴权边界。
2. 对当前已知公网入口执行最小化在线探针，请求首页、健康检查与受保护业务 API，确认真实返回是 `200`、`401` 还是 `302 Access`。
3. 结合 Cloudflare 官方文档，核对 Access Service Token 的创建方式、请求头格式、`workers_dev = false` 与 Preview URL 的默认行为。
4. 整理“当前真实暴露面 -> 风险等级 -> 建议收口动作 -> 收口前置条件”的矩阵，明确先后顺序。
5. 更新 `PROGRESS.md` / `PLANS.md` 记录本轮事实与下一步操作建议，并提交。

### 当前状态
- 已完成：已确认仓库与文档中当前明确出现的公网入口至少有 `classflow.zjhstudio.com`、`classflow-web.zhangjiahe0830.workers.dev`、`classflow-backend.zjhstudio.com` 三个。
- 已完成：现网探针已确认 `https://classflow.zjhstudio.com/` 与 `https://classflow.zjhstudio.com/api/v1/tasks` 当前都会被 Cloudflare Access 重定向到登录页。
- 已完成：现网探针已确认 `https://classflow-backend.zjhstudio.com/api/v1/health` 当前可匿名访问，而 `https://classflow-backend.zjhstudio.com/api/v1/tasks` 在未带 Bearer Token 时会返回 `401`，说明它最外层尚未接入 Access，当前主要依赖应用层 `CLASSFLOW_BEARER_TOKEN`。
- 已完成：现网探针已确认 `https://classflow-web.zhangjiahe0830.workers.dev/api/v1/tasks` 当前可匿名返回完整任务列表；这说明真正的高风险公开入口是 `workers.dev` 默认域名，因为 Worker 会自动补上后端 Bearer Token。
- 已完成：已确认当前仓库的 [wrangler.toml](/home/zjhsteven/ClassFlow/apps/web/wrangler.toml) 里尚未显式声明 `workers_dev = false`，因此后续如果只在面板里手关 `workers.dev`，再次 `wrangler deploy` 时存在被重新打开的风险。
- 已完成：已通过 `npx wrangler versions list` 与 `npx wrangler deployments list` 确认当前 Worker 仍处于持续部署状态，需要把配置文件作为唯一事实源一并收口。
- 正在做：整理最终对外说明与落地步骤，明确“先封 `workers.dev`，再让 Worker 带 Access Service Token 回源后端 Tunnel”的安全收口方案。

## 2026-04-09 Access Service Token 接入与验证 ExecPlan

### 目标
- 把 Cloudflare Access Service Token 正确接入到 Worker 回源链路，确保后端 Tunnel 域名在接入 Access 后，前端代理仍能正常工作。
- 明确回答“前端浏览器、Worker、smartclass 脚本”这三类调用方各自是否需要携带 Service Token，避免把自动化凭证错误地下发到浏览器或脚本侧。
- 通过本地单元测试、现网 `curl` 探针与 Worker 重新部署，验证新链路已经可用。
- 同步更新部署文档、进度记录与计划状态，确保后续关停 `workers.dev` 时不会再丢上下文。

### 执行步骤
1. 查阅 Cloudflare 官方文档，核对 Service Token 的请求头格式、Worker 回源 Access 受保护源站的推荐做法，以及 `workers_dev` / `preview_urls` 的配置项。
2. 审查当前 Worker 代理与测试，设计最小改动：为后端回源请求增加可选的 `CF-Access-Client-Id` / `CF-Access-Client-Secret` 透传能力，但不把凭证暴露给浏览器。
3. 修改 Worker 代码、测试与示例环境变量；同时更新 README / 部署文档，明确哪些调用方需要凭证、哪些不需要。
4. 运行前端 `npm test`、`npm run lint`、`npm run build`，确认改动没有打断现有代理、静态资源与 R2 产物逻辑。
5. 使用真实 Service Token 对现网受 Access 保护入口做最小化 `curl` 验证；若回源配置已就绪，再重新部署 Worker 并做线上探针。
6. 更新 `PROGRESS.md` 记录结论与注意事项，并按仓库要求提交。

### 当前状态
- 已完成：已用 Context7 与 Cloudflare 官方文档核对 Service Token 的标准请求头为 `CF-Access-Client-Id` 与 `CF-Access-Client-Secret`。
- 已完成：已确认当前前端浏览器访问 `classflow.zjhstudio.com` 走的是“人类登录 Access”路径，不应把 Service Token 发到浏览器端。
- 已完成：已确认当前仓库中的 Worker 代理尚未向后端回源请求附带 Access Service Token，因此一旦后端 Tunnel 域名也被 Access 保护，现有 Worker 会直接失效。
- 已完成：已修改 Worker 代理与测试，并补充“浏览器 / Worker / smartclass 脚本”三类调用方的使用边界说明；前端本地验证已通过 `npm test`（17 项）、`npm run lint`、`npm run build`。
- 已完成：已把新的 `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` 写入 Worker secret，并重新部署到 `https://classflow-web.zhangjiahe0830.workers.dev`，当前版本号 `fe03c7e2-42ff-4a06-9e6d-93996c9db66c`。
- 已完成：现网探针已确认 `classflow-web.zhangjiahe0830.workers.dev` 现在也会被 Cloudflare Access 重定向到登录页，说明 `workers.dev` 入口已不再匿名裸奔。
- 已完成：带真实 Service Token 访问 `classflow.zjhstudio.com` 与 `classflow-web.zhangjiahe0830.workers.dev` 仍返回 `302`，且返回元数据里 `service_token_status=false`；这说明 Token 虽已创建，但尚未被挂到对应 Access 应用的 `Service Auth` 策略中，或挂到了错误应用。
