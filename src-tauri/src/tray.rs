use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::engine::Status;
use crate::icons::{self, Icon};
use crate::settings::Settings;

const TRAY_ID: &str = "main";

struct StatusItem(MenuItem<Wry>);
struct HandsFreeItem(CheckMenuItem<Wry>);

fn hotkey_label(settings: &Settings) -> &'static str {
    crate::hotkey::label_for(&settings.hotkey)
}

fn status_text(status: Status, settings: &Settings) -> String {
    match status {
        Status::Idle if settings.hands_free_mode => format!("Tap {} to start and stop", hotkey_label(settings)),
        Status::Idle => format!("Hold {} to dictate", hotkey_label(settings)),
        Status::Recording => "Listening…".into(),
        Status::Transcribing => "Transcribing…".into(),
        Status::NeedsModel => "Speech model not downloaded yet".into(),
    }
}

pub fn build(app: &AppHandle, settings: &Settings) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", status_text(Status::Idle, settings), false, None::<&str>)?;
    let copy_last = MenuItem::with_id(app, "copy-last", "Copy last transcript", true, None::<&str>)?;
    let hands_free = CheckMenuItem::with_id(app, "hands-free", "Hands-free mode", true, settings.hands_free_mode, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Offhand", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Offhand", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &hands_free,
            &copy_last,
            &open,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    app.manage(StatusItem(status));
    app.manage(HandsFreeItem(hands_free));

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icons::render(Icon::Idle))
        .tooltip("Offhand")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "copy-last" => {
                let shared = app.state::<std::sync::Arc<crate::engine::Shared>>();
                let last = shared.history.lock().unwrap().front().map(|h| h.text.clone());
                if let Some(text) = last {
                    crate::clipboard::copy(&text);
                }
            }
            "open" => crate::open_app(app, None),
            "hands-free" => crate::toggle_hands_free(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                crate::open_app(tray.app_handle(), None);
            }
        })
        .build(app)?;
    Ok(())
}

pub fn show_status(app: &AppHandle, status: Status, settings: &Settings) {
    let icon = match status {
        Status::Idle => Icon::Idle,
        Status::Recording => Icon::Recording,
        Status::Transcribing => Icon::Transcribing,
        Status::NeedsModel => Icon::Unavailable,
    };
    let text = status_text(status, settings);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(icons::render(icon)));
        let _ = tray.set_tooltip(Some(format!("Offhand: {text}")));
    }
    if let Some(item) = app.try_state::<StatusItem>() {
        let _ = item.0.set_text(text);
    }
    if let Some(item) = app.try_state::<HandsFreeItem>() {
        let _ = item.0.set_checked(settings.hands_free_mode);
    }
}

pub fn show_download(app: &AppHandle, done: u64, total: u64) {
    let pct = if total > 0 { done * 100 / total } else { 0 };
    let text = format!("Downloading speech model… {pct}%");
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(format!("Offhand: {text}")));
    }
    if let Some(item) = app.try_state::<StatusItem>() {
        let _ = item.0.set_text(text);
    }
}
