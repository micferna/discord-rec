# Contribuer

## Prérequis

```sh
sudo apt install -y libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  librsvg2-dev libjavascriptcoregtk-4.1-dev libayatana-appindicator3-dev \
  gstreamer1.0-tools gstreamer1.0-pipewire gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
  gstreamer1.0-plugins-base-apps pipewire-bin x11-utils
```

Rust stable ≥ 1.87 (via [rustup](https://rustup.rs)).

## Boucle de développement

```sh
cd src-tauri
cargo build --release          # binaire : target/release/discord-rec
cargo test                     # tests unitaires
cargo fmt --check              # formatage
cargo clippy --all-targets -- -D warnings   # lint (pedantic activé)
cargo audit && cargo deny check             # sécurité dépendances
```

La CI rejoue exactement ces commandes : un PR vert localement est vert en CI.

## Règles

- Zéro warning clippy (le niveau pedantic est activé dans `Cargo.toml`) ;
- pas de nouveau processus lancé via un shell (toujours des arguments
  explicites) ;
- les chaînes visibles par l'utilisateur sont en français, comme le reste ;
- un commit = un changement cohérent, message à l'impératif.
