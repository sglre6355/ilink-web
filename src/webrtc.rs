//! Sends the camera to browsers over WebRTC and takes gimbal commands on a
//! data channel.
//!
//! All sessions share one UDP socket. A router thread reads it and hands each
//! packet to the session it belongs to: by the ICE username in STUN requests,
//! and by the sender's address after that. Each session runs its own thread
//! for the WebRTC state machine, one for encoding video and one for camera
//! commands, so that none of them waits for another.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::encode::{self, Encoder, Picture};
use crate::stun;
use crate::video::Video;

/// Bitrate the video starts at before the bandwidth estimate settles.
const START_BITRATE: u32 = 1_500_000;
const MIN_BITRATE: u32 = 150_000;
/// Share of the estimated bandwidth given to the encoder, leaving room for
/// retransmissions and packet overhead.
const BITRATE_SHARE: f64 = 0.85;
/// How long a session may take to connect before it is dropped.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How often camera state is pushed to an idle viewer.
const STATE_INTERVAL: Duration = Duration::from_millis(1500);
/// How often the public address is looked up again from the STUN server.
const STUN_REFRESH: Duration = Duration::from_secs(60);

/// Runs a gimbal command such as `/api/move?pan=10` and returns the JSON reply.
pub type Control = Arc<dyn Fn(&str) -> String + Send + Sync>;

pub struct Options {
    pub video: Arc<Video>,
    pub fps: u32,
    pub max_bitrate: u32,
    /// STUN server to learn this machine's public address from, so that
    /// viewers can reach it through NAT and TURN relays.
    pub stun: Option<String>,
}

pub struct Hub {
    socket: UdpSocket,
    /// The address the socket is bound to.
    bound: SocketAddr,
    options: Options,
    routes: Mutex<Routes>,
    /// This machine's address as seen from the internet, if known.
    public: Mutex<Option<SocketAddr>>,
    stun_query: Mutex<Option<stun::TransactionId>>,
}

#[derive(Default)]
struct Routes {
    by_ufrag: HashMap<String, Sender<Msg>>,
    by_addr: HashMap<SocketAddr, String>,
}

enum Msg {
    Packet(Instant, SocketAddr, Vec<u8>),
    Frame(Frame),
    Reply(String),
}

struct Frame {
    data: Vec<u8>,
    captured: Instant,
    pts: Duration,
}

/// What a session and its encoder thread share.
struct Shared {
    connected: AtomicBool,
    keyframe: AtomicBool,
    bitrate: AtomicU32,
    closed: AtomicBool,
}

impl Hub {
    /// Binds the UDP socket and starts the router (and STUN) threads.
    pub fn start(addr: SocketAddr, options: Options) -> io::Result<Arc<Self>> {
        let socket = UdpSocket::bind(addr)?;
        let bound = socket.local_addr()?;
        let hub = Arc::new(Self {
            socket,
            bound,
            options,
            routes: Mutex::default(),
            public: Mutex::new(None),
            stun_query: Mutex::new(None),
        });
        let router = Arc::clone(&hub);
        thread::spawn(move || router.route());
        if let Some(server) = hub.options.stun.clone() {
            let hub = Arc::clone(&hub);
            thread::spawn(move || hub.query_public_address(&server));
        }
        Ok(hub)
    }

    fn routes(&self) -> MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads the socket forever, passing packets to their sessions.
    fn route(&self) {
        let mut buf = vec![0u8; 2048];
        loop {
            let (n, source) = match self.socket.recv_from(&mut buf) {
                Ok(received) => received,
                Err(error) => {
                    // ICMP errors from an earlier send show up here on Linux.
                    if error.kind() != io::ErrorKind::ConnectionRefused {
                        eprintln!("ilink-web: webrtc: receive: {error}");
                        thread::sleep(Duration::from_millis(10));
                    }
                    continue;
                }
            };
            let data = &buf[..n];
            if let Some(id) = *self.stun_query.lock().unwrap_or_else(|e| e.into_inner())
                && let Some(public) = stun::mapped_address(data, &id)
            {
                let mut known = self.public.lock().unwrap_or_else(|e| e.into_inner());
                if *known != Some(public) {
                    eprintln!("ilink-web: webrtc: public address is {public}");
                }
                *known = Some(public);
                continue;
            }
            let mut routes = self.routes();
            if let Some(ufrag) = stun::request_ufrag(data)
                && routes.by_ufrag.contains_key(ufrag)
            {
                routes.by_addr.insert(source, ufrag.to_string());
            }
            let Some(session) = routes
                .by_addr
                .get(&source)
                .and_then(|ufrag| routes.by_ufrag.get(ufrag))
            else {
                continue;
            };
            let _ = session.send(Msg::Packet(Instant::now(), source, data.to_vec()));
        }
    }

    /// Keeps asking the STUN server for our public address.
    fn query_public_address(&self, server: &str) {
        loop {
            let target = server.to_socket_addrs().ok().and_then(|mut addrs| {
                let v4 = self.socket.local_addr().is_ok_and(|a| a.is_ipv4());
                addrs.find(|a| a.is_ipv4() == v4)
            });
            match target {
                Some(target) => {
                    let id = transaction_id();
                    *self.stun_query.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                    // UDP may drop a request, so send a few.
                    for _ in 0..3 {
                        let _ = self.socket.send_to(&stun::binding_request(&id), target);
                        thread::sleep(Duration::from_millis(500));
                    }
                }
                None => eprintln!("ilink-web: webrtc: cannot resolve STUN server {server}"),
            }
            thread::sleep(STUN_REFRESH);
        }
    }

    /// Accepts a browser's SDP offer, starts a session for it and returns
    /// the SDP answer. `local_ip` is the address the browser reached the web
    /// page on.
    pub fn answer(
        self: &Arc<Self>,
        offer: &str,
        local_ip: IpAddr,
        control: Control,
    ) -> Result<String, String> {
        let offer = SdpOffer::from_sdp_string(&usable_candidates(offer))
            .map_err(|e| format!("bad offer: {e}"))?;
        let mut config = RtcConfig::new()
            .clear_codecs()
            .enable_bwe(Some(Bitrate::bps(START_BITRATE.into())));
        // Constrained baseline with packetization mode 1, which every
        // browser decodes, and RTX for retransmissions.
        config
            .codec_config()
            .add_h264(Pt::from(108), Some(Pt::from(109)), true, 0x42e01f);
        let mut rtc = config.build(Instant::now());

        // Offer the address the socket is bound to, or when it listens on all
        // of them, the one the browser reached the page on.
        let ip = if self.bound.ip().is_unspecified() {
            local_ip.to_canonical()
        } else {
            self.bound.ip()
        };
        let host = SocketAddr::new(ip, self.bound.port());
        let candidate = Candidate::host(host, "udp").map_err(|e| e.to_string())?;
        rtc.add_local_candidate(candidate);
        if let Some(public) = *self.public.lock().unwrap_or_else(|e| e.into_inner())
            && public.ip() != host.ip()
            && let Ok(candidate) = Candidate::server_reflexive(public, host, "udp")
        {
            rtc.add_local_candidate(candidate);
        }
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| format!("cannot accept offer: {e}"))?;

        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let (tx, rx) = mpsc::channel();
        self.routes().by_ufrag.insert(ufrag.clone(), tx.clone());
        let hub = Arc::clone(self);
        thread::spawn(move || {
            Session::new(Arc::clone(&hub), rtc, host, tx, control).run(&rx);
            let mut routes = hub.routes();
            routes.by_ufrag.remove(&ufrag);
            routes.by_addr.retain(|_, u| *u != ufrag);
        });
        Ok(answer.to_sdp_string())
    }
}

/// Removes candidates the offer gives as mDNS names (`….local`), which
/// browsers use to hide local addresses. They cannot be resolved here, and
/// are not needed: the browser's checks reveal its address anyway.
fn usable_candidates(sdp: &str) -> String {
    sdp.split_inclusive('\n')
        .filter(|line| {
            !(line.starts_with("a=candidate:")
                && line
                    .split_whitespace()
                    .nth(4)
                    .is_some_and(|a| a.ends_with(".local")))
        })
        .collect()
}

/// A random STUN transaction id. It only has to be unpredictable enough not
/// to collide, so std's randomly seeded hasher is plenty.
fn transaction_id() -> stun::TransactionId {
    let mut id = [0u8; 12];
    for chunk in id.chunks_mut(8) {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(Instant::now().elapsed().as_nanos());
        chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
    }
    id
}

struct Session {
    hub: Arc<Hub>,
    rtc: Rtc,
    host: SocketAddr,
    shared: Arc<Shared>,
    commands: Sender<String>,
    video: Option<(Mid, Pt)>,
    channel: Option<ChannelId>,
    started: Instant,
}

impl Session {
    fn new(hub: Arc<Hub>, rtc: Rtc, host: SocketAddr, tx: Sender<Msg>, control: Control) -> Self {
        let shared = Arc::new(Shared {
            connected: AtomicBool::new(false),
            keyframe: AtomicBool::new(true),
            bitrate: AtomicU32::new(START_BITRATE),
            closed: AtomicBool::new(false),
        });
        // Start capturing straight away so the camera is warm by the time
        // the connection is up.
        let encoder = EncoderThread {
            video: Arc::clone(&hub.options.video),
            fps: hub.options.fps,
            shared: Arc::clone(&shared),
            tx: tx.clone(),
        };
        thread::spawn(move || encoder.run());
        let (commands, rx) = mpsc::channel();
        thread::spawn(move || run_commands(&rx, &control, &tx));
        Self {
            hub,
            rtc,
            host,
            shared,
            commands,
            video: None,
            channel: None,
            started: Instant::now(),
        }
    }

    fn run(mut self, rx: &Receiver<Msg>) {
        if let Err(error) = self.event_loop(rx) {
            eprintln!("ilink-web: webrtc: session ended: {error}");
        }
        self.shared.closed.store(true, Ordering::Relaxed);
    }

    fn event_loop(&mut self, rx: &Receiver<Msg>) -> Result<(), String> {
        loop {
            let timeout = loop {
                match self.rtc.poll_output().map_err(|e| e.to_string())? {
                    Output::Timeout(at) => break at,
                    Output::Transmit(t) => {
                        let _ = self.hub.socket.send_to(&t.contents, t.destination);
                    }
                    Output::Event(event) => {
                        if !self.handle_event(event) {
                            return Ok(());
                        }
                    }
                }
            };
            if !self.rtc.is_alive() {
                return Ok(());
            }
            if !self.shared.connected.load(Ordering::Relaxed)
                && self.started.elapsed() > CONNECT_TIMEOUT
            {
                return Err("timed out connecting".into());
            }
            let wait = timeout.saturating_duration_since(Instant::now());
            let input = match rx.recv_timeout(wait) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    self.rtc
                        .handle_input(Input::Timeout(Instant::now()))
                        .map_err(|e| e.to_string())?;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            match input {
                Msg::Packet(at, source, data) => {
                    let Ok(contents) = data.as_slice().try_into() else {
                        continue;
                    };
                    let receive = Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: self.host,
                        contents,
                    };
                    self.rtc
                        .handle_input(Input::Receive(at, receive))
                        .map_err(|e| e.to_string())?;
                }
                Msg::Frame(frame) => {
                    if let Some((mid, pt)) = self.video
                        && let Some(writer) = self.rtc.writer(mid)
                    {
                        let ticks = frame.pts.as_micros() * 9 / 100;
                        let rtp_time = MediaTime::new(ticks as u64, Frequency::NINETY_KHZ);
                        writer
                            .write(pt, frame.captured, rtp_time, frame.data)
                            .map_err(|e| e.to_string())?;
                    }
                }
                Msg::Reply(json) => {
                    if let Some(id) = self.channel
                        && let Some(mut channel) = self.rtc.channel(id)
                    {
                        let _ = channel.write(false, json.as_bytes());
                    }
                }
            }
        }
    }

    /// Reacts to one event; returns false when the session is over.
    fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return false,
            Event::Connected => {
                let max = Bitrate::bps(self.hub.options.max_bitrate.into());
                self.rtc.bwe().set_bitrate(Bitrate::bps(0), max);
                self.shared.keyframe.store(true, Ordering::Relaxed);
                self.shared.connected.store(true, Ordering::Relaxed);
            }
            Event::MediaAdded(media) if media.kind == MediaKind::Video => {
                let pt = self.rtc.writer(media.mid).and_then(|w| {
                    w.payload_params()
                        .find(|p| p.spec().codec == Codec::H264)
                        .map(|p| p.pt())
                });
                self.video = pt.map(|pt| (media.mid, pt));
            }
            Event::KeyframeRequest(_) => self.shared.keyframe.store(true, Ordering::Relaxed),
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                let bps = (estimate.as_f64() * BITRATE_SHARE) as u32;
                let bps = bps.clamp(MIN_BITRATE, self.hub.options.max_bitrate);
                self.shared.bitrate.store(bps, Ordering::Relaxed);
            }
            Event::ChannelOpen(id, _) => self.channel = Some(id),
            Event::ChannelClose(id) if self.channel == Some(id) => self.channel = None,
            Event::ChannelData(data) if !data.binary => {
                if let Ok(text) = String::from_utf8(data.data) {
                    let _ = self.commands.send(text);
                }
            }
            _ => {}
        }
        true
    }
}

struct EncoderThread {
    video: Arc<Video>,
    fps: u32,
    shared: Arc<Shared>,
    tx: Sender<Msg>,
}

impl EncoderThread {
    fn run(self) {
        let mut viewer = self.video.watch();
        let mut encoder: Option<Box<dyn Encoder>> = None;
        let mut picture = Picture::default();
        let start = Instant::now();
        while !self.shared.closed.load(Ordering::Relaxed) {
            // Waiting for the newest frame skips any that arrived while the
            // last one was being encoded, so a slow encoder costs frames,
            // not delay.
            let Ok(jpeg) = viewer.next(Duration::from_secs(5)) else {
                continue;
            };
            let captured = Instant::now();
            if !self.shared.connected.load(Ordering::Relaxed) {
                continue;
            }
            if let Err(error) = encode::decode_jpeg(&jpeg, &mut picture) {
                eprintln!("ilink-web: webrtc: {error}");
                continue;
            }
            let size = (picture.width, picture.height);
            let bitrate = self.shared.bitrate.load(Ordering::Relaxed);
            if encoder.as_ref().is_none_or(|e| e.size() != size) {
                match encode::open(size.0, size.1, self.fps, bitrate) {
                    Ok(opened) => {
                        eprintln!(
                            "ilink-web: webrtc: encoding {}x{} with {}",
                            size.0,
                            size.1,
                            opened.name()
                        );
                        encoder = Some(opened);
                        self.shared.keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(error) => {
                        eprintln!("ilink-web: webrtc: {error}");
                        return;
                    }
                }
            }
            let Some(enc) = encoder.as_mut() else {
                continue;
            };
            enc.set_bitrate(bitrate);
            let keyframe = self.shared.keyframe.swap(false, Ordering::Relaxed);
            match enc.encode(&picture, captured - start, keyframe) {
                Ok(data) if data.is_empty() => {}
                Ok(data) => {
                    let frame = Frame {
                        data,
                        captured,
                        pts: captured - start,
                    };
                    if self.tx.send(Msg::Frame(frame)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    eprintln!("ilink-web: webrtc: {} failed: {error}", enc.name());
                    encode::encoder_failed(enc.name());
                    encoder = None;
                }
            }
        }
    }
}

/// Runs camera commands from the data channel one at a time and sends back
/// the camera state, or pushes the state every so often while idle.
fn run_commands(rx: &Receiver<String>, control: &Control, tx: &Sender<Msg>) {
    loop {
        let batch: Vec<String> = match rx.recv_timeout(STATE_INTERVAL) {
            Ok(first) => std::iter::once(first).chain(rx.try_iter()).collect(),
            Err(RecvTimeoutError::Timeout) => vec!["/api/state".into()],
            Err(RecvTimeoutError::Disconnected) => return,
        };
        for (i, command) in batch.iter().enumerate() {
            // A move to an absolute position is overtaken by the next one, so
            // only the newest of a run of them is worth sending to the camera.
            if is_absolute_move(command) && batch.get(i + 1).is_some_and(|c| is_absolute_move(c)) {
                continue;
            }
            if tx.send(Msg::Reply(control(command))).is_err() {
                return;
            }
        }
    }
}

fn is_absolute_move(command: &str) -> bool {
    command.strip_prefix("/api/move?").is_some_and(|query| {
        query.split('&').all(|pair| {
            !pair.starts_with("dpan=") && !pair.starts_with("dtilt=") && !pair.starts_with("dzoom=")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_mdns_candidates_only() {
        let sdp = "v=0\r\n\
                   a=candidate:1 1 udp 2122260223 3f1a2b.local 54321 typ host\r\n\
                   a=candidate:2 1 udp 1686052607 203.0.113.5 54321 typ srflx raddr 0.0.0.0 rport 0\r\n\
                   a=mid:0\r\n";
        let kept = usable_candidates(sdp);
        assert!(!kept.contains(".local"));
        assert!(kept.contains("203.0.113.5"));
        assert!(kept.starts_with("v=0\r\n") && kept.ends_with("a=mid:0\r\n"));
    }

    #[test]
    fn recognises_absolute_moves() {
        assert!(is_absolute_move("/api/move?pan=1&tilt=2&zoom=1.5"));
        assert!(!is_absolute_move("/api/move?dpan=1"));
        assert!(!is_absolute_move("/api/center"));
    }

    #[test]
    fn transaction_ids_differ() {
        assert_ne!(transaction_id(), transaction_id());
    }
}
