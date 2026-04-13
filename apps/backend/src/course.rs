/*!
课程聚合相关的纯函数。

纯函数的价值很高：

1. 不依赖数据库、网络、文件系统，测试非常轻。
2. 路径规则、排序规则、Markdown 模板都能稳定回归。
3. 后端与前端只要围绕同一套字段展示，就不会出现“目录名一套、页面显示另一套”的错位。
*/

use chrono::Utc;
use serde_json::json;

use crate::models::{CourseSummaryResponse, NormalizedTranscript, TaskRecord, TaskStatus};

pub fn build_course_key(
    semester: &str,
    date: &str,
    course_name: &str,
    teacher_name: &str,
) -> String {
    format!("{semester}|{date}|{course_name}|{teacher_name}")
}

pub fn build_segment_key(
    course_key: &str,
    new_id: &str,
    start_time: &str,
    end_time: &str,
    mp4_url: &str,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(course_key.as_bytes());
    hasher.update(b"::");
    hasher.update(new_id.as_bytes());
    hasher.update(b"::");
    hasher.update(start_time.as_bytes());
    hasher.update(b"::");
    hasher.update(end_time.as_bytes());
    hasher.update(b"::");
    hasher.update(mp4_url.as_bytes());
    format!("{:x}", hasher.finalize())
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
}
