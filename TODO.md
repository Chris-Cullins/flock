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
- [x] Add `fl convert` one-command repo conversion workflow:
  - [x] `fl convert --from git` — detect `.git/`, init `.flock/`, import full git history (all branches/tags), set up colocated mode
  - [x] `fl convert --from jj` — detect `.jj/`, init `.flock/`, import jj history preserving change IDs and operation log
  - [x] Progress reporting for large repos (180k+ commits)
  - [x] Incremental/resumable conversion (don't restart from scratch if interrupted)
  - [x] Post-conversion validation (`fl fsck` + compare checkout against original)
  - [x] `--branch` filter to convert only specific branches
  - [x] `--shallow` option to import only recent history (last N commits) for quick onboarding
  - [x] `fl convert --to git` — export full flock history back to a clean `.git/` repo (checkpoints → commits, explorations → branches, tags preserved), then optionally remove `.flock/`

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
- [x] Add checkpoint-to-checkpoint diff:
  - [x] `fl diff <checkpoint-a> <checkpoint-b>` — compare two checkpoints by ID/prefix
  - [x] `fl diff <checkpoint>` — compare a specific checkpoint against the working directory
  - [x] Support `--semantic`, `--intent`, and `--json` flags for checkpoint-to-checkpoint diff
  - [x] Show file-level summary (added/modified/deleted) and semantic-level changes

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
- [x] Lazy/partial clone — only materialize snapshots and events the client actually needs (analogous to git partial clone + sparse checkout)
  - [x] Phase 1: `fl clone` command — convenience wrapper for init + remote add + pull
  - [x] Phase 2: Shallow clone (`--depth N`) — add `depth` to EventPullRequest, graft markers, `fl fetch --deepen N`
  - [x] Phase 3: Sparse checkout (`--sparse "pattern"`) — pull all events but only fetch blocks for matching files; `fl sparse add/remove/list`
  - [x] Phase 4: Focus clone (`--focus <build-target>`) — parse build manifests (Cargo.toml, package.json, go.mod) to compute package dependency closure, fetch only those paths
  - [x] Phase 5: Lazy block fetching (`--lazy`) — download events + snapshot indices only, fault-in blocks on demand via BlockFaultHandler
  - [x] Phase 6: `fl pin <pattern>` — eagerly fetch blocks matching pattern for offline access
  - [x] Phase 7: Build-integrated recovery (`fl fetch --resolve-missing`) — detect missing files from build errors, auto-fetch needed blocks
- [x] Event log compaction — summarize/archive old event ranges while preserving Merkle integrity (keep checkpoints, compact fine-grained events between them)
- [x] Incremental semantic indexing — update AST cache and dependency graph incrementally on new events instead of full recompute
- [x] Large-repo benchmark suite — test against synthetic repos at 1M+ events, 10K+ files, 5M+ LOC to validate scaling targets

## 10. Remote Sync (Client-Side)

Client-side commands and protocol for syncing with a Flock remote (server lives in `../roost`).

### 10a. Remote Config and Transport Protocol
- [x] Define remote repository URL scheme and discovery (`fl roost add origin flock://host/repo`)
- [x] Define wire protocol spec for event sync (shared between flock and roost)
- [x] Implement event sync protocol (client half) — send events after last-known server event ID, receive missing events
- [x] Implement snapshot/blob transport (client half) — upload/download content blocks over HTTPS
- [x] Implement `fl push` to flock remote
- [x] Implement `fl pull` from flock remote
- [x] Handle push rejection (server rejects on conflict) — display semantic conflict info, prompt for merge

### 10b. Client-Side Auth
- [x] SSH key and token-based authentication (leverage existing ed25519 signing)
- [x] `fl remote login` / `fl remote logout` — token management
- [x] Store credentials securely (keychain/credential-helper pattern)

### 10c. Real-Time Client (Websocket)
- [x] Websocket connection lifecycle — connect to remote, authenticate, maintain heartbeat
- [x] Subscribe to event streams with filters (by file, symbol, module, agent)
- [x] `fl task list` / `fl ready` reflect remote state in real time (no poll/pull needed)
- [x] Receive "heads up" warnings when another agent/dev starts working on a symbol you're editing
- [x] Receive conflict forecast warnings based on active explorations' semantic change sets
- [x] Editor plugin protocol — define LSP-like protocol for editors to consume ghost text, presence, and heads-up warnings

## 10-R. Remote Server (Roost — `../roost`)

Server-side features live in the Roost repo. Tracked here for cross-reference only. See `../roost-git/TODO.md` for canonical Roost backlog. Items marked `[x]` are done in Roost.

### 10-R.a. Server Core
- [x] WebSocket gateway with auth, heartbeat, reconnect, backpressure
- [x] Event routing — receive events from clients, broadcast to subscribers
- [x] Task broker — atomic task claiming, claim TTL, force-release
- [x] Presence manager — file-level presence tracking, stale record expiration
- [x] Storage layer — authoritative event log with indexes, content block store, task DB
- [x] Token-based auth (personal tokens and service tokens)
- [ ] Concurrent push conflict detection (reject or auto-merge based on semantic analysis)
- [ ] User/agent identity model — map actors to authenticated users
- [ ] Repository-level access control (read/write/admin per user or team)
- [ ] Per-branch and per-path write permissions

### 10-R.b. Server-Side Semantic Analysis
- [ ] Server-side semantic diff computation (so the web UI doesn't need tree-sitter locally)
- [ ] Server-side merge preview (dry-run semantic merge on push)
- [ ] Semantic change indexing — maintain searchable index of all semantic changes across history

### 10-R.c. Web UI
- [x] Phase 1 minimal web UI — live dashboard, task board, basic review placeholder, presence panel
- [ ] Semantic review page — risk-sorted semantic changes with per-unit approval flow
- [ ] Expandable drill-down into individual semantic changes with actual code diff
- [ ] Full line-level diff fallback view
- [ ] Inline threaded commenting on semantic changes (not just lines)
- [ ] Cross-file change grouping and impact visualization
- [ ] Review policies — auto-approve StyleOnly, require reviewers for Medium/High-risk
- [ ] Continuous review mode — review changes as they land, not batch PRs
- [ ] Exploration inspector — tree view of agent's problem-solving process (attempts, abandoned branches, reasons, promotion path, resource usage)
- [ ] Agent console — fleet management dashboard (status, task assignments, token usage, throughput, anomaly detection, intervention controls)
- [ ] Timeline/history — semantic-level event history filtered by actor/intent/scope, grouped by logical unit of work

### 10-R.d. Server Infrastructure
- [ ] Tiered storage policies (hot/warm/cool — recent events on SSD, old history on object storage)
- [ ] Background jobs: compaction, pruning, semantic indexing, garbage collection
- [ ] Observability: metrics, tracing, structured logs
- [ ] Backup and disaster recovery for hosted repos
- [ ] Webhook system for external integrations (CI, chat, etc.)

### 10-R.e. Real-Time Server (Websocket)
- [ ] Websocket event stream — broadcast events matching client subscription filters
- [ ] Ghost text / semantic presence overlays — relay symbol-level change info between clients
- [ ] Semantic feed — stream meaningful changes ("Agent A added class CurrencyConverter, 3 methods") instead of raw commit notifications
- [ ] Exploration spectating — relay an agent's semantic trail in real time
- [ ] Continuous review — stream semantic changes as they land for incremental approval
- [ ] Conflict forecast — predict likely conflicts based on active explorations and warn before they happen
- [ ] Real-time task sync:
  - [ ] Broadcast task events (create, claim, done, fail) instantly to all connected clients
  - [ ] Task dependency resolution propagates live (task done → blocked tasks unblock across all clients)
  - [ ] Cross-repo task visibility — tasks in repo A can reference/block tasks in repo B
  - [ ] Dashboard view of all agent activity across connected repos (who's working on what, task throughput, queue depth)
- [ ] Agent directives — hot-swap redirection, pause/resume/abort messages from server to agent
- [ ] Streaming code review — `LiveReviewUpdate` messages as agent works
- [ ] Cross-repo coordination — detect cross-repo dependencies, alert on breaking changes

## 11. Intelligence Layer

- [x] Implement natural-language history queries (`fl query`)
- [x] Build TF-IDF search index for events (`fl intel rebuild/stats`)
- [x] Add AI-assisted intent extraction for commits/events
- [x] Add AI-assisted conflict resolution suggestions
- [x] Add confidence scoring and gate integration (`fl confidence`)

## 12. Security, Reliability, and Compliance

- [x] Threat model for agent and human actors (`docs/threat-model.md`)
- [x] Encrypt sensitive metadata at rest — AES-256-GCM + Argon2 key encryption (`fl key encrypt/decrypt/status`)
- [x] Audit trail hardening and tamper-evidence checks — BLAKE3 hash chain (schema v13), `fl audit` command
- [x] Offline mode behavior and reconnect reconciliation (`fl_core::reconcile` — divergence detection, auto-merge)
- [x] Backup and restore strategy for `.flock` data (`fl backup create/restore/verify`)
- [x] Disaster recovery playbooks (`docs/disaster-recovery.md`)

## 12.5. Agent Governance & Policy Engine (Client-Side)

Enforce quality, consistency, and safety at the point of creation. Policies are configured in `.flock/policies.toml` (versioned with code) and enforced locally before events reach the server. See `../roost-git/docs/flock-agent-governance.md` for full design.

### 12.5a. Policy Engine Core
- [x] Parse and validate `.flock/policies.toml` configuration
- [x] Policy evaluation pipeline — intercept file writes, checkpoints, promotions, merges, and task lifecycle events
- [x] Three-verdict model: Allow / Gate (pause for human review) / Block (reject with structured error)
- [x] Policy decision audit trail — log which policies were evaluated and their verdicts in the event log

### 12.5b. Scope Enforcement
- [x] Scope policy types and evaluation logic (fl-policy crate)
- [x] Three enforcement modes: Block (reject out-of-scope), Gate (allow with justification), Split (auto-extract to discovery task)
- [x] Wire scope enforcement into repo operations (requires task-level scope metadata)
- [x] Auto-create discovery tasks for out-of-scope observations in split mode
- [x] Configuration: `[scope] enforce = "split"`, `default_scope_mode = "path" | "semantic" | "module"`

### 12.5c. Change Budget Limits
- [x] Track files modified and lines changed per exploration and per task (requires file change metadata in checkpoint events)
- [x] Track semantic changes per exploration
- [x] Budget policy types and evaluation logic (fl-policy crate)
- [x] Enforce configurable budgets with pause_and_flag / block / warn actions
- [x] Configuration: `[budget] max_files_per_task`, `max_lines_per_task`, `max_semantic_changes_per_exploration`

### 12.5d. Commit Hygiene & Structured Intent Metadata
- [x] Extend checkpoint/commit events with structured intent fields: category (bugfix/feature/refactor/test/docs/style/chore), scope, confidence (high/medium/low), structured description
- [x] Enforce required intent metadata at checkpoint time (configurable per field)
- [x] Checkpoint frequency prompting — warn if agent works >N minutes without checkpointing
- [x] Configuration: `[commit_hygiene] require_category`, `require_scope`, `require_confidence`, `max_time_between_checkpoints`

### 12.5e. DRY / Duplication Prevention
- [x] Layer 1: Signature matching — compare new methods against existing symbol table by return type, parameter types, name similarity
- [x] Layer 2: Body analysis — AST structural comparison of method bodies
- [x] Layer 3: Pattern conformance — detect when new code should implement existing interfaces/patterns
- [x] Proactive reuse suggestions — surface relevant existing code when agent claims a task
- [x] Protected domains — stricter enforcement for sensitive areas (financial calculations, compliance)
- [x] Configuration: `[reuse] enforce`, `similarity_threshold`, `check_signatures`, `check_bodies`, `check_patterns`

### 12.5f. Architecture Rules
- [x] Parse `.flock/arch-rules.toml` for layer boundary, dependency direction, interface requirement, and namespace convention rules
- [x] Enforce architecture rules at file write time using AST and dependency graph
- [x] Configuration: `[architecture] rules = ".flock/arch-rules.toml"`, `enforce = "block" | "gate" | "warn"`

### 12.5g. Anti-Pattern Detection
- [x] Parse `.flock/anti-patterns.toml` for domain-specific AST query rules
- [x] Check file writes against anti-pattern rules (e.g., float for currency, hardcoded rates, audit bypass, PII exposure)
- [x] Structured explanations with fix suggestions on violation
- [x] Configuration: `[anti_patterns] rules = ".flock/anti-patterns.toml"`, `enforce = "block_with_explanation"`

### 12.5h. Dependency & Compatibility Checks
- [x] Parse `.flock/approved-deps.toml` for package allowlist with version ranges
- [x] License blocklist enforcement (e.g., GPL-3.0, AGPL-3.0)
- [x] Vulnerability scanning for new dependencies (CVE checks)
- [x] Run consumer test suites when shared libraries are modified
- [x] Configuration: `[dependencies] approved_packages`, `license_blocklist`, `vuln_check`

### 12.5i. Test Requirements
- [x] Enforce existing tests pass before exploration promotion
- [x] Require new test coverage for new behavior (new method → corresponding test)
- [x] Coverage threshold enforcement for modified modules
- [x] Configuration: `[tests] require_passing`, `require_new_coverage`, `min_coverage_percent`

### 12.5j. Rate Limits & Runaway Prevention
- [x] Track explorations per task, wall-clock time per exploration, undo operations per exploration, token budget per task
- [x] Enforce configurable limits with pause_and_escalate / warn / block actions
- [x] Escalation notifications with context (what the agent tried, links to exploration tree)
- [x] Configuration: `[rate_limits] max_explorations_per_task`, `max_time_per_exploration`, `max_undos_per_exploration`, `max_tokens_per_task`

### 12.5k. Regression Detection & Automatic Rollback
- [x] Post-merge monitoring — watch for test failures and benchmark regressions traceable to recently merged changes
- [x] Automatic rollback via append-only revert event when post-merge issues detected
- [x] Notify originating agent with context for re-exploration
- [x] Configuration: `[regression] monitor_after_merge`, `monitor_window`, `benchmark_threshold`; `[rollback] auto_rollback`, `rollback_on_test_failure`

### Implementation Priority

| Phase | Policies | Rationale | Depends On |
|-------|----------|-----------|------------|
| **1** | Scope, Budget, Rate Limits | Highest impact, lowest complexity | Core event pipeline only |
| **2** | Commit Hygiene, Test Requirements | Improves review quality, prevents broken promotions | Exploration and promotion flow |
| **3** | Architecture Rules, Anti-Patterns | Requires semantic layer (AST, dependency graph) | Semantic layer |
| **4** | DRY/Reuse Enforcement | Requires mature semantic index with body analysis | Semantic index, symbol table |
| **5** | Dependencies, Regression, Rollback | Requires integration with build/test systems | CI integration, test runner |

## 12.6. CLI Gaps for Server Integration

Features the Flock CLI needs to fully support the Roost server coordination model.

- [x] `fl who` command — show active actors and what they're working on (queries presence from server or local presence table)
- [x] Agent directive handling — background listener thread to receive and act on Pause/Resume/Redirect/Abort directives from server
- [x] Method-level presence — upgrade `PresenceUpdate` from file-level to symbol-level granularity (Phase 3 readiness)
- [x] `WorkspacePreview` streaming — optionally stream workspace diffs through WebSocket at configurable frequency for ghost text

## 13. QA and Performance

- [ ] Unit and integration test matrix across core crates
- [ ] Property tests for event replay invariants
- [ ] Fuzzing for parsers and merge engine
- [ ] Large-repo benchmarks and regression thresholds
- [x] Concurrency stress tests (10+ agents)
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

- [x] Add `fl commit` as a user-facing alias for `fl checkpoint` (keep checkpoint as the internal event type)
- [x] Update CLI help text to use "commit" instead of "checkpoint" in user-facing descriptions
- [x] Update README to use "commit" terminology
- [x] Update `fl log` output to say "commit" instead of "checkpoint"
- [x] Audit all CLI output strings and error messages for consistent terminology

## Scalability Hardening

Make the optimized storage paths the defaults so large repos and teams work out of the box.

- [x] Auto-migrate to segmented storage — detect when the event log or refs file crosses a size threshold and transparently upgrade to segmented event log / per-ref files without manual `fl migrate`
- [x] Add filesystem-level locking (`flock(2)` / lockfiles) around event log appends and ref writes to prevent corruption from concurrent writers
- [x] Auto-checkpoint materialized state — periodically snapshot replayed state (e.g. every 1,000 events) so event replay stays O(recent) instead of O(all)
- [ ] Streaming semantic analysis — add size limits / chunked parsing for files >1MB to avoid loading entire large files into memory for tree-sitter parsing
- [ ] Evaluate a server coordination component for team-scale use — file-based advisory locks have a ceiling; consider CRDTs or a lightweight Roost-mediated lock/presence protocol for 10+ concurrent writers

## Release Infrastructure

- [x] Set up cargo-dist for automated releases (v0.5.0)
- [ ] Create `Chris-Cullins/homebrew-flock` repo on GitHub (needed for Homebrew tap publishing)

## Bugs

- [x] `fl init --colocated` writes `mode = "git-compatible"` in `.flock/config.toml` instead of `"git-colocated"` — the `--colocated` flag is ignored when writing the config (verified: already working correctly)
- [x] `fl task show/claim/done/fail` require full UUIDs — short prefixes (e.g. first 8 chars) should work like `fl diff <checkpoint>` does with prefix matching
- [x] `fl diff` (non-semantic) shows "No changes" for unsupported file types — `file_summary_*` functions used `collect_source_files` which filtered by `supported_source()`, excluding files like `.razor`, `.txt` (GitHub #40)
- [x] `fl explore promote` creates checkpoint with potentially inaccessible snapshot — added `ensure_snapshot_available` after promote checkpoint creation to guarantee materialization (GitHub #39)
- [x] `fl push` in native mode pushes 0 blocks — `self.root.join("store/blocks")` should be `self.flock_dir().join("store/blocks")` (wrong path, blocks never found)
- [x] `fl push` block hash construction prepends fanout prefix to full hash — `format!("{prefix}{name}")` produced `3e3e82791e...` instead of `3e82791e...` (blocks uploaded with wrong hashes)
- [x] `fl push` block file read path splits hash incorrectly — used `h[..2]/h[2..]` but files are stored as `h[..2]/h` (full hash as filename)
- [x] `fl push` in native mode doesn't upload snapshot index — added fallback to upload JSON index when snapshot directory doesn't exist
- [x] `fl pull`/`fl clone` doesn't handle native mode snapshots — added JSON index detection and block download on pull side
- [x] `fl clone` always initializes as git-compatible — added Init event mode detection to switch cloned repo to native mode
