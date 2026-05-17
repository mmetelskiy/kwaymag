use std::{os::unix::io::OwnedFd, path::PathBuf};

use anyhow::{Context, Result};
use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SourceType},
    PersistMode,
};

pub struct PortalStream {
    pub pw_fd: OwnedFd,
    pub node_id: u32,
}

fn token_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut h = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            h.push(".local/share");
            h
        });
    base.join("kwaymag").join("restore_token")
}

pub async fn request_screencast() -> Result<PortalStream> {
    let proxy = Screencast::new().await.context("connect to ScreenCast portal")?;
    let session = proxy.create_session().await.context("create session")?;

    let restore_token = std::fs::read_to_string(token_path()).ok();
    if restore_token.is_some() {
        log::info!("reusing saved portal restore token");
    }

    proxy
        .select_sources(
            &session,
            CursorMode::Hidden,
            SourceType::Monitor.into(),
            true,
            restore_token.as_deref(),
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .context("select_sources")?;

    let response = proxy
        .start(&session, &ashpd::WindowIdentifier::default())
        .await
        .context("start screencast")?
        .response()
        .context("screencast start denied")?;

    if let Some(token) = response.restore_token() {
        let path = token_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, token) {
            log::warn!("could not save restore token: {e}");
        } else {
            log::info!("portal restore token saved");
        }
    }

    let stream = response
        .streams()
        .first()
        .context("no streams in portal response")?;

    let node_id = stream.pipe_wire_node_id();

    let pw_fd: OwnedFd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .context("open_pipe_wire_remote")?;

    Ok(PortalStream { pw_fd, node_id })
}
