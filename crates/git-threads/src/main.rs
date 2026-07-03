use clap::{Parser, Subcommand, ValueEnum};
use git_threads::commands::{self, CommentOpts};
use git_threads::store::Store;
use git_threads_core::Side;

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
    /// Start a new thread on a commit, file, or line range
    Comment {
        /// Comment text
        #[arg(short, long)]
        message: String,
        /// Commit whose change is being discussed
        #[arg(default_value = "HEAD")]
        commit: String,
        /// Anchor the thread to this file
        #[arg(long)]
        file: Option<String>,
        /// Line or line range within --file, e.g. 120 or 120-128
        #[arg(long, requires = "file")]
        lines: Option<String>,
        /// Which version of --file the lines refer to
        #[arg(long, value_enum, default_value_t = SideArg::New)]
        side: SideArg,
        /// Diff base (defaults to the commit's first parent)
        #[arg(long)]
        base: Option<String>,
    },
    /// Reply to a thread, or to a specific message in one
    Reply {
        /// Thread ID, or the ID of the comment/reply being answered (or a unique prefix)
        thread: String,
        /// Reply text
        #[arg(short, long)]
        message: String,
    },
    /// Edit a comment or reply (appends an edit event; history is preserved)
    Edit {
        /// Event ID (or unique prefix) of the comment or reply, as shown by `show`
        event: String,
        /// Replacement text
        #[arg(short, long)]
        message: String,
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
    /// Fetch and integrate threads data from a remote
    Pull {
        /// Remote to pull from
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Push local threads data to a remote (integrating remote state first)
    Publish {
        /// Remote to publish to
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// List threads with their current state
    List {
        /// Commit to re-anchor threads against
        #[arg(long, default_value = "HEAD")]
        at: String,
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
    let store = Store::discover()?;
    match Cli::parse().command {
        Command::Init { remote } => commands::init(&store, &remote),
        Command::Comment { message, commit, file, lines, side, base } => {
            commands::comment(
                &store,
                &CommentOpts { commit, message, file, lines, side: side.into(), base },
            )?;
            Ok(())
        }
        Command::Reply { thread, message } => {
            commands::reply(&store, &thread, &message)?;
            Ok(())
        }
        Command::Edit { event, message } => {
            commands::edit(&store, &event, &message)?;
            Ok(())
        }
        Command::Delete { event } => {
            commands::delete(&store, &event)?;
            Ok(())
        }
        Command::Resolve { thread, reopen } => commands::resolve(&store, &thread, !reopen),
        Command::Show { thread, at } => commands::show(&store, &thread, &at),
        Command::Pull { remote } => commands::pull(&store, &remote),
        Command::Publish { remote } => commands::publish(&store, &remote),
        Command::List { at } => commands::list(&store, &at),
    }
}
