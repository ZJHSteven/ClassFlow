/*!
SQLite 持久层。

这里不使用 `sqlx::query!` 宏，而是统一使用运行时 SQL，原因有两个：

1. 当前项目刚起步，避免为了离线编译和 schema 预生成再引入额外复杂度。
2. 测试里会频繁创建临时数据库，运行时 SQL 更灵活。
*/

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool, sqlite::SqlitePoolOptions};
use uuid::Uuid;

use crate::{
    course::{build_course_key, build_segment_key, sort_tasks_for_course},
    error::{AppError, AppResult},
    models::{
        CourseDetailResponse, CourseListQuery, CourseSummaryResponse, IntakeBatchRequest,
        IntakeBatchResponse, TaskDetailResponse, TaskEventRecord, TaskListQuery, TaskRecord,
        TaskStage, TaskStatus,
    },
};

pub struct TaskSuccessUpdate<'a> {
    pub uploaded_source_url: &'a str,
    pub transcript_text: &'a str,
    pub transcript_json: &'a serde_json::Value,
    pub segment_markdown_path: &'a str,
    pub segment_json_path: &'a str,
    pub course_manifest_path: &'a str,
    pub merged_markdown_path: &'a str,
}

pub struct TaskTransferUpdate {
    pub progress_percent: Option<f64>,
    pub transferred_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub rate_bytes_per_sec: Option<u64>,
    pub eta_seconds: Option<u64>,
}

#[derive(Clone)]
pub struct Repository {
    pool: SqlitePool,
}

impl Repository {
    pub async fn connect(db_url: &str) -> AppResult<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(10)
            .connect(db_url)
            .await?;

        let repo = Self { pool };
        repo.init_schema().await?;
        Ok(repo)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn init_schema(&self) -> AppResult<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS batches (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                semester TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                batch_id TEXT NOT NULL,
                segment_key TEXT NOT NULL UNIQUE,
                source TEXT NOT NULL,
                status TEXT NOT NULL,
                stage TEXT NOT NULL,
                semester TEXT NOT NULL,
                course_key TEXT NOT NULL,
                course_name TEXT NOT NULL,
                teacher_name TEXT NOT NULL,
                date TEXT NOT NULL,
                start_time TEXT NOT NULL,
                end_time TEXT NOT NULL,
                page_url TEXT NOT NULL,
                mp4_url TEXT NOT NULL,
                new_id TEXT NOT NULL,
                raw_title TEXT NOT NULL,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                uploaded_source_url TEXT,
                segment_markdown_path TEXT,
                segment_json_path TEXT,
                course_manifest_path TEXT,
                merged_markdown_path TEXT,
                transcript_text TEXT,
                transcript_json TEXT,
                started_at TEXT,
                completed_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_tasks_course_key ON tasks(course_key);
            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_tasks_date ON tasks(date);

            CREATE TABLE IF NOT EXISTS task_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                stage TEXT NOT NULL,
                level TEXT NOT NULL,
                message TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_task_events_task_id ON task_events(task_id);
            "#,
        )
        .execute(&self.pool)
        .await?;

        self.ensure_optional_task_column("progress_percent", "REAL")
            .await?;
        self.ensure_optional_task_column("transferred_bytes", "INTEGER")
            .await?;
        self.ensure_optional_task_column("total_bytes", "INTEGER")
            .await?;
        self.ensure_optional_task_column("rate_bytes_per_sec", "INTEGER")
            .await?;
        self.ensure_optional_task_column("eta_seconds", "INTEGER")
            .await?;

        Ok(())
    }

    async fn ensure_optional_task_column(&self, column_name: &str, column_definition: &str) -> AppResult<()> {
        let rows = sqlx::query("PRAGMA table_info(tasks)")
            .fetch_all(&self.pool)
            .await?;
        let exists = rows
            .iter()
            .any(|row| row.get::<String, _>("name") == column_name);
        if exists {
            return Ok(());
        }

        sqlx::query(&format!(
            "ALTER TABLE tasks ADD COLUMN {column_name} {column_definition}"
        ))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_batch_with_tasks(
        &self,
        request: &IntakeBatchRequest,
        default_semester: &str,
    ) -> AppResult<IntakeBatchResponse> {
        if request.source.trim() != "userscript" {
            return Err(AppError::BadRequest(
                "source 当前仅支持 userscript".to_string(),
            ));
        }
        if request.items.is_empty() {
            return Err(AppError::BadRequest(
                "items 不能为空，至少要传一个片段".to_string(),
            ));
        }

        let now = Utc::now();
        let batch_id = Uuid::new_v4().to_string();
        let semester = request
            .semester
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| default_semester.to_string());

        let mut transaction = self.pool.begin().await?;
        sqlx::query("INSERT INTO batches(id, source, semester, created_at) VALUES(?, ?, ?, ?)")
            .bind(&batch_id)
            .bind(&request.source)
            .bind(&semester)
            .bind(now.to_rfc3339())
            .execute(&mut *transaction)
            .await?;

        let mut task_ids = Vec::new();
        let mut course_keys = Vec::new();

        for item in &request.items {
            validate_intake_item(item)?;
            let course_key =
                build_course_key(&semester, &item.date, &item.course_name, &item.teacher_name);
            let segment_key = build_segment_key(
                &course_key,
                &item.new_id,
                &item.start_time,
                &item.end_time,
                &item.mp4_url,
            );

            let task_id = Uuid::new_v4().to_string();
            let inserted = sqlx::query(
                r#"
                INSERT OR IGNORE INTO tasks (
                    id, batch_id, segment_key, source, status, stage, semester, course_key,
                    course_name, teacher_name, date, start_time, end_time, page_url, mp4_url,
                    new_id, raw_title, created_at, updated_at
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(&task_id)
            .bind(&batch_id)
            .bind(&segment_key)
            .bind(&request.source)
            .bind(TaskStatus::Pending.as_str())
            .bind(TaskStage::Queued.as_str())
            .bind(&semester)
            .bind(&course_key)
            .bind(&item.course_name)
            .bind(&item.teacher_name)
            .bind(&item.date)
            .bind(&item.start_time)
            .bind(&item.end_time)
            .bind(&item.page_url)
            .bind(&item.mp4_url)
            .bind(&item.new_id)
            .bind(&item.raw_title)
            .bind(now.to_rfc3339())
            .bind(now.to_rfc3339())
            .execute(&mut *transaction)
            .await?
            .rows_affected();

            if inserted > 0 {
                task_ids.push(task_id);
                if !course_keys.contains(&course_key) {
                    course_keys.push(course_key);
                }
            }
        }

        transaction.commit().await?;

        Ok(IntakeBatchResponse {
            batch_id,
            accepted_count: task_ids.len(),
            task_ids,
            course_keys,
        })
    }

    pub async fn list_recoverable_task_ids(&self) -> AppResult<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM tasks WHERE status IN ('pending', 'running') ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| row.get::<String, _>("id"))
            .collect())
    }

    pub async fn get_task(&self, task_id: &str) -> AppResult<TaskRecord> {
        let row = sqlx::query("SELECT * FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some(row) => map_task_row(&row),
            None => Err(AppError::NotFound(format!("任务不存在: {task_id}"))),
        }
    }

    pub async fn list_tasks(&self, query: &TaskListQuery) -> AppResult<Vec<TaskRecord>> {
        let mut sql = String::from("SELECT * FROM tasks WHERE 1=1");
        let mut binds = Vec::new();

        if let Some(status) = &query.status {
            sql.push_str(" AND status = ?");
            binds.push(status.clone());
        }
        if let Some(date) = &query.date {
            sql.push_str(" AND date = ?");
            binds.push(date.clone());
        }
        if let Some(course_name) = &query.course_name {
            sql.push_str(" AND course_name = ?");
            binds.push(course_name.clone());
        }
        sql.push_str(" ORDER BY created_at DESC");

        let mut built = sqlx::query(&sql);
        for bind in binds {
            built = built.bind(bind);
        }

        let rows = built.fetch_all(&self.pool).await?;
        rows.into_iter().map(|row| map_task_row(&row)).collect()
    }

    pub async fn get_task_detail(&self, task_id: &str) -> AppResult<TaskDetailResponse> {
        let task = self.get_task(task_id).await?;
        let rows = sqlx::query(
            "SELECT * FROM task_events WHERE task_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;

        let events = rows
            .into_iter()
            .map(|row| -> AppResult<TaskEventRecord> {
                Ok(TaskEventRecord {
                    id: row.get("id"),
                    task_id: row.get("task_id"),
                    stage: row.get("stage"),
                    level: row.get("level"),
                    message: row.get("message"),
                    created_at: parse_datetime(row.get("created_at"))?,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;

        Ok(TaskDetailResponse { task, events })
    }

    pub async fn mark_task_running(&self, task_id: &str, stage: TaskStage) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET status = ?, stage = ?, attempt_count = attempt_count + 1, started_at = ?, updated_at = ?, last_error = NULL, progress_percent = NULL, transferred_bytes = NULL, total_bytes = NULL, rate_bytes_per_sec = NULL, eta_seconds = NULL WHERE id = ?",
        )
        .bind(TaskStatus::Running.as_str())
        .bind(stage.as_str())
        .bind(&now)
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(task_id, stage.as_str(), "info", "任务开始执行")
            .await?;
        Ok(())
    }

    pub async fn update_task_stage(
        &self,
        task_id: &str,
        stage: TaskStage,
        message: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET stage = ?, updated_at = ?, progress_percent = NULL, transferred_bytes = NULL, total_bytes = NULL, rate_bytes_per_sec = NULL, eta_seconds = NULL WHERE id = ?",
        )
            .bind(stage.as_str())
            .bind(&now)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        self.add_task_event(task_id, stage.as_str(), "info", message)
            .await
    }

    /**
     * 写入下载 / 上传阶段的实时进度快照。
     *
     * 这里不会额外写事件日志，因为这些字段更新频率较高。
     * 如果每次更新都插入事件，会很快把事件表刷爆，前端读取也会变得很重。
     */
    pub async fn update_task_transfer_progress(
        &self,
        task_id: &str,
        stage: TaskStage,
        update: TaskTransferUpdate,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET stage = ?, updated_at = ?, progress_percent = ?, transferred_bytes = ?, total_bytes = ?, rate_bytes_per_sec = ?, eta_seconds = ? WHERE id = ?",
        )
        .bind(stage.as_str())
        .bind(&now)
        .bind(update.progress_percent)
        .bind(update.transferred_bytes.map(|value| value as i64))
        .bind(update.total_bytes.map(|value| value as i64))
        .bind(update.rate_bytes_per_sec.map(|value| value as i64))
        .bind(update.eta_seconds.map(|value| value as i64))
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_task_failed(
        &self,
        task_id: &str,
        stage: TaskStage,
        error: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET status = ?, stage = ?, last_error = ?, completed_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(TaskStatus::Failed.as_str())
        .bind(stage.as_str())
        .bind(error)
        .bind(&now)
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(task_id, stage.as_str(), "error", error)
            .await
    }

    /**
     * 保存“上传阶段已经完成”的断点信息。
     *
     * 一旦 `uploaded_source_url` 已经拿到，后续即使任务失败，也没有必要重新上传同一个音频文件。
     * 这里单独落库，就是为了让“上传成功、转写失败”的任务能直接从转写阶段继续。
     */
    pub async fn save_uploaded_source_url(
        &self,
        task_id: &str,
        uploaded_source_url: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE tasks SET uploaded_source_url = ?, updated_at = ? WHERE id = ?")
            .bind(uploaded_source_url)
            .bind(&now)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        self.add_task_event(
            task_id,
            TaskStage::UploadingAudio.as_str(),
            "info",
            "音频上传成功，已保存上传检查点",
        )
        .await
    }

    /**
     * 保存“转写阶段已经完成”的断点信息。
     *
     * 这一步刻意放在“写产物之前”，因为对象存储、课程总稿合并都属于后处理步骤，
     * 成本远低于重新上传和重新调用百炼。只要把转写结果先写进数据库，后续失败就能精准续跑。
     */
    pub async fn save_transcript_checkpoint(
        &self,
        task_id: &str,
        transcript_text: &str,
        transcript_json: &serde_json::Value,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET transcript_text = ?, transcript_json = ?, updated_at = ? WHERE id = ?",
        )
        .bind(transcript_text)
        .bind(transcript_json.to_string())
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(
            task_id,
            TaskStage::Transcribing.as_str(),
            "info",
            "转写完成，已保存转写检查点",
        )
        .await
    }

    pub async fn mark_task_succeeded(
        &self,
        task_id: &str,
        update: TaskSuccessUpdate<'_>,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            UPDATE tasks
            SET status = ?, stage = ?, uploaded_source_url = ?, transcript_text = ?, transcript_json = ?,
                segment_markdown_path = ?, segment_json_path = ?, course_manifest_path = ?,
                merged_markdown_path = ?, progress_percent = NULL, transferred_bytes = NULL,
                total_bytes = NULL, rate_bytes_per_sec = NULL, eta_seconds = NULL,
                completed_at = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(TaskStatus::Succeeded.as_str())
        .bind(TaskStage::Done.as_str())
        .bind(update.uploaded_source_url)
        .bind(update.transcript_text)
        .bind(update.transcript_json.to_string())
        .bind(update.segment_markdown_path)
        .bind(update.segment_json_path)
        .bind(update.course_manifest_path)
        .bind(update.merged_markdown_path)
        .bind(&now)
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(task_id, TaskStage::Done.as_str(), "info", "任务执行成功")
            .await
    }

    pub async fn add_task_event(
        &self,
        task_id: &str,
        stage: &str,
        level: &str,
        message: &str,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO task_events(task_id, stage, level, message, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(task_id)
        .bind(stage)
        .bind(level)
        .bind(message)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn retry_task(&self, task_id: &str) -> AppResult<()> {
        let task = self.get_task(task_id).await?;
        if task.status != TaskStatus::Failed {
            return Err(AppError::BadRequest("只有失败任务才能重试".to_string()));
        }

        sqlx::query(
            "UPDATE tasks SET status = ?, stage = ?, last_error = NULL, completed_at = NULL, updated_at = ?, progress_percent = NULL, transferred_bytes = NULL, total_bytes = NULL, rate_bytes_per_sec = NULL, eta_seconds = NULL WHERE id = ?",
        )
        .bind(TaskStatus::Pending.as_str())
        .bind(TaskStage::Queued.as_str())
        .bind(Utc::now().to_rfc3339())
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(
            task_id,
            TaskStage::Queued.as_str(),
            "info",
            "任务已重新入队",
        )
        .await
    }

    /**
     * 彻底删除某个任务及其事件日志。
     *
     * 当前主要用于“失败任务我不再重试，直接清掉”的场景。
     */
    pub async fn delete_task_and_events(&self, task_id: &str) -> AppResult<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM task_events WHERE task_id = ?")
            .bind(task_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(task_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /**
     * 裁剪 SQLite 中的任务事件日志。
     *
     * 分两步：
     *
     * 1. 先删掉超过保留天数的旧日志。
     * 2. 再按“每个任务最多保留 N 条”裁掉最早事件。
     *
     * 这样能同时控制“绝对时间跨度”和“单任务异常刷屏”两类膨胀来源。
     */
    pub async fn prune_task_events(
        &self,
        retention_days: u64,
        max_rows_per_task: u64,
    ) -> AppResult<()> {
        let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
        sqlx::query("DELETE FROM task_events WHERE created_at < ?")
            .bind(cutoff.to_rfc3339())
            .execute(&self.pool)
            .await?;

        sqlx::query(
            r#"
            DELETE FROM task_events
            WHERE id IN (
                SELECT id FROM (
                    SELECT
                        id,
                        ROW_NUMBER() OVER (
                            PARTITION BY task_id
                            ORDER BY created_at DESC, id DESC
                        ) AS row_num
                    FROM task_events
                )
                WHERE row_num > ?
            )
            "#,
        )
        .bind(max_rows_per_task as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_tasks_by_course_key(&self, course_key: &str) -> AppResult<Vec<TaskRecord>> {
        let rows = sqlx::query("SELECT * FROM tasks WHERE course_key = ? ORDER BY start_time ASC, new_id ASC, created_at ASC")
            .bind(course_key)
            .fetch_all(&self.pool)
            .await?;
        let mut tasks = rows
            .into_iter()
            .map(|row| map_task_row(&row))
            .collect::<AppResult<Vec<_>>>()?;
        sort_tasks_for_course(&mut tasks);
        Ok(tasks)
    }

    pub async fn list_courses(
        &self,
        query: &CourseListQuery,
    ) -> AppResult<Vec<CourseSummaryResponse>> {
        let tasks = self
            .list_tasks(&TaskListQuery {
                status: None,
                date: query.date.clone(),
                course_name: query.course_name.clone(),
            })
            .await?;

        let mut grouped = std::collections::BTreeMap::<String, Vec<TaskRecord>>::new();
        for task in tasks {
            if let Some(semester) = &query.semester
                && &task.semester != semester
            {
                continue;
            }
            grouped
                .entry(task.course_key.clone())
                .or_default()
                .push(task);
        }

        grouped
            .into_values()
            .map(|mut course_tasks| {
                sort_tasks_for_course(&mut course_tasks);
                summarize_course(&course_tasks)
            })
            .collect()
    }

    pub async fn get_course_detail(&self, course_key: &str) -> AppResult<CourseDetailResponse> {
        let tasks = self.list_tasks_by_course_key(course_key).await?;
        if tasks.is_empty() {
            return Err(AppError::NotFound(format!("课程不存在: {course_key}")));
        }

        let summary = summarize_course(&tasks)?;
        Ok(CourseDetailResponse {
            course_key: summary.course_key,
            semester: summary.semester,
            course_name: summary.course_name,
            teacher_name: summary.teacher_name,
            date: summary.date,
            received_segment_count: summary.received_segment_count,
            successful_segment_count: summary.successful_segment_count,
            has_failed_segment: summary.has_failed_segment,
            merged_markdown_path: summary.merged_markdown_path,
            manifest_path: summary.manifest_path,
            segments: tasks.iter().map(Into::into).collect(),
        })
    }
}

fn validate_intake_item(item: &crate::models::IntakeItem) -> AppResult<()> {
    for (label, value) in [
        ("new_id", &item.new_id),
        ("page_url", &item.page_url),
        ("mp4_url", &item.mp4_url),
        ("course_name", &item.course_name),
        ("teacher_name", &item.teacher_name),
        ("date", &item.date),
        ("start_time", &item.start_time),
        ("end_time", &item.end_time),
        ("raw_title", &item.raw_title),
    ] {
        if value.trim().is_empty() {
            return Err(AppError::BadRequest(format!("{label} 不能为空")));
        }
    }
    Ok(())
}

fn parse_status(value: &str) -> AppResult<TaskStatus> {
    match value {
        "pending" => Ok(TaskStatus::Pending),
        "running" => Ok(TaskStatus::Running),
        "succeeded" => Ok(TaskStatus::Succeeded),
        "failed" => Ok(TaskStatus::Failed),
        other => Err(AppError::Internal(format!("未知任务状态: {other}"))),
    }
}

fn parse_stage(value: &str) -> AppResult<TaskStage> {
    match value {
        "queued" => Ok(TaskStage::Queued),
        "downloading" => Ok(TaskStage::Downloading),
        "extracting_audio" => Ok(TaskStage::ExtractingAudio),
        "uploading_audio" => Ok(TaskStage::UploadingAudio),
        "transcribing" => Ok(TaskStage::Transcribing),
        "storing_artifacts" => Ok(TaskStage::StoringArtifacts),
        "merging_course" => Ok(TaskStage::MergingCourse),
        "cleanup" => Ok(TaskStage::Cleanup),
        "done" => Ok(TaskStage::Done),
        other => Err(AppError::Internal(format!("未知任务阶段: {other}"))),
    }
}

fn parse_datetime(value: String) -> AppResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|date| date.with_timezone(&Utc))
        .map_err(|error| AppError::Internal(format!("日期时间解析失败: {error}, value={value}")))
}

fn parse_optional_datetime(value: Option<String>) -> AppResult<Option<DateTime<Utc>>> {
    value.map(parse_datetime).transpose()
}

fn map_task_row(row: &sqlx::sqlite::SqliteRow) -> AppResult<TaskRecord> {
    Ok(TaskRecord {
        id: row.get("id"),
        batch_id: row.get("batch_id"),
        segment_key: row.get("segment_key"),
        source: row.get("source"),
        status: parse_status(row.get::<String, _>("status").as_str())?,
        stage: parse_stage(row.get::<String, _>("stage").as_str())?,
        semester: row.get("semester"),
        course_key: row.get("course_key"),
        course_name: row.get("course_name"),
        teacher_name: row.get("teacher_name"),
        date: row.get("date"),
        start_time: row.get("start_time"),
        end_time: row.get("end_time"),
        page_url: row.get("page_url"),
        mp4_url: row.get("mp4_url"),
        new_id: row.get("new_id"),
        raw_title: row.get("raw_title"),
        attempt_count: row.get("attempt_count"),
        last_error: row.get("last_error"),
        uploaded_source_url: row.get("uploaded_source_url"),
        segment_markdown_path: row.get("segment_markdown_path"),
        segment_json_path: row.get("segment_json_path"),
        course_manifest_path: row.get("course_manifest_path"),
        merged_markdown_path: row.get("merged_markdown_path"),
        transcript_text: row.get("transcript_text"),
        transcript_json: row
            .get::<Option<String>, _>("transcript_json")
            .and_then(|value| serde_json::from_str(&value).ok()),
        progress_percent: row.try_get("progress_percent").ok(),
        transferred_bytes: row.try_get("transferred_bytes").ok(),
        total_bytes: row.try_get("total_bytes").ok(),
        rate_bytes_per_sec: row.try_get("rate_bytes_per_sec").ok(),
        eta_seconds: row.try_get("eta_seconds").ok(),
        started_at: parse_optional_datetime(row.get("started_at"))?,
        completed_at: parse_optional_datetime(row.get("completed_at"))?,
        created_at: parse_datetime(row.get("created_at"))?,
        updated_at: parse_datetime(row.get("updated_at"))?,
    })
}

fn summarize_course(tasks: &[TaskRecord]) -> AppResult<CourseSummaryResponse> {
    let first = tasks
        .first()
        .ok_or_else(|| AppError::Internal("课程聚合时没有任务".to_string()))?;

    let successful_segment_count = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Succeeded)
        .count();
    let has_failed_segment = tasks.iter().any(|task| task.status == TaskStatus::Failed);
    let updated_at = tasks
        .iter()
        .map(|task| task.updated_at)
        .max()
        .unwrap_or(first.updated_at);

    Ok(CourseSummaryResponse {
        course_key: first.course_key.clone(),
        semester: first.semester.clone(),
        course_name: first.course_name.clone(),
        teacher_name: first.teacher_name.clone(),
        date: first.date.clone(),
        received_segment_count: tasks.len(),
        successful_segment_count,
        has_failed_segment,
        merged_markdown_path: tasks
            .iter()
            .find_map(|task| task.merged_markdown_path.clone()),
        manifest_path: tasks
            .iter()
            .find_map(|task| task.course_manifest_path.clone()),
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use crate::models::{IntakeBatchRequest, IntakeItem};

    use super::*;

    async fn repo() -> Repository {
        Repository::connect("sqlite::memory:")
            .await
            .expect("内存数据库应该创建成功")
    }

    fn demo_request() -> IntakeBatchRequest {
        IntakeBatchRequest {
            semester: None,
            source: "userscript".into(),
            items: vec![IntakeItem {
                new_id: "123".into(),
                page_url: "https://example.test/page".into(),
                mp4_url: "https://example.test/video.mp4".into(),
                course_name: "病理学".into(),
                teacher_name: "王老师".into(),
                date: "2026-03-20".into(),
                start_time: "08:00".into(),
                end_time: "08:45".into(),
                raw_title: "病理学 王老师".into(),
            }],
        }
    }

    #[tokio::test]
    async fn should_create_batch_and_task() {
        let repo = repo().await;
        let response = repo
            .create_batch_with_tasks(&demo_request(), "2025-2026-2")
            .await
            .expect("写入任务应该成功");

        assert_eq!(response.accepted_count, 1);
        let tasks = repo
            .list_tasks(&TaskListQuery {
                status: None,
                date: None,
                course_name: None,
            })
            .await
            .expect("查询任务应该成功");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn should_deduplicate_duplicate_segment() {
        let repo = repo().await;
        let request = demo_request();

        let first = repo
            .create_batch_with_tasks(&request, "2025-2026-2")
            .await
            .expect("第一次写入成功");
        let second = repo
            .create_batch_with_tasks(&request, "2025-2026-2")
            .await
            .expect("第二次写入也应该返回成功响应");

        assert_eq!(first.accepted_count, 1);
        assert_eq!(second.accepted_count, 0);
    }

    #[tokio::test]
    async fn should_retry_failed_task() {
        let repo = repo().await;
        let response = repo
            .create_batch_with_tasks(&demo_request(), "2025-2026-2")
            .await
            .expect("写入成功");
        let task_id = &response.task_ids[0];

        repo.mark_task_failed(task_id, TaskStage::Transcribing, "模拟失败")
            .await
            .expect("应能标记失败");
        repo.retry_task(task_id).await.expect("应能重试");

        let task = repo.get_task(task_id).await.expect("任务应存在");
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.stage, TaskStage::Queued);
        assert!(task.last_error.is_none());
    }

    #[tokio::test]
    async fn should_prune_task_events_by_count() {
        let repo = repo().await;
        let response = repo
            .create_batch_with_tasks(&demo_request(), "2025-2026-2")
            .await
            .expect("写入成功");
        let task_id = &response.task_ids[0];

        for index in 0..5 {
            repo.add_task_event(
                task_id,
                TaskStage::Queued.as_str(),
                "info",
                &format!("event-{index}"),
            )
            .await
            .expect("写事件应成功");
        }

        repo.prune_task_events(30, 2).await.expect("裁剪事件应成功");

        let detail = repo.get_task_detail(task_id).await.expect("查询详情应成功");
        assert!(detail.events.len() <= 2);
        assert_eq!(
            detail.events.last().map(|event| event.message.as_str()),
            Some("event-4")
        );
    }
}
