/*!
后台任务调度器。

worker 的职责很单一：

1. 从内存队列中取出 task_id。
2. 按阶段推进任务，并在数据库里更新状态。
3. 成功时写入单节产物、重建课程总稿与 manifest。
4. 失败时记录错误，供前端查看和重试。
*/

use std::{path::PathBuf, sync::Arc};

use tokio::{fs, sync::mpsc};
use tracing::{error, info};

use crate::{
    app::AppState,
    course::{build_course_paths, build_manifest_json, build_merged_markdown, build_segment_markdown, build_segment_paths},
    error::{AppError, AppResult},
    models::{TaskStage, TaskStatus},
};

#[derive(Clone)]
pub struct TaskQueue {
    pub(crate) sender: mpsc::UnboundedSender<String>,
}

impl TaskQueue {
    pub fn enqueue(&self, task_id: String) -> AppResult<()> {
        self.sender
            .send(task_id)
            .map_err(|error| AppError::Internal(format!("任务入队失败: {error}")))
    }
}

pub fn detached_queue() -> TaskQueue {
    let (sender, _receiver) = mpsc::unbounded_channel::<String>();
    TaskQueue { sender }
}

pub fn spawn_workers(
    state: AppState,
    worker_count: usize,
) -> TaskQueue {
    let (sender, receiver) = mpsc::unbounded_channel::<String>();
    let shared_receiver = Arc::new(tokio::sync::Mutex::new(receiver));

    for index in 0..worker_count.max(1) {
        let state = state.clone();
        let receiver = shared_receiver.clone();
        tokio::spawn(async move {
            info!("ClassFlow worker 已启动: {}", index + 1);
            loop {
                let maybe_task_id = {
                    let mut guard = receiver.lock().await;
                    guard.recv().await
                };

                let Some(task_id) = maybe_task_id else {
                    break;
                };

                if let Err(error) = process_task(state.clone(), &task_id).await {
                    error!("任务处理失败: task_id={}, error={}", task_id, error);
                }
            }
        });
    }

    TaskQueue { sender }
}

async fn process_task(state: AppState, task_id: &str) -> AppResult<()> {
    let task = state.repo.get_task(task_id).await?;
    if task.status == TaskStatus::Succeeded {
        return Ok(());
    }

    let work_dir = state.config.temp_root.join("jobs").join(task_id);
    let source_video = work_dir.join("source.mp4");
    let extracted_audio = work_dir.join("audio.wav");

    if let Some(parent) = source_video.parent() {
        fs::create_dir_all(parent).await?;
    }

    state
        .repo
        .mark_task_running(task_id, TaskStage::Downloading)
        .await?;

    if let Err(error) = state.pipeline.download_video(&task.mp4_url, &source_video).await {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::Downloading, &error.to_string())
            .await?;
        return Err(error);
    }

    state
        .repo
        .update_task_stage(task_id, TaskStage::ExtractingAudio, "开始使用 ffmpeg 抽取音频")
        .await?;
    if let Err(error) = state.pipeline.extract_audio(&source_video, &extracted_audio).await {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::ExtractingAudio, &error.to_string())
            .await?;
        return Err(error);
    }

    state
        .repo
        .update_task_stage(task_id, TaskStage::UploadingAudio, "开始上传音频到百炼临时 OSS")
        .await?;
    let uploaded_source_url = match state
        .pipeline
        .upload_audio_for_transcription(&extracted_audio)
        .await
    {
        Ok(url) => url,
        Err(error) => {
            state
                .repo
                .mark_task_failed(task_id, TaskStage::UploadingAudio, &error.to_string())
                .await?;
            return Err(error);
        }
    };

    state
        .repo
        .update_task_stage(task_id, TaskStage::Transcribing, "开始轮询百炼异步转写任务")
        .await?;
    let transcript = match state.pipeline.transcribe_file_url(&uploaded_source_url).await {
        Ok(result) => result,
        Err(error) => {
            state
                .repo
                .mark_task_failed(task_id, TaskStage::Transcribing, &error.to_string())
                .await?;
            return Err(error);
        }
    };

    let current_task = state.repo.get_task(task_id).await?;
    let (segment_markdown_path, segment_json_path) = build_segment_paths(&current_task);
    let (manifest_path, merged_markdown_path) = build_course_paths(&current_task);

    state
        .repo
        .update_task_stage(task_id, TaskStage::StoringArtifacts, "开始写入单节 Markdown 与 JSON")
        .await?;

    let segment_markdown = build_segment_markdown(&current_task, &transcript);
    state
        .artifact_store
        .put_bytes(
            &segment_markdown_path,
            "text/markdown; charset=utf-8",
            segment_markdown.into_bytes(),
        )
        .await?;

    let segment_json = serde_json::to_vec_pretty(&transcript)
        .map_err(|error| AppError::Internal(format!("序列化单节 JSON 失败: {error}")))?;
    state
        .artifact_store
        .put_bytes(
            &segment_json_path,
            "application/json; charset=utf-8",
            segment_json,
        )
        .await?;

    state
        .repo
        .update_task_stage(task_id, TaskStage::MergingCourse, "开始重建课程总稿与 manifest")
        .await?;

    let transcript_json = serde_json::to_value(&transcript)
        .map_err(|error| AppError::Internal(format!("转写结果 JSON 化失败: {error}")))?;
    state
        .repo
        .mark_task_succeeded(
            task_id,
            &uploaded_source_url,
            &transcript.text_accu,
            &transcript_json,
            &segment_markdown_path,
            &segment_json_path,
            &manifest_path,
            &merged_markdown_path,
        )
        .await?;

    let course_tasks = state.repo.list_tasks_by_course_key(&current_task.course_key).await?;
    let summary = state.repo.get_course_detail(&current_task.course_key).await?;

    let merged_markdown = build_merged_markdown(&course_tasks);
    state
        .artifact_store
        .put_bytes(
            &merged_markdown_path,
            "text/markdown; charset=utf-8",
            merged_markdown.into_bytes(),
        )
        .await?;

    let manifest = build_manifest_json(
        &course_tasks,
        &crate::models::CourseSummaryResponse {
            course_key: summary.course_key.clone(),
            semester: summary.semester.clone(),
            course_name: summary.course_name.clone(),
            teacher_name: summary.teacher_name.clone(),
            date: summary.date.clone(),
            received_segment_count: summary.received_segment_count,
            successful_segment_count: summary.successful_segment_count,
            has_failed_segment: summary.has_failed_segment,
            merged_markdown_path: summary.merged_markdown_path.clone(),
            manifest_path: summary.manifest_path.clone(),
            updated_at: chrono::Utc::now(),
        },
    );
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| AppError::Internal(format!("序列化课程 manifest 失败: {error}")))?;
    state
        .artifact_store
        .put_bytes(
            &manifest_path,
            "application/json; charset=utf-8",
            manifest_bytes,
        )
        .await?;

    state
        .repo
        .update_task_stage(task_id, TaskStage::Cleanup, "开始清理本地临时目录")
        .await?;
    state.pipeline.cleanup_dir(&work_dir).await?;
    state
        .repo
        .update_task_stage(task_id, TaskStage::Done, "任务已完成")
        .await?;

    Ok(())
}

pub async fn cleanup_stale_temp_dirs(root: &PathBuf, max_age_hours: u64) -> AppResult<usize> {
    if !root.exists() {
        return Ok(0);
    }

    let mut removed = 0usize;
    let mut entries = fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let metadata = entry.metadata().await?;
        if !metadata.is_dir() {
            continue;
        }

        let modified = metadata.modified().map_err(|error| {
            AppError::Io(format!(
                "读取临时目录修改时间失败: path={}, error={error}",
                entry.path().display()
            ))
        })?;

        let age = std::time::SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            .as_secs();
        if age >= max_age_hours * 3600 {
            fs::remove_dir_all(entry.path()).await?;
            removed += 1;
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn should_cleanup_stale_dirs() {
        let temp = tempdir().expect("临时目录应创建成功");
        let dir = temp.path().join("old-job");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("应创建旧目录");

        let removed = cleanup_stale_temp_dirs(&temp.path().to_path_buf(), 0)
            .await
            .expect("清理应成功");
        assert_eq!(removed, 1);
    }
}
