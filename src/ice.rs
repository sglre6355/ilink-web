//! The `iceServers` list the page hands to `RTCPeerConnection`.
//!
//! It comes from `--ice-servers-command` when one is given, so that a TURN
//! service's short-lived credentials can be fetched by whatever means it
//! offers, and otherwise lists the `--stun` server alone.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long the command may take before the page gets the fallback list.
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct IceServers {
    command: Option<String>,
    stun: Option<String>,
    timeout: Duration,
}

impl IceServers {
    pub fn new(command: Option<String>, stun: Option<String>) -> Self {
        Self {
            command,
            stun,
            timeout: TIMEOUT,
        }
    }

    /// An `{"iceServers": [...]}` object: the command's output, or if there
    /// is no command or it fails, one with the STUN server.
    pub fn json(&self) -> String {
        if let Some(command) = &self.command {
            match run(command, self.timeout) {
                Ok(json) => return json,
                Err(error) => eprintln!("ilink-web: --ice-servers-command: {error}"),
            }
        }
        let servers: Vec<Value> = self
            .stun
            .iter()
            .map(|stun| json!({ "urls": format!("stun:{stun}") }))
            .collect();
        json!({ "iceServers": servers }).to_string()
    }
}

/// Runs `command` with `sh -c` and returns its output, which must be a JSON
/// object with an `iceServers` list.
fn run(command: &str, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run sh: {e}"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = thread::spawn(move || {
        let mut output = String::new();
        stdout.read_to_string(&mut output).map(|_| output)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("no answer within {} s", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(format!("failed ({status})"));
    }
    let output = reader
        .join()
        .map_err(|_| "reading its output failed".to_string())?
        .map_err(|e| format!("reading its output: {e}"))?;
    let value: Value =
        serde_json::from_str(&output).map_err(|e| format!("output is not JSON: {e}"))?;
    if !value.get("iceServers").is_some_and(Value::is_array) {
        return Err("output has no \"iceServers\" list".into());
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers(command: Option<&str>, stun: Option<&str>) -> String {
        IceServers {
            command: command.map(String::from),
            stun: stun.map(String::from),
            timeout: Duration::from_secs(2),
        }
        .json()
    }

    #[test]
    fn lists_the_stun_server_without_a_command() {
        assert_eq!(servers(None, None), r#"{"iceServers":[]}"#);
        assert_eq!(
            servers(None, Some("stun.example.com:3478")),
            r#"{"iceServers":[{"urls":"stun:stun.example.com:3478"}]}"#
        );
    }

    #[test]
    fn passes_on_the_commands_list() {
        let command = r#"echo '{"iceServers":[{"urls":"turn:t.example.com","username":"u","credential":"p"}]}'"#;
        assert_eq!(
            servers(Some(command), Some("stun.example.com:3478")),
            r#"{"iceServers":[{"credential":"p","urls":"turn:t.example.com","username":"u"}]}"#
        );
    }

    #[test]
    fn falls_back_when_the_command_fails() {
        let fallback = r#"{"iceServers":[{"urls":"stun:s.example.com:3478"}]}"#;
        for command in [
            "exit 1",
            "echo not json",
            r#"echo '{"servers":[]}'"#,
            "sleep 5",
        ] {
            assert_eq!(
                servers(Some(command), Some("s.example.com:3478")),
                fallback,
                "{command}"
            );
        }
    }
}
