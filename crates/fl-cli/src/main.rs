use std::env;

use anyhow::Result;
use clap::{Parser, Subcommand};
use fl_core::{EventKind, Repo, SemanticChangeKind};

#[derive(Debug, Parser)]
#[command(name = "fl", about = "Flock CLI (MVP)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Checkpoint {
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
    },
    Log,
    Diff {
        #[arg(long)]
        semantic: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir()?;

    match cli.command {
        Command::Init => {
            let repo = Repo::at(cwd);
            repo.init()?;
            println!(
                "Initialized Flock repository in {}/.flock",
                repo.root().display()
            );
        }
        Command::Checkpoint { message } => {
            let repo = Repo::discover(cwd)?;
            let event = repo.create_checkpoint(message)?;
            let EventKind::Checkpoint(payload) = event.kind;
            println!("checkpoint {} ({})", payload.label, payload.snapshot_id);
        }
        Command::Log => {
            let repo = Repo::discover(cwd)?;
            let events = repo.list_events()?;

            for event in events {
                match event.kind {
                    EventKind::Checkpoint(cp) => println!(
                        "{}  checkpoint  {}  {}",
                        event.timestamp, cp.label, cp.snapshot_id
                    ),
                }
            }
        }
        Command::Diff { semantic } => {
            if !semantic {
                anyhow::bail!("only `fl diff --semantic` is implemented in MVP");
            }

            let repo = Repo::discover(cwd)?;
            let diffs = repo.semantic_diff_from_latest_checkpoint()?;
            if diffs.is_empty() {
                println!("No semantic changes since last checkpoint.");
                return Ok(());
            }

            for diff in diffs {
                println!("{} [{}]", diff.path, diff.language);
                for change in diff.changes {
                    let marker = match change.kind {
                        SemanticChangeKind::Added => "+",
                        SemanticChangeKind::Removed => "-",
                        SemanticChangeKind::Modified => "~",
                        SemanticChangeKind::TextOnly => "=",
                    };
                    println!("  {} {}", marker, change.symbol);
                }
                if diff.parse_fallback {
                    println!("  ! parser fallback used");
                }
            }
        }
    }

    Ok(())
}
