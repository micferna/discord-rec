//! Conversion à la demande d'un enregistrement MKV en MP4 (H.264 + AAC).
//!
//! Tout passe par `gst-launch-1.0` : aucune dépendance hors `GStreamer`, déjà
//! requis pour enregistrer.
//! - La vidéo H.264 est **copiée** telle quelle quand on garde la résolution
//!   (rapide, sans perte), ou décodée → `videoscale` → réencodée pour la
//!   réduire (jamais d'agrandissement).
//! - Chaque piste audio Opus est transcodée en **AAC** : contrairement à Opus
//!   dans un MP4, l'AAC est lu par tous les lecteurs et éditeurs.
//!
//! Le `.mkv` d'origine est conservé.
//!
//! Pièges gérés :
//! - `matroskademux` est mono-thread et `qtmux` attend une frame sur chaque
//!   piste avant de démarrer : sur un transcodage, la branche vidéo (réencodage)
//!   tarde, la file audio sature et bloque le démuxage. D'où des files
//!   « illimitées » (`BIG_QUEUE`) qui découplent les branches.
//! - largeur recalculée arrondie au pair + `pixel-aspect-ratio=1/1` (même
//!   raison qu'à l'enregistrement : un PAR non carré casse la VUI H.264).

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::appimage::CommandAppImageExt;
use crate::recorder::{self, gst_tool, VideoEncoder};

/// Nombre maximal de pistes audio sondées.
const MAX_AUDIO_TRACKS: u32 = 16;

/// Files de découplage « sans limite » : indispensables au transcodage
/// (cf. interblocage démuxeur mono-thread / muxeur ci-dessus). L'audio n'y
/// tient que des paquets Opus compressés (quelques Mo, même pour des heures).
const BIG_QUEUE: &[&str] = &[
    "queue",
    "max-size-time=0",
    "max-size-bytes=0",
    "max-size-buffers=0",
];

struct Probe {
    has_video: bool,
    video_dims: Option<(u32, u32)>,
    audio_tracks: u32,
}

/// `gst-launch` dans le dossier de sortie : les fichiers sont désignés en
/// **relatif** (noms ASCII générés par nous), ce qui évite tout problème
/// d'échappement d'un dossier au chemin accentué.
fn gst_cmd(output_dir: &Path) -> Command {
    let mut cmd = Command::new(gst_tool("gst-launch-1.0"));
    cmd.strip_appimage_env()
        .current_dir(output_dir)
        .stdin(Stdio::null());
    cmd
}

/// Préfixe de sonde : lecture bornée (~4 Mo) — l'en-tête `Tracks` est en début
/// de fichier, donc tous les pads sont exposés sans lire tout l'enregistrement.
fn probe_src(input_name: &str) -> Vec<String> {
    vec![
        "filesrc".into(),
        format!("location={input_name}"),
        "blocksize=65536".into(),
        "num-buffers=64".into(),
        "!".into(),
        "matroskademux".into(),
        "name=d".into(),
    ]
}

/// Première largeur/hauteur des caps verbeuses (`width=(int)1280`, …).
fn parse_dims(text: &str) -> Option<(u32, u32)> {
    let grab = |key: &str| -> Option<u32> {
        text.split(key)
            .nth(1)?
            .trim_start_matches("(int)")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    Some((grab("width=")?, grab("height=")?))
}

async fn probe(output_dir: &Path, input_name: &str) -> Result<Probe> {
    // Vidéo : lier d.video_0 ; succès ⇒ piste présente, et on lit ses
    // dimensions dans les caps verbeuses.
    let mut vargs = vec!["-v".to_owned()];
    vargs.extend(probe_src(input_name));
    vargs.extend(
        ["d.video_0", "!", "fakesink"]
            .iter()
            .map(|s| (*s).to_owned()),
    );
    let vout = gst_cmd(output_dir)
        .args(&vargs)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .context("sonde vidéo")?;
    let has_video = vout.status.success();
    let video_dims = has_video
        .then(|| parse_dims(&String::from_utf8_lossy(&vout.stdout)))
        .flatten();

    // Audio : lier d.audio_N tant que ça réussit (sortie 0 = pad présent).
    let mut audio_tracks = 0;
    while audio_tracks < MAX_AUDIO_TRACKS {
        let mut aargs = probe_src(input_name);
        aargs.push(format!("d.audio_{audio_tracks}"));
        aargs.push("!".into());
        aargs.push("fakesink".into());
        let ok = gst_cmd(output_dir)
            .args(&aargs)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success());
        if !ok {
            break;
        }
        audio_tracks += 1;
    }

    if !has_video && audio_tracks == 0 {
        bail!("aucune piste lisible (fichier non MKV ou corrompu ?)");
    }
    Ok(Probe {
        has_video,
        video_dims,
        audio_tracks,
    })
}

/// Largeur préservant le ratio pour une hauteur cible, arrondie au pair
/// (exigé par H.264 4:2:0).
fn scaled_width(src_w: u32, src_h: u32, target_h: u32) -> u32 {
    if src_h == 0 {
        return target_h & !1;
    }
    let w = u64::from(src_w) * u64::from(target_h) / u64::from(src_h);
    (u32::try_from(w).unwrap_or(target_h) & !1).max(2)
}

/// Bitrate vidéo (kb/s) au réencodage, selon la hauteur cible.
fn rescale_bitrate_kbps(height: u32) -> u32 {
    match height {
        0..=480 => 2500,
        481..=720 => 5000,
        721..=1080 => 8000,
        _ => 12000,
    }
}

fn push_big_queue(args: &mut Vec<String>) {
    args.extend(BIG_QUEUE.iter().map(|s| (*s).to_owned()));
}

/// Choisit un encodeur AAC disponible (`voaacenc` puis `avenc_aac`).
async fn aac_encoder_tokens(bitrate_kbps: u32) -> Result<Vec<String>> {
    let bitrate = u64::from(bitrate_kbps) * 1000;
    for el in ["voaacenc", "avenc_aac"] {
        if recorder::element_exists(el).await {
            return Ok(vec![el.to_owned(), format!("bitrate={bitrate}")]);
        }
    }
    bail!("aucun encodeur AAC disponible (voaacenc / avenc_aac)")
}

/// Traitement à appliquer à la piste vidéo.
enum VideoPlan {
    /// Aucune piste vidéo (enregistrement audio seul).
    None,
    /// Copie du flux H.264 tel quel (on garde la résolution) : rapide, sans
    /// perte.
    Copy,
    /// Décodage → `videoscale` → réencodage vers `width`×`height`.
    Rescale {
        width: u32,
        height: u32,
        encoder: VideoEncoder,
        bitrate_kbps: u32,
    },
}

/// Décide du traitement vidéo selon la sonde et la hauteur cible. Réduit
/// seulement (jamais d'agrandissement) et seulement si les dimensions sont
/// connues ; sinon copie. `encoder` est requis (et détecté) uniquement au
/// réencodage.
fn video_plan(probe: &Probe, target_height: Option<u32>, encoder: VideoEncoder) -> VideoPlan {
    if !probe.has_video {
        return VideoPlan::None;
    }
    if let (Some(h), Some((w, sh))) = (target_height, probe.video_dims) {
        if h < sh {
            return VideoPlan::Rescale {
                width: scaled_width(w, sh, h),
                height: h,
                encoder,
                bitrate_kbps: rescale_bitrate_kbps(h),
            };
        }
    }
    VideoPlan::Copy
}

fn push_video_branch(args: &mut Vec<String>, plan: &VideoPlan) {
    let (width, height, encoder, bitrate_kbps) = match plan {
        VideoPlan::None => return,
        VideoPlan::Copy => {
            // d.video_0 ! queue ! h264parse ! mux.
            args.push("d.video_0".into());
            args.push("!".into());
            push_big_queue(args);
            for t in ["!", "h264parse", "!", "mux."] {
                args.push(t.into());
            }
            return;
        }
        VideoPlan::Rescale {
            width,
            height,
            encoder,
            bitrate_kbps,
        } => (*width, *height, *encoder, *bitrate_kbps),
    };
    // d.video_0 ! queue ! h264parse ! avdec_h264 ! videoconvert ! videoscale
    //   ! video/x-raw,… ! <encodeur> ! h264parse ! mux.
    args.push("d.video_0".into());
    args.push("!".into());
    push_big_queue(args);
    for t in [
        "!",
        "h264parse",
        "!",
        "avdec_h264",
        "!",
        "videoconvert",
        "!",
        "videoscale",
        "!",
    ] {
        args.push(t.into());
    }
    args.push(format!(
        "video/x-raw,width={width},height={height},pixel-aspect-ratio=1/1"
    ));
    args.push("!".into());
    encoder.push_args(args, bitrate_kbps);
    for t in ["!", "h264parse", "!", "mux."] {
        args.push(t.into());
    }
}

fn push_audio_branch(args: &mut Vec<String>, index: u32, aac: &[String]) {
    args.push(format!("d.audio_{index}"));
    args.push("!".into());
    push_big_queue(args);
    for t in [
        "!",
        "opusdec",
        "!",
        "audioconvert",
        "!",
        "audioresample",
        "!",
    ] {
        args.push(t.into());
    }
    args.extend(aac.iter().cloned());
    for t in ["!", "aacparse", "!", "mux."] {
        args.push(t.into());
    }
}

/// Stem (nom sans extension) d'un nom de fichier.
fn stem(name: &str) -> &str {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

/// Construit la liste d'arguments `gst-launch-1.0` complète (fonction pure,
/// testée : c'est ici qu'un `!` manquant casserait tout le pipeline).
fn build_args(
    input_name: &str,
    output_name: &str,
    plan: &VideoPlan,
    audio_tracks: u32,
    aac: &[String],
) -> Vec<String> {
    let mut args = vec![
        "-e".to_owned(),
        "filesrc".to_owned(),
        format!("location={input_name}"),
        "!".to_owned(),
        "matroskademux".to_owned(),
        "name=d".to_owned(),
    ];
    push_video_branch(&mut args, plan);
    for i in 0..audio_tracks {
        push_audio_branch(&mut args, i, aac);
    }
    args.push("qtmux".into());
    args.push("name=mux".into());
    args.push("!".into());
    args.push("filesink".into());
    args.push(format!("location={output_name}"));
    args
}

/// Convertit `input_name` (relatif à `output_dir`) en MP4 dans le même
/// dossier. `target_height = None` garde la résolution source. Renvoie le nom
/// du fichier MP4 produit.
pub async fn to_mp4(
    output_dir: &Path,
    input_name: &str,
    target_height: Option<u32>,
) -> Result<String> {
    if !output_dir.join(input_name).is_file() {
        bail!("fichier introuvable : {input_name}");
    }
    let probe = probe(output_dir, input_name).await?;

    let output_name = match target_height {
        Some(h) => format!("{}_{h}p.mp4", stem(input_name)),
        None => format!("{}.mp4", stem(input_name)),
    };

    // L'encodeur n'est détecté que si un réencodage est nécessaire.
    let needs_reencode = matches!(
        (target_height, probe.video_dims),
        (Some(h), Some((_, sh))) if probe.has_video && h < sh
    );
    let encoder = if needs_reencode {
        recorder::detect_encoder().await
    } else {
        VideoEncoder::X264
    };
    let plan = video_plan(&probe, target_height, encoder);
    let aac = aac_encoder_tokens(160).await?;
    let args = build_args(input_name, &output_name, &plan, probe.audio_tracks, &aac);

    let out = gst_cmd(output_dir)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("lancement de la conversion (gst-launch-1.0)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
        let detail = tail
            .iter()
            .rev()
            .take(2)
            .rev()
            .copied()
            .collect::<Vec<_>>()
            .join(" — ");
        bail!("la conversion a échoué : {detail}");
    }
    Ok(output_name)
}

#[cfg(test)]
mod tests {
    use super::{build_args, parse_dims, scaled_width, stem, VideoEncoder, VideoPlan};

    fn aac() -> Vec<String> {
        vec!["voaacenc".to_owned(), "bitrate=160000".to_owned()]
    }

    #[test]
    fn build_args_remux_copies_video_and_transcodes_each_audio() {
        let p = build_args("in.mkv", "in.mp4", &VideoPlan::Copy, 2, &aac()).join(" ");
        assert!(p.starts_with("-e filesrc location=in.mkv ! matroskademux name=d"));
        assert!(p.contains("d.video_0 ! queue max-size-time=0 max-size-bytes=0 max-size-buffers=0 ! h264parse ! mux."));
        assert!(p.contains(
            "d.audio_0 ! queue max-size-time=0 max-size-bytes=0 max-size-buffers=0 ! opusdec ! audioconvert ! audioresample ! voaacenc bitrate=160000 ! aacparse ! mux."
        ));
        assert!(p.contains("d.audio_1 !"));
        assert!(p.ends_with("qtmux name=mux ! filesink location=in.mp4"));
        // Aucun « ! ! » : signe d'un élément manquant entre deux liens.
        assert!(!p.contains("! !"), "pipeline mal formé : {p}");
    }

    #[test]
    fn build_args_rescale_reencodes_with_square_pixels() {
        let plan = VideoPlan::Rescale {
            width: 1280,
            height: 720,
            encoder: VideoEncoder::X264,
            bitrate_kbps: 5000,
        };
        let p = build_args("in.mkv", "in_720p.mp4", &plan, 1, &aac()).join(" ");
        assert!(p.contains(
            "h264parse ! avdec_h264 ! videoconvert ! videoscale ! video/x-raw,width=1280,height=720,pixel-aspect-ratio=1/1 ! x264enc bitrate=5000"
        ));
        assert!(p.contains("x264enc bitrate=5000 speed-preset=veryfast tune=zerolatency"));
        assert!(p.contains("! h264parse ! mux."));
        assert!(!p.contains("! !"), "pipeline mal formé : {p}");
    }

    #[test]
    fn build_args_audio_only_has_no_video_branch() {
        let p = build_args("in.mkv", "in.mp4", &VideoPlan::None, 1, &aac()).join(" ");
        assert!(!p.contains("video"));
        assert!(p.contains("d.audio_0 !"));
        assert!(!p.contains("! !"), "pipeline mal formé : {p}");
    }

    #[test]
    fn parse_dims_reads_first_pair() {
        let caps =
            "caps = video/x-h264, width=(int)2646, height=(int)1684, framerate=(fraction)30/1";
        assert_eq!(parse_dims(caps), Some((2646, 1684)));
    }

    #[test]
    fn parse_dims_none_when_absent() {
        assert_eq!(parse_dims("could not link d to fakesink0"), None);
    }

    #[test]
    fn scaled_width_keeps_ratio_and_even() {
        // 1920x1080 → hauteur 720 : largeur 1280, paire.
        assert_eq!(scaled_width(1920, 1080, 720), 1280);
        // Source impaire → arrondi au pair.
        assert_eq!(scaled_width(2647, 1685, 480) % 2, 0);
    }

    #[test]
    fn stem_strips_extension() {
        assert_eq!(
            stem("discord-2026-06-22_03-06-47.mkv"),
            "discord-2026-06-22_03-06-47"
        );
    }
}
