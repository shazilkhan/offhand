use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// One of the presets in `hotkey::PRESETS`.
    pub hotkey: String,
    /// Input device name. `None` follows the Windows default mic.
    pub mic: Option<String>,
    /// Mute other apps' audio while the hotkey is held, so speakers don't
    /// bleed into the mic.
    pub mute_while_dictating: bool,
    pub sounds: bool,
    pub trailing_space: bool,
    /// Minutes to keep the model in memory after the last dictation.
    /// 0 unloads right away, -1 keeps it loaded.
    pub keep_model_mins: i32,
    pub start_with_windows: bool,
    /// Keep dictation history on this PC (a local JSONL file). Off keeps
    /// only this session's dictations, in memory.
    pub keep_transcripts: bool,
    /// Days of history to keep; -1 keeps everything.
    pub history_days: i32,
    /// Tap the shortcut to start and tap again to finish, instead of
    /// holding it. Holding still works for a quick burst.
    pub hands_free_mode: bool,
    /// Learn spellings from edits made right after a dictation is pasted.
    pub learn_corrections: bool,
    /// "always", "windowed" (not over fullscreen apps unless dictating),
    /// "talking" (only while dictating) or "never".
    pub pill_mode: String,
    /// Where the pill's underline sits, in screen pixels. `None` is bottom
    /// centre of the primary screen. Owned by the pill; the UI never sets it.
    pub pill_x: Option<i32>,
    pub pill_y: Option<i32>,
    /// "off", "light", or "smart" (Light plus an AI pass; needs an API key,
    /// see smart.rs).
    pub cleanup: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "ctrl+win".into(),
            mic: None,
            mute_while_dictating: true,
            sounds: true,
            trailing_space: true,
            keep_model_mins: 15,
            start_with_windows: true,
            keep_transcripts: true,
            history_days: -1,
            hands_free_mode: false,
            learn_corrections: true,
            pill_mode: "always".into(),
            pill_x: None,
            pill_y: None,
            cleanup: "light".into(),
        }
    }
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Offhand")
}

/// Roaming: settings follow the user.
pub fn config_dir() -> PathBuf {
    env_dir("APPDATA")
}

/// Local: models, logs and crash markers stay on this machine.
pub fn data_dir() -> PathBuf {
    env_dir("LOCALAPPDATA")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn is_first_run() -> bool {
    !settings_path().exists()
}

pub fn load() -> Settings {
    load_from(&settings_path()).unwrap_or_default()
}

pub fn load_from(path: &std::path::Path) -> Option<Settings> {
    std::fs::read_to_string(path)
        .ok()
        // Editors like Notepad and PowerShell 5 may add a byte-order mark.
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    let json = serde_json::to_string_pretty(settings).expect("settings serialize");
    std::fs::write(settings_path(), json)
}
