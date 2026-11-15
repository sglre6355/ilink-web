//! Captures MJPEG frames from the camera and hands the newest one to every
//! viewer.
//!
//! The camera is only opened while at least one viewer is connected, and is
//! released a few seconds after the last one leaves so that other programs can
//! use it.

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use linuxvideo::format::{PixFormat, PixelFormat};
use linuxvideo::{Device, Fract};

/// How long capture keeps running after the last viewer leaves.
const LINGER: Duration = Duration::from_secs(5);
/// How long to wait before trying to open the camera again after a failure.
const RETRY: Duration = Duration::from_secs(2);

/// What to capture.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Device node; `None` means the first Link camera found.
    pub device: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub type Frame = Arc<Vec<u8>>;

pub struct Video {
    settings: Settings,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    frame: Option<Frame>,
    /// Increases by one for every frame, so viewers can tell new from old.
    seq: u64,
    viewers: usize,
    capturing: bool,
    last_failure: Option<Instant>,
    error: Option<String>,
}

impl Video {
    pub fn new(settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            settings,
            state: Mutex::default(),
            changed: Condvar::new(),
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers a viewer, starting capture if it is not already running.
    pub fn watch(self: &Arc<Self>) -> Viewer {
        let mut state = self.lock();
        state.viewers += 1;
        self.ensure_capturing(&mut state);
        Viewer {
            video: Arc::clone(self),
            seq: 0,
        }
    }

    fn ensure_capturing(self: &Arc<Self>, state: &mut State) {
        if state.capturing {
            return;
        }
        if state.last_failure.is_some_and(|t| t.elapsed() < RETRY) {
            return;
        }
        state.capturing = true;
        let video = Arc::clone(self);
        thread::spawn(move || {
            let result = video.capture();
            let mut state = video.lock();
            state.capturing = false;
            state.frame = None;
            match result {
                Ok(()) => state.error = None,
                Err(error) => {
                    eprintln!("ilink-web: video: {error}");
                    state.error = Some(error.to_string());
                    state.last_failure = Some(Instant::now());
                }
            }
            video.changed.notify_all();
        });
    }

    /// Runs until no one has been watching for [`LINGER`].
    fn capture(&self) -> io::Result<()> {
        let s = &self.settings;
        let path = match &s.device {
            Some(path) => path.clone(),
            None => insta360_link::Camera::list()
                .map_err(io::Error::other)?
                .into_iter()
                .next()
                .map(|info| info.id.to_string())
                .ok_or_else(|| io::Error::other("no Link camera found"))?,
        };
        let device = Device::open(&path)?;
        let capture = device.video_capture(PixFormat::new(s.width, s.height, PixelFormat::MJPG))?;
        let format = capture.format();
        if format.pixel_format() != PixelFormat::MJPG {
            return Err(io::Error::other("the camera does not offer MJPEG"));
        }
        // Capture at the requested rate or the nearest the camera offers.
        let _ = capture.set_frame_interval(Fract::new(1, s.fps.max(1)));
        eprintln!(
            "ilink-web: video: capturing {}x{} from {}",
            format.width(),
            format.height(),
            path
        );
        let mut stream = capture.into_stream()?;
        let mut idle_since: Option<Instant> = None;
        loop {
            let frame = stream
                .dequeue(|buf| Ok((!buf.is_error() && !buf.is_empty()).then(|| buf.to_vec())))?;
            let mut state = self.lock();
            if let Some(frame) = frame {
                state.frame = Some(Arc::new(frame));
                state.seq += 1;
                state.error = None;
                self.changed.notify_all();
            }
            if state.viewers == 0 {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= LINGER {
                    eprintln!("ilink-web: video: no viewers, releasing the camera");
                    return Ok(());
                }
            } else {
                idle_since = None;
            }
        }
    }
}

/// One connected viewer. Dropping it unregisters the viewer.
pub struct Viewer {
    video: Arc<Video>,
    seq: u64,
}

impl Viewer {
    /// Waits for a frame newer than the last one returned. Returns `Err` with
    /// the capture error if no frame arrives within `timeout`.
    pub fn next(&mut self, timeout: Duration) -> Result<Frame, String> {
        let deadline = Instant::now() + timeout;
        let mut state = self.video.lock();
        loop {
            if state.seq > self.seq
                && let Some(frame) = &state.frame
            {
                self.seq = state.seq;
                return Ok(Arc::clone(frame));
            }
            self.video.ensure_capturing(&mut state);
            let now = Instant::now();
            if now >= deadline {
                return Err(state
                    .error
                    .clone()
                    .unwrap_or_else(|| "no picture from the camera".into()));
            }
            // Wake at least every RETRY so a failed capture is retried.
            let wait = (deadline - now).min(RETRY);
            state = self
                .video
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.video.lock().viewers -= 1;
    }
}
