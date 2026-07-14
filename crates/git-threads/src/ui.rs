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
    /// Color iff stdout is a terminal that wants it.
    pub fn auto() -> Self {
        let color = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
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

/// Git's log date format ("Sun Jul 13 21:49:00 2026 +0000"). Threads
/// timestamps are stored in UTC, so the offset is always +0000.
pub fn date(ts: &Timestamp) -> String {
    match ts.as_str().parse::<jiff::Timestamp>() {
        Ok(t) => t.strftime("%a %b %-d %H:%M:%S %Y +0000").to_string(),
        Err(_) => ts.to_string(),
    }
}
