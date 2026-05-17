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
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::{
    wp::{
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
            zwp_pointer_constraints_v1::{self, ZwpPointerConstraintsV1},
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
    },
    xdg::{
        decoration::zv1::client::{
            zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
            zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
        },
        shell::client::{
            xdg_surface::{self, XdgSurface},
            xdg_toplevel::{self, XdgToplevel},
            xdg_wm_base::{self, XdgWmBase},
        },
    },
};

use crate::state::{AppState, ZOOM_STEP};

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
            "zwp_pointer_constraints_v1" => {
                state.pointer_constraints = Some(registry.bind(name, version.min(1), qh, ()));
            }
            "zwp_relative_pointer_manager_v1" => {
                state.relative_pointer_mgr = Some(registry.bind(name, version.min(1), qh, ()));
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
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Button { button, state: btn_state, .. } => {
                if button != 272 {
                    return;
                }
                let pressed =
                    btn_state == wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed);
                state.drag_active = pressed;

                if pressed {
                    // Lock pointer and start receiving relative motion.
                    state.drag_vx = state.ptr_x;
                    state.drag_vy = state.ptr_y;
                    if let (Some(constraints), Some(rel_mgr), Some(surface), Some(stored_ptr)) = (
                        state.pointer_constraints.as_ref(),
                        state.relative_pointer_mgr.as_ref(),
                        state.surface.as_ref(),
                        state.pointer.as_ref(),
                    ) {
                        let locked = constraints.lock_pointer(
                            surface,
                            stored_ptr,
                            None,
                            zwp_pointer_constraints_v1::Lifetime::Persistent,
                            qh,
                            (),
                        );
                        state.locked_pointer = Some(locked);
                        state.relative_pointer =
                            Some(rel_mgr.get_relative_pointer(stored_ptr, qh, ()));
                    }
                } else {
                    if let Some(lp) = state.locked_pointer.take() {
                        lp.destroy();
                    }
                    if let Some(rp) = state.relative_pointer.take() {
                        rp.destroy();
                    }
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                // Track absolute position (used to seed drag_vx/drag_vy on press).
                // Panning is handled by ZwpRelativePointerV1 while locked.
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

// Relative motion: pan the view and wrap the virtual cursor at window edges.
impl Dispatch<ZwpRelativePointerV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        else {
            return;
        };

        state.pan(dx_unaccel, dy_unaccel);

        state.drag_vx += dx_unaccel;
        state.drag_vy += dy_unaccel;

        let win_w = state.window_width as f64;
        let win_h = state.window_height as f64;
        // Leave a 1 px margin so the wrapped position is clearly inside the window.
        const MARGIN: f64 = 1.0;

        let mut needs_warp = false;
        if state.drag_vx < MARGIN {
            state.drag_vx += win_w - 2.0 * MARGIN;
            needs_warp = true;
        } else if state.drag_vx > win_w - MARGIN {
            state.drag_vx -= win_w - 2.0 * MARGIN;
            needs_warp = true;
        }
        if state.drag_vy < MARGIN {
            state.drag_vy += win_h - 2.0 * MARGIN;
            needs_warp = true;
        } else if state.drag_vy > win_h - MARGIN {
            state.drag_vy -= win_h - 2.0 * MARGIN;
            needs_warp = true;
        }

        if needs_warp {
            let vx = state.drag_vx;
            let vy = state.drag_vy;
            // Set the position hint and destroy+relock: the compositor moves the
            // cursor to the hinted position when the lock is released, then the
            // new lock freezes it there — giving the "teleport" visual.
            if let Some(lp) = &state.locked_pointer {
                lp.set_cursor_position_hint(vx, vy);
            }
            if let Some(lp) = state.locked_pointer.take() {
                lp.destroy();
            }
            if let (Some(constraints), Some(surface), Some(pointer)) = (
                state.pointer_constraints.as_ref(),
                state.surface.as_ref(),
                state.pointer.as_ref(),
            ) {
                state.locked_pointer = Some(constraints.lock_pointer(
                    surface,
                    pointer,
                    None,
                    zwp_pointer_constraints_v1::Lifetime::Persistent,
                    qh,
                    (),
                ));
            }
        }
    }
}

delegate_noop!(AppState: ZwpPointerConstraintsV1);
delegate_noop!(AppState: ZwpRelativePointerManagerV1);

// ZwpLockedPointerV1 sends `locked` and `unlocked` events — accept silently.
impl Dispatch<ZwpLockedPointerV1, ()> for AppState {
    fn event(_: &mut Self, _: &ZwpLockedPointerV1, _: zwp_locked_pointer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(AppState: ZxdgDecorationManagerV1);
delegate_noop!(AppState: WlCompositor);
delegate_noop!(AppState: WlShmPool);
delegate_noop!(AppState: WlBuffer);
delegate_noop!(AppState: WlKeyboard);
delegate_noop!(AppState: WlShm);
delegate_noop!(AppState: WlOutput);
