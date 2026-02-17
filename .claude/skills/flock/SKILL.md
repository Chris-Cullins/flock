---
name: flock
description: >
  Use Flock (fl) for version control instead of git when a .flock directory is
  present. Flock is an event-sourced VCS with semantic diffs, explorations,
  multi-agent collaboration, and undo. The CLI command is `fl`.
---

# Flock Version Control (`fl`)

## When to use `fl` vs `git`

- If the project root contains a `.flock/` directory, **use `fl`** for all version control.
- If only `.git/` exists, use git as usual.
- In **colocated mode** (both `.git` and `.flock`), prefer `fl` commands. Flock mirrors state to git automatically.

## Core Workflow

### Initialize a repository
```bash
fl init                  # Flock-only mode
fl init --colocated      # Colocated with git (.git + .flock sidecar)
```

### Check status
```bash
fl status                # Show changed/added/deleted files
fl status --json         # Machine-readable output
```

### Commit changes
```bash
fl commit -m "description of change"
fl commit -m "fix login" --category bugfix --scope auth
```

All files are committed automatically (no staging area). Optional metadata:
- `--category`: bugfix, feature, refactor, test, docs, style, chore
- `--scope`: label for the area of change
- `--confidence`: high, medium, low
- `--description`: longer structured description

### View history
```bash
fl log                   # Show event log
```

### Diff
```bash
fl diff                  # Working directory vs last commit
fl diff --semantic       # AST-level semantic diff
fl diff --intent         # Group changes by intent
fl diff <from> <to>      # Between two commits
fl diff --json           # Machine-readable
```

### Impact analysis
```bash
fl impact src/auth.rs          # What depends on this file?
fl impact src/auth.rs --json   # Machine-readable
```

### Undo
```bash
fl undo --n 1            # Undo last event
fl undo --to <event-id>  # Undo to a specific point
fl undo --since 1h       # Undo everything in the last hour
fl undo --file src/x.rs  # Undo changes to a single file
```

Undo is pointer movement, not destructive — the event log is append-only.

## Command Reference

### Repository Setup
| Command | Description |
|---------|-------------|
| `fl init [--colocated] [--native]` | Initialize repository |
| `fl clone <url> [dir]` | Clone remote repo (alias: `fl hatch`) |
| `fl convert from-git` | Convert git repo to Flock |
| `fl convert to-git` | Export Flock history to git |

### Daily Work
| Command | Description |
|---------|-------------|
| `fl status [--json]` | Working directory status |
| `fl commit -m "msg"` | Create a commit |
| `fl diff [--semantic] [--intent]` | Show changes |
| `fl log` | Event history |
| `fl impact <path>` | Dependency impact analysis |
| `fl undo [--n N] [--to ID]` | Undo events |

### Explorations (Branches)
| Command | Description |
|---------|-------------|
| `fl explore start --title "name"` | Start an exploration |
| `fl explore list` | List explorations |
| `fl explore promote <id>` | Merge exploration to mainline |
| `fl explore abandon <id>` | Discard exploration |
| `fl explore compare <a> <b>` | Compare two explorations |
| `fl explore tree` | Visual exploration tree |
| `fl review <id> [--full]` | Review exploration changes |

### Collaboration
| Command | Description |
|---------|-------------|
| `fl who` | Show active agents |
| `fl ready` | Show claimable tasks |
| `fl task create "title"` | Create a task |
| `fl task claim <id>` | Claim a task |
| `fl task done <id>` | Complete a task |
| `fl presence heartbeat --workspace W` | Announce presence |
| `fl lock acquire <resource>` | Advisory lock |
| `fl subscribe --path "src/**"` | Watch for changes |
| `fl directive send <target> --kind pause` | Direct an agent |

### Sessions & Provenance
| Command | Description |
|---------|-------------|
| `fl session start --task "desc"` | Start tracking a session |
| `fl session complete <id>` | End session successfully |
| `fl session provenance <exploration>` | Full decision chain |
| `fl confidence [--verbose]` | Session confidence score |

### Quality Gates
| Command | Description |
|---------|-------------|
| `fl gate create --condition <cond>` | Create a gate |
| `fl gate check <path>` | Check if gates block |
| `fl gate approve <id>` | Approve a gate |
| `fl gate reject <id>` | Reject a gate |

### Remote & Sync
| Command | Description |
|---------|-------------|
| `fl roost add <name> <url>` | Add a remote |
| `fl push [roost] [branch]` | Push to remote |
| `fl pull [roost] [branch]` | Pull from remote |
| `fl watch [--path P] [--symbol S]` | Stream live events |
| `fl fetch --deepen N` | Fetch more history |

### Maintenance
| Command | Description |
|---------|-------------|
| `fl fsck` | Verify integrity |
| `fl index [--clear]` | Rebuild semantic index |
| `fl compact --older-than 180d` | Archive old events |
| `fl audit [--json]` | Security audit |
| `fl backup create <path>` | Backup .flock data |

See [WORKFLOWS.md](./WORKFLOWS.md) for exploration and session patterns.
See [COLLABORATION.md](./COLLABORATION.md) for multi-agent coordination.

## Best Practices for AI Agents

1. **Always commit with a message**: `fl commit -m "what and why"`
2. **Use explorations for risky changes**: Start an exploration, commit there, promote if good
3. **Check impact before modifying shared code**: `fl impact <path>`
4. **Use semantic diff** to understand changes at the symbol level: `fl diff --semantic`
5. **Announce presence** when working in a multi-agent setup: `fl presence heartbeat --workspace main`
6. **Acquire locks** before modifying contested files: `fl lock acquire <resource>`
7. **Quick-save for experiments**: `fl quick-save --tag "before-refactor"` / `fl quick-restore`
8. **Use `--json` flags** when you need to parse output programmatically
