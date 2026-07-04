"""Unit tests for the config.loops typed block (x-ce71).

A malformed level fails safe to "report" (observe only) rather than raising -
a standing loop must never silently upgrade its own autonomy from a config
typo.
"""
from __future__ import annotations

from fno.config import ConfigBlock, LoopEntry


def test_default_level_is_report():
    assert LoopEntry().level == "report"


def test_valid_levels_pass_through():
    assert LoopEntry(level="assisted").level == "assisted"
    assert LoopEntry(level="unattended").level == "unattended"


def test_unknown_level_fails_safe_to_report():
    assert LoopEntry(level="banana").level == "report"


def test_level_is_case_insensitive():
    assert LoopEntry(level="UNATTENDED").level == "unattended"


def test_config_block_loops_defaults_to_empty():
    assert ConfigBlock().loops == {}


def test_config_block_loops_parses_named_entries():
    cb = ConfigBlock(loops={"my-loop": {"level": "assisted"}})
    assert cb.loops["my-loop"].level == "assisted"


def test_config_block_loops_malformed_degrades_to_empty():
    cb = ConfigBlock(loops="not-a-mapping")
    assert cb.loops == {}
