//! Short-lived TURN credentials from Cloudflare's TURN service.
//!
//! The TURN key's API token is a long-term secret, so it stays here: the page
//! only ever receives credentials generated from it, which expire.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const API: &str = "https://rtc.live.cloudflare.com/v1/turn/keys";
/// How long generated credentials stay valid. A viewer keeps using them for
/// as long as they stay connected, so this bounds a session's length.
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long generated credentials are handed out before new ones are made.
/// Reusing them spares an API call each time a page connects.
const REUSE: Duration = Duration::from_secs(60 * 60);
/// Cloudflare's STUN server, which `ilink-web` asks for its public address.
pub const STUN_SERVER: &str = "stun.cloudflare.com:3478";

pub struct CloudflareTurn {
    key_id: String,
    token: String,
    agent: ureq::Agent,
    cached: Mutex<Option<(Instant, String)>>,
}

impl CloudflareTurn {
    pub fn new(key_id: String, token: String) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Self {
            key_id,
            token,
            agent,
            cached: Mutex::new(None),
        }
    }

    /// An `{"iceServers": [...]}` object for `RTCPeerConnection`, with STUN
    /// and TURN entries and fresh credentials.
    pub fn ice_servers(&self) -> Result<String, String> {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((made, json)) = cached.as_ref()
            && made.elapsed() < REUSE
        {
            return Ok(json.clone());
        }
        let url = format!("{API}/{}/credentials/generate-ice-servers", self.key_id);
        let body = format!(r#"{{"ttl":{}}}"#, TTL.as_secs());
        let json = self
            .agent
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .content_type("application/json")
            .send(body)
            .and_then(|mut response| response.body_mut().read_to_string())
            .map_err(|e| format!("Cloudflare TURN: {e}"))?;
        if !json.trim_start().starts_with('{') || !json.contains("\"iceServers\"") {
            return Err("Cloudflare TURN: unexpected reply".into());
        }
        *cached = Some((Instant::now(), json.clone()));
        Ok(json)
    }
}
