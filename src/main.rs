//! A web page to watch and steer an Insta360 Link webcam.
//!
//! The server streams the camera's own MJPEG video to the page and moves the
//! gimbal with the `insta360-link` library.

mod video;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use insta360_link::{Camera, PanTiltUpdate};

use video::Video;

const PAGE: &str = include_str!("index.html");
const BOUNDARY: &str = "ilinkwebframe";

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Address and port to serve the page on
    #[arg(short, long, value_name = "ADDR", default_value = "127.0.0.1:8081")]
    listen: SocketAddr,

    /// Camera device node; the default is the first Link camera found
    #[arg(short, long, value_name = "PATH")]
    device: Option<String>,

    /// Video size to ask the camera for
    #[arg(short, long, value_name = "WxH", default_value = "1920x1080", value_parser = resolution)]
    resolution: (u32, u32),

    /// Frames per second sent to each viewer
    #[arg(short, long, value_name = "N", default_value_t = 15,
          value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,
}

fn resolution(value: &str) -> Result<(u32, u32), String> {
    let (w, h) = value
        .split_once(['x', 'X'])
        .ok_or("expected WIDTHxHEIGHT, for example 1280x720")?;
    match (w.parse(), h.parse()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err("expected WIDTHxHEIGHT, for example 1280x720".into()),
    }
}

/// An open camera and the ranges it accepts.
struct Link {
    camera: Camera,
    pan: RangeInclusive<f64>,
    tilt: RangeInclusive<f64>,
    zoom: RangeInclusive<f64>,
}

impl Link {
    fn open(device: Option<&str>) -> insta360_link::Result<Self> {
        let camera = match device {
            Some(path) => Camera::open(path)?,
            None => Camera::open_first()?,
        };
        let limits = camera.position_limits()?;
        let zoom = camera.zoom_limits()?;
        Ok(Self {
            camera,
            pan: limits.pan,
            tilt: limits.tilt,
            zoom,
        })
    }

    fn state_json(&self) -> insta360_link::Result<String> {
        let position = self.camera.position()?;
        let zoom = self.camera.zoom()?;
        let privacy = self.camera.privacy().unwrap_or(false);
        Ok(format!(
            r#"{{"pan":{},"tilt":{},"zoom":{},"privacy":{},"limits":{{"pan":[{},{}],"tilt":[{},{}],"zoom":[{},{}]}}}}"#,
            position.pan,
            position.tilt,
            zoom,
            privacy,
            self.pan.start(),
            self.pan.end(),
            self.tilt.start(),
            self.tilt.end(),
            self.zoom.start(),
            self.zoom.end()
        ))
    }

    /// Handles one API call and returns the camera state as JSON.
    fn api(&self, path: &str, query: &str) -> Result<String, ApiError> {
        match path {
            "/api/state" => {}
            // Absolute (pan, tilt, zoom) and relative (dpan, dtilt, dzoom) moves.
            "/api/move" => {
                let get = |key| param(query, key);
                let (pan, tilt, dpan, dtilt) = (get("pan"), get("tilt"), get("dpan"), get("dtilt"));
                if pan.is_some() || tilt.is_some() || dpan.is_some() || dtilt.is_some() {
                    let current = self.camera.position()?;
                    let pan = pan.unwrap_or(current.pan) + dpan.unwrap_or(0.0);
                    let tilt = tilt.unwrap_or(current.tilt) + dtilt.unwrap_or(0.0);
                    self.camera.set_position(PanTiltUpdate {
                        pan: Some(clamp(pan, &self.pan).round()),
                        tilt: Some(clamp(tilt, &self.tilt).round()),
                    })?;
                }
                let (zoom, dzoom) = (get("zoom"), get("dzoom"));
                if zoom.is_some() || dzoom.is_some() {
                    let current = self.camera.zoom()?;
                    let zoom = zoom.unwrap_or(current) + dzoom.unwrap_or(0.0);
                    let zoom = (clamp(zoom, &self.zoom) * 10.0).round() / 10.0;
                    if (zoom - current).abs() > 0.05 {
                        self.camera.set_zoom(zoom)?;
                    }
                }
            }
            "/api/center" => self.camera.recenter()?,
            _ => return Err(ApiError::NotFound),
        }
        Ok(self.state_json()?)
    }
}

enum ApiError {
    NotFound,
    Camera(insta360_link::Error),
}

impl From<insta360_link::Error> for ApiError {
    fn from(error: insta360_link::Error) -> Self {
        Self::Camera(error)
    }
}

struct Server {
    device: Option<String>,
    /// The camera, opened on first use and again after it fails, so that
    /// unplugging and replugging it does not need a restart.
    link: Mutex<Option<Link>>,
    video: Arc<Video>,
}

impl Server {
    fn api(&self, path: &str, query: &str) -> (&'static str, String) {
        let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if link.is_none() {
            match Link::open(self.device.as_deref()) {
                Ok(opened) => *link = Some(opened),
                Err(error) => return ("503 Service Unavailable", error_json(&error.to_string())),
            }
        }
        let result = link
            .as_ref()
            .map(|l| l.api(path, query))
            .expect("opened above");
        match result {
            Ok(json) => ("200 OK", json),
            Err(ApiError::NotFound) => ("404 Not Found", error_json("not found")),
            Err(ApiError::Camera(error)) => {
                // Reopen on the next request in case the camera went away.
                if matches!(link.as_ref().map(|l| l.camera.position()), Some(Err(_))) {
                    *link = None;
                }
                ("500 Internal Server Error", error_json(&error.to_string()))
            }
        }
    }

    fn handle(&self, mut stream: TcpStream) {
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(clone);
        let mut request = String::new();
        if reader.read_line(&mut request).is_err() {
            return;
        }
        let mut body_len = 0u64;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                body_len = value.trim().parse().unwrap_or(0);
            }
        }
        let _ = reader.take(body_len.min(4096)).read_to_end(&mut Vec::new());

        let target = request.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        match path {
            "/" | "/index.html" => respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                PAGE.as_bytes(),
            ),
            "/stream" => self.stream_video(&mut stream),
            "/snapshot" => {
                let mut viewer = self.video.watch();
                match viewer.next(Duration::from_secs(5)) {
                    Ok(frame) => respond(&mut stream, "200 OK", "image/jpeg", &frame),
                    Err(error) => respond(
                        &mut stream,
                        "503 Service Unavailable",
                        "application/json",
                        error_json(&error).as_bytes(),
                    ),
                }
            }
            p if p.starts_with("/api/") => {
                let (status, json) = self.api(p, query);
                respond(&mut stream, status, "application/json", json.as_bytes());
            }
            _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
        }
    }

    /// Sends frames as `multipart/x-mixed-replace` until the viewer goes away
    /// or the camera stops sending pictures.
    fn stream_video(&self, stream: &mut TcpStream) {
        let mut viewer = self.video.watch();
        let first = match viewer.next(Duration::from_secs(5)) {
            Ok(frame) => frame,
            Err(error) => {
                let body = error_json(&error);
                respond(
                    stream,
                    "503 Service Unavailable",
                    "application/json",
                    body.as_bytes(),
                );
                return;
            }
        };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace;boundary={BOUNDARY}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let interval = Duration::from_secs(1) / self.video.settings().fps;
        let mut frame = first;
        let mut due = Instant::now();
        loop {
            let part = format!(
                "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                frame.len()
            );
            let sent = stream
                .write_all(part.as_bytes())
                .and_then(|()| stream.write_all(&frame))
                .and_then(|()| stream.write_all(b"\r\n"));
            if sent.is_err() {
                return;
            }
            // Pace each viewer to --fps however fast the camera delivers.
            due += interval;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            } else {
                due = now;
            }
            frame = match viewer.next(Duration::from_secs(5)) {
                Ok(frame) => frame,
                // End the response; the page reconnects by itself.
                Err(_) => return,
            };
        }
    }
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body));
}

/// The value of `key` in a query string, if it is a finite number.
fn param(query: &str, key: &str) -> Option<f64> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
        .filter(|v: &f64| v.is_finite())
}

fn clamp(value: f64, range: &RangeInclusive<f64>) -> f64 {
    value.clamp(*range.start(), *range.end())
}

fn error_json(message: &str) -> String {
    let mut escaped = String::new();
    for c in message.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            c if c.is_control() => escaped.push(' '),
            c => escaped.push(c),
        }
    }
    format!(r#"{{"error":"{escaped}"}}"#)
}

fn main() -> ExitCode {
    let args = Args::parse();
    let (width, height) = args.resolution;
    let server = Arc::new(Server {
        video: Video::new(video::Settings {
            device: args.device.clone(),
            width,
            height,
            fps: args.fps,
        }),
        device: args.device,
        link: Mutex::new(None),
    });

    // Open the camera up front so a missing camera is reported straight away.
    match Link::open(server.device.as_deref()) {
        Ok(link) => {
            eprintln!(
                "{} at {}: pan {:?}, tilt {:?}, zoom {:?}",
                link.camera.info().model.name(),
                link.camera.info().id,
                link.pan,
                link.tilt,
                link.zoom
            );
            *server.link.lock().expect("not poisoned") = Some(link);
        }
        Err(error) => eprintln!("ilink-web: {error}; will retry when the page asks"),
    }

    let listener = match TcpListener::bind(args.listen) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("ilink-web: cannot listen on {}: {error}", args.listen);
            return ExitCode::FAILURE;
        }
    };
    eprintln!("serving on http://{}/", args.listen);
    for stream in listener.incoming().flatten() {
        let server = Arc::clone(&server);
        std::thread::spawn(move || server.handle(stream));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_reads_finite_numbers_only() {
        let query = "pan=12.5&tilt=-3&zoom=NaN&dpan=inf&x=abc";
        assert_eq!(param(query, "pan"), Some(12.5));
        assert_eq!(param(query, "tilt"), Some(-3.0));
        assert_eq!(param(query, "zoom"), None);
        assert_eq!(param(query, "dpan"), None);
        assert_eq!(param(query, "x"), None);
        assert_eq!(param(query, "missing"), None);
        assert_eq!(param("", "pan"), None);
    }

    #[test]
    fn error_json_escapes() {
        assert_eq!(
            error_json("bad \"value\"\n\\"),
            r#"{"error":"bad \"value\" \\"}"#
        );
    }

    #[test]
    fn resolution_parses_width_and_height() {
        assert_eq!(resolution("1920x1080"), Ok((1920, 1080)));
        assert_eq!(resolution("1280X720"), Ok((1280, 720)));
        assert!(resolution("1280").is_err());
        assert!(resolution("0x720").is_err());
        assert!(resolution("axb").is_err());
    }
}
