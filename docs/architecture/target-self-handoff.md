# Target self-handoff: sanctioned session succession at pipeline boundaries

## Problem

A `/target` session that spans blueprint plus multiple do-phase waves carries the blueprint context into every wave, consuming window space and risking context pressure failures mid-execution. The predecessor model was a footgun: in the origin incident (parent session `4ef115b5`, child session `75457ae9`, a sibling repo's W5-2, 2026-06-05), the parent's `autolaunch-on-ready.sh` spawned a second `/target` for the same node the parent held. With no succession protocol, the only sanctioned move was killing the child (`fno agents stop/rm`) and recording a carveout. The one-worker-per-node invariant forced a choice, and there was no way to choose the child.

This design makes succession first-class by sequencing existing primitives: claim release, dispatch reservation, phase-handoff artifacts, plan-path re-entry, cwd-based worktree inheritance, and a new `session_satisfied(trigger=delegated)` close path. The one new component is a transcript-derived context probe.

## 8-step handoff protocol

The sanctioned helper (`skills/target/scripts/handoff.sh`) executes the full sequence in one atomic invocation. The LLM invokes the helper and then performs only step 9 (close). The LLM never executes the individual steps.

| Step | Action |
|------|--------|
| 1 | Preconditions: plan status `ready+`, `plan_path` set on node, caller holds `node:<id>`, generation < cap, no prior handoff sentinel for this session |
| 2 | Write handoff brief artifact (`{plan_path}.artifacts/handoff/{boundary}-{session_id}.md`) with generation, from_session, boundary |
| 3 | Acquire `dispatch:<node>` reservation (TTL 180s) as the bridge token that keeps third parties out during the claim gap |
| 4 | Archive caller's `target-state.md` to `{plan_path}.artifacts/` - the sanctioned helper is the only actor that touches this file |
| 5 | Release `node:<id>` - from this instant the parent is contractually done executing the node |
| 6 | Spawn successor from the parent's worktree cwd: `cd <worktree> && claude --bg --name tgt-<node8>-g<N+1> "/fno:target <modifiers> <node-id>"`. cwd inheritance means the child continues the same branch and same `.fno/` without worktree creation |
| 7 | Verify child registered (registry row / `fno agents list` entry) within 60s. On failure: re-acquire `node:<id>` FIRST, then restore the archived state file, emit `handoff_failed`. If the RE-ACQUIRE itself fails (another worker took the claim in the gap), the manifest stays archived, `handoff_failed` carries `reason=reacquire_failed`, and the helper exits 12 with `handoff-claim-lost` - the parent must NOT continue the node (one-holder invariant). If restoration fails after a successful re-acquire, the archived copy stays in place, `handoff_failed` carries `reason=restore_failed`, and the caller raises `<help reason="handoff-restore-failed">` - never continue silently without a manifest |
| 8 | Emit `delegated` event: `{node_id, from_session, to_session, boundary, generation}` to events.jsonl; append own session_id to plan frontmatter `session_ids` |

**Exit-code contract:**
- `0` - delegated: succession complete, parent may close
- `10` - parked: precondition failed or spawn failed with clean unwind; parent continues in-session
- `12` - must-stop: `handoff-restore-failed` (manifest unrestorable) or `handoff-claim-lost` (re-acquire failed; claim may be held elsewhere); parent raises help and stops, never continues in-session

**Atomicity vs cancel:** Steps 2-8 execute inside one helper invocation. The cancel sentinel (`.fno/.target-cancelled`) is evaluated by the stop hook between LLM turns, never inside the helper. Cancel takes effect either before the helper runs (no handoff state mutated) or after it returns (delegation already recorded; only step 9's close is replaced by an Interrupted termination). There is no cancel window between release (step 5) and the delegated event (step 8).

## Trigger matrix

| Boundary | Trigger | Policy |
|----------|---------|--------|
| blueprint -> do | structural (no measurement) | Unattended: auto. Attended: one-line confirm "Plan ready - dispatch fresh worker for the build? [Y/n]" (mirrors Plan-Mode front door / /agents confirm precedent). Timeout -> park. |
| wave/phase boundaries in do and review | pressure: context probe `used_pct >= config.target.handoff.used_pct_trigger` (default 50) | Auto in both modes; prints notice |
| mid-wave or mid-task | never | Finish the current unit first |
| generation cap reached (default 4) | refuse | `<help reason="handoff-chain-exhausted">`, continue in-session |

Attendance is determined by tty: interactive tty = attended; `--bg`/headless/megawalk-dispatched = unattended. This mirrors the Plan-Mode front door's attended-only rule.

## Delegated close mechanism

The Rust loop-check (`crates/fno-agents/src/loopcheck.rs`) does NOT scan `session_satisfied` events any longer: the old Python phase machine and its completion-accounting scanner were deleted in the control-plane collapse wedge. The delegated close works through a different path: `handoff.sh` step 4 archives `target-state.md`, and `loopcheck.rs` at lines ~1088-1105 allows exit when the manifest is missing or corrupt ("corrupt/missing manifest; allowing exit"). Manifest absence is the mechanical unlock; the stop hook fires, finds no manifest, and allows the session to close green.

The `session_satisfied(trigger=delegated)` event written at step 8 is the audit record for this close path, not the unlock mechanism. This behavior is pinned by `crates/fno-agents/tests/loopcheck_missing_manifest.rs`, which asserts exit-0 plus decision "allow" with message containing "missing manifest" when `--state` points to a nonexistent file. A separate carveout tracks a broader `session_satisfied` regression that is out of scope here.

## Context probe contract

`fno whoami context --transcript <path> --json` reads ground truth from the session transcript JSONL: one implementation in the repo (ported out of the skill-local probe so a codex/agy/opencode worker and every hook reach the same arithmetic). `handoff.sh` calls it at pressure boundaries, `fno whoami` prints the `context:` line so the model reads its own window without reverse-engineering a hook, and the bundled skill's local probe is now a shim over the verb.

- Input: path to the session transcript JSONL (resolved from the session manifest's `claude_transcript_id` field)
- Selects the last assistant message carrying a `usage` block
- Computes `used_tokens = input_tokens + cache_creation_input_tokens + cache_read_input_tokens`
- Window size: 1,000,000 is an **allowlist**, never a catch-all. It covers the `[1m]` suffix (a zai/GLM routing marker) and the ids known to have a 1M window: Opus 5, Sonnet 5, Fable 5, Opus 4.8/4.7/4.6, Sonnet 4.6.
  Everything else falls to 200,000, including Haiku 4.5, the legacy 200K Claudes (Opus 4.5, Sonnet 4.5, Claude 3.x), and any unrecognized or future id.
  No Anthropic model id carries `[1m]`, so a table keyed on that suffix alone put every Claude model on the 200K branch and inflated `used_pct` 5x on a 1M model.
  The fallback direction is asymmetric on purpose: too small a denominator overstates pressure and fires the handoff early, costing one extra succession, while too large understates it and lets the session run out of context, losing the run.
  A `claude-*` catch-all would put every legacy 200K model on the losing side of that trade, so a new 1M model earns a line in the table rather than inheriting one.
- Output (stdout, exit 0): `{"used_tokens": N, "window_tokens": N, "used_pct": N, "model": "..."}`
- Exit 3 ("unreadable"): missing file, jq absent, no assistant line with usage block, parse failure
- Any nonzero exit is treated as no-pressure (fail-safe toward not firing)

The statusline chain (`statusline-wrapper.sh` -> `~/.claude/.session-context.json` -> `hooks/spend-drift-monitor.js`) is **retired**, not merely disqualified.
The wrapper was never shipped, nothing in this repo ever wrote the sidecar or its `/tmp` bridge fallback, and the read consequently failed on every fire, so the hook never warned once.
It was also structurally the wrong source: a second-hand percentage whose denominator footnote could neither see nor validate, which is how it reported 200K-window numbers on a 1M-context model.
`hooks/spend-drift-monitor.js` now carries only the spend-cap and model-drift guards; its context-percentage path was deleted.

`fno whoami context` (the CLI behind the skill-local shim) is the single context-measurement path: first-hand token counts from the transcript, with a denominator this repo owns.

### Why the PreCompact arm hook was silent

`hooks/arm-handoff-precompact.sh` is registered on `PreCompact` and enabled by default, yet it never armed once.
It gated on `kill -0 owner_pid`, and `owner_pid` is the transient `fno do target init` wrapper pid that dies within about a second of init returning, so the gate rejected every live session and the hook returned before reading the payload or running the probe.
`owner_pid` can only ever prove life, never death; death is asserted from the node claim, which is session-pid anchored and TTL-protected.
The hook now follows the same asymmetry as `cli/src/fno/target/orient.py::_manifest_liveness`.

Its unit test hid this for the whole time the bug shipped, because the fixture wrote `owner_pid: $$` - a live pid that no real session has after init.
A guard whose test constructs a state production never produces is not covered, however green the suite reads.

## Terminal-session resolution rule

For a node N executed with handoff, the walker must not judge the parent session for promise - only the terminal session T counts. The rule (plan sec 2.5) is mechanical: follow the `delegated` chain from the dispatched session; T is the last session with no `delegated` event naming a successor.

Three outcomes:
1. T emitted `<promise>` - N is complete.
2. T's `node:N` claim is PID-live - N is in progress; walker waits.
3. T is dead with no promise and no successor - N is incomplete; stale-claim recovery applies and walker may re-dispatch.

This resolution is implemented in `scripts/lib/megawalk-lineage.sh` (the library function) and consumed by `hooks/megawalk-stop-hook.sh` (the pre-advance check). Single-generation nodes (no `delegated` events) follow the existing code path byte-identically per AC4-EDGE.

## Claims at canonical repo root

Claims (`fno agents claim acquire/release`) resolve to the canonical repo root via `git rev-parse --git-common-dir`, following the pattern established by `carveouts.jsonl`. This is Locked Decision 9. Without it, a conductor worktree and the canonical checkout have separate `.fno/claims/` directories; the one-live-holder invariant would hold only via dispatch reservation plus parent vigilance. With canonical-root resolution, handoff children inheriting the parent's worktree cwd and any root-spawned session write to the same claims directory.

If `git rev-parse --git-common-dir` fails (not in a git repo), the fallback is the cwd-relative `.fno/claims/` path (existing behavior, preserved).

## Lineage observability

- `delegated` events in `events.jsonl` are the source of truth for the chain `{from_session -> to_session}` per node; the full lineage is replayable.
- Plan frontmatter `session_ids` (existing completion-stamp inline-list) accumulates every generation's session ID.
- Agent names carry generation suffixes (`tgt-<node8>-g2`, `tgt-<node8>-g3`) for at-a-glance reading in `fno agents list`.
- The ledger attributes each session's cost separately, so multi-generation nodes show per-generation spend.

## Config reference

All keys live under `config.target.handoff` in `.fno/config.toml`. Schema: `HandoffBlock` in `cli/src/fno/config/__init__.py`. Shell consumer: `skills/target/scripts/handoff.sh` reads via `get_config "target.handoff.*"` with matching defaults (see lines ~100-106 of that file).

| Key | Default | Constraint | Description |
|-----|---------|------------|-------------|
| `enabled` | `true` | bool | Master on/off for the handoff feature |
| `used_pct_trigger` | `50` | 1-100 | Context-usage percent threshold for pressure-triggered handoffs |
| `generation_cap` | `4` | >= 1 | Maximum successor depth; cap N refuses at generation N and emits `handoff-chain-exhausted` help |

Example override:
```yaml
config:
  target:
    handoff:
      used_pct_trigger: 75   # delay relay until 75% context used
      generation_cap: 3      # tighter chain for short pipelines
```

## Events added

Three new event types in `cli/src/fno/events/schema.yaml`:

| Type | When emitted | Key data fields |
|------|-------------|-----------------|
| `delegated` | Step 8 of handoff.sh, on successful succession | `node_id`, `from_session`, `to_session`, `boundary`, `generation` |
| `handoff_failed` | Step 7 on spawn failure or unwind | `node_id`, `reason`, `boundary`, `generation` |
| `handoff_probe_unreadable` | When context-probe.sh exits nonzero at a pressure boundary | `node_id`, `boundary`, `probe_exit_code` |

The `session_satisfied` event schema (from `control-plane-loop.md`) gains `delegated` as a constrained source value. The event is audit-only in the delegated close path; manifest archival (step 4) is the mechanical unlock.
