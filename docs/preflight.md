# CI-parity preflight

`scripts/ci/preflight.sh` runs CI's verdict locally before you push, so a local
green means a green PR. It exists to kill the push-wait-red-fix loop: because CI
fails fast and its ~40 smoke steps used to live only in the workflow yaml, each
push surfaced exactly one new failure at ~10 minutes a round.

This is a different thing from the environment preflight (`fno target` Step 3g,
`skills/target/scripts/preflight/`), which checks working-tree cleanliness,
dependencies, and auth. This preflight is a deterministic test/lint runner. It
runs no LLM review; review stays at `config.review.*`.

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
| `shell-registry-step` | a `.sh` wired into a registry step by hand | that step |
| `rust-family` | `crates/<crate>/**` | the registry steps that own the crate |
| `infra-broad` | the runner, the selector, shared test config, a workflow | the selector's own contract tests |

Anything else is listed as `unmapped` and selects nothing.
Inference is best effort: roughly 40% of `cli/src` modules have a conventionally-named test, and the full gate, not the selector, is the completeness backstop.

Three mechanisms keep a partial green from reading as a full one.
The header says `CHANGED SUBSET` and never `FULL`.
The failure record and the receipt live in their own namespace (`.fno/changed-last-*`), so a changed run cannot clear the full runner's failure record and cannot mint or satisfy a `mode=FULL` attestation, and a concurrent full run cannot overwrite the changed receipt.
And the exit codes separate evidence quality:

| Exit | Meaning |
|---|---|
| 0 | the selected packet passed - that claim only |
| the child's own code | a selected step failed (propagated, not flattened) |
| 20 | nothing mapped: evidence about the selector, not that the change is safe |
| 21 | UNEVALUATED - missing base, shallow checkout, or an untrustworthy diff |

Every run writes `.fno/changed-last-receipt.json` with the candidate and base identity, the selections and their rules, the unmapped paths, the verdict, and the timings (selection, execution, time to first signal).

Sharding the full suite is deliberately out of scope until those receipts show that final merge latency, rather than first feedback, is the remaining bottleneck.

### `scripts/ci/preflight.sh` - the hermetic runner

One command to run before pushing. It validates the invoking checkout's
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
   `run_hermetic` unsets every `HARNESS_SESSION_MARKERS` name (derived from the
   Python tuple that is the single source of truth, fail-closed to a hardcoded
   literal list with a warning if the fetch errors).

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
- Serializes with an atomic lock (exit 3 if another run holds it, printing the
  holder). A dead holder's lock is stolen so a crashed run never wedges you.
  The steal is a single atomic rename, so when several runs find the same dead
  holder exactly one wins and the rest exit 3.
- Reuses a prior verdict: a FULL, non-VOID, all-legs-green run records a
  one-line attestation beside the lock, bound to `(full SHA, host)`. The next
  caller on the same SHA + host reads it before taking the lock and exits 0 in
  well under a second without re-running anything - so a second caller is never
  blocked behind a run still holding the lock. The receipt prints its own
  evidence (the matched SHA, the attestation's age, the earning pid, the host),
  because a GREEN printed by a process that ran no tests is exactly the receipt
  a reader should be able to audit; `--force` discards it and re-runs every
  suite. A RED run deletes a matching attestation, so a stale green cannot
  outlive a real failure; a `--retry-failed` (subset) pass or a VOID mints
  nothing. The SHA is a complete cache key because the runner hard-resets a
  dedicated worktree to that SHA and scrubs the environment; `host=` closes the
  one cross-environment hole a SHA key alone leaves.
- Exits 5 (VOID) if the shared preflight worktree or the lock changed hands
  mid-run, printing which of the two it lost. The run earned no verdict, so it
  prints neither GREEN nor RED. Treat 5 as re-run, never as a code failure:
  the verdict it would otherwise have reported was earned by another checkout,
  which is the misattribution this tripwire exists to catch.
- Exit 0 iff every non-advisory suite passed; `cargo audit` findings are shown
  in an advisory row and never flip the exit code.
- `--retry-failed` runs only smoke's recorded failures (a fast SUBSET); run a
  full preflight before the push you expect to settle green.

## Ship-phase wiring

`fno target`'s ship phase and fix loop run preflight before pushing when the
script exists in the repo (see `skills/target/references/ship-phase.md`):

- Full run before the first PR push and before the settle-green push.
- `--retry-failed` between fix-loop commits, then one full run before the push
  you expect to go green.

Verdict reuse needs no caller change: it is checked inside `preflight.sh`
itself, before the lock, so the ship phase and fix loop inherit it from the one
invocation they already make. (A second push of an unchanged SHA therefore
returns instantly; `--force` is there for the rare case a caller wants to
re-prove it. The reuse check is deliberately not duplicated in `ship-phase.md`
or the fix loop - one implementation, every reachable caller.)

The trigger is an existence guard (`[[ -x scripts/ci/preflight.sh ]]`), so it
no-ops in any repo that does not ship the script - a repo-neutral convention,
not a footnote hardcode. Skips are explicit and auditable:
`FNO_SKIP_PREFLIGHT=1`, or a docs-only diff (only documentation, the vault dir, and `*.md` files).
The scripts never self-skip; the skip decision lives in the caller.

## Running it yourself

```bash
scripts/ci/preflight.sh                 # full run; reuses a matching attestation if one exists
scripts/ci/preflight.sh --force         # ignore the cached attestation, run every suite
scripts/ci/preflight.sh --retry-failed  # fast: only last run's failures
fno test smoke --keep-going             # non-hermetic, in your working tree
fno test smoke --changed                # earliest signal: only what your diff maps to
fno test smoke --changed --list         # what it would run, and what it could not map
fno test smoke --list                   # what CI actually runs
```

`fno test smoke --keep-going` run directly in your working tree is the fast,
non-hermetic option for checking before you commit, the same registry minus
the worktree isolation. Preflight refuses a dirty tree on purpose; that direct
smoke run is how you check uncommitted work.
