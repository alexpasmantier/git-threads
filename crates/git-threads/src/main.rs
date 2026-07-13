use clap::{Parser, Subcommand, ValueEnum};
use git_threads::commands::{self, CommentOpts};
use git_threads::editor;
use git_threads::store::Store;
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
    },
    /// Discard drafted events before they're published
    Discard {
        /// Event ID (or unique prefix) of a draft; a draft thread's root discards the whole thread
        event: Option<String>,
        /// Discard every draft
        #[arg(long, conflicts_with = "event")]
        all: bool,
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
    },
    /// Generate man pages into a directory (for packaging)
    #[command(hide = true)]
    Mangen {
        /// Directory to write the pages into
        #[arg(default_value = ".")]
        out: std::path::PathBuf,
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
    match command {
        Command::Init { remote } => commands::init(&store, &remote),
        Command::Deinit { force } => commands::deinit(&store, force),
        Command::Comment { message, target, file, side } => {
            let message = match message {
                Some(text) => text,
                None => {
                    // Catch a bad target before the user types a message.
                    commands::resolve_target(
                        store.repo(),
                        target.as_deref(),
                        file.as_deref(),
                        side.into(),
                    )?;
                    editor::message(store.repo(), "", COMMENT_HINT)?
                }
            };
            commands::comment(&store, &CommentOpts { target, file, message, side: side.into() })?;
            Ok(())
        }
        Command::Reply { thread, message } => {
            let message = match message {
                Some(text) => text,
                None => {
                    let preview = commands::thread_preview(&store, &thread)?;
                    editor::message(store.repo(), "", &format!("{COMMENT_HINT}\n\n{preview}"))?
                }
            };
            commands::reply(&store, &thread, &message)?;
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
            commands::edit(&store, &event, &message)?;
            Ok(())
        }
        Command::Delete { event } => {
            commands::delete(&store, &event)?;
            Ok(())
        }
        Command::Resolve { thread, reopen } => commands::resolve(&store, &thread, !reopen),
        Command::Discard { event, all } => match (event, all) {
            (None, true) => commands::discard_all(&store),
            (Some(event), false) => commands::discard(&store, &event),
            _ => anyhow::bail!("pass a draft event ID, or --all"),
        },
        Command::Show { thread, at } => commands::show(&store, &thread, &at),
        Command::Pull { remote } => commands::pull(&store, &remote),
        Command::Commit => commands::commit(&store),
        Command::Push { remote } => commands::push(&store, &remote),
        Command::List { target, file, at, open, resolved } => {
            let state = match (open, resolved) {
                (true, _) => Some(false),
                (_, true) => Some(true),
                _ => None,
            };
            commands::list(&store, target.as_deref(), file.as_deref(), &at, state)
        }
        Command::Mangen { .. } => unreachable!("handled before store discovery"),
    }
}
