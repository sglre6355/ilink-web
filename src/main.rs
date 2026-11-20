//! A web page to watch and steer an Insta360 Link webcam.
//!
//! The server sends the camera's video to the page over WebRTC (or as plain
//! MJPEG over HTTP) and moves the gimbal with the `insta360-link` library.

mod camera;
mod encode;
#[cfg(feature = "gpu")]
mod gpu;
mod http;
mod ice;
mod stun;
mod video;
mod webrtc;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use camera::Gimbal;
use http::App;
use ice::IceServers;
use video::Video;
use webrtc::Hub;

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
    /// for TURN servers and their credentials; its output is reused for an
    /// hour
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

#[tokio::main]
async fn main() -> ExitCode {
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
    let app = Arc::new(App {
        video,
        hub,
        ice_servers: IceServers::new(args.ice_servers_command, args.stun),
        gimbal: Gimbal::new(args.device),
    });

    // Open the camera up front so a missing camera is reported straight away.
    match app.gimbal.open() {
        Ok(description) => eprintln!("{description}"),
        Err(error) => eprintln!("ilink-web: {error}; will retry when the page asks"),
    }

    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("ilink-web: cannot listen on {}: {error}", args.listen);
            return ExitCode::FAILURE;
        }
    };
    eprintln!("serving on http://{}/", args.listen);
    match http::serve(listener, app).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ilink-web: {error}");
            ExitCode::FAILURE
        }
    }
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
