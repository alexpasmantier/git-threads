use clap::{Parser, Subcommand, ValueEnum};
use git_threads::commands::{self, CommentOpts, Discarded, ListOpts, PushOutcome, SnippetMode};
use git_threads::editor;
use git_threads::render;
use git_threads::store::Store;
use git_threads::ui::{Ui, short};
use git_threads_core::Side;

const COMMENT_HINT: &str = "Enter your message. Lines starting with '#' will be ignored,\n\
    and an empty message aborts the operation.";
const EDIT_HINT: &str = "Edit the text above. Lines starting with '#' will be ignored,\n\
    and an empty message aborts the operation.";

#[derive(Parser)]
#[command(name = "git-threads", version, about = "Anchored, threaded discussions stored in git")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Configure this clone for git-threads (fetch refspec, initial fetch)
    Init {
        /// Remote to configure
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Remove git-threads from this clone: configuration and all local threads data
    Deinit {
        /// Proceed even when drafts or unpushed threads data would be lost
        #[arg(long)]
        force: bool,
    },
    /// Start a new thread on a change: a whole diff, one file of it, or a line range
    Comment {
        /// Comment text; opens your editor if omitted
        #[arg(short, long)]
        message: Option<String>,
        /// What to comment on: a commit, a range like main..topic or main...topic,
        /// or a file / file:lines of HEAD's change [default: HEAD]
        target: Option<String>,
        /// File within the target diff; may carry the lines directly, e.g. src/lib.rs:120-128
        file: Option<String>,
        /// Which version of the file the lines refer to
        #[arg(long, value_enum, default_value_t = SideArg::New)]
        side: SideArg,
    },
    /// Reply to a thread, or to a specific message in one
    Reply {
        /// Thread ID, or the ID of the comment/reply being answered (or a unique prefix)
        thread: String,
        /// Reply text; opens your editor if omitted
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Edit a comment or reply (appends an edit event; history is preserved)
    Edit {
        /// Event ID (or unique prefix) of the comment or reply, as shown by `show`
        event: String,
        /// Replacement text; opens your editor on the current text if omitted
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Retract a comment or reply (appends a tombstone; content stays in history)
    Delete {
        /// Event ID (or unique prefix) of the comment or reply, as shown by `show`
        event: String,
    },
    /// Re-pin a thread to where its code lives now (for outdated threads)
    Move {
        /// Thread ID or the ID of any message in it (or a unique prefix)
        #[arg(required_unless_present = "orphans")]
        thread: Option<String>,
        /// New location: a file at the pin commit, optionally with lines (src/lib.rs:120-128)
        #[arg(required_unless_present = "orphans")]
        file: Option<String>,
        /// Commit to pin at
        #[arg(long, default_value = "HEAD")]
        at: String,
        /// Re-pin every orphaned thread whose code is found verbatim at --at
        #[arg(long, conflicts_with_all = ["thread", "file"])]
        orphans: bool,
    },
    /// Mark a thread resolved
    Resolve {
        /// Thread ID or the ID of any message in it (or a unique prefix)
        thread: String,
        /// Reopen instead of resolving
        #[arg(long)]
        reopen: bool,
    },
    /// Show a thread: anchor context and conversation
    Show {
        /// Thread ID or the ID of any message in it (or a unique prefix)
        thread: String,
        /// Commit to re-anchor the thread against
        #[arg(long, default_value = "HEAD")]
        at: String,
        /// Show the anchored change's full patch, as `git log -p` does
        #[arg(short, long)]
        patch: bool,
        /// Show the anchored change's diffstat, as `git log --stat` does
        #[arg(long, conflicts_with = "patch")]
        stat: bool,
        /// Machine-readable output: one JSON object
        #[arg(long, conflicts_with_all = ["patch", "stat"])]
        json: bool,
    },
    /// Discard drafted events before they're published
    Discard {
        /// Event ID (or unique prefix) of a draft; a draft thread's root discards the whole thread
        event: Option<String>,
        /// Discard every draft
        #[arg(long, conflicts_with = "event")]
        all: bool,
    },
    /// Show what's drafted and what hasn't been pushed
    Status,
    /// Mark a thread seen without opening it — or everything, with no argument
    Seen {
        /// Thread ID or the ID of any message in it (or a unique prefix)
        thread: Option<String>,
        /// Rewind the latest seen mark instead (one step back)
        #[arg(long, conflicts_with = "thread")]
        undo: bool,
    },
    /// Import review discussions from a forge
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
    /// Export local threads onto a forge review (SPEC.md §8.2)
    Export {
        #[command(subcommand)]
        source: ExportSource,
    },
    /// Fetch and integrate threads data from a remote
    Pull {
        /// Remote to pull from
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Seal all drafted events into local threads history as one commit
    Commit,
    /// Push local threads history to a remote (integrating remote state first)
    Push {
        /// Remote to push to
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// List threads with their current state
    List {
        /// Only threads on this change (a commit, or a range like main..topic or
        /// main...topic) — or, alone, on this path across all changes
        target: Option<String>,
        /// Only threads on this file or directory; a file may carry lines, e.g. src/lib.rs:120-128
        file: Option<String>,
        /// Commit to re-anchor threads against
        #[arg(long, default_value = "HEAD")]
        at: String,
        /// Only unresolved threads
        #[arg(long)]
        open: bool,
        /// Only resolved threads
        #[arg(long, conflicts_with = "open")]
        resolved: bool,
        /// Only threads with messages you haven't seen yet
        #[arg(long)]
        new: bool,
        /// One line per thread instead of the full block
        #[arg(long)]
        oneline: bool,
        /// Show each thread's anchored change: its hunks, or a diffstat for whole changes
        #[arg(short, long)]
        patch: bool,
        /// Show each thread's diffstat, as `git log --stat` does
        #[arg(long, conflicts_with = "patch")]
        stat: bool,
        /// Machine-readable output: one JSON array of thread objects
        #[arg(long, conflicts_with_all = ["oneline", "patch", "stat"])]
        json: bool,
        /// Limit the number of threads shown
        #[arg(short = 'n', long = "max-count", value_name = "NUMBER")]
        max_count: Option<usize>,
        /// Only threads imported from this pull/merge request (--mr works too)
        #[arg(long, alias = "mr", value_name = "NUMBER")]
        pr: Option<u64>,
        /// Only threads whose root author matches (substring of name or email)
        #[arg(long)]
        author: Option<String>,
        /// Only threads where some message contains this text (case-insensitive)
        #[arg(long, value_name = "TEXT")]
        grep: Option<String>,
        /// Only threads started after this date (ISO like 2026-07-01, or "2 weeks ago")
        #[arg(long)]
        since: Option<String>,
        /// Only threads started before this date
        #[arg(long)]
        until: Option<String>,
    },
    /// Generate man pages into a directory (for packaging)
    #[command(hide = true)]
    Mangen {
        /// Directory to write the pages into
        #[arg(default_value = ".")]
        out: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
enum ImportSource {
    /// Import a pull request's review threads from GitHub (via the gh CLI)
    Github {
        /// PR number or full PR URL; omit with --all
        pr: Option<String>,
        /// Import every PR of the repository, open and closed
        #[arg(long, conflicts_with = "pr")]
        all: bool,
        /// Remote naming the GitHub repository (and serving the commits)
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Import a merge request's diff discussions from GitLab (via the glab CLI)
    Gitlab {
        /// MR number (also !N) or full MR URL; omit with --all
        mr: Option<String>,
        /// Import every MR of the project, open and closed
        #[arg(long, conflicts_with = "mr")]
        all: bool,
        /// Remote naming the GitLab project (and serving the commits)
        #[arg(long, default_value = "origin")]
        remote: String,
    },
}

#[derive(Subcommand)]
enum ExportSource {
    /// Post this change's threads onto a GitHub pull request (via the gh CLI)
    Github {
        /// PR number or full PR URL
        pr: String,
        /// Show what would be posted, without posting
        #[arg(long)]
        dry_run: bool,
        /// Remote naming the GitHub repository (and serving the commits)
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Post this change's threads onto a GitLab merge request (via the glab CLI)
    Gitlab {
        /// MR number (also !N) or full MR URL
        mr: String,
        /// Show what would be posted, without posting
        #[arg(long)]
        dry_run: bool,
        /// Remote naming the GitLab project (and serving the commits)
        #[arg(long, default_value = "origin")]
        remote: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SideArg {
    Old,
    New,
}

impl From<SideArg> for Side {
    fn from(side: SideArg) -> Side {
        match side {
            SideArg::Old => Side::Old,
            SideArg::New => Side::New,
        }
    }
}

/// Restore SIGPIPE's default disposition (Rust's runtime ignores it, making
/// println! panic when a downstream pipe like `head` closes early). With the
/// default, the process exits quietly like any other CLI tool.
fn reset_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() -> anyhow::Result<()> {
    reset_sigpipe();
    let command = Cli::parse().command;
    if let Command::Mangen { out } = command {
        use clap::CommandFactory;
        std::fs::create_dir_all(&out)?;
        clap_mangen::generate_to(Cli::command(), &out)?;
        return Ok(());
    }
    let store = Store::discover()?;
    // Fold in anything a plain `git fetch` brought since the last command.
    if let Err(err) = commands::integrate_fetched(&store) {
        eprintln!("warning: could not integrate fetched threads data: {err:#}");
    }
    // Reading commands page like git log; the pager winds down on drop.
    let _pager = matches!(command, Command::List { .. } | Command::Show { .. })
        .then(git_threads::pager::Pager::start);
    let ui = Ui::auto();
    match command {
        Command::Init { remote } => commands::init(&store, &remote),
        Command::Deinit { force } => commands::deinit(&store, force),
        Command::Comment { message, target, file, side } => {
            let message = match message {
                Some(text) => text,
                None => {
                    // Catch anything that would reject the comment — bad
                    // target, lines out of range, file outside the diff —
                    // before the user types a message.
                    commands::resolve_anchor(
                        store.repo(),
                        target.as_deref(),
                        file.as_deref(),
                        side.into(),
                    )?;
                    editor::message(store.repo(), "", COMMENT_HINT)?
                }
            };
            let thread = commands::comment(
                &store,
                &CommentOpts { target, file, message, side: side.into() },
            )?;
            println!(
                "drafted thread {} {}",
                ui.yellow(short(&thread)),
                ui.dim("(commit and push to share)")
            );
            Ok(())
        }
        Command::Reply { thread, message } => {
            let message = match message {
                Some(text) => text,
                None => {
                    let view = commands::thread_view(&store, &thread, "HEAD")?;
                    let preview = format!(
                        "replying to:\n\n{}",
                        render::thread(Ui::plain(), &store, &view, SnippetMode::Auto)?
                    );
                    editor::message(store.repo(), "", &format!("{COMMENT_HINT}\n\n{preview}"))?
                }
            };
            let drafted = commands::reply(&store, &thread, &message)?;
            println!(
                "drafted reply {} to thread {}",
                ui.yellow(short(&drafted.event)),
                ui.yellow(short(&drafted.thread))
            );
            Ok(())
        }
        Command::Edit { event, message } => {
            let message = match message {
                Some(text) => text,
                None => {
                    let seed = commands::current_body(&store, &event)?;
                    editor::message(store.repo(), &seed, EDIT_HINT)?
                }
            };
            let amendment = commands::edit(&store, &event, &message)?;
            println!(
                "drafted edit of {} {}",
                ui.yellow(short(&amendment.target)),
                ui.dim(format_args!("(edit event {})", short(&amendment.event)))
            );
            Ok(())
        }
        Command::Delete { event } => {
            let amendment = commands::delete(&store, &event)?;
            println!("drafted retraction of {}", ui.yellow(short(&amendment.target)));
            Ok(())
        }
        Command::Move { thread, file, at, orphans } => {
            let location = |path: &str, lines: Option<git_threads_core::LineRange>| match lines {
                Some(l) => format!("{path}:{}-{}", l.start, l.end),
                None => path.to_string(),
            };
            if orphans {
                let sweep = commands::move_orphans(&store, &at)?;
                let at_short = &sweep.target.to_string()[..12];
                for (draft, status) in &sweep.moved {
                    let status = match status {
                        git_threads_core::ReanchorStatus::Exact => "exact",
                        git_threads_core::ReanchorStatus::Relocated => "relocated",
                        git_threads_core::ReanchorStatus::Fuzzy(f) => &format!("fuzzy({f})"),
                    };
                    println!(
                        "drafted move of thread {} to {} {}",
                        ui.yellow(short(&draft.thread)),
                        ui.bold(location(&draft.path, draft.lines)),
                        ui.dim(format!("({status})"))
                    );
                }
                let stayed = |ids: &[git_threads_core::ThreadId], why: &str| {
                    if !ids.is_empty() {
                        let ids: Vec<&str> = ids.iter().map(short).collect();
                        println!("left in place — {why}: {}", ids.join(", "));
                    }
                };
                stayed(&sweep.unplaced, &format!("no verbatim match at {at_short}"));
                stayed(&sweep.whole_commit, "whole-change threads never re-anchor");
                if sweep.moved.is_empty()
                    && sweep.unplaced.is_empty()
                    && sweep.whole_commit.is_empty()
                {
                    println!("no orphaned threads at {}", ui.bold(at_short));
                } else if !sweep.moved.is_empty() {
                    println!("{}", ui.dim("(commit and push to share)"));
                }
                return Ok(());
            }
            let (thread, file) = (thread.expect("required"), file.expect("required"));
            let moved = commands::move_thread(&store, &thread, &file, &at)?;
            println!(
                "drafted move of thread {} to {} {}",
                ui.yellow(short(&moved.thread)),
                ui.bold(location(&moved.path, moved.lines)),
                ui.dim("(commit and push to share)")
            );
            Ok(())
        }
        Command::Resolve { thread, reopen } => {
            let thread = commands::resolve(&store, &thread, !reopen)?;
            println!(
                "thread {} {} {}",
                ui.yellow(short(&thread)),
                if reopen { "reopened" } else { "resolved" },
                ui.dim("(draft)")
            );
            Ok(())
        }
        Command::Discard { event, all } => match (event, all) {
            (None, true) => {
                match commands::discard_all(&store)? {
                    0 => println!("no drafts"),
                    n => println!("discarded {n} draft{}", if n == 1 { "" } else { "s" }),
                }
                Ok(())
            }
            (Some(event), false) => {
                match commands::discard(&store, &event)? {
                    Discarded::Thread { thread, events } => println!(
                        "discarded draft thread {} {}",
                        ui.yellow(short(&thread)),
                        ui.dim(format_args!(
                            "({events} event{})",
                            if events == 1 { "" } else { "s" }
                        )),
                    ),
                    Discarded::Event(event) => {
                        println!("discarded draft {}", ui.yellow(short(&event)))
                    }
                }
                Ok(())
            }
            _ => anyhow::bail!("pass a draft event ID, or --all"),
        },
        Command::Show { thread, at, patch, stat, json } => {
            let view = commands::thread_view(&store, &thread, &at)?;
            if json {
                // Unlike show proper, --json does not mark the thread seen —
                // a polling tool must not clear the user's inbox.
                println!("{}", serde_json::to_string_pretty(&view)?);
                return Ok(());
            }
            let mode = match (patch, stat) {
                (true, _) => SnippetMode::Patch,
                (_, true) => SnippetMode::Stat,
                _ => SnippetMode::Auto,
            };
            print!("{}", render::thread(ui, &store, &view, mode)?);
            // Reading a thread is what "seen" means; from here on it's not news.
            store.mark_thread_seen(&view.id)?;
            Ok(())
        }
        Command::Status => {
            print!("{}", render::status(ui, &commands::status(&store)?));
            Ok(())
        }
        Command::Seen { thread, undo } => {
            if undo {
                if !store.undo_seen()? {
                    println!("nothing to undo (no seen mark)");
                    return Ok(());
                }
                let with_news = commands::threads_with_news(&store)?;
                println!(
                    "rewound the seen mark; {with_news} thread{} with new activity",
                    if with_news == 1 { "" } else { "s" }
                );
                return Ok(());
            }
            match thread {
                Some(prefix) => {
                    let thread = commands::resolve_thread(&store, &prefix)?;
                    store.mark_thread_seen(&thread)?;
                    println!("marked thread {} seen", ui.yellow(short(&thread)));
                }
                None => {
                    store.mark_all_seen()?;
                    println!("marked all threads seen");
                }
            }
            Ok(())
        }
        Command::Import { source } => {
            let report = match source {
                ImportSource::Github { pr, all, remote } => {
                    if pr.is_none() && !all {
                        anyhow::bail!("pass a PR number or URL, or --all");
                    }
                    git_threads::import::github(&store, &remote, pr.as_deref(), all)?
                }
                ImportSource::Gitlab { mr, all, remote } => {
                    if mr.is_none() && !all {
                        anyhow::bail!("pass an MR number or URL, or --all");
                    }
                    git_threads::gitlab::import(&store, &remote, mr.as_deref(), all)?
                }
            };
            if report.events == 0 {
                match report.known {
                    0 => println!("nothing to import"),
                    n => println!(
                        "everything already imported ({n} event{})",
                        if n == 1 { "" } else { "s" }
                    ),
                }
            } else {
                println!(
                    "imported {} event{} in {} thread{} {}",
                    report.events,
                    if report.events == 1 { "" } else { "s" },
                    report.threads,
                    if report.threads == 1 { "" } else { "s" },
                    ui.dim("(git threads push to share, seen to clear the inbox)")
                );
            }
            if report.skipped > 0 {
                println!(
                    "{} thread{} skipped (code no longer reconstructable)",
                    report.skipped,
                    if report.skipped == 1 { "" } else { "s" }
                );
            }
            Ok(())
        }
        Command::Export { source } => {
            let report = match source {
                ExportSource::Github { pr, dry_run, remote } => {
                    git_threads::export::github(&store, &remote, &pr, dry_run)?
                }
                ExportSource::Gitlab { mr, dry_run, remote } => {
                    git_threads::gitlab::export(&store, &remote, &mr, dry_run)?
                }
            };
            match (report.dry_run, report.posts + report.resolves) {
                (true, _) => {}
                (false, 0) => println!("nothing to export (the change already has everything)"),
                (false, _) => println!(
                    "exported {} message{} in {} thread{}{}",
                    report.posts,
                    if report.posts == 1 { "" } else { "s" },
                    report.threads,
                    if report.threads == 1 { "" } else { "s" },
                    match report.resolves {
                        0 => String::new(),
                        n => format!(", {n} resolution toggle{}", if n == 1 { "" } else { "s" }),
                    },
                ),
            }
            Ok(())
        }
        Command::Pull { remote } => {
            match commands::pull(&store, &remote)? {
                None => println!("no threads data on {remote}"),
                Some(integration) => println!("{}", integration.describe(&remote)),
            }
            Ok(())
        }
        Command::Commit => {
            match commands::commit(&store)? {
                Some(promoted) => println!(
                    "committed {} event{} in {} thread{} {}",
                    promoted.events,
                    if promoted.events == 1 { "" } else { "s" },
                    promoted.threads,
                    if promoted.threads == 1 { "" } else { "s" },
                    ui.dim("(git threads push to share)"),
                ),
                None => println!("nothing to commit (no drafts)"),
            }
            Ok(())
        }
        Command::Push { remote } => {
            if store.drafts_tip()?.is_some() {
                eprintln!("note: you have drafted events; git threads commit to include them");
            }
            match commands::push(&store, &remote)? {
                PushOutcome::Pushed => println!("pushed to {remote}"),
                PushOutcome::NothingToPush => println!("nothing to push"),
            }
            Ok(())
        }
        Command::List {
            target,
            file,
            at,
            open,
            resolved,
            new,
            oneline,
            patch,
            stat,
            json,
            max_count,
            pr,
            author,
            grep,
            since,
            until,
        } => {
            let state = match (open, resolved) {
                (true, _) => Some(false),
                (_, true) => Some(true),
                _ => None,
            };
            let views = commands::list(
                &store,
                &ListOpts {
                    target,
                    file,
                    at,
                    resolved: state,
                    new,
                    max_count,
                    pr,
                    author,
                    grep,
                    since,
                    until,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&views)?);
                return Ok(());
            }
            if views.is_empty() {
                println!("{}", ui.dim("no threads"));
                return Ok(());
            }
            // list -p stays bounded (clipped hunks, diffstat for whole
            // changes): across many threads a full patch each buries the
            // list. show -p is the full-patch deep dive on one thread.
            let snippet_mode = match (patch, stat) {
                (_, true) => Some(SnippetMode::Stat),
                (true, _) => Some(SnippetMode::Auto),
                _ => None,
            };
            for (index, view) in views.iter().enumerate() {
                if !oneline && index > 0 {
                    println!();
                }
                print!("{}", render::list_entry(ui, &store, view, oneline, snippet_mode)?);
            }
            Ok(())
        }
        Command::Mangen { .. } => unreachable!("handled before store discovery"),
    }
}
