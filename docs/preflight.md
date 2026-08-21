# CI-parity preflight

Preflight is an OPT-IN rehearsal of CI, never the gate. CI on the PR head decides the merge. A stock config (`config.preflight.required` absent or false) never runs this before a push. For a project that opts in, `scripts/ci/preflight.sh` runs CI's verdict locally before the push, so a local green means a green PR. It kills the push-wait-red-fix loop: each ~10-minute CI round used to surface one new failure. The default is false for two measured reasons. The rehearsal costs 22 to 47 minutes. It also cannot start under the 256-file-descriptor limit launchd hands every spawned worker. On 2026-08-19 six concurrent runs aged past an hour at load average 415 with zero completions. Three workers held green commits blocked only on this gate. On a stock config the pre-push obligation is the focused checks. They are `cargo fmt --check` for rust changes, markdown style lint for changed `.md`, PR-body length, and the blast-radius tests. CI re-runs all of it. Set `preflight.required = true` to restore the mandatory rehearsal per project.

This is a different thing from the environment preflight (`fno target` Step 3g, `skills/target/scripts/preflight/`), which checks working-tree cleanliness, dependencies, and auth. This preflight is a deterministic test/lint runner. It runs no LLM review. Review stays at `config.review.*`.

## The two scripts

### `fno test smoke` - the step registry

One ordered list of the cli-ci smoke job's test and lint steps, code-owned in
`cli/src/fno/test_cmd.py` (`_STRUCTURAL_STEPS`) plus auto-discovered owned
shell harnesses. The workflow calls `uv run --project cli fno-py test smoke`
(`fno test smoke` is the same runner locally), so there is no second copy of
the list to drift. Environment provisioning (checkout, Python/uv setup, the
Rust toolchain install, the cargo cache, the system PyYAML install) stays in
the workflow yaml - those are CI-runner concerns and that divergence is
deliberate. Everything a test needs at run time (the `uv sync` / `uv build`,
the `fno-agents` debug build) lives in the runner.

Modes:

| Invocation | Behavior |
|---|---|
| `fno test smoke` | Fail-fast. Exactly the pre-extraction CI semantics. |
| `fno test smoke --keep-going` | Run every step, print a summary table, record failures, exit non-zero if any failed. |
| `fno test smoke --only '<glob>'` | Run only steps whose name matches the shell glob. |
| `fno test smoke --retry-failed` | Re-run only the steps recorded by the last `--keep-going` run; full run if the record is missing or corrupt. |
| `fno test smoke --list [--verbose]` | Print the registry (names; with `--verbose`, working dir + command) and exit. |
| `fno test smoke --changed [--base REV --head REV]` | Run only the work the changed paths map to. Explicitly partial: see below. |

The three subset modes (`--changed`, `--only`, `--retry-failed`) are mutually
exclusive and the runner refuses a combination rather than letting one silently
win and mislabel the evidence.

Prerequisites (`uv`, `python3` with `yaml` importable, `cargo` when a selected
step needs it) are asserted up front and named on failure with exit 2. The
runner never installs anything at the system level - locally you install PyYAML
once by hand. A subset run (`--only` / `--retry-failed`) labels itself in the
header so a partial green can never be mistaken for a full green, and preflight
records a `mode=FULL verdict=green` attestation only on a full, all-green run,
so a subset pass can never mint or satisfy one; a run that executes zero steps
exits non-zero rather than reading as green.

### `--changed` - the changed-surface packet

Early feedback, and partial by construction.
It maps the changed paths to a subset of the same steps the full runner owns, so there is no second runner and no CI-only implementation.

Where the paths come from: with explicit `--base`/`--head` it diffs exactly those two revisions, so selection never depends on a mutable remote-tracking ref (this is what CI and preflight pass).
With neither, it takes one snapshot of the merge-base with `origin/main` plus untracked files, which a commit-to-commit diff cannot see but which are part of local changed intent.

Selection is deterministic and every hit names the rule that produced it, so a bad mapping is diagnosable from the receipt rather than from the source:

| Rule | Changed path | Selects |
|---|---|---|
| `test-file-self` | a test file | itself |
| `python-source-stem` | `cli/src/**.py` | `test_<stem>.py` and the `test_<stem>_*.py` family |
| `shell-harness-self` | a discovered harness | itself |
| `shell-helper-reverse` | a sourced helper | every harness that sources it |
| `registry-step` | any path a registry step names by hand, or an orchestrated subtree | that step |
| `rust-family` | `crates/<crate>/**` | the registry steps that own the crate |
| `infra-broad` | the runner, the selector, shared test config, a workflow | the selector's own contract tests |

Anything else is listed as `unmapped` and selects nothing.
Inference is best effort: roughly 40% of `cli/src` modules have a conventionally-named test, and the full gate, not the selector, is the completeness backstop.

Three mechanisms keep a partial green from reading as a full one.
The header says `CHANGED SUBSET` and never `FULL`.
Its receipt is its only durable artifact and lives in its own namespace (`.fno/changed-last-receipt.json`), so a changed run cannot clear the full runner's failure record and cannot mint or satisfy a `mode=FULL` attestation, and a concurrent full run cannot overwrite the changed receipt.
And the exit codes separate evidence quality:

| Exit | Meaning |
|---|---|
| 0 | the selected packet passed - that claim only |
| the child's own code | a selected step failed (propagated, not flattened) |
| 20 | nothing mapped: evidence about the selector, not that the change is safe |
| 21 | UNEVALUATED - missing base, shallow checkout, or an untrustworthy diff |
| 22 | the packet could not run at all (missing tool) - deliberately outside the child exit-code space |

Every executing run (not `--list`, which is a dry run) writes `.fno/changed-last-receipt.json` with the candidate and base identity, the selections and their rules, the unmapped paths, the verdict, and the timings (selection, execution, time to first signal).

Sharding the full suite is out of scope *here*, not ruled out: this node targets first-signal latency, and the merge gate is a separate lever with its own node.
The workflow guard permits a sharded smoke job, but requires an aggregating job that `needs` it, because each shard reports its own check and branch protection pointed at one of them would pass on a fraction of the suite.

### `scripts/ci/preflight.sh` - the hermetic runner

One command to run before pushing, on a project that opted in (`preflight.required = true`) or for a worker that chose the rehearsal. It validates the invoking checkout's
**committed HEAD** inside a persistent, hermetic preflight worktree, then runs
the changed packet, `fno-py test smoke --keep-going`, and the rust-ci legs
(pinned `cargo +1.94.1 fmt --check`, `cargo test --all-targets` for both crates,
advisory `cargo audit`).

The changed packet goes first and stops the whole run on its own failure, so a
broken nearest-neighbour test costs seconds rather than a full suite. It is
given an explicit base (merge-base with `origin/main`) and head (the candidate
SHA) because local mode inside the preflight worktree would read its
deliberately preserved untracked caches as changed paths. A packet that maps
nothing or cannot trust its diff falls through to the full gate instead of
reporting a verdict it did not earn, and `--retry-failed` skips it as a
different subset mode. Only the full legs can mint `mode=FULL`.

Why a separate worktree with a scrubbed environment: the canonical checkout's
`.fno/config.toml` otherwise leaks into the config reader's candidate chain and
produces local-only failures, which is what pushes agents toward selective
`-k` subset runs that then miss CI-only failures. The runner resets a dedicated
worktree to your HEAD and runs it with an environment that mirrors a fresh CI
checkout: a temp `HOME` (no `~/.fno`, `~/.claude`, or `~/.gitconfig`), `FNO_*`
scrubbed, the ambient `HARNESS_SESSION_MARKERS` unset, `FNO_NO_CANONICAL_CONFIG=1`
exported, a worktree-pinned `PYTHONPATH`, and the pytest spawn-leak guard. Cache
directories (`CARGO_HOME`, `RUSTUP_HOME`, `UV_CACHE_DIR`) are deliberately
re-exported so builds stay warm; the worktree's `target/` and `cli/.venv`
persist across runs. Hermeticity comes from environment isolation plus a hard
reset, not from disposing the worktree.

**Two ambient leaks a bare `FNO_*` scrub misses, and how they are sealed.** A
temp `HOME` cannot hide either, because both travel through channels other than
`HOME`/`cwd`. Both are sealed by preflight-internal seams set only by
`run_hermetic`, so default and real-worktree behavior is byte-for-byte unchanged:

1. **Ambient harness identity.** Preflight always runs inside a live harness, so
   `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` / `CODEX_SESSION_ID` /
   `GEMINI_SESSION_ID` are set and `resolve_self_model()` would resolve the real
   session's model instead of the `"unknown"` floor a fresh CI checkout produces.
   `run_hermetic` unsets every `HARNESS_SESSION_MARKERS` **and**
   `LEGACY_HARNESS_SESSION_MARKERS` name (derived from the Python tuples that are
   the single source of truth, fail-closed to a hardcoded literal list with a
   warning if the fetch errors).
   Both tuples are read, not just the canonical one: `CLAUDE_SESSION_ID` lives
   only in the legacy tuple, and `current_session_id()` / `current_session_ids()`
   both read it, so a canonical-only scrub leaves a live claude session
   resolvable inside the run whose whole purpose is to look like a fresh
   checkout. `cli/tests/smoke/test_preflight_hermetic.sh` fails when the
   derivation drops the legacy tuple or when the fallback literal goes stale,
   which is the only thing keeping a last-resort copy honest: the fallback runs
   only when Python cannot, so its gap is invisible wherever anyone is watching.
   The pytest suite seals the same leak independently in `cli/tests/conftest.py`,
   because a bare `pytest` skips preflight entirely.

2. **The canonical config candidate chain.** A worktree reaches the canonical
   checkout's `.fno/config.toml` through the shared git-common-dir, leaking
   `worktrees_base` into path/worktree tests. Pinning `FNO_CONFIG` or
   `FNO_GLOBAL_SETTINGS_PATH` diverges from CI and breaks the suite's own
   config-fixture tests, so instead `run_hermetic` exports
   `FNO_NO_CANONICAL_CONFIG=1`, which drops **only** the canonical candidate from
   `_settings_yaml_locations()`. An explicit `FNO_CONFIG` (candidate #1) and a
   worktree-local config (#2) still win, so no fixture is clobbered. The flag is
   preflight-internal (an env var, not a `config.*` key) and inert unless exactly
   `"1"`. The broader "a worktree always resolves its own config" loader change
   remains a separate root-cause node; this flag is the preflight-scoped subset.

The smoke runner still names every failing step, so any genuine red stays
visible and distinguishable.

Worktree location follows `config.paths.worktrees_base`
(`<base>/<repo>/preflight`), falling back to the harness-native
`<repo>/.claude/worktrees/preflight` when the knob is unset.

Behavior:

- Refuses a dirty invoking tree (exit 4), listing the uncommitted files -
  preflight validates commits, which is how it catches the forgot-to-commit-
  the-fixture class of failure.
- Refuses a stale base (exit 6). When the invoking HEAD is behind `origin/main`, the run cannot attest the merge head. The refusal fires before the cache check and before any lock or queue artifact. Rebase first.
- Serializes with an atomic lock. A dead holder's lock is stolen so a crashed run never wedges you. The steal is a single atomic rename. When several runs find the same dead holder, exactly one wins.
- Waits on a live holder instead of failing. Waiters queue FIFO by arrival. Tickets are atomic-mkdir directories beside the lock. Only the front ticket retries the real acquire. Dead tickets are reaped, so a crashed waiter never blocks the queue. A waiter cannot be lapped. A fresh arrival cannot snipe the lock ahead of a queued waiter.
- The wait is bounded by `--wait-timeout`. The default is 5400s (90m). A value of `0` restores the old immediate fail with exit 3. While waiting, the receipt prints the holder's pid, start time, elapsed time, and tree CPU seconds. When the launcher is gone, it also prints `orphaned=yes`. This makes a starved wait distinct from a healthy queue. To cancel a queued wait, touch `.fno/preflight-cancel` in the invoking checkout. The sentinel is one-shot by atomic rename: one token cancels exactly one waiter, which exits 130 with its ticket removed. A sentinel older than one hour is stale. The next queued run discards it instead of obeying it. Signal traps stay armed as best effort. macOS bash 3.2 does not run INT/TERM traps while waiting on a child. Ctrl-C alone will not stop a wait on that platform. Use the sentinel.
- Steals from a holder that is alive but not computing. The stall test requires under 1s of tree CPU over a 2m window. It also requires either 20m of age or an orphaned holder. An orphan is reparented to pid 1, so its launcher is gone and no floor is owed. The front waiter steals a condemned holder and reports the exception, naming which condemnation fired (stalled or orphaned). The victim's own tripwire VOIDs its verdict. Neither side reports anything false. An orphan that is genuinely computing keeps the lock: parentage only drops the age floor, never the CPU probe.
- Do NOT wrap this script in a retry loop. The wait is built in. Concurrent retry loops are the contention the queue exists to remove.
- Local verification is off by default. A stock config never requires it. On an opted-in project, `FNO_SKIP_PREFLIGHT=1` before `fno pr create` skips that push and relies on CI.
- Reuses a prior verdict: a FULL, non-VOID, all-legs-green run records a one-line attestation in its own slot beside the lock. The carrier directory holds one file per candidate SHA (`.preflight-attestations.d/`), so active worktrees never erase each other's greens. Slots older than 14 days are reaped at write time. The attestation is bound to `(full SHA, host)`. The next caller on the same SHA + host reads it before taking the lock. It exits 0 in well under a second. A second caller is never blocked behind a run still holding the lock. The receipt prints its own evidence (the matched SHA, the attestation's age, the earning pid, the host). A GREEN printed by a process that ran no tests must be auditable. `--force` discards the attestation and re-runs every suite. A RED run deletes a matching attestation, so a stale green cannot outlive a real failure. A subset pass (fewer legs executed than the required scope) or a VOID mints nothing. The mode comes from coverage, not from the flag. A `--retry-failed` run with no usable failure record executes every leg and earns FULL. The SHA is a complete cache key because the runner hard-resets a dedicated worktree to it and scrubs the environment. The `host=` field closes the one cross-environment hole a SHA key alone leaves.
- Exits 5 (VOID) if the shared preflight worktree or the lock changed hands
  mid-run, printing which of the two it lost. The run earned no verdict, so it
  prints neither GREEN nor RED. Treat 5 as re-run, never as a code failure:
  the verdict it would otherwise have reported was earned by another checkout,
  which is the misattribution this tripwire exists to catch.
- Exit 0 iff every non-advisory suite passed; `cargo audit` findings are shown
  in an advisory row and never flip the exit code.
- `--retry-failed` re-runs only the legs named in `.fno/preflight-last-failed-legs.txt`, a fast SUBSET. The record is preflight's, never hand-edited, and a missing or corrupt record means every leg runs. On an opted-in project, run a full preflight before the push you expect to settle green.

## Ship-phase wiring

Preflight runs before a push only at the policy's request. The single decision point is `fno pr evidence-required`. It reads `config.preflight.required` (default false) plus the `FNO_SKIP_PREFLIGHT=1` escape. The ship phase, `fno pr create`, the worker ship lane, and the batch lane all ask it. The bash path and the Python lanes cannot disagree. A failed or unparsable policy call fails open on the ship paths: no local preflight, and CI verifies the PR head. The `/pr create` router is stricter. When the policy call itself cannot be evaluated, it refuses outright.

On a stock config the pre-push obligation is the focused checks below, each taking seconds. There is deliberately no wrapper script for them. A new mandatory local runner rebuilds the gate this contract removes, one rung smaller.

- `cargo fmt --check` for rust changes.
- The tests covering the diff's blast radius.

`fno lint style` remains available for voluntary hand runs. For Markdown, run `fno lint style --surface markdown --files <changed .md> --diff-base origin/main`; it is not part of the stock pre-push or CI gate.

On a project that set `preflight.required = true` (see `skills/target/references/ship-phase.md`):

- Full run before the first PR push and before the settle-green push.
- `--retry-failed` between fix-loop commits, then one full run before the push you expect to go green.

The receipt this mints is **review-entry evidence, not merge eligibility**. `fno pr evidence-check` requires a full/passed receipt bound to the exact HEAD before a PR is opened. The ship and batch lanes behind it inherit the same rule. The requirement exists only on a project that opted in. From there, hosted CI re-runs everything on the PR head and the configured reviewers decide. Nothing local gates the merge: `fno pr merge` reads hosted CI and review attestations, never a preflight receipt. A receipt also survives a rebase. `fno pr evidence-check --allow-rebase-equivalent` accepts a full/passed receipt for an earlier commit whose verbatim patch ids equal HEAD's. A clean rebase still does not destroy evidence about code that did not change. That holds for a main that touched the same files. An edit inside a changed hunk's context window shifts the context lines and the identity refuses. Any code edit, whitespace-only edit, or conflict resolution changes that identity, so the equivalence path refuses exactly there. A failed or pending receipt for HEAD itself is never rescued, because the same patches can fail against a newer main. An incomplete journal walk blocks the path. The flag is review-entry only. The attestation reuse check inside `preflight.sh` stays strict, so a cached carrier from a different commit is never blessed.

Verdict reuse needs no caller change. It is checked inside `preflight.sh` itself, before the lock. The ship phase and fix loop inherit it from the one invocation they already make. A second push of an unchanged SHA therefore returns instantly. `--force` is there for the rare case a caller wants to re-prove it. The reuse check is deliberately not duplicated in `ship-phase.md` or the fix loop - one implementation, every reachable caller.

The runner guard is an existence check (`[[ -x scripts/ci/preflight.sh ]]`). It no-ops in any repo which does not ship the script. The policy call sits in front of it. Skips stay explicit and auditable: a stock config, `FNO_SKIP_PREFLIGHT=1`, or a docs-only diff (`docs/`, `README.md`, and the gated vault path). The scripts never self-skip. The skip decision lives in `fno pr evidence-required`.

## Running it yourself

```bash
scripts/ci/preflight.sh                 # full run; reuses a matching attestation if one exists
scripts/ci/preflight.sh --force         # ignore the cached attestation, run every suite
scripts/ci/preflight.sh --retry-failed  # fast: only the legs that failed last run
fno test smoke --keep-going             # non-hermetic, in your working tree
fno test smoke --changed                # earliest signal: only what your diff maps to
fno test smoke --changed --list         # what it would run, and what it could not map
fno test smoke --list                   # what CI actually runs
```

`fno test smoke --keep-going` in your working tree is the fast, non-hermetic option for checking before you commit, the same registry minus the worktree isolation. Preflight refuses a dirty tree on purpose. That direct smoke run is how you check uncommitted work.
