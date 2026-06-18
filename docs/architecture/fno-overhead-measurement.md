# fno overhead measurement and the deferred daemon

**Status:** Decision recorded 2026-05-14. Daemon feature deferred. Re-measure quarterly or whenever the fno call surface changes materially.

This document records the strategic decision made by the fno-daemon plan's Phase 0 measurement gate, the methodology used, and the conditions under which the decision should be revisited.

## The decision

The aggregate ratio of `fno <verb>` wall time to target phase wall time, measured across eight representative target sessions, is **0.22%** — 70x below the 15% threshold that would have justified building a resident Python daemon for the `fno` CLI.

Per the plan's decision rule:

| Aggregate ratio | Decision | Action |
|---|---|---|
| `< 0.15` | `abort_daemon` | Switch focus to lazy-imports refactor in `cli/src/fno/cli.py` |
| `0.15 <= ratio < 0.30` | `reads_only_v1` | Ship daemon with 8 read handlers; skip the 4 write handlers |
| `>= 0.30` | `full_v1` | Ship full daemon (8 reads + 4 writes) per the original spec |

The decision is `abort_daemon`. The backlog node is marked `_status: deferred` with the rationale recorded on the node itself.

## Why this matters

The daemon was a 2-4 week scope. The `/think panel` pass that preceded the plan demanded measurement-first rather than implementation-first because the same latency wins (subprocess startup cost) can be achieved by a lazy-imports refactor in `cli.py` at 2-3 days of work. The strategic gate exists to detect this case quickly.

Phase 0 took roughly one target iteration. The measurement harness lives at `cli/benchmarks/measure_fno_in_target.py` and is re-runnable.

## Methodology

There were no `fno_invoked` or `fno_completed` events in `.fno/events.jsonl` to support retroactive measurement, so the harness instrumented fresh subprocess timings:

1. **Per-verb wall-time measurement.** The harness times `fno --help` (a zero-side-effect probe that exercises the full subprocess startup path including `__init__.py` import chains) 20 times and uses the median. Argument parsing is negligible; startup cost dominates.

2. **Probe hardening.** Each subprocess invocation runs with `timeout=10s`. Probes that return non-zero, time out, or signal-kill (negative returncode) are dropped from the sample rather than averaged in as fast 0ms runs. If more than 25% of probes fail the run aborts with a clear `RuntimeError`. This guard was added after sigma-review caught the silent-failure mode.

3. **Per-session estimation.** The harness multiplies the median per-call latency by the per-verb expected count per phase, tallied by grepping `skills/target/`, `skills/operator/`, `hooks/`, and the do/check-pr/sigma-review skill bodies for `fno <verb>` invocations.

4. **Session sample.** Sampled sessions come from `.fno/ledger.json`, filtered to those with the `do` phase and duration greater than 3 minutes. Eight sessions met the criteria, ranging from 3.4 min to 121 min total wall time.

5. **Ratio computation.** Aggregate ratio is volume-weighted: `sum(fno_wall_seconds) / sum(phase_wall_seconds)` across the sample. The denominator is total session wall time (conservative against the daemon — using just the "do" sub-phase would make the daemon look more attractive but is harder to bound).

6. **Conservative bounding.** The 19-calls-per-session estimate from the skill grep is a lower bound. Doubling it still yields a ratio of ~0.004 — 37x below the 15% threshold. The decision is robust to a substantial undercount.

The raw per-session timings are committed at `cli/benchmarks/fno_in_target_results.json`. The decision artifact at `.fno/measurements/2026-05-14-fno-daemon-baseline.md` lists sampled session IDs and provides the full table.

## When to re-measure

The decision should be revisited if any of the following change materially:

- The `fno` CLI gains many new verbs or one or more verbs become substantially slower (e.g., a new verb that does heavy IO becomes hot-path).
- Target's call mix shifts toward more `fno` invocations per phase.
- Python subprocess startup cost increases (e.g., the lazy-imports refactor lands and the baseline per-call latency changes).
- The wall-time denominator shifts because target phases get faster overall (the ratio matters in proportion to phase work, not absolute call time).

To re-measure, follow the operational guide at `docs/guides/measuring-fno-overhead.md`.

## What this work delivers

Even though the daemon was not built, this work is durable:

1. **The decision artifact** at `.fno/measurements/2026-05-14-fno-daemon-baseline.md` is a written, auditable record that the team considered the daemon, measured carefully, and chose the lazy-imports path instead. Future contributors who think "we should build a daemon" can find this record and either accept the rationale or re-measure.

2. **The measurement harness** at `cli/benchmarks/measure_fno_in_target.py` is reusable. It will re-run any time conditions warrant.

3. **A canonical `phase_0_decision()` event builder** in `cli/src/fno/events/__init__.py` for future measurement-gated plans, so they emit schema-valid events rather than reproducing the envelope-shape bug found here.

## Related decisions

- The proposed alternative — lazy imports in `cli/src/fno/cli.py` — has no current backlog node. When opened, it should reference this decision in its motivation section so the chain of reasoning is preserved.
- ~~The `fno event emit` CLI subcommand at `cli/src/fno/events/cli.py` still routes through the legacy `events/log.py` path that uses a `payload` envelope incompatible with the schema validator. The canonical builder added in this work bypasses that bug for `phase_0_decision`; a separate fix should bring `fno event emit` into alignment for all event types.~~ Fixed in follow-up work: `fno event emit` now routes through `events._build` + `append_event` for all event types; `--data` is the canonical option name, `--payload` remains as a deprecated alias with a stderr warning.
