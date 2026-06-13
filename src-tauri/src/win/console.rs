//! Arrêt propre de gst-launch sous Windows : l'équivalent du SIGINT Unix
//! est un événement console Ctrl+Break envoyé au groupe de processus de
//! l'enfant (créé avec `CREATE_NEW_PROCESS_GROUP`). gst-launch le convertit
//! en EOS (`-e`) et finalise le MKV — index de seek compris.

use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT,
};

/// Envoie Ctrl+Break au groupe de processus `pid`. Retourne `false` si la
/// console de l'enfant est inaccessible (l'appelant retombe alors sur kill).
pub fn send_ctrl_break(pid: u32) -> bool {
    // SAFETY: séquence Win32 documentée. On s'attache temporairement à la
    // console (cachée) de l'enfant ; notre propre gestionnaire Ctrl est
    // désactivé le temps de l'envoi pour ne pas nous interrompre nous-mêmes.
    unsafe {
        if AttachConsole(pid).is_err() {
            return false;
        }
        let _ = SetConsoleCtrlHandler(None, true);
        let sent = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid).is_ok();
        let _ = FreeConsole();
        let _ = SetConsoleCtrlHandler(None, false);
        sent
    }
}
