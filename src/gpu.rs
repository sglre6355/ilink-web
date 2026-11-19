//! H.264 encoding on the GPU through FFmpeg, with Vulkan Video or VA-API.
//!
//! Vulkan Video works the same way on AMD, Intel and NVIDIA GPUs whose
//! drivers support encoding; VA-API covers AMD and Intel ones that do not.
//!
//! FFmpeg's hardware encoders cannot change their bitrate once open, so a new
//! encoder is opened when the target moves far enough; it starts with a
//! keyframe, which costs one larger picture.

use std::ffi::{CStr, c_int};
use std::ptr::{self, null, null_mut};
use std::sync::Once;
use std::time::{Duration, Instant};

use ffmpeg_sys_next as ff;

use crate::encode::{Encoder, KEYFRAME_INTERVAL, Picture};

/// `AV_PROFILE_H264_CONSTRAINED_BASELINE`: baseline (66) with the constrained flag.
const CONSTRAINED_BASELINE: c_int = 66 | (1 << 9);
/// How far the target must move from the current bitrate, as a fraction of
/// it, before the encoder is reopened.
const REOPEN_CHANGE: f64 = 0.25;
/// The least time between reopenings when lowering and raising the bitrate.
/// Lowering is urgent, as a too-high rate builds up delay on the network.
const REOPEN_DOWN_AFTER: Duration = Duration::from_secs(1);
const REOPEN_UP_AFTER: Duration = Duration::from_secs(4);

/// An encoder option: name and value.
type Opt = (&'static CStr, &'static CStr);

/// The GPU interface to encode through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Vulkan,
    Vaapi,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Self::Vulkan => "Vulkan",
            Self::Vaapi => "VA-API",
        }
    }

    fn device_type(self) -> ff::AVHWDeviceType {
        match self {
            Self::Vulkan => ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
            Self::Vaapi => ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        }
    }

    fn pixel_format(self) -> ff::AVPixelFormat {
        match self {
            Self::Vulkan => ff::AVPixelFormat::AV_PIX_FMT_VULKAN,
            Self::Vaapi => ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
        }
    }

    fn encoder(self) -> &'static CStr {
        match self {
            Self::Vulkan => c"h264_vulkan",
            Self::Vaapi => c"h264_vaapi",
        }
    }

    /// Encoder options for low delay: hand each picture back as soon as it
    /// is done rather than keeping one in flight, and leave out SEI and the
    /// other optional units. The Vulkan hints ask drivers to tune for live
    /// camera video.
    fn options(self) -> &'static [Opt] {
        match self {
            Self::Vulkan => &[
                (c"async_depth", c"1"),
                (c"units", c"0"),
                (c"tune", c"ull"),
                (c"usage", c"conference"),
                (c"content", c"camera"),
            ],
            Self::Vaapi => &[(c"async_depth", c"1"), (c"sei", c"0")],
        }
    }

    /// The option for constant bitrate, which keeps keyframes within the
    /// rate-control buffer. Vulkan drivers otherwise tend to pick a variable
    /// rate and make keyframes several times larger; VA-API picks constant
    /// bitrate by itself when the bitrate and maximum are equal.
    fn constant_bitrate(self) -> Option<Opt> {
        match self {
            Self::Vulkan => Some((c"rc_mode", c"cbr")),
            Self::Vaapi => None,
        }
    }
}

pub struct Gpu {
    api: Api,
    device: *mut ff::AVBufferRef,
    frames: *mut ff::AVBufferRef,
    codec: *mut ff::AVCodecContext,
    upload: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    width: usize,
    height: usize,
    fps: u32,
    bps: u32,
    target: u32,
    opened: Instant,
}

// SAFETY: the FFmpeg objects are owned by this value alone and are only used
// through `&mut self`, so moving it to another thread is fine.
unsafe impl Send for Gpu {}

fn check(ret: c_int, what: &str) -> Result<c_int, String> {
    if ret >= 0 {
        return Ok(ret);
    }
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: av_strerror writes a NUL-terminated string into `buf`.
    let message = unsafe {
        ff::av_strerror(ret, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    };
    Err(format!("{what}: {message}"))
}

impl Gpu {
    pub fn new(api: Api, width: usize, height: usize, fps: u32, bps: u32) -> Result<Self, String> {
        static QUIET: Once = Once::new();
        // SAFETY: plain FFI calls; every pointer is checked before use and
        // freed by `Drop` if a later step fails.
        unsafe {
            QUIET.call_once(|| ff::av_log_set_level(ff::AV_LOG_ERROR));
            let mut this = Self {
                api,
                device: null_mut(),
                frames: null_mut(),
                codec: null_mut(),
                upload: null_mut(),
                packet: null_mut(),
                width,
                height,
                fps,
                bps,
                target: bps,
                opened: Instant::now(),
            };
            check(
                ff::av_hwdevice_ctx_create(
                    &mut this.device,
                    api.device_type(),
                    null(),
                    null_mut(),
                    0,
                ),
                "opening the device",
            )?;

            this.frames = ff::av_hwframe_ctx_alloc(this.device);
            if this.frames.is_null() {
                return Err("allocating GPU frames".into());
            }
            let frames = (*this.frames).data.cast::<ff::AVHWFramesContext>();
            (*frames).format = api.pixel_format();
            (*frames).sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            (*frames).width = c_int::try_from(width).map_err(|e| e.to_string())?;
            (*frames).height = c_int::try_from(height).map_err(|e| e.to_string())?;
            (*frames).initial_pool_size = 8;
            check(
                ff::av_hwframe_ctx_init(this.frames),
                "setting up GPU frames",
            )?;

            this.upload = ff::av_frame_alloc();
            this.packet = ff::av_packet_alloc();
            if this.upload.is_null() || this.packet.is_null() {
                return Err("allocating frames".into());
            }
            (*this.upload).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*this.upload).width = (*frames).width;
            (*this.upload).height = (*frames).height;
            check(
                ff::av_frame_get_buffer(this.upload, 0),
                "allocating a frame",
            )?;

            this.open_codec()?;
            Ok(this)
        }
    }

    /// Opens (or reopens) the encoder at `self.bps`, in constant bitrate
    /// mode if the driver offers it and in its own choice of mode otherwise.
    unsafe fn open_codec(&mut self) -> Result<(), String> {
        // SAFETY: as for `open_codec_with`.
        unsafe {
            match self.api.constant_bitrate() {
                Some(cbr) => self
                    .open_codec_with(Some(cbr))
                    .or_else(|_| self.open_codec_with(None)),
                None => self.open_codec_with(None),
            }
        }
    }

    unsafe fn open_codec_with(&mut self, rate_control: Option<Opt>) -> Result<(), String> {
        // SAFETY: called with a valid device and frames context.
        unsafe {
            ff::avcodec_free_context(&mut self.codec);
            let name = self.api.encoder();
            let encoder = ff::avcodec_find_encoder_by_name(name.as_ptr());
            if encoder.is_null() {
                return Err(format!("FFmpeg has no {} encoder", name.to_string_lossy()));
            }
            self.codec = ff::avcodec_alloc_context3(encoder);
            if self.codec.is_null() {
                return Err("allocating the encoder".into());
            }
            let c = &mut *self.codec;
            c.width = self.width as c_int;
            c.height = self.height as c_int;
            c.time_base = ff::AVRational { num: 1, den: 1000 };
            c.framerate = ff::AVRational {
                num: self.fps as c_int,
                den: 1,
            };
            c.pix_fmt = self.api.pixel_format();
            c.profile = CONSTRAINED_BASELINE;
            c.max_b_frames = 0;
            c.gop_size = (self.fps * KEYFRAME_INTERVAL) as c_int;
            c.bit_rate = i64::from(self.bps);
            c.rc_max_rate = i64::from(self.bps);
            // A small rate-control buffer (200 ms of video) keeps keyframes
            // from growing so large that they take long to send.
            c.rc_buffer_size = (self.bps / 5) as c_int;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_SMPTE170M;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_SMPTE170M;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE170M;
            c.hw_frames_ctx = ff::av_buffer_ref(self.frames);
            for (key, value) in self.api.options().iter().chain(&rate_control) {
                ff::av_opt_set(c.priv_data, key.as_ptr(), value.as_ptr(), 0);
            }
            check(
                ff::avcodec_open2(self.codec, encoder, null_mut()),
                "opening the encoder",
            )?;
            self.opened = Instant::now();
            Ok(())
        }
    }

    /// Reopens the encoder at the target bitrate if it has moved far enough.
    fn maybe_reopen(&mut self) -> Result<bool, String> {
        let (now, target) = (f64::from(self.bps), f64::from(self.target));
        if (target - now).abs() < now * REOPEN_CHANGE {
            return Ok(false);
        }
        let wait = if target < now {
            REOPEN_DOWN_AFTER
        } else {
            REOPEN_UP_AFTER
        };
        if self.opened.elapsed() < wait {
            return Ok(false);
        }
        self.bps = self.target;
        // SAFETY: the device and frames context are valid for our lifetime.
        unsafe { self.open_codec()? };
        Ok(true)
    }

    /// Copies an I420 picture into the NV12 upload frame.
    unsafe fn fill_upload(&mut self, picture: &Picture) -> Result<(), String> {
        // SAFETY: the upload frame was allocated for `width` x `height` NV12,
        // which is what `picture` holds (checked by the caller).
        unsafe {
            check(ff::av_frame_make_writable(self.upload), "preparing a frame")?;
            let f = &*self.upload;
            let (w, h) = (self.width, self.height);
            for row in 0..h {
                let dst = f.data[0].offset(row as isize * f.linesize[0] as isize);
                ptr::copy_nonoverlapping(picture.y[row * w..].as_ptr(), dst, w);
            }
            for row in 0..h / 2 {
                let dst = f.data[1].offset(row as isize * f.linesize[1] as isize);
                let dst = std::slice::from_raw_parts_mut(dst, w);
                let (u, v) = (
                    &picture.u[row * w / 2..][..w / 2],
                    &picture.v[row * w / 2..][..w / 2],
                );
                for ((pair, u), v) in dst.as_chunks_mut::<2>().0.iter_mut().zip(u).zip(v) {
                    pair[0] = *u;
                    pair[1] = *v;
                }
            }
            Ok(())
        }
    }
}

impl Encoder for Gpu {
    fn name(&self) -> &'static str {
        self.api.name()
    }

    fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn encode(
        &mut self,
        picture: &Picture,
        pts: Duration,
        keyframe: bool,
    ) -> Result<Vec<u8>, String> {
        if (picture.width, picture.height) != (self.width, self.height) {
            return Err("picture size changed".into());
        }
        // A reopened encoder starts with a keyframe anyway.
        let keyframe = !self.maybe_reopen()? && keyframe;
        // SAFETY: all objects were set up by `new` and stay valid until drop;
        // the GPU frame is freed on every path.
        unsafe {
            self.fill_upload(picture)?;
            let mut gpu = ff::av_frame_alloc();
            if gpu.is_null() {
                return Err("allocating a frame".into());
            }
            let sent = (|| {
                check(
                    ff::av_hwframe_get_buffer(self.frames, gpu, 0),
                    "getting a GPU frame",
                )?;
                check(
                    ff::av_hwframe_transfer_data(gpu, self.upload, 0),
                    "uploading a frame",
                )?;
                (*gpu).pts = i64::try_from(pts.as_millis()).unwrap_or(i64::MAX);
                if keyframe {
                    (*gpu).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
                }
                check(ff::avcodec_send_frame(self.codec, gpu), "encoding")
            })();
            ff::av_frame_free(&mut gpu);
            sent?;

            let mut out = Vec::new();
            loop {
                let ret = ff::avcodec_receive_packet(self.codec, self.packet);
                if ret == ff::AVERROR(ff::EAGAIN) || ret == ff::AVERROR_EOF {
                    break;
                }
                check(ret, "receiving a packet")?;
                let p = &*self.packet;
                out.extend_from_slice(std::slice::from_raw_parts(p.data, p.size as usize));
                ff::av_packet_unref(self.packet);
            }
            Ok(out)
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.target = bps;
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: each free function accepts null and nulls the pointer.
        unsafe {
            ff::avcodec_free_context(&mut self.codec);
            ff::av_frame_free(&mut self.upload);
            ff::av_packet_free(&mut self.packet);
            ff::av_buffer_unref(&mut self.frames);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}
