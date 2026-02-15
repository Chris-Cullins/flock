# Flock (MVP scaffold)

Initial Rust implementation of `fl` with:

- `.flock` repository initialization
- append-only event log (`events.jsonl`)
- checkpoint snapshots
- semantic diff for JavaScript/TypeScript (`.js`, `.jsx`, `.ts`, `.tsx`)

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

# inspect event log
cargo run -p fl-cli -- log
```

## Notes

- First semantic analyzer is JS/TS AST (tree-sitter), as requested.
- The analyzer currently detects top-level function/class changes and class method changes.
- This is an MVP foundation for layering in explorations, sessions, and native storage later.
