//! The always-on-top floating dashboard (Tauri v2 port of floatdash.swift / MainWindow.cs).
//! The window chrome (transparent + undecorated + CSS drop shadow) and all drawing live in
//! ui/index.html; this module is the shell: window placement, tray, hook auto-install, the
//! 1.5s heartbeat that pushes sessions to the frontend, and the commands the frontend invokes.

use crate::{install, model, styles, terminals};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WindowEvent};

pub fn run() -> tauri::Result<()> {
    // First-run: wire the Claude Code hooks so simply launching the exe is enough.
    if !install::already_installed() {
        let _ = install::run(true);
    }

    // A static 300px panel doesn't need GPU compositing; software raster renders it identically
    // and drops WebView2's private memory by ~100MB (the GPU process). Overridable via the env.
    if std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_err() {
        std::env::set_var(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            "--disable-gpu --disable-gpu-compositing",
        );
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            load_sessions,
            jump,
            close_session,
            new_session,
            toggle_mute,
            rename,
            ai_rename,
            menu_action,
            get_config,
            set_config,
            set_collapsed
        ])
        .setup(|app| {
            // Menu-bar-only app: no Dock icon, no app switcher entry.
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                prevent_app_nap();
            }
            setup_tray(app.handle())?;
            #[cfg(windows)]
            for win in app.webview_windows().values() {
                watch_webview_crashes(win);
            }
            register_hotkeys(app.handle());
            offer_marker_upgrade(app.handle());
            crate::remote::start();
            #[cfg(windows)]
            {
                // Deferred: the strip's click arrives inside a window procedure mid-dispatch.
                let h = app.handle().clone();
                crate::taskbar::on_click(move || {
                    let h2 = h.clone();
                    let _ = h.run_on_main_thread(move || toggle_window(&h2));
                });
            }
            // Auto-hide is a level check exactly once, at startup: launching at login with
            // nothing running should go straight to the tray rather than flash the panel.
            // Folded into the taskbar, the strip (up on the first heartbeat) stands in for it.
            let startup = Pulse::of(&model::load());
            if let Some(win) = app.get_webview_window("main") {
                if (auto_hide() && startup.live == 0) || strip_wanted() {
                    restore_position(&win); // shown later, by auto-show or the tray
                } else {
                    show_remembered(&win);
                }
            }
            // Heartbeat: reload sessions, refresh the tray badge, push to the frontend.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut prev = startup;
                let mut tray_cache = None;
                let mut last_emitted: Option<Value> = None;
                loop {
                    let sessions = all_sessions();
                    update_tray(&handle, &sessions, &mut tray_cache);
                    let pulse = Pulse::of(&sessions);
                    apply_auto_visibility(&handle, &prev, &pulse);
                    prev = pulse;
                    // Emit only when the payload changed AND the panel can see it — every
                    // emit costs a WKWebView wake + full DOM rebuild on the JS side. A tick
                    // skipped while hidden self-heals: last_emitted stays stale, so the
                    // first visible tick after a change emits.
                    let visible = handle
                        .get_webview_window("main")
                        .and_then(|w| w.is_visible().ok())
                        .unwrap_or(true);
                    let payload = panel_payload(&sessions);
                    if visible && last_emitted.as_ref() != Some(&payload) {
                        let _ = handle.emit("sessions", payload.clone());
                        last_emitted = Some(payload);
                    }
                    // Persist the panel's spot and recover it when a display sleeps/disconnects.
                    // Monitor + window-geometry queries must run on the main thread.
                    #[cfg(windows)]
                    let (strip, counts) = (strip_wanted(), chip_counts(&sessions));
                    let h = handle.clone();
                    let _ = handle.run_on_main_thread(move || {
                        if let Some(win) = h.get_webview_window("main") {
                            window_upkeep(&win);
                        }
                        #[cfg(windows)]
                        crate::taskbar::sync(strip, counts);
                    });
                    std::thread::sleep(Duration::from_millis(1500));
                }
            });
            Ok(())
        })
        // Closing (Alt-F4 etc.) hides to the tray instead of quitting.
        .on_window_event(|win, ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                let _ = win.hide();
            }
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

/// Opt this process out of macOS App Nap. Without it, once another app takes focus the OS throttles
/// our run loop and compositor, so the dashboard's hover glow and live updates crawl while it sits
/// in the background — staying responsive there is the whole point of the widget. The
/// AllowingIdleSystemSleep variant exempts only this process; the Mac still sleeps normally when
/// idle. The activity token must outlive the process, so it's leaked deliberately.
/// Opt this process out of macOS App Nap. Without it, once another app takes focus the OS throttles
/// our run loop and compositor, so the dashboard's hover glow and live updates crawl while it sits
/// in the background — staying responsive there is the whole point of the widget. The
/// AllowingIdleSystemSleep variant exempts only this process; the Mac still sleeps normally when
/// idle. The activity token must outlive the process, so it's leaked deliberately.
#[cfg(target_os = "macos")]
fn prevent_app_nap() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let reason = NSString::from_str("Live Claude session status");
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &reason,
    );
    std::mem::forget(token);
}

/// Pin the panel to the monitor's top-right. The html has a 14px shadow gutter, so a 6px
/// outer margin puts the visible panel 20px from the edges, like the other surfaces.
fn position_top_right(win: &tauri::WebviewWindow) {
    let (Ok(Some(mon)), Ok(size)) = (win.current_monitor(), win.outer_size()) else {
        return;
    };
    let x = mon.position().x + mon.size().width as i32
        - size.width as i32
        - (6.0 * mon.scale_factor()) as i32;
    let y = mon.position().y + (26.0 * mon.scale_factor()) as i32;
    place(win, tauri::PhysicalPosition::new(x, y));
}

// ---- remembered window position ----
//
// The panel is draggable (native drag regions), so re-pinning it to the top-right on every show
// threw away wherever the user had put it. We restore the last spot they left it and only fall
// back to the corner when there is none, or when it would land off-screen.
//
// Persistence and display-change recovery both live in `window_upkeep`, run once per heartbeat on
// the main thread. Doing it there — rather than off a `Moved` event — is what makes it robust to a
// display sleeping: on the tick where the monitor layout changes we snap back and refuse to save,
// so macOS shoving the window onto another screen never overwrites the remembered spot.

fn place(win: &tauri::WebviewWindow, pos: tauri::PhysicalPosition<i32>) {
    let _ = win.set_position(pos);
}

/// Last observed monitor layout, so a display sleeping/disconnecting can be detected.
static LAST_MONITORS: Mutex<Option<String>> = Mutex::new(None);
/// Suppress position saves for a moment after a layout change, so a brief disconnect (a display
/// sleeping and waking) doesn't overwrite the remembered spot with the fallback we snapped to.
static SAVE_SUPPRESSED_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn monitor_fingerprint(win: &tauri::WebviewWindow) -> String {
    let Ok(monitors) = win.available_monitors() else {
        return String::new();
    };
    let mut parts: Vec<String> = monitors
        .iter()
        .map(|m| {
            let (p, s) = (m.position(), m.size());
            format!("{},{},{},{}", p.x, p.y, s.width, s.height)
        })
        .collect();
    parts.sort();
    parts.join(";")
}

/// One heartbeat's worth of window bookkeeping, on the main thread: remember where the user has
/// dragged the panel, and recover its spot when a display sleeps or disconnects under it.
fn window_upkeep(win: &tauri::WebviewWindow) {
    let fp = monitor_fingerprint(win);
    let mut last = LAST_MONITORS.lock().unwrap();
    let changed = last.as_ref().is_some_and(|prev| prev != &fp);
    let first = last.is_none();
    *last = Some(fp);
    drop(last);

    #[cfg(windows)]
    if changed || transparency_due() {
        rearm_transparency(win.app_handle());
    }

    // Layout changed: macOS has likely relocated the window. Snap back to the remembered spot
    // (or the corner if that monitor is gone) and don't trust positions for a few seconds.
    if changed {
        *SAVE_SUPPRESSED_UNTIL.lock().unwrap() =
            Some(std::time::Instant::now() + Duration::from_secs(6));
        if win.is_visible().unwrap_or(false) {
            restore_position(win);
        }
        return;
    }
    if first || !win.is_visible().unwrap_or(false) {
        return;
    }
    if SAVE_SUPPRESSED_UNTIL
        .lock()
        .unwrap()
        .is_some_and(|t| std::time::Instant::now() < t)
    {
        return;
    }
    // Stable layout: persist the current spot if the user has moved it somewhere on a monitor.
    if let Ok(pos) = win.outer_position() {
        if on_some_monitor(win, pos.x, pos.y) && saved_position() != Some((pos.x, pos.y)) {
            write_config(|root| root["window_pos"] = json!({ "x": pos.x, "y": pos.y }));
        }
    }
}

/// Wall-clock time of the last heartbeat, and heartbeats since the last re-arm.
#[cfg(windows)]
static REARM_CLOCK: Mutex<(Option<std::time::SystemTime>, u32)> = Mutex::new((None, 0));

/// Whether to re-apply transparency this heartbeat: after a wall-clock gap (the machine slept —
/// SystemTime, since Instant may not advance across sleep) or every ~30s as a catch-all for a
/// DWM restart nothing else reveals (a display waking without a layout change).
#[cfg(windows)]
fn transparency_due() -> bool {
    let now = std::time::SystemTime::now();
    let mut clock = REARM_CLOCK.lock().unwrap();
    let slept = clock.0.is_some_and(|prev| {
        now.duration_since(prev)
            .is_ok_and(|gap| gap > Duration::from_secs(10))
    });
    clock.0 = Some(now);
    clock.1 += 1;
    if slept || clock.1 >= 20 {
        clock.1 = 0;
        return true;
    }
    false
}

/// DWM drops a window's blur-behind state when composition restarts (sleep/resume, a display
/// waking) and tao never re-applies it (its own FIXME on WM_DWMCOMPOSITIONCHANGED), so the
/// transparent gutter around the panel turns solid black until relaunch.
#[cfg(windows)]
fn rearm_transparency(app: &tauri::AppHandle) {
    for win in app.webview_windows().values() {
        if let Ok(h) = win.hwnd() {
            crate::platform::enable_transparency(h.0);
        }
    }
}

// ---- WebView2 crash recovery ----
//
// When WebView2's browser process dies, the native windows outlive it as blank transparent
// layers: the panel and tray menu vanish but still swallow clicks where they sit, and nothing
// ever repaints them. WebView2 only reports this through ProcessFailed and leaves recovery to
// the host, so we relaunch ourselves — a fresh process gets a fresh browser.

/// Set in the relaunched child, holding the unix time of the crash restart that spawned it.
#[cfg(windows)]
const CRASH_RESTART_ENV: &str = "CLAUDE_SESSIONS_CRASH_RESTART";

/// A second crash this soon after a crash restart means WebView2 itself is broken (runtime
/// mid-update, corrupt profile); relaunching again would just spin.
#[cfg_attr(not(windows), allow(dead_code))]
const CRASH_RESTART_COOLDOWN_SECS: u64 = 60;

#[cfg_attr(not(windows), allow(dead_code))]
fn may_crash_restart(last_restart: Option<u64>, now: u64) -> bool {
    last_restart.is_none_or(|t| now.saturating_sub(t) >= CRASH_RESTART_COOLDOWN_SECS)
}

#[cfg(windows)]
fn watch_webview_crashes(win: &tauri::WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::ProcessFailedEventHandler;

    let app = win.app_handle().clone();
    let _ = win.with_webview(move |wv| unsafe {
        let Ok(core) = wv.controller().CoreWebView2() else {
            return;
        };
        let handler = ProcessFailedEventHandler::create(Box::new(move |webview, args| {
            let Some(args) = args else { return Ok(()) };
            let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
            args.ProcessFailedKind(&mut kind)?;
            eprintln!("claude-sessions: WebView2 process failed (kind {})", kind.0);
            if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED {
                recover_from_dead_browser(&app);
            } else if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
                || kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE
            {
                // The browser survived, so a reload brings the page back in place.
                if let Some(webview) = webview {
                    webview.Reload()?;
                }
            }
            // GPU/utility/frame failures are restarted by WebView2 itself.
            Ok(())
        }));
        let mut token = 0i64;
        let _ = core.add_ProcessFailed(&handler, &mut token);
    });
}

#[cfg(windows)]
fn recover_from_dead_browser(app: &tauri::AppHandle) {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Both webviews share the one browser process, so its death fires once per window.
    static RECOVERING: AtomicBool = AtomicBool::new(false);
    if RECOVERING.swap(true, Ordering::SeqCst) {
        return;
    }
    let now = model::now() as u64;
    let last = std::env::var(CRASH_RESTART_ENV)
        .ok()
        .and_then(|v| v.parse().ok());
    if may_crash_restart(last, now) {
        std::env::set_var(CRASH_RESTART_ENV, now.to_string());
        app.restart();
    }
    // Exiting still beats leaving dead windows that eat clicks.
    eprintln!("claude-sessions: WebView2 died again right after a restart; exiting");
    app.exit(1);
}

/// Show the panel where the user left it. Placed on both sides of `show`: macOS quietly drops a
/// `set_position` on a window it has not realised yet, so the placement that actually sticks is
/// the one after — the earlier call just keeps other platforms from flashing the old spot first.
fn show_remembered(win: &tauri::WebviewWindow) {
    restore_position(win);
    let _ = win.show();
    restore_position(win);
}

/// Restore the remembered position, or pin to the top-right when there isn't a usable one.
fn restore_position(win: &tauri::WebviewWindow) {
    match saved_position().filter(|&(x, y)| on_some_monitor(win, x, y)) {
        Some((x, y)) => place(win, tauri::PhysicalPosition::new(x, y)),
        None => position_top_right(win),
    }
}

fn saved_position() -> Option<(i32, i32)> {
    let cfg = crate::paths::load_json(&crate::paths::config_path());
    let pos = cfg.get("window_pos")?;
    Some((
        pos.get("x")?.as_i64()? as i32,
        pos.get("y")?.as_i64()? as i32,
    ))
}

/// Whether the window's top-left still lands on a connected monitor. Checked against the title
/// strip only: a position saved on a monitor that is now gone must not strand the panel where it
/// cannot be dragged back.
fn on_some_monitor(win: &tauri::WebviewWindow, x: i32, y: i32) -> bool {
    let Ok(monitors) = win.available_monitors() else {
        return false;
    };
    let rects: Vec<_> = monitors
        .iter()
        .map(|m| {
            let (p, s) = (m.position(), m.size());
            (p.x, p.y, s.width as i32, s.height as i32)
        })
        .collect();
    point_on_any(&rects, x, y)
}

/// Pure half of `on_some_monitor` (monitor rects as x/y/w/h), so the bounds policy is testable
/// without a display attached.
fn point_on_any(rects: &[(i32, i32, i32, i32)], x: i32, y: i32) -> bool {
    rects
        .iter()
        .any(|&(mx, my, mw, mh)| x >= mx && y >= my && x < mx + mw && y < my + mh)
}

// ---- auto-hide / auto-show ----

/// What counts as a "live" session for the tray badge and the auto-hide/auto-show toggles: a
/// session still in play. A finished one (idle/done) shouldn't hold the dashboard open or pop
/// it back up.
const LIVE_STATES: [&str; 3] = ["needs", "yourturn", "working"];

/// The slice of a heartbeat that auto-hide/auto-show react to.
struct Pulse {
    live: usize,
    /// Ids of the sessions currently asking for you, so a *new* one can be told from a standing one.
    needs: std::collections::HashSet<String>,
}

impl Pulse {
    fn of(sessions: &[model::Sess]) -> Self {
        let now = model::now();
        let live: Vec<&model::Sess> = sessions
            .iter()
            .filter(|s| !s.muted(now) && LIVE_STATES.contains(&s.state.as_str()))
            .collect();
        Self {
            live: live.len(),
            needs: live
                .iter()
                .filter(|s| s.state == "needs")
                .map(|s| s.id.clone())
                .collect(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Auto {
    Show,
    Hide,
    Nothing,
}

/// Both toggles are edge-triggered — they act on the heartbeat where something *changed*, never
/// on the standing level. That's what keeps them from fighting the user: hide the panel by hand
/// with sessions running and it stays hidden, because no transition has happened yet.
fn auto_action(prev: &Pulse, now: &Pulse, hide: bool, show: bool) -> Auto {
    // Show wins over hide: on a heartbeat where the last session goes quiet as another one
    // starts asking for you, surfacing it is the useful answer.
    if show {
        let appeared = prev.live == 0 && now.live > 0;
        let newly_needs = now.needs.difference(&prev.needs).next().is_some();
        if appeared || newly_needs {
            return Auto::Show;
        }
    }
    if hide && prev.live > 0 && now.live == 0 {
        return Auto::Hide;
    }
    Auto::Nothing
}

fn apply_auto_visibility(app: &tauri::AppHandle, prev: &Pulse, now: &Pulse) {
    // Folded into the taskbar, the strip's red dot is the alert; popping the panel would undo the fold.
    if strip_wanted() {
        return;
    }
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    match auto_action(prev, now, auto_hide(), auto_show()) {
        Auto::Show => {
            // No set_focus: this is a passive panel reacting to a background event, and stealing
            // focus mid-keystroke would be worse than not showing at all.
            show_remembered(&win);
        }
        Auto::Hide => {
            let _ = win.hide();
        }
        Auto::Nothing => {}
    }
}

fn toggle_window(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    } else {
        show_remembered(&win);
        let _ = win.set_focus();
    }
}

// ---- tray ----

/// Left-click jumps to the topmost (highest-priority) session, like the WPF tray did; the
/// dashboard toggle lives in the menu. Right-click opens our own webview-rendered menu (the
/// native Win32 tray menu can't be styled — same reason the WPF build had WpfTrayMenu.cs).
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id("main")
        .tooltip("Claude sessions")
        .icon(badge_icon("idle", 0))
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button,
                button_state: MouseButtonState::Up,
                position,
                ..
            } = ev
            {
                match button {
                    MouseButton::Left => jump_to_top(tray.app_handle()),
                    MouseButton::Right => open_tray_menu(tray.app_handle(), position.x, position.y),
                    _ => {}
                }
            }
        })
        .build(app)?;
    Ok(())
}

// ---- global hotkeys (the RegisterEventHotKey pair from menubar.swift, now cross-platform) ----

/// (Re)register both global hotkeys from config. Called at startup and after the options pane
/// saves. Unregisters everything first so a rebind doesn't leave the old combo live. A combo
/// that fails to parse or is already taken by another app is logged and skipped — the widget
/// still runs, that shortcut just stays unbound until it's rebound.
fn register_hotkeys(app: &tauri::AppHandle) {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    let jump = hotkey_jump();
    if !jump.is_empty() {
        if let Err(e) = gs.on_shortcut(jump.as_str(), |app, _sc, ev| {
            if ev.state() == ShortcutState::Pressed {
                jump_to_top(app);
            }
        }) {
            eprintln!("claude-sessions: could not bind jump hotkey {jump:?}: {e}");
        }
    }

    let new = hotkey_new();
    if !new.is_empty() {
        if let Err(e) = gs.on_shortcut(new.as_str(), |app, _sc, ev| {
            if ev.state() == ShortcutState::Pressed {
                spawn_new_session(app);
            }
        }) {
            eprintln!("claude-sessions: could not bind new-session hotkey {new:?}: {e}");
        }
    }
}

/// Jump to the first unmuted session — model::load() already sorts needs > your-turn > working,
/// newest first. With nothing to jump to, fall back to toggling the dashboard.
fn jump_to_top(app: &tauri::AppHandle) {
    let now = model::now();
    let mut sessions = all_sessions();
    model::sort(&mut sessions, now); // one priority order across local and remote sections
    match sessions.iter().find(|s| !s.muted(now)) {
        Some(s) => {
            let s = s.clone();
            std::thread::spawn(move || focus_any(&s));
        }
        None => toggle_window(app),
    }
}

/// Startup offer, once per launch: the marker block in CLAUDE.md was written by an older
/// version of the convention (e.g. before the ◐ background-wait glyph), so the states it
/// produces no longer match what this build's recorder reads. Only fires for users who already
/// opted in — a *missing* block stays opt-in via the tray menu, exactly as before.
fn offer_marker_upgrade(app: &tauri::AppHandle) {
    if install::marker_status() != install::MarkerStatus::Outdated {
        return;
    }
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    app.dialog()
        .message(
            "This version changed the turn-marker instruction Claude follows:\n\
             ◐ now marks a turn that ended while waiting on background work (builds, agents, \
             watchers), so those sessions stay green instead of turning yellow.\n\n\
             Your ~/.claude/CLAUDE.md still has the old block. Replace it now?\n\
             (A timestamped backup is saved next to it.)",
        )
        .title("claude-sessions — turn markers changed")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Update CLAUDE.md".into(),
            "Later".into(),
        ))
        .show(|update| {
            if update {
                let _ = install::sync_claude_md_markers();
            }
        });
}

/// Push the current state to menu.html; it sizes itself, pops up at the cursor and shows.
fn open_tray_menu(app: &tauri::AppHandle, x: f64, y: f64) {
    let monitor = app.monitor_from_point(x, y).ok().flatten().map(|m| {
        json!({
            "x": m.position().x, "y": m.position().y,
            "w": m.size().width, "h": m.size().height,
        })
    });
    let markers = install::marker_status();
    let _ = app.emit_to(
        "menu",
        "menu-open",
        json!({
            "x": x,
            "y": y,
            "monitor": monitor,
            "sessions": to_json(&all_sessions()),
            "markers_missing": markers == install::MarkerStatus::Missing,
            "markers_outdated": markers == install::MarkerStatus::Outdated,
            "theme": theme(),
        }),
    );
}

#[tauri::command]
fn menu_action(app: tauri::AppHandle, id: String) {
    match id.as_str() {
        "toggle" => toggle_window(&app),
        "markers" => {
            let _ = install::sync_claude_md_markers();
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

fn update_tray(
    app: &tauri::AppHandle,
    sessions: &[model::Sess],
    cache: &mut Option<(usize, usize, usize)>,
) {
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let now = model::now();
    let active: Vec<&model::Sess> = sessions.iter().filter(|s| s.mute_until <= now).collect();
    let count = |st: &str| active.iter().filter(|s| s.state == st).count();
    let (needs, yt, wk) = (count("needs"), count("yourturn"), count("working"));
    // Icon + tooltip derive entirely from these three counts — skip the 1024-px badge
    // render and the two tray syscalls when they haven't moved since last tick.
    if *cache == Some((needs, yt, wk)) {
        return;
    }
    *cache = Some((needs, yt, wk));
    let (top, n) = if needs > 0 {
        ("needs", needs)
    } else if yt > 0 {
        ("yourturn", yt)
    } else if wk > 0 {
        ("working", wk)
    } else {
        ("idle", 0)
    };
    let _ = tray.set_icon(Some(badge_icon(top, n)));

    let mut parts = Vec::new();
    if needs > 0 {
        parts.push(format!("{needs} need you"));
    }
    if yt > 0 {
        parts.push(format!("{yt} your turn"));
    }
    if wk > 0 {
        parts.push(format!("{wk} working"));
    }
    let tip = if parts.is_empty() {
        "Claude sessions — idle".into()
    } else {
        format!("Claude — {}", parts.join(", "))
    };
    let _ = tray.set_tooltip(Some(tip));
}

/// A 32×32 RGBA badge: a filled disc in the state colour (small + dim when nothing is active).
fn badge_icon(state: &str, count: usize) -> tauri::image::Image<'static> {
    const W: u32 = 32;
    let (r, g, b) = styles::rgb_for(state);
    let alpha_scale = if count == 0 { 0.6 } else { 1.0 };
    let rad = if count == 0 { 6.0f32 } else { 13.0 };
    let mut rgba = vec![0u8; (W * W * 4) as usize];
    for y in 0..W {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
            let d = (dx * dx + dy * dy).sqrt();
            let cover = (rad + 1.0 - d).clamp(0.0, 1.0);
            let i = ((y * W + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = (255.0 * cover * alpha_scale) as u8;
        }
    }
    tauri::image::Image::new_owned(rgba, W, W)
}

// ---- commands ----

fn to_json(sessions: &[model::Sess]) -> Vec<Value> {
    sessions
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "topic": s.topic,
                "state": s.state,
                "updated": s.updated,
                "message": s.message,
                "terminal": s.terminal,
                "term_pid": s.term_pid,
                "pid": s.pid,
                "tab_title": s.tab_title,
                "mute_until": s.mute_until,
                "cwd": s.cwd,
                "dir": model::dir_name(&s.cwd),
                "host": s.host,
                "origin": s.origin,
            })
        })
        .collect()
}

/// Local sessions first, then each online remote host's, in remotes.json order.
fn all_sessions() -> Vec<model::Sess> {
    let mut all = model::load();
    all.extend(crate::remote::sessions());
    all
}

/// What the panel draws: every session plus each remote host's status for its section header.
fn panel_payload(sessions: &[model::Sess]) -> Value {
    json!({ "sessions": to_json(sessions), "hosts": crate::remote::hosts_json() })
}

#[tauri::command]
fn load_sessions() -> Value {
    panel_payload(&all_sessions())
}

/// A remote session's terminal is on another machine; the best we can do here is the local
/// terminal tab the user's own ssh login runs in, matched by title.
fn focus_any(s: &model::Sess) {
    if s.origin.is_empty() {
        terminals::focus(s);
    } else {
        terminals::focus_remote(s);
    }
}

// The jump/close/new-session actions can sleep between focus and keystrokes, so they run on
// their own threads rather than blocking a command handler. They take the session id and
// re-load state, so the frontend never carries terminal internals.

fn with_session(id: String, f: impl Fn(&model::Sess) + Send + 'static) {
    std::thread::spawn(move || {
        if let Some(s) = all_sessions().into_iter().find(|s| s.id == id) {
            f(&s);
        }
    });
}

#[tauri::command]
fn jump(id: String) {
    with_session(id, focus_any);
}

/// Local only: closing falls back to killing the session's process tree by pid, and a remote
/// session's pid means nothing on this machine.
#[tauri::command]
fn close_session(id: String) {
    with_session(id, |s| {
        if s.origin.is_empty() {
            terminals::close(s);
        }
    });
}

#[tauri::command]
fn new_session(app: tauri::AppHandle) {
    spawn_new_session(&app);
}

/// Open a new Claude session in the configured terminal. Shared by the `+` button command and
/// the new-session global hotkey.
fn spawn_new_session(app: &tauri::AppHandle) {
    let (target, cmds) = (new_session_terminal(), new_session_cmds());
    let app = app.clone();
    std::thread::spawn(move || {
        terminals::new_session(&target, &cmds);
        #[cfg(target_os = "macos")]
        if terminals::take_accessibility_denied() {
            offer_accessibility(&app);
        }
        let _ = &app;
    });
}

/// Terminal.app has no scriptable "new tab", so we click the menu item for it — which macOS
/// refuses until the app is trusted for Accessibility. Explained once, then never again: the
/// fallback (a new window) is what the widget always did, so this is an offer, not an error.
#[cfg(target_os = "macos")]
fn offer_accessibility(app: &tauri::AppHandle) {
    if config_bool("accessibility_asked", false) {
        return;
    }
    write_config(|root| root["accessibility_asked"] = json!(true));

    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    app.dialog()
        .message(concat!(
            "Terminal.app offers no way to script a new tab, so Claude Sessions opens one from ",
            "its Shell menu for you \u{2014} which macOS only allows for apps trusted for ",
            "Accessibility.\n\nUntil then, new sessions open in a new window instead.\n\n",
            "To switch to tabs: open Privacy & Security \u{203a} Accessibility, add Claude ",
            "Sessions, then quit and reopen it.",
        ))
        .title("claude-sessions \u{2014} open new sessions in a tab?")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Open Accessibility settings".into(),
            "Keep using windows".into(),
        ))
        .show(|open| {
            if open {
                let _ = std::process::Command::new("open")
                    .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                    .spawn();
            }
        });
}

// ---- options (config.json in ~/.claude/session-status/) ----

fn config_str(key: &str, default: &str) -> String {
    crate::paths::load_json(&crate::paths::config_path())
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(default)
        .to_string()
}

/// Read-modify-write config.json, creating it (and its directory) when missing.
fn write_config(edit: impl FnOnce(&mut Value)) {
    let path = crate::paths::config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut root = crate::paths::load_json(&path);
    if !root.is_object() {
        root = json!({});
    }
    edit(&mut root);
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&root).unwrap_or_default(),
    );
}

fn config_bool(key: &str, default: bool) -> bool {
    crate::paths::load_json(&crate::paths::config_path())
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

// Independent by design (either, both or neither). Auto-hide defaults off — a dashboard that
// disappears on its own is a surprise you have to learn. Auto-show defaults on — surfacing a
// session that wants you is the widget's whole job.
fn auto_hide() -> bool {
    config_bool("auto_hide", false)
}

fn auto_show() -> bool {
    config_bool("auto_show", true)
}

fn new_session_cmd_raw() -> String {
    config_str("new_session_cmd", "claude")
}

/// Dashboard + tray-menu theme: "system" (follow the OS), "dark" or "light".
fn theme() -> String {
    config_str("theme", "system")
}

/// "compact" (the original 328px panel) or "wide" (adds the working-directory column).
fn panel_size() -> String {
    config_str("panel_size", "compact")
}

/// What the fold button does: "panel" shrinks to the header and counts, "taskbar" (Windows)
/// swaps the panel for a strip of counts on the taskbar.
fn collapse_mode() -> String {
    config_str("collapse_mode", "panel")
}

/// The taskbar strip stands in for the panel while it's folded in taskbar mode.
fn strip_wanted() -> bool {
    cfg!(windows) && config_bool("collapsed", false) && collapse_mode() == "taskbar"
}

/// The footer chips' counts for the strip — muted sessions included, like the panel's.
#[cfg(windows)]
fn chip_counts(sessions: &[model::Sess]) -> [usize; 4] {
    crate::taskbar::STATES.map(|st| sessions.iter().filter(|s| s.state == st).count())
}

/// Bring the strip in line with config now rather than on the next heartbeat.
fn sync_strip(app: &tauri::AppHandle) {
    #[cfg(windows)]
    {
        let (strip, counts) = (strip_wanted(), chip_counts(&all_sessions()));
        let _ = app.run_on_main_thread(move || crate::taskbar::sync(strip, counts));
    }
    let _ = app;
}

/// The fold button. Folding into the taskbar hides the panel; clicking the strip brings it back.
#[tauri::command]
fn set_collapsed(app: tauri::AppHandle, collapsed: bool) {
    write_config(|root| root["collapsed"] = json!(collapsed));
    sync_strip(&app);
    if strip_wanted() {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.hide();
        }
    }
}

// Global-hotkey defaults mirror menubar.swift's ⌃⌥⌘J / ⌃⌥⌘N — CmdOrCtrl maps to ⌘ on mac and
// Ctrl on Windows/Linux. Accelerator strings are what tauri-plugin-global-shortcut parses.
const DEFAULT_HOTKEY_JUMP: &str = "CmdOrCtrl+Alt+J";
const DEFAULT_HOTKEY_NEW: &str = "CmdOrCtrl+Alt+N";

fn hotkey_jump() -> String {
    config_str("hotkey_jump", DEFAULT_HOTKEY_JUMP)
}

fn hotkey_new() -> String {
    config_str("hotkey_new", DEFAULT_HOTKEY_NEW)
}

/// Which terminal `+` opens new sessions in; defaults to the platform's first adapter.
fn new_session_terminal() -> String {
    let default = terminals::spawn_targets().first().map_or("", |(id, _)| id);
    config_str("new_session_terminal", default)
}

/// The configured launch command(s), one per line — typed into the new tab in order.
fn new_session_cmds() -> Vec<String> {
    new_session_cmd_raw()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

#[tauri::command]
fn get_config(app: tauri::AppHandle) -> Value {
    use tauri_plugin_autostart::ManagerExt;
    let targets: Vec<Value> = terminals::spawn_targets()
        .into_iter()
        .map(|(id, label)| json!({ "id": id, "label": label }))
        .collect();
    json!({
        "new_session_cmd": new_session_cmd_raw(),
        "new_session_terminal": new_session_terminal(),
        "theme": theme(),
        "panel_size": panel_size(),
        "collapse_mode": collapse_mode(),
        "collapsed": config_bool("collapsed", false),
        "taskbar_strip": cfg!(windows),
        "hotkey_jump": hotkey_jump(),
        "hotkey_new": hotkey_new(),
        "auto_hide": auto_hide(),
        "auto_show": auto_show(),
        // OS state (HKCU Run / login item), not stored in config.json.
        "autostart": app.autolaunch().is_enabled().unwrap_or(false),
        "terminals": targets,
    })
}

/// What the options pane saves. One struct rather than a parameter per field, so adding an
/// option stays a one-line change here and in the pane.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    new_session_cmd: String,
    new_session_terminal: String,
    theme: String,
    panel_size: String,
    collapse_mode: String,
    hotkey_jump: String,
    hotkey_new: String,
    auto_hide: bool,
    auto_show: bool,
    autostart: bool,
}

#[tauri::command]
fn set_config(app: tauri::AppHandle, settings: Settings) {
    use tauri_plugin_autostart::ManagerExt;
    write_config(|root| {
        root["new_session_cmd"] = json!(settings.new_session_cmd.trim());
        root["new_session_terminal"] = json!(settings.new_session_terminal.trim());
        root["theme"] = json!(settings.theme.trim());
        root["panel_size"] = json!(settings.panel_size.trim());
        root["collapse_mode"] = json!(settings.collapse_mode.trim());
        root["hotkey_jump"] = json!(settings.hotkey_jump.trim());
        root["hotkey_new"] = json!(settings.hotkey_new.trim());
        root["auto_hide"] = json!(settings.auto_hide);
        root["auto_show"] = json!(settings.auto_show);
    });
    // Apply the new bindings immediately; a mode switch while folded moves the fold over.
    register_hotkeys(&app);
    sync_strip(&app);
    // Launch-at-login is OS state — only touch it when the toggle actually changed.
    let mgr = app.autolaunch();
    if settings.autostart != mgr.is_enabled().unwrap_or(false) {
        let _ = if settings.autostart {
            mgr.enable()
        } else {
            mgr.disable()
        };
    }
}

#[tauri::command]
fn toggle_mute(id: String) {
    model::toggle_mute(&id);
}

#[tauri::command]
fn rename(id: String, name: String) {
    model::set_label(&id, &name);
}

/// Shift-click → have the AI labeler name this session from its transcript. Detached like the
/// recorder's spawn: the model call takes seconds and the refresh loop shows the result.
#[tauri::command]
fn ai_rename(id: String) {
    // The labeler reads the session's transcript, which only exists on the session's own host.
    if !model::load().iter().any(|s| s.id == id) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .args(["ai-label", &id, "--force"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pulse with `live` sessions, of which `needs` are asking for you.
    fn pulse(live: usize, needs: &[&str]) -> Pulse {
        Pulse {
            live,
            needs: needs.iter().map(|s| s.to_string()).collect(),
        }
    }

    const BOTH: (bool, bool) = (true, true);

    // A laptop screen with an external monitor sitting to its left (negative x), the layout
    // that strands a remembered position once the external is unplugged.
    const SCREENS: [(i32, i32, i32, i32); 2] = [(0, 0, 1512, 982), (-1920, -200, 1920, 1080)];

    #[test]
    fn remembered_position_is_kept_when_it_is_still_on_a_screen() {
        assert!(point_on_any(&SCREENS, 1200, 40));
        assert!(point_on_any(&SCREENS, -1800, 0));
    }

    #[test]
    fn remembered_position_is_dropped_when_its_monitor_is_gone() {
        assert!(!point_on_any(&SCREENS[..1], -1800, 0));
        assert!(!point_on_any(&SCREENS, 4000, 40));
        assert!(!point_on_any(&SCREENS, 1200, 2000));
    }

    #[test]
    fn monitor_bounds_are_half_open() {
        assert!(point_on_any(&SCREENS[..1], 0, 0));
        assert!(!point_on_any(&SCREENS[..1], 1512, 0));
        assert!(!point_on_any(&SCREENS[..1], 0, 982));
    }

    #[test]
    fn first_crash_restarts() {
        assert!(may_crash_restart(None, 1_000));
    }

    #[test]
    fn a_crash_right_after_a_crash_restart_does_not_loop() {
        assert!(!may_crash_restart(Some(1_000), 1_030));
    }

    #[test]
    fn a_crash_long_after_the_last_restart_restarts_again() {
        assert!(may_crash_restart(
            Some(1_000),
            1_000 + CRASH_RESTART_COOLDOWN_SECS
        ));
    }

    #[test]
    fn shows_when_the_first_session_appears() {
        let a = auto_action(&pulse(0, &[]), &pulse(1, &[]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Show);
    }

    #[test]
    fn shows_when_a_session_newly_needs_you() {
        let a = auto_action(&pulse(2, &[]), &pulse(2, &["s1"]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Show);
    }

    /// The edge-trigger contract: a session that was already asking for you last tick must not
    /// re-show the panel every 1.5s after you hide it by hand.
    #[test]
    fn standing_needs_does_not_re_show() {
        let a = auto_action(&pulse(2, &["s1"]), &pulse(2, &["s1"]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Nothing);
    }

    /// Same contract for a steady stream of live sessions: no transition, no interference.
    #[test]
    fn steady_live_sessions_do_not_re_show() {
        let a = auto_action(&pulse(3, &[]), &pulse(2, &[]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Nothing);
    }

    #[test]
    fn hides_when_the_last_session_goes_quiet() {
        let a = auto_action(&pulse(1, &[]), &pulse(0, &[]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Hide);
    }

    #[test]
    fn stays_hidden_while_nothing_is_live() {
        let a = auto_action(&pulse(0, &[]), &pulse(0, &[]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Nothing);
    }

    /// A different session asking for you is a new edge even though the count is unchanged.
    #[test]
    fn a_different_session_needing_you_is_a_new_edge() {
        let a = auto_action(&pulse(2, &["s1"]), &pulse(2, &["s2"]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Show);
    }

    #[test]
    fn show_wins_when_one_session_ends_as_another_asks() {
        let a = auto_action(&pulse(1, &[]), &pulse(1, &["s2"]), BOTH.0, BOTH.1);
        assert_eq!(a, Auto::Show);
    }

    // The two toggles are independent: each must be inert when off and still fire when it's the
    // only one on.

    #[test]
    fn hide_off_leaves_the_panel_up() {
        let a = auto_action(&pulse(1, &[]), &pulse(0, &[]), false, true);
        assert_eq!(a, Auto::Nothing);
    }

    #[test]
    fn show_off_leaves_the_panel_hidden() {
        let a = auto_action(&pulse(0, &[]), &pulse(1, &["s1"]), true, false);
        assert_eq!(a, Auto::Nothing);
    }

    #[test]
    fn hide_alone_still_hides() {
        let a = auto_action(&pulse(1, &[]), &pulse(0, &[]), true, false);
        assert_eq!(a, Auto::Hide);
    }

    #[test]
    fn show_alone_still_shows() {
        let a = auto_action(&pulse(0, &[]), &pulse(1, &[]), false, true);
        assert_eq!(a, Auto::Show);
    }

    #[test]
    fn both_off_never_acts() {
        assert_eq!(
            auto_action(&pulse(1, &[]), &pulse(0, &[]), false, false),
            Auto::Nothing
        );
        assert_eq!(
            auto_action(&pulse(0, &[]), &pulse(1, &["s1"]), false, false),
            Auto::Nothing
        );
    }

    /// Muted and finished sessions must not count as live, or a muted session would pop the
    /// panel up — the opposite of what muting it meant.
    #[test]
    fn pulse_counts_only_unmuted_live_sessions() {
        let now = model::now();
        let sess = |id: &str, state: &str, mute_until: f64| model::Sess {
            id: id.into(),
            topic: id.into(),
            state: state.into(),
            updated: now,
            message: String::new(),
            terminal: "wt".into(),
            term_pid: 1,
            pid: 1,
            tab_title: String::new(),
            tty: String::new(),
            mute_until,
            ..Default::default()
        };
        let p = Pulse::of(&[
            sess("a", "needs", 0.0),
            sess("b", "working", 0.0),
            sess("c", "needs", now + 3600.0), // muted
            sess("d", "idle", 0.0),           // finished
            sess("e", "done", 0.0),           // finished
        ]);
        assert_eq!(p.live, 2);
        assert_eq!(p.needs, ["a".to_string()].into_iter().collect());
    }
}
