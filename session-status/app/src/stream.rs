//! `claude-sessions stream`: the remote half of multi-host support. Run on another machine
//! (normally over ssh by the widget — see remote.rs), it prints that machine's sessions as one
//! JSON line per change, plus a repeat every HEARTBEAT_SECS so the watcher can tell a quiet host
//! from a dead link. Every line is a full snapshot, so a reader never has to merge deltas.
//! It exits as soon as stdout is gone, so a dropped ssh leaves nothing running on the remote.

use crate::paths::{f64_of, str_of, unix_now};
use crate::{model, platform};
use serde_json::{json, Value};
use std::io::Write;
use std::time::{Duration, Instant};

/// Bumped when a line's shape changes incompatibly; the widget refuses other versions with a
/// hint to update the remote binary instead of silently drawing garbage.
pub const PROTOCOL: u64 = 1;
pub const HEARTBEAT_SECS: u64 = 15;

pub fn run(interval_secs: f64) -> i32 {
    let host = platform::hostname();
    let interval = Duration::from_secs_f64(interval_secs.clamp(0.5, 60.0));
    let heartbeat = Duration::from_secs(HEARTBEAT_SECS);
    let mut last: Option<Vec<Value>> = None;
    let mut last_emit: Option<Instant> = None;
    let mut out = std::io::stdout().lock();
    loop {
        let sessions: Vec<Value> = model::load_with(false).iter().map(model::to_wire).collect();
        let due = last_emit.is_none_or(|t| t.elapsed() >= heartbeat);
        if due || last.as_ref() != Some(&sessions) {
            let line = snapshot_line(&host, unix_now(), &sessions);
            // Rust ignores SIGPIPE, so a closed pipe surfaces here as an error, not a kill.
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return 0;
            }
            last = Some(sessions);
            last_emit = Some(Instant::now());
        }
        std::thread::sleep(interval);
    }
}

pub fn snapshot_line(host: &str, now: f64, sessions: &[Value]) -> String {
    json!({ "v": PROTOCOL, "host": host, "now": now, "sessions": sessions }).to_string()
}

#[derive(Debug)]
pub struct Snapshot {
    pub host: String,
    /// The remote's clock when it wrote the line (0 if absent).
    pub now: f64,
    pub sessions: Vec<Value>,
}

/// Ok(None) for lines that aren't ours — a remote shell rc file can print banners on stdout
/// before the binary starts, and those must not take the host offline.
pub fn parse_line(line: &str) -> Result<Option<Snapshot>, String> {
    let line = line.trim();
    if !line.starts_with('{') {
        return Ok(None);
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    let ver = v.get("v").and_then(Value::as_u64).unwrap_or(0);
    if ver != PROTOCOL {
        return Err(format!(
            "remote speaks protocol v{ver}, this widget v{PROTOCOL} — update the remote binary"
        ));
    }
    Ok(Some(Snapshot {
        host: str_of(&v, "host"),
        now: f64_of(&v, "now"),
        sessions: v
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_line_parses_back() {
        let line = snapshot_line("ln2", 123.0, &[json!({ "id": "a" })]);
        let snap = parse_line(&line).unwrap().unwrap();
        assert_eq!(snap.host, "ln2");
        assert_eq!(snap.now, 123.0);
        assert_eq!(snap.sessions.len(), 1);
    }

    #[test]
    fn banners_and_half_lines_are_ignored() {
        assert!(parse_line("Welcome to the login node").unwrap().is_none());
        assert!(parse_line("").unwrap().is_none());
        assert!(parse_line("{\"v\":1,\"sess").unwrap().is_none());
    }

    #[test]
    fn another_protocol_version_is_an_error() {
        assert!(parse_line("{\"v\":2,\"sessions\":[]}").is_err());
        assert!(parse_line("{\"sessions\":[]}").is_err()); // no version at all
    }

    #[test]
    fn a_snapshot_without_sessions_is_empty_not_an_error() {
        let snap = parse_line("{\"v\":1,\"host\":\"h\"}").unwrap().unwrap();
        assert!(snap.sessions.is_empty());
    }
}
