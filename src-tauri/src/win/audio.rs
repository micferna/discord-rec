//! Détection vocale via les sessions audio WASAPI.
//!
//! Quand Discord rejoint un salon vocal, il ouvre une session **de capture**
//! (micro) active : c'est le signal « en vocal », symétrique du nœud
//! `Stream/Input/Audio` `PipeWire` sous Linux. La session de **rendu** active
//! du même processus donne le PID à passer au loopback ciblé de `wasapi2src`.

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eCapture, eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::voice::Snapshot;
use crate::win::is_discord_pid;

pub fn snapshot() -> Result<Snapshot> {
    // SAFETY: séquence COM standard ; S_FALSE (déjà initialisé) est acceptable.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("énumérateur de périphériques audio indisponible")?;

        let mut snap = Snapshot::default();
        let mut render_active = false;
        for flow in [eCapture, eRender] {
            let Ok(devices) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
                continue;
            };
            let count = devices.GetCount().unwrap_or(0);
            for i in 0..count {
                let Ok(device) = devices.Item(i) else {
                    continue;
                };
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let Ok(sessions) = manager.GetSessionEnumerator() else {
                    continue;
                };
                let session_count = sessions.GetCount().unwrap_or(0);
                for j in 0..session_count {
                    let Ok(session) = sessions.GetSession(j) else {
                        continue;
                    };
                    let Ok(session2) = session.cast::<IAudioSessionControl2>() else {
                        continue;
                    };
                    let Ok(pid) = session2.GetProcessId() else {
                        continue;
                    };
                    if !is_discord_pid(pid) {
                        continue;
                    }
                    let active = session
                        .GetState()
                        .is_ok_and(|s| s == AudioSessionStateActive);
                    if flow == eCapture {
                        if active {
                            snap.in_voice = true;
                        }
                    } else if active || !render_active {
                        // Préfère une session de rendu active ; sinon garde
                        // la première trouvée.
                        snap.audio_target = Some(u64::from(pid));
                        render_active = active;
                    }
                }
            }
        }
        Ok(snap)
    }
}
