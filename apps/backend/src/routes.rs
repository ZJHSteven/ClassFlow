/*!
HTTP 路由定义。

这里保持“薄控制器”原则：

1. 路由只做参数校验、调用仓储/队列、组织响应。
2. 真正的业务编排放到 worker 和纯函数中。
3. 这样接口测试可以更聚焦在状态码、鉴权、序列化上。
*/

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde_json::json;

use crate::{
    app::AppState,
    error::{AppError, AppResult},
    models::{CourseListQuery, IntakeBatchRequest, TaskListQuery, TaskStatus},
    worker::sync_course_artifacts,
};

pub fn build_router(state: AppState) -> Router {
    let protected_routes = Router::new()
        .route("/tasks", get(list_tasks))
        .route("/tasks/{task_id}", get(get_task_detail))
        .route("/tasks/{task_id}", delete(delete_failed_task))
        .route(
            "/tasks/{task_id}/artifacts/{artifact_name}",
            get(get_task_artifact),
        )
        .route("/tasks/{task_id}/retry", post(retry_task))
        .route("/courses", get(list_courses))
        .route("/courses/{course_key}", get(get_course_detail))
        .route(
            "/courses/{course_key}/artifacts/{artifact_name}",
            get(get_course_artifact),
        )
        .route("/intake/batches", post(create_batch))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .route("/api/v1/health", get(health))
        .nest("/api/v1", protected_routes)
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "service": "classflow-backend"
        })),
    )
}

async fn create_batch(
    State(state): State<AppState>,
    Json(request): Json<IntakeBatchRequest>,
) -> AppResult<impl IntoResponse> {
    let response = state
        .repo
        .create_batch_with_tasks(&request, &state.config.default_semester)
        .await?;

    for task_id in &response.task_ids {
        state.queue.enqueue(task_id.clone())?;
    }

    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn list_tasks(
    State(state): State<AppState>,
    Query(query): Query<TaskListQuery>,
) -> AppResult<impl IntoResponse> {
    let tasks = state.repo.list_tasks(&query).await?;
    Ok(Json(
        tasks
            .iter()
            .map(crate::models::TaskSummaryResponse::from)
            .collect::<Vec<_>>(),
    ))
}

async fn get_task_detail(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    Ok(Json(state.repo.get_task_detail(&task_id).await?))
}

async fn retry_task(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    state.repo.retry_task(&task_id).await?;
    state.queue.enqueue(task_id.clone())?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"task_id": task_id, "status": "requeued"})),
    ))
}

async fn get_task_artifact(
    State(state): State<AppState>,
    Path((task_id, artifact_name)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let detail = state.repo.get_task_detail(&task_id).await?;
    match artifact_name.as_str() {
        "segment.md" => {
            let path = detail
                .task
                .segment_markdown_path
                .clone()
                .ok_or_else(|| AppError::NotFound("任务还没有单节 Markdown".to_string()))?;
            let stored = state.artifact_store.get_bytes(&path).await?;
            Ok((
                [(axum::http::header::CONTENT_TYPE, stored.content_type)],
                stored.bytes,
            )
                .into_response())
        }
        "segment.json" => {
            let path = detail
                .task
                .segment_json_path
                .clone()
                .ok_or_else(|| AppError::NotFound("任务还没有单节 JSON".to_string()))?;
            let stored = state.artifact_store.get_bytes(&path).await?;
            Ok((
                [(axum::http::header::CONTENT_TYPE, stored.content_type)],
                stored.bytes,
            )
                .into_response())
        }
        "events.json" => {
            let bytes = serde_json::to_vec_pretty(&detail.events)
                .map_err(|error| AppError::Internal(format!("序列化任务事件失败: {error}")))?;
            Ok((
                [(
                    axum::http::header::CONTENT_TYPE,
                    "application/json; charset=utf-8".to_string(),
                )],
                bytes,
            )
                .into_response())
        }
        "task.json" => {
            let bytes = serde_json::to_vec_pretty(&detail.task)
                .map_err(|error| AppError::Internal(format!("序列化任务详情失败: {error}")))?;
            Ok((
                [(
                    axum::http::header::CONTENT_TYPE,
                    "application/json; charset=utf-8".to_string(),
                )],
                bytes,
            )
                .into_response())
        }
        _ => Err(AppError::BadRequest(
            "artifact_name 目前仅支持 segment.md / segment.json / events.json / task.json"
                .to_string(),
        )),
    }
}

async fn delete_failed_task(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let task = state.repo.get_task(&task_id).await?;
    if task.status != TaskStatus::Failed {
        return Err(AppError::BadRequest(
            "只有失败任务才能执行彻底删除".to_string(),
        ));
    }

    let work_dir = state.config.temp_root.join("jobs").join(&task_id);
    state.pipeline.cleanup_dir(&work_dir).await?;

    for path in [
        task.segment_markdown_path.clone(),
        task.segment_json_path.clone(),
        task.course_manifest_path.clone(),
        task.merged_markdown_path.clone(),
    ]
    .into_iter()
    .flatten()
    {
        state.artifact_store.delete(&path).await?;
    }

    state.repo.delete_task_and_events(&task_id).await?;
    sync_course_artifacts(state.clone(), &task).await?;

    Ok((
        StatusCode::OK,
        Json(json!({"task_id": task_id, "status": "deleted"})),
    ))
}

async fn list_courses(
    State(state): State<AppState>,
    Query(query): Query<CourseListQuery>,
) -> AppResult<impl IntoResponse> {
    Ok(Json(state.repo.list_courses(&query).await?))
}

async fn get_course_detail(
    State(state): State<AppState>,
    Path(course_key): Path<String>,
) -> AppResult<impl IntoResponse> {
    Ok(Json(state.repo.get_course_detail(&course_key).await?))
}

async fn get_course_artifact(
    State(state): State<AppState>,
    Path((course_key, artifact_name)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let detail = state.repo.get_course_detail(&course_key).await?;
    let path = match artifact_name.as_str() {
        "manifest.json" => detail
            .manifest_path
            .clone()
            .ok_or_else(|| AppError::NotFound("课程还没有 manifest".to_string()))?,
        "course.md" => detail
            .merged_markdown_path
            .clone()
            .ok_or_else(|| AppError::NotFound("课程还没有总稿".to_string()))?,
        _ => {
            return Err(AppError::BadRequest(
                "artifact_name 目前仅支持 manifest.json 或 course.md".to_string(),
            ));
        }
    };

    let stored = state.artifact_store.get_bytes(&path).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, stored.content_type)],
        stored.bytes,
    ))
}

async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Result<impl IntoResponse, AppError> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or(AppError::Unauthorized)?;

    if token != state.config.bearer_token {
        return Err(AppError::Unauthorized);
    }

    Ok(next.run(request).await)
}
