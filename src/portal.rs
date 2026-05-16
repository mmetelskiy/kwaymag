use anyhow::{Context, Result};
use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SourceType},
    PersistMode,
};
use std::os::unix::io::OwnedFd;

pub struct PortalStream {
    pub pw_fd: OwnedFd,
    pub node_id: u32,
}

pub async fn request_screencast() -> Result<PortalStream> {
    let proxy = Screencast::new().await.context("connect to ScreenCast portal")?;
    let session = proxy.create_session().await.context("create session")?;

    proxy
        .select_sources(
            &session,
            CursorMode::Hidden,
            SourceType::Monitor.into(),
            false,
            None,
            PersistMode::DoNot,
        )
        .await
        .context("select_sources")?;

    let response = proxy
        .start(&session, &ashpd::WindowIdentifier::default())
        .await
        .context("start screencast")?
        .response()
        .context("screencast start denied")?;

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
