//! A tiny logger: warnings and errors to stderr and to
//! `%LOCALAPPDATA%\Glotzkiste\glotzkiste.log` (truncated at start).

use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Option<Mutex<std::fs::File>>,
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("{:5} {}: {}\n", r.level(), r.target(), r.args());
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(f) = &self.file
            && let Ok(mut f) = f.lock()
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

pub fn init(verbose: bool) {
    let file = gk_twitch::local_dir().and_then(|d| std::fs::File::create(d.join("glotzkiste.log")).ok()).map(Mutex::new);
    let level = if verbose { log::LevelFilter::Info } else { log::LevelFilter::Warn };
    if log::set_boxed_logger(Box::new(Logger { file, level })).is_ok() {
        log::set_max_level(level);
    }
}
