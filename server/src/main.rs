use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{TimeZone, Utc};
use chrono_tz::Europe::London;
use panel4ai_core::{ProviderError, ProviderPaths, QuotaClient, SnapshotEnvelope, UsageSnapshot};
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
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tracing::{error, info, warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/panel4ai/server.toml";
const SNAPSHOT_HISTORY_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
const RESET_RECOVERY_MIN_POINTS: f64 = 3.0;
const RESET_CONFIRMATION_MIN_POINTS: f64 = 1.5;

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
    poll_lock: AsyncMutex<()>,
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
    current_remaining: f64,
    event_kind: String,
    reason: String,
    detail: Option<String>,
    attempts: i64,
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
    let quota = QuotaClient::new(
        ProviderPaths {
            codex_auth_path: config.codex_auth_path.clone(),
            codex_binary_path: config
                .codex_use_app_server
                .then(|| config.codex_binary_path.clone()),
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
        poll_lock: AsyncMutex::new(()),
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

async fn poll_loop(state: Arc<AppState>) {
    loop {
        if let Err(error) = poll_once(&state).await {
            error!(%error, "quota poll failed");
        }
        tokio::time::sleep(Duration::from_secs(state.config.poll_interval_sec)).await;
    }
}

async fn poll_once(state: &Arc<AppState>) -> Result<(), String> {
    let _poll_guard = state.poll_lock.lock().await;
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
            record_provider_recovery(state, provider_key)?;
            let mut runtime = state.runtime.write().await;
            runtime.provider_errors.remove(provider_key);
            runtime.next_allowed_at.remove(provider_key);
        }
        Err(error) => {
            record_provider_failure(state, provider_key, &error)?;
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

    if let Some((old_reset_at, previous_remaining)) = previous {
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
                    &transaction,
                    provider_key,
                    snapshot,
                    candidate.old_reset_at,
                    candidate.baseline_remaining,
                    candidate.first_observed_at,
                    "usage_recovery_confirmed",
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
    if old_boundary_reached && new_reset_at > old_reset_at + 60 {
        return Some("next_reset_advanced");
    }
    if old_boundary_reached
        && new_reset_at == 0
        && previous_remaining < 99.5
        && current_remaining >= 99.5
    {
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

fn enqueue_quota_reset(
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

#[allow(clippy::too_many_arguments)]
fn insert_event(
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
        transaction
            .execute(
                "INSERT INTO email_outbox(event_id, next_attempt_at) VALUES (?1, ?2)",
                params![event_id, created_at],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(inserted > 0)
}

fn provider_failure_alert_threshold(error: &ProviderError) -> i64 {
    match error {
        ProviderError::Unauthorized(_) | ProviderError::MissingCredentials(_) => 3,
        ProviderError::RateLimited { .. } => 12,
        ProviderError::Transport(_) | ProviderError::InvalidResponse(_) => 6,
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
            format_timestamp(first_failed_at)
        );
        insert_event(
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
    if state.postmark_token.is_none() {
        return Ok(());
    }
    let items = load_pending_outbox(state)?;
    for item in items {
        match send_event_email(state, &item).await {
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
                    e.new_reset_at, e.previous_remaining, e.current_remaining,
                    e.event_kind, e.reason, e.detail, o.attempts
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
                current_remaining: row.get(7)?,
                event_kind: row.get(8)?,
                reason: row.get(9)?,
                detail: row.get(10)?,
                attempts: row.get(11)?,
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
        _ => send_reset_email(state, token, item).await,
    }
}

async fn send_reset_email(
    state: &Arc<AppState>,
    token: &str,
    item: &OutboxItem,
) -> Result<String, String> {
    let old_local = format_timestamp(item.old_reset_at);
    let new_local = format_next_reset(item.new_reset_at);
    let provider_name = provider_display_name(&item.provider);
    let subject = format!("[Panel4AI] {provider_name} {} 额度已重置", item.window_type);
    let text = format!(
        "{provider_name} 的 {} 额度窗口已确认重置。\n\n上一个重置点：{old_local}\n下一个重置点：{new_local}\n重置前剩余：{:.1}%\n当前剩余：{:.1}%\n检测方式：{}\n事件 ID：{}\n",
        item.window_type,
        item.previous_remaining,
        item.current_remaining,
        item.reason,
        item.event_id
    );
    let provider_name_html = html_escape(provider_name);
    let window_type_html = html_escape(&item.window_type);
    let reason_html = html_escape(&item.reason);
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

fn provider_display_name(provider: &str) -> &str {
    if provider.starts_with("openai") {
        "Codex"
    } else if provider.starts_with("claude") {
        "Claude"
    } else {
        provider
    }
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

fn format_next_reset(epoch: i64) -> String {
    if epoch <= 0 {
        "未安排（当前额度窗口空闲）".to_string()
    } else {
        format_timestamp(epoch)
    }
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
        let database = Connection::open_in_memory().expect("in-memory database");
        initialize_database(&database).expect("database initialization");
        let quota = QuotaClient::new(
            ProviderPaths {
                codex_auth_path: "/tmp/panel4ai-test/codex/auth.json".into(),
                codex_binary_path: None,
                claude_auth_path: "/tmp/panel4ai-test/claude/auth.json".into(),
            },
            20.0,
        )
        .expect("quota client");
        let state = Arc::new(AppState {
            config: Config::default(),
            quota,
            database: Mutex::new(database),
            postmark_client: reqwest::Client::new(),
            postmark_token: None,
            api_token_hash: sha256(b"test"),
            runtime: RwLock::new(RuntimeStatus::default()),
            poll_lock: AsyncMutex::new(()),
        });
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
