#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod portal;
mod pw;
mod recorder;
mod service;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

use service::{Shared, Status};

type SharedState<'a> = State<'a, Arc<Shared>>;

/// Sous-ensemble de la config exposé à l'interface (le jeton du portail
/// reste interne).
#[derive(Serialize, Deserialize)]
struct UiConfig {
    output_dir: PathBuf,
    video: bool,
    video_bitrate_kbps: u32,
    audio_bitrate_kbps: u32,
    stop_debounce_s: u32,
}

#[derive(Serialize)]
struct RecFile {
    name: String,
    size_bytes: u64,
    modified_ms: u64,
}

#[tauri::command]
fn get_status(shared: SharedState) -> Status {
    shared.status.lock().expect("mutex status").clone()
}

#[tauri::command]
fn set_enabled(shared: SharedState, enabled: bool) {
    shared.enabled.store(enabled, Ordering::Relaxed);
}

#[tauri::command]
fn get_config(shared: SharedState) -> UiConfig {
    let cfg = shared.config_snapshot();
    UiConfig {
        output_dir: cfg.output_dir,
        video: cfg.video,
        video_bitrate_kbps: cfg.video_bitrate_kbps,
        audio_bitrate_kbps: cfg.audio_bitrate_kbps,
        stop_debounce_s: cfg.stop_debounce_s,
    }
}

#[tauri::command]
fn set_config(shared: SharedState, ui: UiConfig) -> Result<(), String> {
    if !ui.output_dir.is_absolute() {
        return Err("le dossier de sortie doit être un chemin absolu".into());
    }
    let mut cfg = shared.config.lock().expect("mutex config");
    cfg.output_dir = ui.output_dir;
    cfg.video = ui.video;
    cfg.video_bitrate_kbps = ui.video_bitrate_kbps;
    cfg.audio_bitrate_kbps = ui.audio_bitrate_kbps;
    cfg.stop_debounce_s = ui.stop_debounce_s;
    cfg.sanitize();
    config::save(&cfg).map_err(|e| format!("{e:#}"))
}

/// Oublie la fenêtre mémorisée : le prochain enregistrement redemandera
/// quelle fenêtre capturer.
#[tauri::command]
fn reset_window_token(shared: SharedState) -> Result<(), String> {
    let mut cfg = shared.config.lock().expect("mutex config");
    cfg.restore_token = None;
    config::save(&cfg).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn list_recordings(shared: SharedState) -> Vec<RecFile> {
    let dir = shared.config_snapshot().output_dir;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<RecFile> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mkv"))
            {
                return None;
            }
            let meta = entry.metadata().ok()?;
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| u64::try_from(d.as_millis()).ok())
                .unwrap_or_default();
            Some(RecFile {
                name,
                size_bytes: meta.len(),
                modified_ms,
            })
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.modified_ms));
    files.truncate(30);
    files
}

#[tauri::command]
fn open_recordings_dir(shared: SharedState) -> Result<(), String> {
    let dir = shared.config_snapshot().output_dir;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::process::Command::new("xdg-open")
        .arg(&dir)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Demande l'arrêt : la boucle de service finalise l'enregistrement en cours
/// puis quitte l'application.
#[tauri::command]
fn quit_app(shared: SharedState) {
    shared.quit.store(true, Ordering::Relaxed);
}

fn main() {
    let shared = Arc::new(Shared::new(config::load()));

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // Relancer le binaire ramène la fenêtre existante.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(shared.clone())
        .setup(move |app| {
            tauri::async_runtime::spawn(service::run(app.handle().clone(), shared));
            Ok(())
        })
        .on_window_event(|window, event| {
            // Fermer la fenêtre cache l'app : le service continue d'enregistrer.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            set_enabled,
            get_config,
            set_config,
            reset_window_token,
            list_recordings,
            open_recordings_dir,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("échec du démarrage de l'application");
}
