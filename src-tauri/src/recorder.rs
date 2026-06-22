//! Pipeline d'enregistrement : un processus `gst-launch-1.0` qui muxe dans un
//! MKV jusqu'à trois flux :
//! - la fenêtre Discord, encodée en H.264 — capture directe X11/`XWayland`
//!   ou portail Wayland (Linux), Windows Graphics Capture (Windows) ;
//! - la sortie audio de Discord (les autres participants), piste Opus 1 —
//!   flux `PipeWire` ciblé (Linux) ou loopback WASAPI par processus (Windows) ;
//! - le micro (source par défaut), piste Opus 2.
//!
//! Arrêt : SIGINT (Linux) ou Ctrl+Break console (Windows) → EOS (`-e`) →
//! MKV finalisé avec son index de seek ; kill en dernier recours (le fichier
//! tronqué reste lisible).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{ensure, Context, Result};
use tokio::process::{Child, Command};

use crate::appimage::CommandAppImageExt;
use crate::config::Config;

const STOP_GRACE: Duration = Duration::from_secs(10);

/// Numéro de fd fixe hérité par gst-launch pour le flux vidéo du portail.
#[cfg(unix)]
const CHILD_VIDEO_FD: i32 = 3;

/// Pas de fenêtre console parasite (`CREATE_NO_WINDOW`) et groupe de
/// processus dédié (`CREATE_NEW_PROCESS_GROUP`) pour pouvoir cibler le
/// Ctrl+Break de l'arrêt propre.
#[cfg(windows)]
const CHILD_CREATION_FLAGS: u32 = 0x0800_0000 | 0x0000_0200;

pub enum VideoSpec {
    /// Capture directe de la fenêtre Discord sous `XWayland` — aucun portail.
    /// `width`/`height` : taille de la fenêtre au démarrage, pour épingler
    /// la résolution du pipeline (un redimensionnement en cours
    /// d'enregistrement est alors absorbé par `videoscale` au lieu de faire
    /// échouer la renégociation de l'encodeur).
    #[cfg(unix)]
    X11Window {
        xid: u64,
        framerate: u32,
        width: u32,
        height: u32,
    },
    /// Flux du portail Wayland (popup au premier choix, jeton ensuite).
    #[cfg(unix)]
    Portal {
        fd: std::os::fd::OwnedFd,
        node_id: u32,
        guard: crate::portal::SessionGuard,
    },
    /// Fenêtre Discord via Windows Graphics Capture (même rôle de
    /// `width`/`height` que pour X11).
    #[cfg(windows)]
    WinWindow {
        hwnd: u64,
        framerate: u32,
        width: u32,
        height: u32,
    },
}

impl VideoSpec {
    /// Résolution épinglée pour la sortie encodée (dimensions paires,
    /// exigées par les encodeurs H.264 en 4:2:0).
    ///
    /// `Option` car la variante Portal (Linux) n'expose pas de taille ;
    /// sous Windows toutes les variantes en ont une et clippy voit un
    /// emballage superflu — faux positif dû au `cfg`.
    #[cfg_attr(windows, allow(clippy::unnecessary_wraps))]
    fn pinned_size(&self) -> Option<(u32, u32)> {
        let (w, h) = match self {
            #[cfg(unix)]
            Self::X11Window { width, height, .. } => (*width, *height),
            #[cfg(unix)]
            Self::Portal { .. } => return None,
            #[cfg(windows)]
            Self::WinWindow { width, height, .. } => (*width, *height),
        };
        Some((w & !1, h & !1))
    }
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

/// Source de la sortie audio de Discord (les autres participants).
#[cfg(unix)]
fn discord_audio_tokens(target: u64) -> Vec<String> {
    vec![
        "pipewiresrc".to_owned(),
        format!("target-object={target}"),
        "do-timestamp=true".to_owned(),
    ]
}

#[cfg(windows)]
fn discord_audio_tokens(pid: u64) -> Vec<String> {
    vec![
        "wasapi2src".to_owned(),
        // Loopback ciblé sur l'arbre de processus Discord (Win10 20H2+).
        "loopback=true".to_owned(),
        "loopback-mode=include-process-tree".to_owned(),
        format!("loopback-target-pid={pid}"),
        "do-timestamp=true".to_owned(),
    ]
}

/// Source micro : périphérique choisi dans les réglages, sinon celui par
/// défaut du système. L'identifiant vient de `mics::list()` et est déjà au
/// format attendu par l'élément source de la plateforme.
#[cfg(unix)]
fn mic_audio_tokens(mic: Option<&str>) -> Vec<String> {
    let mut tokens = vec!["pipewiresrc".to_owned()];
    if let Some(serial) = mic {
        tokens.push(format!("target-object={serial}"));
    }
    tokens.push("do-timestamp=true".to_owned());
    tokens
}

#[cfg(windows)]
fn mic_audio_tokens(mic: Option<&str>) -> Vec<String> {
    let mut tokens = vec!["wasapi2src".to_owned()];
    if let Some(device_token) = mic {
        // Token `device="…"` complet, échappé par gst-device-monitor.
        tokens.push(device_token.to_owned());
    }
    tokens.push("do-timestamp=true".to_owned());
    tokens
}

/// Une source audio et son éventuel filtre de post-traitement, inséré juste
/// après `audioresample` (donc avant l'encodeur Opus ou le mixeur). `filter`
/// vide = aucun traitement. Sert à n'appliquer la réduction de bruit qu'au
/// micro, sans toucher à la sortie Discord.
struct AudioSource {
    src: Vec<String>,
    filter: Vec<String>,
}

/// Filtre de réduction de bruit (`webrtcdsp`) pour le micro. `webrtcdsp`
/// n'accepte que du 8/16/32/48 kHz : on force 48 kHz en amont, puis on
/// reconvertit pour l'encodeur. `echo-cancel=false` évite d'exiger un
/// `webrtcechoprobe` (on ne fait que de la suppression de bruit).
fn denoise_tokens() -> Vec<String> {
    [
        "audio/x-raw,rate=48000",
        "!",
        "webrtcdsp",
        "echo-cancel=false",
        "noise-suppression=true",
        "noise-suppression-level=moderate",
        "high-pass-filter=true",
        "!",
        "audioconvert",
        "!",
        "audioresample",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// Insère, si présent, le filtre de la source juste après `audioresample`.
fn push_filter(args: &mut Vec<String>, filter: &[String]) {
    if !filter.is_empty() {
        args.push("!".into());
        args.extend(filter.iter().cloned());
    }
}

/// Branche audio « pistes séparées » : une source → son propre encodeur Opus
/// → une piste du conteneur. Pratique au montage, mais beaucoup de lecteurs
/// ne lisent que la première piste.
fn audio_branch_separate(args: &mut Vec<String>, source: &AudioSource, bitrate_kbps: u32) {
    args.extend(source.src.iter().cloned());
    for token in ["!", "queue", "!", "audioconvert", "!", "audioresample"] {
        args.push(token.into());
    }
    push_filter(args, &source.filter);
    args.push("!".into());
    args.push("opusenc".into());
    args.push(format!("bitrate={}", u64::from(bitrate_kbps) * 1000));
    for token in ["!", "queue", "!", "mux."] {
        args.push(token.into());
    }
}

/// Branche audio « piste unique mixée » : toutes les sources entrent dans un
/// `audiomixer` → un seul Opus → une piste. Audible dans tous les lecteurs.
fn audio_branch_mixed(args: &mut Vec<String>, sources: &[AudioSource], bitrate_kbps: u32) {
    // Le mixeur d'abord (nommé), suivi de l'encodeur et du mux.
    for token in [
        "audiomixer",
        "name=amix",
        "!",
        "audioconvert",
        "!",
        "opusenc",
    ] {
        args.push(token.into());
    }
    args.push(format!("bitrate={}", u64::from(bitrate_kbps) * 1000));
    for token in ["!", "queue", "!", "mux."] {
        args.push(token.into());
    }
    // Puis chaque source rejoint le mixeur (avec son filtre éventuel).
    for source in sources {
        args.extend(source.src.iter().cloned());
        for token in ["!", "queue", "!", "audioconvert", "!", "audioresample"] {
            args.push(token.into());
        }
        push_filter(args, &source.filter);
        args.push("!".into());
        args.push("amix.".into());
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

/// Localise un outil `GStreamer` (`gst-launch-1.0`, `gst-inspect-1.0`).
///
/// Sous Windows, l'installeur officiel ne met pas son dossier `bin` dans le
/// PATH : on regarde la variable d'environnement qu'il pose, puis le chemin
/// d'installation par défaut, avant de retomber sur le PATH.
pub(crate) fn gst_tool(name: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let exe = format!("{name}.exe");
        let roots = [
            std::env::var_os("GSTREAMER_1_0_ROOT_MSVC_X86_64"),
            std::env::var_os("GSTREAMER_1_0_ROOT_MINGW_X86_64"),
            std::env::var_os("GSTREAMER_1_0_ROOT_X86_64"),
            Some(std::ffi::OsString::from(r"C:\gstreamer\1.0\msvc_x86_64")),
            Some(std::ffi::OsString::from(r"C:\gstreamer\1.0\mingw_x86_64")),
        ];
        for root in roots.into_iter().flatten() {
            let candidate = std::path::Path::new(&root).join("bin").join(&exe);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    std::path::PathBuf::from(name)
}

/// `true` si l'élément `GStreamer` nommé est installé (`gst-inspect --exists`).
pub(crate) async fn element_exists(name: &str) -> bool {
    Command::new(gst_tool("gst-inspect-1.0"))
        .strip_appimage_env()
        .args(["--exists", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Détecte le meilleur encodeur disponible en interrogeant `GStreamer`.
/// Le résultat dépend de la machine, pas de la session : il pourrait être
/// mis en cache, mais l'appel (~50 ms) au démarrage d'un enregistrement
/// reste négligeable et suit les installations/désinstallations de plugins.
pub async fn detect_encoder() -> VideoEncoder {
    for &(element, encoder) in ENCODER_CANDIDATES {
        if element_exists(element).await {
            return encoder;
        }
    }
    VideoEncoder::X264
}

/// `true` si la réduction de bruit micro est possible (plugin `webrtcdsp`,
/// dans `gst-plugins-bad`). Sinon le micro est enregistré sans filtre.
pub async fn denoise_available() -> bool {
    element_exists("webrtcdsp").await
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

    pub(crate) fn push_args(self, args: &mut Vec<String>, bitrate_kbps: u32) {
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
        VideoSpec::X11Window { xid, framerate, .. } => {
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
        VideoSpec::WinWindow {
            hwnd, framerate, ..
        } => {
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
    // Résolution épinglée : si la fenêtre est redimensionnée en cours
    // d'enregistrement, videoscale absorbe le changement de caps au lieu de
    // le propager à l'encodeur (qui échouerait).
    //
    // `pixel-aspect-ratio=1/1` est OBLIGATOIRE : la taille épinglée est
    // arrondie au pair (`w & !1`), donc dès que la fenêtre a une dimension
    // impaire (cas courant hors plein écran), videoscale doit changer le
    // ratio source→cible et calcule un PAR non carré — qui déborde ici en
    // valeur négative aberrante (ex. -842/121). nvh264enc grave ce PAR
    // invalide dans la VUI de la SPS H.264 → tous les décodeurs rejettent la
    // piste vidéo (« not-negotiated ») alors que l'audio reste lisible. En
    // forçant des pixels carrés on évite le bug ; l'écart d'un pixel sur le
    // ratio d'affichage est imperceptible.
    if let Some((width, height)) = spec.pinned_size() {
        args.push("videoscale".into());
        args.push("!".into());
        args.push(format!(
            "video/x-raw,width={width},height={height},pixel-aspect-ratio=1/1"
        ));
        args.push("!".into());
    }
    encoder.push_args(args, bitrate_kbps);
    for token in ["!", "h264parse", "!", "queue", "!", "mux."] {
        args.push(token.into());
    }
}

fn mux_tokens(args: &mut Vec<String>, file_name: &str) {
    args.push("matroskamux".into());
    args.push("name=mux".into());
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
        denoise: bool,
    ) -> Result<Self> {
        ensure!(
            audio_target.is_some() || video.is_some(),
            "rien à enregistrer (aucun flux Discord trouvé)"
        );

        let mut args: Vec<String> = vec!["-e".into()];
        if let Some(spec) = &video {
            video_branch(&mut args, spec, encoder, cfg.video_bitrate_kbps);
        }
        // Sources audio : sortie Discord (si trouvée), sans traitement, + micro,
        // avec la réduction de bruit le cas échéant (sur le micro uniquement).
        let mut sources: Vec<AudioSource> = Vec::new();
        if let Some(target) = audio_target {
            sources.push(AudioSource {
                src: discord_audio_tokens(target),
                filter: Vec::new(),
            });
        }
        sources.push(AudioSource {
            src: mic_audio_tokens(cfg.mic_target.as_deref()),
            filter: if denoise {
                denoise_tokens()
            } else {
                Vec::new()
            },
        });

        if cfg.mix_audio {
            audio_branch_mixed(&mut args, &sources, cfg.audio_bitrate_kbps);
        } else {
            for source in &sources {
                audio_branch_separate(&mut args, source, cfg.audio_bitrate_kbps);
            }
        }
        mux_tokens(&mut args, file_name);

        // gst-launch écrit ses messages d'erreur sur stdout : on journalise
        // les deux flux dans le même fichier.
        let log = std::fs::File::create(cfg.output_dir.join(".gstreamer.log"))
            .context("impossible de créer le journal gstreamer")?;
        let log_err = log.try_clone().context("clonage du journal gstreamer")?;

        let mut cmd = Command::new(gst_tool("gst-launch-1.0"));
        cmd.strip_appimage_env()
            .args(&args)
            .current_dir(&cfg.output_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true);

        #[cfg(windows)]
        cmd.creation_flags(CHILD_CREATION_FLAGS);

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

        let child = cmd
            .spawn()
            .context("impossible de lancer gst-launch-1.0 (runtime GStreamer installé ?)")?;

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
            // Ctrl+Break → EOS → MKV finalisé (index de seek écrit) ;
            // kill seulement si l'arrêt propre échoue ou traîne.
            let graceful = self
                .child
                .id()
                .is_some_and(crate::win::console::send_ctrl_break);
            if !graceful
                || tokio::time::timeout(STOP_GRACE, self.child.wait())
                    .await
                    .is_err()
            {
                let _ = self.child.kill().await;
                let _ = tokio::time::timeout(STOP_GRACE, self.child.wait()).await;
            }
        }
        self.file
    }
}
