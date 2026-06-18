"""Tests for the one-release env back-fill (fno._compat_env).

This file intentionally references the OLD env prefixes (ABILITIES_/ABI_); it is
exempt from the rename sweep + residual-grep guard (see EXCLUDES / KEEP_FILES in
scripts/rename/).
"""
from __future__ import annotations

from fno._compat_env import backfill_legacy_env


def test_abi_prefix_maps_to_fno():
    env = {"ABI_REPO_ROOT": "/x"}
    filled = backfill_legacy_env(env)
    assert env["FNO_REPO_ROOT"] == "/x"
    assert ("ABI_REPO_ROOT", "FNO_REPO_ROOT") in filled


def test_abilities_prefix_maps_to_fno():
    env = {"ABILITIES_HOME": "/h"}
    backfill_legacy_env(env)
    assert env["FNO_HOME"] == "/h"


def test_abi_agents_nested_prefix_preserved():
    # ABI_AGENTS_HOME -> FNO_AGENTS_HOME (strip ABI_, keep AGENTS_*)
    env = {"ABI_AGENTS_HOME": "/d"}
    backfill_legacy_env(env)
    assert env["FNO_AGENTS_HOME"] == "/d"


def test_explicit_new_name_wins():
    # An explicitly-set FNO_* is never overwritten by the legacy value.
    env = {"ABI_DEBUG": "old", "FNO_DEBUG": "new"}
    filled = backfill_legacy_env(env)
    assert env["FNO_DEBUG"] == "new"
    assert filled == []


def test_no_legacy_is_noop():
    env = {"PATH": "/usr/bin", "FNO_HOME": "/h"}
    filled = backfill_legacy_env(env)
    assert filled == []
    assert env == {"PATH": "/usr/bin", "FNO_HOME": "/h"}
