//! The web server: the page, the video as MJPEG, WebRTC setup and the gimbal
//! API.
//!
//! Camera calls and waits for frames block, so they run on tokio's blocking
//! threads; the WebRTC sessions run on threads of their own.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::middleware::map_response;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::serve::IncomingStream;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tokio_stream::wrappers::ReceiverStream;

use crate::camera::{self, Command, Gimbal, error_json};
use crate::ice::IceServers;
use crate::video::{Frame, Video, Viewer};
use crate::webrtc::{self, Hub};

const PAGE: &str = include_str!("index.html");
const BOUNDARY: &str = "ilinkwebframe";
/// Largest request body taken; SDP offers are a few kilobytes.
const MAX_BODY: usize = 64 * 1024;
/// How long to wait for the camera's first picture.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an MJPEG viewer may stop taking frames before it is dropped, so
/// that a stalled connection does not keep the camera open.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

pub struct App {
    pub gimbal: Gimbal,
    pub video: Arc<Video>,
    pub hub: Arc<Hub>,
    pub ice_servers: IceServers,
}

impl App {
    /// Carries out a command and returns the status and the JSON reply: the
    /// camera state, or an error.
    fn run(&self, command: &Command) -> (StatusCode, String) {
        match self.gimbal.run(command) {
            Ok(state) => (
                StatusCode::OK,
                serde_json::to_string(&state).expect("a state always serializes"),
            ),
            Err(error @ camera::Error::Unavailable(_)) => {
                (StatusCode::SERVICE_UNAVAILABLE, error.json())
            }
            Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.json()),
        }
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/index.html", get(page))
        .route(
            "/webrtc",
            get(ice_servers)
                .post(answer)
                .layer(DefaultBodyLimit::max(MAX_BODY)),
        )
        .route("/stream", get(stream))
        .route("/snapshot", get(snapshot))
        .route("/api/{*command}", get(api))
        .fallback(|| async { (StatusCode::NOT_FOUND, "not found") })
        .layer(map_response(no_store))
        .with_state(app)
}

/// Serves the app until the listener fails.
pub async fn serve(listener: TcpListener, app: Arc<App>) -> std::io::Result<()> {
    let service = router(app).into_make_service_with_connect_info::<LocalAddr>();
    axum::serve(listener, service).await
}

/// The address a connection came in on, which is the one the browser can
/// reach this machine at.
#[derive(Clone, Copy)]
struct LocalAddr(Option<SocketAddr>);

impl Connected<IncomingStream<'_, TcpListener>> for LocalAddr {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Self(stream.io().local_addr().ok())
    }
}

/// Every reply is live, so none may be cached.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn json(status: StatusCode, body: String) -> Response {
    (status, [(CONTENT_TYPE, "application/json")], body).into_response()
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn ice_servers(State(app): State<Arc<App>>) -> Response {
    let servers = tokio::task::spawn_blocking(move || app.ice_servers.json())
        .await
        .expect("the ICE command does not panic");
    json(StatusCode::OK, servers)
}

/// Takes the browser's SDP offer and answers with ours.
async fn answer(
    State(app): State<Arc<App>>,
    ConnectInfo(LocalAddr(local)): ConnectInfo<LocalAddr>,
    offer: Bytes,
) -> Response {
    let local_ip = local.map_or([127, 0, 0, 1].into(), |a| a.ip());
    let control: webrtc::Control = {
        let app = Arc::clone(&app);
        Arc::new(move |command: &Command| app.run(command).1)
    };
    match app
        .hub
        .answer(&String::from_utf8_lossy(&offer), local_ip, control)
    {
        Ok(answer) => (
            StatusCode::CREATED,
            [(CONTENT_TYPE, "application/sdp")],
            answer,
        )
            .into_response(),
        Err(error) => json(StatusCode::BAD_REQUEST, error_json(&error)),
    }
}

async fn api(State(app): State<Arc<App>>, uri: Uri) -> Response {
    let target = uri.path_and_query().map_or(uri.path(), |p| p.as_str());
    let Some(command) = Command::parse(target) else {
        return json(StatusCode::NOT_FOUND, error_json("not found"));
    };
    let (status, body) = tokio::task::spawn_blocking(move || app.run(&command))
        .await
        .expect("camera calls do not panic");
    json(status, body)
}

/// Waits for the viewer's next frame on a blocking thread.
async fn next_frame(mut viewer: Viewer) -> (Viewer, Result<Frame, String>) {
    tokio::task::spawn_blocking(move || {
        let frame = viewer.next(FRAME_TIMEOUT);
        (viewer, frame)
    })
    .await
    .expect("waiting for a frame does not panic")
}

async fn snapshot(State(app): State<Arc<App>>) -> Response {
    match next_frame(app.video.watch()).await.1 {
        Ok(frame) => (
            [(CONTENT_TYPE, "image/jpeg")],
            Body::from(frame.as_ref().clone()),
        )
            .into_response(),
        Err(error) => json(StatusCode::SERVICE_UNAVAILABLE, error_json(&error)),
    }
}

/// Sends frames as `multipart/x-mixed-replace` until the viewer goes away
/// or the camera stops sending pictures.
async fn stream(State(app): State<Arc<App>>) -> Response {
    let (viewer, first) = next_frame(app.video.watch()).await;
    let first = match first {
        Ok(frame) => frame,
        Err(error) => return json(StatusCode::SERVICE_UNAVAILABLE, error_json(&error)),
    };
    let interval = Duration::from_secs(1) / app.video.settings().fps;
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
    tokio::spawn(async move {
        let (mut viewer, mut frame) = (viewer, first);
        let mut due = Instant::now();
        loop {
            let mut part = format!(
                "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                frame.len()
            )
            .into_bytes();
            part.extend_from_slice(&frame);
            part.extend_from_slice(b"\r\n");
            if tx
                .send_timeout(Ok(part.into()), SEND_TIMEOUT)
                .await
                .is_err()
            {
                return;
            }
            // Pace each viewer to --fps however fast the camera delivers.
            due += interval;
            if due > Instant::now() {
                sleep_until(due).await;
            } else {
                due = Instant::now();
            }
            let next;
            (viewer, next) = next_frame(viewer).await;
            frame = match next {
                Ok(frame) => frame,
                // End the response; the page reconnects by itself.
                Err(_) => return,
            };
        }
    });
    (
        [(
            CONTENT_TYPE,
            format!("multipart/x-mixed-replace;boundary={BOUNDARY}"),
        )],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}
