//! Détection de l'état vocal de Discord en interrogeant `PipeWire` (`pw-dump`).
//!
//! Quand Discord rejoint un salon vocal, son moteur WebRTC crée deux nœuds :
//! - `Stream/Input/Audio` (capture micro, `recStream`) — n'existe qu'en vocal ;
//! - `Stream/Output/Audio` (lecture des autres participants, `playStream`).
//!
//! Le nœud d'entrée en état `running` est donc le signal « en vocal », et le
//! `object.serial` du nœud de sortie sert de cible de capture audio.

use anyhow::{ensure, Context, Result};
use serde_json::Value;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub in_voice: bool,
    pub discord_out_serial: Option<u64>,
}

pub async fn snapshot() -> Result<Snapshot> {
    let out = tokio::process::Command::new("pw-dump")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("impossible de lancer pw-dump (pipewire-utils installé ?)")?;
    ensure!(out.status.success(), "pw-dump a retourné une erreur");

    let objects: Vec<Value> =
        serde_json::from_slice(&out.stdout).context("sortie pw-dump illisible")?;

    let mut snap = Snapshot::default();
    let mut out_running = false;
    for obj in &objects {
        if obj["type"].as_str() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let info = &obj["info"];
        let props = &info["props"];
        let binary = props["application.process.binary"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !binary.contains("discord") {
            continue;
        }
        let running = info["state"].as_str() == Some("running");
        match props["media.class"].as_str() {
            Some("Stream/Input/Audio") if running => snap.in_voice = true,
            // Préfère un flux actif ; sinon garde le premier trouvé.
            Some("Stream/Output/Audio") if running || !out_running => {
                snap.discord_out_serial = props["object.serial"].as_u64();
                out_running = running;
            }
            _ => {}
        }
    }
    Ok(snap)
}
