use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{self, Shell};
use colored::Colorize;
use fl_core::repo::parse_duration_spec;
use fl_core::{
    ApiCallRecord, ConflictStatus, DecisionAction, DirectiveKind, EventKind, ExplorationStatus,
    GateCondition, GatePolicy, RefKind, Repo, SemanticChangeKind, SemanticCompatibilityStatus,
    SemanticConflictClassification, SemanticRisk, TaskRelation, TaskStatus,
    UndoRequest, UndoScope,
};
use uuid::Uuid;

const SKILL_MD: &str = include_str!("../../../.claude/skills/flock/SKILL.md");
const WORKFLOWS_MD: &str = include_str!("../../../.claude/skills/flock/WORKFLOWS.md");
const COLLABORATION_MD: &str = include_str!("../../../.claude/skills/flock/COLLABORATION.md");

#[derive(Debug, Parser)]
#[command(
    name = "fl",
    about = "Flock — version control for AI agents",
    long_about = "Flock — version control for AI agents\n\nShowing common commands only. Run `fl help <command>` for any command.\nAll commands: init, commit, log, status, diff, blame, stash, explore, undo,\n  merge, review, impact, record, refs, git, workspace, push, pull, clone, fetch,\n  session, task, ready, who, directive, preview, quick-save, quick-restore,\n  confidence, presence, lock, subscribe, unsubscribe, subscriptions, gate,\n  rebase, auto-rebase, conflict, remote, roost, sparse, pin, watch,\n  editor-server, query, intel, migrate, index, materialize, migrate-event-log,\n  compact, convert, backup, key, audit, policy, install-skill, completions, fsck",
    version,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    // ── Core Commands ────────────────────────────────────────────────
    /// Initialize a new Flock repository
    
    Init {
        /// Use git-colocated mode (.git + .flock sidecar)
        #[arg(long)]
        colocated: bool,
        /// Use native block-level storage
        #[arg(long)]
        native: bool,
        /// Re-initialize even if .flock already exists
        #[arg(long)]
        force: bool,
    },
    /// Create a commit (save current state)
    #[command(alias = "checkpoint")]
    Commit {
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// Show full UUIDs and hashes in output
        #[arg(short, long)]
        verbose: bool,
        /// Bypass secret detection (recorded in audit log)
        #[arg(long)]
        allow_secrets: bool,
        /// Skip hook execution (recorded in audit log)
        #[arg(long)]
        skip_hooks: bool,
        /// Change category (bugfix, feature, refactor, test, docs, style, chore)
        #[arg(long)]
        category: Option<String>,
        /// Scope label for the change
        #[arg(long)]
        scope: Option<String>,
        /// Confidence level (high, medium, low)
        #[arg(long)]
        confidence: Option<String>,
        /// Structured description of the change
        #[arg(long)]
        description: Option<String>,
    },
    /// Show the event log
    
    Log {
        /// Maximum number of entries to show
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Filter by event type (e.g. commit, init, exploration, undo)
        #[arg(long = "type")]
        event_type: Option<String>,
        /// Show raw nanosecond timestamps instead of human-readable dates
        #[arg(long)]
        raw: bool,
    },
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
        /// Show changes since a duration ago (e.g. 1h, 2d, 1w)
        #[arg(long)]
        since: Option<String>,
        /// Filter by minimum risk level (low, medium, high)
        #[arg(long)]
        risk: Option<String>,
        /// Filter by change kind (added, removed, modified, renamed, moved, style_only)
        #[arg(long)]
        kind: Option<String>,
        /// Show only breaking changes
        #[arg(long)]
        breaking_only: bool,
        /// One-line summary output
        #[arg(long)]
        summary: bool,
        /// Show source body of each change (patch mode)
        #[arg(long)]
        patch: bool,
        /// Show traditional unified text diff instead of semantic output
        #[arg(long)]
        text: bool,
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
        /// Only show changes from checkpoints within this time window (e.g. 1h, 2d)
        #[arg(long)]
        since: Option<String>,
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
        /// Undo to the previous checkpoint boundary (coarse granularity)
        #[arg(long)]
        to_checkpoint: bool,
        /// Scope undo to an exploration (ID or prefix)
        #[arg(long)]
        exploration: Option<String>,
        /// Scope undo to a session (ID or prefix)
        #[arg(long)]
        session: Option<String>,
        /// Scope undo to a workspace
        #[arg(long)]
        workspace: Option<String>,
        /// Scope undo to an actor
        #[arg(long)]
        actor: Option<String>,
    },
    /// Record specific file changes to the event log (native mode)
    
    Record {
        /// Files to record (if empty, records all changes)
        paths: Vec<String>,
    },
    /// Show per-line attribution (who changed each line and when)
    
    Blame {
        /// File path to annotate
        path: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Temporarily stash working directory changes
    
    Stash {
        #[command(subcommand)]
        command: Option<StashCommand>,
    },

    // ── Branching & Refs ─────────────────────────────────────────────
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

    // ── Agent Workflow ────────────────────────────────────────────────
    /// Agent session tracking and provenance
    #[command(hide = true)]
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Task graph management
    #[command(hide = true)]
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Show tasks ready to be claimed
    #[command(hide = true)]
    Ready {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Stream live updates via WebSocket
        #[arg(long)]
        live: bool,
    },
    /// Show active actors and what they're working on
    #[command(hide = true)]
    Who {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Agent directive management (pause, resume, redirect, abort)
    #[command(hide = true)]
    Directive {
        #[command(subcommand)]
        command: DirectiveCommand,
    },
    /// Stream workspace diffs for ghost text preview
    #[command(hide = true)]
    Preview {
        /// Workspace name
        #[arg(long)]
        workspace: Option<String>,
        /// Preview interval in milliseconds
        #[arg(long, default_value = "2000")]
        interval: u64,
        /// Roost name for WebSocket (defaults to "origin")
        #[arg(long)]
        remote: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Quick commit for agents
    #[command(hide = true)]
    QuickSave {
        #[arg(long)]
        tag: Option<String>,
        /// Show semantic diff of the saved changes
        #[arg(long)]
        show_diff: bool,
    },
    /// Restore to last quick-save
    #[command(hide = true)]
    QuickRestore,
    /// Show session confidence score
    #[command(hide = true)]
    Confidence {
        /// Show detailed factor breakdown
        #[arg(long)]
        verbose: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    // ── Collaboration ────────────────────────────────────────────────
    /// Multi-agent presence tracking
    #[command(hide = true)]
    Presence {
        #[command(subcommand)]
        command: PresenceCommand,
    },
    /// Advisory resource locking
    #[command(hide = true)]
    Lock {
        #[command(subcommand)]
        command: LockCommand,
    },
    /// Subscribe to changes on paths, symbols, or modules
    #[command(hide = true)]
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
    #[command(hide = true)]
    Unsubscribe {
        id: String,
    },
    /// List active subscriptions
    #[command(hide = true)]
    Subscriptions {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Human-in-the-loop quality gates
    #[command(hide = true)]
    Gate {
        #[command(subcommand)]
        command: GateCommand,
    },
    /// Rebase a workspace onto the latest checkpoint
    #[command(hide = true)]
    Rebase {
        /// Workspace to rebase
        #[arg(long)]
        workspace: String,
    },
    /// Auto-rebase all workspaces with auto_rebase enabled
    #[command(hide = true)]
    AutoRebase,
    /// Conflict resolution workflow
    #[command(hide = true)]
    Conflict {
        #[command(subcommand)]
        command: ConflictCommand,
    },

    // ── Remote ───────────────────────────────────────────────────────
    /// Manage remote authentication
    #[command(hide = true)]
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Manage roosts (Flock remotes)
    #[command(hide = true)]
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
    #[command(alias = "hatch")]
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
    /// Fetch additional history or resolve missing blocks
    #[command(hide = true)]
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
    /// Stream live events from a remote via WebSocket
    #[command(hide = true)]
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

    // ── Admin ────────────────────────────────────────────────────────
    /// Migrate repository storage backend
    #[command(hide = true)]
    Migrate {
        /// Migrate to native block-level storage
        #[arg(long)]
        native: bool,
    },
    /// Rebuild or clear the semantic index (AST cache + dependency graph)
    #[command(hide = true)]
    Index {
        /// Clear all cached semantic data instead of rebuilding
        #[arg(long)]
        clear: bool,
    },
    /// Materialize replay state for faster future operations
    #[command(hide = true)]
    Materialize,
    /// Migrate event log to segmented format
    #[command(hide = true)]
    MigrateEventLog,
    /// Compact the event log by archiving old events
    #[command(hide = true)]
    Compact {
        /// Archive events older than this duration (e.g. 180d, 30d, 1y)
        #[arg(long, default_value = "180d")]
        older_than: String,
    },
    /// Convert a repository to/from Flock format
    #[command(hide = true)]
    Convert {
        #[command(subcommand)]
        command: ConvertCommand,
    },
    /// Manage sparse checkout patterns
    #[command(hide = true)]
    Sparse {
        #[command(subcommand)]
        command: SparseCommand,
    },
    /// Pin files for offline access (eagerly fetch blocks)
    #[command(hide = true)]
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
    /// Start editor plugin protocol server (JSON-lines over stdin/stdout)
    #[command(hide = true)]
    EditorServer {
        /// Roost name for WebSocket connection (defaults to "origin")
        #[arg(long)]
        remote: Option<String>,
    },
    /// Search event history using natural language
    #[command(hide = true)]
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
    #[command(name = "intel", hide = true)]
    Intel {
        #[command(subcommand)]
        command: IntelCommand,
    },
    /// Backup and restore .flock data
    #[command(hide = true)]
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Manage signing key encryption
    #[command(hide = true)]
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Audit the event log for security anomalies
    #[command(hide = true)]
    Audit {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Agent governance policy management
    #[command(hide = true)]
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Install the Flock skill for Claude Code
    #[command(hide = true)]
    InstallSkill {
        /// Install to ~/.claude/skills/ (default)
        #[arg(long)]
        claude: bool,
        /// Install to <cwd>/.claude/skills/ (project-local)
        #[arg(long)]
        project: bool,
        /// Install to a custom directory
        #[arg(long)]
        dir: Option<String>,
    },
    /// Generate shell completions for bash, zsh, fish, or powershell
    #[command(hide = true)]
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
        /// Title for the exploration
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
    /// Prune abandoned explorations
    Prune {
        #[arg(long, default_value = "0s")]
        older_than: String,
        /// Skip confirmation prompt and prune immediately
        #[arg(long)]
        force: bool,
    },
    /// Show visual exploration tree grouped by base checkpoint
    Tree,
}

#[derive(Debug, Subcommand)]
enum StashCommand {
    /// Save working directory changes and revert to last commit
    Push {
        /// Optional description for the stash entry
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
    },
    /// Restore the most recent stash (or a specific one)
    Pop {
        /// Stash index to restore (default: 0, the most recent)
        index: Option<usize>,
    },
    /// List all stash entries
    List,
    /// Remove a stash entry without applying it
    Drop {
        /// Stash index to drop (default: 0, the most recent)
        index: Option<usize>,
    },
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
    /// List all refs (branches, tags, workspaces)
    List,
    /// Create or update a ref to point at a target event
    Set {
        kind: RefKindArg,
        name: String,
        target: String,
        #[arg(long)]
        auto_rebase: bool,
    },
    /// Delete a ref
    Delete {
        kind: RefKindArg,
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Create a new workspace
    Create {
        name: String,
        #[arg(long)]
        auto_rebase: bool,
    },
    /// Delete a workspace
    Delete {
        name: String,
    },
    /// Rename a workspace
    Rename {
        old_name: String,
        new_name: String,
    },
    /// List all workspaces
    List,
    /// Show workspace details
    Info {
        name: String,
    },
    /// View or set workspace resource limits
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
    /// Start a new agent session
    Start {
        #[arg(long)]
        task: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        initiator: Option<String>,
    },
    /// List sessions
    List {
        #[arg(long)]
        active: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show session details
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Link a session to an exploration
    Link {
        session_id: String,
        exploration_id: String,
    },
    /// Record a decision about an exploration
    Decision {
        session_id: String,
        exploration_id: String,
        /// Action taken: kept, discarded, or any freeform string
        #[arg(long)]
        action: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "0.9")]
        confidence: f64,
    },
    /// Record resource usage for a session
    Usage {
        session_id: String,
        #[arg(long)]
        tokens: Option<u64>,
        #[arg(long)]
        runtime_ms: Option<u64>,
        #[arg(long = "api-call")]
        api_call: Vec<String>,
    },
    /// Mark a session as completed
    Complete {
        id: String,
        #[arg(long)]
        result: Option<String>,
    },
    /// Mark a session as failed
    Fail {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Show provenance chain for an exploration
    Provenance {
        exploration_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Replay session events
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
        /// Allowed path patterns for scope enforcement (glob)
        #[arg(long = "scope")]
        scope: Vec<String>,
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
    /// Send a heartbeat to announce active presence
    Heartbeat {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        file: Vec<String>,
        /// Active symbols (method/function names)
        #[arg(long)]
        symbol: Vec<String>,
        #[arg(long)]
        intent: Option<String>,
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Signal departure from a workspace
    Depart {
        #[arg(long)]
        workspace: String,
    },
    /// List active agents and their presence info
    List,
}

#[derive(Debug, Subcommand)]
enum DirectiveCommand {
    /// Send a directive to an agent
    Send {
        /// Target actor name
        target: String,
        /// Directive type: pause, resume, redirect, abort
        #[arg(long)]
        kind: String,
        /// Reason for the directive
        #[arg(long)]
        reason: Option<String>,
        /// New task (required for redirect)
        #[arg(long)]
        new_task: Option<String>,
    },
    /// List directives
    List {
        /// Filter by target actor
        #[arg(long)]
        actor: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Listen for directives targeting this actor
    Listen {
        /// Actor name to listen for (defaults to current actor)
        #[arg(long)]
        actor: Option<String>,
        /// Filter by directive kind (pause, resume, redirect, abort)
        #[arg(long)]
        kind: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum LockCommand {
    /// Acquire a lock on a resource
    Acquire {
        resource: String,
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// List active locks
    List,
    /// Release a held lock
    Release {
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum GateCommand {
    /// Create a new review gate
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
    /// List review gates
    List {
        #[arg(long)]
        json: bool,
    },
    /// Check if a path is gated
    Check {
        path: String,
    },
    /// Approve a gated change
    Approve {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reject a gated change
    Reject {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Delete a review gate
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
    // Respect NO_COLOR standard (https://no-color.org/)
    if env::var_os("NO_COLOR").is_some() {
        colored::control::set_override(false);
    }

    let cli = Cli::parse();
    let cwd = env::current_dir()?;

    match cli.command {
        Command::Init { colocated, native, force } => {
            let repo = Repo::at(&cwd);
            // Check if .flock already exists
            if repo.flock_dir().is_dir() && !force {
                eprintln!(
                    "{} Flock repository already exists at {}",
                    "warning:".yellow().bold(),
                    repo.flock_dir().display()
                );
                eprintln!("Use {} to re-initialize.", "--force".bold());
                std::process::exit(1);
            }
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
                "{} Flock repository in {}",
                "Initialized".green().bold(),
                format!("{}/.flock", repo.root().display()).bold()
            );
        }
        Command::Commit {
            message,
            verbose,
            allow_secrets,
            skip_hooks,
            category,
            scope,
            confidence,
            description,
        } => {
            let repo = Repo::discover(cwd)?;

            let intent = if category.is_some() || scope.is_some() || confidence.is_some() || description.is_some() {
                let parsed_category = category.as_deref().map(|c| match c {
                    "bugfix" | "fix" => fl_core::CheckpointCategory::Bugfix,
                    "feature" | "feat" => fl_core::CheckpointCategory::Feature,
                    "refactor" => fl_core::CheckpointCategory::Refactor,
                    "test" => fl_core::CheckpointCategory::Test,
                    "docs" => fl_core::CheckpointCategory::Docs,
                    "style" => fl_core::CheckpointCategory::Style,
                    "chore" => fl_core::CheckpointCategory::Chore,
                    _ => fl_core::CheckpointCategory::Chore,
                });
                Some(fl_core::CheckpointIntentMetadata {
                    category: parsed_category,
                    scope_label: scope,
                    confidence,
                    structured_description: description,
                })
            } else {
                None
            };

            repo.snapshot_working_directory()?;

            // Reject empty commits when the working tree is clean.
            let st = repo.status()?;
            let n_new = st.new_files.len();
            let n_mod = st.modified_files.len();
            let n_del = st.deleted_files.len();
            if st.checkpoint_id.is_some() && n_new == 0 && n_mod == 0 && n_del == 0 {
                bail!("nothing to commit — working tree is clean");
            }

            let event = repo.create_checkpoint_with_options(message, allow_secrets, skip_hooks, intent)?;
            let commit_id = event.id;
            let EventKind::Checkpoint(payload) = event.kind else {
                bail!("unexpected event payload for commit")
            };

            if verbose {
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
            } else {
                let short_id = &commit_id.to_string()[..8];
                println!(
                    "{} {} {}",
                    "commit".green().bold(),
                    short_id.yellow(),
                    payload.label.bold()
                );
                // Show file stats
                let mut parts = Vec::new();
                if n_new > 0 {
                    parts.push(format!("{} new", n_new).green().to_string());
                }
                if n_mod > 0 {
                    parts.push(format!("{} modified", n_mod).yellow().to_string());
                }
                if n_del > 0 {
                    parts.push(format!("{} deleted", n_del).red().to_string());
                }
                let total = n_new + n_mod + n_del;
                if total > 0 {
                    println!(
                        "  {} file{}, {}",
                        total,
                        if total == 1 { "" } else { "s" },
                        parts.join(", ")
                    );
                }
            }
        }
        Command::Log { limit, event_type, raw } => {
            let repo = Repo::discover(cwd)?;
            let events = repo.list_events()?;

            let mut shown = 0usize;
            for event in events {
                // Filter by event type if requested
                if let Some(ref filter) = event_type {
                    if event_type_label(&event.kind) != filter.as_str() {
                        continue;
                    }
                }

                let ts = if raw {
                    event.timestamp.clone()
                } else {
                    format_timestamp(&event.timestamp)
                };
                let ts_display = ts.dimmed();

                match event.kind {
                    EventKind::Init(init) => println!(
                        "{}  {}  mode={}",
                        ts_display, "init".cyan().bold(), init.mode
                    ),
                    EventKind::Checkpoint(cp) => {
                        let short_id = &event.id.to_string()[..8];
                        println!(
                            "{}  {} {} {}",
                            ts_display,
                            "commit".green().bold(),
                            short_id.yellow(),
                            cp.label.bold()
                        );
                    }
                    EventKind::Exploration(exp) => {
                        let action = format!("{:?}", exp.action).to_lowercase();
                        println!(
                            "{}  {} {}  {}",
                            ts_display,
                            "exploration".magenta().bold(),
                            action,
                            exp.title
                        );
                    }
                    EventKind::Undo(undo) => {
                        let target_short = &undo.target_event_id.to_string()[..8];
                        println!(
                            "{}  {}  target={}  scope={}",
                            ts_display,
                            "undo".red().bold(),
                            target_short.yellow(),
                            undo.file_scope.as_deref().unwrap_or("all")
                        );
                    }
                    EventKind::GitBridge(bridge) => {
                        let action = format!("{:?}", bridge.action).to_lowercase();
                        let status = if bridge.success { "ok".green() } else { "FAIL".red() };
                        println!(
                            "{}  {} {}  {}  {}",
                            ts_display, "git".blue().bold(), action, status, bridge.detail
                        );
                    }
                    EventKind::Session(ses) => {
                        let action = format!("{:?}", ses.action).to_lowercase();
                        println!(
                            "{}  {} {}  agent={}",
                            ts_display, "session".blue().bold(), action, ses.agent
                        );
                    }
                    EventKind::Decision(dec) => {
                        let action = format!("{:?}", dec.action).to_lowercase();
                        println!(
                            "{}  {} {}  confidence={:.2}",
                            ts_display, "decision".cyan().bold(), action, dec.confidence
                        );
                    }
                    EventKind::ResourceUsage(usage) => println!(
                        "{}  {}  tokens={}  runtime={}ms",
                        ts_display,
                        "resource".dimmed(),
                        usage.tokens_consumed.unwrap_or(0),
                        usage.runtime_ms.unwrap_or(0)
                    ),
                    EventKind::Task(task) => {
                        let action = format!("{:?}", task.action).to_lowercase();
                        println!(
                            "{}  {} {}  {}",
                            ts_display, "task".cyan().bold(), action, task.title
                        );
                    }
                    EventKind::Presence(p) => {
                        let action = format!("{:?}", p.action).to_lowercase();
                        println!(
                            "{}  {} {}  {}  workspace={}",
                            ts_display, "presence".dimmed(), action, p.actor, p.workspace
                        );
                    }
                    EventKind::Lock(l) => {
                        let action = format!("{:?}", l.action).to_lowercase();
                        println!(
                            "{}  {} {}  resource={}  holder={}",
                            ts_display, "lock".yellow().bold(), action, l.resource, l.holder
                        );
                    }
                    EventKind::Subscription(s) => {
                        let action = format!("{:?}", s.action).to_lowercase();
                        println!(
                            "{}  {} {}  actor={}",
                            ts_display, "subscription".dimmed(), action, s.actor
                        );
                    }
                    EventKind::Gate(g) => {
                        let action = format!("{:?}", g.action).to_lowercase();
                        println!(
                            "{}  {} {}  {}",
                            ts_display, "gate".yellow().bold(), action, g.gate_id
                        );
                    }
                    EventKind::Rebase(r) => println!(
                        "{}  {}  workspace={}  files={}  conflicts={}",
                        ts_display, "rebase".blue().bold(), r.workspace, r.files_merged.len(), r.conflicts_found
                    ),
                    EventKind::ConflictResolution(cr) => {
                        let action = format!("{:?}", cr.action).to_lowercase();
                        println!(
                            "{}  {} {}  path={}",
                            ts_display,
                            "conflict".red().bold(),
                            action,
                            cr.path.as_deref().unwrap_or("-")
                        );
                    }
                    EventKind::Hook(h) => {
                        let status = if h.bypassed {
                            "skipped".dimmed()
                        } else if h.success {
                            "pass".green()
                        } else {
                            "FAIL".red()
                        };
                        println!(
                            "{}  {} {}  {}  {}  {}ms",
                            ts_display, "hook".dimmed(), h.hook_point, h.hook_name, status, h.duration_ms
                        );
                    }
                    EventKind::RemoteSync(rs) => {
                        let action = format!("{:?}", rs.action).to_lowercase();
                        let status = if rs.success { "ok".green() } else { "FAIL".red() };
                        println!(
                            "{}  {} {}  roost={}  events={}  {}",
                            ts_display, "sync".blue().bold(), action, rs.roost_name, rs.event_count, status
                        );
                    }
                    EventKind::Intelligence(intel) => {
                        let action = format!("{:?}", intel.action).to_lowercase();
                        println!(
                            "{}  {} {}  model={}",
                            ts_display,
                            "intel".cyan().bold(),
                            action,
                            intel.model.as_deref().unwrap_or("-"),
                        );
                    }
                    EventKind::Policy(policy) => println!(
                        "{}  {}  {}  {:?}  op={}",
                        ts_display,
                        "policy".yellow().bold(),
                        policy.policy_name,
                        policy.verdict,
                        policy.operation,
                    ),
                    EventKind::Directive(d) => {
                        let kind = format!("{:?}", d.directive).to_lowercase();
                        println!(
                            "{}  {} {}  target={}  by={}",
                            ts_display,
                            "directive".magenta().bold(),
                            kind,
                            d.target_actor,
                            d.issued_by,
                        );
                    }
                    EventKind::FileWrite(fw) => println!(
                        "{}  {}  {}  ({} bytes)",
                        ts_display, "write".green(), fw.path.bold(), fw.size
                    ),
                    EventKind::FileDelete(fd) => println!(
                        "{}  {}  {}",
                        ts_display, "delete".red(), fd.path.bold()
                    ),
                    EventKind::FileRename(fr) => println!(
                        "{}  {}  {} -> {}",
                        ts_display, "rename".yellow(), fr.old_path.bold(), fr.new_path.bold()
                    ),
                }

                shown += 1;
                if let Some(max) = limit {
                    if shown >= max {
                        break;
                    }
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
            repo.snapshot_working_directory()?;
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
                        "ignored_symlinks": report.ignored_symlinks,
                    }))?
                );
            } else {
                println!("On branch {}", report.branch.green().bold());
                if let Some(ref cp) = report.checkpoint_id {
                    let short = &cp.to_string()[..8];
                    println!("Latest commit: {}", short.yellow());
                } else {
                    println!("{}", "No commits yet".dimmed());
                }
                let total =
                    report.new_files.len() + report.modified_files.len() + report.deleted_files.len();
                if total == 0 {
                    println!("\n{}", "Nothing changed since last commit.".dimmed());
                } else {
                    println!();
                    for f in &report.new_files {
                        println!("  {}  {}", "new:     ".green(), f);
                    }
                    for f in &report.modified_files {
                        println!("  {}  {}", "modified:".yellow(), f);
                    }
                    for f in &report.deleted_files {
                        println!("  {}  {}", "deleted: ".red(), f);
                    }
                    println!(
                        "\n{} file(s) changed",
                        total
                    );
                }
                if !report.ignored_symlinks.is_empty() {
                    println!();
                    println!(
                        "{} {} symlink{} ignored (not tracked):",
                        "warning:".yellow().bold(),
                        report.ignored_symlinks.len(),
                        if report.ignored_symlinks.len() == 1 { "" } else { "s" }
                    );
                    for s in &report.ignored_symlinks {
                        println!("  {}  {}", "symlink:".dimmed(), s);
                    }
                }
            }
        }
        Command::Diff {
            semantic,
            intent,
            json,
            since,
            risk,
            kind,
            breaking_only,
            summary,
            patch,
            text,
            from,
            to,
        } => {
            let repo = Repo::discover(cwd)?;
            repo.snapshot_working_directory()?;

            // --text mode: show traditional unified diff and exit early
            if text {
                let text_diffs = if let Some(ref since_str) = since {
                    if from.is_some() || to.is_some() {
                        bail!("cannot use --since with positional FROM/TO arguments");
                    }
                    let duration = parse_duration_spec(since_str)?;
                    let (event, _) = repo.find_checkpoint_before_duration(duration)?;
                    repo.text_diff_checkpoint_vs_working(&event.id.to_string())?
                } else {
                    match (from.as_deref(), to.as_deref()) {
                        (Some(from_prefix), Some(to_prefix)) => {
                            repo.text_diff_between_checkpoints(from_prefix, to_prefix)?
                        }
                        (Some(checkpoint_prefix), None) => {
                            repo.text_diff_checkpoint_vs_working(checkpoint_prefix)?
                        }
                        (None, None) => repo.text_diff_from_latest_checkpoint()?,
                        (None, Some(_)) => {
                            bail!("cannot specify TO without FROM; use `fl diff <from> <to>`");
                        }
                    }
                };
                print_unified_text_diffs(&text_diffs);
                return Ok(());
            }

            let has_filters = risk.is_some() || kind.is_some() || breaking_only;

            // Helper closure to apply filters, summary, and patch to semantic diffs
            let apply_post_processing =
                |mut diffs: Vec<fl_core::SemanticFileDiff>| -> Result<()> {
                    if has_filters {
                        filter_diffs(&mut diffs, risk.as_deref(), kind.as_deref(), breaking_only);
                    }
                    if summary {
                        print_summary_line(&diffs);
                        return Ok(());
                    }
                    print_semantic_diffs_with_patch(&diffs, json, patch)?;
                    Ok(())
                };

            // If --since is provided and no positional args, use duration-based diff
            if let Some(ref since_str) = since {
                if from.is_some() || to.is_some() {
                    bail!("cannot use --since with positional FROM/TO arguments");
                }
                let duration = parse_duration_spec(since_str)?;
                let (event, _) = repo.find_checkpoint_before_duration(duration)?;
                let prefix = event.id.to_string();

                if intent {
                    let groups =
                        repo.semantic_diff_checkpoint_vs_working_with_intents(&prefix)?;
                    print_intent_diff(&groups, json)?;
                } else {
                    let diffs = repo.semantic_diff_checkpoint_vs_working(&prefix)?;
                    if !has_filters && !summary && !patch {
                        let file_summary =
                            repo.file_summary_checkpoint_vs_working(&prefix)?;
                        print_full_diff_summary(&file_summary, &diffs, json)?;
                    } else {
                        apply_post_processing(diffs)?;
                    }
                }
            } else {
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
                        } else {
                            let diffs = repo
                                .semantic_diff_between_checkpoints(from_prefix, to_prefix)?;
                            if !has_filters && !summary && !patch {
                                let file_summary = repo
                                    .file_summary_between_checkpoints(from_prefix, to_prefix)?;
                                if semantic {
                                    print_semantic_diffs_with_patch(&diffs, json, patch)?;
                                } else {
                                    print_full_diff_summary(&file_summary, &diffs, json)?;
                                }
                            } else {
                                apply_post_processing(diffs)?;
                            }
                        }
                    }
                    // fl diff <checkpoint> — checkpoint vs working directory
                    (Some(checkpoint_prefix), None) => {
                        if intent {
                            let groups = repo
                                .semantic_diff_checkpoint_vs_working_with_intents(
                                    checkpoint_prefix,
                                )?;
                            print_intent_diff(&groups, json)?;
                        } else {
                            let diffs =
                                repo.semantic_diff_checkpoint_vs_working(checkpoint_prefix)?;
                            if !has_filters && !summary && !patch {
                                if semantic {
                                    print_semantic_diffs_with_patch(&diffs, json, patch)?;
                                } else {
                                    let file_summary = repo
                                        .file_summary_checkpoint_vs_working(checkpoint_prefix)?;
                                    print_full_diff_summary(&file_summary, &diffs, json)?;
                                }
                            } else {
                                apply_post_processing(diffs)?;
                            }
                        }
                    }
                    // fl diff — latest checkpoint vs working directory
                    (None, None) => {
                        if intent {
                            let groups = repo.semantic_diff_with_intents()?;
                            print_intent_diff(&groups, json)?;
                        } else {
                            let diffs = repo.semantic_diff_from_latest_checkpoint()?;
                            if !has_filters && !summary && !patch {
                                if semantic {
                                    print_semantic_diffs_with_patch(&diffs, json, patch)?;
                                } else {
                                    let file_summary =
                                        repo.file_summary_from_latest_checkpoint()?;
                                    print_full_diff_summary(&file_summary, &diffs, json)?;
                                }
                            } else {
                                apply_post_processing(diffs)?;
                            }
                        }
                    }
                    (None, Some(_)) => {
                        bail!("cannot specify TO without FROM; use `fl diff <from> <to>`");
                    }
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
        Command::Review { id, expand, full, since } => {
            let repo = Repo::discover(cwd)?;
            let exploration_id = resolve_exploration(&id, &repo)?;
            let mut summary = repo.review_exploration(exploration_id)?;

            // If --since is provided, filter diffs to only show changes from
            // checkpoints within the time window
            if let Some(ref since_str) = since {
                let duration = parse_duration_spec(since_str)?;
                let (cutoff_event, _) = repo.find_checkpoint_before_duration(duration)?;
                let cutoff_prefix = cutoff_event.id.to_string();
                // Re-diff from the cutoff checkpoint vs working dir, then
                // intersect with the exploration's file set
                let since_diffs =
                    repo.semantic_diff_checkpoint_vs_working(&cutoff_prefix)?;
                let since_paths: std::collections::HashSet<_> =
                    since_diffs.iter().map(|d| d.path.clone()).collect();
                summary.diffs.retain(|d| since_paths.contains(&d.path));
            }

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
                // Full mode: show expanded detail for every change
                let mut change_index = 0usize;
                for diff in &summary.diffs {
                    println!("{} [{}]", diff.path, diff.language);
                    for change in &diff.changes {
                        change_index += 1;
                        let marker = change_marker(change.kind);
                        let risk = risk_label(change.risk);
                        println!("  #{:<3} {} [{}] {}", change_index, marker, risk, change.symbol);

                        if !change.impact.symbols.is_empty() {
                            println!(
                                "       Impact symbols: {}",
                                change.impact.symbols.join(", ")
                            );
                        }
                        if !change.impact.files.is_empty() {
                            println!(
                                "       Impact files: {}",
                                change.impact.files.join(", ")
                            );
                        }
                        if !change.impact.modules.is_empty() {
                            println!(
                                "       Impact modules: {}",
                                change.impact.modules.join(", ")
                            );
                        }
                        if change.compatibility.status
                            != SemanticCompatibilityStatus::Compatible
                        {
                            let status = compatibility_label(change.compatibility.status);
                            if change.compatibility.notes.is_empty() {
                                println!("       Compatibility: {}", status);
                            } else {
                                println!(
                                    "       Compatibility: {} ({})",
                                    status,
                                    change.compatibility.notes.join("; ")
                                );
                            }
                        }
                    }
                    if diff.parse_fallback {
                        println!("  ! parser fallback used");
                    }
                    println!();
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

                    for exploration in &explorations {
                        let age = format_nanos_age(&exploration.updated_at);
                        let status_colored = match exploration.status {
                            ExplorationStatus::Active => "active".green().to_string(),
                            ExplorationStatus::Promoted => "promoted".blue().to_string(),
                            ExplorationStatus::Abandoned => "abandoned".red().to_string(),
                        };
                        println!(
                            "{}  {}  {}  ({})",
                            &exploration.id.to_string()[..8],
                            status_colored,
                            exploration.title,
                            age.dimmed(),
                        );
                    }
                }
                ExploreCommand::Promote { id } => {
                    let exploration_id = resolve_exploration(&id, &repo)?;
                    let exploration = repo.promote_exploration(exploration_id)?;
                    println!(
                        "exploration {} promoted: {}",
                        exploration.id, exploration.title
                    );
                }
                ExploreCommand::Abandon { id } => {
                    let exploration_id = resolve_exploration(&id, &repo)?;
                    let exploration = repo.abandon_exploration(exploration_id)?;
                    println!(
                        "exploration {} abandoned: {}",
                        exploration.id, exploration.title
                    );
                }
                ExploreCommand::Compare { left, right, json } => {
                    let left_id = resolve_exploration(&left, &repo)?;
                    let right_id = right
                        .as_deref()
                        .map(|r| resolve_exploration(r, &repo))
                        .transpose()?;
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
                        print_semantic_file_diff(diff, false);
                    }
                }
                ExploreCommand::Prune { older_than, force } => {
                    let duration = parse_duration_spec(&older_than)?;

                    if !force {
                        // Preview what would be pruned
                        let candidates = repo.prune_candidates(duration)?;
                        if candidates.is_empty() {
                            println!("No abandoned explorations match the criteria.");
                            return Ok(());
                        }
                        println!(
                            "{} The following {} abandoned exploration{} will be pruned:",
                            "warning:".yellow().bold(),
                            candidates.len(),
                            if candidates.len() == 1 { "" } else { "s" }
                        );
                        for exp in &candidates {
                            let age = format_nanos_age(&exp.updated_at);
                            println!(
                                "  {}  {} (abandoned {})",
                                &exp.id.to_string()[..8],
                                exp.title,
                                age,
                            );
                        }
                        println!(
                            "\nUse {} to proceed.",
                            "--force".bold()
                        );
                        return Ok(());
                    }

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
        Command::Undo { n, to, since, file, to_checkpoint, exploration, session, workspace, actor } => {
            let repo = Repo::discover(cwd)?;
            repo.snapshot_working_directory()?;
            let request = build_undo_request(n, to, since)?;
            let scope = build_undo_scope(exploration, session, workspace, actor, &repo)?;
            let result = if !scope.is_empty() {
                repo.undo_scoped(request, scope)?
            } else if let Some(path) = file {
                repo.undo_file(request, path)?
            } else if to_checkpoint {
                repo.undo_to_checkpoint(request)?
            } else {
                repo.undo(request)?
            };

            let short_target = &result.target_event_id.to_string()[..8];
            if let Some(checkpoint_id) = result.restored_checkpoint_event {
                let short_restored = &checkpoint_id.to_string()[..8];
                println!(
                    "{}  undid event {}  restored commit {}",
                    "undo".red().bold(),
                    short_target.yellow(),
                    short_restored.green()
                );
            } else {
                println!(
                    "{}  undid event {}",
                    "undo".red().bold(),
                    short_target.yellow()
                );
            }
        }
        Command::Record { paths } => {
            let repo = Repo::discover(cwd)?;
            let count = if paths.is_empty() {
                repo.snapshot_working_directory()?
            } else {
                // Record specific files only — snapshot all then report
                // For now, snapshot_working_directory captures everything;
                // specific-path recording could be optimized later.
                repo.snapshot_working_directory()?
            };
            if count == 0 {
                println!("no changes to record");
            } else {
                println!("recorded {} file change(s)", count);
            }
        }
        Command::Blame { path, json } => {
            let repo = Repo::discover(cwd)?;
            let annotations = repo.blame(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!(
                    annotations.iter().map(|a| serde_json::json!({
                        "line_number": a.line_number,
                        "content": a.content,
                        "commit_id": a.commit_id.map(|id| id.to_string()),
                        "author": a.author,
                        "timestamp": a.timestamp,
                        "message": a.message,
                    })).collect::<Vec<_>>()
                ))?);
            } else {
                if annotations.is_empty() {
                    println!("{}", "File has no history (not committed yet).".dimmed());
                    return Ok(());
                }
                for ann in &annotations {
                    let commit_short = ann.commit_id
                        .map(|id| id.to_string()[..8].to_string())
                        .unwrap_or_else(|| "--------".to_string());
                    let author = ann.author.as_deref().unwrap_or("unknown");
                    let ts = ann.timestamp.as_deref()
                        .map(|t| format_timestamp(t))
                        .unwrap_or_else(|| "          ".to_string());
                    // Truncate author to 12 chars for alignment
                    let author_display = if author.len() > 12 {
                        &author[..12]
                    } else {
                        author
                    };
                    println!(
                        "{} {} {:>12} {:>4} | {}",
                        commit_short.yellow(),
                        ts.dimmed(),
                        author_display.cyan(),
                        ann.line_number,
                        ann.content
                    );
                }
            }
        }
        Command::Stash { command } => {
            let repo = Repo::discover(cwd)?;
            let stash_cmd = command.unwrap_or(StashCommand::Push { message: None });
            match stash_cmd {
                StashCommand::Push { message } => {
                    repo.snapshot_working_directory()?;
                    let st = repo.status()?;
                    let total = st.new_files.len() + st.modified_files.len() + st.deleted_files.len();
                    if st.checkpoint_id.is_some() && total == 0 {
                        println!("{}", "No changes to stash.".dimmed());
                        return Ok(());
                    }
                    let index = repo.stash_push(message)?;
                    println!("Saved working directory to stash@{{{}}}", index);
                }
                StashCommand::Pop { index } => {
                    let idx = index.unwrap_or(0);
                    repo.stash_pop(idx)?;
                    println!("Restored stash@{{{}}} and removed it from the stash list", idx);
                }
                StashCommand::List => {
                    let entries = repo.stash_list()?;
                    if entries.is_empty() {
                        println!("{}", "No stash entries.".dimmed());
                        return Ok(());
                    }
                    for (i, entry) in entries.iter().enumerate() {
                        let msg = entry.message.as_deref().unwrap_or("(no message)");
                        let ts = format_timestamp(&entry.timestamp);
                        println!(
                            "stash@{{{}}}  {}  {}",
                            i,
                            ts.dimmed(),
                            msg,
                        );
                    }
                }
                StashCommand::Drop { index } => {
                    let idx = index.unwrap_or(0);
                    repo.stash_drop(idx)?;
                    println!("Dropped stash@{{{}}}", idx);
                }
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
                WorkspaceCommand::Delete { name } => {
                    let removed = repo.delete_workspace(&name)?;
                    if removed {
                        println!("workspace {} deleted", name);
                    } else {
                        println!("workspace {} not found", name);
                    }
                }
                WorkspaceCommand::Rename { old_name, new_name } => {
                    let ws = repo.rename_workspace(&old_name, &new_name)?;
                    println!("workspace {} renamed to {}", old_name, ws.name);
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
                        // No flags: show current limits
                        let info = repo.workspace_info(&name)?;
                        let config = info.workspace.workspace.as_ref().unwrap();
                        println!("workspace {} limits:", info.workspace.name);
                        println!(
                            "  max-snapshots: {}",
                            config
                                .max_snapshots
                                .map(|m| m.to_string())
                                .unwrap_or_else(|| "unlimited".to_string())
                        );
                        println!(
                            "  max-events: {}",
                            config
                                .max_events
                                .map(|m| m.to_string())
                                .unwrap_or_else(|| "unlimited".to_string())
                        );
                    } else {
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
                    let sessions = repo.list_sessions()?;
                    let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
                    let session_id = resolve_uuid_prefix(&id, &session_ids)?;
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
                    let sessions = repo.list_sessions()?;
                    let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
                    let sid = resolve_uuid_prefix(&session_id, &session_ids)?;
                    let eid = resolve_exploration(&exploration_id, &repo)?;
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
                    let sessions = repo.list_sessions()?;
                    let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
                    let sid = resolve_uuid_prefix(&session_id, &session_ids)?;
                    let eid = resolve_exploration(&exploration_id, &repo)?;
                    let decision_action = match action.to_lowercase().as_str() {
                        "kept" => DecisionAction::Kept,
                        "discarded" => DecisionAction::Discarded,
                        _ => DecisionAction::Custom(action),
                    };
                    repo.record_decision(sid, eid, decision_action, reason, confidence)?;
                    println!("decision recorded for session {}", sid);
                }
                SessionCommand::Usage {
                    session_id,
                    tokens,
                    runtime_ms,
                    api_call,
                } => {
                    let sessions = repo.list_sessions()?;
                    let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
                    let sid = resolve_uuid_prefix(&session_id, &session_ids)?;
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
                    let sessions = repo.list_sessions()?;
                    let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
                    let session_id = resolve_uuid_prefix(&id, &session_ids)?;
                    let session = repo.complete_session(session_id, result)?;
                    println!("session {} completed", session.id);
                }
                SessionCommand::Fail { id, reason } => {
                    let sessions = repo.list_sessions()?;
                    let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
                    let session_id = resolve_uuid_prefix(&id, &session_ids)?;
                    let session = repo.fail_session(session_id, reason)?;
                    println!("session {} failed", session.id);
                }
                SessionCommand::Provenance {
                    exploration_id,
                    json,
                } => {
                    let eid = resolve_exploration(&exploration_id, &repo)?;
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
                    let sessions = repo.list_sessions()?;
                    let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
                    let session_id = resolve_uuid_prefix(&id, &session_ids)?;
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
                    scope,
                } => {
                    let deps = depends_on
                        .iter()
                        .filter(|id| !id.is_empty())
                        .map(|id| Ok(repo.find_task_by_prefix(id)?.id))
                        .collect::<Result<Vec<_>>>()?;
                    let discovered = discovered_from
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(|s| -> Result<Uuid> { Ok(repo.find_task_by_prefix(s)?.id) })
                        .transpose()?;
                    let task = repo.create_task(title, description, deps, discovered, scope)?;
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
        Command::Who { json } => {
            let repo = Repo::discover(cwd)?;
            let report = repo.who()?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.actors.is_empty() {
                println!("No active actors.");
            } else {
                println!("{:<16} {:<12} {:<30} {:<20} {}", "ACTOR", "WORKSPACE", "FILES", "INTENT", "TASK");
                for a in &report.actors {
                    let files = if a.active_files.is_empty() {
                        String::from("-")
                    } else {
                        a.active_files.join(", ")
                    };
                    let symbols_suffix = if a.active_symbols.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", a.active_symbols.join(", "))
                    };
                    let intent = a.intent.as_deref().unwrap_or("-");
                    let task = a.current_task.as_deref().unwrap_or("-");
                    println!(
                        "{:<16} {:<12} {:<30} {:<20} {}",
                        a.actor,
                        a.workspace,
                        format!("{}{}", files, symbols_suffix),
                        intent,
                        task
                    );
                }
            }
        }
        Command::Directive { command } => {
            let repo = Repo::discover(cwd)?;
            match command {
                DirectiveCommand::Send { target, kind, reason, new_task } => {
                    let directive_kind = match kind.to_lowercase().as_str() {
                        "pause" => DirectiveKind::Pause,
                        "resume" => DirectiveKind::Resume,
                        "redirect" => {
                            let task = new_task.ok_or_else(|| anyhow::anyhow!("--new-task required for redirect directive"))?;
                            DirectiveKind::Redirect { new_task: task }
                        }
                        "abort" => {
                            let r = reason.clone().unwrap_or_else(|| "no reason given".to_string());
                            DirectiveKind::Abort { reason: r }
                        }
                        other => bail!("unknown directive kind: {} (expected pause, resume, redirect, abort)", other),
                    };
                    let summary = repo.send_directive(target, directive_kind, reason)?;
                    println!("directive {} sent to {}", &summary.id.to_string()[..8], summary.target_actor);
                }
                DirectiveCommand::List { actor, json } => {
                    let directives = if let Some(actor) = actor {
                        repo.list_directives_for_actor(&actor)?
                    } else {
                        repo.list_directives()?
                    };
                    if json {
                        println!("{}", serde_json::to_string_pretty(&directives)?);
                    } else if directives.is_empty() {
                        println!("No directives.");
                    } else {
                        for d in &directives {
                            let detail_str = d.directive_detail.as_deref()
                                .map(|det| format!(": {}", det))
                                .unwrap_or_default();
                            // Only show reason parenthetical when there's no detail
                            // (abort embeds the reason in detail, so showing both duplicates it)
                            let reason_str = if d.directive_detail.is_some() {
                                String::new()
                            } else {
                                d.reason.as_deref()
                                    .map(|r| format!(" ({})", r))
                                    .unwrap_or_default()
                            };
                            println!(
                                "{}  {} → {}  {}{}{}",
                                &d.id.to_string()[..8],
                                d.issued_by,
                                d.target_actor,
                                d.directive_kind,
                                detail_str,
                                reason_str
                            );
                        }
                    }
                }
                DirectiveCommand::Listen { actor, kind: kind_filter } => {
                    let actor = actor.unwrap_or_else(|| repo.current_actor_name());
                    println!("listening for directives targeting {} (Ctrl+C to stop)...", actor);

                    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                    let r = running.clone();
                    ctrlc_flag(&r);

                    let paused_path = repo.flock_dir().join("paused");
                    let mut seen_count = 0usize;

                    while running.load(std::sync::atomic::Ordering::Relaxed) {
                        let directives = repo.list_directives_for_actor(&actor)?;
                        for d in directives.iter().skip(seen_count) {
                            // Apply kind filter if specified
                            if let Some(ref filter) = kind_filter {
                                if d.directive_kind != *filter {
                                    continue;
                                }
                            }
                            match d.directive_kind.as_str() {
                                "pause" => {
                                    eprintln!("DIRECTIVE: paused by {}", d.issued_by);
                                    let _ = std::fs::write(&paused_path, "paused");
                                }
                                "resume" => {
                                    eprintln!("DIRECTIVE: resumed by {}", d.issued_by);
                                    let _ = std::fs::remove_file(&paused_path);
                                }
                                "redirect" => {
                                    let task = d.directive_detail.as_deref().unwrap_or("unknown");
                                    eprintln!("DIRECTIVE: redirected to task '{}' by {}", task, d.issued_by);
                                }
                                "abort" => {
                                    let reason = d.directive_detail.as_deref().unwrap_or("no reason");
                                    eprintln!("DIRECTIVE: abort — {} (by {})", reason, d.issued_by);
                                    return Ok(());
                                }
                                other => {
                                    eprintln!("DIRECTIVE: unknown kind '{}' from {}", other, d.issued_by);
                                }
                            }
                        }
                        seen_count = directives.len();
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
            }
        }
        Command::Preview { workspace, interval, remote, json } => {
            let repo = Repo::discover(cwd)?;
            let roost = remote.as_deref().unwrap_or("origin");
            let ws = repo.ws_connect(roost).ok().map(|(client, _)| client);
            let ws_name = workspace.unwrap_or_else(|| "main".to_string());

            let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let r = running.clone();
            ctrlc_flag(&r);

            println!("streaming workspace preview every {}ms (Ctrl+C to stop)...", interval);

            while running.load(std::sync::atomic::Ordering::Relaxed) {
                let diffs = repo.workspace_preview_diffs()?;
                if !diffs.is_empty() {
                    if json {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                            .to_string();
                        let preview = serde_json::json!({
                            "workspace": ws_name,
                            "diffs": diffs,
                            "timestamp": ts,
                        });
                        println!("{}", serde_json::to_string(&preview)?);
                    } else {
                        for d in &diffs {
                            let symbols = if d.symbols_changed.is_empty() {
                                String::new()
                            } else {
                                format!(" [{}]", d.symbols_changed.join(", "))
                            };
                            println!("  {}  +{} -{}{}",
                                d.path, d.lines_added, d.lines_removed, symbols);
                        }
                    }

                    // Send via WebSocket if connected
                    if let Some(ref ws) = ws {
                        let msg = fl_core::WsClientMessage::StartPreview {
                            workspace: ws_name.clone(),
                            interval_ms: interval,
                        };
                        let _ = ws.send(msg);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(interval));
            }

            if let Some(ref ws) = ws {
                let _ = ws.send(fl_core::WsClientMessage::StopPreview);
                ws.disconnect();
            }
        }
        Command::QuickSave { tag, show_diff } => {
            let repo = Repo::discover(cwd)?;
            let event = repo.quick_save(tag)?;
            let event_id = event.id;
            let EventKind::Checkpoint(payload) = event.kind else {
                bail!("unexpected event type")
            };
            println!("quick-save {} ({})", payload.label, event_id);

            if show_diff {
                let diffs = repo.semantic_diff_for_checkpoint(event_id)?;
                if diffs.is_empty() {
                    println!("No semantic changes.");
                } else {
                    println!();
                    println!("=== Semantic Changes ===");
                    for diff in &diffs {
                        print_semantic_file_diff(diff, false);
                    }
                }
            }
        }
        Command::QuickRestore => {
            let repo = Repo::discover(cwd)?;
            let result = repo.quick_restore()?;
            println!("restored to quick-save {}", result.target_event_id);
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
                    symbol,
                    intent,
                    ttl,
                } => {
                    let presence = repo.heartbeat_with_symbols(workspace, file, symbol, intent, ttl)?;
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
                        let symbols_str = if p.active_symbols.is_empty() {
                            String::new()
                        } else {
                            format!(" symbols=[{}]", p.active_symbols.join(", "))
                        };
                        let intent_str = p
                            .intent
                            .as_deref()
                            .map(|i| format!(" intent=\"{}\"", i))
                            .unwrap_or_default();
                        println!(
                            "{}  {}  ttl={}s{}{}{}",
                            p.actor,
                            p.workspace,
                            p.ttl.as_secs(),
                            files_str,
                            symbols_str,
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
                    let locks = repo.list_locks()?;
                    let lock_ids: Vec<Uuid> = locks.iter().map(|l| l.id).collect();
                    let lock_id = resolve_uuid_prefix(&id, &lock_ids)?;
                    repo.release_lock(lock_id)?;
                    println!("lock {} released", &lock_id.to_string()[..8]);
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
            let subs = repo.list_subscriptions()?;
            let sub_ids: Vec<Uuid> = subs.iter().map(|s| s.id).collect();
            let sub_id = resolve_uuid_prefix(&id, &sub_ids)?;
            repo.unsubscribe(sub_id)?;
            println!("subscription {} cancelled", &sub_id.to_string()[..8]);
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
                    let gates = repo.list_gates()?;
                    let gate_ids: Vec<Uuid> = gates.iter().map(|g| g.id).collect();
                    let gate_id = resolve_uuid_prefix(&id, &gate_ids)?;
                    repo.approve_gate(gate_id, reason)?;
                    println!("gate {} approved", &gate_id.simple().to_string()[..8]);
                }
                GateCommand::Reject { id, reason } => {
                    let gates = repo.list_gates()?;
                    let gate_ids: Vec<Uuid> = gates.iter().map(|g| g.id).collect();
                    let gate_id = resolve_uuid_prefix(&id, &gate_ids)?;
                    repo.reject_gate(gate_id, reason)?;
                    println!("gate {} rejected", &gate_id.simple().to_string()[..8]);
                }
                GateCommand::Delete { id } => {
                    let gates = repo.list_all_gates()?;
                    let gate_ids: Vec<Uuid> = gates.iter().map(|g| g.id).collect();
                    let gate_id = resolve_uuid_prefix(&id, &gate_ids)?;
                    repo.delete_gate(gate_id)?;
                    println!("gate {} deleted", &gate_id.simple().to_string()[..8]);
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
                            let id_str = c.id.map(|id| format!("{}", id))
                                .unwrap_or_else(|| "-".to_string());
                            println!(
                                "  {} {} [{}]: {}",
                                &id_str[..8.min(id_str.len())],
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
            let (ws, repo_path) = repo.ws_connect(roost)?;

            // Subscribe with filters
            let filter = fl_core::SubscriptionFilter {
                paths: path,
                symbols: symbol,
                modules: vec![],
            };
            let sub = fl_core::WsSubscribeRequest {
                repo: repo_path,
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
        Command::InstallSkill { claude: _, project, dir } => {
            let target = if let Some(d) = dir {
                PathBuf::from(d).join("flock")
            } else if project {
                env::current_dir()?.join(".claude").join("skills").join("flock")
            } else {
                let home = env::var("HOME").context("HOME not set")?;
                PathBuf::from(home).join(".claude").join("skills").join("flock")
            };

            std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create {}", target.display()))?;

            let files: &[(&str, &str)] = &[
                ("SKILL.md", SKILL_MD),
                ("WORKFLOWS.md", WORKFLOWS_MD),
                ("COLLABORATION.md", COLLABORATION_MD),
            ];

            for (name, content) in files {
                let path = target.join(name);
                std::fs::write(&path, content)
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }

            println!("Installed flock skill to {}", target.display());
            for (name, _) in files {
                println!("  {}", target.join(name).display());
            }
        }
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "fl", &mut io::stdout());
            io::stdout().flush()?;
        }
    }

    Ok(())
}

/// Format a nanosecond timestamp string into a human-readable date.
fn format_timestamp(ts: &str) -> String {
    let nanos: u128 = match ts.parse() {
        Ok(n) => n,
        Err(_) => return ts.to_string(),
    };
    let secs = (nanos / 1_000_000_000) as u64;

    // Format using libc localtime to get proper timezone
    let secs_i64 = secs as i64;
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs_i64 as *const i64, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

/// Format a nanosecond-epoch timestamp string as a human-readable relative age.
fn format_nanos_age(ts: &str) -> String {
    let nanos: u128 = match ts.parse() {
        Ok(n) => n,
        Err(_) => return "unknown".to_string(),
    };
    let now_nanos: u128 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let age_secs = now_nanos.saturating_sub(nanos) / 1_000_000_000;
    if age_secs < 60 {
        format!("{}s ago", age_secs)
    } else if age_secs < 3600 {
        format!("{}m ago", age_secs / 60)
    } else if age_secs < 86400 {
        format!("{}h ago", age_secs / 3600)
    } else {
        format!("{}d ago", age_secs / 86400)
    }
}

/// Classify an EventKind into a short type label for filtering.
fn event_type_label(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Init(_) => "init",
        EventKind::Checkpoint(_) => "commit",
        EventKind::Exploration(_) => "exploration",
        EventKind::Undo(_) => "undo",
        EventKind::GitBridge(_) => "git",
        EventKind::Session(_) => "session",
        EventKind::Decision(_) => "decision",
        EventKind::ResourceUsage(_) => "resource",
        EventKind::Task(_) => "task",
        EventKind::Presence(_) => "presence",
        EventKind::Lock(_) => "lock",
        EventKind::Subscription(_) => "subscription",
        EventKind::Gate(_) => "gate",
        EventKind::Rebase(_) => "rebase",
        EventKind::ConflictResolution(_) => "conflict",
        EventKind::Hook(_) => "hook",
        EventKind::RemoteSync(_) => "sync",
        EventKind::Intelligence(_) => "intel",
        EventKind::Policy(_) => "policy",
        EventKind::Directive(_) => "directive",
        EventKind::FileWrite(_) => "file-write",
        EventKind::FileDelete(_) => "file-delete",
        EventKind::FileRename(_) => "file-rename",
    }
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

fn print_unified_text_diffs(diffs: &[fl_core::TextFileDiff]) {
    use colored::Colorize;
    for file_diff in diffs {
        let patch = diffy::create_patch(&file_diff.old_content, &file_diff.new_content);
        println!(
            "{}",
            format!("--- a/{}", file_diff.path).red()
        );
        println!(
            "{}",
            format!("+++ b/{}", file_diff.path).green()
        );
        for hunk in patch.hunks() {
            println!(
                "{}",
                format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_range().start(),
                    hunk.old_range().end() - hunk.old_range().start(),
                    hunk.new_range().start(),
                    hunk.new_range().end() - hunk.new_range().start(),
                )
                .cyan()
            );
            for line in hunk.lines() {
                match line {
                    diffy::Line::Context(text) => print!(" {}", text),
                    diffy::Line::Delete(text) => {
                        print!("{}", format!("-{}", text).red());
                    }
                    diffy::Line::Insert(text) => {
                        print!("{}", format!("+{}", text).green());
                    }
                }
            }
        }
    }
}

fn print_semantic_file_diff(diff: &fl_core::SemanticFileDiff, patch: bool) {
    println!("{} [{}]", diff.path.bold(), diff.language.dimmed());
    for change in &diff.changes {
        let marker = change_marker(change.kind);
        let risk = risk_label(change.risk);
        let colored_marker = match change.kind {
            SemanticChangeKind::Added => marker.green(),
            SemanticChangeKind::Removed => marker.red(),
            SemanticChangeKind::Modified => marker.yellow(),
            _ => marker.dimmed(),
        };
        let colored_risk = match change.risk {
            SemanticRisk::Low => risk.green(),
            SemanticRisk::Medium => risk.yellow(),
            SemanticRisk::High => risk.red(),
        };
        println!("  {} [{}] {}", colored_marker, colored_risk, change.symbol);
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
            println!("    {}: {}", "impact".dimmed(), impact_fields.join(" | "));
        }
        if change.compatibility.status != SemanticCompatibilityStatus::Compatible {
            let status = compatibility_label(change.compatibility.status);
            let colored_status = match change.compatibility.status {
                SemanticCompatibilityStatus::PotentiallyBreaking => status.yellow().bold(),
                SemanticCompatibilityStatus::Breaking => status.red().bold(),
                _ => status.normal(),
            };
            if change.compatibility.notes.is_empty() {
                println!("    {}: {}", "compatibility".dimmed(), colored_status);
            } else {
                println!(
                    "    {}: {} ({})",
                    "compatibility".dimmed(),
                    colored_status,
                    change.compatibility.notes.join("; ")
                );
            }
        }
        if patch {
            print_patch_body(change);
        }
    }
    if diff.parse_fallback {
        println!("  {} parser fallback used", "!".yellow());
    }
}

fn print_patch_body(change: &fl_core::SemanticChange) {
    match change.kind {
        SemanticChangeKind::Modified => {
            if let Some(ref old) = change.old_source {
                println!("    {}", "--- old".red());
                for line in old.lines() {
                    println!("    {}", format!("-{}", line).red());
                }
            }
            if let Some(ref new) = change.new_source {
                println!("    {}", "+++ new".green());
                for line in new.lines() {
                    println!("    {}", format!("+{}", line).green());
                }
            }
        }
        SemanticChangeKind::Added => {
            if let Some(ref new) = change.new_source {
                println!("    {}", "+++ added".green());
                for line in new.lines() {
                    println!("    {}", format!("+{}", line).green());
                }
            }
        }
        SemanticChangeKind::Removed => {
            if let Some(ref old) = change.old_source {
                println!("    {}", "--- removed".red());
                for line in old.lines() {
                    println!("    {}", format!("-{}", line).red());
                }
            }
        }
        SemanticChangeKind::Renamed | SemanticChangeKind::Moved => {
            if let Some(ref new) = change.new_source {
                println!("    {}", "+++ relocated".cyan());
                for line in new.lines() {
                    println!("    {}", format!("+{}", line).cyan());
                }
            }
        }
        SemanticChangeKind::StyleOnly => {}
    }
}

fn filter_diffs(
    diffs: &mut Vec<fl_core::SemanticFileDiff>,
    risk_filter: Option<&str>,
    kind_filter: Option<&str>,
    breaking_only: bool,
) {
    let min_risk = risk_filter.map(|r| match r.to_lowercase().as_str() {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    });

    let kind_match = kind_filter.map(|k| k.to_lowercase());

    for diff in diffs.iter_mut() {
        diff.changes.retain(|change| {
            if let Some(min) = min_risk {
                let level = match change.risk {
                    SemanticRisk::High => 3,
                    SemanticRisk::Medium => 2,
                    SemanticRisk::Low => 1,
                };
                if level < min {
                    return false;
                }
            }
            if let Some(ref k) = kind_match {
                let change_kind = match change.kind {
                    SemanticChangeKind::Added => "added",
                    SemanticChangeKind::Removed => "removed",
                    SemanticChangeKind::Modified => "modified",
                    SemanticChangeKind::Renamed => "renamed",
                    SemanticChangeKind::Moved => "moved",
                    SemanticChangeKind::StyleOnly => "style_only",
                };
                if change_kind != k.as_str() {
                    return false;
                }
            }
            if breaking_only {
                if change.compatibility.status != SemanticCompatibilityStatus::Breaking
                    && change.compatibility.status
                        != SemanticCompatibilityStatus::PotentiallyBreaking
                {
                    return false;
                }
            }
            true
        });
    }

    diffs.retain(|d| !d.changes.is_empty());
}

fn print_summary_line(diffs: &[fl_core::SemanticFileDiff]) {
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut modified = 0usize;
    let mut high_risk = 0usize;
    let mut breaking = 0usize;

    for diff in diffs {
        for change in &diff.changes {
            match change.kind {
                SemanticChangeKind::Added => added += 1,
                SemanticChangeKind::Removed => removed += 1,
                SemanticChangeKind::Modified => modified += 1,
                _ => {}
            }
            if change.risk == SemanticRisk::High {
                high_risk += 1;
            }
            if change.compatibility.status == SemanticCompatibilityStatus::Breaking
                || change.compatibility.status == SemanticCompatibilityStatus::PotentiallyBreaking
            {
                breaking += 1;
            }
        }
    }

    println!(
        "{} file{}, +{} -{} ~{} symbols, {} high-risk, {} breaking",
        diffs.len(),
        if diffs.len() == 1 { "" } else { "s" },
        added,
        removed,
        modified,
        high_risk,
        breaking
    );
}

fn print_semantic_diffs_with_patch(
    diffs: &[fl_core::SemanticFileDiff],
    json: bool,
    patch: bool,
) -> Result<()> {
    if diffs.is_empty() {
        if json {
            let output = serde_json::json!({
                "semantic_changes": [],
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("No semantic changes.");
        }
        return Ok(());
    }
    if json {
        let output = serde_json::json!({
            "semantic_changes": diffs,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    for diff in diffs {
        print_semantic_file_diff(diff, patch);
    }
    Ok(())
}


fn print_intent_diff(
    groups: &[(String, Vec<fl_core::SemanticFileDiff>)],
    json: bool,
) -> Result<()> {
    if groups.is_empty() {
        if json {
            let output = serde_json::json!({
                "intent_groups": [],
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
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
        let output = serde_json::json!({
            "intent_groups": json_groups,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
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
            print_semantic_file_diff(diff, false);
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

fn build_undo_scope(
    exploration: Option<String>,
    session: Option<String>,
    workspace: Option<String>,
    actor: Option<String>,
    repo: &Repo,
) -> Result<UndoScope> {
    let exploration_id = match exploration {
        Some(val) => Some(resolve_exploration(&val, repo)?),
        None => None,
    };
    let session_id = match session {
        Some(val) => {
            let sessions = repo.list_sessions()?;
            let known_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
            Some(resolve_uuid_prefix(&val, &known_ids)?)
        }
        None => None,
    };
    Ok(UndoScope {
        exploration_id,
        session_id,
        workspace_name: workspace,
        actor,
    })
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("invalid UUID `{}`", value))
}

/// Resolve a UUID or UUID prefix against a list of known UUIDs.
/// Accepts full UUIDs or unique prefixes (like the 8-char short IDs shown
/// in `fl lock list` and `fl subscriptions`).
fn resolve_uuid_prefix(value: &str, known: &[Uuid]) -> Result<Uuid> {
    // Try exact parse first.
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(id);
    }
    // Prefix match against known UUIDs.
    let prefix = value.replace('-', "").to_lowercase();
    let matches: Vec<Uuid> = known
        .iter()
        .filter(|id| {
            id.simple().to_string().starts_with(&prefix)
        })
        .copied()
        .collect();
    match matches.len() {
        0 => bail!("no UUID matching prefix `{}`", value),
        1 => Ok(matches[0]),
        _ => bail!(
            "ambiguous UUID prefix `{}` — matches {} entries",
            value,
            matches.len()
        ),
    }
}

/// Resolve an exploration identifier that may be a UUID, UUID prefix, or
/// exploration title.
fn resolve_exploration(value: &str, repo: &Repo) -> Result<Uuid> {
    // Try exact UUID parse first.
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(id);
    }
    let explorations = repo.list_explorations()?;
    // Try title match (case-sensitive).
    let by_title: Vec<_> = explorations
        .iter()
        .filter(|e| e.title == value)
        .collect();
    if by_title.len() == 1 {
        return Ok(by_title[0].id);
    }
    if by_title.len() > 1 {
        bail!(
            "ambiguous exploration title `{}` — matches {} explorations",
            value,
            by_title.len()
        );
    }
    // Try UUID prefix match.
    let ids: Vec<Uuid> = explorations.iter().map(|e| e.id).collect();
    resolve_uuid_prefix(value, &ids)
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
        WsServerMessage::Ping { .. } | WsServerMessage::Pong { .. } => {}
        WsServerMessage::Subscribed { subscription_id, .. } => {
            println!("subscribed (id: {})", subscription_id);
        }
        WsServerMessage::EventNotification { event, .. } => {
            println!("[{}] {} {}", event.timestamp, event.actor, event.kind_name());
        }
        WsServerMessage::EventBroadcast { events } => {
            println!("event broadcast: {} events", events.len());
        }
        WsServerMessage::EventsAppended { repo_id, count } => {
            println!("events appended: {} new events in {}", count, repo_id);
        }
        WsServerMessage::SyncResponse { events } => {
            println!("sync response: {} events", events.len());
        }
        WsServerMessage::PresenceUpdate { actor, workspace, files, intent, departed, .. } => {
            if *departed {
                println!("presence: {} departed from {}", actor, workspace);
            } else {
                let intent_str = intent.as_deref().unwrap_or("");
                println!("presence: {} in {} editing {} {}", actor, workspace, files.join(", "), intent_str);
            }
        }
        WsServerMessage::PresenceBroadcast { repo_id, presences } => {
            println!("presence broadcast: {} actors in {}", presences.len(), repo_id);
        }
        WsServerMessage::AgentUpdate { agent_id, status, .. } => {
            println!("agent update: {} -> {}", agent_id, status);
        }
        WsServerMessage::SemanticFeed { symbol_name, kind, file_path, .. } => {
            println!("semantic: {} {} in {}", kind, symbol_name, file_path);
        }
        WsServerMessage::HeadsUpWarning { actor, symbol, path, action } => {
            println!("heads-up: {} is {} {} in {}", actor, action, symbol, path);
        }
        WsServerMessage::ConflictForecast { conflict_count, risk_level, .. } => {
            println!("conflict forecast: {} conflicts (risk: {})", conflict_count, risk_level);
        }
        WsServerMessage::ConflictAlert { alert } => {
            println!("conflict alert [{}]: {} ({} files)", alert.severity, alert.description, alert.affected_files.len());
        }
        WsServerMessage::TaskSync { task_id, status, assigned_agent_id, .. } => {
            let agent_str = assigned_agent_id.as_deref().map(|a| format!(" @{}", a)).unwrap_or_default();
            println!("task: {} [{}]{}", &task_id[..8.min(task_id.len())], status, agent_str);
        }
        WsServerMessage::TaskClaimResult { task_id, success, reason, .. } => {
            if *success {
                println!("task claimed: {}", task_id);
            } else {
                println!("task claim failed: {} — {}", task_id, reason.as_deref().unwrap_or("unknown"));
            }
        }
        WsServerMessage::TaskAssignment { task_id, title, priority, .. } => {
            println!("task assigned: {} [p{}] {}", task_id, priority, title);
        }
        WsServerMessage::LockResult { success, lock_id, reason } => {
            if *success {
                println!("lock ok: {}", lock_id.as_deref().unwrap_or("?"));
            } else {
                println!("lock failed: {}", reason.as_deref().unwrap_or("unknown"));
            }
        }
        WsServerMessage::LockUpdate { action, lock, .. } => {
            println!("lock {}: {} by {}", action, lock.patterns.join(", "), lock.owner);
        }
        WsServerMessage::LockList { locks, .. } => {
            println!("locks: {} active", locks.len());
        }
        WsServerMessage::PolicyVerdict { decisions } => {
            println!("policy: {} decisions", decisions.len());
        }
        WsServerMessage::Error { code, message } => {
            eprintln!("error [{}]: {}", code, message);
        }
        WsServerMessage::Directive { from_actor, directive, reason } => {
            let kind_str = match directive {
                DirectiveKind::Pause => "pause".to_string(),
                DirectiveKind::Resume => "resume".to_string(),
                DirectiveKind::Redirect { new_task } => format!("redirect -> {}", new_task),
                DirectiveKind::Abort { reason } => format!("abort: {}", reason),
            };
            let reason_str = reason.as_deref()
                .map(|r| format!(" ({})", r))
                .unwrap_or_default();
            println!("directive from {}: {}{}", from_actor, kind_str, reason_str);
        }
        WsServerMessage::WorkspacePreview { actor, workspace, diffs, timestamp: _ } => {
            println!("preview: {} in {} ({} files changed)", actor, workspace, diffs.len());
        }
    }
}

fn stream_live_task_updates(repo: &Repo, roost: &str, json: bool) -> Result<()> {
    let (ws, repo_path) = repo.ws_connect(roost)?;
    let sub = fl_core::WsSubscribeRequest {
        repo: repo_path,
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
    let ws = repo.ws_connect(roost).ok().map(|(client, _)| client);

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
        WsServerMessage::ConflictForecast { conflict_count, risk_level, .. } => {
            if *conflict_count == 0 {
                return None;
            }
            // Repo-level forecast — notify editor of overall risk
            Some(EditorNotification::ConflictForecastWarning {
                symbol: String::new(),
                path: String::new(),
                local_change: format!("{} conflicts", conflict_count),
                remote_change: format!("risk: {}", risk_level),
                remote_actor: String::new(),
            })
        }
        WsServerMessage::TaskSync { task_id, status, .. } => {
            Some(EditorNotification::TaskReady {
                task_id: task_id.clone(),
                title: status.clone(),
            })
        }
        WsServerMessage::TaskAssignment { task_id, title, .. } => {
            Some(EditorNotification::TaskReady {
                task_id: task_id.clone(),
                title: title.clone(),
            })
        }
        _ => None,
    }
}
