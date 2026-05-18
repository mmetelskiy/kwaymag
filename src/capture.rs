use std::{cell::RefCell, os::unix::io::{FromRawFd, OwnedFd}, rc::Rc};

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
        pod::{object, property, serialize::PodSerializer, ChoiceValue, Object, Pod, Property, PropertyFlags, Value},
        utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, SpaTypes},
    },
    stream::{StreamFlags, StreamListener, StreamRc},
};

use crate::state::{CaptureState, DmaBufFrame, Frame, ShmFrame};

// GL_BGRA_EXT — byte order is B G R A in memory on little-endian
pub const GL_BGRA_EXT: gl::types::GLenum = 0x80E1;

// Maps a PipeWire VideoFormat to its DRM fourcc code.
// fourcc_code(a,b,c,d) = a | (b<<8) | (c<<16) | (d<<24)
fn video_format_to_drm_fourcc(fmt: VideoFormat) -> u32 {
    match fmt {
        VideoFormat::BGRx => 0x34325258, // DRM_FORMAT_XRGB8888 'X','R','2','4'
        VideoFormat::BGRA => 0x34325241, // DRM_FORMAT_ARGB8888 'A','R','2','4'
        VideoFormat::RGBx => 0x34324258, // DRM_FORMAT_XBGR8888 'X','B','2','4'
        VideoFormat::RGBA => 0x34324241, // DRM_FORMAT_ABGR8888 'A','B','2','4'
        _ => 0x34325258,
    }
}

fn build_buffers_pod() -> Vec<u8> {
    let obj = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: pipewire::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Int(
                (1 << pipewire::spa::sys::SPA_DATA_DmaBuf)
                    | (1 << pipewire::spa::sys::SPA_DATA_MemFd),
            ),
        }],
    };
    let mut bytes = Vec::new();
    PodSerializer::serialize(std::io::Cursor::new(&mut bytes), &Value::Object(obj))
        .expect("pod serialization");
    bytes
}

fn build_video_format_pod() -> Vec<u8> {
    // DRM_FORMAT_MOD_INVALID signals "I accept any modifier" — required by KWin
    // (and other compositors) to enable DmaBuf transport instead of falling back to SHM.
    const DRM_FORMAT_MOD_INVALID: i64 = 0x00ff_ffff_ffff_ffff_u64 as i64;

    let mut pod_object = object! {
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

    pod_object.properties.push(Property {
        key: FormatProperties::VideoModifier.as_raw(),
        flags: PropertyFlags::empty(),
        value: Value::Choice(ChoiceValue::Long(Choice::<i64>(
            ChoiceFlags::empty(),
            ChoiceEnum::<i64>::Enum {
                default: DRM_FORMAT_MOD_INVALID,
                alternatives: vec![DRM_FORMAT_MOD_INVALID],
            },
        ))),
    });

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
        "kwaymag-capture",
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
        .param_changed(move |stream, _, id, pod| {
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
            log::info!(
                "PipeWire format: {:?} {}×{} modifier={:#018x}",
                fmt, size.width, size.height, info.modifier()
            );

            let mut cap = cap_pc.borrow_mut();
            cap.width = size.width;
            cap.height = size.height;
            cap.gl_format = match fmt {
                VideoFormat::RGBA | VideoFormat::RGBx => gl::RGBA,
                _ => GL_BGRA_EXT,
            };
            cap.fourcc = video_format_to_drm_fourcc(fmt);
            cap.modifier = info.modifier();
            drop(cap);

            // Advertise that we accept DmaBuf transport so the compositor
            // can skip the SHM copy and share its GPU buffer directly.
            let buffers_obj = Object {
                type_: SpaTypes::ObjectParamBuffers.as_raw(),
                id: ParamType::Buffers.as_raw(),
                properties: vec![Property {
                    key: pipewire::spa::sys::SPA_PARAM_BUFFERS_dataType,
                    flags: PropertyFlags::empty(),
                    value: Value::Int(
                        (1 << pipewire::spa::sys::SPA_DATA_DmaBuf)
                            | (1 << pipewire::spa::sys::SPA_DATA_MemFd),
                    ),
                }],
            };
            let mut bytes = Vec::new();
            if PodSerializer::serialize(
                std::io::Cursor::new(&mut bytes),
                &Value::Object(buffers_obj),
            )
            .is_ok()
            {
                if let Some(pod) = Pod::from_bytes(&bytes) {
                    match stream.update_params(&mut [pod]) {
                        Ok(()) => log::info!("update_params: DmaBuf transport requested"),
                        Err(e) => log::warn!("update_params (buffers): {e}"),
                    }
                }
            }
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

            match data.type_() {
                DataType::DmaBuf => {
                    match nix::unistd::dup(data.fd()) {
                        Ok(duped_fd) => {
                            let first = cap.frame.is_none();
                            let owned_fd = unsafe { OwnedFd::from_raw_fd(duped_fd) };
                            cap.frame = Some(Frame::DmaBuf(DmaBufFrame {
                                fd: owned_fd,
                                width: cap.width,
                                height: cap.height,
                                fourcc: cap.fourcc,
                                modifier: cap.modifier,
                                offset: data.chunk().offset(),
                                stride: data.chunk().stride() as u32,
                            }));
                            if first {
                                log::info!(
                                    "frame transport: DmaBuf (zero-copy) stride={}",
                                    data.chunk().stride()
                                );
                            }
                            cap.frame_ready = true;
                        }
                        Err(e) => log::warn!("dup DmaBuf fd: {e}"),
                    }
                }
                DataType::MemFd | DataType::MemPtr => {
                    let chunk_size = data.chunk().size() as usize;
                    if chunk_size == 0 {
                        return;
                    }
                    if let Some(slice) = data.data() {
                        let first = cap.frame.is_none();
                        let copy = slice[..chunk_size.min(slice.len())].to_vec();
                        cap.frame = Some(Frame::Shm(ShmFrame {
                            data: copy,
                            width: cap.width,
                            height: cap.height,
                        }));
                        if first {
                            log::info!(
                                "frame transport: SHM ({:?}) {} bytes",
                                data.type_(), chunk_size
                            );
                        }
                        cap.frame_ready = true;
                    }
                }
                _ => {}
            }
        })
        .register()
        .context("stream listener register")?;

    // Connect with format offer + upfront DmaBuf transport preference.
    // Passing the buffers pod at connect time (not only via update_params) lets
    // the portal see our DmaBuf capability before it decides the buffer type.
    let format_bytes = build_video_format_pod();
    let buffers_bytes = build_buffers_pod();
    let format_pod = Pod::from_bytes(&format_bytes).context("invalid format pod")?;
    let buffers_pod = Pod::from_bytes(&buffers_bytes).context("invalid buffers pod")?;
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format_pod, buffers_pod],
        )
        .context("stream connect")?;

    Ok((mainloop, stream, _listener))
}
