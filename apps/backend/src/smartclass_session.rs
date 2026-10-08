/*!
SmartClass 纯 HTTP 会话管理器。

这个模块替代旧的“Windows Chrome + CDP Browser Bridge”长期生产依赖。它只复刻
SmartClass 官方网页自己已经公开发送的 HTTP 请求，不猜测课程 ID，也不扫描其他人的资源：

1. 用 Reqwest `Jar` 保存当前用户本人登录得到的 Cookie，并将 Cookie header 持久化到
   N1 的 0600 状态文件。
2. 动态拉 `/config.json`，按网页 `enHelp.De()` 的实际实现用 AES-256-CBC/PKCS#7 解出
   `csrkKey`。
3. 调 `/Home/GettimeDif` 校准服务端时钟，复刻 `getcsrf()` 生成 `csrkToken`。
4. 每五分钟调 `/Home/GetInteractiveAuth` 刷新网页同款 `platform_token`。
5. 直接 GET `/Webapi/V1/Video/GetMyVideoList`；Collector 只消费结构化分页结果。
6. Session 过期时显式返回 `Unauthorized`。前端可以请求验证码并 POST 账号登录；用户名/
   密码只在内存里短暂存在，按网页 `enHelp.En()` 的同一 AES 参数加密后发送，不落盘。

安全边界：
- Cookie 文件永远不写进仓库/env/日志；Unix 上强制 0600。
- 日志与错误消息不包含 Cookie、platform token、用户名或密码。
- AES key/IV 是 SmartClass 客户端 JS 自带的公开兼容常量，不是用户 secret；仍允许通过
  环境变量覆盖，以便站点升级后无需重新改代码。
*/

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aes::Aes256;
use cbc::{Decryptor, Encryptor};
use cbc::cipher::{
    BlockDecryptMut, BlockEncryptMut, KeyIvInit,
    block_padding::Pkcs7,
};
use reqwest::{
    Client, StatusCode, Url,
    cookie::{CookieStore, Jar},
    header::{ACCEPT, CONTENT_LANGUAGE, HeaderName, REFERER},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};
use tracing::warn;
use uuid::Uuid;

use crate::{
    config::AppConfig,
    error::{AppError, AppResult},
};

const PLATFORM_TOKEN_HEADER: HeaderName = HeaderName::from_static("platform_token");
const SKIP_TOKEN_VERIFY_HEADER: HeaderName = HeaderName::from_static("skip_token_verify");
const X_REQUESTED_WITH: HeaderName = HeaderName::from_static("x-requested-with");
const DEFAULT_LANGUAGE: &str = "zh-CN";
const PLATFORM_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const CSRK_KEY_TTL: Duration = Duration::from_secs(30 * 60);
const TIME_OFFSET_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SmartClassVideo {
    #[serde(rename = "Title", default)]
    pub title: String,
    #[serde(rename = "NewID", default)]
    pub new_id: String,
    #[serde(rename = "StartTime", default)]
    pub start_time: String,
    #[serde(rename = "StopTime", default)]
    pub stop_time: String,
    #[serde(rename = "Cover", default)]
    pub cover: String,
    #[serde(rename = "CourseName", default)]
    pub course_name: String,
    #[serde(rename = "Teachers", default)]
    pub teachers: String,
    #[serde(rename = "CloudVod", default)]
    pub cloud_vod: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SmartClassVideoPage {
    pub total_count: usize,
    pub data: Vec<SmartClassVideo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmartClassAuthStatus {
    pub authenticated: bool,
    pub session_present: bool,
    pub transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmartClassCaptcha {
    pub verify_code_id: String,
    pub image_data_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmartClassLoginRequest {
    pub username: String,
    pub password: String,
    pub verify_code: String,
    pub verify_code_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmartClassLoginResult {
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSession {
    version: u32,
    cookie_header: String,
    updated_at_unix_ms: i64,
}

#[derive(Default)]
struct RuntimeState {
    csrf_key: Option<CachedString>,
    time_offset_ms: i64,
    time_offset_valid_until: Option<Instant>,
    platform_token: Option<CachedString>,
}

struct CachedString {
    value: String,
    valid_until: Instant,
}

#[derive(Clone)]
pub struct SmartClassSessionManager {
    config: Arc<AppConfig>,
    base_url: Url,
    course_list_url: Url,
    jar: Arc<Jar>,
    client: Client,
    runtime: Arc<Mutex<RuntimeState>>,
}

#[derive(Debug, Deserialize)]
struct DomainConfigEnvelope {
    #[serde(rename = "domainConfig")]
    domain_config: String,
}

#[derive(Debug, Deserialize)]
struct DecryptedDomainConfig {
    #[serde(rename = "csrkKey")]
    csrk_key: String,
}

#[derive(Debug, Deserialize)]
struct SmartClassApiEnvelope {
    #[serde(rename = "Success")]
    success: Option<bool>,
    #[serde(rename = "Message")]
    message: Option<String>,
    #[serde(rename = "Value")]
    value: Option<Value>,
    status: Option<i64>,
    #[serde(rename = "localtion")]
    location: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VideoListValue {
    #[serde(rename = "TotalCount", default)]
    total_count: usize,
    #[serde(rename = "Data", default)]
    data: Vec<SmartClassVideo>,
}

#[derive(Debug, Deserialize)]
struct VerifyImageValue {
    #[serde(rename = "Code")]
    code: String,
    #[serde(rename = "img")]
    image_base64: String,
}

impl SmartClassSessionManager {
    pub async fn new(config: Arc<AppConfig>) -> AppResult<Self> {
        let mut base_url = Url::parse(config.smartclass_base_url.trim()).map_err(|error| {
            AppError::Config(format!(
                "CLASSFLOW_SMARTCLASS_BASE_URL 不是合法 URL: {error}"
            ))
        })?;
        if !base_url.path().ends_with('/') {
            base_url.set_path("/");
        }
        let course_list_url = base_url
            .join("UserSpace/CourseList.aspx")
            .map_err(|error| AppError::Config(format!("构造 SmartClass CourseList URL 失败: {error}")))?;

        let jar = Arc::new(Jar::default());
        load_cookie_file(&config.smartclass_session_file, &jar, &base_url).await?;
        let client = Client::builder()
            .cookie_provider(jar.clone())
            .redirect(Policy::none())
            .timeout(Duration::from_secs_f64(
                config.smartclass_request_timeout_secs.max(1.0),
            ))
            .user_agent("ClassFlow-SmartClass/1.0")
            .build()
            .map_err(|error| AppError::Config(format!("创建 SmartClass HTTP client 失败: {error}")))?;

        Ok(Self {
            config,
            base_url,
            course_list_url,
            jar,
            client,
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
        })
    }

    pub async fn auth_status(&self) -> SmartClassAuthStatus {
        let session_present = self.current_cookie_header().is_some();
        if !session_present {
            return SmartClassAuthStatus {
                authenticated: false,
                session_present: false,
                transport: "direct_http",
                reason: Some("N1 尚未保存 SmartClass 登录 Session".to_string()),
            };
        }

        match self.heartbeat().await {
            Ok(()) => SmartClassAuthStatus {
                authenticated: true,
                session_present: true,
                transport: "direct_http",
                reason: None,
            },
            Err(AppError::Unauthorized) => SmartClassAuthStatus {
                authenticated: false,
                session_present: true,
                transport: "direct_http",
                reason: Some("SmartClass Session 已过期，需要重新登录".to_string()),
            },
            Err(error) => SmartClassAuthStatus {
                authenticated: false,
                session_present: true,
                transport: "direct_http",
                reason: Some(format!("SmartClass 连通性检查失败: {error}")),
            },
        }
    }

    pub async fn fetch_captcha(&self) -> AppResult<SmartClassCaptcha> {
        self.ensure_time_offset().await?;
        let token = self.csrf_token().await?;
        let mut url = self.url("home/VerifyImage2")?;
        url.query_pairs_mut()
            .append_pair("r", &format!("{}", now_unix_ms()))
            .append_pair("csrkToken", &token);
        let value = self
            .send_api(self.base_request(self.client.get(url)), None)
            .await?;
        let captcha: VerifyImageValue = serde_json::from_value(value).map_err(|error| {
            AppError::External(format!("解析 SmartClass 验证码响应失败: {error}"))
        })?;
        if captcha.code.trim().is_empty() || captcha.image_base64.trim().is_empty() {
            return Err(AppError::External(
                "SmartClass 返回了空验证码 id 或图片".to_string(),
            ));
        }
        Ok(SmartClassCaptcha {
            verify_code_id: captcha.code,
            image_data_url: format!("data:image/png;base64,{}", captcha.image_base64),
        })
    }

    pub async fn login(&self, request: SmartClassLoginRequest) -> AppResult<SmartClassLoginResult> {
        if request.username.trim().is_empty()
            || request.password.is_empty()
            || request.verify_code.trim().is_empty()
            || request.verify_code_id.trim().is_empty()
        {
            return Err(AppError::BadRequest(
                "用户名、密码、验证码与验证码 id 均不能为空".to_string(),
            ));
        }

        self.ensure_time_offset().await?;
        let token = self.csrf_token().await?;
        let mut url = self.url("Home/Login")?;
        url.query_pairs_mut().append_pair("csrkToken", &token);
        let username = self.encrypt_client_value(request.username.trim())?;
        let password = self.encrypt_client_value(&request.password)?;
        let form = [
            ("UserName", username.as_str()),
            ("PassWord", password.as_str()),
            ("isRember", "false"),
            ("VerifyCode", request.verify_code.trim()),
            ("VerifyCodeId", request.verify_code_id.trim()),
            ("redirectUri", ""),
        ];
        let response = self
            .base_request(self.client.post(url))
            .header(SKIP_TOKEN_VERIFY_HEADER.clone(), "skip_token_verify")
            .form(&form)
            .send()
            .await?;
        self.persist_cookies().await?;
        let body = response.text().await?;
        let value = parse_api_value(&body, false)?;
        let redirect = value.as_str().map(ToOwned::to_owned);

        // 新 Session 建立后，旧的时间差 / platform token 不再可信。保留 csrkKey（它是站点
        // 公共配置），只清登录态相关缓存并重新获取官方 platform_token。
        {
            let mut runtime = self.runtime.lock().await;
            runtime.time_offset_valid_until = None;
            runtime.platform_token = None;
        }
        self.ensure_time_offset().await?;
        let _ = self.platform_token().await?;
        self.heartbeat().await?;

        Ok(SmartClassLoginResult {
            authenticated: true,
            redirect,
        })
    }

    pub(crate) async fn fetch_video_page(
        &self,
        start_date: &str,
        end_date: &str,
        page_number: usize,
        page_size: usize,
    ) -> AppResult<SmartClassVideoPage> {
        if page_number == 1 {
            self.heartbeat().await?;
        }
        self.ensure_time_offset().await?;
        let platform_token = self.platform_token().await?;
        let csrf_token = self.csrf_token().await?;
        let mut url = self.url("Webapi/V1/Video/GetMyVideoList")?;
        url.query_pairs_mut()
            .append_pair("csrkToken", &csrf_token)
            .append_pair("Sort", "StartTime")
            .append_pair("TagID", "")
            .append_pair("SyCommonKey", "")
            .append_pair("GradeId", "")
            .append_pair("StudentID", "")
            .append_pair("StudentName", "")
            .append_pair("NodeID", "")
            .append_pair("BuildingID", "")
            .append_pair("StartDate", start_date)
            .append_pair("EndDate", end_date)
            .append_pair("Order", "1")
            .append_pair("PageSize", &page_size.to_string())
            .append_pair("PageNumber", &page_number.to_string())
            .append_pair("attribute", "")
            .append_pair("IncludePublic", "");

        let value = self
            .send_api(
                self.base_request(self.client.get(url))
                    .header(PLATFORM_TOKEN_HEADER.clone(), platform_token),
                Some("GetMyVideoList"),
            )
            .await?;
        let page: VideoListValue = serde_json::from_value(value).map_err(|error| {
            AppError::External(format!("解析 SmartClass GetMyVideoList 失败: {error}"))
        })?;
        Ok(SmartClassVideoPage {
            total_count: page.total_count,
            data: page.data,
        })
    }

    async fn heartbeat(&self) -> AppResult<()> {
        let response = self
            .base_request(self.client.get(self.course_list_url.clone()))
            .send()
            .await?;
        self.persist_cookies().await?;
        if response.status().is_redirection() {
            return Err(AppError::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(AppError::RetryableExternal(format!(
                "SmartClass heartbeat HTTP {}",
                response.status()
            )));
        }
        let text = response.text().await?;
        if text.contains("name=\"loginName\"") || text.contains("name='loginName'") {
            return Err(AppError::Unauthorized);
        }
        Ok(())
    }

    async fn platform_token(&self) -> AppResult<String> {
        {
            let runtime = self.runtime.lock().await;
            if let Some(cached) = &runtime.platform_token
                && cached.valid_until > Instant::now()
            {
                return Ok(cached.value.clone());
            }
        }

        let csrf = self.csrf_token().await?;
        let mut url = self.url("Home/GetInteractiveAuth")?;
        url.query_pairs_mut().append_pair("csrkToken", &csrf);
        let value = self
            .send_api(self.base_request(self.client.get(url)), None)
            .await?;
        let token = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::External("SmartClass GetInteractiveAuth 返回空 token".to_string())
            })?
            .to_string();
        let mut runtime = self.runtime.lock().await;
        runtime.platform_token = Some(CachedString {
            value: token.clone(),
            // 网页使用 5 分钟过期时间；提前 15 秒刷新，避免任务正好踩在边界。
            valid_until: Instant::now() + PLATFORM_TOKEN_TTL - Duration::from_secs(15),
        });
        Ok(token)
    }

    async fn ensure_time_offset(&self) -> AppResult<i64> {
        {
            let runtime = self.runtime.lock().await;
            if runtime
                .time_offset_valid_until
                .is_some_and(|deadline| deadline > Instant::now())
            {
                return Ok(runtime.time_offset_ms);
            }
        }

        let csrf_key = self.csrf_key().await?;
        let token = build_csrf_token(&csrf_key, now_unix_ms())?;
        let mut url = self.url("Home/GettimeDif")?;
        url.query_pairs_mut().append_pair("csrkToken", &token);
        let request_started = now_unix_ms();
        let value = self
            .send_api(self.base_request(self.client.get(url)), None)
            .await?;
        let request_finished = now_unix_ms();
        let server_ms = parse_i64_value(&value).ok_or_else(|| {
            AppError::External(format!(
                "SmartClass GettimeDif 返回无法解析的时间值: {}",
                summarize_json(&value)
            ))
        })?;
        // 网页直接用“响应时本机时间”相减。这里用请求中点估计能抵消一半 RTT，误差只会
        // 更小；csrkToken 本身仍使用完全相同的 13 位时间戳映射算法。
        let local_midpoint = request_started + (request_finished - request_started) / 2;
        let offset = server_ms - local_midpoint;
        let mut runtime = self.runtime.lock().await;
        runtime.time_offset_ms = offset;
        runtime.time_offset_valid_until = Some(Instant::now() + TIME_OFFSET_TTL);
        Ok(offset)
    }

    async fn csrf_token(&self) -> AppResult<String> {
        let key = self.csrf_key().await?;
        let offset = self.ensure_time_offset().await?;
        build_csrf_token(&key, now_unix_ms().saturating_add(offset))
    }

    async fn csrf_key(&self) -> AppResult<String> {
        {
            let runtime = self.runtime.lock().await;
            if let Some(cached) = &runtime.csrf_key
                && cached.valid_until > Instant::now()
            {
                return Ok(cached.value.clone());
            }
        }

        let mut url = self.url("config.json")?;
        url.query_pairs_mut()
            .append_pair("r", &now_unix_ms().to_string());
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            return Err(AppError::RetryableExternal(format!(
                "SmartClass config.json HTTP {}",
                response.status()
            )));
        }
        let config: DomainConfigEnvelope = response.json().await.map_err(|error| {
            AppError::External(format!("解析 SmartClass config.json 失败: {error}"))
        })?;
        let clear = self.decrypt_client_value(&config.domain_config)?;
        let decoded: DecryptedDomainConfig = serde_json::from_str(&clear).map_err(|error| {
            AppError::External(format!("解析 SmartClass domainConfig 明文失败: {error}"))
        })?;
        if decoded.csrk_key.chars().count() < 10 {
            return Err(AppError::External(
                "SmartClass csrkKey 长度不足 10，无法按数字下标生成 token".to_string(),
            ));
        }
        let mut runtime = self.runtime.lock().await;
        runtime.csrf_key = Some(CachedString {
            value: decoded.csrk_key.clone(),
            valid_until: Instant::now() + CSRK_KEY_TTL,
        });
        Ok(decoded.csrk_key)
    }

    fn base_request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(ACCEPT, "*/*")
            .header(CONTENT_LANGUAGE, DEFAULT_LANGUAGE)
            .header(X_REQUESTED_WITH.clone(), "XMLHttpRequest")
            .header(REFERER, self.course_list_url.as_str())
    }

    async fn send_api(
        &self,
        request: reqwest::RequestBuilder,
        operation: Option<&str>,
    ) -> AppResult<Value> {
        // 没有 platform token 的 bootstrap 请求与网页 ajaxSetup 行为保持一致。
        let request = request.header(SKIP_TOKEN_VERIFY_HEADER.clone(), "skip_token_verify");
        let response = request.send().await?;
        self.persist_cookies().await?;
        if response.status().is_redirection() {
            return Err(AppError::Unauthorized);
        }
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            let label = operation.unwrap_or("SmartClass API");
            return if status.is_server_error()
                || status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
            {
                Err(AppError::RetryableExternal(format!(
                    "{label} HTTP {status}"
                )))
            } else {
                Err(AppError::External(format!("{label} HTTP {status}")))
            };
        }
        parse_api_value(&body, true)
    }

    fn decrypt_client_value(&self, cipher_hex: &str) -> AppResult<String> {
        let mut bytes = hex::decode(cipher_hex.trim()).map_err(|error| {
            AppError::External(format!("SmartClass AES hex 解码失败: {error}"))
        })?;
        let decryptor = Decryptor::<Aes256>::new_from_slices(
            self.config.smartclass_csrf_aes_key.as_bytes(),
            self.config.smartclass_csrf_aes_iv.as_bytes(),
        )
        .map_err(|error| AppError::Config(format!("SmartClass AES key/IV 长度错误: {error}")))?;
        let clear = decryptor
            .decrypt_padded_mut::<Pkcs7>(&mut bytes)
            .map_err(|_| AppError::External("SmartClass AES/PKCS7 解密失败".to_string()))?;
        String::from_utf8(clear.to_vec())
            .map_err(|error| AppError::External(format!("SmartClass 解密结果不是 UTF-8: {error}")))
    }

    fn encrypt_client_value(&self, clear: &str) -> AppResult<String> {
        let plaintext = clear.as_bytes();
        let padded_len = (plaintext.len() / 16 + 1) * 16;
        let mut buffer = vec![0_u8; padded_len];
        buffer[..plaintext.len()].copy_from_slice(plaintext);
        let encryptor = Encryptor::<Aes256>::new_from_slices(
            self.config.smartclass_csrf_aes_key.as_bytes(),
            self.config.smartclass_csrf_aes_iv.as_bytes(),
        )
        .map_err(|error| AppError::Config(format!("SmartClass AES key/IV 长度错误: {error}")))?;
        let encrypted = encryptor
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .map_err(|_| AppError::Internal("SmartClass AES/PKCS7 加密失败".to_string()))?;
        Ok(hex::encode(encrypted))
    }

    fn url(&self, path: &str) -> AppResult<Url> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| AppError::Config(format!("构造 SmartClass URL 失败: {error}")))
    }

    fn current_cookie_header(&self) -> Option<String> {
        self.jar
            .cookies(&self.base_url)
            .and_then(|value| value.to_str().ok().map(ToOwned::to_owned))
            .filter(|value| !value.trim().is_empty())
    }

    async fn persist_cookies(&self) -> AppResult<()> {
        let Some(cookie_header) = self.current_cookie_header() else {
            return Ok(());
        };
        let payload = PersistedSession {
            version: 1,
            cookie_header,
            updated_at_unix_ms: now_unix_ms(),
        };
        write_session_file(&self.config.smartclass_session_file, &payload).await
    }
}

fn parse_api_value(body: &str, map_auth_redirect: bool) -> AppResult<Value> {
    let json: Value = serde_json::from_str(body).map_err(|error| {
        AppError::External(format!("解析 SmartClass JSON 响应失败: {error}"))
    })?;
    let envelope: SmartClassApiEnvelope = serde_json::from_value(json.clone()).map_err(|error| {
        AppError::External(format!("解析 SmartClass envelope 失败: {error}"))
    })?;
    if envelope.status == Some(302)
        || envelope
            .location
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("login.aspx"))
    {
        return Err(AppError::Unauthorized);
    }
    match envelope.success {
        Some(true) => Ok(envelope.value.unwrap_or(Value::Null)),
        Some(false) => {
            let message = envelope
                .message
                .unwrap_or_else(|| "SmartClass 返回 Success=false".to_string());
            if map_auth_redirect && (message.contains("登录") || message.contains("验证不通过")) {
                Err(AppError::Unauthorized)
            } else {
                Err(AppError::BadRequest(format!("SmartClass: {message}")))
            }
        }
        None => Ok(json),
    }
}

fn parse_i64_value(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

fn build_csrf_token(csrk_key: &str, timestamp_ms: i64) -> AppResult<String> {
    let digits = format!("{timestamp_ms:013}");
    if digits.len() != 13 || !digits.bytes().all(|value| value.is_ascii_digit()) {
        return Err(AppError::Internal(format!(
            "无法把时间戳转换为 13 位 csrkToken 输入: {timestamp_ms}"
        )));
    }
    let chars = csrk_key.chars().collect::<Vec<_>>();
    let mut token = String::with_capacity(13);
    for digit in digits.bytes() {
        let index = usize::from(digit - b'0');
        let ch = chars.get(index).ok_or_else(|| {
            AppError::External(format!("SmartClass csrkKey 缺少数字下标 {index}"))
        })?;
        token.push(*ch);
    }
    Ok(token)
}

async fn load_cookie_file(path: &Path, jar: &Jar, base_url: &Url) -> AppResult<()> {
    let text = match fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let session: PersistedSession = serde_json::from_str(&text).map_err(|error| {
        AppError::Io(format!(
            "解析 SmartClass session 文件失败: path={}, error={error}",
            path.display()
        ))
    })?;
    for pair in session.cookie_header.split(';') {
        let pair = pair.trim();
        if !pair.is_empty() && pair.contains('=') {
            jar.add_cookie_str(pair, base_url);
        }
    }
    Ok(())
}

async fn write_session_file(path: &Path, session: &PersistedSession) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec(session)
        .map_err(|error| AppError::Internal(format!("序列化 SmartClass session 失败: {error}")))?;
    let tmp = sibling_temp_path(path);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_data().await?;
    drop(file);
    #[cfg(windows)]
    if fs::try_exists(path).await.unwrap_or(false) {
        let _ = fs::remove_file(path).await;
    }
    fs::rename(&tmp, path).await?;
    Ok(())
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("smartclass-session.json");
    path.with_file_name(format!(".{name}.{}.tmp", Uuid::new_v4().simple()))
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn summarize_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => {
            let chars = value.chars().take(120).collect::<String>();
            if value.chars().count() > 120 {
                format!("{chars}…")
            } else {
                chars
            }
        }
        Value::Array(value) => format!("array(len={})", value.len()),
        Value::Object(value) => format!("object(keys={:?})", value.keys().collect::<Vec<_>>()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_matches_current_web_algorithm() {
        let key = "s0k6e5a1t3i46kglaz9";
        let timestamp = 1_791_438_000_000_i64;
        let expected = timestamp
            .to_string()
            .chars()
            .map(|digit| key.chars().nth(digit.to_digit(10).unwrap() as usize).unwrap())
            .collect::<String>();
        assert_eq!(build_csrf_token(key, timestamp).unwrap(), expected);
    }

    #[test]
    fn detects_business_login_redirect() {
        let body = r#"{"status":302,"localtion":"/login.aspx?ReturnUrl=%2fWebapi"}"#;
        assert!(matches!(
            parse_api_value(body, true),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn unwraps_success_value() {
        let body = r#"{"Success":true,"Message":"","Value":{"TotalCount":3,"Data":[]}}"#;
        let value = parse_api_value(body, true).unwrap();
        assert_eq!(value["TotalCount"], 3);
    }

    #[test]
    fn aes_round_trip_uses_same_public_client_parameters() {
        // 独立线上逆向已用当前 domainConfig 验证 AES-256-CBC/PKCS7。这里再用同一套参数
        // 做本地 round-trip，防止未来依赖升级导致 padding / 编码语义漂移。
        let key = b"80bdbdbaf7494add99198960d715d41b";
        let iv = b"bdbaf7494add9919";
        let clear = b"classflow-smartclass-test";
        let mut buffer = vec![0_u8; (clear.len() / 16 + 1) * 16];
        buffer[..clear.len()].copy_from_slice(clear);
        let cipher = Encryptor::<Aes256>::new_from_slices(key, iv)
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, clear.len())
            .unwrap()
            .to_vec();
        let mut cipher_copy = cipher;
        let decoded = Decryptor::<Aes256>::new_from_slices(key, iv)
            .unwrap()
            .decrypt_padded_mut::<Pkcs7>(&mut cipher_copy)
            .unwrap();
        assert_eq!(decoded, clear);
    }
}
