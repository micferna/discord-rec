; Hook post-installation : installe le runtime GStreamer s'il est absent.
; - Téléchargement depuis le miroir GitHub du dépôt (CDN rapide), repli sur
;   gstreamer.freedesktop.org
; - Intégrité vérifiée contre le SHA-256 officiel publié par gstreamer.org
; - Installation silencieuse msiexec en mode Complete (ADDLOCAL=ALL)
;
; Version 1.26.2 : dernier runtime distribué en MSI (les 1.28.x sont des
; .exe sans options silencieuses documentées) ; le minimum requis par
; l'app est 1.22 (wasapi2 loopback par processus, d3d11).

!define GST_VERSION "1.26.2"
!define GST_MSI_NAME "gstreamer-1.0-msvc-x86_64-${GST_VERSION}.msi"
!define GST_URL_MIRROR "https://github.com/micferna/discord-rec/releases/download/deps-gstreamer-${GST_VERSION}/${GST_MSI_NAME}"
!define GST_URL_UPSTREAM "https://gstreamer.freedesktop.org/data/pkg/windows/${GST_VERSION}/msvc/${GST_MSI_NAME}"
!define GST_MSI_SHA256 "f1897f0f5a132d011d5ddfe76d8740fdd47bb0dc6c7f276a5880ade38976bc9c"

; Hook pré-installation : anti-zombie + anti-doublon.
;
; ${UNINSTKEY} est défini par le template Tauri =
;   Software\Microsoft\Windows\CurrentVersion\Uninstall\Discord REC
!macro NSIS_HOOK_PREINSTALL
  ; 1) Couper toute instance en cours. Sans ça : fichiers verrouillés pendant
  ;    l'install, ET surtout un « zombie » d'ancienne version que le
  ;    single-instance refocaliserait après la mise à jour (= la bannière de
  ;    mise à jour qui réapparaît sans fin).
  nsExec::Exec 'taskkill /F /T /IM "Discord REC.exe"'
  Pop $0

  ; 2) Anti-doublon perMachine -> perUser. L'app s'installe désormais pour
  ;    l'utilisateur courant (HKCU). Une ancienne version (<= 0.3.1) installée
  ;    « pour tous les utilisateurs » a écrit son entrée NSIS sous HKLM ; la
  ;    logique « désinstaller l'ancienne » de Tauri ne regarde que HKCU (mode
  ;    courant) + les installs MSI, donc cette ancienne NSIS perMachine
  ;    survit et les deux cohabitent. On la désinstalle ici.
  ;
  ;    Best-effort : la désinstallation « pour tous les utilisateurs » peut
  ;    déclencher une élévation (UAC). L'install perUser visant
  ;    %LOCALAPPDATA% (chemin distinct), il n'y a pas de conflit même si la
  ;    désinstallation s'exécute en arrière-plan.
  ReadRegStr $0 HKLM "${UNINSTKEY}" "UninstallString"
  StrCmp $0 "" preinstall_done 0
    DetailPrint "Ancienne installation pour tous les utilisateurs detectee : desinstallation…"
    ; $0 = `"C:\Program Files\Discord REC\uninstall.exe"` (deja entre
    ; guillemets) : `'$0 /S'` forme une commande valide.
    ExecWait '$0 /S' $1
  preinstall_done:
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Déjà installé ? (chemins par défaut des installeurs officiels)
  IfFileExists "C:\gstreamer\1.0\msvc_x86_64\bin\gst-launch-1.0.exe" gstreamer_done 0
  IfFileExists "C:\gstreamer\1.0\mingw_x86_64\bin\gst-launch-1.0.exe" gstreamer_done 0

  DetailPrint "Runtime GStreamer absent : téléchargement (~130 Mo) depuis GitHub…"
  nsExec::ExecToLog 'curl.exe -L --fail --retry 2 --connect-timeout 30 -o "$TEMP\gstreamer-runtime.msi" "${GST_URL_MIRROR}"'
  Pop $0
  StrCmp $0 "0" gstreamer_check 0

  DetailPrint "Miroir GitHub indisponible : tentative via gstreamer.org…"
  nsExec::ExecToLog 'curl.exe -L --fail --retry 2 --connect-timeout 30 -o "$TEMP\gstreamer-runtime.msi" "${GST_URL_UPSTREAM}"'
  Pop $0
  StrCmp $0 "0" gstreamer_check gstreamer_failed

gstreamer_check:
  DetailPrint "Vérification de l'intégrité (SHA-256)…"
  nsExec::ExecToLog 'cmd /c certutil -hashfile "$TEMP\gstreamer-runtime.msi" SHA256 | find /i "${GST_MSI_SHA256}"'
  Pop $0
  StrCmp $0 "0" 0 gstreamer_corrupt

  DetailPrint "Installation du runtime GStreamer (mode Complete)…"
  ExecWait 'msiexec /i "$TEMP\gstreamer-runtime.msi" ADDLOCAL=ALL /passive /norestart' $0
  Delete "$TEMP\gstreamer-runtime.msi"
  StrCmp $0 "0" gstreamer_done 0
  ; 3010 = installation réussie, redémarrage conseillé : c'est un succès.
  StrCmp $0 "3010" gstreamer_done gstreamer_failed

gstreamer_corrupt:
  Delete "$TEMP\gstreamer-runtime.msi"
  MessageBox MB_OK|MB_ICONEXCLAMATION "Le téléchargement de GStreamer est corrompu (somme SHA-256 invalide) : installation annulée par sécurité.$\r$\nInstallez-le manuellement : https://gstreamer.freedesktop.org/download/"
  Goto gstreamer_done

gstreamer_failed:
  MessageBox MB_OK|MB_ICONEXCLAMATION "Le runtime GStreamer n'a pas pu être installé automatiquement (code $0).$\r$\n$\r$\nInstallez-le manuellement depuis :$\r$\nhttps://gstreamer.freedesktop.org/download/$\r$\n(paquet runtime MSVC x86_64, installation « Complete »)"

gstreamer_done:
!macroend
