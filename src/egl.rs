use anyhow::{anyhow, Context, Result};
use khronos_egl as egl;
use wayland_client::backend::ObjectId;
use wayland_egl::WlEglSurface;

#[allow(dead_code)]
pub const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
#[allow(dead_code)] pub const EGL_LINUX_DRM_FOURCC_EXT: usize = 0x3271;
#[allow(dead_code)] pub const EGL_DMA_BUF_PLANE0_FD_EXT: usize = 0x3272;
#[allow(dead_code)] pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: usize = 0x3273;
#[allow(dead_code)] pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: usize = 0x3274;
#[allow(dead_code)] pub const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: usize = 0x3443;
#[allow(dead_code)] pub const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: usize = 0x3444;

pub type GlEGLImageTargetTexture2DOES =
    unsafe extern "C" fn(target: gl::types::GLenum, image: *mut std::ffi::c_void);

pub struct EglContext {
    pub instance: egl::DynamicInstance<egl::EGL1_5>,
    pub display: egl::Display,
    // Kept alive so EGL doesn't release the context
    #[allow(dead_code)]
    pub context: egl::Context,
    pub surface: egl::Surface,
    // Must outlive surface
    _wl_egl_surface: WlEglSurface,
}

impl EglContext {
    /// Create an EGL context on the given Wayland display pointer.
    pub fn new(
        wl_display_ptr: *mut std::ffi::c_void,
        surface_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<Self> {
        let lib = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
        }
        .context("load libEGL.so.1")?;

        let display = unsafe {
            lib.get_display(wl_display_ptr as egl::NativeDisplayType)
        }
        .ok_or_else(|| anyhow!("eglGetDisplay failed"))?;

        lib.initialize(display).context("eglInitialize")?;

        lib.bind_api(egl::OPENGL_ES_API).context("eglBindAPI(GLES)")?;

        #[rustfmt::skip]
        let config_attribs: &[i32] = &[
            egl::RED_SIZE,   8,
            egl::GREEN_SIZE, 8,
            egl::BLUE_SIZE,  8,
            egl::ALPHA_SIZE, 0,
            egl::SURFACE_TYPE, egl::WINDOW_BIT as i32,
            egl::RENDERABLE_TYPE, egl::OPENGL_ES3_BIT as i32,
            egl::NONE,
        ];

        let config = lib
            .choose_first_config(display, config_attribs)
            .context("eglChooseConfig")?
            .ok_or_else(|| anyhow!("no matching EGL config"))?;

        #[rustfmt::skip]
        let ctx_attribs: &[i32] = &[
            egl::CONTEXT_CLIENT_VERSION, 3,
            egl::NONE,
        ];

        let context = lib
            .create_context(display, config, None, ctx_attribs)
            .context("eglCreateContext")?;

        let wl_egl_surface = WlEglSurface::new(surface_id, width, height)
            .context("WlEglSurface::new")?;

        let surface = unsafe {
            lib.create_window_surface(
                display,
                config,
                wl_egl_surface.ptr() as egl::NativeWindowType,
                None,
            )
        }
        .context("eglCreateWindowSurface")?;

        lib.make_current(display, Some(surface), Some(surface), Some(context))
            .context("eglMakeCurrent")?;

        // Load OpenGL ES functions via EGL proc address
        gl::load_with(|name| {
            lib.get_proc_address(name)
                .map(|f| f as *const _)
                .unwrap_or(std::ptr::null())
        });

        if let Ok(exts) = lib.query_string(Some(display), egl::EXTENSIONS) {
            let exts = exts.to_string_lossy();
            let dmabuf_import = exts.contains("EGL_EXT_image_dma_buf_import");
            let dmabuf_modifiers = exts.contains("EGL_EXT_image_dma_buf_import_modifiers");
            log::info!(
                "EGL DmaBuf support: import={dmabuf_import} modifiers={dmabuf_modifiers}"
            );
            if !dmabuf_import {
                log::warn!("EGL_EXT_image_dma_buf_import not available — DmaBuf frames will fail");
            }
        }

        Ok(Self {
            instance: lib,
            display,
            context,
            surface,
            _wl_egl_surface: wl_egl_surface,
        })
    }

    pub fn swap_buffers(&self) -> Result<()> {
        self.instance
            .swap_buffers(self.display, self.surface)
            .context("eglSwapBuffers")
    }

    pub fn resize(&self, width: i32, height: i32) {
        self._wl_egl_surface.resize(width, height, 0, 0);
    }

    pub fn create_dmabuf_image(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
        fd: std::os::unix::io::RawFd,
        offset: u32,
        stride: u32,
        modifier: u64,
    ) -> Result<egl::Image> {
        let null_ctx = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        let null_buf = unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };

        let modifier_lo = (modifier & 0xffff_ffff) as usize;
        let modifier_hi = (modifier >> 32) as usize;

        #[rustfmt::skip]
        let attribs: &[usize] = &[
            egl::WIDTH as usize,              width as usize,
            egl::HEIGHT as usize,             height as usize,
            EGL_LINUX_DRM_FOURCC_EXT,         fourcc as usize,
            EGL_DMA_BUF_PLANE0_FD_EXT,        fd as usize,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,    offset as usize,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,     stride as usize,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, modifier_lo,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, modifier_hi,
            egl::NONE as usize,
        ];

        // eglCreateImage (EGL 1.5) uses usize-sized attribs
        self.instance
            .create_image(
                self.display,
                null_ctx,
                EGL_LINUX_DMA_BUF_EXT,
                null_buf,
                attribs,
            )
            .context("eglCreateImage for DMA-BUF")
    }

    pub fn get_proc_address(&self, name: &str) -> Option<extern "system" fn()> {
        self.instance.get_proc_address(name)
    }
}
