"""fno do plan subcommands: stamp/graduate/set-expected are clients of the
keeper-served plan-doc writer."""

__all__ = ["plan_app"]


def __getattr__(name: str):
    # Lazy import: defer the typer-based CLI app so importing the stdlib-only
    # `_stamp` module (its residual atomic write) never pulls typer.
    if name == "plan_app":
        from fno.plan.cli import plan_app

        return plan_app
    raise AttributeError(f"module 'fno.plan' has no attribute {name!r}")
