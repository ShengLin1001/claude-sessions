//! Terminal adapters: how the widget focuses, closes and spawns sessions in each kind of
//! terminal. One file per terminal; adding support for a new one is a new adapter file plus
//! a line in `registry()` — see docs/terminals.md. Callers never branch on terminal type:
//! they call the dispatchers below, which fall back generically when no adapter handles it.

use crate::model::Sess;

mod jetbrains;
#[cfg(target_os = "macos")]
mod macos_terminal;
// Pure WT tab-matching policy; used by wt.rs (Windows) but kept platform-independent so its
// unit tests run on every CI target.
mod tabmatch;
#[cfg(any(windows, target_os = "macos"))]
mod wezterm;
#[cfg(windows)]
mod wt;

/// Terminal.app can only open a tab by sending Cmd+T, which needs Accessibility permission;
/// the GUI polls this after a spawn to offer the explainer the first time it is refused.
#[cfg(target_os = "macos")]
pub use macos_terminal::take_accessibility_denied;

pub trait Terminal: Sync {
    /// Matches the `terminal` field the recorder writes ("wt", "jetbrains", ...).
    fn id(&self) -> &'static str;
    /// Human label for the options dropdown.
    fn label(&self) -> &'static str;
    /// Bring the session's tab/window to front. Return true when the request was handled
    /// (including a deliberate safe no-op); false → generic focus-window-by-pid fallback.
    fn focus(&self, s: &Sess) -> bool;
    /// Close the session's tab. true = handled; false → kill the session's process tree.
    fn close(&self, s: &Sess) -> bool;
    /// Open a new session running `cmds` (one shell command per entry). true = handled.
    fn new_session(&self, cmds: &[String]) -> bool;
}

fn registry() -> &'static [&'static dyn Terminal] {
    #[cfg(windows)]
    {
        &[&wt::Wt, &wezterm::WezTerm, &jetbrains::JetBrains]
    }
    // jetbrains first: it is the default "open new sessions in" target on mac.
    #[cfg(target_os = "macos")]
    {
        &[
            &jetbrains::JetBrains,
            &wezterm::WezTerm,
            &macos_terminal::MacTerminal,
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        &[&jetbrains::JetBrains]
    }
}

pub fn for_id(id: &str) -> Option<&'static dyn Terminal> {
    registry().iter().copied().find(|t| t.id() == id)
}

/// (id, label) pairs for the "open new sessions in" dropdown; first entry is the default.
pub fn spawn_targets() -> Vec<(&'static str, &'static str)> {
    registry().iter().map(|t| (t.id(), t.label())).collect()
}

pub fn focus(s: &Sess) {
    if for_id(&s.terminal).is_some_and(|t| t.focus(s)) {
        return;
    }
    fallback_focus(s);
}

/// A remote session (remote.rs) runs on another machine, but the user's own ssh login to it is
/// usually a local terminal tab carrying the session's title — switch to that tab if one
/// matches; never raise an arbitrary terminal window.
pub fn focus_remote(s: &Sess) {
    #[cfg(windows)]
    let _ = wt::focus_by_title(&s.tab_title, &s.topic);
    #[cfg(not(windows))]
    let _ = s;
}

pub fn close(s: &Sess) {
    if for_id(&s.terminal).is_some_and(|t| t.close(s)) {
        return;
    }
    fallback_close(s);
}

pub fn new_session(target: &str, cmds: &[String]) {
    if for_id(target).is_some_and(|t| t.new_session(cmds)) {
        return;
    }
    // Configured target unavailable — first adapter that will take it.
    for t in registry() {
        if t.id() != target && t.new_session(cmds) {
            return;
        }
    }
}

// ---- generic fallbacks for terminals without an adapter ("console", "vscode", "other") ----

#[allow(unused_variables)]
fn fallback_focus(s: &Sess) {
    #[cfg(windows)]
    {
        let p = if s.term_pid > 0 { s.term_pid } else { s.pid };
        if p > 0 {
            crate::platform::focus_window(crate::platform::main_window_for_pid(p));
        }
    }
}

#[allow(unused_variables)]
fn fallback_close(s: &Sess) {
    // Re-check identity right before the kill: the list is up to 1.5s old, and /T on a pid the OS
    // recycled in that window would take an unrelated process tree down with it.
    if s.pid <= 0 || !crate::model::pid_is_session(s.pid, s.updated) {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &s.pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW) // the widget is a GUI app; taskkill would flash a console
            .spawn();
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(s.pid as i32, libc::SIGTERM);
    }
}
