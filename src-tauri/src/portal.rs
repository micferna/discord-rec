//! Capture de la fenêtre Discord via le portail Wayland
//! (`org.freedesktop.portal.ScreenCast`).
//!
//! Le portail impose un choix de fenêtre par l'utilisateur (popup GNOME) au
//! premier enregistrement ; le `restore_token` retourné est ensuite persisté
//! pour que les sessions suivantes démarrent sans interaction.

use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use ashpd::enumflags2::BitFlags;
use tokio::sync::oneshot;

/// Tient la session du portail ouverte pendant l'enregistrement ; la session
/// est fermée proprement quand le guard est lâché.
pub struct SessionGuard {
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

pub struct VideoSource {
    pub fd: OwnedFd,
    pub node_id: u32,
    pub restore_token: Option<String>,
    pub guard: SessionGuard,
}

type Acquired = (OwnedFd, u32, Option<String>);

pub async fn acquire(restore_token: Option<String>) -> Result<VideoSource> {
    let (tx_res, rx_res) = oneshot::channel::<Result<Acquired>>();
    let (tx_stop, rx_stop) = oneshot::channel::<()>();

    // La session emprunte le proxy : les deux vivent ensemble dans cette tâche,
    // qui ne se termine qu'à la fermeture du guard.
    tauri::async_runtime::spawn(async move {
        let proxy = match Screencast::new().await {
            Ok(p) => p,
            Err(e) => {
                let _ = tx_res.send(Err(e.into()));
                return;
            }
        };
        let result = async {
            let session = proxy
                .create_session(CreateSessionOptions::default())
                .await?;
            let options = SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(BitFlags::from(SourceType::Window))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore_token.as_deref());
            proxy.select_sources(&session, options).await?;
            let response = proxy
                .start(&session, None, StartCastOptions::default())
                .await?
                .response()?;
            let stream = response
                .streams()
                .first()
                .context("aucune fenêtre sélectionnée")?;
            let node_id = stream.pipe_wire_node_id();
            let token = response.restore_token().map(ToOwned::to_owned);
            let fd = proxy
                .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
                .await?;
            anyhow::Ok((session, fd, node_id, token))
        }
        .await;

        match result {
            Ok((session, fd, node_id, token)) => {
                let _ = tx_res.send(Ok((fd, node_id, token)));
                // Garde la session du portail vivante tant qu'on enregistre.
                let _ = rx_stop.await;
                let _ = session.close().await;
            }
            Err(e) => {
                let _ = tx_res.send(Err(e));
            }
        }
    });

    // Si l'utilisateur laisse la popup sans réponse, on ne bloque pas le
    // service indéfiniment : au-delà du délai, repli audio seul.
    let (fd, node_id, restore_token) =
        tokio::time::timeout(std::time::Duration::from_secs(120), rx_res)
            .await
            .context("popup du portail restée sans réponse (120 s)")?
            .context("la tâche du portail s'est interrompue")?
            .context("le portail ScreenCast a refusé")?;

    Ok(VideoSource {
        fd,
        node_id,
        restore_token,
        guard: SessionGuard {
            stop: Some(tx_stop),
        },
    })
}
