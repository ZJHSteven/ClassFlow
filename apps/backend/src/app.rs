/*!
应用装配层。

该模块把前面各层真正组装到一起：

1. 创建仓储、对象存储、流水线实现与 worker。
2. 恢复重启前未完成任务。
3. 启动 axum 服务，并挂载优雅关闭。
*/

use std::sync::Arc;

use axum::Router;
use tokio::signal;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing::{info, warn};

use crate::{
    artifacts::{ArtifactStore, build_artifact_store},
    config::AppConfig,
    error::AppResult,
    pipeline::{PipelineIo, RealPipelineIo},
    repository::Repository,
    routes::build_router,
    worker::{TaskQueue, cleanup_stale_temp_dirs, spawn_workers},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub repo: Repository,
    pub artifact_store: Arc<dyn ArtifactStore>,
    pub pipeline: Arc<dyn PipelineIo>,
    pub queue: TaskQueue,
}

pub async fn run_server(config: AppConfig) -> AppResult<()> {
    init_tracing();
    let state = build_state(config).await?;
    let router = build_app(state.clone());

    let recoverable_task_ids = state.repo.list_recoverable_task_ids().await?;
    for task_id in recoverable_task_ids {
        if let Err(error) = state.queue.enqueue(task_id.clone()) {
            warn!("恢复任务重新入队失败: task_id={}, error={}", task_id, error);
        }
    }

    let temp_jobs_root = state.config.temp_root.join("jobs");
    let removed = cleanup_stale_temp_dirs(&temp_jobs_root, state.config.cleanup_hours).await?;
    if removed > 0 {
        info!("启动时已清理过期临时目录数量: {}", removed);
    }

    let listener = tokio::net::TcpListener::bind(state.config.bind_addr).await?;
    info!("ClassFlow backend 正在监听 {}", state.config.bind_addr);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| crate::error::AppError::Internal(format!("HTTP 服务异常退出: {error}")))?;

    Ok(())
}

pub fn build_app(state: AppState) -> Router {
    build_router(state).layer(TraceLayer::new_for_http()).layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any),
    )
}

pub async fn build_state(config: AppConfig) -> AppResult<AppState> {
    tokio::fs::create_dir_all(config.temp_root.join("jobs")).await?;
    tokio::fs::create_dir_all(&config.local_artifact_root).await?;
    tokio::fs::create_dir_all("./data").await?;

    let repo = Repository::connect(&config.db_url).await?;
    let artifact_store = build_artifact_store(&config).await?;
    let pipeline: Arc<dyn PipelineIo> = Arc::new(RealPipelineIo::new(config.clone()));

    let placeholder_queue = spawn_workers_placeholder();
    let mut state = AppState {
        config: Arc::new(config.clone()),
        repo,
        artifact_store,
        pipeline,
        queue: placeholder_queue,
    };

    let queue = spawn_workers(state.clone(), config.task_worker_count);
    state.queue = queue;
    Ok(state)
}

fn spawn_workers_placeholder() -> TaskQueue {
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    TaskQueue { sender }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "backend=info,tower_http=info".into()),
        )
        .try_init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = signal::ctrl_c().await {
            warn!("监听 Ctrl+C 失败: {}", error);
        }
    };

    #[cfg(unix)]
    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("应该可以监听 SIGTERM");
        signal.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
