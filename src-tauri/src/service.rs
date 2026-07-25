//! Boucle de surveillance : interroge l'état vocal de Discord chaque seconde,
//! démarre l'enregistrement à l'entrée en vocal, l'arrête (avec anti-rebond)
//! à la sortie, et publie l'état vers l'interface.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

use crate::config::Config;
use crate::recorder::{self, Recording, VideoSpec};
use crate::voice;

/// Notification système « best-effort » : la fenêtre pouvant être cachée
/// pendant tout l'enregistrement, c'est le seul retour visible. Un échec
/// (démon de notifications absent…) ne doit jamais perturber l'enregistrement.
fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Nom de fichier (sans le chemin) pour un affichage/notification concis.
fn base_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Ticks de pause après un échec de démarrage (laisse le temps à la cause
/// de disparaître sans spammer le portail en cas de repli Wayland).
const RETRY_COOLDOWN_TICKS: u32 = 10;
const TICK: Duration = Duration::from_secs(1);

#[derive(Clone, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    pub in_voice: bool,
    pub recording: bool,
    /// Enregistrement forcé manuellement (bouton REC), hors détection vocale.
    pub forced: bool,
    pub video_active: bool,
    pub encoder: Option<String>,
    pub file: Option<String>,
    pub started_at_ms: Option<u64>,
    pub last_error: Option<String>,
    pub output_dir: String,
}

pub struct Shared {
    pub config: Mutex<Config>,
    pub status: Mutex<Status>,
    pub enabled: AtomicBool,
    /// Enregistrement manuel demandé (bouton REC) : enregistre même hors vocal.
    /// Transitoire (non persisté) : un redémarrage repart en mode AUTO seul.
    pub force: AtomicBool,
    pub quit: AtomicBool,
}

impl Shared {
    pub fn new(config: Config) -> Self {
        let enabled = config.enabled;
        Self {
            config: Mutex::new(config),
            status: Mutex::new(Status::default()),
            enabled: AtomicBool::new(enabled),
            force: AtomicBool::new(false),
            quit: AtomicBool::new(false),
        }
    }

    pub fn config_snapshot(&self) -> Config {
        self.config.lock().expect("mutex config").clone()
    }

    fn set_error(&self, msg: Option<String>) {
        if let Some(m) = &msg {
            eprintln!(
                "[discord-rec {}] {m}",
                chrono::Local::now().format("%H:%M:%S")
            );
        }
        self.status.lock().expect("mutex status").last_error = msg;
    }
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Choisit la source vidéo pour cette session ; `None` = audio seul.
#[cfg(unix)]
async fn acquire_video(shared: &Shared, cfg: &Config) -> Option<VideoSpec> {
    // 1) Capture directe de la fenêtre Discord via XWayland : aucun
    //    portail, aucune popup. C'est le chemin normal.
    match crate::x11::find_discord_window().await {
        Ok(Some(win)) => {
            shared.set_error(None);
            return Some(VideoSpec::X11Window {
                xid: win.xid,
                framerate: cfg.framerate,
                width: win.width,
                height: win.height,
            });
        }
        Ok(None) => {}
        Err(e) => shared.set_error(Some(format!("recherche fenêtre X11 : {e:#}"))),
    }
    // 2) Repli : portail Wayland (popup au premier choix uniquement).
    match crate::portal::acquire(cfg.restore_token.clone()).await {
        Ok(src) => {
            if src.restore_token != cfg.restore_token {
                let mut locked = shared.config.lock().expect("mutex config");
                locked.restore_token.clone_from(&src.restore_token);
                let _ = crate::config::save(&locked);
            }
            shared.set_error(None);
            Some(VideoSpec::Portal {
                fd: src.fd,
                node_id: src.node_id,
                guard: src.guard,
            })
        }
        Err(e) => {
            // Pas de vidéo (refus, annulation…) : on enregistre l'audio
            // quand même plutôt que de perdre la session.
            shared.set_error(Some(format!("vidéo indisponible ({e:#}) — audio seul")));
            None
        }
    }
}

#[cfg(windows)]
async fn acquire_video(shared: &Shared, cfg: &Config) -> Option<VideoSpec> {
    match tokio::task::spawn_blocking(crate::win::window::find_discord_window).await {
        Ok(Some(win)) => {
            shared.set_error(None);
            Some(VideoSpec::WinWindow {
                hwnd: win.hwnd,
                framerate: cfg.framerate,
                width: win.width,
                height: win.height,
            })
        }
        Ok(None) => {
            shared.set_error(Some("fenêtre Discord introuvable — audio seul".to_owned()));
            None
        }
        Err(e) => {
            shared.set_error(Some(format!("recherche fenêtre Discord : {e:#}")));
            None
        }
    }
}

async fn start_recording(shared: &Shared, snap: &voice::Snapshot) -> Result<Recording> {
    let cfg = shared.config_snapshot();
    std::fs::create_dir_all(&cfg.output_dir).with_context(|| {
        format!(
            "impossible de créer le dossier {}",
            cfg.output_dir.display()
        )
    })?;
    let video = if cfg.video {
        acquire_video(shared, &cfg).await
    } else {
        None
    };

    // Après l'acquisition vidéo (le portail peut attendre l'utilisateur),
    // pour que l'horodatage du fichier corresponde au vrai début.
    let file_name = format!(
        "discord-{}.mkv",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let encoder = recorder::detect_encoder().await;
    // Réduction de bruit micro : seulement si demandée ET disponible. Demandée
    // mais plugin absent → on enregistre quand même, avec une note.
    let denoise = cfg.mic_denoise && recorder::denoise_available().await;
    if cfg.mic_denoise && !denoise {
        shared.set_error(Some(
            "réduction de bruit indisponible (plugin webrtcdsp absent) — micro non filtré"
                .to_owned(),
        ));
    }
    Recording::start(&cfg, &file_name, snap.audio_target, video, encoder, denoise)
}

fn publish(app: &AppHandle, shared: &Shared, snap: &voice::Snapshot, rec: Option<&Recording>) {
    let status = {
        let mut locked = shared.status.lock().expect("mutex status");
        locked.enabled = shared.enabled.load(Ordering::Relaxed);
        locked.forced = shared.force.load(Ordering::Relaxed);
        locked.in_voice = snap.in_voice;
        locked.recording = rec.is_some();
        locked.video_active = rec.is_some_and(|r| r.has_video);
        locked.encoder = rec
            .filter(|r| r.has_video)
            .map(|r| r.encoder.label().to_string());
        locked.file = rec.map(|r| r.file.display().to_string());
        locked.started_at_ms = rec.map(|r| unix_ms(r.started_at));
        locked.output_dir = shared
            .config
            .lock()
            .expect("mutex config")
            .output_dir
            .display()
            .to_string();
        locked.clone()
    };
    let _ = app.emit("status", &status);
}

/// Supprime les enregistrements `discord-*.mkv` du dossier de sortie, sauf
/// `keep` (le nouvel enregistrement qui vient de démarrer). Ne touche JAMAIS
/// aux exports (`.mp4`, clips) ni à un autre fichier : on ne cible que les
/// `.mkv` au nom généré par l'app.
fn prune_old_recordings(keep: &std::path::Path) {
    let (Some(dir), Some(keep_name)) = (keep.parent(), keep.file_name()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == keep_name {
            continue;
        }
        let lossy = name.to_string_lossy();
        let is_recording = lossy.starts_with("discord-")
            && std::path::Path::new(&*lossy)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mkv"));
        if is_recording {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Convertit en tâche de fond le MKV qui vient d'être finalisé en MP4 (remux
/// sans perte, résolution source) si l'option est active. N'affecte jamais le
/// MKV d'origine ; notifie et rafraîchit la liste au succès.
fn spawn_auto_mp4(app: &AppHandle, output_dir: std::path::PathBuf, mkv_name: String) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match crate::convert::to_mp4(&output_dir, &mkv_name, None).await {
            Ok(mp4) => {
                notify(&app, "MP4 prêt", &mp4);
                let _ = app.emit(
                    "recording-saved",
                    output_dir.join(&mp4).display().to_string(),
                );
            }
            Err(e) => {
                eprintln!("[discord-rec] auto-MP4 : {e:#}");
            }
        }
    });
}

/// Applique la rétention aux enregistrements `discord-*.mkv` de `dir`.
///
/// D'abord une purge par âge (les fichiers plus vieux que `max_age`), puis une
/// purge par taille : si le total des survivants dépasse `max_total_bytes`, les
/// PLUS ANCIENS sont supprimés jusqu'à repasser sous le seuil. Ne touche qu'aux
/// `.mkv` générés par l'app (jamais MP4/clips exportés) ni au fichier `keep`
/// (enregistrement en cours). `None` = limite désactivée. `now` est injecté pour
/// la testabilité.
fn prune_by_retention(
    dir: &std::path::Path,
    keep: Option<&std::ffi::OsStr>,
    max_age: Option<Duration>,
    max_total_bytes: Option<u64>,
    now: SystemTime,
) {
    if max_age.is_none() && max_total_bytes.is_none() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // (chemin, mtime, taille) des enregistrements candidats.
    let mut recs: Vec<(std::path::PathBuf, SystemTime, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if keep == Some(name.as_os_str()) {
            continue;
        }
        let lossy = name.to_string_lossy();
        let is_recording = lossy.starts_with("discord-")
            && std::path::Path::new(&*lossy)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mkv"));
        if !is_recording {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta.modified().unwrap_or(now);
        recs.push((entry.path(), mtime, meta.len()));
    }

    // 1) Purge par âge.
    if let Some(max_age) = max_age {
        recs.retain(|(path, mtime, _)| {
            let too_old = now.duration_since(*mtime).is_ok_and(|age| age > max_age);
            if too_old {
                let _ = std::fs::remove_file(path);
            }
            !too_old
        });
    }

    // 2) Purge par taille totale : on garde les plus récents, on supprime les
    //    plus anciens au-delà du budget.
    if let Some(budget) = max_total_bytes {
        recs.sort_by_key(|(_, mtime, _)| std::cmp::Reverse(*mtime));
        let mut used: u64 = 0;
        for (path, _, size) in &recs {
            used = used.saturating_add(*size);
            if used > budget {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Lit les seuils de rétention dans la config et les applique (au démarrage et
/// après chaque enregistrement finalisé).
fn apply_retention(shared: &Shared) {
    let cfg = shared.config_snapshot();
    let max_age = (cfg.retention_days > 0)
        .then(|| Duration::from_secs(u64::from(cfg.retention_days) * 86_400));
    let max_total =
        (cfg.retention_max_gb > 0).then(|| u64::from(cfg.retention_max_gb) * 1_000_000_000);
    prune_by_retention(&cfg.output_dir, None, max_age, max_total, SystemTime::now());
}

pub async fn run(app: AppHandle, shared: Arc<Shared>) {
    // Rétention au démarrage : purge les vieux enregistrements dès le lancement.
    apply_retention(&shared);

    let mut rec: Option<Recording> = None;
    // `true` si l'enregistrement courant a démarré en mode manuel (bouton REC)
    // hors vocal : on l'arrête alors immédiatement à la coupure du bouton, sans
    // l'anti-rebond (réservé aux reconnexions de vocal).
    let mut rec_is_manual = false;
    let mut absent_ticks: u32 = 0;
    let mut cooldown: u32 = 0;
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        if shared.quit.load(Ordering::Relaxed) {
            if let Some(r) = rec.take() {
                r.stop().await;
            }
            app.exit(0);
            return;
        }

        let snap = match voice::snapshot().await {
            Ok(s) => s,
            Err(e) => {
                shared.set_error(Some(format!("détection vocale : {e:#}")));
                voice::Snapshot::default()
            }
        };

        // gst-launch mort tout seul → erreur de pipeline, on nettoie.
        if let Some(r) = rec.as_mut() {
            if let Some(status) = r.exited() {
                shared.set_error(Some(format!(
                    "l'enregistreur s'est arrêté de façon inattendue ({status}) — voir .gstreamer.log"
                )));
                notify(
                    &app,
                    "Enregistrement interrompu",
                    "L'enregistreur s'est arrêté de façon inattendue.",
                );
                rec = None;
                rec_is_manual = false;
                cooldown = RETRY_COOLDOWN_TICKS;
            }
        }

        let enabled = shared.enabled.load(Ordering::Relaxed);
        let force = shared.force.load(Ordering::Relaxed);
        let auto_want = enabled && snap.in_voice;
        // On enregistre si le vocal l'exige (mode AUTO) OU si l'utilisateur l'a
        // forcé (bouton REC).
        let want = auto_want || force;

        if want {
            absent_ticks = 0;
            if rec.is_none() {
                if cooldown > 0 {
                    cooldown -= 1;
                } else {
                    match start_recording(&shared, &snap).await {
                        Ok(r) => {
                            // Option « ne garder que le dernier » : on supprime
                            // les enregistrements précédents (jamais les exports
                            // MP4/clips), maintenant qu'un nouveau a démarré.
                            if shared.config_snapshot().keep_only_last {
                                prune_old_recordings(&r.file);
                            }
                            // Manuel « pur » = forcé alors qu'aucun vocal ne le
                            // justifie : arrêt immédiat au relâchement du bouton.
                            rec_is_manual = force && !auto_want;
                            notify(&app, "Enregistrement démarré", &base_name(&r.file));
                            rec = Some(r);
                        }
                        Err(e) => {
                            shared.set_error(Some(format!("démarrage impossible : {e:#}")));
                            cooldown = RETRY_COOLDOWN_TICKS;
                        }
                    }
                }
            }
        } else if rec.is_some() {
            absent_ticks += 1;
            // Arrêt immédiat si REC manuel relâché ou AUTO désactivé ; sinon
            // anti-rebond sur la sortie de vocal (survit à une reconnexion brève).
            let limit = if rec_is_manual || !enabled {
                0
            } else {
                shared.config_snapshot().stop_debounce_s
            };
            if absent_ticks >= limit {
                if let Some(r) = rec.take() {
                    let file = r.stop().await;
                    rec_is_manual = false;
                    shared.set_error(None);
                    let _ = app.emit("recording-saved", file.display().to_string());
                    notify(&app, "Enregistrement sauvegardé", &base_name(&file));
                    // Auto-conversion MP4 (remux) si demandée : à côté du MKV.
                    let cfg = shared.config_snapshot();
                    if cfg.auto_mp4 {
                        if let Some(name) = file.file_name().and_then(|n| n.to_str()) {
                            spawn_auto_mp4(&app, cfg.output_dir, name.to_owned());
                        }
                    }
                    // Rétention : purge les enregistrements excédentaires.
                    apply_retention(&shared);
                }
                absent_ticks = 0;
            }
        }

        publish(&app, &shared, &snap, rec.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::{prune_by_retention, prune_old_recordings};
    use std::time::{Duration, SystemTime};

    #[test]
    fn prune_keeps_current_and_spares_exports() {
        let dir = std::env::temp_dir().join(format!("disc-rec-prunetest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dossier de test");
        let touch = |name: &str| std::fs::write(dir.join(name), b"x").expect("écriture");

        let keep = dir.join("discord-2026-06-29_10-00-00.mkv");
        touch("discord-2026-06-29_10-00-00.mkv"); // l'enregistrement courant
        touch("discord-2026-06-28_09-00-00.mkv"); // ancien REC → à supprimer
        touch("discord-2026-06-27_08-00-00.mkv"); // ancien REC → à supprimer
        touch("discord-2026-06-28_09-00-00.mp4"); // export MP4 → à garder
        touch("discord-2026-06-28_09-00-00_clip_0-30s.mp4"); // clip → à garder
        touch("notes.txt"); // fichier tiers → à garder

        prune_old_recordings(&keep);

        let exists = |name: &str| dir.join(name).exists();
        assert!(
            exists("discord-2026-06-29_10-00-00.mkv"),
            "courant supprimé"
        );
        assert!(
            !exists("discord-2026-06-28_09-00-00.mkv"),
            "ancien REC gardé"
        );
        assert!(
            !exists("discord-2026-06-27_08-00-00.mkv"),
            "ancien REC gardé"
        );
        assert!(
            exists("discord-2026-06-28_09-00-00.mp4"),
            "export MP4 supprimé"
        );
        assert!(
            exists("discord-2026-06-28_09-00-00_clip_0-30s.mp4"),
            "clip supprimé"
        );
        assert!(exists("notes.txt"), "fichier tiers supprimé");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Crée un fichier **sparse** de taille logique `size` (aucun octet réel
    /// écrit sur le disque, mais `metadata().len()` renvoie `size`) et lui donne
    /// l'horodatage `mtime`. Permet de tester la purge par taille en « Go » sans
    /// écrire des giga-octets.
    fn write_aged(path: &std::path::Path, size: u64, mtime: SystemTime) {
        let f = std::fs::File::create(path).expect("création");
        f.set_len(size).expect("taille logique");
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .expect("mtime");
    }

    #[test]
    fn retention_par_age_et_taille() {
        let dir = std::env::temp_dir().join(format!("disc-rec-rettest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dossier de test");

        let now = SystemTime::now();
        let days = |n: u64| now - Duration::from_secs(n * 86_400);

        // 4 enregistrements d'âges croissants + un export MP4 + un tiers.
        write_aged(&dir.join("discord-a.mkv"), 3_000_000_000, days(0)); // récent, gros
        write_aged(&dir.join("discord-b.mkv"), 3_000_000_000, days(1));
        write_aged(&dir.join("discord-c.mkv"), 3_000_000_000, days(2));
        write_aged(&dir.join("discord-d.mkv"), 3_000_000_000, days(40)); // très vieux
        write_aged(&dir.join("discord-a.mp4"), 10, days(40)); // export → jamais touché
        write_aged(&dir.join("notes.txt"), 10, days(40)); // tiers → jamais touché

        // Âge > 30 j : seul discord-d.mkv part. Puis budget 8 Go sur les 3
        // restants (9 Go) : le plus ancien (c) part → total 6 Go ≤ 8.
        prune_by_retention(
            &dir,
            None,
            Some(Duration::from_secs(30 * 86_400)),
            Some(8_000_000_000),
            now,
        );

        let exists = |name: &str| dir.join(name).exists();
        assert!(exists("discord-a.mkv"), "récent supprimé à tort");
        assert!(exists("discord-b.mkv"), "récent supprimé à tort");
        assert!(!exists("discord-c.mkv"), "plus ancien non purgé (taille)");
        assert!(!exists("discord-d.mkv"), "trop vieux non purgé (âge)");
        assert!(exists("discord-a.mp4"), "export MP4 supprimé à tort");
        assert!(exists("notes.txt"), "fichier tiers supprimé à tort");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_desactivee_ne_touche_rien() {
        let dir = std::env::temp_dir().join(format!("disc-rec-retoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dossier de test");
        write_aged(
            &dir.join("discord-x.mkv"),
            10,
            SystemTime::now() - Duration::from_secs(9_999_999),
        );

        prune_by_retention(&dir, None, None, None, SystemTime::now());

        assert!(
            dir.join("discord-x.mkv").exists(),
            "purge alors que désactivée"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
