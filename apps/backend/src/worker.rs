/*!
后台任务调度器。

worker 的职责很单一：

1. 从内存队列中取出 task_id。
2. 按阶段推进任务，并在数据库里更新状态。
3. 成功时写入单节产物、重建课程总稿与 manifest。
4. 失败时记录错误，供前端查看和重试。
*/

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use tokio::{fs, sync::mpsc, time::sleep};
use tracing::{error, info};

use crate::{
    app::AppState,
    asr::types::{AsrInput, AsrProgressEvent},
    course::{
        build_course_paths, build_manifest_json, build_merged_markdown, build_segment_markdown,
        build_segment_paths,
    },
    error::{AppError, AppResult},
    models::{NormalizedTranscript, TaskRecord, TaskStage, TaskStatus},
    pipeline::{ProgressSink, TransferProgressSnapshot},
    repository::{TaskSuccessUpdate, TaskTransferUpdate},
};

const TEMP_OSS_UPLOAD_REUSE_MAX_AGE_HOURS: i64 = 47;
const DASHSCOPE_TASK_ID_REUSE_MAX_AGE_HOURS: i64 = 24;

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

pub fn spawn_workers(state: AppState, worker_count: usize) -> TaskQueue {
    let (sender, receiver) = mpsc::unbounded_channel::<String>();
    let shared_receiver = Arc::new(tokio::sync::Mutex::new(receiver));
    let queue = TaskQueue {
        sender: sender.clone(),
    };

    for index in 0..worker_count.max(1) {
        let mut state = state.clone();
        // build_state() 为了解开 AppState <-> TaskQueue 的构造环，先放入 placeholder queue，
        // 再调用 spawn_workers。worker 自己后续需要做“延迟重入队”，因此这里必须把真实
        // sender 写回每个 worker 的 state clone，不能继续携带 placeholder。
        state.queue = queue.clone();
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

    queue
}

async fn process_task(state: AppState, task_id: &str) -> AppResult<()> {
    let task = state.repo.get_task(task_id).await?;
    if task.status == TaskStatus::Succeeded {
        return Ok(());
    }

    let work_dir = state.config.temp_root.join("jobs").join(task_id);
    let source_video = work_dir.join("source.mp4");
    let extracted_audio = work_dir.join("audio.m4a");

    if let Some(parent) = source_video.parent() {
        fs::create_dir_all(parent).await?;
    }

    let download_progress_sink: Arc<dyn ProgressSink> = Arc::new(TaskStageProgressSink {
        state: state.clone(),
        task_id: task_id.to_string(),
        stage: TaskStage::Downloading,
    });
    let upload_progress_sink: Arc<dyn ProgressSink> = Arc::new(TaskStageProgressSink {
        state: state.clone(),
        task_id: task_id.to_string(),
        stage: TaskStage::UploadingAudio,
    });

    state
        .repo
        .mark_task_running(task_id, TaskStage::Downloading)
        .await?;
    state.notify_task_list_changed();

    let audio_ready = file_exists_and_non_empty(&extracted_audio).await?;
    let video_ready = download_file_ready(&source_video).await?;

    if audio_ready {
        state
            .repo
            .update_task_stage(
                task_id,
                TaskStage::Downloading,
                "检测到历史音频检查点，跳过重复下载视频",
            )
            .await?;
    } else if video_ready {
        state
            .repo
            .update_task_stage(
                task_id,
                TaskStage::Downloading,
                "检测到历史视频检查点，跳过重复下载视频",
            )
            .await?;
    } else if let Err(error) = state
        .pipeline
        .download_video(&task.mp4_url, &source_video, Some(download_progress_sink))
        .await
    {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::Downloading, &error.to_string())
            .await?;
        state.notify_task_list_changed();
        return Err(error);
    }

    state
        .repo
        .emit_task_stage_event(
            task_id,
            TaskStage::Downloading.as_str(),
            "succeeded",
            "录播下载已完成或已命中可复用检查点",
        )
        .await;
    state
        .repo
        .update_task_stage(
            task_id,
            TaskStage::ExtractingAudio,
            "开始无损提取原音轨到 M4A 检查点",
        )
        .await?;
    state.notify_task_list_changed();
    if audio_ready {
        state
            .repo
            .update_task_stage(
                task_id,
                TaskStage::ExtractingAudio,
                "检测到历史音频检查点，跳过重复抽音频",
            )
            .await?;
    } else if let Err(error) = state
        .pipeline
        .extract_audio(&source_video, &extracted_audio)
        .await
    {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::ExtractingAudio, &error.to_string())
            .await?;
        state.notify_task_list_changed();
        return Err(error);
    }

    // 只有最终 audio.m4a 已存在且非空时才允许删除源视频。这样 remux/ffprobe
    // 失败时 source.mp4 一定保留；而重试命中 M4A checkpoint 时，也会顺手清掉
    // 可能因上次断电而残留的 MP4，避免 8GB eMMC 长期承受双份媒体占用。
    if file_exists_and_non_empty(&extracted_audio).await?
        && let Err(error) = remove_file_if_present(&source_video).await
    {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::ExtractingAudio, &error.to_string())
            .await?;
        state.notify_task_list_changed();
        return Err(error);
    }

    state
        .repo
        .update_task_stage(
            task_id,
            TaskStage::UploadingAudio,
            "开始上传音频到百炼临时 OSS",
        )
        .await?;
    state.notify_task_list_changed();
    let transcript = if is_direct_file_provider(&state) {
        match transcribe_with_direct_file_provider(&state, &task, task_id, &extracted_audio).await {
            Ok(transcript) => transcript,
            Err(AppError::RetryableExternal(message)) if is_remote_capswriter_provider(&state) => {
                schedule_remote_asr_retry(&state, task_id, &message).await?;
                return Ok(());
            }
            Err(error) => {
                let current = state.repo.get_task(task_id).await?;
                state
                    .repo
                    .mark_task_failed(task_id, current.stage, &error.to_string())
                    .await?;
                state.notify_task_list_changed();
                return Err(error);
            }
        }
    } else {
        transcribe_with_dashscope_flow(
            &state,
            &task,
            task_id,
            &extracted_audio,
            upload_progress_sink,
        )
        .await?
    };

    state
        .repo
        .emit_task_stage_event(
            task_id,
            TaskStage::Transcribing.as_str(),
            "succeeded",
            "ASR 已完成，开始持久化课程产物",
        )
        .await;

    let current_task = state.repo.get_task(task_id).await?;
    let (segment_markdown_path, segment_json_path) = build_segment_paths(&current_task);
    let (manifest_path, merged_markdown_path) = build_course_paths(&current_task);

    state
        .repo
        .update_task_stage(
            task_id,
            TaskStage::StoringArtifacts,
            "开始写入单节 Markdown 与 JSON",
        )
        .await?;
    state.notify_task_list_changed();

    let segment_markdown = build_segment_markdown(&current_task, &transcript);
    if let Err(error) = state
        .artifact_store
        .put_bytes(
            &segment_markdown_path,
            "text/markdown; charset=utf-8",
            segment_markdown.into_bytes(),
        )
        .await
    {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::StoringArtifacts, &error.to_string())
            .await?;
        state.notify_task_list_changed();
        return Err(error);
    }

    let segment_json = serde_json::to_vec_pretty(&transcript)
        .map_err(|error| AppError::Internal(format!("序列化单节 JSON 失败: {error}")))?;
    if let Err(error) = state
        .artifact_store
        .put_bytes(
            &segment_json_path,
            "application/json; charset=utf-8",
            segment_json,
        )
        .await
    {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::StoringArtifacts, &error.to_string())
            .await?;
        state.notify_task_list_changed();
        return Err(error);
    }

    state
        .repo
        .update_task_stage(
            task_id,
            TaskStage::MergingCourse,
            "开始重建课程总稿与 manifest",
        )
        .await?;
    state.notify_task_list_changed();

    let final_uploaded_source_url = state
        .repo
        .get_task(task_id)
        .await?
        .uploaded_source_url
        .unwrap_or_default();

    let transcript_json = serde_json::to_value(&transcript)
        .map_err(|error| AppError::Internal(format!("转写结果 JSON 化失败: {error}")))?;
    state
        .repo
        .mark_task_succeeded(
            task_id,
            TaskSuccessUpdate {
                uploaded_source_url: &final_uploaded_source_url,
                transcript_text: &transcript.text_accu,
                transcript_json: &transcript_json,
                segment_markdown_path: &segment_markdown_path,
                segment_json_path: &segment_json_path,
                course_manifest_path: &manifest_path,
                merged_markdown_path: &merged_markdown_path,
            },
        )
        .await?;
    state.notify_task_list_changed();

    if let Err(error) = sync_course_artifacts(state.clone(), &current_task).await {
        state
            .repo
            .mark_task_failed(task_id, TaskStage::MergingCourse, &error.to_string())
            .await?;
        state.notify_task_list_changed();
        return Err(error);
    }

    state
        .repo
        .update_task_stage(task_id, TaskStage::Cleanup, "开始清理本地临时目录")
        .await?;
    state.notify_task_list_changed();
    state.pipeline.cleanup_dir(&work_dir).await?;
    state
        .repo
        .update_task_stage(task_id, TaskStage::Done, "任务已完成")
        .await?;
    state.notify_task_list_changed();

    Ok(())
}

fn is_direct_file_provider(state: &AppState) -> bool {
    matches!(
        state.config.asr_provider.trim().to_lowercase().as_str(),
        "local_qwen" | "remote_capswriter"
    )
}

fn is_remote_capswriter_provider(state: &AppState) -> bool {
    state
        .config
        .asr_provider
        .eq_ignore_ascii_case("remote_capswriter")
}

async fn transcribe_with_direct_file_provider(
    state: &AppState,
    task: &TaskRecord,
    task_id: &str,
    extracted_audio: &Path,
) -> AppResult<NormalizedTranscript> {
    let restored_transcript_checkpoint =
        restore_transcript_checkpoint(task, state, task_id).await?;
    if let Some(saved_transcript) = restored_transcript_checkpoint {
        state
            .repo
            .update_task_stage(
                task_id,
                TaskStage::Transcribing,
                "检测到历史转写检查点，跳过重复 ASR",
            )
            .await?;
        state.notify_task_list_changed();
        return Ok(saved_transcript);
    }

    if is_remote_capswriter_provider(state) {
        return transcribe_with_remote_capswriter(state, task, task_id, extracted_audio).await;
    }

    state
        .repo
        .update_task_stage(
            task_id,
            TaskStage::UploadingAudio,
            "当前使用本地 ASR，跳过云端音频上传阶段",
        )
        .await?;
    state.notify_task_list_changed();

    state
        .repo
        .update_task_stage(
            task_id,
            TaskStage::Transcribing,
            "开始执行本地 Qwen3 ASR 转写",
        )
        .await?;
    state.notify_task_list_changed();

    let result = state
        .asr_provider
        .transcribe_audio(AsrInput {
            audio_path: extracted_audio.to_path_buf(),
            task_id: task_id.to_string(),
            language: Some("zh".to_string()),
            context: Some(task.raw_title.clone()),
            resume_job_id: None,
            progress: None,
        })
        .await?;

    let transcript_json = serde_json::to_value(&result)
        .map_err(|error| AppError::Internal(format!("转写结果 JSON 化失败: {error}")))?;
    state
        .repo
        .save_transcript_checkpoint(task_id, &result.text_accu, &transcript_json)
        .await?;
    state.notify_task_list_changed();
    Ok(result)
}

async fn transcribe_with_remote_capswriter(
    state: &AppState,
    task: &TaskRecord,
    task_id: &str,
    extracted_audio: &Path,
) -> AppResult<NormalizedTranscript> {
    state
        .repo
        .update_task_stage(
            task_id,
            TaskStage::WaitingRemoteAsr,
            if task.remote_capswriter_job_id.is_some() {
                "检测到远端 ASR job 检查点，准备继续轮询"
            } else {
                "音频检查点已就绪，等待远端 ASR 节点"
            },
        )
        .await?;
    state.notify_task_list_changed();

    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<AsrProgressEvent>();
    let input = AsrInput {
        audio_path: extracted_audio.to_path_buf(),
        task_id: task_id.to_string(),
        language: Some("zh".to_string()),
        context: Some(task.raw_title.clone()),
        resume_job_id: task.remote_capswriter_job_id.clone(),
        progress: Some(progress_tx),
    };
    let mut transcribe = Box::pin(state.asr_provider.transcribe_audio(input));

    loop {
        tokio::select! {
            result = &mut transcribe => {
                // provider 很快结束时，Submitted/Invalidated 事件可能已经进 channel、但还没被
                // select 分支消费。返回结果前把队列排空，确保 job checkpoint 不丢。
                while let Ok(event) = progress_rx.try_recv() {
                    handle_remote_asr_progress(state, task_id, event).await?;
                }
                return result;
            }
            maybe_event = progress_rx.recv() => {
                if let Some(event) = maybe_event {
                    handle_remote_asr_progress(state, task_id, event).await?;
                }
            }
        }
    }
}

async fn handle_remote_asr_progress(
    state: &AppState,
    task_id: &str,
    event: AsrProgressEvent,
) -> AppResult<()> {
    match event {
        AsrProgressEvent::Uploading => {
            state
                .repo
                .update_task_stage(
                    task_id,
                    TaskStage::UploadingAudio,
                    "正在把 M4A 检查点流式上传到远端 ASR",
                )
                .await?;
        }
        AsrProgressEvent::UploadProgress {
            transferred_bytes,
            total_bytes,
            rate_bytes_per_sec,
            eta_seconds,
        } => {
            let progress_percent = if total_bytes > 0 {
                Some((transferred_bytes as f64 / total_bytes as f64 * 100.0).min(100.0))
            } else {
                None
            };
            state
                .repo
                .update_task_transfer_progress(
                    task_id,
                    TaskStage::UploadingAudio,
                    TaskTransferUpdate {
                        progress_percent,
                        transferred_bytes: Some(transferred_bytes),
                        total_bytes: Some(total_bytes),
                        rate_bytes_per_sec: Some(rate_bytes_per_sec),
                        eta_seconds,
                    },
                )
                .await?;
        }
        AsrProgressEvent::Submitted { job_id } => {
            state
                .repo
                .save_remote_capswriter_job_checkpoint(task_id, &job_id)
                .await?;
            state
                .repo
                .update_task_stage(
                    task_id,
                    TaskStage::RemoteAsrQueued,
                    &format!("远端 ASR 已提交，job={job_id}"),
                )
                .await?;
        }
        AsrProgressEvent::Queued { job_id } => {
            state
                .repo
                .update_task_stage(
                    task_id,
                    TaskStage::RemoteAsrQueued,
                    &format!("远端 ASR 正在排队，job={job_id}"),
                )
                .await?;
        }
        AsrProgressEvent::Running { job_id } => {
            state
                .repo
                .update_task_stage(
                    task_id,
                    TaskStage::Transcribing,
                    &format!("远端 ASR 正在转写，job={job_id}"),
                )
                .await?;
        }
        AsrProgressEvent::JobInvalidated { job_id, reason } => {
            state
                .repo
                .clear_remote_capswriter_job_checkpoint(
                    task_id,
                    &format!("远端 job {job_id} 不再可复用：{reason}"),
                )
                .await?;
        }
    }
    state.notify_task_list_changed();
    Ok(())
}

async fn schedule_remote_asr_retry(state: &AppState, task_id: &str, error: &str) -> AppResult<()> {
    let task = state.repo.get_task(task_id).await?;
    let base = state.config.remote_capswriter_retry_wait_secs.max(0.05);
    let max_wait = state.config.remote_capswriter_retry_max_wait_secs.max(base);
    let exponent = task.attempt_count.saturating_sub(1).clamp(0, 8) as i32;
    let wait_secs = (base * 2_f64.powi(exponent)).min(max_wait);
    let message = format!(
        "远端 ASR 暂不可用，将在约 {:.0} 秒后自动重试；本地 audio.m4a 与远端 job checkpoint 均保留。原因: {}",
        wait_secs, error
    );
    state
        .repo
        .mark_task_retry_waiting(task_id, &message)
        .await?;
    state.notify_task_list_changed();

    let repo = state.repo.clone();
    let queue = state.queue.clone();
    let retry_task_id = task_id.to_string();
    tokio::spawn(async move {
        sleep(Duration::from_secs_f64(wait_secs)).await;
        let Ok(task) = repo.get_task(&retry_task_id).await else {
            return;
        };
        if task.status == TaskStatus::Pending && task.stage == TaskStage::RetryWaiting {
            let _ = queue.enqueue(retry_task_id);
        }
    });
    Ok(())
}

async fn transcribe_with_dashscope_flow(
    state: &AppState,
    task: &TaskRecord,
    task_id: &str,
    extracted_audio: &Path,
    upload_progress_sink: Arc<dyn ProgressSink>,
) -> AppResult<NormalizedTranscript> {
    let restored_transcript_checkpoint =
        restore_transcript_checkpoint(task, state, task_id).await?;
    let uploaded_source_url = if restored_transcript_checkpoint.is_some() {
        let saved_url = task.uploaded_source_url.clone().unwrap_or_default();
        state
            .repo
            .update_task_stage(
                task_id,
                TaskStage::UploadingAudio,
                "检测到可恢复的历史转写检查点，跳过上传音频",
            )
            .await?;
        saved_url
    } else if let Some(saved_url) = reusable_uploaded_source_url(task) {
        state
            .repo
            .update_task_stage(
                task_id,
                TaskStage::UploadingAudio,
                "检测到仍在有效期内的历史上传检查点，跳过重复上传音频",
            )
            .await?;
        saved_url
    } else {
        if should_report_expired_uploaded_source_url(task) {
            state
                .repo
                .update_task_stage(
                    task_id,
                    TaskStage::UploadingAudio,
                    "历史百炼临时 OSS 上传检查点缺少保存时间或已超过安全复用窗口，重新上传音频",
                )
                .await?;
        }

        match state
            .pipeline
            .upload_audio_for_transcription(extracted_audio, Some(upload_progress_sink))
            .await
        {
            Ok(url) => {
                state.repo.save_uploaded_source_url(task_id, &url).await?;
                if task.dashscope_task_id.is_some() {
                    state
                        .repo
                        .clear_dashscope_task_checkpoint(
                            task_id,
                            "音频已重新上传，旧百炼 task_id 不再复用",
                        )
                        .await?;
                }
                state.notify_task_list_changed();
                url
            }
            Err(error) => {
                state
                    .repo
                    .mark_task_failed(task_id, TaskStage::UploadingAudio, &error.to_string())
                    .await?;
                state.notify_task_list_changed();
                return Err(error);
            }
        }
    };

    state
        .repo
        .update_task_stage(task_id, TaskStage::Transcribing, "开始轮询百炼异步转写任务")
        .await?;
    state.notify_task_list_changed();
    let task_after_upload = state.repo.get_task(task_id).await?;
    if let Some(saved_transcript) = restored_transcript_checkpoint {
        state
            .repo
            .update_task_stage(
                task_id,
                TaskStage::Transcribing,
                "检测到历史转写检查点，跳过重复调用百炼",
            )
            .await?;
        return Ok(saved_transcript);
    }

    match transcribe_with_task_checkpoint(state, &task_after_upload, task_id, &uploaded_source_url)
        .await
    {
        Ok(result) => {
            let transcript_json = serde_json::to_value(&result)
                .map_err(|error| AppError::Internal(format!("转写结果 JSON 化失败: {error}")))?;
            state
                .repo
                .save_transcript_checkpoint(task_id, &result.text_accu, &transcript_json)
                .await?;
            state.notify_task_list_changed();
            Ok(result)
        }
        Err(error) => {
            state
                .repo
                .mark_task_failed(task_id, TaskStage::Transcribing, &error.to_string())
                .await?;
            state.notify_task_list_changed();
            Err(error)
        }
    }
}

async fn transcribe_with_task_checkpoint(
    state: &AppState,
    task: &TaskRecord,
    task_id: &str,
    uploaded_source_url: &str,
) -> AppResult<NormalizedTranscript> {
    let mut saved_dashscope_task_id = reusable_dashscope_task_id(task);
    if task.dashscope_task_id.is_some() && saved_dashscope_task_id.is_none() {
        state
            .repo
            .clear_dashscope_task_checkpoint(
                task_id,
                "历史百炼 task_id 检查点缺少保存时间或已超过安全复用窗口，重新提交转写任务",
            )
            .await?;
    }

    loop {
        let used_saved_checkpoint = saved_dashscope_task_id.is_some();
        let dashscope_task_id = match saved_dashscope_task_id.take() {
            Some(existing_task_id) => {
                state
                    .repo
                    .update_task_stage(
                        task_id,
                        TaskStage::Transcribing,
                        &format!(
                            "检测到仍在安全窗口内的百炼 task_id，继续轮询: {existing_task_id}"
                        ),
                    )
                    .await?;
                existing_task_id
            }
            None => {
                let new_task_id = state
                    .pipeline
                    .submit_transcription_task(uploaded_source_url)
                    .await?;
                state
                    .repo
                    .save_dashscope_task_checkpoint(task_id, &new_task_id)
                    .await?;
                state.notify_task_list_changed();
                new_task_id
            }
        };

        match state
            .pipeline
            .poll_transcription_task(&dashscope_task_id)
            .await
        {
            Ok(result) => return Ok(result),
            Err(error)
                if used_saved_checkpoint && is_dashscope_task_checkpoint_stale_error(&error) =>
            {
                state
                    .repo
                    .clear_dashscope_task_checkpoint(
                        task_id,
                        &format!(
                            "百炼 task_id 检查点已失效，清理后重新提交转写任务: {dashscope_task_id}"
                        ),
                    )
                    .await?;
                state.notify_task_list_changed();
            }
            Err(error) => return Err(error),
        }
    }
}

/**
 * 判断数据库里的上传检查点是否还能复用。
 *
 * `http/https` 这类外部长期 URL 不由 ClassFlow 控制生命周期，沿用原来的复用策略。
 * `oss://` 是百炼 `getPolicy` 生成的临时对象，官方有效期为 48 小时；这里用 47 小时作为安全窗口，
 * 避免卡在边界时“刚判断可用，提交转写时已经过期”。
 *
 * 旧版本数据库没有 `uploaded_source_url_saved_at`，这类历史 `oss://` 检查点一律视为不可复用。
 */
fn reusable_uploaded_source_url(task: &TaskRecord) -> Option<String> {
    let saved_url = task
        .uploaded_source_url
        .clone()
        .filter(|value| !value.trim().is_empty())?;

    if !saved_url.trim_start().starts_with("oss://") {
        return Some(saved_url);
    }

    let saved_at = task.uploaded_source_url_saved_at?;
    let max_age = ChronoDuration::hours(TEMP_OSS_UPLOAD_REUSE_MAX_AGE_HOURS);
    if Utc::now().signed_duration_since(saved_at) < max_age {
        return Some(saved_url);
    }

    None
}

/**
 * 判断是否需要给事件日志写一条“为什么没有复用上传检查点”的解释。
 *
 * 这个函数只负责展示原因，不改变数据库字段。后续真正上传成功时，
 * `save_uploaded_source_url()` 会用新的 `oss://` 和新的保存时间覆盖旧检查点。
 */
fn should_report_expired_uploaded_source_url(task: &TaskRecord) -> bool {
    task.uploaded_source_url
        .as_deref()
        .map(|value| value.trim_start().starts_with("oss://"))
        .unwrap_or(false)
}

fn reusable_dashscope_task_id(task: &TaskRecord) -> Option<String> {
    let task_id = task
        .dashscope_task_id
        .clone()
        .filter(|value| !value.trim().is_empty())?;
    let saved_at = task.dashscope_task_id_saved_at?;
    let max_age = ChronoDuration::hours(DASHSCOPE_TASK_ID_REUSE_MAX_AGE_HOURS);

    if Utc::now().signed_duration_since(saved_at) < max_age {
        return Some(task_id);
    }

    None
}

fn is_dashscope_task_checkpoint_stale_error(error: &AppError) -> bool {
    let AppError::External(message) = error else {
        return false;
    };

    let text = message.to_lowercase();
    [
        "http=404",
        "notfound",
        "not found",
        "not_exist",
        "not exist",
        "resource not exist",
        "resource_not_found",
        "expired",
        "过期",
        "不存在",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[derive(Clone)]
struct TaskStageProgressSink {
    state: AppState,
    task_id: String,
    stage: TaskStage,
}

#[async_trait]
impl ProgressSink for TaskStageProgressSink {
    async fn report(&self, snapshot: TransferProgressSnapshot) -> AppResult<()> {
        self.state
            .repo
            .update_task_transfer_progress(
                &self.task_id,
                self.stage.clone(),
                TaskTransferUpdate {
                    progress_percent: snapshot.progress_percent,
                    transferred_bytes: snapshot.transferred_bytes,
                    total_bytes: snapshot.total_bytes,
                    rate_bytes_per_sec: snapshot.rate_bytes_per_sec,
                    eta_seconds: snapshot.eta_seconds,
                },
            )
            .await?;
        self.state.notify_task_list_changed();
        Ok(())
    }
}

/**
 * 判断某个中间文件是否存在且非空。
 *
 * 这样做的原因很直接：
 *
 * 1. 下载/抽音频中途异常时，经常会留下零字节或截断文件。
 * 2. worker 只有确认文件里确实有内容，才允许把它当成“可复用检查点”。
 */
async fn file_exists_and_non_empty(path: &Path) -> AppResult<bool> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::Io(format!(
            "读取检查点文件失败: path={}, error={error}",
            path.display()
        ))),
    }
}

async fn remove_file_if_present(path: &Path) -> AppResult<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Io(format!(
            "删除已被 M4A 检查点替代的源视频失败: path={}, error={error}",
            path.display()
        ))),
    }
}

/**
 * 下载阶段除了 `source.mp4` 本身，还要检查 aria2 的 sidecar 文件。
 *
 * `aria2c` 未完全结束时会留下 `source.mp4.aria2`。只要 sidecar 还在，就说明这次下载
 * 还没有真正收口，不能把它当作“下次重试可直接跳过”的完整检查点。
 */
async fn download_file_ready(path: &Path) -> AppResult<bool> {
    if !file_exists_and_non_empty(path).await? {
        return Ok(false);
    }

    let aria2_sidecar = PathBuf::from(format!("{}.aria2", path.display()));
    Ok(!aria2_sidecar.exists())
}

/**
 * 尝试恢复已保存的转写检查点。
 *
 * 正常情况下，`transcript_json` 会被反序列化回 `NormalizedTranscript`。
 * 如果历史数据已经损坏，这里不会直接把任务判死，而是记一条警告日志后回退到重新转写。
 */
async fn restore_transcript_checkpoint(
    task: &TaskRecord,
    state: &AppState,
    task_id: &str,
) -> AppResult<Option<NormalizedTranscript>> {
    let Some(raw_transcript) = task.transcript_json.clone() else {
        return Ok(None);
    };

    match serde_json::from_value::<NormalizedTranscript>(raw_transcript) {
        Ok(transcript) => Ok(Some(transcript)),
        Err(error) => {
            state
                .repo
                .add_task_event(
                    task_id,
                    TaskStage::Transcribing.as_str(),
                    "warn",
                    &format!("发现损坏的转写检查点，已回退到重新转写: {error}"),
                )
                .await?;
            Ok(None)
        }
    }
}

/**
 * 按课程维度重建 manifest / merged 产物。
 *
 * 这个函数专门抽出来，是因为它不仅会在“任务成功后”调用，
 * 后续“失败任务被用户彻底删除”时也需要复用同一套逻辑来刷新课程聚合结果。
 *
 * 规则：
 *
 * 1. 课程已不存在时，删除旧的 manifest / merged 对象。
 * 2. 课程仍存在但还没有成功片段时，只保留 manifest，不保留 merged 总稿。
 * 3. 课程有成功片段时，同时重建 manifest 与 merged 总稿。
 */
pub async fn sync_course_artifacts(
    state: AppState,
    sample_task: &crate::models::TaskRecord,
) -> AppResult<()> {
    let course_tasks = state
        .repo
        .list_tasks_by_course_key(&sample_task.course_key)
        .await?;
    let (manifest_path, merged_markdown_path) = build_course_paths(sample_task);

    if course_tasks.is_empty() {
        state.artifact_store.delete(&manifest_path).await?;
        state.artifact_store.delete(&merged_markdown_path).await?;
        return Ok(());
    }

    let summary = state
        .repo
        .get_course_detail(&sample_task.course_key)
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
            merged_markdown_path: if summary.successful_segment_count > 0 {
                Some(merged_markdown_path.clone())
            } else {
                None
            },
            manifest_path: Some(manifest_path.clone()),
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

    if summary.successful_segment_count == 0 {
        state.artifact_store.delete(&merged_markdown_path).await?;
        return Ok(());
    }

    let merged_markdown = build_merged_markdown(&course_tasks);
    state
        .artifact_store
        .put_bytes(
            &merged_markdown_path,
            "text/markdown; charset=utf-8",
            merged_markdown.into_bytes(),
        )
        .await
}

pub async fn cleanup_stale_temp_dirs(
    root: &PathBuf,
    max_age_hours: u64,
    repo: &crate::repository::Repository,
) -> AppResult<usize> {
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
        if age < max_age_hours * 3600 {
            continue;
        }

        let task_id = entry.file_name().to_string_lossy().to_string();
        let task = match repo.get_task(&task_id).await {
            Ok(task) => Some(task),
            Err(AppError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };

        match task {
            None => {
                // SQLite 已经没有这个 task：整个目录就是孤儿缓存，可以直接删。
                fs::remove_dir_all(entry.path()).await?;
                removed += 1;
            }
            Some(task) => {
                let audio = entry.path().join("audio.m4a");
                let audio_ready = file_exists_and_non_empty(&audio).await?;
                match task.status {
                    TaskStatus::Succeeded => {
                        // 成功任务理论上已经在正常 cleanup 阶段删掉 work_dir；若因为断电
                        // 留下残骸，这里可以全部回收。
                        fs::remove_dir_all(entry.path()).await?;
                        removed += 1;
                    }
                    TaskStatus::Pending | TaskStatus::Running if audio_ready => {
                        // 对远端 ASR 最重要的 durable checkpoint 是 audio.m4a。只保留它，
                        // 清掉可能残留的 source.mp4/.aria2/其他临时文件，避免 8GB eMMC
                        // 因“视频 + 音频双份”长期占用。
                        prune_work_dir_to_audio_checkpoint(&entry.path(), &audio).await?;
                    }
                    TaskStatus::Pending | TaskStatus::Running => {
                        // 任务仍会自动恢复，但尚未形成 M4A。source.mp4/.aria2 可能是下载
                        // 断点，不能在启动恢复前删掉。
                    }
                    TaskStatus::Failed if audio_ready => {
                        // 失败任务保留 M4A，用户以后点重试可以直接从 ASR/后处理继续，
                        // 不重新下载视频。
                        prune_work_dir_to_audio_checkpoint(&entry.path(), &audio).await?;
                    }
                    TaskStatus::Failed => {
                        // 没有 M4A 的陈旧失败目录没有不可替代 checkpoint；手动重试时可
                        // 重新下载，因此可以回收。
                        fs::remove_dir_all(entry.path()).await?;
                        removed += 1;
                    }
                }
            }
        }
    }

    Ok(removed)
}

async fn prune_work_dir_to_audio_checkpoint(dir: &Path, audio_checkpoint: &Path) -> AppResult<()> {
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path == audio_checkpoint {
            continue;
        }
        let metadata = entry.metadata().await?;
        if metadata.is_dir() {
            fs::remove_dir_all(path).await?;
        } else {
            fs::remove_file(path).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        body::Bytes,
        extract::{Path as AxumPath, State},
        http::StatusCode,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use tempfile::tempdir;

    use crate::{
        app::build_state,
        config::{AppConfig, ArtifactStoreMode},
        models::{IntakeBatchRequest, IntakeItem},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct RetryRemoteState {
        submits: Arc<AtomicUsize>,
    }

    async fn flaky_remote_submit(
        State(state): State<RetryRemoteState>,
        _body: Bytes,
    ) -> (StatusCode, Json<Value>) {
        let index = state.submits.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":{"code":"offline","message":"laptop sleeping"}})),
            );
        }
        (
            StatusCode::ACCEPTED,
            Json(json!({"id":"job-after-retry","status":"queued"})),
        )
    }

    async fn flaky_remote_status(AxumPath(job_id): AxumPath<String>) -> Json<Value> {
        assert_eq!(job_id, "job-after-retry");
        Json(json!({
            "id": job_id,
            "status": "completed",
            "error": null,
            "result": {
                "text": "自动恢复成功",
                "text_accu": "自动恢复成功",
                "duration": 2.0,
                "tokens": ["自动", "恢复", "成功"],
                "timestamps": [0.0, 0.6, 1.2]
            }
        }))
    }

    #[tokio::test]
    async fn should_cleanup_stale_dirs() {
        let temp = tempdir().expect("临时目录应创建成功");
        let dir = temp.path().join("old-job");
        tokio::fs::create_dir_all(&dir).await.expect("应创建旧目录");

        let db = temp.path().join("cleanup.db");
        let repo =
            crate::repository::Repository::connect(&format!("sqlite://{}?mode=rwc", db.display()))
                .await
                .expect("repo should build");

        let removed = cleanup_stale_temp_dirs(&temp.path().to_path_buf(), 0, &repo)
            .await
            .expect("清理应成功");
        assert_eq!(removed, 1);
    }

    #[tokio::test]
    async fn cleanup_should_preserve_failed_m4a_checkpoint_but_drop_extra_media() {
        let temp = tempdir().expect("tempdir should exist");
        let db = temp.path().join("cleanup-preserve.db");
        let repo =
            crate::repository::Repository::connect(&format!("sqlite://{}?mode=rwc", db.display()))
                .await
                .expect("repo should build");
        let created = repo
            .create_batch_with_tasks(
                &IntakeBatchRequest {
                    semester: Some("2026-2027-1".to_string()),
                    source: "smartclass".to_string(),
                    items: vec![IntakeItem {
                        new_id: "cleanup-preserve".to_string(),
                        page_url: "https://example.test/page".to_string(),
                        mp4_url: "https://example.test/video.mp4".to_string(),
                        course_name: "缓存清理测试".to_string(),
                        teacher_name: "老师".to_string(),
                        date: "2026-10-08".to_string(),
                        start_time: "10:00".to_string(),
                        end_time: "10:45".to_string(),
                        raw_title: "缓存清理测试".to_string(),
                    }],
                },
                "2026-2027-1",
            )
            .await
            .expect("task should create");
        let task_id = &created.task_ids[0];
        repo.mark_task_failed(task_id, TaskStage::Transcribing, "模拟远端失败")
            .await
            .expect("task should fail");

        let root = temp.path().join("jobs");
        let dir = root.join(task_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("job dir should create");
        tokio::fs::write(dir.join("audio.m4a"), b"checkpoint")
            .await
            .expect("audio should write");
        tokio::fs::write(dir.join("source.mp4"), b"redundant-video")
            .await
            .expect("video should write");

        let removed = cleanup_stale_temp_dirs(&root, 0, &repo)
            .await
            .expect("cleanup should succeed");
        assert_eq!(removed, 0);
        assert!(dir.join("audio.m4a").exists());
        assert!(!dir.join("source.mp4").exists());
    }

    #[tokio::test]
    async fn remote_asr_temporary_outage_should_wait_and_resume_from_m4a() {
        let remote_state = RetryRemoteState::default();
        let app = Router::new()
            .route("/v1/transcriptions", post(flaky_remote_submit))
            .route("/v1/transcriptions/{job_id}", get(flaky_remote_status))
            .with_state(remote_state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock remote should bind");
        let addr = listener.local_addr().expect("mock remote should have addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock remote should serve");
        });

        let temp = tempdir().expect("tempdir should exist");
        let mut config = AppConfig::from_env().expect("default config should parse");
        config.bind_addr = "127.0.0.1:0".parse().expect("bind should parse");
        config.db_url = format!(
            "sqlite://{}?mode=rwc",
            temp.path().join("classflow.db").display()
        );
        config.temp_root = temp.path().join("tmp");
        config.local_artifact_root = temp.path().join("artifacts");
        config.artifact_store_mode = ArtifactStoreMode::Local;
        config.asr_provider = "remote_capswriter".to_string();
        config.remote_capswriter_base_url = format!("http://{addr}");
        config.remote_capswriter_token.clear();
        config.remote_capswriter_access_client_id.clear();
        config.remote_capswriter_access_client_secret.clear();
        config.remote_capswriter_retry_wait_secs = 0.05;
        config.remote_capswriter_retry_max_wait_secs = 0.05;
        config.remote_capswriter_poll_interval_secs = 0.01;
        config.remote_capswriter_poll_timeout_secs = 5.0;
        config.remote_capswriter_request_timeout_secs = 1.0;
        config.remote_capswriter_upload_timeout_secs = 1.0;
        config.task_worker_count = 1;
        config.smartclass_collector_enabled = false;
        config.network_health_enabled = false;

        let state = build_state(config).await.expect("state should build");
        let intake = IntakeBatchRequest {
            semester: Some("2026-2027-1".to_string()),
            source: "smartclass".to_string(),
            items: vec![IntakeItem {
                new_id: "retry-remote-1".to_string(),
                page_url: "https://example.test/page".to_string(),
                mp4_url: "https://example.test/video.mp4".to_string(),
                course_name: "远端重试测试".to_string(),
                teacher_name: "测试老师".to_string(),
                date: "2026-10-08".to_string(),
                start_time: "08:00".to_string(),
                end_time: "08:45".to_string(),
                raw_title: "远端重试测试 测试老师".to_string(),
            }],
        };
        let created = state
            .repo
            .create_batch_with_tasks(&intake, "2026-2027-1")
            .await
            .expect("task should create");
        let task_id = created.task_ids[0].clone();
        let work_dir = state.config.temp_root.join("jobs").join(&task_id);
        tokio::fs::create_dir_all(&work_dir)
            .await
            .expect("work dir should exist");
        tokio::fs::write(work_dir.join("audio.m4a"), b"durable-m4a-checkpoint")
            .await
            .expect("m4a checkpoint should exist");

        state
            .queue
            .enqueue(task_id.clone())
            .expect("task should enqueue");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let task = state
                .repo
                .get_task(&task_id)
                .await
                .expect("task should exist");
            if task.status == TaskStatus::Succeeded {
                assert!(task.attempt_count >= 2);
                assert_eq!(task.transcript_text.as_deref(), Some("自动恢复成功"));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "task should auto recover"
            );
            sleep(Duration::from_millis(20)).await;
        }

        assert!(remote_state.submits.load(Ordering::SeqCst) >= 2);
        let detail = state
            .repo
            .get_task_detail(&task_id)
            .await
            .expect("detail should load");
        assert!(
            detail
                .events
                .iter()
                .any(|event| event.stage == "retry_waiting")
        );
        assert!(
            detail
                .events
                .iter()
                .any(|event| event.stage == "remote_asr_queued")
        );

        let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while work_dir.exists() && tokio::time::Instant::now() < cleanup_deadline {
            sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !work_dir.exists(),
            "successful task should clean checkpoint dir"
        );

        server.abort();
    }
}
