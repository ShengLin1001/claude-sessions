//! Windows Terminal. WT has no API to focus/close a tab by process, so: per-tab focus goes
//! through UI Automation (each tab is a UIA TabItem named by its title, supporting
//! SelectionItemPattern), close selects the tab then sends Ctrl+Shift+W, and new-session
//! sends Ctrl+Shift+T and TYPES the configured commands (an elevated WT refuses `wt -w 0`
//! remoting, and typing runs in the user's own shell so profile aliases work). Ported from
//! WtTabs.cs / Interop.cs.

use super::tabmatch::{self, Target};
use super::Terminal;
use crate::model::Sess;
use crate::platform::{focus_window, main_window_for_pid, process_map};
use std::time::Duration;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VK_CONTROL, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

pub struct Wt;

impl Terminal for Wt {
    fn id(&self) -> &'static str {
        "wt"
    }

    fn label(&self) -> &'static str {
        "Windows Terminal"
    }

    fn focus(&self, s: &Sess) -> bool {
        let p = if s.term_pid > 0 { s.term_pid } else { s.pid };
        if p <= 0 {
            return false;
        }
        let h = main_window_for_pid(p);
        if h.is_null() {
            return false;
        }
        let _ = select_tab(h, &s.tab_title, &s.topic); // best-effort; window focus still helps
        focus_window(h);
        true
    }

    /// Select the tab, then Ctrl+Shift+W — guarded on WT actually being foreground so the
    /// chord can't hit another app. If the tab can't be identified confidently, do nothing
    /// (handled: never close the wrong tab, and don't let the caller kill the process tree).
    fn close(&self, s: &Sess) -> bool {
        if s.term_pid <= 0 {
            return false;
        }
        let h = main_window_for_pid(s.term_pid);
        if h.is_null() {
            return false;
        }
        if select_tab(h, &s.tab_title, &s.topic) {
            focus_window(h);
            std::thread::sleep(Duration::from_millis(200));
            if unsafe { GetForegroundWindow() } == h {
                send_chord(&[VK_CONTROL, VK_SHIFT, 0x57]); // Ctrl+Shift+W
            }
        }
        true
    }

    fn new_session(&self, cmds: &[String]) -> bool {
        // When fired by the global hotkey (Ctrl+Alt+N), its modifiers are still physically
        // held as we start. A synthetic Ctrl+Shift+T on top of a held Alt becomes
        // Ctrl+Alt+Shift+T (not WT's new-tab chord) — no tab opens and the commands land in
        // the current one. Wait for the user to let go first. Mouse-triggered calls (the +
        // button) hold nothing, so this returns at once.
        wait_for_modifiers_release();
        let mut h = wt_window();
        if h.is_null() {
            // No terminal open — start one, then find its window.
            if std::process::Command::new("wt").spawn().is_err() {
                return false;
            }
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(250));
                h = wt_window();
                if !h.is_null() {
                    break;
                }
            }
            if h.is_null() {
                return false;
            }
            focus_window(h);
            std::thread::sleep(Duration::from_millis(800)); // let the first shell come up
        } else {
            focus_window(h);
            std::thread::sleep(Duration::from_millis(200));
            if unsafe { GetForegroundWindow() } != h {
                return true; // focus failed — don't type into whatever is active
            }
            send_chord(&[VK_CONTROL, VK_SHIFT, 0x54]); // Ctrl+Shift+T: new tab
            std::thread::sleep(Duration::from_millis(700)); // let the shell start
        }
        for cmd in cmds {
            if unsafe { GetForegroundWindow() } != h {
                return true; // user switched away mid-typing — stop
            }
            type_text(cmd);
            send_chord(&[VK_RETURN]);
            std::thread::sleep(Duration::from_millis(150));
        }
        true
    }
}

/// Focus whichever WT tab matches the title, with no process to anchor on — used for remote
/// sessions, whose pids live on the other machine. False (and nothing raised) when no tab
/// matches confidently.
pub fn focus_by_title(tab_title: &str, topic: &str) -> bool {
    let h = wt_window();
    if h.is_null() || !select_tab(h, tab_title, topic) {
        return false;
    }
    focus_window(h);
    true
}

fn wt_window() -> HWND {
    process_map()
        .iter()
        .find(|(_, (_, name))| name == "windowsterminal.exe")
        .map(|(p, _)| main_window_for_pid(*p))
        .unwrap_or(std::ptr::null_mut())
}

// ---- synthetic keyboard input ----

/// Block until Ctrl/Alt/Shift/Win are all physically released (or ~1s elapses). Lets a global
/// hotkey's own modifiers clear before we synthesize chords/typing, so the injected input
/// isn't corrupted by keys the user is still holding.
fn wait_for_modifiers_release() {
    let any_held = || {
        [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
            .iter()
            .any(|&vk| (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0)
    };
    for _ in 0..100 {
        if !any_held() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

fn send_inputs(inputs: &[INPUT]) {
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

/// Press the keys in order, release in reverse (e.g. Ctrl+Shift+T).
fn send_chord(vks: &[u16]) {
    let mut seq: Vec<INPUT> = vks.iter().map(|&vk| key_input(vk, 0, 0)).collect();
    seq.extend(
        vks.iter()
            .rev()
            .map(|&vk| key_input(vk, 0, KEYEVENTF_KEYUP)),
    );
    send_inputs(&seq);
}

/// Type arbitrary text into the focused window via KEYEVENTF_UNICODE (layout-independent).
fn type_text(s: &str) {
    let mut seq = Vec::new();
    for u in s.encode_utf16() {
        seq.push(key_input(0, u, KEYEVENTF_UNICODE));
        seq.push(key_input(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send_inputs(&seq);
}

// ---- per-tab focus via UI Automation (port of WtTabs.cs) ----
//
// This is just the UIA plumbing: enumerate WT's tab TabItems and their names, then hand the
// names to tabmatch::choose() for the (tested) fuzzy-matching decision and Select() the winner.

fn select_tab(hwnd: HWND, tab_title: &str, topic: &str) -> bool {
    let target = Target::new(tab_title, topic);
    if target.is_empty() {
        return false;
    }
    // UIA can throw on transient window state — treat any failure as "couldn't switch".
    unsafe { uia_select_tab(hwnd, &target).unwrap_or(false) }
}

unsafe fn uia_select_tab(hwnd: HWND, target: &Target) -> windows::core::Result<bool> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationSelectionItemPattern,
        TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_SelectionItemPatternId,
        UIA_TabItemControlTypeId,
    };

    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); // fine if already initialised
    let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
    let root = uia.ElementFromHandle(windows::Win32::Foundation::HWND(hwnd as _))?;
    // Raw VT_I4 VARIANT holding the TabItem control-type id (the 0.61 bindings expose the
    // bare C struct, no From impls).
    let mut var: windows::Win32::System::Variant::VARIANT = std::mem::zeroed();
    (*var.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VT_I4;
    (*var.Anonymous.Anonymous).Anonymous.lVal = UIA_TabItemControlTypeId.0;
    let cond = uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &var)?;
    let tabs = root.FindAll(TreeScope_Descendants, &cond)?;
    let n = tabs.Length()?;
    if n <= 1 {
        return Ok(false); // single tab — nothing to switch to
    }

    // Collect tabs and their names in order, then let the tested policy pick the index.
    let mut els: Vec<IUIAutomationElement> = Vec::with_capacity(n as usize);
    let mut names: Vec<String> = Vec::with_capacity(n as usize);
    for i in 0..n {
        let el = tabs.GetElement(i)?;
        names.push(el.CurrentName().map(|b| b.to_string()).unwrap_or_default());
        els.push(el);
    }
    let Some(idx) = tabmatch::choose(&names, target) else {
        return Ok(false);
    };
    Ok(els[idx]
        .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
        .map(|p| p.Select().is_ok())
        .unwrap_or(false))
}
