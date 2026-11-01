#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod cleanup;
mod clipboard;
mod context;
mod dictionary;
mod engine;
mod hotkey;
mod icons;
mod learn;
mod logger;
mod model;
mod mute;
mod pill;
mod secret;
mod settings;
mod smart;
mod sounds;
mod spell;
mod storage;
mod stats;
mod transcripts;
mod tray;

use dictionary::Entry;
use engine::{Download, HistoryItem, Msg, Shared, Status};
use pill::{PillEvent, PillState};
use settings::Settings;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::ManagerExt;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UiState {
    settings: Settings,
    status: Status,
    model_installed: bool,
    model_loaded: bool,
    model_bytes: u64,
    download: Download,
    /// The latest few dictations, for Home.
    recent: Vec<HistoryItem>,
    history_total: usize,
    dictionary: Vec<Entry>,
    week: stats::Week,
    transcript_count: usize,
    hotkeys: Vec<(&'static str, &'static str)>,
    version: &'static str,
    /// An API key is saved for Smart cleanup (the key itself never goes to the UI).
    smart_key: bool,
}

const KEY_PAGE: &str = "https://platform.claude.com/settings/keys";

pub fn emit_state(app: &AppHandle) {
    let _ = app.emit("state-changed", ());
}

/// The app window is created on demand and destroyed on close, so no
/// WebView2 processes run while Offhand sits in the tray. `view` is "home",
/// "dictionary" or "settings".
pub fn open_app(app: &AppHandle, view: Option<&str>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        if let Some(view) = view {
            let _ = w.emit("navigate", view);
        }
        return;
    }
    let url = match view {
        Some(v) => format!("index.html#{v}"),
        None => "index.html".to_string(),
    };
    let built = WebviewWindowBuilder::new(app, "main", WebviewUrl::App(url.into()))
        .title("Offhand")
        .inner_size(1100.0, 760.0)
        .min_inner_size(760.0, 540.0)
        .decorations(false)
        .shadow(true)
        .center()
        .build();
    if let Err(e) = built {
        log::error!("could not open the app window: {e}");
    }
}

fn start_download(app: &AppHandle, shared: &Arc<Shared>) {
    {
        let mut d = shared.download.lock().unwrap();
        if d.active {
            return;
        }
        *d = Download { active: true, done: 0, total: model::total_bytes(), error: None };
    }
    let (app, shared) = (app.clone(), shared.clone());
    std::thread::spawn(move || {
        let mut last = Instant::now();
        let result = model::download(|done, total| {
            {
                let mut d = shared.download.lock().unwrap();
                d.done = done;
                d.total = total;
            }
            if last.elapsed() > Duration::from_millis(250) {
                last = Instant::now();
                tray::show_download(&app, done, total);
                let pct = if total > 0 { done * 100 / total } else { 0 };
                pill::set(PillState::Message { text: format!("Setting up {pct}%"), error: false, ttl_ms: None });
                emit_state(&app);
            }
        });
        {
            let mut d = shared.download.lock().unwrap();
            d.active = false;
            d.error = result.as_ref().err().cloned();
        }
        match &result {
            Ok(()) => pill::set(PillState::Message { text: "Ready".into(), error: false, ttl_ms: Some(2000) }),
            Err(e) => {
                log::error!("model download failed: {e}");
                pill::set(PillState::Message { text: "Download stopped".into(), error: true, ttl_ms: None });
            }
        }
        // Replace the progress text; Wake then moves NeedsModel to Idle.
        tray::show_status(&app, *shared.status.lock().unwrap(), &shared.settings());
        let _ = shared.tx.send(Msg::Wake);
        emit_state(&app);
    });
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn to_item(e: transcripts::Entry) -> HistoryItem {
    HistoryItem {
        id: e.id,
        text: e.text().to_string(),
        app: context::display_name(e.app.as_deref()),
        at: e.at,
        audio_secs: e.audio_secs,
        took_ms: e.asr_ms,
        raw: e.raw,
    }
}

/// History from the file when it's kept, else this session's (in memory).
fn history_page(shared: &Shared, query: &str, offset: usize, limit: usize) -> (Vec<HistoryItem>, usize) {
    if shared.settings().keep_transcripts {
        let (items, total) = transcripts::list(query, offset, limit);
        return (items.into_iter().map(to_item).collect(), total);
    }
    let q = query.trim().to_lowercase();
    let all: Vec<HistoryItem> = shared
        .history
        .lock()
        .unwrap()
        .iter()
        .filter(|h| q.is_empty() || h.text.to_lowercase().contains(&q) || h.app.to_lowercase().contains(&q))
        .cloned()
        .collect();
    let total = all.len();
    (all.into_iter().skip(offset).take(limit).collect(), total)
}

fn on_learned(app: &AppHandle, heard: String, word: String) {
    let shared = app.state::<Arc<Shared>>();
    let mut dict = shared.dictionary.write().unwrap();
    if !dictionary::learn(&mut dict, &heard, &word) {
        return;
    }
    if let Err(e) = dictionary::save(&dict) {
        log::warn!("saving dictionary: {e}");
    }
    drop(dict);
    pill::note(format!("Added {word} to Dictionary"));
    emit_state(app);
}

fn save(shared: &Shared, update: impl FnOnce(&mut Settings)) -> Settings {
    let mut s = shared.settings.write().unwrap();
    update(&mut s);
    if let Err(e) = settings::save(&s) {
        log::warn!("saving settings: {e}");
    }
    s.clone()
}

/// Flips hands-free mode from the tray or the pill menu.
pub fn toggle_hands_free(app: &AppHandle) {
    let shared = app.state::<Arc<Shared>>();
    let s = save(&shared, |s| s.hands_free_mode = !s.hands_free_mode);
    pill::set_tap_mode(s.hands_free_mode);
    tray::show_status(app, *shared.status.lock().unwrap(), &s);
    emit_state(app);
}

fn on_pill_event(app: &AppHandle, ev: PillEvent) {
    let shared = app.state::<Arc<Shared>>();
    match ev {
        PillEvent::OpenApp | PillEvent::OpenSettings => {
            let view = matches!(ev, PillEvent::OpenSettings).then_some("settings");
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || open_app(&handle, view));
        }
        PillEvent::CopyLast => {
            let last = shared.history.lock().unwrap().front().map(|h| h.text.clone());
            if let Some(text) = last {
                clipboard::copy(&text);
            }
        }
        PillEvent::ToggleTalkingOnly => {
            let s = save(&shared, |s| s.pill_mode = if s.pill_mode == "talking" { "always".into() } else { "talking".into() });
            pill::set_mode(&s.pill_mode);
            emit_state(app);
        }
        PillEvent::ToggleHandsFree => toggle_hands_free(app),
        PillEvent::StopListening => {
            let _ = shared.tx.send(Msg::StopHandsFree);
        }
        PillEvent::Quit => app.exit(0),
        PillEvent::Moved { x, y } => {
            save(&shared, |s| {
                s.pill_x = Some(x);
                s.pill_y = Some(y);
            });
        }
    }
}

#[tauri::command]
async fn get_state(shared: State<'_, Arc<Shared>>) -> Result<UiState, String> {
    Ok(UiState {
        settings: shared.settings(),
        status: *shared.status.lock().unwrap(),
        model_installed: model::is_installed(),
        model_loaded: shared.model_loaded(),
        model_bytes: model::total_bytes(),
        download: shared.download.lock().unwrap().clone(),
        recent: history_page(&shared, "", 0, 5).0,
        history_total: history_page(&shared, "", 0, 0).1,
        dictionary: shared.dictionary.read().unwrap().clone(),
        week: stats::week(),
        transcript_count: transcripts::count(),
        hotkeys: hotkey::PRESETS.iter().map(|p| (p.0, p.1)).collect(),
        version: env!("CARGO_PKG_VERSION"),
        smart_key: secret::is_set(),
    })
}

#[tauri::command]
async fn save_api_key(app: AppHandle, key: String) -> Result<(), String> {
    secret::set(&key)?;
    emit_state(&app);
    Ok(())
}

#[tauri::command]
async fn remove_api_key(app: AppHandle) -> Result<(), String> {
    secret::delete();
    emit_state(&app);
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SmartTest {
    ms: u64,
    text: String,
}

/// Runs one sample through Smart so Settings can show it working and how long it takes.
#[tauri::command]
async fn test_smart() -> Result<SmartTest, String> {
    let sample = "Let's meet on Tuesday, no wait, Wednesday at three thirty. We have fifty signups.";
    tauri::async_runtime::spawn_blocking(move || {
        let t = Instant::now();
        let text = smart::fix(sample, None, &[])?;
        Ok(SmartTest { ms: t.elapsed().as_millis() as u64, text })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open_key_page() -> Result<(), String> {
    std::process::Command::new("explorer").arg(KEY_PAGE).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_mics() -> Result<Vec<String>, String> {
    // Enumerating devices initialises COM, so keep it off shared runtime threads.
    std::thread::spawn(audio::list_input_devices).join().map_err(|_| "mic enumeration failed".to_string())
}

#[tauri::command]
async fn save_settings(app: AppHandle, shared: State<'_, Arc<Shared>>, settings: Settings) -> Result<(), String> {
    let old = shared.settings();
    if old.start_with_windows != settings.start_with_windows {
        let autolaunch = app.autolaunch();
        let r = if settings.start_with_windows { autolaunch.enable() } else { autolaunch.disable() };
        r.map_err(|e| e.to_string())?;
    }
    // The pill owns its position; a stale copy from the UI must not move it.
    let s = save(&shared, |s| {
        *s = Settings { pill_x: old.pill_x, pill_y: old.pill_y, ..settings };
    });
    if old.hotkey != s.hotkey {
        hotkey::set_preset(&s.hotkey);
        pill::set_label(hotkey::label_for(&s.hotkey));
    }
    if old.pill_mode != s.pill_mode {
        pill::set_mode(&s.pill_mode);
    }
    if old.hands_free_mode != s.hands_free_mode {
        pill::set_tap_mode(s.hands_free_mode);
    }
    if old.history_days != s.history_days {
        transcripts::maintain(s.history_days, now_secs());
    }
    if old.learn_corrections && !s.learn_corrections {
        learn::stop();
    }
    tray::show_status(&app, *shared.status.lock().unwrap(), &s);
    let _ = shared.tx.send(Msg::Wake);
    emit_state(&app);
    Ok(())
}

#[tauri::command]
async fn save_dictionary(app: AppHandle, shared: State<'_, Arc<Shared>>, entries: Vec<Entry>) -> Result<(), String> {
    let entries = dictionary::normalize(entries);
    dictionary::save(&entries).map_err(|e| e.to_string())?;
    *shared.dictionary.write().unwrap() = entries;
    emit_state(&app);
    Ok(())
}

#[tauri::command]
async fn reset_pill(shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    save(&shared, |s| {
        s.pill_x = None;
        s.pill_y = None;
    });
    pill::reset_position();
    Ok(())
}

#[tauri::command]
async fn mic_test(shared: State<'_, Arc<Shared>>, on: bool) -> Result<(), String> {
    audio::UI_LEVEL.store(0, Ordering::Relaxed);
    shared.tx.send(Msg::MicTest(on)).map_err(|e| e.to_string())
}

/// Loudest level since the last call, 0..1 on a dB scale.
#[tauri::command]
async fn mic_level() -> Result<f32, String> {
    let peak = f32::from_bits(audio::UI_LEVEL.swap(0, Ordering::Relaxed));
    let db = 20.0 * peak.max(1e-5).log10();
    Ok(((db + 50.0) / 42.0).clamp(0.0, 1.0))
}

#[tauri::command]
async fn download_model(app: AppHandle, shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    start_download(&app, &shared);
    Ok(())
}

#[derive(serde::Serialize)]
struct HistoryPage {
    items: Vec<HistoryItem>,
    total: usize,
}

#[tauri::command]
async fn get_history(shared: State<'_, Arc<Shared>>, query: String, offset: usize, limit: usize) -> Result<HistoryPage, String> {
    let (items, total) = history_page(&shared, &query, offset, limit.min(500));
    Ok(HistoryPage { items, total })
}

#[tauri::command]
async fn delete_history(app: AppHandle, shared: State<'_, Arc<Shared>>, id: u64) -> Result<(), String> {
    shared.history.lock().unwrap().retain(|h| h.id != id);
    transcripts::delete(id).map_err(|e| e.to_string())?;
    emit_state(&app);
    Ok(())
}

#[tauri::command]
async fn clear_transcripts(app: AppHandle, shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    shared.history.lock().unwrap().clear();
    transcripts::clear().map_err(|e| e.to_string())?;
    emit_state(&app);
    Ok(())
}

#[tauri::command]
async fn show_transcripts() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let path = transcripts::path();
    let mut cmd = std::process::Command::new("explorer");
    if path.exists() {
        // Explorer wants the quotes after the comma, so build the arg by hand.
        cmd.raw_arg(format!("/select,\"{}\"", path.display()));
    } else {
        cmd.arg(settings::data_dir());
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn copy_text(text: String) -> Result<(), String> {
    if clipboard::copy(&text) {
        Ok(())
    } else {
        Err("clipboard is busy".into())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.as_slice() {
        [_, flag, file] if flag == "--bench" => return engine::bench(file),
        [_, flag, file] if flag == "--bench-cleanup" => return cleanup::bench(file, false),
        [_, flag, file, opt] if flag == "--bench-cleanup" && opt == "--smart" => return cleanup::bench(file, true),
        _ => {}
    }
    // Started from inside another app's package (the Claude desktop app, say),
    // everything would be saved in that app's private folder. Restart outside.
    if args.len() == 1 && storage::relaunch_if_packaged() {
        return;
    }
    let stuck = storage::adopt_old_name();
    logger::init();
    log::info!("Offhand {} starting", env!("CARGO_PKG_VERSION"));
    for problem in stuck {
        log::warn!("moving data from the old name: {problem}");
    }
    #[cfg(not(debug_assertions))]
    if tauri::is_dev() {
        log::error!("built with plain cargo, so the app window has no pages; use scripts/release-local.ps1");
    }

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if args.iter().any(|a| a == "--pill-demo") {
                pill::demo();
            } else {
                open_app(app, None);
            }
        }))
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .invoke_handler(tauri::generate_handler![
            get_state,
            list_mics,
            save_settings,
            save_dictionary,
            reset_pill,
            mic_test,
            mic_level,
            get_history,
            delete_history,
            download_model,
            clear_transcripts,
            show_transcripts,
            copy_text,
            save_api_key,
            remove_api_key,
            test_smart,
            open_key_page
        ])
        .setup(|app| {
            storage::rescue();
            let handle = app.handle().clone();
            let first_run = settings::is_first_run();
            let s = settings::load();
            if first_run {
                let _ = settings::save(&s);
            }
            let stale_entry = storage::start_entry_is_stale();
            if s.start_with_windows && stale_entry {
                let _ = handle.autolaunch().enable();
            }

            transcripts::maintain(s.history_days, now_secs());
            let (tx, rx) = mpsc::channel();
            let shared = Arc::new(Shared::new(s.clone(), dictionary::load(), tx.clone()));
            app.manage(shared.clone());
            tray::build(&handle, &s)?;
            hotkey::start(tx, &s.hotkey);
            {
                let (handle, shared) = (handle.clone(), shared.clone());
                std::thread::Builder::new()
                    .name("engine".into())
                    .spawn(move || engine::run(handle, shared, rx))?;
            }
            {
                let events = handle.clone();
                let pos = s.pill_x.zip(s.pill_y);
                pill::start(&s.pill_mode, hotkey::label_for(&s.hotkey), s.hands_free_mode, pos, Box::new(move |ev| on_pill_event(&events, ev)));
            }
            {
                let learner = handle.clone();
                learn::start(Box::new(move |heard, word| on_learned(&learner, heard, word)));
            }
            if !model::is_installed() {
                start_download(&handle, &shared);
                open_app(&handle, None);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build app");

    app.run(|_app, event| {
        // Closing the app window must not quit the tray app.
        if let RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
}
