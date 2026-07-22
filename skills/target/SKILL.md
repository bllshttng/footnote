---
name: target
description: "Use when: build this feature, get it done end-to-end, or execute a plan from idea to PR."
argument-hint: "[S|small|M|medium|L|large] [agent|fork] [beastmode|beast] [clean] [adversarial] [auto-merge | no-merge] [combo <name>] [resume|cancel] [expertise] <ab-xxxxxxxx | feature-description | plan-path> [--max-iterations N] [--budget N] [--no-ship] [--no-external] [--no-docs] [--no-browser]"
metadata:
  internal: true
requires:
  binaries:
    - "fno >= 0.1"
    - "gh >= 2.0"
    - "git >= 2.30"
---

# Target

**Get it done.** From idea to a green, reviewed PR.

When `$CODEX_THREAD_ID` is nonblank, before any routing or work, Print exactly once:
`codex posture: target uses the native Stop loop on the main thread; delegated work uses spawn_agent; bg dispatch is Claude-only.`

## The spine (happy path - read this first)

```
resolve node  →  fno target start <node>   worktree off origin/main + claim + init prints the orienter
              →  Step 0 (only if STALE)     orienter says boundary-reconcile: STALE -> read blocker diffs, append landed-facts sections
              →  implement                  edit the plan; atomic commits as you go
              →  /review                     internal sigma panel (cheap insurance)
              →  validate                    fno test  (real exit code; not bare pytest)
              →  /pr create                  Haiku worker opens the PR
              →  <promise>MISSION COMPLETE...  PR green + reviewed = done; merge if config.auto_merge.enabled
```

That is the whole job when a backlog node or plan is already bound. `fno target start` prints an orientation report (node, worktree, tests, done-when) - read it and go. Everything below is detail on a spine step or an **"only if"** branch you skip unless its trigger fires.

**Which path you are on is a READ, not a guess** - dispatch off the node's real state (`fno backlog get <id>`), never self-classification:

- **ready node** (a plan is bound and its frontmatter reads `status: ready`): you are on the spine above. Run it and stop. Do not load the idea, blueprint, discovery, or handoff references - they do not apply.
- **design-rung node** (a plan is bound but its frontmatter still reads `status: design`): take the `/blueprint`-first branch below, then the spine.
- **bare idea** (no plan): take the `/think` then `/blueprint` branch below, then the spine.

**Enter the worktree after the receipt (harness step, do this before implementing).** The `fno target start` receipt names a `worktree:` path and ends with a `cd <path> to continue` line - but a shell `cd` does not persist across tool calls, so prefixing every later command with `cd <worktree> &&` is the failure this step prevents. Instead call the harness **EnterWorktree** tool with `path` set to that receipt worktree line; the session then runs from inside the worktree and every file edit is worktree-relative. This is location-agnostic: any path in `git worktree list` is enterable on first entry, so it works the same for a configured `worktrees_base`, the deprecated conductor base, or the harness-native `.claude/worktrees/` default - never hardcode a base path, read it from the receipt. Two caveats worth knowing: **ExitWorktree never removes a path-entered worktree** (it only returns you to the launch dir; removal is `scripts/setup/archive-worktree.sh`'s job), and after entering, **same-session switches to another worktree are restricted to `.claude/worktrees/`** - irrelevant for a one-node session, surprising only if you try to hop worktrees mid-run.

- **only if** you are already inside a worktree (an attended `/target` in a linked worktree): `fno target start` is a no-op there ("already isolated; nothing created") - run `fno target init --input <node>` (add `--plan-path <path>` for a plan) instead. `start` is the cold-start-from-canonical verb; `init` is what writes the manifest, claims the node, and prints the orienter.
- **only if** you were handed a bare idea (no plan): run `/think` then `/blueprint` before implementing.
- **only if** the node's rung is `design` (a plan IS bound, but its frontmatter still reads `status: design` - a `/think` doc that was never blueprinted): run `/blueprint <plan_path>` FIRST, then implement. A bound `plan_path` alone does NOT mean the plan is executable: `/blueprint` is what appends the Execution Strategy and flips the doc to `ready`. Skipping it here builds off a design doc that has no execution plan. Autonomous selection never hands you this rung (it is gated); you only reach it when a human named the node explicitly, which IS the consent to carry it the rest of the way.
- **only if** `$TARGET_BRIEF` is set in the environment (a dispatcher passed a per-node brief via `dispatch_brief`, US3): read it as extra mission context - the scope/"why" the dispatcher wanted this worker to carry. It is plain text (capped at 8 KB) and travels via env, never the command line; treat it as guidance for this node, not as a command to execute.
- **only if** the run carries `authority: full` (invoked as `/target beastmode`, surfaced on the `attended` line of `fno target status`): a judgment call that would emit `<help>` and stall is decided and recorded instead - [references/beastmode-authority.md](references/beastmode-authority.md).
- **only if** `.fno/target-state.md` already exists for this session: you are **mid-loop** - re-verify the world and re-emit `<promise>`; do NOT re-init or rebuild.
- **only if** dispatching nodes to run unsupervised (`bg`) or running a batch-lane member (`batched`): [references/bg-and-batched-modes.md](references/bg-and-batched-modes.md).
- **only if** the orienter printed `boundary-reconcile: STALE`: perform **Step 0** before any code commit - for each stale blocker, read its merged diff (`gh pr diff <n>`) and append a `### <blocker> landed ... - boundary reconcile` landed-facts section to the plan/brief. This is a *different* thing from de-stub reconcile below (hard-serialized dependent vs a stubbed contract). Full procedure + section format: [references/boundary-reconcile.md](references/boundary-reconcile.md).
- **only if** spawned to de-stub a merged blocker: [§0b Reconcile mode](#0b-reconcile-mode---reconcile-manifest).
- **only if** a Claude Plan-Mode plan was just approved (attended): [references/plan-mode-frontdoor.md](references/plan-mode-frontdoor.md).

---

<HARD-GATE>
NEVER edit ~/.fno/graph.json directly via Edit/Write tools or `jq -i`/`sed -i`.
ALWAYS use `fno backlog` commands or call `locked_mutate_graph()` from Python.
Direct edits are blocked by the PreToolUse hook AND detected via hash sidecar.
</HARD-GATE>

<HARD-GATE>
NEVER hand-edit `.fno/target-state.md` after init via Edit/Write/Bash.
The manifest is WRITE-ONCE: `fno target init` writes it at session start; the ONLY legal post-init mutation is first-fill of an empty `plan_path` via `fno state set --field plan_path`. Any other field write exits 5 (ab-d0337fbc).
There are NO gate booleans, NO `current_phase`, NO `status` field, NO `quality_check_passed` or similar. Do not attempt to write them.
</HARD-GATE>

> **Multi-CLI:** If not on Claude Code, see [references/cli-tool-mapping.md](references/cli-tool-mapping.md) for tool equivalents.

Provider parity is hook-driven. The shared state machine and completion gates stay canonical; provider-specific behavior must come from the hooks layer and provider-scoped agent artifacts rather than from forked per-provider pipelines. Gemini's stable baseline is sequential fallback; it may upgrade into experimental project-agent mode only when the workspace explicitly opts in and `.gemini/agents/` is present.

## Gotchas

Environment-specific traps that defy reasonable assumptions. Read these before you hit them.

- **The `fno target start` receipt can lie** (theme 1, x-39c0). Historically it printed `plan: none` for a node that had a `plan_path`, `node=already-claimed` when the claim was free, and `base=origin/main` when the branch was 10-20 commits stale. Verify the three load-bearing lines against source before trusting them: `fno backlog get <id>` (real `status` + any bound plan), `fno claim status node:<id>` (real holder), `git rev-list --count HEAD..origin/main` (real base distance).
- **`fno test`, not bare `pytest`.** `fno test [paths...]` pins the worktree `PYTHONPATH`, bypasses the rtk tee wrapper, and returns the real exit code. Bare `pytest` in a worktree imports the wrong `fno` and can report a false green; `cmd | tail` masks the real `$?`.
- **Read the RESOLVED `auto_merge_approved` from the manifest, never `fno config get auto_merge`.** Init folds config with this run's modifiers, and `/target bg` injects `no-merge` by default; the raw config would tell you to merge against an explicit per-run prohibition.
- **`git checkout -- <file>` destroys uncommitted work** and is NOT stash-recoverable. In a stale-base worktree, `git add -A` can revert an unmerged merge - stage named files, never `-A`.
- **The manifest is write-once.** After `fno target init`, the only legal write is first-filling an empty `plan_path` via `fno state set`. There are no gate booleans or status fields to set; any other field write exits 5.
- **A `bg` worker is unsupervised, NOT headless.** `/target bg` dispatches and continues without blocking, but the worker is not invisible: it registers an agent-view row and keeps an attachable pane, so it stays observable and drivable after launch (`fno agents logs <name>`, or attach). "Fire-and-forget" describes only the dispatching session's non-blocking stance, never the worker's nature.

## Optional: external loop wrapper

For walk-away / overnight execution, drive this skill from a terminal via the external loop wrapper:

```bash
bash scripts/run-target-loop.sh path/to/plan/
```

The wrapper re-invokes the CLI until the session terminates (DonePRGreen, Budget, NoProgress, or Interrupted). The in-Claude-Code interactive experience remains the recommended default for walk-up feature work. Note: the legacy `fno loop` verb is removed (step-5 group 3); `fno-agents loop-check` is the stop authority and `fno-agents loop run` is the loop runtime.

## Completion: what you do

Emit `<promise>MISSION COMPLETE: ...</promise>` when the PR is up and CI is green - **promise early** (one exception: an approved auto-merge merges FIRST, see below), the external reads hold it. An unsatisfied read just blocks-and-retries naming what is missing; a premature promise never short-circuits the gate. **While waiting on an async check with nothing to do, arm ONE watcher and idle on a `<watching>` tag** (the exact protocol is [How to end every turn](#how-to-end-every-turn) below) - never re-read the poller and re-post the same status on a nudge; that is pure noise. **Read the manifest's resolved `auto_merge_approved` before you promise** - merge authority is config-driven, NOT bg-vs-attended, and nothing in the merge path checks attendance:

```bash
sed -n 's/^auto_merge_approved:[[:space:]]*//p' .fno/target-state.md
```

Read that RESOLVED field, never `fno config get auto_merge` directly. Init folds the config together with this run's modifiers, and a per-run `no-merge` - which `/target bg` injects **by default** - sets it false even when `auto_merge.enabled` is true, so the raw config would tell you to merge against an explicit per-run prohibition. `fno pr merge` reads the same field and refuses too, but that is a backstop: decide from the manifest rather than firing the verb and hoping it catches you.

When it is `true` and `auto_merge.require_checks_pass` is satisfied, **MERGE FIRST, THEN promise**: `fno pr merge <n>`, then `fno backlog reconcile` to close the node (a merge from inside a worktree skips the local post-merge step). Order matters - a promise emitted first terminates the loop as `DonePRGreen` the moment CI goes green, so the session is never re-invoked and the merge silently never happens. Config set once IS the standing authorization; re-asking each time re-imposes the step it was configured to delete.

When it is `false` (the default), stop at a **green, reviewed, mergeable PR** and hand the merge to a human (any out-of-band merge also satisfies `done()`). Never write "handing the merge to a human" without having read that field in the same turn. The gate reads `config.review.required_bots`; the loop-check code default is empty `[]` (no review gate, so a fresh install never hangs on an unconfigured bot), and a maintainer sets it explicitly (e.g. `["chatgpt-codex-connector"]`) to require an external pass. Internal `/review` is advisory.

### How to end every turn

The stop hook reads your final message and makes ONE decision from it. There are exactly three clean ways to end a turn, and one to never use. The "Stop hook error: continue working" line you see is simply the hook **blocking** a turn that ended without one of the first three - it is not a failure, it is the block signal, and it never appears when you end cleanly.

1. **Done** -> `<promise>MISSION COMPLETE: <what shipped></promise>`. Emit it as soon as the PR is up and CI is green (promise early). Once a promise is accepted the loop has terminated: do NOT reflexively re-emit it or re-post the same status on later conversational turns - answer what is asked and stop.
2. **Waiting on an async check with nothing to do** (CI pending, or a bot review not yet posted) -> arm ONE **harness-tracked** watcher whose command carries a hard timeout, then end with the tag and NOTHING else:
   - CI: background Bash `gh pr checks <N> --watch & w=$!; (sleep 1800; kill $w 2>/dev/null) & k=$!; wait $w; kill $k 2>/dev/null`
   - review: a review-state poll (a `gh pr view <N> --json reviews` loop, bounded the same way), NOT `gh pr checks --watch` (it exits the instant CI is green, so on a review wait it wakes immediately and re-blocks)
   - then: `<watching reason="ci|review" pr="<N>" timeout="30m">`
   loop-check verifies the wait against external truth and idles the session to ZERO re-invocations until the watcher fires. The watcher MUST be harness-tracked (background Bash / Monitor) - a detached process (`nohup`, `disown`, or a trailing `&` on the task itself) exits without waking anyone and the session idles forever. The `&` inside the command above is a different thing: the task still ends on `wait`, so the harness sees it exit. On wake: if it settled, proceed; if the bound fired and it is still pending, re-arm and re-emit.
   **Never name `timeout` in a watcher.** It is GNU coreutils and is ABSENT on a stock macOS, which ships no `gtimeout` either, so the command dies with `command not found` before `gh` ever runs - the watcher no-ops, nothing ever wakes you, and the session idles forever on a wait that never started. That is the exact failure this section exists to prevent. Bound the wait with shell builtins instead, as above: they are always there, so there is no fallback branch to get wrong.
3. **Still working** -> just take the next action (a tool call). The stop hook only fires when you STOP, so mid-work turns never reach it.

**Never** end a turn with tag-less prose while the mission is incomplete. That is the ONLY thing that blocks - the hook re-invokes you with "continue working" for zero progress. If you have nothing to add and are not done, do not post: arm-and-tag (2) or take the next action (3).

**Residual-turn austerity.** The arm-and-tag turn (and any timeout re-arm) must be near-empty: the tag plus at most one short line. No status recap, no "waiting for it to settle", no restating what you armed. The transcript is the operator's review artifact; the wait machinery's job is to be invisible in it.

Only Claude sessions idle on `<watching>` today; codex/gemini keep the block-every-tick behavior until their daemon waker ships, so on those harnesses ending a wait turn near-silently still costs a nudge, but keep it terse anyway.

**Drain reviews BEFORE you promise - even when `required_bots` is empty.** CI green is NOT "ready to promise": the codex bot posts `COMMENTED` reviews carrying real inline findings while leaving `reviewDecision` EMPTY, so the shortcut "green checks + empty decision = ready" has shipped unaddressed findings. Read every posted review's inline comments (`gh api repos/{owner}/{repo}/pulls/<n>/comments`), at first-post rather than only at green, and run any configured local review gate + `/codex:review` before `<promise>`. The full ordering, cursor-file watch protocol, the three review-gate flavors, and head-pinning are in [references/ship-and-promise.md](references/ship-and-promise.md) - load it when the PR is up.

Run tests with `fno test [paths...]` (pins worktree `PYTHONPATH`, bypasses rtk, returns the real exit code) and read a PR's CI with `fno pr status <n>` (one `green|red|pending|unknown` verdict) - not bare `pytest` or hand-rolled `jq`. To cancel: `touch .fno/.target-cancelled`.

**Preflight before every push (existence-guarded).** When `scripts/ci/preflight.sh` exists, the ship phase and fix loop run it before pushing - a hermetic worktree runs exactly what CI runs (smoke registry + rust legs) so a local green means a green PR, killing the push-wait-red-fix loop. Full run before the first PR push and the settle-green push; `--retry-failed` between fix commits. Skips on `FNO_SKIP_PREFLIGHT=1` or a docs-only diff. Full contract: [references/ship-phase.md](references/ship-phase.md) and the repo-root `docs/preflight.md`.

Completion is decided by `fno-agents loop-check` from external truth (PR + CI + review), not from any file you write - you cannot self-authorize. The full machinery (immutable manifest, stop-hook shim, `done()` read list, fingerprint backstop, `TerminationReason`, degraded modes) is one Read hop away in **[references/completion-model.md](references/completion-model.md)**.

## The full pipeline + phase philosophy

For a from-idea or multi-phase run, the whole phase map (think -> blueprint -> do -> clean -> review -> validate -> docs -> ship -> external) and the compose-don't-hardcode rationale (which skill runs each phase, on which model, and when a phase applies) live in [references/pipeline-and-philosophy.md](references/pipeline-and-philosophy.md). A ready node with a bound plan does not need it - the spine is the happy path. Completion is the world (PR green + reviewed), never a phase checklist.

## Usage

```bash
# Size profiles (primary interface)
/target S "fix the login bug"            # small: do + PR, no ceremony
/target M "add user auth"                # medium: operator + docs + external
/target L "rebuild billing"              # large: everything including adversarial
/target "add user auth"                  # no size = medium (default)

# From existing plan or graph node
/target path/to/plan                     # default size (M)
/target L path/to/plan                   # override: run with full ceremony
/target M ab-9f5a1f8c                    # graph node ID resolves to plan_path

# Execution modes (combinable with sizes)
/target agent "feature"                  # subagent dispatch
/target fork path/to/plans-folder/       # worktree isolation per plan
/target bg ab-A ab-B                     # dispatch-and-continue: ready node(s) as claude --bg /target workers, unsupervised (US5)
/target bg --all-ready                   # dispatch every ready, non-deferred node; planning session keeps going

# Modifiers
/target beastmode <node>                 # walk-away authority: decide judgment calls, never stall
/target beast <node>                     # same thing (also accepts a mobile-autocorrected "beast mode")
/target clean "feature"                  # run /simplify after execute
/target adversarial "feature"            # add adversarial challenge
/target auto-merge "feature"             # auto-merge after external approves
/target no-merge "feature"               # disable auto-merge for this run
/target combo my-stack "feature"         # route via a provider combo (Plan B, ab-0e5a921e)

# Controls
/target --max-iterations 20 "feature"
/target --budget 25 "feature"
/target resume
/target cancel
```

For the full execution-mode comparison, interactive-mode wizard, override-flag table, and context lifecycle (interactive vs unattended), load [references/usage-detail.md](references/usage-detail.md).

| Subcommand | Name | Executor | Ceremony |
|------------|------|----------|----------|
| `S` / `small` | Small | do | Build + PR only |
| `M` / `medium` (or omit) | Medium (default) | operator | + external, docs |
| `L` / `large` | Large | operator | Everything: research, adversarial, browser, clean |

Load [references/size-profiles.md](references/size-profiles.md) for the full capability matrix and [references/flag-migration.md](references/flag-migration.md) for the override-flag list.

For walk-away / overnight execution, run target unattended via the external loop wrapper (`scripts/run-target-loop.sh`); fresh-context restarts are governed by `config.target.restart_after_n_turns`.

## Process

### 0. Cancel / Override

If `cancel` is passed: run the `/fno:cancel-target` command (display current state, drop a session-keyed `.fno/.target-cancelled-final` tombstone, remove `target-state.md`, exit). The tombstone lets the orphan detector allow a clean exit instead of re-blocking (ab-e95531e2). If no state file exists, report "No active session."

If `override <reason>` is passed: the operator-override machinery was removed in the control-plane collapse (no gates to bypass, no status field to flip). Override is no longer a supported subcommand. To close an off-ceremony session manually, touch `.fno/.target-cancelled` (which signals `Interrupted` to the loop-check verb), then run `fno backlog done <node-id>` to mark the backlog node complete. Acknowledge this to the user and stop.

### 0a. Background dispatch (`bg`) and batched mode (`batched`)

**only if** the argument leads with `bg <node...>` / `bg --all-ready` (dispatch ready nodes as unsupervised `claude --bg /target` workers - each keeps an agent-view row and an attachable pane, so "unsupervised" not "headless") or `batched <node>` (a batch-lane member run on a shared branch): neither is a normal pipeline. Load [references/bg-and-batched-modes.md](references/bg-and-batched-modes.md) for the constrained flow before doing anything else.

### 0b. Reconcile mode (`--reconcile <manifest>`)

If ARGUMENTS carry a `--reconcile <manifest-path>` token, this is a **G4 de-stub pass** for a `contract` dependent whose blocker just merged (spawned by `fno backlog advance` / `backlog.reconcile_dispatch`). It is a constrained `/target`: pull main, run the executable drift gate (`fno stub-manifest reconcile-validate`), de-stub + finalize + flip the EXISTING draft PR ready on authorize, or refuse (carveout + draft-held PR comment) on drift/missing-manifest. It never creates a new PR and never merges. Load [references/reconcile-mode.md](references/reconcile-mode.md) for the full contract before proceeding.

### 1-3f. Initialization

The full initialization sequence (load workspace config, codemap, project config, size profile, init state, detect input type, cross-project, Linear, plan validation, domain resolution, discovery gate, checkpoint, kill criteria) lives in [references/init-state.md](references/init-state.md).

Quick summary:
- **HARD-GATE (location), with attended offer:** Before invoking `init-target-state.sh`, consult the shared location verdict (the SAME one `/do`, `/fix`, and the SessionStart heads-up use, so there is no per-skill drift):

  ```bash
  PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/plugin-root" 2>/dev/null || git rev-parse --show-toplevel 2>/dev/null)}}"
  LOC_HELPER="$PLUGIN_ROOT/hooks/helpers/check-impl-location.sh"
  [[ -f "$LOC_HELPER" ]] && bash "$LOC_HELPER" || echo "verdict=ok"
  ```

  If the output carries `verdict=canonical-protected` (you are on the canonical checkout's protected branch, where sibling terminals share `.fno/`):
  - **Attended** (`attended: true`, interactive human present): OFFER, do not just refuse. Ask `On canonical <branch> - create a worktree at ~/conductor/workspaces/<repo>/<slug> and continue there? [Y/n]`. On **y/Enter**: `git worktree add ~/conductor/workspaces/<repo>/<slug> -b feature/<slug>`, `cd` into it, `bash scripts/setup/setup-worktree.sh` (if present), then run init and the rest of the pipeline FROM the new worktree. Derive `<slug>` from the resolved backlog-node slug if one was resolved, else a filesystem-safe slug of the feature/branch; collision-check against existing conductor paths. On **any failure** of `git worktree add` / `setup-worktree.sh`: abort the offer, stay in place, and fall through to the refusal (never leave a half-relocated session). On **n**: fall through to the refusal.
  - **Unattended / headless** (`attended: false`, or no interactive surface): do NOT prompt. Rely on the `init-target-state.sh` refusal backstop. A bg `/target` self-creates its worktree before building, so it inits from inside a worktree and the verdict is `ok`.
  - **Refusal backstop:** `init-target-state.sh` itself refuses a canonical-protected branch and prints the worktree / `git checkout -b feature/<slug>` / `TARGET_LOCATION_OK=main-acknowledged` options. See backlog ab-efcde945.
- **MANDATORY:** Bootstrap the session with `fno target init --input "<original arg>"` (add `--plan-path <path>` for plan inputs). This discoverable verb wraps the canonical `hooks/helpers/init-target-state.sh` (with `TARGET_START=1` + `TARGET_INPUT`/`TARGET_PLAN_PATH`), records the `owner_cwd` worktree binding, and REFUSES to write a stub. Do NOT substitute `fno state init` - it writes an empty stub the stop hook archives (and will redirect you here). If `fno` is unavailable, run `hooks/helpers/init-target-state.sh` directly with `TARGET_START=1` and `TARGET_INPUT` set. On this path you MUST also set `TARGET_BEASTMODE` explicitly - `1` when the invocation carried the `beastmode` / `beast` modifier, empty otherwise. The helper reads the bare env var and cannot tell an explicit grant from one inherited from an ancestor shell or a spawning parent; `fno target init` scrubs that for you, and this path has no such scrub.
- **`fno target init` owns the node claim - do NOT claim it yourself.** Init acquires `node:<id>` via `fno claim` (TTL-anchored to the durable session PID) and records `target_claim_key`/`holder`/`ttl` in the manifest on success. A `note: legacy graph-claim skipped (non-fatal)` line is EXPECTED and is not a failure - the authoritative `fno claim` runs right after it. Never run `fno claim acquire` manually to "fix" it: a claim from a transient shell PID dies instantly and goes `stale`, clobbering init's good claim. To confirm ownership, run `fno claim status node:<id>` and check that the live `holder` equals your own session_id (`fno whoami` prints it). Do NOT trust the `target_claim_*` manifest fields for this: they are an init-time SNAPSHOT and can lie after the supervisor PID is respawned - the live lockfile holder is the only ownership truth (x-ba4b). A `suspect` state (TTL-unexpired but dead pid) still belongs to your session; it is never up for grabs.
- For plan inputs, run `validate-plan.sh` against the plan folder. Skip for single-file (quick) plans which the validator does not understand.
- Resolve domain from CLI flag → plan → settings → `code` default.
- For idea inputs, run the discovery gate before /blueprint.
- Evaluate the plan's `kill_criteria:` block every iteration (see [references/kill-criteria.md](references/kill-criteria.md)).

### 3f-pm. Plan Mode Front Door (Mode 1, Claude Code only)

**only if** a Claude Code native Plan-Mode plan was just approved AND the run is attended: after init and before preflight, back-fill the approved native plan into an executable one. This whole step is a no-op on CLIs without the capture hook, in any unattended / headless run, and whenever no fresh sidecar exists - so `/target` behaves exactly as today there (US4). Full procedure (detection results, backfill adapter, synthesize-validate-confirm): [references/plan-mode-frontdoor.md](references/plan-mode-frontdoor.md).

### 3g. Preflight Check (MANDATORY unless --skip-preflight)

After `init-target-state.sh` completes and before any pipeline phase fires, run the environment preflight:

```bash
if [[ "${TARGET_SKIP_PREFLIGHT:-0}" != "1" ]]; then
  bash "${SKILL_DIR}/scripts/preflight/run-checks.sh" || {
    PREFLIGHT_EXIT=$?
    # Touch the cancel sentinel so the loop-check verb terminates with Interrupted.
    touch .fno/.target-cancelled
    echo ""
    echo "preflight failed: one or more environment checks failed."
    echo "Fix the issues listed above, then re-run /target."
    echo "To skip preflight (not recommended): /target --skip-preflight \"...\""
    echo "<promise>MISSION BLOCKED: preflight failure - fix environment checks then re-run</promise>"
    exit 0
  }
fi
```

**Override:** Pass `--skip-preflight` to bypass (not recommended - blocks surface real problems): `/target --skip-preflight "my feature"`. Even when skipped, the skip is recorded in target-state.md so it's auditable.

**What gets checked:** See [references/preflight-checks.md](references/preflight-checks.md) for the full check catalog (working tree clean, branch state, deps installed, auth valid, disk space, codemap freshness). Checks that produce `warn` or `unknown` do not block - only `fail` status blocks.

### 3h. Phase Handoff Artifacts (best-effort)

**Flat single-file plans skip `ph_write` entirely** - it is scaffolding a one-shot change never misses (G4). For a multi-phase run only, each phase may write a small structured artifact and read the prior phase's, so a transition has a clean handoff without reconstructing context from the full transcript. It is best-effort, never a gate: `loop-check` never reads these, so a missing artifact never blocks completion. The helper, per-phase write/read schema, prior-phase map, and concurrency note are in [references/phase-handoff.md](references/phase-handoff.md) - load it only when the run spans phases.

### 4. Execute Pipeline

**Boundary handoff + cross-project routing.** At a pipeline boundary (blueprint->do, or a wave boundary during do/review) you may hand the rest of the run to a fresh-context successor; and a legacy `cross_project: true` manifest routes to spawn-into-project rather than a (removed) parallel pipeline. Both, plus the `RESULT: BLOCKED` claim-wait stop and the decision-line exit table, are in [references/self-handoff.md](references/self-handoff.md) - load it at a boundary. Never invoke a handoff mid-wave or mid-task.

---

**PHASE EXECUTION PROTOCOL (MANDATORY)**

After completing each phase:
1. **INVOKE the next skill immediately** - do NOT stop between phases

The pipeline runs in order (think -> blueprint/plan -> do -> sigma-review -> validate -> docs -> ship -> external review). Phases are not enforced by gates; completion proof is the world itself (PR green + reviewed). The acceptance-criteria check that runs before `/do waves` is documented in [references/phase-transition-guards.md](references/phase-transition-guards.md).

The phase-routing table, invocation logic, scratchpad writes (after think and after spec), confirmation check (for `confirm: true` skills), Linear status sync, and the validate-phase artifact write live in [references/phase-invocations.md](references/phase-invocations.md) and [references/scratchpad-writes.md](references/scratchpad-writes.md).

#### Postcondition Checking

Per-phase postcondition verifiers were removed in the control-plane collapse (ab-d0337fbc). Postcondition checking collapsed into the external-truth `done()` reads performed by `fno-agents loop-check` when a `<promise>` is seen: PR exists for HEAD + CI green + reviewed. Run sigma-review and validate phases thoroughly; the loop-check verb verifies the outcome against the world, not against state booleans.

#### Atomic Commit Discipline (NON-NEGOTIABLE for M/L)

For Medium and Large size profiles, each completed task or wave MUST produce an atomic commit before moving to the next. Do NOT accumulate all changes into a single kitchen-sink commit at the end of execution.

**Rules:**

1. After completing each task in `fno:do waves`, create a commit scoped to that task's files with a conventional commit message referencing the task
2. If a wave has multiple sequential tasks, commit after each task
3. If a wave has parallel tasks (subagent mode), each agent commits its own work
4. Commit messages follow the project's `commit_style` from config.toml
5. Never `git add .` or `git add -A` - only stage files relevant to the task

**Commit message format:**

```
feat|fix|refactor(scope): what changed

Task {N.M}: {task title from plan}
```

**Why this matters:** Large undifferentiated diffs are hard to review, hard to revert, and break `git bisect`. Atomic commits per task make the review phase more effective and let the user revert individual tasks without losing everything.

**For Small (`-S`) size:** Atomic commits are encouraged but not enforced. A single feature may only have one logical commit, which is fine.

**FORBIDDEN:** Deferring all commits to the end of execution. If you reach the review phase with a single commit covering multiple tasks, that is a failure of commit discipline.

#### Phase Bodies

The clean phase (3.5), review phase (4) deferred-gate semantics, and direction-alignment check (every 2 phases) all live in [references/phase-bodies.md](references/phase-bodies.md).

#### Failure Recovery

When validation fails or the same error fires repeatedly, target has structured recovery: validation-failure recovery (rollback to checkpoint after 3 same-phase failures), circuit breaker (rotate approach after 3 same-error failures), and standard error responses for /do waves and review failures. See [references/failure-recovery.md](references/failure-recovery.md).

#### Secondary Repo Inline Commit

When a plan task touches a secondary repo (e.g., a frontend plan with a backend migration), branch+commit+PR inline before returning to the main repo. See [references/secondary-repo-commit.md](references/secondary-repo-commit.md). For >3 files or meaningful parallel work, model the other repo as its own backlog node (linked by `blocked_by`) and let spawn-into-project dispatch it.

#### Auto-Merge Mechanics

If `auto_merge_approved: true` in target-state.md, Phase 6a runs `rebase-resolve.sh` before `/pr create`, and Phase 8a runs `pr-merge.sh` after `external_review_passed` succeeds. See [references/auto-merge-mechanics.md](references/auto-merge-mechanics.md) and the cross-skill protocol in [references/auto-merge.md](references/auto-merge.md).

### 5. Log Metrics

Append feature-level metrics to `.fno/ledger.json`. Per-wave cost estimates use `scripts/metrics/cost-tracker.sh`. See [references/settings.md](references/settings.md).

### 6. Completion (Pre-Promise Sequence)

The full pre-promise sequence is mandatory and must run in order: calculate session cost → handoff artifact → completion summary → task registry update → plan stamp → plan graduate → cross-project ship recap → promise output. See [references/pre-promise.md](references/pre-promise.md).

The cross-project completion-gate variant (all projects must have status COMPLETE + pr_url + pr_number) and the post-promise behavior contract (STOP IMMEDIATELY, no AskUserQuestion, ignore late notifications) are in the same reference.

## State Files

| File | Purpose | Owner |
|------|---------|-------|
| `.fno/target-state.md` | Immutable session manifest (written once by `fno target init`) | target |
| `.fno/STATE.md` | Wave/task progress | /do waves |
| `.fno/SUMMARY.md` | Task completion notes | archer |
| `.fno/ledger.json` | Feature metrics | target |
| `.fno/events.jsonl` | Loop-check events (loop_check, termination, etc.) | fno-agents loop-check |

## Model Fallback (Interactive)

If the API returns a rate-limit or overload error during execution, present the user with wait/switch/pause options via AskUserQuestion. See [references/model-fallback.md](references/model-fallback.md).

## Resume

```bash
/target resume   # Continue from target-state.md
```

Reads state, skips completed steps, continues from last position. After every resume, re-read `project.vision` and `project.goals` from config.toml — context lost during compaction is reconstructed there. See [references/resume.md](references/resume.md).

## Settings

Configuration lives in `.fno/config.toml` (project-local) with `~/.fno/config.toml` as global fallback. The full schema (project topology + execution defaults + worktree config + autonomous defaults) is in [references/settings.md](references/settings.md).

## References

Loaded by state — the "read X when Y" load conditions are inline above; this is the index.

- [references/beastmode-authority.md](references/beastmode-authority.md) - Walk-away authority grant (load when `authority: full`)
- [references/bg-and-batched-modes.md](references/bg-and-batched-modes.md) - `bg` dispatch-and-continue (unsupervised, still observable) + `batched` member runs
- [references/plan-mode-frontdoor.md](references/plan-mode-frontdoor.md) - Attended Claude Plan-Mode backfill front door
- [references/plan-mode-backfill.md](references/plan-mode-backfill.md) - Backfill adapter mechanics (deeper contract)
- [references/ship-and-promise.md](references/ship-and-promise.md) - Draining reviews + local review gates before `<promise>`
- [references/self-handoff.md](references/self-handoff.md) - Pipeline-boundary handoff + retired cross-project routing
- [references/phase-handoff.md](references/phase-handoff.md) - Best-effort per-phase handoff artifacts (multi-phase only)
- [references/pipeline-and-philosophy.md](references/pipeline-and-philosophy.md) - Full phase map + compose-don't-hardcode rationale
- [references/completion-model.md](references/completion-model.md) - Completion internals: manifest, stop-hook shim, loop-check verb, done() reads, backstop, TerminationReason
- [references/state-schema.md](references/state-schema.md) - Immutable manifest field list and write-once rule
- [references/size-profiles.md](references/size-profiles.md) - Size capability matrix
- [references/flag-migration.md](references/flag-migration.md) - Override-flag list
- [references/usage-detail.md](references/usage-detail.md) - Interactive wizard, execution modes, context lifecycle, model optimization
- [references/init-state.md](references/init-state.md) - Steps 1-3f initialization sequence
- [references/boundary-reconcile.md](references/boundary-reconcile.md) - STALE boundary reconcile procedure
- [references/reconcile-mode.md](references/reconcile-mode.md) - `--reconcile` de-stub pass contract
- [references/phase-transition-guards.md](references/phase-transition-guards.md) - Acceptance criteria gate before /do waves
- [references/phase-invocations.md](references/phase-invocations.md) - Phase routing + invocation logic + Linear sync + validate artifact
- [references/scratchpad-writes.md](references/scratchpad-writes.md) - Cross-phase state files (think findings, plan summary)
- [references/phase-bodies.md](references/phase-bodies.md) - Clean, review, direction-alignment phases
- [references/failure-recovery.md](references/failure-recovery.md) - Validation-failure recovery + circuit breaker + error responses
- [references/secondary-repo-commit.md](references/secondary-repo-commit.md) - Inline secondary-repo commit pattern
- [references/auto-merge-mechanics.md](references/auto-merge-mechanics.md) - Phase 6a + 8a + resolution chain
- [references/pre-promise.md](references/pre-promise.md) - Full pre-promise sequence + promise output
- [references/model-fallback.md](references/model-fallback.md) - Rate-limit handling
- [references/resume.md](references/resume.md) - Resume protocol + project vision re-read
- [references/settings.md](references/settings.md) - config.toml schema + state files + cost tracking
- [references/multi-plan.md](references/multi-plan.md) - Multi-plan worktree mode
- [references/domain-profiles.md](references/domain-profiles.md) - Domain phase resolution
- [references/kill-criteria.md](references/kill-criteria.md) - Kill-criteria predicate syntax
- [references/ship-phase.md](references/ship-phase.md) - Ship-phase exit-42 dispatch loop
- [references/iteration-loop.md](references/iteration-loop.md) - Bounded iteration protocol
- [references/preflight-checks.md](references/preflight-checks.md) - Preflight check catalog
- [references/cli-tool-mapping.md](references/cli-tool-mapping.md) - Multi-CLI tool equivalents
- [references/gate-artifacts.md](references/gate-artifacts.md) - SUPERSEDED: see docs/architecture/control-plane-loop.md
- [references/phase-verifiers.md](references/phase-verifiers.md) - SUPERSEDED: see docs/architecture/control-plane-loop.md
- `docs/architecture/control-plane-loop.md` (repo root) - Post-wedge stop hook architecture: shim + loop-check verb + immutable manifest
