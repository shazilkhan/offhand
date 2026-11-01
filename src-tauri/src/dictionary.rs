//! The user's dictionary: words Offhand should always spell their way, plus
//! what speech recognition sometimes writes instead. Stored next to
//! settings.json.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub word: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Added automatically from a correction the user made.
    #[serde(default)]
    pub learned: bool,
}

fn path() -> PathBuf {
    crate::settings::config_dir().join("dictionary.json")
}

pub fn load() -> Vec<Entry> {
    load_from(&path())
}

pub fn load_from(path: &std::path::Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .ok()
        // Editors like Notepad and PowerShell 5 may add a byte-order mark.
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

/// Trims, drops empty words and aliases, and drops aliases equal to the word.
pub fn normalize(entries: Vec<Entry>) -> Vec<Entry> {
    entries
        .into_iter()
        .filter_map(|e| {
            let word = e.word.trim().to_string();
            if word.is_empty() {
                return None;
            }
            let aliases = e
                .aliases
                .iter()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case(&word))
                .collect();
            Some(Entry { word, aliases, learned: e.learned })
        })
        .collect()
}

/// Adds `heard` as a spelling of `word`, creating the word if it's new.
/// Returns false when the dictionary already knew it.
pub fn learn(entries: &mut Vec<Entry>, heard: &str, word: &str) -> bool {
    // A capitals fix ("github" -> "GitHub") needs only the spelling: the word
    // itself already matches any casing.
    if heard.eq_ignore_ascii_case(word) {
        if let Some(e) = entries.iter_mut().find(|e| e.word.eq_ignore_ascii_case(word)) {
            if e.word == word {
                return false;
            }
            e.word = word.to_string();
            return true;
        }
        entries.insert(0, Entry { word: word.to_string(), aliases: vec![], learned: true });
        return true;
    }
    if let Some(e) = entries.iter_mut().find(|e| e.word.eq_ignore_ascii_case(word)) {
        if e.aliases.iter().any(|a| a.eq_ignore_ascii_case(heard)) {
            return false;
        }
        e.aliases.push(heard.to_string());
        return true;
    }
    entries.insert(0, Entry { word: word.to_string(), aliases: vec![heard.to_string()], learned: true });
    true
}

/// Adds another dictionary's words and spellings to this one.
pub fn merge(entries: &mut Vec<Entry>, other: Vec<Entry>) {
    for o in other {
        match entries.iter_mut().find(|e| e.word.eq_ignore_ascii_case(&o.word)) {
            Some(e) => {
                for a in o.aliases {
                    if !e.aliases.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
                        e.aliases.push(a);
                    }
                }
            }
            None => entries.push(o),
        }
    }
}

pub fn save(entries: &[Entry]) -> std::io::Result<()> {
    std::fs::create_dir_all(crate::settings::config_dir())?;
    std::fs::write(path(), serde_json::to_string_pretty(entries).expect("dictionary serialize"))
}
