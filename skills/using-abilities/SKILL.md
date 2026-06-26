---
name: using-abilities
description: Loaded at every SessionStart so the agent knows the two footnote surfaces (slash-command workflows + fno CLI primitives) exist from turn one. Mirrors the using-superpowers pattern.
---

# You are in an footnote-enabled project

This workspace has the `footnote` plugin installed. There are **two surfaces** for getting things done, and they compose - skills call CLI verbs internally. Knowing both keeps you out of loops where you would otherwise hand-edit state files the CLI is meant to manage.

## 1. Slash-command workflows (orchestration, reasoning-required)

Invoke via `/fno:<verb>`. These compose multiple steps and require LLM reasoning. The full skill body loads when invoked.

Five verbs are the advertised front door:

| Verb | Purpose |
|------|---------|
| `/fno:target` | End-to-end pipeline: think -> plan -> do -> review -> ship. The flagship execution front door. |
| `/fno:think` | Reason about a design before building. Routes: `think` (design+BDD, default), `what-if` (stress test), `panel` (multi-persona debate). |
| `/fno:review` | Review a diff. Routes: `sigma` (internal six-agent panel, default), `peer` (cross-model second opinion). |
| `/fno:pr` | Drive a PR through its lifecycle. Routes: `create` (Haiku worker), `check` (poll + implement external review), `merged` (the post-merge ritual). |
| `/fno:fix` | Repair a broken state. Routes: `fix` (fast one-fix-per-iteration loop with auto-revert, default), `investigate` (scientific-method hypothesis loop). |

**Everything else stays invocable by its full name** - it is just not surfaced at the top. Common non-advertised verbs: `/fno:blueprint` (plan authoring), `/fno:do` (execute a plan; `do waves` runs wave orchestration), `/fno:ship` (the deliverable umbrella: `ship pr` = `/pr`, `ship doc` ships a research brief), `/fno:setup`, `/fno:triage`, `/fno:agent`, `/fno:mail` (message a peer/project over `fno mail`), `/fno:inbox`, `/fno:ship-docs`, `/fno:audit`, `/fno:speculate`.

The full skill catalog is in your session skill list - look for entries prefixed `footnote:`. The harness enumerates every skill with a description, so the non-advertised verbs remain discoverable; this curated set is the recommended entry point, not an access boundary.

## 2. CLI primitives (`fno <verb>`, mechanical, fast)

Atomic, lock-protected, schema-validated. Callable from anywhere (bash, Python, hooks, even from inside other skills). Use these when you need an exact state transition, NOT for orchestration.

| Verb family | What it owns |
|-------------|--------------|
| `fno event emit\|audit\|verify-evidence` | events.jsonl writes + audit. |
| `fno backlog ...` | graph.json mutations: intake, update, done, defer, supersede, reprioritize, find, get. Replaces direct `roadmap-tasks.py` calls. |
| `fno pr merge\|verify\|rebase` | PR ops with footnote-canonical guards. |
| `fno plan stamp\|graduate` | Plan frontmatter stamping at ship time. |
| `fno executor resolve` | Three-tier executor chain (locked / inferred / default). |
| `fno phase kill-check` | Kill criteria evaluation. |
| `fno notify TITLE BODY` | OS notification. |
| `fno state` | Read/write/validate state files. The ONLY legal post-init mutation on a target manifest is first-fill of an empty `plan_path` via `fno state set --field plan_path` (else exit 5). |
| `fno-agents loop run --driver target\|megawalk` | The unified Rust loop (step 5). Front door: `scripts/run-target-loop.sh`. The old `fno loop` verb is removed. |
| `fno whoami\|status` | Self-introspection. Run when confused after compaction. |
| `fno mail send --to-project` / `fno mail reply\|unread\|ack` | Cross-project messaging: one namespace over the jsonl-canon bus log. `send` publishes (durable-first); `unread`/`ack` are the per-recipient cursor consume; `reply` correlates. (The legacy inbox + agents-send messaging surfaces are retired.) |
| `fno agents spawn\|promote\|host\|ask\|watch ...` | Cross-CLI agent lifecycle (claude / codex / gemini). Per-provider support differs (e.g. `promote` adopts claude into a stream-json lane; `drive`/`grid` are codex/gemini-only; `watch` is claude-only) - see `docs/provider-command-matrix.md`. To message a peer, use `fno mail send`. |
| `fno carveout add` | Capture left-out work (deferred decisions, out-of-scope bugs) to a session ledger for retro-triage at merge. |

**Capture left-out work as you go.** The moment you consciously leave work undone - defer a decision pending an open question, or spot an out-of-scope bug while building something else - record it: `fno carveout add --kind deferred|oos-bug [--need "<open question>"] [--priority pN] "<what + why>"`. This is advisory, not a gate: it appends one line to `.fno/carveouts.jsonl` and the retro-triage harvest at merge turns surviving items into backlog nodes (deduped, classified). A missed call is tolerated (the merge-time harvest of skipped reviews + deferred findings is the backstop); the point is that a *decided-but-not-done* item should not evaporate when the session ends. Applies in every pipeline - `/target`, `/do` (including `do waves`), `/megawalk`, and the autonomous loops.

**Discovery:** run `fno help` for the verb catalog, `fno help <verb>` for any subcommand's call shape (e.g. `fno help gate set`). The git-style form is preferred over `fno --help` in canonical instructions.

## 3. Forbidden surfaces

The `PreToolUse` hook detects direct graph mutations post-hoc.

- NEVER edit `~/.fno/graph.json` directly via Edit/Write/`jq -i`/`sed -i`. Use `fno backlog` commands.
- NEVER mutate `.fno/target-state.md` after init via Edit/Write/Bash. It is an immutable session manifest. The only legal post-init write is first-fill of an empty `plan_path` field via `fno state set --field plan_path` (any other field exits 5, per ab-d0337fbc). There are no gate booleans, no `current_phase`, no `status` field to write.
- To trigger a cancel, `touch .fno/.target-cancelled` or export `TARGET_CANCEL=1`.

## 4. Picking the right surface

Common failure mode: knowing both exist but reaching for the wrong one. Heuristic:

| You want to... | Use |
|----------------|-----|
| "Build this feature end-to-end" | `/fno:target` |
| "Mark backlog node `<id>` done" | `fno backlog done <id>` (NOT a skill) |
| "Review my changes before pushing" | `/fno:review` (sigma panel by default) |
| "Find which task to work on next" | `fno backlog next` or `fno backlog ready` |
| "Know what state I'm in after compaction" | `fno whoami` then `fno status` |
| "Open a PR for current commits" | `/fno:pr create` |
| "Wait for external review on a PR" | `/fno:pr check` |
| "Merge an approved PR" | `fno pr merge` |
| "Rebase before merge" | `fno pr rebase --base=origin/main` |

When in doubt, prefer the smaller / more atomic surface. A skill invocation spawns a new agent context; a CLI call doesn't.
