use chrono::Utc;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::timeout;

const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_BETA: &str = "oauth-2025-04-20";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub provider: String,
    pub window_type: String,
    pub window_start: i64,
    pub window_end: i64,
    pub used: f64,
    pub limit: f64,
    pub remaining_percent: f64,
    pub reset_at: i64,
    pub updated_at: i64,
    pub status: String,
    pub message: Option<String>,
}

impl UsageSnapshot {
    pub fn error(provider: &str, message: impl Into<String>) -> Self {
        Self {
            provider: provider.to_string(),
            window_type: String::new(),
            window_start: 0,
            window_end: 0,
            used: 0.0,
            limit: 0.0,
            remaining_percent: 0.0,
            reset_at: 0,
            updated_at: Utc::now().timestamp(),
            status: "error".to_string(),
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEnvelope {
    pub server_time: i64,
    pub last_poll_at: Option<i64>,
    pub snapshots: Vec<UsageSnapshot>,
    #[serde(default)]
    pub provider_errors: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ProviderPaths {
    pub codex_auth_path: PathBuf,
    pub codex_binary_path: Option<PathBuf>,
    pub claude_auth_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct QuotaClient {
    client: reqwest::Client,
    paths: ProviderPaths,
    alert_threshold_percent: f64,
}

#[derive(Debug, Clone)]
pub enum ProviderError {
    MissingCredentials(String),
    Unauthorized(String),
    RateLimited { retry_after_secs: Option<u64> },
    Transport(String),
    InvalidResponse(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCredentials(message)
            | Self::Unauthorized(message)
            | Self::Transport(message)
            | Self::InvalidResponse(message) => f.write_str(message),
            Self::RateLimited { retry_after_secs } => match retry_after_secs {
                Some(seconds) => write!(f, "rate limited; retry after {seconds}s"),
                None => f.write_str("rate limited"),
            },
        }
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CodexAuthFile {
    #[serde(rename = "OPENAI_API_KEY", default)]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<CodexTokens>,
    #[serde(default)]
    last_refresh: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CodexTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ClaudeCredentialsFile {
    #[serde(default)]
    claude_ai_oauth: Option<ClaudeTokens>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ClaudeTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    subscription_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhamUsageResponse {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<WhamRateLimit>,
    #[serde(default)]
    code_review_rate_limit: Option<WhamCodeReviewRateLimit>,
}

#[derive(Debug, Deserialize)]
struct WhamRateLimit {
    #[serde(default)]
    primary_window: Option<WhamWindow>,
    #[serde(default)]
    secondary_window: Option<WhamWindow>,
}

#[derive(Debug, Deserialize)]
struct WhamCodeReviewRateLimit {
    #[serde(default)]
    primary_window: Option<WhamWindow>,
}

#[derive(Debug, Clone, Deserialize)]
struct WhamWindow {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    reset_at: Option<i64>,
    #[serde(default)]
    limit_window_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppServerRateLimitsResult {
    rate_limits: AppServerRateLimits,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppServerRateLimits {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    primary: Option<AppServerRateLimitWindow>,
    #[serde(default)]
    secondary: Option<AppServerRateLimitWindow>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppServerRateLimitWindow {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    window_duration_mins: Option<i64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(flatten)]
    windows: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct ClaudeWindow {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<EpochOrDateTime>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EpochOrDateTime {
    Int(i64),
    Float(f64),
    Text(String),
}

impl QuotaClient {
    pub fn new(paths: ProviderPaths, alert_threshold_percent: f64) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("panel4ai-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            paths,
            alert_threshold_percent,
        })
    }

    pub async fn fetch_openai(&self) -> Result<Vec<UsageSnapshot>, ProviderError> {
        if let Some(binary_path) = self.paths.codex_binary_path.as_deref() {
            return self.fetch_openai_app_server(binary_path).await;
        }
        self.fetch_openai_legacy().await
    }

    async fn fetch_openai_legacy(&self) -> Result<Vec<UsageSnapshot>, ProviderError> {
        let mut document = read_json_document(&self.paths.codex_auth_path)?;
        let mut auth: CodexAuthFile =
            serde_json::from_value(document.clone()).map_err(|error| {
                ProviderError::InvalidResponse(format!("invalid Codex auth file: {error}"))
            })?;
        let mut tokens = auth.tokens.clone().ok_or_else(|| {
            ProviderError::MissingCredentials("Codex OAuth tokens are missing".to_string())
        })?;
        if tokens.access_token.trim().is_empty() {
            return Err(ProviderError::MissingCredentials(
                "Codex OAuth access token is empty".to_string(),
            ));
        }

        let first = self
            .fetch_openai_payload(&tokens.access_token, tokens.account_id.as_deref())
            .await;
        let usage = match first {
            Ok(usage) => usage,
            Err(ProviderError::Unauthorized(_)) => {
                let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
                    ProviderError::Unauthorized(
                        "Codex token expired and no refresh token is available".to_string(),
                    )
                })?;
                let refreshed = self.refresh_openai(&refresh_token).await?;
                tokens.access_token = refreshed.access_token;
                if let Some(token) = refreshed.refresh_token {
                    tokens.refresh_token = Some(token);
                }
                if let Some(token) = refreshed.id_token {
                    tokens.id_token = Some(token);
                }
                auth.tokens = Some(tokens.clone());
                auth.last_refresh = Some(Utc::now().to_rfc3339());
                merge_struct_into_document(&mut document, &auth)?;
                write_json_document_atomic(&self.paths.codex_auth_path, &document)?;
                self.fetch_openai_payload(&tokens.access_token, tokens.account_id.as_deref())
                    .await?
            }
            Err(error) => return Err(error),
        };

        Ok(self.openai_snapshots(usage))
    }

    async fn fetch_openai_app_server(
        &self,
        binary_path: &Path,
    ) -> Result<Vec<UsageSnapshot>, ProviderError> {
        let codex_home = self
            .paths
            .codex_auth_path
            .parent()
            .ok_or_else(|| {
                ProviderError::InvalidResponse(format!(
                    "{} has no parent directory",
                    self.paths.codex_auth_path.display()
                ))
            })?
            .to_path_buf();
        let home = codex_home
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| codex_home.clone());
        let mut command = Command::new(binary_path);
        command
            .arg("app-server")
            .arg("--stdio")
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            ProviderError::Transport(format!(
                "cannot start Codex app-server at {}: {error}",
                binary_path.display()
            ))
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Transport("Codex app-server stdin is unavailable".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Transport("Codex app-server stdout is unavailable".to_string())
        })?;
        let mut stdout = BufReader::new(stdout);

        let result = async {
            write_app_server_message(
                &mut stdin,
                serde_json::json!({
                    "method": "initialize",
                    "id": 1,
                    "params": {
                        "clientInfo": {
                            "name": "panel4ai",
                            "title": "Panel4AI quota monitor",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }
                }),
            )
            .await?;
            read_app_server_response(&mut stdout, 1).await?;
            write_app_server_message(
                &mut stdin,
                serde_json::json!({ "method": "initialized", "params": {} }),
            )
            .await?;
            write_app_server_message(
                &mut stdin,
                serde_json::json!({
                    "method": "account/rateLimits/read",
                    "id": 2,
                    "params": {}
                }),
            )
            .await?;
            let response = read_app_server_response(&mut stdout, 2).await?;
            parse_app_server_rate_limits(response)
        }
        .await;
        drop(stdin);
        stop_child(&mut child).await;
        result.map(|limits| self.app_server_openai_snapshots(limits))
    }

    pub async fn fetch_claude(&self) -> Result<Vec<UsageSnapshot>, ProviderError> {
        let mut document = read_json_document(&self.paths.claude_auth_path)?;
        let mut auth: ClaudeCredentialsFile =
            serde_json::from_value(document.clone()).map_err(|error| {
                ProviderError::InvalidResponse(format!("invalid Claude credentials: {error}"))
            })?;
        let mut tokens = auth.claude_ai_oauth.clone().ok_or_else(|| {
            ProviderError::MissingCredentials("Claude OAuth tokens are missing".to_string())
        })?;
        if tokens.access_token.trim().is_empty() {
            return Err(ProviderError::MissingCredentials(
                "Claude OAuth access token is empty".to_string(),
            ));
        }

        if token_expires_soon(tokens.expires_at) {
            if let Some(refresh_token) = tokens.refresh_token.clone() {
                let refreshed = self.refresh_claude(&refresh_token).await?;
                apply_claude_refresh(&mut tokens, refreshed);
                auth.claude_ai_oauth = Some(tokens.clone());
                merge_struct_into_document(&mut document, &auth)?;
                write_json_document_atomic(&self.paths.claude_auth_path, &document)?;
            }
        }

        let first = self.fetch_claude_payload(&tokens.access_token).await;
        let usage = match first {
            Ok(usage) => usage,
            Err(ProviderError::Unauthorized(_)) => {
                let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
                    ProviderError::Unauthorized(
                        "Claude token expired and no refresh token is available".to_string(),
                    )
                })?;
                let refreshed = self.refresh_claude(&refresh_token).await?;
                apply_claude_refresh(&mut tokens, refreshed);
                auth.claude_ai_oauth = Some(tokens.clone());
                merge_struct_into_document(&mut document, &auth)?;
                write_json_document_atomic(&self.paths.claude_auth_path, &document)?;
                self.fetch_claude_payload(&tokens.access_token).await?
            }
            // A 429 is deliberately not turned into a token refresh. The caller must
            // honor Retry-After and back off.
            Err(error) => return Err(error),
        };

        Ok(self.claude_snapshots(&tokens, usage))
    }

    async fn fetch_openai_payload(
        &self,
        access_token: &str,
        account_id: Option<&str>,
    ) -> Result<WhamUsageResponse, ProviderError> {
        let mut request = self
            .client
            .get(OPENAI_USAGE_URL)
            .bearer_auth(access_token)
            .header("Accept", "application/json");
        if let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
        parse_provider_response(request.send().await, "Codex").await
    }

    async fn fetch_claude_payload(
        &self,
        access_token: &str,
    ) -> Result<ClaudeUsageResponse, ProviderError> {
        let request = self
            .client
            .get(CLAUDE_USAGE_URL)
            .bearer_auth(access_token)
            .header("anthropic-beta", CLAUDE_BETA)
            .header("Accept", "application/json");
        parse_provider_response(request.send().await, "Claude").await
    }

    async fn refresh_openai(
        &self,
        refresh_token: &str,
    ) -> Result<OAuthRefreshResponse, ProviderError> {
        let response = self
            .client
            .post(OPENAI_TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", OPENAI_OAUTH_CLIENT_ID),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        parse_token_response(response, "Codex").await
    }

    async fn refresh_claude(
        &self,
        refresh_token: &str,
    ) -> Result<ClaudeRefreshResponse, ProviderError> {
        let response = self
            .client
            .post(CLAUDE_TOKEN_URL)
            .header("anthropic-beta", CLAUDE_BETA)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLAUDE_OAUTH_CLIENT_ID,
            }))
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        parse_token_response(response, "Claude").await
    }

    fn openai_snapshots(&self, usage: WhamUsageResponse) -> Vec<UsageSnapshot> {
        let provider = usage
            .plan_type
            .filter(|value| !value.trim().is_empty())
            .map(|plan| format!("openai-oauth ({plan})"))
            .unwrap_or_else(|| "openai-oauth".to_string());
        let mut windows = Vec::new();
        if let Some(rate_limit) = usage.rate_limit {
            if let Some(window) = rate_limit.primary_window {
                let kind = classify_openai_window(window.limit_window_seconds, "hourly5");
                windows.push((kind, window, 18_000));
            }
            if let Some(window) = rate_limit.secondary_window {
                let kind = classify_openai_window(window.limit_window_seconds, "weekly");
                windows.push((kind, window, 604_800));
            }
        }
        if let Some(window) = usage
            .code_review_rate_limit
            .and_then(|limit| limit.primary_window)
        {
            windows.push(("code_review_weekly".to_string(), window, 604_800));
        }
        windows
            .into_iter()
            .map(|(kind, window, default_seconds)| {
                let used = window.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
                let reset_at = window.reset_at.unwrap_or(0);
                let seconds = window.limit_window_seconds.unwrap_or(default_seconds);
                snapshot(
                    &provider,
                    &kind,
                    used,
                    reset_at,
                    seconds,
                    self.alert_threshold_percent,
                    None,
                )
            })
            .collect()
    }

    fn app_server_openai_snapshots(&self, limits: AppServerRateLimitsResult) -> Vec<UsageSnapshot> {
        let provider = limits
            .rate_limits
            .plan_type
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("openai-app-server ({})", value.trim()))
            .unwrap_or_else(|| "openai-app-server".to_string());
        let mut windows = Vec::new();
        if let Some(window) = limits.rate_limits.primary {
            let seconds = window.window_duration_mins.unwrap_or(300) * 60;
            let kind = classify_openai_window(Some(seconds), "hourly5");
            windows.push((kind, window, seconds));
        }
        if let Some(window) = limits.rate_limits.secondary {
            let seconds = window.window_duration_mins.unwrap_or(10_080) * 60;
            let kind = classify_openai_window(Some(seconds), "weekly");
            windows.push((kind, window, seconds));
        }
        windows
            .into_iter()
            .map(|(kind, window, seconds)| {
                snapshot(
                    &provider,
                    &kind,
                    window.used_percent.unwrap_or(0.0).clamp(0.0, 100.0),
                    window.resets_at.unwrap_or(0),
                    seconds,
                    self.alert_threshold_percent,
                    None,
                )
            })
            .collect()
    }

    fn claude_snapshots(
        &self,
        tokens: &ClaudeTokens,
        usage: ClaudeUsageResponse,
    ) -> Vec<UsageSnapshot> {
        let mut details = Vec::new();
        if let Some(value) = tokens.subscription_type.as_deref() {
            if !value.trim().is_empty() {
                details.push(value.trim());
            }
        }
        if let Some(value) = tokens.rate_limit_tier.as_deref() {
            if !value.trim().is_empty() {
                details.push(value.trim());
            }
        }
        let provider = if details.is_empty() {
            "claude-oauth".to_string()
        } else {
            format!("claude-oauth ({})", details.join(", "))
        };

        let mut snapshots = Vec::new();
        for (key, value) in usage.windows {
            if key == "extra_usage" || !value.is_object() {
                continue;
            }
            let Ok(window) = serde_json::from_value::<ClaudeWindow>(value) else {
                continue;
            };
            if window.utilization.is_none() && window.resets_at.is_none() {
                continue;
            }
            let seconds = if key == "five_hour" { 18_000 } else { 604_800 };
            let kind = match key.as_str() {
                "five_hour" => "hourly5".to_string(),
                "seven_day" => "weekly".to_string(),
                "seven_day_sonnet" => "weekly_sonnet".to_string(),
                "seven_day_opus" => "weekly_opus".to_string(),
                other => other.to_string(),
            };
            snapshots.push(snapshot(
                &provider,
                &kind,
                window.utilization.unwrap_or(0.0).clamp(0.0, 100.0),
                parse_reset_epoch(window.resets_at.as_ref()).unwrap_or(0),
                seconds,
                self.alert_threshold_percent,
                None,
            ));
        }
        snapshots.sort_by(|left, right| left.window_type.cmp(&right.window_type));
        snapshots
    }
}

async fn write_app_server_message(
    stdin: &mut tokio::process::ChildStdin,
    message: Value,
) -> Result<(), ProviderError> {
    let mut encoded = serde_json::to_vec(&message)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    encoded.push(b'\n');
    stdin.write_all(&encoded).await.map_err(|error| {
        ProviderError::Transport(format!("cannot write to Codex app-server: {error}"))
    })?;
    stdin.flush().await.map_err(|error| {
        ProviderError::Transport(format!("cannot flush Codex app-server request: {error}"))
    })
}

async fn read_app_server_response(
    stdout: &mut BufReader<ChildStdout>,
    expected_id: i64,
) -> Result<Value, ProviderError> {
    timeout(Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            let bytes = stdout.read_line(&mut line).await.map_err(|error| {
                ProviderError::Transport(format!("cannot read Codex app-server response: {error}"))
            })?;
            if bytes == 0 {
                return Err(ProviderError::Transport(
                    "Codex app-server exited before replying".to_string(),
                ));
            }
            let message: Value = serde_json::from_str(&line).map_err(|error| {
                ProviderError::InvalidResponse(format!(
                    "invalid Codex app-server JSONL response: {error}"
                ))
            })?;
            if message.get("id").and_then(Value::as_i64) != Some(expected_id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(ProviderError::Unauthorized(format!(
                    "Codex app-server request failed: {}",
                    compact_json(error)
                )));
            }
            return message.get("result").cloned().ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "Codex app-server response has no result".to_string(),
                )
            });
        }
    })
    .await
    .map_err(|_| ProviderError::Transport("Codex app-server timed out".to_string()))?
}

fn parse_app_server_rate_limits(value: Value) -> Result<AppServerRateLimitsResult, ProviderError> {
    serde_json::from_value(value).map_err(|error| {
        ProviderError::InvalidResponse(format!(
            "invalid Codex app-server rate-limit response: {error}"
        ))
    })
}

fn compact_json(value: &Value) -> String {
    let encoded = value.to_string();
    const LIMIT_CHARS: usize = 500;
    if encoded.chars().count() <= LIMIT_CHARS {
        encoded
    } else {
        format!("{}…", encoded.chars().take(LIMIT_CHARS).collect::<String>())
    }
}

async fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

pub fn classify_openai_window(limit_window_seconds: Option<i64>, fallback: &str) -> String {
    match limit_window_seconds {
        Some(seconds) if seconds > 0 && seconds <= 86_400 => "hourly5".to_string(),
        Some(seconds) if seconds >= 259_200 => "weekly".to_string(),
        _ => fallback.to_string(),
    }
}

fn snapshot(
    provider: &str,
    window_type: &str,
    used: f64,
    reset_at: i64,
    window_seconds: i64,
    alert_threshold: f64,
    message: Option<String>,
) -> UsageSnapshot {
    let remaining = (100.0 - used).clamp(0.0, 100.0);
    let status = if remaining <= alert_threshold / 2.0 {
        "danger"
    } else if remaining <= alert_threshold {
        "warning"
    } else {
        "ok"
    };
    UsageSnapshot {
        provider: provider.to_string(),
        window_type: window_type.to_string(),
        window_start: if reset_at > 0 {
            reset_at - window_seconds
        } else {
            0
        },
        window_end: reset_at,
        used,
        limit: 100.0,
        remaining_percent: remaining,
        reset_at,
        updated_at: Utc::now().timestamp(),
        status: status.to_string(),
        message,
    }
}

async fn parse_provider_response<T: for<'de> Deserialize<'de>>(
    response: Result<reqwest::Response, reqwest::Error>,
    provider: &str,
) -> Result<T, ProviderError> {
    let response = response.map_err(|error| ProviderError::Transport(error.to_string()))?;
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Err(ProviderError::Unauthorized(format!(
            "{provider} OAuth token is unauthorized"
        )));
    }
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err(ProviderError::RateLimited {
            retry_after_secs: retry_after_seconds(&response),
        });
    }
    if !response.status().is_success() {
        return Err(ProviderError::Transport(format!(
            "{provider} usage request failed with HTTP {}",
            response.status()
        )));
    }
    response.json::<T>().await.map_err(|error| {
        ProviderError::InvalidResponse(format!("invalid {provider} usage response: {error}"))
    })
}

async fn parse_token_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    provider: &str,
) -> Result<T, ProviderError> {
    if !response.status().is_success() {
        return Err(ProviderError::Unauthorized(format!(
            "{provider} OAuth refresh failed with HTTP {}",
            response.status()
        )));
    }
    response.json::<T>().await.map_err(|error| {
        ProviderError::InvalidResponse(format!("invalid {provider} refresh response: {error}"))
    })
}

fn retry_after_seconds(response: &reqwest::Response) -> Option<u64> {
    let raw = response.headers().get("retry-after")?.to_str().ok()?;
    raw.parse::<u64>().ok()
}

fn token_expires_soon(expires_at_ms: Option<i64>) -> bool {
    let Some(expires_at_ms) = expires_at_ms else {
        return false;
    };
    expires_at_ms <= Utc::now().timestamp_millis() + 300_000
}

fn apply_claude_refresh(tokens: &mut ClaudeTokens, refreshed: ClaudeRefreshResponse) {
    tokens.access_token = refreshed.access_token;
    if let Some(refresh_token) = refreshed.refresh_token {
        tokens.refresh_token = Some(refresh_token);
    }
    if let Some(expires_in) = refreshed.expires_in {
        tokens.expires_at = Some(Utc::now().timestamp_millis() + expires_in * 1000);
    }
    if let Some(scope) = refreshed.scope {
        tokens.scopes = scope.split_whitespace().map(ToString::to_string).collect();
    }
}

fn parse_reset_epoch(value: Option<&EpochOrDateTime>) -> Option<i64> {
    match value? {
        EpochOrDateTime::Int(value) => Some(normalize_epoch(*value)),
        EpochOrDateTime::Float(value) => Some(normalize_epoch(*value as i64)),
        EpochOrDateTime::Text(value) => {
            value.parse::<i64>().ok().map(normalize_epoch).or_else(|| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|date| date.timestamp())
            })
        }
    }
}

fn normalize_epoch(value: i64) -> i64 {
    // Unix seconds remain well below this boundary for thousands of years;
    // current millisecond timestamps are around 1.8e12.
    if value > 100_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn read_json_document(path: &Path) -> Result<Value, ProviderError> {
    let content = fs::read_to_string(path).map_err(|error| {
        ProviderError::MissingCredentials(format!("cannot read {}: {error}", path.display()))
    })?;
    serde_json::from_str(&content).map_err(|error| {
        ProviderError::InvalidResponse(format!("invalid JSON in {}: {error}", path.display()))
    })
}

fn merge_struct_into_document<T: Serialize>(
    document: &mut Value,
    update: &T,
) -> Result<(), ProviderError> {
    let update = serde_json::to_value(update)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    let (Some(document), Some(update)) = (document.as_object_mut(), update.as_object()) else {
        return Err(ProviderError::InvalidResponse(
            "credentials document must be a JSON object".to_string(),
        ));
    };
    for (key, value) in update {
        document.insert(key.clone(), value.clone());
    }
    Ok(())
}

fn write_json_document_atomic(path: &Path, document: &Value) -> Result<(), ProviderError> {
    let parent = path.parent().ok_or_else(|| {
        ProviderError::InvalidResponse(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| ProviderError::Transport(error.to_string()))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let content = serde_json::to_vec_pretty(document)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    fs::write(&temporary, content).map_err(|error| ProviderError::Transport(error.to_string()))?;
    set_permissions_600(&temporary);
    fs::rename(&temporary, path).map_err(|error| ProviderError::Transport(error.to_string()))?;
    Ok(())
}

#[cfg(unix)]
fn set_permissions_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_permissions_600(_path: &Path) {}

pub fn normalize_server_url(raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw.trim()).map_err(|error| error.to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("server URL must use http or https".to_string());
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_reset_is_not_invented() {
        let snapshot = snapshot("provider", "hourly5", 40.0, 0, 18_000, 20.0, None);
        assert_eq!(snapshot.reset_at, 0);
        assert_eq!(snapshot.window_start, 0);
    }

    #[test]
    fn normalizes_millisecond_epochs() {
        assert_eq!(normalize_epoch(2_500_000_000_000), 2_500_000_000);
        assert_eq!(normalize_epoch(1_784_000_000_000), 1_784_000_000);
        assert_eq!(normalize_epoch(1_700_000_000), 1_700_000_000);
    }

    #[test]
    fn classifies_openai_windows_by_duration_not_position() {
        assert_eq!(classify_openai_window(Some(18_000), "weekly"), "hourly5");
        assert_eq!(classify_openai_window(Some(604_800), "hourly5"), "weekly");
        assert_eq!(classify_openai_window(None, "hourly5"), "hourly5");
    }

    #[test]
    fn supports_future_official_five_hour_and_weekly_windows() {
        let limits = parse_app_server_rate_limits(serde_json::json!({
            "rateLimits": {
                "primary": {
                    "usedPercent": 25,
                    "windowDurationMins": 300,
                    "resetsAt": 1_800_000_000
                },
                "secondary": {
                    "usedPercent": 40,
                    "windowDurationMins": 10_080,
                    "resetsAt": 1_800_500_000
                },
                "rateLimitReachedType": null
            },
            "rateLimitResetCredits": null
        }))
        .expect("valid app-server payload");
        let client = QuotaClient::new(
            ProviderPaths {
                codex_auth_path: "/tmp/codex/auth.json".into(),
                codex_binary_path: None,
                claude_auth_path: "/tmp/claude/auth.json".into(),
            },
            20.0,
        )
        .expect("quota client");
        let snapshots = client.app_server_openai_snapshots(limits);
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].window_type, "hourly5");
        assert_eq!(snapshots[0].remaining_percent, 75.0);
        assert_eq!(snapshots[1].window_type, "weekly");
        assert_eq!(snapshots[1].remaining_percent, 60.0);
    }

    #[test]
    fn supports_current_weekly_only_window_without_assuming_primary_is_five_hour() {
        let limits = parse_app_server_rate_limits(serde_json::json!({
            "rateLimits": {
                "primary": {
                    "usedPercent": 71,
                    "windowDurationMins": 10_080,
                    "resetsAt": 1_800_500_000
                },
                "secondary": null,
                "planType": "prolite"
            },
            "rateLimitResetCredits": {
                "availableCount": 0,
                "credits": []
            }
        }))
        .expect("valid weekly-only app-server payload");
        let client = QuotaClient::new(
            ProviderPaths {
                codex_auth_path: "/tmp/codex/auth.json".into(),
                codex_binary_path: None,
                claude_auth_path: "/tmp/claude/auth.json".into(),
            },
            20.0,
        )
        .expect("quota client");
        let snapshots = client.app_server_openai_snapshots(limits);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].window_type, "weekly");
        assert_eq!(snapshots[0].remaining_percent, 29.0);
        assert_eq!(snapshots[0].provider, "openai-app-server (prolite)");
    }

    #[test]
    fn server_url_gets_trailing_slash() {
        assert_eq!(
            normalize_server_url("http://azure-server:8787")
                .unwrap()
                .as_str(),
            "http://azure-server:8787/"
        );
    }
}
