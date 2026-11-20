//! Owned FFmpeg objects, freed when dropped, and error checking.

use std::ffi::{CStr, c_char, c_int};
use std::ptr::null_mut;

pub use ffmpeg_sys_next as ff;

/// Turns a negative FFmpeg return value into an error naming the step.
pub fn check(ret: c_int, what: &str) -> Result<c_int, String> {
    if ret >= 0 {
        return Ok(ret);
    }
    let mut buf = [0 as c_char; 128];
    // SAFETY: av_strerror writes a NUL-terminated string into `buf`.
    let message = unsafe {
        ff::av_strerror(ret, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    };
    Err(format!("{what}: {message}"))
}

/// Keeps FFmpeg's own log to errors, once for the whole process.
pub fn quiet() {
    static QUIET: std::sync::Once = std::sync::Once::new();
    // SAFETY: setting the log level has no preconditions.
    QUIET.call_once(|| unsafe { ff::av_log_set_level(ff::AV_LOG_ERROR) });
}

macro_rules! owned {
    ($name:ident, $raw:ty, $free:expr) => {
        pub struct $name(*mut $raw);

        impl $name {
            pub fn as_ptr(&self) -> *mut $raw {
                self.0
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                // SAFETY: the free function accepts null and nulls the pointer.
                unsafe { $free(&mut self.0) }
            }
        }
    };
}

owned!(Frame, ff::AVFrame, ff::av_frame_free);
owned!(Packet, ff::AVPacket, ff::av_packet_free);
owned!(Codec, ff::AVCodecContext, ff::avcodec_free_context);
owned!(Buffer, ff::AVBufferRef, ff::av_buffer_unref);

impl Frame {
    pub fn new() -> Result<Self, String> {
        // SAFETY: plain allocation; null is checked.
        let frame = unsafe { ff::av_frame_alloc() };
        if frame.is_null() {
            return Err("allocating a frame".into());
        }
        Ok(Self(frame))
    }

    /// A frame with its own picture buffer of the given format and size.
    pub fn with_buffer(
        format: ff::AVPixelFormat,
        width: c_int,
        height: c_int,
    ) -> Result<Self, String> {
        let frame = Self::new()?;
        // SAFETY: the frame was just allocated and is not shared.
        unsafe {
            let f = &mut *frame.0;
            f.format = format as c_int;
            f.width = width;
            f.height = height;
            check(ff::av_frame_get_buffer(frame.0, 0), "allocating a picture")?;
        }
        Ok(frame)
    }

    pub fn width(&self) -> c_int {
        // SAFETY: the frame is valid while `self` lives.
        unsafe { (*self.0).width }
    }

    pub fn height(&self) -> c_int {
        // SAFETY: as above.
        unsafe { (*self.0).height }
    }
}

impl Packet {
    pub fn new() -> Result<Self, String> {
        // SAFETY: plain allocation; null is checked.
        let packet = unsafe { ff::av_packet_alloc() };
        if packet.is_null() {
            return Err("allocating a packet".into());
        }
        Ok(Self(packet))
    }
}

impl Codec {
    /// An unopened context for the named encoder, or the given decoder.
    pub fn new(codec: *const ff::AVCodec) -> Result<Self, String> {
        // SAFETY: avcodec_alloc_context3 accepts any codec pointer, and null
        // is checked.
        let context = unsafe { ff::avcodec_alloc_context3(codec) };
        if context.is_null() {
            return Err("allocating a codec".into());
        }
        Ok(Self(context))
    }
}

impl Buffer {
    pub fn null() -> Self {
        Self(null_mut())
    }

    /// For FFmpeg functions that fill in the pointer.
    pub fn as_out(&mut self) -> *mut *mut ff::AVBufferRef {
        &mut self.0
    }
}
