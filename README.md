<p align="center">
  <img src="assets/flock-goose.svg" alt="Gus the Flock Goose" width="160"/>
</p>

<h1 align="center">Flock</h1>

<p align="center">
  <strong>Version control for a world where AI agents write code alongside you.</strong>
</p>

<p align="center">
  <a href="#installation">Installation</a> &middot;
  <a href="#why-flock">Why Flock</a> &middot;
  <a href="#quick-start">Quick Start</a> &middot;
  <a href="#features">Features</a> &middot;
  <a href="#architecture">Architecture</a> &middot;
  <a href="#commands">Commands</a> &middot;
  <a href="#roadmap">Roadmap</a>
</p>

---

Git was built for humans emailing patches. Flock is built for 10 AI agents refactoring your codebase at 3 AM.

Flock preserves git's brilliant foundations &mdash; content-addressable storage, Merkle trees, distributed operation &mdash; while rethinking everything else for high-frequency, multi-agent, semantically-aware development.

## Why Flock

### The problem

You've got an AI agent renaming a function while another agent adds a call to it. Git sees two conflicting text edits and panics. You spend 20 minutes resolving a "conflict" that wasn't actually a conflict at all.

Meanwhile, another agent made a bad change 47 commits ago. Good luck with `git revert`.

### What Flock does differently

| Git | Flock |
|-----|-------|
| Text-based diffs &mdash; lines added/removed | **Semantic diffs** &mdash; "function `processPayment` signature changed, 3 callers affected" |
| Merge conflicts on formatting changes | **AST-level merging** that knows a reformat and a new method don't conflict |
| `git revert` creates a new commit, can fail | **O(1) undo** &mdash; rewind by count, time, specific event, or single file |
| One working tree, branch switching | **Isolated workspaces** &mdash; 10 agents, zero merge hell |
| No concept of agent sessions | **First-class agent tracking** &mdash; sessions, provenance chains, resource usage |
| Manual branch management | **Explorations** &mdash; lightweight experiments with auto-prune TTLs |

## Installation

**Prerequisites:** Rust toolchain (edition 2024)

```bash
# Clone and install
git clone https://github.com/Chris-Cullins/flock.git
cd flock
cargo install --path crates/fl-cli

# Verify
fl --help
```

Or build without installing:

```bash
cargo build --release
# Binary at target/release/fl
```

## Quick Start

```bash
# Initialize Flock in your project
fl init

# See what's tracked
fl status

# Make some changes, then commit
fl commit -m "implement payment processor"

# See what changed at the code level, not the line level
fl diff --semantic

# Oops, that was wrong. Undo it instantly.
fl undo

# Start an exploration (like a lightweight branch)
fl explore start --title "try-new-parser"

# ... hack away ...
fl commit -m "parser v2"

# It worked! Promote it.
fl explore promote <id>

# Or it didn't. Abandon and auto-cleanup.
fl explore abandon <id>
```

### Working alongside git

Flock runs as a sidecar next to your existing `.git` directory:

```bash
# Initialize in colocated mode
fl init --colocated

# Use Flock for semantic operations, git for collaboration
fl diff --semantic          # See semantic changes
fl git commit -m "feature"  # Commit through Flock (tracks lineage)
fl git push                 # Push to remote
fl git import               # Import git history into Flock
```

### Converting existing repos

```bash
fl convert from-git                # Import full git history into Flock
fl convert from-git --shallow 100  # Just the last 100 commits
fl convert from-jj                 # Import from Jujutsu
fl convert to-git                  # Export back to a clean git repo
```

## Features

### Semantic Diff & Merge

Flock parses your code into ASTs using tree-sitter, so it understands what changed at the code structure level.

```bash
# See semantic changes grouped by intent
fl diff --intent

# Analyze the blast radius of a change
fl impact src/PaymentProcessor.ts

# Preview a merge before doing it
fl merge --dry-run --semantic base left right --json
```

**What the semantic layer detects:**
- Functions, classes, methods, interfaces, type aliases, enums, exports
- Signature compatibility (compatible / potentially-breaking / breaking)
- Change classifications: Added, Removed, Modified, Renamed, Moved, StyleOnly
- Risk scoring: Low, Medium, High
- Cross-file impact (signature change in file A breaks callers in file B)

**Conflict classifications** (machine-readable, not just "<<<< HEAD"):
- `divergent_edit` &mdash; both sides changed the same symbol differently
- `delete_vs_edit` &mdash; one side deleted what the other modified
- `concurrent_addition` &mdash; both sides added a symbol with the same name
- `kind_mismatch` &mdash; symbol changed type (e.g., function to class)
- `text_fallback` &mdash; unsupported language, fell back to text diff

**Language support:**
- **Tree-sitter AST analyzers:** JavaScript, JSX, TypeScript, TSX, Python, Go, Rust, C#
- **Structured format analyzers:** JSON/JSONL (by key), YAML/TOML (by key path), XML/HTML (by element), CSS/SCSS (by selector), Markdown (by section)
- All other languages fall back to text-based analysis

### Instant Undo

Every operation is an append-only event. Undo is pointer movement, not file rewriting. O(1), always.

```bash
fl undo                    # Undo last event
fl undo --n 5              # Undo last 5 events
fl undo --to <event-id>    # Rewind to a specific point
fl undo --since 10m        # Undo everything from the last 10 minutes
fl undo --file src/app.ts  # Undo changes to just one file
```

### Explorations & Workspaces

Explorations are lightweight, time-limited experiments. Workspaces provide isolated environments for parallel agents.

```bash
# Explorations
fl explore start --title "refactor-auth"
fl explore list
fl explore compare <id1> <id2>
fl explore promote <id>
fl explore abandon <id>
fl explore prune --older-than 7d

# Workspaces (isolated agent environments)
fl workspace create agent-1 --auto-rebase
fl workspace list
fl workspace info agent-1
fl workspace limits agent-1 --max-snapshots 50
```

### Agent Sessions & Task Graph

Built-in primitives for tracking what agents are doing and why.

```bash
# Sessions
fl session start --task task-1 --agent claude
fl session list --active
fl session show <id>
fl session decision <session-id> <exploration-id> --action kept --reason "passes tests"
fl session usage <id> --tokens 50000 --runtime-ms 30000
fl session complete <id> --result "success"
fl session provenance <exploration-id>    # Full lineage chain
fl session replay <id>                    # Replay timeline

# Tasks
fl task create "Implement auth" --description "OAuth2 flow" --depends-on task-0
fl task list
fl task claim <id> --assignee agent-1
fl task done <id> --result "implemented and tested"
fl task graph --json                      # Visualize dependencies
fl ready --json                           # What's unblocked right now?

# Quick save/restore (for agents)
fl quick-save --tag "before-refactor"
fl quick-restore
```

### Secret Detection

Built-in scanning on every commit. No secrets leak by accident.

```bash
fl commit -m "add config"
# ERROR: potential secret detected in config.yaml:12 (AWS access key)

fl commit -m "add config" --allow-secrets   # Override (recorded in audit log)
```

- **Pattern library:** AWS (`AKIA...`), GCP, Azure, OpenAI (`sk-...`), GitHub (`ghp_...`), private keys, generic password/token assignments
- **Hard block by default** &mdash; commit fails, no `--no-verify` escape hatch
- **`--allow-secrets`** is the only bypass, and it's recorded in the audit trail
- **`.flock/secrets.toml`** &mdash; custom patterns, allowed paths (test fixtures), toggle block vs warn

### Declarative Hooks

Version-controlled hooks that auto-apply to everyone who clones. No manual installation step.

```toml
# .flock/hooks.toml
[[hooks]]
point = "pre-commit"
command = "cargo test"
block_on_failure = true
timeout = 60
```

- **Hook points:** `pre-commit`, `post-commit`, `pre-push`, `post-push`, `pre-merge`, `post-merge`, `pre-explore`, `post-explore-promote`
- **Agent-aware:** hooks can check `$FL_ACTOR` to run different rules for agents vs humans
- **Structured output:** hooks can emit JSON for richer error messages
- **No `--no-verify`** &mdash; `--skip-hooks` is explicit and auditable

### Remote Sync

Push and pull to Flock remotes (Roost servers). Real-time streaming via WebSocket.

```bash
# Configure a remote
fl roost add origin flock://my-server.example.com/repo

# Authenticate
fl remote login my-server.example.com --token <token>

# Sync
fl push
fl pull

# Stream live events
fl watch --path "src/**" --agent claude --json
```

### Collaboration

Multi-agent coordination with presence, locks, subscriptions, and quality gates.

```bash
# Presence — who's working where
fl presence heartbeat --workspace agent-1 --file src/auth.rs --intent "refactoring"
fl presence list

# Advisory locks
fl lock acquire src/payment.rs --ttl 300
fl lock list
fl lock release <id>

# Subscriptions — get notified on changes
fl subscribe --path "src/api/**" --symbol "processPayment"

# Quality gates — human-in-the-loop approval
fl gate create --condition high-risk --policy block
fl gate check src/payment.rs
fl gate approve <id>

# Conflict resolution workflow
fl conflict detect --workspace agent-1
fl conflict suggest <id>
fl conflict resolve <id> --resolution "kept left side"

# Rebase
fl rebase --workspace agent-1
```

### Intelligence Layer

Natural language search over your event history. Optional AI-assisted features.

```bash
# Search history in plain English
fl query "who changed the payment module last week"

# With AI enhancement (requires FL_LLM_API_KEY)
fl query "why was the auth flow refactored" --ai

# Manage the search index
fl intel rebuild
fl intel stats

# Confidence scoring for agent sessions
fl confidence --verbose
```

### Security & Backup

Encryption at rest, audit trails, and backup/restore for `.flock` data.

```bash
# Encrypt/decrypt the signing key
fl key encrypt
fl key decrypt
fl key status

# Audit the event log for anomalies
fl audit --json

# Backup and restore
fl backup create ./my-backup.tar.gz
fl backup restore ./my-backup.tar.gz
fl backup verify ./my-backup.tar.gz
```

### Code Review

Review explorations at the semantic level, not the line-diff level.

```bash
fl review <exploration-id>
fl review <exploration-id> --expand 5    # Show context
fl review <exploration-id> --full        # Everything
```

### Storage Integrity

```bash
fl log                     # Inspect the event log
fl fsck                    # Verify Merkle roots and event chain integrity
```

## Architecture

```
                    ┌─────────────┐
                    │   fl-cli    │  CLI (clap)
                    └──────┬──────┘
                           │
                    ┌──────▼──────┐
                    │   fl-core   │  Repo orchestration & public API
                    └──────┬──────┘
                           │
          ┌────────────────┼────────────────┐
          │                │                │
  ┌───────▼──────┐ ┌──────▼───────┐ ┌──────▼──────┐
  │  fl-storage  │ │ fl-semantic  │ │ fl-workflow  │
  │  Event log,  │ │ Tree-sitter, │ │ Explorations │
  │  refs, snaps │ │ AST diff/    │ │ undo, replay │
  └──────────────┘ │ merge engine │ └──────────────┘
                   └──────────────┘
          ┌────────────────┼────────────────┐
          │                                 │
  ┌───────▼──────┐                  ┌───────▼──────┐
  │ fl-bridge-git│                  │  fl-collab   │
  │ Git colocated│                  │ Coordination │
  │ import/export│                  │  primitives  │
  └──────────────┘                  └──────────────┘
```

**7 crates**, layered from foundation to CLI:

| Crate | Purpose |
|-------|---------|
| **fl-cli** | Clap-based CLI, delegates to fl-core |
| **fl-core** | `Repo` struct &mdash; central entry point for all operations |
| **fl-storage** | Append-only JSONL event log, JSON refs, snapshot store |
| **fl-semantic** | Tree-sitter analyzers, semantic diff/merge, plugin registry |
| **fl-workflow** | Exploration lifecycle, undo timeline, event replay |
| **fl-bridge-git** | Git colocated mode, import/export, shadow safety checks |
| **fl-collab** | Agent coordination (presence, locks, subscriptions, gates) |

### Core design principles

- **Event sourcing** &mdash; All state changes are immutable, append-only events. Current state is reconstructed by replay. Undo is pointer movement.
- **Semantic-first** &mdash; AST-level understanding via tree-sitter, falling back gracefully to text diff for unsupported languages.
- **Pluggable analyzers** &mdash; `SemanticAnalyzerPlugin` trait with registry. Process analyzers run out-of-process via JSON-over-stdio with auto-restart.
- **Layered degradation** &mdash; Every layer is optional. Semantic fails? Text fallback. Process analyzer crashes? Auto-restart. Git not present? Pure Flock mode.
- **Content-addressable** &mdash; BLAKE3 Merkle trees for integrity, deduplication, and deterministic snapshots.

### `.flock` directory layout

```
.flock/
├── config.toml              # Mode, semantic settings, analyzer config
├── event-log/events.jsonl   # Append-only event history
├── refs/refs.json           # Branches, tags, workspaces
├── keys/ed25519.sk          # Optional event signing key
├── secrets.toml             # Secret detection config
├── hooks.toml               # Declarative hook definitions
└── snapshots/<uuid>/        # Commit snapshots
```

## Commands

### Core

| Command | Description |
|---------|-------------|
| `fl init [--colocated] [--native]` | Initialize a Flock repository |
| `fl commit -m <msg> [--allow-secrets] [--skip-hooks]` | Create a commit snapshot |
| `fl status [--json]` | Show working directory status |
| `fl log` | Show event history |
| `fl fsck` | Verify storage integrity |
| `fl diff [--semantic] [--intent] [--json] [FROM] [TO]` | Show changes (working dir or between commits) |
| `fl impact <path> [--json]` | Analyze blast radius of a change |
| `fl merge --dry-run --semantic <base> <left> <right>` | Preview semantic merge |
| `fl undo [--n N] [--to ID] [--since DUR] [--file PATH]` | Undo events |
| `fl review <exploration-id> [--expand N] [--full]` | Review exploration semantically |

### Explorations & Workspaces

| Command | Description |
|---------|-------------|
| `fl explore start\|list\|promote\|abandon\|compare\|prune\|tree` | Manage explorations |
| `fl workspace create\|list\|info\|limits` | Manage isolated workspaces |
| `fl rebase --workspace <name>` | Rebase a workspace onto latest |
| `fl auto-rebase` | Auto-rebase all enabled workspaces |

### Agent Sessions & Tasks

| Command | Description |
|---------|-------------|
| `fl session start\|list\|show\|complete\|fail\|replay\|...` | Track agent sessions |
| `fl task create\|list\|show\|claim\|done\|fail\|graph\|link\|compact` | Manage task graph |
| `fl ready [--json] [--live]` | List dependency-unblocked tasks |
| `fl quick-save [--tag TAG]` | Quick commit for agents |
| `fl quick-restore` | Restore to before last event |

### Collaboration

| Command | Description |
|---------|-------------|
| `fl presence heartbeat\|depart\|list` | Multi-agent presence tracking |
| `fl lock acquire\|list\|release` | Advisory resource locking |
| `fl subscribe --path\|--symbol\|--module` | Subscribe to changes |
| `fl unsubscribe <id>` | Cancel a subscription |
| `fl subscriptions [--json]` | List active subscriptions |
| `fl gate create\|list\|check\|approve\|reject\|delete` | Human-in-the-loop quality gates |
| `fl conflict detect\|suggest\|resolve\|verify\|record\|list` | Conflict resolution workflow |

### Remote Sync

| Command | Description |
|---------|-------------|
| `fl roost add\|remove\|list\|set-url` | Manage Flock remotes |
| `fl push [roost] [branch]` | Push events to a remote |
| `fl pull [roost] [branch]` | Pull events from a remote |
| `fl watch [--path] [--symbol] [--agent] [--kind] [--json]` | Stream live events via WebSocket |
| `fl remote login\|logout\|status` | Manage remote authentication |

### Intelligence & Security

| Command | Description |
|---------|-------------|
| `fl query <text> [--ai] [--json]` | Search event history with natural language |
| `fl intel rebuild\|stats` | Manage the search index |
| `fl confidence [--verbose] [--json]` | Show session confidence score |
| `fl key encrypt\|decrypt\|status` | Manage signing key encryption |
| `fl audit [--json]` | Audit event log for anomalies |
| `fl backup create\|restore\|verify` | Backup and restore `.flock` data |

### Repository Management

| Command | Description |
|---------|-------------|
| `fl refs list\|set\|delete` | Manage branches, tags, workspaces |
| `fl convert from-git\|from-jj\|to-git` | Convert repositories to/from Flock |
| `fl git status\|commit\|push\|pull\|import\|export` | Git bridge operations |
| `fl index [--clear]` | Rebuild or clear the semantic index |
| `fl migrate --native` | Migrate to native block-level storage |
| `fl compact [--older-than DUR]` | Archive old events |
| `fl materialize` | Snapshot replay state for faster ops |
| `fl completions <shell>` | Generate shell completions |
| `fl editor-server` | Start editor plugin protocol server |

## Roadmap

Flock is being built in phases. Here's where things stand:

| Phase | Status | Description |
|-------|--------|-------------|
| 0. Foundations | Done | Workspace, CLI, `.flock` layout, tree-sitter |
| 1. Core Storage | Done | Event log, refs, checksums, undo, secret detection, hooks |
| 2. Git Compatibility | Done | Colocated mode, import/export bridge, `fl convert` |
| 3. Semantic Layer | Done | Symbol extraction, diff, merge, conflict classification |
| 4. Developer Experience | Done | `diff --intent`, `impact`, `review`, checkpoint-to-checkpoint diff |
| 5. Explorations & Workspaces | Done | Lifecycle, TTL, limits, comparison |
| 6. Agent Sessions | Done | Sessions, decisions, resource tracking, provenance |
| 7. Work Queue | Done | Task graph, dependencies, ready selection |
| 8. Collaboration | Done | Real-time presence, advisory locks, auto-rebase, conflict resolution |
| 8.5. Semantic Improvements | Done | 8 language analyzers, structured formats, cross-file conflicts, AST cache |
| 9. Native Storage | Done | Block-level content store, sub-file undo, segmented log, compaction |
| 10. Remote Sync (Client) | Done | Push/pull, auth, WebSocket streaming, editor protocol |
| 11. Intelligence | Done | NLP history queries, TF-IDF index, AI-assisted merge, confidence scoring |
| 12. Security | Done | Encryption at rest, audit trail, backup/restore, disaster recovery |
| 10-R. Remote Server (Roost) | Planned | Server-side sync, web UI, tiered storage (separate repo) |
| 13. QA & Performance | Planned | Fuzzing, benchmarks, test matrix |
| 14. Docs & Adoption | Planned | Migration guides, tutorials |

See [TODO.md](TODO.md) for the full build plan and [flock-architecture.md](flock-architecture.md) for the design document.

## Development

```bash
cargo build                          # Build all crates
cargo test                           # Run all tests
cargo test -p fl-semantic            # Test a specific crate
cargo run -p fl-cli -- <command>     # Run without installing
```

## License

MIT
