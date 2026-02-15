use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fl_core::repo::parse_duration_spec;
use fl_core::{
    ApiCallRecord, DecisionAction, EventKind, GateCondition, GatePolicy, RefKind, Repo,
    SemanticChangeKind, SemanticCompatibilityStatus, SemanticConflictClassification, SemanticRisk,
    UndoRequest,
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
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    Ready {
        #[arg(long)]
        json: bool,
    },
    QuickSave {
        #[arg(long)]
        tag: Option<String>,
    },
    QuickRestore,
    Presence {
        #[command(subcommand)]
        command: PresenceCommand,
    },
    Lock {
        #[command(subcommand)]
        command: LockCommand,
    },
    Subscribe {
        #[arg(long)]
        path: Vec<String>,
        #[arg(long)]
        symbol: Vec<String>,
        #[arg(long)]
        module: Vec<String>,
        #[arg(long)]
        notify: Option<String>,
    },
    Unsubscribe {
        id: String,
    },
    Subscriptions {
        #[arg(long)]
        json: bool,
    },
    Gate {
        #[command(subcommand)]
        command: GateCommand,
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
    Compare {
        left: String,
        right: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Prune {
        #[arg(long, default_value = "7d")]
        older_than: String,
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

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    Create {
        name: String,
        #[arg(long)]
        auto_rebase: bool,
    },
    List,
    Info {
        name: String,
    },
    Limits {
        name: String,
        #[arg(long)]
        max_snapshots: Option<usize>,
        #[arg(long)]
        max_events: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Start {
        #[arg(long)]
        task: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        initiator: Option<String>,
    },
    List {
        #[arg(long)]
        active: bool,
        #[arg(long)]
        json: bool,
    },
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Link {
        session_id: String,
        exploration_id: String,
    },
    Decision {
        session_id: String,
        exploration_id: String,
        #[arg(long)]
        action: DecisionActionArg,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "0.9")]
        confidence: f64,
    },
    Usage {
        session_id: String,
        #[arg(long)]
        tokens: Option<u64>,
        #[arg(long)]
        runtime_ms: Option<u64>,
        #[arg(long = "api-call")]
        api_call: Vec<String>,
    },
    Complete {
        id: String,
        #[arg(long)]
        result: Option<String>,
    },
    Fail {
        id: String,
        #[arg(long)]
        reason: String,
    },
    Provenance {
        exploration_id: String,
        #[arg(long)]
        json: bool,
    },
    Replay {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        #[arg(long = "discovered-from")]
        discovered_from: Option<String>,
    },
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Claim {
        id: String,
        #[arg(long)]
        assignee: Option<String>,
    },
    Done {
        id: String,
        #[arg(long)]
        result: Option<String>,
    },
    Fail {
        id: String,
        #[arg(long)]
        reason: String,
    },
    Graph {
        #[arg(long)]
        json: bool,
    },
    Link {
        task_id: String,
        event_ids: Vec<String>,
    },
    Compact {
        #[arg(long, default_value = "7d")]
        older_than: String,
    },
}

#[derive(Debug, Subcommand)]
enum PresenceCommand {
    Heartbeat {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        file: Vec<String>,
        #[arg(long)]
        intent: Option<String>,
        #[arg(long)]
        ttl: Option<u64>,
    },
    Depart {
        #[arg(long)]
        workspace: String,
    },
    List,
}

#[derive(Debug, Subcommand)]
enum LockCommand {
    Acquire {
        resource: String,
        #[arg(long)]
        ttl: Option<u64>,
    },
    List,
    Release {
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum GateCommand {
    Create {
        #[arg(long)]
        condition: GateConditionArg,
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long)]
        threshold: Option<u32>,
        #[arg(long, default_value = "block")]
        policy: GatePolicyArg,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Check {
        path: String,
    },
    Approve {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    Reject {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    Delete {
        id: String,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum GateConditionArg {
    FileTouched,
    SymbolModified,
    ImpactExceeds,
    SecuritySensitive,
    AgentConfidenceLow,
}

#[derive(Debug, Clone, ValueEnum)]
enum GatePolicyArg {
    Block,
    QueueAndContinue,
}

#[derive(Debug, Clone, ValueEnum)]
enum DecisionActionArg {
    Kept,
    Discarded,
}

impl From<DecisionActionArg> for DecisionAction {
    fn from(value: DecisionActionArg) -> Self {
        match value {
            DecisionActionArg::Kept => DecisionAction::Kept,
            DecisionActionArg::Discarded => DecisionAction::Discarded,
        }
    }
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
                    EventKind::Session(ses) => println!(
                        "{}  session:{:?}  {}  agent={}",
                        event.timestamp, ses.action, ses.session_id, ses.agent
                    ),
                    EventKind::Decision(dec) => println!(
                        "{}  decision:{:?}  session={}  exploration={}  confidence={:.2}",
                        event.timestamp,
                        dec.action,
                        dec.session_id,
                        dec.exploration_id,
                        dec.confidence
                    ),
                    EventKind::ResourceUsage(usage) => println!(
                        "{}  resource-usage  session={}  tokens={}  runtime={}ms",
                        event.timestamp,
                        usage.session_id,
                        usage.tokens_consumed.unwrap_or(0),
                        usage.runtime_ms.unwrap_or(0)
                    ),
                    EventKind::Task(task) => println!(
                        "{}  task:{:?}  {}  {}",
                        event.timestamp, task.action, task.task_id, task.title
                    ),
                    EventKind::Presence(p) => println!(
                        "{}  presence:{:?}  {}  workspace={}",
                        event.timestamp, p.action, p.actor, p.workspace
                    ),
                    EventKind::Lock(l) => println!(
                        "{}  lock:{:?}  {}  resource={}  holder={}",
                        event.timestamp, l.action, l.lock_id, l.resource, l.holder
                    ),
                    EventKind::Subscription(s) => println!(
                        "{}  subscription:{:?}  {}  actor={}",
                        event.timestamp, s.action, s.subscription_id, s.actor
                    ),
                    EventKind::Gate(g) => println!(
                        "{}  gate:{:?}  {}",
                        event.timestamp, g.action, g.gate_id
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
                ExploreCommand::Compare { left, right, json } => {
                    let left_id = parse_uuid(&left)?;
                    let right_id = right.as_deref().map(parse_uuid).transpose()?;
                    let diffs = repo.compare_explorations(left_id, right_id)?;

                    if diffs.is_empty() {
                        if json {
                            println!("[]");
                        } else {
                            println!("No differences.");
                        }
                        return Ok(());
                    }

                    if json {
                        println!("{}", serde_json::to_string_pretty(&diffs)?);
                        return Ok(());
                    }

                    for diff in &diffs {
                        print_semantic_file_diff(diff);
                    }
                }
                ExploreCommand::Prune { older_than } => {
                    let duration = parse_duration_spec(&older_than)?;
                    let pruned = repo.prune_explorations(duration)?;
                    println!(
                        "pruned {} abandoned exploration{}",
                        pruned,
                        if pruned == 1 { "" } else { "s" }
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
        Command::Workspace { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                WorkspaceCommand::Create { name, auto_rebase } => {
                    let ws = repo.create_workspace(name, auto_rebase)?;
                    let config = ws.workspace.as_ref().unwrap();
                    println!(
                        "workspace {} created -> {} (auto-rebase={}, base={})",
                        ws.name,
                        ws.target_event_id,
                        config.auto_rebase,
                        config
                            .base_snapshot_id
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "none".to_string())
                    );
                }
                WorkspaceCommand::List => {
                    let workspaces = repo.list_workspaces()?;
                    if workspaces.is_empty() {
                        println!("No workspaces.");
                        return Ok(());
                    }
                    for ws in workspaces {
                        let config = ws.workspace.as_ref().unwrap();
                        println!(
                            "{}  {}  auto-rebase={}",
                            ws.name, ws.target_event_id, config.auto_rebase
                        );
                    }
                }
                WorkspaceCommand::Info { name } => {
                    let info = repo.workspace_info(&name)?;
                    let config = info.workspace.workspace.as_ref().unwrap();
                    println!("Workspace: {}", info.workspace.name);
                    println!("  Target event: {}", info.workspace.target_event_id);
                    println!("  Auto-rebase: {}", config.auto_rebase);
                    println!(
                        "  Base snapshot: {}",
                        config
                            .base_snapshot_id
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "none".to_string())
                    );
                    println!("  Events: {}", info.event_count);
                    println!("  Checkpoints: {}", info.checkpoint_count);
                    println!("  Snapshots: {}", info.snapshot_count);
                    if let Some(max) = config.max_snapshots {
                        println!("  Max snapshots: {}", max);
                    }
                    if let Some(max) = config.max_events {
                        println!("  Max events: {}", max);
                    }
                    if !info.limits_exceeded.is_empty() {
                        println!("  Warnings:");
                        for warning in &info.limits_exceeded {
                            println!("    ! {}", warning);
                        }
                    }
                }
                WorkspaceCommand::Limits {
                    name,
                    max_snapshots,
                    max_events,
                } => {
                    if max_snapshots.is_none() && max_events.is_none() {
                        bail!("specify at least one of --max-snapshots or --max-events");
                    }
                    let ws = repo.set_workspace_limits(&name, max_snapshots, max_events)?;
                    let config = ws.workspace.as_ref().unwrap();
                    println!("workspace {} limits updated:", ws.name);
                    if let Some(max) = config.max_snapshots {
                        println!("  max-snapshots: {}", max);
                    }
                    if let Some(max) = config.max_events {
                        println!("  max-events: {}", max);
                    }
                }
            }
        }
        Command::Session { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                SessionCommand::Start {
                    task,
                    agent,
                    initiator,
                } => {
                    let session = repo.start_session(task, agent, initiator)?;
                    println!("session {} started: {}", session.id, session.task_description.as_deref().unwrap_or(""));
                }
                SessionCommand::List { active, json } => {
                    let mut sessions = repo.list_sessions()?;
                    if active {
                        sessions.retain(|s| s.status == fl_core::SessionStatus::Active);
                    }

                    if json {
                        let json_sessions: Vec<serde_json::Value> = sessions
                            .iter()
                            .map(|s| {
                                serde_json::json!({
                                    "id": s.id.to_string(),
                                    "agent": s.agent,
                                    "initiator": s.initiator,
                                    "task": s.task_description,
                                    "status": s.status.to_string(),
                                    "explorations": s.explorations.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                                    "created_at": s.created_at,
                                    "completed_at": s.completed_at,
                                    "result": s.result,
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&json_sessions)?);
                        return Ok(());
                    }

                    if sessions.is_empty() {
                        println!("No sessions.");
                        return Ok(());
                    }

                    for session in sessions {
                        println!(
                            "{}  {}  {}  {}",
                            session.id,
                            session.status,
                            session.agent,
                            session.task_description.as_deref().unwrap_or("")
                        );
                    }
                }
                SessionCommand::Show { id, json } => {
                    let session_id = parse_uuid(&id)?;
                    let session = repo.session_info(session_id)?;

                    if json {
                        let json_val = serde_json::json!({
                            "id": session.id.to_string(),
                            "agent": session.agent,
                            "initiator": session.initiator,
                            "task": session.task_description,
                            "status": session.status.to_string(),
                            "explorations": session.explorations.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                            "decisions": session.decisions.iter().map(|d| serde_json::json!({
                                "exploration_id": d.exploration_id.to_string(),
                                "action": format!("{:?}", d.action),
                                "reason": d.reason,
                                "confidence": d.confidence,
                                "timestamp": d.timestamp,
                            })).collect::<Vec<_>>(),
                            "resource_usage": {
                                "total_tokens": session.resource_usage.total_tokens,
                                "total_runtime_ms": session.resource_usage.total_runtime_ms,
                                "api_calls": session.resource_usage.api_calls.iter().map(|c| serde_json::json!({
                                    "service": c.service,
                                    "endpoint": c.endpoint,
                                    "count": c.count,
                                })).collect::<Vec<_>>(),
                            },
                            "created_at": session.created_at,
                            "completed_at": session.completed_at,
                            "result": session.result,
                        });
                        println!("{}", serde_json::to_string_pretty(&json_val)?);
                        return Ok(());
                    }

                    println!("Session: {}", session.id);
                    println!("  Agent: {}", session.agent);
                    if let Some(initiator) = &session.initiator {
                        println!("  Initiator: {}", initiator);
                    }
                    if let Some(task) = &session.task_description {
                        println!("  Task: {}", task);
                    }
                    println!("  Status: {}", session.status);
                    println!("  Created: {}", session.created_at);
                    if let Some(completed) = &session.completed_at {
                        println!("  Completed: {}", completed);
                    }
                    if let Some(result) = &session.result {
                        println!("  Result: {}", result);
                    }

                    if !session.explorations.is_empty() {
                        println!("  Explorations:");
                        for exp_id in &session.explorations {
                            println!("    {}", exp_id);
                        }
                    }

                    if !session.decisions.is_empty() {
                        println!("  Decisions:");
                        for dec in &session.decisions {
                            println!(
                                "    {:?} {} (confidence={:.2}) - {}",
                                dec.action, dec.exploration_id, dec.confidence, dec.reason
                            );
                        }
                    }

                    let usage = &session.resource_usage;
                    if usage.total_tokens > 0 || usage.total_runtime_ms > 0 {
                        println!("  Resource usage:");
                        println!("    Tokens: {}", usage.total_tokens);
                        println!("    Runtime: {}ms", usage.total_runtime_ms);
                        for call in &usage.api_calls {
                            println!("    API: {}:{} x{}", call.service, call.endpoint, call.count);
                        }
                    }
                }
                SessionCommand::Link {
                    session_id,
                    exploration_id,
                } => {
                    let sid = parse_uuid(&session_id)?;
                    let eid = parse_uuid(&exploration_id)?;
                    repo.link_session_exploration(sid, eid)?;
                    println!("linked exploration {} to session {}", eid, sid);
                }
                SessionCommand::Decision {
                    session_id,
                    exploration_id,
                    action,
                    reason,
                    confidence,
                } => {
                    let sid = parse_uuid(&session_id)?;
                    let eid = parse_uuid(&exploration_id)?;
                    repo.record_decision(sid, eid, action.into(), reason, confidence)?;
                    println!("decision recorded for session {}", sid);
                }
                SessionCommand::Usage {
                    session_id,
                    tokens,
                    runtime_ms,
                    api_call,
                } => {
                    let sid = parse_uuid(&session_id)?;
                    let api_calls = if api_call.is_empty() {
                        None
                    } else {
                        let mut records = Vec::new();
                        for spec in &api_call {
                            let parts: Vec<&str> = spec.splitn(3, ':').collect();
                            if parts.len() != 3 {
                                bail!(
                                    "invalid --api-call format `{}`; expected service:endpoint:count",
                                    spec
                                );
                            }
                            let count: u32 = parts[2].parse().with_context(|| {
                                format!("invalid count in --api-call `{}`", spec)
                            })?;
                            records.push(ApiCallRecord {
                                service: parts[0].to_string(),
                                endpoint: parts[1].to_string(),
                                count,
                            });
                        }
                        Some(records)
                    };
                    repo.record_resource_usage(sid, tokens, runtime_ms, api_calls)?;
                    println!("resource usage recorded for session {}", sid);
                }
                SessionCommand::Complete { id, result } => {
                    let session_id = parse_uuid(&id)?;
                    let session = repo.complete_session(session_id, result)?;
                    println!("session {} completed", session.id);
                }
                SessionCommand::Fail { id, reason } => {
                    let session_id = parse_uuid(&id)?;
                    let session = repo.fail_session(session_id, reason)?;
                    println!("session {} failed", session.id);
                }
                SessionCommand::Provenance {
                    exploration_id,
                    json,
                } => {
                    let eid = parse_uuid(&exploration_id)?;
                    let info = repo.query_provenance(eid)?;

                    if json {
                        let json_val = serde_json::json!({
                            "exploration": {
                                "id": info.exploration.id.to_string(),
                                "title": info.exploration.title,
                                "status": info.exploration.status.to_string(),
                            },
                            "session": info.session.as_ref().map(|s| serde_json::json!({
                                "id": s.id.to_string(),
                                "agent": s.agent,
                                "status": s.status.to_string(),
                            })),
                            "decisions": info.decisions.iter().map(|d| serde_json::json!({
                                "action": format!("{:?}", d.action),
                                "reason": d.reason,
                                "confidence": d.confidence,
                            })).collect::<Vec<_>>(),
                            "related_event_count": info.related_events.len(),
                        });
                        println!("{}", serde_json::to_string_pretty(&json_val)?);
                        return Ok(());
                    }

                    println!(
                        "Provenance: {} [{}]",
                        info.exploration.title, info.exploration.status
                    );
                    if let Some(session) = &info.session {
                        println!(
                            "  Session: {} (agent={}, status={})",
                            session.id, session.agent, session.status
                        );
                        if let Some(task) = &session.task_description {
                            println!("  Task: {}", task);
                        }
                    } else {
                        println!("  No session linked.");
                    }
                    if !info.decisions.is_empty() {
                        println!("  Decisions:");
                        for dec in &info.decisions {
                            println!(
                                "    {:?} (confidence={:.2}) - {}",
                                dec.action, dec.confidence, dec.reason
                            );
                        }
                    }
                    println!("  Related events: {}", info.related_events.len());
                }
                SessionCommand::Replay { id, json } => {
                    let session_id = parse_uuid(&id)?;
                    let replay = repo.replay_session(session_id)?;

                    if json {
                        let json_val = serde_json::json!({
                            "session": {
                                "id": replay.session.id.to_string(),
                                "agent": replay.session.agent,
                                "status": replay.session.status.to_string(),
                            },
                            "timeline_event_count": replay.timeline.len(),
                        });
                        println!("{}", serde_json::to_string_pretty(&json_val)?);
                        return Ok(());
                    }

                    println!("Session replay: {} [{}]", replay.session.id, replay.session.status);
                    println!("  Agent: {}", replay.session.agent);
                    if let Some(task) = &replay.session.task_description {
                        println!("  Task: {}", task);
                    }
                    println!();
                    println!("Timeline ({} events):", replay.timeline.len());
                    for event in &replay.timeline {
                        let kind_label = match &event.kind {
                            EventKind::Session(s) => format!("session:{:?}", s.action),
                            EventKind::Decision(d) => format!("decision:{:?}", d.action),
                            EventKind::ResourceUsage(_) => "resource-usage".to_string(),
                            EventKind::Exploration(e) => {
                                format!("exploration:{:?}", e.action)
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        println!("  {}  {}", event.timestamp, kind_label);
                    }
                }
            }
        }
        Command::Task { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                TaskCommand::Create {
                    title,
                    description,
                    depends_on,
                    discovered_from,
                } => {
                    let deps = depends_on
                        .iter()
                        .map(|id| parse_uuid(id))
                        .collect::<Result<Vec<_>>>()?;
                    let discovered = discovered_from
                        .as_deref()
                        .map(parse_uuid)
                        .transpose()?;
                    let task = repo.create_task(title, description, deps, discovered)?;
                    println!("task {} created: {}", task.id, task.title);
                }
                TaskCommand::List { all, json } => {
                    let mut tasks = repo.list_tasks()?;
                    if !all {
                        tasks.retain(|t| {
                            t.status == fl_core::TaskStatus::Open
                                || t.status == fl_core::TaskStatus::Claimed
                        });
                    }

                    if json {
                        let json_tasks: Vec<serde_json::Value> = tasks
                            .iter()
                            .map(|t| task_to_json(t))
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&json_tasks)?);
                        return Ok(());
                    }

                    if tasks.is_empty() {
                        println!("No tasks.");
                        return Ok(());
                    }

                    for task in &tasks {
                        let deps_str = if task.dependencies.is_empty() {
                            String::new()
                        } else {
                            let dep_ids: Vec<String> = task
                                .dependencies
                                .iter()
                                .map(|id| id.to_string()[..8].to_string())
                                .collect();
                            format!(" [deps: {}]", dep_ids.join(", "))
                        };
                        let assignee_str = task
                            .assignee
                            .as_deref()
                            .map(|a| format!(" @{}", a))
                            .unwrap_or_default();
                        println!(
                            "{}  {}  {}{}{}",
                            &task.id.to_string()[..8],
                            task.status,
                            task.title,
                            assignee_str,
                            deps_str,
                        );
                    }
                }
                TaskCommand::Show { id, json } => {
                    let task_id = parse_uuid(&id)?;
                    let task = repo.task_info(task_id)?;

                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&task_to_json(&task))?
                        );
                        return Ok(());
                    }

                    println!("Task: {}", task.id);
                    println!("  Title: {}", task.title);
                    if let Some(desc) = &task.description {
                        println!("  Description: {}", desc);
                    }
                    println!("  Status: {}", task.status);
                    if let Some(assignee) = &task.assignee {
                        println!("  Assignee: {}", assignee);
                    }
                    println!("  Created: {}", task.created_at);
                    if let Some(claimed_at) = &task.claimed_at {
                        println!("  Claimed: {}", claimed_at);
                    }
                    if let Some(completed_at) = &task.completed_at {
                        println!("  Completed: {}", completed_at);
                    }
                    if let Some(result) = &task.result {
                        println!("  Result: {}", result);
                    }
                    if !task.dependencies.is_empty() {
                        println!("  Dependencies:");
                        for dep in &task.dependencies {
                            println!("    {}", dep);
                        }
                    }
                    if !task.dependents.is_empty() {
                        println!("  Dependents:");
                        for dep in &task.dependents {
                            println!("    {}", dep);
                        }
                    }
                    if !task.linked_events.is_empty() {
                        println!("  Linked events:");
                        for ev in &task.linked_events {
                            println!("    {}", ev);
                        }
                    }
                    if let Some(discovered) = &task.discovered_from {
                        println!("  Discovered from: {}", discovered);
                    }
                }
                TaskCommand::Claim { id, assignee } => {
                    let task_id = parse_uuid(&id)?;
                    let task = repo.claim_task(task_id, assignee)?;
                    println!(
                        "task {} claimed by {}",
                        &task.id.to_string()[..8],
                        task.assignee.as_deref().unwrap_or("unknown")
                    );
                }
                TaskCommand::Done { id, result } => {
                    let task_id = parse_uuid(&id)?;
                    let task = repo.complete_task(task_id, result)?;
                    println!("task {} completed", &task.id.to_string()[..8]);
                }
                TaskCommand::Fail { id, reason } => {
                    let task_id = parse_uuid(&id)?;
                    let task = repo.fail_task(task_id, reason)?;
                    println!("task {} failed", &task.id.to_string()[..8]);
                }
                TaskCommand::Graph { json } => {
                    let graph = repo.task_graph()?;

                    if json {
                        let json_val = serde_json::json!({
                            "tasks": graph.tasks.iter().map(|t| task_to_json(t)).collect::<Vec<_>>(),
                            "edges": graph.edges.iter().map(|e| serde_json::json!({
                                "from": e.from_task.to_string(),
                                "to": e.to_task.to_string(),
                                "relation": format!("{:?}", e.relation),
                            })).collect::<Vec<_>>(),
                        });
                        println!("{}", serde_json::to_string_pretty(&json_val)?);
                        return Ok(());
                    }

                    if graph.tasks.is_empty() {
                        println!("No tasks.");
                        return Ok(());
                    }

                    println!("Tasks ({}):", graph.tasks.len());
                    for task in &graph.tasks {
                        let marker = match task.status {
                            fl_core::TaskStatus::Open => " ",
                            fl_core::TaskStatus::Claimed => ">",
                            fl_core::TaskStatus::Completed => "x",
                            fl_core::TaskStatus::Failed => "!",
                        };
                        println!(
                            "  [{}] {}  {}",
                            marker,
                            &task.id.to_string()[..8],
                            task.title,
                        );
                    }

                    if !graph.edges.is_empty() {
                        println!("\nEdges:");
                        for edge in &graph.edges {
                            let relation = match edge.relation {
                                fl_core::TaskRelation::DependsOn => "depends-on",
                                fl_core::TaskRelation::DiscoveredFrom => "discovered-from",
                            };
                            println!(
                                "  {} --{}--> {}",
                                &edge.from_task.to_string()[..8],
                                relation,
                                &edge.to_task.to_string()[..8],
                            );
                        }
                    }
                }
                TaskCommand::Link { task_id, event_ids } => {
                    let tid = parse_uuid(&task_id)?;
                    let eids = event_ids
                        .iter()
                        .map(|id| parse_uuid(id))
                        .collect::<Result<Vec<_>>>()?;
                    repo.link_task_event(tid, eids)?;
                    println!("linked events to task {}", &task_id[..8.min(task_id.len())]);
                }
                TaskCommand::Compact { older_than } => {
                    let duration = parse_duration_spec(&older_than)?;
                    let count = repo.compact_tasks_dry_run(duration)?;
                    if count == 0 {
                        println!("No completed tasks older than {} to compact.", older_than);
                    } else {
                        println!(
                            "{} completed task(s) older than {} eligible for compaction.",
                            count, older_than
                        );
                        println!("(In append-only mode, completed tasks are hidden from default views.)");
                    }
                }
            }
        }
        Command::Ready { json } => {
            let repo = Repo::discover(cwd)?;
            let ready = repo.ready_tasks()?;

            if json {
                let json_tasks: Vec<serde_json::Value> = ready
                    .iter()
                    .map(|t| task_to_json(t))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_tasks)?);
                return Ok(());
            }

            if ready.is_empty() {
                println!("No ready tasks.");
                return Ok(());
            }

            for task in &ready {
                println!("{}  {}", &task.id.to_string()[..8], task.title);
            }
        }
        Command::QuickSave { tag } => {
            let repo = Repo::discover(cwd)?;
            let event = repo.quick_save(tag)?;
            let EventKind::Checkpoint(payload) = event.kind else {
                bail!("unexpected event type")
            };
            println!("quick-save {} ({})", payload.label, event.id);
        }
        Command::QuickRestore => {
            let repo = Repo::discover(cwd)?;
            let result = repo.quick_restore()?;
            println!("restored to before event {}", result.target_event_id);
            if let Some(cp) = result.restored_checkpoint_event {
                println!("new checkpoint: {}", cp);
            }
        }
        Command::Presence { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                PresenceCommand::Heartbeat {
                    workspace,
                    file,
                    intent,
                    ttl,
                } => {
                    let presence = repo.heartbeat(workspace, file, intent, ttl)?;
                    println!(
                        "heartbeat: {} in {} (ttl={}s)",
                        presence.actor,
                        presence.workspace,
                        presence.ttl.as_secs()
                    );
                }
                PresenceCommand::Depart { workspace } => {
                    repo.depart(workspace.clone())?;
                    println!("departed from workspace {}", workspace);
                }
                PresenceCommand::List => {
                    let presences = repo.list_presence()?;
                    if presences.is_empty() {
                        println!("No active presence.");
                        return Ok(());
                    }
                    for p in &presences {
                        let files_str = if p.active_files.is_empty() {
                            String::new()
                        } else {
                            format!(" files=[{}]", p.active_files.join(", "))
                        };
                        let intent_str = p
                            .intent
                            .as_deref()
                            .map(|i| format!(" intent=\"{}\"", i))
                            .unwrap_or_default();
                        println!(
                            "{}  {}  ttl={}s{}{}",
                            p.actor,
                            p.workspace,
                            p.ttl.as_secs(),
                            files_str,
                            intent_str
                        );
                    }
                }
            }
        }
        Command::Lock { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                LockCommand::Acquire { resource, ttl } => {
                    let lock = repo.acquire_lock(resource, ttl)?;
                    println!(
                        "lock {} acquired on `{}` by {} (ttl={}s)",
                        &lock.id.to_string()[..8],
                        lock.resource,
                        lock.holder,
                        lock.ttl.as_secs()
                    );
                }
                LockCommand::List => {
                    let locks = repo.list_locks()?;
                    if locks.is_empty() {
                        println!("No active locks.");
                        return Ok(());
                    }
                    for lock in &locks {
                        println!(
                            "{}  {}  holder={}  ttl={}s",
                            &lock.id.to_string()[..8],
                            lock.resource,
                            lock.holder,
                            lock.ttl.as_secs()
                        );
                    }
                }
                LockCommand::Release { id } => {
                    let lock_id = parse_uuid(&id)?;
                    repo.release_lock(lock_id)?;
                    println!("lock {} released", &id[..8.min(id.len())]);
                }
            }
        }
        Command::Subscribe {
            path,
            symbol,
            module,
            notify,
        } => {
            let repo = Repo::discover(cwd)?;
            let sub = repo.subscribe(path, symbol, module, notify)?;
            println!(
                "subscription {} created (notify={})",
                &sub.id.to_string()[..8],
                sub.notify
            );
        }
        Command::Unsubscribe { id } => {
            let repo = Repo::discover(cwd)?;
            let sub_id = parse_uuid(&id)?;
            repo.unsubscribe(sub_id)?;
            println!("subscription {} cancelled", &id[..8.min(id.len())]);
        }
        Command::Subscriptions { json } => {
            let repo = Repo::discover(cwd)?;
            let subs = repo.list_subscriptions()?;

            if json {
                let json_subs: Vec<serde_json::Value> = subs
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id.to_string(),
                            "actor": s.actor,
                            "status": s.status.to_string(),
                            "paths": s.paths,
                            "symbols": s.symbols,
                            "modules": s.modules,
                            "notify": s.notify.to_string(),
                            "created_at": s.created_at,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_subs)?);
                return Ok(());
            }

            if subs.is_empty() {
                println!("No active subscriptions.");
                return Ok(());
            }
            for sub in &subs {
                let mut filters = Vec::new();
                if !sub.paths.is_empty() {
                    filters.push(format!("paths=[{}]", sub.paths.join(", ")));
                }
                if !sub.symbols.is_empty() {
                    filters.push(format!("symbols=[{}]", sub.symbols.join(", ")));
                }
                if !sub.modules.is_empty() {
                    filters.push(format!("modules=[{}]", sub.modules.join(", ")));
                }
                println!(
                    "{}  {}  {}  notify={}",
                    &sub.id.to_string()[..8],
                    sub.actor,
                    filters.join(" "),
                    sub.notify
                );
            }
        }
        Command::Gate { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                GateCommand::Create {
                    condition,
                    pattern,
                    threshold,
                    policy,
                } => {
                    let gate_condition = match condition {
                        GateConditionArg::FileTouched => {
                            let p = pattern.ok_or_else(|| {
                                anyhow::anyhow!("--pattern required for file-touched condition")
                            })?;
                            GateCondition::FileTouched(p)
                        }
                        GateConditionArg::SymbolModified => {
                            let p = pattern.ok_or_else(|| {
                                anyhow::anyhow!("--pattern required for symbol-modified condition")
                            })?;
                            GateCondition::SymbolModified(p)
                        }
                        GateConditionArg::ImpactExceeds => {
                            let t = threshold.ok_or_else(|| {
                                anyhow::anyhow!("--threshold required for impact-exceeds condition")
                            })?;
                            GateCondition::ImpactExceeds(t)
                        }
                        GateConditionArg::SecuritySensitive => GateCondition::SecuritySensitive,
                        GateConditionArg::AgentConfidenceLow => {
                            let t = threshold.unwrap_or(80);
                            GateCondition::AgentConfidenceLow(t)
                        }
                    };
                    let gate_policy = match policy {
                        GatePolicyArg::Block => GatePolicy::Block,
                        GatePolicyArg::QueueAndContinue => GatePolicy::QueueAndContinue,
                    };
                    let gate = repo.create_gate(gate_condition, gate_policy)?;
                    println!(
                        "gate {} created: {} (policy={})",
                        &gate.id.to_string()[..8],
                        gate.condition,
                        gate.policy
                    );
                }
                GateCommand::List { json } => {
                    let gates = repo.list_gates()?;
                    if json {
                        let json_gates: Vec<serde_json::Value> = gates
                            .iter()
                            .map(|g| {
                                serde_json::json!({
                                    "id": g.id.to_string(),
                                    "status": g.status.to_string(),
                                    "condition": g.condition.to_string(),
                                    "policy": g.policy.to_string(),
                                    "created_at": g.created_at,
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&json_gates)?);
                        return Ok(());
                    }
                    if gates.is_empty() {
                        println!("No active gates.");
                        return Ok(());
                    }
                    for gate in &gates {
                        println!(
                            "{}  {}  {}  policy={}",
                            &gate.id.to_string()[..8],
                            gate.status,
                            gate.condition,
                            gate.policy
                        );
                    }
                }
                GateCommand::Check { path } => {
                    let blocking = repo.check_gates_for_path(&path)?;
                    if blocking.is_empty() {
                        println!("No gates block path `{}`.", path);
                    } else {
                        println!(
                            "{} gate{} block path `{}`:",
                            blocking.len(),
                            if blocking.len() == 1 { "" } else { "s" },
                            path
                        );
                        for gate in &blocking {
                            println!(
                                "  {}  {}  policy={}",
                                &gate.id.to_string()[..8],
                                gate.condition,
                                gate.policy
                            );
                        }
                    }
                }
                GateCommand::Approve { id, reason } => {
                    let gate_id = parse_uuid(&id)?;
                    repo.approve_gate(gate_id, reason)?;
                    println!("gate {} approved", &id[..8.min(id.len())]);
                }
                GateCommand::Reject { id, reason } => {
                    let gate_id = parse_uuid(&id)?;
                    repo.reject_gate(gate_id, reason)?;
                    println!("gate {} rejected", &id[..8.min(id.len())]);
                }
                GateCommand::Delete { id } => {
                    let gate_id = parse_uuid(&id)?;
                    repo.delete_gate(gate_id)?;
                    println!("gate {} deleted", &id[..8.min(id.len())]);
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

fn task_to_json(task: &fl_core::TaskSummary) -> serde_json::Value {
    serde_json::json!({
        "id": task.id.to_string(),
        "title": task.title,
        "description": task.description,
        "status": task.status.to_string(),
        "assignee": task.assignee,
        "dependencies": task.dependencies.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "dependents": task.dependents.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "created_at": task.created_at,
        "claimed_at": task.claimed_at,
        "completed_at": task.completed_at,
        "result": task.result,
        "linked_events": task.linked_events.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "discovered_from": task.discovered_from.map(|id| id.to_string()),
    })
}
