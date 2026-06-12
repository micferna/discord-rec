# Discord REC

Enregistre **automatiquement** tes sessions vocales Discord sur Linux
(Wayland + PipeWire) : dès que tu rejoins un salon vocal, un enregistrement
démarre ; quand tu quittes, il s'arrête et le fichier est finalisé.

- **Audio Discord uniquement** (les autres participants) — piste Opus 1
- **Ton micro** — piste Opus 2 (pistes séparées, pratique pour le montage)
- **Vidéo** : la fenêtre Discord seule, via le portail Wayland — H.264
- Conteneur **MKV** (lisible même si l'enregistrement est interrompu),
  un fichier horodaté par session dans `~/Vidéos/discord-rec/`

## Fonctionnement

1. La boucle de service interroge PipeWire (`pw-dump`) chaque seconde.
   Quand Discord est en vocal, son moteur WebRTC expose un nœud
   `Stream/Input/Audio` en état `running` : c'est le déclencheur.
2. Au démarrage d'un enregistrement, le portail `ScreenCast` fournit le flux
   de la fenêtre Discord (au premier lancement, GNOME demande quelle fenêtre
   capturer ; le choix est mémorisé via `restore_token`).
3. Un processus `gst-launch-1.0` muxe les trois flux dans un MKV.
   À la sortie du vocal (anti-rebond configurable), SIGINT → EOS → fichier
   finalisé proprement.

## Dépendances système

- PipeWire (`pw-dump`)
- GStreamer : `gstreamer1.0-tools`, `gstreamer1.0-pipewire`,
  plugins good/bad/ugly (x264, opus, matroska)
- Tauri : `libwebkit2gtk-4.1-dev`, `libgtk-3-dev` (compilation)
- `xdg-desktop-portal-gnome` (capture de fenêtre sous Wayland)

## Compiler & lancer

```sh
cd src-tauri
cargo build --release
./target/release/discord-rec
```

Fermer la fenêtre **ne quitte pas** le service (relancer le binaire fait
réapparaître la fenêtre — instance unique). Le bouton **Quitter** arrête
proprement l'enregistrement en cours puis ferme l'application.

### Lancement automatique à l'ouverture de session

```sh
cp discord-rec.desktop ~/.config/autostart/
```

## Qualité / sécurité

```sh
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings   # pedantic activé
cargo audit                                  # avis RUSTSEC
cargo deny check                             # licences + doublons + avis
```

- Aucun shell intermédiaire : tous les processus sont lancés avec des
  arguments explicites (pas d'injection possible).
- CSP stricte (`default-src 'self'`), aucune ressource distante dans l'UI.
- Le fichier de config (`~/.config/discord-rec/config.json`, mode `600`)
  contient le jeton du portail ; il ne quitte jamais la machine.

## Limites connues

- Si la fenêtre Discord est redimensionnée pendant un enregistrement, le
  pipeline vidéo peut s'interrompre (l'enregistrement audio redémarre alors
  automatiquement après le délai de garde).
- La capture suit la fenêtre choisie dans la popup du portail ; si Discord
  est relancé, utiliser « Re-choisir la fenêtre ».
