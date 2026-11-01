//! Dictation history kept on this PC: one JSON line per dictation in
//! %LOCALAPPDATA%\Offhand\transcripts.jsonl. Powers the History page, and is
//! the same format `--bench-cleanup` reads for tuning. Never uploaded; the
//! user can turn it off, set how long to keep it, or delete it.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Serialises file rewrites (delete, prune) against appends.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Unix milliseconds when it was saved; unique per entry.
    #[serde(default)]
    pub id: u64,
    pub at: u64,
    pub app: Option<String>,
    pub audio_secs: f32,
    pub asr_ms: u64,
    /// Time spent on the Smart pass, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_ms: Option<u64>,
    pub raw: String,
    /// Present only when cleanup changed the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned: Option<String>,
}

impl Entry {
    pub fn text(&self) -> &str {
        self.cleaned.as_deref().unwrap_or(&self.raw)
    }
}

pub fn path() -> PathBuf {
    crate::settings::data_dir().join("transcripts.jsonl")
}

fn read_all() -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path()) else { return vec![] };
    text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn write_all(entries: &[Entry]) -> std::io::Result<()> {
    let mut out = String::with_capacity(entries.len() * 200);
    for e in entries {
        if let Ok(line) = serde_json::to_string(e) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    let tmp = path().with_extension("jsonl.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(tmp, path())
}

pub fn append(entry: &Entry) {
    let _guard = LOCK.lock().unwrap();
    let line = match serde_json::to_string(entry) {
        Ok(l) => l,
        Err(e) => return log::warn!("history: {e}"),
    };
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path());
    if let Err(e) = file.and_then(|mut f| writeln!(f, "{line}")) {
        log::warn!("history: {e}");
    }
}

/// Newest first, filtered by `query` (text or app), paged.
pub fn list(query: &str, offset: usize, limit: usize) -> (Vec<Entry>, usize) {
    let q = query.trim().to_lowercase();
    let mut all: Vec<Entry> = read_all()
        .into_iter()
        .filter(|e| {
            q.is_empty()
                || e.text().to_lowercase().contains(&q)
                || crate::context::display_name(e.app.as_deref()).to_lowercase().contains(&q)
        })
        .collect();
    all.reverse();
    let total = all.len();
    (all.into_iter().skip(offset).take(limit).collect(), total)
}

pub fn count() -> usize {
    std::fs::read_to_string(path()).map(|s| s.lines().filter(|l| !l.trim().is_empty()).count()).unwrap_or(0)
}

pub fn delete(id: u64) -> std::io::Result<()> {
    let _guard = LOCK.lock().unwrap();
    let mut all = read_all();
    let before = all.len();
    all.retain(|e| e.id != id);
    if all.len() != before {
        write_all(&all)?;
    }
    Ok(())
}

pub fn clear() -> std::io::Result<()> {
    let _guard = LOCK.lock().unwrap();
    match std::fs::remove_file(path()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Adds the entries from another history file (data moved back from
/// elsewhere), skipping ids already here, and keeps the file in time order.
pub fn merge_from(other: &std::path::Path) -> std::io::Result<usize> {
    let _guard = LOCK.lock().unwrap();
    let text = std::fs::read_to_string(other)?;
    let mut all = read_all();
    let ids: std::collections::HashSet<u64> = all.iter().map(|e| e.id).collect();
    let extra: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| e.id == 0 || !ids.contains(&e.id))
        .collect();
    let n = extra.len();
    if n > 0 {
        all.extend(extra);
        all.sort_by_key(|e| (e.at, e.id));
        std::fs::create_dir_all(crate::settings::data_dir())?;
        write_all(&all)?;
    }
    Ok(n)
}

/// Drops entries older than `days` (negative keeps everything) and gives
/// older entries written before ids existed a stable id.
pub fn maintain(days: i32, now_secs: u64) {
    let _guard = LOCK.lock().unwrap();
    let mut all = read_all();
    let mut changed = false;
    for (i, e) in all.iter_mut().enumerate() {
        if e.id == 0 {
            e.id = e.at * 1000 + (i as u64 % 1000);
            changed = true;
        }
    }
    if days >= 0 {
        let cutoff = now_secs.saturating_sub(days as u64 * 86_400);
        let before = all.len();
        all.retain(|e| e.at >= cutoff);
        changed |= all.len() != before;
    }
    if changed {
        if let Err(e) = write_all(&all) {
            log::warn!("history upkeep: {e}");
        }
    }
}
