# Flock Remote — Killer Feature Brainstorm

Ideas for the Flock remote server that go beyond "GitHub but for flock."

---

## Ghost Text (Semantic Presence Overlays)

See a faint overlay in your editor showing what another dev or agent is currently changing — at the symbol level, not raw keystrokes.

Example:
```
// ghost: agent-B is modifying function:validate
// ghost: +2 params, body rewritten, ~15 lines
function validate(payment) {              ← your version, solid
┊ function validate(payment, currency, opts) {  ← their version, ghosted
    ...
}
```

Why it works with flock:
- Presence events already track who's working where
- Semantic awareness means you see "they're touching function:validate", not "line 47"
- Event streaming is trivial with append-only log

Key distinction: this shows **semantic previews**, not character-by-character streams. Way more useful, way less distracting.

---

## Semantic Feed

A real-time feed of *meaningful* changes instead of git noise.

Not: "Chris pushed 3 commits"

Instead: "Agent A added `CurrencyConverter` class (3 methods), modified `PaymentProcessor.process()` signature (breaking, 12 callers affected)."

Could run in a sidebar, terminal pane, or web dashboard. Every update is a structured semantic change, not a raw diff.

---

## "Heads Up" Warnings (Proactive Conflict Avoidance)

You start editing a function → the server knows another agent claimed a task touching the same symbol → before you even have a conflict:

> "Agent B is actively working on `validate()` via task tk-a1b2 — coordinate or continue?"

This is proactive coordination framed as natural UX, not an explicit lock protocol. Advisory, not blocking.

---

## Exploration Spectating

Watch an agent work in real time — not its terminal output, but its **semantic trail**:

- Started exploration "try-async-approach"
- Modified 3 functions in `payment.ts`
- Checkpointed
- Abandoned (reason: "async added complexity without benefit")
- Started new exploration "extract-validation"
- ...

You see the exploration tree building live. Great for understanding what an agent is doing without reading its stream-of-consciousness output.

---

## Continuous Review (Review-as-it-happens)

Instead of waiting for a PR to be "done" and then reviewing 4,000 lines, review semantic changes as they land:

1. Agent finishes refactoring `validate()` → you get a notification
2. You review just that one semantic unit → approve or comment
3. Agent continues working on the next piece

Continuous review instead of batch review. The batch PR model is a git limitation, not a law of physics. This directly attacks the "3,000 line PR review" problem.

---

## Conflict Forecast

The server sees all active explorations and their semantic change sets. It can predict conflicts before they happen:

> "Agent A and Agent C are both modifying `PaymentProcessor` — likely conflict in ~10 minutes."

Auto-coordinate before the conflict materializes. Nobody else does this.

---

## Technical Model

All of these features share the same underlying architecture:

- Agents and editors maintain a **websocket** to the remote, streaming events in real time
- The server is an **event log with pub/sub** on top
- Clients **subscribe** to events matching their interest (file, symbol, module, agent)
- The **semantic layer on the server** classifies events before broadcasting — clients get structured updates, not raw diffs

Key insight: none of this requires shared-state editing (the Google Docs problem). Everyone still works in their own workspace with their own files. The real-time layer is **awareness, not collaboration**. You're not co-editing — you're co-aware. That's a much simpler problem technically, and arguably more useful for code.

---

## Strongest Wedge

Ghost text + continuous review is the combo that directly competes with GitHub PRs. It replaces the worst part of the current workflow (batch reviewing massive diffs) with something genuinely better.

---

## Minimum Viable Remote (prerequisites)

Before any of these features, the remote needs:
- Event + snapshot transport (push/pull over HTTPS)
- Auth (SSH keys or tokens — ed25519 signing already exists)
- Web UI for semantic review (renders `fl review` / `fl diff --semantic`)
- Multi-user refs (who owns which branch/exploration)

The real-time features layer on top via websocket event streaming.
