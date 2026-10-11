/*!
课程聚合相关的纯函数。

纯函数的价值很高：

1. 不依赖数据库、网络、文件系统，测试非常轻。
2. 路径规则、排序规则、Markdown 模板都能稳定回归。
3. 后端与前端只要围绕同一套字段展示，就不会出现“目录名一套、页面显示另一套”的错位。
*/

use chrono::{NaiveTime, Timelike, Utc};
use serde_json::json;

use crate::models::{
    CourseSummaryResponse, LectureSummaryResponse, NormalizedTranscript, TaskRecord, TaskStage,
    TaskStatus,
};

/// 无课表先验时，Notes worker 在课程最后一个录像结束后继续等待的保守时间。
///
/// 注意：这个常量**不再参与课程分组**。ClassFlow 早已有稳定的
/// `course_key = semester|date|course|teacher` 聚合层，Phase 3 直接复用它；
/// 这里只把 30 分钟当作“课程大概率已经结束”的 fallback grace。
pub const LECTURE_MAX_GAP_MINUTES: i64 = 30;

/// Phase 3 对既有 course 聚合的兼容包装。
///
/// 历史上 ClassFlow 已经按 `course_key` 把同日、同课程、同老师的 SmartClass 片段
/// 聚合成 manifest / merged course。这里保留 `LectureGroup` 类型，是为了不破坏
/// 已上线的 Notes 状态机/API 字段；它不再维护第二套“30 分钟课间/最多 4 段”的业务边界。
#[derive(Debug, Clone)]
pub struct LectureGroup {
    pub lecture_key: String,
    pub semester: String,
    pub course_key: String,
    pub course_name: String,
    pub teacher_name: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub tasks: Vec<TaskRecord>,
}

impl LectureGroup {
    pub fn segment_count(&self) -> usize {
        self.tasks.len()
    }

    /// 只有所有源片段都成功且拥有非空 ASR 时，才允许送进 Notes route。
    pub fn transcript_ready(&self) -> bool {
        let successful_transcripts = self
            .tasks
            .iter()
            .filter(|task| task_has_usable_transcript(task))
            .count();

        successful_transcripts > 0
            && self
                .tasks
                .iter()
                .all(|task| task_has_usable_transcript(task) || task_source_permanently_missing(task))
    }

    /// 已明确永久缺少源录像的分段数量。
    ///
    /// 该数量只用于 Notes/ASR 完整性提示，不会把普通下载失败或 ASR 失败误当成“永远拿不到”。
    pub fn permanently_missing_segment_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task_source_permanently_missing(task))
            .count()
    }

    /// 生成发送给 ChatGPT Project 的**完整课堂转写**。
    ///
    /// 边界只表达录像/课间切分，绝不在这里做摘要、逐块笔记化或知识重排。
    pub fn combined_transcript(&self) -> Option<String> {
        if !self.transcript_ready() {
            return None;
        }

        Some(
            self.tasks
                .iter()
                .enumerate()
                .map(|(index, task)| {
                    if task_source_permanently_missing(task) {
                        return format!(
                            "【第{}节 {}-{}】\n[本段源录像永久缺失，未生成 ASR；不得推断或补写本时段内容]",
                            index + 1,
                            task.start_time,
                            task.end_time,
                        );
                    }
                    format!(
                        "����{}�� {}-{}��\n{}",
                        index + 1,
                        task.start_time,
                        task.end_time,
                        task.transcript_text.as_deref().unwrap_or_default().trim()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }
}

fn task_has_usable_transcript(task: &TaskRecord) -> bool {
    task.status == TaskStatus::Succeeded
        && task
            .transcript_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
}

/// 判断失败分段是否已经明确属于“源录像永久不存在”。
///
/// 只接受下载阶段的强证据，避免把 ASR 失败、临时网络错误等可恢复问题误判成永久缺失。
/// aria2 exit code 3 会落成“资源不存在”；同时兼容以后 downloader 直接暴露 HTTP 404/410。
pub fn task_source_permanently_missing(task: &TaskRecord) -> bool {
    if task.status != TaskStatus::Failed || task.stage != TaskStage::Downloading {
        return false;
    }
    let message = task.last_error.as_deref().unwrap_or_default().to_lowercase();
    message.contains("资源不存在")
        || message.contains("http=404")
        || message.contains("http 404")
        || message.contains("http=410")
        || message.contains("http 410")
}

pub fn summarize_lecture(lecture: &LectureGroup) -> LectureSummaryResponse {
    LectureSummaryResponse {
        lecture_key: lecture.lecture_key.clone(),
        start_time: lecture.start_time.clone(),
        end_time: lecture.end_time.clone(),
        segment_count: lecture.segment_count(),
        successful_segment_count: lecture
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Succeeded)
            .count(),
        transcript_ready: lecture.transcript_ready(),
        notes_status: "not_started".to_string(),
        notes_title: None,
        notes_ready: false,
        notes_last_error: None,
        notes_recovery_kind: None,
        notes_conversation_id: None,
        notes_submitted_at: None,
        notes_github_path: None,
        task_ids: lecture.tasks.iter().map(|task| task.id.clone()).collect(),
    }
}

/// 用户下载/人工检查用的完整 lecture ASR。
///
/// Notes 首次发送与课程库下载共用这一份 Markdown，不再维护第二套轻量正文格式。
/// 这样既保留课程/时间/Lecture Key 等可追踪元数据，也让提交结果未知时能用 Lecture Key
/// 在 Archive 本地索引中安全认领已经创建的 conversation。
pub fn build_lecture_asr_markdown(lecture: &LectureGroup) -> Option<String> {
    let combined = lecture.combined_transcript()?;
    Some(format!(
        "# 完整课堂 ASR\n\n\
        - 学期：{}\n\
        - 课程：{}\n\
        - 老师：{}\n\
        - 日期：{}\n\
        - 时间：{} - {}\n\
        - 录像片段数：{}\n\
        - Lecture Key：{}\n\n\
        ## 转写正文\n\n{}\n",
        lecture.semester,
        lecture.course_name,
        lecture.teacher_name,
        lecture.date,
        lecture.start_time,
        lecture.end_time,
        lecture.segment_count(),
        lecture.lecture_key,
        combined,
    ))
}

pub fn build_course_key(
    semester: &str,
    date: &str,
    course_name: &str,
    teacher_name: &str,
) -> String {
    format!("{semester}|{date}|{course_name}|{teacher_name}")
}

/// 把课程片段时间规范成稳定字符串，避免同一时刻因为展示精度不同而产生不同身份。
///
/// SmartClass 的不同入口历史上同时出现过 `08:00` 和 `08:00:00`。这两个字符串
/// 语义上都是同一个整分时刻，但如果直接参与哈希，就会制造两个不同的
/// `segment_key`，最终让 Collector 把同一录像再次入队。
///
/// 规范化规则刻意保持保守：
///
/// 1. 能识别为标准时刻时，零秒统一写成 `HH:MM`；
/// 2. 秒数非零时保留 `HH:MM:SS`，避免真的存在秒级差异时错误合并；
/// 3. 无法识别的旧数据原样返回，让异常值仍保持原来的可追踪性，而不是静默改义。
pub fn normalize_segment_time(value: &str) -> String {
    let trimmed = value.trim();

    for pattern in ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M"] {
        if let Ok(time) = NaiveTime::parse_from_str(trimmed, pattern) {
            return if time.second() == 0 && time.nanosecond() == 0 {
                time.format("%H:%M").to_string()
            } else if time.nanosecond() == 0 {
                time.format("%H:%M:%S").to_string()
            } else {
                time.format("%H:%M:%S%.f").to_string()
            };
        }
    }

    trimmed.to_string()
}

pub fn build_segment_key(
    course_key: &str,
    new_id: &str,
    start_time: &str,
    end_time: &str,
    mp4_url: &str,
) -> String {
    use sha2::{Digest, Sha256};

    // 片段身份必须基于“时刻语义”而不是调用方恰好采用的显示格式。
    // 这样历史 `08:00` 与 Collector 新返回的 `08:00:00` 会落到同一个哈希。
    let normalized_start_time = normalize_segment_time(start_time);
    let normalized_end_time = normalize_segment_time(end_time);

    let mut hasher = Sha256::new();
    hasher.update(course_key.as_bytes());
    hasher.update(b"::");
    hasher.update(new_id.as_bytes());
    hasher.update(b"::");
    hasher.update(normalized_start_time.as_bytes());
    hasher.update(b"::");
    hasher.update(normalized_end_time.as_bytes());
    hasher.update(b"::");
    hasher.update(mp4_url.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn build_lecture_group(mut tasks: Vec<TaskRecord>) -> LectureGroup {
    sort_tasks_for_course(&mut tasks);
    let first = tasks.first().expect("lecture 至少需要一个片段");
    let last = tasks.last().expect("lecture 至少需要一个片段");

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(first.course_key.as_bytes());
    hasher.update(b"::lecture::");
    // lecture 只在课间窗口关闭后才进入 Notes 队列，因此以后追加正常后续 segment 不应发生；
    // 以第一片段身份作为稳定根，避免“后面多一节”导致已有 Notes key 整体改名。
    hasher.update(first.segment_key.as_bytes());
    hasher.update(b"::");
    hasher.update(normalize_segment_time(&first.start_time).as_bytes());
    let lecture_key = format!("{:x}", hasher.finalize());

    LectureGroup {
        lecture_key,
        semester: first.semester.clone(),
        course_key: first.course_key.clone(),
        course_name: first.course_name.clone(),
        teacher_name: first.teacher_name.clone(),
        date: first.date.clone(),
        start_time: first.start_time.clone(),
        end_time: last.end_time.clone(),
        tasks,
    }
}

/// 把任务映射回 ClassFlow **既有 course 聚合**。
///
/// `course_key` 从项目早期就定义为 `semester|date|course|teacher`，worker 也一直用它
/// 生成 manifest 与 merged/course.md。因此 Phase 3 不应该再发明第二套 lecture 聚类规则。
///
/// 调用方仍可以传任意顺序、甚至混合课程的任务；这里只按 `course_key` 分组，并在每组内
/// 按开始时间排序。这样 Notes、课程库和历史 course artifact 永远共享同一个业务边界。
pub fn group_tasks_into_lectures(tasks: &[TaskRecord]) -> Vec<LectureGroup> {
    let mut ordered = tasks.to_vec();
    ordered.sort_by(|left, right| {
        left.date
            .cmp(&right.date)
            .then_with(|| left.course_name.cmp(&right.course_name))
            .then_with(|| left.teacher_name.cmp(&right.teacher_name))
            .then_with(|| left.start_time.cmp(&right.start_time))
            .then_with(|| left.new_id.cmp(&right.new_id))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut groups = Vec::new();
    let mut current = Vec::new();

    for task in ordered {
        if current
            .first()
            .is_some_and(|first: &TaskRecord| first.course_key != task.course_key)
        {
            groups.push(build_lecture_group(std::mem::take(&mut current)));
        }
        current.push(task);
    }

    if !current.is_empty() {
        groups.push(build_lecture_group(current));
    }
    groups
}

pub fn sanitize_path_part(value: &str) -> String {
    let mut sanitized = value
        .trim()
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect::<String>();

    if sanitized.is_empty() {
        sanitized = "unknown".to_string();
    }

    sanitized
}

pub fn build_course_prefix(task: &TaskRecord) -> String {
    format!(
        "{}/{}/{}-{}",
        sanitize_path_part(&task.semester),
        sanitize_path_part(&task.course_name),
        sanitize_path_part(&task.date),
        sanitize_path_part(&task.teacher_name),
    )
}

pub fn build_segment_paths(task: &TaskRecord) -> (String, String) {
    let prefix = build_course_prefix(task);
    let file_stem = format!(
        "{}-{}",
        sanitize_path_part(&task.start_time),
        sanitize_path_part(&task.end_time)
    );
    (
        format!("{prefix}/segments/{file_stem}.md"),
        format!("{prefix}/segments/{file_stem}.json"),
    )
}

pub fn build_course_paths(task: &TaskRecord) -> (String, String) {
    let prefix = build_course_prefix(task);
    (
        format!("{prefix}/manifest.json"),
        format!("{prefix}/merged/course.md"),
    )
}

pub fn sort_tasks_for_course(tasks: &mut [TaskRecord]) {
    tasks.sort_by(|left, right| {
        left.start_time
            .cmp(&right.start_time)
            .then_with(|| left.new_id.cmp(&right.new_id))
            .then_with(|| left.id.cmp(&right.id))
    });
}

pub fn build_segment_markdown(task: &TaskRecord, transcript: &NormalizedTranscript) -> String {
    format!(
        "# 单节转写备份\n\n\
        - 学期：{}\n\
        - 课程：{}\n\
        - 老师：{}\n\
        - 日期：{}\n\
        - 时间：{} - {}\n\
        - NewID：{}\n\
        - 页面地址：{}\n\
        - 视频地址：{}\n\
        - 生成时间：{}\n\n\
        ## 正文\n\n{}\n",
        task.semester,
        task.course_name,
        task.teacher_name,
        task.date,
        task.start_time,
        task.end_time,
        task.new_id,
        task.page_url,
        task.mp4_url,
        Utc::now().to_rfc3339(),
        transcript.text_accu,
    )
}

pub fn build_merged_markdown(tasks: &[TaskRecord]) -> String {
    let first = tasks.first().expect("构建课程总稿时至少需要一个片段");
    let mut successful_tasks = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Succeeded && task.transcript_text.is_some())
        .cloned()
        .collect::<Vec<_>>();
    sort_tasks_for_course(&mut successful_tasks);

    let segment_lines = successful_tasks
        .iter()
        .map(|task| {
            format!(
                "- {} - {}（任务 {}）",
                task.start_time, task.end_time, task.id
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let sections = successful_tasks
        .iter()
        .map(|task| {
            format!(
                "## {} - {}\n\n{}\n",
                task.start_time,
                task.end_time,
                task.transcript_text.clone().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "# 课程总稿\n\n\
        - 学期：{}\n\
        - 课程：{}\n\
        - 老师：{}\n\
        - 日期：{}\n\
        - 已收片段数：{}\n\
        - 成功片段数：{}\n\
        - 片段列表：\n{}\n\
        - 生成时间：{}\n\n\
        {}\n\
        本稿由 {} 个片段合并生成。\n",
        first.semester,
        first.course_name,
        first.teacher_name,
        first.date,
        tasks.len(),
        successful_tasks.len(),
        segment_lines,
        Utc::now().to_rfc3339(),
        sections,
        successful_tasks.len(),
    )
}

pub fn build_manifest_json(
    tasks: &[TaskRecord],
    summary: &CourseSummaryResponse,
) -> serde_json::Value {
    json!({
        "course_key": summary.course_key,
        "semester": summary.semester,
        "course_name": summary.course_name,
        "teacher_name": summary.teacher_name,
        "date": summary.date,
        "received_segment_count": summary.received_segment_count,
        "successful_segment_count": summary.successful_segment_count,
        "has_failed_segment": summary.has_failed_segment,
        "merged_markdown_path": summary.merged_markdown_path,
        "manifest_path": summary.manifest_path,
        "generated_at": Utc::now().to_rfc3339(),
        "segments": tasks.iter().map(|task| json!({
            "task_id": task.id,
            "status": task.status.as_str(),
            "stage": task.stage.as_str(),
            "start_time": task.start_time,
            "end_time": task.end_time,
            "new_id": task.new_id,
            "segment_markdown_path": task.segment_markdown_path,
            "segment_json_path": task.segment_json_path,
            "last_error": task.last_error,
        })).collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use crate::models::{TaskRecord, TaskStage, TaskStatus};

    use super::*;

    fn demo_task(
        start: &str,
        end: &str,
        status: TaskStatus,
        transcript_text: Option<&str>,
    ) -> TaskRecord {
        TaskRecord {
            id: format!("task-{start}"),
            batch_id: "batch-1".into(),
            segment_key: "segment-1".into(),
            source: "userscript".into(),
            status,
            stage: TaskStage::Done,
            semester: "2025-2026-2".into(),
            course_key: build_course_key("2025-2026-2", "2026-03-20", "病理学", "王老师"),
            course_name: "病理学".into(),
            teacher_name: "王老师".into(),
            date: "2026-03-20".into(),
            start_time: start.into(),
            end_time: end.into(),
            page_url: "https://example.test/page".into(),
            mp4_url: "https://example.test/video.mp4".into(),
            new_id: "123".into(),
            raw_title: "病理学 王老师".into(),
            attempt_count: 1,
            last_error: None,
            uploaded_source_url: None,
            uploaded_source_url_saved_at: None,
            dashscope_task_id: None,
            dashscope_task_id_saved_at: None,
            remote_capswriter_job_id: None,
            remote_capswriter_job_id_saved_at: None,
            segment_markdown_path: Some("segment.md".into()),
            segment_json_path: Some("segment.json".into()),
            course_manifest_path: Some("manifest.json".into()),
            merged_markdown_path: Some("course.md".into()),
            transcript_text: transcript_text.map(ToOwned::to_owned),
            transcript_json: Some(json!({"text":"ok"})),
            progress_percent: None,
            transferred_bytes: None,
            total_bytes: None,
            rate_bytes_per_sec: None,
            eta_seconds: None,
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn should_build_course_key() {
        assert_eq!(
            build_course_key("2025-2026-2", "2026-03-20", "病理学", "王老师"),
            "2025-2026-2|2026-03-20|病理学|王老师"
        );
    }

    #[test]
    fn should_normalize_zero_second_time_without_losing_real_seconds() {
        assert_eq!(normalize_segment_time("08:00"), "08:00");
        assert_eq!(normalize_segment_time("08:00:00"), "08:00");
        assert_eq!(normalize_segment_time("08:00:07"), "08:00:07");
    }

    #[test]
    fn should_treat_hhmm_and_hhmmss_as_the_same_segment_identity() {
        let course_key = build_course_key("2026-2027-1", "2026-09-21", "实验诊断学", "王倩");
        let compact = build_segment_key(
            &course_key,
            "same-new-id",
            "08:00",
            "08:45",
            "https://example.test/VGA.mp4",
        );
        let with_seconds = build_segment_key(
            &course_key,
            "same-new-id",
            "08:00:00",
            "08:45:00",
            "https://example.test/VGA.mp4",
        );

        assert_eq!(compact, with_seconds);
    }

    #[test]
    fn should_sort_segments_by_start_time_then_new_id() {
        let mut tasks = vec![
            demo_task("10:00", "10:45", TaskStatus::Succeeded, Some("第二节")),
            demo_task("08:00", "08:45", TaskStatus::Succeeded, Some("第一节")),
        ];
        sort_tasks_for_course(&mut tasks);
        assert_eq!(tasks[0].start_time, "08:00");
        assert_eq!(tasks[1].start_time, "10:00");
    }

    #[test]
    fn should_render_merged_markdown_with_segment_count() {
        let tasks = vec![
            demo_task("08:00", "08:45", TaskStatus::Succeeded, Some("第一节内容")),
            demo_task("10:00", "10:45", TaskStatus::Succeeded, Some("第二节内容")),
        ];
        let markdown = build_merged_markdown(&tasks);
        assert!(markdown.contains("已收片段数：2"));
        assert!(markdown.contains("本稿由 2 个片段合并生成"));
        assert!(markdown.contains("## 08:00 - 08:45"));
    }

    #[test]
    fn should_group_three_adjacent_smartclass_segments_into_one_lecture() {
        let tasks = vec![
            demo_task(
                "15:30",
                "16:15",
                TaskStatus::Succeeded,
                Some("第三节完整内容"),
            ),
            demo_task(
                "13:30",
                "14:15",
                TaskStatus::Succeeded,
                Some("第一节完整内容"),
            ),
            demo_task(
                "14:25",
                "15:10",
                TaskStatus::Succeeded,
                Some("第二节完整内容"),
            ),
        ];

        let lectures = group_tasks_into_lectures(&tasks);
        assert_eq!(lectures.len(), 1);
        assert_eq!(lectures[0].segment_count(), 3);
        assert_eq!(lectures[0].start_time, "13:30");
        assert_eq!(lectures[0].end_time, "16:15");
        assert!(lectures[0].transcript_ready());

        let combined = lectures[0]
            .combined_transcript()
            .expect("三节都已完成，应可合并");
        let first = combined
            .find("【第1节 13:30-14:15】")
            .expect("应保留第一节边界");
        let second = combined
            .find("【第2节 14:25-15:10】")
            .expect("应保留第二节边界");
        let third = combined
            .find("【第3节 15:30-16:15】")
            .expect("应保留第三节边界");
        assert!(first < second && second < third);
        assert!(combined.contains("第一节完整内容"));
        assert!(combined.contains("第二节完整内容"));
        assert!(combined.contains("第三节完整内容"));
    }

    #[test]
    fn should_keep_same_course_key_even_when_segment_gap_is_large() {
        let tasks = vec![
            demo_task("13:30", "14:15", TaskStatus::Succeeded, Some("下午第一场")),
            demo_task("17:00", "17:45", TaskStatus::Succeeded, Some("晚上另一场")),
        ];

        let lectures = group_tasks_into_lectures(&tasks);
        assert_eq!(lectures.len(), 1);
        assert_eq!(lectures[0].segment_count(), 2);
    }

    #[test]
    fn should_not_invent_a_second_boundary_after_four_segments() {
        let tasks = vec![
            demo_task("08:00", "08:45", TaskStatus::Succeeded, Some("1")),
            demo_task("08:55", "09:40", TaskStatus::Succeeded, Some("2")),
            demo_task("09:50", "10:35", TaskStatus::Succeeded, Some("3")),
            demo_task("10:45", "11:30", TaskStatus::Succeeded, Some("4")),
            demo_task("11:40", "12:25", TaskStatus::Succeeded, Some("5")),
        ];

        let lectures = group_tasks_into_lectures(&tasks);
        assert_eq!(lectures.len(), 1);
        assert_eq!(lectures[0].segment_count(), 5);
    }

    #[test]
    fn should_split_when_course_identity_changes() {
        let first = demo_task("08:00", "08:45", TaskStatus::Succeeded, Some("病理"));
        let mut second = demo_task("08:55", "09:40", TaskStatus::Succeeded, Some("统计"));
        second.course_name = "医学统计学".into();
        second.course_key = build_course_key(
            &second.semester,
            &second.date,
            &second.course_name,
            &second.teacher_name,
        );

        let lectures = group_tasks_into_lectures(&[first, second]);
        assert_eq!(lectures.len(), 2);
    }

    #[test]
    fn incomplete_segment_should_block_combined_transcript() {
        let tasks = vec![
            demo_task("13:30", "14:15", TaskStatus::Succeeded, Some("已有 ASR")),
            demo_task("14:25", "15:10", TaskStatus::Running, None),
        ];

        let lectures = group_tasks_into_lectures(&tasks);
        assert_eq!(lectures.len(), 1);
        assert!(!lectures[0].transcript_ready());
        assert!(lectures[0].combined_transcript().is_none());
    }

    #[test]
    fn lecture_key_should_stay_stable_when_later_segment_is_appended() {
        let first_two = vec![
            demo_task("13:30", "14:15", TaskStatus::Succeeded, Some("1")),
            demo_task("14:25", "15:10", TaskStatus::Succeeded, Some("2")),
        ];
        let mut first_three = first_two.clone();
        first_three.push(demo_task(
            "15:30",
            "16:15",
            TaskStatus::Succeeded,
            Some("3"),
        ));

        let key_before = group_tasks_into_lectures(&first_two)[0].lecture_key.clone();
        let key_after = group_tasks_into_lectures(&first_three)[0]
            .lecture_key
            .clone();
        assert_eq!(key_before, key_after);
    }

    #[test]
    fn should_render_human_downloadable_lecture_asr_markdown() {
        let tasks = vec![
            demo_task("13:30", "14:15", TaskStatus::Succeeded, Some("第一节正文")),
            demo_task("14:25", "15:10", TaskStatus::Succeeded, Some("第二节正文")),
        ];
        let lecture = &group_tasks_into_lectures(&tasks)[0];
        let markdown = build_lecture_asr_markdown(lecture).expect("完整 lecture 应可导出 ASR");
        assert!(markdown.contains("# 完整课堂 ASR"));
        assert!(markdown.contains("时间：13:30 - 15:10"));
        assert!(markdown.contains("录像片段数：2"));
        assert!(markdown.contains("【第1节 13:30-14:15】"));
        assert!(markdown.contains("第二节正文"));
    }
}
