# Lazy/Partial Clone Design

Flock's lazy/partial clone allows you to clone only the parts of a repository you need, fetching the rest on demand. Unlike git's partial clone (which was bolted onto a system designed around monolithic packfiles), Flock's architecture — separate event log, snapshot indices, and content-addressed block store — makes partial materialization natural.

## Design Principles

1. **Events and indices are cheap, blocks are expensive.** Always pull the full event history and snapshot indices (small JSON metadata). Be selective about which content blocks to download.
2. **Absent blocks are implicit.** If a block hash in a snapshot index isn't in the local content store, it simply hasn't been fetched yet. No separate "promisor" bookkeeping needed — the content store's `has(hash)` check is O(1).
3. **Build-system awareness over AST tracing.** To determine what files are needed to build a target, parse build manifests (Cargo.toml, package.json, go.mod) rather than walking AST import graphs. Manifests are the authoritative dependency graph and don't miss build config, codegen, or asset files.
4. **Graceful degradation.** If a file is accessed but its blocks aren't local, Flock can either fault them in from the remote or produce a clear error with a fetch command to run.

## Three Axes of "Partial"

### 1. Shallow (time axis)

Only fetch recent history. Useful for CI, new contributors, or repos with deep history.

```bash
fl clone --depth 50 flock://host/owner/repo
fl fetch --deepen 100   # extend history backward later
```

**How it works:**
- `EventPullRequest` gains a `depth: Option<usize>` field
- Server sends only the last N checkpoint events (plus non-checkpoint events between them)
- The oldest checkpoint becomes a "graft point" — its parent link is null locally
- Deepening sends a follow-up pull starting from the graft point

### 2. Sparse (space axis)

Only download blocks for files matching a set of path patterns. All events and indices are still pulled (they're tiny).

```bash
fl clone --sparse "src/frontend/**" flock://host/owner/repo
fl sparse add "src/api/**"
fl sparse remove "src/frontend/legacy/**"
fl sparse list
```

**How it works:**
- `EventPullRequest` gains `sparse_paths: Option<Vec<String>>` field
- Server filters `referenced_block_hashes` in `EventPullResponse` to only include blocks for files matching the patterns
- Client stores the sparse spec in `.flock/config.toml`
- Working directory only materializes matched files
- `fl pull` respects the sparse set — new events arrive but only matching blocks are fetched

### 3. Focus (build-target axis)

Automatically determine the file set needed to build a specific target by parsing build system manifests.

```bash
# Cargo workspace: clone only fl-cli and its workspace dependencies
fl clone --focus fl-cli flock://host/owner/repo

# Node.js monorepo: clone only the api package
fl clone --focus packages/api flock://host/owner/repo
```

**How it works:**
1. Pull all events, refs, and snapshot indices
2. Materialize build manifests first (Cargo.toml, package.json, go.mod, etc.)
3. Parse the manifest to compute the dependency closure of the specified target
4. Convert the package/crate list into a set of path patterns
5. Fetch blocks only for those paths (same mechanism as sparse checkout)
6. Everything else is a lazy stub — fetched on demand if accessed

**Supported build systems (planned):**

| Build System | Manifest | Dependency Discovery |
|---|---|---|
| Cargo (Rust) | `Cargo.toml` workspace members + dependencies | Workspace `[dependencies]` with `path = "..."` |
| npm/yarn/pnpm | `package.json` workspaces | `"dependencies"` / `"devDependencies"` with `"workspace:*"` or `"file:..."` |
| Go modules | `go.mod` + `go.work` | `require` directives with local `replace` |
| Python (uv/pip) | `pyproject.toml` / `setup.cfg` | `[project.dependencies]` with path refs |

### 4. Lazy (materialization axis)

Download zero blocks at clone time. Blocks are fetched on demand when files are accessed.

```bash
fl clone --lazy flock://host/owner/repo
```

**How it works:**
- Clone downloads events + snapshot indices only (fast, small)
- When `fl checkout`, `fl diff`, or a file read occurs, the snapshot store checks if the needed blocks exist locally
- Missing blocks trigger a `BlockFaultHandler` that fetches them from the configured remote
- Local block store acts as a growing cache
- Combine with `fl pin` for offline access guarantees

## Composability

All axes compose independently:

```bash
# Shallow + sparse
fl clone --depth 20 --sparse "src/frontend/**" flock://host/owner/repo

# Focus + lazy (fetch target's blocks, everything else on demand)
fl clone --focus fl-cli --lazy flock://host/owner/repo

# Shallow + focus
fl clone --depth 50 --focus packages/api flock://host/owner/repo
```

## Offline Support: `fl pin`

Lazy and sparse clones may not have all blocks locally. The `pin` command eagerly fetches blocks to guarantee offline access:

```bash
fl pin "src/**"           # Fetch all blocks for src/
fl pin --all              # Fetch everything (convert to full clone)
fl pin --list             # Show pinned patterns
fl pin --unpin "docs/**"  # Allow docs blocks to be evicted
```

## Build-Integrated Recovery

When a build fails because files are missing from a sparse/lazy clone, Flock can help:

```bash
# Detect missing files from build output and fetch them
fl fetch --resolve-missing

# Or wrap the build command for automatic retry
fl build cargo build
# → Detected 3 missing files referenced by build errors
# → Fetching blocks for crates/fl-semantic/...
# → Retrying build...
```

## Protocol Changes

### EventPullRequest additions

```rust
pub struct EventPullRequest {
    pub last_known_event: Option<Uuid>,
    pub branch: Option<String>,
    // New fields:
    pub depth: Option<usize>,           // shallow: max checkpoint count
    pub sparse_paths: Option<Vec<String>>, // sparse: only blocks for these globs
}
```

### New: BlockFaultRequest

```rust
/// Request blocks that weren't fetched during initial clone.
pub struct BlockFaultRequest {
    pub block_hashes: Vec<String>,
}

pub struct BlockFaultResponse {
    pub blocks: Vec<BlockPayload>,
}
```

### .flock/config.toml additions

```toml
[clone]
depth = 50                    # graft depth (omit for full history)
lazy = true                   # blocks fetched on demand
focus_target = "fl-cli"       # build target for focus clone

[sparse]
include = ["src/frontend/**", "package.json", "tsconfig.json"]
# exclude = ["src/frontend/legacy/**"]  # optional exclusions
```

## Implementation Phases

1. **`fl clone` command** — Convenience wrapper for `init` + `remote add` + `pull`. Foundation for all other flags.
2. **Shallow clone** — `--depth N`, graft markers, `fl fetch --deepen`. Minimal protocol change, high value.
3. **Sparse checkout** — `--sparse`, `fl sparse add/remove/list`. Biggest bandwidth savings for large repos.
4. **Focus clone** — `--focus <target>`, manifest parsing. Build-system-aware sparse sets.
5. **Lazy block fetching** — `--lazy`, `BlockFaultHandler`. Most powerful but most complex.
6. **`fl pin`** — Offline guarantees for lazy/sparse clones.
7. **Build-integrated recovery** — `fl fetch --resolve-missing`, `fl build` wrapper.

## Why Not Use AST Import Tracing?

We considered using Flock's semantic layer to trace import graphs from an entrypoint to determine the minimal file set. While the AST can identify `import`/`use`/`require` statements, it misses:

- Build system configuration (Cargo.toml, Makefile, tsconfig.json)
- Build-time code generation (build.rs, protobuf .proto files, Webpack loaders)
- Assets referenced by string (`include_str!()`, `fs.readFileSync()`)
- Dynamic imports (`require(variable)`, `importlib.import_module()`)
- Test fixtures and data files

The failure mode is bad: the build fails, you don't know why, you have to fetch more and retry. Build manifests are the authoritative source and don't have these gaps.

The AST layer remains useful as a *refinement* (narrowing within a large package) and as a *recovery* mechanism (analyzing build errors to suggest what to fetch), but not as the primary dependency discovery method.
