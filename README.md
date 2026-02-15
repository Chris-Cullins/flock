# Flock (MVP scaffold)

Initial Rust implementation of `fl` with:

- `.flock` repository initialization
- append-only event log (`events.jsonl`)
- checkpoint snapshots
- checkpoint metadata includes deterministic snapshot Merkle roots
- semantic diff for JavaScript/TypeScript (`.js`, `.jsx`, `.ts`, `.tsx`)
- exploration lifecycle commands (`start/list/promote/abandon`)
- undo variants on the event timeline
- git bridge commands (`commit/push/pull`) with event logging
- git import/export commands for checkpoint history translation
- optional git-colocated mode (`--colocated`) with checkpoint-to-git commit mapping
- shadow mode safety checks for git bridge operations (`push/pull/export`) with recovery guidance
- repository refs abstraction (`branch`, `tag`, `workspace`)
- colocated ref mirroring into git refs:
  - branches: `refs/flock/branches/<name>`
  - tags: `refs/flock/tags/<name>`
  - workspaces: `refs/flock/workspaces/<name>`
- colocated push/pull bridge sync for `refs/flock/*` to/from git remotes

Workspace crates:

- `fl-core`: repo orchestration and public API used by CLI
- `fl-storage`: event schema, metadata layout constants, event log IO
- `fl-semantic`: TS/JS semantic analyzer and diff engine
- `fl-workflow`: exploration/undo workflow domain types + timeline logic
- `fl-bridge-git`: git command execution utilities for bridge operations
- `fl-collab`: collaboration domain contracts (presence/locks) for upcoming phases

## Build

```bash
cargo build
```

## Run

```bash
# initialize metadata in current project
cargo run -p fl-cli -- init

# initialize colocated mode (.git + .flock sidecar)
cargo run -p fl-cli -- init --colocated

# create a checkpoint snapshot
cargo run -p fl-cli -- checkpoint -m "base"

# show semantic changes vs last checkpoint
cargo run -p fl-cli -- diff --semantic
cargo run -p fl-cli -- diff --semantic --json

# inspect event log
cargo run -p fl-cli -- log

# verify storage integrity
cargo run -p fl-cli -- fsck

# exploration commands
cargo run -p fl-cli -- explore start --title \"new-parser\"
cargo run -p fl-cli -- explore list
cargo run -p fl-cli -- explore promote <exploration-uuid>
cargo run -p fl-cli -- explore abandon <exploration-uuid>

# undo commands
cargo run -p fl-cli -- undo
cargo run -p fl-cli -- undo --n 2
cargo run -p fl-cli -- undo --to <event-id-or-prefix>
cargo run -p fl-cli -- undo --since 5m
cargo run -p fl-cli -- undo --file src/app.ts

# git bridge commands
cargo run -p fl-cli -- git status
cargo run -p fl-cli -- git commit -m \"checkpoint from fl\"
cargo run -p fl-cli -- git push
cargo run -p fl-cli -- git pull
cargo run -p fl-cli -- git import
cargo run -p fl-cli -- git export

# refs commands
cargo run -p fl-cli -- refs list
cargo run -p fl-cli -- refs set branch main <event-id-or-prefix>
cargo run -p fl-cli -- refs set tag v1 <checkpoint-event-id-or-prefix>
cargo run -p fl-cli -- refs set workspace agent/a <event-id-or-prefix> --auto-rebase
cargo run -p fl-cli -- refs delete workspace agent/a
```

## Notes

- First semantic analyzer is JS/TS AST (tree-sitter), as requested.
- The analyzer detects functions (including arrow/function expressions assigned to vars), classes, constructors, methods, class fields, interfaces, type aliases, enums, and export/re-export statements.
- This is an MVP foundation for layering in explorations, sessions, and native storage later.

## Shadow Mode Safety + Recovery

- Use `fl git status` to run shadow-mode health checks in colocated repositories.
- `fl git push`, `fl git pull`, and `fl git export` now enforce preflight checks:
  - `.flock/` must be excluded from git tracking.
  - the working tree must be clean (for pull/import/export).
  - `HEAD` must align with `refs/flock/branches/main` when checkpoints exist.
- If `HEAD` drifts from Flock refs (for example after manual `git commit`), run:
  - `fl git import` to map git commits into checkpoint lineage.
  - `fl checkpoint -m "sync"` if you need to re-establish/refine forward mapping.
