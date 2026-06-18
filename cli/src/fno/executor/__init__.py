"""fno executor - wrapper around the three-tier executor resolver chain."""

__all__ = ["executor_app"]


def __getattr__(name: str):
    # Lazy import: the stdlib-only _surface / _locked submodules are run via
    # `python3 -m fno.executor._surface` from in-clone bash hooks (infer-has-ui,
    # resolve-plan-executor, the frontend-craft gate harness). Importing the
    # typer-based CLI app eagerly here would make those fail in a minimal
    # environment that has no typer. Defer it so only an actual `fno executor`
    # invocation (where typer is present) pays for it.
    if name == "executor_app":
        from fno.executor.cli import executor_app

        return executor_app
    raise AttributeError(f"module 'fno.executor' has no attribute {name!r}")
