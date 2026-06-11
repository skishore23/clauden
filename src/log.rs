//! Minimal log sink. Defaults to stderr; can be redirected to a file so that
//! when clauden launches Claude Code (whose TUI owns the terminal) the proxy's
//! own diagnostics don't scribble over the interface.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

enum Sink {
    Stderr,
    File(std::fs::File),
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

fn sink() -> &'static Mutex<Sink> {
    SINK.get_or_init(|| Mutex::new(Sink::Stderr))
}

/// Redirect all subsequent log lines to `path` (truncating it). Best-effort:
/// falls back to stderr if the file can't be opened.
pub fn to_file(path: &Path) {
    if let Ok(file) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        if let Some(m) = SINK.get() {
            *m.lock().unwrap() = Sink::File(file);
        } else {
            let _ = SINK.set(Mutex::new(Sink::File(file)));
        }
    }
}

/// Write one log line to the active sink.
pub fn line(msg: &str) {
    let mut guard = sink().lock().unwrap();
    match &mut *guard {
        Sink::Stderr => eprintln!("{msg}"),
        Sink::File(f) => {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }
}

/// Format + write a log line. `log!("x {}", y)`.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}
