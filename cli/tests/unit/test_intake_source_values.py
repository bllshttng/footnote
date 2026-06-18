"""INTAKE_SOURCE_VALUES exports a frozen set with both source spellings."""
from __future__ import annotations

import pytest


def test_constant_contains_both_spellings():
    from fno.graph._intake import INTAKE_SOURCE_VALUES
    assert "adopt" in INTAKE_SOURCE_VALUES
    assert "intake" in INTAKE_SOURCE_VALUES


def test_constant_is_frozenset():
    from fno.graph._intake import INTAKE_SOURCE_VALUES
    assert isinstance(INTAKE_SOURCE_VALUES, frozenset)
    with pytest.raises(AttributeError):
        INTAKE_SOURCE_VALUES.add("imported")


def test_membership_for_known_values():
    from fno.graph._intake import INTAKE_SOURCE_VALUES
    for value in ("adopt", "intake"):
        assert value in INTAKE_SOURCE_VALUES


def test_membership_rejects_unknown_values():
    from fno.graph._intake import INTAKE_SOURCE_VALUES
    for value in (None, "imported", "manual", "", "Adopt"):
        assert value not in INTAKE_SOURCE_VALUES


def test_constant_size_is_two():
    """Locks the contract that exactly two spellings are accepted right now.

    A future plan adding a third source value (e.g. "auto-discovered") would
    fail this test and force the author to update the back-compat reasoning.
    """
    from fno.graph._intake import INTAKE_SOURCE_VALUES
    assert len(INTAKE_SOURCE_VALUES) == 2
