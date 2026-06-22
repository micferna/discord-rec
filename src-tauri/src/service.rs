//! Boucle de surveillance : interroge l'état vocal de Discord chaque seconde,
//! démarre l'enregistrement à l'entrée en vocal, l'arrête (avec anti-rebond)
//! à la sortie, et publie l'état vers l'interface.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::config::Config;
use crate::recorder::{self, Recording, VideoSpec};
use crate::voice;

/// Ticks de pause après un échec de démarrage (laisse le temps à la cause
/// de disparaître sans spammer le portail en cas de repli Wayland).
const RETRY_COOLDOWN_TICKS: u32 = 10;
const TICK: Duration = Duration::from_secs(1);

#[derive(Clone, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    pub in_voice: bool,
    pub recording: bool,
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
    pub quit: AtomicBool,
}

impl Shared {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            status: Mutex::new(Status::default()),
            enabled: AtomicBool::new(true),
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

pub async fn run(app: AppHandle, shared: Arc<Shared>) {
    let mut rec: Option<Recording> = None;
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
                rec = None;
                cooldown = RETRY_COOLDOWN_TICKS;
            }
        }

        let enabled = shared.enabled.load(Ordering::Relaxed);
        let want = enabled && snap.in_voice;

        if want {
            absent_ticks = 0;
            if rec.is_none() {
                if cooldown > 0 {
                    cooldown -= 1;
                } else {
                    match start_recording(&shared, &snap).await {
                        Ok(r) => rec = Some(r),
                        Err(e) => {
                            shared.set_error(Some(format!("démarrage impossible : {e:#}")));
                            cooldown = RETRY_COOLDOWN_TICKS;
                        }
                    }
                }
            }
        } else if rec.is_some() {
            absent_ticks += 1;
            let limit = if enabled {
                shared.config_snapshot().stop_debounce_s
            } else {
                0 // désactivation manuelle : arrêt immédiat
            };
            if absent_ticks >= limit {
                if let Some(r) = rec.take() {
                    let file = r.stop().await;
                    shared.set_error(None);
                    let _ = app.emit("recording-saved", file.display().to_string());
                }
                absent_ticks = 0;
            }
        }

        publish(&app, &shared, &snap, rec.as_ref());
    }
}
