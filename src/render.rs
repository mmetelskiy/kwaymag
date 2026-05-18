use anyhow::{anyhow, bail, Result};

use crate::egl::{EglContext, GlEGLImageTargetTexture2DOES};

pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub struct Size {
    pub w: i32,
    pub h: i32,
}

pub struct GlResources {
    pub src_fbo: gl::types::GLuint,
    pub texture: gl::types::GLuint,
    #[allow(dead_code)]
    gl_egl_image_target: GlEGLImageTargetTexture2DOES,
}

impl GlResources {
    pub fn new(egl: &EglContext) -> Result<Self> {
        let gl_egl_image_target = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .ok_or_else(|| anyhow!("glEGLImageTargetTexture2DOES not available"))?;
        let gl_egl_image_target: GlEGLImageTargetTexture2DOES =
            unsafe { std::mem::transmute(gl_egl_image_target) };

        let mut src_fbo = 0;
        let mut texture = 0;
        unsafe {
            gl::GenFramebuffers(1, &mut src_fbo);
            gl::GenTextures(1, &mut texture);

            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::BindTexture(gl::TEXTURE_2D, 0);

            check_gl_error("init")?;
        }

        Ok(Self {
            src_fbo,
            texture,
            gl_egl_image_target,
        })
    }

    #[allow(dead_code)]
    pub fn bind_egl_image(&self, egl_image: khronos_egl::Image) {
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.texture);
            (self.gl_egl_image_target)(gl::TEXTURE_2D, egl_image.as_ptr());
            gl::BindTexture(gl::TEXTURE_2D, 0);

            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, self.src_fbo);
            gl::FramebufferTexture2D(
                gl::READ_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                self.texture,
                0,
            );
            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, 0);
        }
    }

    /// Upload SHM pixel data to the texture and set up the source FBO.
    /// `format` is the GL internal/external format (GL_RGBA or GL_BGRA_EXT).
    pub fn upload_shm(
        &self,
        data: &[u8],
        width: u32,
        height: u32,
        gl_format: gl::types::GLenum,
    ) {
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.texture);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA as i32,
                width as i32,
                height as i32,
                0,
                gl_format,
                gl::UNSIGNED_BYTE,
                data.as_ptr() as *const _,
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);

            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, self.src_fbo);
            gl::FramebufferTexture2D(
                gl::READ_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                self.texture,
                0,
            );
            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, 0);
        }
    }

    /// Blit a source region into a destination rectangle within the window.
    ///
    /// All coordinates are in screen/window space with y=0 at the top.
    /// The destination Y axis is flipped for GL convention internally.
    /// Areas of the window outside `dst` are cleared to black, so the caller
    /// can pass a smaller destination when the source is clipped at a border.
    pub fn blit(&self, src: Rect, dst: Rect, win_h: i32) {
        unsafe {
            // Clear to black first so the border area (outside dst) is black.
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, 0);
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);

            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, self.src_fbo);

            // Y-flip: screen y=0 is top; GL y=0 is bottom.
            // dst.y (screen top of blit) → GL y = win_h - dst.y
            // dst.y + dst.h (screen bottom) → GL y = win_h - (dst.y + dst.h)
            gl::BlitFramebuffer(
                src.x,
                src.y,
                src.x + src.w,
                src.y + src.h,
                dst.x,
                win_h - dst.y,
                dst.x + dst.w,
                win_h - (dst.y + dst.h),
                gl::COLOR_BUFFER_BIT,
                gl::NEAREST,
            );

            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, 0);
        }
    }
}

impl Drop for GlResources {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteFramebuffers(1, &self.src_fbo);
            gl::DeleteTextures(1, &self.texture);
        }
    }
}

pub fn check_gl_error(label: &str) -> Result<()> {
    let err = unsafe { gl::GetError() };
    if err != gl::NO_ERROR {
        bail!("GL error after {label}: 0x{err:x}");
    }
    Ok(())
}
