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

## 2. Git/JJ Compatibility Layer

- [x] Implement git-colocated mode (`.git` + `.flock` sidecar)
- [x] Map checkpoint operations to git commits
- [x] Map Flock refs to git refs/bookmarks strategy
- [x] Implement push/pull bridge to git remotes
- [x] Implement git import and export commands
- [x] Implement shadow mode safety checks and recovery docs
- [x] Define jj import design and metadata mapping

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
- [ ] Add TUI views for exploration trees and task graph
- [ ] Add shell completion and command help polish

## 5. Explorations and Workspaces

- [ ] Implement exploration model with lifecycle states
- [ ] Add exploration commands:
  - [x] `fl explore start`
  - [x] `fl explore list`
  - [ ] `fl explore compare`
  - [x] `fl explore promote`
  - [x] `fl explore abandon`
- [ ] Add TTL and background pruning for abandoned/expired explorations
- [ ] Add workspace isolation model (base snapshot + overlay events)
- [ ] Add workspace resource limits and policy enforcement
- [ ] Add checkpoint/rollback shortcuts for agents

## 6. Agent Sessions and Provenance

- [ ] Implement session entity and linkage to explorations/tasks
- [ ] Track decisions (kept/discarded + reason + confidence)
- [ ] Add resource usage accounting (tokens, runtime, external API calls)
- [ ] Implement provenance query commands
- [ ] Implement session replay command

## 7. Work Queue (Built-in Task Graph)

- [ ] Implement task schema and DAG dependencies
- [ ] Add task lifecycle commands:
  - [ ] `fl task create`
  - [ ] `fl task list`
  - [ ] `fl task claim`
  - [ ] `fl task done`
  - [ ] `fl task show`
  - [ ] `fl task graph`
- [ ] Implement `fl ready` priority + dependency-unblocked selection
- [ ] Add `--json` output for agent consumption
- [ ] Link tasks to events/checkpoints/explorations
- [ ] Add discovered-from relationships and auto-linking
- [ ] Implement task compaction for old completed tasks

## 8. Collaboration Layer

- [ ] Implement presence model and heartbeat protocol
- [ ] Add advisory lock API with TTL:
  - [ ] `fl lock acquire`
  - [ ] `fl lock list`
  - [ ] `fl lock release`
- [ ] Implement change subscriptions and notification filters
- [ ] Implement human-in-the-loop gates and policies
- [ ] Implement continuous auto-rebase for active workspaces
- [ ] Implement conflict resolution workflow:
  - [ ] detect
  - [ ] classify
  - [ ] suggest
  - [ ] resolve
  - [ ] verify
  - [ ] record

## 9. Native Storage Engine (Phase 2 Backend)

- [ ] Design native `.flock/store` layout
- [ ] Implement block-level content store with BLAKE3 keys
- [ ] Implement file index mapping `(path, event) -> block refs`
- [ ] Evaluate block strategy:
  - [ ] fixed-size chunking
  - [ ] content-defined chunking
  - [ ] language-aware chunking
- [ ] Implement native copy-on-write snapshots
- [ ] Implement sub-file undo (true file-scoped rewind)
- [ ] Implement migration command `fl migrate --native`
- [ ] Add performance benchmarks against colocated mode

## 10. Scale and Server Components

- [ ] Define server architecture for enterprise features
- [ ] Add authn/authz model and access control
- [ ] Add real-time presence service
- [ ] Add tiered storage policies (hot/warm/cool)
- [ ] Add background jobs (compaction, pruning, indexing)
- [ ] Add replication/sync protocol for events + blobs
- [ ] Add observability (metrics, tracing, logs)

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

## Bugs

- [ ] `fl init --colocated` writes `mode = "git-compatible"` in `.flock/config.toml` instead of `"git-colocated"` — the `--colocated` flag is ignored when writing the config
