"""Unit tests for the config.loops typed block (x-ce71).

A malformed level fails safe to "report" (observe only) rather than raising -
a standing loop must never silently upgrade its own autonomy from a config
typo.
"""
from __future__ import annotations

from fno.config import ConfigBlock, LoopEntry


def test_unknown_level_fails_safe_to_report():
    assert LoopEntry(level="banana").level == "report"


def test_config_block_bad_entry_shape_is_dropped_not_raised():
    """A per-loop entry that isn't a mapping must not crash the whole load.

    Regression for codex peer review P2: ``ConfigBlock(loops={...})`` used to
    raise a pydantic ValidationError for a bare-string or null entry, which
    would break `load_settings()` (and therefore every `fno` command) for a
    project with one config typo.
    """
    cb = ConfigBlock(loops={"good-loop": {"level": "assisted"}, "bad-loop": "assisted"})
    assert cb.loops["good-loop"].level == "assisted"
    assert "bad-loop" not in cb.loops
