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

### `scripts/ci/smoke.sh` - the step registry

One ordered list of the cli-ci smoke job's test and lint steps. The workflow
calls `bash scripts/ci/smoke.sh` instead of spelling the steps inline, so there
is no second copy of the list to drift. Environment provisioning (checkout,
Python/uv setup, the Rust toolchain install, the cargo cache, the system PyYAML
install) stays in the workflow yaml - those are CI-runner concerns and that
divergence is deliberate. Everything a test needs at run time (the `uv sync` /
`uv build`, the `fno-agents` debug build) lives in the script.

Modes:

| Invocation | Behavior |
|---|---|
| `smoke.sh` | Fail-fast. Exactly the pre-extraction CI semantics. |
| `smoke.sh --keep-going` | Run every step, print a summary table, record failures, exit non-zero if any failed. |
| `smoke.sh --only '<glob>'` | Run only steps whose name matches the shell glob. |
| `smoke.sh --retry-failed` | Re-run only the steps recorded by the last `--keep-going` run; full run if the record is missing or corrupt. |
| `smoke.sh --list [--verbose]` | Print the registry (names; with `--verbose`, working dir + command) and exit. |

Prerequisites (`uv`, `python3` with `yaml` importable, `cargo` when a selected
step needs it) are asserted up front and named on failure with exit 2. The
script never installs anything at the system level - locally you install PyYAML
once by hand. A subset run (`--only` / `--retry-failed`) labels itself in the
header so a partial green can never be mistaken for a full green; a run that
executes zero steps exits non-zero rather than reading as green.

### `scripts/ci/preflight.sh` - the hermetic runner

One command to run before pushing. It validates the invoking checkout's
**committed HEAD** inside a persistent, hermetic preflight worktree, then runs
`smoke.sh --keep-going` plus the rust-ci legs (pinned `cargo +1.94.1 fmt
--check`, `cargo test --all-targets` for both crates, advisory `cargo audit`).

Why a separate worktree with a scrubbed environment: the canonical checkout's
`.fno/config.toml` otherwise leaks into the config reader's candidate chain and
produces local-only failures, which is what pushes agents toward selective
`-k` subset runs that then miss CI-only failures. A worktree reaches that
canonical config through the shared git-common-dir, not through `HOME` or
`cwd`, so scrubbing `HOME` alone is not enough. The load-bearing isolation seam
is `FNO_CONFIG` pinned to an empty file: `$FNO_CONFIG`, when set, is the config
loader's only candidate, so the canonical config cannot leak, and an empty file
yields the same `SettingsModel` defaults a fresh CI checkout (no committed
`.fno/config.toml`) resolves to. The runner also uses a temp `HOME`,
`FNO_GLOBAL_SETTINGS_PATH=/dev/null`, no leaked `FNO_*`, and a worktree-pinned
`PYTHONPATH`. Cache directories (`CARGO_HOME`, `RUSTUP_HOME`, `UV_CACHE_DIR`)
are deliberately re-exported so builds stay warm; the worktree's `target/` and
`cli/.venv` persist across runs. Hermeticity comes from environment isolation
plus a hard reset, not from disposing the worktree. (The config candidate-chain
leak itself is tracked as a separate root-cause fix; preflight papers over it
until that lands.)

Worktree location follows `config.paths.worktrees_base`
(`<base>/<repo>/preflight`), falling back to the harness-native
`<repo>/.claude/worktrees/preflight` when the knob is unset.

Behavior:

- Refuses a dirty invoking tree (exit 4), listing the uncommitted files -
  preflight validates commits, which is how it catches the forgot-to-commit-
  the-fixture class of failure.
- Serializes with an atomic lock (exit 3 if another run holds it, printing the
  holder). A dead holder's lock is stolen so a crashed run never wedges you.
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

The trigger is an existence guard (`[[ -x scripts/ci/preflight.sh ]]`), so it
no-ops in any repo that does not ship the script - a repo-neutral convention,
not a footnote hardcode. Skips are explicit and auditable:
`FNO_SKIP_PREFLIGHT=1`, or a docs-only diff (only documentation, the vault dir, and `*.md` files).
The scripts never self-skip; the skip decision lives in the caller.

## Running it yourself

```bash
scripts/ci/preflight.sh                 # full run against your committed HEAD
scripts/ci/preflight.sh --retry-failed  # fast: only last run's failures
bash scripts/ci/smoke.sh --keep-going   # non-hermetic, in your working tree
bash scripts/ci/smoke.sh --list         # what CI actually runs
```

`smoke.sh --keep-going` run directly in your working tree is the fast,
non-hermetic option for checking before you commit - the same registry, minus
the worktree isolation. Preflight refuses a dirty tree on purpose; that direct
smoke run is how you check uncommitted work.
