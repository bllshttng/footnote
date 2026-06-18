"""
Megawalk driver registry.

Provides DRIVERS dict and resolve_driver() for selecting an agent CLI driver
by name or via auto-detection from PATH.
"""
from __future__ import annotations

from .base import Driver, DriverError, InvokeResult, NoCapableDriver, UnsupportedDriverMode
from .claude_code import ClaudeCodeDriver
from .hermes import HermesDriver
from .openclaw import OpenclawDriver

DRIVERS: dict[str, type] = {
    "claude-code": ClaudeCodeDriver,
    "hermes": HermesDriver,
    "openclaw": OpenclawDriver,
}

# Auto-detect preference order
_AUTODETECT_ORDER = ["claude-code", "hermes", "openclaw"]


def resolve_driver(name: str | None = None, *, env: dict | None = None) -> Driver:
    """Resolve a driver by name or auto-detect from PATH.

    Parameters
    ----------
    name:
        Explicit driver name ('claude-code', 'hermes', 'openclaw').
        Pass None to auto-detect.
    env:
        Unused; reserved for future env-based override injection.

    Raises
    ------
    ValueError
        If an explicit name is given that is not in DRIVERS.
    RuntimeError
        If an explicit driver is requested but its binary is not on PATH,
        or if auto-detection finds no available driver at all.
    """
    if name is not None:
        if name not in DRIVERS:
            raise ValueError(
                f"Unknown driver: {name!r}. Choices: {list(DRIVERS)}"
            )
        driver = DRIVERS[name]()
        if not driver.is_available():
            raise RuntimeError(
                f"Driver '{name}' requested but binary not on PATH"
            )
        return driver

    # Auto-detect: prefer claude-code, fall back to hermes, then openclaw.
    for candidate in _AUTODETECT_ORDER:
        driver = DRIVERS[candidate]()
        if driver.is_available():
            return driver

    raise RuntimeError(
        "No driver available on PATH; install claude / hermes-agent / openclaw"
    )


__all__ = [
    "Driver",
    "DriverError",
    "InvokeResult",
    "NoCapableDriver",
    "UnsupportedDriverMode",
    "ClaudeCodeDriver",
    "HermesDriver",
    "OpenclawDriver",
    "DRIVERS",
    "resolve_driver",
]
