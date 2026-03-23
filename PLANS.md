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

## 当前决策
- 后端使用 `axum + tokio + sqlx(sqlite) + reqwest`。
- 前端使用 Cloudflare 官方 React Workers 脚手架方向，浏览器只访问 Worker，不直接访问后端。
- 课程归组键固定为 `学期 + 日期 + 课程名 + 老师`。
- 第一版只报告已收片段数，不启发式推断课程理论节数。
- 后端公网出口使用 `classflow-backend.zjhstudio.com` 固定域名；Quick Tunnel 仅保留为应急调试手段。
- 前端默认关闭自动轮询，优先降低闪烁与 Worker 请求次数。
