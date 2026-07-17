//! Text rendering for the CLI: the git-log-style blocks `list`, `show`, and
//! `status` print, built from the typed views the command layer computes.
//! `--json` bypasses all of this — it serializes the same views directly, so
//! nothing here can drift from what machine consumers see.

use crate::commands::{self, SnippetMode};
use crate::reanchor::Reanchor;
use crate::store::Store;
use crate::ui::{self, Ui, short};
use crate::view::{AnchorContext, StatusView, ThreadView};
use anyhow::Result;
use git_threads_core::{EventKind, LineRange, ReanchorStatus, Side, SnippetTarget, derive_snippet};
use std::fmt::Write;

/// The full thread as `show` prints it: header, location history (Original /
/// Moved / Current), code context, conversation.
pub fn thread(ui: Ui, store: &Store, view: &ThreadView, mode: SnippetMode) -> Result<String> {
    let anchor = &view.anchor;
    let target_short = &view.at.as_str()[..12];
    let mut out = String::new();

    // Placement (outdated, drift) lives in the field lines below; the
    // decorations carry thread state only.
    let mut deco = vec![if view.resolved { ui.magenta("resolved") } else { ui.green("open") }];
    if view.moved_to.is_some() {
        deco.push(ui.yellow("moved"));
    }
    writeln!(out, "{} {}", ui.yellow(format_args!("thread {}", view.id)), decorate(ui, &deco))
        .unwrap();
    let side = match anchor.side {
        Some(Side::Old) => ui.dim(" (old side)"),
        _ => String::new(),
    };
    // The full story, one line per chapter: what the author anchored to,
    // where a human re-pinned it (if anyone did), and where the code sits at
    // --at. Unlike list's single current-first line, nothing is suppressed —
    // an explicit exact `Current:` is the confirmation it's still there.
    writeln!(
        out,
        "Original: {}{side} {}",
        ui.bold(location(anchor, false)),
        ui.dim(format_args!(
            "of {}..{}",
            &anchor.diff.base.as_str()[..12],
            &anchor.diff.head.as_str()[..12]
        ))
    )
    .unwrap();

    if let (Some(moved_to), Some(moved_by)) = (&view.moved_to, &view.moved_by) {
        writeln!(
            out,
            "Moved:    {} {}",
            ui.bold(location(moved_to, false)),
            ui.dim(format_args!("at {} by {}", &moved_to.diff.head.as_str()[..12], moved_by.name))
        )
        .unwrap();
    }

    match &view.placement {
        Reanchor::WholeCommit => {}
        Reanchor::Located { path, lines, status } => {
            let exact = matches!(status, ReanchorStatus::Exact);
            let lines = lines.map(|l| format!(":{}-{}", l.start, l.end)).unwrap_or_default();
            let status = format!("({status})");
            let status = if exact { ui.green(status) } else { ui.yellow(status) };
            writeln!(
                out,
                "Current:  {} at {target_short} {status}",
                ui.bold(format_args!("{path}{lines}"))
            )
            .unwrap();
        }
        Reanchor::Outdated => {
            writeln!(out, "Current:  {} at {target_short}", ui.red("no match")).unwrap();
        }
    }

    if let Some(ctx) = commands::anchor_context(store, view, mode)? {
        out.push_str(&context(ui, &ctx));
    }
    out.push_str(&conversation(ui, view));
    Ok(out)
}

/// One `list` entry: the git-log-style block, or the compact `--oneline`
/// form. No leading separator — the caller spaces the blocks.
pub fn list_entry(
    ui: Ui,
    store: &Store,
    view: &ThreadView,
    oneline: bool,
    snippet_mode: Option<SnippetMode>,
) -> Result<String> {
    let mut out = String::new();
    let mut deco = vec![if view.resolved { ui.magenta("resolved") } else { ui.green("open") }];
    if view.moved_to.is_some() {
        deco.push(ui.yellow("moved"));
    }
    if view.messages.len() > 1 {
        deco.push(ui.dim(format_args!("{} messages", view.messages.len())));
    }
    let drafts = view.messages.iter().filter(|m| m.draft).count();
    if drafts > 0 {
        deco.push(ui.yellow(format_args!("{drafts} draft{}", if drafts == 1 { "" } else { "s" })));
    }
    let news = view.messages.iter().filter(|m| m.new).count();
    if news > 0 {
        deco.push(ui.cyan(format_args!("{news} new")));
    }
    let decoration = decorate(ui, &deco);
    let root = view.root();
    // One location for the scanning eye: where the thread is at --at.
    // Approximate placements carry their status; when nothing matches, the
    // original anchor stands, marked outdated. The full original vs current
    // story is show's job.
    let (place, on_diff, note) = match &view.placement {
        Reanchor::WholeCommit => (location(&view.anchor, oneline), true, String::new()),
        Reanchor::Located { path, lines, status } => {
            let lines = lines.map(|l| format!(":{}-{}", l.start, l.end)).unwrap_or_default();
            let note = match status {
                ReanchorStatus::Exact => String::new(),
                status => format!(" {}", ui.yellow(format_args!("({status})"))),
            };
            (format!("{path}{lines}"), false, note)
        }
        Reanchor::Outdated => {
            (location(&view.anchor, oneline), true, format!(" {}", ui.red("(outdated)")))
        }
    };
    // Only lines that couldn't be re-located need their diff spelled out.
    let diff = if on_diff {
        format!(
            " {}",
            ui.dim(format_args!(
                "of {}..{}",
                &view.anchor.diff.base.as_str()[..12],
                &view.anchor.diff.head.as_str()[..12]
            ))
        )
    } else {
        String::new()
    };
    if oneline {
        let title = root.and_then(|r| r.body.as_deref()).unwrap_or("").lines().next().unwrap_or("");
        writeln!(out, "{} {decoration} {place}{note}  {title}", ui.yellow(short(&view.id)))
            .unwrap();
    } else {
        writeln!(out, "{} {decoration}", ui.yellow(format_args!("thread {}", view.id))).unwrap();
        if let Some(root) = root {
            writeln!(
                out,
                "Author: {} {}",
                ui.bold(&root.author.name),
                ui.dim(format_args!("<{}>", root.author.email))
            )
            .unwrap();
            writeln!(out, "Date:   {}", ui::date(&root.ts)).unwrap();
        }
        writeln!(out, "Anchor: {}{diff}{note}", ui.bold(&place)).unwrap();
        let body = match root {
            Some(root) if root.retracted => ui.dim("[retracted]"),
            Some(root) => root.body.clone().unwrap_or_default(),
            None => String::new(),
        };
        if !body.is_empty() {
            out.push('\n');
            for line in body.lines() {
                writeln!(out, "    {line}").unwrap();
            }
        }
    }
    if let Some(mode) = snippet_mode
        && let Some(ctx) = commands::anchor_context(store, view, mode)?
    {
        out.push_str(&context(ui, &ctx));
    }
    Ok(out)
}

/// An anchor's location the way every view spells it: `path:start-end`,
/// `path`, or — for whole changes — `commit <head>` in compact contexts and
/// "whole change" in field lines.
fn location(anchor: &git_threads_core::Anchor, compact: bool) -> String {
    match (&anchor.path, &anchor.lines) {
        (Some(path), Some(lines)) => format!("{path}:{}-{}", lines.start, lines.end),
        (Some(path), None) => path.clone(),
        _ if compact => format!("commit {}", &anchor.diff.head.as_str()[..12]),
        _ => "whole change".to_string(),
    }
}

/// The conversation as `show` prints it: one block per message, blank-line
/// separated, starting with a blank line.
fn conversation(ui: Ui, view: &ThreadView) -> String {
    let mut out = String::new();
    for message in &view.messages {
        let kind = if message.kind == EventKind::Reply { "reply" } else { "comment" };
        let edited =
            if message.edited { format!(" {}", ui.dim("(edited)")) } else { String::new() };
        let draft = if message.draft {
            format!(" {}", ui.yellow("(draft)"))
        } else if message.new {
            format!(" {}", ui.cyan("(new)"))
        } else {
            String::new()
        };
        writeln!(
            out,
            "\n{}  {} {}  {}{edited}{draft}",
            ui.yellow(format_args!("{kind:<7} {}", short(&message.id))),
            ui.bold(&message.author.name),
            ui.dim(format_args!("<{}>", message.author.email)),
            ui.dim(ui::date(&message.ts)),
        )
        .unwrap();
        if message.retracted {
            writeln!(out, "    {}", ui.dim("[retracted]")).unwrap();
        } else if let Some(body) = &message.body {
            for line in body.lines() {
                writeln!(out, "    {line}").unwrap();
            }
        }
    }
    out
}

/// The `status` report: drafted events, unpushed counts per remote, new
/// activity.
pub fn status(ui: Ui, view: &StatusView) -> String {
    let mut out = String::new();
    let events = view.drafted_events();
    if events == 0 {
        writeln!(out, "nothing drafted").unwrap();
    } else {
        writeln!(
            out,
            "{} drafted event{} in {} thread{} {}:",
            events,
            if events == 1 { "" } else { "s" },
            view.drafted.len(),
            if view.drafted.len() == 1 { "" } else { "s" },
            ui.dim("(git threads commit to seal, discard to drop)")
        )
        .unwrap();
        for drafts in &view.drafted {
            for event in &drafts.events {
                // A drafted root gets the anchor location; later events point
                // at their thread, the way the conversation view labels them.
                let context = if event.id == drafts.thread {
                    location(&drafts.anchor, true)
                } else {
                    format!("thread {}", short(&drafts.thread))
                };
                let kind = String::from(event.kind.clone());
                write!(
                    out,
                    "  {}  {context}",
                    ui.yellow(format_args!("{kind:<7} {}", short(&event.id)))
                )
                .unwrap();
                if let Some(first) = event.body.as_deref().and_then(|b| b.lines().next()) {
                    write!(out, "  {}", ui.dim(first)).unwrap();
                }
                out.push('\n');
            }
        }
    }

    for remote in &view.remotes {
        if remote.unpushed == 0 {
            writeln!(out, "up to date with {}", remote.remote).unwrap();
        } else {
            writeln!(
                out,
                "{} event{} not yet on {} {}",
                remote.unpushed,
                if remote.unpushed == 1 { "" } else { "s" },
                remote.remote,
                ui.dim("(git threads push to share)")
            )
            .unwrap();
        }
    }

    if view.threads_with_news > 0 {
        writeln!(
            out,
            "{} thread{} with new activity {}",
            view.threads_with_news,
            if view.threads_with_news == 1 { "" } else { "s" },
            ui.dim("(git threads list --new)")
        )
        .unwrap();
    }

    if view.repins > 0 {
        writeln!(
            out,
            "{} thread{} stranded by a rewrite can re-pin here {}",
            view.repins,
            if view.repins == 1 { "" } else { "s" },
            ui.dim("(git threads move --orphans)")
        )
        .unwrap();
    }
    out
}

/// A thread's code context, colorized. Each form renders preceded by a blank
/// line, so it drops straight under the header fields.
pub fn context(ui: Ui, context: &AnchorContext) -> String {
    match context {
        AnchorContext::Stat(text) => stat(ui, text),
        AnchorContext::Diff { text, side, lines, headers, clip } => {
            diff(ui, text, *side, *lines, *headers, *clip)
        }
        AnchorContext::Excerpt { content, lines } => excerpt(ui, content, *lines),
    }
}

/// Render `git diff --stat` output the way git log --stat does: insertion
/// marks green, deletion marks red.
fn stat(ui: Ui, stat: &str) -> String {
    if stat.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n");
    for line in stat.lines() {
        match line.rsplit_once('|') {
            Some((left, graph)) => {
                let graph = graph.replace('+', &ui.green("+")).replace('-', &ui.red("-"));
                writeln!(out, " {left}|{graph}").unwrap();
            }
            None => writeln!(out, " {line}").unwrap(),
        }
    }
    out
}

/// Render a unified diff the way git colors it, marking the anchored
/// `lines` on `side` in the gutter. With `clip`, only hunks overlapping the
/// anchored lines are kept (all of them when `lines` is `None`). File
/// headers are shown only for whole-change diffs (`headers`), where they
/// separate files.
fn diff(
    ui: Ui,
    diff: &str,
    side: Side,
    lines: Option<LineRange>,
    headers: bool,
    clip: bool,
) -> String {
    let mut out = String::new();
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let mut in_hunk = false;
    let mut wanted = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            let mut fields = line.split_whitespace().skip(1);
            let span = |token: Option<&str>, sign: char| -> (u32, u32) {
                let Some(token) = token.and_then(|t| t.strip_prefix(sign)) else { return (0, 0) };
                match token.split_once(',') {
                    Some((start, len)) => (start.parse().unwrap_or(0), len.parse().unwrap_or(0)),
                    None => (token.parse().unwrap_or(0), 1),
                }
            };
            let old = span(fields.next(), '-');
            let new = span(fields.next(), '+');
            (old_no, new_no) = (old.0, new.0);
            let (start, len) = match side {
                Side::Old => old,
                Side::New => new,
            };
            wanted = !clip
                || lines.is_none_or(|want| {
                    let end = start + len.max(1) - 1;
                    want.start <= end && want.end >= start
                });
            if wanted {
                writeln!(out, "  {}", ui.cyan(line)).unwrap();
            }
            continue;
        }
        if !in_hunk || !line.starts_with(['+', '-', ' ', '\\']) {
            in_hunk = false;
            if headers {
                writeln!(out, "  {}", ui.bold(line)).unwrap();
            }
            continue;
        }
        let on = |no: u32| lines.is_some_and(|l| l.start <= no && no <= l.end);
        let (marked, rendered) = match line.as_bytes()[0] {
            b'-' => {
                let marked = side == Side::Old && on(old_no);
                old_no += 1;
                (marked, ui.red(line))
            }
            b'+' => {
                let marked = side == Side::New && on(new_no);
                new_no += 1;
                (marked, ui.green(line))
            }
            b' ' => {
                let marked = on(match side {
                    Side::Old => old_no,
                    Side::New => new_no,
                });
                old_no += 1;
                new_no += 1;
                (marked, ui.dim(line))
            }
            _ => (false, ui.dim(line)), // "\ No newline at end of file"
        };
        if wanted {
            let mark = if marked { ui.cyan(">") } else { " ".to_string() };
            writeln!(out, "{mark} {rendered}").unwrap();
        }
    }
    if out.is_empty() { out } else { format!("\n{out}") }
}

/// A marked, line-numbered excerpt of `lines` out of `content`, preceded by
/// a blank line. Context lines are dimmed so the target lines carry the eye.
fn excerpt(ui: Ui, content: &str, lines: LineRange) -> String {
    let Some(snippet) = derive_snippet(content, lines) else {
        return String::new();
    };
    let mut out = String::from("\n");
    let push_line = |out: &mut String, line_no: &mut u32, line: &str, marked: bool| {
        let gutter = ui.dim(format_args!("{line_no:>5} │"));
        if marked {
            writeln!(out, "{} {gutter} {line}", ui.cyan(">")).unwrap();
        } else {
            writeln!(out, "  {gutter} {}", ui.dim(line)).unwrap();
        }
        *line_no += 1;
    };
    let mut line_no = snippet.first_line;
    for line in &snippet.before {
        push_line(&mut out, &mut line_no, line, false);
    }
    match &snippet.target {
        SnippetTarget::Full(lines) => {
            for line in lines {
                push_line(&mut out, &mut line_no, line, true);
            }
        }
        SnippetTarget::Truncated { head, tail, omitted, .. } => {
            for line in head {
                push_line(&mut out, &mut line_no, line, true);
            }
            writeln!(out, "        {}", ui.dim(format_args!("⋮ {omitted} lines omitted"))).unwrap();
            line_no += *omitted as u32;
            for line in tail {
                push_line(&mut out, &mut line_no, line, true);
            }
        }
    }
    for line in &snippet.after {
        push_line(&mut out, &mut line_no, line, false);
    }
    out
}

/// A git-log-style decoration list — `(open, 2 messages, 1 draft)` — with
/// dim punctuation around already-colored parts.
fn decorate(ui: Ui, parts: &[String]) -> String {
    format!("{}{}{}", ui.dim("("), parts.join(&ui.dim(", ")), ui.dim(")"))
}
