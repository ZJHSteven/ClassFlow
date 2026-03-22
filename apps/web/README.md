# ClassFlow Web

`apps/web` 同时包含两部分：

- React 管理界面
- Cloudflare Worker 代理入口

浏览器永远只访问 Worker，不直接访问校园机后端。Worker 负责：

- 为 `/api/*` 请求追加 Bearer Token
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

## 发布前检查

```bash
npm run lint
npm test
npm run build
npx wrangler whoami
```
