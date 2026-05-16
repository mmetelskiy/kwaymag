use std::{
    fs::{File, OpenOptions},
    os::unix::io::{AsFd, AsRawFd, OwnedFd},
    path::Path,
};

use anyhow::{Context, Result};
use drm_fourcc::DrmFourcc;
use gbm::{BufferObjectFlags, Device as GbmDevice};
use nix::{
    sys::mman::{mmap, munmap, MapFlags, ProtFlags},
    unistd::ftruncate,
};
use wayland_client::{
    protocol::{wl_buffer::WlBuffer, wl_shm::WlShm, wl_shm_pool::WlShmPool},
    QueueHandle,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::{egl::EglContext, AppState};

/// DMA-BUF backed buffer: the compositor writes screen pixels directly into
/// our GBM buffer object. Zero CPU copies on the capture path.
pub struct DmaBufBuffer {
    /// GBM device (owns the DRM file)
    _device: GbmDevice<File>,
    /// GBM buffer object allocated for the capture size
    _bo: gbm::BufferObject<()>,
    /// DMA-BUF fd (dup'd from the BO)
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub fourcc: u32,
    /// The EGL image wrapping the DMA-BUF (lazily set after EGL is ready)
    pub egl_image: Option<khronos_egl::Image>,
    /// Wayland buffer backed by the DMA-BUF
    pub wl_buffer: WlBuffer,
}

/// Shared-memory backed buffer: the compositor copies pixels to a mmapped
/// region which we then upload to a GL texture.
pub struct ShmBuffer {
    // wl_pool must be kept alive for as long as wl_buffer is in use
    #[allow(dead_code)]
    pub wl_pool: WlShmPool,
    pub wl_buffer: WlBuffer,
    pub ptr: *mut u8,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)]
    pub stride: u32,
    pub gl_format: gl::types::GLenum,
}

unsafe impl Send for ShmBuffer {}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = munmap(
                std::ptr::NonNull::new(self.ptr as *mut _).unwrap(),
                self.size,
            );
        }
    }
}

impl ShmBuffer {
    pub fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
    }
}

pub enum AppBuffer {
    DmaBuf(DmaBufBuffer),
    Shm(ShmBuffer),
}

impl AppBuffer {
    pub fn wl_buffer(&self) -> &WlBuffer {
        match self {
            AppBuffer::DmaBuf(b) => &b.wl_buffer,
            AppBuffer::Shm(b) => &b.wl_buffer,
        }
    }
}

/// Try to allocate a DMA-BUF buffer.
///
/// Returns `None` if DMA-BUF is not supported or allocation fails, so the
/// caller can fall back to SHM.
pub fn try_alloc_dmabuf(
    drm_path: &Path,
    width: u32,
    height: u32,
    formats: &[(u32, u64)],
    dmabuf_global: &ZwpLinuxDmabufV1,
    qh: &QueueHandle<AppState>,
) -> Option<Result<DmaBufBuffer>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(drm_path)
        .ok()?;

    let device = GbmDevice::new(file).ok()?;

    // Pick first advertised DRM format/modifier pair
    let (fourcc_raw, modifier) = pick_format(formats)?;
    let fourcc = DrmFourcc::try_from(fourcc_raw).ok()?;

    let bo = if modifier == u64::MAX {
        // DRM_FORMAT_MOD_INVALID: allocate without modifier constraint
        device
            .create_buffer_object::<()>(width, height, fourcc, BufferObjectFlags::empty())
            .ok()?
    } else {
        device
            .create_buffer_object_with_modifiers::<()>(
                width,
                height,
                fourcc,
                std::iter::once(gbm::Modifier::try_from(modifier).ok()?),
            )
            .ok()?
    };

    let actual_modifier: u64 = bo.modifier().into();
    let stride = bo.stride();
    let offset = bo.offset(0);
    // fd_for_plane returns a dup'd OwnedFd
    let fd: OwnedFd = bo.fd_for_plane(0).ok()?;

    // Create a wl_buffer backed by the DMA-BUF via zwp_linux_dmabuf_v1
    let params: ZwpLinuxBufferParamsV1 = dmabuf_global.create_params(qh, ());
    params.add(
        fd.as_fd(),
        0,
        offset,
        stride,
        (actual_modifier >> 32) as u32,
        actual_modifier as u32,
    );
    let wl_buffer = params.create_immed(
        width as i32,
        height as i32,
        fourcc_raw,
        wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );

    Some(Ok(DmaBufBuffer {
        _device: device,
        _bo: bo,
        fd,
        width,
        height,
        stride,
        offset,
        modifier: actual_modifier,
        fourcc: fourcc_raw,
        egl_image: None,
        wl_buffer,
    }))
}

/// Allocate an SHM buffer of the required size.
pub fn alloc_shm(
    shm_global: &WlShm,
    width: u32,
    height: u32,
    shm_formats: &[u32],
    qh: &QueueHandle<AppState>,
) -> Result<ShmBuffer> {
    // Pick GL-compatible SHM format.
    // WL_SHM_FORMAT_ARGB8888 = 0 (BGRA in memory on LE) → GL_BGRA_EXT
    // WL_SHM_FORMAT_XRGB8888 = 1 (BGRX in memory on LE) → GL_BGRA_EXT (ignore alpha)
    let (wl_fmt, gl_format): (u32, gl::types::GLenum) = shm_formats
        .iter()
        .find_map(|&f| {
            if f == 0 || f == 1 {
                // 0 = ARGB8888, 1 = XRGB8888 — both use BGRA byte layout
                Some((f, 0x80E1)) // GL_BGRA_EXT = 0x80E1
            } else {
                None
            }
        })
        .unwrap_or((1, 0x80E1)); // default XRGB8888

    let stride = width * 4;
    let size = (stride * height) as usize;

    // Create an anonymous memfd for the SHM pool
    let memfd: OwnedFd = nix::sys::memfd::memfd_create(
        std::ffi::CStr::from_bytes_with_nul(b"kmag-shm\0").unwrap(),
        nix::sys::memfd::MemFdCreateFlag::empty(),
    )
    .context("memfd_create")?;

    ftruncate(&memfd, size as i64).context("ftruncate")?;

    let ptr = unsafe {
        mmap(
            None,
            std::num::NonZeroUsize::new(size).unwrap(),
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            &memfd,
            0,
        )
        .context("mmap shm")?
        .as_ptr() as *mut u8
    };

    let pool = shm_global.create_pool(memfd.as_fd(), size as i32, qh, ());
    let wl_buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wayland_client::protocol::wl_shm::Format::try_from(wl_fmt)
            .unwrap_or(wayland_client::protocol::wl_shm::Format::Xrgb8888),
        qh,
        (),
    );

    Ok(ShmBuffer {
        wl_pool: pool,
        wl_buffer,
        ptr,
        size,
        width,
        height,
        stride,
        gl_format,
    })
}

/// Attach the DMA-BUF to the EGL context if not already done.
pub fn ensure_egl_image(buf: &mut DmaBufBuffer, egl: &EglContext) -> Result<khronos_egl::Image> {
    if let Some(img) = buf.egl_image {
        return Ok(img);
    }
    let img = egl
        .create_dmabuf_image(
            buf.width,
            buf.height,
            buf.fourcc,
            buf.fd.as_raw_fd(),
            buf.offset,
            buf.stride,
            buf.modifier,
        )
        .context("create EGL image from DMA-BUF")?;
    buf.egl_image = Some(img);
    Ok(img)
}

/// Prefer XRGB/ARGB formats (common, GL-friendly).
fn pick_format(formats: &[(u32, u64)]) -> Option<(u32, u64)> {
    const XRGB8888: u32 = 0x34325258; // DRM_FORMAT_XRGB8888
    const ARGB8888: u32 = 0x34325241; // DRM_FORMAT_ARGB8888

    formats
        .iter()
        .find(|(f, _)| *f == XRGB8888)
        .or_else(|| formats.iter().find(|(f, _)| *f == ARGB8888))
        .or_else(|| formats.first())
        .copied()
}
