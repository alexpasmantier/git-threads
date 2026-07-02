use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "git-threads", version, about = "Anchored, threaded discussions stored in git")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Configure this clone for git-threads (refspecs, initial fetch)
    Init,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => anyhow::bail!("not implemented yet"),
    }
}
