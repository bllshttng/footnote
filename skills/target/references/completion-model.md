# Completion Model (internals)

The machinery behind the one paragraph in `SKILL.md`'s "Completion: what you do". You rarely need this; read it only when debugging why a `<promise>` did or did not terminate the loop. The actionable contract (promise early, bg hands off the merge, `fno test`, `fno pr status`) lives in the skill body.

## The immutable manifest

`target-state.md` is written once by `fno target init` and is never modified afterward. It is an input manifest: session_id, created_at, input, plan_path, target_size, skip flags (no_external, no_docs, no_ship, no_browser, no_clean, no_how_to, no_memory, no_deferrals_capture), has_ui, attended, advisory, budget caps, provider, owner binding fields, claude_transcript_id, cross_project, scratchpad_path, and auto_merge fields. The only legal post-init write is first-fill of an empty `plan_path` via `fno state set --field plan_path` (any other field write exits 5). There is no `status`, no `current_phase`, no `iteration`, no `blocked_reason`, no gate booleans, no provenance nonce.

## The stop hook shim

`hooks/target-stop-hook.sh` is a 118-line read-only shim. It resolves the `fno-agents` binary in order: `$FNO_AGENTS_BIN`, repo `target/release`, repo `target/debug`, then PATH. When the binary is missing the shim emits a `loop_check_binary_missing` event to both `.fno/events.jsonl` and `~/.fno/events.jsonl` and allows exit (session must be re-spawned once the binary is available). The shim sources no `scripts/lib/*.sh` files and makes no decisions itself.

## The loop-check verb

The shim delegates all stop/allow logic to `fno-agents loop-check`. Output is one JSON object: `{decision, termination_reason, message, fires, fingerprint}`. Exit 0 for both allow and block; exit 2 for CLI misuse only.

**Decision algorithm:**

1. If `<aborted reason="...">` appears in transcript output: terminate with `Aborted`.
2. If `<promise>MISSION COMPLETE: ...</promise>` appears: run the `done()` read. If the read passes, terminate with the appropriate `TerminationReason`. If an external read fails, block with the failing read named; the session continues.
3. If no promise: run the backstop check.

**`done()` reads** (what must be true when a promise is seen):

- PR exists for HEAD commit and CI is green. When `no_ship: true`, this is skipped.
- Every bot in `config.review.required_bots` has at least one completed review pass (default `["chatgpt-codex-connector"]`; explicit `[]` declares the no-review-gate path - PR + CI carry the gate). When `no_external: true` in the manifest, the review reads are skipped (step 2, ab-f1c5a9ed).
- No unaddressed blocking inline finding (codex P1 / gemini critical|high on `/pulls/N/comments`). A finding is addressed when its thread has a non-bot reply AND (a fix commit landed after it OR the reply carries `wontfix:`). `/pr check` Step 8a is the matching writer.
- CI is green on the PR. When `ci.declared_none: true` in settings, the CI read is skipped (the project declared CI is not applicable).
- A promise with an unsatisfied read blocks with the failing read named (missing bot or finding path:line); the loop continues until the world catches up.

**Backstop:** A 4-component fingerprint (`HEAD sha | PR state | CI conclusion | latest review/comment ts`) unchanged across N consecutive fires (3 unattended / 5 attended) terminates with `NoProgress`. A "done but mute" session (all reads pass but no promise) resolves as a late `DonePRGreen`. Budget cap (`config.budget` nested, or flat `budget_cap` fallback) terminates with `Budget`. gh-errored fires are transparent to the streak (an outage freezes it; budget is the sole ceiling), while a no-PR fire counts as real world-state.

**TerminationReason enum:** `DonePRGreen | DoneAdvisory | NoWork | Budget | NoProgress | Interrupted | Aborted`

Events land in both `.fno/events.jsonl` and `~/.fno/events.jsonl` with envelope `{ts, type, source:"hook", data}`. Event kinds: `loop_check`, `termination`, `loop_check_gh_error`, `loop_advisory_mode`, `loop_check_binary_missing`, `loop_check_legacy_manifest`.

**Degraded modes:**

- (a) No `gh` binary: advisory mode only. `promise` -> `DoneAdvisory`; `aborted` -> `Aborted`; unattended refuses. Cancel sentinel `.fno/.target-cancelled` -> `Interrupted`.
- (b) Transient gh failure during a `done()` read: the affected read fails closed (emits `loop_check_gh_error`, blocks with the failing read named). The loop retries on the next stop-hook fire; the backstop clock keeps ticking.

**Back-compat:** Legacy manifests (have a `status:` key, pre-wedge) trigger allow-exit immediately; a `loop_check_legacy_manifest` event is emitted and no `done()` or backstop runs. This lets old workflows exit cleanly without a full session restart.

**Advisory units** (`no_ship: true` or `advisory: true` in manifest): promise + budget only. No gh machine needed.

## The Golden Rule

The loop continues until `<promise>MISSION COMPLETE: ...</promise>` AND the world agrees (PR green + reviewed), or a backstop, budget cap, or cancel sentinel terminates it. Emit `<promise>` only when the pipeline is genuinely done. The loop-check verb decides; you cannot self-authorize completion by writing to any file.

To cancel: `touch .fno/.target-cancelled` (or invoke `/target cancel`). The shim detects the sentinel and terminates with `Interrupted`.

## bg terminal state, promise timing, and the review gate (x-8b64)

- **A bg/unattended agent cannot merge** (the two-factor merge guard refuses an unattended actor). Its terminal state is a **green, reviewed, mergeable PR**, not a merged one. Emit `<promise>MISSION COMPLETE: ...</promise>` once the PR is green and reviewed, then hand the merge off to a human. **Any merge path counts as done:** a PR merged out-of-band (web/mobile UI, or `gh pr merge`) satisfies `done()` as `DonePRGreen` on the next stop-hook fire - the loop stops re-poking a finished session.
- **Promise early; the external reads hold it.** You do not need to wait for the external review to pass before emitting `<promise>`. Emit it when the work is shipped (PR up, CI green); if the required bot has not reviewed yet, the `done()` reads simply block-and-retry (naming the missing bot) until the world catches up. A premature promise is safe - it never short-circuits the gate.
- **The gate reads `config.review.required_bots`, NOT `external_reviewers`.** Internal sigma-review is advisory; the external required bot is the gate. Read the current value with `fno config get review.required_bots` (the bare key works; the `config.` prefix is optional). An empty list declares the no-review-gate path (PR + CI carry the gate).

## Running tests / reading CI here (x-8b64)

- **Run the Python suite with `fno test [paths...]`**, not a bare `pytest`. It pins `PYTHONPATH` to the worktree source (a bare `pytest` in a worktree imports the *canonical* `fno`), bypasses rtk (a bare `pytest`/`cargo` can stall for minutes under rtk), and returns pytest's **real exit code** (no `... | tail && echo OK`, which masks failures into a false green). For `cargo`, prefix the run with `RTK_DISABLED=1` to take the same scoped rtk bypass.
- **Read a PR's CI verdict with `fno pr status <n>`**, not hand-rolled `jq` over `statusCheckRollup` or `gh pr checks` (which disagrees with the rollup). It prints one JSON verdict (`green|red|pending|unknown`) and exits 0/1/2/3 accordingly; an in-progress check reads as pending, a PR with no checks reads as unknown - neither is a false red.

## What changed (ab-d0337fbc)

The pre-wedge control plane had three layers removed in Tasks 1.1-3.2: (1) the 1101-line bash stop hook with thrash/budget/phase-stall/help-escalation/orphan detectors and three-factor gate provenance machinery (~7575 LOC of `scripts/lib/` helpers deleted); (2) the `fno gate` CLI surface and `gates/` package (~1460 LOC + both `gate_reality_map.yaml` copies deleted); (3) the phase verifier scripts in `skills/target/scripts/verifiers/` (~900 LOC deleted). The replacement is `fno-agents loop-check` (a single Rust verb) + the 118-line shim.
