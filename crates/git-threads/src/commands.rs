use crate::store::Store;
use anyhow::{Context, Result, bail};
use git_threads_core::fold_thread;
use std::process::Command;

const FETCH_REFSPEC: &str = "+refs/threads/*:refs/threads/*";

/// Configure this clone (SPEC.md §7.1): add the additive fetch refspec, then
/// attempt an initial fetch. No push refspec is written — publishing pushes
/// explicitly to avoid replacing git's default push behavior.
pub fn init(remote: &str) -> Result<()> {
    Store::discover()?;
    let remotes = git(&["remote"])?;
    if !remotes.lines().any(|line| line == remote) {
        bail!("remote {remote:?} not found (git remote add it first, or pass --remote)");
    }
    let key = format!("remote.{remote}.fetch");
    let existing = git_ok(&["config", "--get-all", &key]);
    if existing.lines().any(|line| line == FETCH_REFSPEC) {
        println!("{key} already includes {FETCH_REFSPEC}");
    } else {
        git(&["config", "--add", &key, FETCH_REFSPEC])?;
        println!("configured {key} += {FETCH_REFSPEC}");
    }
    match git(&["fetch", remote]) {
        Ok(_) => println!("fetched from {remote}"),
        Err(err) => eprintln!("warning: initial fetch from {remote} failed: {err:#}"),
    }
    Ok(())
}

/// List threads in the current snapshot with their folded state.
pub fn list() -> Result<()> {
    let store = Store::discover()?;
    let mut threads = store.threads()?;
    if threads.is_empty() {
        println!("no threads");
        return Ok(());
    }
    // Newest first, by root timestamp.
    threads.sort_by_key(|t| {
        std::cmp::Reverse(t.events.iter().map(|(_, e)| e.ts.clone()).min())
    });
    for thread in threads {
        let folded = fold_thread(thread.events.clone());
        let status = if folded.resolved { "resolved" } else { "open" };
        let location = match (&thread.anchor.path, &thread.anchor.lines) {
            (Some(path), Some(lines)) => format!("{path}:{}-{}", lines.start, lines.end),
            (Some(path), None) => path.clone(),
            _ => format!("commit {}", &thread.anchor.diff.head.as_str()[..12]),
        };
        let title = folded
            .events
            .first()
            .and_then(|root| root.effective_body.as_deref())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!(
            "{}  [{status}] {location}  ({} message{})  {title}",
            &thread.id.as_str()[..12],
            folded.events.len(),
            if folded.events.len() == 1 { "" } else { "s" },
        );
    }
    Ok(())
}

fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Like [`git`] but treats failure as empty output (e.g. `config --get-all`
/// exits non-zero when the key is unset).
fn git_ok(args: &[&str]) -> String {
    git(args).unwrap_or_default()
}
