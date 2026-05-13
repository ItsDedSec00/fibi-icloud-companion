//! System-tray icon with a context menu.
//!   - **iCloud-Status** (disabled label) — shows whether the pyicloud
//!     bridge is currently authenticated against Apple.
//!   - **iCloud neu verbinden** — opens the Settings window for Apple-ID
//!     password + 2FA re-auth.
//!   - **Zuhören** (check item) — toggles the wake-word listener
//!     (Python sidecar) on or off at runtime. Off = mic is closed.
//!   - **Debug-Overlay umschalten** sends `cat://toggle-debug` so the user
//!     can flip the diagnostic overlay without having to remember a hotkey.
//!   - **Beenden** exits the app cleanly.

use std::sync::Mutex;

use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{App, AppHandle, Emitter, Manager, Wry};

use crate::voice::controller::VoiceController;

/// Holds the iCloud-status MenuItem so any module can update its label
/// when it learns about a state transition (prewarm success, reauth
/// success, auth-failure, …).
pub struct ICloudStatusItem(Mutex<Option<MenuItem<Wry>>>);

impl ICloudStatusItem {
    fn new() -> Self {
        Self(Mutex::new(None))
    }
    fn store(&self, item: MenuItem<Wry>) {
        *self.0.lock().expect("status item poisoned") = Some(item);
    }
    pub fn set_connected(&self, ok: bool) {
        let label = if ok {
            "iCloud: verbunden \u{2713}"
        } else {
            "iCloud: nicht verbunden"
        };
        if let Some(item) = self.0.lock().expect("status item poisoned").as_ref() {
            let _ = item.set_text(label);
        }
    }
}

/// Convenience for callers outside this module — uses `try_state` so we
/// don't panic if the controller isn't managed yet (rare race during
/// app boot).
pub fn set_icloud_status(app: &AppHandle, connected: bool) {
    if let Some(item) = app.try_state::<ICloudStatusItem>() {
        item.set_connected(connected);
    }
}

pub fn setup(app: &App) -> tauri::Result<()> {
    // First item: disabled label showing iCloud auth status. Updated from
    // anywhere via `set_icloud_status(app, bool)`.
    let icloud_status = MenuItem::with_id(
        app,
        "icloud-status",
        "iCloud: prüfe…",
        false, // disabled — purely informational
        None::<&str>,
    )?;
    let status_holder = ICloudStatusItem::new();
    status_holder.store(icloud_status.clone());
    app.manage(status_holder);

    let listen = CheckMenuItem::with_id(
        app,
        "listen",
        "Zuhören (\u{201E}Fibi\u{201C})",
        true,  // enabled (clickable)
        true,  // initially checked — controller is enabled at startup
        None::<&str>,
    )?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Beim Login starten",
        true,
        crate::platform::autostart::is_enabled(),
        None::<&str>,
    )?;
    let settings = MenuItem::with_id(
        app,
        "settings",
        "iCloud neu verbinden…",
        true,
        None::<&str>,
    )?;
    let debug =
        MenuItem::with_id(app, "debug", "Debug-Overlay umschalten", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Beenden", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&icloud_status, &settings, &listen, &autostart, &debug, &quit],
    )?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    // Clone the menu-item handle into the event closure so we can flip
    // its checked state after we toggle the controller.
    let listen_for_event = listen.clone();
    let autostart_for_event = autostart.clone();

    TrayIconBuilder::new()
        .icon(icon)
        .tooltip("Companion")
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "autostart" => {
                let next = !crate::platform::autostart::is_enabled();
                let res = if next {
                    crate::platform::autostart::enable()
                } else {
                    crate::platform::autostart::disable()
                };
                match res {
                    Ok(()) => {
                        let _ = autostart_for_event.set_checked(next);
                    }
                    Err(e) => {
                        tracing::warn!("autostart toggle failed: {}", e);
                        // Re-sync the checkmark with actual state.
                        let _ = autostart_for_event
                            .set_checked(crate::platform::autostart::is_enabled());
                    }
                }
            }
            "listen" => {
                let ctrl = app.state::<VoiceController>();
                if ctrl.is_enabled() {
                    ctrl.disable();
                    let _ = listen_for_event.set_checked(false);
                } else {
                    ctrl.enable();
                    let _ = listen_for_event.set_checked(true);
                }
            }
            "settings" => {
                if let Err(e) = crate::commands::open_settings(app.clone()) {
                    tracing::warn!("open_settings failed: {}", e);
                }
            }
            "debug" => {
                let _ = app.emit_to("cat", "cat://toggle-debug", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}
