# Politique de sécurité

## Signaler une vulnérabilité

Merci de **ne pas ouvrir d'issue publique** pour une faille de sécurité.
Utilisez plutôt l'onglet **Security → Report a vulnerability** du dépôt
GitHub (signalement privé).

## Périmètre

Discord REC est une application locale : elle n'ouvre aucun port, ne contacte
aucun serveur et n'embarque aucune ressource distante (CSP `default-src
'self'`). Les sujets sensibles sont :

- l'invocation des processus externes (`gst-launch-1.0`, `pw-dump`,
  `xwininfo`, `xdg-open`) — toujours sans shell, arguments explicites ;
- le jeton du portail Wayland, stocké dans
  `~/.config/discord-rec/config.json` (mode `600`) ;
- les enregistrements produits, qui peuvent contenir des données privées —
  ils ne quittent jamais la machine.

## Vérifications automatisées

Chaque commit passe `cargo audit` (avis RUSTSEC) et `cargo deny`
(licences, sources, doublons) en CI, plus `clippy` pedantic sans warning.
La CI **échoue** si un avis de sécurité corrigeable apparaît dans les
dépendances.

## Risques connus et assumés (transparence)

Rien n'est masqué silencieusement : voici l'état exact, réévalué à chaque
montée de version de Tauri.

| Sujet | Nature | Pourquoi c'est accepté |
|---|---|---|
| Avis « unmaintained » gtk-rs/GTK3 (16 entrées dans `deny.toml`) | Crates non maintenus, **pas des vulnérabilités** | Pile GTK3 imposée par Tauri/wry sur Linux ; aucun chemin de mise à jour tant que Tauri n'a pas migré. Liste exhaustive et commentée dans `deny.toml`. |
| [RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429) — unsoundness `glib::VariantStrIter` | Unsoundness locale (pas d'exploitation à distance) | L'API concernée n'est pas appelée par ce projet ; le correctif (glib 0.20) est incompatible avec gtk 0.18 requis par Tauri. **L'alerte Dependabot est laissée ouverte volontairement** pour rester visible. |

## Binaires Windows et antivirus

Les installeurs ne sont **pas signés** (la signature de code exige un
certificat payant). Certains moteurs antivirus heuristiques peuvent signaler
un faux positif « Generic ML/PUA », fréquent pour les installeurs NSIS
récents sans réputation. Chaque artefact de release est accompagné d'une
**attestation de provenance** GitHub prouvant cryptographiquement qu'il a
été compilé par la CI publique de ce dépôt, depuis ce code source :

```sh
gh attestation verify Discord.REC_X.Y.Z_x64-setup.exe --repo micferna/discord-rec
```

En cas de doute : compilez depuis les sources (voir `CONTRIBUTING.md`).
