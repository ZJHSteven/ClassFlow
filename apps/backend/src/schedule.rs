/*!
课程表先验。

ClassFlow 的录像发现与 ASR 可以尽快执行，但 Notes 不能仅根据“最后一个已知录像片段”
猜测整堂课已经结束。这里读取由 LifeOps/学习通生成的课表快照，用计划结束时间作为
优先封堂依据；只有找不到匹配课表时，Notes 才退回原来的片段间隔启发式。
*/

use std::path::Path;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CourseSchedule {
    #[serde(default)]
    pub courses: Vec<ScheduledCourse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledCourse {
    pub course_name: String,
    #[serde(default)]
    pub semester: Option<String>,
    /// 单门课程缺省是否要求 SmartClass 录播；实验课程可设为 false。
    /// 这里只影响课程覆盖告警，绝不删除课表或阻止以后导入手动录音。
    #[serde(default)]
    pub smartclass_disabled: bool,
    #[serde(default)]
    pub events: Vec<ScheduledEvent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledEvent {
    pub start: String,
    pub end: String,
    /// 单次课的特殊覆盖（例如理论课课程里的实验时段）。
    /// None 表示沿用课程级设置，Some(true) 可以恢复课次的录播预期。
    #[serde(default)]
    pub smartclass_expected: Option<bool>,
    #[serde(default)]
    pub repeat: Option<ScheduledRepeat>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledRepeat {
    pub frequency: String,
    #[serde(default, rename = "daysOfWeek")]
    pub days_of_week: Vec<u32>,
    #[serde(default)]
    pub until: Option<String>,
}

/// 课表中的“应到课”事实：与 SmartClass 是否实际有录播是两个独立概念。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedClass {
    pub semester: String,
    pub course_name: String,
    pub date: String,
    pub end_utc: DateTime<Utc>,
    /// 按课表时间估算应有多少个 45 分钟录播分段。
    /// 它是异常线索而非学校授课节数的权威字段。
    pub expected_segments: usize,
}

impl CourseSchedule {
    pub async fn load_optional(path: &Path) -> AppResult<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                AppError::Config(format!("课程表快照 {} 解析失败: {error}", path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// 展开一段日期的固定和每周重复课程。
    ///
    /// 一堂课在课表中可能分成多个时间事件（例如上午、下午同一实验）。
    /// 按“学期+课程+日期”聚合，采用最晚结束时间判定整堂课是否超时。
    /// 此方法不主动推断课表中不存在的课程，也不把无录像直接判为采集故障。
    pub fn planned_classes_between(
        &self,
        semester: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Vec<PlannedClass> {
        let mut classes = BTreeMap::<(String, String, String), (DateTime<Utc>, usize)>::new();
        if from > to {
            return Vec::new();
        }
        for course in &self.courses {
            if course
                .semester
                .as_deref()
                .is_some_and(|value| value != semester)
            {
                continue;
            }
            for event in &course.events {
                if !event.smartclass_expected.unwrap_or(!course.smartclass_disabled) {
                    // disabled 只从 SmartClass 覆盖巡检中排除，不改变 Notes closure。
                    continue;
                }
                let mut date = from;
                while date <= to {
                    if let Some(end) = event.end_for_date(date) {
                        let key = (
                            semester.to_string(),
                            course.course_name.clone(),
                            date.to_string(),
                        );
                        let end_utc = end.with_timezone(&Utc);
                        let expected = event.estimated_segment_count();
                        classes
                            .entry(key)
                            .and_modify(|(current_end, count)| {
                                *current_end = (*current_end).max(end_utc);
                                *count += expected;
                            })
                            .or_insert((end_utc, expected));
                    }
                    date += Duration::days(1);
                }
            }
        }
        classes
            .into_iter()
            .map(|((semester, course_name, date), (end_utc, expected_segments))| PlannedClass {
                semester,
                course_name,
                date,
                end_utc,
                expected_segments,
            })
            .collect()
    }

    pub fn planned_end_utc(
        &self,
        semester: &str,
        course_name: &str,
        date: &str,
    ) -> Option<DateTime<Utc>> {
        let target_date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
        self.courses
            .iter()
            .filter(|course| {
                course.course_name == course_name
                    && course
                        .semester
                        .as_deref()
                        .is_none_or(|value| value == semester)
            })
            .flat_map(|course| course.events.iter())
            .filter_map(|event| event.end_for_date(target_date))
            .max()
            .map(|value| value.with_timezone(&Utc))
    }
}

impl ScheduledEvent {
    fn estimated_segment_count(&self) -> usize {
        let (Ok(start), Ok(end)) = (
            DateTime::parse_from_rfc3339(&self.start),
            DateTime::parse_from_rfc3339(&self.end),
        ) else {
            return 1;
        };
        let minutes = (end - start).num_minutes().max(1);
        // SmartClass 常见的一个片段约45分钟、片段之间休息约10分钟。
        // 55分钟取最近整数，覆盖常见45/100/165/220分钟课时。
        // 仅用于告警：录播与课表边界不同时必须留给人工确认。
        ((minutes + 27) / 55).max(1) as usize
    }
    fn end_for_date(&self, target_date: NaiveDate) -> Option<DateTime<chrono::FixedOffset>> {
        let start = DateTime::parse_from_rfc3339(&self.start).ok()?;
        let end = DateTime::parse_from_rfc3339(&self.end).ok()?;

        let occurs = match &self.repeat {
            None => start.date_naive() == target_date,
            Some(repeat) if repeat.frequency.eq_ignore_ascii_case("weekly") => {
                let before_start = target_date < start.date_naive();
                let after_end = repeat
                    .until
                    .as_deref()
                    .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
                    .is_some_and(|until| target_date > until);
                !before_start
                    && !after_end
                    && repeat
                        .days_of_week
                        .contains(&target_date.weekday().num_days_from_sunday())
            }
            Some(_) => false,
        };

        if !occurs {
            return None;
        }

        end.with_year(target_date.year())?
            .with_month(target_date.month())?
            .with_day(target_date.day())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_event_should_return_planned_end() {
        let schedule = CourseSchedule {
            courses: vec![ScheduledCourse {
                course_name: "泌尿生殖系统".to_string(),
                semester: Some("2026-2027-1".to_string()),
                smartclass_disabled: false,
                events: vec![ScheduledEvent {
                    start: "2026-10-10T08:00:00+08:00".to_string(),
                    end: "2026-10-10T08:45:00+08:00".to_string(),
                    smartclass_expected: None,
                    repeat: None,
                }],
            }],
        };
        assert_eq!(
            schedule
                .planned_end_utc("2026-2027-1", "泌尿生殖系统", "2026-10-10")
                .map(|value| value.to_rfc3339()),
            Some("2026-10-10T00:45:00+00:00".to_string())
        );
    }

    #[test]
    fn weekly_event_should_expand_only_inside_repeat_window() {
        let schedule = CourseSchedule {
            courses: vec![ScheduledCourse {
                course_name: "流行病学".to_string(),
                semester: Some("2026-2027-1".to_string()),
                smartclass_disabled: false,
                events: vec![ScheduledEvent {
                    start: "2026-10-13T15:30:00+08:00".to_string(),
                    end: "2026-10-13T17:10:00+08:00".to_string(),
                    smartclass_expected: None,
                    repeat: Some(ScheduledRepeat {
                        frequency: "weekly".to_string(),
                        days_of_week: vec![2],
                        until: Some("2027-01-06".to_string()),
                    }),
                }],
            }],
        };
        assert!(
            schedule
                .planned_end_utc("2026-2027-1", "流行病学", "2026-10-20")
                .is_some()
        );
        assert!(
            schedule
                .planned_end_utc("2026-2027-1", "流行病学", "2027-01-12")
                .is_none()
        );
    }

    #[test]
    fn planned_classes_should_merge_same_day_and_respect_repeat_dates() {
        let schedule = CourseSchedule {
            courses: vec![ScheduledCourse {
                course_name: "呼吸系统".to_string(),
                semester: Some("2026-2027-1".to_string()),
                smartclass_disabled: false,
                events: vec![
                    ScheduledEvent {
                        start: "2026-10-29T13:30:00+08:00".into(),
                        end: "2026-10-29T15:10:00+08:00".into(),
                        smartclass_expected: None,
                        repeat: Some(ScheduledRepeat {
                            frequency: "weekly".into(),
                            days_of_week: vec![4],
                            until: Some("2026-11-06".into()),
                        }),
                    },
                    ScheduledEvent {
                        start: "2026-10-29T15:30:00+08:00".into(),
                        end: "2026-10-29T16:15:00+08:00".into(),
                        smartclass_expected: None,
                        repeat: None,
                    },
                ],
            }],
        };
        let classes = schedule.planned_classes_between(
            "2026-2027-1",
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap(),
            NaiveDate::from_ymd_opt(2026, 11, 7).unwrap(),
        );
        assert_eq!(classes.len(), 2);
        assert_eq!(classes[0].end_utc.to_rfc3339(), "2026-10-29T08:15:00+00:00");
        assert_eq!(classes[0].expected_segments, 3);
        assert_eq!(classes[1].date, "2026-11-05");
        assert!(
            schedule
                .planned_classes_between(
                    "other-semester",
                    NaiveDate::from_ymd_opt(2026, 10, 28).unwrap(),
                    NaiveDate::from_ymd_opt(2026, 11, 7).unwrap()
                )
                .is_empty()
        );
    }

    #[test]
    fn disabled_experiment_should_not_enter_smartclass_monitor_but_keeps_class_schedule() {
        let raw = r#"{"courses":[
            {"course_name":"机能实验学","semester":"2026-2027-1","smartclass_disabled":true,
             "events":[{"start":"2026-11-18T08:00:00+08:00","end":"2026-11-18T11:40:00+08:00"}]},
            {"course_name":"实验诊断学","semester":"2026-2027-1",
             "events":[{"start":"2026-11-12T08:00:00+08:00","end":"2026-11-12T11:40:00+08:00","smartclass_expected":false},
                       {"start":"2026-11-16T08:00:00+08:00","end":"2026-11-16T09:40:00+08:00"}]}
        ]}"#;
        let schedule: CourseSchedule = serde_json::from_str(raw).unwrap();
        let from = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let monitored = schedule.planned_classes_between("2026-2027-1", from, to);
        assert_eq!(monitored.len(), 1);
        assert_eq!(monitored[0].course_name, "实验诊断学");
        assert_eq!(monitored[0].date, "2026-11-16");
        assert_eq!(monitored[0].expected_segments, 2);
        // disabled 影响的是录播预期，不会清除原计划授课的结束时间。
        assert!(schedule.planned_end_utc("2026-2027-1","机能实验学","2026-11-18").is_some());
    }
}
