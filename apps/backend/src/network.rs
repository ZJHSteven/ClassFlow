/*!
网络健康闸门。

这个模块只解决一类问题：本机网络正在 AP 漫游、网卡重连、代理链路重建时，
外部 HTTP 请求会集中失败。此时继续快速重试没有意义，反而会把“暂时断网”
误判成“任务真正失败”。

所以这里做成一个很小的状态机：

1. 平时处于 `Healthy`，请求可以直接执行。
2. 一旦真实外部请求返回明显的网络类错误，状态切到 `Unhealthy`。
3. 后续外部请求进入等待，直到探针连续成功，再恢复为 `Healthy`。

普通业务错误不会进入这个闸门。例如鉴权失败、参数错误、百炼内容类失败，
都应该按原来的业务路径返回，而不是等待网络。
*/

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use reqwest::Client;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    config::AppConfig,
    error::{AppError, AppResult},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkHealthState {
    Healthy,
    Unhealthy,
    Recovering,
}

#[derive(Debug)]
struct NetworkHealthSnapshot {
    state: NetworkHealthState,
    last_error: Option<String>,
    last_changed_at: Instant,
}

#[derive(Clone)]
pub struct NetworkHealthGate {
    enabled: bool,
    probe_urls: Arc<Vec<String>>,
    probe_timeout: Duration,
    check_interval: Duration,
    stable_successes: usize,
    client: Client,
    snapshot: Arc<Mutex<NetworkHealthSnapshot>>,
}

impl NetworkHealthGate {
    pub fn from_config(config: &AppConfig) -> Self {
        let probe_urls = config
            .network_health_probe_urls
            .iter()
            .filter(|url| !url.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();

        let client = Client::builder()
            .connect_timeout(Duration::from_secs_f64(
                config.network_health_probe_timeout_secs.max(1.0),
            ))
            .timeout(Duration::from_secs_f64(
                config.network_health_probe_timeout_secs.max(1.0),
            ))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            enabled: config.network_health_enabled && !probe_urls.is_empty(),
            probe_urls: Arc::new(probe_urls),
            probe_timeout: Duration::from_secs_f64(
                config.network_health_probe_timeout_secs.max(1.0),
            ),
            check_interval: Duration::from_secs_f64(
                config.network_health_check_interval_secs.max(0.1),
            ),
            stable_successes: config.network_health_stable_successes.max(1),
            client,
            snapshot: Arc::new(Mutex::new(NetworkHealthSnapshot {
                state: NetworkHealthState::Healthy,
                last_error: None,
                last_changed_at: Instant::now(),
            })),
        }
    }

    pub fn disabled_for_tests() -> Self {
        Self {
            enabled: false,
            probe_urls: Arc::new(Vec::new()),
            probe_timeout: Duration::from_secs(1),
            check_interval: Duration::from_millis(10),
            stable_successes: 1,
            client: Client::new(),
            snapshot: Arc::new(Mutex::new(NetworkHealthSnapshot {
                state: NetworkHealthState::Healthy,
                last_error: None,
                last_changed_at: Instant::now(),
            })),
        }
    }

    #[cfg(test)]
    pub fn enabled_for_tests(probe_urls: Vec<String>) -> Self {
        Self {
            enabled: true,
            probe_urls: Arc::new(probe_urls),
            probe_timeout: Duration::from_secs(1),
            check_interval: Duration::from_millis(10),
            stable_successes: 1,
            client: Client::new(),
            snapshot: Arc::new(Mutex::new(NetworkHealthSnapshot {
                state: NetworkHealthState::Healthy,
                last_error: None,
                last_changed_at: Instant::now(),
            })),
        }
    }

    pub async fn wait_until_ready(&self, operation: &str) -> AppResult<()> {
        if !self.enabled {
            return Ok(());
        }

        if self.current_state().await == NetworkHealthState::Healthy {
            return Ok(());
        }

        warn!(operation, "网络健康状态不佳，暂停外部操作并等待恢复");
        self.wait_for_recovery(operation).await
    }

    pub async fn pause_if_network_error(
        &self,
        operation: &str,
        error: &AppError,
    ) -> AppResult<bool> {
        if !self.enabled || !is_network_like_error(error) {
            return Ok(false);
        }

        self.mark_unhealthy(operation, error).await;
        self.wait_for_recovery(operation).await?;
        Ok(true)
    }

    async fn current_state(&self) -> NetworkHealthState {
        self.snapshot.lock().await.state
    }

    async fn mark_unhealthy(&self, operation: &str, error: &AppError) {
        let mut snapshot = self.snapshot.lock().await;
        snapshot.state = NetworkHealthState::Unhealthy;
        snapshot.last_error = Some(error.to_string());
        snapshot.last_changed_at = Instant::now();
        warn!(
            operation,
            error = %error,
            "检测到网络类错误，网络健康状态已切换为 Unhealthy"
        );
    }

    async fn mark_recovering(&self) {
        let mut snapshot = self.snapshot.lock().await;
        if snapshot.state != NetworkHealthState::Recovering {
            snapshot.state = NetworkHealthState::Recovering;
            snapshot.last_changed_at = Instant::now();
        }
    }

    async fn mark_healthy(&self, operation: &str) {
        let mut snapshot = self.snapshot.lock().await;
        snapshot.state = NetworkHealthState::Healthy;
        snapshot.last_error = None;
        snapshot.last_changed_at = Instant::now();
        info!(operation, "网络探针已连续成功，外部操作恢复执行");
    }

    async fn wait_for_recovery(&self, operation: &str) -> AppResult<()> {
        let mut success_count = 0usize;
        loop {
            self.mark_recovering().await;
            if self.probe_once().await {
                success_count += 1;
                if success_count >= self.stable_successes {
                    self.mark_healthy(operation).await;
                    return Ok(());
                }
            } else {
                success_count = 0;
            }

            tokio::time::sleep(self.check_interval).await;
        }
    }

    async fn probe_once(&self) -> bool {
        for url in self.probe_urls.iter() {
            let result = self
                .client
                .get(url)
                .timeout(self.probe_timeout)
                .send()
                .await;

            match result {
                Ok(response) if response.status().as_u16() < 500 => return true,
                Ok(response) => {
                    warn!(
                        url,
                        status = %response.status(),
                        "网络健康探针收到服务端错误，继续等待"
                    );
                }
                Err(error) => {
                    warn!(url, error = %error, "网络健康探针失败，继续等待");
                }
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, extract::State, http::StatusCode, routing::get};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn network_error_should_wait_until_probe_recovers() {
        #[derive(Clone)]
        struct ProbeState {
            calls: Arc<AtomicUsize>,
        }

        async fn handle_probe(State(state): State<ProbeState>) -> StatusCode {
            let current = state.calls.fetch_add(1, Ordering::SeqCst);
            if current == 0 {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::OK
            }
        }

        let state = ProbeState {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/health", get(handle_probe))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("测试监听器应启动成功");
        let addr = listener.local_addr().expect("监听地址应能读取成功");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("测试 HTTP 服务应运行成功");
        });

        let gate = NetworkHealthGate::enabled_for_tests(vec![format!("http://{addr}/health")]);
        let paused = gate
            .pause_if_network_error(
                "测试外部请求",
                &AppError::External("请求超时: 模拟 Wi-Fi 漫游".to_string()),
            )
            .await
            .expect("网络健康闸门等待恢复应成功");

        assert!(paused, "网络类错误应触发暂停与恢复探针");
        assert_eq!(
            gate.current_state().await,
            NetworkHealthState::Healthy,
            "探针恢复后状态应回到 Healthy"
        );
        assert!(
            state.calls.load(Ordering::SeqCst) >= 2,
            "第一次探针失败后应继续等待下一次成功探针"
        );

        server.abort();
    }
}

pub fn is_network_like_error(error: &AppError) -> bool {
    match error {
        AppError::Io(_) => true,
        AppError::External(message) => {
            let text = message.to_lowercase();
            [
                "请求超时",
                "连接失败",
                "请求发送失败",
                "network",
                "timed out",
                "timeout",
                "connection",
                "connect",
                "dns",
                "temporarily unavailable",
                "no recent network activity",
                "error sending request",
            ]
            .iter()
            .any(|needle| text.contains(needle))
        }
        _ => false,
    }
}
