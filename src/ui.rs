//! Terminal UI helpers: TTY-aware ANSI colors, width-correct padding, and a
//! compact quota bar. No external dependencies — just escape codes.

use std::io::IsTerminal;
use std::sync::OnceLock;

static COLOR: OnceLock<bool> = OnceLock::new();

/// Whether to emit ANSI color (stdout is a TTY and NO_COLOR is unset).
pub fn color_enabled() -> bool {
    *COLOR.get_or_init(|| std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err())
}

fn wrap(code: &str, s: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    wrap("1", s)
}
pub fn dim(s: &str) -> String {
    wrap("2", s)
}
pub fn green(s: &str) -> String {
    wrap("32", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33", s)
}
pub fn red(s: &str) -> String {
    wrap("31", s)
}
pub fn cyan(s: &str) -> String {
    wrap("36", s)
}
pub fn magenta(s: &str) -> String {
    wrap("35", s)
}

/// Visible length of a string, ignoring ANSI escape sequences.
pub fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until the terminating 'm' of the SGR sequence.
            for n in chars.by_ref() {
                if n == 'm' {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Pad `s` on the right to `width` visible columns.
pub fn pad_end(s: &str, width: usize) -> String {
    let v = visible_len(s);
    if v >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - v))
    }
}

/// Fit a plain string into exactly `width` columns: truncate with an ellipsis
/// if too long, pad on the right if too short. (Assumes no ANSI codes.)
pub fn fit(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return pad_end(s, width);
    }
    if width == 0 {
        return String::new();
    }
    let truncated: String = s.chars().take(width - 1).collect();
    format!("{truncated}…")
}

/// Pad `s` on the left to `width` visible columns.
pub fn pad_start(s: &str, width: usize) -> String {
    let v = visible_len(s);
    if v >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - v))
    }
}

/// A 10-cell utilization bar, colored by severity. `util` is 0.0–1.0.
pub fn quota_bar(util: Option<f64>) -> String {
    match util {
        None => dim("░░░░░░░░░░   —"),
        Some(u) => {
            let u = u.clamp(0.0, 1.0);
            let filled = (u * 10.0).round() as usize;
            let bar: String = (0..10)
                .map(|i| if i < filled { '▰' } else { '▱' })
                .collect();
            let pct = format!("{:>3.0}%", u * 100.0);
            let colored = if u >= 0.95 {
                red(&bar)
            } else if u >= 0.70 {
                yellow(&bar)
            } else {
                green(&bar)
            };
            format!("{colored} {pct}")
        }
    }
}

/// A colored status dot + label.
pub fn status_dot(label: &str, level: Status) -> String {
    let dot = match level {
        Status::Ready => green("●"),
        Status::Warn => yellow("●"),
        Status::Down => red("●"),
    };
    format!("{dot} {label}")
}

#[derive(Clone, Copy)]
pub enum Status {
    Ready,
    Warn,
    Down,
}
