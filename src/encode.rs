//! Turns the camera's JPEG frames into H.264 for WebRTC, all through FFmpeg.
//!
//! The camera can send H.264 itself, but at a fixed 30 to 40 Mbit/s that no
//! remote connection keeps up with, so frames are decoded and encoded again at
//! the rate the connection can take: on the GPU with Vulkan Video (AMD, Intel
//! and NVIDIA) or VA-API (AMD and Intel) when one works, and on the CPU with
//! OpenH264 or x264 otherwise.
//!
//! Most of FFmpeg's H.264 encoders cannot change their bitrate once open, so
//! a new encoder is opened when the target moves far enough; it starts with a
//! keyframe, which costs one larger picture.

use std::ffi::{CStr, c_int};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::ffmpeg::{self, Buffer, Codec, Frame, Packet, check, ff};

/// Seconds between keyframes the encoder inserts on its own. Viewers also
/// ask for one whenever they lose packets.
const KEYFRAME_INTERVAL: u32 = 10;
/// `AV_PROFILE_H264_CONSTRAINED_BASELINE`: baseline (66) with the constrained
/// flag, which every browser decodes.
const CONSTRAINED_BASELINE: c_int = 66 | (1 << 9);
/// How far the target must move from the current bitrate, as a fraction of
/// it, before the encoder is reopened.
const REOPEN_CHANGE: f64 = 0.25;
/// The least time between reopenings when lowering and raising the bitrate.
/// Lowering is urgent, as a too-high rate builds up delay on the network.
const REOPEN_DOWN_AFTER: Duration = Duration::from_secs(1);
const REOPEN_UP_AFTER: Duration = Duration::from_secs(4);
/// swscale's bilinear filter. Newer FFmpeg declares the flags as an enum and
/// older ones as constants, so the value is spelled out.
const SWS_BILINEAR: c_int = 2;

/// An encoder option: name and value.
type Opt = (&'static CStr, &'static CStr);

/// The H.264 encoders to try, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Vulkan,
    Vaapi,
    OpenH264,
    X264,
}

const BACKENDS: [Backend; 4] = [
    Backend::Vulkan,
    Backend::Vaapi,
    Backend::OpenH264,
    Backend::X264,
];

/// Set once an encoder has failed, so that later viewers skip it.
static FAILED: [AtomicBool; 4] = [const { AtomicBool::new(false) }; 4];

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Vulkan => "Vulkan",
            Self::Vaapi => "VA-API",
            Self::OpenH264 => "OpenH264",
            Self::X264 => "x264",
        }
    }

    fn encoder(self) -> &'static CStr {
        match self {
            Self::Vulkan => c"h264_vulkan",
            Self::Vaapi => c"h264_vaapi",
            Self::OpenH264 => c"libopenh264",
            Self::X264 => c"libx264",
        }
    }

    fn failed(self) -> &'static AtomicBool {
        &FAILED[self as usize]
    }

    /// The GPU device type and frame format, for the GPU encoders.
    fn gpu(self) -> Option<(ff::AVHWDeviceType, ff::AVPixelFormat)> {
        match self {
            Self::Vulkan => Some((
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                ff::AVPixelFormat::AV_PIX_FMT_VULKAN,
            )),
            Self::Vaapi => Some((
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
            )),
            Self::OpenH264 | Self::X264 => None,
        }
    }

    /// The format pictures are handed over in, or uploaded from to the GPU.
    fn input_format(self) -> ff::AVPixelFormat {
        match self.gpu() {
            Some(_) => ff::AVPixelFormat::AV_PIX_FMT_NV12,
            None => ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
        }
    }

    /// Options for low delay. The GPU encoders hand each picture back as
    /// soon as it is done rather than keeping one in flight and leave out
    /// SEI and the other optional units; the Vulkan hints ask drivers to tune
    /// for live camera video.
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
            Self::OpenH264 => &[(c"rc_mode", c"bitrate"), (c"allow_skip_frames", c"1")],
            // x264 does not take the constrained baseline profile from the
            // codec context, so it is named here.
            Self::X264 => &[
                (c"preset", c"veryfast"),
                (c"tune", c"zerolatency"),
                (c"profile", c"baseline"),
                (c"forced-idr", c"1"),
            ],
        }
    }

    /// The option for constant bitrate, which keeps keyframes within the
    /// rate-control buffer. Vulkan drivers otherwise tend to pick a variable
    /// rate and make keyframes several times larger; the others keep to the
    /// rate by themselves when the bitrate and maximum are equal.
    fn constant_bitrate(self) -> Option<Opt> {
        match self {
            Self::Vulkan => Some((c"rc_mode", c"cbr")),
            Self::Vaapi | Self::OpenH264 | Self::X264 => None,
        }
    }
}

/// Decodes JPEG frames, converts them and encodes them as H.264 Annex B,
/// opening the best encoder that works for the frames' size.
pub struct Transcoder {
    fps: u32,
    decoder: Decoder,
    scaler: Scaler,
    encoder: Option<Encoder>,
}

impl Transcoder {
    pub fn new(fps: u32) -> Result<Self, String> {
        ffmpeg::quiet();
        Ok(Self {
            fps,
            decoder: Decoder::new()?,
            scaler: Scaler::default(),
            encoder: None,
        })
    }

    /// Encodes one JPEG at about `bps` bit/s. An empty result means the
    /// encoder skipped the picture.
    pub fn encode(
        &mut self,
        jpeg: &[u8],
        pts: Duration,
        bps: u32,
        keyframe: bool,
    ) -> Result<Vec<u8>, String> {
        let format = self.decoder.decode(jpeg)?;
        let picture = &self.decoder.frame;
        // 4:2:0 needs even sizes, so an odd last row or column is dropped.
        let size = (picture.width() & !1, picture.height() & !1);
        if self.encoder.as_ref().is_none_or(|e| e.size() != size) {
            let encoder = open_best(size, self.fps, bps)?;
            eprintln!(
                "ilink-web: encoding {}x{} with {}",
                size.0,
                size.1,
                encoder.backend.name()
            );
            self.encoder = Some(encoder);
        }
        let encoder = self.encoder.as_mut().expect("opened above");
        encoder.set_bitrate(bps);
        let input = match encoder.input() {
            Ok(input) => input,
            Err(error) => return Err(give_up(&mut self.encoder, error)),
        };
        self.scaler.convert(picture, format, input)?;
        let encoder = self.encoder.as_mut().expect("still open");
        encoder
            .encode(pts, keyframe)
            .map_err(|error| give_up(&mut self.encoder, error))
    }
}

/// Gives up on an encoder that failed, so that the next picture opens the
/// next best one.
fn give_up(encoder: &mut Option<Encoder>, error: String) -> String {
    let Some(encoder) = encoder.take() else {
        return error;
    };
    encoder.backend.failed().store(true, Ordering::Relaxed);
    format!("{} failed: {error}", encoder.backend.name())
}

/// Opens the first encoder that works, skipping those that failed before.
fn open_best(size: (c_int, c_int), fps: u32, bps: u32) -> Result<Encoder, String> {
    for backend in BACKENDS {
        if backend.failed().load(Ordering::Relaxed) {
            continue;
        }
        match Encoder::open(backend, size, fps, bps) {
            Ok(encoder) => return Ok(encoder),
            Err(error) => {
                eprintln!("ilink-web: {} unavailable: {error}", backend.name());
                backend.failed().store(true, Ordering::Relaxed);
            }
        }
    }
    Err("no H.264 encoder works".into())
}

/// FFmpeg's JPEG decoder.
struct Decoder {
    codec: Codec,
    frame: Frame,
    packet: Packet,
}

impl Decoder {
    fn new() -> Result<Self, String> {
        // SAFETY: plain FFI calls; every pointer is checked before use.
        unsafe {
            let decoder = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_MJPEG);
            if decoder.is_null() {
                return Err("FFmpeg has no JPEG decoder".into());
            }
            let codec = Codec::new(decoder)?;
            check(
                ff::avcodec_open2(codec.as_ptr(), decoder, null_mut()),
                "opening the JPEG decoder",
            )?;
            Ok(Self {
                codec,
                frame: Frame::new()?,
                packet: Packet::new()?,
            })
        }
    }

    /// Decodes a JPEG into `self.frame` and returns its pixel format.
    fn decode(&mut self, jpeg: &[u8]) -> Result<ff::AVPixelFormat, String> {
        let len = c_int::try_from(jpeg.len()).map_err(|_| "JPEG too large".to_string())?;
        // SAFETY: av_new_packet allocates `len` bytes (and padding) that the
        // JPEG is copied into; the decoder holds no frame in reserve, so one
        // packet in gives one picture out.
        unsafe {
            let packet = self.packet.as_ptr();
            ff::av_packet_unref(packet);
            check(ff::av_new_packet(packet, len), "allocating a packet")?;
            std::ptr::copy_nonoverlapping(jpeg.as_ptr(), (*packet).data, jpeg.len());
            check(
                ff::avcodec_send_packet(self.codec.as_ptr(), packet),
                "decoding the JPEG",
            )?;
            check(
                ff::avcodec_receive_frame(self.codec.as_ptr(), self.frame.as_ptr()),
                "decoding the JPEG",
            )?;
            Ok((*self.codec.as_ptr()).pix_fmt)
        }
    }
}

/// Converts decoded pictures to the encoder's format.
///
/// JPEG stores full-range YCbCr while H.264 decoders in browsers assume video
/// range, so levels are scaled too; otherwise blacks and whites look washed
/// out.
#[derive(Default)]
struct Scaler {
    context: Option<std::ptr::NonNull<ff::SwsContext>>,
    /// The sizes and formats the context was set up for: source width,
    /// height and format, then the same for the destination.
    setup: [c_int; 6],
}

impl Scaler {
    fn convert(
        &mut self,
        src: &Frame,
        src_format: ff::AVPixelFormat,
        dst: *mut ff::AVFrame,
    ) -> Result<(), String> {
        // SAFETY: `src` is a decoded picture and `dst` a writable one with
        // its own buffer; the cached context matches both after the call.
        unsafe {
            let (s, d) = (&*src.as_ptr(), &*dst);
            let old = self.context.map_or(null_mut(), |c| c.as_ptr());
            let context = ff::sws_getCachedContext(
                old,
                s.width,
                s.height,
                src_format,
                d.width,
                d.height,
                Backend::from_input(d.format),
                SWS_BILINEAR,
                null_mut(),
                null_mut(),
                null(),
            );
            let Some(context) = std::ptr::NonNull::new(context) else {
                self.context = None;
                return Err("cannot convert the picture".into());
            };
            // A changed size or format gives a new context, which needs the
            // levels set again. Its address can be the old one's, so the
            // setup is compared rather than the pointer.
            let setup = [
                s.width,
                s.height,
                src_format as c_int,
                d.width,
                d.height,
                d.format,
            ];
            if setup != self.setup {
                self.setup = setup;
                let bt601 = ff::sws_getCoefficients(ff::SWS_CS_ITU601);
                ff::sws_setColorspaceDetails(
                    context.as_ptr(),
                    bt601,
                    1,
                    bt601,
                    0,
                    0,
                    1 << 16,
                    1 << 16,
                );
            }
            self.context = Some(context);
            check(
                ff::sws_scale_frame(context.as_ptr(), dst, src.as_ptr()),
                "converting the picture",
            )?;
        }
        Ok(())
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        if let Some(context) = self.context {
            // SAFETY: the context is ours and not used after this.
            unsafe { ff::sws_freeContext(context.as_ptr()) };
        }
    }
}

impl Backend {
    /// The pixel format of an encoder's input frame, from its raw value.
    fn from_input(format: c_int) -> ff::AVPixelFormat {
        if format == ff::AVPixelFormat::AV_PIX_FMT_NV12 as c_int {
            ff::AVPixelFormat::AV_PIX_FMT_NV12
        } else {
            ff::AVPixelFormat::AV_PIX_FMT_YUV420P
        }
    }
}

/// One open H.264 encoder.
struct Encoder {
    backend: Backend,
    /// The GPU frames, for the GPU encoders; null otherwise. They hold a
    /// reference to the device.
    frames: Buffer,
    codec: Codec,
    /// The next picture, in the backend's input format.
    input: Frame,
    packet: Packet,
    fps: u32,
    bps: u32,
    target: u32,
    opened: Instant,
}

impl Encoder {
    fn open(backend: Backend, size: (c_int, c_int), fps: u32, bps: u32) -> Result<Self, String> {
        let mut frames = Buffer::null();
        if let Some((device_type, format)) = backend.gpu() {
            let mut device = Buffer::null();
            // SAFETY: plain FFI calls; every pointer is checked before use,
            // and the buffers free themselves if a later step fails.
            unsafe {
                check(
                    ff::av_hwdevice_ctx_create(device.as_out(), device_type, null(), null_mut(), 0),
                    "opening the device",
                )?;
                *frames.as_out() = ff::av_hwframe_ctx_alloc(device.as_ptr());
                if frames.as_ptr().is_null() {
                    return Err("allocating GPU frames".into());
                }
                let f = (*frames.as_ptr()).data.cast::<ff::AVHWFramesContext>();
                (*f).format = format;
                (*f).sw_format = backend.input_format();
                (*f).width = size.0;
                (*f).height = size.1;
                (*f).initial_pool_size = 8;
                check(
                    ff::av_hwframe_ctx_init(frames.as_ptr()),
                    "setting up GPU frames",
                )?;
            }
        }
        let codec = open_codec(backend, &frames, size, fps, bps)?;
        Ok(Self {
            backend,
            frames,
            codec,
            input: Frame::with_buffer(backend.input_format(), size.0, size.1)?,
            packet: Packet::new()?,
            fps,
            bps,
            target: bps,
            opened: Instant::now(),
        })
    }

    fn size(&self) -> (c_int, c_int) {
        (self.input.width(), self.input.height())
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.target = bps;
    }

    /// The frame to put the next picture in.
    fn input(&mut self) -> Result<*mut ff::AVFrame, String> {
        // SAFETY: the frame was allocated with a buffer; making it writable
        // copies the buffer if the encoder still holds the last picture.
        unsafe {
            check(
                ff::av_frame_make_writable(self.input.as_ptr()),
                "preparing a frame",
            )?;
        }
        Ok(self.input.as_ptr())
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
        self.codec = open_codec(self.backend, &self.frames, self.size(), self.fps, self.bps)?;
        self.opened = Instant::now();
        Ok(true)
    }

    /// Encodes the picture in the input frame.
    fn encode(&mut self, pts: Duration, keyframe: bool) -> Result<Vec<u8>, String> {
        // A reopened encoder starts with a keyframe anyway.
        let keyframe = !self.maybe_reopen()? && keyframe;
        // SAFETY: all objects were set up by `open` and stay valid while
        // `self` lives; the GPU frame frees itself.
        unsafe {
            let uploaded;
            let frame = if self.frames.as_ptr().is_null() {
                self.input.as_ptr()
            } else {
                uploaded = Frame::new()?;
                check(
                    ff::av_hwframe_get_buffer(self.frames.as_ptr(), uploaded.as_ptr(), 0),
                    "getting a GPU frame",
                )?;
                check(
                    ff::av_hwframe_transfer_data(uploaded.as_ptr(), self.input.as_ptr(), 0),
                    "uploading a frame",
                )?;
                uploaded.as_ptr()
            };
            (*frame).pts = i64::try_from(pts.as_millis()).unwrap_or(i64::MAX);
            (*frame).pict_type = if keyframe {
                ff::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            check(
                ff::avcodec_send_frame(self.codec.as_ptr(), frame),
                "encoding",
            )?;

            let mut out = Vec::new();
            loop {
                let ret = ff::avcodec_receive_packet(self.codec.as_ptr(), self.packet.as_ptr());
                if ret == ff::AVERROR(ff::EAGAIN) || ret == ff::AVERROR_EOF {
                    break;
                }
                check(ret, "receiving a packet")?;
                let p = &*self.packet.as_ptr();
                out.extend_from_slice(std::slice::from_raw_parts(p.data, p.size as usize));
                ff::av_packet_unref(self.packet.as_ptr());
            }
            Ok(out)
        }
    }
}

/// Opens an encoder context at `bps`, in constant bitrate mode if the
/// backend has one and the driver takes it, and in its own choice of mode
/// otherwise.
fn open_codec(
    backend: Backend,
    frames: &Buffer,
    size: (c_int, c_int),
    fps: u32,
    bps: u32,
) -> Result<Codec, String> {
    match backend.constant_bitrate() {
        Some(cbr) => open_codec_with(backend, frames, size, fps, bps, Some(cbr))
            .or_else(|_| open_codec_with(backend, frames, size, fps, bps, None)),
        None => open_codec_with(backend, frames, size, fps, bps, None),
    }
}

fn open_codec_with(
    backend: Backend,
    frames: &Buffer,
    size: (c_int, c_int),
    fps: u32,
    bps: u32,
    rate_control: Option<Opt>,
) -> Result<Codec, String> {
    let name = backend.encoder();
    // SAFETY: plain FFI calls on a context we own; `frames` is a valid GPU
    // frames context or null.
    unsafe {
        let encoder = ff::avcodec_find_encoder_by_name(name.as_ptr());
        if encoder.is_null() {
            return Err(format!("FFmpeg has no {} encoder", name.to_string_lossy()));
        }
        let codec = Codec::new(encoder)?;
        let c = &mut *codec.as_ptr();
        c.width = size.0;
        c.height = size.1;
        c.time_base = ff::AVRational { num: 1, den: 1000 };
        c.framerate = ff::AVRational {
            num: fps as c_int,
            den: 1,
        };
        c.pix_fmt = backend.gpu().map_or(backend.input_format(), |(_, f)| f);
        c.profile = CONSTRAINED_BASELINE;
        c.max_b_frames = 0;
        c.gop_size = (fps * KEYFRAME_INTERVAL) as c_int;
        c.bit_rate = i64::from(bps);
        c.rc_max_rate = i64::from(bps);
        // A small rate-control buffer (200 ms of video) keeps keyframes from
        // growing so large that they take long to send.
        c.rc_buffer_size = (bps / 5) as c_int;
        c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
        c.colorspace = ff::AVColorSpace::AVCOL_SPC_SMPTE170M;
        c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_SMPTE170M;
        c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE170M;
        if !frames.as_ptr().is_null() {
            c.hw_frames_ctx = ff::av_buffer_ref(frames.as_ptr());
        }
        for (key, value) in backend.options().iter().chain(&rate_control) {
            ff::av_opt_set(c.priv_data, key.as_ptr(), value.as_ptr(), 0);
        }
        check(
            ff::avcodec_open2(codec.as_ptr(), encoder, null_mut()),
            "opening the encoder",
        )?;
        Ok(codec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grey JPEG of the given size and brightness, made with FFmpeg's
    /// JPEG encoder.
    fn jpeg(width: c_int, height: c_int, luma: u8) -> Vec<u8> {
        unsafe {
            let encoder = ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_MJPEG);
            let codec = Codec::new(encoder).unwrap();
            let c = &mut *codec.as_ptr();
            c.width = width;
            c.height = height;
            c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P;
            c.time_base = ff::AVRational { num: 1, den: 25 };
            check(
                ff::avcodec_open2(codec.as_ptr(), encoder, null_mut()),
                "open",
            )
            .unwrap();
            let frame =
                Frame::with_buffer(ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P, width, height).unwrap();
            let f = &*frame.as_ptr();
            for (plane, value, rows) in [
                (0, luma, height),
                (1, 128, height / 2),
                (2, 128, height / 2),
            ] {
                std::ptr::write_bytes(f.data[plane], value, (f.linesize[plane] * rows) as usize);
            }
            check(
                ff::avcodec_send_frame(codec.as_ptr(), frame.as_ptr()),
                "send",
            )
            .unwrap();
            let packet = Packet::new().unwrap();
            check(
                ff::avcodec_receive_packet(codec.as_ptr(), packet.as_ptr()),
                "receive",
            )
            .unwrap();
            let p = &*packet.as_ptr();
            std::slice::from_raw_parts(p.data, p.size as usize).to_vec()
        }
    }

    /// The luma values of a JPEG after decoding and conversion to I420.
    fn converted_luma(data: &[u8]) -> Vec<u8> {
        let mut decoder = Decoder::new().unwrap();
        let format = decoder.decode(data).unwrap();
        let picture = &decoder.frame;
        let (w, h) = (picture.width(), picture.height());
        let out = Frame::with_buffer(ff::AVPixelFormat::AV_PIX_FMT_YUV420P, w, h).unwrap();
        Scaler::default()
            .convert(picture, format, out.as_ptr())
            .unwrap();
        unsafe {
            let f = &*out.as_ptr();
            (0..h as usize)
                .flat_map(|row| {
                    std::slice::from_raw_parts(
                        f.data[0].add(row * f.linesize[0] as usize),
                        w as usize,
                    )
                })
                .copied()
                .collect()
        }
    }

    #[test]
    fn scales_full_range_to_video_range() {
        let near = |values: Vec<u8>, level: u8| values.iter().all(|&v| v.abs_diff(level) <= 2);
        assert!(near(converted_luma(&jpeg(64, 48, 0)), 16));
        assert!(near(converted_luma(&jpeg(64, 48, 255)), 235));
    }

    #[test]
    fn encodes_annex_b_with_keyframes_on_request() {
        let picture = jpeg(320, 240, 100);
        let mut transcoder = Transcoder::new(30).unwrap();
        let first = transcoder
            .encode(&picture, Duration::ZERO, 500_000, false)
            .unwrap();
        let nal_types = |data: &[u8]| -> Vec<u8> {
            data.windows(4)
                .enumerate()
                .filter(|(_, w)| w[..3] == [0, 0, 1])
                .map(|(i, _)| data[i + 3] & 0x1f)
                .collect()
        };
        assert!(first.starts_with(&[0, 0, 0, 1]), "starts with a start code");
        assert!(nal_types(&first).contains(&7), "has an SPS");
        // The page offers constrained baseline (42e0..), so the SPS must
        // say baseline (66) with constraint_set1, whichever encoder ran.
        let sps = first
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
            .unwrap()
            + 4;
        assert_eq!(first[sps], 66, "baseline profile");
        assert_ne!(first[sps + 1] & 0x40, 0, "constrained");
        assert!(nal_types(&first).contains(&5), "starts with a keyframe");
        let mut forced = Vec::new();
        for i in 1..5 {
            forced = transcoder
                .encode(&picture, Duration::from_millis(33 * i), 500_000, i == 4)
                .unwrap();
        }
        assert!(nal_types(&forced).contains(&5), "a keyframe on request");
    }
}
