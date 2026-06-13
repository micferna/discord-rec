#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod mics;
#[cfg(unix)]
mod portal;
#[cfg(unix)]
mod pw;
mod recorder;
mod service;
mod updates;
mod voice;
#[cfg(windows)]
mod win;
#[cfg(unix)]
mod x11;

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
    framerate: u32,
    stop_debounce_s: u32,
    mic_target: Option<String>,
    mix_audio: bool,
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
        framerate: cfg.framerate,
        stop_debounce_s: cfg.stop_debounce_s,
        mic_target: cfg.mic_target,
        mix_audio: cfg.mix_audio,
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
    cfg.framerate = ui.framerate;
    cfg.stop_debounce_s = ui.stop_debounce_s;
    cfg.mic_target = ui.mic_target;
    cfg.mix_audio = ui.mix_audio;
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
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn list_mics() -> Vec<mics::Mic> {
    mics::list().await
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

/// Ouvre un chemin/URL avec le gestionnaire du système.
///
/// L'app est compilée sans console (`windows_subsystem = "windows"`) : ses
/// descripteurs standard sont invalides. Sans rediriger ceux de l'enfant
/// vers `null`, `explorer`/`xdg-open` héritent de descripteurs invalides et
/// échouent avec « os error 6 » (`ERROR_INVALID_HANDLE`). On force donc des
/// flux nuls valides.
fn open_with_system(arg: &std::ffi::OsStr) -> Result<(), String> {
    use std::process::Stdio;
    #[cfg(unix)]
    let opener = "xdg-open";
    #[cfg(windows)]
    let opener = "explorer";
    std::process::Command::new(opener)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_recordings_dir(shared: SharedState) -> Result<(), String> {
    let dir = shared.config_snapshot().output_dir;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    open_with_system(dir.as_os_str())
}

/// Demande l'arrêt : la boucle de service finalise l'enregistrement en cours
/// puis quitte l'application.
#[tauri::command]
fn quit_app(shared: SharedState) {
    shared.quit.store(true, Ordering::Relaxed);
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Icône de barre d'état : un point d'entrée visible (l'app reste cachée
/// pendant l'enregistrement) et un vrai « Quitter ». Évite les instances
/// fantômes invisibles.
fn build_tray(app: &tauri::AppHandle, shared: Arc<Shared>) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};

    let show = MenuItem::with_id(app, "show", "Afficher", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let tray_shared = shared.clone();
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().expect("icône fenêtre").clone())
        .tooltip("Discord REC")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => tray_shared.quit.store(true, Ordering::Relaxed),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    let shared = Arc::new(Shared::new(config::load()));
    let setup_shared = shared.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // Relancer le binaire ramène la fenêtre existante.
            show_main_window(app);
        }))
        .manage(shared.clone())
        .setup(move |app| {
            // Tray non bloquant : sur un Linux sans appindicator, l'app
            // fonctionne quand même (juste sans icône de barre d'état).
            if let Err(e) = build_tray(app.handle(), setup_shared.clone()) {
                eprintln!("[discord-rec] icône de barre d'état indisponible : {e}");
            }
            tauri::async_runtime::spawn(service::run(app.handle().clone(), setup_shared));
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
            get_app_version,
            list_mics,
            reset_window_token,
            list_recordings,
            open_recordings_dir,
            quit_app,
            updates::check_update,
            updates::install_update,
            updates::open_releases_page
        ])
        .run(tauri::generate_context!())
        .expect("échec du démarrage de l'application");
}
