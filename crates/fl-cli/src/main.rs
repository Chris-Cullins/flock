use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{self, Shell};
use fl_core::repo::parse_duration_spec;
use fl_core::{
    ApiCallRecord, ConflictStatus, DecisionAction, EventKind, ExplorationStatus, GateCondition,
    GatePolicy, RefKind, Repo, SemanticChangeKind, SemanticCompatibilityStatus,
    SemanticConflictClassification, SemanticRisk, TaskRelation, TaskStatus,
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
    /// Initialize a new Flock repository
    Init {
        /// Use git-colocated mode (.git + .flock sidecar)
        #[arg(long)]
        colocated: bool,
        /// Use native block-level storage
        #[arg(long)]
        native: bool,
    },
    /// Create a commit (save current state)
    #[command(alias = "checkpoint")]
    Commit {
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// Bypass secret detection (recorded in audit log)
        #[arg(long)]
        allow_secrets: bool,
        /// Skip hook execution (recorded in audit log)
        #[arg(long)]
        skip_hooks: bool,
    },
    /// Show the event log
    Log,
    /// Verify repository integrity
    Fsck,
    /// Show working directory status vs last commit
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show semantic diff between commits or against working directory
    Diff {
        /// Enable semantic diff output
        #[arg(long)]
        semantic: bool,
        /// Group changes by intent
        #[arg(long)]
        intent: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// First commit ID/prefix (compare against working dir if only one given)
        #[arg(name = "FROM")]
        from: Option<String>,
        /// Second commit ID/prefix (compare FROM..TO)
        #[arg(name = "TO")]
        to: Option<String>,
    },
    /// Analyze impact of changes to a path or symbol
    Impact {
        path: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Preview semantic merge of three files
    Merge {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        semantic: bool,
        base: String,
        left: String,
        right: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Review an exploration's changes
    Review {
        id: String,
        /// Expand detail for change #n
        #[arg(long)]
        expand: Option<usize>,
        /// Show full line-level diff
        #[arg(long)]
        full: bool,
    },
    /// Manage explorations (branching workflows)
    Explore {
        #[command(subcommand)]
        command: ExploreCommand,
    },
    /// Undo events on the timeline
    Undo {
        /// Undo last N events
        #[arg(long)]
        n: Option<usize>,
        /// Undo to a specific event ID
        #[arg(long)]
        to: Option<String>,
        /// Undo events newer than duration (e.g. 1h, 2d)
        #[arg(long)]
        since: Option<String>,
        /// Scope undo to a single file
        #[arg(long = "file")]
        file: Option<String>,
    },
    /// Git bridge operations (colocated mode)
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    /// Manage refs (branches, tags, workspaces)
    Refs {
        #[command(subcommand)]
        command: RefsCommand,
    },
    /// Manage workspaces
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Agent session tracking and provenance
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Task graph management
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Show tasks ready to be claimed
    Ready {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Stream live updates via WebSocket
        #[arg(long)]
        live: bool,
    },
    /// Quick commit for agents
    QuickSave {
        #[arg(long)]
        tag: Option<String>,
    },
    /// Restore to last quick-save
    QuickRestore,
    /// Multi-agent presence tracking
    Presence {
        #[command(subcommand)]
        command: PresenceCommand,
    },
    /// Advisory resource locking
    Lock {
        #[command(subcommand)]
        command: LockCommand,
    },
    /// Subscribe to changes on paths, symbols, or modules
    Subscribe {
        #[arg(long)]
        path: Vec<String>,
        #[arg(long)]
        symbol: Vec<String>,
        #[arg(long)]
        module: Vec<String>,
        /// Notification method (log, webhook URL)
        #[arg(long)]
        notify: Option<String>,
    },
    /// Cancel a subscription
    Unsubscribe {
        id: String,
    },
    /// List active subscriptions
    Subscriptions {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Human-in-the-loop quality gates
    Gate {
        #[command(subcommand)]
        command: GateCommand,
    },
    /// Rebase a workspace onto the latest checkpoint
    Rebase {
        /// Workspace to rebase
        #[arg(long)]
        workspace: String,
    },
    /// Auto-rebase all workspaces with auto_rebase enabled
    AutoRebase,
    /// Conflict resolution workflow
    Conflict {
        #[command(subcommand)]
        command: ConflictCommand,
    },
    /// Migrate repository storage backend
    Migrate {
        /// Migrate to native block-level storage
        #[arg(long)]
        native: bool,
    },
    /// Rebuild or clear the semantic index (AST cache + dependency graph)
    Index {
        /// Clear all cached semantic data instead of rebuilding
        #[arg(long)]
        clear: bool,
    },
    /// Materialize replay state for faster future operations
    Materialize,
    /// Migrate event log to segmented format
    MigrateEventLog,
    /// Compact the event log by archiving old events
    Compact {
        /// Archive events older than this duration (e.g. 180d, 30d, 1y)
        #[arg(long, default_value = "180d")]
        older_than: String,
    },
    /// Convert a repository to/from Flock format
    Convert {
        #[command(subcommand)]
        command: ConvertCommand,
    },
    /// Manage remote authentication
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Manage roosts (Flock remotes)
    Roost {
        #[command(subcommand)]
        command: RoostCommand,
    },
    /// Push events and content to a roost
    Push {
        /// Roost name (defaults to "origin")
        roost: Option<String>,
        /// Branch to push
        branch: Option<String>,
    },
    /// Pull events and content from a roost
    Pull {
        /// Roost name (defaults to "origin")
        roost: Option<String>,
        /// Branch to pull
        branch: Option<String>,
    },
    /// Clone a remote repository
    Clone {
        /// Remote URL (flock:// or file://)
        url: String,
        /// Target directory (defaults to repo name from URL)
        dir: Option<String>,
        /// Shallow clone: only fetch last N checkpoints
        #[arg(long)]
        depth: Option<usize>,
        /// Sparse clone: only fetch blocks matching these globs
        #[arg(long)]
        sparse: Vec<String>,
        /// Focus clone: auto-compute sparse set from build target
        #[arg(long)]
        focus: Option<String>,
        /// Lazy clone: download events/indices only, fetch blocks on demand
        #[arg(long)]
        lazy: bool,
    },
    /// Manage sparse checkout patterns
    Sparse {
        #[command(subcommand)]
        command: SparseCommand,
    },
    /// Fetch additional history or resolve missing blocks
    Fetch {
        /// Extend shallow history by N additional checkpoints
        #[arg(long)]
        deepen: Option<usize>,
        /// Scan for and download missing blocks
        #[arg(long)]
        resolve_missing: bool,
        /// Roost name (defaults to "origin")
        #[arg(long)]
        roost: Option<String>,
    },
    /// Pin files for offline access (eagerly fetch blocks)
    Pin {
        /// Glob pattern to pin (e.g. "src/**")
        pattern: Option<String>,
        /// Pin all files (convert to full clone)
        #[arg(long)]
        all: bool,
        /// List pinned patterns
        #[arg(long)]
        list: bool,
        /// Remove a pin pattern
        #[arg(long)]
        unpin: Option<String>,
        /// Roost name (defaults to "origin")
        #[arg(long)]
        roost: Option<String>,
    },
    /// Stream live events from a remote via WebSocket
    Watch {
        /// Roost name (defaults to "origin")
        #[arg(long)]
        remote: Option<String>,
        /// Filter by file path glob
        #[arg(long)]
        path: Vec<String>,
        /// Filter by symbol name
        #[arg(long)]
        symbol: Vec<String>,
        /// Filter by agent name
        #[arg(long)]
        agent: Vec<String>,
        /// Filter by event kind (e.g. Task, Checkpoint, Presence)
        #[arg(long)]
        kind: Vec<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Start editor plugin protocol server (JSON-lines over stdin/stdout)
    EditorServer {
        /// Roost name for WebSocket connection (defaults to "origin")
        #[arg(long)]
        remote: Option<String>,
    },
    /// Search event history using natural language
    Query {
        /// Search text
        text: String,
        /// Maximum number of results
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Use AI to enhance search results (requires FL_LLM_API_KEY)
        #[arg(long)]
        ai: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage the intelligence search index
    #[command(name = "intel")]
    Intel {
        #[command(subcommand)]
        command: IntelCommand,
    },
    /// Show session confidence score
    Confidence {
        /// Show detailed factor breakdown
        #[arg(long)]
        verbose: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Backup and restore .flock data
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Manage signing key encryption
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Audit the event log for security anomalies
    Audit {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Agent governance policy management
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Generate shell completions for bash, zsh, fish, or powershell
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

#[derive(Debug, Subcommand)]
enum BackupCommand {
    /// Create a backup of the .flock directory
    Create {
        /// Output path for the backup archive
        path: String,
    },
    /// Restore a backup archive
    Restore {
        /// Path to the backup archive
        archive: String,
        /// Target directory (defaults to current directory)
        #[arg(long)]
        target: Option<String>,
    },
    /// Verify a backup archive contains all required data
    Verify {
        /// Path to the backup archive
        archive: String,
    },
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    /// Encrypt the signing key with a passphrase
    Encrypt {
        /// Passphrase (or set FL_KEY_PASSPHRASE env var)
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// Decrypt the signing key
    Decrypt {
        /// Passphrase (or set FL_KEY_PASSPHRASE env var)
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// Show signing key status (plaintext, encrypted, or absent)
    Status,
}

#[derive(Debug, Subcommand)]
enum RemoteCommand {
    /// Authenticate with a remote host using a token
    Login {
        /// Remote host to authenticate with (e.g. "example.com")
        host: String,
        /// Bearer token for authentication
        #[arg(long)]
        token: Option<String>,
        /// Authenticate using SSH key (ed25519 challenge-response)
        #[arg(long)]
        ssh_key: bool,
    },
    /// Remove stored credentials for a remote host
    Logout {
        /// Remote host to log out from
        host: String,
    },
    /// List stored credentials
    Status,
}

#[derive(Debug, Subcommand)]
enum RoostCommand {
    /// Add a new roost
    Add {
        name: String,
        url: String,
    },
    /// Remove a roost
    Remove {
        name: String,
    },
    /// List configured roosts
    List,
    /// Change the URL of a roost
    SetUrl {
        name: String,
        url: String,
    },
}

#[derive(Debug, Subcommand)]
enum SparseCommand {
    /// Add a sparse checkout pattern
    Add {
        /// Glob pattern (e.g. "src/**")
        pattern: String,
        /// Roost name (defaults to "origin")
        #[arg(long)]
        roost: Option<String>,
    },
    /// Remove a sparse checkout pattern
    Remove {
        /// Glob pattern to remove
        pattern: String,
        /// Roost name (defaults to "origin")
        #[arg(long)]
        roost: Option<String>,
    },
    /// List sparse checkout patterns
    List {
        /// Roost name (defaults to "origin")
        #[arg(long)]
        roost: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConvertCommand {
    /// Convert from git repository to Flock
    FromGit {
        /// Only convert specific branches (comma-separated)
        #[arg(long)]
        branch: Option<String>,
        /// Only import last N commits per branch (shallow)
        #[arg(long)]
        shallow: Option<usize>,
    },
    /// Convert from jj repository to Flock
    FromJj {
        /// Only import last N commits (shallow)
        #[arg(long)]
        shallow: Option<usize>,
    },
    /// Export Flock history to a clean git repository
    ToGit {
        /// Remove .flock/ directory after successful export
        #[arg(long)]
        remove_flock: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ExploreCommand {
    /// Start a new exploration
    Start {
        #[arg(long)]
        title: String,
    },
    /// List all explorations
    List,
    /// Promote an exploration to mainline
    Promote { id: String },
    /// Abandon an exploration
    Abandon { id: String },
    /// Compare two explorations (or one against mainline)
    Compare {
        left: String,
        right: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Prune old abandoned explorations
    Prune {
        #[arg(long, default_value = "7d")]
        older_than: String,
    },
    /// Show visual exploration tree grouped by base checkpoint
    Tree,
}

#[derive(Debug, Subcommand)]
enum GitCommand {
    /// Show shadow mode status and safety checks
    Status,
    /// Create a git commit from current state
    Commit {
        #[arg(short = 'm', long = "message")]
        message: String,
    },
    /// Push to a git remote
    Push {
        remote: Option<String>,
        branch: Option<String>,
    },
    /// Pull from a git remote
    Pull {
        remote: Option<String>,
        branch: Option<String>,
    },
    /// Import git history into Flock events
    Import { git_ref: Option<String> },
    /// Export Flock checkpoints to a git branch
    Export { branch: Option<String> },
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
    /// Create a new task
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        #[arg(long = "discovered-from")]
        discovered_from: Option<String>,
    },
    /// List tasks (open and claimed by default)
    List {
        /// Include completed and failed tasks
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        /// Stream live task updates via WebSocket
        #[arg(long)]
        live: bool,
    },
    /// Show task details
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Claim a task for work
    Claim {
        id: String,
        #[arg(long)]
        assignee: Option<String>,
    },
    /// Mark a task as completed (auto-commits unless --no-checkpoint)
    Done {
        id: String,
        #[arg(long)]
        result: Option<String>,
        /// Skip the automatic commit
        #[arg(long)]
        no_checkpoint: bool,
    },
    /// Mark a task as failed
    Fail {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Show task dependency graph as ASCII tree
    Graph {
        #[arg(long)]
        json: bool,
    },
    /// Link events to a task
    Link {
        task_id: String,
        event_ids: Vec<String>,
    },
    /// Compact old completed tasks
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

#[derive(Debug, Subcommand)]
enum IntelCommand {
    /// Rebuild the intelligence search index from all events
    Rebuild,
    /// Show search index statistics
    Stats {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConflictCommand {
    /// Detect conflicts for a workspace against the latest checkpoint
    Detect {
        #[arg(long)]
        workspace: String,
    },
    /// Suggest resolution for a conflict
    Suggest {
        id: String,
    },
    /// Mark a conflict as resolved
    Resolve {
        id: String,
        /// Description of how the conflict was resolved
        #[arg(long)]
        resolution: String,
    },
    /// Verify a resolved conflict
    Verify {
        id: String,
        /// Whether verification passed
        #[arg(long)]
        passed: bool,
    },
    /// Record a verified conflict (final step)
    Record {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// List conflicts
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<ConflictStatusArg>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PolicyCommand {
    /// Display current policy configuration
    Show,
    /// Write default policies.toml to .flock/
    Init,
    /// Show policy decisions from the event log
    Audit {
        /// Filter by task ID
        #[arg(long)]
        task: Option<String>,
        /// Filter by exploration ID
        #[arg(long)]
        exploration: Option<String>,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum ConflictStatusArg {
    Detected,
    Classified,
    Suggested,
    Resolved,
    Verified,
    Recorded,
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
        Command::Init { colocated, native } => {
            let repo = Repo::at(cwd);
            if colocated && native {
                bail!("--colocated and --native are mutually exclusive");
            } else if native {
                repo.init_native()?;
            } else if colocated {
                repo.init_colocated()?;
            } else {
                repo.init()?;
            }
            println!(
                "Initialized Flock repository in {}/.flock",
                repo.root().display()
            );
        }
        Command::Commit {
            message,
            allow_secrets,
            skip_hooks,
        } => {
            let repo = Repo::discover(cwd)?;
            let event = repo.create_checkpoint_with_options(message, allow_secrets, skip_hooks)?;
            let commit_id = event.id;
            let EventKind::Checkpoint(payload) = event.kind else {
                bail!("unexpected event payload for commit")
            };
            println!(
                "commit {} ({}) id={} parent={} merkle={}",
                payload.label,
                payload.snapshot_id,
                commit_id,
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
                        "{}  commit  {}  {}  parent={}",
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
                    EventKind::Rebase(r) => println!(
                        "{}  rebase  workspace={}  files={}  conflicts={}",
                        event.timestamp, r.workspace, r.files_merged.len(), r.conflicts_found
                    ),
                    EventKind::ConflictResolution(cr) => println!(
                        "{}  conflict:{:?}  {}  path={}",
                        event.timestamp,
                        cr.action,
                        cr.conflict_id,
                        cr.path.as_deref().unwrap_or("-")
                    ),
                    EventKind::Hook(h) => {
                        let status = if h.bypassed {
                            "skipped"
                        } else if h.success {
                            "pass"
                        } else {
                            "FAIL"
                        };
                        println!(
                            "{}  hook:{}  {}  {}  {}ms",
                            event.timestamp, h.hook_point, h.hook_name, status, h.duration_ms
                        );
                    }
                    EventKind::RemoteSync(rs) => println!(
                        "{}  sync:{:?}  roost={}  events={}  blocks={}  success={}",
                        event.timestamp, rs.action, rs.roost_name, rs.event_count,
                        rs.block_count, rs.success
                    ),
                    EventKind::Intelligence(intel) => println!(
                        "{}  intel:{:?}  model={}  confidence={}",
                        event.timestamp,
                        intel.action,
                        intel.model.as_deref().unwrap_or("-"),
                        intel.confidence.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "-".to_string()),
                    ),
                    EventKind::Policy(policy) => println!(
                        "{}  policy  {}  {:?}  op={}  reason={}",
                        event.timestamp,
                        policy.policy_name,
                        policy.verdict,
                        policy.operation,
                        policy.reason.as_deref().unwrap_or("-"),
                    ),
                }
            }
        }
        Command::Fsck => {
            let repo = Repo::discover(cwd)?;
            let report = repo.fsck()?;
            println!(
                "fsck ok: events={} commits={} snapshots={} refs={} hash_chain={}",
                report.event_count,
                report.checkpoint_count,
                report.snapshot_count,
                report.ref_count,
                if report.hash_chain_verified { "verified" } else { "unverified" }
            );
        }
        Command::Status { json } => {
            let repo = Repo::discover(cwd)?;
            let report = repo.status()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "branch": report.branch,
                        "checkpoint": report.checkpoint_id,
                        "new": report.new_files,
                        "modified": report.modified_files,
                        "deleted": report.deleted_files,
                    }))?
                );
            } else {
                println!("On branch {}", report.branch);
                if let Some(ref cp) = report.checkpoint_id {
                    println!("Latest commit: {}", cp);
                } else {
                    println!("No commits yet");
                }
                let total =
                    report.new_files.len() + report.modified_files.len() + report.deleted_files.len();
                if total == 0 {
                    println!("\nNothing changed since last commit.");
                } else {
                    println!();
                    for f in &report.new_files {
                        println!("  new:      {}", f);
                    }
                    for f in &report.modified_files {
                        println!("  modified: {}", f);
                    }
                    for f in &report.deleted_files {
                        println!("  deleted:  {}", f);
                    }
                    println!(
                        "\n{} file(s) changed",
                        total
                    );
                }
            }
        }
        Command::Diff {
            semantic,
            intent,
            json,
            from,
            to,
        } => {
            let repo = Repo::discover(cwd)?;

            // Determine diff mode based on positional args
            match (from.as_deref(), to.as_deref()) {
                // fl diff <from> <to> — checkpoint-to-checkpoint diff
                (Some(from_prefix), Some(to_prefix)) => {
                    if intent {
                        let groups = repo.semantic_diff_between_checkpoints_with_intents(
                            from_prefix,
                            to_prefix,
                        )?;
                        print_intent_diff(&groups, json)?;
                    } else if semantic {
                        let diffs =
                            repo.semantic_diff_between_checkpoints(from_prefix, to_prefix)?;
                        print_semantic_diffs(&diffs, json)?;
                    } else {
                        // File-level summary + semantic changes
                        let summary =
                            repo.file_summary_between_checkpoints(from_prefix, to_prefix)?;
                        let diffs =
                            repo.semantic_diff_between_checkpoints(from_prefix, to_prefix)?;
                        print_full_diff_summary(&summary, &diffs, json)?;
                    }
                }
                // fl diff <checkpoint> — checkpoint vs working directory
                (Some(checkpoint_prefix), None) => {
                    if intent {
                        let groups = repo
                            .semantic_diff_checkpoint_vs_working_with_intents(checkpoint_prefix)?;
                        print_intent_diff(&groups, json)?;
                    } else if semantic {
                        let diffs =
                            repo.semantic_diff_checkpoint_vs_working(checkpoint_prefix)?;
                        print_semantic_diffs(&diffs, json)?;
                    } else {
                        let summary =
                            repo.file_summary_checkpoint_vs_working(checkpoint_prefix)?;
                        let diffs =
                            repo.semantic_diff_checkpoint_vs_working(checkpoint_prefix)?;
                        print_full_diff_summary(&summary, &diffs, json)?;
                    }
                }
                // fl diff — latest checkpoint vs working directory (existing behavior)
                (None, None) => {
                    if intent {
                        let groups = repo.semantic_diff_with_intents()?;
                        print_intent_diff(&groups, json)?;
                    } else {
                        if !semantic {
                            bail!("only `fl diff --semantic` or `fl diff --intent` is implemented;\nor use `fl diff <commit-id>` to diff a specific commit");
                        }
                        let diffs = repo.semantic_diff_from_latest_checkpoint()?;
                        print_semantic_diffs(&diffs, json)?;
                    }
                }
                (None, Some(_)) => {
                    bail!("cannot specify TO without FROM; use `fl diff <from> <to>`");
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
                        SemanticConflictClassification::CrossFileBreakage => {
                            "cross-file-breakage"
                        }
                    };
                    println!("    [{}] {}", classification, conflict.symbol);
                    if !conflict.explanation.is_empty() {
                        println!("      {}", conflict.explanation);
                    }
                    if !conflict.affected_files.is_empty() {
                        println!(
                            "      affected files: {}",
                            conflict.affected_files.join(", ")
                        );
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
                "  Base commit: {}",
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
                ExploreCommand::Tree => {
                    let explorations = repo.list_explorations()?;
                    if explorations.is_empty() {
                        println!("No explorations.");
                        return Ok(());
                    }
                    print_exploration_tree(&explorations);
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
                println!("restored commit event: {}", checkpoint_id);
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
                TaskCommand::List { all, json, live } => {
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

                    if live {
                        stream_live_task_updates(&repo, "origin", json)?;
                    }
                }
                TaskCommand::Show { id, json } => {
                    let task = repo.find_task_by_prefix(&id)?;

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
                    let task_id = repo.find_task_by_prefix(&id)?.id;
                    let task = repo.claim_task(task_id, assignee)?;
                    println!(
                        "task {} claimed by {}",
                        &task.id.to_string()[..8],
                        task.assignee.as_deref().unwrap_or("unknown")
                    );
                }
                TaskCommand::Done {
                    id,
                    result,
                    no_checkpoint,
                } => {
                    let task_id = repo.find_task_by_prefix(&id)?.id;
                    let task = repo.complete_task(task_id, result)?;
                    println!("task {} completed", &task.id.to_string()[..8]);

                    if !no_checkpoint {
                        let message = format!("task done: {}", task.title);
                        match repo.create_checkpoint(Some(message.clone())) {
                            Ok(event) => {
                                println!(
                                    "commit {} created: {}",
                                    &event.id.to_string()[..8],
                                    message
                                );
                            }
                            Err(e) => {
                                eprintln!("warning: auto-commit failed: {}", e);
                            }
                        }
                    }
                }
                TaskCommand::Fail { id, reason } => {
                    let task_id = repo.find_task_by_prefix(&id)?.id;
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

                    print_task_graph(&graph);
                }
                TaskCommand::Link { task_id, event_ids } => {
                    let tid = repo.find_task_by_prefix(&task_id)?.id;
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
        Command::Ready { json, live } => {
            let repo = Repo::discover(cwd)?;
            let ready = repo.ready_tasks()?;

            if json {
                let json_tasks: Vec<serde_json::Value> = ready
                    .iter()
                    .map(|t| task_to_json(t))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_tasks)?);
            } else if ready.is_empty() {
                println!("No ready tasks.");
            } else {
                for task in &ready {
                    println!("{}  {}", &task.id.to_string()[..8], task.title);
                }
            }

            if live {
                stream_live_task_updates(&repo, "origin", json)?;
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
                println!("new commit: {}", cp);
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
        Command::Rebase { workspace } => {
            let repo = Repo::discover(cwd)?;
            let result = repo.rebase_workspace(&workspace)?;
            if result.already_up_to_date {
                println!("workspace `{}` is already up to date", workspace);
            } else {
                println!(
                    "rebased workspace `{}` ({} -> {})",
                    workspace,
                    &result.old_base_event.to_string()[..8],
                    &result.new_base_event.to_string()[..8],
                );
                if !result.files_merged.is_empty() {
                    println!("  files merged: {}", result.files_merged.len());
                    for f in &result.files_merged {
                        println!("    {}", f);
                    }
                }
                if !result.conflicts.is_empty() {
                    println!("  conflicts: {}", result.conflicts.len());
                    for c in &result.conflicts {
                        println!(
                            "    {} [{}]: {}",
                            c.path,
                            c.classification,
                            c.explanation
                        );
                    }
                }
            }
        }
        Command::AutoRebase => {
            let repo = Repo::discover(cwd)?;
            let results = repo.auto_rebase_workspaces()?;
            if results.is_empty() {
                println!("no workspaces with auto-rebase enabled");
            } else {
                for result in &results {
                    if result.already_up_to_date {
                        println!("workspace `{}`: up to date", result.workspace);
                    } else {
                        println!(
                            "workspace `{}`: rebased ({} files, {} conflicts)",
                            result.workspace,
                            result.files_merged.len(),
                            result.conflicts.len(),
                        );
                    }
                }
            }
        }
        Command::Conflict { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                ConflictCommand::Detect { workspace } => {
                    let conflicts = repo.detect_conflicts(&workspace)?;
                    if conflicts.is_empty() {
                        println!("no conflicts detected for workspace `{}`", workspace);
                    } else {
                        println!("{} conflict(s) detected:", conflicts.len());
                        for c in &conflicts {
                            println!(
                                "  {} [{}]: {}",
                                c.path,
                                c.classification,
                                c.explanation
                            );
                        }
                    }
                }
                ConflictCommand::Suggest { id } => {
                    let conflict_id = parse_uuid(&id)?;
                    let suggestion = repo.suggest_resolution(conflict_id)?;
                    println!("suggestion: {}", suggestion);
                }
                ConflictCommand::Resolve { id, resolution } => {
                    let conflict_id = parse_uuid(&id)?;
                    repo.resolve_conflict(conflict_id, resolution)?;
                    println!("conflict {} marked as resolved", &id[..8.min(id.len())]);
                }
                ConflictCommand::Verify { id, passed } => {
                    let conflict_id = parse_uuid(&id)?;
                    repo.verify_conflict(conflict_id, passed)?;
                    println!(
                        "conflict {} verification: {}",
                        &id[..8.min(id.len())],
                        if passed { "passed" } else { "failed" }
                    );
                }
                ConflictCommand::Record { id, reason } => {
                    let conflict_id = parse_uuid(&id)?;
                    repo.record_conflict(conflict_id, reason)?;
                    println!("conflict {} recorded", &id[..8.min(id.len())]);
                }
                ConflictCommand::List { status, json } => {
                    let status_filter = status.map(|s| match s {
                        ConflictStatusArg::Detected => ConflictStatus::Detected,
                        ConflictStatusArg::Classified => ConflictStatus::Classified,
                        ConflictStatusArg::Suggested => ConflictStatus::Suggested,
                        ConflictStatusArg::Resolved => ConflictStatus::Resolved,
                        ConflictStatusArg::Verified => ConflictStatus::Verified,
                        ConflictStatusArg::Recorded => ConflictStatus::Recorded,
                    });

                    let conflicts = repo.list_conflicts(status_filter)?;

                    if json {
                        println!("{}", serde_json::to_string_pretty(&conflicts)?);
                    } else if conflicts.is_empty() {
                        println!("no conflicts found");
                    } else {
                        for c in &conflicts {
                            println!(
                                "{}  {}  {}  [{}]  {}",
                                &c.id.to_string()[..8],
                                c.status,
                                c.path,
                                c.classification.as_deref().unwrap_or("-"),
                                c.symbol.as_deref().unwrap_or("-"),
                            );
                        }
                    }
                }
            }
        }
        Command::Migrate { native } => {
            if !native {
                bail!("specify --native to migrate to native block-level storage");
            }
            let repo = Repo::discover(cwd)?;
            let report = repo.migrate_to_native()?;
            println!(
                "migrated {} snapshot{} to native storage",
                report.snapshots_migrated,
                if report.snapshots_migrated == 1 { "" } else { "s" }
            );
            println!(
                "  blocks stored: {}",
                report.blocks_stored
            );
            if report.bytes_before > 0 {
                let ratio = report.bytes_after as f64 / report.bytes_before as f64;
                println!(
                    "  storage: {} -> {} ({:.1}%)",
                    format_bytes(report.bytes_before),
                    format_bytes(report.bytes_after),
                    ratio * 100.0
                );
            }
        }
        Command::Index { clear } => {
            let repo = Repo::discover(cwd)?;
            if clear {
                repo.clear_index()?;
                println!("semantic index cleared");
            } else {
                let report = repo.build_index()?;
                println!(
                    "indexed {} files, {} dependency edges",
                    report.files_indexed, report.edges_computed
                );
            }
        }
        Command::Materialize => {
            let repo = Repo::discover(cwd)?;
            let event_count = repo.materialize()?;
            println!("materialized state at {} events", event_count);
        }
        Command::MigrateEventLog => {
            let repo = Repo::discover(cwd)?;
            let report = repo.migrate_event_log()?;
            println!(
                "migrated {} events into {} segments",
                report.events_migrated, report.segments_created
            );
        }
        Command::Compact { older_than } => {
            let repo = Repo::discover(cwd)?;
            let duration = parse_duration_spec(&older_than)?;
            let report = repo.compact(duration)?;
            println!(
                "compacted event log: {} total, {} retained, {} archived",
                report.total_events, report.retained_events, report.archived_events
            );
        }
        Command::Convert { command } => match command {
            ConvertCommand::FromGit { branch, shallow } => {
                let repo = Repo::at(&cwd);
                let report = repo.convert_from_git(branch, shallow)?;
                println!("{report}");
            }
            ConvertCommand::FromJj { shallow } => {
                let repo = Repo::at(&cwd);
                let report = repo.convert_from_jj(shallow)?;
                println!("{report}");
            }
            ConvertCommand::ToGit { remove_flock } => {
                let repo = Repo::discover(&cwd)?;
                let report = repo.convert_to_git(remove_flock)?;
                println!("{report}");
            }
        },
        Command::Remote { command } => {
            match command {
                RemoteCommand::Login { host, token, ssh_key } => {
                    if ssh_key {
                        let repo = Repo::discover(cwd)?;
                        let result = repo.remote_login_ssh(&host)?;
                        if result.success {
                            println!(
                                "authenticated with {} via SSH key{}",
                                host,
                                result.identity.map(|id| format!(" ({})", id)).unwrap_or_default()
                            );
                        } else {
                            eprintln!(
                                "authentication failed: {}",
                                result.error.as_deref().unwrap_or("unknown error")
                            );
                            std::process::exit(1);
                        }
                    } else {
                        let tok = match token {
                            Some(t) => t,
                            None => {
                                // Read token from stdin.
                                eprint!("token: ");
                                io::stderr().flush()?;
                                let mut buf = String::new();
                                io::stdin().read_line(&mut buf)?;
                                buf.trim().to_string()
                            }
                        };
                        if tok.is_empty() {
                            bail!("token cannot be empty; provide --token or enter interactively");
                        }
                        let repo = Repo::discover(cwd)?;
                        let result = repo.remote_login(&host, &tok)?;
                        if result.success {
                            println!(
                                "authenticated with {}{}",
                                host,
                                result.identity.map(|id| format!(" ({})", id)).unwrap_or_default()
                            );
                        } else {
                            eprintln!(
                                "authentication failed: {}",
                                result.error.as_deref().unwrap_or("unknown error")
                            );
                            std::process::exit(1);
                        }
                    }
                }
                RemoteCommand::Logout { host } => {
                    let repo = Repo::discover(cwd)?;
                    let removed = repo.remote_logout(&host)?;
                    if removed {
                        println!("credentials removed for {}", host);
                    } else {
                        println!("no credentials stored for {}", host);
                    }
                }
                RemoteCommand::Status => {
                    let repo = Repo::discover(cwd)?;
                    let creds = repo.remote_credentials_list()?;
                    if creds.is_empty() {
                        println!("no stored credentials");
                    } else {
                        for c in &creds {
                            println!("{}  ({})", c.host, c.method);
                        }
                    }
                }
            }
        }
        Command::Roost { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                RoostCommand::Add { name, url } => {
                    repo.roost_add(&name, &url)?;
                    println!("roost '{}' added: {}", name, url);
                }
                RoostCommand::Remove { name } => {
                    repo.roost_remove(&name)?;
                    println!("roost '{}' removed", name);
                }
                RoostCommand::List => {
                    let roosts = repo.roost_list()?;
                    if roosts.is_empty() {
                        println!("no roosts configured");
                    } else {
                        for r in &roosts {
                            let synced = r
                                .last_synced_event
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| "never".to_string());
                            println!("{}  {}  (last sync: {})", r.name, r.url, synced);
                        }
                    }
                }
                RoostCommand::SetUrl { name, url } => {
                    repo.roost_set_url(&name, &url)?;
                    println!("roost '{}' url updated to {}", name, url);
                }
            }
        }
        Command::Push { roost, branch } => {
            let repo = Repo::discover(cwd)?;
            let roost_name = roost.as_deref().unwrap_or("origin");
            let report = repo.push(roost_name, branch.as_deref())?;
            if report.rejected {
                eprintln!(
                    "push rejected: {}",
                    report.detail.as_deref().unwrap_or("unknown reason")
                );
                std::process::exit(1);
            } else {
                println!(
                    "pushed {} event(s), {} block(s) to '{}'",
                    report.events_pushed, report.blocks_uploaded, report.roost_name
                );
            }
        }
        Command::Pull { roost, branch } => {
            let repo = Repo::discover(cwd)?;
            let roost_name = roost.as_deref().unwrap_or("origin");
            let report = repo.pull(roost_name, branch.as_deref())?;
            if report.events_pulled == 0 {
                println!("already up to date with '{}'", report.roost_name);
            } else {
                println!(
                    "pulled {} event(s), {} block(s) from '{}'",
                    report.events_pulled, report.blocks_downloaded, report.roost_name
                );
                if !report.refs_updated.is_empty() {
                    println!("refs updated: {}", report.refs_updated.join(", "));
                }
            }
        }
        Command::Clone {
            url,
            dir,
            depth,
            sparse,
            focus,
            lazy,
        } => {
            // Derive directory name from URL if not specified.
            let target_dir = if let Some(d) = dir {
                PathBuf::from(d)
            } else {
                let parsed = fl_core::RemoteUrl::parse(&url)?;
                let name = parsed.path.rsplit('/').next().unwrap_or("repo");
                PathBuf::from(name)
            };

            let report = Repo::clone_from(
                &url,
                &target_dir,
                depth,
                sparse,
                lazy,
                focus.as_deref(),
            )?;

            println!(
                "cloned to '{}': {} event(s), {} block(s)",
                report.clone_dir,
                report.pull.events_pulled,
                report.pull.blocks_downloaded,
            );
            if !report.sparse_patterns.is_empty() {
                println!("sparse patterns: {}", report.sparse_patterns.join(", "));
            }
            if report.lazy {
                println!("lazy mode: blocks fetched on demand");
            }
            if let Some(d) = report.depth {
                println!("shallow depth: {} checkpoints", d);
            }
        }
        Command::Sparse { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                SparseCommand::Add { pattern, roost } => {
                    let roost_name = roost.as_deref().unwrap_or("origin");
                    repo.sparse_add(roost_name, &pattern)?;
                    println!("sparse pattern '{}' added to '{}'", pattern, roost_name);
                }
                SparseCommand::Remove { pattern, roost } => {
                    let roost_name = roost.as_deref().unwrap_or("origin");
                    repo.sparse_remove(roost_name, &pattern)?;
                    println!("sparse pattern '{}' removed from '{}'", pattern, roost_name);
                }
                SparseCommand::List { roost } => {
                    let roost_name = roost.as_deref().unwrap_or("origin");
                    let patterns = repo.sparse_list(roost_name)?;
                    if patterns.is_empty() {
                        println!("no sparse patterns configured for '{}'", roost_name);
                    } else {
                        for p in &patterns {
                            println!("  {}", p);
                        }
                    }
                }
            }
        }
        Command::Fetch {
            deepen,
            resolve_missing,
            roost,
        } => {
            let repo = Repo::discover(cwd)?;
            let roost_name = roost.as_deref().unwrap_or("origin");
            if let Some(n) = deepen {
                let report = repo.fetch_deepen(roost_name, n)?;
                println!(
                    "deepened by {}: pulled {} event(s), {} block(s)",
                    n, report.events_pulled, report.blocks_downloaded
                );
            } else if resolve_missing {
                let count = repo.fetch_resolve_missing(roost_name)?;
                if count == 0 {
                    println!("no missing blocks");
                } else {
                    println!("fetched {} missing block(s)", count);
                }
            } else {
                println!("specify --deepen N or --resolve-missing");
            }
        }
        Command::Pin {
            pattern,
            all,
            list,
            unpin,
            roost,
        } => {
            let repo = Repo::discover(cwd)?;
            let roost_name = roost.as_deref().unwrap_or("origin");
            if list {
                let patterns = repo.pin_list(roost_name)?;
                if patterns.is_empty() {
                    println!("no pinned patterns for '{}'", roost_name);
                } else {
                    for p in &patterns {
                        println!("  {}", p);
                    }
                }
            } else if let Some(ref pat) = unpin {
                repo.pin_remove(roost_name, pat)?;
                println!("unpinned '{}'", pat);
            } else if all {
                let count = repo.pin(roost_name, "**")?;
                println!("pinned all files: fetched {} block(s)", count);
            } else if let Some(ref pat) = pattern {
                let count = repo.pin(roost_name, pat)?;
                println!("pinned '{}': fetched {} block(s)", pat, count);
            } else {
                println!("specify a pattern, --all, --list, or --unpin");
            }
        }
        Command::Watch {
            remote,
            path,
            symbol,
            agent,
            kind,
            json,
        } => {
            let repo = Repo::discover(cwd)?;
            let roost = remote.as_deref().unwrap_or("origin");
            let ws = repo.ws_connect(roost)?;

            // Subscribe with filters
            let filter = fl_core::SubscriptionFilter {
                paths: path,
                symbols: symbol,
                modules: vec![],
            };
            let sub = fl_core::WsSubscribeRequest {
                filter,
                agents: agent,
                event_kinds: kind,
            };
            ws.send(fl_core::WsClientMessage::Subscribe(sub))?;

            // Set up Ctrl+C handler
            let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let r = running.clone();
            ctrlc_flag(&r);

            println!("watching for events (Ctrl+C to stop)...");
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(msg) = ws.recv_timeout(std::time::Duration::from_millis(500)) {
                    if json {
                        println!("{}", serde_json::to_string(&msg)?);
                    } else {
                        print_ws_message(&msg);
                    }
                }
            }
            ws.disconnect();
        }
        Command::EditorServer { remote } => {
            let repo = Repo::discover(cwd)?;
            run_editor_server(&repo, remote.as_deref().unwrap_or("origin"))?;
        }
        Command::Query { text, limit, ai, json } => {
            let repo = Repo::discover(cwd)?;
            let results = repo.query(&text, ai, limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else if results.is_empty() {
                println!("no matching events found");
            } else {
                for (i, r) in results.iter().enumerate() {
                    println!(
                        "{}. [score {:.2}] {} – {}",
                        i + 1,
                        r.relevance,
                        &r.event_id.to_string()[..8],
                        r.snippet.chars().take(80).collect::<String>(),
                    );
                    if let Some(ref explanation) = r.ai_explanation {
                        println!("   AI: {}", explanation);
                    }
                }
            }
        }
        Command::Intel { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                IntelCommand::Rebuild => {
                    let report = repo.rebuild_intelligence_index()?;
                    println!(
                        "indexed {} events, {} unique terms",
                        report.events_indexed, report.terms_indexed
                    );
                }
                IntelCommand::Stats { json } => {
                    let stats = repo.intelligence_index_stats()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&stats)?);
                    } else {
                        println!("documents: {}", stats.document_count);
                        println!("terms:     {}", stats.term_count);
                        println!("size:      {} bytes", stats.index_size_bytes);
                    }
                }
            }
        }
        Command::Confidence { verbose, json } => {
            let repo = Repo::discover(cwd)?;
            let score = repo.calculate_session_confidence()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&score)?);
            } else {
                println!("confidence: {}/100 ({})", score.score, score.recommendation);
                if verbose {
                    println!();
                    for f in &score.factors {
                        println!(
                            "  {:<25} {:>3}/100  (weight {:.0}%)  {}",
                            f.name,
                            f.score,
                            f.weight * 100.0,
                            f.detail
                        );
                    }
                }
            }
        }
        Command::Backup { command } => {
            match command {
                BackupCommand::Create { path } => {
                    let repo = Repo::discover(&cwd)?;
                    fl_core::backup::create_backup(repo.root(), Path::new(&path))?;
                    println!("Backup created: {}", path);
                }
                BackupCommand::Restore { archive, target } => {
                    let target_dir = target.map(PathBuf::from).unwrap_or_else(|| cwd.clone());
                    fl_core::backup::restore_backup(Path::new(&archive), &target_dir)?;
                    println!("Backup restored to {}", target_dir.display());
                }
                BackupCommand::Verify { archive } => {
                    let verification = fl_core::backup::verify_backup(Path::new(&archive))?;
                    println!("Backup verification:");
                    println!("  files: {}", verification.file_count);
                    println!("  total size: {} bytes", verification.total_size);
                    println!("  event log: {}", if verification.has_event_log { "present" } else { "MISSING" });
                    println!("  refs: {}", if verification.has_refs { "present" } else { "MISSING" });
                    println!("  config: {}", if verification.has_config { "present" } else { "MISSING" });
                    if verification.is_complete() {
                        println!("  status: complete");
                    } else {
                        println!("  status: INCOMPLETE");
                    }
                }
            }
        }
        Command::Key { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                KeyCommand::Encrypt { passphrase } => {
                    let pass = passphrase
                        .or_else(|| env::var("FL_KEY_PASSPHRASE").ok())
                        .ok_or_else(|| anyhow::anyhow!("passphrase required (--passphrase or FL_KEY_PASSPHRASE)"))?;
                    repo.encrypt_signing_key(&pass)?;
                    println!("Signing key encrypted.");
                }
                KeyCommand::Decrypt { passphrase } => {
                    let pass = passphrase
                        .or_else(|| env::var("FL_KEY_PASSPHRASE").ok())
                        .ok_or_else(|| anyhow::anyhow!("passphrase required (--passphrase or FL_KEY_PASSPHRASE)"))?;
                    repo.decrypt_signing_key(&pass)?;
                    println!("Signing key decrypted.");
                }
                KeyCommand::Status => {
                    let status = repo.key_status();
                    match status {
                        fl_core::key_crypto::KeyStatus::None => println!("No signing key found."),
                        fl_core::key_crypto::KeyStatus::Plaintext => println!("Signing key: plaintext"),
                        fl_core::key_crypto::KeyStatus::Encrypted => println!("Signing key: encrypted"),
                    }
                }
            }
        }
        Command::Audit { json } => {
            let repo = Repo::discover(cwd)?;
            let report = repo.audit()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Audit Report ({} events)", report.total_events);
                println!();
                println!("Signing Identities:");
                if report.signing_identities.is_empty() {
                    println!("  (none)");
                }
                for id in &report.signing_identities {
                    println!(
                        "  {}...{}: {} events (first: {}, last: {})",
                        &id.public_key[..8.min(id.public_key.len())],
                        &id.public_key[id.public_key.len().saturating_sub(8)..],
                        id.event_count,
                        id.first_seen,
                        id.last_seen
                    );
                }
                if !report.timeline_gaps.is_empty() {
                    println!();
                    println!("Timeline Gaps (>24h):");
                    for gap in &report.timeline_gaps {
                        println!(
                            "  {:.1}h gap between {} and {}",
                            gap.gap_hours, gap.from_event_id, gap.to_event_id
                        );
                    }
                }
                if !report.anomalies.is_empty() {
                    println!();
                    println!("Anomalies:");
                    for anomaly in &report.anomalies {
                        println!(
                            "  [{}] {}: {}",
                            anomaly.kind, anomaly.event_id, anomaly.detail
                        );
                    }
                }
                if report.anomalies.is_empty() && report.timeline_gaps.is_empty() {
                    println!();
                    println!("No anomalies detected.");
                }
            }
        }
        Command::Policy { command } => match command {
            PolicyCommand::Show => {
                let repo = Repo::discover(cwd)?;
                let config = fl_core::load_policies_config(repo.root());
                println!("Agent Governance Policies");
                println!("========================");
                println!();
                println!("[scope]");
                println!("  enabled = {}", config.scope.enabled);
                println!("  enforce_mode = {:?}", config.scope.enforce_mode);
                println!("  scope_mode = {:?}", config.scope.scope_mode);
                println!();
                println!("[budget]");
                println!("  enabled = {}", config.budget.enabled);
                if let Some(v) = config.budget.max_files_per_task {
                    println!("  max_files_per_task = {}", v);
                }
                if let Some(v) = config.budget.max_files_per_exploration {
                    println!("  max_files_per_exploration = {}", v);
                }
                if let Some(v) = config.budget.max_lines_per_task {
                    println!("  max_lines_per_task = {}", v);
                }
                println!("  on_exceed = {:?}", config.budget.on_exceed);
                println!();
                println!("[rate_limits]");
                println!("  enabled = {}", config.rate_limits.enabled);
                if let Some(v) = config.rate_limits.max_explorations_per_task {
                    println!("  max_explorations_per_task = {}", v);
                }
                if let Some(v) = config.rate_limits.max_undos_per_exploration {
                    println!("  max_undos_per_exploration = {}", v);
                }
                if let Some(v) = config.rate_limits.max_checkpoints_per_window {
                    println!("  max_checkpoints_per_window = {}", v);
                }
                println!("  window_secs = {}", config.rate_limits.window_secs);
                println!("  on_exceed = {:?}", config.rate_limits.on_exceed);
            }
            PolicyCommand::Init => {
                let repo = Repo::discover(cwd)?;
                let path = repo.root().join(".flock/policies.toml");
                if path.exists() {
                    bail!("policies.toml already exists at {}", path.display());
                }
                std::fs::write(&path, fl_core::DEFAULT_POLICIES_TOML)?;
                println!("Created {}", path.display());
            }
            PolicyCommand::Audit { task, exploration } => {
                let repo = Repo::discover(cwd)?;
                let state = repo.replay_state()?;

                let task_filter = task.map(|s| {
                    Uuid::parse_str(&s).unwrap_or_else(|_| {
                        eprintln!("warning: invalid task UUID '{}', showing all", s);
                        Uuid::nil()
                    })
                });
                let exploration_filter = exploration.map(|s| {
                    Uuid::parse_str(&s).unwrap_or_else(|_| {
                        eprintln!("warning: invalid exploration UUID '{}', showing all", s);
                        Uuid::nil()
                    })
                });

                let decisions: Vec<_> = state
                    .policy_decisions
                    .iter()
                    .filter(|d| {
                        if let Some(tid) = task_filter {
                            if !tid.is_nil() && d.task_id != Some(tid) {
                                return false;
                            }
                        }
                        if let Some(eid) = exploration_filter {
                            if !eid.is_nil() && d.exploration_id != Some(eid) {
                                return false;
                            }
                        }
                        true
                    })
                    .collect();

                if decisions.is_empty() {
                    println!("No policy decisions recorded.");
                } else {
                    println!("Policy Decisions ({} total)", decisions.len());
                    println!();
                    for d in &decisions {
                        println!(
                            "  {} {} [{}] verdict={} op={}",
                            d.timestamp, d.policy_name, d.policy_category, d.verdict, d.operation
                        );
                        if let Some(reason) = &d.reason {
                            println!("    reason: {}", reason);
                        }
                        if let Some(tid) = d.task_id {
                            println!("    task: {}", tid);
                        }
                        if let Some(eid) = d.exploration_id {
                            println!("    exploration: {}", eid);
                        }
                        if !d.affected_files.is_empty() {
                            println!("    files: {}", d.affected_files.join(", "));
                        }
                    }
                }
            }
        },
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "fl", &mut io::stdout());
            io::stdout().flush()?;
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

fn print_semantic_diffs(
    diffs: &[fl_core::SemanticFileDiff],
    json: bool,
) -> Result<()> {
    if diffs.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No semantic changes.");
        }
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&diffs)?);
        return Ok(());
    }
    for diff in diffs {
        print_semantic_file_diff(diff);
    }
    Ok(())
}

fn print_intent_diff(
    groups: &[(String, Vec<fl_core::SemanticFileDiff>)],
    json: bool,
) -> Result<()> {
    if groups.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No semantic changes.");
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
    for (intent_label, files) in groups {
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
    Ok(())
}

fn print_full_diff_summary(
    summary: &fl_core::repo::FileSummary,
    diffs: &[fl_core::SemanticFileDiff],
    json: bool,
) -> Result<()> {
    if json {
        let output = serde_json::json!({
            "files": {
                "added": summary.added,
                "modified": summary.modified,
                "deleted": summary.deleted,
            },
            "semantic_changes": diffs,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // File-level summary
    let total = summary.added.len() + summary.modified.len() + summary.deleted.len();
    if total == 0 && diffs.is_empty() {
        println!("No changes.");
        return Ok(());
    }

    println!("=== File Summary ===");
    for f in &summary.added {
        println!("  + {}", f);
    }
    for f in &summary.modified {
        println!("  ~ {}", f);
    }
    for f in &summary.deleted {
        println!("  - {}", f);
    }
    println!(
        "{} added, {} modified, {} deleted\n",
        summary.added.len(),
        summary.modified.len(),
        summary.deleted.len()
    );

    // Semantic changes
    if !diffs.is_empty() {
        println!("=== Semantic Changes ===");
        for diff in diffs {
            print_semantic_file_diff(diff);
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

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn print_exploration_tree(explorations: &[fl_core::ExplorationSummary]) {
    // Group by status
    let mut active = Vec::new();
    let mut promoted = Vec::new();
    let mut abandoned = Vec::new();

    for exp in explorations {
        match exp.status {
            ExplorationStatus::Active => active.push(exp),
            ExplorationStatus::Promoted => promoted.push(exp),
            ExplorationStatus::Abandoned => abandoned.push(exp),
        }
    }

    println!(
        "Explorations ({} total: {} active, {} promoted, {} abandoned)",
        explorations.len(),
        active.len(),
        promoted.len(),
        abandoned.len()
    );
    println!();

    // Group explorations by base checkpoint to show tree structure
    let mut by_base: HashMap<Option<Uuid>, Vec<&fl_core::ExplorationSummary>> = HashMap::new();
    for exp in explorations {
        by_base
            .entry(exp.base_checkpoint_event)
            .or_default()
            .push(exp);
    }

    // Print tree rooted at each base checkpoint
    let mut bases: Vec<_> = by_base.keys().copied().collect();
    bases.sort_by_key(|b| b.map(|id| id.to_string()).unwrap_or_default());

    for (base_idx, base) in bases.iter().enumerate() {
        let base_label = base
            .map(|id| format!("commit {}", &id.to_string()[..8]))
            .unwrap_or_else(|| "no base".to_string());
        let is_last_base = base_idx == bases.len() - 1;
        let base_prefix = if is_last_base { "└── " } else { "├── " };
        let child_prefix = if is_last_base { "    " } else { "│   " };

        println!("{}{}", base_prefix, base_label);

        let children = &by_base[base];
        for (i, exp) in children.iter().enumerate() {
            let is_last = i == children.len() - 1;
            let connector = if is_last { "└── " } else { "├── " };
            let status_icon = match exp.status {
                ExplorationStatus::Active => "*",
                ExplorationStatus::Promoted => "^",
                ExplorationStatus::Abandoned => "x",
            };
            println!(
                "{}{}[{}] {} ({})",
                child_prefix,
                connector,
                status_icon,
                exp.title,
                &exp.id.to_string()[..8]
            );
        }
    }

    println!();
    println!("Legend: [*] active  [^] promoted  [x] abandoned");
}

fn print_task_graph(graph: &fl_core::TaskGraph) {
    // Build adjacency: for each task, find what it depends on and what depends on it
    let task_map: HashMap<Uuid, &fl_core::TaskSummary> =
        graph.tasks.iter().map(|t| (t.id, t)).collect();

    // Find root tasks (no dependencies)
    let roots: Vec<&fl_core::TaskSummary> = graph
        .tasks
        .iter()
        .filter(|t| t.dependencies.is_empty() && t.discovered_from.is_none())
        .collect();

    // Build children map from edges
    let mut children_map: HashMap<Uuid, Vec<(Uuid, &TaskRelation)>> = HashMap::new();
    for edge in &graph.edges {
        children_map
            .entry(edge.to_task)
            .or_default()
            .push((edge.from_task, &edge.relation));
    }

    let status_counts = graph.tasks.iter().fold([0u32; 4], |mut acc, t| {
        match t.status {
            TaskStatus::Open => acc[0] += 1,
            TaskStatus::Claimed => acc[1] += 1,
            TaskStatus::Completed => acc[2] += 1,
            TaskStatus::Failed => acc[3] += 1,
        }
        acc
    });
    println!(
        "Task Graph ({} total: {} open, {} claimed, {} completed, {} failed)",
        graph.tasks.len(),
        status_counts[0],
        status_counts[1],
        status_counts[2],
        status_counts[3]
    );
    println!();

    if roots.is_empty() {
        // No clear roots, just list all tasks with markers
        for task in &graph.tasks {
            print_task_node(task, "", true, &children_map, &task_map, &mut Vec::new());
        }
    } else {
        for (i, root) in roots.iter().enumerate() {
            let is_last = i == roots.len() - 1;
            print_task_node(root, "", is_last, &children_map, &task_map, &mut Vec::new());
        }

        // Print orphans (tasks that aren't reachable from roots but aren't roots themselves)
        let mut printed: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        collect_reachable(&roots, &children_map, &mut printed);
        let orphans: Vec<_> = graph
            .tasks
            .iter()
            .filter(|t| !printed.contains(&t.id))
            .collect();
        if !orphans.is_empty() {
            println!();
            println!("Unlinked tasks:");
            for (i, task) in orphans.iter().enumerate() {
                let is_last = i == orphans.len() - 1;
                print_task_node(task, "", is_last, &children_map, &task_map, &mut Vec::new());
            }
        }
    }

    println!();
    println!("Legend: [ ] open  [>] claimed  [x] completed  [!] failed");
    println!("Edges:  ── depends-on  ~~ discovered-from");
}

fn collect_reachable(
    roots: &[&fl_core::TaskSummary],
    children_map: &HashMap<Uuid, Vec<(Uuid, &TaskRelation)>>,
    visited: &mut std::collections::HashSet<Uuid>,
) {
    for root in roots {
        if visited.insert(root.id) {
            if let Some(children) = children_map.get(&root.id) {
                // We don't have the task objects for children, just mark IDs
                for (child_id, _) in children {
                    visited.insert(*child_id);
                }
            }
        }
    }
}

fn print_task_node(
    task: &fl_core::TaskSummary,
    prefix: &str,
    is_last: bool,
    children_map: &HashMap<Uuid, Vec<(Uuid, &TaskRelation)>>,
    task_map: &HashMap<Uuid, &fl_core::TaskSummary>,
    visited: &mut Vec<Uuid>,
) {
    // Prevent cycles
    if visited.contains(&task.id) {
        return;
    }
    visited.push(task.id);

    let connector = if prefix.is_empty() && is_last && visited.len() <= 1 {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };

    let marker = match task.status {
        TaskStatus::Open => " ",
        TaskStatus::Claimed => ">",
        TaskStatus::Completed => "x",
        TaskStatus::Failed => "!",
    };

    let assignee_str = task
        .assignee
        .as_deref()
        .map(|a| format!(" @{}", a))
        .unwrap_or_default();

    println!(
        "{}{}[{}] {} ({}){}",
        prefix,
        connector,
        marker,
        task.title,
        &task.id.to_string()[..8],
        assignee_str
    );

    // Print children
    if let Some(children) = children_map.get(&task.id) {
        let child_prefix = if prefix.is_empty() && connector.is_empty() {
            String::new()
        } else {
            format!(
                "{}{}",
                prefix,
                if is_last { "    " } else { "│   " }
            )
        };

        for (i, (child_id, relation)) in children.iter().enumerate() {
            let is_last_child = i == children.len() - 1;
            let edge_symbol = match relation {
                TaskRelation::DependsOn => "──",
                TaskRelation::DiscoveredFrom => "~~",
            };

            if let Some(child_task) = task_map.get(child_id) {
                let child_connector = if is_last_child {
                    format!("└{} ", edge_symbol)
                } else {
                    format!("├{} ", edge_symbol)
                };

                let child_marker = match child_task.status {
                    TaskStatus::Open => " ",
                    TaskStatus::Claimed => ">",
                    TaskStatus::Completed => "x",
                    TaskStatus::Failed => "!",
                };

                let child_assignee = child_task
                    .assignee
                    .as_deref()
                    .map(|a| format!(" @{}", a))
                    .unwrap_or_default();

                println!(
                    "{}{}[{}] {} ({}){}",
                    child_prefix,
                    child_connector,
                    child_marker,
                    child_task.title,
                    &child_task.id.to_string()[..8],
                    child_assignee
                );

                // Recurse into grandchildren
                if children_map.contains_key(child_id) {
                    let grandchild_prefix = format!(
                        "{}{}",
                        child_prefix,
                        if is_last_child { "    " } else { "│   " }
                    );
                    if let Some(grandchildren) = children_map.get(child_id) {
                        for (j, (gc_id, _gc_rel)) in grandchildren.iter().enumerate() {
                            let is_last_gc = j == grandchildren.len() - 1;
                            if let Some(gc_task) = task_map.get(gc_id) {
                                print_task_node(
                                    gc_task,
                                    &grandchild_prefix,
                                    is_last_gc,
                                    children_map,
                                    task_map,
                                    visited,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
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

fn print_ws_message(msg: &fl_core::WsServerMessage) {
    use fl_core::WsServerMessage;
    match msg {
        WsServerMessage::AuthResult { success, identity, error } => {
            if *success {
                println!("authenticated as {}", identity.as_deref().unwrap_or("unknown"));
            } else {
                eprintln!("auth failed: {}", error.as_deref().unwrap_or("unknown"));
            }
        }
        WsServerMessage::Pong { .. } => {}
        WsServerMessage::Subscribed { subscription_id, .. } => {
            println!("subscribed (id: {})", subscription_id);
        }
        WsServerMessage::EventNotification { event, .. } => {
            println!("[{}] {} {}", event.timestamp, event.actor, event.kind_name());
        }
        WsServerMessage::PresenceUpdate { actor, workspace, files, intent, departed, .. } => {
            if *departed {
                println!("presence: {} departed from {}", actor, workspace);
            } else {
                let intent_str = intent.as_deref().unwrap_or("");
                println!("presence: {} in {} editing {} {}", actor, workspace, files.join(", "), intent_str);
            }
        }
        WsServerMessage::HeadsUpWarning { actor, symbol, path, action } => {
            println!("heads-up: {} is {} {} in {}", actor, action, symbol, path);
        }
        WsServerMessage::ConflictForecast { symbol, path, local_change, remote_change, remote_actor } => {
            println!(
                "conflict forecast: {} in {} — local:{} vs {}:{}",
                symbol, path, local_change, remote_actor, remote_change
            );
        }
        WsServerMessage::TaskUpdate { task_id, title, status, assignee } => {
            let assignee_str = assignee.as_deref().map(|a| format!(" @{}", a)).unwrap_or_default();
            println!("task: {} [{}] {}{}", &task_id[..8.min(task_id.len())], status, title, assignee_str);
        }
        WsServerMessage::Error { code, message } => {
            eprintln!("error [{}]: {}", code, message);
        }
    }
}

fn stream_live_task_updates(repo: &Repo, roost: &str, json: bool) -> Result<()> {
    let ws = repo.ws_connect(roost)?;
    let sub = fl_core::WsSubscribeRequest {
        filter: fl_core::SubscriptionFilter {
            paths: vec![],
            symbols: vec![],
            modules: vec![],
        },
        agents: vec![],
        event_kinds: vec!["Task".to_string()],
    };
    ws.send(fl_core::WsClientMessage::Subscribe(sub))?;

    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    ctrlc_flag(&r);

    println!("streaming task updates (Ctrl+C to stop)...");
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        if let Some(msg) = ws.recv_timeout(std::time::Duration::from_millis(500)) {
            if json {
                println!("{}", serde_json::to_string(&msg).unwrap_or_default());
            } else {
                print_ws_message(&msg);
            }
        }
    }
    ws.disconnect();
    Ok(())
}

fn ctrlc_flag(flag: &std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let f = flag.clone();
    let _ = ctrlc::set_handler(move || {
        f.store(false, std::sync::atomic::Ordering::Relaxed);
    });
}

fn run_editor_server(repo: &Repo, roost: &str) -> Result<()> {
    use fl_core::editor_protocol::{EditorRequest, EditorSession, EditorNotification};
    use std::io::BufRead;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut session = EditorSession::new();

    // Try to connect WebSocket (non-fatal if it fails — editor still works locally)
    let ws = repo.ws_connect(roost).ok();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }

        let request: EditorRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err = EditorNotification::Error {
                    message: format!("parse error: {}", e),
                };
                writeln!(stdout, "{}", serde_json::to_string(&err)?)?;
                stdout.flush()?;
                continue;
            }
        };

        let is_shutdown = matches!(&request, EditorRequest::Shutdown);

        if let Some(response) = session.handle_request(&request) {
            writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
            stdout.flush()?;
        }

        // Forward presence/warnings from WebSocket if connected
        if let Some(ref ws) = ws {
            while let Some(msg) = ws.try_recv() {
                if let Some(notification) = ws_message_to_editor_notification(&msg, &session) {
                    writeln!(stdout, "{}", serde_json::to_string(&notification)?)?;
                }
            }
            stdout.flush()?;
        }

        if is_shutdown {
            if let Some(ref ws) = ws {
                ws.disconnect();
            }
            break;
        }
    }
    Ok(())
}

fn ws_message_to_editor_notification(
    msg: &fl_core::WsServerMessage,
    session: &fl_core::editor_protocol::EditorSession,
) -> Option<fl_core::editor_protocol::EditorNotification> {
    use fl_core::editor_protocol::{EditorNotification, PresenceEntry};
    use fl_core::WsServerMessage;

    match msg {
        WsServerMessage::PresenceUpdate { actor, files, symbols, intent, departed, .. } => {
            if *departed {
                return None;
            }
            // Only notify for files the editor has open
            let relevant_files: Vec<_> = files
                .iter()
                .filter(|f| session.should_notify_for_path(f))
                .collect();
            if relevant_files.is_empty() {
                return None;
            }
            let entries = relevant_files
                .into_iter()
                .map(|f| PresenceEntry {
                    actor: actor.clone(),
                    file: f.clone(),
                    symbol: symbols.first().cloned(),
                    intent: intent.clone(),
                })
                .collect();
            Some(EditorNotification::PresenceOverlay { entries })
        }
        WsServerMessage::HeadsUpWarning { actor, symbol, path, action } => {
            if !session.should_notify_for_path(path) {
                return None;
            }
            Some(EditorNotification::HeadsUpWarning {
                actor: actor.clone(),
                symbol: symbol.clone(),
                path: path.clone(),
                action: action.clone(),
            })
        }
        WsServerMessage::ConflictForecast { symbol, path, local_change, remote_change, remote_actor } => {
            if !session.should_notify_for_path(path) {
                return None;
            }
            Some(EditorNotification::ConflictForecastWarning {
                symbol: symbol.clone(),
                path: path.clone(),
                local_change: local_change.clone(),
                remote_change: remote_change.clone(),
                remote_actor: remote_actor.clone(),
            })
        }
        WsServerMessage::TaskUpdate { task_id, title, .. } => {
            Some(EditorNotification::TaskReady {
                task_id: task_id.clone(),
                title: title.clone(),
            })
        }
        _ => None,
    }
}
