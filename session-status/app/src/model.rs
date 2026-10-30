//! Reads the shared runtime state (~/.claude/session-status/) into the session list the widget
//! draws. Mirrors StateReader.cs + the sort in MainWindow.Refresh.

use crate::paths::*;
use crate::platform;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct Sess {
    pub id: String,
    pub topic: String, // display name (custom rename > tab_title > topic)
    pub state: String, // needs | yourturn | working | idle | done | ...
    pub updated: f64,
    pub message: String,
    pub terminal: String,
    pub term_pid: i64,
    pub pid: i64,
    pub tab_title: String,
    pub tty: String, // mac only; the IntelliJ plugin matches focus requests by tty
    pub mute_until: f64,
    pub cwd: String,
    /// Machine the session runs on (the recorder's hostname); empty in older state files.
    pub host: String,
    /// "" for this machine's sessions, else the remotes.json name it was streamed from.
    pub origin: String,
}

impl Sess {
    pub fn muted(&self, now: f64) -> bool {
        self.mute_until > now
    }
}

/// How long a "working" session can go without a recorder hook before the widget stops trusting
/// the green and re-derives its state from the transcript. A genuinely busy Claude fires
/// PostToolUse on every tool call, so a gap this long means the turn has gone quiet; long-running
/// single tools (builds) are still kept green by the transcript check itself. Tunable.
const WORKING_STALE_SECS: f64 = 180.0;

pub fn now() -> f64 {
    unix_now()
}

/// Is [pid] still the Claude process this session recorded? Alive is not enough: pids get
/// recycled, so the process holding it now must also have started *before* the session last
/// wrote its state. Anything newer inherited the number from a session that is long over.
pub fn pid_is_session(pid: i64, updated: f64) -> bool {
    platform::is_alive(pid) && started_before(platform::process_start(pid), updated)
}

/// Split out so the rule is testable without live processes. Unknown values (0) fail open — an
/// unreadable start time or a state file without a timestamp must not hide a real session.
fn started_before(start: f64, updated: f64) -> bool {
    start <= 0.0 || updated <= 0.0 || start <= updated
}

/// A state file written on another host (shared home on HPC login nodes) can't be
/// liveness-checked from here — its pid belongs to that machine — so one this quiet is
/// treated as a leftover and skipped rather than shown as a ghost.
const FOREIGN_STALE_SECS: f64 = 24.0 * 3600.0;

/// Load all live sessions, sorted the way the widget shows them.
pub fn load() -> Vec<Sess> {
    load_with(true)
}

/// `focusable_only` hides sessions with no terminal to jump to (headless agents), which the
/// widget wants; `stream` passes false — a remote's sessions are never focusable from the
/// machine watching them anyway, and an ssh login often has no detectable terminal.
pub fn load_with(focusable_only: bool) -> Vec<Sess> {
    let n = now();
    let me = platform::hostname();
    let mutes = load_f64_map(&mutes_path());
    let labels = load_str_map(&labels_path());
    let mut out = Vec::new();

    let Ok(entries) = std::fs::read_dir(state_dir()) else {
        return out;
    };
    for e in entries.flatten() {
        if e.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(root) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
            continue; // partial write mid-refresh
        };

        let pid = i64_of(&root, "pid");
        let updated = f64_of(&root, "updated_at");
        let host = str_of(&root, "host");
        if !is_this_host(&host, &me) {
            if updated > 0.0 && n - updated > FOREIGN_STALE_SECS {
                continue; // not ours to delete: that host's own reader prunes it
            }
        } else if pid > 0 && !pid_is_session(pid, updated) {
            // The session's Claude process is gone. Delete the file rather than skip it: the OS
            // recycles pids (within days on Windows), so a state file left lying around comes
            // back to life the moment an unrelated process inherits its pid — an ancient session
            // reappearing in the list, popping the panel open, and offering to close a tab that
            // belongs to whatever now owns the number. SessionEnd normally removes the file, but
            // it never fires for a crashed or force-killed session.
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        let terminal = str_of(&root, "terminal");
        let term_pid = i64_of(&root, "term_pid");
        if focusable_only && terminal.is_empty() && term_pid == 0 {
            continue; // nothing focusable (headless / IDE) — hidden, matching the mac surface
        }

        let id = str_of(&root, "session_id");
        let tab_title = str_of(&root, "tab_title");
        let topic = str_of(&root, "topic");
        let display = labels
            .get(&id)
            .filter(|s| !s.is_empty())
            .cloned()
            .or_else(|| {
                if tab_title.is_empty() {
                    None
                } else {
                    Some(tab_title.clone())
                }
            })
            .unwrap_or_else(|| {
                if topic.is_empty() {
                    "?".into()
                } else {
                    topic.clone()
                }
            });

        out.push(Sess {
            id: id.clone(),
            topic: display,
            state: {
                let s = str_of(&root, "state");
                if s.is_empty() {
                    "?".into()
                } else if s == "working" && updated > 0.0 && n - updated > WORKING_STALE_SECS {
                    // "working" is only refreshed while Claude Code keeps firing hooks. Once one
                    // has gone quiet this long, re-derive the truth from the transcript so a
                    // finished-but-Stop-less chat (e.g. a follow-up you walked away from) stops
                    // showing green. See recorder::reconcile_stale_working.
                    crate::recorder::reconcile_stale_working(&str_of(&root, "transcript"))
                } else {
                    s
                }
            },
            updated,
            message: str_of(&root, "message"),
            terminal,
            term_pid,
            pid,
            tab_title,
            tty: str_of(&root, "tty"),
            mute_until: mutes.get(&id).copied().unwrap_or(0.0),
            cwd: str_of(&root, "cwd"),
            host,
            origin: String::new(),
        });
    }

    sort(&mut out, n);
    out
}

/// Files from recorders predating the `host` field, or a machine whose name can't be read,
/// count as this host — exactly the behaviour before hosts were tracked.
fn is_this_host(host: &str, me: &str) -> bool {
    host.is_empty() || me.is_empty() || host.eq_ignore_ascii_case(me)
}

/// Muted last, then needs > your-turn > working > the rest, newest first within a state.
pub fn sort(out: &mut [Sess], n: f64) {
    out.sort_by(|a, b| {
        let (am, bm) = (a.muted(n), b.muted(n));
        if am != bm {
            return if am {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }
        let (oa, ob) = (state_order(&a.state), state_order(&b.state));
        if oa != ob {
            return oa.cmp(&ob);
        }
        b.updated
            .partial_cmp(&a.updated)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

// ---- remote wire format (one `stream` line carries a list of these) ----

/// The per-session fields a remote host's `stream` sends — what its widget section draws.
pub fn to_wire(s: &Sess) -> Value {
    json!({
        "id": s.id,
        "topic": s.topic,
        "state": s.state,
        "updated": s.updated,
        "message": s.message,
        "cwd": s.cwd,
        "host": s.host,
        "tab_title": s.tab_title,
    })
}

/// Parse one streamed session. `clock_offset` (local minus remote clock) moves its timestamp
/// onto this machine's clock, so ages don't drift by however far the two clocks disagree.
pub fn from_wire(v: &Value, origin: &str, clock_offset: f64) -> Option<Sess> {
    let id = str_of(v, "id");
    if id.is_empty() {
        return None;
    }
    let updated = f64_of(v, "updated");
    let state = str_of(v, "state");
    Some(Sess {
        id,
        topic: str_of(v, "topic"),
        state: if state.is_empty() { "?".into() } else { state },
        updated: if updated > 0.0 { updated + clock_offset } else { 0.0 },
        message: str_of(v, "message"),
        cwd: str_of(v, "cwd"),
        host: str_of(v, "host"),
        tab_title: str_of(v, "tab_title"),
        origin: origin.into(),
        ..Default::default()
    })
}

/// Local renames and mutes also apply to remote sessions — they are keyed by session id and
/// stored on this machine, the one whose widget the user is clicking.
pub fn apply_overlays(list: &mut [Sess]) {
    let mutes = load_f64_map(&mutes_path());
    let labels = load_str_map(&labels_path());
    for s in list {
        if let Some(l) = labels.get(&s.id).filter(|l| !l.is_empty()) {
            s.topic = l.clone();
        }
        s.mute_until = mutes.get(&s.id).copied().unwrap_or(0.0);
    }
}

/// Last path component of a cwd from either OS ("D:\a\b\" → "b", "/home/u/x" → "x"); a bare
/// root ("/", "C:\") is returned as-is so the column never goes blank.
pub fn dir_name(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(last) if !last.is_empty() && !last.ends_with(':') => last.to_string(),
        _ => cwd.to_string(),
    }
}

pub fn state_order(state: &str) -> i32 {
    match state {
        "needs" => 0,
        "yourturn" => 1,
        "working" => 2,
        "idle" => 3,
        _ => 4,
    }
}

/// Age string like the mac/Windows surfaces: "12s", "3m", "2h". Empty if no timestamp.
pub fn age_str(ts: f64) -> String {
    if ts <= 0.0 {
        return String::new();
    }
    let s = (now() - ts) as i64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

// ---- mutations (mute / rename), used by the widget ----

pub fn toggle_mute(id: &str) {
    if id.is_empty() {
        return;
    }
    let n = now();
    let mut m = load_f64_map(&mutes_path());
    m.retain(|_, &mut v| v > n); // drop expired
    match m.get(id) {
        Some(&until) if until > n => {
            m.remove(id);
        }
        _ => {
            m.insert(id.to_string(), n + 3600.0); // snooze 1h
        }
    }
    save_map(&mutes_path(), &m);
}

pub fn set_label(id: &str, name: &str) {
    if id.is_empty() {
        return;
    }
    let mut m = load_str_map(&labels_path());
    let n = name.trim();
    if n.is_empty() {
        m.remove(id);
    } else {
        m.insert(id.to_string(), n.to_string());
    }
    save_map(&labels_path(), &m);
}

// ---- small map io ----

fn load_f64_map(path: &std::path::Path) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    if let Value::Object(m) = load_json(path) {
        for (k, v) in m {
            if let Some(n) = v.as_f64() {
                out.insert(k, n);
            }
        }
    }
    out
}

fn load_str_map(path: &std::path::Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Value::Object(m) = load_json(path) {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                out.insert(k, s.to_string());
            }
        }
    }
    out
}

fn save_map<T: serde::Serialize>(path: &std::path::Path, m: &HashMap<String, T>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(m) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_older_than_the_state_file_is_the_session() {
        assert!(started_before(1_700_000_000.0, 1_700_000_600.0));
    }

    #[test]
    fn a_process_started_after_the_last_write_is_a_recycled_pid() {
        // The pid of a session last seen a week ago, now held by a browser started minutes ago.
        assert!(!started_before(1_700_600_000.0, 1_700_000_000.0));
    }

    #[test]
    fn unreadable_start_or_timestamp_fails_open() {
        assert!(started_before(0.0, 1_700_000_000.0));
        assert!(started_before(1_700_000_000.0, 0.0));
    }

    #[test]
    fn unknown_or_matching_host_is_this_host() {
        assert!(is_this_host("", "ln2"));
        assert!(is_this_host("ln2", ""));
        assert!(is_this_host("LN2", "ln2"));
        assert!(!is_this_host("ln1", "ln2"));
    }

    #[test]
    fn dir_name_takes_the_last_component_on_either_os() {
        assert_eq!(dir_name(r"F:\BaiduSyncdisk\main_code_space\other"), "other");
        assert_eq!(dir_name(r"F:\a\机器学习势函数\"), "机器学习势函数");
        assert_eq!(dir_name("/home/u/work/mlp-train/"), "mlp-train");
        assert_eq!(dir_name("/"), "/");
        assert_eq!(dir_name(r"C:\"), r"C:\");
        assert_eq!(dir_name(""), "");
    }

    #[test]
    fn wire_round_trip_keeps_fields_and_shifts_the_clock() {
        let s = Sess {
            id: "abc".into(),
            topic: "train".into(),
            state: "needs".into(),
            updated: 1_000.0,
            message: "Claude needs your permission".into(),
            cwd: "/home/u/x".into(),
            host: "ln2".into(),
            tab_title: "train".into(),
            pid: 42, // local-only fields don't travel
            ..Default::default()
        };
        let back = from_wire(&to_wire(&s), "zcm6", 5.0).unwrap();
        assert_eq!(back.id, "abc");
        assert_eq!(back.state, "needs");
        assert_eq!(back.updated, 1_005.0);
        assert_eq!(back.cwd, "/home/u/x");
        assert_eq!(back.host, "ln2");
        assert_eq!(back.origin, "zcm6");
        assert_eq!(back.pid, 0);
    }

    #[test]
    fn wire_without_id_or_timestamp_is_handled() {
        assert!(from_wire(&json!({ "state": "working" }), "r", 0.0).is_none());
        let s = from_wire(&json!({ "id": "x" }), "r", 9.0).unwrap();
        assert_eq!(s.updated, 0.0); // no timestamp stays "no age", not 9s
        assert_eq!(s.state, "?");
    }
}
