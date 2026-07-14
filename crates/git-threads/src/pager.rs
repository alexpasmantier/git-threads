//! Automatic pager for reading commands, mirroring `git log`: the pager is
//! resolved like git's (`git var GIT_PAGER`, honoring GIT_PAGER, core.pager,
//! PAGER), started only when stdout is a terminal, and skipped for `cat`.
//! `git --no-pager threads ...` works because git exports GIT_PAGER=cat.

use std::process::{Child, Command, Stdio};

/// While alive, stdout is redirected into the user's pager; dropping it
/// restores stdout and waits for the pager to exit.
pub struct Pager {
    #[cfg(unix)]
    active: Option<(Child, i32)>,
}

impl Pager {
    pub fn start() -> Self {
        #[cfg(unix)]
        {
            Pager { active: start_unix() }
        }
        #[cfg(not(unix))]
        {
            Pager {}
        }
    }
}

#[cfg(unix)]
fn start_unix() -> Option<(Child, i32)> {
    use std::io::IsTerminal;
    use std::os::unix::io::AsRawFd;

    if !std::io::stdout().is_terminal() {
        return None;
    }
    // Lock in the color decision while stdout still is the terminal: what
    // follows replaces it with a pipe, and less -R renders the colors.
    let _ = crate::ui::Ui::auto();
    let pager = pager_command()?;

    let mut command = Command::new("sh");
    command.arg("-c").arg(&pager).stdin(Stdio::piped());
    // Same defaults git gives less and lv when the user set nothing.
    if std::env::var_os("LESS").is_none() {
        command.env("LESS", "FRX");
    }
    if std::env::var_os("LV").is_none() {
        command.env("LV", "-c");
    }
    let mut child = command.spawn().ok()?;
    let stdin = child.stdin.take().expect("stdin was piped");
    let saved = unsafe { libc::dup(1) };
    if saved < 0 {
        return None;
    }
    unsafe { libc::dup2(stdin.as_raw_fd(), 1) };
    // `stdin` drops here, leaving fd 1 as the only write end of the pipe.
    Some((child, saved))
}

#[cfg(unix)]
impl Drop for Pager {
    fn drop(&mut self) {
        if let Some((mut child, saved)) = self.active.take() {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            // Point stdout back at the terminal; that closes the pipe's last
            // write end, so the pager sees EOF and can finish.
            unsafe {
                libc::dup2(saved, 1);
                libc::close(saved);
            }
            let _ = child.wait();
        }
    }
}

/// The pager to run, or `None` when paging is off (empty or `cat`).
#[cfg(unix)]
fn pager_command() -> Option<String> {
    let out = Command::new("git").args(["var", "GIT_PAGER"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let pager = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if pager.is_empty() || pager == "cat" { None } else { Some(pager) }
}
