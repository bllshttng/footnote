__all__ = ["plan_app"]


def __getattr__(name: str):
    if name == "plan_app":
        from fno.plan.cli import plan_app

        return plan_app
    raise AttributeError(f"module 'fno.plan' has no attribute {name!r}")
