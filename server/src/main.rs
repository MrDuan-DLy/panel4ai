use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{TimeZone, Utc};
use chrono_tz::Europe::London;
use panel4ai_core::{ProviderError, ProviderPaths, QuotaClient, SnapshotEnvelope, UsageSnapshot};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/panel4ai/server.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    bind_addr: String,
    database_path: PathBuf,
    codex_auth_path: PathBuf,
    claude_auth_path: PathBuf,
    api_token_file: PathBuf,
    postmark_token_file: PathBuf,
    postmark_from: String,
    postmark_to: String,
    postmark_message_stream: String,
    poll_interval_sec: u64,
    alert_threshold_percent: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:8787".to_string(),
            database_path: "/var/lib/panel4ai/panel4ai.sqlite3".into(),
            codex_auth_path: "/var/lib/panel4ai/auth/codex.json".into(),
            claude_auth_path: "/var/lib/panel4ai/auth/claude.json".into(),
            api_token_file: "/etc/panel4ai/api-token".into(),
            postmark_token_file: "/etc/panel4ai/postmark-token".into(),
            postmark_from: "Panel4AI <quota@example.com>".to_string(),
            postmark_to: "you@example.com".to_string(),
            postmark_message_stream: "outbound".to_string(),
            poll_interval_sec: 300,
            alert_threshold_percent: 20.0,
        }
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    started_at: i64,
    last_poll_at: Option<i64>,
    provider_errors: HashMap<String, String>,
    next_allowed_at: HashMap<String, i64>,
}

struct AppState {
    config: Config,
    quota: QuotaClient,
    database: Mutex<Connection>,
    postmark_client: reqwest::Client,
    postmark_token: Option<String>,
    api_token_hash: [u8; 32],
    runtime: RwLock<RuntimeStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    server_time: i64,
    started_at: i64,
    last_poll_at: Option<i64>,
    postmark_configured: bool,
    pending_emails: i64,
    providers: HashMap<String, &'static str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionResponse {
    ok: bool,
    message: String,
}

#[derive(Debug)]
struct OutboxItem {
    id: i64,
    event_id: String,
    provider: String,
    window_type: String,
    old_reset_at: i64,
    new_reset_at: i64,
    previous_remaining: f64,
    attempts: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PostmarkResponse {
    #[serde(default)]
    error_code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    message_id: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "panel4ai_server=info".into()),
        )
        .with_target(false)
        .init();

    let config_path = env::var("PANEL4AI_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.into());
    let config = load_config(Path::new(&config_path))?;
    ensure_parent(&config.database_path)?;
    let database = Connection::open(&config.database_path)?;
    initialize_database(&database)?;

    let api_token = read_required_secret(&config.api_token_file)?;
    let postmark_token = read_optional_secret(&config.postmark_token_file)?;
    let quota = QuotaClient::new(
        ProviderPaths {
            codex_auth_path: config.codex_auth_path.clone(),
            claude_auth_path: config.claude_auth_path.clone(),
        },
        config.alert_threshold_percent,
    )?;
    let postmark_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("panel4ai-server/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let state = Arc::new(AppState {
        config: config.clone(),
        quota,
        database: Mutex::new(database),
        postmark_client,
        postmark_token,
        api_token_hash: sha256(api_token.as_bytes()),
        runtime: RwLock::new(RuntimeStatus {
            started_at: Utc::now().timestamp(),
            ..RuntimeStatus::default()
        }),
    });

    let poll_state = Arc::clone(&state);
    tokio::spawn(async move { poll_loop(poll_state).await });
    let outbox_state = Arc::clone(&state);
    tokio::spawn(async move { outbox_loop(outbox_state).await });

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/snapshots", get(snapshots))
        .route("/api/v1/poll", post(force_poll))
        .route("/api/v1/test-email", post(test_email))
        .with_state(state);

    let address: SocketAddr = config.bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(%address, "panel4ai server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)?;
    if config.poll_interval_sec < 60 {
        return Err("poll_interval_sec must be at least 60".into());
    }
    if config.postmark_from.trim().is_empty() || config.postmark_to.trim().is_empty() {
        return Err("Postmark sender and recipient must not be empty".into());
    }
    Ok(config)
}

fn ensure_parent(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn read_required_secret(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let value = fs::read_to_string(path)?.trim().to_string();
    if value.is_empty() {
        return Err(format!("secret file {} is empty", path.display()).into());
    }
    Ok(value)
}

fn read_optional_secret(path: &Path) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match fs::read_to_string(path) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_string())),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn initialize_database(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS snapshots (
            provider_key TEXT NOT NULL,
            window_type TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            reset_at INTEGER NOT NULL,
            observed_at INTEGER NOT NULL,
            PRIMARY KEY (provider_key, window_type)
        );
        CREATE TABLE IF NOT EXISTS reset_events (
            event_id TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            window_type TEXT NOT NULL,
            old_reset_at INTEGER NOT NULL,
            new_reset_at INTEGER NOT NULL,
            previous_remaining REAL NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS email_outbox (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id TEXT NOT NULL UNIQUE REFERENCES reset_events(event_id),
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL,
            last_error TEXT,
            postmark_message_id TEXT,
            sent_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS email_outbox_pending
            ON email_outbox(sent_at, next_attempt_at);
        ",
    )
}

async fn poll_loop(state: Arc<AppState>) {
    loop {
        if let Err(error) = poll_once(&state).await {
            error!(%error, "quota poll failed");
        }
        tokio::time::sleep(Duration::from_secs(state.config.poll_interval_sec)).await;
    }
}

async fn poll_once(state: &Arc<AppState>) -> Result<(), String> {
    let now = Utc::now().timestamp();
    let (openai_allowed, claude_allowed) = {
        let runtime = state.runtime.read().await;
        (
            runtime.next_allowed_at.get("openai").copied().unwrap_or(0) <= now,
            runtime.next_allowed_at.get("claude").copied().unwrap_or(0) <= now,
        )
    };

    let openai = async {
        if openai_allowed {
            Some(state.quota.fetch_openai().await)
        } else {
            None
        }
    };
    let claude = async {
        if claude_allowed {
            Some(state.quota.fetch_claude().await)
        } else {
            None
        }
    };
    let (openai, claude) = tokio::join!(openai, claude);

    if let Some(result) = openai {
        handle_provider_result(state, "openai", result).await?;
    }
    if let Some(result) = claude {
        handle_provider_result(state, "claude", result).await?;
    }
    state.runtime.write().await.last_poll_at = Some(Utc::now().timestamp());
    Ok(())
}

async fn handle_provider_result(
    state: &Arc<AppState>,
    provider_key: &str,
    result: Result<Vec<UsageSnapshot>, ProviderError>,
) -> Result<(), String> {
    match result {
        Ok(snapshots) => {
            let active_windows = snapshots
                .iter()
                .map(|snapshot| snapshot.window_type.clone())
                .collect::<HashSet<_>>();
            for snapshot in snapshots {
                persist_snapshot(state, provider_key, &snapshot)?;
            }
            prune_inactive_snapshots(state, provider_key, &active_windows)?;
            let mut runtime = state.runtime.write().await;
            runtime.provider_errors.remove(provider_key);
            runtime.next_allowed_at.remove(provider_key);
        }
        Err(error) => {
            let mut runtime = state.runtime.write().await;
            if let ProviderError::RateLimited { retry_after_secs } = &error {
                let delay = retry_after_secs
                    .unwrap_or(state.config.poll_interval_sec)
                    .max(60);
                runtime.next_allowed_at.insert(
                    provider_key.to_string(),
                    Utc::now().timestamp() + delay as i64,
                );
            }
            runtime
                .provider_errors
                .insert(provider_key.to_string(), error.to_string());
            warn!(provider = provider_key, %error, "provider poll failed");
        }
    }
    Ok(())
}

fn prune_inactive_snapshots(
    state: &Arc<AppState>,
    provider_key: &str,
    active_windows: &HashSet<String>,
) -> Result<(), String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let mut statement = database
        .prepare("SELECT window_type FROM snapshots WHERE provider_key = ?1")
        .map_err(|error| error.to_string())?;
    let existing = statement
        .query_map(params![provider_key], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for window_type in existing {
        if !active_windows.contains(&window_type) {
            database
                .execute(
                    "DELETE FROM snapshots WHERE provider_key = ?1 AND window_type = ?2",
                    params![provider_key, window_type],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn persist_snapshot(
    state: &Arc<AppState>,
    provider_key: &str,
    snapshot: &UsageSnapshot,
) -> Result<(), String> {
    let payload = serde_json::to_string(snapshot).map_err(|error| error.to_string())?;
    let now = Utc::now().timestamp();
    let mut database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let transaction = database.transaction().map_err(|error| error.to_string())?;
    let previous: Option<(i64, f64)> = transaction
        .query_row(
            "SELECT reset_at, json_extract(payload_json, '$.remainingPercent')
             FROM snapshots WHERE provider_key = ?1 AND window_type = ?2",
            params![provider_key, snapshot.window_type],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;

    transaction
        .execute(
            "INSERT INTO snapshots(provider_key, window_type, payload_json, reset_at, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider_key, window_type) DO UPDATE SET
               payload_json = excluded.payload_json,
               reset_at = excluded.reset_at,
               observed_at = excluded.observed_at",
            params![
                provider_key,
                snapshot.window_type,
                payload,
                snapshot.reset_at,
                now
            ],
        )
        .map_err(|error| error.to_string())?;

    if let Some((old_reset_at, previous_remaining)) = previous {
        if old_reset_at > 0 && snapshot.reset_at > old_reset_at + 60 {
            let event_id = reset_event_id(provider_key, &snapshot.window_type, old_reset_at);
            let inserted = transaction
                .execute(
                    "INSERT OR IGNORE INTO reset_events(
                       event_id, provider, window_type, old_reset_at, new_reset_at,
                       previous_remaining, created_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        event_id,
                        snapshot.provider,
                        snapshot.window_type,
                        old_reset_at,
                        snapshot.reset_at,
                        previous_remaining,
                        now
                    ],
                )
                .map_err(|error| error.to_string())?;
            if inserted > 0 {
                transaction
                    .execute(
                        "INSERT INTO email_outbox(event_id, next_attempt_at) VALUES (?1, ?2)",
                        params![event_id, now],
                    )
                    .map_err(|error| error.to_string())?;
                info!(
                    provider = provider_key,
                    window = snapshot.window_type,
                    old_reset_at,
                    new_reset_at = snapshot.reset_at,
                    "confirmed quota reset"
                );
            }
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn reset_event_id(provider: &str, window_type: &str, old_reset_at: i64) -> String {
    let digest = sha256(format!("{provider}|{window_type}|{old_reset_at}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn outbox_loop(state: Arc<AppState>) {
    loop {
        if let Err(error) = process_outbox(&state).await {
            error!(%error, "email outbox processing failed");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn process_outbox(state: &Arc<AppState>) -> Result<(), String> {
    if state.postmark_token.is_none() {
        return Ok(());
    }
    let items = load_pending_outbox(state)?;
    for item in items {
        match send_reset_email(state, &item).await {
            Ok(message_id) => mark_outbox_sent(state, item.id, &message_id)?,
            Err(error) => mark_outbox_failed(state, item.id, item.attempts + 1, &error)?,
        }
    }
    Ok(())
}

fn load_pending_outbox(state: &Arc<AppState>) -> Result<Vec<OutboxItem>, String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let mut statement = database
        .prepare(
            "SELECT o.id, e.event_id, e.provider, e.window_type, e.old_reset_at,
                    e.new_reset_at, e.previous_remaining, o.attempts
             FROM email_outbox o
             JOIN reset_events e ON e.event_id = o.event_id
             WHERE o.sent_at IS NULL AND o.next_attempt_at <= ?1
             ORDER BY o.id LIMIT 10",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![Utc::now().timestamp()], |row| {
            Ok(OutboxItem {
                id: row.get(0)?,
                event_id: row.get(1)?,
                provider: row.get(2)?,
                window_type: row.get(3)?,
                old_reset_at: row.get(4)?,
                new_reset_at: row.get(5)?,
                previous_remaining: row.get(6)?,
                attempts: row.get(7)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

async fn send_reset_email(state: &Arc<AppState>, item: &OutboxItem) -> Result<String, String> {
    let token = state
        .postmark_token
        .as_deref()
        .ok_or_else(|| "Postmark token is not configured".to_string())?;
    let old_local = format_timestamp(item.old_reset_at);
    let new_local = format_timestamp(item.new_reset_at);
    let provider_name = if item.provider.starts_with("openai") {
        "Codex"
    } else if item.provider.starts_with("claude") {
        "Claude"
    } else {
        &item.provider
    };
    let subject = format!("[Panel4AI] {provider_name} {} 额度已重置", item.window_type);
    let text = format!(
        "{provider_name} 的 {} 额度窗口已确认重置。\n\n上一个重置点：{old_local}\n下一个重置点：{new_local}\n重置前剩余：{:.1}%\n事件 ID：{}\n",
        item.window_type, item.previous_remaining, item.event_id
    );
    let html = format!(
        "<h2>{provider_name} 额度已重置</h2><p>窗口：<strong>{}</strong></p><ul><li>上一个重置点：{old_local}</li><li>下一个重置点：{new_local}</li><li>重置前剩余：{:.1}%</li></ul><p style=\"color:#777;font-size:12px\">事件 ID：{}</p>",
        item.window_type, item.previous_remaining, item.event_id
    );
    send_postmark(state, token, &subject, &text, &html, &item.event_id).await
}

async fn send_postmark(
    state: &Arc<AppState>,
    token: &str,
    subject: &str,
    text: &str,
    html: &str,
    event_id: &str,
) -> Result<String, String> {
    let response = state
        .postmark_client
        .post("https://api.postmarkapp.com/email")
        .header("Accept", "application/json")
        .header("X-Postmark-Server-Token", token)
        .json(&serde_json::json!({
            "From": state.config.postmark_from,
            "To": state.config.postmark_to,
            "Subject": subject,
            "TextBody": text,
            "HtmlBody": html,
            "MessageStream": state.config.postmark_message_stream,
            "Tag": "quota-reset",
            "Metadata": { "event_id": event_id }
        }))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let payload = response
        .json::<PostmarkResponse>()
        .await
        .map_err(|error| format!("Postmark HTTP {status}: invalid response: {error}"))?;
    if !status.is_success() || payload.error_code != 0 {
        return Err(format!(
            "Postmark HTTP {status}, code {}: {}",
            payload.error_code, payload.message
        ));
    }
    Ok(payload.message_id)
}

fn mark_outbox_sent(state: &Arc<AppState>, id: i64, message_id: &str) -> Result<(), String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    database
        .execute(
            "UPDATE email_outbox SET sent_at = ?1, postmark_message_id = ?2,
             last_error = NULL WHERE id = ?3",
            params![Utc::now().timestamp(), message_id, id],
        )
        .map_err(|error| error.to_string())?;
    info!(
        outbox_id = id,
        postmark_message_id = message_id,
        "email accepted"
    );
    Ok(())
}

fn mark_outbox_failed(
    state: &Arc<AppState>,
    id: i64,
    attempts: i64,
    message: &str,
) -> Result<(), String> {
    let delays = [60_i64, 300, 900, 3600, 21_600];
    let index = (attempts.saturating_sub(1) as usize).min(delays.len() - 1);
    let next_attempt = Utc::now().timestamp() + delays[index];
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    database
        .execute(
            "UPDATE email_outbox SET attempts = ?1, next_attempt_at = ?2,
             last_error = ?3 WHERE id = ?4",
            params![attempts, next_attempt, message, id],
        )
        .map_err(|error| error.to_string())?;
    warn!(
        outbox_id = id,
        attempts,
        error = message,
        "email send failed"
    );
    Ok(())
}

fn format_timestamp(epoch: i64) -> String {
    let Some(utc) = Utc.timestamp_opt(epoch, 0).single() else {
        return "未知".to_string();
    };
    format!(
        "{}（UTC {}）",
        utc.with_timezone(&London).format("%Y-%m-%d %H:%M:%S %Z"),
        utc.format("%Y-%m-%d %H:%M:%S")
    )
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let runtime = state.runtime.read().await;
    let pending_emails = state
        .database
        .lock()
        .ok()
        .and_then(|database| {
            database
                .query_row(
                    "SELECT COUNT(*) FROM email_outbox WHERE sent_at IS NULL",
                    [],
                    |row| row.get(0),
                )
                .ok()
        })
        .unwrap_or(-1);
    let mut providers = HashMap::new();
    for provider in ["openai", "claude"] {
        providers.insert(
            provider.to_string(),
            if runtime.provider_errors.contains_key(provider) {
                "error"
            } else {
                "ok"
            },
        );
    }
    Json(HealthResponse {
        status: "ok",
        server_time: Utc::now().timestamp(),
        started_at: runtime.started_at,
        last_poll_at: runtime.last_poll_at,
        postmark_configured: state.postmark_token.is_some(),
        pending_emails,
        providers,
    })
}

async fn snapshots(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SnapshotEnvelope>, StatusCode> {
    authorize(&state, &headers)?;
    let snapshots = load_snapshots(&state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let runtime = state.runtime.read().await;
    Ok(Json(SnapshotEnvelope {
        server_time: Utc::now().timestamp(),
        last_poll_at: runtime.last_poll_at,
        snapshots,
        provider_errors: runtime.provider_errors.clone(),
    }))
}

async fn force_poll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ActionResponse>, StatusCode> {
    authorize(&state, &headers)?;
    poll_once(&state)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ActionResponse {
        ok: true,
        message: "poll completed".to_string(),
    }))
}

async fn test_email(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ActionResponse>, (StatusCode, Json<ActionResponse>)> {
    authorize(&state, &headers).map_err(|status| {
        (
            status,
            Json(ActionResponse {
                ok: false,
                message: "unauthorized".to_string(),
            }),
        )
    })?;
    let token = state.postmark_token.as_deref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ActionResponse {
                ok: false,
                message: "Postmark token is not configured".to_string(),
            }),
        )
    })?;
    let event_id = format!("test-{}", Utc::now().timestamp());
    send_postmark(
        &state,
        token,
        "[Panel4AI] 测试邮件",
        "Panel4AI VPS 邮件通道工作正常。",
        "<h2>Panel4AI 测试成功</h2><p>VPS 邮件通道工作正常。</p>",
        &event_id,
    )
    .await
    .map_err(|message| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ActionResponse { ok: false, message }),
        )
    })?;
    Ok(Json(ActionResponse {
        ok: true,
        message: format!("test email accepted for {}", state.config.postmark_to),
    }))
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let candidate = sha256(header.as_bytes());
    if constant_time_eq(&candidate, &state.api_token_hash) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn sha256(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn load_snapshots(state: &Arc<AppState>) -> Result<Vec<UsageSnapshot>, String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let mut statement = database
        .prepare("SELECT payload_json FROM snapshots ORDER BY provider_key, window_type")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut snapshots = Vec::new();
    for row in rows {
        let payload = row.map_err(|error| error.to_string())?;
        snapshots.push(serde_json::from_str(&payload).map_err(|error| error.to_string())?);
    }
    Ok(snapshots)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_event_ids_are_stable_and_window_specific() {
        let first = reset_event_id("openai", "hourly5", 1_700_000_000);
        let second = reset_event_id("openai", "hourly5", 1_700_000_000);
        let different = reset_event_id("openai", "weekly", 1_700_000_000);
        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn secret_comparison_checks_all_bytes() {
        let expected = sha256(b"expected");
        assert!(constant_time_eq(&expected, &sha256(b"expected")));
        assert!(!constant_time_eq(&expected, &sha256(b"different")));
    }
}
