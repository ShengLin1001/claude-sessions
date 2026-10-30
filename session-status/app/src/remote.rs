//! Remote hosts: run each configured command (normally `ssh <host> claude-sessions stream`),
//! read its JSON lines (stream.rs) and keep the latest snapshot per host for the heartbeat to
//! show below the local sessions.
//!
//! Config, ~/.claude/session-status/remotes.json — read once at startup:
//!   {"remotes": [{"name": "zcm6",
//!                 "cmd": ["ssh", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR",
//!                         "zcm6", "/path/to/claude-sessions", "stream"]}]}
//! `cmd` is any argv whose stdout speaks the stream protocol — ssh, a jump host, `wsl.exe -- …`.
//! Names key the sections and the sessions' `origin`, so keep them unique.
//!
//! Nothing outlives the widget: when it exits, the pipe to ssh closes, ssh dies on its next
//! write (a heartbeat at most HEARTBEAT_SECS away) and the remote `stream` then dies on EPIPE.

use crate::paths::{load_json, remotes_path, str_of};
use crate::{model, stream};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

/// No line for this long means the link is dead even if ssh hasn't noticed (a resumed laptop,
/// a half-open TCP connection). Three missed heartbeats.
const SILENCE_SECS: u64 = 3 * stream::HEARTBEAT_SECS;
const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 60;

struct Remote {
    name: String,
    cmd: Vec<String>,
}

#[derive(Clone, Default)]
struct HostState {
    name: String,
    /// "connecting" until the first snapshot, then "online" / "offline".
    status: &'static str,
    /// Local unix time of the last snapshot (0 = never).
    last_seen: f64,
    /// Why the host is offline: the last stderr line, a timeout or a protocol mismatch.
    error: String,
    /// Clock-corrected, without local renames/mutes (applied on read, so they show at once).
    sessions: Vec<model::Sess>,
}

static HOSTS: Mutex<Vec<HostState>> = Mutex::new(Vec::new());

pub fn start() {
    let remotes = load_config();
    *HOSTS.lock().unwrap() = remotes
        .iter()
        .map(|r| HostState {
            name: r.name.clone(),
            status: "connecting",
            ..Default::default()
        })
        .collect();
    for (i, r) in remotes.into_iter().enumerate() {
        std::thread::spawn(move || watch(i, r));
    }
}

fn load_config() -> Vec<Remote> {
    let root = load_json(&remotes_path());
    let Some(list) = root.get("remotes").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter().filter_map(parse_remote).collect()
}

fn parse_remote(v: &Value) -> Option<Remote> {
    let name = str_of(v, "name");
    let cmd: Vec<String> = v
        .get("cmd")?
        .as_array()?
        .iter()
        .filter_map(|c| c.as_str().map(String::from))
        .collect();
    (!name.is_empty() && !cmd.is_empty()).then_some(Remote { name, cmd })
}

/// Sessions of every online host, in config order, each host's list sorted like the local one.
/// Offline hosts contribute nothing: their last snapshot can't be trusted as live.
pub fn sessions() -> Vec<model::Sess> {
    let hosts = HOSTS.lock().unwrap().clone();
    let n = model::now();
    let mut out = Vec::new();
    for h in hosts.into_iter().filter(|h| h.status == "online") {
        let mut list = h.sessions;
        model::apply_overlays(&mut list);
        model::sort(&mut list, n);
        out.extend(list);
    }
    out
}

/// Per-host status for the widget's section headers, in config order.
pub fn hosts_json() -> Vec<Value> {
    HOSTS
        .lock()
        .unwrap()
        .iter()
        .map(|h| {
            json!({
                "name": h.name,
                "status": h.status,
                "last_seen": h.last_seen,
                "error": h.error,
            })
        })
        .collect()
}

fn update(i: usize, f: impl FnOnce(&mut HostState)) {
    if let Some(h) = HOSTS.lock().unwrap().get_mut(i) {
        f(h);
    }
}

/// Reconnect forever. A link that delivered anything retries soon; one that keeps failing
/// backs off so an unreachable host doesn't spawn ssh every few seconds.
fn watch(i: usize, r: Remote) {
    let mut backoff = BACKOFF_MIN_SECS;
    loop {
        let worked = run_once(i, &r);
        update(i, |h| {
            h.status = "offline";
            h.sessions.clear();
        });
        backoff = next_backoff(backoff, worked);
        std::thread::sleep(Duration::from_secs(backoff));
    }
}

fn next_backoff(prev: u64, worked: bool) -> u64 {
    if worked {
        BACKOFF_MIN_SECS
    } else {
        (prev * 2).clamp(BACKOFF_MIN_SECS, BACKOFF_MAX_SECS)
    }
}

/// One connection: spawn, read lines until EOF, silence or a protocol mismatch.
/// True if at least one snapshot arrived.
fn run_once(i: usize, r: &Remote) -> bool {
    let mut cmd = Command::new(&r.cmd[0]);
    cmd.args(&r.cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW); // ssh.exe is a console app; the widget is not
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            update(i, |h| h.error = format!("{}: {e}", r.cmd[0]));
            return false;
        }
    };

    let (tx, rx) = mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    update(i, |h| h.error = line);
                }
            }
        });
    }

    let mut worked = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(SILENCE_SECS)) {
            Ok(line) => match stream::parse_line(&line) {
                Ok(Some(snap)) => {
                    worked = true;
                    apply(i, &r.name, snap);
                }
                Ok(None) => {} // banner / noise from the remote shell
                Err(e) => {
                    update(i, |h| h.error = e);
                    break;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                update(i, |h| h.error = format!("no data for {SILENCE_SECS}s"));
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // the command exited
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    worked
}

fn apply(i: usize, origin: &str, snap: stream::Snapshot) {
    let now = model::now();
    let offset = clock_offset(now, snap.now);
    let list: Vec<model::Sess> = snap
        .sessions
        .iter()
        .filter_map(|v| model::from_wire(v, origin, offset))
        .collect();
    update(i, |h| {
        h.status = "online";
        h.last_seen = now;
        h.error.clear();
        h.sessions = list;
    });
}

/// Local minus remote clock, measured on a line that was written moments ago (the stream is
/// live, so transit time is noise next to the clock skew it corrects).
fn clock_offset(local_now: f64, remote_now: f64) -> f64 {
    if remote_now > 0.0 {
        local_now - remote_now
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_need_a_name_and_a_command() {
        assert!(parse_remote(&json!({ "name": "zcm6", "cmd": ["ssh", "zcm6"] })).is_some());
        assert!(parse_remote(&json!({ "name": "zcm6", "cmd": [] })).is_none());
        assert!(parse_remote(&json!({ "name": "", "cmd": ["ssh"] })).is_none());
        assert!(parse_remote(&json!({ "name": "x", "cmd": "ssh x" })).is_none()); // argv, not a string
    }

    #[test]
    fn backoff_doubles_to_a_cap_and_resets_after_a_working_link() {
        assert_eq!(next_backoff(BACKOFF_MIN_SECS, false), 10);
        assert_eq!(next_backoff(40, false), BACKOFF_MAX_SECS);
        assert_eq!(next_backoff(BACKOFF_MAX_SECS, false), BACKOFF_MAX_SECS);
        assert_eq!(next_backoff(BACKOFF_MAX_SECS, true), BACKOFF_MIN_SECS);
    }

    #[test]
    fn clock_offset_ignores_a_missing_remote_clock() {
        assert_eq!(clock_offset(100.0, 90.0), 10.0);
        assert_eq!(clock_offset(100.0, 0.0), 0.0);
    }
}
