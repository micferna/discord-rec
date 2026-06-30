//! Coupe par temps d'un enregistrement : extrait la fenêtre `[start,
//! start+durée]` d'un MKV (ou MP4) et la réécrit en MP4 (H.264 + AAC).
//!
//! Sert deux usages :
//! - **clip live (A)** : « les N dernières minutes » de l'enregistrement en
//!   cours, sans l'arrêter — `start = écoulé − N`, `durée = N` (avec une marge
//!   de sécurité avant le bord live, cf. `main.rs`) ;
//! - **montage (B)** : point d'entrée / point de sortie arbitraires sur un
//!   enregistrement terminé.
//!
//! Contrairement au reste du projet (qui shell-out vers `gst-launch`), la coupe
//! est pilotée **par programme** : `gst-launch` ne sait pas couper par temps
//! (pas de seek en CLI, et `nle` n'est pas pilotable depuis la ligne de
//! commande).
//!
//! **Deux pipelines découplés** (clé du fonctionnement) : un muxeur (`qtmux`)
//! ne survit pas à un flush en cours de flux — un seek casserait l'écriture de
//! son index (`moov`), MP4 illisible. On sépare donc :
//! - décodage `filesrc ! decodebin ! [réencodage] ! appsink` — **seekable** :
//!   on lui demande exactement la fenêtre `[start, stop]`, donc il ne décode
//!   que ça ;
//! - muxage `appsrc ! qtmux ! filesink` — ne voit **jamais** le flush (la
//!   frontière appsink→appsrc l'isole), il reçoit un flux propre et finalise.
//!
//! La frontière entre les deux est un pont manuel (callback `appsink` →
//! `appsrc`) qui rebase les timestamps à 0. Une *pad probe* de fenêtrage reste
//! en filet de sécurité si le seek échoue (fichier en cours sans index) :
//! décodage depuis 0, drop hors fenêtre, EOS franc à `stop`.
//!
//! Cross-plateforme (Linux + Windows). Sous Windows, les DLL `GStreamer` liées
//! par le crate sont chargées en *delay-load* (cf. `build.rs`) et le dossier
//! `bin` de `GStreamer` est ajouté au PATH au démarrage (`add_gstreamer_dll_dir`
//! dans `main.rs`), car l'installeur officiel ne l'y met pas.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

use crate::recorder::{self, VideoEncoder};

/// Délai max d'attente du préroll (PAUSED atteint) avant d'abandonner.
const PREROLL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(30);

/// Paires (appsink décodage, appsrc muxage) à relier : collectées pendant la
/// découverte des flux, puis dont on fixe les caps après le préroll.
type Bridges = Arc<Mutex<Vec<(gst_app::AppSink, gst_app::AppSrc)>>>;

/// Bitrate vidéo (kb/s) au réencodage du clip, selon la hauteur finale.
/// Mêmes paliers que la conversion MKV→MP4.
fn bitrate_kbps(height: u32) -> u32 {
    match height {
        0..=480 => 2500,
        481..=720 => 5000,
        721..=1080 => 8000,
        _ => 12000,
    }
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

/// Construit l'encodeur vidéo H.264 choisi, en réglant ses propriétés depuis
/// des chaînes — exactement les mêmes valeurs que `gst-launch` (cf.
/// `VideoEncoder::push_args`), ce qui évite tout souci de type et garde un
/// comportement identique à l'enregistrement.
fn make_video_encoder(encoder: VideoEncoder, bitrate_kbps: u32) -> Result<gst::Element> {
    let (factory, props): (&str, Vec<(&str, String)>) = match encoder {
        VideoEncoder::Nvenc => (
            "nvh264enc",
            vec![
                ("bitrate", bitrate_kbps.to_string()),
                ("gop-size", "120".into()),
            ],
        ),
        #[cfg(unix)]
        VideoEncoder::Vaapi => (
            "vah264enc",
            vec![
                ("bitrate", bitrate_kbps.to_string()),
                ("key-int-max", "120".into()),
            ],
        ),
        #[cfg(windows)]
        VideoEncoder::Qsv => (
            "qsvh264enc",
            vec![
                ("bitrate", bitrate_kbps.to_string()),
                ("gop-size", "120".into()),
            ],
        ),
        #[cfg(windows)]
        VideoEncoder::Amf => (
            "amfh264enc",
            vec![
                ("bitrate", bitrate_kbps.to_string()),
                ("gop-size", "120".into()),
            ],
        ),
        #[cfg(windows)]
        VideoEncoder::MediaFoundation => ("mfh264enc", vec![("bitrate", bitrate_kbps.to_string())]),
        VideoEncoder::X264 => (
            "x264enc",
            vec![
                ("bitrate", bitrate_kbps.to_string()),
                ("speed-preset", "veryfast".into()),
                ("tune", "zerolatency".into()),
                ("key-int-max", "120".into()),
            ],
        ),
    };
    let el = gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("élément {factory} indisponible"))?;
    for (name, value) in props {
        el.set_property_from_str(name, &value);
    }
    Ok(el)
}

/// Premier encodeur AAC disponible (`voaacenc` puis `avenc_aac`).
fn make_aac_encoder(bitrate_bps: u32) -> Result<gst::Element> {
    for factory in ["voaacenc", "avenc_aac"] {
        if let Ok(el) = gst::ElementFactory::make(factory).build() {
            el.set_property_from_str("bitrate", &bitrate_bps.to_string());
            return Ok(el);
        }
    }
    bail!("aucun encodeur AAC disponible (voaacenc / avenc_aac)")
}

/// `appsink` du pipeline de décodage : on consomme aussi vite que possible
/// (`sync=false`), pas en temps réel.
fn make_appsink() -> gst_app::AppSink {
    gst_app::AppSink::builder().sync(false).build()
}

/// `appsrc` du pipeline de muxage : timestamps fournis par nous, avec
/// contre-pression (`block`) pour ne jamais perdre de buffer ni exploser la
/// mémoire. Les caps sont fixés au 1er buffer.
fn make_appsrc() -> gst_app::AppSrc {
    gst_app::AppSrc::builder()
        .format(gst::Format::Time)
        .is_live(false)
        .do_timestamp(false)
        .block(true)
        .max_bytes(16 * 1024 * 1024)
        .build()
}

/// Construit la branche d'encodage vidéo du pipeline de décodage :
/// `queue ! videoconvert ! [videoscale ! caps] ! <encodeur> ! h264parse`,
/// terminée par un `appsink`. Réduit la résolution seulement si `target_height`
/// est plus petite que la source.
fn build_video_encode_branch(
    structure: &gst::StructureRef,
    target_height: Option<u32>,
    encoder: VideoEncoder,
) -> Result<(Vec<gst::Element>, gst_app::AppSink)> {
    let src_w = u32::try_from(structure.get::<i32>("width").unwrap_or(0)).unwrap_or(0);
    let src_h = u32::try_from(structure.get::<i32>("height").unwrap_or(0)).unwrap_or(0);
    let final_h = match target_height {
        Some(h) if src_h != 0 && h < src_h => h,
        _ => src_h,
    };

    let queue = gst::ElementFactory::make("queue").build()?;
    let convert = gst::ElementFactory::make("videoconvert").build()?;
    let mut elements = vec![queue, convert];

    // Redimensionnement seulement si on réduit réellement.
    if final_h != 0 && final_h < src_h {
        let width = scaled_width(src_w, src_h, final_h);
        let scale = gst::ElementFactory::make("videoscale").build()?;
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", i32::try_from(width).unwrap_or(i32::MAX))
            .field("height", i32::try_from(final_h).unwrap_or(i32::MAX))
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &caps)
            .build()?;
        elements.push(scale);
        elements.push(capsfilter);
    }

    let bitrate = bitrate_kbps(if final_h == 0 { src_h } else { final_h });
    elements.push(make_video_encoder(encoder, bitrate)?);
    elements.push(gst::ElementFactory::make("h264parse").build()?);
    // qtmux (MP4) exige du H.264 « avc » (codec_data dans les caps), pas du
    // byte-stream : sans qtmux en aval direct pour l'imposer, on le force ici,
    // sinon l'appsrc présente des caps byte-stream → qtmux « not-negotiated ».
    let avc = gst::Caps::builder("video/x-h264")
        .field("stream-format", "avc")
        .field("alignment", "au")
        .build();
    elements.push(
        gst::ElementFactory::make("capsfilter")
            .property("caps", &avc)
            .build()?,
    );
    Ok((elements, make_appsink()))
}

/// Branche d'encodage audio : `queue ! audioconvert ! audioresample ! <aac> !
/// aacparse`, terminée par un `appsink`. Une par piste.
fn build_audio_encode_branch() -> Result<(Vec<gst::Element>, gst_app::AppSink)> {
    let elements = vec![
        gst::ElementFactory::make("queue").build()?,
        gst::ElementFactory::make("audioconvert").build()?,
        gst::ElementFactory::make("audioresample").build()?,
        make_aac_encoder(160_000)?,
        gst::ElementFactory::make("aacparse").build()?,
    ];
    Ok((elements, make_appsink()))
}

/// Sentinelle « base de timestamp pas encore fixée ».
const BASE_UNSET: u64 = u64::MAX;

/// Marge (ns) au-delà de la durée de fenêtre tolérée pour un PTS rebasé : sert
/// à rejeter les buffers au timestamp aberrant qu'un encodeur/resampler peut
/// émettre à une discontinuité (drop ou flush) — sinon qtmux en déduit une
/// durée délirante.
const REBASE_SLACK: u64 = 10_000_000_000; // 10 s

/// Pont appsink (décodage) → appsrc (muxage) : pousse chaque buffer encodé en
/// rebasant ses timestamps sur le **1er buffer de CE flux** (qui devient t=0).
/// Indispensable car les encodeurs ne partagent pas la même base : `x264enc`,
/// par ex., décale la vidéo d'un offset constant énorme (~1000 h) là où l'audio
/// reste à l'horodatage source — une base partagée désynchroniserait tout. Un
/// rebase par flux remet chaque piste à 0, donc synchronisées (même fenêtre).
/// Les buffers hors `[0, durée + marge]` (timestamps poubelle) sont jetés.
fn wire_bridge(appsink: &gst_app::AppSink, appsrc: &gst_app::AppSrc, window: gst::ClockTime) {
    let appsrc_sample = appsrc.clone();
    let appsrc_eos = appsrc.clone();
    let max_ns = window.nseconds() + REBASE_SLACK;
    let base = Arc::new(std::sync::atomic::AtomicU64::new(BASE_UNSET));
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                // Base = horodatage (dts ≤ pts) du 1er buffer de ce flux.
                if let Some(ts) = buffer.dts().or_else(|| buffer.pts()) {
                    let _ = base.compare_exchange(
                        BASE_UNSET,
                        ts.nseconds(),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                }
                let base_ns = base.load(Ordering::Relaxed);
                if base_ns == BASE_UNSET {
                    return Ok(gst::FlowSuccess::Ok); // buffer sans horodatage : ignoré
                }
                let rebase = |t: Option<gst::ClockTime>| {
                    t.map(|v| gst::ClockTime::from_nseconds(v.nseconds().saturating_sub(base_ns)))
                };
                // Buffer au timestamp aberrant (discontinuité) : on le jette.
                let pts = rebase(buffer.pts());
                if pts.is_none_or(|p| p.nseconds() > max_ns) {
                    return Ok(gst::FlowSuccess::Ok);
                }
                let mut out = buffer.copy();
                {
                    let m = out.make_mut();
                    m.set_pts(pts);
                    m.set_dts(rebase(buffer.dts()));
                }
                appsrc_sample
                    .push_buffer(out)
                    .map_err(|_| gst::FlowError::Error)?;
                Ok(gst::FlowSuccess::Ok)
            })
            .eos(move |_sink| {
                let _ = appsrc_eos.end_of_stream();
            })
            .build(),
    );
}

/// Filet de sécurité si le seek échoue : jette les frames avant `start`, et
/// injecte un EOS franc sur le pad dès la 1re frame à `stop` ou au-delà.
fn install_trim_probe(pad: &gst::Pad, start: gst::ClockTime, stop: gst::ClockTime) {
    let start_ns = start.nseconds();
    let stop_ns = stop.nseconds();
    let eos_sent = Arc::new(AtomicBool::new(false));
    pad.add_probe(gst::PadProbeType::BUFFER, move |probe_pad, info| {
        let Some(buffer) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(pts) = buffer.pts() else {
            return gst::PadProbeReturn::Ok;
        };
        let pts_ns = pts.nseconds();
        if pts_ns >= stop_ns {
            if !eos_sent.swap(true, Ordering::SeqCst) {
                probe_pad.push_event(gst::event::Eos::new());
            }
            return gst::PadProbeReturn::Drop;
        }
        if pts_ns < start_ns {
            return gst::PadProbeReturn::Drop;
        }
        gst::PadProbeReturn::Ok
    });
}

/// Branche un flux décodé : monte sa chaîne de réencodage (→ appsink) dans le
/// pipeline de décodage, un `appsrc` correspondant dans le pipeline de muxage,
/// et les relie par le pont. Renvoie `true` si le flux a été branché (vidéo ou
/// audio), `false` s'il a été ignoré (sous-titres…).
#[allow(clippy::too_many_arguments)]
fn bridge_stream(
    decode: &gst::Pipeline,
    mux: &gst::Pipeline,
    qtmux: &gst::Element,
    src_pad: &gst::Pad,
    target_height: Option<u32>,
    encoder: VideoEncoder,
    start: gst::ClockTime,
    stop: gst::ClockTime,
    bridges: &Bridges,
) -> Result<bool> {
    let caps = src_pad
        .current_caps()
        .ok_or_else(|| anyhow!("caps absentes sur un pad décodé"))?;
    let structure = caps
        .structure(0)
        .ok_or_else(|| anyhow!("structure de caps absente"))?;
    let media = structure.name();

    let (elements, appsink, mux_tmpl) = if media.starts_with("video/") {
        let (els, sink) = build_video_encode_branch(structure, target_height, encoder)?;
        (els, sink, "video_%u")
    } else if media.starts_with("audio/") {
        let (els, sink) = build_audio_encode_branch()?;
        (els, sink, "audio_%u")
    } else {
        return Ok(false); // sous-titres / autre : ignoré
    };

    // Chaîne d'encodage dans le pipeline de décodage.
    decode
        .add_many(&elements)
        .context("ajout de la branche d'encodage")?;
    decode.add(&appsink).context("ajout de l'appsink")?;
    gst::Element::link_many(&elements).context("liaison interne de la branche")?;
    let last = elements.last().expect("branche non vide");
    last.link(&appsink).context("branche → appsink")?;
    for el in &elements {
        el.sync_state_with_parent()
            .context("synchronisation d'état (branche)")?;
    }
    appsink
        .sync_state_with_parent()
        .context("synchronisation d'état (appsink)")?;
    let head_sink = elements[0]
        .static_pad("sink")
        .ok_or_else(|| anyhow!("pad sink d'entrée de branche manquant"))?;
    src_pad
        .link(&head_sink)
        .map_err(|e| anyhow!("liaison decodebin→branche : {e}"))?;

    // appsrc correspondant dans le pipeline de muxage.
    let appsrc = make_appsrc();
    mux.add(&appsrc).context("ajout de l'appsrc")?;
    let mux_pad = qtmux
        .request_pad_simple(mux_tmpl)
        .ok_or_else(|| anyhow!("pad {mux_tmpl} refusé par qtmux"))?;
    appsrc
        .static_pad("src")
        .ok_or_else(|| anyhow!("pad src de l'appsrc manquant"))?
        .link(&mux_pad)
        .map_err(|e| anyhow!("liaison appsrc→qtmux : {e}"))?;
    appsrc
        .sync_state_with_parent()
        .context("synchronisation d'état (appsrc)")?;

    wire_bridge(&appsink, &appsrc, stop.saturating_sub(start));
    install_trim_probe(src_pad, start, stop);
    // Conservé pour fixer les caps de l'appsrc après le préroll (cf. run_clip).
    bridges
        .lock()
        .expect("mutex bridges")
        .push((appsink, appsrc));
    Ok(true)
}

/// Construit les deux pipelines (décodage, muxage) avec leurs éléments
/// terminaux. Renvoie `(décodage, decodebin, muxage, qtmux)`.
fn build_clip_pipelines(
    input: &Path,
    output: &Path,
) -> Result<(gst::Pipeline, gst::Element, gst::Pipeline, gst::Element)> {
    let decode = gst::Pipeline::default();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", input.to_string_lossy().as_ref())
        .build()
        .context("élément filesrc")?;
    // force-sw-decoders : on évite les décodeurs matériels (D3D11 sous Windows,
    // VA/CUDA sous Linux) qui sortent de la mémoire GPU, incompatible avec le
    // `videoconvert` (mémoire système) en aval → sinon « not-negotiated » et le
    // pipeline échoue à passer en PAUSED. Le décodage logiciel reste rapide,
    // borné à la fenêtre par le seek.
    let decodebin = gst::ElementFactory::make("decodebin")
        .property("force-sw-decoders", true)
        .build()
        .context("élément decodebin")?;
    decode
        .add_many([&filesrc, &decodebin])
        .context("construction du pipeline de décodage")?;
    filesrc.link(&decodebin).context("filesrc → decodebin")?;

    let mux = gst::Pipeline::default();
    let qtmux = gst::ElementFactory::make("qtmux")
        .build()
        .context("élément qtmux")?;
    let filesink = gst::ElementFactory::make("filesink")
        .property("location", output.to_string_lossy().as_ref())
        .build()
        .context("élément filesink")?;
    mux.add_many([&qtmux, &filesink])
        .context("construction du pipeline de muxage")?;
    qtmux.link(&filesink).context("qtmux → filesink")?;

    Ok((decode, decodebin, mux, qtmux))
}

/// Fixe les caps de chaque appsrc depuis les caps négociées de l'appsink
/// correspondant (connues une fois le décodage prérollé).
fn set_appsrc_caps(bridges: &Bridges) -> Result<()> {
    for (sink, src) in bridges.lock().expect("mutex bridges").iter() {
        let caps = sink
            .static_pad("sink")
            .and_then(|p| p.current_caps())
            .ok_or_else(|| anyhow!("caps d'un flux non négociées au préroll"))?;
        src.set_caps(Some(&caps));
    }
    Ok(())
}

/// Travail synchrone de coupe (exécuté dans un thread bloquant). `start` /
/// `stop` en temps média ; `target_height` réduit la résolution si plus petite
/// que la source (jamais d'agrandissement).
fn run_clip(
    input: &Path,
    output: &Path,
    start: gst::ClockTime,
    stop: gst::ClockTime,
    target_height: Option<u32>,
    encoder: VideoEncoder,
) -> Result<()> {
    gst::init().context("initialisation de GStreamer")?;

    // Pipeline de décodage (seekable) et pipeline de muxage (jamais flushé).
    let (decode, decodebin, mux, qtmux) = build_clip_pipelines(input, output)?;

    let stream_count = Arc::new(AtomicU32::new(0));
    let bridges: Bridges = Arc::new(Mutex::new(Vec::new()));

    let decode_weak = decode.downgrade();
    let mux_weak = mux.downgrade();
    let qtmux_weak = qtmux.downgrade();
    let count_c = stream_count.clone();
    let bridges_c = bridges.clone();
    decodebin.connect_pad_added(move |_dbin, src_pad| {
        let (Some(decode), Some(mux), Some(qtmux)) = (
            decode_weak.upgrade(),
            mux_weak.upgrade(),
            qtmux_weak.upgrade(),
        ) else {
            return;
        };
        match bridge_stream(
            &decode,
            &mux,
            &qtmux,
            src_pad,
            target_height,
            encoder,
            start,
            stop,
            &bridges_c,
        ) {
            Ok(true) => {
                count_c.fetch_add(1, Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("[discord-rec] pont clip : {e:#}");
                let _ = decode.post_message(gst::message::Application::new(
                    gst::Structure::builder("clip-bridge-error")
                        .field("detail", format!("{e:#}"))
                        .build(),
                ));
            }
        }
    });

    // Préroll du décodage : pads découverts et ponts construits.
    if decode.set_state(gst::State::Paused).is_err() {
        let detail = drain_error(&decode).unwrap_or_else(|| "passage à PAUSED échoué".to_owned());
        let _ = decode.set_state(gst::State::Null);
        let _ = mux.set_state(gst::State::Null);
        bail!("décodage → PAUSED : {detail}");
    }
    let (res, _, _) = decode.state(PREROLL_TIMEOUT);
    if res.is_err() {
        let detail = drain_error(&decode)
            .unwrap_or_else(|| "préroll impossible (fichier illisible ?)".to_owned());
        let _ = decode.set_state(gst::State::Null);
        let _ = mux.set_state(gst::State::Null);
        bail!("{detail}");
    }
    if stream_count.load(Ordering::Relaxed) == 0 {
        let _ = decode.set_state(gst::State::Null);
        let _ = mux.set_state(gst::State::Null);
        bail!("aucune piste lisible (fichier non média ou corrompu ?)");
    }

    // Caps des appsrc fixées depuis les caps négociées des appsink AVANT de
    // lancer le muxage (sinon qtmux démarre sur un appsrc sans caps →
    // « not-negotiated »).
    if let Err(e) = set_appsrc_caps(&bridges) {
        let _ = decode.set_state(gst::State::Null);
        let _ = mux.set_state(gst::State::Null);
        return Err(e);
    }

    // Le muxage démarre (appsrc prêts), puis on demande au décodage la fenêtre
    // exacte [start, stop]. Best-effort : le seek est émis sur decodebin (il
    // remonte à la source sans toucher au muxage, sur un autre pipeline) ; s'il
    // échoue, la probe de fenêtrage prend le relais (décodage depuis 0).
    mux.set_state(gst::State::Playing)
        .context("muxage → PLAYING")?;
    let _ = decodebin.seek(
        1.0,
        gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
        gst::SeekType::Set,
        start,
        gst::SeekType::Set,
        stop,
    );
    decode
        .set_state(gst::State::Playing)
        .context("décodage → PLAYING")?;

    let result = wait_for_completion(&decode, &mux);
    let _ = decode.set_state(gst::State::Null);
    let _ = mux.set_state(gst::State::Null);
    result
}

/// Attend la fin du muxage (EOS), en surveillant les deux bus pour remonter
/// toute erreur (décodage, pont, ou muxage) au plus vite.
fn wait_for_completion(decode: &gst::Pipeline, mux: &gst::Pipeline) -> Result<()> {
    use gst::MessageView;
    let decode_bus = decode.bus().expect("bus décodage");
    let mux_bus = mux.bus().expect("bus muxage");
    loop {
        // Erreurs côté décodage / pont (non bloquant).
        while let Some(msg) = decode_bus.pop() {
            match msg.view() {
                MessageView::Error(err) => {
                    bail!(
                        "décodage : {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    )
                }
                MessageView::Application(app) => {
                    if let Some(s) = app.structure() {
                        if s.name() == "clip-bridge-error" {
                            bail!("{}", s.get::<String>("detail").unwrap_or_default());
                        }
                    }
                }
                _ => {}
            }
        }
        // Avancement du muxage.
        if let Some(msg) = mux_bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            match msg.view() {
                MessageView::Eos(_) => return Ok(()),
                MessageView::Error(err) => {
                    bail!(
                        "muxage : {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    )
                }
                _ => {}
            }
        }
    }
}

/// Durée (s) d'un fichier média, par préroll + requête. Sert à l'interface de
/// montage (bornes des points d'entrée/sortie) et à la validation des clips.
pub(crate) fn media_duration_secs(path: &Path) -> Result<f64> {
    gst::init().context("initialisation de GStreamer")?;
    let pipeline = gst::Pipeline::default();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", path.to_string_lossy().as_ref())
        .build()
        .context("élément filesrc")?;
    let decodebin = gst::ElementFactory::make("decodebin")
        .build()
        .context("élément decodebin")?;
    pipeline
        .add_many([&filesrc, &decodebin])
        .context("construction du pipeline de sonde")?;
    filesrc.link(&decodebin).context("filesrc → decodebin")?;

    // Chaque pad décodé est jeté dans un fakesink : on ne veut que prérouler.
    let pipeline_weak = pipeline.downgrade();
    decodebin.connect_pad_added(move |_dbin, src_pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let Ok(sink) = gst::ElementFactory::make("fakesink").build() else {
            return;
        };
        if pipeline.add(&sink).is_err() || sink.sync_state_with_parent().is_err() {
            return;
        }
        if let Some(pad) = sink.static_pad("sink") {
            let _ = src_pad.link(&pad);
        }
    });

    pipeline
        .set_state(gst::State::Paused)
        .context("passage à PAUSED (sonde)")?;
    let (res, _, _) = pipeline.state(PREROLL_TIMEOUT);
    let duration = if res.is_ok() {
        pipeline.query_duration::<gst::ClockTime>()
    } else {
        None
    };
    let _ = pipeline.set_state(gst::State::Null);
    let duration = duration.ok_or_else(|| anyhow!("durée inconnue"))?;
    // ns d'une durée média < 2^53 (jusqu'à ~104 jours) : conversion exacte.
    #[allow(clippy::cast_precision_loss)]
    Ok(duration.nseconds() as f64 / 1e9)
}

/// Récupère le 1er message d'erreur en attente sur le bus, pour un diagnostic.
fn drain_error(pipeline: &gst::Pipeline) -> Option<String> {
    let bus = pipeline.bus()?;
    while let Some(msg) = bus.pop() {
        if let gst::MessageView::Error(err) = msg.view() {
            return Some(format!(
                "{} ({})",
                err.error(),
                err.debug().unwrap_or_default()
            ));
        }
    }
    None
}

/// Découpe `input_name` (du dossier `output_dir`) sur `[start_s, start_s +
/// duration_s]` et écrit un MP4 dans le même dossier. Renvoie le nom du MP4
/// produit. `target_height = None` garde la résolution source.
pub async fn clip(
    output_dir: &Path,
    input_name: &str,
    start_s: f64,
    duration_s: f64,
    target_height: Option<u32>,
) -> Result<String> {
    let input = output_dir.join(input_name);
    if !input.is_file() {
        bail!("fichier introuvable : {input_name}");
    }
    if !duration_s.is_finite() || duration_s <= 0.0 {
        bail!("durée de clip invalide");
    }
    let start_s = if start_s.is_finite() {
        start_s.max(0.0)
    } else {
        0.0
    };

    // Bornes en temps média (conversion via Duration : aucun cast flottant).
    let start = gst::ClockTime::try_from(std::time::Duration::from_secs_f64(start_s))
        .unwrap_or(gst::ClockTime::ZERO);
    let stop = gst::ClockTime::try_from(std::time::Duration::from_secs_f64(start_s + duration_s))
        .unwrap_or(gst::ClockTime::ZERO);

    // Nom de sortie : <stem>_clip_<début>-<fin>s.mp4 (lisible, et unique pour
    // ne pas écraser un clip précédent).
    let stem = Path::new(input_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(input_name);
    let output_name = format!("{stem}_clip_{}-{}s.mp4", start.seconds(), stop.seconds());
    let output = output_dir.join(&output_name);

    let encoder = recorder::detect_encoder().await;

    tokio::task::spawn_blocking(move || {
        run_clip(&input, &output, start, stop, target_height, encoder)
    })
    .await
    .context("tâche de coupe interrompue")??;

    Ok(output_name)
}

/// Durée (s) d'un enregistrement du dossier de sortie. Sert à l'interface de
/// montage pour borner les points d'entrée/sortie.
pub async fn duration(output_dir: &Path, input_name: &str) -> Result<f64> {
    let input = output_dir.join(input_name);
    if !input.is_file() {
        bail!("fichier introuvable : {input_name}");
    }
    tokio::task::spawn_blocking(move || media_duration_secs(&input))
        .await
        .context("tâche de sonde interrompue")?
}

#[cfg(test)]
mod tests {
    use super::{bitrate_kbps, media_duration_secs, run_clip, scaled_width, VideoEncoder};
    use gstreamer as gst;

    #[test]
    fn scaled_width_keeps_ratio_and_even() {
        assert_eq!(scaled_width(1920, 1080, 720), 1280);
        assert_eq!(scaled_width(2647, 1685, 480) % 2, 0);
        assert_eq!(scaled_width(1280, 0, 480), 480);
    }

    #[test]
    fn bitrate_grows_with_height() {
        assert!(bitrate_kbps(480) < bitrate_kbps(1080));
        assert_eq!(bitrate_kbps(2160), 12000);
    }

    /// Présence des éléments nécessaires au test de bout en bout (absents en CI
    /// sans gst-plugins ugly/good : on saute alors le test).
    fn elements_present(names: &[&str]) -> bool {
        gst::init().is_ok() && names.iter().all(|n| gst::ElementFactory::find(n).is_some())
    }

    /// Encodeur réel de la machine (NVENC si dispo) pour exercer le vrai chemin.
    fn best_encoder() -> VideoEncoder {
        if gst::ElementFactory::find("nvh264enc").is_some() {
            VideoEncoder::Nvenc
        } else {
            VideoEncoder::X264
        }
    }

    /// Génère un MKV jetable de 20 s : H.264 + `audio_tracks` pistes Opus,
    /// comme un enregistrement (1 piste = mode mixé, 2 = mode séparé).
    fn generate_mkv(src: &std::path::Path, audio_tracks: u32) {
        let mut args: Vec<String> = [
            "-e",
            "videotestsrc",
            "num-buffers=600",
            "!",
            "video/x-raw,framerate=30/1,width=320,height=240",
            "!",
            "x264enc",
            "bitrate=800",
            "key-int-max=30",
            "!",
            "h264parse",
            "!",
            "matroskamux",
            "name=m",
            "!",
            "filesink",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        // Slashes avant : la syntaxe de gst-launch traite '\' comme un
        // échappement, donc un chemin Windows en backslashes serait mal
        // interprété (le MKV irait ailleurs). gstreamer accepte les '/'.
        args.push(format!("location={}", src.display().to_string().replace('\\', "/")));
        for _ in 0..audio_tracks {
            for token in [
                "audiotestsrc",
                "num-buffers=900",
                "!",
                "audioconvert",
                "!",
                "opusenc",
                "!",
                "m.",
            ] {
                args.push(token.to_owned());
            }
        }
        let status = std::process::Command::new(crate::recorder::gst_tool("gst-launch-1.0"))
            .args(&args)
            .status()
            .expect("génération du MKV de test");
        assert!(status.success(), "gst-launch n'a pas généré le MKV de test");
    }

    /// Coupe réelle d'un MKV à `audio_tracks` pistes : extrait [8 s, 13 s] et
    /// vérifie que le MP4 produit dure ~5 s — preuve que le découplage
    /// décodage/muxage coupe au bon endroit ET que qtmux finalise.
    fn check_clip_window(audio_tracks: u32) {
        let needed = [
            "videotestsrc",
            "audiotestsrc",
            "x264enc",
            "h264parse",
            "matroskamux",
            "opusenc",
            "voaacenc",
            "avdec_h264",
            "opusdec",
            "qtmux",
            "decodebin",
        ];
        if !elements_present(&needed) {
            eprintln!("check_clip_window : éléments GStreamer absents, test sauté");
            return;
        }
        // gst-launch (génération du MKV de test) doit être exécutable — sur
        // certains CI il n'est pas dans le PATH ; on saute alors proprement.
        let runnable = std::process::Command::new(crate::recorder::gst_tool("gst-launch-1.0"))
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !runnable {
            eprintln!("check_clip_window : gst-launch-1.0 indisponible, test sauté");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "disc-rec-cliptest-{}-{audio_tracks}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("dossier de test");
        let src = dir.join("src.mkv");
        let out = dir.join("out.mp4");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&out);

        generate_mkv(&src, audio_tracks);

        run_clip(
            &src,
            &out,
            gst::ClockTime::from_seconds(8),
            gst::ClockTime::from_seconds(13),
            None,
            best_encoder(),
        )
        .expect("coupe du clip");

        assert!(out.is_file(), "le MP4 de clip n'a pas été créé");
        let dur = media_duration_secs(&out).expect("durée du clip");
        assert!(
            (4.0..=6.5).contains(&dur),
            "clip attendu ~5 s, obtenu {dur:.2} s ({audio_tracks} pistes audio)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mode mixé : une seule piste audio.
    #[test]
    fn clip_extracts_window_single_audio() {
        check_clip_window(1);
    }

    /// Mode séparé : deux pistes audio (Discord + micro) → deux pads qtmux.
    #[test]
    fn clip_extracts_window_two_audio() {
        check_clip_window(2);
    }
}
