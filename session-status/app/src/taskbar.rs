//! The taskbar strip (Windows): the panel footer's counts — a dot and a number per state — in a
//! small native window parented into the taskbar, the way claude-usage-monitor and TrafficMonitor
//! sit there. Shown while the panel is folded in "taskbar" mode; clicking it toggles the panel.
//!
//! Native GDI rather than a second webview: DWM transparency doesn't reach child windows, and
//! hosting WebView2 inside explorer's taskbar is a lot of risk for four numbers. Being a child of
//! the taskbar (not a topmost popup) means it hides with it — fullscreen apps, auto-hide.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use windows_sys::w;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateRoundRectRgn,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, GdiFlush, GetDC, GetTextExtentPoint32W,
    InvalidateRect, MapWindowPoints, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    SetWindowRgn, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_SINGLELINE, DT_VCENTER, FW_SEMIBOLD, HDC,
    HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, FindWindowW, GetClientRect,
    GetParent, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, LoadCursorW,
    RegisterClassW, SetWindowPos, HWND_TOP, IDC_HAND, MA_NOACTIVATE, SWP_NOACTIVATE, WM_ERASEBKGND,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_NCDESTROY, WM_PAINT, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_VISIBLE,
};

/// The footer chips, in order. Callers count sessions in these states.
pub const STATES: [&str; 4] = ["needs", "yourturn", "working", "done"];

type Rgb = (u8, u8, u8);

/// The panel's two themes (ui/index.html), worn to match the taskbar.
struct Palette {
    bg: Rgb,
    text: Rgb,
    faint: Rgb,
    zero_dot: Rgb,
    dots: [Rgb; 4], // in STATES order
}

const DARK: Palette = Palette {
    bg: (0x20, 0x20, 0x23),
    text: (0xF5, 0xF5, 0xF7),
    faint: (0x6A, 0x6A, 0x6E),
    zero_dot: (0x6A, 0x6A, 0x6E),
    dots: [
        (0xFF, 0x45, 0x3A),
        (0xFF, 0xD6, 0x0A),
        (0x34, 0xC7, 0x59),
        (0x8E, 0x8E, 0x93),
    ],
};

const LIGHT: Palette = Palette {
    bg: (0xFA, 0xF8, 0xF4),
    text: (0x2A, 0x26, 0x22),
    faint: (0xB5, 0xAE, 0xA4),
    zero_dot: (0xCC, 0xC5, 0xBA),
    dots: [(215, 0, 21), (240, 160, 0), (30, 125, 58), (110, 110, 115)],
};

static STRIP: AtomicIsize = AtomicIsize::new(0); // our HWND, 0 while there is none
static COUNTS: Mutex<[usize; 4]> = Mutex::new([0; 4]);
static LIGHT_BAR: AtomicBool = AtomicBool::new(false);
static ON_CLICK: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

pub fn on_click(f: impl Fn() + Send + Sync + 'static) {
    let _ = ON_CLICK.set(Box::new(f));
}

/// Create, place, repaint or remove the strip. Main thread only: the window belongs to the thread
/// that creates it, and only the main thread pumps messages — a child of the taskbar on an
/// unpumped thread would freeze explorer's taskbar with it (their input queues are joined).
pub fn sync(wanted: bool, counts: [usize; 4]) {
    unsafe {
        let taskbar = FindWindowW(w!("Shell_TrayWnd"), std::ptr::null());
        let mut hwnd = STRIP.load(Ordering::Relaxed) as HWND;
        // Explorer restarting replaces the taskbar; drop a strip left on the old one.
        if !hwnd.is_null() && (!wanted || GetParent(hwnd) != taskbar) {
            DestroyWindow(hwnd);
            hwnd = std::ptr::null_mut();
        }
        if wanted && hwnd.is_null() && !taskbar.is_null() {
            hwnd = create(taskbar);
        }
        STRIP.store(hwnd as isize, Ordering::Relaxed);
        if hwnd.is_null() {
            return;
        }
        let changed = std::mem::replace(&mut *COUNTS.lock().unwrap(), counts) != counts;
        let light = light_taskbar();
        let rethemed = LIGHT_BAR.swap(light, Ordering::Relaxed) != light;
        place(hwnd, taskbar, &counts);
        if changed || rethemed {
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
    }
}

unsafe fn create(taskbar: HWND) -> HWND {
    let inst = GetModuleHandleW(std::ptr::null());
    let class = w!("ClaudeSessionsTaskbarStrip");
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(wndproc);
    wc.hInstance = inst;
    wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_HAND);
    wc.lpszClassName = class;
    RegisterClassW(&wc); // fails harmlessly once the class exists
    CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class,
        w!(""),
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
        0,
        0,
        0,
        0,
        taskbar,
        std::ptr::null_mut(),
        inst,
        std::ptr::null(),
    )
}

/// The taskbar follows Windows' "default Windows mode", not the app mode.
fn light_taskbar() -> bool {
    let (mut v, mut n) = (0u32, 4u32);
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut v as *mut u32).cast(),
            &mut n,
        )
    } == 0;
    ok && v == 1
}

fn px(scale: f32, v: f32) -> i32 {
    (v * scale).round() as i32
}

fn colorref((r, g, b): Rgb) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// A 32-bit DIB pixel (BGRA in memory).
fn pixel((r, g, b): Rgb) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

unsafe fn font(scale: f32) -> HFONT {
    CreateFontW(
        -px(scale, 12.0),
        0,
        0,
        0,
        FW_SEMIBOLD as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        CLEARTYPE_QUALITY as u32,
        DEFAULT_PITCH as u32,
        w!("Segoe UI"),
    )
}

fn label(n: usize) -> Vec<u16> {
    n.to_string().encode_utf16().collect()
}

/// Pixel width of each count's number in the DC's current font.
unsafe fn text_widths(hdc: HDC, counts: &[usize; 4]) -> [i32; 4] {
    counts.map(|n| {
        let t = label(n);
        let mut size = std::mem::zeroed();
        GetTextExtentPoint32W(hdc, t.as_ptr(), t.len() as i32, &mut size);
        size.cx
    })
}

/// Left edge of each chip (dot, gap, number) and the strip's total width.
fn chip_layout(scale: f32, text_w: [i32; 4]) -> ([i32; 4], i32) {
    let (pad, dot, gap, space) = (
        px(scale, 9.0),
        px(scale, 10.0),
        px(scale, 4.0),
        px(scale, 10.0),
    );
    let mut xs = [0; 4];
    let mut x = pad;
    for (i, w) in text_w.iter().enumerate() {
        xs[i] = x;
        x += dot + gap + w + space;
    }
    (xs, x - space + pad)
}

/// Screen x for the strip's left edge: left of the tray, and of any other app's widget already
/// parked there (claude-usage-monitor sits flush against the tray). `widgets` are x spans.
fn strip_left(tray_left: i32, widgets: &[(i32, i32)], width: i32, gap: i32) -> i32 {
    let mut right = tray_left;
    // Re-check after each step: moving past one widget can land the strip on the next.
    while let Some(&(l, _)) = widgets
        .iter()
        .find(|&&(l, r)| l < right && r > right - width - gap)
    {
        right = l;
    }
    right - gap - width
}

unsafe fn rect_of(hwnd: HWND) -> RECT {
    let mut r = std::mem::zeroed();
    GetWindowRect(hwnd, &mut r);
    r
}

/// Size the strip for these counts and park it left of the tray. Runs every heartbeat, so it
/// follows the tray growing or another widget moving within 1.5s.
// shortcut: assumes a horizontal taskbar on the primary monitor; a side-docked one clips it.
unsafe fn place(hwnd: HWND, taskbar: HWND, counts: &[usize; 4]) {
    let scale = GetDpiForWindow(taskbar) as f32 / 96.0;
    let hdc = GetDC(hwnd);
    let f = font(scale);
    let old = SelectObject(hdc, f);
    let (_, width) = chip_layout(scale, text_widths(hdc, counts));
    SelectObject(hdc, old);
    DeleteObject(f);
    ReleaseDC(hwnd, hdc);

    let bar = rect_of(taskbar);
    let tray = FindWindowExW(
        taskbar,
        std::ptr::null_mut(),
        w!("TrayNotifyWnd"),
        std::ptr::null(),
    );
    let tray_left = if tray.is_null() {
        bar.right
    } else {
        rect_of(tray).left
    };
    let mut shell_pid = 0;
    GetWindowThreadProcessId(taskbar, &mut shell_pid);
    let mut widgets = Vec::new();
    // FindWindowExW, not GetWindow(GW_CHILD): during Aero Peek (hovering a task thumbnail)
    // GW_CHILD returns null, and the strip slid right onto claude-usage-monitor.
    let next = |after| FindWindowExW(taskbar, after, std::ptr::null(), std::ptr::null());
    let mut c = next(std::ptr::null_mut());
    while !c.is_null() {
        let mut pid = 0;
        GetWindowThreadProcessId(c, &mut pid);
        if c != hwnd && pid != shell_pid && IsWindowVisible(c) != 0 {
            let r = rect_of(c);
            widgets.push((r.left, r.right));
        }
        c = next(c);
    }

    let height = px(scale, 24.0).min(bar.bottom - bar.top);
    let mut at = POINT {
        x: strip_left(tray_left, &widgets, width, px(scale, 6.0)),
        y: bar.top + (bar.bottom - bar.top - height) / 2,
    };
    MapWindowPoints(std::ptr::null_mut(), taskbar, &mut at, 1); // child coords are the taskbar's
    let before = rect_of(hwnd);
    // HWND_TOP every tick keeps it above the task-button band, which shares the strip's spot.
    SetWindowPos(hwnd, HWND_TOP, at.x, at.y, width, height, SWP_NOACTIVATE);
    if before.right - before.left != width || before.bottom - before.top != height {
        // Pill ends; the system owns the region from here, so it isn't deleted.
        SetWindowRgn(
            hwnd,
            CreateRoundRectRgn(0, 0, width + 1, height + 1, height, height),
            1,
        );
    }
}

/// Blend an antialiased dot (GDI's Ellipse isn't) of diameter `d` into a `stride`-wide buffer.
fn draw_dot(buf: &mut [u32], stride: i32, x: i32, y: i32, d: i32, col: Rgb, bg: Rgb) {
    let c = d as f32 / 2.0;
    for j in 0..d {
        for i in 0..d {
            let (dx, dy) = (i as f32 + 0.5 - c, j as f32 + 0.5 - c);
            let a = (c + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            let mix = |fg: u8, bg: u8| (bg as f32 + (fg as f32 - bg as f32) * a) as u8;
            let at = (y + j) * stride + x + i;
            if let Some(p) = usize::try_from(at).ok().and_then(|at| buf.get_mut(at)) {
                *p = pixel((mix(col.0, bg.0), mix(col.1, bg.1), mix(col.2, bg.2)));
            }
        }
    }
}

/// Draw into our own 32-bit buffer and blit it: GDI leaves alpha at 0, which the taskbar's
/// transparency effect shows as see-through — drawn straight, the strip came out a blank pill.
unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = std::mem::zeroed::<RECT>();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h, // top-down rows
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..std::mem::zeroed()
    };
    let mut bits = std::ptr::null_mut();
    let mem = CreateCompatibleDC(hdc);
    let dib = CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bits.is_null() || w <= 0 || h <= 0 {
        DeleteObject(dib);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
        return;
    }
    let old_bmp = SelectObject(mem, dib);
    // Borrowed afresh around each GDI call, never across one: GDI writes the same memory.
    let buf = || std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);

    let p = if LIGHT_BAR.load(Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    };
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let counts = *COUNTS.lock().unwrap();
    let f = font(scale);
    let old_font = SelectObject(mem, f);
    SetBkMode(mem, TRANSPARENT as i32);
    let (xs, _) = chip_layout(scale, text_widths(mem, &counts));
    let (dot, gap) = (px(scale, 10.0), px(scale, 4.0));

    buf().fill(pixel(p.bg));
    for (i, &n) in counts.iter().enumerate() {
        // Zero counts go grey, like the panel's footer chips.
        let col = if n > 0 { p.dots[i] } else { p.zero_dot };
        draw_dot(buf(), w, xs[i], (h - dot) / 2, dot, col, p.bg);
    }
    for (i, &n) in counts.iter().enumerate() {
        SetTextColor(mem, colorref(if n > 0 { p.text } else { p.faint }));
        let t = label(n);
        let mut r = RECT {
            left: xs[i] + dot + gap,
            top: 0,
            right: w,
            bottom: h,
        };
        DrawTextW(
            mem,
            t.as_ptr(),
            t.len() as i32,
            &mut r,
            DT_SINGLELINE | DT_VCENTER,
        );
    }
    GdiFlush(); // GDI may batch; finish its writes before touching the bits
    for v in buf() {
        *v |= 0xFF00_0000;
    }
    BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);

    SelectObject(mem, old_font);
    DeleteObject(f);
    SelectObject(mem, old_bmp);
    DeleteObject(dib);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1, // WM_PAINT fills everything; erasing first would flicker
        // Left to the taskbar, a click first activates it and the click itself gets lost.
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_LBUTTONUP => {
            if let Some(f) = ON_CLICK.get() {
                f();
            }
            0
        }
        WM_NCDESTROY => {
            // Explorer exiting takes its taskbar's children with it.
            let _ = STRIP.compare_exchange(hwnd as isize, 0, Ordering::Relaxed, Ordering::Relaxed);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_lay_out_left_to_right_with_padding_on_both_ends() {
        let (xs, w) = chip_layout(1.0, [7, 7, 14, 7]);
        assert_eq!(xs, [9, 40, 71, 109]); // pad 9; each chip dot 10 + gap 4 + text + space 10
        assert_eq!(w, 109 + 10 + 4 + 7 + 9);
    }

    #[test]
    fn dots_are_solid_in_the_middle_and_background_at_the_corners() {
        let mut buf = vec![0u32; 12 * 12];
        draw_dot(&mut buf, 12, 1, 1, 10, (255, 0, 0), (0, 0, 255));
        assert_eq!(buf[6 * 12 + 6], pixel((255, 0, 0)));
        assert_eq!(buf[12 + 1], pixel((0, 0, 255)));
        assert_eq!(buf[0], 0); // outside the dot's square: untouched
    }

    #[test]
    fn strip_sits_left_of_the_tray_when_nothing_else_is_there() {
        assert_eq!(strip_left(1600, &[], 100, 6), 1494);
    }

    #[test]
    fn strip_steps_past_widgets_in_its_way() {
        // usage monitor flush against the tray, and another widget right of where we'd land
        let widgets = [(1290, 1600), (1200, 1250), (10, 50)];
        assert_eq!(strip_left(1600, &widgets, 100, 6), 1094);
    }

    #[test]
    fn widgets_clear_of_the_strip_are_ignored() {
        assert_eq!(strip_left(1600, &[(1290, 1380)], 100, 6), 1494);
    }
}
