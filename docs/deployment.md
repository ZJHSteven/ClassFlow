# ClassFlow 部署说明

本文档按“校园机后端 + Cloudflare Worker 前端代理 + cloudflared Tunnel 暴露后端”的目标环境来写。

## 0. 当前这台校园机的真实部署位置

以下路径是 2026-04-03 这台校园机已经验证过、当前正在使用的真实位置，后续排障优先以这里为准：

- 仓库根目录：`/home/zjhsteven/ClassFlow`
- 后端可执行文件：`/home/zjhsteven/ClassFlow/target/release/backend`
- 系统级 systemd 单元：`/etc/systemd/system/classflow-backend.service`
- 后端环境变量文件：`/etc/classflow/backend.env`
- 当前后端监听地址：`127.0.0.1:8787`
- SQLite 数据库：`/home/zjhsteven/.local/state/classflow/data/classflow.db`
- 临时工作根目录：`/home/zjhsteven/.local/state/classflow/tmp`
- 任务临时工作目录：`/home/zjhsteven/.local/state/classflow/tmp/jobs`
- 本地产物目录：`/home/zjhsteven/.local/state/classflow/artifacts`
- cloudflared 系统级单元：`/etc/systemd/system/cloudflared.service`
- 旧的用户级后端单元：`/home/zjhsteven/.config/systemd/user/classflow-backend.service`

说明：

- 旧的用户级单元现在已经 `disable --now`，保留文件只是为了排查历史，不再参与开机托管。
- 当前系统级后端服务以 `zjhsteven` 用户身份运行，但由 PID 1 管理，因此不再依赖 SSH / VS Code Remote 会话是否在线。
- 当前机器上的数据库、临时目录、产物目录都继续沿用用户目录下的既有真实路径，没有因为切换到系统级托管而迁库。
- 查看后端日志请优先使用 `journalctl -u classflow-backend.service`；查看 Tunnel 日志请使用 `journalctl -u cloudflared.service`。

## 1. 目录与角色

- `apps/backend`
  - Rust 后端，负责任务编排、SQLite、DashScope，以及通过 Worker 私有接口写入最终产物
- `apps/web`
  - React 管理前端 + Cloudflare Worker 代理 + Worker 绑定 R2
- `deploy/systemd`
  - 后端常驻与临时目录清理的 systemd 模板
- `deploy/cloudflared`
  - Tunnel ingress 示例

## 2. 机器前置依赖

校园机上至少需要：

```bash
sudo apt-get update
sudo apt-get install -y ffmpeg sqlite3 cloudflared aria2
rustup default stable
```

前端构建与 Worker 发布机器上至少需要：

```bash
node --version
npm --version
npx wrangler whoami
```

说明：

- `ffmpeg` 是后端从 MP4 抽音频的硬依赖，没有它就不可能进入 DashScope。
- `aria2` 是视频下载器依赖；当前后端已经切到 `aria2c`，没有它就无法执行下载阶段。
- `cloudflared` 负责把校园机本地 `127.0.0.1:8787` 暴露到公网。
- `wrangler whoami` 用来确认 Cloudflare 账号已登录；如果没登录，先执行 `npx wrangler login`。

## 3. 后端环境变量

1. 复制 [env.example](/home/zjhsteven/ClassFlow/apps/backend/env.example) 到服务器：
   - 推荐路径：`/etc/classflow/backend.env`
2. 至少填好这些关键项：
   - `CLASSFLOW_BEARER_TOKEN`
   - `DASHSCOPE_API_KEY`
   - `CLASSFLOW_DASHSCOPE_MODEL`
   - `CLASSFLOW_ARTIFACT_STORE_MODE`
   - 下载 / 上传稳健性相关参数至少要确认：
     - `CLASSFLOW_DOWNLOAD_CONCURRENCY`
     - `CLASSFLOW_UPLOAD_CONCURRENCY`
     - `CLASSFLOW_TRANSCRIBE_CONCURRENCY`
     - `CLASSFLOW_ARIA2_BIN`
   - 若使用 `worker` 模式，则还要填：
     - `CLASSFLOW_ARTIFACT_PROXY_BASE_URL`
     - `CLASSFLOW_ARTIFACT_PROXY_TOKEN`
     - 如果这个 Worker 域名已经被 Cloudflare Access 保护，还要填 `CLASSFLOW_ARTIFACT_PROXY_ACCESS_CLIENT_ID` / `CLASSFLOW_ARTIFACT_PROXY_ACCESS_CLIENT_SECRET`
   - 若使用 `r2` 直连模式，则还要填：
     - `CLASSFLOW_R2_BUCKET`
     - `CLASSFLOW_R2_ENDPOINT`
     - `CLASSFLOW_R2_ACCESS_KEY_ID`
     - `CLASSFLOW_R2_SECRET_ACCESS_KEY`

说明：

- `CLASSFLOW_BEARER_TOKEN` 是后端真正校验的共享鉴权值。
- Worker 的 `BACKEND_TOKEN` 必须与 `CLASSFLOW_BEARER_TOKEN` 完全一致。
- 语音转文字模型名就在 `/etc/classflow/backend.env` 的 `CLASSFLOW_DASHSCOPE_MODEL`。当前仓库默认值是 `fun-asr-mtl`；如果你想换成别的 DashScope 录音文件识别模型，直接改这个环境变量后重启后端即可，不需要改源码。
- 这个模型值会被后端同时用于两处：`GET /api/v1/uploads?action=getPolicy&model=...` 的临时上传凭证申请，以及 `POST /api/v1/services/audio/asr/transcription` 的异步转写提交。两处必须一致，否则 `oss://` 临时文件即使上传成功，也会在后续转写时失败。
- `CLASSFLOW_ARTIFACT_PROXY_TOKEN` 是“后端访问 Worker 私有产物接口”使用的单独密钥，不给浏览器、不写进 userscript。
- `CLASSFLOW_ARTIFACT_PROXY_ACCESS_CLIENT_ID` / `CLASSFLOW_ARTIFACT_PROXY_ACCESS_CLIENT_SECRET` 是“后端访问受 Cloudflare Access 保护的 Worker 私有产物接口”时使用的 Service Token。它们与 Worker 回源后端使用的 `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` 是同类凭证，但方向不同：前者是后端打 Worker，后者是 Worker 打后端。若你想少配一组变量名，后端也会兼容读取 `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`。
- 百炼官方临时 `oss://` 上传对象有效期为 48 小时；后端会用 47 小时作为安全复用窗口。超过窗口或历史数据缺少保存时间时，重试会重新上传音频，避免继续拿已过期的临时对象提交转写。
- 新版本把“上传并发”和“转写并发”拆开了；校园网弱上行环境下，建议先从 `2 / 2` 起步，不要再沿用旧的 `CLASSFLOW_DASHSCOPE_CONCURRENCY=8`。
- 下载链路现在依赖 `aria2c`，并支持断点续传、自动重试、连接超时，以及“按需启用”的低速退出；默认配置不会因为网速慢就主动失败，只有显式填写正数 `CLASSFLOW_DOWNLOAD_LOWEST_SPEED_LIMIT_BYTES` 时才会启用该阈值。推荐直接使用 [env.example](/home/zjhsteven/ClassFlow/apps/backend/env.example) 里的默认稳健参数起步。
- 如果 userscript 直连后端 Tunnel 域名，那么脚本里的 `Bearer Token` 也必须填这同一个值。
- 如果 userscript 访问的是 Worker 域名，则推荐让脚本侧 `Bearer Token` 留空，由 Worker 代为补上。

## 4. 构建并运行后端

```bash
cd /opt/classflow
cargo build --release --manifest-path apps/backend/Cargo.toml
```

如果先做本机验收，执行：

```bash
cargo fmt --check --manifest-path apps/backend/Cargo.toml
cargo clippy --manifest-path apps/backend/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path apps/backend/Cargo.toml
```

## 5. 安装 systemd

1. 拷贝模板：
   - [classflow-backend.service](/home/zjhsteven/ClassFlow/deploy/systemd/classflow-backend.service)
   - [classflow-cleanup.service](/home/zjhsteven/ClassFlow/deploy/systemd/classflow-cleanup.service)
   - [classflow-cleanup.timer](/home/zjhsteven/ClassFlow/deploy/systemd/classflow-cleanup.timer)
2. 拷贝清理脚本：
   - [classflow-cleanup-temp.sh](/home/zjhsteven/ClassFlow/scripts/classflow-cleanup-temp.sh)
3. 根据服务器真实路径修改：
   - `WorkingDirectory`
   - `ExecStart`
   - `ReadWritePaths`
   - `User` / `Group`

启用命令：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now classflow-backend.service
sudo systemctl enable --now classflow-cleanup.timer
sudo systemctl status classflow-backend.service
sudo systemctl status classflow-cleanup.timer
```

### 当前这台机器已经落地的系统级单元

本机当前安装的是系统级单元 `/etc/systemd/system/classflow-backend.service`，内容等价于：

```ini
[Unit]
Description=ClassFlow Backend
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=zjhsteven
Group=zjhsteven
WorkingDirectory=/home/zjhsteven/ClassFlow
EnvironmentFile=/etc/classflow/backend.env
ExecStart=/home/zjhsteven/ClassFlow/target/release/backend
Restart=always
RestartSec=3
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

与仓库模板的差异：

- 仓库模板假设使用 `/opt/classflow` 与独立 `classflow` 用户，更适合全新机器的标准化部署。
- 当前校园机为了最快止损，先继续沿用既有仓库目录 `/home/zjhsteven/ClassFlow` 与既有数据目录 `/home/zjhsteven/.local/state/classflow/*`。
- 这样做的核心好处是“不迁数据库、不迁临时目录、不改 Worker / Tunnel 配置”，只把后端进程的托管层级从 `systemd --user` 改成系统级 `systemd`。

当前机器可直接使用的运维命令：

```bash
sudo systemctl status classflow-backend.service
sudo systemctl restart classflow-backend.service
sudo systemctl stop classflow-backend.service
sudo systemctl disable classflow-backend.service
journalctl -u classflow-backend.service -n 200 --no-pager
curl http://127.0.0.1:8787/api/v1/health
curl https://classflow-backend.zjhstudio.com/api/v1/health
```

## 6. 配置 cloudflared Tunnel

示例见 [config.example.yml](/home/zjhsteven/ClassFlow/deploy/cloudflared/config.example.yml)。

核心思路：

- Tunnel 公网域名，例如：`classflow-backend.example.com`
- 本机回源地址：`http://127.0.0.1:8787`

完成后检查：

```bash
curl https://classflow-backend.example.com/api/v1/health
```

如果响应 `{"status":"ok","service":"classflow-backend"}`，说明 Tunnel 已打通。

## 7. 构建并发布 Cloudflare Worker 前端

先在 [apps/web/.dev.vars.example](/home/zjhsteven/ClassFlow/apps/web/.dev.vars.example) 基础上准备本地变量：

```bash
cd /opt/classflow/apps/web
cp .dev.vars.example .dev.vars
```

本地开发：

```bash
npm install
npm run lint
npm test
npm run build
npx wrangler dev
```

发布前，把 Worker 需要的 secret 写进去：

```bash
npx wrangler secret put BACKEND_BASE_URL
npx wrangler secret put BACKEND_TOKEN
npx wrangler secret put CF_ACCESS_CLIENT_ID
npx wrangler secret put CF_ACCESS_CLIENT_SECRET
npx wrangler secret put ARTIFACT_PROXY_TOKEN
```

填写规则：

- `BACKEND_BASE_URL`
  - 填 Tunnel 暴露出来的后端地址，例如 `https://classflow-backend.example.com`
- `BACKEND_TOKEN`
  - 必须与后端环境变量 `CLASSFLOW_BEARER_TOKEN` 完全一致
- `ARTIFACT_PROXY_TOKEN`
  - 必须与后端环境变量 `CLASSFLOW_ARTIFACT_PROXY_TOKEN` 完全一致
- `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`
  - 只有当 `BACKEND_BASE_URL` 指向的后端 Tunnel 域名已经被 Cloudflare Access 保护时才需要填写
  - 它们只应该存在于 Worker secret 中，不应该发给浏览器或 userscript

另外，`wrangler.toml` 里需要把 `ARTIFACTS` 绑定到真实的 R2 bucket。当前仓库里提供的是 `binding` 名称和示例 bucket 名，正式部署前请改成你自己的 bucket。

然后发布：

```bash
npm run build
npx wrangler deploy
```

## 8. Worker 代理规则

浏览器只访问 Worker 域名。

- `/api/*`
  - Worker 自动追加 `Authorization: Bearer <BACKEND_TOKEN>`
  - 转发到 `BACKEND_BASE_URL`
- 其它路径
  - 直接走 `ASSETS` 静态资源绑定

这样浏览器不会直接接触校园机后端密钥，也不会暴露 Tunnel 实际地址。

补充建议：

- React 前端天然就是访问 Worker，所以浏览器侧不需要知道后端真实 token。
- 如果后端 Tunnel 又额外接入了 Access，也只需要让 Worker 持有 Access Service Token；浏览器仍不应该看到这两个值。
- userscript 也推荐把“ClassFlow 后端地址”填成 Worker 域名，例如 `https://classflow-web.<your-subdomain>.workers.dev` 或你绑定的前端域名。
- 当 userscript 走 Worker 时，脚本里的 `Bearer Token` 可以留空；只有脚本直连后端 Tunnel 域名时，才需要手工填写 `CLASSFLOW_BEARER_TOKEN`。
- 不建议把 Cloudflare Access Service Token 写进 userscript。若脚本访问的是受 Access 保护的 Worker / 前端域名，应优先复用浏览器已登录的 Access 会话。

## 8.1 Cloudflare Access 收口步骤

当前现网状态需要特别注意：

- `classflow.zjhstudio.com` 已经被 Cloudflare Access 保护，未登录访问会跳转到 Access 登录页。
- `classflow-web.zhangjiahe0830.workers.dev` 现已被 Cloudflare Access 保护，未登录访问同样会跳转到 Access 登录页。
- `classflow-backend.zjhstudio.com` 当前没有被 Access 挡在最外层，实际保护依赖的是应用自己的 `CLASSFLOW_BEARER_TOKEN`。

建议的收口顺序不是“立刻关域名”，而是先把自动化访问路径搭好，再逐步收紧：

### A. 在 Zero Trust 里准备两个 Access 应用

1. 打开 Cloudflare Zero Trust Dashboard。
2. 进入 `Access -> Applications`。
3. 新建第一个 `Self-hosted` 应用：
   - 应用名可填：`ClassFlow Frontend`
   - 域名填：`classflow.zjhstudio.com`
   - Path 留空，表示整站都受保护。
4. 如果你准备连 `workers.dev` 也一起保护，再新建第二个 `Self-hosted` 应用：
   - 应用名可填：`ClassFlow WorkersDev`
   - 域名填：`classflow-web.zhangjiahe0830.workers.dev`
   - Path 留空。
5. 如果你准备把后端 Tunnel 域名也纳入 Access，再新建第三个 `Self-hosted` 应用：
   - 应用名可填：`ClassFlow Backend`
   - 域名填：`classflow-backend.zjhstudio.com`
   - Path 留空。

### B. 给“人访问”配置正常登录策略

在每个 Access 应用里增加一条浏览器用策略：

- Action：`Allow`
- Rules：按你的常用登录方式选择，例如邮箱、GitHub、Google、One-time PIN 等

这条策略是给你自己在浏览器里正常登录看的。

### C. 给“Worker / 自动化 / AI 测试”配置 Service Token

1. 在 Zero Trust Dashboard 进入 `Access -> Service Auth -> Service Tokens`。
2. 创建一个新的 Service Token，例如命名为 `classflow-automation`。
3. 记下两项值：
   - `Client ID`
   - `Client Secret`
4. 回到刚才的 Access 应用，在策略里新增一条自动化策略：
   - Action：`Service Auth`
   - Include：选择刚刚创建的 `classflow-automation`

之后，自动化请求访问受 Access 保护的域名时，要额外带这两个头：

```text
CF-Access-Client-Id: <client-id>
CF-Access-Client-Secret: <client-secret>
```

### D. 在 ClassFlow 架构里怎么落地

推荐分成两步，不要一步到位同时改三层：

1. 先保护前端入口：
   - 先把 `classflow.zjhstudio.com` 与 `classflow-web.zhangjiahe0830.workers.dev` 都接入 Access。
   - 浏览器通过正常登录访问。
   - AI / curl / 自动化测试通过 Service Token 访问。
2. 再保护后端 Tunnel 域名：
   - 给 `classflow-backend.zjhstudio.com` 加 Access。
   - 但在 Worker 里增加对 `CF-Access-Client-Id` / `CF-Access-Client-Secret` 的转发支持后，再正式启用。
   - 否则 Worker 当前只会带 `Authorization: Bearer <BACKEND_TOKEN>`，一旦后端域名被 Access 拦住，前端代理会直接失效。

### E. 测试与自动化阶段的建议

- 浏览器人工测试：
  - 直接登录 Access。
- 本机开发测试：
  - 优先测 `http://127.0.0.1:8787` 或 `wrangler dev`，不经过 Access。
- 远端自动化测试：
  - 统一走 Access Service Token。
  - 不建议继续依赖“留一个裸奔入口”作为长期方案。

### F. 后续准备关闭 `workers.dev` 时

- 需要先确认你的自定义前端域名与自动化 Service Token 路径都可用。
- 然后在 `wrangler.toml` 里显式加入 `workers_dev = false`，避免只在面板里关闭却被后续部署重新打开。

## 9. R2 目录约定

Worker 绑定的 R2 里，最终文本成品沿用以下布局：

```text
<semester>/<course_name>/<date>-<teacher_name>/manifest.json
<semester>/<course_name>/<date>-<teacher_name>/segments/<start>-<end>.md
<semester>/<course_name>/<date>-<teacher_name>/segments/<start>-<end>.json
<semester>/<course_name>/<date>-<teacher_name>/merged/course.md
```

## 10. 最终联调顺序

1. 后端本机健康检查通过：
   - `curl http://127.0.0.1:8787/api/v1/health`
2. Tunnel 健康检查通过：
   - `curl https://classflow-backend.example.com/api/v1/health`
3. Worker 本地构建与测试通过：
   - `npm run lint`
   - `npm test`
   - `npm run build`
4. userscript 已配置：
   - `ClassFlow 后端地址`
   - 如果填的是 Worker 域名：`Bearer Token` 可留空
   - 如果填的是后端 Tunnel 域名：`Bearer Token` 必须填 `CLASSFLOW_BEARER_TOKEN`
   - `默认学期`
5. 在智慧课堂页面切到 `ClassFlow` 模式，点击“提交当天全部”。
6. 打开前端 `任务台` 与 `课程库`，观察任务推进和总稿生成。
