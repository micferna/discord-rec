//! Implémentations Windows : détection vocale WASAPI, localisation de la
//! fenêtre Discord, et job object pour ne jamais orphaner gst-launch.

pub mod audio;
pub mod console;
pub mod job;
pub mod window;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Exécutables Discord officiels (sans extension). Liste exacte : un simple
/// préfixe matcherait notre propre `Discord REC.exe` — l'app s'enregistrait
/// elle-même.
const DISCORD_EXES: &[&str] = &[
    "discord",
    "discordptb",
    "discordcanary",
    "discorddevelopment",
];

/// Le processus `pid` est-il un vrai client Discord ?
pub(crate) fn is_discord_pid(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    // SAFETY: appels Win32 documentés ; le handle est toujours refermé.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut buf = [0u16; 1024];
        let mut len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| {
                DISCORD_EXES
                    .iter()
                    .any(|exe| stem.eq_ignore_ascii_case(exe))
            })
    }
}
