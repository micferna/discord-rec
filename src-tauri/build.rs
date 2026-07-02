fn main() {
    // Sous Windows, le crate `gstreamer` lie des DLL que l'installeur officiel
    // ne met PAS dans le PATH. En liaison classique (load-time), l'app ne se
    // lancerait même pas. On les passe donc en *delay-load* : elles ne sont
    // résolues qu'au 1er appel GStreamer, après que `main()` ait ajouté le
    // dossier `bin` de GStreamer au PATH (cf. `add_gstreamer_dll_dir`).
    //
    // On teste la plateforme CIBLE (`CARGO_CFG_TARGET_OS`), pas l'hôte, pour
    // rester correct en cas de cross-compilation. `/DELAYLOAD` sur une DLL non
    // importée n'est qu'un avertissement (LNK4199), sans danger.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for dll in [
            "gstreamer-1.0-0.dll",
            "gstapp-1.0-0.dll",
            "gstbase-1.0-0.dll",
            "gobject-2.0-0.dll",
            "glib-2.0-0.dll",
            "gmodule-2.0-0.dll",
            "gio-2.0-0.dll",
        ] {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        // Helper de delay-load fourni par MSVC.
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
    tauri_build::build();
}
