use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

struct FileLogger {
    file: Mutex<File>,
    started: Instant,
}

impl Log for FileLogger {
    fn enabled(&self, meta: &Metadata) -> bool {
        // Our own crate logs at info; dependencies (ort, tauri) only at warn.
        meta.level() <= Level::Warn || meta.target().starts_with("offhand")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{:>9.3}s] {:<5} {}\n",
            self.started.elapsed().as_secs_f64(),
            record.level(),
            record.args()
        );
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
        #[cfg(debug_assertions)]
        eprint!("{line}");
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

pub fn init() {
    let dir = crate::settings::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("offhand.log");
    let too_big = std::fs::metadata(&path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
    let file = OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(&path);
    let Ok(file) = file else { return };
    let logger = FileLogger { file: Mutex::new(file), started: Instant::now() };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}
