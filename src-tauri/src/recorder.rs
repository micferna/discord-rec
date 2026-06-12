//! Pipeline d'enregistrement : un processus `gst-launch-1.0` qui muxe dans un
//! MKV jusqu'à trois flux :
//! - la fenêtre Discord, encodée en H.264 — capture directe X11/`XWayland`
//!   ou portail Wayland (Linux), Windows Graphics Capture (Windows) ;
//! - la sortie audio de Discord (les autres participants), piste Opus 1 —
//!   flux `PipeWire` ciblé (Linux) ou loopback WASAPI par processus (Windows) ;
//! - le micro (source par défaut), piste Opus 2.
//!
//! Arrêt sous Linux : SIGINT → EOS (`-e`) → MKV finalisé, SIGKILL en dernier
//! recours. Sous Windows : mux en mode `streamable` (lisible sans
//! finalisation) puis arrêt du processus.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{ensure, Context, Result};
use tokio::process::{Child, Command};

use crate::config::Config;

const STOP_GRACE: Duration = Duration::from_secs(10);

/// Numéro de fd fixe hérité par gst-launch pour le flux vidéo du portail.
#[cfg(unix)]
const CHILD_VIDEO_FD: i32 = 3;

/// Pas de fenêtre console parasite pour le processus gst (`CREATE_NO_WINDOW`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub enum VideoSpec {
    /// Capture directe de la fenêtre Discord sous `XWayland` — aucun portail.
    #[cfg(unix)]
    X11Window { xid: u64, framerate: u32 },
    /// Flux du portail Wayland (popup au premier choix, jeton ensuite).
    #[cfg(unix)]
    Portal {
        fd: std::os::fd::OwnedFd,
        node_id: u32,
        guard: crate::portal::SessionGuard,
    },
    /// Fenêtre Discord via Windows Graphics Capture.
    #[cfg(windows)]
    WinWindow { hwnd: u64, framerate: u32 },
}

pub struct Recording {
    child: Child,
    pub file: PathBuf,
    pub has_video: bool,
    pub encoder: VideoEncoder,
    pub started_at: SystemTime,
    // Conservés vivants pendant tout l'enregistrement.
    #[cfg(unix)]
    _video_fd: Option<std::os::fd::OwnedFd>,
    #[cfg(unix)]
    _portal_session: Option<crate::portal::SessionGuard>,
    /// Tue gst si l'app disparaît sans passer par `stop()` (kill-on-close).
    #[cfg(windows)]
    _job: Option<crate::win::job::JobHandle>,
}

/// Élément source + propriétés pour capturer la sortie audio de Discord
/// (`target` renseigné) ou le micro par défaut (`target` vide).
#[cfg(unix)]
fn audio_source_tokens(target: Option<u64>) -> Vec<String> {
    let mut tokens = vec!["pipewiresrc".to_owned()];
    if let Some(serial) = target {
        tokens.push(format!("target-object={serial}"));
    }
    tokens.push("do-timestamp=true".to_owned());
    tokens
}

#[cfg(windows)]
fn audio_source_tokens(target: Option<u64>) -> Vec<String> {
    let mut tokens = vec!["wasapi2src".to_owned()];
    if let Some(pid) = target {
        // Loopback ciblé sur l'arbre de processus Discord (Win10 20H2+).
        tokens.push("loopback=true".to_owned());
        tokens.push("loopback-mode=include-process-tree".to_owned());
        tokens.push(format!("loopback-target-pid={pid}"));
    }
    tokens.push("do-timestamp=true".to_owned());
    tokens
}

fn audio_branch(args: &mut Vec<String>, target: Option<u64>, bitrate_kbps: u32) {
    args.extend(audio_source_tokens(target));
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

/// Encodeur H.264 à utiliser, du plus léger au plus universel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoder {
    /// NVENC (GPU NVIDIA) : charge CPU quasi nulle.
    Nvenc,
    /// VA-API (GPU Intel/AMD) : charge CPU quasi nulle.
    #[cfg(unix)]
    Vaapi,
    /// Quick Sync (GPU Intel).
    #[cfg(windows)]
    Qsv,
    /// AMF (GPU AMD).
    #[cfg(windows)]
    Amf,
    /// Media Foundation : encodeur système Windows (matériel si dispo).
    #[cfg(windows)]
    MediaFoundation,
    /// x264 logiciel : disponible partout, coûteux en 4K.
    X264,
}

#[cfg(unix)]
const ENCODER_CANDIDATES: &[(&str, VideoEncoder)] = &[
    ("nvh264enc", VideoEncoder::Nvenc),
    ("vah264enc", VideoEncoder::Vaapi),
];

#[cfg(windows)]
const ENCODER_CANDIDATES: &[(&str, VideoEncoder)] = &[
    ("nvh264enc", VideoEncoder::Nvenc),
    ("qsvh264enc", VideoEncoder::Qsv),
    ("amfh264enc", VideoEncoder::Amf),
    ("mfh264enc", VideoEncoder::MediaFoundation),
];

/// Détecte le meilleur encodeur disponible en interrogeant `GStreamer`.
/// Le résultat dépend de la machine, pas de la session : il pourrait être
/// mis en cache, mais l'appel (~50 ms) au démarrage d'un enregistrement
/// reste négligeable et suit les installations/désinstallations de plugins.
pub async fn detect_encoder() -> VideoEncoder {
    for &(element, encoder) in ENCODER_CANDIDATES {
        let found = Command::new("gst-inspect-1.0")
            .args(["--exists", element])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success());
        if found {
            return encoder;
        }
    }
    VideoEncoder::X264
}

impl VideoEncoder {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nvenc => "NVENC (GPU NVIDIA)",
            #[cfg(unix)]
            Self::Vaapi => "VA-API (GPU)",
            #[cfg(windows)]
            Self::Qsv => "Quick Sync (GPU Intel)",
            #[cfg(windows)]
            Self::Amf => "AMF (GPU AMD)",
            #[cfg(windows)]
            Self::MediaFoundation => "Media Foundation",
            Self::X264 => "x264 (CPU)",
        }
    }

    fn push_args(self, args: &mut Vec<String>, bitrate_kbps: u32) {
        match self {
            Self::Nvenc => {
                args.push("nvh264enc".into());
                args.push(format!("bitrate={bitrate_kbps}"));
                args.push("gop-size=120".into());
            }
            #[cfg(unix)]
            Self::Vaapi => {
                args.push("vah264enc".into());
                args.push(format!("bitrate={bitrate_kbps}"));
                args.push("key-int-max=120".into());
            }
            #[cfg(windows)]
            Self::Qsv => {
                args.push("qsvh264enc".into());
                args.push(format!("bitrate={bitrate_kbps}"));
                args.push("gop-size=120".into());
            }
            #[cfg(windows)]
            Self::Amf => {
                args.push("amfh264enc".into());
                args.push(format!("bitrate={bitrate_kbps}"));
                args.push("gop-size=120".into());
            }
            #[cfg(windows)]
            Self::MediaFoundation => {
                args.push("mfh264enc".into());
                args.push(format!("bitrate={bitrate_kbps}"));
            }
            Self::X264 => {
                args.push("x264enc".into());
                args.push(format!("bitrate={bitrate_kbps}"));
                args.push("speed-preset=veryfast".into());
                args.push("tune=zerolatency".into());
                args.push("key-int-max=120".into());
            }
        }
    }
}

fn video_branch(
    args: &mut Vec<String>,
    spec: &VideoSpec,
    encoder: VideoEncoder,
    bitrate_kbps: u32,
) {
    match spec {
        #[cfg(unix)]
        VideoSpec::X11Window { xid, framerate } => {
            args.push("ximagesrc".into());
            args.push(format!("xid={xid}"));
            args.push("use-damage=0".into());
            args.push("!".into());
            args.push(format!("video/x-raw,framerate={framerate}/1"));
        }
        #[cfg(unix)]
        VideoSpec::Portal { node_id, .. } => {
            args.push("pipewiresrc".into());
            args.push(format!("fd={CHILD_VIDEO_FD}"));
            args.push(format!("path={node_id}"));
            args.push("do-timestamp=true".into());
        }
        #[cfg(windows)]
        VideoSpec::WinWindow { hwnd, framerate } => {
            // Windows Graphics Capture d'une fenêtre précise (gst ≥ 1.22).
            args.push("d3d11screencapturesrc".into());
            args.push(format!("window-handle={hwnd}"));
            args.push("show-cursor=true".into());
            args.push("!".into());
            args.push(format!("video/x-raw,framerate={framerate}/1"));
            args.push("!".into());
            args.push("d3d11download".into());
        }
    }
    for token in ["!", "queue", "!", "videoconvert", "!"] {
        args.push(token.into());
    }
    encoder.push_args(args, bitrate_kbps);
    for token in ["!", "h264parse", "!", "queue", "!", "mux."] {
        args.push(token.into());
    }
}

fn mux_tokens(args: &mut Vec<String>, file_name: &str) {
    args.push("matroskamux".into());
    args.push("name=mux".into());
    // Sans SIGINT sous Windows, le fichier doit rester lisible sans
    // finalisation : matroska « streamable » n'exige pas d'index final.
    #[cfg(windows)]
    args.push("streamable=true".into());
    args.push("!".into());
    args.push("filesink".into());
    args.push(format!("location={file_name}"));
}

impl Recording {
    /// Démarre gst-launch dans `cfg.output_dir` (le nom de fichier est relatif,
    /// généré par nous : aucun problème d'échappement, pas de shell).
    pub fn start(
        cfg: &Config,
        file_name: &str,
        audio_target: Option<u64>,
        video: Option<VideoSpec>,
        encoder: VideoEncoder,
    ) -> Result<Self> {
        ensure!(
            audio_target.is_some() || video.is_some(),
            "rien à enregistrer (aucun flux Discord trouvé)"
        );

        let mut args: Vec<String> = vec!["-e".into()];
        if let Some(spec) = &video {
            video_branch(&mut args, spec, encoder, cfg.video_bitrate_kbps);
        }
        if let Some(target) = audio_target {
            audio_branch(&mut args, Some(target), cfg.audio_bitrate_kbps);
        }
        // Micro : source par défaut (pas de cible explicite).
        audio_branch(&mut args, None, cfg.audio_bitrate_kbps);
        mux_tokens(&mut args, file_name);

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

        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let portal_raw_fd = match &video {
                Some(VideoSpec::Portal { fd, .. }) => Some(fd.as_raw_fd()),
                _ => None,
            };
            // SAFETY: prctl/dup2/fcntl sont async-signal-safe, autorisés dans
            // pre_exec.
            unsafe {
                cmd.pre_exec(move || {
                    // Si l'app meurt sans passer par stop(), gst reçoit SIGINT
                    // (→ EOS → MKV finalisé) au lieu de tourner orphelin.
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(raw) = portal_raw_fd {
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
                    }
                    Ok(())
                });
            }
        }

        let child = cmd.spawn().context(
            "impossible de lancer gst-launch-1.0 (GStreamer installé et dans le PATH ?)",
        )?;

        #[cfg(windows)]
        let job = child
            .id()
            .and_then(|pid| crate::win::job::kill_on_close(pid).ok());

        let has_video = video.is_some();
        #[cfg(unix)]
        let (video_fd, portal_session) = match video {
            Some(VideoSpec::Portal { fd, guard, .. }) => (Some(fd), Some(guard)),
            _ => (None, None),
        };
        Ok(Self {
            child,
            file: cfg.output_dir.join(file_name),
            has_video,
            encoder,
            started_at: SystemTime::now(),
            #[cfg(unix)]
            _video_fd: video_fd,
            #[cfg(unix)]
            _portal_session: portal_session,
            #[cfg(windows)]
            _job: job,
        })
    }

    /// `Some(status)` si gst-launch est mort tout seul (erreur de pipeline).
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Arrêt propre. Linux : SIGINT → EOS → finalisation du MKV, SIGKILL
    /// au-delà du délai de grâce. Windows : arrêt direct (le mux streamable
    /// garde le fichier lisible).
    pub async fn stop(mut self) -> PathBuf {
        #[cfg(unix)]
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
        #[cfg(windows)]
        {
            let _ = self.child.kill().await;
            let _ = tokio::time::timeout(STOP_GRACE, self.child.wait()).await;
        }
        self.file
    }
}
