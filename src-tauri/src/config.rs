use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub output_dir: PathBuf,
    pub video: bool,
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    /// Images/s pour la capture X11 directe.
    pub framerate: u32,
    /// Secondes sans vocal avant d'arrêter l'enregistrement (anti-flap reconnexion).
    pub stop_debounce_s: u32,
    /// Micro à enregistrer (identifiant de `mics::list()`) ; `None` = défaut.
    pub mic_target: Option<String>,
    /// `true` = micro et Discord mixés dans UNE piste (audible partout) ;
    /// `false` = deux pistes séparées (pratique au montage).
    pub mix_audio: bool,
    /// Jeton du portail Wayland pour réutiliser la fenêtre choisie sans redemander.
    pub restore_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let videos = dirs::video_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            output_dir: videos.join("discord-rec"),
            video: true,
            video_bitrate_kbps: 8000,
            audio_bitrate_kbps: 128,
            framerate: 30,
            stop_debounce_s: 3,
            mic_target: None,
            mix_audio: true,
            restore_token: None,
        }
    }
}

impl Config {
    /// Borne les valeurs saisies par l'utilisateur dans des plages sûres.
    pub fn sanitize(&mut self) {
        self.video_bitrate_kbps = self.video_bitrate_kbps.clamp(500, 20_000);
        self.audio_bitrate_kbps = self.audio_bitrate_kbps.clamp(32, 510);
        self.framerate = self.framerate.clamp(5, 60);
        self.stop_debounce_s = self.stop_debounce_s.clamp(1, 120);
        if self
            .mic_target
            .as_deref()
            .is_some_and(|m| m.trim().is_empty())
        {
            self.mic_target = None;
        }
    }
}

fn config_path() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join("discord-rec").join("config.json"))
        .context("dossier de configuration utilisateur introuvable")
}

pub fn load() -> Config {
    let mut cfg: Config = config_path()
        .ok()
        .and_then(|p| fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    cfg.sanitize();
    cfg
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(cfg)?)?;
    // Le fichier contient le jeton du portail : lecture pour l'utilisateur seul.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn sanitize_clamps_user_values() {
        let mut cfg = Config {
            video_bitrate_kbps: 1,
            audio_bitrate_kbps: 9999,
            framerate: 0,
            stop_debounce_s: 0,
            ..Config::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.video_bitrate_kbps, 500);
        assert_eq!(cfg.audio_bitrate_kbps, 510);
        assert_eq!(cfg.framerate, 5);
        assert_eq!(cfg.stop_debounce_s, 1);
    }

    #[test]
    fn defaults_are_already_sane() {
        let mut cfg = Config::default();
        let before = serde_json::to_string(&cfg).expect("sérialisation");
        cfg.sanitize();
        assert_eq!(before, serde_json::to_string(&cfg).expect("sérialisation"));
    }
}
