use axum::extract::{DefaultBodyLimit, Path as RoutePath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use panel4ai_core::{
    snapshot as build_snapshot, ProviderError, ProviderPaths, QuotaClient, SnapshotEnvelope,
    UsageSnapshot,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, RwLock, Semaphore};
use tracing::{error, info, warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/panel4ai/server.toml";
const SNAPSHOT_HISTORY_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
const RESET_RECOVERY_MIN_POINTS: f64 = 10.0;
const RESET_CONFIRMATION_MIN_POINTS: f64 = 1.5;
const UNUSED_WINDOW_MIN_REMAINING: f64 = 99.5;
const EARLY_RESET_REASON: &str = "usage_recovery_confirmed";
const PULL_PROVIDERS: [&str; 3] = ["openai", "claude", "grok"];
const PROVIDER_TICK_SECS: u64 = 15;
const MAX_BACKOFF_SECS: u64 = 3600;
const UNAUTHORIZED_ALERT_FAILURES: i64 = 3;
const BARK_MAX_AGE_SECS: i64 = 6 * 60 * 60;
const PUSH_WATCH_INTERVAL_SECS: u64 = 300;
const MAX_INGEST_WINDOWS: usize = 32;
const MAX_INGEST_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    bind_addr: String,
    database_path: PathBuf,
    codex_auth_path: PathBuf,
    codex_binary_path: PathBuf,
    codex_use_app_server: bool,
    claude_auth_path: PathBuf,
    api_token_file: PathBuf,
    postmark_token_file: PathBuf,
    postmark_from: String,
    postmark_to: String,
    postmark_message_stream: String,
    poll_interval_sec: u64,
    alert_threshold_percent: f64,
    enabled_providers: Vec<String>,
    push_providers: Vec<String>,
    provider_intervals: HashMap<String, u64>,
    grok_binary_path: PathBuf,
    grok_home: PathBuf,
    ingest_token_file: PathBuf,
    bark_key_file: PathBuf,
    bark_server: String,
    display_timezone: String,
    push_stale_after_sec: i64,
    push_stale_alert_after_sec: i64,
    notify: NotifyPolicy,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct NotifyPolicy {
    /// Email a scheduled reset of a short (5-hour) window, not only push it.
    short_window_email: bool,
    /// Push a scheduled short-window reset only when less than this was left.
    short_window_bark_below_remaining: f64,
}

impl Default for NotifyPolicy {
    fn default() -> Self {
        Self {
            short_window_email: false,
            short_window_bark_below_remaining: 50.0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:8787".to_string(),
            database_path: "/var/lib/panel4ai/panel4ai.sqlite3".into(),
            codex_auth_path: "/var/lib/panel4ai/auth/codex.json".into(),
            codex_binary_path: "/var/lib/panel4ai/.local/bin/codex".into(),
            codex_use_app_server: true,
            claude_auth_path: "/var/lib/panel4ai/auth/claude.json".into(),
            api_token_file: "/etc/panel4ai/api-token".into(),
            postmark_token_file: "/etc/panel4ai/postmark-token".into(),
            postmark_from: "Panel4AI <quota@example.com>".to_string(),
            postmark_to: "you@example.com".to_string(),
            postmark_message_stream: "outbound".to_string(),
            poll_interval_sec: 300,
            alert_threshold_percent: 20.0,
            enabled_providers: vec!["openai".to_string(), "claude".to_string()],
            push_providers: Vec::new(),
            provider_intervals: HashMap::new(),
            grok_binary_path: "/var/lib/panel4ai/.grok/bin/grok".into(),
            grok_home: "/var/lib/panel4ai/.grok".into(),
            ingest_token_file: "/etc/panel4ai/ingest-token".into(),
            bark_key_file: "/etc/panel4ai/bark-key".into(),
            bark_server: "https://api.day.app".to_string(),
            display_timezone: "Asia/Shanghai".to_string(),
            push_stale_after_sec: 3600,
            push_stale_alert_after_sec: 86_400,
            notify: NotifyPolicy::default(),
        }
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    started_at: i64,
    last_poll_at: Option<i64>,
    provider_errors: HashMap<String, String>,
    next_poll_at: HashMap<String, i64>,
    rate_limited_until: HashMap<String, i64>,
    failure_streak: HashMap<String, u32>,
}

struct AppState {
    config: Config,
    quota: QuotaClient,
    database: Mutex<Connection>,
    http_client: reqwest::Client,
    postmark_token: Option<String>,
    bark_key: Option<String>,
    api_token_hash: [u8; 32],
    ingest_token_hash: Option<[u8; 32]>,
    timezone: Tz,
    runtime: RwLock<RuntimeStatus>,
    provider_locks: HashMap<String, AsyncMutex<()>>,
    /// One provider CLI at a time, so small VPSes never hold several in memory.
    cli_gate: Semaphore,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    server_time: i64,
    started_at: i64,
    last_poll_at: Option<i64>,
    postmark_configured: bool,
    bark_configured: bool,
    pending_emails: i64,
    pending_pushes: i64,
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
    current_remaining: f64,
    event_kind: String,
    reason: String,
    detail: Option<String>,
    attempts: i64,
    created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Channels {
    email: bool,
    bark: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestRequest {
    #[serde(default)]
    windows: Vec<IngestWindow>,
    #[serde(default)]
    error: Option<IngestError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestWindow {
    window_type: String,
    used_percent: f64,
    reset_at: i64,
    window_seconds: i64,
}

#[derive(Debug, Deserialize)]
struct IngestError {
    kind: String,
    message: String,
}

#[derive(Debug)]
struct ResetCandidate {
    baseline_remaining: f64,
    candidate_remaining: f64,
    old_reset_at: i64,
    first_observed_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PostmarkResponse {
    #[serde(default)]
    error_code: i64,
    #[serde(default)]
    message: String,
    #[serde(default, rename = "MessageID")]
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
    let bark_key = read_optional_secret(&config.bark_key_file)?;
    let ingest_token = read_optional_secret(&config.ingest_token_file)?;
    let timezone = parse_timezone(&config.display_timezone)?;
    let quota = QuotaClient::new(
        ProviderPaths {
            codex_auth_path: config.codex_auth_path.clone(),
            codex_binary_path: config
                .codex_use_app_server
                .then(|| config.codex_binary_path.clone()),
            claude_auth_path: config.claude_auth_path.clone(),
            grok_binary_path: Some(config.grok_binary_path.clone()),
            grok_home: Some(config.grok_home.clone()),
        },
        config.alert_threshold_percent,
    )?;
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("panel4ai-server/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let provider_locks = config
        .enabled_providers
        .iter()
        .chain(&config.push_providers)
        .map(|provider| (provider.clone(), AsyncMutex::new(())))
        .collect();
    let state = Arc::new(AppState {
        config: config.clone(),
        quota,
        database: Mutex::new(database),
        http_client,
        postmark_token,
        bark_key,
        api_token_hash: sha256(api_token.as_bytes()),
        ingest_token_hash: ingest_token.map(|token| sha256(token.as_bytes())),
        timezone,
        runtime: RwLock::new(RuntimeStatus {
            started_at: Utc::now().timestamp(),
            ..RuntimeStatus::default()
        }),
        provider_locks,
        cli_gate: Semaphore::new(1),
    });

    for provider in &config.enabled_providers {
        let poll_state = Arc::clone(&state);
        let provider = provider.clone();
        tokio::spawn(async move { provider_loop(poll_state, provider).await });
    }
    if !config.push_providers.is_empty() {
        let watch_state = Arc::clone(&state);
        tokio::spawn(async move { push_watch_loop(watch_state).await });
    }
    let outbox_state = Arc::clone(&state);
    tokio::spawn(async move { outbox_loop(outbox_state).await });

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/snapshots", get(snapshots))
        .route("/api/v1/poll", post(force_poll))
        .route("/api/v1/test-email", post(test_email))
        .route("/api/v1/test-push", post(test_push))
        .route(
            "/api/v1/ingest/{provider}",
            post(ingest).layer(DefaultBodyLimit::max(MAX_INGEST_BODY_BYTES)),
        )
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
    validate_providers(&config)?;
    parse_timezone(&config.display_timezone)?;
    Ok(config)
}

fn validate_providers(config: &Config) -> Result<(), String> {
    if let Some(unknown) = config
        .enabled_providers
        .iter()
        .find(|provider| !PULL_PROVIDERS.contains(&provider.as_str()))
    {
        return Err(format!("unknown enabled provider: {unknown}"));
    }
    if let Some(invalid) = config
        .push_providers
        .iter()
        .find(|provider| !valid_identifier(provider) || PULL_PROVIDERS.contains(&provider.as_str()))
    {
        return Err(format!("invalid push provider: {invalid}"));
    }
    if config
        .provider_intervals
        .values()
        .any(|interval| *interval < 60)
    {
        return Err("provider_intervals must be at least 60 seconds".to_string());
    }
    Ok(())
}

fn parse_timezone(name: &str) -> Result<Tz, String> {
    name.parse::<Tz>()
        .map_err(|_| format!("unknown display_timezone: {name}"))
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.starts_with(|first: char| first.is_ascii_lowercase())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
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
            current_remaining REAL NOT NULL DEFAULT 100,
            event_kind TEXT NOT NULL DEFAULT 'quota_reset',
            reason TEXT NOT NULL DEFAULT 'legacy',
            detail TEXT,
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
        CREATE TABLE IF NOT EXISTS snapshot_observations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_key TEXT NOT NULL,
            window_type TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            reset_at INTEGER NOT NULL,
            observed_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS snapshot_observations_lookup
            ON snapshot_observations(provider_key, window_type, observed_at);
        CREATE TABLE IF NOT EXISTS reset_candidates (
            provider_key TEXT NOT NULL,
            window_type TEXT NOT NULL,
            provider TEXT NOT NULL,
            baseline_remaining REAL NOT NULL,
            candidate_remaining REAL NOT NULL,
            old_reset_at INTEGER NOT NULL,
            new_reset_at INTEGER NOT NULL,
            first_observed_at INTEGER NOT NULL,
            last_observed_at INTEGER NOT NULL,
            confirmations INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY(provider_key, window_type)
        );
        CREATE TABLE IF NOT EXISTS provider_health (
            provider_key TEXT PRIMARY KEY,
            consecutive_failures INTEGER NOT NULL,
            first_failed_at INTEGER NOT NULL,
            last_failed_at INTEGER NOT NULL,
            last_error TEXT NOT NULL,
            alert_event_id TEXT
        );
        CREATE TABLE IF NOT EXISTS bark_outbox (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id TEXT NOT NULL UNIQUE REFERENCES reset_events(event_id),
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL,
            last_error TEXT,
            sent_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS bark_outbox_pending
            ON bark_outbox(sent_at, next_attempt_at);
        ",
    )?;
    ensure_column(
        connection,
        "reset_events",
        "current_remaining",
        "REAL NOT NULL DEFAULT 100",
    )?;
    ensure_column(
        connection,
        "reset_events",
        "event_kind",
        "TEXT NOT NULL DEFAULT 'quota_reset'",
    )?;
    ensure_column(
        connection,
        "reset_events",
        "reason",
        "TEXT NOT NULL DEFAULT 'legacy'",
    )?;
    ensure_column(connection, "reset_events", "detail", "TEXT")?;
    Ok(())
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|existing| existing == column) {
        connection.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

async fn provider_loop(state: Arc<AppState>, provider: String) {
    loop {
        if let Err(error) = poll_provider(&state, &provider, false).await {
            error!(provider, %error, "quota poll failed");
        }
        tokio::time::sleep(Duration::from_secs(PROVIDER_TICK_SECS)).await;
    }
}

/// A forced poll skips our own failure backoff, but never a wait the provider asked for.
async fn poll_provider(state: &Arc<AppState>, provider: &str, force: bool) -> Result<(), String> {
    let lock = state
        .provider_locks
        .get(provider)
        .ok_or_else(|| format!("unknown provider: {provider}"))?;
    let _guard = lock.lock().await;
    let now = Utc::now().timestamp();
    {
        let runtime = state.runtime.read().await;
        let wait_until = |waits: &HashMap<String, i64>| waits.get(provider).copied().unwrap_or(0);
        if wait_until(&runtime.rate_limited_until) > now
            || (!force && wait_until(&runtime.next_poll_at) > now)
        {
            return Ok(());
        }
    }
    let result = fetch_provider(state, provider).await;
    handle_provider_result(state, provider, result).await?;
    state.runtime.write().await.last_poll_at = Some(Utc::now().timestamp());
    Ok(())
}

async fn fetch_provider(
    state: &Arc<AppState>,
    provider: &str,
) -> Result<Vec<UsageSnapshot>, ProviderError> {
    match provider {
        "claude" => state.quota.fetch_claude().await,
        "openai" | "grok" => {
            let _permit = state
                .cli_gate
                .acquire()
                .await
                .map_err(|error| ProviderError::Transport(error.to_string()))?;
            if provider == "openai" {
                state.quota.fetch_openai().await
            } else {
                state.quota.fetch_grok().await
            }
        }
        other => Err(ProviderError::InvalidResponse(format!(
            "no fetcher for provider {other}"
        ))),
    }
}

fn poll_interval(config: &Config, provider: &str) -> u64 {
    config
        .provider_intervals
        .get(provider)
        .copied()
        .unwrap_or(config.poll_interval_sec)
}

/// Doubles after each failure in a row up to an hour. A refused login keeps the
/// normal cadence only until its alert is due, then asks once an hour.
fn backoff_secs(error: &ProviderError, failures: u32, interval: u64) -> u64 {
    let doubled = interval
        .saturating_mul(1_u64 << failures.saturating_sub(1).min(16))
        .min(MAX_BACKOFF_SECS);
    match error {
        ProviderError::RateLimited {
            retry_after_secs: Some(seconds),
        } if *seconds > 0 => (*seconds).max(60),
        ProviderError::Unauthorized(_) | ProviderError::MissingCredentials(_) => {
            if i64::from(failures) < UNAUTHORIZED_ALERT_FAILURES {
                interval
            } else {
                MAX_BACKOFF_SECS
            }
        }
        _ => doubled,
    }
}

async fn handle_provider_result(
    state: &Arc<AppState>,
    provider_key: &str,
    result: Result<Vec<UsageSnapshot>, ProviderError>,
) -> Result<(), String> {
    let interval = poll_interval(&state.config, provider_key);
    match result {
        Ok(snapshots) => {
            let active_windows = snapshots
                .iter()
                .map(|snapshot| snapshot.window_type.clone())
                .collect::<HashSet<_>>();
            let plan_changed = record_plan_change(state, provider_key, &snapshots)?;
            for snapshot in snapshots {
                persist_snapshot(state, provider_key, &snapshot, !plan_changed)?;
            }
            prune_inactive_snapshots(state, provider_key, &active_windows)?;
            record_provider_recovery(state, provider_key)?;
            let mut runtime = state.runtime.write().await;
            runtime.provider_errors.remove(provider_key);
            runtime.rate_limited_until.remove(provider_key);
            runtime.failure_streak.remove(provider_key);
            runtime.next_poll_at.insert(
                provider_key.to_string(),
                Utc::now().timestamp() + interval as i64,
            );
        }
        Err(error) => {
            record_provider_failure(state, provider_key, &error)?;
            let mut runtime = state.runtime.write().await;
            let failures = {
                let streak = runtime
                    .failure_streak
                    .entry(provider_key.to_string())
                    .or_insert(0);
                *streak = streak.saturating_add(1);
                *streak
            };
            // The provider's Retry-After is a minimum, even when it exceeds our
            // own backoff cap. Saturate an unrepresentable deadline, never wrap it.
            let delay = i64::try_from(backoff_secs(&error, failures, interval)).unwrap_or(i64::MAX);
            let wait_until = Utc::now().timestamp().saturating_add(delay);
            runtime
                .next_poll_at
                .insert(provider_key.to_string(), wait_until);
            if matches!(error, ProviderError::RateLimited { .. }) {
                runtime
                    .rate_limited_until
                    .insert(provider_key.to_string(), wait_until);
            }
            runtime
                .provider_errors
                .insert(provider_key.to_string(), error.to_string());
            warn!(provider = provider_key, %error, "provider poll failed");
        }
    }
    Ok(())
}

/// Text in the provider string's parentheses, such as `prolite` or `pro, default_claude_ai`.
fn plan_label(provider: &str) -> Option<&str> {
    let start = provider.find('(')?;
    let end = provider.rfind(')')?;
    let label = provider.get(start + 1..end)?.trim();
    (!label.is_empty()).then_some(label)
}

/// A renewal or re-login can move an account to another plan, whose fresh windows
/// look exactly like a reset. Report the change itself and restart the baselines.
fn record_plan_change(
    state: &Arc<AppState>,
    provider_key: &str,
    snapshots: &[UsageSnapshot],
) -> Result<bool, String> {
    let Some(current) = snapshots.first().map(|snapshot| snapshot.provider.as_str()) else {
        return Ok(false);
    };
    let mut database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let transaction = database.transaction().map_err(|error| error.to_string())?;
    let previous: Option<String> = transaction
        .query_row(
            "SELECT json_extract(payload_json, '$.provider')
             FROM snapshots WHERE provider_key = ?1 LIMIT 1",
            params![provider_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let plans = previous
        .as_deref()
        .and_then(plan_label)
        .zip(plan_label(current));
    let Some((old_plan, new_plan)) = plans.filter(|(old, new)| old != new) else {
        return Ok(false);
    };
    let now = Utc::now().timestamp();
    let event_id = monitor_event_id(
        provider_key,
        &format!("plan_changed|{old_plan}|{new_plan}"),
        now,
    );
    let detail = format!("{old_plan} → {new_plan}");
    insert_event(
        state,
        &transaction,
        &event_id,
        provider_key,
        "monitor",
        0,
        0,
        0.0,
        0.0,
        "plan_changed",
        "provider_plan_changed",
        Some(&detail),
        now,
    )?;
    warn!(provider = provider_key, %detail, "provider plan changed");
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(true)
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
    detect_resets: bool,
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
    let candidate = load_reset_candidate(&transaction, provider_key, &snapshot.window_type)?;

    transaction
        .execute(
            "INSERT INTO snapshot_observations(
               provider_key, window_type, payload_json, reset_at, observed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                provider_key,
                snapshot.window_type,
                payload,
                snapshot.reset_at,
                now
            ],
        )
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
    transaction
        .execute(
            "DELETE FROM snapshot_observations WHERE observed_at < ?1",
            params![now - SNAPSHOT_HISTORY_RETENTION_SECS],
        )
        .map_err(|error| error.to_string())?;

    if !detect_resets {
        delete_reset_candidate(&transaction, provider_key, &snapshot.window_type)?;
    } else if let Some((old_reset_at, previous_remaining)) = previous {
        if let Some(reason) = immediate_reset_reason(
            old_reset_at,
            snapshot.reset_at,
            previous_remaining,
            snapshot.remaining_percent,
            now,
        ) {
            let baseline = candidate
                .as_ref()
                .map(|value| value.baseline_remaining)
                .unwrap_or(previous_remaining);
            let event_old_reset = candidate
                .as_ref()
                .map(|value| value.old_reset_at)
                .unwrap_or(old_reset_at);
            enqueue_quota_reset(
                state,
                &transaction,
                provider_key,
                snapshot,
                event_old_reset,
                baseline,
                now,
                reason,
            )?;
            delete_reset_candidate(&transaction, provider_key, &snapshot.window_type)?;
        } else if let Some(candidate) = candidate {
            if reset_candidate_confirmed(&candidate, snapshot.remaining_percent) {
                enqueue_quota_reset(
                    state,
                    &transaction,
                    provider_key,
                    snapshot,
                    candidate.old_reset_at,
                    candidate.baseline_remaining,
                    candidate.first_observed_at,
                    EARLY_RESET_REASON,
                )?;
            }
            delete_reset_candidate(&transaction, provider_key, &snapshot.window_type)?;
        } else if is_significant_recovery(previous_remaining, snapshot.remaining_percent) {
            transaction
                .execute(
                    "INSERT INTO reset_candidates(
                       provider_key, window_type, provider, baseline_remaining,
                       candidate_remaining, old_reset_at, new_reset_at,
                       first_observed_at, last_observed_at, confirmations
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 1)
                     ON CONFLICT(provider_key, window_type) DO UPDATE SET
                       provider = excluded.provider,
                       baseline_remaining = excluded.baseline_remaining,
                       candidate_remaining = excluded.candidate_remaining,
                       old_reset_at = excluded.old_reset_at,
                       new_reset_at = excluded.new_reset_at,
                       first_observed_at = excluded.first_observed_at,
                       last_observed_at = excluded.last_observed_at,
                       confirmations = 1",
                    params![
                        provider_key,
                        snapshot.window_type,
                        snapshot.provider,
                        previous_remaining,
                        snapshot.remaining_percent,
                        old_reset_at,
                        snapshot.reset_at,
                        now
                    ],
                )
                .map_err(|error| error.to_string())?;
            info!(
                provider = provider_key,
                window = snapshot.window_type,
                previous_remaining,
                current_remaining = snapshot.remaining_percent,
                "quota recovery candidate detected"
            );
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn immediate_reset_reason(
    old_reset_at: i64,
    new_reset_at: i64,
    previous_remaining: f64,
    current_remaining: f64,
    observed_at: i64,
) -> Option<&'static str> {
    let old_boundary_reached = old_reset_at > 0 && observed_at >= old_reset_at.saturating_sub(120);
    // A window that was never used has nothing to report when its boundary passes.
    if !old_boundary_reached || previous_remaining >= UNUSED_WINDOW_MIN_REMAINING {
        return None;
    }
    if new_reset_at > old_reset_at + 60 {
        return Some("next_reset_advanced");
    }
    if new_reset_at == 0 && current_remaining >= UNUSED_WINDOW_MIN_REMAINING {
        return Some("window_completed_idle");
    }
    None
}

fn is_significant_recovery(previous_remaining: f64, current_remaining: f64) -> bool {
    current_remaining - previous_remaining >= RESET_RECOVERY_MIN_POINTS
}

fn reset_candidate_confirmed(candidate: &ResetCandidate, current_remaining: f64) -> bool {
    candidate.candidate_remaining > candidate.baseline_remaining
        && current_remaining - candidate.baseline_remaining >= RESET_CONFIRMATION_MIN_POINTS
}

fn reset_event_id(
    provider: &str,
    window_type: &str,
    detected_at: i64,
    old_reset_at: i64,
) -> String {
    let digest =
        sha256(format!("v2|{provider}|{window_type}|{detected_at}|{old_reset_at}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn load_reset_candidate(
    transaction: &Transaction<'_>,
    provider_key: &str,
    window_type: &str,
) -> Result<Option<ResetCandidate>, String> {
    transaction
        .query_row(
            "SELECT baseline_remaining, candidate_remaining, old_reset_at,
                    first_observed_at
             FROM reset_candidates
             WHERE provider_key = ?1 AND window_type = ?2",
            params![provider_key, window_type],
            |row| {
                Ok(ResetCandidate {
                    baseline_remaining: row.get(0)?,
                    candidate_remaining: row.get(1)?,
                    old_reset_at: row.get(2)?,
                    first_observed_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn delete_reset_candidate(
    transaction: &Transaction<'_>,
    provider_key: &str,
    window_type: &str,
) -> Result<(), String> {
    transaction
        .execute(
            "DELETE FROM reset_candidates WHERE provider_key = ?1 AND window_type = ?2",
            params![provider_key, window_type],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enqueue_quota_reset(
    state: &AppState,
    transaction: &Transaction<'_>,
    provider_key: &str,
    snapshot: &UsageSnapshot,
    old_reset_at: i64,
    previous_remaining: f64,
    detected_at: i64,
    reason: &str,
) -> Result<(), String> {
    let event_id = reset_event_id(
        provider_key,
        &snapshot.window_type,
        detected_at,
        old_reset_at,
    );
    let inserted = insert_event(
        state,
        transaction,
        &event_id,
        &snapshot.provider,
        &snapshot.window_type,
        old_reset_at,
        snapshot.reset_at,
        previous_remaining,
        snapshot.remaining_percent,
        "quota_reset",
        reason,
        None,
        Utc::now().timestamp(),
    )?;
    if inserted {
        info!(
            provider = provider_key,
            window = snapshot.window_type,
            old_reset_at,
            new_reset_at = snapshot.reset_at,
            previous_remaining,
            current_remaining = snapshot.remaining_percent,
            reason,
            "confirmed quota reset"
        );
    }
    Ok(())
}

fn notification_channels(
    policy: &NotifyPolicy,
    event_kind: &str,
    window_type: &str,
    reason: &str,
    previous_remaining: f64,
) -> Channels {
    let scheduled_short_reset = event_kind == "quota_reset"
        && window_type.starts_with("hourly5")
        && reason != EARLY_RESET_REASON;
    if scheduled_short_reset {
        Channels {
            email: policy.short_window_email,
            bark: previous_remaining < policy.short_window_bark_below_remaining,
        }
    } else {
        Channels {
            email: true,
            bark: true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_event(
    state: &AppState,
    transaction: &Transaction<'_>,
    event_id: &str,
    provider: &str,
    window_type: &str,
    old_reset_at: i64,
    new_reset_at: i64,
    previous_remaining: f64,
    current_remaining: f64,
    event_kind: &str,
    reason: &str,
    detail: Option<&str>,
    created_at: i64,
) -> Result<bool, String> {
    let inserted = transaction
        .execute(
            "INSERT OR IGNORE INTO reset_events(
               event_id, provider, window_type, old_reset_at, new_reset_at,
               previous_remaining, current_remaining, event_kind, reason, detail, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                event_id,
                provider,
                window_type,
                old_reset_at,
                new_reset_at,
                previous_remaining,
                current_remaining,
                event_kind,
                reason,
                detail,
                created_at
            ],
        )
        .map_err(|error| error.to_string())?;
    if inserted > 0 {
        let channels = notification_channels(
            &state.config.notify,
            event_kind,
            window_type,
            reason,
            previous_remaining,
        );
        if channels.email {
            transaction
                .execute(
                    "INSERT INTO email_outbox(event_id, next_attempt_at) VALUES (?1, ?2)",
                    params![event_id, created_at],
                )
                .map_err(|error| error.to_string())?;
        }
        if channels.bark && state.bark_key.is_some() {
            transaction
                .execute(
                    "INSERT INTO bark_outbox(event_id, next_attempt_at) VALUES (?1, ?2)",
                    params![event_id, created_at],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(inserted > 0)
}

fn provider_failure_alert_threshold(error: &ProviderError) -> i64 {
    match error {
        ProviderError::Unauthorized(_) | ProviderError::MissingCredentials(_) => {
            UNAUTHORIZED_ALERT_FAILURES
        }
        ProviderError::RateLimited { .. } => 5,
        ProviderError::Transport(_) | ProviderError::InvalidResponse(_) => 4,
    }
}

fn record_provider_failure(
    state: &Arc<AppState>,
    provider_key: &str,
    error: &ProviderError,
) -> Result<(), String> {
    let now = Utc::now().timestamp();
    let mut database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let transaction = database.transaction().map_err(|error| error.to_string())?;
    let previous: Option<(i64, i64, Option<String>)> = transaction
        .query_row(
            "SELECT consecutive_failures, first_failed_at, alert_event_id
             FROM provider_health WHERE provider_key = ?1",
            params![provider_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let (consecutive_failures, first_failed_at, existing_alert) = match previous {
        Some((count, first_failed_at, alert)) => (count + 1, first_failed_at, alert),
        None => (1, now, None),
    };
    transaction
        .execute(
            "INSERT INTO provider_health(
               provider_key, consecutive_failures, first_failed_at,
               last_failed_at, last_error, alert_event_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(provider_key) DO UPDATE SET
               consecutive_failures = excluded.consecutive_failures,
               first_failed_at = excluded.first_failed_at,
               last_failed_at = excluded.last_failed_at,
               last_error = excluded.last_error,
               alert_event_id = excluded.alert_event_id",
            params![
                provider_key,
                consecutive_failures,
                first_failed_at,
                now,
                error.to_string(),
                existing_alert
            ],
        )
        .map_err(|error| error.to_string())?;
    if existing_alert.is_none() && consecutive_failures >= provider_failure_alert_threshold(error) {
        let event_id = monitor_event_id(provider_key, "provider_error", first_failed_at);
        let inserted = insert_event(
            state,
            &transaction,
            &event_id,
            provider_key,
            "monitor",
            0,
            0,
            0.0,
            0.0,
            "provider_error",
            "consecutive_poll_failures",
            Some(&error.to_string()),
            now,
        )?;
        if inserted {
            transaction
                .execute(
                    "UPDATE provider_health SET alert_event_id = ?1 WHERE provider_key = ?2",
                    params![event_id, provider_key],
                )
                .map_err(|error| error.to_string())?;
            warn!(
                provider = provider_key,
                consecutive_failures, "provider monitoring failure alert queued"
            );
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn record_provider_recovery(state: &Arc<AppState>, provider_key: &str) -> Result<(), String> {
    let now = Utc::now().timestamp();
    let mut database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let transaction = database.transaction().map_err(|error| error.to_string())?;
    let health: Option<(i64, i64, String, Option<String>)> = transaction
        .query_row(
            "SELECT consecutive_failures, first_failed_at, last_error, alert_event_id
             FROM provider_health WHERE provider_key = ?1",
            params![provider_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((consecutive_failures, first_failed_at, last_error, Some(alert_event_id))) = health
    {
        let event_id = monitor_event_id(provider_key, "provider_recovered", now);
        let detail = format!(
            "Recovered after {consecutive_failures} failures since {}. Last error: {last_error}. Alert event: {alert_event_id}",
            format_timestamp(first_failed_at, state.timezone)
        );
        insert_event(
            state,
            &transaction,
            &event_id,
            provider_key,
            "monitor",
            0,
            0,
            0.0,
            0.0,
            "provider_recovered",
            "poll_succeeded",
            Some(&detail),
            now,
        )?;
        info!(provider = provider_key, "provider monitoring recovered");
    }
    transaction
        .execute(
            "DELETE FROM provider_health WHERE provider_key = ?1",
            params![provider_key],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn monitor_event_id(provider: &str, kind: &str, timestamp: i64) -> String {
    let digest = sha256(format!("monitor|{provider}|{kind}|{timestamp}").as_bytes());
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
    if state.postmark_token.is_some() {
        for item in load_pending_outbox(state, "email_outbox")? {
            match send_event_email(state, &item).await {
                Ok(message_id) => mark_outbox_sent(state, item.id, &message_id)?,
                Err(error) => {
                    mark_outbox_failed(state, "email_outbox", item.id, item.attempts + 1, &error)?
                }
            }
        }
    }
    if let Some(key) = state.bark_key.as_deref() {
        for item in load_pending_outbox(state, "bark_outbox")? {
            if Utc::now().timestamp() - item.created_at > BARK_MAX_AGE_SECS {
                // A push about an old reset would read as a new one.
                mark_push_finished(state, item.id, Some("expired before delivery"))?;
                continue;
            }
            let (title, body) = push_message(state, &item);
            match send_bark(state, key, &title, &body).await {
                Ok(()) => mark_push_finished(state, item.id, None)?,
                Err(error) => {
                    mark_outbox_failed(state, "bark_outbox", item.id, item.attempts + 1, &error)?
                }
            }
        }
    }
    Ok(())
}

fn load_pending_outbox(state: &Arc<AppState>, table: &str) -> Result<Vec<OutboxItem>, String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    let mut statement = database
        .prepare(&format!(
            "SELECT o.id, e.event_id, e.provider, e.window_type, e.old_reset_at,
                    e.new_reset_at, e.previous_remaining, e.current_remaining,
                    e.event_kind, e.reason, e.detail, o.attempts, e.created_at
             FROM {table} o
             JOIN reset_events e ON e.event_id = o.event_id
             WHERE o.sent_at IS NULL AND o.next_attempt_at <= ?1
             ORDER BY o.id LIMIT 10"
        ))
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
                current_remaining: row.get(7)?,
                event_kind: row.get(8)?,
                reason: row.get(9)?,
                detail: row.get(10)?,
                attempts: row.get(11)?,
                created_at: row.get(12)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

async fn send_event_email(state: &Arc<AppState>, item: &OutboxItem) -> Result<String, String> {
    let token = state
        .postmark_token
        .as_deref()
        .ok_or_else(|| "Postmark token is not configured".to_string())?;
    match item.event_kind.as_str() {
        "provider_error" => send_provider_error_email(state, token, item).await,
        "provider_recovered" => send_provider_recovery_email(state, token, item).await,
        "plan_changed" => send_plan_changed_email(state, token, item).await,
        _ => send_reset_email(state, token, item).await,
    }
}

async fn send_reset_email(
    state: &Arc<AppState>,
    token: &str,
    item: &OutboxItem,
) -> Result<String, String> {
    let old_local = format_timestamp(item.old_reset_at, state.timezone);
    let new_local = format_next_reset(item.new_reset_at, state.timezone);
    let provider_name = provider_display_name(&item.provider);
    let window_name = window_label(&item.window_type);
    let reason_name = reset_reason_label(&item.reason);
    let subject = format!(
        "[Panel4AI] {provider_name} {window_name} 额度{}",
        reset_verb(&item.reason)
    );
    let text = format!(
        "{provider_name} 的 {window_name} 额度窗口已确认重置。\n\n上一个重置点：{old_local}\n下一个重置点：{new_local}\n重置前剩余：{:.1}%\n当前剩余：{:.1}%\n检测方式：{}\n事件 ID：{}\n",
        item.previous_remaining,
        item.current_remaining,
        reason_name,
        item.event_id
    );
    let provider_name_html = html_escape(provider_name);
    let window_type_html = html_escape(&window_name);
    let reason_html = html_escape(reason_name);
    let event_id_html = html_escape(&item.event_id);
    let html = format!(
        "<h2>{provider_name_html} 额度已重置</h2><p>窗口：<strong>{window_type_html}</strong></p><ul><li>上一个重置点：{old_local}</li><li>下一个重置点：{new_local}</li><li>重置前剩余：{:.1}%</li><li>当前剩余：{:.1}%</li><li>检测方式：{reason_html}</li></ul><p style=\"color:#777;font-size:12px\">事件 ID：{event_id_html}</p>",
        item.previous_remaining,
        item.current_remaining,
    );
    send_postmark(
        state,
        token,
        &subject,
        &text,
        &html,
        &item.event_id,
        "quota-reset",
    )
    .await
}

async fn send_provider_error_email(
    state: &Arc<AppState>,
    token: &str,
    item: &OutboxItem,
) -> Result<String, String> {
    let provider_name = provider_display_name(&item.provider);
    let subject = format!("[Panel4AI] {provider_name} 额度监控异常");
    let detail = item.detail.as_deref().unwrap_or("未知错误");
    let provider_name_html = html_escape(provider_name);
    let text = format!(
        "{provider_name} 额度监控连续失败，当前无法可靠检测重置。\n\n错误：{detail}\n事件 ID：{}\n",
        item.event_id
    );
    let html = format!(
        "<h2>{provider_name_html} 额度监控异常</h2><p>连续查询失败，当前无法可靠检测重置。</p><p><strong>错误：</strong>{}</p><p style=\"color:#777;font-size:12px\">事件 ID：{}</p>",
        html_escape(detail),
        html_escape(&item.event_id)
    );
    send_postmark(
        state,
        token,
        &subject,
        &text,
        &html,
        &item.event_id,
        "monitor-health",
    )
    .await
}

async fn send_provider_recovery_email(
    state: &Arc<AppState>,
    token: &str,
    item: &OutboxItem,
) -> Result<String, String> {
    let provider_name = provider_display_name(&item.provider);
    let subject = format!("[Panel4AI] {provider_name} 额度监控已恢复");
    let detail = item.detail.as_deref().unwrap_or("查询已恢复正常");
    let provider_name_html = html_escape(provider_name);
    let text = format!(
        "{provider_name} 额度监控已恢复。\n\n{detail}\n事件 ID：{}\n",
        item.event_id
    );
    let html = format!(
        "<h2>{provider_name_html} 额度监控已恢复</h2><p>{}</p><p style=\"color:#777;font-size:12px\">事件 ID：{}</p>",
        html_escape(detail),
        html_escape(&item.event_id)
    );
    send_postmark(
        state,
        token,
        &subject,
        &text,
        &html,
        &item.event_id,
        "monitor-health",
    )
    .await
}

async fn send_plan_changed_email(
    state: &Arc<AppState>,
    token: &str,
    item: &OutboxItem,
) -> Result<String, String> {
    let provider_name = provider_display_name(&item.provider);
    let detail = item.detail.as_deref().unwrap_or("未知");
    let subject = format!("[Panel4AI] {provider_name} 账号套餐变化：{detail}");
    let advice =
        "这不是额度重置。如果不是你主动改的，可能是续费后登录失效，需要在服务器上重新登录。";
    let text = format!(
        "{provider_name} 上报的套餐变了：{detail}。\n\n{advice}\n事件 ID：{}\n",
        item.event_id
    );
    let html = format!(
        "<h2>{} 账号套餐变化</h2><p><strong>{}</strong></p><p>{advice}</p><p style=\"color:#777;font-size:12px\">事件 ID：{}</p>",
        html_escape(provider_name),
        html_escape(detail),
        html_escape(&item.event_id)
    );
    send_postmark(
        state,
        token,
        &subject,
        &text,
        &html,
        &item.event_id,
        "monitor-health",
    )
    .await
}

fn provider_display_name(provider: &str) -> &str {
    if provider.starts_with("openai") {
        "Codex"
    } else if provider.starts_with("claude") {
        "Claude"
    } else if provider.starts_with("grok") {
        "Grok"
    } else if provider.starts_with("antigravity") {
        "Antigravity"
    } else {
        provider
    }
}

/// `weekly` -> `每周`, `weekly_gemini` -> `Gemini 每周`; anything else stays as reported.
fn window_label(window_type: &str) -> String {
    if window_type == "code_review_weekly" {
        return "Code Review 每周".to_string();
    }
    let bases = [
        ("hourly5", "5 小时"),
        ("weekly", "每周"),
        ("daily", "每日"),
        ("monthly", "每月"),
    ];
    for (prefix, base) in bases {
        let Some(rest) = window_type.strip_prefix(prefix) else {
            continue;
        };
        let scope = rest.trim_start_matches(['_', '-']);
        if scope.is_empty() {
            return base.to_string();
        }
        return format!("{} {base}", scope_label(scope));
    }
    window_type.to_string()
}

fn scope_label(scope: &str) -> String {
    if scope == "claudegpt" {
        return "Claude+GPT".to_string();
    }
    let mut chars = scope.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn reset_verb(reason: &str) -> &'static str {
    if reason == EARLY_RESET_REASON {
        "提前重置"
    } else {
        "已重置"
    }
}

fn reset_reason_label(reason: &str) -> &str {
    match reason {
        "next_reset_advanced" => "到达重置时间，下一个重置点已前移",
        "window_completed_idle" => "窗口到期，当前空闲",
        EARLY_RESET_REASON => "早于预定时间，用量回升已连续确认",
        other => other,
    }
}

fn push_message(state: &AppState, item: &OutboxItem) -> (String, String) {
    let provider_name = provider_display_name(&item.provider);
    let detail = item.detail.as_deref().unwrap_or("");
    match item.event_kind.as_str() {
        "provider_error" => (
            format!("{provider_name} 额度监控异常"),
            truncate_chars(detail, 200),
        ),
        "provider_recovered" => (
            format!("{provider_name} 额度监控已恢复"),
            "查询已恢复正常".to_string(),
        ),
        "plan_changed" => (
            format!("{provider_name} 账号套餐变化"),
            format!("{detail}。这不是额度重置，可能需要重新登录。"),
        ),
        _ => {
            let next = if item.new_reset_at > 0 {
                format!(
                    "，下次重置 {}",
                    format_short_time(item.new_reset_at, state.timezone)
                )
            } else {
                String::new()
            };
            (
                format!(
                    "{provider_name} {} 额度{}",
                    window_label(&item.window_type),
                    reset_verb(&item.reason)
                ),
                format!(
                    "重置前剩余 {:.0}%，现在 {:.0}%{next}",
                    item.previous_remaining, item.current_remaining
                ),
            )
        }
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(limit).collect::<String>())
    }
}

async fn send_bark(state: &AppState, key: &str, title: &str, body: &str) -> Result<(), String> {
    let url = format!("{}/push", state.config.bark_server.trim_end_matches('/'));
    let response = state
        .http_client
        .post(url)
        // The key goes in the JSON body so it stays out of URLs and error messages.
        .json(&serde_json::json!({
            "device_key": key,
            "title": title,
            "body": body,
            "group": "panel4ai"
        }))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Bark HTTP {status}"));
    }
    Ok(())
}

fn mark_push_finished(state: &Arc<AppState>, id: i64, note: Option<&str>) -> Result<(), String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    database
        .execute(
            "UPDATE bark_outbox SET sent_at = ?1, last_error = ?2 WHERE id = ?3",
            params![Utc::now().timestamp(), note, id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn send_postmark(
    state: &Arc<AppState>,
    token: &str,
    subject: &str,
    text: &str,
    html: &str,
    event_id: &str,
    tag: &str,
) -> Result<String, String> {
    let response = state
        .http_client
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
            "Tag": tag,
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
    table: &str,
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
            &format!(
                "UPDATE {table} SET attempts = ?1, next_attempt_at = ?2,
                 last_error = ?3 WHERE id = ?4"
            ),
            params![attempts, next_attempt, message, id],
        )
        .map_err(|error| error.to_string())?;
    warn!(
        outbox_id = id,
        attempts,
        error = message,
        table,
        "notification send failed"
    );
    Ok(())
}

fn format_timestamp(epoch: i64, timezone: Tz) -> String {
    let Some(utc) = Utc.timestamp_opt(epoch, 0).single() else {
        return "未知".to_string();
    };
    format!(
        "{}（UTC {}）",
        utc.with_timezone(&timezone).format("%Y-%m-%d %H:%M:%S %Z"),
        utc.format("%Y-%m-%d %H:%M:%S")
    )
}

fn format_short_time(epoch: i64, timezone: Tz) -> String {
    match Utc.timestamp_opt(epoch, 0).single() {
        Some(utc) => utc
            .with_timezone(&timezone)
            .format("%m-%d %H:%M")
            .to_string(),
        None => "未知".to_string(),
    }
}

fn format_next_reset(epoch: i64, timezone: Tz) -> String {
    if epoch <= 0 {
        "未安排（当前额度窗口空闲）".to_string()
    } else {
        format_timestamp(epoch, timezone)
    }
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let runtime = state.runtime.read().await;
    let pending = |table: &str| {
        state
            .database
            .lock()
            .ok()
            .and_then(|database| {
                database
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE sent_at IS NULL"),
                        [],
                        |row| row.get(0),
                    )
                    .ok()
            })
            .unwrap_or(-1)
    };
    let pending_emails = pending("email_outbox");
    let pending_pushes = pending("bark_outbox");
    let stale = stale_push_providers(&state).unwrap_or_default();
    let mut providers = HashMap::new();
    for provider in state
        .config
        .enabled_providers
        .iter()
        .chain(&state.config.push_providers)
    {
        let status = if runtime.provider_errors.contains_key(provider) {
            "error"
        } else if stale.contains_key(provider) {
            "stale"
        } else {
            "ok"
        };
        providers.insert(provider.clone(), status);
    }
    Json(HealthResponse {
        status: "ok",
        server_time: Utc::now().timestamp(),
        started_at: runtime.started_at,
        last_poll_at: runtime.last_poll_at,
        postmark_configured: state.postmark_token.is_some(),
        bark_configured: state.bark_key.is_some(),
        pending_emails,
        pending_pushes,
        providers,
    })
}

async fn snapshots(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SnapshotEnvelope>, StatusCode> {
    authorize(&state, &headers)?;
    let snapshots = load_snapshots(&state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let stale_providers =
        stale_push_providers(&state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let runtime = state.runtime.read().await;
    Ok(Json(SnapshotEnvelope {
        server_time: Utc::now().timestamp(),
        last_poll_at: runtime.last_poll_at,
        snapshots,
        provider_errors: runtime.provider_errors.clone(),
        stale_providers,
    }))
}

async fn force_poll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ActionResponse>, StatusCode> {
    authorize(&state, &headers)?;
    for provider in &state.config.enabled_providers {
        poll_provider(&state, provider, true)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
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
        "test",
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

async fn test_push(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ActionResponse>, (StatusCode, Json<ActionResponse>)> {
    let refuse =
        |status: StatusCode, message: String| (status, Json(ActionResponse { ok: false, message }));
    authorize(&state, &headers).map_err(|status| refuse(status, "unauthorized".to_string()))?;
    let key = state.bark_key.as_deref().ok_or_else(|| {
        refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "Bark key is not configured".to_string(),
        )
    })?;
    send_bark(
        &state,
        key,
        "Panel4AI 测试推送",
        "服务器到手机的通道工作正常",
    )
    .await
    .map_err(|message| refuse(StatusCode::BAD_GATEWAY, message))?;
    Ok(Json(ActionResponse {
        ok: true,
        message: "test push accepted".to_string(),
    }))
}

/// Receives quota from a machine that keeps its own login, for providers whose
/// credentials should not live on this server. A failed poll is reported the same
/// way, so an expired login there still raises the normal monitoring alert.
async fn ingest(
    State(state): State<Arc<AppState>>,
    RoutePath(provider): RoutePath<String>,
    headers: HeaderMap,
    Json(request): Json<IngestRequest>,
) -> Result<Json<ActionResponse>, (StatusCode, Json<ActionResponse>)> {
    let refuse =
        |status: StatusCode, message: String| (status, Json(ActionResponse { ok: false, message }));
    authorize_ingest(&state, &headers)
        .map_err(|status| refuse(status, "unauthorized".to_string()))?;
    if !state.config.push_providers.contains(&provider) {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            "not a push provider".to_string(),
        ));
    }
    let result = match request.error {
        Some(error) => Err(ingest_error(&error)),
        None => Ok(ingest_snapshots(
            &provider,
            &request.windows,
            state.config.alert_threshold_percent,
        )
        .map_err(|message| refuse(StatusCode::UNPROCESSABLE_ENTITY, message))?),
    };
    let lock = state
        .provider_locks
        .get(&provider)
        .ok_or_else(|| refuse(StatusCode::NOT_FOUND, "unknown provider".to_string()))?;
    let _guard = lock.lock().await;
    handle_provider_result(&state, &provider, result)
        .await
        .map_err(|message| refuse(StatusCode::INTERNAL_SERVER_ERROR, message))?;
    Ok(Json(ActionResponse {
        ok: true,
        message: "accepted".to_string(),
    }))
}

fn ingest_error(error: &IngestError) -> ProviderError {
    let message = truncate_chars(&error.message, 300);
    match error.kind.as_str() {
        "unauthorized" => ProviderError::Unauthorized(message),
        "rate_limited" => ProviderError::RateLimited {
            retry_after_secs: None,
        },
        "invalid" => ProviderError::InvalidResponse(message),
        _ => ProviderError::Transport(message),
    }
}

fn ingest_snapshots(
    provider: &str,
    windows: &[IngestWindow],
    alert_threshold: f64,
) -> Result<Vec<UsageSnapshot>, String> {
    if windows.is_empty() || windows.len() > MAX_INGEST_WINDOWS {
        return Err(format!(
            "expected 1 to {MAX_INGEST_WINDOWS} windows, got {}",
            windows.len()
        ));
    }
    let source = format!("{provider}-push");
    windows
        .iter()
        .map(|window| {
            if !valid_identifier(&window.window_type) {
                return Err(format!("invalid window type: {}", window.window_type));
            }
            if !(0.0..=100.0).contains(&window.used_percent) {
                return Err(format!("invalid usage for {}", window.window_type));
            }
            if window.reset_at < 0 || window.window_seconds <= 0 {
                return Err(format!("invalid period for {}", window.window_type));
            }
            Ok(build_snapshot(
                &source,
                &window.window_type,
                window.used_percent,
                window.reset_at,
                window.window_seconds,
                alert_threshold,
                None,
            ))
        })
        .collect()
}

fn push_last_seen(state: &AppState, provider: &str) -> Result<Option<i64>, String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "database mutex poisoned")?;
    database
        .query_row(
            "SELECT MAX(observed_at) FROM snapshots WHERE provider_key = ?1",
            params![provider],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn stale_push_providers(state: &AppState) -> Result<HashMap<String, i64>, String> {
    let now = Utc::now().timestamp();
    let mut stale = HashMap::new();
    for provider in &state.config.push_providers {
        match push_last_seen(state, provider)? {
            Some(last) if now - last <= state.config.push_stale_after_sec => {}
            last => {
                stale.insert(provider.clone(), last.unwrap_or(0));
            }
        }
    }
    Ok(stale)
}

/// A pusher that has gone quiet cannot report its own failure. A short gap is just
/// a sleeping Mac; after a day it is worth one alert, cleared by the next push.
async fn push_watch_loop(state: Arc<AppState>) {
    loop {
        if let Err(error) = check_push_silence(&state) {
            error!(%error, "push watch failed");
        }
        tokio::time::sleep(Duration::from_secs(PUSH_WATCH_INTERVAL_SECS)).await;
    }
}

fn check_push_silence(state: &Arc<AppState>) -> Result<(), String> {
    let now = Utc::now().timestamp();
    for provider in &state.config.push_providers {
        let Some(last) = push_last_seen(state, provider)? else {
            continue;
        };
        let silent_for = now - last;
        if silent_for > state.config.push_stale_alert_after_sec {
            let error = ProviderError::Transport(format!(
                "已有 {} 小时没有收到推送，推送端可能离线或采集脚本已停止",
                silent_for / 3600
            ));
            record_provider_failure(state, provider, &error)?;
        }
    }
    Ok(())
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    authorize_bearer(headers, &state.api_token_hash)
}

fn authorize_ingest(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let expected = state
        .ingest_token_hash
        .as_ref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    authorize_bearer(headers, expected)
}

fn authorize_bearer(headers: &HeaderMap, expected: &[u8; 32]) -> Result<(), StatusCode> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let candidate = sha256(header.as_bytes());
    if constant_time_eq(&candidate, expected) {
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
        let first = reset_event_id("openai", "hourly5", 1_700_000_000, 1_700_100_000);
        let second = reset_event_id("openai", "hourly5", 1_700_000_000, 1_700_100_000);
        let different = reset_event_id("openai", "weekly", 1_700_000_000, 1_700_100_000);
        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn confirms_reset_when_next_boundary_advances_after_old_boundary() {
        assert_eq!(
            immediate_reset_reason(1_000, 2_000, 20.0, 99.0, 1_001),
            Some("next_reset_advanced")
        );
    }

    #[test]
    fn confirms_claude_reset_when_completed_window_becomes_idle() {
        assert_eq!(
            immediate_reset_reason(1_000, 0, 12.5, 100.0, 1_001),
            Some("window_completed_idle")
        );
    }

    #[test]
    fn unused_window_passing_its_boundary_is_not_a_reset() {
        assert_eq!(
            immediate_reset_reason(1_000, 2_000, 100.0, 100.0, 1_001),
            None
        );
        assert_eq!(
            immediate_reset_reason(1_000, 2_000, 99.8, 100.0, 1_001),
            None
        );
        assert_eq!(immediate_reset_reason(1_000, 0, 100.0, 100.0, 1_001), None);
    }

    #[test]
    fn sliding_idle_window_is_not_a_reset() {
        // An unused Antigravity or Codex window keeps pushing its reset time ahead.
        assert_eq!(
            immediate_reset_reason(9_000, 9_300, 100.0, 100.0, 1_000),
            None
        );
    }

    #[test]
    fn early_reset_uses_usage_confirmation_instead_of_time_gate() {
        assert_eq!(
            immediate_reset_reason(2_000, 3_000, 12.5, 100.0, 1_000),
            None
        );
        assert!(is_significant_recovery(12.5, 100.0));
        let candidate = ResetCandidate {
            baseline_remaining: 12.5,
            candidate_remaining: 100.0,
            old_reset_at: 2_000,
            first_observed_at: 1_000,
        };
        assert!(reset_candidate_confirmed(&candidate, 98.0));
    }

    #[test]
    fn ignores_small_or_reverted_usage_changes() {
        assert!(!is_significant_recovery(80.0, 82.0));
        // Real cases that used to send a reset email.
        assert!(!is_significant_recovery(94.0, 97.0));
        assert!(!is_significant_recovery(97.0, 100.0));
        assert!(is_significant_recovery(89.0, 100.0));
        let candidate = ResetCandidate {
            baseline_remaining: 40.0,
            candidate_remaining: 70.0,
            old_reset_at: 2_000,
            first_observed_at: 1_000,
        };
        assert!(!reset_candidate_confirmed(&candidate, 40.5));
    }

    #[test]
    fn database_initialization_is_idempotent_and_adds_event_metadata() {
        let database = Connection::open_in_memory().expect("in-memory database");
        initialize_database(&database).expect("first initialization");
        initialize_database(&database).expect("second initialization");
        let mut statement = database
            .prepare("PRAGMA table_info(reset_events)")
            .expect("table info");
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column query")
            .collect::<Result<Vec<_>, _>>()
            .expect("columns");
        let bark_table: String = database
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'bark_outbox'",
                [],
                |row| row.get(0),
            )
            .expect("bark outbox table");
        assert_eq!(bark_table, "bark_outbox");
        for expected in ["current_remaining", "event_kind", "reason", "detail"] {
            assert!(columns.iter().any(|column| column == expected));
        }
        let history_table: String = database
            .query_row(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name = 'snapshot_observations'",
                [],
                |row| row.get(0),
            )
            .expect("history table");
        assert_eq!(history_table, "snapshot_observations");
    }

    #[test]
    fn repeated_auth_failures_queue_one_alert_and_recovery() {
        let state = test_state(None);
        let failure = ProviderError::Unauthorized("expired login".to_string());
        for _ in 0..3 {
            record_provider_failure(&state, "openai", &failure).expect("record failure");
        }
        {
            let database = state.database.lock().expect("database");
            let alerts: i64 = database
                .query_row(
                    "SELECT COUNT(*) FROM reset_events WHERE event_kind = 'provider_error'",
                    [],
                    |row| row.get(0),
                )
                .expect("alert count");
            let pending: i64 = database
                .query_row("SELECT COUNT(*) FROM email_outbox", [], |row| row.get(0))
                .expect("outbox count");
            assert_eq!(alerts, 1);
            assert_eq!(pending, 1);
        }
        record_provider_failure(&state, "openai", &failure).expect("record extra failure");
        record_provider_recovery(&state, "openai").expect("record recovery");
        let database = state.database.lock().expect("database");
        let events = database
            .prepare("SELECT event_kind FROM reset_events ORDER BY created_at, event_kind")
            .expect("events statement")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("events query")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        assert_eq!(
            events,
            vec![
                "provider_error".to_string(),
                "provider_recovered".to_string()
            ]
        );
        let health_rows: i64 = database
            .query_row("SELECT COUNT(*) FROM provider_health", [], |row| row.get(0))
            .expect("health count");
        assert_eq!(health_rows, 0);
    }

    fn test_state(bark_key: Option<&str>) -> Arc<AppState> {
        let database = Connection::open_in_memory().expect("in-memory database");
        initialize_database(&database).expect("database initialization");
        let quota = QuotaClient::new(
            ProviderPaths {
                codex_auth_path: "/tmp/panel4ai-test/codex/auth.json".into(),
                codex_binary_path: None,
                claude_auth_path: "/tmp/panel4ai-test/claude/auth.json".into(),
                grok_binary_path: None,
                grok_home: None,
            },
            20.0,
        )
        .expect("quota client");
        let config = Config {
            push_providers: vec!["antigravity".to_string()],
            ..Config::default()
        };
        let provider_locks = config
            .enabled_providers
            .iter()
            .chain(&config.push_providers)
            .map(|provider| (provider.clone(), AsyncMutex::new(())))
            .collect();
        Arc::new(AppState {
            timezone: parse_timezone(&config.display_timezone).expect("timezone"),
            config,
            quota,
            database: Mutex::new(database),
            http_client: reqwest::Client::new(),
            postmark_token: None,
            bark_key: bark_key.map(ToString::to_string),
            api_token_hash: sha256(b"test"),
            ingest_token_hash: Some(sha256(b"ingest")),
            runtime: RwLock::new(RuntimeStatus::default()),
            provider_locks,
            cli_gate: Semaphore::new(1),
        })
    }

    fn weekly(provider: &str, used: f64, reset_at: i64) -> UsageSnapshot {
        build_snapshot(provider, "weekly", used, reset_at, 604_800, 20.0, None)
    }

    fn count(state: &AppState, sql: &str) -> i64 {
        let database = state.database.lock().expect("database");
        database
            .query_row(sql, [], |row| row.get(0))
            .expect("count query")
    }

    #[tokio::test]
    async fn plan_change_is_reported_instead_of_a_reset() {
        // 2026-10-14: a renewal moved Codex from prolite to free, and the fresh
        // free window produced a "7% -> 100% reset" email.
        let state = test_state(None);
        let future = Utc::now().timestamp() + 86_400;
        let before = weekly("openai-app-server (prolite)", 93.0, future);
        handle_provider_result(&state, "openai", Ok(vec![before]))
            .await
            .expect("first poll");
        let after = weekly("openai-app-server (free)", 0.0, future + 2_592_000);
        for _ in 0..2 {
            handle_provider_result(&state, "openai", Ok(vec![after.clone()]))
                .await
                .expect("later poll");
        }
        assert_eq!(
            count(
                &state,
                "SELECT COUNT(*) FROM reset_events WHERE event_kind = 'quota_reset'"
            ),
            0
        );
        assert_eq!(
            count(
                &state,
                "SELECT COUNT(*) FROM reset_events WHERE event_kind = 'plan_changed'"
            ),
            1
        );
        assert_eq!(count(&state, "SELECT COUNT(*) FROM reset_candidates"), 0);
    }

    #[tokio::test]
    async fn early_reset_needs_two_polls_and_emails() {
        let state = test_state(Some("key"));
        let future = Utc::now().timestamp() + 86_400;
        let plan = "openai-app-server (prolite)";
        for used in [59.0, 0.0, 0.0] {
            handle_provider_result(&state, "openai", Ok(vec![weekly(plan, used, future)]))
                .await
                .expect("poll");
        }
        assert_eq!(count(&state, "SELECT COUNT(*) FROM reset_events"), 1);
        assert_eq!(
            count(
                &state,
                "SELECT COUNT(*) FROM reset_events WHERE reason = 'usage_recovery_confirmed'"
            ),
            1
        );
        assert_eq!(count(&state, "SELECT COUNT(*) FROM email_outbox"), 1);
        assert_eq!(count(&state, "SELECT COUNT(*) FROM bark_outbox"), 1);
    }

    #[tokio::test]
    async fn scheduled_short_reset_pushes_only_when_it_mattered() {
        let state = test_state(Some("key"));
        let past = Utc::now().timestamp() - 10;
        let next = past + 18_000;
        let window = |used, reset_at| {
            build_snapshot(
                "claude-oauth (max)",
                "hourly5",
                used,
                reset_at,
                18_000,
                20.0,
                None,
            )
        };
        handle_provider_result(&state, "claude", Ok(vec![window(30.0, past)]))
            .await
            .expect("light use");
        handle_provider_result(&state, "claude", Ok(vec![window(0.0, next)]))
            .await
            .expect("after reset");
        assert_eq!(count(&state, "SELECT COUNT(*) FROM reset_events"), 1);
        assert_eq!(count(&state, "SELECT COUNT(*) FROM email_outbox"), 0);
        assert_eq!(count(&state, "SELECT COUNT(*) FROM bark_outbox"), 0);

        let state = test_state(Some("key"));
        handle_provider_result(&state, "claude", Ok(vec![window(80.0, past)]))
            .await
            .expect("heavy use");
        handle_provider_result(&state, "claude", Ok(vec![window(0.0, next)]))
            .await
            .expect("after reset");
        assert_eq!(count(&state, "SELECT COUNT(*) FROM email_outbox"), 0);
        assert_eq!(count(&state, "SELECT COUNT(*) FROM bark_outbox"), 1);
    }

    #[test]
    fn notification_table_matches_the_policy() {
        let policy = NotifyPolicy::default();
        let channels = |kind, window, reason, remaining| {
            notification_channels(&policy, kind, window, reason, remaining)
        };
        let both = Channels {
            email: true,
            bark: true,
        };
        assert_eq!(
            channels("quota_reset", "weekly", "next_reset_advanced", 90.0),
            both
        );
        assert_eq!(
            channels("quota_reset", "weekly_gemini", "next_reset_advanced", 90.0),
            both
        );
        assert_eq!(
            channels("quota_reset", "hourly5", EARLY_RESET_REASON, 90.0),
            both
        );
        assert_eq!(
            channels("quota_reset", "hourly5_gemini", "next_reset_advanced", 20.0),
            Channels {
                email: false,
                bark: true,
            }
        );
        assert_eq!(
            channels("quota_reset", "hourly5", "window_completed_idle", 80.0),
            Channels {
                email: false,
                bark: false,
            }
        );
        assert_eq!(channels("plan_changed", "monitor", "x", 0.0), both);
        assert_eq!(channels("provider_error", "monitor", "x", 0.0), both);
    }

    #[test]
    fn backoff_doubles_and_respects_the_provider() {
        let transport = ProviderError::Transport("down".to_string());
        let delays: Vec<u64> = (1..=6).map(|n| backoff_secs(&transport, n, 300)).collect();
        assert_eq!(delays, vec![300, 600, 1200, 2400, 3600, 3600]);
        let refused = ProviderError::Unauthorized("expired".to_string());
        assert_eq!(backoff_secs(&refused, 1, 300), 300);
        assert_eq!(backoff_secs(&refused, 2, 300), 300);
        assert_eq!(backoff_secs(&refused, 3, 300), 3600);
        let limited = |seconds| ProviderError::RateLimited {
            retry_after_secs: seconds,
        };
        assert_eq!(backoff_secs(&limited(Some(90)), 1, 300), 90);
        assert_eq!(backoff_secs(&limited(Some(99_999)), 1, 300), 99_999);
        assert_eq!(backoff_secs(&limited(None), 2, 300), 600);
    }

    #[tokio::test]
    async fn failure_backoff_only_blocks_scheduled_polls_when_not_rate_limited() {
        let state = test_state(None);
        let failure = Err(ProviderError::Transport("down".to_string()));
        handle_provider_result(&state, "openai", failure)
            .await
            .expect("record failure");
        let runtime = state.runtime.read().await;
        assert!(runtime.next_poll_at.get("openai").copied().unwrap_or(0) > Utc::now().timestamp());
        assert!(!runtime.rate_limited_until.contains_key("openai"));
        assert_eq!(runtime.failure_streak.get("openai"), Some(&1));
    }

    #[tokio::test]
    async fn forced_poll_respects_long_and_unrepresentable_retry_after() {
        for seconds in [7200, u64::MAX] {
            let state = test_state(None);
            let before = Utc::now().timestamp();
            handle_provider_result(
                &state,
                "openai",
                Err(ProviderError::RateLimited {
                    retry_after_secs: Some(seconds),
                }),
            )
            .await
            .expect("record rate limit");
            let deadline = state.runtime.read().await.rate_limited_until["openai"];
            let expected = before.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX));
            assert!(deadline >= expected);
            poll_provider(&state, "openai", true)
                .await
                .expect("skip forced poll until Retry-After");
            let runtime = state.runtime.read().await;
            assert_eq!(runtime.rate_limited_until["openai"], deadline);
            assert_eq!(runtime.failure_streak["openai"], 1);
        }
    }

    #[test]
    fn window_labels_read_naturally() {
        assert_eq!(window_label("weekly"), "每周");
        assert_eq!(window_label("hourly5"), "5 小时");
        assert_eq!(window_label("weekly_gemini"), "Gemini 每周");
        assert_eq!(window_label("hourly5_claudegpt"), "Claude+GPT 5 小时");
        assert_eq!(window_label("weekly_sonnet"), "Sonnet 每周");
        assert_eq!(window_label("code_review_weekly"), "Code Review 每周");
        assert_eq!(window_label("seven_day_other"), "seven_day_other");
    }

    #[test]
    fn plan_label_reads_the_parentheses() {
        assert_eq!(plan_label("openai-app-server (prolite)"), Some("prolite"));
        assert_eq!(
            plan_label("claude-oauth (pro, default_claude_ai)"),
            Some("pro, default_claude_ai")
        );
        assert_eq!(plan_label("grok-cli"), None);
    }

    #[test]
    fn ingest_validates_pushed_windows() {
        let window = |window_type: &str, used_percent, reset_at| IngestWindow {
            window_type: window_type.to_string(),
            used_percent,
            reset_at,
            window_seconds: 604_800,
        };
        let snapshots = ingest_snapshots(
            "antigravity",
            &[window("weekly_argon", 74.0, 1_800_000_000)],
            20.0,
        )
        .expect("valid window");
        assert_eq!(snapshots[0].provider, "antigravity-push");
        assert_eq!(snapshots[0].remaining_percent, 26.0);
        assert!(ingest_snapshots("antigravity", &[], 20.0).is_err());
        assert!(ingest_snapshots("antigravity", &[window("Bad Name", 1.0, 1)], 20.0).is_err());
        assert!(ingest_snapshots("antigravity", &[window("weekly", 101.0, 1)], 20.0).is_err());
        assert!(ingest_snapshots("antigravity", &[window("weekly", f64::NAN, 1)], 20.0).is_err());
        assert!(ingest_snapshots("antigravity", &[window("weekly", 1.0, -1)], 20.0).is_err());
    }

    #[test]
    fn ingest_authorization_uses_its_own_token() {
        let state = test_state(None);
        let headers = |token: &str| {
            let mut map = HeaderMap::new();
            map.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().expect("header"),
            );
            map
        };
        assert!(authorize_ingest(&state, &headers("ingest")).is_ok());
        assert!(authorize_ingest(&state, &headers("test")).is_err());
        assert!(authorize(&state, &headers("ingest")).is_err());
        assert!(authorize(&state, &headers("test")).is_ok());
    }

    #[tokio::test]
    async fn stale_push_is_separate_from_provider_errors() {
        let state = test_state(None);
        assert_eq!(
            stale_push_providers(&state)
                .expect("stale")
                .get("antigravity"),
            Some(&0)
        );
        let window = build_snapshot(
            "antigravity-push",
            "weekly_argon",
            74.0,
            Utc::now().timestamp() + 86_400,
            604_800,
            20.0,
            None,
        );
        handle_provider_result(&state, "antigravity", Ok(vec![window]))
            .await
            .expect("push");
        assert!(stale_push_providers(&state).expect("stale").is_empty());
        assert!(state.runtime.read().await.provider_errors.is_empty());
    }

    #[test]
    fn long_silent_pusher_alerts_once_and_recovers() {
        let state = test_state(None);
        {
            let database = state.database.lock().expect("database");
            database
                .execute(
                    "INSERT INTO snapshots(provider_key, window_type, payload_json, reset_at, observed_at)
                     VALUES ('antigravity', 'weekly', '{}', 0, ?1)",
                    params![Utc::now().timestamp() - 2 * 86_400],
                )
                .expect("old push");
        }
        for _ in 0..6 {
            check_push_silence(&state).expect("watch");
        }
        assert_eq!(
            count(
                &state,
                "SELECT COUNT(*) FROM reset_events WHERE event_kind = 'provider_error'"
            ),
            1
        );
        record_provider_recovery(&state, "antigravity").expect("recovery");
        assert_eq!(
            count(
                &state,
                "SELECT COUNT(*) FROM reset_events WHERE event_kind = 'provider_recovered'"
            ),
            1
        );
    }

    #[test]
    fn rejects_unknown_providers_and_timezones() {
        let config = |enabled: &str, push: &str| Config {
            enabled_providers: vec![enabled.to_string()],
            push_providers: vec![push.to_string()],
            ..Config::default()
        };
        assert!(validate_providers(&config("grok", "antigravity")).is_ok());
        assert!(validate_providers(&config("gemini", "antigravity")).is_err());
        assert!(validate_providers(&config("grok", "grok")).is_err());
        assert!(validate_providers(&config("grok", "Not Valid")).is_err());
        assert!(parse_timezone("Asia/Shanghai").is_ok());
        assert!(parse_timezone("Mars/Base").is_err());
    }

    #[test]
    fn secret_comparison_checks_all_bytes() {
        let expected = sha256(b"expected");
        assert!(constant_time_eq(&expected, &sha256(b"expected")));
        assert!(!constant_time_eq(&expected, &sha256(b"different")));
    }

    #[test]
    fn parses_postmark_message_id() {
        let response: PostmarkResponse = serde_json::from_value(serde_json::json!({
            "ErrorCode": 0,
            "Message": "OK",
            "MessageID": "message-id"
        }))
        .expect("valid Postmark response");
        assert_eq!(response.message_id, "message-id");
    }
}
