use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fl_core::repo::parse_duration_spec;
use fl_core::{
    EventKind, RefKind, Repo, SemanticChangeKind, SemanticCompatibilityStatus,
    SemanticConflictClassification, SemanticRisk, UndoRequest,
};
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
        intent: bool,
        #[arg(long)]
        json: bool,
    },
    Impact {
        path: String,
        #[arg(long)]
        json: bool,
    },
    Merge {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        semantic: bool,
        base: String,
        left: String,
        right: String,
        #[arg(long)]
        json: bool,
    },
    Review {
        id: String,
        #[arg(long)]
        expand: Option<usize>,
        #[arg(long)]
        full: bool,
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
    Status,
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
    Import {
        git_ref: Option<String>,
    },
    Export {
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
        Command::Diff {
            semantic,
            intent,
            json,
        } => {
            if intent {
                // --intent implies --semantic
                let repo = Repo::discover(cwd)?;
                let groups = repo.semantic_diff_with_intents()?;
                if groups.is_empty() {
                    if json {
                        println!("[]");
                    } else {
                        println!("No semantic changes since last checkpoint.");
                    }
                    return Ok(());
                }

                if json {
                    let json_groups: Vec<serde_json::Value> = groups
                        .iter()
                        .map(|(intent, files)| {
                            serde_json::json!({
                                "intent": intent,
                                "files": serde_json::to_value(files).unwrap_or_default(),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json_groups)?);
                    return Ok(());
                }

                for (intent_label, files) in &groups {
                    let total_changes: usize = files.iter().map(|f| f.changes.len()).sum();
                    println!(
                        "## {} ({} file{}, {} change{})",
                        intent_label,
                        files.len(),
                        if files.len() == 1 { "" } else { "s" },
                        total_changes,
                        if total_changes == 1 { "" } else { "s" }
                    );
                    for diff in files {
                        println!("  {} [{}]", diff.path, diff.language);
                        for change in &diff.changes {
                            let marker = change_marker(change.kind);
                            let risk = risk_label(change.risk);
                            println!("    {} [{}] {}", marker, risk, change.symbol);
                        }
                    }
                    println!();
                }
            } else {
                if !semantic {
                    bail!("only `fl diff --semantic` or `fl diff --intent` is implemented");
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
                    print_semantic_file_diff(&diff);
                }
            }
        }
        Command::Impact { path, json } => {
            let repo = Repo::discover(cwd)?;
            let report = repo.impact_analysis(&path)?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "target": report.target,
                        "direct_dependents": report.direct_dependents,
                        "transitive_dependents": report.transitive_dependents,
                        "symbols": report.symbols,
                    }))?
                );
                return Ok(());
            }

            println!("Impact analysis: {}", report.target);
            println!();

            if !report.symbols.is_empty() {
                println!("Symbols:");
                for symbol in &report.symbols {
                    println!("  {}", symbol);
                }
                println!();
            }

            if report.direct_dependents.is_empty() && report.transitive_dependents.is_empty() {
                println!("No dependents found.");
            } else {
                if !report.direct_dependents.is_empty() {
                    println!(
                        "Direct dependents ({}):",
                        report.direct_dependents.len()
                    );
                    for dep in &report.direct_dependents {
                        println!("  {}", dep);
                    }
                }
                if !report.transitive_dependents.is_empty() {
                    println!(
                        "Transitive dependents ({}):",
                        report.transitive_dependents.len()
                    );
                    for dep in &report.transitive_dependents {
                        println!("  {}", dep);
                    }
                }
            }
        }
        Command::Merge {
            dry_run,
            semantic,
            base,
            left,
            right,
            json,
        } => {
            if !dry_run || !semantic {
                bail!("`fl merge` currently requires both --dry-run and --semantic flags");
            }

            let repo = Repo::discover(cwd)?;
            let result = repo.semantic_merge_preview(
                &PathBuf::from(&base),
                &PathBuf::from(&left),
                &PathBuf::from(&right),
            )?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }

            println!("Merge preview: {} [{}]", result.path, result.language);
            if result.parse_fallback {
                println!("  ! parser fallback used");
            }

            if result.conflicts.is_empty() {
                println!("  No conflicts - clean merge");
            } else {
                println!(
                    "  {} conflict{}:",
                    result.conflicts.len(),
                    if result.conflicts.len() == 1 { "" } else { "s" }
                );
                for conflict in &result.conflicts {
                    let classification = match conflict.classification {
                        SemanticConflictClassification::Unclassified => "unclassified",
                        SemanticConflictClassification::DivergentEdit => "divergent-edit",
                        SemanticConflictClassification::DeleteVsEdit => "delete-vs-edit",
                        SemanticConflictClassification::ConcurrentAddition => {
                            "concurrent-addition"
                        }
                        SemanticConflictClassification::KindMismatch => "kind-mismatch",
                        SemanticConflictClassification::TextFallback => "text-fallback",
                    };
                    println!("    [{}] {}", classification, conflict.symbol);
                    if !conflict.explanation.is_empty() {
                        println!("      {}", conflict.explanation);
                    }
                }
            }

            println!();
            println!("Merged source ({} bytes):", result.merged_source.len());
            // Show first 20 lines as preview
            for (i, line) in result.merged_source.lines().take(20).enumerate() {
                println!("  {:>3} | {}", i + 1, line);
            }
            let line_count = result.merged_source.lines().count();
            if line_count > 20 {
                println!("  ... ({} more lines)", line_count - 20);
            }
        }
        Command::Review { id, expand, full } => {
            let repo = Repo::discover(cwd)?;
            let exploration_id = parse_uuid(&id)?;
            let summary = repo.review_exploration(exploration_id)?;

            println!(
                "Review: {} [{}]",
                summary.exploration.title, summary.exploration.status
            );
            println!("  Exploration: {}", summary.exploration.id);
            println!(
                "  Base checkpoint: {}",
                summary
                    .exploration
                    .base_checkpoint_event
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "none".to_string())
            );
            println!();

            let stats = &summary.stats;
            println!(
                "Stats: {} file{} changed, +{} -{} ~{} symbols",
                stats.files_changed,
                if stats.files_changed == 1 { "" } else { "s" },
                stats.symbols_added,
                stats.symbols_removed,
                stats.symbols_modified
            );
            if stats.high_risk_count > 0 || stats.breaking_count > 0 {
                println!(
                    "  {} high-risk, {} breaking",
                    stats.high_risk_count, stats.breaking_count
                );
            }
            println!();

            if let Some(n) = expand {
                // Expand mode: show detail for change #n (1-indexed)
                let mut change_index = 0usize;
                let mut found = false;
                for diff in &summary.diffs {
                    for change in &diff.changes {
                        change_index += 1;
                        if change_index == n {
                            println!(
                                "Change #{}: {} in {} [{}]",
                                n, change.symbol, diff.path, diff.language
                            );
                            let marker = change_marker(change.kind);
                            let risk = risk_label(change.risk);
                            println!("  {} [{}] {}", marker, risk, change.symbol);

                            if !change.impact.symbols.is_empty() {
                                println!(
                                    "  Impact symbols: {}",
                                    change.impact.symbols.join(", ")
                                );
                            }
                            if !change.impact.files.is_empty() {
                                println!(
                                    "  Impact files: {}",
                                    change.impact.files.join(", ")
                                );
                            }
                            if !change.impact.modules.is_empty() {
                                println!(
                                    "  Impact modules: {}",
                                    change.impact.modules.join(", ")
                                );
                            }
                            if change.compatibility.status
                                != SemanticCompatibilityStatus::Compatible
                            {
                                let status = compatibility_label(change.compatibility.status);
                                println!("  Compatibility: {}", status);
                                for note in &change.compatibility.notes {
                                    println!("    {}", note);
                                }
                            }
                            found = true;
                            break;
                        }
                    }
                    if found {
                        break;
                    }
                }
                if !found {
                    bail!(
                        "change #{} not found (total changes: {})",
                        n,
                        change_index
                    );
                }
            } else if full {
                // Full mode: show complete semantic diff output
                for diff in &summary.diffs {
                    print_semantic_file_diff(diff);
                }
            } else {
                // Summary mode: numbered list of changes
                let mut change_index = 0usize;
                for diff in &summary.diffs {
                    println!("{} [{}]", diff.path, diff.language);
                    for change in &diff.changes {
                        change_index += 1;
                        let marker = change_marker(change.kind);
                        let risk = risk_label(change.risk);
                        println!(
                            "  #{:<3} {} [{}] {}",
                            change_index, marker, risk, change.symbol
                        );
                    }
                }
                if change_index > 0 {
                    println!();
                    println!(
                        "Use --expand <n> to see detail for a specific change, or --full for complete diff."
                    );
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
                GitCommand::Status => {
                    let report = repo.git_shadow_status()?;
                    println!(
                        "shadow mode: {} ({})",
                        report.mode,
                        if report.clean {
                            "ok"
                        } else {
                            "attention required"
                        }
                    );
                    for check in report.checks {
                        println!(
                            "[{}] {}: {}",
                            if check.ok { "ok" } else { "fail" },
                            check.name,
                            check.detail
                        );
                        if let Some(recovery) = check.recovery {
                            println!("  recovery: {}", recovery);
                        }
                    }
                }
                GitCommand::Commit { message } => {
                    let out = repo.git_commit(message)?;
                    if !out.is_empty() {
                        println!("{}", out);
                    }
                }
                GitCommand::Push { remote, branch } => {
                    let out = repo.git_push(remote, branch)?;
                    if !out.is_empty() {
                        println!("{}", out);
                    }
                }
                GitCommand::Pull { remote, branch } => {
                    let out = repo.git_pull(remote, branch)?;
                    if !out.is_empty() {
                        println!("{}", out);
                    }
                }
                GitCommand::Import { git_ref } => {
                    let out = repo.git_import(git_ref)?;
                    if !out.is_empty() {
                        println!("{}", out);
                    }
                }
                GitCommand::Export { branch } => {
                    let out = repo.git_export(branch)?;
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

fn change_marker(kind: SemanticChangeKind) -> &'static str {
    match kind {
        SemanticChangeKind::Added => "+",
        SemanticChangeKind::Removed => "-",
        SemanticChangeKind::Modified => "~",
        SemanticChangeKind::Renamed => "R",
        SemanticChangeKind::Moved => "M",
        SemanticChangeKind::StyleOnly => "=",
    }
}

fn risk_label(risk: SemanticRisk) -> &'static str {
    match risk {
        SemanticRisk::Low => "low",
        SemanticRisk::Medium => "medium",
        SemanticRisk::High => "high",
    }
}

fn compatibility_label(status: SemanticCompatibilityStatus) -> &'static str {
    match status {
        SemanticCompatibilityStatus::Compatible => "compatible",
        SemanticCompatibilityStatus::PotentiallyBreaking => "potentially-breaking",
        SemanticCompatibilityStatus::Breaking => "breaking",
    }
}

fn print_semantic_file_diff(diff: &fl_core::SemanticFileDiff) {
    println!("{} [{}]", diff.path, diff.language);
    for change in &diff.changes {
        let marker = change_marker(change.kind);
        let risk = risk_label(change.risk);
        println!("  {} [{}] {}", marker, risk, change.symbol);
        let mut impact_fields = Vec::new();
        if !change.impact.symbols.is_empty() {
            impact_fields.push(format!("symbols={}", change.impact.symbols.join(", ")));
        }
        if !change.impact.files.is_empty() {
            impact_fields.push(format!("files={}", change.impact.files.join(", ")));
        }
        if !change.impact.modules.is_empty() {
            impact_fields.push(format!("modules={}", change.impact.modules.join(", ")));
        }
        if !impact_fields.is_empty() {
            println!("    impact: {}", impact_fields.join(" | "));
        }
        if change.compatibility.status != SemanticCompatibilityStatus::Compatible {
            let status = compatibility_label(change.compatibility.status);
            if change.compatibility.notes.is_empty() {
                println!("    compatibility: {}", status);
            } else {
                println!(
                    "    compatibility: {} ({})",
                    status,
                    change.compatibility.notes.join("; ")
                );
            }
        }
    }
    if diff.parse_fallback {
        println!("  ! parser fallback used");
    }
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
