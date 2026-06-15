//! Localisation de la fenêtre principale de Discord sous `XWayland`.
//!
//! Discord (Electron) tourne en client X11 : sa fenêtre est capturable
//! directement avec `ximagesrc`, sans portail ni popup. On prend la plus
//! grande fenêtre de classe `discord` (les autres sont des fenêtres
//! techniques de quelques pixels).

use anyhow::{Context, Result};

use crate::appimage::CommandAppImageExt;

/// Surface minimale (px²) pour écarter les fenêtres techniques d'Electron.
const MIN_AREA: u64 = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscordWindow {
    pub xid: u64,
    /// Taille au moment de la détection : sert à épingler la résolution du
    /// pipeline pour survivre aux redimensionnements en cours
    /// d'enregistrement.
    pub width: u32,
    pub height: u32,
}

pub async fn find_discord_window() -> Result<Option<DiscordWindow>> {
    let out = tokio::process::Command::new("xwininfo")
        .strip_appimage_env()
        .args(["-root", "-tree", "-int"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("xwininfo introuvable (paquet x11-utils)")?;
    if !out.status.success() {
        // Pas de serveur X accessible : pas une erreur, juste pas de X11.
        return Ok(None);
    }
    Ok(parse_xwininfo_tree(&String::from_utf8_lossy(&out.stdout)))
}

/// Extrait de la sortie de `xwininfo -root -tree -int` la plus grande
/// fenêtre de classe `discord` (identifiant + géométrie), si sa surface est
/// plausible.
fn parse_xwininfo_tree(text: &str) -> Option<DiscordWindow> {
    let mut best: Option<DiscordWindow> = None;
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
        let Some((width, height)) = line.split_whitespace().find_map(|t| {
            let (w, rest) = t.split_once('x')?;
            let (h, _) = rest.split_once('+')?;
            Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
        }) else {
            continue;
        };
        let area = u64::from(width) * u64::from(height);
        let best_area = best.map_or(0, |b| u64::from(b.width) * u64::from(b.height));
        if area > best_area {
            best = Some(DiscordWindow { xid, width, height });
        }
    }
    best.filter(|b| u64::from(b.width) * u64::from(b.height) >= MIN_AREA)
}

#[cfg(test)]
mod tests {
    use super::{parse_xwininfo_tree, DiscordWindow};

    const SAMPLE: &str = r#"
xwininfo: Window id: 1320 (the root window) (has no name)

  Root window id: 1320 (the root window) (has no name)
  Parent window id: 0 (none)
     20 children:
     8388618 "scattered media, dakom - Discord": ("discord" "discord")  3840x2160+0+0  +0+0
     12582923 "discord": ("discord" "Discord")  200x200+0+0  +0+0
     12582915 "discord": ("discord" "Discord")  16x16+0+0  +0+0
     12582913 "discord": ("discord" "Discord")  10x10+10+10  +10+10
     6291467 "Mozilla Firefox": ("Navigator" "firefox")  2560x1380+0+0  +0+0
"#;

    #[test]
    fn picks_largest_discord_window_with_geometry() {
        assert_eq!(
            parse_xwininfo_tree(SAMPLE),
            Some(DiscordWindow {
                xid: 8_388_618,
                width: 3840,
                height: 2160
            })
        );
    }

    #[test]
    fn ignores_other_classes_and_tiny_windows() {
        let only_helpers = r#"
     12582923 "discord": ("discord" "Discord")  200x200+0+0  +0+0
     6291467 "Discord - Mozilla Firefox": ("Navigator" "firefox")  2560x1380+0+0  +0+0
"#;
        assert_eq!(parse_xwininfo_tree(only_helpers), None);
    }

    #[test]
    fn empty_tree_yields_none() {
        assert_eq!(parse_xwininfo_tree(""), None);
    }
}
