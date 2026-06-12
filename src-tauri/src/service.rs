//! Boucle de surveillance : interroge `PipeWire` chaque seconde, démarre
//! l'enregistrement à l'entrée en vocal, l'arrête (avec anti-rebond) à la
//! sortie, et publie l'état vers l'interface.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::config::{self, Config};
use crate::portal;
use crate::pw;
use crate::recorder::{Recording, VideoInput};

/// Ticks de pause après un échec de démarrage, pour ne pas spammer le portail.
const RETRY_COOLDOWN_TICKS: u32 = 30;
const TICK: Duration = Duration::from_secs(1);

#[derive(Clone, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    pub in_voice: bool,
    pub recording: bool,
    pub video_active: bool,
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
        self.status.lock().expect("mutex status").last_error = msg;
    }
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

async fn start_recording(shared: &Shared, snap: &pw::Snapshot) -> Result<Recording> {
    let cfg = shared.config_snapshot();
    std::fs::create_dir_all(&cfg.output_dir).with_context(|| {
        format!(
            "impossible de créer le dossier {}",
            cfg.output_dir.display()
        )
    })?;
    let file_name = format!(
        "discord-{}.mkv",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );

    let mut video = None;
    if cfg.video {
        match portal::acquire(cfg.restore_token.clone()).await {
            Ok(src) => {
                if src.restore_token != cfg.restore_token {
                    let mut locked = shared.config.lock().expect("mutex config");
                    locked.restore_token.clone_from(&src.restore_token);
                    let _ = config::save(&locked);
                }
                video = Some(VideoInput {
                    fd: src.fd,
                    node_id: src.node_id,
                    guard: src.guard,
                });
                shared.set_error(None);
            }
            Err(e) => {
                // Pas de vidéo (refus, annulation…) : on enregistre l'audio
                // quand même plutôt que de perdre la session.
                shared.set_error(Some(format!("vidéo indisponible ({e:#}) — audio seul")));
            }
        }
    }

    Recording::start(&cfg, &file_name, snap.discord_out_serial, video)
}

fn publish(app: &AppHandle, shared: &Shared, snap: &pw::Snapshot, rec: Option<&Recording>) {
    let status = {
        let mut locked = shared.status.lock().expect("mutex status");
        locked.enabled = shared.enabled.load(Ordering::Relaxed);
        locked.in_voice = snap.in_voice;
        locked.recording = rec.is_some();
        locked.video_active = rec.is_some_and(|r| r.has_video);
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

        let snap = match pw::snapshot().await {
            Ok(s) => s,
            Err(e) => {
                shared.set_error(Some(format!("PipeWire : {e:#}")));
                pw::Snapshot::default()
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
