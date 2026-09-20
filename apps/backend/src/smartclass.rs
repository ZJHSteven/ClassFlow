/*!
SmartClass 自动录播采集器。

浏览器桥持有用户本人正常登录得到的 SmartClass 会话，并让网页自己的 JavaScript 发起官方请求；
Rust 后端只读取课程列表快照，负责分页、历史回扫、VOD URL 正规化、SQLite overlap 检查和任务入队。
这样 SmartClass 将来即使修改页面请求校验方式，ClassFlow 也不需要自己逆向认证算法。
*/

use std::time::Duration as StdDuration;

use chrono::{Duration, Local, NaiveDate};
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    AppState,
    error::{AppError, AppResult},
    models::{IntakeBatchRequest, IntakeItem},
};

#[derive(Debug, Deserialize)]
struct BridgePage {
    authenticated: bool,
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    data: Vec<SmartClassVideo>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SmartClassVideo {
    #[serde(rename = "Title", default)]
    title: String,
    #[serde(rename = "NewID", default)]
    new_id: String,
    #[serde(rename = "StartTime", default)]
    start_time: String,
    #[serde(rename = "StopTime", default)]
    stop_time: String,
    #[serde(rename = "Cover", default)]
    cover: String,
    #[serde(rename = "CourseName", default)]
    course_name: String,
    #[serde(rename = "Teachers", default)]
    teachers: String,
    #[serde(rename = "CloudVod", default)]
    cloud_vod: bool,
}

#[derive(Clone)]
struct SmartClassCollector {
    state: AppState,
    client: Client,
}

pub fn spawn_smartclass_collector(state: AppState) {
    if !state.config.smartclass_collector_enabled {
        info!("SmartClass Collector 未启用");
        return;
    }

    let timeout = StdDuration::from_secs_f64(state.config.smartclass_request_timeout_secs.max(1.0));
    let client = match Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(error) => {
            warn!("SmartClass Collector HTTP client 创建失败: {error}");
            return;
        }
    };

    let collector = SmartClassCollector { state, client };
    tokio::spawn(async move {
        collector.run_forever().await;
    });
}

impl SmartClassCollector {
    async fn run_forever(self) {
        let mut first_scan = true;
        let interval_secs = self.state.config.smartclass_poll_interval_secs.max(60);

        loop {
            let lookback_days = if first_scan {
                self.state.config.smartclass_initial_backfill_days.max(1)
            } else {
                self.state
                    .config
                    .smartclass_incremental_lookback_days
                    .max(1)
            };

            if let Err(error) = self.scan_recent_days(lookback_days).await {
                warn!("SmartClass Collector 本轮失败: {error}");
            }
            first_scan = false;
            tokio::time::sleep(StdDuration::from_secs(interval_secs)).await;
        }
    }

    async fn scan_recent_days(&self, lookback_days: i64) -> AppResult<()> {
        let end_date = Local::now().date_naive();
        let start_date = end_date - Duration::days(lookback_days.saturating_sub(1));
        self.scan_range(start_date, end_date).await
    }

    async fn scan_range(&self, start_date: NaiveDate, end_date: NaiveDate) -> AppResult<()> {
        let page_size = self.state.config.smartclass_page_size.max(1);
        let first_page = self.fetch_page(start_date, end_date, 1, page_size).await?;
        if !first_page.authenticated {
            return Err(AppError::Unauthorized);
        }

        let total_count = first_page.total_count;
        let total_pages = total_count.div_ceil(page_size).max(1);
        let mut videos = first_page.data;
        for page_number in 2..=total_pages {
            let page = self
                .fetch_page(start_date, end_date, page_number, page_size)
                .await?;
            if !page.authenticated {
                return Err(AppError::Unauthorized);
            }
            videos.extend(page.data);
        }

        let mut candidates = Vec::new();
        let mut skipped_cloud_vod = 0usize;
        let mut skipped_invalid = 0usize;
        for video in videos {
            match video_to_intake_item(video) {
                Ok(Some(item)) => candidates.push(item),
                Ok(None) => skipped_cloud_vod += 1,
                Err(error) => {
                    skipped_invalid += 1;
                    warn!("跳过无法正规化的 SmartClass 条目: {error}");
                }
            }
        }

        if candidates.is_empty() {
            info!(
                "SmartClass 扫描完成: {}..{}, total_count={}, 无可入队标准录播, cloud_vod={}, invalid={}",
                start_date, end_date, total_count, skipped_cloud_vod, skipped_invalid
            );
            return Ok(());
        }

        let missing = self
            .state
            .repo
            .filter_missing_intake_items(&candidates, &self.state.config.default_semester)
            .await?;
        if missing.is_empty() {
            info!(
                "SmartClass 扫描完成: {}..{}, total_count={}, {} 个标准录播均已存在",
                start_date,
                end_date,
                total_count,
                candidates.len()
            );
            return Ok(());
        }

        let request = IntakeBatchRequest {
            semester: Some(self.state.config.default_semester.clone()),
            source: "smartclass".to_string(),
            items: missing,
        };
        let response = self
            .state
            .repo
            .create_batch_with_tasks(&request, &self.state.config.default_semester)
            .await?;
        for task_id in &response.task_ids {
            self.state.queue.enqueue(task_id.clone())?;
        }
        if !response.task_ids.is_empty() {
            self.state.notify_task_list_changed();
        }

        info!(
            "SmartClass 自动补录完成: {}..{}, 发现总数={}, 新增任务={}, cloud_vod跳过={}, invalid跳过={}",
            start_date,
            end_date,
            total_count,
            response.accepted_count,
            skipped_cloud_vod,
            skipped_invalid
        );
        Ok(())
    }

    async fn fetch_page(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
        page_number: usize,
        page_size: usize,
    ) -> AppResult<BridgePage> {
        let url = format!(
            "{}/v1/smartclass/videos",
            self.state
                .config
                .smartclass_bridge_base_url
                .trim_end_matches('/')
        );
        let response = self
            .client
            .get(url)
            .query(&[
                ("start_date", start_date.to_string()),
                ("end_date", end_date.to_string()),
                ("page_number", page_number.to_string()),
                ("page_size", page_size.to_string()),
            ])
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AppError::External(format!(
                "SmartClass 浏览器桥返回 HTTP {}",
                response.status()
            )));
        }
        let page: BridgePage = response.json().await?;
        if !page.authenticated {
            warn!(
                "SmartClass 浏览器桥报告未登录: {}",
                page.error.as_deref().unwrap_or("浏览器会话已未登录")
            );
        }
        Ok(page)
    }
}

fn video_to_intake_item(video: SmartClassVideo) -> AppResult<Option<IntakeItem>> {
    if video.cloud_vod {
        return Ok(None);
    }
    if video.new_id.trim().is_empty() {
        return Err(AppError::External("SmartClass 条目缺少 NewID".to_string()));
    }

    let (date, start_time) = split_datetime(&video.start_time, "StartTime")?;
    let (_, end_time) = split_datetime(&video.stop_time, "StopTime")?;
    let mp4_url = cover_to_vga_url(&video.cover)?;
    let course_name = non_empty(&video.course_name)
        .or_else(|| non_empty(&video.title))
        .ok_or_else(|| AppError::External("SmartClass 条目缺少课程名/标题".to_string()))?;
    let teacher_name = non_empty(&video.teachers).unwrap_or("未知教师");

    Ok(Some(IntakeItem {
        new_id: video.new_id.clone(),
        page_url: format!(
            "https://tmu.smartclass.cn/PlayPages/Video.aspx?NewID={}",
            video.new_id
        ),
        mp4_url,
        course_name: course_name.to_string(),
        teacher_name: teacher_name.to_string(),
        date: date.to_string(),
        start_time: start_time.to_string(),
        end_time: end_time.to_string(),
        raw_title: non_empty(&video.title).unwrap_or(course_name).to_string(),
    }))
}

fn split_datetime<'a>(value: &'a str, field: &str) -> AppResult<(&'a str, &'a str)> {
    let mut parts = value.split_whitespace();
    let date = parts.next().unwrap_or_default();
    let time = parts.next().unwrap_or_default();
    if date.is_empty() || time.is_empty() {
        return Err(AppError::External(format!(
            "SmartClass {field} 格式异常: {value}"
        )));
    }
    Ok((date, time))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn cover_to_vga_url(cover: &str) -> AppResult<String> {
    let normalized = cover.trim().replace('\\', "/");
    let without_query = normalized.split('?').next().unwrap_or_default();
    let prefix = without_query
        .strip_suffix("/000.jpg")
        .or_else(|| without_query.strip_suffix("/000.JPG"))
        .ok_or_else(|| AppError::External(format!("无法从 Cover 推导 VGA.mp4: {without_query}")))?;
    Ok(format!("{prefix}/VGA.mp4"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_video() -> SmartClassVideo {
        SmartClassVideo {
            title: "医学人口学 韩钰 第二教室".to_string(),
            new_id: "ecba497b532442df8681b4c1aaa6f85d".to_string(),
            start_time: "2026-09-16 18:55:00".to_string(),
            stop_time: "2026-09-16 19:40:00".to_string(),
            cover: "https://tmuvod.smartclass.cn:8010/cunchu01/2026/09/16/66531474/2/000.jpg?x=1"
                .to_string(),
            course_name: "医学人口学".to_string(),
            teachers: "韩钰".to_string(),
            cloud_vod: false,
        }
    }

    #[test]
    fn should_derive_vga_url_from_cover() {
        let actual = cover_to_vga_url(
            "https://tmuvod.smartclass.cn:8010/cunchu01/2026/09/16/66531474/2/000.jpg?token=abc",
        )
        .unwrap();
        assert_eq!(
            actual,
            "https://tmuvod.smartclass.cn:8010/cunchu01/2026/09/16/66531474/2/VGA.mp4"
        );
    }

    #[test]
    fn should_map_standard_video_to_existing_intake_shape() {
        let item = video_to_intake_item(demo_video()).unwrap().unwrap();
        assert_eq!(item.course_name, "医学人口学");
        assert_eq!(item.teacher_name, "韩钰");
        assert_eq!(item.date, "2026-09-16");
        assert_eq!(item.start_time, "18:55:00");
        assert_eq!(item.end_time, "19:40:00");
        assert!(item.mp4_url.ends_with("/66531474/2/VGA.mp4"));
    }

    #[test]
    fn should_skip_cloud_vod_without_guessing_layout() {
        let mut video = demo_video();
        video.cloud_vod = true;
        assert!(video_to_intake_item(video).unwrap().is_none());
    }

    #[test]
    fn should_reject_unknown_cover_layout() {
        let mut video = demo_video();
        video.cover = "https://example.com/random-cover.png".to_string();
        assert!(video_to_intake_item(video).is_err());
    }
}
