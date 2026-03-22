/*!
`backend` crate 是 ClassFlow 第一阶段后端的主入口。

模块划分遵循“主流程清晰、细节下沉”的原则：

1. `config`：环境变量与运行参数。
2. `models`：接口结构、数据库实体、业务值对象。
3. `repository`：SQLite 持久层。
4. `pipeline`：视频下载、FFmpeg 抽音频、百炼上传与转写。
5. `artifacts`：课程产物上传与读取（本地目录 / R2）。
6. `course`：课程聚合、路径生成、Markdown/Manifest 构建。
7. `app` / `routes` / `worker`：HTTP 服务与后台任务调度。

这样做的好处是：

1. HTTP 层不用关心外部 API 细节。
2. 测试可以分别替换“数据库 / 流水线 / 对象存储”。
3. 后续要把本地存储切到 R2、或把 mock 流水线换成真实服务时，不需要重写接口层。
*/

pub mod app;
pub mod artifacts;
pub mod config;
pub mod course;
pub mod error;
pub mod models;
pub mod pipeline;
pub mod repository;
pub mod routes;
pub mod worker;

pub use app::{AppState, build_app, run_server};
pub use config::AppConfig;
pub use error::{AppError, AppResult};
