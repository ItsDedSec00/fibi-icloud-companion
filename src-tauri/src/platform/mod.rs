#[cfg(windows)]
pub mod autostart;
pub mod cpu;
#[cfg(windows)]
pub mod win;

#[cfg(windows)]
pub use win::*;

#[cfg(not(windows))]
pub fn cursor_pos() -> Option<(i32, i32)> {
    None
}

#[cfg(not(windows))]
pub fn reassert_topmost(_hwnd: isize) {}

#[cfg(not(windows))]
pub fn taskbar_height() -> i32 {
    48
}

#[cfg(not(windows))]
pub fn user_busy_or_fullscreen() -> bool {
    false
}

#[cfg(not(windows))]
pub fn idle_duration_secs() -> u32 {
    0
}

#[cfg(not(windows))]
pub fn install_activity_hooks() {}

#[cfg(not(windows))]
pub struct StubRect {
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}

#[cfg(not(windows))]
pub fn taskbar_clock_rect() -> Option<StubRect> {
    None
}
