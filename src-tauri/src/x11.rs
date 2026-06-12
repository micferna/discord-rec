//! Localisation de la fenêtre principale de Discord sous `XWayland`.
//!
//! Discord (Electron) tourne en client X11 : sa fenêtre est capturable
//! directement avec `ximagesrc`, sans portail ni popup. On prend la plus
//! grande fenêtre de classe `discord` (les autres sont des fenêtres
//! techniques de quelques pixels).

use anyhow::{Context, Result};

/// Surface minimale (px²) pour écarter les fenêtres techniques d'Electron.
const MIN_AREA: u64 = 200_000;

pub async fn find_discord_window() -> Result<Option<u64>> {
    let out = tokio::process::Command::new("xwininfo")
        .args(["-root", "-tree", "-int"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("xwininfo introuvable (paquet x11-utils)")?;
    if !out.status.success() {
        // Pas de serveur X accessible : pas une erreur, juste pas de X11.
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut best: Option<(u64, u64)> = None; // (xid, surface)
    for line in text.lines() {
        // Format : `  <id> "titre": ("instance" "classe")  <W>x<H>+X+Y  +X+Y`
        if !line.to_ascii_lowercase().contains("(\"discord\"") {
            continue;
        }
        let Some(xid) = line
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(area) = line.split_whitespace().find_map(|t| {
            let (w, rest) = t.split_once('x')?;
            let (h, _) = rest.split_once('+')?;
            Some(w.parse::<u64>().ok()? * h.parse::<u64>().ok()?)
        }) else {
            continue;
        };
        if best.is_none_or(|(_, a)| area > a) {
            best = Some((xid, area));
        }
    }
    Ok(best
        .filter(|&(_, area)| area >= MIN_AREA)
        .map(|(xid, _)| xid))
}
