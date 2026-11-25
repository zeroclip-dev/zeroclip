//! The toast card: a plain Win32 layered window, drawn with GDI and shown with
//! [`UpdateLayeredWindow`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-updatelayeredwindow).
//!
//! This is how a crosshair overlay stays above a game, so it is the same recipe: a pop-up with
//! `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`,
//! its own message loop on its own thread, `SW_SHOWNOACTIVATE`, and the top-most place asserted
//! again while the card is up (some games take it back). Clicks pass through, it never takes
//! focus, and `WDA_EXCLUDEFROMCAPTURE` keeps it out of recordings and screenshots. No injection,
//! no hooks: public Win32 calls only.
//!
//! The card is opaque, so GDI can draw its text; a pass over the pixels then rounds the corners
//! and premultiplies, as a layered window with per-pixel alpha wants. The fade is the window's
//! constant alpha, stepped by a 16 ms timer.
//!
//! Limit: borderless and windowed games only. An exclusive-fullscreen game owns the display and
//! composites nothing over itself.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::mpsc::channel;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    CreateSolidBrush, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, Ellipse, FillRect,
    GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GWL_EXSTYLE, GetMessageW, GetWindowLongPtrW,
    HWND_TOPMOST, KillTimer, MSG, PostMessageW, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetTimer, SetWindowDisplayAffinity, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow,
    WDA_EXCLUDEFROMCAPTURE, WM_APP, WM_LBUTTONDOWN, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

/// Card size and corner radius in logical pixels, and the gap from the screen's top-left corner.
const CARD_W: f32 = 300.0;
const CARD_H: f32 = 60.0;
const RADIUS: f32 = 10.0;
const MARGIN: f32 = 24.0;
/// The alert popup is larger than the card, and has a close button in its top-right corner.
const ALERT_W: f32 = 360.0;
const ALERT_H: f32 = 92.0;
const CLOSE_SIZE: f32 = 36.0;

const WM_SHOW: u32 = WM_APP + 1;
const TICK: usize = 1;
/// How often (in ticks, 16 ms each) the top-most place is asserted again.
const RAISE_EVERY: u32 = 10;

/// What a toast shows. Colours are `0xRRGGBB`.
#[derive(Clone, Debug)]
pub struct Toast {
    /// The bold first line.
    pub title: String,
    /// The smaller second line, if any.
    pub detail: Option<String>,
    /// The status dot.
    pub accent: u32,
    /// The card.
    pub background: u32,
    /// The 1 px outline.
    pub border: u32,
    /// The title.
    pub text: u32,
    /// The detail line.
    pub muted: u32,
    /// Time at full opacity.
    pub hold: Duration,
    /// Fade-in and fade-out, each. Zero shows and hides it at once.
    pub fade: Duration,
    /// A failure popup: larger, an alert icon, and a close (x) button. It takes clicks (the card
    /// does not), and a plain card can't replace it while it is up.
    pub alert: bool,
}

/// The toast waiting for the window thread to pick it up. A newer one replaces it.
static PENDING: Mutex<Option<Toast>> = Mutex::new(None);
/// The window, made once on its own thread. `Err` says why there is none.
static WINDOW: OnceLock<Result<isize, String>> = OnceLock::new();

/// Shows `toast`, replacing the one that is up.
///
/// # Errors
/// The window couldn't be made, or couldn't be excluded from capture. Nothing is shown then.
pub fn show(toast: Toast) -> Result<(), String> {
    let hwnd = WINDOW.get_or_init(spawn_window).as_ref().map_err(Clone::clone)?;
    *PENDING.lock().map_err(|e| e.to_string())? = Some(toast);
    // SAFETY: the window belongs to a thread that runs for the life of the process.
    unsafe { PostMessageW(Some(HWND(*hwnd as *mut c_void)), WM_SHOW, WPARAM(0), LPARAM(0)) }
        .map_err(|e| e.to_string())
}

/// The overlay window, if it has been made. Other Win32 helpers use it as an owner window.
pub(crate) fn window() -> Option<HWND> {
    WINDOW.get()?.as_ref().ok().map(|h| HWND(*h as *mut c_void))
}

/// Starts the window thread and waits for it to say whether the window exists.
fn spawn_window() -> Result<isize, String> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("zc-toast".into())
        .spawn(move || {
            let made = make_window();
            let ok = made.is_ok();
            let _ = tx.send(made.map(|h| h.0 as isize));
            if ok {
                message_loop();
            }
        })
        .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

fn make_window() -> Result<HWND, String> {
    // SAFETY: plain window-class and window creation on this thread; every pointer passed is to a
    // value that lives across its call.
    unsafe {
        let inst = GetModuleHandleW(None).map_err(|e| e.to_string())?.into();
        let class = w!("ZeroClipToast");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            lpszClassName: class,
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(inst),
            None,
        )
        .map_err(|e| e.to_string())?;
        // Before it is ever shown, so a capture never sees it. No affinity, no card.
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).map_err(|e| e.to_string())?;
        Ok(hwnd)
    }
}

fn message_loop() {
    let mut msg = MSG::default();
    // SAFETY: the standard message loop, on the thread that made the window.
    unsafe {
        // -1 is an error, and `as_bool()` would treat it as true and spin.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The card on screen: its bitmap, and when it started.
struct Live {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    size: SIZE,
    at: POINT,
    started: Instant,
    hold: Duration,
    fade: Duration,
    ticks: u32,
    /// The close button's hit area in window pixels, for an alert.
    close: Option<RECT>,
    alert: bool,
}

impl Live {
    /// Frees the bitmap and its device context.
    fn free(self) {
        // SAFETY: these were created by `render` and are freed once, here.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }

    /// 0..=255 for how long the card has been up: in, hold, out. `None` once it is over.
    fn alpha(&self) -> Option<u8> {
        let (e, f) = (self.started.elapsed(), self.fade);
        let total = f * 2 + self.hold;
        if e >= total {
            return None;
        }
        let level = if e < f {
            e.as_secs_f32() / f.as_secs_f32()
        } else if e < f + self.hold {
            1.0
        } else {
            (total - e).as_secs_f32() / f.as_secs_f32()
        };
        Some((level.clamp(0.0, 1.0) * 255.0).round() as u8)
    }
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

fn colorref(rgb: u32) -> COLORREF {
    COLORREF(((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF))
}

/// Draws the card into a new bitmap.
fn render(toast: &Toast, scale: f32) -> Option<Live> {
    let (cw, ch) = if toast.alert { (ALERT_W, ALERT_H) } else { (CARD_W, CARD_H) };
    let (w, h) = ((cw * scale).round() as i32, (ch * scale).round() as i32);
    // SAFETY: GDI calls on handles made here; the bitmap's pixels are read and written only
    // between `CreateDIBSection` and `DeleteObject`, within its `w * h` size.
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let made = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0);
        ReleaseDC(None, screen);
        let Ok(bitmap) = made else {
            let _ = DeleteDC(dc);
            return None;
        };
        let old = SelectObject(dc, bitmap.into());

        let fill = |r: RECT, rgb: u32| {
            let brush = CreateSolidBrush(colorref(rgb));
            FillRect(dc, &r, brush);
            let _ = DeleteObject(brush.into());
        };
        fill(RECT { left: 0, top: 0, right: w, bottom: h }, toast.border);
        fill(RECT { left: 1, top: 1, right: w - 1, bottom: h - 1 }, toast.background);

        // The status dot, or for an alert a larger disc with a "!" in it.
        let dot = ((if toast.alert { 28.0 } else { 10.0 }) * scale).round() as i32;
        let (dx, dy) = ((16.0 * scale).round() as i32, (h - dot) / 2);
        let brush = CreateSolidBrush(colorref(toast.accent));
        let before = SelectObject(dc, brush.into());
        let _ = Ellipse(dc, dx, dy, dx + dot, dy + dot);
        SelectObject(dc, before);
        let _ = DeleteObject(brush.into());

        SetBkMode(dc, TRANSPARENT);
        let font_for = |px: f32, weight: i32| {
            CreateFontW(
                -((px * scale).round() as i32),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            )
        };
        let centered = |dc: HDC, text: &str, px: f32, rgb: u32, mut rect: RECT| {
            let font = font_for(px, 700);
            let before = SelectObject(dc, font.into());
            SetTextColor(dc, colorref(rgb));
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(
                dc,
                &mut wide,
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(dc, before);
            let _ = DeleteObject(font.into());
        };
        let mut close = None;
        let mut right_pad = (12.0 * scale).round() as i32;
        if toast.alert {
            let glyph = RECT { left: dx, top: dy, right: dx + dot, bottom: dy + dot };
            centered(dc, "!", 17.0, toast.background, glyph);
            let side = (CLOSE_SIZE * scale).round() as i32;
            let hit = RECT { left: w - side - 4, top: 4, right: w - 4, bottom: 4 + side };
            centered(dc, "×", 22.0, toast.muted, hit);
            close = Some(hit);
            right_pad = side + 8;
        }

        // The text: a semibold title, and a smaller muted line under it.
        let left = dx + dot + (12.0 * scale).round() as i32;
        let line = |dc: HDC, text: &str, px: f32, weight: i32, rgb: u32, top: i32, bottom: i32| {
            let font = font_for(px, weight);
            let before = SelectObject(dc, font.into());
            SetTextColor(dc, colorref(rgb));
            let mut rect = RECT { left, top, right: w - right_pad, bottom };
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(
                dc,
                &mut wide,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(dc, before);
            let _ = DeleteObject(font.into());
        };
        let pad = (8.0 * scale).round() as i32;
        match &toast.detail {
            Some(detail) => {
                line(dc, &toast.title, 15.0, 600, toast.text, pad, h / 2 + (2.0 * scale) as i32);
                line(dc, detail, 12.0, 400, toast.muted, h / 2, h - pad);
            }
            None => line(dc, &toast.title, 15.0, 600, toast.text, 0, h),
        }
        let _ = GdiFlush();

        // Rounded corners, and premultiplied alpha: GDI left every alpha at 0.
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (w * h) as usize);
        let r = RADIUS * scale;
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                // Distance past the corner's arc, when the pixel is in a corner square.
                let cx = if fx < r {
                    r
                } else if fx > w as f32 - r {
                    w as f32 - r
                } else {
                    fx
                };
                let cy = if fy < r {
                    r
                } else if fy > h as f32 - r {
                    h as f32 - r
                } else {
                    fy
                };
                let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                let cover = (r - d + 0.5).clamp(0.0, 1.0);
                let p = &mut pixels[(y * w + x) as usize];
                let (red, green, blue) = ((*p >> 16) & 0xFF, (*p >> 8) & 0xFF, *p & 0xFF);
                let m = |c: u32| (c as f32 * cover).round() as u32;
                *p =
                    ((cover * 255.0).round() as u32) << 24 | m(red) << 16 | m(green) << 8 | m(blue);
            }
        }

        Some(Live {
            dc,
            bitmap,
            old,
            size: SIZE { cx: w, cy: h },
            at: POINT { x: (MARGIN * scale).round() as i32, y: (MARGIN * scale).round() as i32 },
            started: Instant::now(),
            hold: toast.hold,
            fade: toast.fade,
            ticks: 0,
            close,
            alert: toast.alert,
        })
    }
}

/// Puts the card on screen at constant alpha `alpha`.
fn present(hwnd: HWND, live: &Live, alpha: u8) {
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // SAFETY: `hwnd` is our window; the pointers are to locals and to `live`, all alive across the
    // call; the screen DC is released right after.
    unsafe {
        let screen = GetDC(None);
        let _ = UpdateLayeredWindow(
            hwnd,
            Some(screen),
            Some(&live.at),
            Some(&live.size),
            Some(live.dc),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        ReleaseDC(None, screen);
    }
}

fn raise(hwnd: HWND) {
    // SAFETY: `hwnd` is our window; no move, resize or activation, only the z-order changes.
    let _ = unsafe {
        SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)
    };
}

/// Hides the card and frees its bitmap.
fn dismiss(hwnd: HWND) {
    // SAFETY: `hwnd` is our window.
    unsafe {
        let _ = KillTimer(Some(hwnd), TICK);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    if let Some(old) = LIVE.with_borrow_mut(Option::take) {
        old.free();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SHOW => {
            let toast = PENDING.lock().ok().and_then(|mut p| p.take());
            let Some(toast) = toast else { return LRESULT(0) };
            // SAFETY: reads the system DPI; no arguments.
            let scale = (unsafe { GetDpiForSystem() } as f32 / 96.0).max(1.0);
            // A plain card never knocks an alert off the screen.
            let alert_up =
                LIVE.with_borrow(|l| l.as_ref().is_some_and(|l| l.alert && l.alpha().is_some()));
            if alert_up && !toast.alert {
                return LRESULT(0);
            }
            let Some(live) = render(&toast, scale) else { return LRESULT(0) };
            // An alert takes clicks, for its close button; a card passes them through.
            // SAFETY: reads and writes our own window's extended style.
            unsafe {
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                let clicks = WS_EX_TRANSPARENT.0 as isize;
                let ex = if toast.alert { ex & !clicks } else { ex | clicks };
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
            }
            present(hwnd, &live, live.alpha().unwrap_or(255));
            // SAFETY: `hwnd` is our window. SW_SHOWNOACTIVATE shows it without taking focus.
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                SetTimer(Some(hwnd), TICK, 16, None);
            }
            raise(hwnd);
            if let Some(old) = LIVE.with_borrow_mut(|l| l.replace(live)) {
                old.free();
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TICK => {
            let done = LIVE.with_borrow_mut(|slot| {
                let Some(live) = slot.as_mut() else { return true };
                live.ticks += 1;
                match live.alpha() {
                    Some(alpha) => {
                        present(hwnd, live, alpha);
                        // Some games take the top-most place back; get above them again.
                        if live.ticks % RAISE_EVERY == 0 {
                            raise(hwnd);
                        }
                        false
                    }
                    None => true,
                }
            });
            if done {
                dismiss(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // The click's position is the low and high words of `lp`, in client pixels.
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
            let hit = LIVE.with_borrow(|l| {
                l.as_ref()
                    .and_then(|l| l.close)
                    .is_some_and(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            });
            if hit {
                dismiss(hwnd);
            }
            LRESULT(0)
        }
        // SAFETY: the default handling of a message this window doesn't deal with.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
