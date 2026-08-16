"""footnote: autonomous delivery loop for Claude Code.

The ``run_loop`` / ``target`` Python API has been removed. Drive work via
``/target`` in a Claude Code session instead.
"""
# Keep in lockstep with crates/fno and crates/fno-agents (Rust).
__version__ = "0.3.1"

__all__ = ["__version__"]

# Armed here, at the one line every entry into the package crosses: the imports
# it guards are written inside command bodies, so there is no later hook that
# all of them pass through.  See fno._import_guard.
from fno._import_guard import install_reinstall_window_finder as _install_guard

_install_guard()


def __getattr__(name: str):
    if name in ("run_loop", "target"):
        raise AttributeError(
            f"fno.{name} has been removed: drive work via /target in a "
            "Claude Code session instead"
        )
    raise AttributeError(f"module 'fno' has no attribute {name!r}")
