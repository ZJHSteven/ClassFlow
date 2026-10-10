/*!
课程表先验。

ClassFlow 的录像发现与 ASR 可以尽快执行，但 Notes 不能仅根据“最后一个已知录像片段”
猜测整堂课已经结束。这里读取由 LifeOps/学习通生成的课表快照，用计划结束时间作为
优先封堂依据；只有找不到匹配课表时，Notes 才退回原来的片段间隔启发式。
*/

use std::path::Path;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::Deserialize;

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
    #[serde(default)]
    pub events: Vec<ScheduledEvent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledEvent {
    pub start: String,
    pub end: String,
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
                events: vec![ScheduledEvent {
                    start: "2026-10-10T08:00:00+08:00".to_string(),
                    end: "2026-10-10T08:45:00+08:00".to_string(),
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
                events: vec![ScheduledEvent {
                    start: "2026-10-13T15:30:00+08:00".to_string(),
                    end: "2026-10-13T17:10:00+08:00".to_string(),
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
}
