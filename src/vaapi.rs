//! H.264 encoding on the GPU through FFmpeg's `h264_vaapi` encoder.
//!
//! FFmpeg's VA-API encoder cannot change its bitrate once open, so a new
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

pub struct Vaapi {
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
unsafe impl Send for Vaapi {}

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

impl Vaapi {
    pub fn new(width: usize, height: usize, fps: u32, bps: u32) -> Result<Self, String> {
        static QUIET: Once = Once::new();
        // SAFETY: plain FFI calls; every pointer is checked before use and
        // freed by `Drop` if a later step fails.
        unsafe {
            QUIET.call_once(|| ff::av_log_set_level(ff::AV_LOG_ERROR));
            let mut this = Self {
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
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    null(),
                    null_mut(),
                    0,
                ),
                "opening the VA-API device",
            )?;

            this.frames = ff::av_hwframe_ctx_alloc(this.device);
            if this.frames.is_null() {
                return Err("allocating GPU frames".into());
            }
            let frames = (*this.frames).data.cast::<ff::AVHWFramesContext>();
            (*frames).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
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

    /// Opens (or reopens) the encoder at `self.bps`.
    unsafe fn open_codec(&mut self) -> Result<(), String> {
        // SAFETY: called with a valid device and frames context.
        unsafe {
            ff::avcodec_free_context(&mut self.codec);
            let encoder = ff::avcodec_find_encoder_by_name(c"h264_vaapi".as_ptr());
            if encoder.is_null() {
                return Err("FFmpeg has no h264_vaapi encoder".into());
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
            c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
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
            // Hand each picture back as soon as it is encoded rather than
            // keeping one in flight, and leave out the SEI messages.
            ff::av_opt_set(c.priv_data, c"async_depth".as_ptr(), c"1".as_ptr(), 0);
            ff::av_opt_set(c.priv_data, c"sei".as_ptr(), c"0".as_ptr(), 0);
            check(
                ff::avcodec_open2(self.codec, encoder, null_mut()),
                "opening h264_vaapi",
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

impl Encoder for Vaapi {
    fn name(&self) -> &'static str {
        "VA-API"
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

impl Drop for Vaapi {
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
