//! Garde anti-instance-obsolète.
//!
//! Avant de lancer Tauri, on coupe les éventuelles instances « discord-rec »
//! qui tournent encore sur un AUTRE binaire que le nôtre — typiquement une
//! ancienne version dont le binaire a été remplacé par une mise à jour
//! (`.deb`), ou une copie installée ailleurs. Sans ça, le plugin
//! `single-instance` refocaliserait cette vieille fenêtre, qui se croit (à
//! juste titre) en retard et réaffiche sans fin la bannière de mise à jour.
//!
//! On ne touche JAMAIS à une instance qui tourne sur EXACTEMENT le même
//! binaire (même chemin, non remplacé) : ce cas légitime reste géré par
//! `single-instance` (il refocalise la fenêtre existante).

/// Termine les instances obsolètes de l'application encore en mémoire.
///
/// Sous Windows, ce nettoyage est assuré à l'installation par le hook NSIS
/// (`taskkill`), donc cette fonction n'y fait rien.
#[cfg(target_os = "linux")]
pub fn terminate_stale_instances() {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    let me = std::process::id();
    let current: Option<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok());

    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };

    let mut victims: Vec<i32> = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(target) = fs::read_link(format!("/proc/{pid}/exe")) else {
            continue;
        };
        let target = target.to_string_lossy();
        // read_link d'un binaire remplacé renvoie « /chemin (deleted) ».
        let deleted = target.ends_with(" (deleted)");
        let real = target.trim_end_matches(" (deleted)");
        let is_ours = Path::new(real)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("discord-rec"));
        if !is_ours {
            continue;
        }
        // Même binaire, non remplacé → instance légitime : on laisse
        // single-instance la refocaliser.
        let same_binary = !deleted && current.as_deref() == Some(Path::new(real));
        if same_binary {
            continue;
        }
        if let Ok(pid) = i32::try_from(pid) {
            victims.push(pid);
        }
    }

    if victims.is_empty() {
        return;
    }
    eprintln!(
        "[discord-rec] {} instance(s) obsolète(s) détectée(s) : arrêt avant démarrage",
        victims.len()
    );

    // SIGTERM d'abord : laisse l'ancienne instance finaliser proprement un
    // éventuel enregistrement (PDEATHSIG SIGINT sur gst), puis SIGKILL pour
    // les récalcitrants.
    for &pid in &victims {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        // kill(pid, 0) renvoie 0 tant que le process existe.
        if victims
            .iter()
            .all(|&pid| unsafe { libc::kill(pid, 0) } != 0)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for &pid in &victims {
        if unsafe { libc::kill(pid, 0) } == 0 {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Sur les plateformes autres que Linux, le nettoyage des doublons/zombies est
/// assuré ailleurs (hook NSIS à l'installation sous Windows) : no-op.
#[cfg(not(target_os = "linux"))]
pub fn terminate_stale_instances() {}
