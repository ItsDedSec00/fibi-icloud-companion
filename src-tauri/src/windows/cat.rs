use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::{App, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::events::SpriteRect;
use crate::platform;

const CAT_WINDOW_HEIGHT: u32 = 540;

#[derive(Default)]
pub struct SpriteHitbox {
    pub x: AtomicI32,
    pub y: AtomicI32,
    pub w: AtomicI32,
    pub h: AtomicI32,
    pub valid: AtomicBool,
}

impl SpriteHitbox {
    pub fn set(&self, rect: &SpriteRect) {
        self.x.store(rect.x, Ordering::Relaxed);
        self.y.store(rect.y, Ordering::Relaxed);
        self.w.store(rect.width, Ordering::Relaxed);
        self.h.store(rect.height, Ordering::Relaxed);
        self.valid.store(true, Ordering::Relaxed);
    }

    fn contains(&self, screen_x: i32, screen_y: i32, win_x: i32, win_y: i32) -> bool {
        if !self.valid.load(Ordering::Relaxed) {
            return false;
        }
        let sx = self.x.load(Ordering::Relaxed) + win_x;
        let sy = self.y.load(Ordering::Relaxed) + win_y;
        let sw = self.w.load(Ordering::Relaxed);
        let sh = self.h.load(Ordering::Relaxed);
        screen_x >= sx && screen_x < sx + sw && screen_y >= sy && screen_y < sy + sh
    }
}

pub fn setup(app: &mut App) -> tauri::Result<()> {
    let window = app
        .get_webview_window("cat")
        .expect("cat window missing from tauri.conf.json");

    position_over_taskbar(&window)?;
    window.set_ignore_cursor_events(true)?;
    window.show()?;

    let hitbox = Arc::new(SpriteHitbox::default());
    app.manage(hitbox.clone());

    spawn_hit_test_loop(window.clone(), hitbox);
    spawn_topmost_keepalive(window.clone());
    spawn_fullscreen_watcher(window);

    Ok(())
}

/// Hides the cat while a fullscreen app, game, or presentation is running on
/// the foreground monitor. When hidden the webview is paused by the OS, which
/// drops RAF callbacks and most CPU usage to ~0 — the only ongoing work is
/// this 1.5 s poll.
///
/// Before hiding we emit `cat://will-hide` so the frontend can pre-position
/// the cat off-screen on the right; that way when the window is shown again
/// (fullscreen ended) the cat is already off-screen and runs in from the
/// right edge instead of popping back into place.
fn spawn_fullscreen_watcher(window: WebviewWindow) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(1500));
        let mut currently_hidden = false;
        loop {
            ticker.tick().await;
            let should_hide = platform::user_busy_or_fullscreen();
            if should_hide == currently_hidden {
                continue;
            }
            if should_hide {
                let _ = window.emit("cat://will-hide", ());
                // Give the webview a moment to apply the off-screen
                // position before we hide it — without this, the hide
                // sometimes wins the race and we get a brief flash of the
                // cat at its old position when shown again.
                tokio::time::sleep(Duration::from_millis(120)).await;
                if window.hide().is_ok() {
                    currently_hidden = true;
                }
            } else if window.show().is_ok() {
                currently_hidden = false;
            }
        }
    });
}

fn position_over_taskbar(window: &WebviewWindow) -> tauri::Result<()> {
    let monitor = window
        .current_monitor()?
        .or_else(|| window.primary_monitor().ok().flatten())
        .ok_or_else(|| tauri::Error::from(anyhow::anyhow!("no monitor available")))?;

    let size = monitor.size();
    let pos = monitor.position();
    let taskbar = platform::taskbar_height();
    let width = size.width;
    let height = CAT_WINDOW_HEIGHT;
    let x = pos.x;
    let y = pos.y + size.height as i32 - taskbar - height as i32;

    window.set_size(PhysicalSize { width, height })?;
    window.set_position(PhysicalPosition { x, y })?;
    Ok(())
}

fn spawn_hit_test_loop(window: WebviewWindow, hitbox: Arc<SpriteHitbox>) {
    tauri::async_runtime::spawn(async move {
        let mut last_ignore = true;
        let mut ticker = tokio::time::interval(Duration::from_millis(33));
        loop {
            ticker.tick().await;
            let (cx, cy) = match platform::cursor_pos() {
                Some(p) => p,
                None => continue,
            };
            let pos = match window.outer_position() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let over_sprite = hitbox.contains(cx, cy, pos.x, pos.y);
            let want_ignore = !over_sprite;
            if want_ignore != last_ignore {
                if window.set_ignore_cursor_events(want_ignore).is_ok() {
                    last_ignore = want_ignore;
                }
            }
        }
    });
}

fn spawn_topmost_keepalive(window: WebviewWindow) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3));
        loop {
            ticker.tick().await;
            #[cfg(windows)]
            if let Ok(hwnd) = window.hwnd() {
                platform::reassert_topmost(hwnd.0 as isize);
            }
            #[cfg(not(windows))]
            let _ = &window;
        }
    });
}
