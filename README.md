# Flock (MVP scaffold)

Initial Rust implementation of `fl` with:

- `.flock` repository initialization
- append-only event log (`events.jsonl`)
- checkpoint snapshots
- checkpoint metadata includes deterministic snapshot Merkle roots
- semantic diff for JavaScript/TypeScript (`.js`, `.jsx`, `.ts`, `.tsx`)
- exploration lifecycle commands (`start/list/promote/abandon`)
- undo variants on the event timeline
- git bridge command stubs (`commit/push/pull`)
- repository refs abstraction (`branch`, `tag`, `workspace`)

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

# git bridge stubs (pass-through to git + event logging)
cargo run -p fl-cli -- git commit -m \"checkpoint from fl\"
cargo run -p fl-cli -- git push
cargo run -p fl-cli -- git pull

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
