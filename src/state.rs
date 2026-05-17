use anyhow::{Context, Result};
use wayland_client::{
    protocol::{
        wl_compositor::WlCompositor,
        wl_pointer::WlPointer,
        wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
    Proxy,
};
use wayland_protocols::{
    wp::{
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::ZwpRelativePointerV1,
        },
    },
    xdg::{
        decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        shell::client::{
            xdg_surface::XdgSurface,
            xdg_toplevel::XdgToplevel,
            xdg_wm_base::XdgWmBase,
        },
    },
};

use crate::{egl, render};

// ─── constants ───────────────────────────────────────────────────────────────

pub const DEFAULT_WINDOW_W: i32 = 400;
pub const DEFAULT_WINDOW_H: i32 = 300;
const DEFAULT_ZOOM: f64 = 2.0;
pub const ZOOM_STEP: f64 = 0.25;
const ZOOM_MIN: f64 = 1.0;
const ZOOM_MAX: f64 = 32.0;

// ─── shared capture state ─────────────────────────────────────────────────────

pub struct CaptureState {
    pub shm_frame: Option<ShmFrame>,
    pub width: u32,
    pub height: u32,
    pub gl_format: gl::types::GLenum,
    pub frame_ready: bool,
}

pub struct ShmFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl CaptureState {
    pub fn new() -> Self {
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
    pub compositor: Option<WlCompositor>,
    pub xdg_wm_base: Option<XdgWmBase>,
    pub seat: Option<WlSeat>,
    pub decoration_mgr: Option<ZxdgDecorationManagerV1>,

    pub surface: Option<WlSurface>,
    pub xdg_surface: Option<XdgSurface>,
    pub xdg_toplevel: Option<XdgToplevel>,
    pub window_width: i32,
    pub window_height: i32,
    pub window_configured: bool,

    pub pointer: Option<WlPointer>,
    pub pointer_constraints: Option<ZwpPointerConstraintsV1>,
    pub relative_pointer_mgr: Option<ZwpRelativePointerManagerV1>,
    pub locked_pointer: Option<ZwpLockedPointerV1>,
    pub relative_pointer: Option<ZwpRelativePointerV1>,
    pub drag_active: bool,
    pub ptr_x: f64,
    pub ptr_y: f64,
    // Virtual cursor position tracked during drag for wrapping
    pub drag_vx: f64,
    pub drag_vy: f64,

    pub egl_ctx: Option<egl::EglContext>,
    pub gl_res: Option<render::GlResources>,

    pub region_cx: f64,
    pub region_cy: f64,
    pub zoom: f64,

    pub running: bool,
}

impl AppState {
    pub fn new() -> Self {
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
            pointer_constraints: None,
            relative_pointer_mgr: None,
            locked_pointer: None,
            relative_pointer: None,
            drag_active: false,
            ptr_x: 0.0,
            ptr_y: 0.0,
            drag_vx: 0.0,
            drag_vy: 0.0,
            egl_ctx: None,
            gl_res: None,
            region_cx: 0.0,
            region_cy: 0.0,
            zoom: DEFAULT_ZOOM,
            running: true,
        }
    }

    pub fn init_egl(&mut self, wl_display_ptr: *mut std::ffi::c_void) -> Result<()> {
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

    pub fn render(&mut self, cap: &mut CaptureState) -> Result<()> {
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

    pub fn adjust_zoom(&mut self, delta: f64) {
        self.zoom = (self.zoom + delta).clamp(ZOOM_MIN, ZOOM_MAX);
        log::debug!("zoom = {:.2}", self.zoom);
    }

    pub fn pan(&mut self, dx: f64, dy: f64) {
        self.region_cx = (self.region_cx - dx / self.zoom).max(0.0);
        self.region_cy = (self.region_cy - dy / self.zoom).max(0.0);
    }
}
