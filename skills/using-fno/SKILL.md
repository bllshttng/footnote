---
name: using-fno
description: Loaded at every SessionStart so the agent knows the two footnote surfaces (slash-command workflows + fno CLI primitives) exist from turn one. Mirrors the using-superpowers pattern.
---

# You are in a footnote-enabled project

This workspace has the `footnote` plugin installed. Two surfaces compose: skills call CLI verbs internally. Knowing both keeps you from hand-editing state files the CLI manages.

**Worktree-first default:** enter a dedicated feature worktree before editing, generating, or committing; keep the canonical checkout unclogged; prune after the PR lands.

## 1. Slash-command workflows (orchestration, reasoning-required)

Invoke via `/fno:<verb>`. Front door:

| Verb | Purpose |
|------|---------|
| `/fno:target` | End-to-end pipeline: think -> plan -> do -> review -> ship. |
| `/fno:think` | Design reasoning. Routes: `think` (default), `what-if`, `panel`. |
| `/fno:review` | Review a diff. Routes: `sigma` (six-agent panel, default), `peer` (cross-model). |
| `/fno:pr` | PR lifecycle. Routes: `create` (Haiku worker), `check`, `merged`. |
| `/fno:fix` | Repair. Routes: `fix` (default), `investigate`. |

Everything else stays invocable by full name: `/fno:blueprint`, `/fno:do` (`do waves` for orchestration), `/fno:ship` (`ship pr` = `/fno:pr`, `ship doc`), `/fno:setup`, `/fno:triage`, `/fno:agent`, `/fno:mail`, `/fno:ship-docs`, `/fno:audit`, `/fno:speculate`. The session skill list enumerates all of them; this curated set is the entry point, not an access boundary.

## 2. CLI primitives (`fno <verb>`, mechanical, fast)

Atomic, lock-protected, schema-validated. Use for exact state transitions, not orchestration.

| Verb family | What it owns |
|-------------|--------------|
| `fno event emit\|audit\|verify-evidence` | events.jsonl writes + audit. |
| `fno backlog ...` | graph.json mutations: intake, update, done, defer, supersede, find, get. |
| `fno pr merge\|verify\|rebase` | PR ops with canonical guards. |
| `fno plan stamp\|graduate` | Plan frontmatter stamping at ship time. |
| `fno executor resolve` / `fno phase kill-check` | Executor chain / kill criteria. |
| `fno notify TITLE BODY` | OS notification. |
| `fno state` | State files. Only legal post-init target-manifest write: first-fill of empty `plan_path` via `fno state set --field plan_path` (else exit 5). |
| `fno-agents loop run --driver target\|megawalk` | The unified Rust loop; front door `scripts/run-target-loop.sh`. |
| `fno whoami\|status` | Self-introspection; run when confused after compaction. |
| `fno mail send\|reply\|unread\|ack` | Cross-project messaging over the jsonl bus; live-inject-first, durable fallback. |
| `fno agents spawn\|ask\|peek\|attach\|resume\|wait` | Cross-CLI agent lifecycle; per-harness support in `docs/harness-command-matrix.md`. |
| `fno carveout add` | Capture left-out work for retro-triage at merge. |

**Replying to a2a mail (the one rule).** A message arrives as `<fno_mail from="H" ...>`. Reply with `fno mail send H "..."` - pass back the exact `from` handle, nothing else; the CLI resolves it across every live source and falls back to the durable bus. Never inspect `harness`/`model` to pick a transport. Replying is optional for FYIs.

**A durable receipt is not delivery.** Read the send receipt: `delivered (hosted)` = confirmed into the peer's session; `queued (durable)` = NOT confirmed, waiting on a drain that may never run. Don't wait on a durable send: `fno agents peek <handle>` (landed? alive?), `resume <handle>` then re-send, or `attach <handle>`. Check with `peek` before re-sending - a busy recipient can still receive the queued turn, so a blind re-send double-delivers.

**Correlated reply when draining your inbox.** `fno mail unread` / `drain-self` list messages with `id:`; answer a specific one with `fno mail reply --to <id> "..."` (threads `in_reply_to`). A live-injected message has no bus id - use `send <from>` for those.

**Sending with a reply address.** Name-lane `send <name>` self-stamps your handle. `--to-project` stamps the project; if you will hold for the answer, add `--from-self`. The `mail:` line of `fno whoami` is the only valid `--from-name`.

**Observing = `fno agents peek <handle>`**, the read-only twin of send; same resolver, so any peer you can message you can watch (`--lines N`, `--follow`). Distinct from `fno agents logs <name>` (registry-scoped).

**You are one of many agents (the mesh).** The loop is backlog -> spawn -> target -> mail: pull work with `fno backlog next`, spawn a peer into any project via `fno agents spawn --cwd <repo-root> "/fno:target <node>"` (the `--cwd` is load-bearing - never do another project's work inline), coordinate over `fno mail send <handle>`. Spawned workers are roster citizens addressable by bare `<shortid>`; a hand-started session joins via `/fno-me` (or `agents.auto_register_sessions = true`). `fno mux` hosts all of it as panes you can watch, drive, or message.

**Fold in small fixes; capture the rest.** A small pre-existing bug found mid-task gets fixed in the current PR as its own atomic commit (optionally file a born-done record: `fno backlog idea` + `update --pr-number`). Not-small, or decided-but-deferred: `fno carveout add --kind deferred|oos-bug [--need "..."] "<what + why>"` - advisory, harvested into backlog nodes at merge. Applies in every pipeline.

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
| "Merge an approved PR" | `fno pr merge` |
| "Rebase before merge" | `fno pr rebase --base=origin/main` |

When in doubt, prefer the smaller / more atomic surface. A skill spawns a new agent context; a CLI call doesn't.
