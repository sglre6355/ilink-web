//! A web page to watch and steer an Insta360 Link webcam.
//!
//! The server sends the camera's video to the page over WebRTC (or as plain
//! MJPEG over HTTP) and moves the gimbal with the `insta360-link` library.

mod encode;
mod stun;
mod turn;
#[cfg(feature = "vaapi")]
mod vaapi;
mod video;
mod webrtc;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use insta360_link::{Camera, PanTiltUpdate};

use turn::CloudflareTurn;
use video::Video;
use webrtc::Hub;

const PAGE: &str = include_str!("index.html");
const BOUNDARY: &str = "ilinkwebframe";
/// Largest request body read; SDP offers are a few kilobytes.
const MAX_BODY: u64 = 64 * 1024;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Address and port to serve the page on
    #[arg(short, long, value_name = "ADDR", default_value = "127.0.0.1:8081")]
    listen: SocketAddr,

    /// Address and UDP port for WebRTC video; the default is the --listen
    /// address
    #[arg(short, long, value_name = "ADDR")]
    udp: Option<SocketAddr>,

    /// Camera device node; the default is the first Link camera found
    #[arg(short, long, value_name = "PATH")]
    device: Option<String>,

    /// Video size to ask the camera for
    #[arg(short, long, value_name = "WxH", default_value = "1920x1080", value_parser = resolution)]
    resolution: (u32, u32),

    /// Frames per second to capture and send
    #[arg(short, long, value_name = "N", default_value_t = 30,
          value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,

    /// Highest video bitrate per viewer, in kbit/s
    #[arg(long, value_name = "KBPS", default_value_t = 8000,
          value_parser = clap::value_parser!(u32).range(200..=50000))]
    max_bitrate: u32,

    /// STUN server (HOST:PORT) for learning this machine's public address
    #[arg(long, value_name = "HOST:PORT")]
    stun: Option<String>,

    /// TURN server for viewers that cannot reach this machine directly, such
    /// as turn:turn.example.com:3478
    #[arg(long, value_name = "URL", requires_all = ["turn_user", "turn_password"])]
    turn: Option<String>,

    /// User name for the TURN server
    #[arg(long, value_name = "NAME", requires = "turn")]
    turn_user: Option<String>,

    /// Password for the TURN server; it is handed to every viewer
    #[arg(long, value_name = "PASSWORD", requires = "turn")]
    turn_password: Option<String>,

    /// Cloudflare TURN key ID; viewers get short-lived credentials made from
    /// it, and Cloudflare's STUN server is used unless --stun is given
    #[arg(
        long,
        value_name = "ID",
        env = "CLOUDFLARE_TURN_KEY_ID",
        requires = "cloudflare_turn_token",
        conflicts_with = "turn"
    )]
    cloudflare_turn_key: Option<String>,

    /// API token of the Cloudflare TURN key
    #[arg(
        long,
        value_name = "TOKEN",
        env = "CLOUDFLARE_TURN_KEY_API_TOKEN",
        hide_env_values = true,
        requires = "cloudflare_turn_key"
    )]
    cloudflare_turn_token: Option<String>,
}

impl Args {
    /// The `iceServers` list for the page's `RTCPeerConnection`.
    fn ice_servers(&self) -> String {
        let mut servers = Vec::new();
        if let Some(stun) = &self.stun {
            servers.push(format!(r#"{{"urls":"stun:{}"}}"#, json_escape(stun)));
        }
        if let (Some(url), Some(user), Some(password)) =
            (&self.turn, &self.turn_user, &self.turn_password)
        {
            servers.push(format!(
                r#"{{"urls":"{}","username":"{}","credential":"{}"}}"#,
                json_escape(url),
                json_escape(user),
                json_escape(password)
            ));
        }
        format!(r#"{{"iceServers":[{}]}}"#, servers.join(","))
    }

    /// Where to ask for this machine's public address: the STUN server,
    /// Cloudflare's when using its TURN service, or failing that a plain-UDP
    /// TURN server, which answers STUN requests too.
    fn stun_server(&self) -> Option<String> {
        if let Some(stun) = &self.stun {
            return Some(stun.clone());
        }
        if self.cloudflare_turn_key.is_some() {
            return Some(turn::STUN_SERVER.to_string());
        }
        let rest = self.turn.as_deref()?.strip_prefix("turn:")?;
        let host = rest.split('?').next()?;
        let has_port = !host.ends_with(']')
            && host
                .rsplit_once(':')
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
        Some(if has_port {
            host.to_string()
        } else {
            format!("{host}:3478")
        })
    }
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
                    // Skip reading the position when it is not needed: every
                    // camera round trip delays the move.
                    let absolute =
                        pan.is_some() && tilt.is_some() && dpan.is_none() && dtilt.is_none();
                    let current = if absolute {
                        None
                    } else {
                        Some(self.camera.position()?)
                    };
                    let pan = pan.or(current.as_ref().map(|c| c.pan)).unwrap_or_default()
                        + dpan.unwrap_or(0.0);
                    let tilt = tilt
                        .or(current.as_ref().map(|c| c.tilt))
                        .unwrap_or_default()
                        + dtilt.unwrap_or(0.0);
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
    hub: Arc<Hub>,
    turn: Option<CloudflareTurn>,
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

    /// The `iceServers` for the page: fresh Cloudflare credentials if
    /// configured, otherwise the servers given on the command line.
    fn ice_servers(&self) -> String {
        if let Some(turn) = &self.turn {
            match turn.ice_servers() {
                Ok(json) => return json,
                Err(error) => eprintln!("ilink-web: {error}"),
            }
        }
        self.hub.ice_servers().to_string()
    }

    /// Runs an API call given as a path with a query string and returns the
    /// JSON reply, for commands arriving over WebRTC.
    fn command(&self, target: &str) -> String {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        self.api(path, query).1
    }

    fn handle(self: &Arc<Self>, mut stream: TcpStream) {
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
        let mut body = Vec::new();
        let _ = reader.take(body_len.min(MAX_BODY)).read_to_end(&mut body);

        let mut words = request.split_whitespace();
        let method = words.next().unwrap_or("GET");
        let target = words.next().unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        match path {
            "/" | "/index.html" => respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                PAGE.as_bytes(),
            ),
            "/webrtc" if method == "POST" => {
                let local_ip = stream
                    .local_addr()
                    .map_or(IpAddr::from([127, 0, 0, 1]), |a| a.ip());
                let server = Arc::clone(self);
                let control: webrtc::Control = Arc::new(move |t: &str| server.command(t));
                let offer = String::from_utf8_lossy(&body);
                match self.hub.answer(&offer, local_ip, control) {
                    Ok(answer) => respond(
                        &mut stream,
                        "201 Created",
                        "application/sdp",
                        answer.as_bytes(),
                    ),
                    Err(error) => respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        error_json(&error).as_bytes(),
                    ),
                }
            }
            "/webrtc" => {
                let servers = self.ice_servers();
                respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    servers.as_bytes(),
                );
            }
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

fn json_escape(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            c if c.is_control() => escaped.push(' '),
            c => escaped.push(c),
        }
    }
    escaped
}

fn error_json(message: &str) -> String {
    format!(r#"{{"error":"{}"}}"#, json_escape(message))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let (width, height) = args.resolution;
    str0m::crypto::from_feature_flags().install_process_default();
    let video = Video::new(video::Settings {
        device: args.device.clone(),
        width,
        height,
        fps: args.fps,
    });
    let options = webrtc::Options {
        video: Arc::clone(&video),
        fps: args.fps,
        max_bitrate: args.max_bitrate * 1000,
        stun: args.stun_server(),
        ice_servers: args.ice_servers(),
    };
    let udp = args.udp.unwrap_or(args.listen);
    let hub = match Hub::start(udp, options) {
        Ok(hub) => hub,
        Err(error) => {
            eprintln!("ilink-web: cannot listen on {udp} (UDP): {error}");
            return ExitCode::FAILURE;
        }
    };
    let turn = args
        .cloudflare_turn_key
        .zip(args.cloudflare_turn_token)
        .map(|(key, token)| CloudflareTurn::new(key, token));
    let server = Arc::new(Server {
        video,
        hub,
        turn,
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
    fn ice_servers_list_stun_and_turn() {
        let args = Args::parse_from([
            "ilink-web",
            "--stun",
            "stun.example.com:3478",
            "--turn",
            "turn:turn.example.com?transport=udp",
            "--turn-user",
            "me",
            "--turn-password",
            "p\"w",
        ]);
        assert_eq!(
            args.ice_servers(),
            r#"{"iceServers":[{"urls":"stun:stun.example.com:3478"},{"urls":"turn:turn.example.com?transport=udp","username":"me","credential":"p\"w"}]}"#
        );
        assert_eq!(args.stun_server().as_deref(), Some("stun.example.com:3478"));
        let plain = Args::parse_from(["ilink-web"]);
        assert_eq!(plain.ice_servers(), r#"{"iceServers":[]}"#);
        assert_eq!(plain.stun_server(), None);
    }

    #[test]
    fn turn_server_doubles_as_stun_server() {
        let turn = |url: &str| {
            let args = [
                "ilink-web",
                "--turn",
                url,
                "--turn-user",
                "u",
                "--turn-password",
                "p",
            ];
            Args::parse_from(args).stun_server()
        };
        assert_eq!(
            turn("turn:t.example.com").as_deref(),
            Some("t.example.com:3478")
        );
        assert_eq!(
            turn("turn:t.example.com:3479?transport=udp").as_deref(),
            Some("t.example.com:3479")
        );
        assert_eq!(turn("turns:t.example.com:5349"), None);
    }

    #[test]
    fn cloudflare_turn_uses_cloudflare_stun() {
        let cloudflare = ["--cloudflare-turn-key", "k", "--cloudflare-turn-token", "t"];
        let args = Args::try_parse_from(["ilink-web"].iter().chain(&cloudflare)).unwrap();
        assert_eq!(args.stun_server().as_deref(), Some(turn::STUN_SERVER));
        let with_turn = [
            "ilink-web",
            "--turn",
            "turn:x",
            "--turn-user",
            "u",
            "--turn-password",
            "p",
        ];
        assert!(Args::try_parse_from(with_turn.iter().chain(&cloudflare)).is_err());
        assert!(Args::try_parse_from(["ilink-web", "--cloudflare-turn-key", "k"]).is_err());
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
