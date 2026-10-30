// Hide the console window in release so the recorder (called by hooks) never flashes a window and
// the GUI launches clean. Debug builds keep a console for logging.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Some helpers (label_for, set_label, etc.) are scaffolding for later phases (rename, mute UI).
#![allow(dead_code)]

// The `gui` feature (default) is the widget; `--no-default-features` builds a headless binary
// for remote hosts — recorder, installer and `stream` only, no webview/tray dependencies.
#[cfg(feature = "gui")]
mod gui;
mod install;
mod model;
mod paths;
mod platform;
mod recorder;
#[cfg(feature = "gui")]
mod remote;
#[cfg(feature = "gui")]
mod selfinstall;
mod stream;
mod styles;
#[cfg(all(windows, feature = "gui"))]
mod taskbar;
mod terminals;

use std::io::Write;

const USAGE: &str = "\
usage: claude-sessions [command]
  (none)              launch the widget (GUI builds only)
  record <state>      hook entry point: record a session state from stdin
  install|uninstall   wire / unwire the Claude Code hooks in ~/.claude/settings.json
  markers             add or upgrade the turn-marker block in ~/.claude/CLAUDE.md
  stream [interval]   print this machine's sessions as JSON lines (for a remote widget)
  install-app|uninstall-app, focus <id>, new-session [terminal], ai-label <id>";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // Called by Claude Code hooks — write the session state and exit (never launches the GUI).
        Some("record") => {
            let state = args.get(1).map(String::as_str).unwrap_or("working");
            recorder::record(state);
            std::process::exit(0);
        }
        // Spawned detached by the recorder (auto) or the dashboard's shift-click (--force) —
        // asks a small model to name a session, then exits.
        Some("ai-label") => {
            let sid = args.get(1).map(String::as_str).unwrap_or("");
            let force = args.get(2).map(String::as_str) == Some("--force");
            recorder::run_ai_label(sid, force);
            std::process::exit(0);
        }
        // CLI subcommands attach the launching console first so their output is visible in
        // release builds (windows subsystem detaches stdio).
        Some("install") => {
            platform::attach_parent_console();
            std::process::exit(install::run(true))
        }
        Some("uninstall") => {
            platform::attach_parent_console();
            std::process::exit(install::run(false))
        }
        // Copy the binary to a permanent location and register launch-at-login there, so
        // autostart survives moving/cleaning the build tree. `uninstall-app` reverses it.
        #[cfg(feature = "gui")]
        Some("install-app") => {
            platform::attach_parent_console();
            std::process::exit(selfinstall::install_app())
        }
        #[cfg(feature = "gui")]
        Some("uninstall-app") => {
            platform::attach_parent_console();
            std::process::exit(selfinstall::uninstall_app())
        }
        // Diagnostic: jump to a session (id prefix match) exactly like clicking its row.
        Some("focus") => {
            platform::attach_parent_console();
            let id = args.get(1).cloned().unwrap_or_default();
            match model::load().into_iter().find(|s| s.id.starts_with(&id)) {
                Some(s) => {
                    terminals::focus(&s);
                    std::process::exit(0)
                }
                None => {
                    eprintln!("no live session matching '{id}'");
                    std::process::exit(1)
                }
            }
        }
        // Diagnostic: open a session in the configured terminal, exactly like the `+` button.
        // Useful for checking a terminal adapter without the GUI in the way — notably whether
        // Terminal.app lands a tab or falls back to a window.
        Some("new-session") => {
            platform::attach_parent_console();
            let target = args.get(1).cloned().unwrap_or_else(|| {
                terminals::spawn_targets()
                    .first()
                    .map_or("", |(id, _)| id)
                    .into()
            });
            terminals::new_session(&target, &["echo diagnostic".into()]);
            std::process::exit(0)
        }
        // Add the optional ●/○/◐ turn-marker instruction to the global CLAUDE.md, or upgrade
        // an outdated block in place.
        Some("markers") => {
            platform::attach_parent_console();
            std::process::exit(if install::sync_claude_md_markers() {
                0
            } else {
                1
            })
        }
        // Remote side of multi-host support: the widget runs this over ssh (see remote.rs).
        Some("stream") => {
            platform::attach_parent_console();
            let interval = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
            std::process::exit(stream::run(interval))
        }
        // Without this, any unknown argument (`--help` included) launches the GUI and, on a
        // first run, wires the hooks — surprising for someone who only asked for help.
        Some("-h" | "--help" | "help") => {
            platform::attach_parent_console();
            // writeln, not println: `--help | head` closes the pipe early and println panics.
            let _ = writeln!(std::io::stdout(), "{USAGE}");
            std::process::exit(0)
        }
        // Default (double-click): launch the floating dashboard + tray.
        #[cfg(feature = "gui")]
        _ => {
            if let Err(e) = gui::run() {
                eprintln!("failed to start: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(not(feature = "gui"))]
        _ => {
            eprintln!("headless build (no widget)\n{USAGE}");
            std::process::exit(2)
        }
    }
}
