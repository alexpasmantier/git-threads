use clap::{Parser, Subcommand};
use git_threads::commands;

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
    /// List threads with their current state
    List,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Init { remote } => commands::init(&remote),
        Command::List => commands::list(),
    }
}
