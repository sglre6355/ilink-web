//! A web page to watch and steer an Insta360 Link webcam.
//!
//! The server sends the camera's video to the page over WebRTC (or as plain
//! MJPEG over HTTP) and moves the gimbal with the `insta360-link` library.

mod camera;
mod encode;
#[cfg(feature = "gpu")]
mod gpu;
mod ice;
mod stun;
mod video;
mod webrtc;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;

use camera::{Command, Gimbal, error_json};
use ice::IceServers;
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

    /// STUN server (HOST:PORT) to learn this machine's public address from;
    /// the page also gets it unless --ice-servers-command is given
    #[arg(long, value_name = "HOST:PORT")]
    stun: Option<String>,

    /// Shell command printing the page's `{"iceServers": [...]}` as JSON,
    /// run each time a page connects; for TURN servers and their credentials
    #[arg(long, value_name = "COMMAND")]
    ice_servers_command: Option<String>,
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

struct Server {
    gimbal: Gimbal,
    video: Arc<Video>,
    hub: Arc<Hub>,
    ice_servers: IceServers,
}

impl Server {
    /// Carries out a command and returns the HTTP status and the JSON reply:
    /// the camera state, or an error.
    fn run(&self, command: &Command) -> (&'static str, String) {
        match self.gimbal.run(command) {
            Ok(state) => (
                "200 OK",
                serde_json::to_string(&state).expect("a state always serializes"),
            ),
            Err(error @ camera::Error::Unavailable(_)) => ("503 Service Unavailable", error.json()),
            Err(error) => ("500 Internal Server Error", error.json()),
        }
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
        let path = target.split_once('?').map_or(target, |(path, _)| path);
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
                let control: webrtc::Control = Arc::new(move |c: &Command| server.run(c).1);
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
                let servers = self.ice_servers.json();
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
                let (status, json) = match Command::parse(target) {
                    Some(command) => self.run(&command),
                    None => ("404 Not Found", error_json("not found")),
                };
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
        stun: args.stun.clone(),
    };
    let udp = args.udp.unwrap_or(args.listen);
    let hub = match Hub::start(udp, options) {
        Ok(hub) => hub,
        Err(error) => {
            eprintln!("ilink-web: cannot listen on {udp} (UDP): {error}");
            return ExitCode::FAILURE;
        }
    };
    let server = Arc::new(Server {
        video,
        hub,
        ice_servers: IceServers::new(args.ice_servers_command, args.stun),
        gimbal: Gimbal::new(args.device),
    });

    // Open the camera up front so a missing camera is reported straight away.
    match server.gimbal.open() {
        Ok(description) => eprintln!("{description}"),
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
    fn resolution_parses_width_and_height() {
        assert_eq!(resolution("1920x1080"), Ok((1920, 1080)));
        assert_eq!(resolution("1280X720"), Ok((1280, 720)));
        assert!(resolution("1280").is_err());
        assert!(resolution("0x720").is_err());
        assert!(resolution("axb").is_err());
    }
}
