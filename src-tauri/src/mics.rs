//! Énumération des micros disponibles, pour le sélecteur des réglages.
//!
//! L'identifiant retourné est directement consommable par le pipeline :
//! - Linux : `object.serial` du nœud `PipeWire` (`Audio/Source`), pour
//!   `pipewiresrc target-object=…` ;
//! - Windows : le token `device="…"` complet (échappement compris) tel que
//!   suggéré par `gst-device-monitor-1.0`, pour `wasapi2src`.

use serde::Serialize;

#[cfg(unix)]
use crate::appimage::CommandAppImageExt;
#[cfg(windows)]
use crate::winproc::CommandNoConsoleExt;

#[derive(Clone, Serialize)]
pub struct Mic {
    pub id: String,
    pub label: String,
}

#[cfg(unix)]
pub async fn list() -> Vec<Mic> {
    let Ok(out) = tokio::process::Command::new("pw-dump")
        .strip_appimage_env()
        .stdin(std::process::Stdio::null())
        .output()
        .await
    else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
        .map(|objects| parse_pw_sources(&objects))
        .unwrap_or_default()
}

#[cfg(unix)]
fn parse_pw_sources(objects: &[serde_json::Value]) -> Vec<Mic> {
    let mut mics = Vec::new();
    for obj in objects {
        if obj["type"].as_str() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let props = &obj["info"]["props"];
        if props["media.class"].as_str() != Some("Audio/Source") {
            continue;
        }
        let Some(serial) = props["object.serial"].as_u64() else {
            continue;
        };
        let label = props["node.description"]
            .as_str()
            .or_else(|| props["node.nick"].as_str())
            .or_else(|| props["node.name"].as_str())
            .unwrap_or("Source audio")
            .to_owned();
        mics.push(Mic {
            id: serial.to_string(),
            label,
        });
    }
    mics
}

#[cfg(windows)]
pub async fn list() -> Vec<Mic> {
    let Ok(Ok(out)) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(crate::recorder::gst_tool("gst-device-monitor-1.0"))
            .no_console()
            .arg("Audio/Source")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    else {
        return Vec::new();
    };
    parse_device_monitor(&String::from_utf8_lossy(&out.stdout))
}

/// Extrait de la sortie de `gst-device-monitor-1.0 Audio/Source` les blocs
/// « Device found » : le libellé (`name :`) et le token `device="…"` de la
/// ligne `gst-launch-1.0 wasapi2src …` suggérée (déjà échappé par `GStreamer`).
#[cfg(windows)]
fn parse_device_monitor(text: &str) -> Vec<Mic> {
    let mut mics = Vec::new();
    let mut label: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            if let Some((_, value)) = rest.split_once(':') {
                label = Some(value.trim().to_owned());
            }
        }
        if trimmed.starts_with("gst-launch-1.0") && trimmed.contains("wasapi2src") {
            if let Some(token) = extract_device_token(trimmed) {
                mics.push(Mic {
                    id: token,
                    label: label.take().unwrap_or_else(|| "Micro".to_owned()),
                });
            }
        }
    }
    mics
}

/// Isole `device="…"` (guillemet fermant non échappé) dans une ligne
/// gst-launch suggérée.
#[cfg(windows)]
fn extract_device_token(line: &str) -> Option<String> {
    let start = line.find("device=\"")?;
    let value_start = start + "device=\"".len();
    let bytes = line.as_bytes();
    let mut i = value_start;
    while i < bytes.len() {
        if bytes[i] == b'"' && bytes[i - 1] != b'\\' {
            return Some(line[start..=i].to_owned());
        }
        i += 1;
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::parse_pw_sources;

    #[test]
    fn lists_only_audio_sources() {
        let objects: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
            {"type":"PipeWire:Interface:Node","info":{"props":{
                "media.class":"Audio/Source","object.serial":61,
                "node.description":"Micro USB Blue Yeti"}}},
            {"type":"PipeWire:Interface:Node","info":{"props":{
                "media.class":"Audio/Sink","object.serial":62,
                "node.description":"Casque"}}},
            {"type":"PipeWire:Interface:Port","info":{"props":{}}}
        ]"#,
        )
        .expect("json");
        let mics = parse_pw_sources(&objects);
        assert_eq!(mics.len(), 1);
        assert_eq!(mics[0].id, "61");
        assert_eq!(mics[0].label, "Micro USB Blue Yeti");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::parse_device_monitor;

    #[test]
    fn parses_wasapi_devices() {
        let sample = r#"
Device found:

	name  : Microphone (USB Audio Device)
	class : Audio/Source
	caps  : audio/x-raw, format=F32LE
	properties:
		device.api = wasapi2
	gst-launch-1.0 wasapi2src device="\\?\SWD#MMDEVAPI#\{0.0.1.00000000\}.\{abc\}" ! ...
"#;
        let mics = parse_device_monitor(sample);
        assert_eq!(mics.len(), 1);
        assert_eq!(mics[0].label, "Microphone (USB Audio Device)");
        assert!(mics[0].id.starts_with("device=\""));
        assert!(mics[0].id.ends_with('"'));
    }
}
