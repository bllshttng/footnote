# Measuring fno overhead in target phases

This guide explains how to re-run the fno-overhead measurement harness when conditions warrant a revisit of the deferred-daemon decision (see `docs/architecture/fno-overhead-measurement.md` for the recorded rationale).

## When to re-run

Re-run if any of the following changed since the last measurement landed:

- The `fno` CLI added many new verbs, or one or more verbs got materially slower.
- Target's call mix per phase shifted toward more `fno` invocations.
- Python subprocess startup cost shifted (e.g., the lazy-imports refactor landed, or a new heavy import was added to `cli/src/fno/cli.py`).
- The wall-time denominator shifted because target phases got faster overall (ratio scales with phase work, not absolute call time).

Quarterly cadence is a reasonable default if none of the above triggers fire.

## Running the harness

```bash
cd cli
uv run python benchmarks/measure_fno_in_target.py
```

Output goes to `cli/benchmarks/fno_in_target_results.json`. The script also prints a one-line summary with the aggregate ratio and the recommended decision.

### Requirements

- The repo root must contain a populated `.fno/ledger.json` with at least three completed target sessions that have the `do` phase and duration > 3 minutes. Without this, the harness exits with a `<help reason="insufficient-sample-data">` marker.
- The `fno` binary must be on `PATH`. If `fno --version` fails before running the harness, recover by running `uv tool upgrade footnote` or `fno update`.

### What gets measured

The harness times `fno --help` 20 times and uses the median. It then multiplies the per-call median by the per-verb expected count per target phase (tallied by grepping the skill bodies for `fno <verb>` invocations) and divides by representative `phase_wall_seconds` from the ledger.

Subprocess probes that time out (10s default), return non-zero, or signal-kill are dropped. If more than 25% of probes fail the harness aborts with a `RuntimeError` so you do not get a confidently-wrong ratio.

## Applying the decision rule

The decision rule has three buckets:

| Aggregate ratio | Decision | Recommended next step |
|---|---|---|
| `< 0.15` | `abort_daemon` | Lazy-imports refactor in `cli/src/fno/cli.py` (2-3 days) |
| `0.15 <= ratio < 0.30` | `reads_only_v1` | Daemon with 8 read handlers, no writes (medium cost) |
| `>= 0.30` | `full_v1` | Full daemon: 8 reads + 4 writes per the original spec |

Boundary values fall into the HIGHER bucket (ratio exactly 0.15 -> reads_only_v1; exactly 0.30 -> full_v1). The harness's `apply_decision_rule()` enforces this; tests at `cli/benchmarks/test_measure_fno.py::TestDecisionRuleBoundaries` pin the behavior.

## Recording a new decision

If the result diverges from the previous decision (e.g., the ratio crosses 0.15 because target got faster), record a new measurement artifact:

1. **Write the artifact.** Create `.fno/measurements/{date}-fno-overhead.md` with sampled session IDs, the per-session table, the aggregate ratio, the decision, and a recommendation. The previous artifact at `.fno/measurements/2026-05-14-fno-daemon-baseline.md` is the template.

2. **Emit the event.** Either the canonical Python builder (preferred when emission happens inside a script) or the CLI (preferred for ad-hoc shell work; both routes write the same canonical envelope):

```python
from fno.events import phase_0_decision, append_event

ev = phase_0_decision(
    ratio=0.18,  # example
    decision="reads_only_v1",
    evidence_path=".fno/measurements/2026-08-01-fno-overhead.md",
    source="target",  # or "subagent" if dispatched from one
)
append_event(ev)
```

The builder validates the decision enum at build time; an unknown decision raises `ValidationError` rather than landing a malformed event on disk.

3. **Update the backlog if the decision changed.** If the previous decision was `abort_daemon` and the new ratio is now `reads_only_v1` or `full_v1`, open or revive the daemon backlog node:

```bash
fno backlog undefer <node-id>  # revives the existing deferred node
# or open a new spec referencing the new measurement
```

## Files

| Path | Purpose |
|---|---|
| `cli/benchmarks/measure_fno_in_target.py` | The harness script |
| `cli/benchmarks/test_measure_fno.py` | Tests pinning the decision rule and subprocess-hardening behavior |
| `cli/benchmarks/fno_in_target_results.json` | Most recent raw measurement output |
| `.fno/measurements/*.md` | Decision artifacts (one per measurement run that produced a new decision) |
| `cli/src/fno/events/__init__.py` | Canonical `phase_0_decision()` event builder |
| `cli/src/fno/events/schema.yaml` | Schema entry for `phase_0_decision` events |
