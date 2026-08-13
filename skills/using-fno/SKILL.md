---
name: using-fno
description: Loaded at every SessionStart so the agent knows the two footnote surfaces (slash-command workflows + fno CLI primitives) exist from turn one. Mirrors the using-superpowers pattern.
---

# You are in a footnote-enabled project

This workspace has the `footnote` plugin installed. Two surfaces compose: skills call CLI verbs internally. Knowing both keeps you from hand-editing state files the CLI manages.

**Worktree-first default:** whenever possible, enter a dedicated feature worktree before editing, generating, or committing; keep the canonical checkout unclogged; prune after the PR lands. Exception: a project whose resolved `worktree.policy` is `never` works in place on the canonical checkout by design.

## 1. Slash-command workflows (orchestration, reasoning-required)

Invoke via `/fno:<verb>`. Front door:

| Verb | Purpose |
|------|---------|
| `/fno:target` | End-to-end pipeline: think -> plan -> do -> review -> ship. |
| `/fno:think` | Research cited findings to one file. Briefs: what-if, panel, class. Prefix `bg`/`subagent` to run off-thread. |
| `/fno:review` | Review a diff. Routes: `sigma` (six-agent panel, default), `peer` (cross-model). |
| `/fno:pr` | PR lifecycle. Routes: `create` (Haiku worker), `check`, `merged`. |
| `/fno:fix` | Repair. Routes: `fix` (default), `investigate`. |

Everything else stays invocable by full name: `/fno:blueprint`, `/fno:do` (`do waves` for orchestration), `/fno:ship` (`ship pr` = `/fno:pr`, `ship doc`), `/fno:setup`, `/fno:triage`, `/fno:agent`, `/fno:mail`, `/fno:ship-docs`, `/fno:audit`, `/fno:speculate`. The session skill list enumerates all of them; this curated set is the entry point, not an access boundary.

## 2. CLI primitives (`fno <verb>`, mechanical, fast)

Atomic, lock-protected, schema-validated. Use for exact state transitions, not orchestration.

| Verb family | What it owns |
|-------------|--------------|
| `fno event emit\|audit` | events.jsonl writes + audit. |
| `fno backlog ...` | graph.json mutations: intake, update, done, defer, supersede, find, get. |
| `fno pr status <n>` | Merge-readiness verdict. `statusCheckRollup` shows SUPERSEDED runs; `gh pr checks` ignores reviews. Reports `ready` + `optional_reviews_unresolved`. |
| `fno pr merge\|verify\|rebase` | PR ops with canonical guards. |
| `fno plan stamp\|graduate` | Plan frontmatter stamping at ship time. |
| `fno executor resolve` / `fno phase kill-check` | Executor chain / kill criteria. |
| `fno notify TITLE BODY` | OS notification. |
| `fno state` | State files. Only legal post-init target-manifest write: first-fill of empty `plan_path` via `fno state set --field plan_path` (else exit 5). |
| `fno-agents loop run --driver target` | The unified Rust loop; front door `scripts/run-target-loop.sh`. |
| `fno whoami\|status` | Self-introspection; run when confused after compaction. |
| `fno mail send\|reply\|unread\|ack` | Cross-project messaging over the jsonl bus; live-inject-first, durable fallback. |
| `fno agents spawn\|ask\|peek\|attach\|resume\|wait` | Cross-CLI agent lifecycle; per-harness support in `docs/harness-command-matrix.md`. |
| `fno carveout add` | Last resort: work too big for this PR. Else fix it here. |
| `fno outstanding` | Awaiting a human: carve-outs + questions; `ask`/`clear`. |

**Replying to a2a mail (the one rule).** Answer any `<fno_mail ... id="X">` with `fno mail reply --to X "..."`: it threads the reply and resolves the sender itself, whether the message arrived live or was drained, so never re-type a handle or inspect `harness`/`model`. Optional for FYIs.

**Read send evidence literally.** `delivered (hosted)` is confirmed; `queued (durable)` may never drain - no receipt is no coordination. Before re-sending, `peek` (a busy recipient may still get it), then `resume`/`attach`.

**Sending with a reply address.** `send <name>` self-stamps your handle; `--to-project` stamps the project (add `--from-self` if you will hold for the answer). Only `fno whoami`'s `mail:` line is a valid `--from-name`.

**Observing = `fno agents peek <handle>`** (`--lines`, `--follow`): tails a transcript peer or a pane worker via its mux ref. Distinct from `fno agents logs <name>` (registry-scoped).

**You are one of many agents (the mesh).** The loop is backlog -> spawn -> target -> mail: pull work with `fno backlog next`, spawn a peer into any project via `fno agents spawn --cwd <repo-root> "/fno:target <node>"` (the `--cwd` is load-bearing - never do another project's work inline), coordinate over `fno mail send <handle>`. Spawned workers are roster citizens; a hand-started session joins via `/fno-me`. `fno mux` hosts all of it as panes you can watch, drive, or message.

**Citizens vs limbs.** `fno agents spawn` makes a *citizen* (registry row, mail handle, survives its spawner); your harness's native subagent makes a *limb* (nested, no handle, no life of its own).
Spawn for citizens, harness subagent for limbs - the limb for one-shot work you consume next turn; spawn when it must outlive its spawner, be driven by a third party, or hold a claim.
Neither is always correct; a limb is observable only (`fno agents top --subagents`), not addressable.
Detail: [docs/architecture/coordination.md](docs/architecture/coordination.md).

**Mail is user-shaped.** `fno mail send` injects as user-shaped text, so it is the fallback when a worker's Skill-tool self-invocation is refused: `fno mail send <worker> --raw '/<verb>'` fires it at the prompt line (a wrapped reply does not). No live king -> advisory self-review (`docs/architecture/review-lanes.md`). A probe over mail tests only the user-triggered path, never autonomous action.

**Fix what you find. Carve out only what is too big.** A problem you spot mid-task gets FIXED in this PR as its own commit, unrelated or not. SIZE is the only justification for filing instead: `fno carveout add --kind deferred|oos-bug "<what + why>"`. Harvested at merge, cleared only by `fno retro sweep-carveouts --apply`. Prefer a node. Applies in every pipeline.

**Discovery:** `fno help` for the catalog, `fno help <verb>` for call shapes.

## 3. Forbidden surfaces

- NEVER edit `~/.fno/graph.json` directly (Edit/Write/`jq -i`/`sed -i`). Use `fno backlog`.
- NEVER mutate `.fno/target-state.md` after init (immutable manifest; sole exception above).
- Cancel: `touch .fno/.target-cancelled` or `TARGET_CANCEL=1`.

## 4. Picking the right surface

| You want to... | Use |
|----------------|-----|
| "Build this feature end-to-end" | `/fno:target` |
| "Mark node `<id>` done" | `fno backlog done <id>` (NOT a skill) |
| "Review my changes" | `/fno:review` |
| "Which task next?" | `fno backlog next` / `ready` |
| "What state am I in after compaction?" | `fno whoami` then `fno status` |
| "Open a PR" | `/fno:pr create` |
| "Wait for external review" | `/fno:pr check` |
| "Is this PR ready to merge?" | `fno pr status <n>` |
| "Merge an approved PR" | `fno pr merge` |
| "Rebase before merge" | `fno pr rebase --base=origin/main` |

When in doubt, prefer the smaller / more atomic surface. A skill spawns a new agent context; a CLI call doesn't.
