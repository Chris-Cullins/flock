# Flock Build TODO

This file tracks the full build-out from current scaffold to the complete architecture described in `/Users/chriscullins/src/flock/flock-architecture.md`.

## 0. Project Foundations

- [x] Create Rust workspace and split into `fl-core` and `fl-cli`
- [x] Add `fl init`, `fl checkpoint`, `fl log`, `fl diff --semantic` commands
- [x] Add initial `.flock` metadata layout (`event-log`, `snapshots`, `config.toml`)
- [x] Add first semantic analyzer for TS/JS via tree-sitter
- [x] Define crate boundaries for long-term architecture:
  - [x] `fl-storage` (event log + content store)
  - [x] `fl-semantic` (language analyzers + semantic merge)
  - [x] `fl-workflow` (explorations, sessions, work queue)
  - [x] `fl-collab` (presence, locks, subscriptions, gates)
  - [x] `fl-bridge-git` (colocated mode + import/export)

## 1. Core Storage Engine (Git-Compatible First)

- [x] Implement append-only event log API with typed events and versioned schema
- [x] Add event parent pointers and causal validation
- [x] Add event signatures (ed25519)
- [x] Add event replay and deterministic state reconstruction
- [x] Add checkpoints as first-class events (commit-equivalent)
- [x] Add undo events:
  - [x] `undo last`
  - [x] `undo --n`
  - [x] `undo --to`
  - [x] `undo --since`
- [x] Add file-scoped undo semantics in colocated mode (best-effort fallback)
- [x] Add repository refs abstraction (branches/tags/workspaces)
- [x] Add Merkle snapshot hash generation for checkpoints
- [x] Add storage integrity verifier command (`fl fsck`)
- [x] Implement `.flockignore` support:
  - [x] `.flockignore` file format (gitignore-compatible glob patterns)
  - [x] Respect `.flockignore` during `fl checkpoint` snapshot creation (exclude matched paths)
  - [x] Respect `.flockignore` during `fl diff` and `fl quick-save`
  - [x] Built-in default ignores (`.flock/`, `.git/`, `node_modules/`, `target/`, `__pycache__/`, `.env`)
  - [x] Nested `.flockignore` files (per-directory overrides, like gitignore)
  - [x] `fl status` command showing tracked/untracked/ignored files
  - [x] In colocated mode, optionally fall back to `.gitignore` if no `.flockignore` exists
- [x] Built-in secret detection on commit:
  - [x] Scan file contents during `fl checkpoint` for known secret patterns (AWS keys, OpenAI keys, private keys, generic high-entropy tokens)
  - [x] Hard block by default — commit fails with warning showing file:line and matched pattern
  - [x] `--allow-secrets` flag to override (recorded in event log as audit trail)
  - [x] `.flock/secrets.toml` config: custom patterns, allowed paths (test fixtures), toggle block vs warn
  - [x] Built-in pattern library: AWS (`AKIA...`), GCP, Azure, OpenAI (`sk-...`), GitHub tokens (`ghp_...`), private keys (`-----BEGIN.*PRIVATE KEY-----`), generic `password=`/`secret=`/`token=` assignments
  - [x] No `--no-verify` style escape hatch — `--allow-secrets` is the only bypass and it's auditable
- [x] Declarative hook system (`.flock/hooks.toml`, version-controlled):
  - [x] Hook points: `pre-commit`, `post-commit`, `pre-push`, `post-push`, `pre-merge`, `post-merge`, `pre-explore`, `post-explore-promote`
  - [x] Declarative config: command, block_on_failure (bool), timeout
  - [x] Hooks auto-apply to everyone who clones — no manual installation step
  - [x] Structured output (hooks can emit JSON for richer error messages / warnings)
  - [x] No `--no-verify` — skip requires `--skip-hooks` flag, recorded in event log
  - [x] Agent-aware: hooks can check `$FL_ACTOR` to run different rules for agents vs humans
  - [x] Hook execution report in `fl log` (which hooks ran, pass/fail, duration)

## 2. Git/JJ Compatibility Layer

- [x] Implement git-colocated mode (`.git` + `.flock` sidecar)
- [x] Map checkpoint operations to git commits
- [x] Map Flock refs to git refs/bookmarks strategy
- [x] Implement push/pull bridge to git remotes
- [x] Implement git import and export commands
- [x] Implement shadow mode safety checks and recovery docs
- [x] Define jj import design and metadata mapping
- [ ] Add `fl convert` one-command repo conversion workflow:
  - [ ] `fl convert --from git` — detect `.git/`, init `.flock/`, import full git history (all branches/tags), set up colocated mode
  - [ ] `fl convert --from jj` — detect `.jj/`, init `.flock/`, import jj history preserving change IDs and operation log
  - [ ] Progress reporting for large repos (180k+ commits)
  - [ ] Incremental/resumable conversion (don't restart from scratch if interrupted)
  - [ ] Post-conversion validation (`fl fsck` + compare checkout against original)
  - [ ] `--branch` filter to convert only specific branches
  - [ ] `--shallow` option to import only recent history (last N commits) for quick onboarding
  - [ ] `fl convert --to git` — export full flock history back to a clean `.git/` repo (checkpoints → commits, explorations → branches, tags preserved), then optionally remove `.flock/`

## 3. Semantic Layer (TS/JS First)

- [x] Expand TS/JS symbol extraction beyond declarations:
  - [x] Arrow functions assigned to const/let
  - [x] Function expressions
  - [x] Exported declarations and re-exports
  - [x] Interfaces, type aliases, enums
  - [x] Class fields and constructors
- [x] Add semantic change taxonomy:
  - [x] Added, Removed, Modified, Renamed, Moved, StyleOnly
- [x] Add risk scoring (Low/Medium/High)
- [x] Add impact tracking (affected symbols/files/modules)
- [x] Add compatibility checks for signature changes
- [x] Implement semantic merge for TS/JS with text fallback
- [x] Implement semantic conflict classification and explanation
- [x] Add machine-readable semantic diff output (`--json`)
- [x] Add plugin trait/API for additional analyzers
- [x] Add analyzer process boundary (FFI or gRPC) and lifecycle management
- [x] Add first non-TS/JS fallback analyzer contract tests

## 4. Developer Experience (CLI + Review)

- [x] Implement `fl diff --intent`
- [x] Implement `fl impact <path-or-symbol>`
- [x] Implement `fl merge --dry-run --semantic`
- [x] Implement semantic review views:
  - [x] `fl review <exploration>` summary mode
  - [x] `fl review --expand <n>` drill-down mode
  - [x] `fl review --full` line diff fallback
- [x] Add TUI views for exploration trees and task graph
- [x] Add shell completion and command help polish

## 5. Explorations and Workspaces

- [x] Implement exploration model with lifecycle states
- [x] Add exploration commands:
  - [x] `fl explore start`
  - [x] `fl explore list`
  - [x] `fl explore compare`
  - [x] `fl explore promote`
  - [x] `fl explore abandon`
- [x] Add TTL and background pruning for abandoned/expired explorations (`fl explore prune`)
- [x] Add workspace isolation model (base snapshot + overlay events)
- [x] Add workspace resource limits and policy enforcement
- [x] Add checkpoint/rollback shortcuts for agents (`fl quick-save`, `fl quick-restore`)

## 6. Agent Sessions and Provenance

- [x] Implement session entity and linkage to explorations/tasks
- [x] Track decisions (kept/discarded + reason + confidence)
- [x] Add resource usage accounting (tokens, runtime, external API calls)
- [x] Implement provenance query commands
- [x] Implement session replay command

## 7. Work Queue (Built-in Task Graph)

- [x] Implement task schema and DAG dependencies
- [x] Add task lifecycle commands:
  - [x] `fl task create`
  - [x] `fl task list`
  - [x] `fl task claim`
  - [x] `fl task done`
  - [x] `fl task show`
  - [x] `fl task graph`
- [x] Implement `fl ready` priority + dependency-unblocked selection
- [x] Add `--json` output for agent consumption
- [x] Link tasks to events/checkpoints/explorations
- [x] Add discovered-from relationships and auto-linking
- [x] Implement task compaction for old completed tasks

## 8. Collaboration Layer

- [x] Implement presence model and heartbeat protocol
- [x] Add advisory lock API with TTL:
  - [x] `fl lock acquire`
  - [x] `fl lock list`
  - [x] `fl lock release`
- [x] Implement change subscriptions and notification filters
- [x] Implement human-in-the-loop gates and policies
- [x] Implement continuous auto-rebase for active workspaces
- [x] Implement conflict resolution workflow:
  - [x] detect
  - [x] classify
  - [x] suggest
  - [x] resolve
  - [x] verify
  - [x] record

## 8.5. Semantic Merge Improvements

These should land before the storage rework — they improve merge quality independent of the backend.

- [x] Auto-resolve StyleOnly vs logic conflicts in semantic merge (one side is whitespace-only → auto-pick the logic change)
- [x] Cross-file semantic conflict detection (signature change in file A breaks callers in file B)
- [x] Wire `fl impact` dependency data into merge conflict reporting
- [x] Add additional language analyzers:
  - [x] Python (tree-sitter)
  - [x] Go (tree-sitter)
  - [x] Rust (tree-sitter)
  - [x] C# (tree-sitter — original arch doc target)
- [x] Add structured analyzers for non-programming languages (nice-to-have):
  - [x] JSON/JSONL — diff by top-level keys instead of lines
  - [x] YAML/TOML — diff by key paths
  - [x] XML/HTML — diff by element/attribute
  - [x] CSS/SCSS — diff by selector/rule
  - [x] Markdown — diff by heading/section
- [x] AST cache keyed by content hash (don't re-parse unchanged files)

## 9. Native Storage Engine (Phase 2 Backend)

- [x] Design native `.flock/store` layout
- [x] Implement block-level content store with BLAKE3 keys
- [x] Implement file index mapping `(path, event) -> block refs`
- [x] Evaluate block strategy:
  - [ ] fixed-size chunking
  - [x] content-defined chunking
  - [ ] language-aware chunking
- [x] Implement native copy-on-write snapshots
- [x] Implement sub-file undo (true file-scoped rewind)
- [x] Implement migration command `fl migrate --native`
- [x] Add performance benchmarks against colocated mode

## 9.5. Large-Repo Scaling

Critical for repos at scale (millions of LOC, hundreds of thousands of events). Should be built alongside or immediately after the native storage engine.

- [x] Indexed event log — replace single JSONL scan with segmented log + B-tree/LSM index by event ID, timestamp, and actor (target: O(log n) lookup, not O(n) scan)
- [x] Materialized state snapshots — periodically snapshot computed state so replay starts from last materialized point, not from the beginning of time
- [x] Content-addressable snapshot dedup — two checkpoints where only 3 files changed should not store the entire repo twice (file-level dedup at minimum, block-level ideally)
- [x] Segmented refs — replace single `refs.json` with one-file-per-ref or sorted index with append-only updates to avoid rewriting all refs on every update
- [ ] Lazy/partial clone — only materialize snapshots and events the client actually needs (analogous to git partial clone + sparse checkout)
- [x] Event log compaction — summarize/archive old event ranges while preserving Merkle integrity (keep checkpoints, compact fine-grained events between them)
- [ ] Incremental semantic indexing — update AST cache and dependency graph incrementally on new events instead of full recompute
- [x] Large-repo benchmark suite — test against synthetic repos at 1M+ events, 10K+ files, 5M+ LOC to validate scaling targets

## 10. Flock Remote (Self-Hosted Repo Server)

Flock's own remote hosting — no dependency on GitHub/GitLab. Replaces the traditional forge model with semantic-aware collaboration.

### 10a. Core Transport
- [ ] Define remote repository URL scheme and discovery (`fl remote add origin flock://host/repo`)
- [ ] Implement event sync protocol — client sends events after last-known server event ID, server responds with events client is missing
- [ ] Implement snapshot/blob transport — upload/download content blocks over HTTPS
- [ ] Implement `fl push` to flock remote
- [ ] Implement `fl pull` from flock remote
- [ ] Handle concurrent push conflicts (reject or auto-merge based on semantic analysis)

### 10b. Auth and Multi-User
- [ ] SSH key and token-based authentication (leverage existing ed25519 signing)
- [ ] User/agent identity model — map actors to authenticated users
- [ ] Repository-level access control (read/write/admin per user or team)
- [ ] Per-branch and per-path write permissions

### 10c. Server-Side Semantic Analysis
- [ ] Server-side semantic diff computation (so the web UI doesn't need tree-sitter locally)
- [ ] Server-side merge preview (dry-run semantic merge on push)
- [ ] Semantic change indexing — maintain searchable index of all semantic changes across history

### 10d. Web UI for Semantic Review
- [ ] Render `fl review` output in browser — semantic change list grouped by risk
- [ ] Expandable drill-down into individual semantic changes
- [ ] Full line-level diff fallback view
- [ ] Inline commenting on semantic changes (not just lines)
- [ ] Review approval workflow (approve/request-changes per semantic unit)
- [ ] Exploration comparison view (side-by-side exploration outcomes)

### 10e. Server Infrastructure
- [ ] Repository storage layout on server (event log + content store + refs)
- [ ] Tiered storage policies (hot/warm/cool — recent events on SSD, old history on object storage)
- [ ] Background jobs: compaction, pruning, semantic indexing, garbage collection
- [ ] Observability: metrics, tracing, structured logs
- [ ] Backup and disaster recovery for hosted repos
- [ ] Webhook system for external integrations (CI, chat, etc.)

## 10.5. Real-Time Collaboration Features

Layer on top of the remote server via websocket event streaming. See `docs/remote-features-brainstorm.md` for full vision.

- [ ] Websocket event stream — clients subscribe to events matching filters (by file, symbol, module, agent)
- [ ] Ghost text / semantic presence overlays — see what another dev or agent is currently changing at the symbol level, rendered as faint overlay in editor
- [ ] Semantic feed — real-time stream of meaningful changes ("Agent A added class CurrencyConverter, 3 methods") instead of raw commit notifications
- [ ] "Heads up" warnings — proactive notification when another agent/dev starts working on a symbol you're currently editing
- [ ] Exploration spectating — watch an agent's semantic trail in real time (exploration starts, checkpoints, abandons, new approaches)
- [ ] Continuous review — review semantic changes as they land instead of batch-reviewing a finished PR; approve individual semantic units incrementally
- [ ] Conflict forecast — server predicts likely conflicts based on active explorations' semantic change sets and warns before they happen
- [ ] Editor plugin protocol — define LSP-like protocol for editors to consume ghost text, presence, and heads-up warnings
- [ ] Real-time task sync across connected repos:
  - [ ] Task events (create, claim, done, fail) broadcast instantly to all connected clients via websocket
  - [ ] `fl task list` / `fl ready` reflect remote state in real time (no poll/pull needed)
  - [ ] Agent claims a task → all other agents see it claimed instantly, pick next available
  - [ ] Task dependency resolution propagates live (task done → blocked tasks unblock across all clients)
  - [ ] Cross-repo task visibility — tasks in repo A can reference/block tasks in repo B
  - [ ] Dashboard view of all agent activity across all connected repos (who's working on what, task throughput, queue depth)

## 11. Intelligence Layer

- [ ] Implement natural-language history queries (`fl query`)
- [ ] Build vector index for intents and semantic changes
- [ ] Add AI-assisted intent extraction for commits/events
- [ ] Add AI-assisted conflict resolution suggestions
- [ ] Add confidence scoring and gate integration

## 12. Security, Reliability, and Compliance

- [ ] Threat model for agent and human actors
- [ ] Encrypt sensitive metadata at rest (configurable)
- [ ] Audit trail hardening and tamper-evidence checks
- [ ] Offline mode behavior and reconnect reconciliation
- [ ] Backup and restore strategy for `.flock` data
- [ ] Disaster recovery playbooks

## 13. QA and Performance

- [ ] Unit and integration test matrix across core crates
- [ ] Property tests for event replay invariants
- [ ] Fuzzing for parsers and merge engine
- [ ] Large-repo benchmarks and regression thresholds
- [ ] Concurrency stress tests (10+ agents)
- [ ] Compatibility tests across macOS/Linux/Windows

## 14. Documentation and Adoption

- [ ] Author migration guide from git and jj
- [ ] Write architecture and data model reference docs
- [ ] Publish command reference with examples
- [ ] Provide starter workflows for TS monorepos
- [ ] Add contribution guide and RFC process
- [ ] Add release plan and versioning policy

## Immediate Next Milestone (Suggested)

- [x] M1.1 Add `fl explore start/list/promote/abandon`
- [x] M1.2 Add semantic diff JSON output
- [x] M1.3 Improve TS/JS analyzer coverage (arrow functions, interfaces, types)
- [x] M1.4 Add `fl undo` basic variants on event timeline
- [x] M1.5 Add basic git-colocated commit/push/pull bridge stubs

## Terminology / UX Polish

- [ ] Add `fl commit` as a user-facing alias for `fl checkpoint` (keep checkpoint as the internal event type)
- [ ] Update CLI help text to use "commit" instead of "checkpoint" in user-facing descriptions
- [ ] Update README to use "commit" terminology
- [ ] Update `fl log` output to say "commit" instead of "checkpoint"
- [ ] Audit all CLI output strings and error messages for consistent terminology

## Bugs

- [ ] `fl init --colocated` writes `mode = "git-compatible"` in `.flock/config.toml` instead of `"git-colocated"` — the `--colocated` flag is ignored when writing the config
