mod capture;
mod dispatch;
mod egl;
mod portal;
mod render;
mod state;

use std::{
    cell::RefCell,
    os::unix::io::{AsFd, AsRawFd},
    rc::Rc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use nix::poll::{poll, PollFd, PollFlags};
use wayland_client::{
    protocol::{
        wl_surface::WlSurface,
    },
    Connection,
};
use wayland_protocols::xdg::{
    decoration::zv1::client::zxdg_toplevel_decoration_v1,
    shell::client::{xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel},
};

use capture::connect_pipewire_stream;
use state::{AppState, CaptureState};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // ── Portal handshake (async) ──────────────────────────────────────────────
    log::info!("requesting screencast via xdg-desktop-portal…");
    let portal_stream = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?
        .block_on(portal::request_screencast())
        .context("portal handshake")?;
    log::info!(
        "portal OK: node_id={} fd={}",
        portal_stream.node_id,
        portal_stream.pw_fd.as_raw_fd()
    );

    // ── Wayland connection ────────────────────────────────────────────────────
    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let wl_display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;

    let mut event_queue = conn.new_event_queue::<AppState>();
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = AppState::new();
    event_queue.roundtrip(&mut state).context("initial roundtrip")?;

    if state.compositor.is_none() {
        bail!("wl_compositor not available");
    }
    if state.xdg_wm_base.is_none() {
        bail!("xdg_wm_base not available");
    }

    // Create window
    let surface: WlSurface = state.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg_surface: XdgSurface = state
        .xdg_wm_base
        .as_ref()
        .unwrap()
        .get_xdg_surface(&surface, &qh, ());
    let xdg_toplevel: XdgToplevel = xdg_surface.get_toplevel(&qh, ());
    xdg_toplevel.set_title("kmag".into());
    xdg_toplevel.set_app_id("kmag".into());

    // Request server-side decorations (title bar, borders, buttons).
    // Must be done before the first commit so the compositor knows before configure.
    if let Some(mgr) = &state.decoration_mgr {
        let decoration = mgr.get_toplevel_decoration(&xdg_toplevel, &qh, ());
        decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
    } else {
        log::warn!("zxdg_decoration_manager_v1 not available; window will have no frame");
    }

    surface.commit();

    state.surface = Some(surface);
    state.xdg_surface = Some(xdg_surface);
    state.xdg_toplevel = Some(xdg_toplevel);

    event_queue.roundtrip(&mut state).context("seat roundtrip")?;

    while !state.window_configured {
        event_queue.blocking_dispatch(&mut state).context("wait configure")?;
    }

    state.init_egl(wl_display_ptr)?;

    // Submit an initial black frame so the compositor actually shows the window.
    // Until eglSwapBuffers is called at least once the surface has no buffer attached.
    unsafe {
        gl::ClearColor(0.0, 0.0, 0.0, 1.0);
        gl::Clear(gl::COLOR_BUFFER_BIT);
    }
    if let Some(egl) = &state.egl_ctx {
        egl.swap_buffers().context("initial swap")?;
    }
    conn.flush().context("flush after initial frame")?;

    // ── PipeWire stream ───────────────────────────────────────────────────────
    let cap = Rc::new(RefCell::new(CaptureState::new()));

    let (pw_mainloop, _stream_keep_alive, _listener_keep_alive) = connect_pipewire_stream(
        portal_stream.pw_fd,
        portal_stream.node_id,
        Rc::clone(&cap),
    )?;

    log::info!("starting main loop (zoom={:.2}, drag=left-click)", state.zoom);

    let wl_raw_fd = conn.as_fd().as_raw_fd();
    let pw_raw_fd = pw_mainloop.loop_().fd().as_raw_fd();

    // ── Main loop ─────────────────────────────────────────────────────────────
    while state.running {
        event_queue.dispatch_pending(&mut state).context("wl dispatch")?;
        pw_mainloop.loop_().iterate(Duration::ZERO);

        let frame_ready = cap.borrow().frame_ready;
        if frame_ready {
            cap.borrow_mut().frame_ready = false;
            let mut cap_borrow = cap.borrow_mut();
            match state.render(&mut cap_borrow) {
                Ok(()) => {}
                Err(e) => log::error!("render: {e}"),
            }
        }

        conn.flush().context("flush")?;

        // Sleep until either fd wakes us (cap 16ms ≈ 60fps)
        if !cap.borrow().frame_ready {
            let wl_bfd = unsafe {
                std::os::unix::io::BorrowedFd::borrow_raw(wl_raw_fd)
            };
            let pw_bfd = unsafe {
                std::os::unix::io::BorrowedFd::borrow_raw(pw_raw_fd)
            };
            let mut fds = [
                PollFd::new(wl_bfd, PollFlags::POLLIN),
                PollFd::new(pw_bfd, PollFlags::POLLIN),
            ];
            let _ = poll(&mut fds, 16u8);
        }
    }

    Ok(())
}
