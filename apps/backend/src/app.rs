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
use tokio::sync::broadcast;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing::{info, warn};

use crate::{
    artifacts::{ArtifactStore, build_artifact_store},
    asr::{AsrProvider, DashScopeAsrProvider, LocalQwenAsrProvider, RemoteCapsWriterAsrProvider},
    config::AppConfig,
    error::AppResult,
    network::NetworkHealthGate,
    notes::spawn_notes_worker,
    notifications::spawn_notification_worker,
    pipeline::{PipelineIo, RealPipelineIo},
    repository::Repository,
    routes::build_router,
    schedule::CourseSchedule,
    smartclass::spawn_smartclass_collector,
    smartclass_session::SmartClassSessionManager,
    worker::{TaskQueue, cleanup_stale_temp_dirs, spawn_workers},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub repo: Repository,
    pub artifact_store: Arc<dyn ArtifactStore>,
    pub pipeline: Arc<dyn PipelineIo>,
    pub asr_provider: Arc<dyn AsrProvider>,
    pub network_health: Arc<NetworkHealthGate>,
    pub course_schedule: Arc<CourseSchedule>,
    /// SmartClass 会话管理器；browser_bridge 模式下为空。
    pub smartclass_session: Option<Arc<SmartClassSessionManager>>,
    pub queue: TaskQueue,
    pub task_list_events: broadcast::Sender<()>,
}

impl AppState {
    /**
     * 向任务台订阅端广播“任务列表可能已经变化”。
     *
     * 这里故意只发送一个空信号，而不直接把完整任务数组塞进广播：
     *
     * 1. 广播频道主要承担“唤醒 SSE 推送协程”的职责，不负责长期缓存大对象。
     * 2. 真实任务列表仍以数据库为准，SSE 端每次收到信号后再按当前筛选条件重新查询。
     * 3. 即使短时间内连续触发多次更新，接收端也只需要知道“有变化了”，不需要逐条回放。
     */
    pub fn notify_task_list_changed(&self) {
        let _ = self.task_list_events.send(());
    }

    /**
     * 为每个 SSE 客户端创建独立的广播接收端。
     *
     * 这样每个浏览器页签都能按自己的连接生命周期消费更新，不会互相干扰。
     */
    pub fn subscribe_task_list_events(&self) -> broadcast::Receiver<()> {
        self.task_list_events.subscribe()
    }
}

pub async fn run_server(config: AppConfig) -> AppResult<()> {
    init_tracing();
    let state = build_state(config).await?;
    let router = build_app(state.clone());

    state
        .repo
        .prune_task_events(
            state.config.task_event_retention_days,
            state.config.task_event_retention_rows_per_task,
        )
        .await?;

    let temp_jobs_root = state.config.temp_root.join("jobs");
    // 启动清理必须发生在 recoverable task 重新入队之前，否则 cleaner 可能与刚启动的
    // worker 同时操作同一 work_dir。清理器会查询 SQLite，只删除真正无引用/可重建的
    // 陈旧缓存，并保留仍被任务引用的 M4A checkpoint。
    let removed =
        cleanup_stale_temp_dirs(&temp_jobs_root, state.config.cleanup_hours, &state.repo).await?;
    if removed > 0 {
        info!("启动时已清理过期临时目录数量: {}", removed);
    }

    // 上一次进程被停止时，SQLite 里可能仍有 status=running 和最后一帧下载速度。
    // 先把这些“瞬时运行态”归一化成 queued，再统一重新入队，避免 UI 显示僵尸速度。
    let normalized_running = state.repo.normalize_interrupted_running_tasks().await?;
    if normalized_running > 0 {
        info!("启动恢复时已归一化中断任务数量: {}", normalized_running);
    }

    let recoverable_task_ids = state.repo.list_recoverable_task_ids().await?;
    for task_id in recoverable_task_ids {
        if let Err(error) = state.queue.enqueue(task_id.clone()) {
            warn!("恢复任务重新入队失败: task_id={}, error={}", task_id, error);
        }
    }

    spawn_smartclass_collector(state.clone());
    spawn_notes_worker(state.clone());
    spawn_notification_worker(state.clone());

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
    let course_schedule =
        Arc::new(CourseSchedule::load_optional(&config.course_schedule_path).await?);
    if course_schedule.courses.is_empty() {
        warn!(
            "未加载课程表先验，Notes 将退回录像片段启发式: {}",
            config.course_schedule_path.display()
        );
    } else {
        info!(
            "已加载课程表先验: {} 门课程，来源 {}",
            course_schedule.courses.len(),
            config.course_schedule_path.display()
        );
    }
    let network_health = Arc::new(NetworkHealthGate::from_config(&config));
    let artifact_store = build_artifact_store(&config).await?;
    let artifact_store =
        crate::artifacts::NetworkAwareArtifactStore::wrap(artifact_store, network_health.clone());
    let pipeline: Arc<dyn PipelineIo> =
        Arc::new(RealPipelineIo::new(config.clone(), network_health.clone()));
    let config_arc = Arc::new(config.clone());
    let smartclass_session = match config
        .smartclass_transport
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "direct_http" => Some(Arc::new(
            SmartClassSessionManager::new(config_arc.clone()).await?,
        )),
        "browser_bridge" => None,
        other => {
            return Err(crate::error::AppError::Config(format!(
                "不支持的 CLASSFLOW_SMARTCLASS_TRANSPORT: {other}"
            )));
        }
    };
    let asr_provider: Arc<dyn AsrProvider> =
        match config.asr_provider.trim().to_lowercase().as_str() {
            "dashscope" => Arc::new(DashScopeAsrProvider::new(pipeline.clone())),
            "local_qwen" => Arc::new(LocalQwenAsrProvider::new(config_arc.clone())),
            "remote_capswriter" => Arc::new(RemoteCapsWriterAsrProvider::new(config_arc.clone())),
            other => {
                return Err(crate::error::AppError::Config(format!(
                    "不支持的 CLASSFLOW_ASR_PROVIDER: {other}"
                )));
            }
        };
    let (task_list_events, _) = broadcast::channel::<()>(256);

    let placeholder_queue = spawn_workers_placeholder();
    let mut state = AppState {
        config: config_arc,
        repo,
        artifact_store,
        pipeline,
        asr_provider,
        network_health,
        course_schedule,
        notes_submit_cooldown_until: Arc::new(Mutex::new(None)),
        smartclass_session,
        queue: placeholder_queue,
        task_list_events,
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

