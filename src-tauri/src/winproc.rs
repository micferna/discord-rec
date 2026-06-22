//! Suppression des fenêtres console parasites des sous-processus sous Windows.
//!
//! L'app GUI est compilée sans console (`windows_subsystem = "windows"`).
//! Chaque binaire console qu'elle lance (`gst-inspect-1.0`,
//! `gst-device-monitor-1.0`, `gst-launch-1.0`…) ouvrirait alors une fenêtre
//! `cmd` qui clignote. On pose `CREATE_NO_WINDOW` : le processus n'a pas de
//! console, et ses petits-enfants (ex. `gst-plugin-scanner`) en héritent —
//! donc aucune fenêtre, à aucun niveau.
//!
//! No-op hors Windows.

/// `CREATE_NO_WINDOW` : le processus enfant n'a pas de console.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Ajoute `.no_console()` aux `Command` (std et tokio), à poser avant de
/// spawner un binaire console.
pub(crate) trait CommandNoConsoleExt {
    fn no_console(&mut self) -> &mut Self;
}

impl CommandNoConsoleExt for std::process::Command {
    fn no_console(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

impl CommandNoConsoleExt for tokio::process::Command {
    fn no_console(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}
