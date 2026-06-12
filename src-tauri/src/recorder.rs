//! Pipeline d'enregistrement : un processus `gst-launch-1.0` qui muxe dans un
//! MKV jusqu'à trois flux :
//! - la fenêtre Discord — en direct via X11/XWayland (`ximagesrc`, aucun
//!   portail) ou, à défaut, via le portail Wayland — encodée en H.264 ;
//! - la sortie audio de Discord (les autres participants), piste Opus 1 ;
//! - le micro (source par défaut), piste Opus 2.
//!
//! L'arrêt envoie SIGINT : avec `-e`, gst-launch convertit le signal en EOS et
//! finalise le fichier (index Matroska écrit). SIGKILL en dernier recours —
//! le MKV reste lisible même tronqué.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{ensure, Context, Result};
use tokio::process::{Child, Command};

use crate::config::Config;
use crate::portal::SessionGuard;

/// Numéro de fd fixe hérité par gst-launch pour le flux vidéo du portail.
const CHILD_VIDEO_FD: i32 = 3;
const STOP_GRACE: Duration = Duration::from_secs(10);

pub enum VideoSpec {
    /// Capture directe de la fenêtre Discord sous `XWayland` — aucun portail.
    X11Window { xid: u64, framerate: u32 },
    /// Flux du portail Wayland (popup au premier choix, jeton ensuite).
    Portal {
        fd: OwnedFd,
        node_id: u32,
        guard: SessionGuard,
    },
}

pub struct Recording {
    child: Child,
    pub file: PathBuf,
    pub has_video: bool,
    pub started_at: SystemTime,
    // Conservés vivants pendant tout l'enregistrement.
    _video_fd: Option<OwnedFd>,
    _portal_session: Option<SessionGuard>,
}

fn audio_branch(args: &mut Vec<String>, target_serial: Option<u64>, bitrate_kbps: u32) {
    args.push("pipewiresrc".into());
    if let Some(serial) = target_serial {
        args.push(format!("target-object={serial}"));
    }
    args.push("do-timestamp=true".into());
    for token in [
        "!",
        "queue",
        "!",
        "audioconvert",
        "!",
        "audioresample",
        "!",
        "opusenc",
    ] {
        args.push(token.into());
    }
    args.push(format!("bitrate={}", u64::from(bitrate_kbps) * 1000));
    for token in ["!", "queue", "!", "mux."] {
        args.push(token.into());
    }
}

fn video_branch(args: &mut Vec<String>, spec: &VideoSpec, bitrate_kbps: u32) {
    match spec {
        VideoSpec::X11Window { xid, framerate } => {
            args.push("ximagesrc".into());
            args.push(format!("xid={xid}"));
            args.push("use-damage=0".into());
            args.push("!".into());
            args.push(format!("video/x-raw,framerate={framerate}/1"));
        }
        VideoSpec::Portal { node_id, .. } => {
            args.push("pipewiresrc".into());
            args.push(format!("fd={CHILD_VIDEO_FD}"));
            args.push(format!("path={node_id}"));
            args.push("do-timestamp=true".into());
        }
    }
    for token in ["!", "queue", "!", "videoconvert", "!", "x264enc"] {
        args.push(token.into());
    }
    args.push(format!("bitrate={bitrate_kbps}"));
    for token in [
        "speed-preset=veryfast",
        "tune=zerolatency",
        "key-int-max=120",
        "!",
        "h264parse",
        "!",
        "queue",
        "!",
        "mux.",
    ] {
        args.push(token.into());
    }
}

impl Recording {
    /// Démarre gst-launch dans `cfg.output_dir` (le nom de fichier est relatif,
    /// généré par nous : aucun problème d'échappement, pas de shell).
    pub fn start(
        cfg: &Config,
        file_name: &str,
        discord_out_serial: Option<u64>,
        video: Option<VideoSpec>,
    ) -> Result<Self> {
        ensure!(
            discord_out_serial.is_some() || video.is_some(),
            "rien à enregistrer (aucun flux Discord trouvé)"
        );

        let mut args: Vec<String> = vec!["-e".into()];
        if let Some(spec) = &video {
            video_branch(&mut args, spec, cfg.video_bitrate_kbps);
        }
        if let Some(serial) = discord_out_serial {
            audio_branch(&mut args, Some(serial), cfg.audio_bitrate_kbps);
        }
        // Micro : source par défaut (pas de cible explicite).
        audio_branch(&mut args, None, cfg.audio_bitrate_kbps);
        args.push("matroskamux".into());
        args.push("name=mux".into());
        args.push("!".into());
        args.push("filesink".into());
        args.push(format!("location={file_name}"));

        // gst-launch écrit ses messages d'erreur sur stdout : on journalise
        // les deux flux dans le même fichier.
        let log = std::fs::File::create(cfg.output_dir.join(".gstreamer.log"))
            .context("impossible de créer le journal gstreamer")?;
        let log_err = log.try_clone().context("clonage du journal gstreamer")?;

        let mut cmd = Command::new("gst-launch-1.0");
        cmd.args(&args)
            .current_dir(&cfg.output_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true);

        if let Some(VideoSpec::Portal { fd, .. }) = &video {
            let raw = fd.as_raw_fd();
            // SAFETY: dup2/fcntl sont async-signal-safe, autorisés dans pre_exec.
            unsafe {
                cmd.pre_exec(move || {
                    if raw == CHILD_VIDEO_FD {
                        // Déjà au bon numéro : il suffit de retirer CLOEXEC.
                        let flags = libc::fcntl(raw, libc::F_GETFD);
                        if flags == -1
                            || libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else if libc::dup2(raw, CHILD_VIDEO_FD) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let child = cmd.spawn().context("impossible de lancer gst-launch-1.0")?;
        let has_video = video.is_some();
        let (video_fd, portal_session) = match video {
            Some(VideoSpec::Portal { fd, guard, .. }) => (Some(fd), Some(guard)),
            Some(VideoSpec::X11Window { .. }) | None => (None, None),
        };
        Ok(Self {
            child,
            file: cfg.output_dir.join(file_name),
            has_video,
            started_at: SystemTime::now(),
            _video_fd: video_fd,
            _portal_session: portal_session,
        })
    }

    /// `Some(status)` si gst-launch est mort tout seul (erreur de pipeline).
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Arrêt propre : SIGINT → EOS → finalisation du MKV ; SIGKILL au-delà
    /// du délai de grâce.
    pub async fn stop(mut self) -> PathBuf {
        if let Some(pid) = self.child.id() {
            // SAFETY: simple envoi de signal au processus enfant.
            unsafe {
                libc::kill(pid.cast_signed(), libc::SIGINT);
            }
            if tokio::time::timeout(STOP_GRACE, self.child.wait())
                .await
                .is_err()
            {
                let _ = self.child.kill().await;
            }
        }
        self.file
    }
}
