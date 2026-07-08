---
name: target
description: "Use when: build this feature, get it done end-to-end, or execute a plan from idea to PR."
argument-hint: "[S|small|M|medium|L|large] [agent|fork] [clean] [adversarial] [auto-merge | no-merge] [combo <name>] [resume|cancel] [expertise] <ab-xxxxxxxx | feature-description | plan-path> [--max-iterations N] [--budget N] [--no-ship] [--no-external] [--no-docs] [--no-browser]"
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

## The spine (happy path - read this first)

```
resolve node  →  fno target start <node>   worktree off origin/main + claim + init prints the orienter
              →  Step 0 (only if STALE)     orienter says boundary-reconcile: STALE -> read blocker diffs, append landed-facts sections
              →  implement                  edit the plan; atomic commits as you go
              →  /review                     internal sigma panel (cheap insurance)
              →  validate                    fno test  (real exit code; not bare pytest)
              →  /pr create                  Haiku worker opens the PR
              →  <promise>MISSION COMPLETE...  PR green + reviewed = done; you're bg, so hand the merge to a human
```

That is the whole job when a backlog node or plan is already bound. `fno target start` prints an orientation report (node, worktree, tests, done-when) - read it and go. Everything below is detail on a spine step or an **"only if"** branch you skip unless its trigger fires:

**Enter the worktree after the receipt (harness step, do this before implementing).** The `fno target start` receipt names a `worktree:` path and ends with a `cd <path> to continue` line - but a shell `cd` does not persist across tool calls, so prefixing every later command with `cd <worktree> &&` is the failure this step prevents. Instead call the harness **EnterWorktree** tool with `path` set to that receipt worktree line; the session then runs from inside the worktree and every file edit is worktree-relative. This is location-agnostic: any path in `git worktree list` is enterable on first entry, so it works the same for a configured `worktrees_base`, the deprecated conductor base, or the harness-native `.claude/worktrees/` default - never hardcode a base path, read it from the receipt. Two caveats worth knowing: **ExitWorktree never removes a path-entered worktree** (it only returns you to the launch dir; removal is `scripts/setup/archive-worktree.sh`'s job), and after entering, **same-session switches to another worktree are restricted to `.claude/worktrees/`** - irrelevant for a one-node session, surprising only if you try to hop worktrees mid-run.

- **only if** you are already inside a worktree (an attended `/target` in a linked worktree): `fno target start` is a no-op there ("already isolated; nothing created") - run `fno target init --input <node>` (add `--plan-path <path>` for a plan) instead. `start` is the cold-start-from-canonical verb; `init` is what writes the manifest, claims the node, and prints the orienter.
- **only if** you were handed a bare idea (no plan): run `/think` then `/blueprint` before implementing.
- **only if** `.fno/target-state.md` already exists for this session: you are **mid-loop** - re-verify the world and re-emit `<promise>`; do NOT re-init or rebuild.
- **only if** dispatching nodes fire-and-forget: [§0a Background Dispatch](#0a-background-dispatch-bg).
- **only if** the orienter printed `boundary-reconcile: STALE`: perform **Step 0** before any code commit - for each stale blocker, read its merged diff (`gh pr diff <n>`) and append a `### <blocker> landed ... - boundary reconcile` landed-facts section to the plan/brief. This is a *different* thing from de-stub reconcile below (hard-serialized dependent vs a stubbed contract). Full procedure + section format: [references/boundary-reconcile.md](references/boundary-reconcile.md).
- **only if** spawned to de-stub a merged blocker: [§0b Reconcile mode](#0b-reconcile-mode---reconcile-manifest).
- **only if** a Claude Plan-Mode plan was just approved: [§3f-pm Plan Mode Front Door](#3f-pm-plan-mode-front-door-mode-1-claude-code-only).

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

## Optional: external loop wrapper

For walk-away / overnight execution, drive this skill from a terminal via the external loop wrapper:

```bash
bash scripts/run-target-loop.sh path/to/plan/
```

The wrapper re-invokes the CLI until the session terminates (DonePRGreen, Budget, NoProgress, or Interrupted). The in-Claude-Code interactive experience remains the recommended default for walk-up feature work. Note: the legacy `fno loop` verb is removed (step-5 group 3); `fno-agents loop-check` is the stop authority and `fno-agents loop run` is the loop runtime.

## Completion: what you do

Emit `<promise>MISSION COMPLETE: ...</promise>` when the PR is up and CI is green - **promise early**, the external reads hold it. An unsatisfied read just blocks-and-retries naming what is missing; a premature promise never short-circuits the gate. You are a bg/unattended agent, so your terminal state is a **green, reviewed, mergeable PR**, not a merged one: hand the merge to a human (any out-of-band merge also satisfies `done()`). The gate reads `config.review.required_bots`; the loop-check code default is empty `[]` (no review gate, so a fresh install never hangs on an unconfigured bot), and a maintainer sets it explicitly (e.g. `["chatgpt-codex-connector"]`) to require an external pass. Internal `/review` is advisory.

<IMPORTANT>
CI green is NOT "ready to promise". A posted optional-bot review (`gemini-code-assist`, `chatgpt-codex-connector`) is part of the review you must drain BEFORE `<promise>` even when `required_bots` is empty. Do NOT infer "done" from `gh pr checks` + `reviewDecision` alone: both bots post `COMMENTED` reviews that carry real inline findings while leaving `reviewDecision` EMPTY, and a `*-bootstrap` CI check is the bot's setup job, not its review. You MUST list the posted reviews and read every one's inline comments (`gh api repos/{owner}/{repo}/pulls/<n>/comments`), not just its body. That exact shortcut (green checks + empty decision = ready) has shipped unaddressed findings; it is the failure this section exists to prevent.

Drain reviews FIRST - as early as they post, not after CI goes green. A finding read at first-post folds into the fix round already in flight; a finding read only at green forces a fresh push that re-runs CI from scratch (the ping-pong this ordering avoids). CI is meant to be tackled locally ahead of time (hermetic preflight), so the remote CI round-trip is a confirmation, not your feedback loop - for code fixes and review fixes alike.
</IMPORTANT>

**Watch for posted optional reviews WHILE CI is polling - read them at first-post, not only at green.** Every wait round that reads `fno pr status <n>` should also list posted reviews and read any new one immediately, so a real finding folds into the fix round already in flight instead of adding a post-green round. Rides the existing poll cadence - one extra cheap read per round, no new loop, no higher frequency. On each poll: project the review list (`gh pr view <n> --json reviews --jq '.reviews[] | {id, author: (.author.login // "ghost"), submittedAt}'` - the `// "ghost"` fallback keeps the read from crashing on a deleted/ghost author) and compare ids against a cursor file `.fno/scratchpad/pr-<n>-reviews-seen.txt` (the manifest `scratchpad_path`, keyed by PR number, deduped by review id - NEVER the immutable manifest; a missing/malformed cursor is treated as empty, re-reading is harmless). For each NEW id: read it in full - its body (from the review object, e.g. `gh pr view <n> --json reviews --jq '.reviews[] | select(.id=="<id>") | .body'`) AND its inline comments (`gh api repos/{owner}/{repo}/pulls/<n>/comments`, which returns every inline review comment on the PR; that endpoint carries the inline comments only, never the review body, so both reads are needed), because a codex review carries its findings in inline comments under a boilerplate body, so a body-only read silently misses everything - then print ONE line (`new optional review from <bot>: <k> findings`) and classify each finding: *actionable* -> fix it in the current round (the fix rides the next push that was already going out); *stale/inapplicable* -> note why and leave it for the backstop's judgment. A review posted against an EARLIER commit is still read in full and classified against current HEAD (stale placement != stale finding). Quiet by default: a poll round with no new id prints nothing. Degrade, never abort: a failed `gh` read skips the watch that round (CI polling continues); an unwritable cursor proceeds without persisting and warns once. Same never-wait-for-unposted rule as the backstop (a silent or usage-limited bot never blocks - the watch reads only what exists), and the same `--no-external` / config skip.

**Drain a posted optional review before you promise (even when `required_bots` is empty).** An empty gate means external review is not *required*, NOT that it should be *ignored*. Once CI is green, poll the PR head ONCE for an already-posted external review from an optional reviewer (`gemini-code-assist`, `chatgpt-codex-connector`, or any `config.review.peers` login): `gh pr view <n> --json reviews,comments`. **Never gate this on `reviewDecision` or the `gh pr checks` grid alone:** both bots post `COMMENTED` reviews that carry real inline findings while leaving `reviewDecision` EMPTY, and a bot's `*-bootstrap` CI check passing is only its setup job, not its review - an empty decision means "no *blocking formal* verdict", NOT "nothing posted to address". List the reviews themselves and read every `COMMENTED` one's inline comments (`gh api repos/{owner}/{repo}/pulls/<n>/comments`). If one has posted with unaddressed findings, run `/pr check <n>` (or address the findings inline, push, reply) BEFORE emitting `<promise>`. This is a single poll of what is already there - do NOT wait for a review that has not posted (a bot that is silent or usage-limited never blocks; note it and move on). Skips only under `--no-external` / config. This closes the gap where a bot posts real findings after CI goes green and the promise fires without them ever being read.

**Run the configured local review gate before you promise.** The three review-gate flavors compose independently: `config.review.required_bots`/`github_apps` (GitHub App bots, drained above), `config.review.reviewers` (local attestation), and `config.review.peers` (posted CLI review). loop-check already *reads* all three - but the producer side is yours: when `reviewers` or `peers` is non-empty, `/target` must RUN the named reviewers on the **final shipped HEAD** and emit BEFORE `<promise>`, or loop-check requires an attestation nothing produces and the loop hangs with no explanation. Empty config (today's default) makes this whole step a no-op, so a fresh install never blocks. `--no-external` scopes NARROWLY: it skips only *external* posting (`peers`, like the drain of posted bots), NEVER the local `reviewers` attestation - loop-check's `no_external` gates only its GitHub-login reads and still requires `reviewers_all_attested` independently (loopcheck.rs, test `no_external_still_honors_reviewers_gate`), so skipping a configured local reviewer would leave its gate permanently unmet and hang the loop forever (the exact failure this feature exists to prevent). Reuse the existing producers - there is no new gate machinery here, only invocation:

- **`reviewers`** (`fno config get review.reviewers`, a subset of `sigma | code-review | declare`) - ALWAYS run when configured (loop-check requires the attestation regardless of `--no-external`); run each entry and emit its head-pinned `review_attestation`: `sigma` -> `/review sigma` (its Step 6c auto-emits on a clean pass; a blocking finding emits nothing, so fix -> commit -> push -> re-run on the new HEAD); `declare` -> `/review declare` (explicit self-cert emit); `code-review` -> run `/code-review`, then `bash skills/review/scripts/emit-attestation.sh code-review`.
- **`peers`** (`fno config get review.peers`) - external posting, so skipped under `--no-external` / config (loop-check also skips the peers login gate then). Otherwise, for each provider run `/review peer <pr#> <provider> --post`. It posts under `config.review.peer_identity` (a distinct machine account) and gates via `config.review.peers`. If `peer_identity` or its PAT is unset the helper fails loud and the gate stays UNMET - surface that reason and stop, never fake a post that did not happen.
- **Head-pinning is load-bearing.** Emit LAST, on the final HEAD. If any later fix moves HEAD (a peer finding, an external drain), re-run the reviewer and re-emit; loop-check discards a stale-head attestation, so a superseded emit can never falsely clear the gate.
- **Sigma runs once, post-ship.** When `reviewers` includes `sigma`, skip the pre-ship advisory sigma run (see [references/phase-bodies.md](references/phase-bodies.md)) - its attestation would be invalidated by any later fix anyway, so the single gating run is this one. Nits (P3) are advisory and still attest, matching `/review`'s own stance; only an unaddressed P1/P2 holds the gate.

Run tests with `fno test [paths...]` (pins worktree `PYTHONPATH`, bypasses rtk, returns the real exit code) and read a PR's CI with `fno pr status <n>` (one `green|red|pending|unknown` verdict) - not bare `pytest` or hand-rolled `jq`. To cancel: `touch .fno/.target-cancelled`.

Completion is decided by `fno-agents loop-check` from external truth (PR + CI + review), not from any file you write - you cannot self-authorize. The full machinery (immutable manifest, stop-hook shim, `done()` read list, fingerprint backstop, `TerminationReason`, degraded modes) is one Read hop away in **[references/completion-model.md](references/completion-model.md)**.

## The Full Pipeline

```
"I want an AI chat feature"
         │
         ▼
┌─────────────────────────────────────────────────────────────┐
│  /think          → Design thinking, explore problem space   │
│  discovery gate  → Surface unknowns before planning         │
│  /blueprint      → Create implementation plan with waves    │
│  /do waves {expertise} → Execute with TDD (archer agents)    │
│  /simplify       → Remove AI slop patterns (clean modifier)  │
│  /review    → Internal quality gates                   │
│  validate        → Run tests / typecheck / build            │
│  /ship-docs      → Architecture docs + how-to guides        │
│  browser testing → If has_ui, run Chrome DevTools checks    │
│  /pr create      → Create PR (fork to Haiku)                │
│  /pr check       → Wait for external review + implement     │
│  auto-merge      → Optional, only if auto_merge_approved    │
└─────────────────────────────────────────────────────────────┘
         │
         ▼
   PR ready for merge (docs + browser verification included)
```

Docs and browser testing run BEFORE `/pr create` so they ride in the same PR, get reviewed alongside the code, and are included in any auto-merge. Historic versions of this skill ran docs last, which led to docs landing in a follow-up PR whenever `auto_merge_approved: true` tripped immediately after external review.

## Philosophy

**Compose, don't hardcode.** This skill orchestrates other skills:

| Phase | Skill Used | Purpose | When to Run | Model |
|-------|------------|---------|-------------|-------|
| Think | `/think` | Design exploration | If starting from idea | Opus (inline) |
| Plan | `/blueprint` | Create waves + tasks | If no plan exists | Opus (inline) |
| Execute |`/do waves` | Wave orchestration + TDD | Always | Opus (inline) |
| Clean | `/simplify` | Remove AI slop patterns | Only with `clean` modifier | Opus (inline) |
| Review | `/review` | Internal quality gates (BEFORE push) | Always | Opus (inline) |
| Validate | _(bash)_ | npm run build / pytest | Always | Opus (inline) |
| Docs | `/ship-docs` | Architecture + how-to in parallel | Default YES, skip with `--no-docs` or config - runs BEFORE ship so docs ride in the same PR | **Sonnet** (agents) |
| Browser | `/tdd` (browser-testing ref) | Human-like UI checks (advisory: runs and logs, never gates `<promise>`) | If `has_ui` - runs BEFORE ship | Sonnet (agent) |
| Ship | /pr create | PR creation (fresh agent) | Always | **Haiku** (agent) |
| External | `/pr check` | Wait for external review + implement | Default YES, skip with `--no-external` or config | Sonnet (review response), Opus (code fixes) |
| Auto-merge | `${SKILL_DIR}/scripts/lib/pr-merge.sh` | Merge after external approves | If `auto_merge_approved: true` | n/a (shell) |

See [references/usage-detail.md](references/usage-detail.md) for model-optimization rationale (when to keep Opus inline vs spawn cheaper agents).

**Phase applicability is judgment, not a gate.** Every phase above is available; run the ones the work needs. User skip flags (CLI) and project config (`.fno/config.toml`) still force-skip. Otherwise judge by what the change is:

- **/think + /blueprint**: only if you started from a bare idea. A bound node or plan skips straight to implement.
- **/do waves**: for a multi-task plan with parallelizable waves. A single-file or locked refactor runs **inline**, not through the wave orchestrator.
- **/simplify (clean)**: only with the `clean` modifier, or on AI-slop-prone new code.
- **/review**: run it; it is cheap insurance. For a tiny prose/config change a light self-review is enough.
- **/ship-docs**: skip for an internal refactor with no public API or architecture change; run it when behavior or a public surface changed.
- **browser testing**: only if `has_ui`.
- **/pr create + `<promise>`**: always. That is the deliverable.

When unsure whether ceremony applies, prefer running it. But never let "did every phase fire?" gate the promise - completion is the world (PR green + reviewed), not a phase checklist.

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
/target bg ab-A ab-B                     # fire-and-forget: dispatch ready node(s) as claude --bg /target workers (US5)
/target bg --all-ready                   # dispatch every ready, non-deferred node; planning session keeps going

# Modifiers
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

### 0a. Background Dispatch (`bg`)

If `bg <node...>` or `bg --all-ready` is passed (US5 targeted bg-dispatch): this is a **fire-and-forget** dispatch of one or more `ready` backlog nodes as fresh `claude --bg` `/target` workers. Do NOT init a pipeline, do NOT resume, do NOT write `target-state.md` — the planning session stays usable for more `/think` + `/blueprint` while the dispatched workers run on their own (the fresh bg process IS the context "clear"; the agent cannot `/clear` itself). Run the dispatch primitive, relay its per-node outcome lines verbatim with the `fno agents logs <name>` hints, then STOP:

```bash
bash "${SKILL_DIR}/scripts/dispatch-node.sh" <node...|--all-ready> [--flags "<size/modifiers>"] [--allow-merge] [--max N]
```

Each line is one of `launched` / `already-running` / `parked` / `skipped-done` / `failed` / `deferred-cap`, followed by a `summary:` line; never silent. Locked semantics:
- Under `--all-ready` only `ready` nodes dispatch. An **explicitly-named** node also dispatches when its status is `idea` (the triage pile; naming it is the human's vet, the worker runs think->blueprint->do); `blocked`/`deferred` are always **parked** (pre-planned future work), never launched. A node a live worker already holds (`node:<id>` claim) is **already-running**, never double-dispatched.
- Each worker launches via `fno agents spawn --provider claude --substrate bg` (the detached `claude --bg` thread; Group 1 ab-8b3e4fe0 moved creation off `ask`), NEVER `--bare`/`-p` (subscription lane only). The `--substrate bg` key is load-bearing: the post-x-3ab8 default substrate is `pane` (owned-PTY), which would stall a fire-and-forget dispatch at a placement prompt (x-2c27).
- `no-merge` is injected by default (an autonomous worker lands a PR for review, not an auto-merge); pass `--allow-merge` to opt out.
- A dispatch failure is surfaced and leaves the node `ready`/re-dispatchable; it never reports a launch that did not happen, and never falls back to `-p`/API-credit billing.

Multi-CLI: `bg` requires `claude --bg` + `fno agents`; on a CLI without them the dispatch reports the failure and the node stays `ready` (degrade, never fake a launch).

**Spawn substrate axis (x-2c27).** `fno agents spawn --substrate <pane|bg|headless>` names one axis - where an off-thread `/target` runs: `pane` (owned-PTY drivable pane; the default), `bg` (detached `claude --bg` thread; what `/target bg` dispatches), `headless` (a one-shot `claude -p` / `codex --exec` / `agy -p`). `/target bg` is the autonomous-dispatch verb because only `bg` is both detached AND able to run the full multi-phase pipeline. For an attended drivable pane use `/agent spawn /target <node>` (substrate `pane`); `headless` (one-shot) does not fit a multi-phase `/target` run and is a `/agent spawn headless` surface for cross-provider one-shots, not a `/target` dispatch verb. `bg` is claude-only; a non-claude `bg` is a hard error pointing to `headless`.

### 0b. Reconcile mode (`--reconcile <manifest>`)

If ARGUMENTS carry a `--reconcile <manifest-path>` token, this is a **G4 de-stub pass** for a `contract` dependent whose blocker just merged (spawned by `fno backlog advance` / `backlog.reconcile_dispatch`). It is a constrained `/target`: pull main, run the executable drift gate (`fno stub-manifest reconcile-validate`), de-stub + finalize + flip the EXISTING draft PR ready on authorize, or refuse (carveout + draft-held PR comment) on drift/missing-manifest. It never creates a new PR and never merges. Load [references/reconcile-mode.md](references/reconcile-mode.md) for the full contract before proceeding.

### 0c. Batched mode (`batched <node>`)

If ARGUMENTS lead with `batched <node>` (dispatched by the active-backlog daemon when `config.batch.enabled` is on and the batch policy says join-or-start), this is a **batch-lane member run** (x-6cdf). The member's commits land on a **shared batch branch** and ship via ONE batch PR, not a per-node PR. It is a constrained `/target`:

1. **Skip cold-start.** The daemon already created/selected the batch worktree and passed `TARGET_BATCH_WORKTREE` + `TARGET_BATCH_BRANCH`. Do NOT run `fno target start` (that would mint a second worktree). Instead `cd "$TARGET_BATCH_WORKTREE"` and run `TARGET_BATCHED=1 fno target init --input <node>` so the manifest carries `batched: true` (loop-check reads it to terminate as `DoneBatched`, not a hang). Do NOT set `no_ship`/advisory - a batched unit is neither.
2. **Implement + commit atomically to the shared branch.** Same per-task commit discipline as a normal run; the commits accumulate on `$TARGET_BATCH_BRANCH` alongside the batch's other members. Run `/review` + `fno test` as usual.
3. **Join the batch + mark the node.** After the work is committed: `fno backlog batch join --domain <domain> --node <node> --summary "<one-line>"` (adds the member to the batch state), then read the batch id (`fno backlog batch status --domain <domain>` -> `batch_id`) and `fno backlog update <node> --batch <batch_id>` (marks the graph node so `next`/`ready` stop selecting it). Mark the node **before** promising.
4. **Do NOT create a PR.** Batched mode skips the ship phase entirely - the batch's single `/pr create` (driven by `fno backlog batch ship` when the batch closes) opens one PR for all members. Emit `<promise>MISSION COMPLETE: batched member committed to <branch></promise>`; loop-check sees `batched: true` + the promise and terminates the member as `DoneBatched`.

The daemon consults `should_close` after the member returns and, when the batch closes (full / next node is a different domain / drain), runs `fno backlog batch ship` to open the batch PR and record the shared PR ref on every member. On merge, `fno backlog reconcile` closes each member by that shared pr_number. If a batched member FAILS/BLOCKS, the daemon abandons the batch (`fno backlog batch ship` requeues members as individual PRs). v1 has no revert surgery - a bad batch costs at most the same CI as no batching.

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
- **MANDATORY:** Bootstrap the session with `fno target init --input "<original arg>"` (add `--plan-path <path>` for plan inputs). This discoverable verb wraps the canonical `hooks/helpers/init-target-state.sh` (with `TARGET_START=1` + `TARGET_INPUT`/`TARGET_PLAN_PATH`), records the `owner_cwd` worktree binding, and REFUSES to write a stub. Do NOT substitute `fno state init` - it writes an empty stub the stop hook archives (and will redirect you here). If `fno` is unavailable, run `hooks/helpers/init-target-state.sh` directly with `TARGET_START=1` and `TARGET_INPUT` set.
- **`fno target init` owns the node claim - do NOT claim it yourself.** Init acquires `node:<id>` via `fno claim` (TTL-anchored to the durable session PID) and records `target_claim_key`/`holder`/`ttl` in the manifest on success. A `note: legacy graph-claim skipped (non-fatal)` line is EXPECTED and is not a failure - the authoritative `fno claim` runs right after it. Never run `fno claim acquire` manually to "fix" it: a claim from a transient shell PID dies instantly and goes `stale`, clobbering init's good claim. To confirm ownership, run `fno claim status node:<id>` and check that the live `holder` equals your own session_id (`fno whoami` prints it). Do NOT trust the `target_claim_*` manifest fields for this: they are an init-time SNAPSHOT and can lie after the supervisor PID is respawned - the live lockfile holder is the only ownership truth (x-ba4b). A `suspect` state (TTL-unexpired but dead pid) still belongs to your session; it is never up for grabs.
- For plan inputs, run `validate-plan.sh` against the plan folder. Skip for single-file (quick) plans which the validator does not understand.
- Resolve domain from CLI flag → plan → settings → `code` default.
- For idea inputs, run the discovery gate before /blueprint.
- Evaluate the plan's `kill_criteria:` block every iteration (see [references/kill-criteria.md](references/kill-criteria.md)).

### 3f-pm. Plan Mode Front Door (Mode 1, Claude Code only)

After init (which session-start-wipes a stale sidecar) and before preflight,
check for a plan approved in Claude Code's native Plan Mode. This whole step is
a no-op on CLIs without the capture hook and whenever no fresh sidecar exists,
so `/target` behaves exactly as today there (US4). Full contract:
[references/plan-mode-backfill.md](references/plan-mode-backfill.md).

**Attended-only.** SKIP this entire step in any unattended / headless run -
megawalk-spawned workers (e.g. the bundled `archer` agent), `--unattended`,
`config.unattended.enabled`, or any context with no interactive human. The
front door requires a human confirm (`[y/N]`), so a headless run must NOT
detect, backfill, or consume a sidecar (Open Question 2: headless never
consumes). Those runs already carry their own plan/backlog node, so there is
nothing to detect anyway.

Run detection (pass the user's explicit argument, if any, so precedence is decided):

```bash
DET="$(bash "${SKILL_DIR}/scripts/detect-pending-plan.sh" detect --arg "${ORIGINAL_ARG:-}")"
RESULT="$(printf '%s\n' "$DET" | sed -n 's/^result=//p')"
```

Branch on `$RESULT`:

- `none` / `expired` / `malformed` -> proceed with normal `/target` behavior. For
  `superseded_by_arg`/`malformed`/`expired` the sidecar was logged, not fatal.
- `superseded_by_arg` -> the user gave an explicit argument AND a fresh sidecar
  exists. The explicit argument WINS (US3). Print exactly once, then proceed with
  the argument (the sidecar stays `pending`, re-offerable):
  > "a pending approved plan also exists; ignored in favor of your argument. Run bare /target to use it."
- `pending` -> run the **backfill adapter**, then confirm:
  1. Extract the native body: `bash "${SKILL_DIR}/scripts/detect-pending-plan.sh" body "$STAGE/native.md"`.
  2. Skeleton: `bash "${SKILL_DIR}/scripts/backfill-plan.sh" skeleton "$STAGE/native.md" "$STAGE/enriched.md"` (stage under `.fno/`, e.g. `.fno/.pending-plan.enriched.md`). Read its `has_failure_modes` / `has_acceptance_criteria` report.
  3. **Synthesize (LLM step, the one new piece of reasoning):** if a section is reported absent, append it to the enriched doc from the native plan's intent - a `## Failure Modes` section with the four bold sub-labels `**Boundaries**` / `**Errors**` / `**Invariants**` / `**Concurrency**`, and a `## Acceptance Criteria` section with all 5 BDD types (`#### AC1-HP:` / `AC1-ERR:` / `AC1-UI:` / `AC1-EDGE:` / `AC1-FR:`). A section reported present is REUSED, never duplicated (AC2-EDGE). Preserve the native body verbatim - ADD sections only (AC2-FR).
  4. Validate + bounded retry: `bash "${SKILL_DIR}/scripts/backfill-plan.sh" check-sections "$STAGE/enriched.md"`. If it lists `missing:` items, re-synthesize ONLY those sections and re-check. **Max 2 attempts.** On persistent failure, print the partial doc path and STOP - do NOT enter the autonomous loop on a half-built plan (AC1-ERR / AC2-ERR).
  5. Blueprint: invoke `/blueprint "$STAGE/enriched.md"` (Skill) to append Execution Strategy + File Ownership Map + kill_criteria and set `status: ready`. A `/blueprint` failure surfaces and stops (AC1-ERR).
  6. Show the diff + confirm: `bash "${SKILL_DIR}/scripts/backfill-plan.sh" render-diff "$STAGE/native.md" "$STAGE/enriched.md"`, then ask the user **"Execute autonomously? [y/N]"** (no auto-proceeding default - AC1-UI).
     - **y** -> `bash "${SKILL_DIR}/scripts/detect-pending-plan.sh" consume --holder "target-session:$SESSION_ID"`. If consume exits 3 (already consumed / claimed by a racing run), STOP - another `/target` owns this plan (Concurrency). On success, move/keep the enriched doc as the canonical plan, set it as `plan_path`, and continue into the normal do -> sigma-review -> ship loop.
     - **N** -> leave the sidecar `pending` (do NOT consume). Print "Kept. Run bare `/target` again to use it." and stop (AC1-FR).

Consume ONLY after confirm-yes (Invariant: a declined confirm leaves the plan re-offerable; one authoritative plan per run - explicit argument XOR sidecar).

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

**Override:** Pass `--skip-preflight` to bypass (not recommended - blocks surface real problems):

```bash
/target --skip-preflight "my feature"
```

Even when skipped, the skip is recorded in target-state.md so it's auditable.

**What gets checked:** See [references/preflight-checks.md](references/preflight-checks.md) for the full check catalog (working tree clean, branch state, deps installed, auth valid, disk space, codemap freshness). Checks that produce `warn` or `unknown` do not block - only `fail` status blocks.

### 3h. Phase Handoff Artifacts (best-effort)

Best-effort, not a gate: `loop-check` never reads these, so a missing artifact
never blocks completion. They are a convenience - each phase writes a small
structured artifact at the end of its work and reads the prior phase's at the
start, so a pipeline transition has a clean handoff without the next phase
reconstructing context from the full session transcript. Write them when the run
spans multiple phases; skip them for a short single-phase change.

**Source the helper at the start of each phase:**

```bash
source "${CLAUDE_PLUGIN_ROOT:-$(git rev-parse --show-toplevel)}/scripts/lib/phase-handoff.sh"
```

**Prior-phase read (at phase start, after init):**

```bash
# Each phase reads the immediately preceding phase's artifact.
# If no prior artifact exists, proceed with reduced context - do NOT block.
PRIOR=$(ph_read <prior-phase> "$SESSION_ID" 2>/dev/null || echo "")
if [[ -n "$PRIOR" ]]; then
  echo "handoff loaded from <prior-phase>: $(echo "$PRIOR" | head -3)" >&2
else
  echo "no prior handoff from <prior-phase> - proceeding with reduced context" >&2
fi
```

Prior-phase mapping (fixed by pipeline order):

| Current phase | Reads artifact from |
|---------------|---------------------|
| plan | think |
| do | plan |
| clean | do |
| review | clean |
| validate | review |
| docs | validate |
| ship | docs |
| external | ship |

The `think` phase has no prior and skips the read.

**Artifact write (at phase end, before yielding to next phase):**

```bash
# think phase example
ph_write think "$SESSION_ID" "$(cat <<EOF
design_docs_produced: [${THINK_DOCS:-}]
key_decisions:
  - "${KEY_DECISION_1:-}"
open_questions: [${OPEN_QUESTIONS:-}]
EOF
)"

# plan phase example
ph_write plan "$SESSION_ID" "$(cat <<EOF
plan_path: ${PLAN_PATH:-}
phases_planned: ${PHASES_PLANNED:-}
scope_classification: ${SCOPE:-feature}
EOF
)"

# do phase example
ph_write do "$SESSION_ID" "$(cat <<EOF
stories_completed: [${DONE_IDS:-}]
files_changed: $(git diff --name-only HEAD 2>/dev/null | jq -R . | jq -s . 2>/dev/null || echo "[]")
notes_for_next_phase: |
  ${NOTES_FOR_NEXT:-}
EOF
)"

# clean phase example
ph_write clean "$SESSION_ID" "$(cat <<EOF
files_simplified: [${SIMPLIFIED:-}]
patterns_removed: [${PATTERNS:-}]
notes_for_review: |
  ${CLEAN_NOTES:-}
EOF
)"

# review phase - extends the existing gate artifact; write handoff with same session
ph_write review "$SESSION_ID" "$(cat <<EOF
sigma_review_artifact_path: .fno/artifacts/review-${SESSION_ID}.md
blocking_issues: [${BLOCKING:-}]
advisory_notes: [${ADVISORY:-}]
EOF
)" 2>/dev/null || true  # gate artifact already written by sigma-review; handoff is supplemental

# validate phase example
ph_write validate "$SESSION_ID" "$(cat <<EOF
build_command: ${BUILD_CMD:-}
test_command: ${TEST_CMD:-}
output_summary: "${VALIDATE_SUMMARY:-}"
exit_codes:
  build: ${BUILD_EXIT:-0}
  test: ${TEST_EXIT:-0}
EOF
)"

# ship phase example
ph_write ship "$SESSION_ID" "$(cat <<EOF
pr_number: ${PR_NUMBER:-}
pr_url: ${PR_URL:-}
branch_name: ${BRANCH:-}
base_branch: ${BASE_BRANCH:-main}
EOF
)"

# external phase example
ph_write external "$SESSION_ID" "$(cat <<EOF
review_status: ${EXT_STATUS:-}
blocking_comments: [${BLOCKING_COMMENTS:-}]
approval_state: ${APPROVAL_STATE:-}
EOF
)"

# docs phase example
ph_write docs "$SESSION_ID" "$(cat <<EOF
docs_updated: [${DOCS_UPDATED:-}]
sections_added: [${SECTIONS_ADDED:-}]
EOF
)"
```

Artifacts are written to `.fno/artifacts/handoff/{phase}-{session_id}.md`.
The `handoff/` subdirectory namespaces away from gate-attestation artifacts owned
by /review, /pr create, /pr check, etc.

**Concurrency safety:** two target runs in different worktrees use different
`session_id` values so artifact files never collide even when they share the
same project directory.

See [references/phase-artifacts.md](references/phase-artifacts.md) for the full
per-phase schema and the complete size invariant (500-token cap).

### 4. Execute Pipeline

#### CROSS-PROJECT IS RETIRED (migration shim)

The `scope: cross-project` parallel-worktree pipeline has been removed. A
session works only in its OWN project; foreign work is spawned into its
project via `fno agents spawn --cwd <root>` (spawn-into-project). A multi-repo
feature is now a set of single-project backlog nodes linked by `blocked_by`,
each shipping its own PR in its own repo.

**Check target-state.md BEFORE any execution.** If `cross_project: true`
(a legacy `cross-project` subcommand, or a plan with `scope: cross-project`):

1. **WARN** the user: "scope: cross-project is deprecated and the parallel pipeline was removed. Model multi-repo work as one backlog node per project (linked by blocked_by); each ships its own PR. Use `fno backlog decompose` to split a legacy plan."
2. **Do NOT** invoke any cross-project pipeline (removed) and **do NOT** `cd` into other repos to write code.
3. **Route to spawn-into-project:** continue THIS session in its own project only. Foreign waves are handled by `/do` (auto-spawn when the foreign node is unblocked; defer + carveout when it is blocked); cross-project dependents are dispatched on merge by `fno backlog advance`.

`cross_project: true` no longer forks the pipeline; it only triggers this
deprecation warning + the spawn-into-project routing above. The manifest
field and the plan-graduation timing in `fno-agents finalize` are retained
so an already-stamped legacy plan still parses and graduates correctly.

#### Self-Handoff at Pipeline Boundaries (ab-534bcc55)

Session succession hands the rest of the pipeline to a fresh-context worker via `bash "${SKILL_DIR}/scripts/handoff.sh"`. The helper performs all state mutations atomically; the LLM invokes it and obeys the decision line.

**Claim-wait BLOCKED:** If `fno target init` (or `init-target-state.sh`) output contains `RESULT: BLOCKED`, the session MUST stop immediately. Relay the block contract as your final output (`REASON: ...` / `UNBLOCKS_AFTER: ...`). Do NOT run any pipeline phases without a live claim.

**blueprint->do boundary (structural)**

- **Unattended** (`attended: false` in target-state.md): run `bash "${SKILL_DIR}/scripts/handoff.sh" --boundary blueprint-do` automatically.
- **Attended** (`attended: true`): ask exactly `Plan ready - dispatch fresh worker for the build? [Y/n]` (one line, no preamble). `y` or Enter -> run the helper. `n` -> park (continue in-session, no claim churn, no spawn). If the question cannot be answered (timeout or no interactive surface) -> park conservatively and continue in-session.

**Wave/phase boundaries during do + review (pressure)**

Run `bash "${SKILL_DIR}/scripts/handoff.sh" --boundary wave` at each wave boundary. The helper parks on no-pressure (probe reads < `config.target.handoff.used_pct_trigger`, default 50); always invoke it and obey the decision line. Never invoke mid-wave or mid-task.

**Decision-line handling**

| Exit | Decision line prefix | Action |
|------|---------------------|--------|
| 0 | `delegated <node> ...` | Print `result: do-phase delegated to <child> (<session>)`. Then **stop immediately** - do NOT continue pipeline phases, do NOT run `claude stop`. The parent's close is sanctioned; the stop hook allows exit because the manifest was archived. |
| 10 | `parked <node> reason="..."` | Continue in-session exactly as if no handoff was attempted. If the reason contains `chain-exhausted`, emit `<help reason="handoff-chain-exhausted" evidence="<reason>">` first, then continue. |
| 12 | `handoff-restore-failed <node> ...` | Emit `<help reason="handoff-restore-failed" evidence="<reason>">` and stop work. Never continue silently without a manifest. |
| 12 | `handoff-claim-lost <node> ...` | Emit `<help reason="handoff-claim-lost" evidence="<reason>">` and stop work. The claim may be held by another worker; do NOT continue in-session on this node. |

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

- [references/completion-model.md](references/completion-model.md) - Completion internals: manifest, stop-hook shim, loop-check verb, done() reads, backstop, TerminationReason
- [references/state-schema.md](references/state-schema.md) - Immutable manifest field list and write-once rule
- [references/size-profiles.md](references/size-profiles.md) - Size capability matrix
- [references/flag-migration.md](references/flag-migration.md) - Override-flag list
- [references/usage-detail.md](references/usage-detail.md) - Interactive wizard, execution modes, context lifecycle, model optimization
- [references/init-state.md](references/init-state.md) - Steps 1-3f initialization sequence
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
- [references/gate-artifacts.md](references/gate-artifacts.md) - SUPERSEDED: see docs/architecture/control-plane-loop.md
- [references/phase-verifiers.md](references/phase-verifiers.md) - SUPERSEDED: see docs/architecture/control-plane-loop.md
- [references/kill-criteria.md](references/kill-criteria.md) - Kill-criteria predicate syntax
- [references/ship-phase.md](references/ship-phase.md) - Ship-phase exit-42 dispatch loop
- [references/iteration-loop.md](references/iteration-loop.md) - Bounded iteration protocol
- `docs/architecture/control-plane-loop.md` (repo root) - Post-wedge stop hook architecture: shim + loop-check verb + immutable manifest
