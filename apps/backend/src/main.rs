/*!
该文件只保留“程序入口”这一件事：

1. 初始化日志。
2. 从环境变量读取配置。
3. 构建数据库、对象存储、任务执行器与 HTTP 路由。
4. 启动后台任务 worker，并托管 axum 服务。

之所以把真正的业务逻辑放到 `lib.rs` 里，是为了后续测试时可以直接复用同一套构建函数，
避免把逻辑写死在 `main` 导致集成测试很难启动完整应用。
*/

use backend::{AppConfig, run_server};

#[tokio::main]
async fn main() -> Result<(), backend::AppError> {
    let config = AppConfig::from_env()?;
    run_server(config).await
}
