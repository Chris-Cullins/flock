# jj Import Design and Metadata Mapping

This document defines the Phase 2 design for importing Jujutsu (`jj`) history into Flock.

## Scope

- Define a deterministic mapping from `jj` concepts to Flock events, refs, and metadata.
- Reuse the existing checkpoint and git bridge machinery where possible.
- Preserve enough provenance to make imports idempotent and auditable.

## Non-goals (for first implementation)

- Full export from Flock back to native `jj` operation history.
- Reproducing every `jj` operation-log entry as a first-class Flock event kind.
- Supporting non-git `jj` backends in v1.

## Constraints from Current Flock Model

- Checkpoints are first-class history nodes (`EventKind::Checkpoint`).
- Git mapping metadata is recorded through `EventKind::GitBridge` with `action=Commit`.
- Mapping data is currently key/value tokens inside `GitBridgeEvent.detail`.
- Refs are modeled as `Branch`, `Tag`, or `Workspace`.

## Source-to-Target Mapping

| `jj` concept | Flock representation | Notes |
| --- | --- | --- |
| Backend commit id (git SHA) | `CheckpointEvent` + `GitBridgeEvent(action=Commit)` | Reuse existing `create_checkpoint_from_git_commit` path. |
| Change id | `Tag` ref named `jj/change/<change-id>` -> checkpoint event id | Stable pointer despite rewritten commit ids. |
| Bookmark | `Branch` ref named `jj/bookmark/<bookmark>` -> checkpoint event id | Preserves named heads from `jj`. |
| Working copy commit | `Workspace` ref named `jj/working-copy` | Tracks the imported working copy tip. |
| Parent/ancestor links | `CheckpointEvent.parent_checkpoint_event` | Keeps deterministic lineage replay. |
| Conflict state marker | `GitBridgeEvent.detail` tokens + sidecar metadata file | Metadata only in v1, no semantic conflict reconstruction yet. |
| Operation id | Stored in metadata for traceability | Imported as provenance, not a first-class event type. |

## Metadata Schema

### Per-checkpoint mapping (`GitBridgeAction::Commit`)

Each imported `jj` commit appends a successful `GitBridge` commit mapping event with at least:

- `checkpoint=<flock-event-uuid>`
- `git_commit=<backend-commit-sha>`
- `source=jj`
- `jj_change_id=<change-id>`
- `jj_operation_id=<operation-id-or-none>`
- `jj_conflicted=<true|false>`

Example:

`checkpoint=5a0... git_commit=18f... source=jj jj_change_id=qpwz... jj_operation_id=3f7... jj_conflicted=false`

### Import summary (`GitBridgeAction::Import`)

- `source=jj`
- `revset=<revset-or-head>`
- `imported=<n>`
- `skipped=<n>`
- `head_change_id=<change-id>`
- `head_git_commit=<sha>`

## Sidecar Provenance File

Because `GitBridgeEvent.detail` is token-based, richer data is stored in:

- `.flock/jj/change-map.jsonl`

Each line contains:

- `jj_change_id`
- `jj_commit_id` (backend id)
- `jj_operation_id`
- `flock_checkpoint_event_id`
- `conflicted` boolean
- `bookmarks` array
- `author`, `timestamp`, `description`

This file is append-only and regenerated only through explicit repair/reimport flows.

## Import Algorithm (v1)

1. Validate repository and `jj` availability.
2. Enumerate import target commits in topological order (default `::` / reachable set from `@` depending on command mode).
3. Build existing mapping set from successful `GitBridgeAction::Commit` events where `source=jj`.
4. For each unmapped backend commit:
   - materialize snapshot via existing git tree extraction path;
   - create checkpoint with `parent_checkpoint_event` from prior imported ancestor;
   - append commit mapping metadata including `jj_change_id` and operation id;
   - update `jj/change/<id>` tag ref.
5. Upsert bookmark refs as `jj/bookmark/<name>`.
6. Upsert `jj/working-copy` workspace ref.
7. Append `GitBridgeAction::Import` summary event.

## Idempotency Rules

- Primary key for skip logic: backend commit id (`git_commit`) with `source=jj`.
- Re-import of the same commit must not create a second checkpoint.
- Ref upserts are last-write-wins and deterministic.

## Failure and Recovery

- Partial import is recoverable by rerunning import (idempotent mapping).
- If sidecar metadata exists but mapping events are missing, treat events as source of truth and rebuild sidecar.
- If mapping events exist but sidecar is corrupted, regenerate sidecar from events and `jj` queries.

## CLI Shape (planned)

- `fl jj import [--revset <expr>] [--bookmarks-only]`
- `fl jj inspect-map [--change-id <id>]`

`fl git import` remains unchanged; `fl jj import` is the explicit jj path.

## Compatibility Notes

- v1 assumes `jj` repositories using git backend commits.
- Non-git backend support is deferred and should use snapshot materialization through `jj` plumbing commands.
- Metadata keys are additive to preserve backward compatibility with existing `parse_git_commit_from_detail` logic.

## Acceptance Criteria

- A `jj` repo imports into checkpoints with deterministic lineage.
- Change ids and bookmarks are queryable through Flock refs.
- Re-running import does not duplicate checkpoints.
- Provenance for each imported checkpoint includes `source=jj` and `jj_change_id`.
