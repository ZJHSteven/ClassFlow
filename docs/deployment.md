# ClassFlow 部署说明

本文档按“校园机后端 + Cloudflare Worker 前端代理 + cloudflared Tunnel 暴露后端”的目标环境来写。

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
sudo apt-get install -y ffmpeg sqlite3 cloudflared
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
- `cloudflared` 负责把校园机本地 `127.0.0.1:8787` 暴露到公网。
- `wrangler whoami` 用来确认 Cloudflare 账号已登录；如果没登录，先执行 `npx wrangler login`。

## 3. 后端环境变量

1. 复制 [env.example](/home/zjhsteven/ClassFlow/apps/backend/env.example) 到服务器：
   - 推荐路径：`/etc/classflow/backend.env`
2. 至少填好这些关键项：
   - `CLASSFLOW_BEARER_TOKEN`
   - `DASHSCOPE_API_KEY`
   - `CLASSFLOW_ARTIFACT_STORE_MODE`
   - 若使用 `worker` 模式，则还要填：
     - `CLASSFLOW_ARTIFACT_PROXY_BASE_URL`
     - `CLASSFLOW_ARTIFACT_PROXY_TOKEN`
   - 若使用 `r2` 直连模式，则还要填：
     - `CLASSFLOW_R2_BUCKET`
     - `CLASSFLOW_R2_ENDPOINT`
     - `CLASSFLOW_R2_ACCESS_KEY_ID`
     - `CLASSFLOW_R2_SECRET_ACCESS_KEY`

说明：

- `CLASSFLOW_BEARER_TOKEN` 是后端真正校验的共享鉴权值。
- Worker 的 `BACKEND_TOKEN` 必须与 `CLASSFLOW_BEARER_TOKEN` 完全一致。
- `CLASSFLOW_ARTIFACT_PROXY_TOKEN` 是“后端访问 Worker 私有产物接口”使用的单独密钥，不给浏览器、不写进 userscript。
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

发布前，把 Worker 需要的三个变量写成 secret：

```bash
npx wrangler secret put BACKEND_BASE_URL
npx wrangler secret put BACKEND_TOKEN
npx wrangler secret put ARTIFACT_PROXY_TOKEN
```

填写规则：

- `BACKEND_BASE_URL`
  - 填 Tunnel 暴露出来的后端地址，例如 `https://classflow-backend.example.com`
- `BACKEND_TOKEN`
  - 必须与后端环境变量 `CLASSFLOW_BEARER_TOKEN` 完全一致
- `ARTIFACT_PROXY_TOKEN`
  - 必须与后端环境变量 `CLASSFLOW_ARTIFACT_PROXY_TOKEN` 完全一致

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
- userscript 也推荐把“ClassFlow 后端地址”填成 Worker 域名，例如 `https://classflow-web.<your-subdomain>.workers.dev` 或你绑定的前端域名。
- 当 userscript 走 Worker 时，脚本里的 `Bearer Token` 可以留空；只有脚本直连后端 Tunnel 域名时，才需要手工填写 `CLASSFLOW_BEARER_TOKEN`。

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
