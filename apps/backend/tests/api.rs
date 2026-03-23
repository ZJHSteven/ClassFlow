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
    AppConfig, AppState,
    app::build_app,
    artifacts::{ArtifactStore, LocalArtifactStore},
    config::ArtifactStoreMode,
    error::AppResult,
    models::{NormalizedTranscript, TaskListQuery, TaskStatus},
    pipeline::PipelineIo,
    repository::Repository,
    worker::{detached_queue, spawn_workers},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::ServiceExt;

#[derive(Clone)]
struct MockPipeline {
    call_count: Arc<AtomicUsize>,
    fail_first_transcription: bool,
}

#[async_trait]
impl PipelineIo for MockPipeline {
    async fn download_video(&self, _url: &str, target_path: &std::path::Path) -> AppResult<()> {
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
        if let Some(parent) = target_audio_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(target_audio_path, b"fake-audio").await?;
        Ok(())
    }

    async fn upload_audio_for_transcription(
        &self,
        _audio_path: &std::path::Path,
    ) -> AppResult<String> {
        Ok("oss://mock/audio.wav".to_string())
    }

    async fn transcribe_file_url(&self, _file_url: &str) -> AppResult<NormalizedTranscript> {
        let current = self.call_count.fetch_add(1, Ordering::SeqCst);
        if self.fail_first_transcription && current == 0 {
            return Err(backend::AppError::External("模拟转写失败".to_string()));
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

async fn build_test_state(fail_first_transcription: bool) -> (AppState, tempfile::TempDir) {
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
        dashscope_concurrency: 1,
        r2_concurrency: 1,
        cleanup_hours: 24,
        artifact_store_mode: ArtifactStoreMode::Local,
        dashscope_api_key: String::new(),
        dashscope_model: "fun-asr".to_string(),
        dashscope_submit_url:
            "https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription".to_string(),
        dashscope_task_url_template: "https://dashscope.aliyuncs.com/api/v1/tasks/{task_id}"
            .to_string(),
        dashscope_upload_policy_url: "https://dashscope.aliyuncs.com/api/v1/uploads".to_string(),
        dashscope_poll_interval_secs: 1.0,
        dashscope_poll_timeout_secs: 60.0,
        r2_bucket: String::new(),
        r2_endpoint: String::new(),
        r2_access_key_id: String::new(),
        r2_secret_access_key: String::new(),
        r2_region: "auto".to_string(),
        artifact_proxy_base_url: String::new(),
        artifact_proxy_token: String::new(),
    };

    let repo = Repository::connect(&config.db_url)
        .await
        .expect("测试数据库应连接成功");
    let artifact_store: Arc<dyn ArtifactStore> = Arc::new(LocalArtifactStore::new(artifact_root));
    let pipeline: Arc<dyn PipelineIo> = Arc::new(MockPipeline {
        call_count: Arc::new(AtomicUsize::new(0)),
        fail_first_transcription,
    });

    let mut state = AppState {
        config: Arc::new(config.clone()),
        repo,
        artifact_store,
        pipeline,
        queue: detached_queue(),
    };
    state.queue = spawn_workers(state.clone(), 1);
    (state, temp)
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
    let (state, _temp) = build_test_state(false).await;
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
    let (state, _temp) = build_test_state(false).await;
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
    let (state, _temp) = build_test_state(false).await;
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
    let (state, _temp) = build_test_state(true).await;
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
}
