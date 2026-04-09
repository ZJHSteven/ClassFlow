# ClassFlow Web

`apps/web` 同时包含两部分：

- React 管理界面
- Cloudflare Worker 代理入口

浏览器永远只访问 Worker，不直接访问校园机后端。Worker 负责：

- 为 `/api/*` 请求追加 Bearer Token
- 若后端 Tunnel 域名接入了 Cloudflare Access，则再追加 Service Token 请求头
- 转发到 cloudflared 暴露出来的后端地址
- 回退到 `ASSETS` 绑定提供静态资源

## 常用命令

```bash
npm install
npm run lint
npm test
npm run build
npx wrangler dev
npx wrangler deploy
```

## 本地变量

复制 [`.dev.vars.example`](/home/zjhsteven/ClassFlow/apps/web/.dev.vars.example) 为 `.dev.vars`：

```bash
cp .dev.vars.example .dev.vars
```

需要提供：

- `BACKEND_BASE_URL`
- `BACKEND_TOKEN`
- 若后端已接入 Access：`CF_ACCESS_CLIENT_ID`、`CF_ACCESS_CLIENT_SECRET`

## 发布前检查

```bash
npm run lint
npm test
npm run build
npx wrangler whoami
```

说明：

- 浏览器前端自己不需要持有 Cloudflare Access Service Token；人类用户应直接登录 Access。
- 只有 Worker 回源“已被 Access 保护”的后端 Tunnel 域名时，才需要在 Worker secret 中配置 `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`。
- 若 `smartclass` 脚本访问的是 Worker / 前端域名，也不建议把 Service Token 塞进脚本里；应优先复用浏览器已经完成的 Access 登录态。
