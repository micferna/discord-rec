#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appimage;
mod clip;
mod config;
mod convert;
mod instance;
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
mod winproc;
#[cfg(unix)]
mod x11;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
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
    mic_denoise: bool,
    keep_only_last: bool,
    auto_mp4: bool,
    retention_days: u32,
    retention_max_gb: u32,
    clip_after_s: u32,
}

#[derive(Serialize)]
struct RecFile {
    name: String,
    size_bytes: u64,
    modified_ms: u64,
}

#[tauri::command]
fn get_status(shared: SharedState) -> Status {
    let mut status = shared.status.lock().expect("mutex status").clone();
    // Reflète l'état AUTO réel dès le premier rendu (avant le premier publish).
    status.enabled = shared.enabled.load(Ordering::Relaxed);
    status
}

#[tauri::command]
fn set_enabled(shared: SharedState, enabled: bool) {
    shared.enabled.store(enabled, Ordering::Relaxed);
    // Persiste l'état du bouton AUTO pour qu'il survive au redémarrage.
    let mut cfg = shared.config.lock().expect("mutex config");
    cfg.enabled = enabled;
    let _ = config::save(&cfg);
}

/// Force (ou arrête) un enregistrement manuel, indépendamment de la détection
/// vocale : utile pour capturer un partage d'écran hors salon. Transitoire —
/// non persisté (au redémarrage, l'app repart en mode AUTO seul).
#[tauri::command]
fn set_force_recording(shared: SharedState, on: bool) {
    shared.force.store(on, Ordering::Relaxed);
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
        mic_denoise: cfg.mic_denoise,
        keep_only_last: cfg.keep_only_last,
        auto_mp4: cfg.auto_mp4,
        retention_days: cfg.retention_days,
        retention_max_gb: cfg.retention_max_gb,
        clip_after_s: cfg.clip_after_s,
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
    cfg.mic_denoise = ui.mic_denoise;
    cfg.keep_only_last = ui.keep_only_last;
    cfg.auto_mp4 = ui.auto_mp4;
    cfg.retention_days = ui.retention_days;
    cfg.retention_max_gb = ui.retention_max_gb;
    cfg.clip_after_s = ui.clip_after_s;
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
            if !std::path::Path::new(&name).extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("mkv") || ext.eq_ignore_ascii_case("mp4")
            }) {
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
    use crate::appimage::CommandAppImageExt;
    use std::process::Stdio;
    #[cfg(unix)]
    let opener = "xdg-open";
    #[cfg(windows)]
    let opener = "explorer";
    std::process::Command::new(opener)
        .strip_appimage_env()
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

/// Ouvre un enregistrement/clip avec le lecteur par défaut du système.
#[tauri::command]
fn open_recording(shared: SharedState, name: String) -> Result<(), String> {
    if !valid_media_name(&name) {
        return Err("fichier invalide".into());
    }
    let path = shared.config_snapshot().output_dir.join(&name);
    if !path.is_file() {
        return Err("fichier introuvable".into());
    }
    open_with_system(path.as_os_str())
}

/// Supprime un enregistrement/clip du dossier de sortie. Refuse de supprimer
/// l'enregistrement en cours.
#[tauri::command]
fn delete_recording(shared: SharedState, name: String) -> Result<(), String> {
    if !valid_media_name(&name) {
        return Err("fichier invalide".into());
    }
    // Ne pas supprimer le fichier en cours d'écriture.
    let current = shared.status.lock().expect("mutex status").file.clone();
    if let Some(cur) = current {
        if std::path::Path::new(&cur).file_name() == Some(std::ffi::OsStr::new(&name)) {
            return Err("enregistrement en cours — arrête-le avant de le supprimer".into());
        }
    }
    let path = shared.config_snapshot().output_dir.join(&name);
    std::fs::remove_file(&path).map_err(|e| format!("suppression : {e}"))
}

/// Convertit un enregistrement MKV en MP4 (audio AAC, lisible partout).
/// `height = None` garde la résolution source (copie vidéo sans perte) ;
/// sinon réduit jusqu'à cette hauteur. Renvoie le nom du MP4 produit.
#[tauri::command]
async fn convert_recording(
    shared: SharedState<'_>,
    name: String,
    height: Option<u32>,
) -> Result<String, String> {
    // Le nom doit rester un simple fichier MKV sûr du dossier de sortie
    // (`is_safe_file_name` bloque l'injection de pipeline via nom hostile).
    if !is_safe_file_name(&name)
        || std::path::Path::new(&name)
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("mkv"))
    {
        return Err("fichier à convertir invalide".into());
    }
    let dir = shared.config_snapshot().output_dir;
    convert::to_mp4(&dir, &name, height)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Marge (s) retirée du bord live d'un clip : le dernier cluster du MKV en
/// cours d'écriture peut être incomplet, on s'arrête un peu avant.
const LIVE_CLIP_MARGIN_S: f64 = 3.0;

/// Vérifie qu'un nom désigne un simple fichier du dossier de sortie, sans
/// séparateur de chemin **ni caractère de contrôle**.
///
/// Le rejet des caractères de contrôle (tabulation, saut de ligne, retour
/// chariot…) ferme une injection de pipeline : `convert::to_mp4` passe le nom à
/// `gst-launch-1.0` sous la forme `location=<nom>`, et `gst_parse_launchv`
/// re-parse ses arguments en n'échappant QUE l'espace. Une tabulation dans le
/// nom serait donc interprétée comme de la syntaxe `GStreamer` (`… ! filesink
/// location=…`), permettant d'instancier des éléments arbitraires. L'espace,
/// lui, est échappé par gst et reste sûr.
fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
}

/// Vérifie qu'un nom désigne un simple fichier (mkv/mp4) sûr du dossier de sortie.
fn valid_media_name(name: &str) -> bool {
    is_safe_file_name(name)
        && std::path::Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mkv") || e.eq_ignore_ascii_case("mp4"))
}

/// Montage (B) : extrait `[start_s, start_s + duration_s]` d'un enregistrement
/// existant vers un MP4. `height = None` garde la résolution source.
#[tauri::command]
async fn clip_range(
    shared: SharedState<'_>,
    name: String,
    start_s: f64,
    duration_s: f64,
    height: Option<u32>,
) -> Result<String, String> {
    if !valid_media_name(&name) {
        return Err("fichier à découper invalide".into());
    }
    let dir = shared.config_snapshot().output_dir;
    // Fichier terminé (montage) : le seek est fiable → fenêtrage rapide.
    clip::clip(&dir, &name, start_s, duration_s, height, false)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Clip live (A) : « les `minutes` dernières minutes » de l'enregistrement en
/// cours, sans l'arrêter. Lit le fichier et l'instant de début dans le statut
/// publié, calcule la fenêtre (avec marge avant le bord live) et découpe.
#[tauri::command]
async fn clip_live(shared: SharedState<'_>, minutes: f64) -> Result<String, String> {
    if !minutes.is_finite() || minutes <= 0.0 {
        return Err("durée de clip invalide".into());
    }
    // Instantané du statut : fichier en cours + horodatage de début.
    let (file, started_at_ms) = {
        let status = shared.status.lock().expect("mutex status");
        (status.file.clone(), status.started_at_ms)
    };
    let (Some(file), Some(started_at_ms)) = (file, started_at_ms) else {
        return Err("aucun enregistrement en cours à clipper".into());
    };

    let path = std::path::PathBuf::from(&file);
    let dir = path.parent().map_or_else(
        || shared.config_snapshot().output_dir,
        std::path::Path::to_path_buf,
    );
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("nom de l'enregistrement en cours illisible")?
        .to_owned();

    // Secondes capturées APRÈS le clic (réglage) : on attend ce délai pour que
    // l'enregistrement en cours écrive la suite, puis on l'inclut dans la
    // fenêtre. `0` = clip jusqu'au bord live actuel (comportement d'origine).
    // La marge live reste retranchée : l'« après » effectif vaut `après − marge`.
    let after_s = f64::from(shared.config_snapshot().clip_after_s);

    // Début de fenêtre ancré à l'instant du clic ; la fin suivra le bord live
    // (après l'éventuelle marge « après »).
    let press_ms = now_unix_ms().unwrap_or(started_at_ms);
    let start_s = (elapsed_secs(started_at_ms, press_ms) - minutes * 60.0).max(0.0);

    if after_s > 0.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(after_s)).await;
    }

    // On vise jusqu'à un peu avant le bord live (cluster en cours).
    let edge_ms = now_unix_ms().unwrap_or(press_ms);
    let stop_s = (elapsed_secs(started_at_ms, edge_ms) - LIVE_CLIP_MARGIN_S).max(0.0);
    let duration_s = stop_s - start_s;
    if duration_s < 1.0 {
        return Err(
            "enregistrement trop court pour ce clip (réessaie dans quelques secondes)".into(),
        );
    }
    // Fichier EN COURS d'écriture : le seek n'est pas fiable (pas d'index, et
    // il se comporte mal sous Windows) → fenêtrage par probe uniquement.
    clip::clip(&dir, &name, start_s, duration_s, None, true)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Millisecondes epoch actuelles (`None` si l'horloge est avant l'epoch).
fn now_unix_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

/// Écoulé (secondes) entre `started_at_ms` et `at_ms` (ms epoch). Durée média
/// `< 2^53` ms → conversion exacte en flottant.
#[allow(clippy::cast_precision_loss)]
fn elapsed_secs(started_at_ms: u64, at_ms: u64) -> f64 {
    at_ms.saturating_sub(started_at_ms) as f64 / 1000.0
}

/// Durée + résolution renvoyées à l'interface pour enrichir la liste.
#[derive(Clone, Copy, Serialize)]
struct ProbeInfo {
    duration_s: f64,
    width: Option<u32>,
    height: Option<u32>,
}

/// Entrée de cache : la sonde, valide tant que taille ET mtime sont inchangés.
struct CachedProbe {
    size: u64,
    mtime_ms: u64,
    info: ProbeInfo,
}

/// Cache mémoire des sondes (durée/résolution), pour ne prérouler chaque fichier
/// qu'une fois. Les enregistrements finalisés sont immuables ; l'invalidation par
/// taille+mtime couvre le cas d'un fichier remplacé.
fn probe_cache() -> &'static Mutex<HashMap<String, CachedProbe>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedProbe>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Sonde un enregistrement (durée + résolution) pour la liste, via le cache.
/// Un fichier en cours d'écriture (taille/mtime changeants) est re-sondé ; s'il
/// n'a pas encore de durée connue, l'erreur est simplement remontée (l'UI
/// n'affiche alors pas de méta pour cette ligne).
#[tauri::command]
async fn probe_recording(shared: SharedState<'_>, name: String) -> Result<ProbeInfo, String> {
    if !valid_media_name(&name) {
        return Err("fichier invalide".into());
    }
    let dir = shared.config_snapshot().output_dir;
    let meta = std::fs::metadata(dir.join(&name)).map_err(|_| "fichier introuvable".to_owned())?;
    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    // Cache hit : taille ET mtime identiques.
    if let Some(cached) = probe_cache().lock().expect("mutex cache").get(&name) {
        if cached.size == size && cached.mtime_ms == mtime_ms {
            return Ok(cached.info);
        }
    }
    let media = clip::probe(&dir, &name)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let info = ProbeInfo {
        duration_s: media.duration_s,
        width: media.width,
        height: media.height,
    };
    probe_cache().lock().expect("mutex cache").insert(
        name,
        CachedProbe {
            size,
            mtime_ms,
            info,
        },
    );
    Ok(info)
}

/// Durée (s) d'un enregistrement : l'interface de montage en a besoin pour
/// borner les points d'entrée/sortie.
#[tauri::command]
async fn media_duration(shared: SharedState<'_>, name: String) -> Result<f64, String> {
    if !valid_media_name(&name) {
        return Err("fichier invalide".into());
    }
    let dir = shared.config_snapshot().output_dir;
    clip::duration(&dir, &name)
        .await
        .map_err(|e| format!("{e:#}"))
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

/// Windows : ajoute le dossier `bin` de `GStreamer` au PATH du processus, pour
/// que les DLL `GStreamer` (liées en delay-load, cf. `build.rs`) soient
/// trouvées au 1er appel du crate `gstreamer` (montage/clip) — l'installeur
/// officiel ne met pas ce dossier dans le PATH. Avant tout appel `GStreamer`.
#[cfg(windows)]
fn add_gstreamer_dll_dir() {
    let Some(bin) = recorder::gstreamer_bin_dir() else {
        return;
    };
    let mut path = bin.into_os_string();
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(";");
        path.push(existing);
    }
    std::env::set_var("PATH", path);
}

fn main() {
    #[cfg(windows)]
    add_gstreamer_dll_dir();

    // Avant tout : couper les instances obsolètes encore en mémoire (ancienne
    // version dont le binaire a été remplacé par une mise à jour). Sinon
    // single-instance refocaliserait cette vieille fenêtre, qui réafficherait
    // sans fin la bannière de mise à jour.
    instance::terminate_stale_instances();

    let shared = Arc::new(Shared::new(config::load()));
    let setup_shared = shared.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
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
            set_force_recording,
            get_config,
            set_config,
            get_app_version,
            list_mics,
            reset_window_token,
            list_recordings,
            convert_recording,
            clip_range,
            clip_live,
            media_duration,
            probe_recording,
            open_recording,
            delete_recording,
            open_recordings_dir,
            quit_app,
            updates::check_update,
            updates::install_update,
            updates::open_releases_page
        ])
        .run(tauri::generate_context!())
        .expect("échec du démarrage de l'application");
}

#[cfg(test)]
mod tests {
    use super::{is_safe_file_name, valid_media_name};

    #[test]
    fn accepte_les_noms_generes_par_lapp() {
        for name in [
            "discord-2026-07-25_15-30-00.mkv",
            "discord-2026-07-25_15-30-00.mp4",
            "discord-2026-07-25_15-30-00_720p.mp4",
            "discord-2026-07-25_15-30-00_clip_0-30s.mp4",
        ] {
            assert!(valid_media_name(name), "devrait accepter : {name:?}");
            assert!(is_safe_file_name(name));
        }
    }

    /// Non-régression de l'injection de pipeline `GStreamer` : un nom porteur
    /// d'un caractère de contrôle (tabulation/saut de ligne) — le vecteur qui
    /// injecte `! filesink location=…` dans `gst-launch` — doit être refusé,
    /// même s'il se termine par une extension média « valide ».
    #[test]
    fn rejette_linjection_par_caractere_de_controle() {
        for hostile in [
            "a\t! filesink location=owned.mkv",   // tabulation
            "a\n!\nfilesink\nlocation=owned.mkv", // saut de ligne
            "a\r! fakesink.mkv",                  // retour chariot
            "a\x0b.mkv",                          // tabulation verticale
            "a\x0c.mkv",                          // saut de page
        ] {
            assert!(
                !valid_media_name(hostile),
                "aurait dû rejeter (injection) : {hostile:?}"
            );
            assert!(!is_safe_file_name(hostile));
        }
    }

    #[test]
    fn rejette_les_separateurs_et_le_vide() {
        for bad in ["", "../x.mkv", "sub/x.mkv", "sub\\x.mkv", "x.txt", "x"] {
            assert!(!valid_media_name(bad), "aurait dû rejeter : {bad:?}");
        }
    }

    /// L'espace est sûr (échappé par gst) : un nom de fichier avec espaces
    /// reste accepté, on ne casse pas les fichiers légitimes de l'utilisateur.
    #[test]
    fn accepte_les_espaces() {
        assert!(valid_media_name("ma session discord.mkv"));
        assert!(is_safe_file_name("mon fichier"));
    }
}
