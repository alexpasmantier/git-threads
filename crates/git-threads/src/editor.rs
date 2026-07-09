//! Message entry via the user's editor, mirroring `git commit` without `-m`.

use anyhow::{Context, Result, bail};
use std::process::Command;

/// Open the user's editor on a seeded `.git/THREADS_EDITMSG` file and return
/// what they wrote. The editor is resolved exactly like git's (`GIT_EDITOR`,
/// `core.editor`, `VISUAL`, `EDITOR`, then the default) via `git var`.
/// Lines starting with `#` are stripped; an empty result aborts.
pub fn message(repo: &gix::Repository, seed: &str, hint: &str) -> Result<String> {
    let editor = git_editor()?;
    let path = repo.git_dir().join("THREADS_EDITMSG");

    let mut contents = String::from(seed);
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push('\n');
    for line in hint.lines() {
        contents.push_str("# ");
        contents.push_str(line);
        contents.push('\n');
    }
    std::fs::write(&path, &contents)?;

    // Same invocation shape git uses: the editor value is a shell fragment.
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch editor {editor:?}"))?;
    if !status.success() {
        bail!("editor {editor:?} exited with {status}");
    }

    let text = std::fs::read_to_string(&path)?;
    let message =
        text.lines().filter(|line| !line.starts_with('#')).collect::<Vec<_>>().join("\n");
    let message = message.trim();
    if message.is_empty() {
        bail!("aborting due to empty message");
    }
    Ok(message.to_string())
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
