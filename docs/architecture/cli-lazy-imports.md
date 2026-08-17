# fno CLI lazy-imports

The `fno` CLI is the agent-facing entry point for everything in the
footnote pipeline. A target session spawns it ~19 times per phase (state
reads, gate flips, event emits, postcondition verifiers). Each
invocation paid the full top-level import cost of every sub-app
regardless of which verb actually ran, dominating subprocess wall time.

This document describes the lazy-imports refactor that cuts that cost,
the design decisions baked into `cli/src/fno/_lazy_group.py`, and
the contracts a future change must preserve.

## Problem

Before the refactor, `cli/src/fno/cli.py` started with ~30 eager
imports of sub-app modules:

```python
from fno.state.cli import cli as state_app
from fno.graph.cli import cli as graph_app
# ... 28 more
app.add_typer(state_app, name="state")
# ... 28 more
```

Every `fno <verb>` invocation paid the union cost of importing all 30
sub-apps before even parsing `argv`. `fno --help` median wall time was
225ms p50; `fno paths state-dir` (the cheapest possible hot-path call)
was 206ms. Earlier measurement found the fno-daemon proposal would save ~0.22%
of target phase wall time and deferred the daemon in favor of this
simpler approach.

## Solution: Click LazyGroup, adapted for Typer

Click documents a lazy-loading-subcommands pattern: subclass
`click.Group`, store a `name -> "module:attr"` map, and import the
module only inside `get_command()`. Typer 0.9+ wraps Click and forwards
a `cls=` parameter to its underlying group, so the pattern applies via
`typer.Typer(cls=LazyTypeGroup, ...)`.

Two non-trivial adaptations were required:

### 1. `cls=` must subclass `TyperGroup`, not `click.Group`

Typer 0.24 asserts `issubclass(cls, TyperGroup)` when building the
top-level command, so `LazyTypeGroup` extends `typer.core.TyperGroup`
rather than `click.Group`. The lazy-loading methods (`list_commands`,
`get_command`, `resolve_command`) override the corresponding TyperGroup
implementations.

### 2. Stubs preserve help display without imports

Typer's rich help formatter calls `obj.get_command(ctx, name)` for every
listed command to read its short help. A naive implementation that
imports the module inside `get_command` would defeat the purpose:
`fno --help` would still pay the full import cost, just at help-display
time instead of module-load time.

The fix is `_LazyStub`, a placeholder `click.Group` returned by
`LazyTypeGroup.get_command()` for lazy entries. The stub carries only
the stored name, short help, hidden flag, and import path. Help display
reads `stub.help` directly and never touches the underlying module.

When the user actually invokes a lazy command, Click calls
`stub.make_context(info_name, args, parent)`. At that point the stub
imports the real module, gets the attribute, and delegates
`make_context` to the real command. Click's invocation loop then uses
`sub_ctx.command.invoke(sub_ctx)` where `sub_ctx.command` is the real
command set during `make_context`.

### 3. Single-command Typer apps need `get_group_from_info`

`typer.main.get_command(typer_app)` collapses a Typer app with exactly
one registered command into a bare `TyperCommand`. That changes the
invocation shape from `fno executor resolve <args>` to
`fno executor <args>` and breaks every nested call. The eager-load path
used `app.add_typer()` which never collapses, so the refactor must
preserve that shape.

`_LazyStub._load_real()` uses `typer.main.get_group_from_info()` for
Typer instances rather than `get_command()`. This keeps the group +
subcommand shape regardless of how many commands the sub-app
registered.

### 4. Parent-side overrides flow through `info_overrides`

`app.add_typer(sub, help="extended docs", invoke_without_command=True)`
attaches options at the parent registration site. Those options are
stored in a `TyperInfo` and applied when the parent builds the sub
group. With lazy loading the parent never sees a TyperInfo, so the
overrides would be lost.

The `LAZY_SUBCOMMANDS` map carries an optional third element with
options, e.g. `{"help": "...", "invoke_without_command": True}`. Those
options are forwarded to `TyperInfo(...)` inside `_LazyStub._load_real`
so `fno megawalk --help` keeps its extended exit-code documentation and
megawalk's bare-invocation behavior survives.

## Performance

Measured via 20-sample subprocess timing on `fno --help`:

| Metric  | Pre-refactor | Post-refactor |
|---------|--------------|---------------|
| p25     | -            | 107.7ms       |
| p50     | 225ms        | 110.0ms (-51%) |
| p75     | -            | 111.8ms       |

Baseline was pinned on 2026-05-14 and is hard-coded in
`cli/benchmarks/measure_cli_help.py` to avoid each branch comparing
against its own moving target. AC was a 30% drop (target 158ms);
shipped at 51% drop.

Re-bench guidance: run `rm -rf cli/.venv && uv sync` first to avoid
`__pycache__` confounds, then `python cli/benchmarks/measure_cli_help.py`.

## The reinstall-window hazard

Deferring an import is deferring a **disk read**. A running `fno` process holds a
partially-imported package and reaches back into `site-packages` at an arbitrary
later moment, so `uv tool install --reinstall` (what `fno update` runs) deletes
and rewrites the very tree that process is executing from. For the length of the
install, every subcommand the process has not yet imported fails.

This is a real, reproduced operator failure, not a theoretical one, and it took
three sessions to identify because it presents as three unrelated symptoms that
are one event:

| Symptom | Who the tenant is |
|---|---|
| `ModuleNotFoundError: No module named 'fno.graph._reconcile'` | the invoked subcommand's first-time import |
| `ImportError: cannot import name 'rich_utils' from 'typer'` | the **error-reporting path itself** |
| `/bin/sh: .../fno-py: No such file or directory` | the post-install chain's exec, same window |

The second one is what made this expensive. Typer defers `from . import
rich_utils` to exception time in two places: `typer.main.except_hook` (gated by
`pretty_exceptions_enable`) and the `ClickException` arm of `typer.core._main`
(gated by `rich_markup_mode`, which defaults to `"rich"`). Both are first-time
imports on the failure path, so the reporter died inside the same window and the
operator saw the reporter's own error instead of the cause. `cli.py` disables
both. Disabling only one leaves the live carrier untouched: the `ClickException`
arm is the path a lazy-import failure actually travels.

Eagerly importing `typer.rich_utils` to make it resident is not an option: it
costs ~237ms, which is more than the entire startup budget this refactor bought.
Disabling it is also a small win on the help path, which was paying that import.

### Reproducing it

Start `uv tool install --reinstall --refresh <repo>/cli` against the tool dir,
and inside that window invoke an `fno` subcommand whose module the running
process has not yet imported. The lazy group guarantees the import lands in the
window.

A synthetic loop that re-imports modules already resident in `sys.modules` will
**not** reproduce it: a cached import never touches the disk. That false negative
is what kept this misdiagnosed. There is no automated test for this; racing a
multi-second 18-package install to re-prove an understood mechanism buys less
than it costs, so the recipe lives here for hand-running instead.

### What is and is not fixed

Fixed, in four layers.

**Legibility.** The error path performs no first-time import, and a missing
module under the `fno` package says so and names both candidate causes (a
reinstall in flight, retry; a stale install, `fno update` then `fno doctor`).

**Verify-then-retry.** On an `ImportError` for a module under the `fno` package,
`_load_real` re-checks whether that module is on disk *now* and, if it is,
retries the import exactly once. The disk re-check is what separates this from a
hopeful sleep-retry: a genuinely stale or broken install answers "absent", is not
retried, and fails with the same message as before, so nothing is masked. This
matters because the window is not rare on a real machine. A box running several
launchd agents plus live sessions nearly always has an `fno` process mid-flight
during the few seconds `uv tool install --reinstall` takes, so every `fno update`
was hitting it.

**The same retry, on the other import path.** `_load_real` guards the lazy command-group import. That is one path of two. The other one carries far more traffic. It is a deferred `from fno. ...` written inside a command body, and there are about 2000 of those. `fno agents truth` reaches `fno.agents.session_truth` that way. The fno-agents daemon runs that verb as a continuous per-session liveness probe, so it is the highest-frequency reader of the window. Those sites used to fail with a bare `ModuleNotFoundError`, no retry and no dual-cause message. A guard on one of two reachable paths is decorative, so the guard moved to the one site both paths cross.

`fno/__init__.py` appends a meta-path finder, `_ReinstallWindowFinder`, at the END of `sys.meta_path`. Every `fno.*` import imports `fno` first, so no caller can miss it. That covers the console script, `python -m fno.cli`, a spawned worker, and all 2000 in-body imports. Being last means it is consulted only after every normal finder has already answered "no such module". So it costs nothing on the happy path. At that point it runs the same `_module_is_now_on_disk` re-check `_load_real` runs, then asks `PathFinder` one more time. Present now, the import proceeds. Still absent, it raises the dual-cause message instead of the bare error. An absent module is neither slept on nor masked. Both paths share one check and one message, and both live in `fno/__init__.py`. A second copy rebuilds the same one-of-N trap.

Measured cost: `import fno` self-time rises from 99us to 171us per process. That is 0.07% of a 110ms `fno --help`. This file carries no `from __future__ import annotations` for the same reason. That import alone measured 154us, and nothing in the module needs it.

The re-check itself is not free, and the number is worth stating. `importlib.invalidate_caches()` ends in `from importlib.metadata import MetadataPathFinder`, so its first call in a process that has not already loaded that module pulls in `importlib.metadata`, `email` and `zipfile`. Measured on a bare interpreter: 17.9ms and 84 modules for that first call, 0.003ms for every call after it. The figure moves with what the process has already imported, so read it as tens of milliseconds, once. Every one of them is paid on an import that has already failed, so no successful path sees any of it.

**One transient failure tolerated at the daemon probe.** The truth probe is the loudest victim of the window. `family1_truth_probe` in `crates/fno-agents/src/claude_ask.rs` now retries once before it warns. The discriminator is narrow on purpose. It fires on a non-zero exit with NO parseable body on stdout, and on a failure to spawn at all. Both shapes mean the process never reached the code that writes a verdict. The reinstall replaces the `fno` console script as well as the tree behind it. So ENOENT on spawn is as much a face of the window as an import crash. Guarding only the crash reaches one shape of two. A refusal always carries its `{state, reason}` body. So the volume case, `not-found`, still answers on the first attempt. No dead registry row pays for a second process. A probe that crashes twice keeps its WARN. This tolerates a transient failure. It does not hide a persistent one.

**The exposure gap (symptom 3).** `/bin/sh: .../fno-py: No such file or directory`
fails in the shell before any interpreter starts, so no import-level retry can
reach it. Measured by sampling every 5ms across a reinstall: the venv's `python3`
is present in every sample, so the shebang interpreter is never the cause; what
vanishes is the console script `<tools>/fno/bin/fno-py`, for ~490ms, taking the
`~/.local/bin` exposure symlink with it. That gap closed only ~40ms before uv
exited on an idle machine, so `fno update`'s `&&`-gated post-install chain is
correct in principle and much too tight in practice. `update.py` now waits up to
3s for the console script before running the refresh, and if it never returns,
skips loudly with the manual commands rather than leaving a launchd agent pinned
to the old binary.

**The same wait, on all four verify sites.** Naming `update.py` alone was itself a one-of-N gap, and it cost a second bug. FOUR places install the tool and then verify it, and every one of them verified single-shot, the instant uv returned. Each therefore raced the install it was verifying. The symptom on a real box: `fno` refused every verb, reran a full 19-package install each time, and reported "the install does not verify: no console script". The sites are `install_verified` in `crates/fno/src/bootstrap.rs`, `uv_install_verifies` in `.claude-plugin/postinstall.sh` and again in `scripts/install/fno.sh`, and the verify inside `_uv_retry_sh` in `cli/src/fno/update.py`. That last one is the sharpest lesson. `update.py` was believed fixed because `_await_binary` waits. That wait runs AFTER the verify, and the verify's `exit 1` short-circuits the chain, so `_await_binary` was never reached.

**The sentinel is what made it a storm.** The verify explains the refusal. It does not explain why every later call re-provisioned too. `run()` in the same file takes a fast path off a recorded sentinel. It read a single `is_executable` miss as "wheel uninstalled" and DELETED the sentinel. That removes the fast path for the next process too. Each one then re-provisions with its own `--force`, re-breaking the tree for the others. That is the shape that reached 100 concurrent uv processes on 2026-08-16. The sentinel has TWO arms that drop it, and the second is the one a racing install actually reaches. `uv tool install --force` recreates the console script, so the recorded mtime never matches. Control lands on `verify_ours`, which runs the venv's own python against a venv still being rewritten. Both arms now re-check on the same budget. So do the other two `verify_ours` callers in the same function. The adopt-an-existing-install path propagates with `?`, so one unlucky probe became a hard exit. The post-install verify writes a stamp on failure, which refuses every call for ten minutes. A genuinely foreign package fails every pass and is still refused, so the "never run a foreign fno" invariant is unchanged.

Two limits worth stating rather than discovering. The 3s ceiling comes from an idle-machine measurement, and the incident it addresses had 100 concurrent uv processes. Under that load a `--force` reinstall of 19 packages can leave the script absent past 3s, and the sentinel is dropped anyway. Raising the ceiling is a fleet-wide tuning decision, so it is filed rather than guessed at here. The second limit is that 3s is the per-waiter budget, not the wall time of a call. The sentinel path spends `executable_within` and then `verify_ours_within` in sequence, so it gives up at 6s. A call that falls through to the adopt path pays that path's own two waits as well. So one `fno` invocation can spend 12s before it provisions or refuses. A call that goes on to provision pays three more waits in sequence: `install_verified_within` inside `install_wheel`, then the post-install `executable_within` and `verify_ours_within`. The ceiling for one invocation is therefore about 21s of waiting, on top of the install itself. Size any harness budget that shells `fno` against 21s, not 3s. That is deliberate. Each wait starts only once the previous one has seen its own subject come back. The alternative to waiting is the reinstall storm this change exists to stop. Read 3s as one waiter's ceiling, never as the ceiling for a call.

One refusal is exempt, and deliberately so. A foreign package at the resolved path is refused on the first pass rather than the last. Its probe SUCCEEDED and named a stranger, so no amount of waiting rewrites that answer. Retrying it charges every later call the full budget plus 16 spawns of the stranger's own interpreter. The identity refusal returns above `write_failure_stamp`, so the stamp never relieves it. Only instrument failures are retried: a venv python missing mid-rewrite, a probe that will not spawn, metadata being rewritten. `BootErr::stable` carries that distinction and a unit test counts the spawns, because the difference between one and sixteen is invisible in a passing suite. What earns the exemption is a COMPLETE answer, not every refusal `decide_identity` returns. A dist-info whose METADATA is still half-written parses fine and yields no Name and no Version. The old rule refused that as `name=` and stamped it for the whole cooldown. So the probe now reports an absent header as an empty line. The stability check asks for a name and a version before it calls a verdict final.

All four now spend the same 15 times 0.2s. All four RE-CHECK rather than sleeping blind, so a genuinely broken install still fails with the message it always had. The budget is duplicated across Rust, bash, and emitted POSIX sh, because no implementation crosses those boundaries. `tests/ci/test_uv_install_verify_wait.sh` reads all four files and fails on drift, so the pin is a test rather than a comment.

What the meta-path finder closed is the COVERAGE gap, not the window. Every `fno.*` import now gets the same one retry. The paragraph below still holds.

The message reaches one shape less than the retry does. For `from fno.pkg import submodule`, CPython's `_handle_fromlist` swallows our ModuleNotFoundError and raises `cannot import name ... from ...` in its place. Nothing at this layer can reach that decision. The retry still runs, because it happens inside `find_spec` before the exception exists. The in-body imports are written as `from fno.pkg.submodule import name`, which keeps the message. Raising from a finder also inverts one stdlib contract: `importlib.util.find_spec` on an absent `fno.*` module raises rather than returning None. No caller in this repo probes an fno module that way, and the error stays an ImportError subclass.

Not fixed, and unfixable at this layer: the window itself. A process whose import
lands while the file is genuinely still absent still fails. Closing that would
require quiescing running `fno` processes or new cross-process state, both of
which cost more than a transient, self-healing failure. Specifically rejected:

- **A provision lock.** uv already serializes tool installs (measured: 8 rounds
  of concurrent `--reinstall --refresh` against `--force` on the real
  18-package source, 0 failures, `bin/fno-py` exposure intact every round).
  There is one installer here, not two. A lock would serialize nothing.
- **Eager-importing subcommand modules.** That deletes the lazy group to avoid a
  transient failure.
- **Retrying the failed import after a sleep.** A blind delay hides a genuinely
  stale or broken install. The shipped retry is a different thing and the
  distinction is the point: it retries only after confirming on disk that the
  module is present, so an absent module is never waited on and never masked.

### The bytecode-write race (fixed at install time)

A plain `uv tool install` ships zero `.pyc`. Every process that runs out of the tool venv then writes `__pycache__` bytecode into `site-packages` as it imports. The pr-watch daemon on its timer, every hook, every manual call. A later `--force`/`--reinstall` deletes that tree. New `.pyc` entries appear behind uv's walk, the closing `rmdir` returns ENOTEMPTY (`os error 66`), and no entrypoint is left behind. The failed install is sticky. The next `fno` call re-attempts it and hits the same error, so the CLI stays down until an install lands in a quiet window. This is the mechanism behind the 2026-08-14 CLI outage, measured at 427 `.pyc` files written into the venv during the failure window.

Every provisioning site therefore passes `--compile-bytecode`. The venv ships its own bytecode up front, and a matching `.pyc` gives no process a reason to write. The steady-state writer is gone for every caller at once. `PYTHONDONTWRITEBYTECODE` was rejected as the fix because it protects only the processes that remember to set it. `scripts/ci/check-uv-install-compiles-bytecode.sh` keeps this true: it fails CI on any `uv tool install` run site without the flag. Its `--self-test` proves the search matches a real invocation rather than certifying an empty result. One residual window remains and is measured, not assumed. During the removal phase of a `--force` install, an import can land after uv deleted a module's `.pyc` but before its `.py`. It recompiles and rewrites that `.pyc` behind the walk, so ENOTEMPTY stays reachable: 3 of 6 bare installs under five concurrent import loops. The provisioning paths absorb that with a bounded retry on exactly this signature. The retry accepts success only through a positive marker, the `fno-py` console script plus shipped bytecode, never the exit code alone. The bytecode marker is any nonzero count, deliberately. A hard count is brittle across versions, and a pinned baseline count recreates the stale-baseline trap the CI gate already had to fix once. Precompile shrinks the race from continuous writes to that sub-second removal window. The retry closes what remains of it for the install paths.

## Contracts (do not break)

A future refactor must preserve:

1. `fno --help` does not import sub-app bodies. Test: `tests/test_lazy_imports.py::test_fno_help_does_not_import_sub_app_modules`.
2. `fno paths state-dir` does not import the heavy sub-apps. Test: `test_fno_paths_does_not_import_heavy_subapps`.
3. Single-command sub-apps keep their group shape. Test: `test_executor_resolve_group_shape_preserved`.
4. Parent-side `add_typer` overrides survive lazy loading (see `info_overrides` above). Test: `test_executor_resolve_group_shape_preserved` covers the group shape. The overrides themselves are exercised by the `--help` tests.
5. Misconfigured lazy entries fail loudly with the bad path in stderr. Tests: `test_bad_lazy_entry_fails_loud`, `test_bad_module_path_fails_loud`.
6. The error path never first-imports `typer.rich_utils` (see the reinstall-window hazard above). Tests: `test_error_path_never_first_imports_rich_utils`, `test_building_the_command_does_not_import_rich_utils`.
7. A missing module under the `fno` package explains itself and names both causes. A missing third-party dependency collects no reinstall speculation. Tests: `test_fno_module_import_failure_names_reinstall_window`, `test_third_party_import_failure_has_no_reinstall_hint`.

Adding a new sub-app: add one line to `LAZY_SUBCOMMANDS` in `cli.py` with the import path and a short help string. Run the test suite to confirm coverage. No changes to `_lazy_group.py` are required for a normal sub-app addition.
