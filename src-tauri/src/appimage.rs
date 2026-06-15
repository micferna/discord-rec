//! Assainissement de l'environnement des binaires système spawnés.
//!
//! Lancée en `AppImage`, l'app hérite d'un environnement qui pointe libs et
//! plugins `GStreamer` vers ceux EMBARQUÉS dans l'`AppImage` (`LD_LIBRARY_PATH`,
//! `GST_PLUGIN_SYSTEM_PATH_1_0`, …). Un binaire SYSTÈME spawné ensuite
//! (`gst-launch-1.0`, `pw-dump`, `xwininfo`, `xdg-open`) chargerait alors les
//! mauvais plugins/libs → « pas d'élément ximagesrc », symbole glib manquant…
//!
//! On retire donc ces variables des processus enfants — **uniquement** quand on
//! tourne en `AppImage` (variable `APPIMAGE` présente) ; sinon elles ne sont pas
//! définies et c'est un no-op.

/// Variables injectées par le `AppRun` de l'`AppImage` qui dérouteraient un
/// binaire système vers les libs/plugins embarqués.
#[cfg(target_os = "linux")]
const INJECTED: &[&str] = &[
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "GST_PLUGIN_SYSTEM_PATH",
    "GST_PLUGIN_SYSTEM_PATH_1_0",
    "GST_PLUGIN_PATH",
    "GST_PLUGIN_PATH_1_0",
    "GST_PLUGIN_SCANNER",
    "GST_PLUGIN_SCANNER_1_0",
    "GIO_MODULE_DIR",
    "GDK_PIXBUF_MODULE_FILE",
];

/// Ajoute `.strip_appimage_env()` aux `Command` (std et tokio), à appeler avant
/// de spawner un binaire système.
pub(crate) trait CommandAppImageExt {
    fn strip_appimage_env(&mut self) -> &mut Self;
}

impl CommandAppImageExt for std::process::Command {
    fn strip_appimage_env(&mut self) -> &mut Self {
        #[cfg(target_os = "linux")]
        if std::env::var_os("APPIMAGE").is_some() {
            for &var in INJECTED {
                self.env_remove(var);
            }
        }
        self
    }
}

impl CommandAppImageExt for tokio::process::Command {
    fn strip_appimage_env(&mut self) -> &mut Self {
        #[cfg(target_os = "linux")]
        if std::env::var_os("APPIMAGE").is_some() {
            for &var in INJECTED {
                self.env_remove(var);
            }
        }
        self
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::CommandAppImageExt;

    #[test]
    fn retire_les_vars_injectees_en_appimage() {
        std::env::set_var("APPIMAGE", "/tmp/x.AppImage");
        std::env::set_var("LD_LIBRARY_PATH", "/appimage/lib");
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", "/appimage/gst");

        let mut cmd = std::process::Command::new("true");
        cmd.strip_appimage_env();

        // env_remove apparaît dans get_envs avec une valeur None.
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();

        assert!(removed.iter().any(|k| k == "LD_LIBRARY_PATH"));
        assert!(removed.iter().any(|k| k == "GST_PLUGIN_SYSTEM_PATH_1_0"));

        std::env::remove_var("APPIMAGE");
        std::env::remove_var("LD_LIBRARY_PATH");
        std::env::remove_var("GST_PLUGIN_SYSTEM_PATH_1_0");
    }
}
