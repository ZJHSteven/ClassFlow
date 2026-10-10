/*!
SQLite 持久层。

这里不使用 `sqlx::query!` 宏，而是统一使用运行时 SQL，原因有两个：

1. 当前项目刚起步，避免为了离线编译和 schema 预生成再引入额外复杂度。
2. 测试里会频繁创建临时数据库，运行时 SQL 更灵活。
*/

use std::{str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{
    Column, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use uuid::Uuid;

use crate::{
    course::{
        LectureGroup, build_course_key, build_segment_key, group_tasks_into_lectures,
        sort_tasks_for_course, summarize_lecture,
    },
    error::{AppError, AppResult},
    models::{
        CourseDetailResponse, CourseListQuery, CourseSummaryResponse, IntakeBatchRequest,
        IntakeBatchResponse, IntakeItem, LectureNotesRecord, LectureNotesStatus,
        TaskDetailResponse, TaskDetailTaskResponse, TaskEventRecord, TaskListQuery, TaskRecord,
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

const TASK_SELECT_COLUMNS: &str = r#"
id, batch_id, segment_key, source, status, stage, semester, course_key,
course_name, teacher_name, date, start_time, end_time, page_url, mp4_url,
new_id, raw_title, attempt_count, last_error, uploaded_source_url,
segment_markdown_path, segment_json_path, course_manifest_path, merged_markdown_path,
transcript_text, transcript_json, started_at, completed_at, created_at, updated_at,
progress_percent, transferred_bytes, total_bytes, rate_bytes_per_sec, eta_seconds,
uploaded_source_url_saved_at, dashscope_task_id, dashscope_task_id_saved_at,
remote_capswriter_job_id, remote_capswriter_job_id_saved_at
"#;

impl Repository {
    pub async fn connect(db_url: &str) -> AppResult<Self> {
        // ClassFlow 会同时有下载进度、ASR 进度、SSE/页面读取和 Collector 写入。
        // SQLite 仍然非常适合这种单机任务队列，但默认 DELETE journal 下多个连接争写时，
        // 很容易在两个 worker 同时结束下载/切阶段时把短暂锁竞争暴露为 SQLITE_BUSY。
        //
        // WAL 让读操作不再挡住 writer；NORMAL 是 WAL 下常用的性能/可靠性平衡；
        // busy_timeout 则让“另一个 writer 只差几十毫秒就提交”的场景等待，而不是直接
        // 杀掉当前任务。真正持续 15 秒以上的锁仍会报错，因此不会掩盖逻辑死锁。
        let connect_options = SqliteConnectOptions::from_str(db_url)?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(15));
        let pool = SqlitePoolOptions::new()
            .max_connections(10)
            .connect_with(connect_options)
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

            CREATE TABLE IF NOT EXISTS lecture_notes (
                lecture_key TEXT PRIMARY KEY,
                course_key TEXT NOT NULL,
                semester TEXT NOT NULL,
                course_name TEXT NOT NULL,
                teacher_name TEXT NOT NULL,
                date TEXT NOT NULL,
                start_time TEXT NOT NULL,
                end_time TEXT NOT NULL,
                sequence_no INTEGER NOT NULL,
                status TEXT NOT NULL,
                transcript_sha256 TEXT NOT NULL,
                conversation_id TEXT,
                notes_title TEXT,
                notes_markdown TEXT,
                notes_sha256 TEXT,
                notes_final_ordinal INTEGER,
                title_requested_at TEXT,
                github_audio_path TEXT,
                github_notes_path TEXT,
                github_commit_sha TEXT,
                last_error TEXT,
                recovery_kind TEXT,
                submit_attempt_count INTEGER NOT NULL DEFAULT 0,
                submitted_at TEXT,
                completed_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_lecture_notes_status
                ON lecture_notes(status, updated_at);
            CREATE INDEX IF NOT EXISTS idx_lecture_notes_course
                ON lecture_notes(semester, course_name, teacher_name, date, start_time);
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
        self.ensure_optional_task_column("uploaded_source_url_saved_at", "TEXT")
            .await?;
        self.ensure_optional_task_column("dashscope_task_id", "TEXT")
            .await?;
        self.ensure_optional_task_column("dashscope_task_id_saved_at", "TEXT")
            .await?;
        self.ensure_optional_task_column("remote_capswriter_job_id", "TEXT")
            .await?;
        self.ensure_optional_task_column("remote_capswriter_job_id_saved_at", "TEXT")
            .await?;
        self.ensure_optional_lecture_notes_column("notes_final_ordinal", "INTEGER")
            .await?;
        self.ensure_optional_lecture_notes_column("title_requested_at", "TEXT")
            .await?;
        self.ensure_optional_lecture_notes_column("recovery_kind", "TEXT")
            .await?;
        // 兼容 Phase 3 早期留下的两类已知失败，让升级后前端和 worker 能立刻用新语义接管。
        sqlx::query(
            r#"
            UPDATE lecture_notes
            SET status='pending', recovery_kind='submission_unknown'
            WHERE status='failed'
              AND recovery_kind IS NULL
              AND last_error LIKE '%提交结果不确定%'
            "#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
            UPDATE lecture_notes
            SET recovery_kind='safe_resubmit'
            WHERE status='failed'
              AND recovery_kind IS NULL
              AND last_error LIKE '%DOM drift detected before navigation%'
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn ensure_optional_task_column(
        &self,
        column_name: &str,
        column_definition: &str,
    ) -> AppResult<()> {
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

    async fn ensure_optional_lecture_notes_column(
        &self,
        column_name: &str,
        column_definition: &str,
    ) -> AppResult<()> {
        let rows = sqlx::query("PRAGMA table_info(lecture_notes)")
            .fetch_all(&self.pool)
            .await?;
        let exists = rows
            .iter()
            .any(|row| row.get::<String, _>("name") == column_name);
        if exists {
            return Ok(());
        }

        sqlx::query(&format!(
            "ALTER TABLE lecture_notes ADD COLUMN {column_name} {column_definition}"
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
        if !matches!(request.source.trim(), "userscript" | "smartclass") {
            return Err(AppError::BadRequest(
                "source 当前仅支持 userscript / smartclass".to_string(),
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

            // `segment_key` 是当前代码生成的规范身份；`new_id` 是上游录像本身的
            // 稳定身份。两者同时检查，可以兼容升级前已经用旧时间字符串算法
            // 写入数据库的任务，防止同一个 NewID 在代码升级后再次被创建。
            let already_exists = sqlx::query_scalar::<_, i64>(
                "SELECT 1 FROM tasks WHERE segment_key = ? OR new_id = ? LIMIT 1",
            )
            .bind(&segment_key)
            .bind(&item.new_id)
            .fetch_optional(&mut *transaction)
            .await?
            .is_some();

            if already_exists {
                continue;
            }

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

    /// 从候选录播中筛出数据库里真正还不存在的片段。
    ///
    /// Collector 会周期性回看最近几天，同一节课被重复看到是正常现象。
    /// 这里复用正式入库完全相同的 `course_key + segment_key` 计算规则，
    /// 避免每 10 分钟都创建一个没有任务的空 batch。
    pub async fn filter_missing_intake_items(
        &self,
        items: &[IntakeItem],
        semester: &str,
    ) -> AppResult<Vec<IntakeItem>> {
        let mut missing = Vec::new();

        for item in items {
            validate_intake_item(item)?;
            let course_key =
                build_course_key(semester, &item.date, &item.course_name, &item.teacher_name);
            let segment_key = build_segment_key(
                &course_key,
                &item.new_id,
                &item.start_time,
                &item.end_time,
                &item.mp4_url,
            );
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT 1 FROM tasks WHERE segment_key = ? OR new_id = ? LIMIT 1",
            )
            .bind(segment_key)
            .bind(&item.new_id)
            .fetch_optional(&self.pool)
            .await?
            .is_some();

            if !exists {
                missing.push(item.clone());
            }
        }

        Ok(missing)
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

    /// 后端启动前把“上一次进程突然停止时残留的 running”归一化为 queued。
    ///
    /// systemd stop、主机重启或进程崩溃都不会有机会把数据库里的 running 清掉；如果直接
    /// 重新入队，前端会在 worker 真正拿到任务前一直显示昨晚最后一次下载速度，甚至出现
    /// “4 个 running、实际只有 2 个 worker”的假象。恢复前统一清掉瞬时传输指标即可；
    /// M4A / source.mp4 / remote job id 等真正 durable checkpoint 完全不动，worker 仍会按
    /// 文件与 checkpoint 自动跳过已经完成的昂贵阶段。
    pub async fn normalize_interrupted_running_tasks(&self) -> AppResult<u64> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE tasks SET status = ?, stage = ?, updated_at = ?, progress_percent = NULL, transferred_bytes = NULL, total_bytes = NULL, rate_bytes_per_sec = NULL, eta_seconds = NULL WHERE status = ?",
        )
        .bind(TaskStatus::Pending.as_str())
        .bind(TaskStage::Queued.as_str())
        .bind(&now)
        .bind(TaskStatus::Running.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn get_task(&self, task_id: &str) -> AppResult<TaskRecord> {
        let row = sqlx::query(&format!(
            "SELECT {TASK_SELECT_COLUMNS} FROM tasks WHERE id = ?"
        ))
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => map_task_row(&row),
            None => Err(AppError::NotFound(format!("任务不存在: {task_id}"))),
        }
    }

    pub async fn list_tasks(&self, query: &TaskListQuery) -> AppResult<Vec<TaskRecord>> {
        let mut sql = format!("SELECT {TASK_SELECT_COLUMNS} FROM tasks WHERE 1=1");
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
        sql.push_str(" ORDER BY date DESC, start_time DESC, created_at DESC");

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

        Ok(TaskDetailResponse {
            task: TaskDetailTaskResponse::from(&task),
            events,
        })
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

    /// 把可安全恢复的外部依赖失败变成“等待重试”，而不是永久失败。
    ///
    /// status 使用 pending，因此后端重启后 `list_recoverable_task_ids()` 会自动重新入队；
    /// stage 单独使用 retry_waiting，前端可以明确告诉用户“任务没丢，只是在等远端恢复”。
    pub async fn mark_task_retry_waiting(&self, task_id: &str, reason: &str) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET status = ?, stage = ?, last_error = ?, completed_at = NULL, updated_at = ?, progress_percent = NULL, transferred_bytes = NULL, total_bytes = NULL, rate_bytes_per_sec = NULL, eta_seconds = NULL WHERE id = ?",
        )
        .bind(TaskStatus::Pending.as_str())
        .bind(TaskStage::RetryWaiting.as_str())
        .bind(reason)
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(task_id, TaskStage::RetryWaiting.as_str(), "warning", reason)
            .await
    }

    /**
     * 保存“上传阶段已经完成”的断点信息。
     *
     * 一旦 `uploaded_source_url` 已经拿到，短时间内的重试可以直接复用它，
     * 这样“上传成功、转写失败”的任务不用立刻重新上传同一个音频文件。
     *
     * 注意：百炼官方临时 `oss://` URL 只有 48 小时有效期。
     * 所以这里必须同时保存检查点写入时间，worker 后续才能判断它是否已经过期。
     */
    pub async fn save_uploaded_source_url(
        &self,
        task_id: &str,
        uploaded_source_url: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET uploaded_source_url = ?, uploaded_source_url_saved_at = ?, updated_at = ? WHERE id = ?",
        )
            .bind(uploaded_source_url)
            .bind(&now)
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
     * 保存“百炼异步转写任务已经提交”的断点信息。
     *
     * 这一步必须紧跟百炼返回 `task_id` 之后执行。这样即使本机随后因为
     * Wi-Fi 漫游断线、进程重启或轮询失败，后续也可以先继续查询同一个
     * 百炼任务，而不是立刻重新提交一次音频转写。
     */
    pub async fn save_dashscope_task_checkpoint(
        &self,
        task_id: &str,
        dashscope_task_id: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET dashscope_task_id = ?, dashscope_task_id_saved_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(dashscope_task_id)
        .bind(&now)
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(
            task_id,
            TaskStage::Transcribing.as_str(),
            "info",
            &format!("百炼任务已提交，已保存 task_id 检查点: {dashscope_task_id}"),
        )
        .await
    }

    /**
     * 清理已经失效的百炼 `task_id` 检查点。
     *
     * 百炼云端任务不会永久保存；当本地检查点超过安全窗口，或百炼明确返回
     * 404 / 不存在 / 过期时，必须清掉旧值，让后续流程可以重新提交转写。
     */
    pub async fn clear_dashscope_task_checkpoint(
        &self,
        task_id: &str,
        reason: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET dashscope_task_id = NULL, dashscope_task_id_saved_at = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(task_id, TaskStage::Transcribing.as_str(), "info", reason)
            .await
    }

    /// 保存 Windows CapsWriterGateway 已接受的异步 job id。
    pub async fn save_remote_capswriter_job_checkpoint(
        &self,
        task_id: &str,
        job_id: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET remote_capswriter_job_id = ?, remote_capswriter_job_id_saved_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(job_id)
        .bind(&now)
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(
            task_id,
            TaskStage::RemoteAsrQueued.as_str(),
            "info",
            &format!("远端 ASR job 已提交并保存检查点: {job_id}"),
        )
        .await
    }

    /// Gateway 重启后内存 job 可能不存在，或 job 已进入可重试 failed。
    /// 这里只清远端 job id，不碰本地 M4A checkpoint。
    pub async fn clear_remote_capswriter_job_checkpoint(
        &self,
        task_id: &str,
        reason: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE tasks SET remote_capswriter_job_id = NULL, remote_capswriter_job_id_saved_at = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        self.add_task_event(
            task_id,
            TaskStage::WaitingRemoteAsr.as_str(),
            "warning",
            reason,
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
            SET status = ?, stage = ?, uploaded_source_url = ?,
                uploaded_source_url_saved_at = COALESCE(uploaded_source_url_saved_at, ?),
                transcript_text = ?, transcript_json = ?,
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
        .bind(&now)
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
        let rows = sqlx::query(&format!(
            "SELECT {TASK_SELECT_COLUMNS} FROM tasks WHERE course_key = ? ORDER BY start_time ASC, new_id ASC, created_at ASC"
        ))
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

        let mut summaries = grouped
            .into_values()
            .map(|mut course_tasks| {
                sort_tasks_for_course(&mut course_tasks);
                summarize_course(&course_tasks)
            })
            .collect::<AppResult<Vec<_>>>()?;

        summaries.sort_by(|left, right| {
            right
                .date
                .cmp(&left.date)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| left.course_name.cmp(&right.course_name))
                .then_with(|| left.teacher_name.cmp(&right.teacher_name))
        });

        Ok(summaries)
    }

    pub async fn get_course_detail(&self, course_key: &str) -> AppResult<CourseDetailResponse> {
        let tasks = self.list_tasks_by_course_key(course_key).await?;
        if tasks.is_empty() {
            return Err(AppError::NotFound(format!("课程不存在: {course_key}")));
        }

        let summary = summarize_course(&tasks)?;
        let lectures = group_tasks_into_lectures(&tasks);
        let mut lecture_summaries = lectures.iter().map(summarize_lecture).collect::<Vec<_>>();
        for summary in &mut lecture_summaries {
            if let Some(notes) = self.get_lecture_notes(&summary.lecture_key).await? {
                summary.notes_status = notes.status.as_str().to_string();
                summary.notes_title = notes.notes_title.clone();
                summary.notes_ready = notes.status == LectureNotesStatus::Succeeded;
                summary.notes_last_error = notes.last_error.clone();
                summary.notes_recovery_kind = notes.recovery_kind.clone();
                summary.notes_conversation_id = notes.conversation_id.clone();
                summary.notes_submitted_at = notes.submitted_at.clone();
                summary.notes_github_path = notes.github_notes_path.clone();
            }
        }

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
            lectures: lecture_summaries,
            segments: tasks.iter().map(Into::into).collect(),
        })
    }

    pub async fn get_lecture_group(
        &self,
        course_key: &str,
        lecture_key: &str,
    ) -> AppResult<LectureGroup> {
        let tasks = self.list_tasks_by_course_key(course_key).await?;
        if tasks.is_empty() {
            return Err(AppError::NotFound(format!("课程不存在: {course_key}")));
        }
        group_tasks_into_lectures(&tasks)
            .into_iter()
            .find(|lecture| lecture.lecture_key == lecture_key)
            .ok_or_else(|| AppError::NotFound(format!("完整课堂不存在: {lecture_key}")))
    }

    pub async fn list_course_keys_since(&self, since_date: &str) -> AppResult<Vec<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT course_key FROM tasks WHERE date >= ? ORDER BY date ASC, start_time ASC",
        )
        .bind(since_date)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn get_lecture_notes(
        &self,
        lecture_key: &str,
    ) -> AppResult<Option<LectureNotesRecord>> {
        let row = sqlx::query("SELECT * FROM lecture_notes WHERE lecture_key = ? LIMIT 1")
            .bind(lecture_key)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(map_lecture_notes_row).transpose()
    }

    pub async fn list_lecture_note_titles_for_course(
        &self,
        course_name: &str,
        exclude_lecture_key: &str,
    ) -> AppResult<Vec<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            r#"
            SELECT notes_title
            FROM lecture_notes
            WHERE course_name = ?
              AND lecture_key <> ?
              AND notes_title IS NOT NULL
              AND trim(notes_title) <> ''
              AND status IN ('publishing', 'succeeded')
            ORDER BY sequence_no ASC
            "#,
        )
        .bind(course_name)
        .bind(exclude_lecture_key)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn ensure_lecture_notes_job(
        &self,
        lecture: &LectureGroup,
        transcript_sha256: &str,
    ) -> AppResult<LectureNotesRecord> {
        if let Some(existing) = self.get_lecture_notes(&lecture.lecture_key).await? {
            return Ok(existing);
        }

        let next_sequence = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COALESCE(MAX(sequence_no), 0) + 1
            FROM lecture_notes
            WHERE semester = ? AND course_name = ? AND teacher_name = ?
            "#,
        )
        .bind(&lecture.semester)
        .bind(&lecture.course_name)
        .bind(&lecture.teacher_name)
        .fetch_one(&self.pool)
        .await?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO lecture_notes(
                lecture_key, course_key, semester, course_name, teacher_name, date,
                start_time, end_time, sequence_no, status, transcript_sha256,
                created_at, updated_at
            ) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)
            "#,
        )
        .bind(&lecture.lecture_key)
        .bind(&lecture.course_key)
        .bind(&lecture.semester)
        .bind(&lecture.course_name)
        .bind(&lecture.teacher_name)
        .bind(&lecture.date)
        .bind(&lecture.start_time)
        .bind(&lecture.end_time)
        .bind(next_sequence)
        .bind(transcript_sha256)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        self.get_lecture_notes(&lecture.lecture_key)
            .await?
            .ok_or_else(|| AppError::Internal("创建 lecture Notes 任务后无法读回".to_string()))
    }

    pub async fn list_active_lecture_notes(
        &self,
        limit: i64,
    ) -> AppResult<Vec<LectureNotesRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM lecture_notes
            WHERE status IN ('pending', 'generating', 'publishing')
            ORDER BY
              CASE status WHEN 'generating' THEN 0 WHEN 'publishing' THEN 1 ELSE 2 END,
              updated_at ASC
            LIMIT ?
            "#,
        )
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(map_lecture_notes_row).collect()
    }

    pub async fn mark_lecture_notes_pending_error(
        &self,
        lecture_key: &str,
        error: &str,
    ) -> AppResult<()> {
        sqlx::query("UPDATE lecture_notes SET status='pending', last_error=?, recovery_kind=NULL, updated_at=? WHERE lecture_key=?")
            .bind(error)
            .bind(Utc::now().to_rfc3339())
            .bind(lecture_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_lecture_notes_generating(
        &self,
        lecture_key: &str,
        conversation_id: &str,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE lecture_notes SET status='generating', conversation_id=?, submit_attempt_count=submit_attempt_count+1, submitted_at=COALESCE(submitted_at, ?), last_error=NULL, recovery_kind=NULL, updated_at=? WHERE lecture_key=?",
        )
        .bind(conversation_id)
        .bind(&now)
        .bind(&now)
        .bind(lecture_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_lecture_notes_failed(&self, lecture_key: &str, error: &str) -> AppResult<()> {
        self.mark_lecture_notes_failed_with_kind(lecture_key, error, "manual_review")
            .await
    }

    pub async fn mark_lecture_notes_failed_with_kind(
        &self,
        lecture_key: &str,
        error: &str,
        recovery_kind: &str,
    ) -> AppResult<()> {
        sqlx::query("UPDATE lecture_notes SET status='failed', last_error=?, recovery_kind=?, updated_at=? WHERE lecture_key=?")
            .bind(error)
            .bind(recovery_kind)
            .bind(Utc::now().to_rfc3339())
            .bind(lecture_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_lecture_notes_submission_unknown(
        &self,
        lecture_key: &str,
        error: &str,
    ) -> AppResult<()> {
        sqlx::query(
            "UPDATE lecture_notes SET status='pending', last_error=?, recovery_kind='submission_unknown', updated_at=? WHERE lecture_key=?",
        )
        .bind(error)
        .bind(Utc::now().to_rfc3339())
        .bind(lecture_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_lecture_notes_error(
        &self,
        lecture_key: &str,
        error: &str,
    ) -> AppResult<()> {
        sqlx::query("UPDATE lecture_notes SET last_error=?, updated_at=? WHERE lecture_key=?")
            .bind(error)
            .bind(Utc::now().to_rfc3339())
            .bind(lecture_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_lecture_notes_notes_ready(
        &self,
        lecture_key: &str,
        markdown: &str,
        notes_sha256: &str,
        final_ordinal: i64,
    ) -> AppResult<()> {
        sqlx::query("UPDATE lecture_notes SET status='generating', notes_title=NULL, notes_markdown=?, notes_sha256=?, notes_final_ordinal=?, title_requested_at=NULL, last_error=NULL, recovery_kind=NULL, updated_at=? WHERE lecture_key=?")
            .bind(markdown)
            .bind(notes_sha256)
            .bind(final_ordinal)
            .bind(Utc::now().to_rfc3339())
            .bind(lecture_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_lecture_notes_title_requested(&self, lecture_key: &str) -> AppResult<()> {
        sqlx::query("UPDATE lecture_notes SET title_requested_at=?, last_error=NULL, recovery_kind=NULL, updated_at=? WHERE lecture_key=?")
            .bind(Utc::now().to_rfc3339())
            .bind(Utc::now().to_rfc3339())
            .bind(lecture_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_lecture_notes_publishing(
        &self,
        lecture_key: &str,
        title: &str,
    ) -> AppResult<()> {
        sqlx::query("UPDATE lecture_notes SET status='publishing', notes_title=?, last_error=NULL, recovery_kind=NULL, updated_at=? WHERE lecture_key=?")
            .bind(title)
            .bind(Utc::now().to_rfc3339())
            .bind(lecture_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_lecture_notes_succeeded(
        &self,
        lecture_key: &str,
        github_audio_path: &str,
        github_notes_path: &str,
        github_commit_sha: Option<&str>,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE lecture_notes SET status='succeeded', github_audio_path=?, github_notes_path=?, github_commit_sha=?, last_error=NULL, recovery_kind=NULL, completed_at=?, updated_at=? WHERE lecture_key=?")
            .bind(github_audio_path)
            .bind(github_notes_path)
            .bind(github_commit_sha)
            .bind(&now)
            .bind(&now)
            .bind(lecture_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn retry_lecture_notes(&self, lecture_key: &str) -> AppResult<()> {
        let existing = self
            .get_lecture_notes(lecture_key)
            .await?
            .ok_or_else(|| AppError::NotFound("Notes 任务不存在".to_string()))?;
        if existing.status != LectureNotesStatus::Failed {
            return Err(AppError::BadRequest(
                "只有失败的 Notes 任务才能重试".to_string(),
            ));
        }
        sqlx::query(
            if existing.notes_markdown.as_deref().is_some_and(|value| !value.trim().is_empty())
                && existing.conversation_id.as_deref().is_some_and(|value| !value.trim().is_empty())
            {
                "UPDATE lecture_notes SET status='generating', notes_title=NULL, title_requested_at=NULL, last_error=NULL, recovery_kind=NULL, completed_at=NULL, updated_at=? WHERE lecture_key=?"
            } else {
                "UPDATE lecture_notes SET status='pending', conversation_id=NULL, notes_title=NULL, notes_markdown=NULL, notes_sha256=NULL, notes_final_ordinal=NULL, title_requested_at=NULL, github_audio_path=NULL, github_notes_path=NULL, github_commit_sha=NULL, last_error=NULL, recovery_kind=NULL, submitted_at=NULL, completed_at=NULL, updated_at=? WHERE lecture_key=?"
            },
        )
        .bind(Utc::now().to_rfc3339())
        .bind(lecture_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn prepare_lecture_notes_safe_resubmit(&self, lecture_key: &str) -> AppResult<()> {
        let existing = self
            .get_lecture_notes(lecture_key)
            .await?
            .ok_or_else(|| AppError::NotFound("Notes 任务不存在".to_string()))?;
        if existing.recovery_kind.as_deref() != Some("safe_resubmit") {
            return Err(AppError::BadRequest(
                "当前失败没有被证明为“明确未提交”，禁止直接重建 conversation".to_string(),
            ));
        }
        sqlx::query(
            "UPDATE lecture_notes SET status='pending', conversation_id=NULL, notes_title=NULL, notes_markdown=NULL, notes_sha256=NULL, notes_final_ordinal=NULL, title_requested_at=NULL, github_audio_path=NULL, github_notes_path=NULL, github_commit_sha=NULL, last_error=NULL, recovery_kind=NULL, submitted_at=NULL, completed_at=NULL, updated_at=? WHERE lecture_key=?",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(lecture_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn force_lecture_notes_resubmit_after_review(
        &self,
        lecture_key: &str,
    ) -> AppResult<()> {
        let existing = self
            .get_lecture_notes(lecture_key)
            .await?
            .ok_or_else(|| AppError::NotFound("Notes 任务不存在".to_string()))?;
        if existing.recovery_kind.as_deref() != Some("submission_unknown") {
            return Err(AppError::BadRequest(
                "只有“提交结果未知”的任务允许人工确认后强制重建 conversation".to_string(),
            ));
        }
        sqlx::query(
            "UPDATE lecture_notes SET status='pending', conversation_id=NULL, last_error=NULL, recovery_kind=NULL, submitted_at=NULL, completed_at=NULL, updated_at=? WHERE lecture_key=?",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(lecture_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn resume_lecture_notes_existing_conversation(
        &self,
        lecture_key: &str,
    ) -> AppResult<()> {
        let existing = self
            .get_lecture_notes(lecture_key)
            .await?
            .ok_or_else(|| AppError::NotFound("Notes 任务不存在".to_string()))?;
        if existing
            .conversation_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(AppError::BadRequest(
                "当前任务没有已知 conversation_id，不能从已有会话继续".to_string(),
            ));
        }
        sqlx::query(
            "UPDATE lecture_notes SET status='generating', last_error=NULL, recovery_kind=NULL, completed_at=NULL, updated_at=? WHERE lecture_key=?",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(lecture_key)
        .execute(&self.pool)
        .await?;
        Ok(())
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
        "waiting_remote_asr" => Ok(TaskStage::WaitingRemoteAsr),
        "uploading_audio" => Ok(TaskStage::UploadingAudio),
        "remote_asr_queued" => Ok(TaskStage::RemoteAsrQueued),
        "transcribing" => Ok(TaskStage::Transcribing),
        "retry_waiting" => Ok(TaskStage::RetryWaiting),
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

fn row_has_column(row: &sqlx::sqlite::SqliteRow, column_name: &str) -> bool {
    row.columns()
        .iter()
        .any(|column| column.name() == column_name)
}

fn try_get_optional_string(
    row: &sqlx::sqlite::SqliteRow,
    column_name: &str,
) -> AppResult<Option<String>> {
    if !row_has_column(row, column_name) {
        return Ok(None);
    }
    row.try_get::<Option<String>, _>(column_name)
        .map_err(AppError::from)
}

fn try_get_optional_i64(
    row: &sqlx::sqlite::SqliteRow,
    column_name: &str,
) -> AppResult<Option<i64>> {
    if !row_has_column(row, column_name) {
        return Ok(None);
    }
    row.try_get::<Option<i64>, _>(column_name)
        .map_err(AppError::from)
}

fn try_get_optional_f64(
    row: &sqlx::sqlite::SqliteRow,
    column_name: &str,
) -> AppResult<Option<f64>> {
    if !row_has_column(row, column_name) {
        return Ok(None);
    }
    row.try_get::<Option<f64>, _>(column_name)
        .map_err(AppError::from)
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
        uploaded_source_url_saved_at: parse_optional_datetime(try_get_optional_string(
            row,
            "uploaded_source_url_saved_at",
        )?)?,
        dashscope_task_id: try_get_optional_string(row, "dashscope_task_id")?,
        dashscope_task_id_saved_at: parse_optional_datetime(try_get_optional_string(
            row,
            "dashscope_task_id_saved_at",
        )?)?,
        remote_capswriter_job_id: try_get_optional_string(row, "remote_capswriter_job_id")?,
        remote_capswriter_job_id_saved_at: parse_optional_datetime(try_get_optional_string(
            row,
            "remote_capswriter_job_id_saved_at",
        )?)?,
        segment_markdown_path: row.get("segment_markdown_path"),
        segment_json_path: row.get("segment_json_path"),
        course_manifest_path: row.get("course_manifest_path"),
        merged_markdown_path: row.get("merged_markdown_path"),
        transcript_text: row.get("transcript_text"),
        transcript_json: row
            .get::<Option<String>, _>("transcript_json")
            .and_then(|value| serde_json::from_str(&value).ok()),
        progress_percent: try_get_optional_f64(row, "progress_percent")?,
        transferred_bytes: try_get_optional_i64(row, "transferred_bytes")?,
        total_bytes: try_get_optional_i64(row, "total_bytes")?,
        rate_bytes_per_sec: try_get_optional_i64(row, "rate_bytes_per_sec")?,
        eta_seconds: try_get_optional_i64(row, "eta_seconds")?,
        started_at: parse_optional_datetime(row.get("started_at"))?,
        completed_at: parse_optional_datetime(row.get("completed_at"))?,
        created_at: parse_datetime(row.get("created_at"))?,
        updated_at: parse_datetime(row.get("updated_at"))?,
    })
}

fn map_lecture_notes_row(row: &sqlx::sqlite::SqliteRow) -> AppResult<LectureNotesRecord> {
    Ok(LectureNotesRecord {
        lecture_key: row.get("lecture_key"),
        course_key: row.get("course_key"),
        semester: row.get("semester"),
        course_name: row.get("course_name"),
        teacher_name: row.get("teacher_name"),
        date: row.get("date"),
        start_time: row.get("start_time"),
        end_time: row.get("end_time"),
        sequence_no: row.get("sequence_no"),
        status: row
            .get::<String, _>("status")
            .parse::<LectureNotesStatus>()
            .map_err(AppError::Internal)?,
        transcript_sha256: row.get("transcript_sha256"),
        conversation_id: row.get("conversation_id"),
        notes_title: row.get("notes_title"),
        notes_markdown: row.get("notes_markdown"),
        notes_sha256: row.get("notes_sha256"),
        notes_final_ordinal: row.get("notes_final_ordinal"),
        title_requested_at: row.get("title_requested_at"),
        github_audio_path: row.get("github_audio_path"),
        github_notes_path: row.get("github_notes_path"),
        github_commit_sha: row.get("github_commit_sha"),
        last_error: row.get("last_error"),
        recovery_kind: row.get("recovery_kind"),
        submit_attempt_count: row.get("submit_attempt_count"),
        submitted_at: row.get("submitted_at"),
        completed_at: row.get("completed_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
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
    async fn should_deduplicate_legacy_segment_key_by_new_id() {
        let repo = repo().await;
        let request = demo_request();

        let first = repo
            .create_batch_with_tasks(&request, "2025-2026-2")
            .await
            .expect("首次写入应成功");
        assert_eq!(first.accepted_count, 1);

        // 模拟升级前已经落库的旧记录：其 segment_key 来自旧的时间字符串算法，
        // 因而故意与当前规范化算法生成的 key 不同。
        sqlx::query("UPDATE tasks SET segment_key = ? WHERE id = ?")
            .bind("legacy-pre-time-normalization-key")
            .bind(&first.task_ids[0])
            .execute(&repo.pool)
            .await
            .expect("应能构造旧版 segment_key 场景");

        let mut legacy_equivalent = request.items[0].clone();
        legacy_equivalent.start_time = "08:00:00".into();
        legacy_equivalent.end_time = "08:45:00".into();

        let missing = repo
            .filter_missing_intake_items(&[legacy_equivalent.clone()], "2025-2026-2")
            .await
            .expect("Collector 去重检查应成功");
        assert!(missing.is_empty());

        let replay = IntakeBatchRequest {
            semester: None,
            source: "smartclass".into(),
            items: vec![legacy_equivalent],
        };
        let second = repo
            .create_batch_with_tasks(&replay, "2025-2026-2")
            .await
            .expect("旧记录兼容去重应正常返回");
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
    async fn should_normalize_interrupted_running_task_before_startup_recovery() {
        let repo = repo().await;
        let response = repo
            .create_batch_with_tasks(&demo_request(), "2025-2026-2")
            .await
            .expect("写入成功");
        let task_id = &response.task_ids[0];

        repo.mark_task_running(task_id, TaskStage::Downloading)
            .await
            .expect("应能模拟运行中的下载任务");
        repo.update_task_transfer_progress(
            task_id,
            TaskStage::Downloading,
            TaskTransferUpdate {
                progress_percent: Some(42.0),
                transferred_bytes: Some(42_000),
                total_bytes: Some(100_000),
                rate_bytes_per_sec: Some(256_000),
                eta_seconds: Some(1),
            },
        )
        .await
        .expect("应能写入模拟下载进度");

        let normalized = repo
            .normalize_interrupted_running_tasks()
            .await
            .expect("启动恢复归一化应成功");
        assert_eq!(normalized, 1);

        let task = repo.get_task(task_id).await.expect("任务应存在");
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.stage, TaskStage::Queued);
        assert!(task.progress_percent.is_none());
        assert!(task.transferred_bytes.is_none());
        assert!(task.total_bytes.is_none());
        assert!(task.rate_bytes_per_sec.is_none());
        assert!(task.eta_seconds.is_none());
    }

    #[tokio::test]
    async fn sqlite_connections_should_wait_for_short_writer_contention() {
        let repo = repo().await;
        let busy_timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(repo.pool())
            .await
            .expect("应能读取 SQLite busy_timeout");
        assert_eq!(busy_timeout_ms, 15_000);
    }

    #[tokio::test]
    async fn should_persist_remote_job_and_keep_retry_waiting_recoverable() {
        let repo = repo().await;
        let response = repo
            .create_batch_with_tasks(&demo_request(), "2025-2026-2")
            .await
            .expect("写入成功");
        let task_id = &response.task_ids[0];

        repo.save_remote_capswriter_job_checkpoint(task_id, "tr_resume_me")
            .await
            .expect("应保存远端 job checkpoint");
        repo.mark_task_retry_waiting(task_id, "Windows 暂时离线")
            .await
            .expect("应进入等待重试");

        let task = repo.get_task(task_id).await.expect("任务应存在");
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.stage, TaskStage::RetryWaiting);
        assert_eq!(
            task.remote_capswriter_job_id.as_deref(),
            Some("tr_resume_me")
        );
        assert!(task.remote_capswriter_job_id_saved_at.is_some());
        assert_eq!(task.last_error.as_deref(), Some("Windows 暂时离线"));

        let recoverable = repo
            .list_recoverable_task_ids()
            .await
            .expect("应查询可恢复任务");
        assert!(recoverable.iter().any(|id| id == task_id));

        repo.clear_remote_capswriter_job_checkpoint(task_id, "Gateway 已重启")
            .await
            .expect("应清理失效 job");
        let cleared = repo.get_task(task_id).await.expect("任务应存在");
        assert!(cleared.remote_capswriter_job_id.is_none());
        assert!(cleared.remote_capswriter_job_id_saved_at.is_none());
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

    #[tokio::test]
    async fn should_sort_tasks_and_courses_by_latest_date_first() {
        let repo = repo().await;

        let mut older_course_request = demo_request();
        older_course_request.items[0].date = "2026-03-17".into();
        older_course_request.items[0].course_name = "生理学".into();
        older_course_request.items[0].new_id = "older".into();
        older_course_request.items[0].raw_title = "生理学 王老师".into();

        let mut latest_course_request = demo_request();
        latest_course_request.items[0].date = "2026-03-23".into();
        latest_course_request.items[0].start_time = "10:00".into();
        latest_course_request.items[0].end_time = "10:45".into();
        latest_course_request.items[0].new_id = "latest".into();
        latest_course_request.items[0].raw_title = "病理学 王老师 最新".into();

        repo.create_batch_with_tasks(&older_course_request, "2025-2026-2")
            .await
            .expect("旧日期课程写入应成功");
        repo.create_batch_with_tasks(&latest_course_request, "2025-2026-2")
            .await
            .expect("新日期课程写入应成功");

        let tasks = repo
            .list_tasks(&TaskListQuery {
                status: None,
                date: None,
                course_name: None,
            })
            .await
            .expect("任务查询应成功");
        assert_eq!(tasks[0].date, "2026-03-23");
        assert_eq!(tasks[1].date, "2026-03-17");

        let courses = repo
            .list_courses(&CourseListQuery {
                semester: None,
                date: None,
                course_name: None,
            })
            .await
            .expect("课程查询应成功");
        assert_eq!(courses[0].date, "2026-03-23");
        assert_eq!(courses[1].date, "2026-03-17");
    }
}
