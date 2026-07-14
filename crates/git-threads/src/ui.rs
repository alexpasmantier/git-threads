//! Terminal styling: ANSI colors when stdout is a terminal, plain text
//! everywhere else (pipes, NO_COLOR, TERM=dumb), the way git decides
//! `color.ui=auto`.

use git_threads_core::Timestamp;
use std::fmt::Display;
use std::io::IsTerminal;

/// Carries the one decision — color or not — to every formatting site.
#[derive(Clone, Copy)]
pub struct Ui {
    color: bool,
}

impl Ui {
    /// Color iff stdout is a terminal that wants it. Decided once per
    /// process, so the pager can lock it in before replacing stdout with
    /// its pipe.
    pub fn auto() -> Self {
        static COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let color = *COLOR.get_or_init(|| {
            std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
        });
        Ui { color }
    }

    /// Never color: for text that ends up in files (editor previews).
    pub fn plain() -> Self {
        Ui { color: false }
    }

    fn paint(&self, sgr: &str, text: impl Display) -> String {
        if self.color { format!("\x1b[{sgr}m{text}\x1b[m") } else { text.to_string() }
    }

    pub fn bold(&self, text: impl Display) -> String {
        self.paint("1", text)
    }

    pub fn dim(&self, text: impl Display) -> String {
        self.paint("2", text)
    }

    pub fn red(&self, text: impl Display) -> String {
        self.paint("31", text)
    }

    pub fn green(&self, text: impl Display) -> String {
        self.paint("32", text)
    }

    pub fn yellow(&self, text: impl Display) -> String {
        self.paint("33", text)
    }

    pub fn magenta(&self, text: impl Display) -> String {
        self.paint("35", text)
    }

    pub fn cyan(&self, text: impl Display) -> String {
        self.paint("36", text)
    }
}

/// Columns available on the terminal, decided once per process so the pager
/// can lock it in before replacing stdout with its pipe. `None` when stdout
/// isn't a terminal: piped output should not be wrapped.
pub fn text_width() -> Option<usize> {
    static WIDTH: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *WIDTH.get_or_init(|| {
        if !std::io::stdout().is_terminal() {
            return None;
        }
        #[cfg(unix)]
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return Some(ws.ws_col as usize);
            }
        }
        Some(80)
    })
}

/// Wrap one line of prose to `width` columns at word boundaries. Leading
/// whitespace is kept and repeated on continuation lines, so indented text
/// (bullets, quoted code) stays visually grouped. Lines that fit — and
/// single words that don't — pass through untouched.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let mut lines = Vec::new();
    let mut current = indent.clone();
    let mut empty = true;
    for word in line.split_whitespace() {
        let sep = if empty { 0 } else { 1 };
        if !empty && current.chars().count() + sep + word.chars().count() > width {
            lines.push(std::mem::replace(&mut current, indent.clone()));
            empty = true;
        }
        if !empty {
            current.push(' ');
        }
        current.push_str(word);
        empty = false;
    }
    lines.push(current);
    lines
}

/// Git's log date format ("Sun Jul 13 21:49:00 2026 +0000"). Threads
/// timestamps are stored in UTC, so the offset is always +0000.
pub fn date(ts: &Timestamp) -> String {
    match ts.as_str().parse::<jiff::Timestamp>() {
        Ok(t) => t.strftime("%a %b %-d %H:%M:%S %Y +0000").to_string(),
        Err(_) => ts.to_string(),
    }
}
