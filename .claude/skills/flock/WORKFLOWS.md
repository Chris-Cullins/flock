---
name: flock-workflows
description: >
  Exploration lifecycle, session tracking, quality gates, and conflict
  resolution workflows for Flock version control.
---

# Flock Workflows

## Explorations (Branch-Like Workflows)

Explorations are Flock's alternative to branches. They isolate work and can be
promoted to mainline or abandoned without losing history.

### Lifecycle

```
start → commit → (review) → promote OR abandon
```

```bash
# 1. Start an exploration
fl explore start --title "add-auth-module"

# 2. Do work and commit as normal
fl commit -m "implement JWT validation"
fl commit -m "add login endpoint"

# 3. Review the exploration's changes
fl review <exploration-id>          # Summary view
fl review <exploration-id> --full   # Full line-level diffs
fl review <exploration-id> --expand 2  # Expand change #2

# 4a. Promote to mainline (merge)
fl explore promote <exploration-id>

# 4b. Or abandon if the approach didn't work
fl explore abandon <exploration-id>
```

### Comparing explorations

```bash
fl explore compare <left-id> <right-id>    # Compare two explorations
fl explore compare <id> mainline           # Compare against mainline
fl explore compare <left> <right> --json   # Machine-readable
```

### Exploration tree

```bash
fl explore tree    # ASCII tree showing exploration hierarchy
```

### Pruning old explorations

```bash
fl explore prune                    # Remove abandoned explorations
fl explore prune --older-than 30d   # Only those older than 30 days
```

## Session Tracking

Sessions track an agent's work from start to finish, linking explorations,
decisions, and resource usage for full provenance.

```bash
# Start a session (returns session ID)
fl session start --task "implement feature X" --agent "claude"

# Link an exploration to the session
fl session link <session-id> <exploration-id>

# Record a decision about an exploration
fl session decision <session-id> <exploration-id> \
  --action kept --reason "tests pass" --confidence 0.95

# Record resource usage
fl session usage <session-id> --tokens 5000 --runtime-ms 12000

# Complete the session
fl session complete <session-id> --result "feature implemented and tested"

# Or mark it as failed
fl session fail <session-id> --reason "blocked by missing API key"
```

### Querying sessions

```bash
fl session list                  # All sessions
fl session list --active         # Only active sessions
fl session show <id>             # Full session detail
fl session show <id> --json      # Machine-readable
fl session provenance <expl-id>  # Full chain for an exploration
fl session replay <id>           # Replay session events
```

### Confidence scoring

```bash
fl confidence             # Current session confidence score
fl confidence --verbose   # Breakdown by factor
fl confidence --json      # Machine-readable
```

## Quality Gates

Gates enforce human-in-the-loop review before certain changes land.

### Gate conditions

| Condition | Triggers when... |
|-----------|-----------------|
| `file-touched` | A file matching `--pattern` is modified |
| `symbol-modified` | A symbol matching `--pattern` is changed |
| `impact-exceeds` | Impact score exceeds `--threshold` |
| `security-sensitive` | Security-related file is touched |
| `agent-confidence-low` | Agent's confidence is below threshold |

### Workflow

```bash
# Create a gate
fl gate create --condition file-touched --pattern "src/auth/**" --policy block

# Check if any gates fire for a path
fl gate check src/auth/login.rs

# Approve or reject
fl gate approve <gate-id> --reason "reviewed, looks good"
fl gate reject <gate-id> --reason "missing error handling"

# Manage gates
fl gate list [--json]
fl gate delete <gate-id>
```

## Conflict Detection & Resolution

Flock detects conflicts at the semantic level (symbol vs symbol), not just
line vs line.

### Workflow

```bash
# 1. Detect conflicts for a workspace
fl conflict detect --workspace my-workspace

# 2. Get a suggested resolution
fl conflict suggest <conflict-id>

# 3. Apply your fix, then mark resolved
fl conflict resolve <conflict-id> --resolution "merged both changes manually"

# 4. Verify the resolution
fl conflict verify <conflict-id> --passed true

# 5. Record the resolution for future reference
fl conflict record <conflict-id> --reason "combined both approaches"

# List conflicts by status
fl conflict list
fl conflict list --status detected
fl conflict list --status resolved --json
```

### Conflict statuses

`detected` → `classified` → `suggested` → `resolved` → `verified` → `recorded`

## Merge

Preview a three-way merge without committing:

```bash
fl merge --semantic base.rs left.rs right.rs
fl merge --dry-run base.rs left.rs right.rs    # Preview only
fl merge --semantic base.rs left.rs right.rs --json
```

## Workspace Rebase

Keep workspaces up to date with the latest mainline:

```bash
fl rebase --workspace my-workspace   # Rebase one workspace
fl auto-rebase                       # Rebase all auto-rebase workspaces
```
