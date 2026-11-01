//! Keeps Offhand's data where it belongs. Windows runs anything started by a
//! packaged app (Microsoft Store or MSIX, like the Claude desktop app)
//! inside that app's private file system, so what Offhand saved to AppData
//! landed in %LOCALAPPDATA%\Packages\<app>\LocalCache instead, and
//! vanished from view the next time Offhand started normally. Two guards:
//! - `relaunch_if_packaged`: started from inside another app's package,
//!   Offhand restarts itself through Explorer, outside it.
//! - `rescue`: data an earlier run left in a package's private folder is
//!   moved back to the real AppData and merged with anything saved since.
//!
//! Offhand used to be called Sotto, and `adopt_old_name` moves what was saved
//! under that name.

use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, APPMODEL_ERROR_NO_PACKAGE, ERROR_SUCCESS};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

/// The app's name before it became Offhand.
const OLD_NAME: &str = "Sotto";
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

/// Moves everything saved under the old name to the new one: history, stats,
/// the speech model, settings, the Dictionary, the API key and the app
/// window's data. Runs first thing at startup, before anything is opened.
/// Returns what couldn't be moved, for the log, which isn't open yet.
pub fn adopt_old_name() -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        let data = crate::settings::data_dir();
        problems.extend(move_contents(&local.join(OLD_NAME), &data));
        problems.extend(move_contents(&local.join("tech.shazil.sotto"), &local.join("tech.shazil.offhand")));
        let (old_log, log) = (data.join("sotto.log"), data.join("offhand.log"));
        if old_log.is_file() && !log.exists() {
            let _ = std::fs::rename(old_log, log);
        }
    }
    if let Some(roaming) = std::env::var_os("APPDATA").map(PathBuf::from) {
        problems.extend(move_contents(&roaming.join(OLD_NAME), &crate::settings::config_dir()));
    }
    crate::secret::adopt_old_name();
    problems
}

/// Moves what's in `from` into `to`, never over anything already there,
/// then removes `from` if that emptied it.
fn move_contents(from: &Path, to: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(from) else { return vec![] };
    let _ = std::fs::create_dir_all(to);
    let mut problems = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let dest = to.join(&name);
        if dest.exists() {
            problems.push(format!("kept {}: already at the new name", entry.path().display()));
        } else if let Err(e) = std::fs::rename(entry.path(), dest) {
            problems.push(format!("couldn't move {}: {e}", entry.path().display()));
        }
    }
    let _ = std::fs::remove_dir(from);
    problems
}

/// The value of a start-with-Windows entry.
fn run_value(name: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let found = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, name, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
    };
    (found == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
}

/// True when the start-with-Windows entry has to be written: it's missing
/// (a new install, or an upgrade took it), still has the old name (removed
/// here), or points at an exe that has moved.
pub fn start_entry_is_stale() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = exe.to_string_lossy().to_lowercase();
    let old = run_value(w!("Sotto")).is_some();
    if old {
        let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Sotto")) };
    }
    old || run_value(w!("Offhand")).is_none_or(|v| !v.to_lowercase().contains(&exe))
}

/// The package this process runs inside, if any. Offhand has none of its own.
fn package_name() -> Option<String> {
    unsafe {
        let mut len = 0u32;
        if GetCurrentPackageFullName(&mut len, None) == APPMODEL_ERROR_NO_PACKAGE {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let _ = GetCurrentPackageFullName(&mut len, Some(PWSTR(buf.as_mut_ptr())));
        Some(String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]))
    }
}

/// The package whose private folder this process's AppData writes land in.
/// Windows doesn't always give a child of a packaged app that package's
/// identity, so asking isn't enough: a probe file is written and looked for
/// inside each package's private folder.
fn redirected_to() -> Option<String> {
    let data = crate::settings::data_dir();
    let probe = format!(".where-{}", std::process::id());
    std::fs::create_dir_all(&data).ok()?;
    std::fs::write(data.join(&probe), b"").ok()?;
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let found = std::fs::read_dir(local.join("Packages"))
        .ok()?
        .flatten()
        .find(|p| p.path().join("LocalCache").join("Local").join("Offhand").join(&probe).exists())
        .map(|p| p.file_name().to_string_lossy().into_owned());
    let _ = std::fs::remove_file(data.join(&probe));
    found
}

/// Running inside some app's package, by identity or by redirected files.
fn boxed_in() -> Option<String> {
    package_name().or_else(redirected_to)
}

/// Exe name of the process that started this one, lowercased.
fn parent_exe() -> Option<String> {
    let me = std::process::id();
    let mut procs = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let name = String::from_utf16_lossy(&entry.szExeFile).trim_end_matches('\0').to_lowercase();
            procs.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    let parent = procs.iter().find(|p| p.0 == me)?.1;
    procs.into_iter().find(|p| p.0 == parent).map(|p| p.2)
}

/// When started from inside another app's package, asks Explorer to start a
/// clean copy and returns true; the caller then exits. A copy that Explorer
/// started never relaunches again, so this can't loop.
pub fn relaunch_if_packaged() -> bool {
    if parent_exe().as_deref() == Some("explorer.exe") || boxed_in().is_none() {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let windir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    Command::new(windir.join("explorer.exe")).arg(exe).spawn().is_ok()
}

/// Moves data an earlier run left inside any package's private folder back
/// to the real AppData. Runs at startup, before anything is loaded.
pub fn rescue() {
    if let Some(pkg) = boxed_in() {
        log::warn!("running inside the app package {pkg}; data goes to its private folder until Offhand is started normally");
        return;
    }
    let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return };
    let Ok(packages) = std::fs::read_dir(local.join("Packages")) else { return };
    for pkg in packages.flatten() {
        let base = pkg.path().join("LocalCache");
        let name = pkg.file_name().to_string_lossy().into_owned();
        for folder in ["Offhand", OLD_NAME] {
            let local_dir = base.join("Local").join(folder);
            if local_dir.is_dir() {
                rescue_local(&local_dir, &name);
                retire(&local_dir);
            }
            let roaming_dir = base.join("Roaming").join(folder);
            if roaming_dir.is_dir() {
                rescue_roaming(&roaming_dir, &name);
                retire(&roaming_dir);
            }
        }
    }
}

/// History, stats and the speech model.
fn rescue_local(from: &Path, pkg: &str) {
    let to = crate::settings::data_dir();
    let _ = std::fs::create_dir_all(&to);
    let history = from.join("transcripts.jsonl");
    if history.is_file() {
        match crate::transcripts::merge_from(&history) {
            Ok(n) => log::info!("moved {n} dictations back from {pkg}'s private folder"),
            Err(e) => log::warn!("moving history back from {pkg}: {e}"),
        }
    }
    let stats = from.join("stats.json");
    if stats.is_file() {
        crate::stats::merge_from(&stats);
    }
    if crate::model::is_complete(from) && !crate::model::is_installed() {
        // Same volume, so this is a rename, not a 640 MB copy. A partial
        // download in the real folder is replaced.
        let _ = std::fs::remove_dir_all(to.join("models"));
        match std::fs::rename(from.join("models"), to.join("models")) {
            Ok(()) => log::info!("moved the speech model back from {pkg}'s private folder"),
            Err(e) => log::warn!("moving the speech model back from {pkg}: {e}"),
        }
    }
}

/// Settings and the Dictionary.
fn rescue_roaming(from: &Path, pkg: &str) {
    let to = crate::settings::config_dir();
    let _ = std::fs::create_dir_all(&to);
    if let Some(theirs) = crate::settings::load_from(&from.join("settings.json")) {
        // Keep settings changed since; replace ones a fresh start left at
        // their defaults (where the pill sits doesn't count).
        let untouched = crate::settings::load_from(&crate::settings::settings_path())
            .is_none_or(|s| crate::settings::Settings { pill_x: None, pill_y: None, ..s } == Default::default());
        if untouched {
            match crate::settings::save(&theirs) {
                Ok(()) => log::info!("moved settings back from {pkg}'s private folder"),
                Err(e) => log::warn!("moving settings back from {pkg}: {e}"),
            }
        }
    }
    let theirs = crate::dictionary::load_from(&from.join("dictionary.json"));
    if !theirs.is_empty() {
        let mut entries = crate::dictionary::load();
        crate::dictionary::merge(&mut entries, theirs);
        match crate::dictionary::save(&entries) {
            Ok(()) => log::info!("moved the dictionary back from {pkg}'s private folder"),
            Err(e) => log::warn!("moving the dictionary back from {pkg}: {e}"),
        }
    }
}

/// Renames a rescued folder so it's never merged twice. Nothing is deleted.
fn retire(dir: &Path) {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if let Err(e) = std::fs::rename(dir, dir.with_file_name(format!("{name}.moved-{stamp}"))) {
        log::warn!("couldn't retire {}: {e}", dir.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_data_moves_without_replacing_anything() {
        let root = std::env::temp_dir().join(format!("offhand-move-{}", std::process::id()));
        let (old, new) = (root.join("old"), root.join("new"));
        std::fs::create_dir_all(old.join("models")).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("models").join("model.onnx"), "model").unwrap();
        std::fs::write(old.join("dictionary.json"), "old words").unwrap();
        std::fs::write(old.join("settings.json"), "old settings").unwrap();
        std::fs::write(new.join("settings.json"), "newer settings").unwrap();

        let problems = move_contents(&old, &new);

        assert_eq!(std::fs::read_to_string(new.join("models").join("model.onnx")).unwrap(), "model");
        assert_eq!(std::fs::read_to_string(new.join("dictionary.json")).unwrap(), "old words");
        // Something already at the new name stays, and the old copy is kept.
        assert_eq!(std::fs::read_to_string(new.join("settings.json")).unwrap(), "newer settings");
        assert_eq!(std::fs::read_to_string(old.join("settings.json")).unwrap(), "old settings");
        assert_eq!(problems.len(), 1);

        std::fs::remove_file(old.join("settings.json")).unwrap();
        assert!(move_contents(&old, &new).is_empty());
        assert!(!old.exists());
        assert!(move_contents(&old, &new).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
