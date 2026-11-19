//! Turns the camera's JPEG frames into H.264 for WebRTC.
//!
//! The camera can send H.264 itself, but at a fixed 30 to 40 Mbit/s that no
//! remote connection keeps up with, so frames are decoded and encoded again at
//! the rate the connection can take, on the GPU (Vulkan Video or VA-API)
//! when one works and with the OpenH264 software encoder otherwise.

#[cfg(feature = "gpu")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, EncoderConfig, FrameRate, IntraFramePeriod, Profile, RateControlMode,
    UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

/// Seconds between keyframes the encoder inserts on its own. Viewers also
/// ask for one whenever they lose packets.
pub const KEYFRAME_INTERVAL: u32 = 10;

/// One picture in I420 (planar 4:2:0) with video ("limited") range levels.
#[derive(Default)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// Decodes a JPEG into `picture`, reusing its buffers.
///
/// JPEG stores full-range YCbCr while H.264 decoders in browsers assume video
/// range, so levels are scaled here; otherwise blacks and whites look washed out.
pub fn decode_jpeg(data: &[u8], picture: &mut Picture) -> Result<(), String> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::YCbCr);
    let mut decoder = JpegDecoder::new_with_options(data, options);
    let pixels = decoder.decode().map_err(|e| format!("JPEG: {e:?}"))?;
    let (w, h) = decoder
        .dimensions()
        .ok_or_else(|| "JPEG: no dimensions".to_string())?;
    if pixels.len() < w * h * 3 {
        return Err("JPEG: short picture".into());
    }
    ycbcr_to_i420(&pixels, w, h, picture);
    Ok(())
}

/// Converts interleaved full-range YCbCr 4:4:4 to video-range I420. An odd
/// last row or column is dropped, as 4:2:0 needs even sizes.
fn ycbcr_to_i420(pixels: &[u8], width: usize, height: usize, out: &mut Picture) {
    let (w, h) = (width & !1, height & !1);
    let luma = level_table(219);
    let chroma = level_table(224);
    out.width = w;
    out.height = h;
    out.y.resize(w * h, 0);
    out.u.resize(w * h / 4, 0);
    out.v.resize(w * h / 4, 0);
    for row in 0..h {
        let src = &pixels[row * width * 3..][..w * 3];
        for (dst, px) in out.y[row * w..][..w].iter_mut().zip(src.as_chunks::<3>().0) {
            *dst = luma[usize::from(px[0])];
        }
    }
    for row in 0..h / 2 {
        let top = &pixels[2 * row * width * 3..][..w * 3];
        let bottom = &pixels[(2 * row + 1) * width * 3..][..w * 3];
        let pairs = top.as_chunks::<6>().0.iter().zip(bottom.as_chunks::<6>().0);
        let start = row * (w / 2);
        let dst = out.u[start..][..w / 2]
            .iter_mut()
            .zip(&mut out.v[start..][..w / 2]);
        for ((u, v), (a, b)) in dst.zip(pairs) {
            let avg = |i: usize| {
                let sum =
                    u16::from(a[i]) + u16::from(a[i + 3]) + u16::from(b[i]) + u16::from(b[i + 3]);
                usize::from((sum + 2) / 4)
            };
            *u = chroma[avg(1)];
            *v = chroma[avg(2)];
        }
    }
}

/// Maps 0..=255 onto 16..=16+span.
fn level_table(span: u32) -> [u8; 256] {
    std::array::from_fn(|i| (16 + (i as u32 * span + 127) / 255) as u8)
}

/// An H.264 encoder producing Annex B byte streams, constrained baseline
/// profile so every browser can decode it.
pub trait Encoder: Send {
    fn name(&self) -> &'static str;
    fn size(&self) -> (usize, usize);
    /// Encodes one picture. An empty result means the encoder skipped it.
    fn encode(
        &mut self,
        picture: &Picture,
        pts: Duration,
        keyframe: bool,
    ) -> Result<Vec<u8>, String>;
    /// Changes the target bitrate in bit/s for the following pictures.
    fn set_bitrate(&mut self, bps: u32);
}

/// The GPU interfaces to try, best first, each with a flag that is set once
/// it has failed so that later viewers skip it.
#[cfg(feature = "gpu")]
static GPU_APIS: [(crate::gpu::Api, AtomicBool); 2] = [
    (crate::gpu::Api::Vulkan, AtomicBool::new(false)),
    (crate::gpu::Api::Vaapi, AtomicBool::new(false)),
];

/// Opens the best available encoder for pictures of the given size: Vulkan
/// Video, then VA-API, then OpenH264 on the CPU.
pub fn open(width: usize, height: usize, fps: u32, bps: u32) -> Result<Box<dyn Encoder>, String> {
    #[cfg(feature = "gpu")]
    for (api, failed) in &GPU_APIS {
        if failed.load(Ordering::Relaxed) {
            continue;
        }
        match crate::gpu::Gpu::new(*api, width, height, fps, bps) {
            Ok(encoder) => return Ok(Box::new(encoder)),
            Err(error) => {
                eprintln!("ilink-web: {} unavailable: {error}", api.name());
                failed.store(true, Ordering::Relaxed);
            }
        }
    }
    Ok(Box::new(OpenH264::new(width, height, fps, bps)?))
}

/// Records that the encoder with this name failed mid-stream, so that it is
/// not tried again.
pub fn encoder_failed(name: &str) {
    #[cfg(feature = "gpu")]
    for (api, failed) in &GPU_APIS {
        if api.name() == name {
            failed.store(true, Ordering::Relaxed);
        }
    }
    #[cfg(not(feature = "gpu"))]
    let _ = name;
}

pub struct OpenH264 {
    encoder: openh264::encoder::Encoder,
    size: (usize, usize),
    bps: u32,
}

impl OpenH264 {
    pub fn new(width: usize, height: usize, fps: u32, bps: u32) -> Result<Self, String> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .profile(Profile::Baseline)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bps))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .skip_frames(true)
            .complexity(Complexity::Low)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * KEYFRAME_INTERVAL))
            .vui(VuiConfig::bt601());
        let encoder =
            openh264::encoder::Encoder::with_api_config(OpenH264API::from_source(), config)
                .map_err(|e| format!("OpenH264: {e}"))?;
        Ok(Self {
            encoder,
            size: (width, height),
            bps,
        })
    }

    fn set_option(&mut self, option: openh264_sys2::ENCODER_OPTION, bps: u32) {
        let mut info = openh264_sys2::SBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: bps.try_into().unwrap_or(i32::MAX),
        };
        // SAFETY: the options take a pointer to an SBitrateInfo, which lives
        // for the duration of the call; the encoder is initialised by `new`
        // and the first `encode`.
        unsafe {
            self.encoder
                .raw_api()
                .set_option(option, (&raw mut info).cast());
        }
    }
}

impl Encoder for OpenH264 {
    fn name(&self) -> &'static str {
        "OpenH264"
    }

    fn size(&self) -> (usize, usize) {
        self.size
    }

    fn encode(
        &mut self,
        picture: &Picture,
        pts: Duration,
        keyframe: bool,
    ) -> Result<Vec<u8>, String> {
        let (w, h) = (picture.width, picture.height);
        let source = YUVSlices::new(
            (&picture.y, &picture.u, &picture.v),
            (w, h),
            (w, w / 2, w / 2),
        );
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let millis = u64::try_from(pts.as_millis()).unwrap_or(u64::MAX);
        let stream = self
            .encoder
            .encode_at(&source, openh264::Timestamp::from_millis(millis))
            .map_err(|e| format!("OpenH264: {e}"))?;
        Ok(stream.to_vec())
    }

    fn set_bitrate(&mut self, bps: u32) {
        if bps == self.bps {
            return;
        }
        // The maximum must never be below the target, so the order matters.
        if bps > self.bps {
            self.set_option(openh264_sys2::ENCODER_OPTION_MAX_BITRATE, bps);
            self.set_option(openh264_sys2::ENCODER_OPTION_BITRATE, bps);
        } else {
            self.set_option(openh264_sys2::ENCODER_OPTION_BITRATE, bps);
            self.set_option(openh264_sys2::ENCODER_OPTION_MAX_BITRATE, bps);
        }
        self.bps = bps;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_levels_and_subsamples_chroma() {
        // 2x2 picture: black, white / mid grey with chroma 0 and 255.
        let pixels = [
            0, 0, 0, 255, 0, 0, //
            128, 255, 255, 128, 255, 255,
        ];
        let mut picture = Picture::default();
        ycbcr_to_i420(&pixels, 2, 2, &mut picture);
        assert_eq!((picture.width, picture.height), (2, 2));
        assert_eq!(picture.y, [16, 235, 126, 126]);
        // Cb averages 0, 0, 255, 255 to 128.
        assert_eq!(picture.u, [128]);
        assert_eq!(picture.v, [128]);
    }

    #[test]
    fn drops_odd_edges() {
        let pixels = [128u8; 3 * 3 * 3];
        let mut picture = Picture::default();
        ycbcr_to_i420(&pixels, 3, 3, &mut picture);
        assert_eq!((picture.width, picture.height), (2, 2));
        assert_eq!(picture.y.len(), 4);
        assert_eq!(picture.u.len(), 1);
    }

    #[test]
    fn openh264_produces_annex_b() {
        let (w, h) = (64, 48);
        let picture = Picture {
            width: w,
            height: h,
            y: vec![100; w * h],
            u: vec![128; w * h / 4],
            v: vec![128; w * h / 4],
        };
        let mut encoder = OpenH264::new(w, h, 30, 500_000).unwrap();
        let first = encoder.encode(&picture, Duration::ZERO, false).unwrap();
        assert!(first.starts_with(&[0, 0, 0, 1]), "starts with a start code");
        // SPS (type 7) comes first in the first picture.
        assert_eq!(first[4] & 0x1f, 7);
        encoder.set_bitrate(200_000);
        let forced = encoder
            .encode(&picture, Duration::from_millis(33), true)
            .unwrap();
        assert!(
            forced
                .windows(5)
                .any(|w| w[..4] == [0, 0, 0, 1] && w[4] & 0x1f == 5)
        );
    }
}
