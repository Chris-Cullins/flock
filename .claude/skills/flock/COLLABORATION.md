---
name: flock-collaboration
description: >
  Multi-agent collaboration features: presence, locks, subscriptions,
  directives, and task management in Flock.
---

# Flock Collaboration

## Presence

Presence tracking lets agents announce where they are working so others can
avoid conflicts.

```bash
# Send a heartbeat (repeat periodically)
fl presence heartbeat --workspace main \
  --file src/auth.rs --symbol "fn login" \
  --intent "refactoring auth flow" --ttl 300

# Announce departure
fl presence depart --workspace main

# See who's active
fl presence list
fl who              # Shortcut: active agents and what they're working on
fl who --json       # Machine-readable
```

## Advisory Locking

Locks prevent concurrent edits to contested resources. They are advisory —
agents are expected to check and respect them.

```bash
# Acquire a lock (returns lock ID)
fl lock acquire src/config.toml --ttl 600

# List active locks
fl lock list

# Release when done
fl lock release <lock-id>
```

Locks expire after their TTL (in seconds). Always release locks explicitly
when finished.

## Subscriptions

Subscribe to changes on paths, symbols, or modules to get notified when
relevant files change.

```bash
# Subscribe to path changes
fl subscribe --path "src/auth/**" --path "src/config.toml"

# Subscribe to symbol changes
fl subscribe --symbol "fn process_payment"

# Subscribe to module changes
fl subscribe --module "auth" --module "billing"

# Custom notification method
fl subscribe --path "src/**" --notify webhook:https://example.com/hook

# List active subscriptions
fl subscriptions [--json]

# Cancel a subscription
fl unsubscribe <subscription-id>
```

## Directives

Directives let humans or orchestrator agents control other agents in
real time.

### Directive kinds

| Kind | Effect |
|------|--------|
| `pause` | Agent should pause current work |
| `resume` | Agent should resume paused work |
| `redirect` | Agent should switch to `--new-task` |
| `abort` | Agent should stop immediately |

```bash
# Send a directive
fl directive send agent-1 --kind pause --reason "waiting for review"
fl directive send agent-1 --kind redirect --new-task "fix bug #42"
fl directive send agent-1 --kind resume
fl directive send agent-1 --kind abort --reason "approach abandoned"

# List directives
fl directive list [--actor agent-1] [--json]

# Listen for directives targeting this agent (blocks)
fl directive listen
```

## Task Management

Flock has a built-in task graph with dependencies, claims, and lifecycle
tracking.

### Task lifecycle

```
create → claim → done/fail
```

```bash
# Create a task
fl task create "implement login endpoint" \
  --description "Add POST /login with JWT" \
  --scope src/auth --scope src/routes

# Create with dependencies
fl task create "write auth tests" --depends-on <task-id>

# Create discovered from an exploration
fl task create "fix edge case" --discovered-from <exploration-id>

# List tasks
fl task list              # Open and claimed tasks
fl task list --all        # Include completed
fl task list --json       # Machine-readable
fl task list --live       # Stream updates via WebSocket

# See what's ready to work on
fl ready                  # Tasks with no unresolved dependencies
fl ready --json
fl ready --live           # Stream as tasks become ready

# Show task details
fl task show <id> [--json]

# Claim a task
fl task claim <id> --assignee "claude-agent-1"

# Complete a task (auto-commits by default)
fl task done <id> --result "endpoint implemented with tests"
fl task done <id> --no-checkpoint   # Skip auto-commit

# Mark a task as failed
fl task fail <id> --reason "blocked by external API"

# Link events to a task after the fact
fl task link <task-id> <event-id-1> <event-id-2>

# Visualize the dependency graph
fl task graph [--json]

# Clean up old completed tasks
fl task compact --older-than 7d
```

## Quick Save / Restore

Lightweight save points for experiments — no exploration overhead.

```bash
fl quick-save --tag "before-refactor"
# ... experiment ...
fl quick-restore                        # Revert to the save point
```

## Putting It All Together

A typical multi-agent workflow:

```bash
# Agent announces presence
fl presence heartbeat --workspace main --intent "working on auth"

# Agent checks for ready tasks
fl ready

# Agent claims a task
fl task claim <id> --assignee "agent-1"

# Agent locks contested files
fl lock acquire src/auth.rs --ttl 600

# Agent starts an exploration
fl explore start --title "auth-refactor"

# Agent does work and commits
fl commit -m "refactor auth module"

# Agent checks for conflicts
fl conflict detect --workspace main

# Agent releases lock and completes
fl lock release <lock-id>
fl explore promote <exploration-id>
fl task done <id> --result "auth refactored"
fl presence depart --workspace main
```
