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

`skills/target/scripts/context-probe.sh` reads ground truth from the session transcript JSONL using the same access pattern as loop-check. It is called by `handoff.sh` during precondition evaluation at pressure boundaries. The LLM never calls it directly.

- Input: path to the session transcript JSONL (resolved from the session manifest's `claude_transcript_id` field)
- Selects the last assistant message carrying a `usage` block
- Computes `used_tokens = input_tokens + cache_creation_input_tokens + cache_read_input_tokens`
- Window size: `[1m]` in model string -> 1,000,000; else 200,000
- Output (stdout, exit 0): `{"used_tokens": N, "window_tokens": N, "used_pct": N, "model": "..."}`
- Exit 3 ("unreadable"): missing file, jq absent, no assistant line with usage block, parse failure
- Any nonzero exit is treated as no-pressure (fail-safe toward not firing)

The statusline chain (`statusline-wrapper.sh` -> `~/.claude/.session-context.json` -> `hooks/context-monitor.js`) is disqualified as a trigger source for two reasons: the wrapper is not installed in the current environment (the sidecar is 1 byte; the monitor exits silently every fire), and when it was wired it produced false positives consistent with percentages computed against a 200K window on a 1M-context model (`opus-4-8[1m]`). `hooks/context-monitor.js` remains as advisory UX only; its repair or retirement is a separate carveout.

## Terminal-session resolution rule

For a node N executed with handoff, the walker must not judge the parent session for promise - only the terminal session T counts. The rule (plan sec 2.5) is mechanical: follow the `delegated` chain from the dispatched session; T is the last session with no `delegated` event naming a successor.

Three outcomes:
1. T emitted `<promise>` - N is complete.
2. T's `node:N` claim is PID-live - N is in progress; walker waits.
3. T is dead with no promise and no successor - N is incomplete; stale-claim recovery applies and walker may re-dispatch.

This resolution is implemented in `scripts/lib/megawalk-lineage.sh` (the library function) and consumed by `hooks/megawalk-stop-hook.sh` (the pre-advance check). Single-generation nodes (no `delegated` events) follow the existing code path byte-identically per AC4-EDGE.

## Claims at canonical repo root

Claims (`fno claim acquire/release`) resolve to the canonical repo root via `git rev-parse --git-common-dir`, following the pattern established by `carveouts.jsonl`. This is Locked Decision 9. Without it, a conductor worktree and the canonical checkout have separate `.fno/claims/` directories; the one-live-holder invariant would hold only via dispatch reservation plus parent vigilance. With canonical-root resolution, handoff children inheriting the parent's worktree cwd and any root-spawned session write to the same claims directory.

If `git rev-parse --git-common-dir` fails (not in a git repo), the fallback is the cwd-relative `.fno/claims/` path (existing behavior, preserved).

## Lineage observability

- `delegated` events in `events.jsonl` are the source of truth for the chain `{from_session -> to_session}` per node; the full lineage is replayable.
- Plan frontmatter `session_ids` (existing completion-stamp inline-list) accumulates every generation's session ID.
- Agent names carry generation suffixes (`tgt-<node8>-g2`, `tgt-<node8>-g3`) for at-a-glance reading in `fno agents list`.
- The ledger attributes each session's cost separately, so multi-generation nodes show per-generation spend.

## Config reference

All keys live under `config.target.handoff` in `.fno/settings.yaml`. Schema: `HandoffBlock` in `cli/src/fno/config/__init__.py`. Shell consumer: `skills/target/scripts/handoff.sh` reads via `get_config "target.handoff.*"` with matching defaults (see lines ~100-106 of that file).

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
