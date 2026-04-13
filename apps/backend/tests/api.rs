/*!
这些集成测试直接启动完整的 axum 路由、SQLite 仓储、worker 与 mock 流水线。

测试目标不是只验证某个函数，而是验证“接口 -> 入库 -> 后台执行 -> 课程产物”的整条链路：

1. 鉴权是否生效。
2. intake 是否会真正触发后台任务。
3. 课程聚合与产物读取是否能拿到最终结果。
4. 失败任务是否能通过重试接口重新执行成功。
*/

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use backend::{
    AppConfig, AppError, AppState,
    app::build_app,
    artifacts::{ArtifactStore, LocalArtifactStore},
    config::ArtifactStoreMode,
    error::AppResult,
    models::{NormalizedTranscript, TaskListQuery, TaskStage, TaskStatus},
    pipeline::{PipelineIo, ProgressSink},
    repository::Repository,
    worker::{detached_queue, spawn_workers},
};
use chrono::{Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::ServiceExt;

#[derive(Default)]
struct MockPipelineCounters {
    download_calls: AtomicUsize,
    extract_calls: AtomicUsize,
    upload_calls: AtomicUsize,
    transcribe_calls: AtomicUsize,
}

#[derive(Clone)]
struct MockPipeline {
    counters: Arc<MockPipelineCounters>,
    fail_first_transcription: bool,
}

#[async_trait]
impl PipelineIo for MockPipeline {
    async fn download_video(
        &self,
        _url: &str,
        target_path: &std::path::Path,
        _progress_sink: Option<Arc<dyn ProgressSink>>,
    ) -> AppResult<()> {
        self.counters.download_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(parent) = target_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(target_path, b"fake-mp4").await?;
        Ok(())
    }

    async fn extract_audio(
        &self,
        _source_video_path: &std::path::Path,
        target_audio_path: &std::path::Path,
    ) -> AppResult<()> {
        self.counters.extract_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(parent) = target_audio_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(target_audio_path, b"fake-audio").await?;
        Ok(())
    }

    async fn upload_audio_for_transcription(
        &self,
        _audio_path: &std::path::Path,
        _progress_sink: Option<Arc<dyn ProgressSink>>,
    ) -> AppResult<String> {
        self.counters.upload_calls.fetch_add(1, Ordering::SeqCst);
        Ok("oss://mock/audio.wav".to_string())
    }

    async fn transcribe_file_url(&self, _file_url: &str) -> AppResult<NormalizedTranscript> {
        let current = self
            .counters
            .transcribe_calls
            .fetch_add(1, Ordering::SeqCst);
        if self.fail_first_transcription && current == 0 {
            return Err(AppError::External("模拟转写失败".to_string()));
        }

        Ok(NormalizedTranscript {
            text_display: "这是测试转写".to_string(),
            text_accu: "这是测试转写".to_string(),
            tokens: vec!["这".into(), "是".into(), "测".into(), "试".into()],
            timestamps: vec![0.0, 0.2, 0.4, 0.6],
            duration_seconds: 1.0,
            raw_task_output: json!({"mock": true}),
        })
    }

    async fn cleanup_dir(&self, dir_path: &std::path::Path) -> AppResult<()> {
        if dir_path.exists() {
            tokio::fs::remove_dir_all(dir_path).await?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct MockArtifactStoreCounters {
    put_calls: AtomicUsize,
}

struct MockArtifactStore {
    inner: LocalArtifactStore,
    counters: Arc<MockArtifactStoreCounters>,
    fail_first_put: bool,
}

#[async_trait]
impl ArtifactStore for MockArtifactStore {
    async fn put_bytes(&self, path: &str, content_type: &str, bytes: Vec<u8>) -> AppResult<()> {
        let current = self.counters.put_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_first_put && current == 0 {
            return Err(AppError::External("模拟产物写入失败".to_string()));
        }
        self.inner.put_bytes(path, content_type, bytes).await
    }

    async fn get_bytes(&self, path: &str) -> AppResult<backend::models::StoredObject> {
        self.inner.get_bytes(path).await
    }

    async fn delete(&self, path: &str) -> AppResult<()> {
        self.inner.delete(path).await
    }
}

async fn build_test_state(
    fail_first_transcription: bool,
    fail_first_artifact_put: bool,
) -> (AppState, tempfile::TempDir, Arc<MockPipelineCounters>) {
    let temp = tempdir().expect("临时目录应创建成功");
    let db_path = temp.path().join("classflow.db");
    let artifact_root = temp.path().join("artifacts");
    let temp_root = temp.path().join("tmp");

    let config = AppConfig {
        bind_addr: "127.0.0.1:0".parse().expect("测试地址应合法"),
        db_url: format!("sqlite://{}?mode=rwc", db_path.display()),
        bearer_token: "test-token".to_string(),
        default_semester: "2025-2026-2".to_string(),
        temp_root,
        local_artifact_root: artifact_root.clone(),
        task_worker_count: 1,
        download_concurrency: 1,
        upload_concurrency: 1,
        transcribe_concurrency: 1,
        r2_concurrency: 1,
        cleanup_hours: 24,
        artifact_store_mode: ArtifactStoreMode::Local,
        aria2_bin: "aria2c".to_string(),
        download_retry_attempts: 3,
        download_retry_wait_secs: 1.0,
        download_connect_timeout_secs: 5.0,
        download_timeout_secs: 30.0,
        download_split: 2,
        download_connections_per_server: 2,
        download_lowest_speed_limit_bytes: 1024,
        dashscope_api_key: String::new(),
        dashscope_model: "fun-asr-mtl".to_string(),
        dashscope_submit_url:
            "https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription".to_string(),
        dashscope_task_url_template: "https://dashscope.aliyuncs.com/api/v1/tasks/{task_id}"
            .to_string(),
        dashscope_upload_policy_url: "https://dashscope.aliyuncs.com/api/v1/uploads".to_string(),
        dashscope_request_timeout_secs: 30.0,
        dashscope_request_retry_attempts: 2,
        dashscope_request_retry_wait_secs: 0.1,
        upload_retry_attempts: 2,
        upload_retry_wait_secs: 0.1,
        upload_timeout_secs: 30.0,
        dashscope_poll_interval_secs: 1.0,
        dashscope_poll_timeout_secs: 60.0,
        r2_bucket: String::new(),
        r2_endpoint: String::new(),
        r2_access_key_id: String::new(),
        r2_secret_access_key: String::new(),
        r2_region: "auto".to_string(),
        artifact_proxy_base_url: String::new(),
        artifact_proxy_token: String::new(),
        artifact_proxy_access_client_id: String::new(),
        artifact_proxy_access_client_secret: String::new(),
        artifact_proxy_connect_timeout_secs: 10.0,
        artifact_proxy_timeout_secs: 30.0,
        artifact_proxy_retry_attempts: 2,
        artifact_proxy_retry_wait_secs: 0.1,
        task_event_retention_days: 30,
        task_event_retention_rows_per_task: 200,
    };

    let repo = Repository::connect(&config.db_url)
        .await
        .expect("测试数据库应连接成功");
    let pipeline_counters = Arc::new(MockPipelineCounters::default());
    let artifact_store: Arc<dyn ArtifactStore> = Arc::new(MockArtifactStore {
        inner: LocalArtifactStore::new(artifact_root),
        counters: Arc::new(MockArtifactStoreCounters::default()),
        fail_first_put: fail_first_artifact_put,
    });
    let pipeline: Arc<dyn PipelineIo> = Arc::new(MockPipeline {
        counters: pipeline_counters.clone(),
        fail_first_transcription,
    });

    let mut state = AppState {
        config: Arc::new(config.clone()),
        repo,
        artifact_store,
        pipeline,
        queue: detached_queue(),
        task_list_events: tokio::sync::broadcast::channel::<()>(64).0,
    };
    state.queue = spawn_workers(state.clone(), 1);
    (state, temp, pipeline_counters)
}

#[tokio::test]
async fn should_stream_task_snapshots_over_sse() {
    let (state, _temp, _counters) = build_test_state(false, false).await;
    let app = build_app(state.clone());

    let intake_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/intake/batches")
                .method("POST")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-token")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 请求应成功");
    assert_eq!(intake_response.status(), StatusCode::ACCEPTED);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/tasks/stream")
                .header("authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("SSE 请求应构造成功"),
        )
        .await
        .expect("SSE 请求应成功");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("应在超时前收到首帧")
        .expect("SSE body 不应提前结束")
        .expect("SSE 首帧读取应成功");
    let bytes = frame.into_data().expect("首帧应为数据帧");
    let text = String::from_utf8(bytes.to_vec()).expect("SSE 首帧应为 UTF-8");

    assert!(text.contains("event: tasks_snapshot"));
    assert!(text.contains("病理学"));
}

fn intake_request_json() -> Value {
    json!({
        "source": "userscript",
        "items": [{
            "new_id": "123",
            "page_url": "https://example.test/page",
            "mp4_url": "https://example.test/video.mp4",
            "course_name": "病理学",
            "teacher_name": "王老师",
            "date": "2026-03-20",
            "start_time": "08:00",
            "end_time": "08:45",
            "raw_title": "病理学 王老师 2026-03-20 08:00-08:45"
        }]
    })
}

async fn wait_for_condition(
    timeout: Duration,
    mut check: impl FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
) {
    let started = tokio::time::Instant::now();
    loop {
        if check().await {
            return;
        }

        assert!(
            started.elapsed() < timeout,
            "等待条件超时，超过 {:?}",
            timeout
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn health_should_not_require_auth() {
    let (state, _temp, _counters) = build_test_state(false, false).await;
    let app = build_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("健康检查请求应成功");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn protected_route_should_require_auth() {
    let (state, _temp, _counters) = build_test_state(false, false).await;
    let app = build_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/tasks")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("请求应返回响应");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn intake_should_create_task_and_artifacts() {
    let (state, _temp, _counters) = build_test_state(false, false).await;
    let repo = state.repo.clone();

    let intake_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/intake/batches")
                .header("Authorization", "Bearer test-token")
                .header("Content-Type", "application/json")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 请求应返回响应");

    assert_eq!(intake_response.status(), StatusCode::ACCEPTED);
    let payload: Value = serde_json::from_slice(
        &intake_response
            .into_body()
            .collect()
            .await
            .expect("响应体应可读取")
            .to_bytes(),
    )
    .expect("响应体应是合法 JSON");

    let task_id = payload["task_ids"][0]
        .as_str()
        .expect("应返回 task_id")
        .to_string();

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Succeeded)
                .unwrap_or(false)
        })
    })
    .await;

    let tasks = repo
        .list_tasks(&TaskListQuery {
            status: None,
            date: None,
            course_name: None,
        })
        .await
        .expect("查询任务应成功");
    let course_key = tasks[0].course_key.clone();
    let merged_markdown_path = tasks[0]
        .merged_markdown_path
        .clone()
        .expect("任务成功后应记录课程总稿路径");

    wait_for_condition(Duration::from_secs(3), || {
        let artifact_store = state.artifact_store.clone();
        let merged_markdown_path = merged_markdown_path.clone();
        Box::pin(async move {
            artifact_store
                .get_bytes(&merged_markdown_path)
                .await
                .is_ok()
        })
    })
    .await;

    let artifact_response = build_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/courses/{course_key}/artifacts/course.md"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("课程产物请求应成功");

    assert_eq!(artifact_response.status(), StatusCode::OK);
    let markdown_text = String::from_utf8(
        artifact_response
            .into_body()
            .collect()
            .await
            .expect("Markdown 响应体应可读取")
            .to_bytes()
            .to_vec(),
    )
    .expect("Markdown 应是合法 UTF-8");
    assert!(markdown_text.contains("课程总稿"));
    assert!(markdown_text.contains("这是测试转写"));
}

#[tokio::test]
async fn retry_should_requeue_failed_task() {
    let (state, _temp, counters) = build_test_state(true, false).await;
    let repo = state.repo.clone();

    let intake_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/intake/batches")
                .header("Authorization", "Bearer test-token")
                .header("Content-Type", "application/json")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 应返回响应");
    assert_eq!(intake_response.status(), StatusCode::ACCEPTED);

    let payload: Value = serde_json::from_slice(
        &intake_response
            .into_body()
            .collect()
            .await
            .expect("响应体应可读取")
            .to_bytes(),
    )
    .expect("响应体应是合法 JSON");
    let task_id = payload["task_ids"][0]
        .as_str()
        .expect("应返回 task_id")
        .to_string();

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Failed)
                .unwrap_or(false)
        })
    })
    .await;

    let failed_task = repo.get_task(&task_id).await.expect("失败任务应能查询成功");
    assert_eq!(
        failed_task.uploaded_source_url.as_deref(),
        Some("oss://mock/audio.wav"),
        "上传成功后应立刻持久化上传检查点"
    );
    assert!(
        failed_task.uploaded_source_url_saved_at.is_some(),
        "上传检查点必须同时记录保存时间，后续才能判断临时 OSS 是否过期"
    );
    assert!(
        failed_task.transcript_json.is_none(),
        "转写失败前不应提前写入转写检查点"
    );

    let retry_response = build_app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/tasks/{task_id}/retry"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("重试请求应返回响应");
    assert_eq!(retry_response.status(), StatusCode::ACCEPTED);

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Succeeded)
                .unwrap_or(false)
        })
    })
    .await;

    assert_eq!(
        counters.download_calls.load(Ordering::SeqCst),
        1,
        "转写失败后再次重试不应重新下载视频"
    );
    assert_eq!(
        counters.extract_calls.load(Ordering::SeqCst),
        1,
        "转写失败后再次重试不应重新抽取音频"
    );
    assert_eq!(
        counters.upload_calls.load(Ordering::SeqCst),
        1,
        "转写失败后再次重试不应重新上传音频"
    );
    assert_eq!(
        counters.transcribe_calls.load(Ordering::SeqCst),
        2,
        "转写失败后再次重试只应再次触发转写阶段"
    );
}

#[tokio::test]
async fn retry_should_reupload_expired_temporary_oss_checkpoint() {
    let (state, _temp, counters) = build_test_state(true, false).await;
    let repo = state.repo.clone();

    let intake_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/intake/batches")
                .header("Authorization", "Bearer test-token")
                .header("Content-Type", "application/json")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 应返回响应");
    assert_eq!(intake_response.status(), StatusCode::ACCEPTED);

    let payload: Value = serde_json::from_slice(
        &intake_response
            .into_body()
            .collect()
            .await
            .expect("响应体应可读取")
            .to_bytes(),
    )
    .expect("响应体应是合法 JSON");
    let task_id = payload["task_ids"][0]
        .as_str()
        .expect("应返回 task_id")
        .to_string();

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Failed)
                .unwrap_or(false)
        })
    })
    .await;

    let stale_saved_at = (Utc::now() - ChronoDuration::hours(49)).to_rfc3339();
    sqlx::query(
        "UPDATE tasks SET uploaded_source_url = ?, uploaded_source_url_saved_at = ? WHERE id = ?",
    )
    .bind("oss://mock/audio.wav")
    .bind(stale_saved_at)
    .bind(&task_id)
    .execute(repo.pool())
    .await
    .expect("测试应能模拟过期上传检查点");

    let retry_response = build_app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/tasks/{task_id}/retry"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("重试请求应返回响应");
    assert_eq!(retry_response.status(), StatusCode::ACCEPTED);

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Succeeded)
                .unwrap_or(false)
        })
    })
    .await;

    let retried_task = repo
        .get_task(&task_id)
        .await
        .expect("重试后任务应能查询成功");
    assert_eq!(retried_task.status, TaskStatus::Succeeded);
    assert_eq!(
        counters.upload_calls.load(Ordering::SeqCst),
        2,
        "临时 oss:// 上传检查点超过安全窗口后，重试必须重新上传音频"
    );
    assert_eq!(
        counters.transcribe_calls.load(Ordering::SeqCst),
        2,
        "重新上传后仍应继续执行转写阶段"
    );

    let detail = repo
        .get_task_detail(&task_id)
        .await
        .expect("任务详情应能查询成功");
    assert!(
        detail.events.iter().any(|event| {
            event.stage == TaskStage::UploadingAudio.as_str()
                && event.message.contains("重新上传音频")
        }),
        "事件日志应解释为什么这次没有继续复用旧 oss:// 检查点"
    );
}

#[tokio::test]
async fn should_download_task_level_artifacts() {
    let (state, _temp, _counters) = build_test_state(false, false).await;
    let repo = state.repo.clone();

    let intake_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/intake/batches")
                .header("Authorization", "Bearer test-token")
                .header("Content-Type", "application/json")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 请求应返回响应");

    let payload: Value = serde_json::from_slice(
        &intake_response
            .into_body()
            .collect()
            .await
            .expect("响应体应可读取")
            .to_bytes(),
    )
    .expect("响应体应是合法 JSON");
    let task_id = payload["task_ids"][0]
        .as_str()
        .expect("应返回 task_id")
        .to_string();

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Succeeded)
                .unwrap_or(false)
        })
    })
    .await;

    let segment_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/tasks/{task_id}/artifacts/segment.md"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("片段请求应成功");
    assert_eq!(segment_response.status(), StatusCode::OK);

    let events_response = build_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/tasks/{task_id}/artifacts/events.json"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("事件请求应成功");
    assert_eq!(events_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn should_delete_failed_task_and_work_dir() {
    let (state, _temp, _counters) = build_test_state(true, false).await;
    let repo = state.repo.clone();

    let intake_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/intake/batches")
                .header("Authorization", "Bearer test-token")
                .header("Content-Type", "application/json")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 应返回响应");
    assert_eq!(intake_response.status(), StatusCode::ACCEPTED);

    let payload: Value = serde_json::from_slice(
        &intake_response
            .into_body()
            .collect()
            .await
            .expect("响应体应可读取")
            .to_bytes(),
    )
    .expect("响应体应是合法 JSON");
    let task_id = payload["task_ids"][0]
        .as_str()
        .expect("应返回 task_id")
        .to_string();

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Failed)
                .unwrap_or(false)
        })
    })
    .await;

    let work_dir = state.config.temp_root.join("jobs").join(&task_id);
    assert!(work_dir.exists(), "失败任务的工作目录应保留下来");

    let delete_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/tasks/{task_id}"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("删除请求应返回响应");
    assert_eq!(delete_response.status(), StatusCode::OK);

    assert!(!work_dir.exists(), "删除失败任务后应清掉本地工作目录");
    let tasks = repo
        .list_tasks(&TaskListQuery {
            status: None,
            date: None,
            course_name: None,
        })
        .await
        .expect("查询任务应成功");
    assert!(tasks.is_empty(), "失败任务被删除后，任务列表应为空");
}

#[tokio::test]
async fn retry_should_resume_from_transcript_checkpoint_after_artifact_failure() {
    let (state, _temp, counters) = build_test_state(false, true).await;
    let repo = state.repo.clone();

    let intake_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/intake/batches")
                .header("Authorization", "Bearer test-token")
                .header("Content-Type", "application/json")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 应返回响应");
    assert_eq!(intake_response.status(), StatusCode::ACCEPTED);

    let payload: Value = serde_json::from_slice(
        &intake_response
            .into_body()
            .collect()
            .await
            .expect("响应体应可读取")
            .to_bytes(),
    )
    .expect("响应体应是合法 JSON");
    let task_id = payload["task_ids"][0]
        .as_str()
        .expect("应返回 task_id")
        .to_string();

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Failed)
                .unwrap_or(false)
        })
    })
    .await;

    let failed_task = repo.get_task(&task_id).await.expect("失败任务应能查询成功");
    assert!(
        failed_task.transcript_json.is_some(),
        "写产物失败前应已保存转写检查点"
    );

    let retry_response = build_app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/tasks/{task_id}/retry"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("重试请求应返回响应");
    assert_eq!(retry_response.status(), StatusCode::ACCEPTED);

    wait_for_condition(Duration::from_secs(3), || {
        let repo = repo.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            repo.get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Succeeded)
                .unwrap_or(false)
        })
    })
    .await;

    assert_eq!(
        counters.download_calls.load(Ordering::SeqCst),
        1,
        "产物写入失败后再次重试不应重新下载视频"
    );
    assert_eq!(
        counters.extract_calls.load(Ordering::SeqCst),
        1,
        "产物写入失败后再次重试不应重新抽取音频"
    );
    assert_eq!(
        counters.upload_calls.load(Ordering::SeqCst),
        1,
        "产物写入失败后再次重试不应重新上传音频"
    );
    assert_eq!(
        counters.transcribe_calls.load(Ordering::SeqCst),
        1,
        "产物写入失败后再次重试不应重新调用转写"
    );
}

#[tokio::test]
async fn task_detail_should_not_inline_transcript_payload_but_task_json_should_keep_it() {
    let (state, _temp, _counters) = build_test_state(false, false).await;

    let intake_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/intake/batches")
                .header("Authorization", "Bearer test-token")
                .header("Content-Type", "application/json")
                .body(Body::from(intake_request_json().to_string()))
                .expect("请求应构造成功"),
        )
        .await
        .expect("intake 应返回响应");
    assert_eq!(intake_response.status(), StatusCode::ACCEPTED);

    let payload: Value = serde_json::from_slice(
        &intake_response
            .into_body()
            .collect()
            .await
            .expect("响应体应可读取")
            .to_bytes(),
    )
    .expect("响应体应是合法 JSON");
    let task_id = payload["task_ids"][0]
        .as_str()
        .expect("应返回 task_id")
        .to_string();

    wait_for_condition(Duration::from_secs(3), || {
        let state = state.clone();
        let task_id = task_id.clone();
        Box::pin(async move {
            state
                .repo
                .get_task(&task_id)
                .await
                .map(|task| task.status == TaskStatus::Succeeded)
                .unwrap_or(false)
        })
    })
    .await;

    let detail_response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/tasks/{task_id}"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("详情请求应返回响应");
    assert_eq!(detail_response.status(), StatusCode::OK);

    let detail_json: Value = serde_json::from_slice(
        &detail_response
            .into_body()
            .collect()
            .await
            .expect("详情响应应可读取")
            .to_bytes(),
    )
    .expect("详情响应应是合法 JSON");

    assert!(
        detail_json["task"].get("transcript_json").is_none(),
        "前端详情接口不应再内联 transcript_json"
    );
    assert!(
        detail_json["task"].get("transcript_text").is_none(),
        "前端详情接口不应再内联 transcript_text"
    );

    let task_json_response = build_app(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/tasks/{task_id}/artifacts/task.json"))
                .header("Authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("请求应构造成功"),
        )
        .await
        .expect("task.json 请求应返回响应");
    assert_eq!(task_json_response.status(), StatusCode::OK);

    let task_json: Value = serde_json::from_slice(
        &task_json_response
            .into_body()
            .collect()
            .await
            .expect("task.json 响应应可读取")
            .to_bytes(),
    )
    .expect("task.json 响应应是合法 JSON");

    assert!(
        task_json.get("transcript_json").is_some(),
        "任务快照下载仍应保留完整 transcript_json"
    );
    assert!(
        task_json.get("transcript_text").is_some(),
        "任务快照下载仍应保留完整 transcript_text"
    );
}
