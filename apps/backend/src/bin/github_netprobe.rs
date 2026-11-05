//! GitHub API 只读网络探针。
//!
//! 背景：生产环境使用 reqwest/rustls 发布 GitHub Contents 时报告网络异常，
//! 但 curl 成功。程序直接复用 ClassFlow 的依赖，分别验证默认协商和
//! HTTP/1.1，并打印网络错误的底层原因。
//!
//! 不需要 Token、不发送任何写请求，也不输出凭据。

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use std::{error::Error as _, time::Duration};

#[tokio::main]
async fn main() {
    // 可选传入准确的 Contents URL，以排除非 ASCII 路径/URL 转义差异。
    // Token 只从进程环境获取，避免出现在命令行参数和日志里。
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        "https://api.github.com/repos/ZJHSteven/med-notes-2026-fall-y3/contents".to_string()
    });
    let token = std::env::var("CLASSFLOW_NOTES_GITHUB_TOKEN").ok();
    let invalid_put = std::env::args().any(|arg| arg == "--invalid-put");
    for force_http1 in [false, true] {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15));
        if force_http1 {
            builder = builder.http1_only();
        }
        let client = builder.build().expect("构造只读HTTP客户端");
        let mut request = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "ClassFlow-NetworkProbe/1.0");
        if let Some(token) = token.as_deref().filter(|s| !s.is_empty()) {
            request = request.bearer_auth(token);
        }
        let result = request.send().await;
        match result {
            Ok(response) => println!(
                "PROBE http1_only={force_http1}, transport=ok, status={}, version={:?}",
                response.status(),
                response.version()
            ),
            Err(error) => {
                println!(
                    "PROBE http1_only={force_http1}, transport=failed, timeout={}, connect={}, error={error}",
                    error.is_timeout(),
                    error.is_connect()
                );
                let mut cause = error.source();
                while let Some(next) = cause {
                    println!("  CAUSE: {next}");
                    cause = next.source();
                }
            }
        }
        if invalid_put {
            // 特意指定绝不可能存在的分支：GitHub 拒绝写入，避免诊断污染仓库。
            // 同时发送不同大小的 JSON 主体，确定错误是否与上传长度相关。
            for data_size in [512, 120_000] {
                let body = serde_json::json!({
                    "message": "ClassFlow diagnostic: this request must not write",
                    "content": BASE64_STANDARD.encode(vec![b'X'; data_size]),
                    "branch": "__classflow_probe_nonexistent_branch_20261105_f7d28d__"
                });
                let mut write = client
                    .put(&url)
                    .header(reqwest::header::USER_AGENT, "ClassFlow-NetworkProbe/1.0")
                    .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                    .json(&body);
                if let Some(token) = token.as_deref().filter(|s| !s.is_empty()) {
                    write = write.bearer_auth(token);
                }
                match write.send().await {
                    Ok(reply) => println!(
                        "INVALID_PUT http1_only={force_http1} body_bytes={data_size} transport=ok status={}",
                        reply.status()
                    ),
                    Err(err) => {
                        println!(
                            "INVALID_PUT http1_only={force_http1} body_bytes={data_size} transport=failed timeout={} connect={} error={err}",
                            err.is_timeout(),
                            err.is_connect()
                        );
                        let mut cause = err.source();
                        while let Some(next) = cause {
                            println!("  CAUSE: {next}");
                            cause = next.source();
                        }
                    }
                }
            }
        }
    }
}
