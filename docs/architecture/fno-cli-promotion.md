# fno CLI Promotion: Typer Wrappers Around scripts/lib/

The `fno` CLI promotes long-tail bash helpers under `scripts/lib/` to a discoverable Typer surface. The bash scripts remain canonical; the wrappers are a thin forwarding layer that gives LLMs and humans `fno <verb> --help` instead of having to read `scripts/lib/*.sh` source.

This document describes the pattern introduced in PR 1 (8 new verbs) and the constraints downstream PRs inherit.

## Why a Wrapper Layer

The pipeline executes from inside skill bodies that source canonical bash scripts directly:

```
bash scripts/lib/set-gate.sh --invoked-by canonical-skill .fno/target-state.md \
     quality_check_passed true review agents_dispatched='[...]'
```

Three problems with that surface:

1. The argv parser lives at the top of the bash file. There is no `--help` discoverability for an LLM that has not read the source.
2. Enum values (`--invoked-by canonical-skill|inline-equivalent|substituted-executor`) reject at runtime with a bash error, not at parse time with a Typer error pointing at the bad position.
3. Direct script invocations spread across `skills/`, `hooks/`, `references/`, and lint remediation messages. A future cross-skill refactor has to touch every call site.

A thin Typer wrapper that forwards args verbatim closes all three: `--help` is auto-generated, enum members reject at parse time with rc=2, and a single `fno <verb>` symbol becomes the migration target.

## The Pattern

Each wrapper follows the same shape:

```python
# cli/src/fno/pr/cli.py
def _run_script(script_relpath: str, args: List[str]) -> int:
    script = resolve_repo_root() / script_relpath
    if not script.is_file():
        typer.echo(
            f"fno pr: canonical script not found: {script}\n"
            "Is the footnote plugin installed correctly? "
            "Set FNO_REPO_ROOT to the plugin root if running outside the repo.",
            err=True,
        )
        return 2
    cmd = ["bash", str(script)] + args
    result = subprocess.run(cmd, check=False)
    return propagate_returncode(result.returncode)
```

Three invariants:

| Invariant | Where enforced |
|-----------|----------------|
| Wrapper does NOT diverge from the canonical script | `cli/tests/unit/test_cli_wrappers_*.py` monkeypatches `subprocess.run`, asserts `[bash, <abs path>, *user_args]` layout |
| Exit codes propagate unchanged | Same tests assert `result.exit_code == returncode_returned_by_stub` for at least one non-zero value |
| Signal-killed children produce shell-convention exit codes | `cli/src/fno/_subprocess_util.py::propagate_returncode` normalises `-9 -> 137`, `-15 -> 143` |

Two non-trivial shapes deviate slightly:

- **`fno pr verify --kind merged|reviews`** uses a Python `enum.Enum` so Typer rejects unknown values at parse time. Dispatch on the enum picks the right script (`verify-pr-merged.sh` vs `verify-review-replies.sh`).
- **`fno executor resolve`** chains `parse-locked-executor.sh` (stdin: design doc) then `infer-task-executor.sh` (stdin: file list) to implement the three-tier executor resolution chain. This is the one wrapper with non-trivial dispatch logic; both sub-scripts get the captured-output + check-returncode treatment so a failure surfaces as rc=2 rather than silent fall-through.

Sourceable bash libraries (e.g., `notify.sh`, `phase-verifier.sh`) are called via:

```python
cmd = [
    "bash", "-c",
    'source "$1"; <fn_name> "$2" "$3"',
    "<wrapper-label>",   # $0
    str(script_path),    # $1
    arg1, arg2,          # $2, $3
]
```

The wrapper validates `script_path.is_file()` before the subprocess so a missing canonical produces rc=2 with a clear stderr message instead of a bash-level "file not found" inside the subshell.

## The 8 Verbs in PR 1

| Verb | Wraps | Notes |
|------|-------|-------|
| `fno gate set` | `scripts/lib/set-gate.sh` | atomically flip gate bool + emit phase_transition event |
| `fno pr verify --kind merged\|reviews` | `verify-pr-merged.sh` OR `verify-review-replies.sh` | enum dispatches to the right script |
| `fno pr rebase` | `scripts/lib/rebase-resolve.sh` | conflict-delegation rebase protocol |
| `fno event verify-evidence SESSION_ID NONCE EVENTS_FILE ARTIFACT_PATH` | `fno-agents verify-evidence event` (binary; logic folded out of the deleted `verify-event-evidence.sh` in US1) | stop-hook rc=2 fallback for non-Claude providers |
| `fno phase verify PHASE_NAME [--session-id]` | `phase-verifier.sh::pv_run` (sourceable) | per-phase postcondition verifier |
| `fno phase kill-check [PLAN_PATH]` | `fno-agents kill-check` (binary; logic folded out of the deleted `kill-criteria.sh` in US1) | plan kill-criteria evaluator |
| `fno executor resolve [--plan-path] [--task-files] [--explain]` | `parse-locked-executor.sh` then `infer-task-executor.sh` | three-tier executor resolver |
| `fno notify TITLE MESSAGE` | `notify.sh::notify` (sourceable) | OS notification helper |

All eight verbs expose the same return-code contract as the underlying bash: rc=0 success, rc=1 logical failure, rc=2 substrate failure (or invalid args via Typer), other codes pass through with `propagate_returncode` applied to handle negative signal-derived values.

## The Drift Lint

`scripts/lint/no-unwrapped-lib-scripts.sh` walks `scripts/lib/*.sh` and `*.py`, excludes pure-library helpers via `.unwrapped-lib-allowlist.txt`, and warns when a remaining script has no `fno <verb>` mention in `cli/src/fno/**/*.py`. Today the lint emits warnings only (rc=0 always); the `.github/workflows/cli-ci.yml` job is wired with `continue-on-error: true`.

The advisory mode is intentional for PR 1. PR 2 (the canonical-instruction sweep) will migrate every direct `bash scripts/lib/X.sh` invocation in skill bodies, references, and hooks to the wrapped surface. Once that lands, the lint flips to a hard CI fail.

When you add a new helper to `scripts/lib/`, the lint will warn until you either:

1. Add a matching `fno <verb>` wrapper in `cli/src/fno/<noun>/cli.py` (the normal path), or
2. Add the basename to `scripts/lint/.unwrapped-lib-allowlist.txt` if the script is a sourced library, not a CLI entry point.

## Constraints PR 2 Inherits

The wrapper pattern documented here is the migration target for PR 2's sweep. A few constraints that follow from this PR's design:

- **No path re-resolution in wrappers.** Each wrapper resolves the canonical via `resolve_repo_root() / "scripts/lib/X.sh"`. Callers do not pass the script path - they call `fno <verb>` and Typer dispatch handles the resolution.
- **No argument re-validation.** When the bash script has its own enum check (e.g., `--invoked-by canonical-skill|inline-equivalent|substituted-executor`), the wrapper forwards the value verbatim and lets bash reject it. The exception is the dispatch case (`fno pr verify --kind`) where the enum picks the script; there the wrapper validates because the dispatch depends on it.
- **No state changes outside the canonical.** Wrappers do not flip target-state.md booleans, write artifacts, or emit events. Those side effects live in the bash scripts (with their `mkdir`-mutex locks and atomic-rename writes); the wrapper is a transport layer.
- **No buffering.** Wrappers pass `stdout`/`stderr` through to the parent process by default. The `fno executor resolve` exception uses `capture_output=True` because the chained scripts produce a single-token answer that the wrapper consumes; it surfaces stderr explicitly on rc!=0.

## Related Work

- **The gate-honesty followups** locked in the three-factor gate verification model: state bool + session-scoped artifact + provenance event. The `fno gate set` wrapper makes the canonical helper that flips all three discoverable.
- **The gate-honesty work** introduced the HARD-GATE skill preambles that block direct edits to gate artifacts and graph.json. The wrappers do not bypass those guards; they invoke the canonical helpers that the guards expect.
- **PR 2 (cli-promotion-sweep, planned)** migrates every direct `bash scripts/lib/X.sh` call to `fno <verb>`. Until that lands the drift lint stays advisory.

## Files

- `cli/src/fno/{gates,pr,events,phase,executor,notify}/cli.py` - wrapper modules
- `cli/src/fno/_subprocess_util.py` - shared `propagate_returncode`
- `cli/tests/unit/test_cli_wrappers_*.py` - per-verb tests (forwards-args-verbatim, missing-canonical-rc-2, etc.)
- `cli/tests/unit/test_cli_wrappers.py` - cross-wrapper smoke (`--help` for every new verb)
- `scripts/lint/no-unwrapped-lib-scripts.sh` + `.unwrapped-lib-allowlist.txt` - drift lint
- `.github/workflows/cli-ci.yml` - lint hooked as advisory job
