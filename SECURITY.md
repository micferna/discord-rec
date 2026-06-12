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
