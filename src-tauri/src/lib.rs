use chrono::Utc;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, Position, Size, State};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutoStartExt};
use tauri_plugin_notification::NotificationExt;

const MAIN_WINDOW_LABEL: &str = "main";
const SETTINGS_FILE: &str = "settings.json";
const PANEL_WIDTH: u32 = 360;
const PANEL_HEIGHT: u32 = 420;
const TRAY_ID: &str = "main-tray";

const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const OPENAI_OAUTH_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const CLAUDE_OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_OAUTH_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const OPENAI_OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CLAUDE_OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_OAUTH_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    provider: String,
    window_type: String,
    window_start: i64,
    window_end: i64,
    used: f64,
    limit: f64,
    remaining_percent: f64,
    reset_at: i64,
    updated_at: i64,
    status: String,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    provider: String,
    refresh_interval_sec: u64,
    alert_threshold_percent: f64,
    collapse_delay_ms: u64,
    auto_start: bool,
    dock_position: String,
    selected_window_type: String,
    codex_auth_path: String,
    claude_auth_path: String,
    limits_by_window: HashMap<String, f64>,
}

impl Default for AppSettings {
    fn default() -> Self {
        let mut limits_by_window = HashMap::new();
        limits_by_window.insert("hourly5".to_string(), 100.0);
        limits_by_window.insert("weekly".to_string(), 100.0);

        Self {
            provider: "openai_oauth".to_string(),
            refresh_interval_sec: 120,
            alert_threshold_percent: 20.0,
            collapse_delay_ms: 0,
            auto_start: true,
            dock_position: "top-right".to_string(),
            selected_window_type: "hourly5".to_string(),
            codex_auth_path: "".to_string(),
            claude_auth_path: "".to_string(),
            limits_by_window,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OAuthStatus {
    available: bool,
    source: String,
    message: Option<String>,
}

struct AppState {
    settings: Mutex<AppSettings>,
    last_notification_epoch: Mutex<Option<i64>>,
    http_client: reqwest::Client,
    panel_pinned: AtomicBool,
    panel_hovered: AtomicBool,
    hover_generation: AtomicU64,
}

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
    claude_ai_oauth: Option<ClaudeOAuthTokens>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ClaudeOAuthTokens {
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

#[derive(Debug, Clone, Deserialize)]
struct OAuthRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeOAuthRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WhamUsageResponse {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<WhamRateLimit>,
    #[serde(default)]
    code_review_rate_limit: Option<WhamCodeReviewRateLimit>,
}

#[derive(Debug, Clone, Deserialize)]
struct WhamRateLimit {
    #[serde(default)]
    primary_window: Option<WhamWindow>,
    #[serde(default)]
    secondary_window: Option<WhamWindow>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(default)]
    five_hour: Option<ClaudeUsageWindow>,
    #[serde(default)]
    seven_day: Option<ClaudeUsageWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<ClaudeUsageWindow>,
    #[serde(default)]
    extra_usage: Option<ClaudeExtraUsage>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeUsageWindow {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<EpochOrDateTime>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeExtraUsage {
    #[serde(default)]
    is_enabled: Option<bool>,
    #[serde(default)]
    monthly_limit: Option<f64>,
    #[serde(default)]
    used_credits: Option<f64>,
    #[serde(default)]
    utilization: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum EpochOrDateTime {
    Int(i64),
    Float(f64),
    Text(String),
}

struct OpenAiAuthSource {
    path: Option<PathBuf>,
    source: String,
    auth: CodexAuthFile,
}

struct ClaudeAuthSource {
    path: Option<PathBuf>,
    source: String,
    auth: ClaudeCredentialsFile,
}

enum ApiFetchError {
    Unauthorized,
    RateLimited { retry_after_secs: Option<u64> },
    Http(String),
}

impl ApiFetchError {
    fn message(self) -> String {
        match self {
            Self::Unauthorized => "OAuth token is unauthorized".to_string(),
            Self::RateLimited { retry_after_secs } => {
                if let Some(secs) = retry_after_secs {
                    format!("Rate limited (retry-after {secs}s). Token may be expired — run `claude auth login` to refresh.")
                } else {
                    "Rate limited (HTTP 429). Token may be expired — run `claude auth login` to refresh.".to_string()
                }
            }
            Self::Http(msg) => msg,
        }
    }
}

fn compute_status(remaining_percent: f64, alert_threshold: f64) -> &'static str {
    if remaining_percent <= alert_threshold / 2.0 {
        "danger"
    } else if remaining_percent <= alert_threshold {
        "warning"
    } else {
        "ok"
    }
}

fn error_snapshot(provider: &str, error: String) -> UsageSnapshot {
    UsageSnapshot {
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
        message: Some(error),
    }
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> AppSettings {
    state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone()
}

#[tauri::command]
fn get_oauth_status(state: State<'_, AppState>) -> OAuthStatus {
    let settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    let openai_ok = resolve_openai_auth_source(&settings).is_ok();
    let claude_ok = resolve_claude_auth_source(&settings).is_ok();

    if openai_ok || claude_ok {
        let mut sources = Vec::new();
        if openai_ok {
            sources.push("OpenAI");
        }
        if claude_ok {
            sources.push("Claude");
        }
        OAuthStatus {
            available: true,
            source: sources.join(", "),
            message: Some("OAuth token(s) loaded".to_string()),
        }
    } else {
        OAuthStatus {
            available: false,
            source: "none".to_string(),
            message: Some(
                "No OAuth providers configured. Run `codex login` or `claude auth login`."
                    .to_string(),
            ),
        }
    }
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    mut settings: AppSettings,
) -> Result<AppSettings, String> {
    normalize_settings(&mut settings);
    persist_settings(&app, &settings)?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();

    if settings.auto_start {
        let _ = app.autolaunch().enable();
    } else {
        let _ = app.autolaunch().disable();
    }

    Ok(settings)
}

#[tauri::command]
fn hide_panel(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.panel_pinned.store(false, Ordering::Relaxed);
    state.hover_generation.fetch_add(1, Ordering::Relaxed);
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "Main window not found".to_string())?;
    window.hide().map_err(|e| e.to_string())
}

#[tauri::command]
fn panel_mouse_enter(state: State<'_, AppState>) {
    state.panel_hovered.store(true, Ordering::Relaxed);
    state.hover_generation.fetch_add(1, Ordering::Relaxed);
}

#[tauri::command]
fn panel_mouse_leave(app: AppHandle, state: State<'_, AppState>) {
    state.panel_hovered.store(false, Ordering::Relaxed);
    if state.panel_pinned.load(Ordering::Relaxed) {
        return;
    }
    let gen = state.hover_generation.load(Ordering::Relaxed);
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let state = handle.state::<AppState>();
        if state.hover_generation.load(Ordering::Relaxed) != gen {
            return;
        }
        if state.panel_pinned.load(Ordering::Relaxed) || state.panel_hovered.load(Ordering::Relaxed) {
            return;
        }
        if let Some(window) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
            let _ = window.hide();
        }
    });
}

#[tauri::command]
fn resize_panel(app: AppHandle, height: u32) -> Result<(), String> {
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "Main window not found".to_string())?;

    // Maintain bottom edge: grow/shrink upward
    let current_pos = window.outer_position().map_err(|e| e.to_string())?;
    let current_size = window.outer_size().map_err(|e| e.to_string())?;
    let bottom_y = current_pos.y + current_size.height as i32;
    let new_y = bottom_y - height as i32;

    window
        .set_size(Size::Physical(PhysicalSize::new(PANEL_WIDTH, height)))
        .map_err(|e| e.to_string())?;
    window
        .set_position(Position::Physical(PhysicalPosition::new(
            current_pos.x,
            new_y,
        )))
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn update_tray_status(app: AppHandle, status: String) -> Result<(), String> {
    let tray = app
        .tray_by_id(TRAY_ID)
        .ok_or_else(|| "Tray icon not found".to_string())?;
    let icon_bytes: &[u8] = match status.as_str() {
        "danger" => include_bytes!("../icons/tray-danger.png"),
        "warning" => include_bytes!("../icons/tray-warning.png"),
        _ => include_bytes!("../icons/tray-ok.png"),
    };
    let icon = tauri::image::Image::from_bytes(icon_bytes).map_err(|e| e.to_string())?;
    tray.set_icon(Some(icon)).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn notify_if_needed(
    app: AppHandle,
    state: State<'_, AppState>,
    snapshot: UsageSnapshot,
) -> Result<bool, String> {
    let settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();

    if snapshot.remaining_percent > settings.alert_threshold_percent {
        return Ok(false);
    }

    let now = Utc::now().timestamp();
    {
        let guard = state
            .last_notification_epoch
            .lock()
            .expect("notify mutex poisoned");
        if let Some(last) = *guard {
            if now - last < 3600 {
                return Ok(false);
            }
        }
    }

    app.notification()
        .builder()
        .title("panel4ai quota alert")
        .body(format!(
            "{}: remaining {:.1}%",
            snapshot.provider, snapshot.remaining_percent
        ))
        .show()
        .map_err(|e| e.to_string())?;

    *state
        .last_notification_epoch
        .lock()
        .expect("notify mutex poisoned") = Some(now);

    Ok(true)
}

#[tauri::command]
async fn get_all_usage_snapshots(state: State<'_, AppState>) -> Result<Vec<UsageSnapshot>, String> {
    let (settings, client) = {
        let s = state
            .settings
            .lock()
            .expect("settings mutex poisoned")
            .clone();
        let c = state.http_client.clone();
        (s, c)
    };

    let openai_available = resolve_openai_auth_source(&settings).is_ok();
    let claude_available = resolve_claude_auth_source(&settings).is_ok();

    if !openai_available && !claude_available {
        return Err(
            "No OAuth providers configured. Run `codex login` or `claude auth login`, then set paths in Settings if needed."
                .to_string(),
        );
    }

    let openai_fut = async {
        if openai_available {
            Some(match get_openai_usage_snapshot(&settings, &client).await {
                Ok(snap) => snap,
                Err(e) => error_snapshot("openai-oauth", e),
            })
        } else {
            None
        }
    };

    let claude_fut = async {
        if claude_available {
            Some(match get_claude_usage_snapshot(&settings, &client).await {
                Ok(snap) => snap,
                Err(e) => error_snapshot("claude-oauth", e),
            })
        } else {
            None
        }
    };

    let (openai_snap, claude_snap) = tokio::join!(openai_fut, claude_fut);
    let mut snapshots = Vec::new();
    if let Some(snap) = openai_snap {
        snapshots.push(snap);
    }
    if let Some(snap) = claude_snap {
        snapshots.push(snap);
    }

    Ok(snapshots)
}

async fn get_openai_usage_snapshot(
    settings: &AppSettings,
    client: &reqwest::Client,
) -> Result<UsageSnapshot, String> {
    let mut source = resolve_openai_auth_source(settings)?;
    let mut tokens = source
        .auth
        .tokens
        .clone()
        .ok_or_else(|| "OAuth tokens are missing in auth source".to_string())?;

    if tokens.access_token.trim().is_empty() {
        return Err("OAuth access token is empty. Please run codex login again.".to_string());
    }

    let account_id = tokens.account_id.clone();
    let usage = match fetch_wham_usage(client, &tokens.access_token, account_id.as_deref()).await {
        Ok(usage) => usage,
        Err(ApiFetchError::Unauthorized) => {
            let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
                "OAuth token expired and no refresh token found. Please run codex login."
                    .to_string()
            })?;

            let refreshed = refresh_openai_oauth_token(client, &refresh_token).await?;
            tokens.access_token = refreshed.access_token;
            if let Some(new_refresh_token) = refreshed.refresh_token {
                tokens.refresh_token = Some(new_refresh_token);
            }
            if let Some(new_id_token) = refreshed.id_token {
                tokens.id_token = Some(new_id_token);
            }

            if let Some(stored_tokens) = source.auth.tokens.as_mut() {
                *stored_tokens = tokens.clone();
            }
            source.auth.last_refresh = Some(Utc::now().to_rfc3339());
            if let Some(path) = source.path.as_ref() {
                persist_codex_auth_file(path, &source.auth)?;
            }

            fetch_wham_usage(client, &tokens.access_token, account_id.as_deref())
                .await
                .map_err(ApiFetchError::message)?
        }
        Err(err) => return Err(err.message()),
    };

    wham_to_snapshot(settings, &source.source, usage)
}

fn wham_to_snapshot(
    settings: &AppSettings,
    auth_source: &str,
    usage: WhamUsageResponse,
) -> Result<UsageSnapshot, String> {
    let selected = settings.selected_window_type.as_str();

    let (window_type, window) = match selected {
        "hourly5" => (
            "hourly5".to_string(),
            usage
                .rate_limit
                .as_ref()
                .and_then(|r| r.primary_window.clone())
                .or_else(|| {
                    usage
                        .rate_limit
                        .as_ref()
                        .and_then(|r| r.secondary_window.clone())
                }),
        ),
        "code_review_weekly" => (
            "code_review_weekly".to_string(),
            usage
                .code_review_rate_limit
                .as_ref()
                .and_then(|r| r.primary_window.clone())
                .or_else(|| {
                    usage
                        .rate_limit
                        .as_ref()
                        .and_then(|r| r.secondary_window.clone())
                }),
        ),
        _ => (
            "weekly".to_string(),
            usage
                .rate_limit
                .as_ref()
                .and_then(|r| r.secondary_window.clone())
                .or_else(|| {
                    usage
                        .rate_limit
                        .as_ref()
                        .and_then(|r| r.primary_window.clone())
                }),
        ),
    };

    let window = window.ok_or_else(|| "No usage window found in OAuth response".to_string())?;
    let used_percent = window.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let reset_at = window.reset_at.unwrap_or_else(|| Utc::now().timestamp());
    let window_seconds = window
        .limit_window_seconds
        .unwrap_or(match window_type.as_str() {
            "hourly5" => 18_000,
            _ => 604_800,
        });

    let remaining_percent = (100.0 - used_percent).clamp(0.0, 100.0);
    let status = compute_status(remaining_percent, settings.alert_threshold_percent).to_string();

    let provider = match usage.plan_type {
        Some(plan) if !plan.trim().is_empty() => format!("openai-oauth ({plan})"),
        _ => "openai-oauth".to_string(),
    };

    Ok(UsageSnapshot {
        provider,
        window_type,
        window_start: reset_at - window_seconds,
        window_end: reset_at,
        used: used_percent,
        limit: 100.0,
        remaining_percent,
        reset_at,
        updated_at: Utc::now().timestamp(),
        status,
        message: Some(format!("auth source: {auth_source}")),
    })
}

async fn get_claude_usage_snapshot(
    settings: &AppSettings,
    client: &reqwest::Client,
) -> Result<UsageSnapshot, String> {
    let mut source = resolve_claude_auth_source(settings)?;
    let mut tokens = source
        .auth
        .claude_ai_oauth
        .clone()
        .ok_or_else(|| "Claude OAuth tokens are missing in credentials file".to_string())?;

    if tokens.access_token.trim().is_empty() {
        return Err(
            "Claude OAuth access token is empty. Please run `claude auth login`.".to_string(),
        );
    }

    // If token is very old (expired > 24h), try to refresh first before calling usage API
    let token_long_expired = tokens
        .expires_at
        .map(|ea| ea < Utc::now().timestamp_millis() - 86_400_000)
        .unwrap_or(false);

    if token_long_expired {
        if let Some(refresh_token) = tokens.refresh_token.clone() {
            match refresh_claude_oauth_token(client, &refresh_token).await {
                Ok(refreshed) => {
                    log::info!("Claude token was long-expired, successfully refreshed");
                    apply_claude_refresh(&mut tokens, refreshed);
                    source.auth.claude_ai_oauth = Some(tokens.clone());
                    if let Some(path) = source.path.as_ref() {
                        persist_claude_credentials_file(path, &source.auth)?;
                    }
                }
                Err(_) => {
                    return Err(
                        "Claude token expired. Refresh failed — please run `claude auth login` to re-authenticate.".to_string(),
                    );
                }
            }
        } else {
            return Err(
                "Claude token expired and no refresh token. Please run `claude auth login`.".to_string(),
            );
        }
    }

    // Only proactively refresh if token expires within 5 min but isn't long-expired
    if is_token_expiring_soon(tokens.expires_at) {
        if let Some(refresh_token) = tokens.refresh_token.clone() {
            match refresh_claude_oauth_token(client, &refresh_token).await {
                Ok(refreshed) => {
                    apply_claude_refresh(&mut tokens, refreshed);
                    source.auth.claude_ai_oauth = Some(tokens.clone());
                    if let Some(path) = source.path.as_ref() {
                        persist_claude_credentials_file(path, &source.auth)?;
                    }
                }
                Err(err) => {
                    log::warn!("Proactive Claude token refresh failed: {err}");
                }
            }
        }
    }

    // First attempt. Only retry 429 if retry-after is short (< 30s).
    // Long retry-after (e.g. 2600s) means Anthropic is throttling the token — retrying is pointless.
    let first_result = match fetch_claude_usage(client, &tokens.access_token).await {
        Err(ApiFetchError::RateLimited {
            retry_after_secs: Some(secs),
        }) if secs <= 30 => {
            log::info!("Claude usage 429 (retry-after {secs}s), waiting...");
            tokio::time::sleep(std::time::Duration::from_secs(secs.max(2))).await;
            fetch_claude_usage(client, &tokens.access_token).await
        }
        Err(ApiFetchError::RateLimited {
            retry_after_secs: None,
        }) => {
            log::info!("Claude usage 429 (no retry-after), retrying after 10s...");
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            fetch_claude_usage(client, &tokens.access_token).await
        }
        other => other,
    };

    let usage = match first_result {
        Ok(usage) => usage,
        Err(ApiFetchError::Unauthorized) => {
            let refresh_token = tokens
        .refresh_token
        .clone()
        .ok_or_else(|| "Claude token expired and no refresh token found. Please run `claude auth login`.".to_string())?;

            let refreshed = refresh_claude_oauth_token(client, &refresh_token).await?;
            apply_claude_refresh(&mut tokens, refreshed);
            source.auth.claude_ai_oauth = Some(tokens.clone());
            if let Some(path) = source.path.as_ref() {
                persist_claude_credentials_file(path, &source.auth)?;
            }

            fetch_claude_usage(client, &tokens.access_token)
                .await
                .map_err(ApiFetchError::message)?
        }
        Err(err) => return Err(err.message()),
    };

    claude_to_snapshot(settings, &source.source, &tokens, usage)
}

fn claude_to_snapshot(
    settings: &AppSettings,
    auth_source: &str,
    tokens: &ClaudeOAuthTokens,
    usage: ClaudeUsageResponse,
) -> Result<UsageSnapshot, String> {
    let selected = settings.selected_window_type.as_str();
    let (window_type, window) = match selected {
        "hourly5" => (
            "hourly5".to_string(),
            usage.five_hour.clone().or(usage.seven_day.clone()),
        ),
        "code_review_weekly" => (
            "code_review_weekly".to_string(),
            usage
                .seven_day_sonnet
                .clone()
                .or(usage.seven_day.clone())
                .or(usage.five_hour.clone()),
        ),
        _ => (
            "weekly".to_string(),
            usage.seven_day.clone().or(usage.five_hour.clone()),
        ),
    };

    let window = window.ok_or_else(|| "Claude usage window is not available yet.".to_string())?;
    let used_percent = normalize_utilization(window.utilization.unwrap_or(0.0));
    let window_seconds = match window_type.as_str() {
        "hourly5" => 18_000,
        _ => 604_800,
    };
    let reset_at = parse_reset_epoch(window.resets_at.as_ref())
        .unwrap_or_else(|| Utc::now().timestamp() + window_seconds);
    let remaining_percent = (100.0 - used_percent).clamp(0.0, 100.0);
    let status = compute_status(remaining_percent, settings.alert_threshold_percent).to_string();

    let provider = {
        let mut parts: Vec<String> = Vec::new();
        if let Some(subscription_type) = tokens.subscription_type.as_ref() {
            if !subscription_type.trim().is_empty() {
                parts.push(subscription_type.trim().to_string());
            }
        }
        if let Some(rate_limit_tier) = tokens.rate_limit_tier.as_ref() {
            if !rate_limit_tier.trim().is_empty() {
                parts.push(rate_limit_tier.trim().to_string());
            }
        }

        if parts.is_empty() {
            "claude-oauth".to_string()
        } else {
            format!("claude-oauth ({})", parts.join(", "))
        }
    };

    let mut message = format!("auth source: {auth_source}");
    if let Some(extra_usage) = usage.extra_usage {
        if extra_usage.is_enabled.unwrap_or(false) {
            let mut extra_parts: Vec<String> = Vec::new();
            if let Some(utilization) = extra_usage.utilization {
                extra_parts.push(format!("{:.1}% used", normalize_utilization(utilization)));
            }
            if let Some(used_credits) = extra_usage.used_credits {
                extra_parts.push(format!("credits {:.0}", used_credits));
            }
            if let Some(monthly_limit) = extra_usage.monthly_limit {
                extra_parts.push(format!("monthly limit {:.0}", monthly_limit));
            }
            if !extra_parts.is_empty() {
                message = format!("{message}; extra usage {}", extra_parts.join(", "));
            }
        }
    }

    Ok(UsageSnapshot {
        provider,
        window_type,
        window_start: reset_at - window_seconds,
        window_end: reset_at,
        used: used_percent,
        limit: 100.0,
        remaining_percent,
        reset_at,
        updated_at: Utc::now().timestamp(),
        status,
        message: Some(message),
    })
}

async fn fetch_wham_usage(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<WhamUsageResponse, ApiFetchError> {
    let mut request = client
        .get(OPENAI_WHAM_USAGE_URL)
        .bearer_auth(access_token)
        .header("Accept", "application/json");

    if let Some(account_id) = account_id {
        if !account_id.trim().is_empty() {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
    }

    let response = request
        .send()
        .await
        .map_err(|e| ApiFetchError::Http(e.to_string()))?;

    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return Err(ApiFetchError::Unauthorized);
    }

    if !response.status().is_success() {
        return Err(ApiFetchError::Http(format!(
            "Failed to fetch OAuth usage: {}",
            response.status()
        )));
    }

    response
        .json::<WhamUsageResponse>()
        .await
        .map_err(|e| ApiFetchError::Http(format!("Invalid usage payload: {e}")))
}

async fn refresh_openai_oauth_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<OAuthRefreshResponse, String> {
    let response = client
        .post(OPENAI_OAUTH_REFRESH_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", OPENAI_OAUTH_CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to refresh OAuth token: {}",
            response.status()
        ));
    }

    response
        .json::<OAuthRefreshResponse>()
        .await
        .map_err(|e| format!("Invalid OAuth refresh response: {e}"))
}

async fn fetch_claude_usage(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<ClaudeUsageResponse, ApiFetchError> {
    let response = client
        .get(CLAUDE_OAUTH_USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", CLAUDE_OAUTH_BETA_HEADER)
        .header("Content-Type", "application/json")
        .header("User-Agent", concat!("panel4ai/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .map_err(|e| ApiFetchError::Http(e.to_string()))?;

    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return Err(ApiFetchError::Unauthorized);
    }

    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        let retry_after_secs = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        return Err(ApiFetchError::RateLimited { retry_after_secs });
    }

    if !response.status().is_success() {
        return Err(ApiFetchError::Http(format!(
            "Failed to fetch Claude OAuth usage: {}",
            response.status()
        )));
    }

    response
        .json::<ClaudeUsageResponse>()
        .await
        .map_err(|e| ApiFetchError::Http(format!("Invalid Claude usage payload: {e}")))
}

async fn refresh_claude_oauth_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<ClaudeOAuthRefreshResponse, String> {
    let response = client
        .post(CLAUDE_OAUTH_TOKEN_URL)
        .header("anthropic-beta", CLAUDE_OAUTH_BETA_HEADER)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_OAUTH_CLIENT_ID
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Failed to refresh Claude OAuth token: {status} — {body}"
        ));
    }

    response
        .json::<ClaudeOAuthRefreshResponse>()
        .await
        .map_err(|e| format!("Invalid Claude OAuth refresh response: {e}"))
}

fn apply_claude_refresh(tokens: &mut ClaudeOAuthTokens, refreshed: ClaudeOAuthRefreshResponse) {
    tokens.access_token = refreshed.access_token;
    if let Some(refresh_token) = refreshed.refresh_token {
        tokens.refresh_token = Some(refresh_token);
    }
    if let Some(expires_in) = refreshed.expires_in {
        tokens.expires_at = Some(Utc::now().timestamp_millis() + expires_in * 1000);
    }
    if let Some(scope) = refreshed.scope {
        tokens.scopes = scope
            .split_whitespace()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
}

/// Only proactively refresh if the token expires within 5 min.
/// If the token expired more than 1 hour ago, skip proactive refresh
/// (the API may still accept it; if not, the 401-retry path handles it).
fn is_token_expiring_soon(expires_at_ms: Option<i64>) -> bool {
    let Some(expires_at_ms) = expires_at_ms else {
        return false;
    };
    let now = Utc::now().timestamp_millis();
    expires_at_ms <= now + 300_000 && expires_at_ms >= now - 3_600_000
}

fn normalize_utilization(value: f64) -> f64 {
    // Claude API returns utilization as a percentage (0-100), not a fraction.
    value.clamp(0.0, 100.0)
}

fn parse_reset_epoch(value: Option<&EpochOrDateTime>) -> Option<i64> {
    let value = value?;
    match value {
        EpochOrDateTime::Int(v) => Some(normalize_epoch(*v)),
        EpochOrDateTime::Float(v) => Some(normalize_epoch(*v as i64)),
        EpochOrDateTime::Text(v) => {
            if let Ok(epoch) = v.parse::<i64>() {
                return Some(normalize_epoch(epoch));
            }
            chrono::DateTime::parse_from_rfc3339(v)
                .ok()
                .map(|d| d.timestamp())
        }
    }
}

fn normalize_epoch(value: i64) -> i64 {
    if value > 2_000_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn resolve_openai_auth_source(settings: &AppSettings) -> Result<OpenAiAuthSource, String> {
    if !settings.codex_auth_path.trim().is_empty() {
        let path = PathBuf::from(settings.codex_auth_path.trim());
        if !path.exists() {
            return Err(format!(
                "Configured codex auth file does not exist: {}",
                path.display()
            ));
        }

        let auth = read_codex_auth_file(&path)?;
        ensure_openai_auth_has_oauth_tokens(&auth)?;
        return Ok(OpenAiAuthSource {
            path: Some(path.clone()),
            source: format!("custom: {}", path.display()),
            auth,
        });
    }

    for path in candidate_codex_auth_paths() {
        if !path.exists() {
            continue;
        }

        let auth = match read_codex_auth_file(&path) {
            Ok(auth) => auth,
            Err(_) => continue,
        };

        if ensure_openai_auth_has_oauth_tokens(&auth).is_ok() {
            return Ok(OpenAiAuthSource {
                path: Some(path.clone()),
                source: format!("codex auth file: {}", path.display()),
                auth,
            });
        }
    }

    Err("OpenAI OAuth tokens not found. Run `codex login` or set path in Settings.".to_string())
}

fn resolve_claude_auth_source(settings: &AppSettings) -> Result<ClaudeAuthSource, String> {
    if !settings.claude_auth_path.trim().is_empty() {
        let path = PathBuf::from(settings.claude_auth_path.trim());
        if !path.exists() {
            return Err(format!(
                "Configured Claude credentials file does not exist: {}",
                path.display()
            ));
        }

        let auth = read_claude_credentials_file(&path)?;
        ensure_claude_auth_has_oauth_tokens(&auth)?;
        return Ok(ClaudeAuthSource {
            path: Some(path.clone()),
            source: format!("custom: {}", path.display()),
            auth,
        });
    }

    for path in candidate_claude_auth_paths() {
        if !path.exists() {
            continue;
        }

        let auth = match read_claude_credentials_file(&path) {
            Ok(auth) => auth,
            Err(_) => continue,
        };

        if ensure_claude_auth_has_oauth_tokens(&auth).is_ok() {
            return Ok(ClaudeAuthSource {
                path: Some(path.clone()),
                source: format!("claude credentials: {}", path.display()),
                auth,
            });
        }
    }

    Err("Claude OAuth tokens not found. Run `claude auth login` or set path in Settings."
        .to_string())
}

fn ensure_openai_auth_has_oauth_tokens(auth: &CodexAuthFile) -> Result<(), String> {
    let tokens = auth
        .tokens
        .as_ref()
        .ok_or_else(|| "tokens section is missing".to_string())?;

    if tokens.access_token.trim().is_empty() {
        return Err("access_token is empty".to_string());
    }

    Ok(())
}

fn ensure_claude_auth_has_oauth_tokens(auth: &ClaudeCredentialsFile) -> Result<(), String> {
    let tokens = auth
        .claude_ai_oauth
        .as_ref()
        .ok_or_else(|| "claudeAiOauth section is missing".to_string())?;

    if tokens.access_token.trim().is_empty() {
        return Err("claudeAiOauth.accessToken is empty".to_string());
    }

    Ok(())
}

fn read_codex_auth_file(path: &Path) -> Result<CodexAuthFile, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str::<CodexAuthFile>(&content).map_err(|e| e.to_string())
}

fn persist_codex_auth_file(path: &Path, auth: &CodexAuthFile) -> Result<(), String> {
    let mut doc: serde_json::Value = if path.exists() {
        let content = fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::Object(Default::default())
    };

    let update = serde_json::to_value(auth).map_err(|e| e.to_string())?;
    if let (Some(doc_obj), Some(update_obj)) = (doc.as_object_mut(), update.as_object()) {
        for (key, value) in update_obj {
            doc_obj.insert(key.clone(), value.clone());
        }
    }

    let content = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;
    set_file_permissions_600(path);
    Ok(())
}

fn read_claude_credentials_file(path: &Path) -> Result<ClaudeCredentialsFile, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str::<ClaudeCredentialsFile>(&content).map_err(|e| e.to_string())
}

fn persist_claude_credentials_file(
    path: &Path,
    auth: &ClaudeCredentialsFile,
) -> Result<(), String> {
    let mut doc: serde_json::Value = if path.exists() {
        let content = fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::Object(Default::default())
    };

    let update = serde_json::to_value(auth).map_err(|e| e.to_string())?;
    if let (Some(doc_obj), Some(update_obj)) = (doc.as_object_mut(), update.as_object()) {
        for (key, value) in update_obj {
            doc_obj.insert(key.clone(), value.clone());
        }
    }

    let content = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;
    set_file_permissions_600(path);
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_permissions_600(_path: &Path) {}

fn candidate_codex_auth_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        if !codex_home.trim().is_empty() {
            paths.push(PathBuf::from(codex_home).join("auth.json"));
        }
    }

    if let Some(home) = home_dir() {
        paths.push(home.join(".codex").join("auth.json"));
        paths.push(home.join(".config").join("codex").join("auth.json"));
    }

    paths
}

fn candidate_claude_auth_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home_dir() {
        paths.push(home.join(".claude").join(".credentials.json"));
        paths.push(
            home.join(".config")
                .join("claude")
                .join(".credentials.json"),
        );
    }
    paths
}

fn home_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return Some(PathBuf::from(home));
        }
    }

    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        if !user_profile.trim().is_empty() {
            return Some(PathBuf::from(user_profile));
        }
    }

    None
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let config_dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    Ok(config_dir.join(SETTINGS_FILE))
}

fn persist_settings(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    let content = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())
}

fn load_settings(app: &AppHandle) -> AppSettings {
    let path = match settings_path(app) {
        Ok(path) => path,
        Err(_) => return AppSettings::default(),
    };

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return AppSettings::default(),
    };

    let mut settings = serde_json::from_str::<AppSettings>(&content).unwrap_or_default();
    normalize_settings(&mut settings);
    settings
}

fn normalize_settings(settings: &mut AppSettings) {
    settings.collapse_delay_ms = 0;
}

#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    open::that(&url).map_err(|e| format!("Failed to open URL: {e}"))
}

#[tauri::command]
async fn start_openai_oauth(
    state: State<'_, AppState>,
    code_challenge: String,
    code_verifier: String,
    oauth_state: String,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let client = state.http_client.clone();
    let settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();

    // Start TCP listener BEFORE opening browser
    let listener = tokio::net::TcpListener::bind("127.0.0.1:1455")
        .await
        .map_err(|e| format!("Failed to bind port 1455: {e}"))?;

    // Build auth URL
    let mut auth_url =
        url::Url::parse("https://auth.openai.com/oauth/authorize").expect("valid base URL");
    auth_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", OPENAI_OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", OPENAI_OAUTH_REDIRECT_URI)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &oauth_state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "codex_cli_rs");

    open::that(auth_url.as_str()).map_err(|e| format!("Failed to open browser: {e}"))?;

    // Wait for valid callback in a loop (reject spurious/non-matching requests)
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    let code: String = loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| "Login timed out after 5 minutes".to_string())?
            .map_err(|e| format!("Failed to accept callback: {e}"))?;

        let mut buf = vec![0u8; 8192];
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let first_line = request.lines().next().unwrap_or("");
        let path = first_line.split_whitespace().nth(1).unwrap_or("");

        // Ignore non-callback paths (e.g. favicon.ico)
        if !path.starts_with("/auth/callback") {
            let html_404 = "<html><body>Not found</body></html>";
            let resp = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                html_404.len(), html_404
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.flush().await;
            continue;
        }

        let callback_url = match url::Url::parse(&format!("http://localhost{}", path)) {
            Ok(u) => u,
            Err(_) => continue,
        };

        // Check for error in callback
        let callback_error = callback_url.query_pairs().find(|(k, _)| k == "error").map(|(_, err)| {
            let desc = callback_url
                .query_pairs()
                .find(|(k, _)| k == "error_description")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            format!("Authorization denied: {} {}", err, desc)
        });

        // Validate state parameter
        let cb_state = callback_url.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.to_string());
        let state_valid = cb_state.as_deref() == Some(oauth_state.as_str());

        // Extract code
        let extracted_code = callback_url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string());

        let is_ok = callback_error.is_none() && state_valid && extracted_code.is_some();
        let html = if is_ok {
            "<html><body style=\"font-family:system-ui;text-align:center;padding:60px\"><h2>Login successful!</h2><p>You can close this tab.</p></body></html>"
        } else {
            "<html><body style=\"font-family:system-ui;text-align:center;padding:60px\"><h2>Login failed</h2><p>Authentication could not be completed. Please try again.</p></body></html>"
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(), html
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;
        drop(stream);

        if let Some(err) = callback_error {
            drop(listener);
            return Err(err);
        }
        if !state_valid {
            // State mismatch — might be a stale/spurious request, keep waiting
            log::warn!("OAuth callback with mismatched state, ignoring");
            continue;
        }
        match extracted_code {
            Some(c) => {
                drop(listener);
                break c;
            }
            None => continue,
        }
    };

    // Exchange code for tokens (form-urlencoded)
    let response = client
        .post(OPENAI_OAUTH_REFRESH_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", OPENAI_OAUTH_REDIRECT_URI),
            ("client_id", OPENAI_OAUTH_CLIENT_ID),
            ("code_verifier", code_verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Token exchange failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Token exchange failed: {status} — {body}"));
    }

    let token_response: OAuthRefreshResponse = response
        .json()
        .await
        .map_err(|e| format!("Invalid token response: {e}"))?;

    let auth = CodexAuthFile {
        openai_api_key: None,
        tokens: Some(CodexTokens {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            id_token: token_response.id_token,
            account_id: None,
        }),
        last_refresh: Some(Utc::now().to_rfc3339()),
    };

    let save_path = if !settings.codex_auth_path.trim().is_empty() {
        PathBuf::from(settings.codex_auth_path.trim())
    } else {
        candidate_codex_auth_paths()
            .into_iter()
            .next()
            .ok_or_else(|| "Cannot determine auth save path".to_string())?
    };

    if let Some(parent) = save_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    persist_codex_auth_file(&save_path, &auth)?;
    log::info!("OpenAI credentials saved to {}", save_path.display());

    Ok(())
}

#[tauri::command]
async fn exchange_claude_auth_code(
    state: State<'_, AppState>,
    code: String,
    code_verifier: String,
    oauth_state: String,
) -> Result<(), String> {
    let client = state.http_client.clone();
    let settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();

    // Handle CODE#STATE format: split on '#' and use just the code part
    let actual_code = if let Some(pos) = code.find('#') {
        code[..pos].to_string()
    } else {
        code.clone()
    };

    log::info!(
        "Exchanging Claude auth code (len={}, verifier_len={}, state_len={})",
        actual_code.len(),
        code_verifier.len(),
        oauth_state.len()
    );

    let response = client
        .post(CLAUDE_OAUTH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLAUDE_OAUTH_CLIENT_ID,
            "code": actual_code,
            "redirect_uri": CLAUDE_OAUTH_REDIRECT_URI,
            "code_verifier": code_verifier,
            "state": oauth_state
        }))
        .send()
        .await
        .map_err(|e| format!("Token exchange request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Token exchange failed: {status} — {body} [code_len={}, endpoint={}]",
            actual_code.len(),
            CLAUDE_OAUTH_TOKEN_URL
        ));
    }

    let token_response: ClaudeOAuthRefreshResponse = response
        .json()
        .await
        .map_err(|e| format!("Invalid token exchange response: {e}"))?;

    let mut tokens = ClaudeOAuthTokens::default();
    tokens.access_token = token_response.access_token;
    tokens.refresh_token = token_response.refresh_token;
    if let Some(expires_in) = token_response.expires_in {
        tokens.expires_at = Some(Utc::now().timestamp_millis() + expires_in * 1000);
    }
    if let Some(scope) = token_response.scope {
        tokens.scopes = scope
            .split_whitespace()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    let creds = ClaudeCredentialsFile {
        claude_ai_oauth: Some(tokens),
    };

    let save_path = if !settings.claude_auth_path.trim().is_empty() {
        PathBuf::from(settings.claude_auth_path.trim())
    } else {
        candidate_claude_auth_paths()
            .into_iter()
            .next()
            .ok_or_else(|| "Cannot determine credentials save path".to_string())?
    };

    if let Some(parent) = save_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    persist_claude_credentials_file(&save_path, &creds)?;
    log::info!("Claude credentials saved to {}", save_path.display());
    Ok(())
}

fn position_panel_near_tray(window: &tauri::WebviewWindow, tray_pos: Position, height: u32) {
    let (tx, ty) = match tray_pos {
        Position::Physical(p) => (p.x as f64, p.y as f64),
        Position::Logical(p) => (p.x, p.y),
    };
    let x = (tx - PANEL_WIDTH as f64 / 2.0) as i32;
    let y = (ty - height as f64 - 8.0) as i32;

    let _ = window.set_size(Size::Physical(PhysicalSize::new(PANEL_WIDTH, height)));
    let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, y)));
}

fn setup_tray(app: &tauri::App) -> Result<(), String> {
    let quit =
        MenuItem::with_id(app, "quit", "Quit", true, None::<&str>).map_err(|e| e.to_string())?;
    let menu = Menu::with_items(app, &[&quit]).map_err(|e| e.to_string())?;

    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-ok.png"))
        .map_err(|e| e.to_string())?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip("panel4ai")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            let app = tray.app_handle();
            match event {
                TrayIconEvent::Enter { rect, .. } => {
                    let state = app.state::<AppState>();
                    state.hover_generation.fetch_add(1, Ordering::Relaxed);
                    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                        if !window.is_visible().unwrap_or(false) {
                            position_panel_near_tray(&window, rect.position, PANEL_HEIGHT);
                            let _ = window.show();
                            // No set_focus — hover is just a preview
                        }
                    }
                }
                TrayIconEvent::Leave { .. } => {
                    let state = app.state::<AppState>();
                    if state.panel_pinned.load(Ordering::Relaxed) {
                        return;
                    }
                    let gen = state.hover_generation.load(Ordering::Relaxed);
                    let handle = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        let state = handle.state::<AppState>();
                        if state.hover_generation.load(Ordering::Relaxed) != gen {
                            return;
                        }
                        if state.panel_pinned.load(Ordering::Relaxed) || state.panel_hovered.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Some(window) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
                            let _ = window.hide();
                        }
                    });
                }
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    rect,
                    ..
                } => {
                    let state = app.state::<AppState>();
                    state.hover_generation.fetch_add(1, Ordering::Relaxed);
                    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                        let pinned = state.panel_pinned.load(Ordering::Relaxed);
                        if pinned && window.is_visible().unwrap_or(false) {
                            state.panel_pinned.store(false, Ordering::Relaxed);
                            let _ = window.hide();
                        } else {
                            state.panel_pinned.store(true, Ordering::Relaxed);
                            if !window.is_visible().unwrap_or(false) {
                                position_panel_near_tray(&window, rect.position, PANEL_HEIGHT);
                                let _ = window.show();
                            }
                            let _ = window.set_focus();
                        }
                    }
                }
                _ => {}
            }
        })
        .build(app)
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("failed to create HTTP client");

    tauri::Builder::default()
        .manage(AppState {
            settings: Mutex::new(AppSettings::default()),
            last_notification_epoch: Mutex::new(None),
            http_client,
            panel_pinned: AtomicBool::new(false),
            panel_hovered: AtomicBool::new(false),
            hover_generation: AtomicU64::new(0),
        })
        .plugin(tauri_plugin_log::Builder::default().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let loaded = load_settings(&app.handle());
            let state: State<'_, AppState> = app.state();
            *state.settings.lock().expect("settings mutex poisoned") = loaded.clone();

            if loaded.auto_start {
                let _ = app.handle().autolaunch().enable();
            }

            setup_tray(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            get_oauth_status,
            save_settings,
            get_all_usage_snapshots,
            hide_panel,
            panel_mouse_enter,
            panel_mouse_leave,
            resize_panel,
            update_tray_status,
            notify_if_needed,
            open_external_url,
            start_openai_oauth,
            exchange_claude_auth_code
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- compute_status ---

    #[test]
    fn compute_status_ok_above_threshold() {
        assert_eq!(compute_status(50.0, 20.0), "ok");
    }

    #[test]
    fn compute_status_warning_at_threshold() {
        assert_eq!(compute_status(20.0, 20.0), "warning");
    }

    #[test]
    fn compute_status_warning_between_threshold_and_half() {
        assert_eq!(compute_status(15.0, 20.0), "warning");
    }

    #[test]
    fn compute_status_danger_at_half_threshold() {
        assert_eq!(compute_status(10.0, 20.0), "danger");
    }

    #[test]
    fn compute_status_danger_below_half() {
        assert_eq!(compute_status(5.0, 20.0), "danger");
    }

    #[test]
    fn compute_status_danger_at_zero() {
        assert_eq!(compute_status(0.0, 20.0), "danger");
    }

    // --- error_snapshot ---

    #[test]
    fn error_snapshot_fields() {
        let snap = error_snapshot("test-provider", "something broke".to_string());
        assert_eq!(snap.provider, "test-provider");
        assert_eq!(snap.status, "error");
        assert_eq!(snap.message, Some("something broke".to_string()));
        assert_eq!(snap.used, 0.0);
        assert_eq!(snap.limit, 0.0);
    }

    // --- normalize_settings ---

    #[test]
    fn normalize_settings_resets_collapse_delay() {
        let mut s = AppSettings::default();
        s.collapse_delay_ms = 500;
        normalize_settings(&mut s);
        assert_eq!(s.collapse_delay_ms, 0);
    }

    #[test]
    fn normalize_settings_preserves_weekly_window_type() {
        let mut s = AppSettings::default();
        s.selected_window_type = "weekly".to_string();
        normalize_settings(&mut s);
        assert_eq!(s.selected_window_type, "weekly");
    }

    // --- normalize_epoch ---

    #[test]
    fn normalize_epoch_seconds() {
        assert_eq!(normalize_epoch(1700000000), 1700000000);
    }

    #[test]
    fn normalize_epoch_below_threshold_stays() {
        // 1_700_000_000_000 is NOT > 2_000_000_000_000, so stays as-is
        assert_eq!(normalize_epoch(1_700_000_000_000), 1_700_000_000_000);
    }

    #[test]
    fn normalize_epoch_above_threshold_converts() {
        // Values > 2_000_000_000_000 are treated as milliseconds
        assert_eq!(normalize_epoch(2_000_000_000_001), 2_000_000_000);
    }

    // --- normalize_utilization ---

    #[test]
    fn normalize_utilization_within_range() {
        assert_eq!(normalize_utilization(50.0), 50.0);
    }

    #[test]
    fn normalize_utilization_clamps_above_100() {
        assert_eq!(normalize_utilization(150.0), 100.0);
    }

    #[test]
    fn normalize_utilization_clamps_below_0() {
        assert_eq!(normalize_utilization(-5.0), 0.0);
    }

    // --- parse_reset_epoch ---

    #[test]
    fn parse_reset_epoch_none() {
        assert_eq!(parse_reset_epoch(None), None);
    }

    #[test]
    fn parse_reset_epoch_int() {
        let val = EpochOrDateTime::Int(1700000000);
        assert_eq!(parse_reset_epoch(Some(&val)), Some(1700000000));
    }

    #[test]
    fn parse_reset_epoch_float() {
        let val = EpochOrDateTime::Float(1700000000.5);
        assert_eq!(parse_reset_epoch(Some(&val)), Some(1700000000));
    }

    #[test]
    fn parse_reset_epoch_millis_int() {
        // Only values > 2_000_000_000_000 get divided by 1000
        let val = EpochOrDateTime::Int(2_500_000_000_000);
        assert_eq!(parse_reset_epoch(Some(&val)), Some(2_500_000_000));
    }

    #[test]
    fn parse_reset_epoch_text_numeric() {
        let val = EpochOrDateTime::Text("1700000000".to_string());
        assert_eq!(parse_reset_epoch(Some(&val)), Some(1700000000));
    }

    #[test]
    fn parse_reset_epoch_text_rfc3339() {
        let val = EpochOrDateTime::Text("2023-11-14T22:13:20Z".to_string());
        let result = parse_reset_epoch(Some(&val));
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 1700000000);
    }

    #[test]
    fn parse_reset_epoch_text_invalid() {
        let val = EpochOrDateTime::Text("not-a-date".to_string());
        assert_eq!(parse_reset_epoch(Some(&val)), None);
    }

    // --- AppSettings default ---

    #[test]
    fn app_settings_default_values() {
        let s = AppSettings::default();
        assert_eq!(s.provider, "openai_oauth");
        assert_eq!(s.refresh_interval_sec, 60);
        assert_eq!(s.alert_threshold_percent, 20.0);
        assert_eq!(s.collapse_delay_ms, 0);
        assert!(s.auto_start);
        assert_eq!(s.selected_window_type, "hourly5");
        assert_eq!(s.limits_by_window.get("hourly5"), Some(&100.0));
        assert_eq!(s.limits_by_window.get("weekly"), Some(&100.0));
    }

    // --- AppSettings serde round-trip ---

    #[test]
    fn app_settings_serde_roundtrip() {
        let original = AppSettings::default();
        let json = serde_json::to_string(&original).unwrap();
        let restored: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(original.provider, restored.provider);
        assert_eq!(original.refresh_interval_sec, restored.refresh_interval_sec);
        assert_eq!(original.selected_window_type, restored.selected_window_type);
    }

    // --- UsageSnapshot serde ---

    #[test]
    fn usage_snapshot_serializes_camel_case() {
        let snap = UsageSnapshot {
            provider: "openai".to_string(),
            window_type: "hourly5".to_string(),
            window_start: 100,
            window_end: 200,
            used: 50.0,
            limit: 100.0,
            remaining_percent: 50.0,
            reset_at: 300,
            updated_at: 400,
            status: "ok".to_string(),
            message: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"windowType\""));
        assert!(json.contains("\"remainingPercent\""));
        assert!(json.contains("\"resetAt\""));
        assert!(!json.contains("window_type"));
    }

    // --- ApiFetchError ---

    #[test]
    fn api_fetch_error_unauthorized_message() {
        let err = ApiFetchError::Unauthorized;
        assert!(err.message().contains("unauthorized"));
    }

    #[test]
    fn api_fetch_error_rate_limited_with_retry() {
        let err = ApiFetchError::RateLimited { retry_after_secs: Some(60) };
        let msg = err.message();
        assert!(msg.contains("60s"));
    }

    #[test]
    fn api_fetch_error_rate_limited_without_retry() {
        let err = ApiFetchError::RateLimited { retry_after_secs: None };
        let msg = err.message();
        assert!(msg.contains("429"));
    }

    #[test]
    fn api_fetch_error_http() {
        let err = ApiFetchError::Http("server error".to_string());
        assert_eq!(err.message(), "server error");
    }

    // --- CodexAuthFile serde ---

    #[test]
    fn codex_auth_file_deserialize_with_api_key() {
        let json = r#"{"OPENAI_API_KEY":"sk-test","tokens":null}"#;
        let auth: CodexAuthFile = serde_json::from_str(json).unwrap();
        assert_eq!(auth.openai_api_key, Some("sk-test".to_string()));
    }

    #[test]
    fn codex_auth_file_deserialize_with_tokens() {
        let json = r#"{"tokens":{"access_token":"at","refresh_token":"rt"}}"#;
        let auth: CodexAuthFile = serde_json::from_str(json).unwrap();
        let tokens = auth.tokens.unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token, Some("rt".to_string()));
    }

    // --- WhamUsageResponse serde ---

    #[test]
    fn wham_usage_response_empty_json() {
        let json = "{}";
        let resp: WhamUsageResponse = serde_json::from_str(json).unwrap();
        assert!(resp.plan_type.is_none());
        assert!(resp.rate_limit.is_none());
    }

    // --- ClaudeUsageResponse serde ---

    #[test]
    fn claude_usage_response_partial_json() {
        let json = r#"{"five_hour":{"utilization":42.5}}"#;
        let resp: ClaudeUsageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.five_hour.unwrap().utilization, Some(42.5));
        assert!(resp.seven_day.is_none());
    }

    // --- is_token_expiring_soon ---

    #[test]
    fn token_expiring_soon_none() {
        assert!(!is_token_expiring_soon(None));
    }

    #[test]
    fn token_expiring_soon_far_future() {
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        assert!(!is_token_expiring_soon(Some(far_future)));
    }

    #[test]
    fn token_expiring_soon_within_5min() {
        let soon = Utc::now().timestamp_millis() + 60_000; // 1 min from now
        assert!(is_token_expiring_soon(Some(soon)));
    }

    #[test]
    fn token_expiring_soon_just_expired() {
        let just_expired = Utc::now().timestamp_millis() - 60_000; // 1 min ago
        assert!(is_token_expiring_soon(Some(just_expired)));
    }

    #[test]
    fn token_expiring_soon_expired_over_1h() {
        let long_ago = Utc::now().timestamp_millis() - 7_200_000; // 2h ago
        assert!(!is_token_expiring_soon(Some(long_ago)));
    }
}
