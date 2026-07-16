"""footnote: autonomous delivery loop for Claude Code.

The ``run_loop`` / ``target`` Python API has been removed. Drive work via
``/target`` in a Claude Code session instead.
"""
# Keep in lockstep with crates/fno and crates/fno-agents (Rust).
__version__ = "0.3.0"

__all__ = ["__version__"]


def __getattr__(name: str):
    if name in ("run_loop", "target"):
        raise AttributeError(
            f"fno.{name} has been removed: drive work via /target in a "
            "Claude Code session instead"
        )
    raise AttributeError(f"module 'fno' has no attribute {name!r}")
