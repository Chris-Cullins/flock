use std::env;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fl_core::repo::parse_duration_spec;
use fl_core::{EventKind, RefKind, Repo, SemanticChangeKind, UndoRequest};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "fl", about = "Flock CLI (MVP)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init {
        #[arg(long)]
        colocated: bool,
    },
    Checkpoint {
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
    },
    Log,
    Fsck,
    Diff {
        #[arg(long)]
        semantic: bool,
        #[arg(long)]
        json: bool,
    },
    Explore {
        #[command(subcommand)]
        command: ExploreCommand,
    },
    Undo {
        #[arg(long)]
        n: Option<usize>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long = "file")]
        file: Option<String>,
    },
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    Refs {
        #[command(subcommand)]
        command: RefsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ExploreCommand {
    Start {
        #[arg(long)]
        title: String,
    },
    List,
    Promote {
        id: String,
    },
    Abandon {
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum GitCommand {
    Commit {
        #[arg(short = 'm', long = "message")]
        message: String,
    },
    Push {
        remote: Option<String>,
        branch: Option<String>,
    },
    Pull {
        remote: Option<String>,
        branch: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum RefsCommand {
    List,
    Set {
        kind: RefKindArg,
        name: String,
        target: String,
        #[arg(long)]
        auto_rebase: bool,
    },
    Delete {
        kind: RefKindArg,
        name: String,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum RefKindArg {
    Branch,
    Tag,
    Workspace,
}

impl From<RefKindArg> for RefKind {
    fn from(value: RefKindArg) -> Self {
        match value {
            RefKindArg::Branch => RefKind::Branch,
            RefKindArg::Tag => RefKind::Tag,
            RefKindArg::Workspace => RefKind::Workspace,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir()?;

    match cli.command {
        Command::Init { colocated } => {
            let repo = Repo::at(cwd);
            if colocated {
                repo.init_colocated()?;
            } else {
                repo.init()?;
            }
            println!(
                "Initialized Flock repository in {}/.flock",
                repo.root().display()
            );
        }
        Command::Checkpoint { message } => {
            let repo = Repo::discover(cwd)?;
            let event = repo.create_checkpoint(message)?;
            let checkpoint_id = event.id;
            let EventKind::Checkpoint(payload) = event.kind else {
                bail!("unexpected event payload for checkpoint")
            };
            println!(
                "checkpoint {} ({}) id={} parent={} merkle={}",
                payload.label,
                payload.snapshot_id,
                checkpoint_id,
                payload
                    .parent_checkpoint_event
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "none".to_string()),
                payload.snapshot_merkle_root.as_deref().unwrap_or("none")
            );
        }
        Command::Log => {
            let repo = Repo::discover(cwd)?;
            let events = repo.list_events()?;

            for event in events {
                match event.kind {
                    EventKind::Checkpoint(cp) => println!(
                        "{}  checkpoint  {}  {}  parent={}",
                        event.timestamp,
                        cp.label,
                        cp.snapshot_id,
                        cp.parent_checkpoint_event
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "none".to_string())
                    ),
                    EventKind::Exploration(exp) => println!(
                        "{}  exploration:{:?}  {}  {}",
                        event.timestamp, exp.action, exp.exploration_id, exp.title
                    ),
                    EventKind::Undo(undo) => println!(
                        "{}  undo  target={}  restored={}  scope={}",
                        event.timestamp,
                        undo.target_event_id,
                        undo.restored_checkpoint_event
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "none".to_string()),
                        undo.file_scope.as_deref().unwrap_or("all")
                    ),
                    EventKind::GitBridge(bridge) => println!(
                        "{}  git:{:?}  success={}  {}",
                        event.timestamp, bridge.action, bridge.success, bridge.detail
                    ),
                }
            }
        }
        Command::Fsck => {
            let repo = Repo::discover(cwd)?;
            let report = repo.fsck()?;
            println!(
                "fsck ok: events={} checkpoints={} snapshots={} refs={}",
                report.event_count,
                report.checkpoint_count,
                report.snapshot_count,
                report.ref_count
            );
        }
        Command::Diff { semantic, json } => {
            if !semantic {
                bail!("only `fl diff --semantic` is implemented in MVP");
            }

            let repo = Repo::discover(cwd)?;
            let diffs = repo.semantic_diff_from_latest_checkpoint()?;
            if diffs.is_empty() {
                if json {
                    println!("[]");
                } else {
                    println!("No semantic changes since last checkpoint.");
                }
                return Ok(());
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&diffs)?);
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
        Command::Explore { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                ExploreCommand::Start { title } => {
                    let exploration = repo.start_exploration(title)?;
                    println!(
                        "exploration {} started: {}",
                        exploration.id, exploration.title
                    );
                }
                ExploreCommand::List => {
                    let explorations = repo.list_explorations()?;
                    if explorations.is_empty() {
                        println!("No explorations.");
                        return Ok(());
                    }

                    for exploration in explorations {
                        println!(
                            "{}  {}  {}",
                            exploration.id, exploration.status, exploration.title
                        );
                    }
                }
                ExploreCommand::Promote { id } => {
                    let exploration_id = parse_uuid(&id)?;
                    let exploration = repo.promote_exploration(exploration_id)?;
                    println!(
                        "exploration {} promoted: {}",
                        exploration.id, exploration.title
                    );
                }
                ExploreCommand::Abandon { id } => {
                    let exploration_id = parse_uuid(&id)?;
                    let exploration = repo.abandon_exploration(exploration_id)?;
                    println!(
                        "exploration {} abandoned: {}",
                        exploration.id, exploration.title
                    );
                }
            }
        }
        Command::Undo { n, to, since, file } => {
            let repo = Repo::discover(cwd)?;
            let request = build_undo_request(n, to, since)?;
            let result = if let Some(path) = file {
                repo.undo_file(request, path)?
            } else {
                repo.undo(request)?
            };

            println!("undo target event: {}", result.target_event_id);
            if let Some(checkpoint_id) = result.restored_checkpoint_event {
                println!("restored checkpoint event: {}", checkpoint_id);
            }
        }
        Command::Git { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                GitCommand::Commit { message } => {
                    let out = repo.git_commit_stub(message)?;
                    if !out.is_empty() {
                        println!("{}", out);
                    }
                }
                GitCommand::Push { remote, branch } => {
                    let out = repo.git_push_stub(remote, branch)?;
                    if !out.is_empty() {
                        println!("{}", out);
                    }
                }
                GitCommand::Pull { remote, branch } => {
                    let out = repo.git_pull_stub(remote, branch)?;
                    if !out.is_empty() {
                        println!("{}", out);
                    }
                }
            }
        }
        Command::Refs { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                RefsCommand::List => {
                    let refs = repo.list_refs()?;
                    if refs.is_empty() {
                        println!("No refs.");
                        return Ok(());
                    }

                    for entry in refs {
                        match entry.kind {
                            RefKind::Workspace => {
                                let auto_rebase = entry
                                    .workspace
                                    .as_ref()
                                    .map(|workspace| workspace.auto_rebase)
                                    .unwrap_or(false);
                                println!(
                                    "workspace  {}  {}  auto-rebase={}",
                                    entry.name, entry.target_event_id, auto_rebase
                                );
                            }
                            RefKind::Branch => {
                                println!("branch     {}  {}", entry.name, entry.target_event_id);
                            }
                            RefKind::Tag => {
                                println!("tag        {}  {}", entry.name, entry.target_event_id);
                            }
                        }
                    }
                }
                RefsCommand::Set {
                    kind,
                    name,
                    target,
                    auto_rebase,
                } => {
                    let kind: RefKind = kind.into();
                    let auto_rebase = match kind {
                        RefKind::Workspace => Some(auto_rebase),
                        RefKind::Branch | RefKind::Tag => {
                            if auto_rebase {
                                bail!("--auto-rebase can only be used with workspace refs");
                            }
                            None
                        }
                    };

                    let reference = repo.upsert_ref(kind, name, target, auto_rebase)?;
                    match reference.kind {
                        RefKind::Workspace => {
                            let auto_rebase = reference
                                .workspace
                                .as_ref()
                                .map(|workspace| workspace.auto_rebase)
                                .unwrap_or(false);
                            println!(
                                "set workspace {} -> {} (auto-rebase={})",
                                reference.name, reference.target_event_id, auto_rebase
                            );
                        }
                        RefKind::Branch => {
                            println!(
                                "set branch {} -> {}",
                                reference.name, reference.target_event_id
                            );
                        }
                        RefKind::Tag => {
                            println!(
                                "set tag {} -> {}",
                                reference.name, reference.target_event_id
                            );
                        }
                    }
                }
                RefsCommand::Delete { kind, name } => {
                    let removed = repo.delete_ref(kind.into(), &name)?;
                    if removed {
                        println!("deleted ref {}", name);
                    } else {
                        println!("ref {} not found", name);
                    }
                }
            }
        }
    }

    Ok(())
}

fn build_undo_request(
    n: Option<usize>,
    to: Option<String>,
    since: Option<String>,
) -> Result<UndoRequest> {
    let mut count = 0;
    if n.is_some() {
        count += 1;
    }
    if to.is_some() {
        count += 1;
    }
    if since.is_some() {
        count += 1;
    }

    if count > 1 {
        bail!("use only one of --n, --to, or --since");
    }

    if let Some(n) = n {
        return Ok(UndoRequest::N(n));
    }

    if let Some(to) = to {
        return Ok(UndoRequest::To(to));
    }

    if let Some(since) = since {
        return Ok(UndoRequest::Since(parse_duration_spec(&since)?));
    }

    Ok(UndoRequest::Last)
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("invalid UUID `{}`", value))
}
