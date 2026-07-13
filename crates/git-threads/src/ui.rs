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

/// A human date: relative while recent ("20 minutes ago"), the plain date
/// once relative stops being meaningful.
pub fn when(ts: &Timestamp) -> String {
    let Ok(then) = ts.as_str().parse::<jiff::Timestamp>() else {
        return ts.to_string();
    };
    let secs = jiff::Timestamp::now().duration_since(then).as_secs();
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        plural(secs / 60, "minute")
    } else if secs < 86_400 {
        plural(secs / 3_600, "hour")
    } else if secs < 604_800 {
        plural(secs / 86_400, "day")
    } else {
        // The ISO form is `YYYY-MM-DDThh:mm:ssZ`; the date is its first 10 bytes.
        ts.as_str()[..10].to_string()
    }
}
