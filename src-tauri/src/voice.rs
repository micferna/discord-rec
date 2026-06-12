//! Abstraction plateforme : « Discord est-il en vocal, et où capturer
//! son audio ? ». Implémentations : `PipeWire` (Linux) et WASAPI (Windows).

use anyhow::Result;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub in_voice: bool,
    /// Cible de capture de la sortie audio Discord :
    /// `object.serial` du flux `PipeWire` (Linux) ou PID du processus (Windows).
    pub audio_target: Option<u64>,
}

#[cfg(unix)]
pub async fn snapshot() -> Result<Snapshot> {
    crate::pw::snapshot().await
}

#[cfg(windows)]
pub async fn snapshot() -> Result<Snapshot> {
    // L'énumération COM est bloquante : hors du runtime async.
    tokio::task::spawn_blocking(crate::win::audio::snapshot).await?
}
