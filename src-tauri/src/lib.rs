use chrono::Utc;
use panel4ai_core::SnapshotEnvelope;
pub use panel4ai_core::UsageSnapshot;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Position, Size, State};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutoStartExt};
use tauri_plugin_notification::NotificationExt;

const MAIN_WINDOW_LABEL: &str = "main";
const SETTINGS_FILE: &str = "settings.json";
const PANEL_WIDTH: u32 = 360;
const PANEL_HEIGHT: u32 = 420;
const TRAY_ID: &str = "main-tray";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    refresh_interval_sec: u64,
    alert_threshold_percent: f64,
    collapse_delay_ms: u64,
    auto_start: bool,
    dock_position: String,
    server_url: String,
    server_api_token: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            refresh_interval_sec: 120,
            alert_threshold_percent: 20.0,
            collapse_delay_ms: 0,
            auto_start: true,
            dock_position: "top-right".to_string(),
            server_url: "http://127.0.0.1:8787".to_string(),
            server_api_token: "".to_string(),
        }
    }
}

struct AppState {
    settings: Mutex<AppSettings>,
    last_notification_epoch: Mutex<Option<i64>>,
    http_client: reqwest::Client,
    panel_pinned: AtomicBool,
    panel_hovered: AtomicBool,
    hover_generation: AtomicU64,
    panel_height: AtomicU32,
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
        if state.panel_pinned.load(Ordering::Relaxed) || state.panel_hovered.load(Ordering::Relaxed)
        {
            return;
        }
        if let Some(window) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
            let _ = window.hide();
        }
    });
}

#[tauri::command]
fn resize_panel(app: AppHandle, state: State<'_, AppState>, height: u32) -> Result<(), String> {
    state.panel_height.store(height, Ordering::Relaxed);
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "Main window not found".to_string())?;

    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let position = window
        .outer_position()
        .map_err(|e| e.to_string())?
        .to_logical::<f64>(scale);
    let current_height = window
        .outer_size()
        .map_err(|e| e.to_string())?
        .to_logical::<f64>(scale)
        .height;

    let panel_h = height as f64;
    // macOS anchors the top edge and grows downward; Windows anchors the bottom edge.
    let y = if cfg!(target_os = "macos") {
        position.y
    } else {
        position.y + current_height - panel_h
    };
    place_panel(&window, position.x, y, panel_h);
    Ok(())
}

/// Sizes and moves the panel in logical units, keeping it on the current monitor.
fn place_panel(window: &tauri::WebviewWindow, mut x: f64, mut y: f64, panel_h: f64) {
    let panel_w = PANEL_WIDTH as f64;
    const MARGIN: f64 = 4.0;
    if let Ok(Some(monitor)) = window.current_monitor() {
        let scale = monitor.scale_factor();
        let left = monitor.position().x as f64 / scale;
        let top = monitor.position().y as f64 / scale;
        let right = left + monitor.size().width as f64 / scale;
        let bottom = top + monitor.size().height as f64 / scale;
        // min before max, so a panel taller than the screen pins to its top edge.
        x = x.min(right - panel_w - MARGIN).max(left + MARGIN);
        y = y.min(bottom - panel_h - MARGIN).max(top + MARGIN);
    }
    let _ = window.set_size(Size::Logical(LogicalSize::new(panel_w, panel_h)));
    let _ = window.set_position(Position::Logical(LogicalPosition::new(x, y)));
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

#[cfg(target_os = "macos")]
fn configure_macos_panel_window(window: &tauri::WebviewWindow) {
    use objc2_app_kit::{NSStatusWindowLevel, NSWindow, NSWindowCollectionBehavior};

    if let Err(err) = window.set_always_on_top(true) {
        log::warn!("Failed to keep panel always on top: {err}");
    }
    if let Err(err) = window.set_visible_on_all_workspaces(true) {
        log::warn!("Failed to show panel on all workspaces: {err}");
    }

    let window = window.clone();
    let ns_window_owner = window.clone();
    if let Err(err) = window.run_on_main_thread(move || unsafe {
        let Ok(ns_window) = ns_window_owner.ns_window() else {
            log::warn!("Failed to get NSWindow handle for panel");
            return;
        };

        let ns_window: &NSWindow = &*ns_window.cast();
        let mut collection_behavior = ns_window.collectionBehavior();
        collection_behavior |= NSWindowCollectionBehavior::CanJoinAllSpaces;
        collection_behavior |= NSWindowCollectionBehavior::FullScreenAuxiliary;
        collection_behavior |= NSWindowCollectionBehavior::Transient;
        ns_window.setCollectionBehavior(collection_behavior);
        ns_window.setLevel(NSStatusWindowLevel);
    }) {
        log::warn!("Failed to configure macOS panel window on main thread: {err}");
    }
}

#[cfg(target_os = "macos")]
fn order_macos_panel_front(window: &tauri::WebviewWindow) {
    use objc2_app_kit::NSWindow;

    let window = window.clone();
    let ns_window_owner = window.clone();
    if let Err(err) = window.run_on_main_thread(move || unsafe {
        let Ok(ns_window) = ns_window_owner.ns_window() else {
            log::warn!("Failed to get NSWindow handle for front ordering");
            return;
        };

        let ns_window: &NSWindow = &*ns_window.cast();
        ns_window.orderFrontRegardless();
    }) {
        log::warn!("Failed to bring macOS panel to front: {err}");
    }
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
async fn get_usage_report(state: State<'_, AppState>) -> Result<SnapshotEnvelope, String> {
    let (settings, client) = {
        let settings = state
            .settings
            .lock()
            .expect("settings mutex poisoned")
            .clone();
        (settings, state.http_client.clone())
    };
    if settings.server_api_token.trim().is_empty() {
        return Err("Server access token is not configured".to_string());
    }
    let base = panel4ai_core::normalize_server_url(&settings.server_url)?;
    let url = base
        .join("api/v1/snapshots")
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .bearer_auth(settings.server_api_token.trim())
        .send()
        .await
        .map_err(|error| format!("Server request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Server returned HTTP {}", response.status()));
    }
    response
        .json::<SnapshotEnvelope>()
        .await
        .map_err(|error| format!("Invalid server response: {error}"))
}

#[tauri::command]
async fn send_test_email(state: State<'_, AppState>) -> Result<String, String> {
    let (settings, client) = {
        let settings = state
            .settings
            .lock()
            .expect("settings mutex poisoned")
            .clone();
        (settings, state.http_client.clone())
    };
    if settings.server_api_token.trim().is_empty() {
        return Err("Server access token is not configured".to_string());
    }
    let base = panel4ai_core::normalize_server_url(&settings.server_url)?;
    let url = base
        .join("api/v1/test-email")
        .map_err(|error| error.to_string())?;
    let response = client
        .post(url)
        .bearer_auth(settings.server_api_token.trim())
        .send()
        .await
        .map_err(|error| format!("Server request failed: {error}"))?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Invalid server response: {error}"))?;
    let message = payload
        .get("message")
        .and_then(|value| value.as_str())
        .unwrap_or("No response message")
        .to_string();
    if status.is_success() {
        Ok(message)
    } else {
        Err(message)
    }
}

#[cfg(unix)]
fn write_private_file(path: &Path, content: &[u8]) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    file.set_len(0).map_err(|error| error.to_string())?;
    file.write_all(content).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, content: &[u8]) -> Result<(), String> {
    fs::write(path, content).map_err(|error| error.to_string())
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let config_dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    Ok(config_dir.join(SETTINGS_FILE))
}

fn persist_settings(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    let content = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    write_private_file(&path, content.as_bytes())
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
    settings.refresh_interval_sec = settings.refresh_interval_sec.max(15);
    settings.alert_threshold_percent = settings.alert_threshold_percent.clamp(1.0, 99.0);
    settings.server_url = settings.server_url.trim().trim_end_matches('/').to_string();
}

fn position_panel_near_tray(window: &tauri::WebviewWindow, tray_rect: &tauri::Rect, height: u32) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let (tx, ty) = match tray_rect.position {
        Position::Physical(p) => (p.x as f64 / scale, p.y as f64 / scale),
        Position::Logical(p) => (p.x, p.y),
    };
    let tray_h = match tray_rect.size {
        Size::Physical(s) => s.height as f64 / scale,
        Size::Logical(s) => s.height,
    };

    let panel_h = height as f64;
    let x = tx - PANEL_WIDTH as f64 / 2.0;
    let y = if cfg!(target_os = "macos") {
        ty + tray_h + 4.0
    } else {
        ty - panel_h - 8.0
    };
    place_panel(window, x, y, panel_h);
}

fn show_panel_preview(app: &AppHandle, rect: &tauri::Rect) {
    let state = app.state::<AppState>();
    state.hover_generation.fetch_add(1, Ordering::Relaxed);

    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let pinned = state.panel_pinned.load(Ordering::Relaxed);
        if !pinned {
            position_panel_near_tray(&window, rect, state.panel_height.load(Ordering::Relaxed));
        }
        if !window.is_visible().unwrap_or(false) {
            let _ = window.show();
        }
        #[cfg(target_os = "macos")]
        if !pinned {
            order_macos_panel_front(&window);
        }
    }
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
                TrayIconEvent::Enter { rect, .. } | TrayIconEvent::Move { rect, .. } => {
                    show_panel_preview(&app, &rect);
                    // No set_focus — hover is just a preview
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
                        if state.panel_pinned.load(Ordering::Relaxed)
                            || state.panel_hovered.load(Ordering::Relaxed)
                        {
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
                                position_panel_near_tray(
                                    &window,
                                    &rect,
                                    state.panel_height.load(Ordering::Relaxed),
                                );
                                let _ = window.show();
                                #[cfg(target_os = "macos")]
                                order_macos_panel_front(&window);
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
            panel_height: AtomicU32::new(PANEL_HEIGHT),
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

            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                if let Some(win) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                    use window_vibrancy::{
                        apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState,
                    };
                    configure_macos_panel_window(&win);
                    let _ = apply_vibrancy(
                        &win,
                        NSVisualEffectMaterial::HudWindow,
                        Some(NSVisualEffectState::Active),
                        Some(12.0),
                    );
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_usage_report,
            hide_panel,
            panel_mouse_enter,
            panel_mouse_leave,
            resize_panel,
            update_tray_status,
            notify_if_needed,
            send_test_email
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_points_at_a_local_server() {
        let settings = AppSettings::default();
        assert_eq!(settings.refresh_interval_sec, 120);
        assert_eq!(settings.server_api_token, "");
    }

    #[test]
    fn normalize_clamps_values_and_trims_the_url() {
        let mut settings = AppSettings {
            refresh_interval_sec: 1,
            alert_threshold_percent: 500.0,
            collapse_delay_ms: 9,
            server_url: " http://100.1.2.3:8787/ ".to_string(),
            ..AppSettings::default()
        };
        normalize_settings(&mut settings);
        assert_eq!(settings.refresh_interval_sec, 15);
        assert_eq!(settings.alert_threshold_percent, 99.0);
        assert_eq!(settings.collapse_delay_ms, 0);
        assert_eq!(settings.server_url, "http://100.1.2.3:8787");
    }

    #[test]
    fn settings_from_older_versions_keep_the_server_fields() {
        let json = r#"{
            "provider": "openai_oauth",
            "dataSource": "remote_fallback",
            "codexAuthPath": "/tmp/auth.json",
            "limitsByWindow": { "weekly": 100 },
            "selectedWindowType": "hourly5",
            "serverUrl": "http://100.1.2.3:8787",
            "serverApiToken": "kept",
            "refreshIntervalSec": 60
        }"#;
        let settings: AppSettings = serde_json::from_str(json).expect("old settings parse");
        assert_eq!(settings.server_url, "http://100.1.2.3:8787");
        assert_eq!(settings.server_api_token, "kept");
        assert_eq!(settings.refresh_interval_sec, 60);
    }

    #[test]
    fn report_without_new_fields_still_parses() {
        let json = r#"{ "serverTime": 1, "lastPollAt": null, "snapshots": [] }"#;
        let report: SnapshotEnvelope = serde_json::from_str(json).expect("report parse");
        assert!(report.stale_providers.is_empty());
        assert!(report.provider_errors.is_empty());
    }
}
