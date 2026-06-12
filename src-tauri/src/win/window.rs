//! Localisation de la fenêtre principale de Discord (la plus grande fenêtre
//! visible appartenant à un processus `Discord*.exe`).

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::win::is_discord_pid;

/// Surface minimale (px²) pour écarter les fenêtres techniques d'Electron.
const MIN_AREA: i64 = 200_000;

#[derive(Default)]
struct Best {
    hwnd: u64,
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
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if !is_discord_pid(pid) {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect).is_err() } {
        return BOOL(1);
    }
    let area = i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top);
    if area > best.area {
        best.area = area;
        best.hwnd = hwnd.0 as u64;
    }
    BOOL(1) // continuer l'énumération
}

pub fn find_discord_window() -> Option<u64> {
    let mut best = Best::default();
    // SAFETY: le callback ne fait que des lectures Win32 et écrit dans `best`.
    unsafe {
        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(std::ptr::from_mut(&mut best) as isize),
        );
    }
    (best.area >= MIN_AREA).then_some(best.hwnd)
}
