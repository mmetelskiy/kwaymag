use std::{cell::RefCell, os::unix::io::OwnedFd, rc::Rc};

use anyhow::{Context, Result};
use pipewire::{
    context::ContextRc,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        buffer::DataType,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
            ParamType,
        },
        pod::{object, property, serialize::PodSerializer, Pod, Value},
        utils::{Direction, SpaTypes},
    },
    stream::{StreamFlags, StreamListener, StreamRc},
};

use crate::state::{CaptureState, ShmFrame};

// GL_BGRA_EXT — byte order is B G R A in memory on little-endian
pub const GL_BGRA_EXT: gl::types::GLenum = 0x80E1;

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

pub fn connect_pipewire_stream(
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
