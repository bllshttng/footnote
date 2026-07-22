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

## Contracts (do not break)

A future refactor must preserve:

1. `fno --help` does not import sub-app bodies. Test:
   `tests/test_lazy_imports.py::test_fno_help_does_not_import_sub_app_modules`.
2. `fno paths state-dir` does not import `megawalk` or `megatron`.
   Test: `test_fno_paths_does_not_import_megawalk`.
3. Single-command sub-apps keep their group shape. Test:
   `test_executor_resolve_group_shape_preserved`.
4. Megawalk and megatron's extended exit-code documentation appears in
   `fno <verb> --help`. Tests:
   `test_megawalk_help_carries_exit_codes`,
   `test_megatron_help_carries_exit_codes`.
5. Misconfigured lazy entries fail loudly with the bad path in stderr.
   Tests: `test_bad_lazy_entry_fails_loud`, `test_bad_module_path_fails_loud`.

Adding a new sub-app: add one line to `LAZY_SUBCOMMANDS` in `cli.py`
with the import path and a short help string. Run the test suite to
confirm coverage. No changes to `_lazy_group.py` should be required for
a normal sub-app addition.
