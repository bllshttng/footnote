---
name: using-fno
description: Loaded at every SessionStart so the agent knows the two footnote surfaces (slash-command workflows + fno CLI primitives) exist from turn one. Mirrors the using-superpowers pattern.
---

# You are in a footnote-enabled project

This workspace has the `footnote` plugin installed. There are **two surfaces** for getting things done, and they compose - skills call CLI verbs internally. Knowing both keeps you out of loops where you would otherwise hand-edit state files the CLI is meant to manage.

**Worktree-first default:** for any repo work, create or enter a dedicated feature worktree whenever possible before editing files, running generators, or committing. Keep the canonical main checkout unclogged and pullable. If you are already in the correct feature worktree, continue there. After the PR lands, prune the finished worktree.

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

**Everything else stays invocable by its full name** - it is just not surfaced at the top. Common non-advertised verbs: `/fno:blueprint` (plan authoring), `/fno:do` (execute a plan; `do waves` runs wave orchestration), `/fno:ship` (the deliverable umbrella: `ship pr` = `/fno:pr`, `ship doc` ships a research brief), `/fno:setup`, `/fno:triage`, `/fno:agent`, `/fno:mail` (message a peer/project over `fno mail`, the single front door for cross-project `--kind` notes), `/fno:ship-docs`, `/fno:audit`, `/fno:speculate`.

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
| `fno mail send --to-project` / `fno mail reply\|unread\|ack` | Cross-project messaging: one namespace over the jsonl-canon bus log. `send` delivers live-inject-first (the durable bus is the offline fallback, node x-1f23); `unread`/`ack` are the per-recipient cursor consume; `reply` correlates. (The legacy inbox + agents-send messaging surfaces are retired.) |
| `fno agents spawn\|ask\|peek\|attach\|resume\|wait ...` | Cross-CLI agent lifecycle (claude / codex / gemini / agy / opencode). Per-harness support differs (e.g. `--substrate bg` and `watch`/`attach` are claude-only; agy is stateless; opencode is pane-only) - see `docs/harness-command-matrix.md`. To message a peer, use `fno mail send`. |
| `fno carveout add` | Capture left-out work (deferred decisions, out-of-scope bugs) to a session ledger for retro-triage at merge. |

**Replying to an a2a message (the one rule).** A message from another agent arrives self-addressed as `<fno_mail from="H" harness="..." model="...">...`. To reply, run `fno mail send H "..."` — pass back the exact `from` handle and nothing else. Do NOT inspect `harness`/`model` to pick a transport or look the sender up: the CLI resolves `H` across every live source (registry, daemon roster, disk, codex) and falls back to the durable bus, so a rostered `claude --bg` worker, a live codex thread, and an offline peer are all addressed the same way. Your own outbound envelope auto-stamps your real handle + model, so the recipient can reply to you by the same rule. Replying is optional — an FYI or broadcast needs none; reply when the message asks or expects one.

**A durable receipt is not delivery.** `fno mail send` injects into the recipient's live session and only falls back to the durable bus on a miss, so read the receipt: `delivered (hosted)` means the inject was confirmed into the peer's session; `queued (durable)` means it was NOT confirmed, and now waits on a drain that may never run. Do not wait on a durable send — the handle you mailed is normally the same id `fno agents peek <handle>` (did it land? is it alive?), `resume <handle>` (idle → live, then re-send), and `attach <handle>` (drive it yourself) take, though resume/attach need a registry row where `send` does not. Check with `peek` before re-sending: a busy recipient can queue the injected turn past the confirm budget and still receive it, so a blind re-send double-delivers.

**Correlated reply when draining your inbox.** The rule above is for a message injected **live** into your session (it carries no bus id to correlate against). When you instead **drain your inbox** — `fno mail unread` / `fno mail drain-self` list each message with its `id:` — answer a specific one with `fno mail reply --to <id> "..."`. Same one delivery path as `send`, but it addresses the original sender for you (no re-typed handle) and threads `in_reply_to`, so "was this message answered?" becomes a queryable fact on the bus. `reply --to <id>` needs the id to exist on the bus, so it is the drain-time verb; a live-injected message with no id still uses `send <from>`.

**Being reachable when you send-and-hold.** A name-lane `send <name>` self-stamps your reply handle automatically (omit `--from-name`). A `--to-project` send stamps the *project* by default — fine for a fire-and-forget note, but a reply then lands in the project inbox, not your session. If you `--to-project` and will hold for the answer, pass `--from-self` (it stamps your own handle; exit 2 if there is no ambient identity, never a silent floor). The `mail:` line of `fno whoami` is the only field that is a valid `--from-name`; the `run:` line is a ledger id, not a handle.

**Reply vs observe — the same handle, both legs.** Reply is `fno mail send <handle>`; **observe** is `fno agents peek <handle>` — the read-only twin. peek resolves `<handle>` through that same union resolver, so any peer you can message you can also watch: `fno agents peek <handle> [--lines N] [--follow]` prints the peer's recent transcript (or its normalized status events when present) without writing anything the peer reads. Distinct from `fno agents logs <name>`, which is registry-scoped (fno-spawned names only); peek works for a live codex thread or unrostered `claude --bg` session too.

**You are one of many agents (the mesh).** footnote sessions coordinate as peers, and the loop is backlog -> spawn -> target -> mail: pull work with `fno backlog next`, **spawn a peer into any project with its context** via `fno agents spawn --cwd <repo-root> "/fno:target <node>"`, then coordinate over `fno mail send <handle>`. The `--cwd` is load-bearing - it lands a worker in a *foreign* repo, so a multi-repo feature ships one PR per repo instead of one session editing both; do NOT do another project's work inline, spawn a worker there and hand it context. Every spawned worker is a roster citizen addressable by its canonical bare `<shortid>` handle; a session a human started by hand is NOT, until it runs **`/fno-me`** (`fno agents register`) to join the roster so peers can find and message it. All of this can be hosted in **`fno mux`**, the terminal multiplexer that renders each session as a pane you can watch, drive, or message. (Set `agents.auto_register_sessions = true` to auto-join every hand-started session instead of per-session `/fno-me`.)

**Fold in small fixes; capture the rest.** When you spot a small pre-existing bug while building something else, the default is to fold the fix into the current PR as its own atomic commit - not defer it (optionally file a born-done record so the graph remembers: `fno backlog idea "<bug>"` then `fno backlog update <id> --pr-number <n>`, which closes at merge via reconcile). The warm window is the only window: deferral-born work returns within days or never. "Small" means fixable with its own focused commit inside the current session, without growing the PR's review surface beyond recognition; when in doubt, or when the fix wants its own tests or design, it is not small - capture it instead. Capture-and-defer is the path for a non-small find, or a decision you consciously leave open pending an open question: `fno carveout add --kind deferred|oos-bug [--need "<open question>"] [--priority pN] "<what + why>"`. This is advisory, not a gate: it appends one line to `.fno/carveouts.jsonl` and the retro-triage harvest at merge turns surviving items into backlog nodes (deduped, classified). A missed call is tolerated (the merge-time harvest of skipped reviews + deferred findings is the backstop); the point is that a *decided-but-not-done* item should not evaporate when the session ends. Applies in every pipeline - `/fno:target`, `/fno:do` (including `do waves`), `/fno:megawalk`, and the autonomous loops.

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
