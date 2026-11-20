//! The gimbal: the commands that move it, and its state as the page sees it.

use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard};

use insta360_link::{Camera, PanTiltUpdate};
use serde::Serialize;
use serde_json::json;

/// What the page asks of the gimbal, over HTTP or the WebRTC data channel,
/// as a path with a query string such as `/api/move?pan=10`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    State,
    Move(Move),
    Center,
}

/// A move to an absolute position (`pan`, `tilt`, `zoom`) or by an amount
/// (`dpan`, `dtilt`, `dzoom`); a missing value leaves that axis alone.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Move {
    pub pan: Option<f64>,
    pub tilt: Option<f64>,
    pub zoom: Option<f64>,
    pub dpan: Option<f64>,
    pub dtilt: Option<f64>,
    pub dzoom: Option<f64>,
}

impl Command {
    /// Reads a command from a path and query string. Values that are not
    /// finite numbers are left out.
    pub fn parse(target: &str) -> Option<Self> {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        match path {
            "/api/state" => Some(Self::State),
            "/api/center" => Some(Self::Center),
            "/api/move" => {
                let get = |key| param(query, key);
                Some(Self::Move(Move {
                    pan: get("pan"),
                    tilt: get("tilt"),
                    zoom: get("zoom"),
                    dpan: get("dpan"),
                    dtilt: get("dtilt"),
                    dzoom: get("dzoom"),
                }))
            }
            _ => None,
        }
    }

    /// Whether this is a move to an absolute position, which the next such
    /// move overtakes.
    pub fn is_absolute_move(&self) -> bool {
        matches!(self, Self::Move(m) if m.dpan.is_none() && m.dtilt.is_none() && m.dzoom.is_none())
    }
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

/// Where the gimbal is and how far it goes, as sent to the page.
#[derive(Debug, PartialEq, Serialize)]
pub struct State {
    pub pan: f64,
    pub tilt: f64,
    pub zoom: f64,
    pub privacy: bool,
    pub limits: Limits,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Limits {
    pub pan: [f64; 2],
    pub tilt: [f64; 2],
    pub zoom: [f64; 2],
}

pub enum Error {
    /// The camera could not be opened.
    Unavailable(insta360_link::Error),
    /// The camera did not carry out the command.
    Failed(insta360_link::Error),
}

impl Error {
    pub fn json(&self) -> String {
        match self {
            Self::Unavailable(error) | Self::Failed(error) => error_json(&error.to_string()),
        }
    }
}

/// `{"error": message}`, the reply to anything that went wrong.
pub fn error_json(message: &str) -> String {
    json!({ "error": message }).to_string()
}

/// The camera's gimbal, opened on first use and again after it fails, so
/// that unplugging and replugging the camera does not need a restart.
pub struct Gimbal {
    device: Option<String>,
    link: Mutex<Option<Link>>,
}

impl Gimbal {
    pub fn new(device: Option<String>) -> Self {
        Self {
            device,
            link: Mutex::new(None),
        }
    }

    fn link(&self) -> MutexGuard<'_, Option<Link>> {
        self.link.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Opens the camera if it is not open yet, and describes it.
    pub fn open(&self) -> Result<String, insta360_link::Error> {
        let mut link = self.link();
        let link = match link.as_mut() {
            Some(link) => link,
            None => link.insert(Link::open(self.device.as_deref())?),
        };
        let info = link.camera.info();
        Ok(format!(
            "{} at {}: pan {:?}, tilt {:?}, zoom {:?}",
            info.model.name(),
            info.id,
            link.pan,
            link.tilt,
            link.zoom
        ))
    }

    /// Carries out a command and returns the state that follows.
    pub fn run(&self, command: &Command) -> Result<State, Error> {
        let mut link = self.link();
        if link.is_none() {
            *link = Some(Link::open(self.device.as_deref()).map_err(Error::Unavailable)?);
        }
        let opened = link.as_ref().expect("opened above");
        let error = match opened.run(command) {
            Ok(state) => return Ok(state),
            Err(error) => error,
        };
        // Open the camera again next time if it went away.
        if opened.camera.position().is_err() {
            *link = None;
        }
        Err(Error::Failed(error))
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

    fn run(&self, command: &Command) -> insta360_link::Result<State> {
        match command {
            Command::State => {}
            Command::Move(m) => self.go(m)?,
            Command::Center => self.camera.recenter()?,
        }
        self.state()
    }

    fn go(&self, m: &Move) -> insta360_link::Result<()> {
        if m.pan.is_some() || m.tilt.is_some() || m.dpan.is_some() || m.dtilt.is_some() {
            // Skip reading the position when it is not needed: every camera
            // round trip delays the move.
            let absolute =
                m.pan.is_some() && m.tilt.is_some() && m.dpan.is_none() && m.dtilt.is_none();
            let current = if absolute {
                None
            } else {
                Some(self.camera.position()?)
            };
            let pan = m
                .pan
                .or(current.as_ref().map(|c| c.pan))
                .unwrap_or_default()
                + m.dpan.unwrap_or(0.0);
            let tilt = m
                .tilt
                .or(current.as_ref().map(|c| c.tilt))
                .unwrap_or_default()
                + m.dtilt.unwrap_or(0.0);
            self.camera.set_position(PanTiltUpdate {
                pan: Some(clamp(pan, &self.pan).round()),
                tilt: Some(clamp(tilt, &self.tilt).round()),
            })?;
        }
        if m.zoom.is_some() || m.dzoom.is_some() {
            let current = self.camera.zoom()?;
            let zoom = m.zoom.unwrap_or(current) + m.dzoom.unwrap_or(0.0);
            let zoom = (clamp(zoom, &self.zoom) * 10.0).round() / 10.0;
            if (zoom - current).abs() > 0.05 {
                self.camera.set_zoom(zoom)?;
            }
        }
        Ok(())
    }

    fn state(&self) -> insta360_link::Result<State> {
        let position = self.camera.position()?;
        let range = |r: &RangeInclusive<f64>| [*r.start(), *r.end()];
        Ok(State {
            pan: position.pan,
            tilt: position.tilt,
            zoom: self.camera.zoom()?,
            privacy: self.camera.privacy().unwrap_or(false),
            limits: Limits {
                pan: range(&self.pan),
                tilt: range(&self.tilt),
                zoom: range(&self.zoom),
            },
        })
    }
}

fn clamp(value: f64, range: &RangeInclusive<f64>) -> f64 {
    value.clamp(*range.start(), *range.end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(Command::parse("/api/state"), Some(Command::State));
        assert_eq!(Command::parse("/api/center?x=1"), Some(Command::Center));
        assert_eq!(Command::parse("/api/other"), None);
        assert_eq!(
            Command::parse("/api/move?pan=12.5&tilt=-3&zoom=NaN&dpan=inf&dzoom=abc"),
            Some(Command::Move(Move {
                pan: Some(12.5),
                tilt: Some(-3.0),
                ..Move::default()
            }))
        );
    }

    #[test]
    fn recognises_absolute_moves() {
        let absolute = |target| Command::parse(target).unwrap().is_absolute_move();
        assert!(absolute("/api/move?pan=1&tilt=2&zoom=1.5"));
        assert!(!absolute("/api/move?dpan=1"));
        assert!(!absolute("/api/move?zoom=2&dzoom=1"));
        assert!(!absolute("/api/center"));
    }

    #[test]
    fn state_serializes_as_the_page_expects() {
        let state = State {
            pan: 35.0,
            tilt: -28.0,
            zoom: 1.5,
            privacy: false,
            limits: Limits {
                pan: [-145.0, 145.0],
                tilt: [-90.0, 100.0],
                zoom: [1.0, 4.0],
            },
        };
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"pan":35.0,"tilt":-28.0,"zoom":1.5,"privacy":false,"limits":{"pan":[-145.0,145.0],"tilt":[-90.0,100.0],"zoom":[1.0,4.0]}}"#
        );
    }

    #[test]
    fn error_json_escapes() {
        assert_eq!(
            error_json("bad \"value\"\n\\"),
            r#"{"error":"bad \"value\"\n\\"}"#
        );
    }
}
