---
name: using-fno
description: Loaded at every SessionStart so the agent knows the two footnote surfaces (slash-command workflows + fno CLI primitives) exist from turn one. Mirrors the using-superpowers pattern.
---

<!-- style-exception: mechanical verb rename preserves pre-existing prose -->
# You are in a footnote-enabled project

This workspace has the `footnote` plugin installed. Two surfaces compose: skills call CLI verbs internally. Knowing both keeps you from hand-editing state files the CLI manages.

**Worktree-first default:** whenever possible, enter a dedicated feature worktree before editing, generating, or committing; keep the canonical checkout unclogged; prune after the PR lands. Exception: a project whose resolved `worktree.policy` is `never` works in place on the canonical checkout by design.

## Relay compression contract

Agent-authored `fno agents mail send`, `fno agents mail reply`, and `fno mux pane send` are handoffs. Use 80 words or fewer.

Think fully. Send outcome, reason, next action. Drop articles only where clear. Cut filler, pleasantries, hedges, repeated context. Fragments work. Keep technical terms, commands, errors, numbers, negation exact. Put findings on node/doc. Send link. Operator text stays exact.

Use `Status: X. Why Y. Done at Z.` or `Approval: Problem X. Options Y/Z. Recommend Z because A. Your call?`

## 1. Slash-command workflows (orchestration, reasoning-required)

Invoke via `/fno:<verb>`. Front door:

| Verb | Purpose |
|------|---------|
| `/fno:target` | End-to-end pipeline: think -> plan -> do -> review -> ship. |
| `/fno:think` | Research cited findings to one file. Briefs: what-if, panel, class. Prefix `bg`/`subagent` to run off-thread. |
| `/fno:review` | Review a diff. Routes: `sigma` (six-agent panel, default), `peer` (cross-model). |
| `/fno:pr` | PR lifecycle. Routes: `create` (Haiku worker), `check`, `merged`. |
| `/fno:fix` | Repair. Routes: `fix` (default), `investigate`. |

Everything else stays invocable by full name: `/fno:blueprint`, `/fno:execute` (`execute waves` for orchestration), `/fno:ship` (`ship pr` = `/fno:pr`, `ship doc`), `/fno:setup`, `/fno:triage`, `/fno:agent`, `/fno:mail`, `/fno:law`, `/fno:ship-docs`, `/fno:audit`, `/fno:speculate`. The session skill list enumerates all of them; this curated set is the entry point, not an access boundary.

## 2. CLI primitives (`fno <verb>`, mechanical, fast)

Atomic, lock-protected, schema-validated. Use for exact state transitions, not orchestration.

| Verb family | What it owns |
|-------------|--------------|
| `fno doctor event emit\|audit` | events.jsonl writes + audit. |
| `fno backlog ...` | graph.json mutations: intake, update, done, defer, supersede, find, get. |
| `fno do pr status <n>` | Merge-readiness verdict. `statusCheckRollup` shows SUPERSEDED runs; `gh pr checks` ignores reviews. Reports `ready` + `optional_reviews_unresolved` + `review_activity`. A review that is RUNNING now blocks `ready` (`review_in_flight`, `worktree_dirty`): coverage only knows what verdicts EXIST, and CI green reliably arrives before the review of that same head finishes. |
| `fno do pr merge\|verify\|rebase` | PR ops with canonical guards. |
| `fno do plan stamp\|graduate` | Plan frontmatter stamping at ship time. |
| `fno do phase kill-check` | Plan kill-criteria evaluation. |
| `fno inbox notify TITLE BODY` | OS notification. |
| `fno do state` | State files. Only legal post-init target-manifest write: first-fill of empty `plan_path` via `fno do state set --field plan_path` (else exit 5). |
| `fno-agents loop run --driver target` | The unified Rust loop; front door `scripts/run-target-loop.sh`. |
| `fno whoami\|status` | Self-introspection; run when confused after compaction. |
| `fno agents mail send\|reply\|unread\|ack` | Cross-project jsonl messaging; live-inject-first, durable fallback. |
| `fno agents spawn\|ask\|peek\|attach\|resume\|wait` | Cross-CLI agent lifecycle; per-harness support in `docs/harness-command-matrix.md`. |
| `fno backlog carveout add` | Last resort: work too big for this PR. Else fix it here. |
| `fno outstanding` / `fno backlog` | Awaiting a human: carve-outs + questions; `ask`/`clear`. `clear --answer` delivers the answer to the asker over mail, or states why it cannot. `backlog decide` records a ruling; `backlog decisions` recovers it (no subject = recent). |

**Replying to a2a mail (the one rule).** Answer any `<fno_mail ... id="X">` with `fno agents mail reply --to X "..."`: it threads the reply and resolves the sender itself, whether the message arrived live or was drained, so never re-type a handle or inspect `harness`/`model`. Optional for FYIs.

**Read send evidence literally.** `delivered (hosted)` is confirmed. `queued (durable)` can sit undrained - no receipt is no coordination. Before re-sending, `peek` (a busy recipient can still get it), then `resume`/`attach`. One exception: a `[bus-only]` queue drains by design. The recipient's turn-boundary `notify-self` surfaces it. A bus-only receipt IS coordination, never a stranded message.

**Pane drives carry an envelope; `typed` is not `delivered`.** `fno mux pane send` wraps in `<fno_mail>` by default and refuses a pane showing an option prompt. `--raw` types bytes verbatim; without `--submit` a send only types, and prints nothing. On a `live-miss` that reads busy rather than dead, `fno mail send --force` types the wrapped body, keeping the msg-id, reply handle and outbox row. `typed (pane <id>)` is not delivery. [Details](docs/architecture/pane-transport.md).

**Codex: full session_id or pane, never head-8.** A codex UUIDv7 head-8 is a ~65.5s clock bucket, so siblings from one minute collide and `mail send` refuses that shape. A claude UUIDv4 head-8 is safe. On an old ambiguous one, `mail reply --sender-session <full-id>` keeps the thread.

**Sending with a reply address.** `send <name>` self-stamps your handle; `--to-project` stamps the project (add `--from-self` if you will hold for the answer). Only `fno whoami`'s `mail:` line is a valid `--from-name`.

**Observing = `fno agents peek <handle>`** (`--lines`, `--follow`): tails a transcript peer or a pane worker via its mux ref. Distinct from `fno agents logs <name>` (registry-scoped).

**You are one of many agents (the mesh).** The loop is backlog -> spawn -> target -> mail: pull work with `fno backlog next`, spawn a peer into any project via `fno agents spawn --cwd <repo-root> "/fno:target <node>"` (the `--cwd` is load-bearing - never do another project's work inline), coordinate over `fno agents mail send <handle>`. Spawned workers are roster citizens; a hand-started session joins via `/fno-me`. `fno mux` hosts all of it as panes you can watch, drive, or message.

**Citizens vs limbs.** `fno agents spawn` makes an addressable, durable roster citizen. A native subagent is a one-shot, observable-only limb. Spawn work that must outlive you, hold a claim, or receive mail. Use a limb for a result consumed next turn. [Details](docs/architecture/coordination.md).

**Mail is user-shaped.** It is the fallback after a worker's own invocation is refused: `fno agents mail send <worker> --raw '/<verb>'`. A mail probe proves user-triggered behavior, never autonomy. No live king means [advisory self-review](docs/architecture/review-lanes.md).

**Fix what you find. Carve out only what is too big.** A problem you spot mid-task gets FIXED in this PR as its own commit, unrelated or not. SIZE is the only justification for filing instead: `fno backlog carveout add --kind deferred|oos-bug "<what + why>"`. Harvested at merge, cleared only by `fno backlog retro sweep-carveouts --apply`. Prefer a node. Applies in every pipeline.

**Discovery:** `fno help` for the catalog, `fno help <verb>` for call shapes.

## Known Limitations and Deferred Work

- Live receipts can become stale. See [LIMITATIONS.md](LIMITATIONS.md).

## Picking the right surface

| You want to... | Use |
|----------------|-----|
| "Build this feature end-to-end" | `/fno:target` |
| "Mark node `<id>` done" | `fno backlog done <id>` (NOT a skill) |
| "Review my changes" | `/fno:review` |
| "Which task next?" | `fno backlog next` / `ready` |
| "What state am I in after compaction?" | `fno whoami` then `fno whoami status` |
| "Open a PR" | `/fno:pr create` |
| "Wait for external review" | `/fno:pr check` |
| "Is this PR ready to merge?" | `fno do pr status <n>` |
| "Merge an approved PR" | `fno do pr merge` |
| "Rebase before merge" | `fno do pr rebase --base=origin/main` |

When in doubt, prefer the smaller / more atomic surface. A skill spawns a new agent context; a CLI call doesn't.
