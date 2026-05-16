mod egl;
mod portal;
mod render;

use std::{
    cell::RefCell,
    os::unix::io::{AsFd, AsRawFd, OwnedFd},
    rc::Rc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use nix::poll::{poll, PollFd, PollFlags};
use pipewire::{
    main_loop::MainLoopRc,
    context::ContextRc,
    stream::{StreamFlags, StreamListener, StreamRc},
    spa::{
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            ParamType,
        },
        pod::{
            object, property,
            serialize::PodSerializer,
            Pod, Value,
        },
        utils::{Direction, SpaTypes},
        buffer::DataType,
        param::video::{VideoFormat, VideoInfoRaw},
    },
    properties::properties,
};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::WlKeyboard,
        wl_output::WlOutput,
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_surface::{self, WlSurface},
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::xdg::{
    decoration::zv1::client::{
        zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
    },
    shell::client::{
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::{self, XdgToplevel},
        xdg_wm_base::{self, XdgWmBase},
    },
};

// ─── constants ───────────────────────────────────────────────────────────────

const DEFAULT_WINDOW_W: i32 = 400;
const DEFAULT_WINDOW_H: i32 = 300;
const DEFAULT_ZOOM: f64 = 2.0;
const ZOOM_STEP: f64 = 0.25;
const ZOOM_MIN: f64 = 1.0;
const ZOOM_MAX: f64 = 32.0;

// GL_BGRA_EXT — byte order is B G R A in memory on little-endian
const GL_BGRA_EXT: gl::types::GLenum = 0x80E1;

// ─── shared capture state ─────────────────────────────────────────────────────

struct CaptureState {
    shm_frame: Option<ShmFrame>,
    width: u32,
    height: u32,
    gl_format: gl::types::GLenum,
    frame_ready: bool,
}

struct ShmFrame {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl CaptureState {
    fn new() -> Self {
        Self {
            shm_frame: None,
            width: 0,
            height: 0,
            gl_format: gl::RGBA,
            frame_ready: false,
        }
    }
}

// ─── application state ───────────────────────────────────────────────────────

pub struct AppState {
    compositor: Option<WlCompositor>,
    xdg_wm_base: Option<XdgWmBase>,
    seat: Option<WlSeat>,
    decoration_mgr: Option<ZxdgDecorationManagerV1>,

    surface: Option<WlSurface>,
    xdg_surface: Option<XdgSurface>,
    xdg_toplevel: Option<XdgToplevel>,
    window_width: i32,
    window_height: i32,
    window_configured: bool,

    pointer: Option<WlPointer>,
    drag_active: bool,
    ptr_x: f64,
    ptr_y: f64,

    egl_ctx: Option<egl::EglContext>,
    gl_res: Option<render::GlResources>,

    region_cx: f64,
    region_cy: f64,
    zoom: f64,

    running: bool,
}

impl AppState {
    fn new() -> Self {
        Self {
            compositor: None,
            xdg_wm_base: None,
            seat: None,
            decoration_mgr: None,
            surface: None,
            xdg_surface: None,
            xdg_toplevel: None,
            window_width: DEFAULT_WINDOW_W,
            window_height: DEFAULT_WINDOW_H,
            window_configured: false,
            pointer: None,
            drag_active: false,
            ptr_x: 0.0,
            ptr_y: 0.0,
            egl_ctx: None,
            gl_res: None,
            region_cx: 0.0,
            region_cy: 0.0,
            zoom: DEFAULT_ZOOM,
            running: true,
        }
    }

    fn init_egl(&mut self, wl_display_ptr: *mut std::ffi::c_void) -> Result<()> {
        let surface_id = self.surface.as_ref().context("surface not created")?.id();
        let ctx = egl::EglContext::new(
            wl_display_ptr,
            surface_id,
            self.window_width,
            self.window_height,
        )?;
        let gl_res = render::GlResources::new(&ctx)?;
        self.egl_ctx = Some(ctx);
        self.gl_res = Some(gl_res);
        log::info!("EGL/GL initialised");
        Ok(())
    }

    fn render(&mut self, cap: &mut CaptureState) -> Result<()> {
        let gl_res = self.gl_res.as_ref().context("no GL resources")?;

        let (buf_w, buf_h) = if let Some(frame) = &cap.shm_frame {
            gl_res.upload_shm(&frame.data, frame.width, frame.height, cap.gl_format);
            (frame.width as f64, frame.height as f64)
        } else {
            return Ok(());
        };

        // Centre view on first frame
        if self.region_cx == 0.0 && self.region_cy == 0.0 {
            self.region_cx = buf_w / 2.0;
            self.region_cy = buf_h / 2.0;
        }

        let win_w = self.window_width as f64;
        let win_h = self.window_height as f64;
        let half_w = win_w / (2.0 * self.zoom);
        let half_h = win_h / (2.0 * self.zoom);

        // Full (unclamped) source region — may extend past buffer edges.
        let full_sx0 = self.region_cx - half_w;
        let full_sy0 = self.region_cy - half_h;
        let full_sx1 = self.region_cx + half_w;
        let full_sy1 = self.region_cy + half_h;

        // Clamp to buffer bounds.
        let sx0 = full_sx0.max(0.0) as i32;
        let sy0 = full_sy0.max(0.0) as i32;
        let sx1 = full_sx1.min(buf_w) as i32;
        let sy1 = full_sy1.min(buf_h) as i32;

        // Destination rectangle: shrink proportionally to match the clipped source,
        // so the zoom level is preserved and no stretching occurs.
        let dst_x = ((sx0 as f64 - full_sx0) * self.zoom).round() as i32;
        let dst_y = ((sy0 as f64 - full_sy0) * self.zoom).round() as i32;
        let dst_w = ((sx1 - sx0) as f64 * self.zoom).round() as i32;
        let dst_h = ((sy1 - sy0) as f64 * self.zoom).round() as i32;

        gl_res.blit(
            sx0, sy0, sx1 - sx0, sy1 - sy0,
            dst_x, dst_y, dst_w, dst_h,
            self.window_width, self.window_height,
        );
        self.egl_ctx.as_ref().context("no EGL context")?.swap_buffers()?;
        Ok(())
    }

    fn adjust_zoom(&mut self, delta: f64) {
        self.zoom = (self.zoom + delta).clamp(ZOOM_MIN, ZOOM_MAX);
        log::debug!("zoom = {:.2}", self.zoom);
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        self.region_cx = (self.region_cx - dx / self.zoom).max(0.0);
        self.region_cy = (self.region_cy - dy / self.zoom).max(0.0);
    }
}

// ─── Wayland dispatch impls ───────────────────────────────────────────────────

impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(6), qh, ()));
            }
            "xdg_wm_base" => {
                state.xdg_wm_base = Some(registry.bind(name, version.min(5), qh, ()));
            }
            "wl_seat" => {
                state.seat = Some(registry.bind(name, version.min(8), qh, ()));
            }
            "zxdg_decoration_manager_v1" => {
                state.decoration_mgr = Some(registry.bind(name, version.min(1), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for AppState {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.window_configured = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    state.window_width = width;
                    state.window_height = height;
                    if let Some(egl_ctx) = &state.egl_ctx {
                        egl_ctx.resize(width, height);
                    }
                }
            }
            xdg_toplevel::Event::Close => {
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for AppState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let caps = match capabilities {
                wayland_client::WEnum::Value(v) => v,
                _ => return,
            };
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Button { button, state: btn_state, .. } => {
                if button == 272 {
                    state.drag_active =
                        btn_state == wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed);
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                if state.drag_active {
                    let dx = surface_x - state.ptr_x;
                    let dy = surface_y - state.ptr_y;
                    state.pan(dx, dy);
                }
                state.ptr_x = surface_x;
                state.ptr_y = surface_y;
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if axis == wayland_client::WEnum::Value(wl_pointer::Axis::VerticalScroll) {
                    state.adjust_zoom(-value * ZOOM_STEP / 15.0);
                }
            }
            _ => {}
        }
    }
}

// WlSurface sends enter/leave/preferred_buffer_* events — accept silently.
impl Dispatch<WlSurface, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// Server-side decoration: accept whatever mode the compositor configures.
impl Dispatch<ZxdgToplevelDecorationV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZxdgToplevelDecorationV1,
        _: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(AppState: ZxdgDecorationManagerV1);
delegate_noop!(AppState: WlCompositor);
delegate_noop!(AppState: WlShmPool);
delegate_noop!(AppState: WlBuffer);
delegate_noop!(AppState: WlKeyboard);
delegate_noop!(AppState: WlShm);
delegate_noop!(AppState: WlOutput);

// ─── PipeWire format pod ──────────────────────────────────────────────────────

/// Serialize a video format pod offering BGRA (with RGBA fallback) to PipeWire.
fn build_video_format_pod() -> Vec<u8> {
    let pod_object = object! {
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(
            FormatProperties::MediaType,
            Id,
            MediaType::Video
        ),
        property!(
            FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        property!(
            FormatProperties::VideoFormat,
            Choice, Enum, Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
    };

    let mut bytes = Vec::new();
    PodSerializer::serialize(std::io::Cursor::new(&mut bytes), &Value::Object(pod_object))
        .expect("pod serialization");
    bytes
}

// ─── PipeWire stream setup ────────────────────────────────────────────────────

fn connect_pipewire_stream(
    pw_fd: OwnedFd,
    node_id: u32,
    cap: Rc<RefCell<CaptureState>>,
) -> Result<(MainLoopRc, StreamRc, StreamListener<()>)> {
    let mainloop = MainLoopRc::new(None).context("pw MainLoop")?;
    let context = ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_fd_rc(pw_fd, None)
        .context("pw connect_fd")?;

    let stream = StreamRc::new(
        core,
        "kmag-capture",
        properties! {
            "media.type" => "Video",
            "media.category" => "Capture",
            "media.role" => "Screen",
        },
    )
    .context("pw Stream::new")?;

    // ── param_changed: format negotiated ─────────────────────────────────────
    let cap_pc = Rc::clone(&cap);
    // IMPORTANT: keep `_listener` alive for the lifetime of the stream.
    // Dropping it unregisters the callbacks immediately.
    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_stream, _, id, pod| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let pod = match pod {
                Some(p) => p,
                None => return,
            };
            let mut info = VideoInfoRaw::new();
            if info.parse(pod).is_err() {
                return;
            }
            let fmt = info.format();
            let size = info.size();
            log::info!("PipeWire format: {:?} {}×{}", fmt, size.width, size.height);

            let mut cap = cap_pc.borrow_mut();
            cap.width = size.width;
            cap.height = size.height;
            cap.gl_format = match fmt {
                VideoFormat::RGBA | VideoFormat::RGBx => gl::RGBA,
                _ => GL_BGRA_EXT,
            };
        })
        // ── process: new frame available ──────────────────────────────────────
        .process(move |stream, _| {
            let mut buf = match stream.dequeue_buffer() {
                Some(b) => b,
                None => return,
            };

            let datas = buf.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];

            let mut cap = cap.borrow_mut();
            if cap.width == 0 || cap.height == 0 {
                return;
            }

            // With MAP_BUFFERS, DmaBuf memory is mmapped and accessible via data().
            // Treat DmaBuf the same as MemFd/MemPtr — just read the mapped bytes.
            match data.type_() {
                DataType::MemFd | DataType::MemPtr | DataType::DmaBuf => {
                    let chunk_size = data.chunk().size() as usize;
                    if chunk_size == 0 {
                        return;
                    }
                    if let Some(slice) = data.data() {
                        let copy = slice[..chunk_size.min(slice.len())].to_vec();
                        cap.shm_frame = Some(ShmFrame {
                            data: copy,
                            width: cap.width,
                            height: cap.height,
                        });
                        cap.frame_ready = true;
                    }
                }
                _ => {}
            }
        })
        .register()
        .context("stream listener register")?;

    // Connect with our format offer
    let format_bytes = build_video_format_pod();
    let format_pod = Pod::from_bytes(&format_bytes).context("invalid format pod")?;
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format_pod],
        )
        .context("stream connect")?;

    Ok((mainloop, stream, _listener))
}

// ─── main ─────────────────────────────────────────────────────────────────────

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
