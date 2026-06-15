//! Mises à jour automatiques depuis les releases GitHub.
//!
//! Le manifeste `latest.json` (généré et signé par la CI à chaque release)
//! est interrogé via le plugin updater. Quand une mise à jour existe pour la
//! plateforme courante (Windows : NSIS), elle est téléchargée, vérifiée par
//! signature, installée, puis l'app redémarre. Sous Linux (.deb), le plugin
//! ne gère pas l'installation : l'UI propose simplement d'ouvrir la page de
//! la release.

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
    /// `true` si le plugin peut installer tout seul (Windows, ou Linux quand
    /// l'app tourne en `AppImage` : remplacement en place) ; sinon l'UI renvoie
    /// vers la page de release (cas du `.deb`).
    pub installable: bool,
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            current: update.current_version.clone(),
            notes: update.body.clone(),
            installable: self_installable(),
        })),
        Ok(None) => Ok(None),
        // Plateforme absente du manifeste (ex. installation .deb) : pas
        // d'erreur bloquante, juste pas de mise à jour automatique.
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "aucune mise à jour disponible".to_owned())?;
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

/// Ouvre la page de la dernière release (chemin Linux/.deb).
#[tauri::command]
pub fn open_releases_page() -> Result<(), String> {
    use crate::appimage::CommandAppImageExt;
    use std::process::Stdio;
    #[cfg(unix)]
    let opener = "xdg-open";
    #[cfg(windows)]
    let opener = "explorer";
    // Flux nuls : l'app GUI n'a pas de descripteurs standard valides à
    // hériter (sinon « os error 6 »).
    std::process::Command::new(opener)
        .strip_appimage_env()
        .arg("https://github.com/micferna/discord-rec/releases/latest")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// L'app peut-elle se mettre à jour toute seule sur cette plateforme/binaire ?
///
/// Windows : oui (NSIS). Linux : seulement en `AppImage` (le plugin remplace
/// le fichier en place) — Tauri expose la variable `APPIMAGE` dans ce cas. Pour
/// un `.deb`, l'UI renvoie vers la page de release.
fn self_installable() -> bool {
    self_installable_with(running_as_appimage())
}

#[cfg(target_os = "linux")]
fn running_as_appimage() -> bool {
    std::env::var_os("APPIMAGE").is_some()
}

#[cfg(not(target_os = "linux"))]
fn running_as_appimage() -> bool {
    false
}

#[cfg(windows)]
fn self_installable_with(_appimage: bool) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn self_installable_with(appimage: bool) -> bool {
    appimage
}

#[cfg(not(any(windows, target_os = "linux")))]
fn self_installable_with(_appimage: bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::self_installable_with;

    #[test]
    fn installable_seulement_windows_ou_appimage_linux() {
        if cfg!(windows) {
            assert!(self_installable_with(true));
            assert!(self_installable_with(false));
        } else if cfg!(target_os = "linux") {
            assert!(self_installable_with(true)); // AppImage → auto-install
            assert!(!self_installable_with(false)); // .deb → page de release
        } else {
            assert!(!self_installable_with(true));
        }
    }
}
