# ExecPlan

## 目标
- 在 `ClassFlow` 仓库中实现第一阶段完整方案：Rust 后端、React + Cloudflare Worker 前端、以及与 `smartclass-downloader` 的联动改造。

## 执行步骤
1. 初始化主仓库基础文件，建立 `PLANS.md`、`PROGRESS.md`、`.gitignore` 等执行基线。
2. 搭建 Rust 后端工作区与核心模块，先打通配置、数据模型、API、SQLite 持久层、任务执行骨架。
3. 为后端补全单元测试、集成测试与 mock 端到端测试。
4. 搭建 React + Cloudflare Worker 前端，完成任务台、课程库、代理 API 与测试。
5. 克隆并改造 `smartclass-downloader`，增加“推送到 ClassFlow 后端”模式，并补上脚本测试。
6. 整理部署文档、systemd 示例、cloudflared 示例，并完成最终验收测试。

## 当前决策
- 后端使用 `axum + tokio + sqlx(sqlite) + reqwest`。
- 前端使用 Cloudflare 官方 React Workers 脚手架方向，浏览器只访问 Worker，不直接访问后端。
- 课程归组键固定为 `学期 + 日期 + 课程名 + 老师`。
- 第一版只报告已收片段数，不启发式推断课程理论节数。
