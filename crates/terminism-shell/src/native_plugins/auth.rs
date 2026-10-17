//! Auth service: Microsoft OAuth device code and authlib-injector logins.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use terminism_kernel::host::{HostHandle, Json, NativePlugin};
use terminism_kernel::logger::Logger;
use terminism_kernel::manifest::Manifest;

use super::manifest;

mod privacy {
    include!(concat!(env!("OUT_DIR"), "/privacy.rs"));
}

const SCOPE: &str = "XboxLive.signin offline_access";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const XBL_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN_URL: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

#[derive(Debug, Clone, PartialEq)]
pub struct AccountDraft {
    pub name: String,
    pub kind: String,
    pub uuid: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub server: Option<String>,
}

impl AccountDraft {
    fn to_json(&self) -> Json {
        json!({
            "name": self.name,
            "kind": self.kind,
            "uuid": self.uuid,
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "expires_at": self.expires_at,
            "server": self.server,
        })
    }
}

#[derive(Debug, Clone)]
enum Update {
    State {
        job: String,
        kind: String,
        phase: String,
        message: Option<String>,
    },
    Device {
        job: String,
        code: String,
        url: String,
        expires_in: u64,
    },
    Result {
        job: String,
        account: AccountDraft,
    },
    Error {
        job: String,
        kind: &'static str,
        message: String,
    },
}

#[derive(Debug)]
struct AuthError {
    kind: &'static str,
    message: String,
}

impl AuthError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

pub struct Auth {
    sender: Sender<Update>,
    receiver: Receiver<Update>,
    jobs: HashMap<String, Arc<AtomicBool>>,
    next_job: u64,
}

impl Default for Auth {
    fn default() -> Self {
        Self::new()
    }
}

impl Auth {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender,
            receiver,
            jobs: HashMap::new(),
            next_job: 0,
        }
    }

    fn spawn(
        &mut self,
        run: impl FnOnce(Sender<Update>, String, Arc<AtomicBool>) + Send + 'static,
    ) -> String {
        self.next_job += 1;
        let job = format!("auth-{}", self.next_job);
        let cancel = Arc::new(AtomicBool::new(false));
        self.jobs.insert(job.clone(), cancel.clone());
        let sender = self.sender.clone();
        let panic_sender = sender.clone();
        let thread_job = job.clone();
        let panic_job = thread_job.clone();
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(sender, thread_job, cancel)
            }));
            if outcome.is_err() {
                let _ = panic_sender.send(Update::Error {
                    job: panic_job,
                    kind: "auth_http",
                    message: "登录任务异常退出，请重试".to_string(),
                });
            }
        });
        job
    }

    fn begin_microsoft(&mut self, client_id: String) -> String {
        self.spawn(move |sender, job, cancel| microsoft_flow(sender, job, cancel, client_id))
    }

    fn begin_third_party(&mut self, server: String, username: String, password: String) -> String {
        self.spawn(move |sender, job, cancel| {
            third_party_flow(&sender, &job, &cancel, &server, &username, &password)
        })
    }

    fn begin_refresh(&mut self, account: Json, client_id: String) -> String {
        self.spawn(move |sender, job, cancel| {
            refresh_flow(sender, job, cancel, &account, client_id)
        })
    }

    fn microsoft_client_id(host: &HostHandle) -> Option<String> {
        let override_id = host
            .config()
            .borrow()
            .get("auth", "microsoft_client_id")
            .and_then(Json::as_str)
            .map(str::to_string);
        resolve_client_id(override_id.as_deref(), privacy::AZURE_CLIENT_ID)
    }

    fn require_client_id(host: &mut HostHandle) -> Option<String> {
        let client_id = Self::microsoft_client_id(host);
        if client_id.is_none() {
            host.publish(
                "auth.error",
                json!({
                    "job": "",
                    "kind": "auth_unconfigured",
                    "message": "未配置 Microsoft client id：请在 config.toml 设置 [auth] microsoft_client_id，或在本地创建 privacy.rs",
                }),
            );
        }
        client_id
    }

    fn cancel(&mut self, job: &str) {
        if let Some(cancel) = self.jobs.get(job) {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl NativePlugin for Auth {
    fn manifest(&self) -> Manifest {
        manifest(
            "auth",
            "Auth",
            &["auth.command"],
            "Runs Microsoft and authlib-injector login flows on worker threads",
        )
    }

    fn init(&mut self, _host: &mut HostHandle, logger: &Logger) {
        logger.log("auth service ready");
    }

    fn on_event(&mut self, name: &str, payload: &Json, host: &mut HostHandle, logger: &Logger) {
        if name != "auth.command" {
            return;
        }
        let action = payload.get("action").and_then(Json::as_str).unwrap_or("");
        match action {
            "microsoft.begin" => {
                let Some(client_id) = Self::require_client_id(host) else {
                    return;
                };
                let job = self.begin_microsoft(client_id);
                logger.log(&format!("auth job started: {job} microsoft"));
            }
            "third_party.begin" => {
                let server = payload.get("server").and_then(Json::as_str).unwrap_or("");
                let username = payload.get("username").and_then(Json::as_str).unwrap_or("");
                let password = payload.get("password").and_then(Json::as_str).unwrap_or("");
                let job = self.begin_third_party(
                    server.to_string(),
                    username.to_string(),
                    password.to_string(),
                );
                logger.log(&format!("auth job started: {job} third-party"));
            }
            "refresh" => {
                let Some(client_id) = Self::require_client_id(host) else {
                    return;
                };
                let account = payload.get("account").cloned().unwrap_or(Value::Null);
                let job = self.begin_refresh(account, client_id);
                logger.log(&format!("auth job started: {job} refresh"));
            }
            "cancel" => {
                let job = payload.get("job").and_then(Json::as_str).unwrap_or("");
                self.cancel(job);
                host.publish(
                    "auth.state",
                    json!({"job": job, "kind": "", "phase": "cancelled"}),
                );
            }
            other => logger.log(&format!("unknown auth command: {other}")),
        }
    }

    fn on_tick(&mut self, host: &mut HostHandle, logger: &Logger) {
        while let Ok(update) = self.receiver.try_recv() {
            match update {
                Update::State {
                    job,
                    kind,
                    phase,
                    message,
                } => {
                    host.publish(
                        "auth.state",
                        json!({"job": job, "kind": kind, "phase": phase, "message": message}),
                    );
                }
                Update::Device {
                    job,
                    code,
                    url,
                    expires_in,
                } => {
                    host.publish(
                        "auth.device",
                        json!({"job": job, "code": code, "url": url, "expires_in": expires_in}),
                    );
                }
                Update::Result { job, account } => {
                    self.jobs.remove(&job);
                    logger.log(&format!("auth job finished: {job}"));
                    host.publish(
                        "auth.result",
                        json!({"job": job, "account": account.to_json()}),
                    );
                }
                Update::Error { job, kind, message } => {
                    self.jobs.remove(&job);
                    logger.log(&format!("auth job failed: {job} {kind}"));
                    host.publish(
                        "auth.error",
                        json!({"job": job, "kind": kind, "message": message}),
                    );
                }
            }
        }
    }
}

/* ---------------- flow plumbing ---------------- */

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build();
    config.into()
}

fn map_error(error: ureq::Error) -> AuthError {
    match error {
        ureq::Error::Timeout(_) => AuthError::new("auth_network", "请求超时"),
        ureq::Error::HostNotFound => AuthError::new("auth_network", "无法解析服务器地址"),
        ureq::Error::ConnectionFailed => AuthError::new("auth_network", "网络连接失败"),
        ureq::Error::Io(error) => AuthError::new("auth_network", error.to_string()),
        ureq::Error::Tls(message) => AuthError::new("auth_network", format!("TLS 错误: {message}")),
        ureq::Error::StatusCode(status) => {
            AuthError::new("auth_http", format!("HTTP 状态码 {status}"))
        }
        other => AuthError::new("auth_http", other.to_string()),
    }
}

fn read_response(
    response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<(u16, Value), AuthError> {
    let mut response = response.map_err(map_error)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| AuthError::new("auth_http", error.to_string()))?;
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, value))
}

fn request_json(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Result<(u16, Value), AuthError> {
    if method == "GET" {
        let mut request = agent.get(url);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        read_response(request.call())
    } else {
        let mut request = agent.post(url);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.send_empty(),
        };
        read_response(response)
    }
}

fn request_form(
    agent: &ureq::Agent,
    url: &str,
    fields: &[(&str, &str)],
) -> Result<(u16, Value), AuthError> {
    let request = agent.post(url);
    read_response(request.send_form(fields.iter().copied()))
}

fn aadsts_hint(value: &Value) -> &'static str {
    let code = value
        .get("error_codes")
        .and_then(Value::as_array)
        .and_then(|codes| codes.first())
        .and_then(Value::as_u64);
    match code {
        Some(70002) => {
            "（Entra 应用需添加 Mobile and desktop applications 平台并允许公共客户端流；XboxLive.signin 还要求微软批准的应用注册，见 https://aka.ms/AppRegInfo）"
        }
        Some(700016) => "（client id 对应的 Entra 应用不存在）",
        Some(7000218) => "（Entra 应用未启用公共客户端流）",
        Some(9002325) | Some(70011) => {
            "（需要在 Entra 应用添加重定向 URI https://login.microsoftonline.com/common/oauth2/nativeclient）"
        }
        Some(65001) => "（Entra 应用缺少 XboxLive.signin 委托权限）",
        _ => "",
    }
}

fn error_text(value: &Value) -> String {
    value
        .get("error_description")
        .or_else(|| value.get("errorMessage"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("登录失败")
        .to_string()
}

fn is_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

fn cancelled() -> AuthError {
    AuthError::new("auth_cancelled", "登录已取消")
}

fn send(sender: &Sender<Update>, update: Update) {
    let _ = sender.send(update);
}

fn state(sender: &Sender<Update>, job: &str, kind: &str, phase: &str, message: Option<&str>) {
    send(
        sender,
        Update::State {
            job: job.to_string(),
            kind: kind.to_string(),
            phase: phase.to_string(),
            message: message.map(str::to_string),
        },
    );
}

fn report_error(sender: &Sender<Update>, job: &str, error: AuthError) {
    send(
        sender,
        Update::Error {
            job: job.to_string(),
            kind: error.kind,
            message: error.message,
        },
    );
}

fn microsoft_flow(sender: Sender<Update>, job: String, cancel: Arc<AtomicBool>, client_id: String) {
    state(&sender, &job, "microsoft", "starting", None);
    match microsoft_login(&sender, &job, &cancel, &client_id) {
        Ok(account) => {
            state(&sender, &job, "microsoft", "success", None);
            send(&sender, Update::Result { job, account });
        }
        Err(error) => report_error(&sender, &job, error),
    }
}

fn microsoft_login(
    sender: &Sender<Update>,
    job: &str,
    cancel: &AtomicBool,
    client_id: &str,
) -> Result<AccountDraft, AuthError> {
    let agent = agent();
    let (status, body) = request_form(
        &agent,
        DEVICE_CODE_URL,
        &[("client_id", client_id), ("scope", SCOPE)],
    )?;
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "auth_http",
            format!(
                "无法获取设备码（HTTP {status}）：{}{}",
                error_text(&body),
                aadsts_hint(&body)
            ),
        ));
    }
    let device_code = body
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "设备码响应缺少 device_code"))?
        .to_string();
    let user_code = body
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let url = body
        .get("verification_uri")
        .or_else(|| body.get("verification_url"))
        .and_then(Value::as_str)
        .unwrap_or("https://microsoft.com/link")
        .to_string();
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(900);
    let mut interval = body
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .max(1);
    send(
        sender,
        Update::Device {
            job: job.to_string(),
            code: user_code,
            url,
            expires_in,
        },
    );
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let token = loop {
        if is_cancelled(cancel) {
            return Err(cancelled());
        }
        if Instant::now() >= deadline {
            return Err(AuthError::new("auth_expired", "设备码已过期，请重新登录"));
        }
        std::thread::sleep(Duration::from_secs(interval));
        state(sender, job, "microsoft", "polling", None);
        let (status, body) = request_form(
            &agent,
            TOKEN_URL,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", &device_code),
            ],
        )?;
        if (200..300).contains(&status) {
            break body;
        }
        match body.get("error").and_then(Value::as_str).unwrap_or("") {
            "authorization_pending" => continue,
            "slow_down" => {
                interval += 5;
                continue;
            }
            "expired_token" => {
                return Err(AuthError::new("auth_expired", "设备码已过期，请重新登录"));
            }
            "authorization_declined" | "access_denied" => {
                return Err(AuthError::new("auth_denied", "用户拒绝了登录"));
            }
            _ => {
                return Err(AuthError::new(
                    "auth_http",
                    format!(
                        "令牌轮询失败（HTTP {status}）：{}{}",
                        error_text(&body),
                        aadsts_hint(&body)
                    ),
                ));
            }
        }
    };
    let access_token = token
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "令牌响应缺少 access_token"))?
        .to_string();
    let refresh_token = token
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    state(sender, job, "microsoft", "authenticating", None);
    let (name, uuid, mc_access, mc_expires) =
        minecraft_profile(&agent, &access_token, sender, job)?;
    Ok(AccountDraft {
        name,
        kind: "online".to_string(),
        uuid,
        access_token: mc_access,
        refresh_token,
        expires_at: Some(now_secs() + mc_expires),
        server: None,
    })
}

fn minecraft_profile(
    agent: &ureq::Agent,
    ms_access: &str,
    sender: &Sender<Update>,
    job: &str,
) -> Result<(String, String, String, u64), AuthError> {
    // Xbox Live user token
    let (status, body) = request_json(
        agent,
        "POST",
        XBL_URL,
        None,
        Some(&json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_access}"),
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        })),
    )?;
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "auth_denied",
            format!("Xbox Live 认证失败（HTTP {status}）：{}", error_text(&body)),
        ));
    }
    let xbl_token = body
        .get("Token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "Xbox 响应缺少 Token"))?
        .to_string();
    let uhs = body
        .get("DisplayClaims")
        .and_then(|claims| claims.get("xui"))
        .and_then(|xui| xui.get(0))
        .and_then(|entry| entry.get("uhs"))
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "Xbox 响应缺少 uhs"))?
        .to_string();
    // XSTS token
    let (status, body) = request_json(
        agent,
        "POST",
        XSTS_URL,
        None,
        Some(&json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [xbl_token],
            },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
        })),
    )?;
    if !(200..300).contains(&status) {
        let code = body.get("XErr").and_then(Value::as_u64).unwrap_or(0);
        let message = match code {
            2148916233 => "该微软账号没有 Xbox 账户，请先登录 Xbox 创建",
            2148916235 => "Xbox Live 在该账号所在地区不可用",
            2148916238 => "儿童账号需要家长批准后才能登录",
            _ => "XSTS 授权失败",
        };
        return Err(AuthError::new("auth_denied", message));
    }
    let xsts = body
        .get("Token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "XSTS 响应缺少 Token"))?
        .to_string();
    state(sender, job, "microsoft", "authenticating", None);
    let (status, body) = request_json(
        agent,
        "POST",
        MC_LOGIN_URL,
        None,
        Some(&json!({"identityToken": format!("XBL3.0 x={uhs};{xsts}")})),
    )?;
    if !(200..300).contains(&status) {
        if status == 403 {
            let message = body
                .get("errorMessage")
                .and_then(Value::as_str)
                .unwrap_or("Invalid app registration");
            return Err(AuthError::new(
                "auth_denied",
                format!(
                    "Minecraft 服务拒绝了该应用注册（{message}）；需要微软批准的 app registration，见 https://aka.ms/AppRegInfo"
                ),
            ));
        }
        return Err(AuthError::new(
            "auth_denied",
            format!("Minecraft 登录失败（HTTP {status}）"),
        ));
    }
    let mc_access = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "Minecraft 登录缺少 access_token"))?
        .to_string();
    let mc_expires = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(86400);
    let (status, profile) = request_json(agent, "GET", MC_PROFILE_URL, Some(&mc_access), None)?;
    if status == 404 {
        return Err(AuthError::new(
            "auth_no_profile",
            "该账号没有 Minecraft Java 版，请先在 minecraft.net 获取",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "auth_http",
            format!("获取 Minecraft 资料失败（HTTP {status}）"),
        ));
    }
    let name = profile
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "资料缺少 name"))?
        .to_string();
    let id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "资料缺少 id"))?;
    Ok((name, dashed_uuid(id), mc_access, mc_expires))
}

fn third_party_flow(
    sender: &Sender<Update>,
    job: &str,
    cancel: &AtomicBool,
    server: &str,
    username: &str,
    password: &str,
) {
    state(sender, job, "third-party", "starting", None);
    let result = third_party_login(sender, job, cancel, server, username, password);
    match result {
        Ok(account) => {
            state(sender, job, "third-party", "success", None);
            send(
                sender,
                Update::Result {
                    job: job.to_string(),
                    account,
                },
            );
        }
        Err(error) => report_error(sender, job, error),
    }
}

fn third_party_login(
    sender: &Sender<Update>,
    job: &str,
    cancel: &AtomicBool,
    server: &str,
    username: &str,
    password: &str,
) -> Result<AccountDraft, AuthError> {
    let server = normalize_server(server)?;
    if is_cancelled(cancel) {
        return Err(cancelled());
    }
    let agent = agent();
    let client_token = pseudo_uuid();
    state(sender, job, "third-party", "authenticating", None);
    let url = format!("{server}/authserver/authenticate");
    let (status, body) = request_json(
        &agent,
        "POST",
        &url,
        None,
        Some(&json!({
            "username": username,
            "password": password,
            "clientToken": client_token,
            "agent": {"name": "Minecraft", "version": 1},
            "requestUser": false,
        })),
    )?;
    if status == 403 || status == 401 {
        return Err(AuthError::new("auth_denied", "用户名或密码错误"));
    }
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "auth_http",
            format!("第三方登录失败（HTTP {status}）：{}", error_text(&body)),
        ));
    }
    let access_token = body
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "第三方响应缺少 accessToken"))?
        .to_string();
    let profile = body
        .get("selectedProfile")
        .filter(|value| value.is_object())
        .or_else(|| {
            body.get("availableProfiles")
                .and_then(Value::as_array)
                .and_then(|profiles| profiles.first())
        })
        .ok_or_else(|| AuthError::new("auth_no_profile", "该第三方账号没有可用角色"))?;
    let name = profile
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "第三方角色缺少 name"))?
        .to_string();
    let id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "第三方角色缺少 id"))?;
    Ok(AccountDraft {
        name,
        kind: "third-party".to_string(),
        uuid: dashed_uuid(id),
        access_token,
        refresh_token: None,
        expires_at: None,
        server: Some(server),
    })
}

fn refresh_flow(
    sender: Sender<Update>,
    job: String,
    cancel: Arc<AtomicBool>,
    account: &Json,
    client_id: String,
) {
    state(&sender, &job, "refresh", "starting", None);
    let result = refresh_account(&sender, &job, &cancel, account, &client_id);
    match result {
        Ok(account) => {
            state(&sender, &job, "refresh", "success", None);
            send(&sender, Update::Result { job, account });
        }
        Err(error) => report_error(&sender, &job, error),
    }
}

fn refresh_account(
    sender: &Sender<Update>,
    job: &str,
    cancel: &AtomicBool,
    account: &Json,
    client_id: &str,
) -> Result<AccountDraft, AuthError> {
    let kind = account.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != "online" {
        return Err(AuthError::new(
            "auth_unsupported",
            "该账号类型暂不支持刷新，请重新登录",
        ));
    }
    let refresh_token = account
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| AuthError::new("auth_expired", "缺少刷新令牌，请重新登录"))?;
    if is_cancelled(cancel) {
        return Err(cancelled());
    }
    let agent = agent();
    let (status, body) = request_form(
        &agent,
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ],
    )?;
    if !(200..300).contains(&status) {
        return Err(AuthError::new("auth_expired", "刷新失败，请重新登录"));
    }
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("auth_http", "刷新响应缺少 access_token"))?
        .to_string();
    let next_refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| Some(refresh_token.to_string()));
    state(sender, job, "refresh", "authenticating", None);
    let (name, uuid, mc_access, mc_expires) =
        minecraft_profile(&agent, &access_token, sender, job)?;
    Ok(AccountDraft {
        name,
        kind: "online".to_string(),
        uuid,
        access_token: mc_access,
        refresh_token: next_refresh,
        expires_at: Some(now_secs() + mc_expires),
        server: None,
    })
}

/* ---------------- pure helpers ---------------- */

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn pseudo_uuid() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&nanos.to_be_bytes());
    bytes[8..].copy_from_slice(&pid.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn dashed_uuid(raw: &str) -> String {
    let hex: String = raw.chars().filter(|character| *character != '-').collect();
    if hex.len() != 32 {
        return raw.to_string();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn normalize_server(server: &str) -> Result<String, AuthError> {
    let trimmed = server.trim().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return Err(AuthError::new(
            "auth_invalid_server",
            "请填写第三方服务器地址",
        ));
    }
    let secure = trimmed.starts_with("https://");
    let local = trimmed.starts_with("http://localhost") || trimmed.starts_with("http://127.0.0.1");
    if !secure && !local {
        return Err(AuthError::new(
            "auth_invalid_server",
            "第三方服务器地址必须使用 https",
        ));
    }
    Ok(trimmed)
}

fn resolve_client_id(config_override: Option<&str>, compiled: &str) -> Option<String> {
    config_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let compiled = compiled.trim();
            (!compiled.is_empty()).then(|| compiled.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashed_uuid_formats_mojang_ids() {
        assert_eq!(
            dashed_uuid("069a79f444e94726a5befca90e38aaf5"),
            "069a79f4-44e9-4726-a5be-fca90e38aaf5"
        );
        assert_eq!(dashed_uuid("bad"), "bad");
    }

    #[test]
    fn server_validation_requires_https_or_localhost() {
        assert_eq!(
            normalize_server("https://littleskin.cn/api/yggdrasil/").unwrap(),
            "https://littleskin.cn/api/yggdrasil"
        );
        assert!(normalize_server("http://littleskin.cn").is_err());
        assert!(normalize_server("http://localhost:8080").is_ok());
        assert!(normalize_server("").is_err());
    }

    #[test]
    fn client_id_prefers_config_then_compiled() {
        assert_eq!(
            resolve_client_id(Some(" config "), "compiled").as_deref(),
            Some("config")
        );
        assert_eq!(
            resolve_client_id(None, "compiled").as_deref(),
            Some("compiled")
        );
        assert_eq!(resolve_client_id(Some("  "), " "), None);
        assert_eq!(resolve_client_id(None, ""), None);
    }

    #[test]
    fn pseudo_uuid_has_uuid_shape() {
        let value = pseudo_uuid();
        assert_eq!(value.len(), 36);
        assert_eq!(
            value.chars().filter(|character| *character == '-').count(),
            4
        );
    }

    #[test]
    fn authlib_profile_picks_first_available() {
        let body = json!({
            "accessToken": "token",
            "availableProfiles": [{"id": "069a79f444e94726a5befca90e38aaf5", "name": "Steve"}],
        });
        let profile = body
            .get("selectedProfile")
            .filter(|value| value.is_object())
            .or_else(|| {
                body.get("availableProfiles")
                    .and_then(Value::as_array)
                    .and_then(|profiles| profiles.first())
            })
            .unwrap();
        assert_eq!(profile.get("name").and_then(Value::as_str), Some("Steve"));
    }

    #[test]
    fn aadsts_errors_get_actionable_hints() {
        assert!(aadsts_hint(&json!({"error_codes": [70002]})).contains("公共客户端"));
        assert!(aadsts_hint(&json!({"error_codes": [700016]})).contains("不存在"));
        assert_eq!(aadsts_hint(&json!({"error_codes": [1]})), "");
        assert_eq!(aadsts_hint(&json!({})), "");
    }

    #[test]
    fn device_code_response_fields_parse() {
        let body = json!({
            "device_code": "device",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://microsoft.com/link",
            "expires_in": 900,
            "interval": 5,
        });
        assert_eq!(
            body.get("user_code").and_then(Value::as_str),
            Some("ABCD-EFGH")
        );
        assert_eq!(body.get("interval").and_then(Value::as_u64), Some(5));
    }
}
