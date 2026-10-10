/*!
该模块集中放置“接口入参/出参 + 数据库存储实体 + 中间值对象”。

把这些结构统一放在一起，最大的好处是：

1. HTTP 接口与数据库字段可以一眼对照。
2. 测试夹具复用方便，不用在多个模块里来回复制定义。
3. 课程聚合、对象存储路径、Markdown 渲染都围绕同一套结构工作。
*/

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntakeBatchRequest {
    pub semester: Option<String>,
    pub source: String,
    pub items: Vec<IntakeItem>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntakeItem {
    pub new_id: String,
    pub page_url: String,
    pub mp4_url: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub raw_title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntakeBatchResponse {
    pub batch_id: String,
    pub accepted_count: usize,
    pub task_ids: Vec<String>,
    pub course_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStage {
    Queued,
    Downloading,
    ExtractingAudio,
    WaitingRemoteAsr,
    UploadingAudio,
    RemoteAsrQueued,
    Transcribing,
    RetryWaiting,
    StoringArtifacts,
    MergingCourse,
    Cleanup,
    Done,
}

impl TaskStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::ExtractingAudio => "extracting_audio",
            Self::WaitingRemoteAsr => "waiting_remote_asr",
            Self::UploadingAudio => "uploading_audio",
            Self::RemoteAsrQueued => "remote_asr_queued",
            Self::Transcribing => "transcribing",
            Self::RetryWaiting => "retry_waiting",
            Self::StoringArtifacts => "storing_artifacts",
            Self::MergingCourse => "merging_course",
            Self::Cleanup => "cleanup",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub batch_id: String,
    pub segment_key: String,
    pub source: String,
    pub status: TaskStatus,
    pub stage: TaskStage,
    pub semester: String,
    pub course_key: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub page_url: String,
    pub mp4_url: String,
    pub new_id: String,
    pub raw_title: String,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub uploaded_source_url: Option<String>,
    pub uploaded_source_url_saved_at: Option<DateTime<Utc>>,
    pub dashscope_task_id: Option<String>,
    pub dashscope_task_id_saved_at: Option<DateTime<Utc>>,
    /// Windows CapsWriterGateway 的异步 job checkpoint。
    ///
    /// 这两个字段和本地 `audio.m4a` 一起构成远端 ASR 的 durable resume 边界：
    /// N1 在提交成功后断网时，下次先继续轮询旧 job，而不是重复提交 GPU 任务。
    pub remote_capswriter_job_id: Option<String>,
    pub remote_capswriter_job_id_saved_at: Option<DateTime<Utc>>,
    pub segment_markdown_path: Option<String>,
    pub segment_json_path: Option<String>,
    pub course_manifest_path: Option<String>,
    pub merged_markdown_path: Option<String>,
    pub transcript_text: Option<String>,
    pub transcript_json: Option<Value>,
    pub progress_percent: Option<f64>,
    pub transferred_bytes: Option<i64>,
    pub total_bytes: Option<i64>,
    pub rate_bytes_per_sec: Option<i64>,
    pub eta_seconds: Option<i64>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEventRecord {
    pub id: i64,
    pub task_id: String,
    pub stage: String,
    pub level: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageEventRecord {
    pub id: i64,
    pub event_key: String,
    pub source_kind: String,
    pub source_id: String,
    pub stage: String,
    pub outcome: String,
    pub title: String,
    pub message: String,
    pub payload: Value,
    pub delivery_status: String,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskListQuery {
    pub status: Option<String>,
    pub date: Option<String>,
    pub course_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskSummaryResponse {
    pub id: String,
    pub batch_id: String,
    pub status: TaskStatus,
    pub stage: TaskStage,
    pub semester: String,
    pub course_key: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub last_error: Option<String>,
    pub progress_percent: Option<f64>,
    pub transferred_bytes: Option<i64>,
    pub total_bytes: Option<i64>,
    pub rate_bytes_per_sec: Option<i64>,
    pub eta_seconds: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetailTaskResponse {
    pub id: String,
    pub batch_id: String,
    pub status: TaskStatus,
    pub stage: TaskStage,
    pub semester: String,
    pub course_key: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub new_id: String,
    pub page_url: String,
    pub mp4_url: String,
    pub last_error: Option<String>,
    pub progress_percent: Option<f64>,
    pub transferred_bytes: Option<i64>,
    pub total_bytes: Option<i64>,
    pub rate_bytes_per_sec: Option<i64>,
    pub eta_seconds: Option<i64>,
    pub segment_markdown_path: Option<String>,
    pub segment_json_path: Option<String>,
    pub course_manifest_path: Option<String>,
    pub merged_markdown_path: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetailResponse {
    pub task: TaskDetailTaskResponse,
    pub events: Vec<TaskEventRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CourseListQuery {
    pub semester: Option<String>,
    pub date: Option<String>,
    pub course_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CourseSummaryResponse {
    pub course_key: String,
    pub semester: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub received_segment_count: usize,
    pub successful_segment_count: usize,
    pub has_failed_segment: bool,
    pub merged_markdown_path: Option<String>,
    pub manifest_path: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CourseDetailResponse {
    pub course_key: String,
    pub semester: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub received_segment_count: usize,
    pub successful_segment_count: usize,
    pub has_failed_segment: bool,
    pub merged_markdown_path: Option<String>,
    pub manifest_path: Option<String>,
    pub lectures: Vec<LectureSummaryResponse>,
    pub segments: Vec<TaskSummaryResponse>,
}

/// 一堂完整课堂在课程库中的稳定摘要。
///
/// Phase 3 Notes 以 lecture 为单位，而不是以 SmartClass 单个 45 分钟 segment 为单位。
#[derive(Debug, Clone, Serialize)]
pub struct LectureSummaryResponse {
    pub lecture_key: String,
    pub start_time: String,
    pub end_time: String,
    pub segment_count: usize,
    pub successful_segment_count: usize,
    pub transcript_ready: bool,
    pub notes_status: String,
    pub notes_title: Option<String>,
    pub notes_ready: bool,
    pub notes_last_error: Option<String>,
    pub notes_recovery_kind: Option<String>,
    pub notes_conversation_id: Option<String>,
    pub notes_submitted_at: Option<String>,
    pub notes_github_path: Option<String>,
    pub task_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LectureNotesStatus {
    Pending,
    Generating,
    Publishing,
    Succeeded,
    Failed,
}

impl LectureNotesStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Generating => "generating",
            Self::Publishing => "publishing",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

impl std::str::FromStr for LectureNotesStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "generating" => Ok(Self::Generating),
            "publishing" => Ok(Self::Publishing),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => Err(format!("未知 Notes 状态: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LectureNotesRecord {
    pub lecture_key: String,
    pub course_key: String,
    pub semester: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub sequence_no: i64,
    pub status: LectureNotesStatus,
    pub transcript_sha256: String,
    pub conversation_id: Option<String>,
    pub notes_title: Option<String>,
    pub notes_markdown: Option<String>,
    pub notes_sha256: Option<String>,
    pub notes_final_ordinal: Option<i64>,
    pub title_requested_at: Option<String>,
    pub github_audio_path: Option<String>,
    pub github_notes_path: Option<String>,
    pub github_commit_sha: Option<String>,
    pub last_error: Option<String>,
    pub recovery_kind: Option<String>,
    pub submit_attempt_count: i64,
    pub submitted_at: Option<String>,
    pub completed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedTranscript {
    pub text_display: String,
    pub text_accu: String,
    pub tokens: Vec<String>,
    pub timestamps: Vec<f64>,
    pub duration_seconds: f64,
    pub raw_task_output: Value,
}

#[derive(Debug, Clone)]
pub struct StoredObject {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl From<&TaskRecord> for TaskSummaryResponse {
    fn from(task: &TaskRecord) -> Self {
        Self {
            id: task.id.clone(),
            batch_id: task.batch_id.clone(),
            status: task.status.clone(),
            stage: task.stage.clone(),
            semester: task.semester.clone(),
            course_key: task.course_key.clone(),
            course_name: task.course_name.clone(),
            teacher_name: task.teacher_name.clone(),
            date: task.date.clone(),
            start_time: task.start_time.clone(),
            end_time: task.end_time.clone(),
            last_error: task.last_error.clone(),
            progress_percent: task.progress_percent,
            transferred_bytes: task.transferred_bytes,
            total_bytes: task.total_bytes,
            rate_bytes_per_sec: task.rate_bytes_per_sec,
            eta_seconds: task.eta_seconds,
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }
}

impl From<&TaskRecord> for TaskDetailTaskResponse {
    fn from(task: &TaskRecord) -> Self {
        Self {
            id: task.id.clone(),
            batch_id: task.batch_id.clone(),
            status: task.status.clone(),
            stage: task.stage.clone(),
            semester: task.semester.clone(),
            course_key: task.course_key.clone(),
            course_name: task.course_name.clone(),
            teacher_name: task.teacher_name.clone(),
            date: task.date.clone(),
            start_time: task.start_time.clone(),
            end_time: task.end_time.clone(),
            new_id: task.new_id.clone(),
            page_url: task.page_url.clone(),
            mp4_url: task.mp4_url.clone(),
            last_error: task.last_error.clone(),
            progress_percent: task.progress_percent,
            transferred_bytes: task.transferred_bytes,
            total_bytes: task.total_bytes,
            rate_bytes_per_sec: task.rate_bytes_per_sec,
            eta_seconds: task.eta_seconds,
            segment_markdown_path: task.segment_markdown_path.clone(),
            segment_json_path: task.segment_json_path.clone(),
            course_manifest_path: task.course_manifest_path.clone(),
            merged_markdown_path: task.merged_markdown_path.clone(),
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }
}
