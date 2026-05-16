use anyhow::{anyhow, bail, Result};

use crate::egl::{EglContext, GlEGLImageTargetTexture2DOES};

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
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
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

    /// Blit the captured source region (in screen coordinates, y=0 at top) to
    /// the window draw FBO (y=0 at bottom in GL convention).
    ///
    /// The Y axis is flipped to convert from screen coords (top-origin) to
    /// GL window coords (bottom-origin). The DMA-BUF EGL image has its first
    /// row mapped to GL FBO y=0 (bottom), so a dst-flip is required.
    pub fn blit(
        &self,
        src_x: i32,
        src_y: i32,
        src_w: i32,
        src_h: i32,
        win_w: i32,
        win_h: i32,
    ) {
        unsafe {
            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, self.src_fbo);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, 0);

            // Flip destination Y so that screen-top (FBO y=0 in source)
            // appears at window top (GL y=win_h in destination).
            gl::BlitFramebuffer(
                src_x,
                src_y,
                src_x + src_w,
                src_y + src_h,
                0,
                win_h, // dst y0 = top of window
                win_w,
                0, // dst y1 = bottom of window
                gl::COLOR_BUFFER_BIT,
                gl::LINEAR,
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
