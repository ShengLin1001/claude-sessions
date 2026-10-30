//! The Claude Code hook recorder: reads hook JSON on stdin, writes the per-session state file.
//! Ported 1:1 from record.py and verified against it. Fail-safe: callers exit 0 regardless.

use crate::paths::*;
use crate::platform;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;

pub fn record(state: &str) {
    // Sessions spawned by the AI labeler (claude -p) inherit this marker via the hook
    // environment — recording them would show phantom sessions and recurse the labeler.
    if std::env::var("CLAUDE_SESSIONS_SUPPRESS").is_ok() {
        return;
    }
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let data: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    };

    let sid = str_of(&data, "session_id");
    if sid.is_empty() {
        return;
    }
    if std::fs::create_dir_all(state_dir()).is_err() {
        return;
    }
    let path = state_dir().join(format!("{sid}.json"));

    if state == "end" {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(ai_label_path(&sid));
        let _ = std::fs::remove_file(ai_pending_path(&sid));
        remember_cc_name(&sid, None);
        return;
    }

    let prev = load_json(&path);
    let (reg, reg_path) = registry_for(&sid);
    // A real /rename is a permanent choice — pin it now so no later auto/AI/pivot label can
    // take the name back if user_named() ever stops recognising it (CC re-derives nameSource,
    // or the applied-name echo collides). The pin outranks every other source everywhere.
    pin_user_rename(&sid, &reg);

    let cwd = first_nonempty(&[
        str_of(&reg, "cwd"),
        str_of(&data, "cwd"),
        str_of(&prev, "cwd"),
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ]);
    let self_ppid = platform::parent_pid(std::process::id() as i64);
    let mut pid = i64_of(&reg, "pid");
    if pid <= 0 {
        pid = self_ppid;
    }
    let transcript = str_of(&data, "transcript_path");

    let mut topic = str_of(&prev, "topic");
    let prompt = str_of(&data, "prompt");
    let has_prompt = !prompt.is_empty();
    let mut cmd_label = str_of(&prev, "cmd_label");
    if let Some((cmd, args)) = parse_command(&prompt) {
        // A later bare command (no args) must not steal the label from an earlier
        // argument-carrying one (e.g. /run-tests after /implement PROJ-x).
        if !args.is_empty() || cmd_label.is_empty() {
            cmd_label = command_topic(&cmd, &args);
        }
    }
    let ai_label = read_ai_label(&sid);
    // Prompts since the current AI label was minted — a session that has drifted far enough
    // from its label earns one fresh naming call (the pivot re-label).
    let mut since = i64_of(&prev, "prompts_since_label");
    if ai_label != str_of(&prev, "ai_label") {
        since = 0;
    }
    if state == "working" && has_prompt && wants_ai_label(&prompt) {
        since += 1;
    }
    let relabel_after = relabel_after();
    let pivoted = relabel_after > 0 && !ai_label.is_empty() && since >= relabel_after;
    if (ai_label.is_empty() || pivoted)
        && cmd_label.is_empty()
        && has_prompt
        && ai_labels_enabled()
        && custom_label(&sid).is_none()
        && !user_named(&reg, &sid)
        && wants_ai_label(&prompt)
    {
        spawn_ai_labeler(&sid, &prompt);
        since = 0;
    }
    if custom_label(&sid).is_some()
        || topic.is_empty()
        || matches!(state, "start" | "done")
        || (state == "working" && has_prompt)
        || ai_label != str_of(&prev, "ai_label")
    {
        let d = derive_label(&sid, &reg, &cwd, &transcript, &cmd_label, &ai_label);
        if !d.is_empty() {
            topic = d;
        }
    }
    // Only curated labels (manual > command > AI) are pushed into Claude Code — the blunt
    // fallbacks (branch, prompt cut) are no better than CC's own auto-names.
    let curated = custom_label(&sid).is_some_and(|c| c == topic)
        || (!cmd_label.is_empty() && cmd_label == topic)
        || (!ai_label.is_empty() && ai_label == topic);
    if curated {
        sync_cc_name(&sid, &reg, reg_path.as_deref(), &transcript, &topic);
    }

    let mut eff = if state == "start" {
        "idle".to_string()
    } else {
        state.to_string()
    };
    let mut msg = sanitize(&str_of(&data, "message"));

    if eff == "needs" && msg.to_lowercase().contains("waiting for") {
        return;
    }

    if state == "done" {
        let (verdict, snippet) = classify_stop(&data, &transcript);
        if verdict == "yourturn" {
            eff = "yourturn".to_string();
            if msg.is_empty() {
                msg = if snippet.is_empty() {
                    "your turn".into()
                } else {
                    snippet
                };
            }
        } else if verdict == "working" {
            eff = "working".to_string();
        }
    }

    let mut rec = Map::new();
    rec.insert("session_id".into(), json!(sid));
    rec.insert("state".into(), json!(eff));
    rec.insert("topic".into(), json!(topic));
    rec.insert("cmd_label".into(), json!(cmd_label));
    rec.insert("ai_label".into(), json!(ai_label));
    rec.insert("prompts_since_label".into(), json!(since));
    rec.insert("transcript".into(), json!(transcript));
    rec.insert("cwd".into(), json!(cwd));
    rec.insert("pid".into(), json!(pid));
    rec.insert("ppid".into(), json!(self_ppid));
    // Which machine owns `pid`: a home shared by several hosts (HPC login nodes) holds state
    // files whose pids mean nothing here, so model::load only liveness-checks its own.
    rec.insert("host".into(), json!(platform::hostname()));
    rec.insert("updated_at".into(), json!(unix_now()));
    platform::annotate(&mut rec, pid, &transcript, &topic);
    if (eff == "needs" || eff == "yourturn") && !msg.is_empty() {
        rec.insert("message".into(), json!(msg));
    }

    write_atomic(&path, &Value::Object(rec));
}

// ---- label derivation ----

const DEFAULT_BRANCHES: &[&str] = &["main", "master", "develop", "trunk"];
const META_TITLES: &[&str] = &[
    "dig deeper and follow up",
    "dig deeper",
    "follow up",
    "follow-up",
    "continue",
    "keep going",
    "next",
    "next steps",
    "more",
    "help",
    "untitled",
    "new chat",
    "wip",
    "test",
    "testing",
    "debugging",
    "conversation",
];
const TRIVIAL_PROMPTS: &[&str] = &[
    "yes",
    "no",
    "y",
    "n",
    "ok",
    "okay",
    "sure",
    "go",
    "do it",
    "continue",
    "proceed",
    "next",
    "commit",
    "push",
    "thanks",
    "ty",
    "yep",
    "yeah",
    "nope",
    "stop",
    "wait",
    "please",
    "done",
    "good",
    "perfect",
    "nice",
    "cool",
    "great",
    "fix it",
    "go on",
    "keep going",
    "carry on",
];

fn custom_label(sid: &str) -> Option<String> {
    let m = load_json(&labels_path());
    let v = m.get(sid)?.as_str()?.trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn is_substantial(txt: &str) -> bool {
    if txt.is_empty() || txt.starts_with('<') || txt.starts_with("Caveat:") {
        return false;
    }
    let s = txt.trim().to_lowercase();
    let s = s.trim_end_matches(['.', '!', '?']);
    s.chars().count() >= 6 && !TRIVIAL_PROMPTS.contains(&s)
}

pub fn short(txt: &str, n: usize) -> String {
    let txt = txt.trim();
    if txt.chars().count() <= n {
        return txt.to_string();
    }
    let cut: String = txt.chars().take(n).collect();
    let head = match cut.rsplit_once(' ') {
        Some((h, _)) if !h.is_empty() => h.to_string(),
        _ => cut,
    };
    format!("{head}…")
}

fn derive_label(
    sid: &str,
    reg: &Value,
    cwd: &str,
    transcript: &str,
    cmd_label: &str,
    ai_label: &str,
) -> String {
    if let Some(c) = custom_label(sid) {
        return c;
    }
    // Claude Code ≥2.1 auto-names every session ("fullstack-a4", nameSource "derived") —
    // worthless as a label. Only honour registry names the user set (e.g. /rename); a name
    // this recorder synced back into CC is an echo, not a user choice, and must not outrank
    // the label sources it mirrors.
    if user_named(reg, sid) {
        return str_of(reg, "name").trim().to_string();
    }
    if !cmd_label.is_empty() {
        return cmd_label.to_string();
    }
    if !ai_label.is_empty() {
        return ai_label.to_string();
    }
    let (title, latest) = transcript_titles(transcript);
    let br = git_branch(cwd);
    if !br.is_empty() && !DEFAULT_BRANCHES.contains(&br.to_lowercase().as_str()) {
        return short(&br, 44);
    }
    if !title.is_empty() && !META_TITLES.contains(&title.trim().to_lowercase().as_str()) {
        return title;
    }
    if !latest.is_empty() {
        return short(&latest, 44);
    }
    if !title.is_empty() {
        return title;
    }
    let base = Path::new(cwd.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base.is_empty() {
        cwd.to_string()
    } else {
        base
    }
}

// ---- Claude Code name sync ----
//
// Curated labels are pushed back into Claude Code itself so /resume and the session list
// show them too: the live-session registry name (what /rename sets) plus a custom-title
// transcript entry (what /resume reads for past sessions). Default on; "cc_name_sync": false
// in config.json disables it. Same never-clobber rule as the IDE TabNamer: only a derived
// auto-name or a name this recorder set previously is ever overwritten — a real /rename wins.

fn cc_sync_enabled() -> bool {
    load_json(&config_path())
        .get("cc_name_sync")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// True when the registry name is a genuine user choice (e.g. /rename) — not CC's derived
/// auto-name and not an echo this recorder synced back.
fn user_named(reg: &Value, sid: &str) -> bool {
    let name = str_of(reg, "name");
    !name.trim().is_empty()
        && str_of(reg, "nameSource") != "derived"
        && cc_applied_name(sid).as_deref() != Some(name.trim())
}

/// Snapshot a genuine /rename into the custom-pin store, making it permanent: the pin is the
/// top source in both derive_label and the widget, and it blocks the AI labeler. Only fires
/// while the registry name is still recognisably a user choice, and only when it differs from
/// the current pin, so it costs nothing on the steady state, a later /rename updates the pin,
/// and a shift-click forced relabel (which clears the pin and syncs its own name back) is not
/// re-pinned.
fn pin_user_rename(sid: &str, reg: &Value) {
    if !user_named(reg, sid) {
        return;
    }
    let name = str_of(reg, "name");
    let name = name.trim();
    if name.is_empty() || custom_label(sid).as_deref() == Some(name) {
        return;
    }
    let mut m = load_json(&labels_path());
    if !m.is_object() {
        m = Value::Object(Map::new());
    }
    if let Some(o) = m.as_object_mut() {
        o.insert(sid.to_string(), json!(name));
        write_atomic(&labels_path(), &m);
    }
}

/// Prompts after which a drifted AI-labeled session earns one fresh naming call. 0 disables
/// the pivot re-label; "ai_relabel_prompts" in config.json overrides the default of 10.
fn relabel_after() -> i64 {
    load_json(&config_path())
        .get("ai_relabel_prompts")
        .and_then(Value::as_i64)
        .unwrap_or(10)
}

fn cc_applied_path() -> std::path::PathBuf {
    base().join("cc-names-applied.json")
}

/// The name this recorder last synced into CC for [sid], if any.
fn cc_applied_name(sid: &str) -> Option<String> {
    let v = load_json(&cc_applied_path());
    let s = v.get(sid)?.as_str()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn remember_cc_name(sid: &str, name: Option<&str>) {
    let mut v = load_json(&cc_applied_path());
    if !v.is_object() {
        v = Value::Object(Map::new());
    }
    let Some(m) = v.as_object_mut() else { return };
    match name {
        Some(n) => {
            m.insert(sid.to_string(), json!(n));
        }
        None => {
            if m.remove(sid).is_none() {
                return; // nothing to persist
            }
        }
    }
    write_atomic(&cc_applied_path(), &v);
}

fn sync_cc_name(
    sid: &str,
    reg: &Value,
    reg_path: Option<&std::path::Path>,
    transcript: &str,
    topic: &str,
) {
    if topic.is_empty() || !cc_sync_enabled() {
        return;
    }
    let ours = cc_applied_name(sid);
    if ours.as_deref() == Some(topic) {
        return; // already synced
    }
    // A registry name we didn't write and CC didn't derive is a real /rename — hands off.
    if user_named(reg, sid) {
        return;
    }
    if let Some(p) = reg_path {
        let mut v = load_json(p);
        if let Some(m) = v.as_object_mut() {
            m.insert("name".into(), json!(topic));
            m.insert("nameSource".into(), json!("user"));
            write_atomic(p, &v);
        }
    }
    if !transcript.is_empty() {
        let p = expand_user(transcript);
        if p.is_file() {
            use std::io::Write;
            let line = json!({"type": "custom-title", "customTitle": topic, "sessionId": sid});
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&p) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
    remember_cc_name(sid, Some(topic));
}

// ---- AI labels (opt-in: "ai_labels": true in config.json) ----
//
// Freeform sessions get no deterministic label, so the recorder spawns a detached one-shot
// `claude -p --model haiku` call to name the session (once per session, never blocking the
// hook). The child writes ai-labels/<sid>.json; the next hook event picks it up as the topic.

fn ai_labels_enabled() -> bool {
    load_json(&config_path())
        .get("ai_labels")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn ai_label_path(sid: &str) -> std::path::PathBuf {
    ai_labels_dir().join(format!("{sid}.json"))
}

fn ai_pending_path(sid: &str) -> std::path::PathBuf {
    ai_labels_dir().join(format!("{sid}.pending"))
}

fn read_ai_label(sid: &str) -> String {
    str_of(&load_json(&ai_label_path(sid)), "label")
}

/// Only real freeform requests are worth a model call — slash commands have deterministic
/// labels and trivial/meta prompts would just name the session "ok".
fn wants_ai_label(prompt: &str) -> bool {
    let p = prompt.trim_start();
    !p.starts_with('/') && is_substantial(p)
}

/// One labeler at a time per session: claim the pending file, taking over a stale claim
/// (crashed labeler) after 3min.
fn claim_pending(sid: &str) -> bool {
    if std::fs::create_dir_all(ai_labels_dir()).is_err() {
        return false;
    }
    let pending = ai_pending_path(sid);
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)
        .is_ok()
    {
        return true;
    }
    let fresh = std::fs::metadata(&pending)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_secs() < 180)
        .unwrap_or(true);
    !fresh
}

/// Fire-and-forget: claim the pending file (the concurrency guard), then respawn ourselves as
/// `ai-label <sid>` fully detached so the hook returns immediately.
fn spawn_ai_labeler(sid: &str, prompt: &str) {
    if !claim_pending(sid) {
        return;
    }
    let pending = ai_pending_path(sid);
    let excerpt: String = prompt.chars().take(2000).collect();
    if std::fs::write(&pending, excerpt).is_err() {
        let _ = std::fs::remove_file(&pending);
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        let _ = std::fs::remove_file(&pending);
        return;
    };
    let _ = std::process::Command::new(exe)
        .args(["ai-label", sid])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The detached child: ask a small model for a label and store it. Runs outside any hook, so
/// latency is free; the marker env stops the nested claude's own hooks from recording it.
/// `force` is the on-demand path (shift-click in the dashboard): context comes from the
/// session's transcript instead of a pending prompt, and the result lands at custom (top)
/// priority and is pushed through every surface immediately.
pub fn run_ai_label(sid: &str, force: bool) {
    if sid.is_empty() {
        return;
    }
    let pending = ai_pending_path(sid);
    let excerpt = if force {
        if !claim_pending(sid) {
            return;
        }
        transcript_excerpt(sid)
    } else {
        let Ok(e) = std::fs::read_to_string(&pending) else {
            return;
        };
        e
    };
    if excerpt.trim().is_empty() {
        let _ = std::fs::remove_file(&pending);
        return;
    }
    let cfg = load_json(&config_path());
    let model = {
        let m = str_of(&cfg, "ai_label_model");
        if m.is_empty() {
            "haiku".to_string()
        } else {
            m
        }
    };
    let instruction = format!(
        "Compose a short label (maximum 20 characters) naming this coding session, based on \
         the request below. Keep identifiers exactly as written (ticket ids like PROJ-123, \
         PR #456) and put them first, followed by one or two action words. Reply with ONLY \
         the label — no quotes, no punctuation around it, nothing else.\n\nRequest:\n{excerpt}"
    );
    let mut cmd = std::process::Command::new("claude");
    cmd.args(["-p", &instruction, "--model", &model])
        .env("CLAUDE_SESSIONS_SUPPRESS", "1")
        .stdin(std::process::Stdio::null());
    let api_key = str_of(&cfg, "ai_label_api_key");
    if !api_key.is_empty() {
        cmd.env("ANTHROPIC_API_KEY", api_key);
    }
    if let Ok(out) = cmd.output() {
        let label = clean_ai_label(&String::from_utf8_lossy(&out.stdout));
        if !label.is_empty() {
            if force {
                apply_forced_label(sid, &label);
            } else {
                write_atomic(
                    &ai_label_path(sid),
                    &json!({"label": label, "ts": unix_now()}),
                );
            }
        }
    }
    let _ = std::fs::remove_file(&pending);
}

/// Last few substantial user messages from the session's transcript (path recorded in the
/// state file) — the naming context for an on-demand rename.
fn transcript_excerpt(sid: &str) -> String {
    let st = load_json(&state_dir().join(format!("{sid}.json")));
    let path = str_of(&st, "transcript");
    if path.is_empty() {
        return str_of(&st, "topic");
    }
    let mut msgs: Vec<String> = Vec::new();
    for obj in read_tail_entries(&path, 262_144) {
        if !obj.is_object() {
            continue;
        }
        let m = if obj.get("message").map(Value::is_object).unwrap_or(false) {
            obj.get("message").unwrap()
        } else {
            &obj
        };
        let role = {
            let r = str_of(m, "role");
            if r.is_empty() {
                str_of(&obj, "type")
            } else {
                r
            }
        };
        if role == "user" {
            let txt = extract_text(m.get("content")).trim().replace('\n', " ");
            if is_substantial(&txt) {
                msgs.push(txt);
            }
        }
    }
    // First message + last two, not just the tail: recency alone names the session after
    // its latest tangent (often the very message that triggered the rename), while the
    // opener anchors what the session is actually about.
    let picked: Vec<&String> = if msgs.len() <= 3 {
        msgs.iter().collect()
    } else {
        let mut v = vec![&msgs[0]];
        v.extend(&msgs[msgs.len() - 2..]);
        v
    };
    picked
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n")
        .chars()
        .take(2000)
        .collect()
}

/// Forced rename: stored as a regular AI label (NOT a custom pin — auto/pivot labeling may
/// still evolve it later), but pushed through every surface now — an idle session may not
/// fire another hook event for hours, so waiting for one would leave stale names.
fn apply_forced_label(sid: &str, label: &str) {
    write_atomic(
        &ai_label_path(sid),
        &json!({"label": label, "ts": unix_now()}),
    );
    // Drop any custom pin (an earlier alt-click rename, or a pre-0.15 forced rename) —
    // it outranks AI labels, and the user's latest explicit action should win.
    let mut pins = load_json(&labels_path());
    if let Some(o) = pins.as_object_mut() {
        if o.remove(sid).is_some() {
            write_atomic(&labels_path(), &pins);
        }
    }
    let sp = state_dir().join(format!("{sid}.json"));
    let mut st = load_json(&sp);
    let transcript = str_of(&st, "transcript");
    if st.is_object() {
        st["topic"] = json!(label);
        st["ai_label"] = json!(label);
        // A slash-command label outranks AI labels in derive_label — without clearing it,
        // the next hook event would revert the name the user just asked for.
        st["cmd_label"] = json!("");
        st["prompts_since_label"] = json!(0);
        write_atomic(&sp, &st);
    }
    let (reg, reg_path) = registry_for(sid);
    sync_cc_name(sid, &reg, reg_path.as_deref(), &transcript, label);
}

/// Model output → tab-safe label: first meaningful line, stripped of wrapping quotes/backticks
/// and of turn-marker lines (a global CLAUDE.md may make even `claude -p` emit ●/○/◐), then
/// word-boundary-capped to the tab budget.
fn clean_ai_label(raw: &str) -> String {
    let line = raw
        .lines()
        .map(|l| l.trim().trim_matches(['"', '\'', '`', '*']).trim())
        .map(|l| l.trim_matches(['●', '○', '◐']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let flat = line.split_whitespace().collect::<Vec<_>>().join(" ");
    short(&flat, TAB_LABEL_MAX)
}

// ---- slash-command labels ----

/// The tab-name budget TabNamer applies (word-boundary cut at 20). Command topics are
/// composed to fit it so the IDE tab shows them whole.
const TAB_LABEL_MAX: usize = 20;

/// Built-in CLI commands that would make meaningless labels.
const BUILTIN_COMMANDS: &[&str] = &[
    "clear", "compact", "config", "cost", "doctor", "exit", "fast", "help", "init", "login",
    "logout", "mcp", "memory", "model", "quit", "rename", "resume", "status",
];

/// Parse a slash-command prompt into (command, args). Handles both the raw form
/// ("/implement PROJ-18546") and the transcript XML form the hook may deliver
/// ("<command-name>/implement</command-name>…<command-args>PROJ-18546</command-args>").
fn parse_command(prompt: &str) -> Option<(String, String)> {
    let p = prompt.trim();
    if let Some(name) = between(p, "<command-name>", "</command-name>") {
        let args = between(p, "<command-args>", "</command-args>").unwrap_or_default();
        return normalize_command(&name, &args);
    }
    let rest = p.strip_prefix('/')?;
    let mut it = rest.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or("").to_string();
    let args = it.next().unwrap_or("").to_string();
    normalize_command(&name, &args)
}

fn between(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let end = start + s[start..].find(close)?;
    Some(s[start..end].trim().to_string())
}

fn normalize_command(name: &str, args: &str) -> Option<(String, String)> {
    let name = name.trim().trim_start_matches('/');
    // Plugin-scoped names ("acme-tools:release-prep") label by the skill part.
    let name = name.rsplit(':').next().unwrap_or(name).trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || BUILTIN_COMMANDS.contains(&name.to_lowercase().as_str())
    {
        return None;
    }
    Some((
        name.to_string(),
        args.split_whitespace().collect::<Vec<_>>().join(" "),
    ))
}

/// Compose "cmd args" into a label that fits [TAB_LABEL_MAX]. Args are compressed first
/// (PR URLs → "PR #n", flags dropped). If the natural form still overflows, identifiers
/// come first and the command name is trimmed to whatever fits ("run-team-test
/// PROJ-18033" → "PROJ-18033 team-test") — never a blind cut through an identifier.
fn command_topic(cmd: &str, args: &str) -> String {
    let toks = compress_args(args);
    let natural = if toks.is_empty() {
        cmd.to_string()
    } else {
        format!("{cmd} {}", toks.join(" "))
    };
    if natural.chars().count() <= TAB_LABEL_MAX {
        return natural;
    }
    let ids: Vec<&String> = toks.iter().filter(|t| is_identifier(t)).collect();
    if ids.is_empty() {
        return short(&natural, TAB_LABEL_MAX);
    }
    let mut base = String::new();
    for id in ids {
        let grown = if base.is_empty() {
            id.clone()
        } else {
            format!("{base} {id}")
        };
        if grown.chars().count() > TAB_LABEL_MAX {
            break;
        }
        base = grown;
    }
    if base.is_empty() {
        return short(&natural, TAB_LABEL_MAX);
    }
    let budget = TAB_LABEL_MAX.saturating_sub(base.chars().count() + 1);
    let action = fit_command(cmd, budget);
    if action.is_empty() {
        base
    } else {
        format!("{base} {action}")
    }
}

/// Longest tail of the hyphen-split command words that fits [budget]
/// ("prepare-release-notes", 13 → "release-notes"), falling back to initials ("PRN"), else "".
fn fit_command(cmd: &str, budget: usize) -> String {
    let words: Vec<&str> = cmd.split(['-', '_']).filter(|w| !w.is_empty()).collect();
    for i in 0..words.len() {
        let cand = words[i..].join("-");
        if cand.chars().count() <= budget {
            return cand;
        }
    }
    let initials: String = words
        .iter()
        .filter_map(|w| w.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if !initials.is_empty() && initials.chars().count() <= budget {
        return initials;
    }
    String::new()
}

/// Tokenize args for a label: GitHub PR/issue URLs and "pr #n" pairs become "PR #n",
/// ticket ids are uppercased, --flags and other URLs are dropped.
fn compress_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut toks = args.split_whitespace().peekable();
    while let Some(t) = toks.next() {
        if t.starts_with('-') && t.len() > 1 && !t[1..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Some(n) = url_number(t) {
            out.push(format!("PR #{n}"));
            continue;
        }
        if t.contains("://") {
            continue;
        }
        let low = t.to_lowercase();
        if low == "pr" || low == "pull" {
            if let Some(next) = toks.peek() {
                if let Some(n) = hash_number(next) {
                    toks.next();
                    out.push(format!("PR #{n}"));
                    continue;
                }
            }
        }
        if hash_number(t).is_some() || is_ticket(t) {
            out.push(t.to_uppercase());
            continue;
        }
        out.push(t.to_string());
    }
    out
}

fn is_identifier(t: &str) -> bool {
    t.starts_with("PR #") || hash_number(t).is_some() || is_ticket(t)
}

/// "PROJ-18546"-shaped ticket id: 2+ letters, a dash, digits.
fn is_ticket(t: &str) -> bool {
    let Some((alpha, num)) = t.split_once('-') else {
        return false;
    };
    alpha.chars().count() >= 2
        && alpha.chars().all(|c| c.is_ascii_alphabetic())
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

/// "#123" → Some("123").
fn hash_number(t: &str) -> Option<&str> {
    let n = t.strip_prefix('#')?;
    if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
        Some(n)
    } else {
        None
    }
}

/// Trailing number of a ".../pull/123" or ".../issues/123" URL.
fn url_number(t: &str) -> Option<String> {
    if !t.contains("://") {
        return None;
    }
    for marker in ["/pull/", "/issues/"] {
        if let Some(pos) = t.find(marker) {
            let digits: String = t[pos + marker.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
    }
    None
}

fn registry_for(sid: &str) -> (Value, Option<std::path::PathBuf>) {
    let mut best: Option<(Value, std::path::PathBuf)> = None;
    let mut best_updated = f64::MIN;
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return (Value::Null, None);
    };
    for e in entries.flatten() {
        if e.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if !v.is_object() || str_of(&v, "sessionId") != sid {
            continue;
        }
        let up = f64_of(&v, "updatedAt");
        if best.is_none() || up > best_updated {
            best_updated = up;
            best = Some((v, e.path()));
        }
    }
    match best {
        Some((v, p)) => (v, Some(p)),
        None => (Value::Null, None),
    }
}

fn transcript_titles(path: &str) -> (String, String) {
    let (mut title, mut latest) = (String::new(), String::new());
    for obj in read_all_entries(path) {
        if !obj.is_object() {
            continue;
        }
        match str_of(&obj, "type").as_str() {
            "ai-title" if !str_of(&obj, "aiTitle").is_empty() => {
                title = str_of(&obj, "aiTitle").trim().to_string()
            }
            "custom-title" if !str_of(&obj, "customTitle").is_empty() => {
                title = str_of(&obj, "customTitle").trim().to_string()
            }
            "user" => {
                let m = if obj.get("message").map(Value::is_object).unwrap_or(false) {
                    obj.get("message").unwrap()
                } else {
                    &obj
                };
                let txt = extract_text(m.get("content")).trim().replace('\n', " ");
                if is_substantial(&txt) {
                    latest = txt;
                }
            }
            _ => {}
        }
    }
    (title, latest)
}

fn git_branch(cwd: &str) -> String {
    if let Ok(o) = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
    {
        if o.status.success() {
            let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !b.is_empty() && b != "HEAD" {
                return b;
            }
        }
    }
    String::new()
}

pub fn transcript_title(path: &str) -> String {
    transcript_titles(path).0
}

// ---- turn classification ----

/// Classify a Stop from the reply Claude Code hands the hook (`last_assistant_message`), falling
/// back to the transcript on older versions. The transcript can lag the Stop hook by more than
/// the retry window — mostly on short text-only turns — so it is only the fallback.
fn classify_stop(data: &Value, transcript: &str) -> (String, String) {
    let reply = str_of(data, "last_assistant_message");
    if reply.trim().is_empty() {
        classify_turn(transcript)
    } else {
        classify_turn_text(&reply)
    }
}

fn classify_turn(transcript: &str) -> (String, String) {
    let mut text = String::new();
    for _ in 0..12 {
        text = current_turn_text(transcript);
        if !text.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if text.is_empty() {
        // The reply has not reached the transcript yet. Stay green and let the stale-working
        // reconcile read the finished transcript, rather than guessing done.
        return ("working".into(), String::new());
    }
    classify_turn_text(&text)
}

/// Decide Done vs Your-turn vs still-Working from the assistant's final message text, plus a
/// short snippet to show for your-turn. Looks at the last non-empty line: `○` (hollow) → your
/// turn (snippet = the line above it), `◐` (half) → working (the turn ended but background work
/// will wake the session), `●` (filled) → done, otherwise a trailing `?` is a weak your-turn
/// fallback, else done. `○` outranks `◐`: a question must never be hidden by a pending task.
fn classify_turn_text(text: &str) -> (String, String) {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let Some(last) = lines.last() else {
        return ("done".into(), String::new());
    };
    let last = last.trim();
    if last.contains('○') {
        let snippet = if lines.len() >= 2 {
            lines[lines.len() - 2].trim().to_string()
        } else {
            String::new()
        };
        return ("yourturn".into(), snippet);
    }
    if last.contains('◐') {
        return ("working".into(), String::new());
    }
    if last.contains('●') {
        return ("done".into(), String::new());
    }
    if last.trim_end().ends_with('?') {
        return ("yourturn".into(), last.to_string());
    }
    ("done".into(), String::new())
}

fn current_turn_text(transcript: &str) -> String {
    current_turn_text_entries(&read_tail_entries(transcript, 1_048_576))
}

/// The transcript entry's inner message object (unwrapping the `{message:{...}}` envelope) and its
/// role — falling back to the entry's `type` (system/attachment/...) when there is no chat role.
fn msg_and_role(obj: &Value) -> (&Value, String) {
    let m = if obj.get("message").map(Value::is_object).unwrap_or(false) {
        obj.get("message").unwrap()
    } else {
        obj
    };
    let r = str_of(m, "role");
    let role = if r.is_empty() { str_of(obj, "type") } else { r };
    (m, role)
}

fn content_has_block(m: &Value, kind: &str) -> bool {
    m.get("content")
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.iter().any(|b| str_of(b, "type") == kind))
}

/// The assistant text of the last completed turn (the message after the last user message), or
/// empty when the last thing in the transcript is an unanswered user prompt.
fn current_turn_text_entries(entries: &[Value]) -> String {
    let (mut last_user, mut a_idx) = (-1i64, -1i64);
    let mut a_text = String::new();
    for (i, obj) in entries.iter().enumerate() {
        if !obj.is_object() {
            continue;
        }
        let (m, role) = msg_and_role(obj);
        if role == "user" {
            last_user = i as i64;
        } else if role == "assistant" {
            let txt = extract_text(m.get("content"));
            if !txt.trim().is_empty() {
                a_idx = i as i64;
                a_text = txt;
            }
        }
    }
    if a_idx > last_user {
        a_text
    } else {
        String::new()
    }
}

/// Is the assistant's current turn still running? True when a tool call is in flight (the last
/// real message is an assistant `tool_use` with no result yet — a multi-minute build emits no hook
/// until it returns) or a `tool_result` Claude is about to act on. Meta rows (the Claude Code 2.1
/// away-summary/recap, attachments, file-history snapshots) are skipped — only real user/assistant
/// messages mark the turn boundary.
fn turn_in_progress(entries: &[Value]) -> bool {
    for obj in entries.iter().rev() {
        if !obj.is_object() {
            continue;
        }
        let (m, role) = msg_and_role(obj);
        match role.as_str() {
            "assistant" => return content_has_block(m, "tool_use"),
            "user" => return content_has_block(m, "tool_result"),
            _ => continue,
        }
    }
    false
}

/// Re-derive the true state of a session the state file still calls "working" but whose recorder
/// hooks have gone quiet. Claude Code fires PostToolUse on every tool call, so a stale "working"
/// means one of: a single long-running tool is in flight (still working, stays green); the turn
/// finished but no Stop hook classified it (trust the transcript's ●/○/◐ markers); or you typed a
/// follow-up and walked away — Claude Code 2.1's away-summary — leaving an unanswered prompt with
/// no Stop (idle, so a finished chat stops reading as active/green).
pub fn reconcile_stale_working(transcript: &str) -> String {
    let entries = read_tail_entries(transcript, 262_144);
    if turn_in_progress(&entries) {
        return "working".into();
    }
    let text = current_turn_text_entries(&entries);
    if text.trim().is_empty() {
        return "idle".into();
    }
    classify_turn_text(&text).0
}

fn read_all_entries(path: &str) -> Vec<Value> {
    if path.is_empty() {
        return vec![];
    }
    let Ok(bytes) = std::fs::read(expand_user(path)) else {
        return vec![];
    };
    parse_jsonl(&bytes)
}

fn read_tail_entries(path: &str, max_bytes: u64) -> Vec<Value> {
    if path.is_empty() {
        return vec![];
    }
    let p = expand_user(path);
    let Ok(meta) = std::fs::metadata(&p) else {
        return vec![];
    };
    let size = meta.len();
    let Ok(mut f) = std::fs::File::open(&p) else {
        return vec![];
    };
    use std::io::{Seek, SeekFrom};
    if size > max_bytes {
        let _ = f.seek(SeekFrom::Start(size - max_bytes));
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    if size > max_bytes {
        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            if nl + 1 < buf.len() {
                buf = buf[nl + 1..].to_vec();
            }
        }
    }
    parse_jsonl(&buf)
}

fn parse_jsonl(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for block in arr {
                if block.is_object() && str_of(block, "type") == "text" {
                    parts.push(str_of(block, "text"));
                } else if let Value::String(s) = block {
                    parts.push(s.clone());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

// ---- small io helpers ----

fn write_atomic(path: &Path, v: &Value) {
    let Some(dir) = path.parent() else { return };
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), unix_now() as u64));
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn sanitize(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    s.replace('"', "'")
        .replace(['\n', '\r'], " ")
        .trim()
        .to_string()
}

fn expand_user(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix('~') {
        home().join(rest.trim_start_matches(['/', '\\']))
    } else {
        std::path::PathBuf::from(p)
    }
}

fn first_nonempty(xs: &[String]) -> String {
    xs.iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_done_when_last_line_has_filled_circle() {
        assert_eq!(
            classify_turn_text("did the thing\n\n●"),
            ("done".into(), String::new())
        );
    }

    #[test]
    fn marker_yourturn_when_last_line_has_hollow_circle() {
        let (verdict, snippet) = classify_turn_text("Which database should I use?\n○");
        assert_eq!(verdict, "yourturn");
        assert_eq!(snippet, "Which database should I use?"); // snippet is the line above ○
    }

    #[test]
    fn hollow_circle_alone_gives_empty_snippet() {
        assert_eq!(classify_turn_text("○"), ("yourturn".into(), String::new()));
    }

    #[test]
    fn marker_working_when_last_line_has_half_circle() {
        // Turn ended but a background task/agent/monitor will wake the session — still working.
        assert_eq!(
            classify_turn_text("build kicked off, I'll report back\n\n◐"),
            ("working".into(), String::new())
        );
    }

    #[test]
    fn hollow_circle_outranks_half_circle() {
        // A turn that both asks and waits must keep the question visible.
        assert_eq!(
            classify_turn_text("pick a variant while the build runs\n○ ◐").0,
            "yourturn"
        );
    }

    #[test]
    fn half_circle_beats_a_dangling_earlier_question() {
        assert_eq!(
            classify_turn_text("Do you want X?\nWaiting on the deploy watcher.\n◐").0,
            "working"
        );
    }

    #[test]
    fn trailing_question_is_a_weak_yourturn() {
        let (verdict, snippet) = classify_turn_text("some reasoning\nShould I proceed?");
        assert_eq!(verdict, "yourturn");
        assert_eq!(snippet, "Should I proceed?");
    }

    #[test]
    fn plain_text_and_empty_are_done() {
        assert_eq!(classify_turn_text("all set, nothing needed").0, "done");
        assert_eq!(classify_turn_text("").0, "done");
        assert_eq!(classify_turn_text("   \n  \n").0, "done");
    }

    #[test]
    fn filled_circle_beats_a_dangling_earlier_question() {
        // A `?` earlier in the turn must not override a final ● line.
        assert_eq!(
            classify_turn_text("Do you want X?\nOkay, done.\n●").0,
            "done"
        );
    }

    fn user_prompt(text: &str) -> Value {
        json!({ "type": "user", "message": { "role": "user", "content": text } })
    }
    fn assistant_text(text: &str) -> Value {
        json!({ "type": "assistant", "message": { "role": "assistant",
            "content": [ { "type": "text", "text": text } ] } })
    }
    fn assistant_tool_use(id: &str) -> Value {
        json!({ "type": "assistant", "message": { "role": "assistant",
            "content": [ { "type": "tool_use", "id": id, "name": "Bash" } ] } })
    }
    fn tool_result(id: &str) -> Value {
        json!({ "type": "user", "message": { "role": "user",
            "content": [ { "type": "tool_result", "tool_use_id": id, "content": "ok" } ] } })
    }
    fn away_summary() -> Value {
        json!({ "type": "system", "subtype": "away_summary", "content": "recap...\n\n●" })
    }

    #[test]
    fn turn_in_progress_true_while_a_tool_runs() {
        // The assistant issued a tool_use and no result has come back — a long build is running.
        let entries = [user_prompt("build it"), assistant_tool_use("t1")];
        assert!(turn_in_progress(&entries));
    }

    #[test]
    fn turn_in_progress_true_right_after_a_tool_result() {
        // Claude has the tool output and is about to continue the same turn.
        let entries = [
            user_prompt("build it"),
            assistant_tool_use("t1"),
            tool_result("t1"),
        ];
        assert!(turn_in_progress(&entries));
    }

    #[test]
    fn turn_over_after_final_assistant_text() {
        let entries = [
            user_prompt("build it"),
            assistant_tool_use("t1"),
            tool_result("t1"),
            assistant_text("all done\n\n●"),
        ];
        assert!(!turn_in_progress(&entries));
    }

    #[test]
    fn turn_over_on_an_unanswered_human_prompt() {
        // The reported bug: a follow-up you walked away from, with no assistant turn after it.
        let entries = [
            assistant_text("previous turn\n\n●"),
            user_prompt("I got a mail the build failed"),
        ];
        assert!(!turn_in_progress(&entries));
    }

    #[test]
    fn meta_rows_do_not_end_the_turn() {
        // A trailing away-summary/recap (Claude Code 2.1) must not be read as the turn boundary.
        let entries = [
            user_prompt("do X"),
            assistant_tool_use("t1"),
            away_summary(),
        ];
        assert!(turn_in_progress(&entries));
    }

    #[test]
    fn reconcile_idle_for_a_walked_away_follow_up() {
        // Assistant finished (●, Stop wrote "done"), then a follow-up fired UserPromptSubmit
        // ("working") with no assistant turn / Stop after — the exact stuck-green case.
        let lines = [
            assistant_text("Both done, brother. ✅"),
            user_prompt("I got a mail the build failed"),
            away_summary(),
        ];
        let path = write_temp_jsonl("reconcile-walkaway", &lines);
        assert_eq!(reconcile_stale_working(&path), "idle");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reconcile_keeps_green_while_a_build_runs() {
        let lines = [user_prompt("cargo build"), assistant_tool_use("t1")];
        let path = write_temp_jsonl("reconcile-build", &lines);
        assert_eq!(reconcile_stale_working(&path), "working");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reconcile_trusts_a_stopless_but_finished_turn() {
        // No Stop hook fired, but the transcript shows the turn ended asking a question (○) —
        // surface it as your-turn rather than leaving it green.
        let lines = [
            user_prompt("which db?"),
            assistant_text("Postgres or SQLite?\n○"),
        ];
        let path = write_temp_jsonl("reconcile-yourturn", &lines);
        assert_eq!(reconcile_stale_working(&path), "yourturn");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stop_trusts_the_hook_reply_over_a_lagging_transcript() {
        // Arrange
        let lines = [user_prompt("flip it when the deploy lands")];
        let path = write_temp_jsonl("stop-lagging", &lines);
        let data = json!({"last_assistant_message": "Waiting on the deploy watcher.\n\n◐"});

        // Act
        let verdict = classify_stop(&data, &path);

        // Assert
        assert_eq!(verdict.0, "working");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stop_reads_the_transcript_when_the_hook_has_no_reply() {
        // Arrange
        let lines = [
            user_prompt("which db?"),
            assistant_text("Postgres or SQLite?\n○"),
        ];
        let path = write_temp_jsonl("stop-fallback", &lines);

        // Act
        let verdict = classify_stop(&json!({}), &path);

        // Assert
        assert_eq!(verdict, ("yourturn".into(), "Postgres or SQLite?".into()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stop_stays_working_when_the_reply_never_reaches_the_transcript() {
        // Arrange
        let lines = [user_prompt("flip it when the deploy lands")];
        let path = write_temp_jsonl("stop-missing", &lines);

        // Act
        let verdict = classify_stop(&json!({}), &path);

        // Assert
        assert_eq!(verdict.0, "working");
        let _ = std::fs::remove_file(&path);
    }

    fn write_temp_jsonl(tag: &str, entries: &[Value]) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("cs-test-{}-{}.jsonl", tag, std::process::id()));
        let body: String = entries
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn is_substantial_filters_trivial_and_short_prompts() {
        assert!(is_substantial("please refactor the auth module"));
        assert!(!is_substantial("ok")); // trivial + too short
        assert!(!is_substantial("yes")); // in TRIVIAL_PROMPTS
        assert!(!is_substantial("<command-name>")); // tool/meta noise
        assert!(!is_substantial(""));
    }

    #[test]
    fn parse_command_reads_raw_and_xml_forms() {
        assert_eq!(
            parse_command("/implement PROJ-18546"),
            Some(("implement".into(), "PROJ-18546".into()))
        );
        assert_eq!(
            parse_command(
                "<command-name>/acme-tools:release-prep</command-name>\
                 <command-message>release-prep</command-message>\
                 <command-args>PROJ-18546</command-args>"
            ),
            Some(("release-prep".into(), "PROJ-18546".into()))
        );
        assert_eq!(parse_command("address comments for pr #1234"), None); // not a command
        assert_eq!(parse_command("/model"), None); // built-in
        assert_eq!(parse_command("/ divided we fall"), None); // not a command name
    }

    #[test]
    fn command_topic_keeps_the_natural_form_when_it_fits() {
        assert_eq!(
            command_topic("implement", "PROJ-18546"),
            "implement PROJ-18546"
        ); // exactly 20
        assert_eq!(command_topic("standup-notes", ""), "standup-notes");
        assert_eq!(command_topic("review-prs", "5"), "review-prs 5");
    }

    #[test]
    fn command_topic_drops_flags_and_compresses_urls() {
        // dropping --direct brings it back under the cap
        assert_eq!(
            command_topic("implement", "PROJ-18546 --direct"),
            "implement PROJ-18546"
        );
        assert_eq!(
            command_topic(
                "review-pr",
                "https://github.com/octocat/hello-world/pull/1234"
            ),
            "review-pr PR #1234"
        );
    }

    #[test]
    fn command_topic_puts_identifiers_first_when_over_budget() {
        assert_eq!(
            command_topic("run-team-test", "PROJ-18033"),
            "PROJ-18033 team-test"
        );
        // no command tail fits the leftover budget → initials
        assert_eq!(
            command_topic("prepare-release-notes", "PROJ-1234567890"),
            "PROJ-1234567890 PRN"
        );
    }

    #[test]
    fn clean_ai_label_strips_wrapping_and_marker_noise() {
        assert_eq!(clean_ai_label("PR #923 address\n\n○\n"), "PR #923 address");
        assert_eq!(clean_ai_label("\"PROJ-123 hotfix\""), "PROJ-123 hotfix");
        assert_eq!(clean_ai_label("`fix login flow`  \n"), "fix login flow");
        assert_eq!(clean_ai_label("●"), "");
        assert_eq!(clean_ai_label("◐"), "");
        assert_eq!(clean_ai_label(""), "");
        // over-long output still lands under the tab cap
        assert!(
            clean_ai_label("a very long label the model ignored the cap on")
                .chars()
                .count()
                <= 20
        );
    }

    #[test]
    fn wants_ai_label_skips_commands_and_trivia() {
        assert!(wants_ai_label("hey address the review comments on pr #923"));
        assert!(!wants_ai_label("/implement PROJ-18546")); // deterministic label path
        assert!(!wants_ai_label("ok")); // trivial
        assert!(!wants_ai_label("<command-name>/foo</command-name>")); // meta noise
    }

    #[test]
    fn compress_args_normalizes_pr_references() {
        assert_eq!(compress_args("pr #1234"), vec!["PR #1234"]);
        assert_eq!(compress_args("proj-18546"), vec!["PROJ-18546"]);
        assert_eq!(
            compress_args("https://example.com/some/page"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn short_truncates_on_a_word_boundary_with_ellipsis() {
        assert_eq!(short("keep me", 20), "keep me"); // under the limit, untouched
        assert_eq!(short("one two three four five", 12), "one two…");
    }

    #[test]
    fn sanitize_flattens_quotes_and_newlines() {
        assert_eq!(
            sanitize("he said \"hi\"\nthen left"),
            "he said 'hi' then left"
        );
        assert_eq!(sanitize(""), "");
    }
}
