# Discord REC

[![CI](https://github.com/micferna/discord-rec/actions/workflows/ci.yml/badge.svg)](https://github.com/micferna/discord-rec/actions/workflows/ci.yml)
[![Licence: MIT](https://img.shields.io/badge/licence-MIT-green.svg)](LICENSE)
![Plateforme](https://img.shields.io/badge/plateforme-Linux%20%7C%20Windows%2010%2F11%20(b%C3%AAta)-blue)

Enregistre **automatiquement** tes sessions vocales Discord sur Linux
(Wayland + PipeWire) : dès que tu rejoins un salon vocal, un enregistrement
démarre ; quand tu quittes, il s'arrête et le fichier est finalisé.

- **Audio Discord uniquement** (les autres participants) — piste Opus 1
- **Ton micro** — piste Opus 2 (pistes séparées, pratique pour le montage)
- **Vidéo** : la fenêtre Discord seule, capturée **directement via
  X11/XWayland** (`ximagesrc`) — aucune popup, aucun portail — H.264
- Conteneur **MKV** (lisible même si l'enregistrement est interrompu),
  un fichier horodaté par session dans `~/Vidéos/discord-rec/`

## Fonctionnement

1. La boucle de service interroge PipeWire (`pw-dump`) chaque seconde.
   Quand Discord est en vocal, son moteur WebRTC expose un nœud
   `Stream/Input/Audio` en état `running` : c'est le déclencheur.
2. Au démarrage d'un enregistrement, la fenêtre Discord est localisée dans
   l'arbre X11 (`xwininfo`) — Discord/Electron tourne sous XWayland — et
   capturée directement par `ximagesrc`. Si Discord passait un jour en
   Wayland natif, repli automatique sur le portail `ScreenCast` (popup de
   choix une fois, mémorisée via `restore_token`).
3. Un processus `gst-launch-1.0` muxe les trois flux dans un MKV.
   À la sortie du vocal (anti-rebond configurable), SIGINT → EOS → fichier
   finalisé proprement.

## Dépendances système

- PipeWire (`pw-dump`)
- GStreamer : `gstreamer1.0-tools`, `gstreamer1.0-pipewire`,
  plugins good/bad/ugly (x264, opus, matroska)
- Tauri : `libwebkit2gtk-4.1-dev`, `libgtk-3-dev` (compilation)
- `xdg-desktop-portal-gnome` (capture de fenêtre sous Wayland)

## Installation & mises à jour (Linux)

Deux artefacts sont publiés à chaque release :

- **`.deb`** — installation système classique
  (`sudo apt install ./Discord\ REC_*_amd64.deb`). La mise à jour ne s'installe
  pas toute seule : l'app ouvre la page de release pour télécharger le nouveau
  `.deb`.
- **`.AppImage`** — binaire portable **avec mise à jour automatique en place**
  (bouton « Mettre à jour » dans l'app, sans mot de passe). Nécessite
  **GStreamer installé sur le système** (l'AppImage ne le bundle pas : l'app
  appelle `gst-launch-1.0`) — cf. *Dépendances système* ci-dessus.

## Compiler & lancer

```sh
cd src-tauri
cargo build --release
./target/release/discord-rec
```

Fermer la fenêtre **ne quitte pas** le service : il continue d'enregistrer en
arrière-plan. Une **icône de barre d'état** permet de rouvrir la fenêtre
(clic) ou de **Quitter** proprement (l'enregistrement en cours est finalisé
avant la fermeture). Relancer le binaire ramène aussi la fenêtre existante
(instance unique).

### Lancement automatique à l'ouverture de session

```sh
sed "s|@PREFIX@|$PWD|" discord-rec.desktop > ~/.config/autostart/discord-rec.desktop
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

## Windows 10/11 (bêta)

Le même binaire fonctionne sous Windows avec des mécanismes natifs :

- **Détection vocale** : sessions audio WASAPI — Discord ouvre une session de
  capture (micro) active quand tu es en vocal ;
- **Audio Discord seul** : loopback WASAPI **ciblé par processus**
  (`wasapi2src loopback-target-pid`, Windows 10 20H2 minimum) ;
- **Vidéo** : fenêtre Discord via Windows Graphics Capture
  (`d3d11screencapturesrc`) ;
- **Encodeur** : NVENC > Quick Sync > AMF > Media Foundation > x264.

Installation : prendre le **`setup.exe`** attaché à la release — il
télécharge et installe automatiquement le runtime GStreamer s'il est absent
(~130 Mo, une seule fois). Le `.msi` est l'alternative sans cette étape
automatique : il faut alors installer soi-même le
[runtime GStreamer MSVC](https://gstreamer.freedesktop.org/download/)
(mode « Complete »). Dans les deux cas, pas besoin de toucher au `PATH` :
l'app localise GStreamer toute seule.

> ⚠️ Le support Windows est compilé et vérifié en CI mais encore peu testé
> en conditions réelles — les retours sont bienvenus dans les issues.

### Antivirus / VirusTotal

Les binaires ne sont pas signés (certificat payant) : quelques moteurs
heuristiques peuvent afficher un faux positif « Generic ML/PUA », courant
pour les installeurs récents sans réputation. Chaque artefact de release a
une **attestation de provenance** prouvant qu'il a été compilé par la CI
publique de ce dépôt :

```sh
gh attestation verify Discord.REC_X.Y.Z_x64-setup.exe --repo micferna/discord-rec
```

L'installeur **MSI** est fourni en alternative au NSIS (moins sujet aux
faux positifs). Voir aussi `SECURITY.md`.

## Limites connues

- La résolution vidéo est figée à la taille de la fenêtre au début de
  l'enregistrement : un redimensionnement en cours de session est absorbé
  (l'image est remise à l'échelle, légère déformation possible si le ratio
  change) au lieu d'interrompre le pipeline.
- La capture X11 enregistre la fenêtre telle qu'elle est rendue : si elle est
  minimisée pendant le vocal, les images peuvent se figer (l'audio continue).
- L'encodeur H.264 est choisi automatiquement : **NVENC** (GPU NVIDIA,
  ~0,3 cœur en 4K) > **VA-API** (GPU Intel/AMD) > **x264** logiciel
  (~4-5 cœurs en 4K — baisser FPS/bitrate dans ce cas). L'encodeur actif
  est affiché dans l'en-tête de l'app pendant l'enregistrement.
