// Companion — a desktop pet that lives on the Windows taskbar.

mod api;
mod commands;
mod context;
mod events;
mod icloud;
mod mail;
mod paths;
mod platform;
mod reminders;
mod storage;
pub mod voice;
mod windows;

use std::sync::Arc;
use tauri::Manager;
use tokio::sync::Mutex;

pub struct AppState {
    pub history: Arc<Mutex<storage::history::History>>,
}

pub fn run() {
    storage::secrets::load_env_file();
    platform::install_activity_hooks();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,companion=debug")),
        )
        .init();

    tauri::Builder::default()
        .setup(|app| {
            let app_dir = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data dir");
            std::fs::create_dir_all(&app_dir).ok();
            let db_path = app_dir.join("companion.sqlite");

            let history = storage::history::History::open(&db_path)?;
            app.manage(AppState {
                history: Arc::new(Mutex::new(history)),
            });

            windows::cat::setup(app)?;

            // Voice wake-word listener. Wrap in a controller so the tray
            // menu can toggle it. Must be managed BEFORE tray::setup so
            // the menu handler can look it up via `app.state()`.
            let voice_ctrl = voice::controller::VoiceController::new(app.handle().clone());
            voice_ctrl.enable();
            app.manage(voice_ctrl);

            windows::tray::setup(app)?;
            reminders::spawn(app.handle().clone());

            // CPU heat indicator — Fibi gets a glowing hot plate under
            // her when the system gets warm/hot.
            platform::cpu::spawn(app.handle().clone());

            // Eagerly auth + warm CloudKit indices for iCloud Reminders so
            // the first chat query about reminders doesn't pay the
            // ~30-60s "Indexing scheduled" tax.
            icloud::bridge::spawn_prewarm(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::set_sprite_rect,
            commands::send_message,
            commands::list_messages,
            commands::clear_history,
            commands::get_api_key_status,
            commands::open_settings,
            commands::icloud_auth_login,
            commands::icloud_auth_submit_2fa,
            commands::icloud_auth_username,
            commands::get_model,
            commands::set_model,
            commands::get_sleep_position_x,
            commands::get_idle_seconds,
            commands::get_runtime_context,
            commands::get_calendar_sources,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Companion");
}
