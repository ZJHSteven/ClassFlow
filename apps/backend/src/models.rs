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
    UploadingAudio,
    Transcribing,
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
            Self::UploadingAudio => "uploading_audio",
            Self::Transcribing => "transcribing",
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
pub struct TaskDetailResponse {
    pub task: TaskRecord,
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
    pub segments: Vec<TaskSummaryResponse>,
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
