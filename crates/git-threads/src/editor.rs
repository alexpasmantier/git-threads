//! Message entry via the user's editor, mirroring `git commit` without `-m`.

use anyhow::{Context, Result, bail};
use std::process::Command;

/// Open the user's editor on a seeded `.git/THREADS_EDITMSG` file and return
/// what they wrote. The editor is resolved exactly like git's (`GIT_EDITOR`,
/// `core.editor`, `VISUAL`, `EDITOR`, then the default) via `git var`.
/// Lines starting with `#` are stripped; an empty result exits quietly
/// without drafting anything, the way `git commit` aborts.
pub fn message(repo: &gix::Repository, seed: &str, hint: &str) -> Result<String> {
    let editor = git_editor()?;
    let path = repo.git_dir().join("THREADS_EDITMSG");

    let mut contents = String::from(seed);
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push('\n');
    for line in hint.lines() {
        if line.is_empty() {
            contents.push_str("#\n");
        } else {
            contents.push_str("# ");
            contents.push_str(line);
            contents.push('\n');
        }
    }
    std::fs::write(&path, &contents)?;

    let status = spawn_editor(&editor, &path)
        .with_context(|| format!("failed to launch editor {editor:?}"))?;
    if !status.success() {
        bail!("editor {editor:?} exited with {status}");
    }

    let text = std::fs::read_to_string(&path)?;
    let message = text.lines().filter(|line| !line.starts_with('#')).collect::<Vec<_>>().join("\n");
    let message = message.trim();
    if message.is_empty() {
        eprintln!("Aborting due to empty message.");
        std::process::exit(1);
    }
    Ok(message.to_string())
}

/// Run the editor on `path`. The editor value is a shell fragment (git's
/// convention), so it goes through a shell: `sh` where there is one, `cmd`
/// on Windows.
#[cfg(unix)]
fn spawn_editor(editor: &str, path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    // Same invocation shape git uses.
    Command::new("sh").arg("-c").arg(format!("{editor} \"$@\"")).arg(editor).arg(path).status()
}

#[cfg(windows)]
fn spawn_editor(editor: &str, path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    use std::os::windows::process::CommandExt;
    // cmd re-parses the raw line itself; quote only the file argument.
    Command::new("cmd").arg("/C").raw_arg(format!("{editor} \"{}\"", path.display())).status()
}

fn git_editor() -> Result<String> {
    let out = Command::new("git")
        .args(["var", "GIT_EDITOR"])
        .output()
        .context("failed to run git var GIT_EDITOR")?;
    if !out.status.success() {
        bail!("git var GIT_EDITOR failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}
