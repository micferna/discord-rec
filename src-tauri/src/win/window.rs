//! Localisation de la fenêtre principale de Discord (la plus grande fenêtre
//! visible appartenant à un processus `Discord*.exe`).

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::win::is_discord_pid;

/// Surface minimale (px²) pour écarter les fenêtres techniques d'Electron.
const MIN_AREA: i64 = 200_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct DiscordWindow {
    pub hwnd: u64,
    /// Taille au moment de la détection : sert à épingler la résolution du
    /// pipeline pour survivre aux redimensionnements en cours
    /// d'enregistrement.
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
struct Best {
    win: DiscordWindow,
    area: i64,
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam pointe vers le `Best` de `find_discord_window`, vivant
    // pendant toute la durée d'EnumWindows.
    let best = unsafe { &mut *(lparam.0 as *mut Best) };

    if unsafe { !IsWindowVisible(hwnd).as_bool() } {
        return BOOL(1);
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    if !is_discord_pid(pid) {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &raw mut rect).is_err() } {
        return BOOL(1);
    }
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    let area = i64::from(width) * i64::from(height);
    if area > best.area {
        best.area = area;
        best.win = DiscordWindow {
            hwnd: hwnd.0 as u64,
            width: width.unsigned_abs(),
            height: height.unsigned_abs(),
        };
    }
    BOOL(1) // continuer l'énumération
}

pub fn find_discord_window() -> Option<DiscordWindow> {
    let mut best = Best::default();
    // SAFETY: le callback ne fait que des lectures Win32 et écrit dans `best`.
    unsafe {
        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(std::ptr::from_mut(&mut best) as isize),
        );
    }
    (best.area >= MIN_AREA).then_some(best.win)
}
