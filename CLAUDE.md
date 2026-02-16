# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Flock

Flock is a next-generation version control system written in Rust, designed for AI agents as first-class participants. It preserves git's foundations (content-addressable storage, Merkle trees) while adding semantic-aware merging, exploration workflows, and multi-agent coordination. The CLI command is `fl`.

## Build & Test Commands

```bash
cargo build                          # Build all crates
cargo test                           # Run all tests
cargo test -p fl-semantic            # Test a specific crate
cargo test process_boundary --test   # Run integration test by name
cargo run -p fl-cli -- <command>     # Run the CLI (e.g., init, checkpoint, diff)
```

## Architecture

Cargo workspace with 7 crates, layered from foundation to CLI:

```
fl-cli          CLI (clap derive-based parsing, delegates to fl-core)
fl-core         Repo orchestration & public API (the Repo struct)
fl-storage      Append-only event log (JSONL), refs store (JSON), .flock layout
fl-semantic     Language analyzers, semantic diff/merge engine, plugin registry
fl-workflow     Exploration lifecycle, undo timeline, event replay
fl-bridge-git   Git colocated mode, import/export bridge
fl-collab       Collaboration contracts (stub for future phases)
```

**Dependency flow**: `fl-cli` → `fl-core` → `fl-storage`, `fl-semantic`, `fl-workflow`, `fl-bridge-git`, `fl-collab`

### Key Types

- **`fl_core::Repo`** — Central entry point. Discovered via upward `.flock` directory walk. All CLI commands delegate here.
- **`fl_storage::Event`** — Immutable, append-only events with `EventKind` variants: `Checkpoint`, `Exploration`, `Undo`, `GitBridge`. Events form causal chains via `parent_id`.
- **`fl_storage::RepoRef`** — Refs (Branch/Tag/Workspace) point to event IDs, not snapshots.
- **`fl_semantic::SemanticAnalyzerPlugin`** — Trait for language analyzers (`diff()`, `merge()`, `supports_path()`). Registry-based; latest registration wins.
- **`fl_semantic::SemanticChange`** — Change with kind (Added/Removed/Modified/Renamed/Moved/StyleOnly), risk (Low/Medium/High), and signature compatibility.
- **`fl_semantic::SemanticMergeResult`** — Merge output with conflict classifications: DivergentEdit, DeleteVsEdit, ConcurrentAddition, KindMismatch, TextFallback.
- **`fl_workflow::ReplayedState`** — Deterministic state reconstructed from replaying the event log.

### Core Design Patterns

- **Event sourcing**: All state changes are append-only events. State is reconstructed by replaying events. Undo is pointer movement, not file rewriting.
- **Semantic-first merging**: AST-level understanding via tree-sitter (TS/JS/Python/Go/Rust/C#), falling back to text diff for unsupported languages.
- **Plugin analyzers**: `SemanticAnalyzerPlugin` trait with an `AnalyzerRegistry`. Process analyzers run out-of-process via JSON-over-stdio protocol with auto-restart.
- **Layered degradation**: Semantic layer is optional. Process analyzers auto-restart on failure. Unsupported languages use `FallbackTextAnalyzer`.
- **Two modes**: `GitCompatible` (`.flock` only) and `GitColocated` (`.git` + `.flock` sidecar with mirror refs under `refs/flock/*`).

### .flock Directory Layout

```
.flock/
├── config.toml          # mode, semantic_default, analyzers list
├── event-log/events.jsonl
├── refs/refs.json
├── keys/ed25519.sk
└── snapshots/<uuid>/    # Checkpoint state snapshots
```

## Key Design Documents

- `flock-architecture.md` — Comprehensive design doc (65KB+): git model primer, Flock architecture, jj lessons
- `TODO.md` — Build roadmap (sections 0-14), tracks what's done vs planned
- `docs/jj-import-design.md` — Jujutsu import/export metadata mapping spec

## Bug Tracking

Known bugs are tracked in the `## Bugs` section at the bottom of `TODO.md`. Add new entries there whenever a bug is discovered during development or dogfooding. Use the same `- [ ]` checkbox format as the rest of the TODO file.

## Codebase Notes

- Rust edition 2024, synchronous (no async/tokio)
- No external database — all state in JSONL/JSON files under `.flock/`
- Tree-sitter bindings for JS, JSX, TS, TSX, Python, Go, Rust, C#; other languages use text fallback
- Unit tests are inline (`#[cfg(test)]` modules); integration tests in `crates/*/tests/`
- `fl-semantic/src/lib.rs` contains ~2,400 lines including ~2,100 lines of tests covering symbol extraction, conflict classification, and signature compatibility
- Events are signed with ed25519 (optional), include Merkle snapshot roots for integrity
- Schema versions: event=6, refs=1
