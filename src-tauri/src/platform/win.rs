use std::sync::atomic::{AtomicI64, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowExW, FindWindowW, GetCursorPos, GetWindowRect, SetWindowPos,
    SetWindowsHookExW, EVENT_SYSTEM_FOREGROUND, HHOOK, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, WH_KEYBOARD_LL, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut point = POINT { x: 0, y: 0 };
    unsafe {
        if GetCursorPos(&mut point).is_ok() {
            Some((point.x, point.y))
        } else {
            None
        }
    }
}

/// Reassert HWND_TOPMOST so the cat sits above the taskbar even if Explorer
/// promotes Shell_TrayWnd back to the top of the topmost band.
pub fn reassert_topmost(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut _);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Returns true if the user is in a Direct3D fullscreen app, presentation
/// mode, or otherwise "do not disturb" state — in those cases the cat hides
/// itself so it never overlays a game, video, or slideshow.
pub fn user_busy_or_fullscreen() -> bool {
    unsafe {
        match SHQueryUserNotificationState() {
            Ok(state) => {
                state == QUNS_BUSY
                    || state == QUNS_RUNNING_D3D_FULL_SCREEN
                    || state == QUNS_PRESENTATION_MODE
            }
            Err(_) => false,
        }
    }
}

/// Query the actual height of the Windows taskbar via Shell_TrayWnd.
/// Falls back to 48 if anything goes wrong. V1 assumes the taskbar is on the
/// bottom edge — multi-edge support is a V2 task using SHAppBarMessage.
pub fn taskbar_height() -> i32 {
    unsafe {
        if let Some(hwnd) = find_window("Shell_TrayWnd") {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok() {
                let h = rect.bottom - rect.top;
                if h > 0 && h < 200 {
                    return h;
                }
            }
        }
        48
    }
}

/// Returns the screen-space rect (in physical pixels) of the Windows taskbar
/// clock zone — used to anchor the cat's sleep position above the clock.
///
/// Windows 10 exposes `TrayClockWClass` as a Win32 window we can find by
/// class name. Windows 11's newer XAML taskbar doesn't always expose that
/// class — so we fall back to the right edge of `TrayNotifyWnd` (the system
/// tray container, which is always present) or, last resort, the right edge
/// of `Shell_TrayWnd` itself. Either fallback returns a thin strip near the
/// right edge of the taskbar where the clock visually sits.
pub fn taskbar_clock_rect() -> Option<RECT> {
    unsafe {
        let tray = find_window("Shell_TrayWnd")?;

        // 1. Try the explicit Win10-era class, optionally nested under TrayNotifyWnd.
        let clock = find_child(tray, "TrayClockWClass").or_else(|| {
            let notify = find_child(tray, "TrayNotifyWnd")?;
            find_child(notify, "TrayClockWClass")
        });
        if let Some(c) = clock {
            let mut rect = RECT::default();
            if GetWindowRect(c, &mut rect).is_ok() && rect.right > rect.left {
                return Some(rect);
            }
        }

        // 2. Fall back to the right ~60 px of TrayNotifyWnd — that's where the
        //    clock visually sits on Windows 11 with the XAML taskbar.
        if let Some(notify) = find_child(tray, "TrayNotifyWnd") {
            let mut rect = RECT::default();
            if GetWindowRect(notify, &mut rect).is_ok() && rect.right > rect.left {
                return Some(RECT {
                    left: rect.right - 60,
                    top: rect.top,
                    right: rect.right - 8,
                    bottom: rect.bottom,
                });
            }
        }

        // 3. Last resort: right edge of the whole taskbar.
        let mut rect = RECT::default();
        if GetWindowRect(tray, &mut rect).is_ok() && rect.right > rect.left {
            return Some(RECT {
                left: rect.right - 90,
                top: rect.top,
                right: rect.right - 30,
                bottom: rect.bottom,
            });
        }
        None
    }
}

// ── Activity tracking: keyboard + foreground-window switches only ─────────
//
// `GetLastInputInfo` would lump in mouse motion (jitter, hover scrolling),
// which makes the cat too jumpy. Instead we install:
//   - WH_KEYBOARD_LL  → updates LAST_ACTIVITY on every keystroke
//   - SetWinEventHook(EVENT_SYSTEM_FOREGROUND) → updates it whenever the user
//     brings a different program/window to the foreground (e.g. Alt+Tab,
//     launching an app, clicking a taskbar icon).
// Mouse movement is *not* counted. So "scrolling Twitter without clicking"
// will eventually be considered idle, which matches the spec.

static LAST_ACTIVITY_TICK_MS: AtomicI64 = AtomicI64::new(-1);

unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        LAST_ACTIVITY_TICK_MS.store(GetTickCount64() as i64, Ordering::Relaxed);
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

unsafe extern "system" fn foreground_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    LAST_ACTIVITY_TICK_MS.store(GetTickCount64() as i64, Ordering::Relaxed);
}

pub fn install_activity_hooks() {
    LAST_ACTIVITY_TICK_MS.store(unsafe { GetTickCount64() } as i64, Ordering::Relaxed);
    unsafe {
        let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        // Hooks are intentionally never unhooked — they live for the lifetime
        // of the process. Tauri's main thread provides the message pump that
        // both hooks require.
        let _ = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook_proc),
            HINSTANCE(hmod.0),
            0,
        );
        let _ = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
    }
}

/// Seconds since the user last typed a key or switched to a different window.
/// Mouse motion alone never resets this counter.
pub fn idle_duration_secs() -> u32 {
    let last = LAST_ACTIVITY_TICK_MS.load(Ordering::Relaxed);
    if last < 0 {
        return 0;
    }
    let now = unsafe { GetTickCount64() } as i64;
    ((now - last).max(0) / 1000) as u32
}

// ── Internal helpers ───────────────────────────────────────────────────────

fn find_window(class_name: &str) -> Option<HWND> {
    let name: Vec<u16> = class_name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let hwnd = FindWindowW(PCWSTR(name.as_ptr()), PCWSTR::null()).ok()?;
        if hwnd.0.is_null() {
            None
        } else {
            Some(hwnd)
        }
    }
}

fn find_child(parent: HWND, class_name: &str) -> Option<HWND> {
    let name: Vec<u16> = class_name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let hwnd = FindWindowExW(parent, None, PCWSTR(name.as_ptr()), PCWSTR::null()).ok()?;
        if hwnd.0.is_null() {
            None
        } else {
            Some(hwnd)
        }
    }
}
