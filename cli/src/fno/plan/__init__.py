"""fno plan subcommands - thin wrappers over the in-package fno.plan._stamp module."""

__all__ = ["plan_app"]


def __getattr__(name: str):
    # Lazy import: fno.plan._stamp is run via `python3 -m fno.plan._stamp` from
    # Rust finalize.rs and the stamp bash tests, in environments that may not
    # have typer. Defer the typer-based CLI app so importing the stdlib-only
    # _stamp module never pulls typer.
    if name == "plan_app":
        from fno.plan.cli import plan_app

        return plan_app
    raise AttributeError(f"module 'fno.plan' has no attribute {name!r}")
