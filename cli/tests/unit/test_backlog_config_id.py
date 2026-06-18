"""Unit tests for config.backlog.id_prefix + id_hex_width (ab-bbfccb8f, T1.1).

The configurable node-ID scheme. Generation is strict (these validated config
fields drive minting); resolution stays liberal elsewhere. Schema defaults are
the LEGACY values (id_prefix absent -> ab- fallback at the accessor; hex width
8) so an existing config.backlog block with no id keys is byte-identical.

Filter: `uv run pytest cli/tests -k backlog_config_id -q`
"""
from __future__ import annotations

import pytest
from pydantic import ValidationError

from fno.config import BacklogBlock, ConfigBlock


# --- defaults: legacy preservation ----------------------------------------


def test_default_id_prefix_is_none():
    # None means "not configured" -> the accessor falls back to legacy ab-.
    assert ConfigBlock().backlog.id_prefix is None
    assert BacklogBlock().id_prefix is None


def test_default_id_hex_width_is_8_legacy():
    # AC3-FR: an absent width resolves to the legacy 8, NOT the wizard's 4.
    assert ConfigBlock().backlog.id_hex_width == 8
    assert BacklogBlock().id_hex_width == 8


# --- id_prefix: accept + normalize ----------------------------------------


def test_prefix_accepts_and_normalizes_trailing_dash():
    assert BacklogBlock(id_prefix="xy").id_prefix == "xy-"
    assert BacklogBlock(id_prefix="xy-").id_prefix == "xy-"
    assert BacklogBlock(id_prefix="fno-").id_prefix == "fno-"


def test_prefix_lowercases():
    assert BacklogBlock(id_prefix="XY").id_prefix == "xy-"


# --- id_prefix: reject (AC1-ERR) ------------------------------------------


@pytest.mark.parametrize("bad", ["cv-", "CV-", "fu-", "tgt-", "fu", "tgt"])
def test_prefix_rejects_reserved_families(bad):
    with pytest.raises(ValidationError):
        BacklogBlock(id_prefix=bad)


@pytest.mark.parametrize("bad", ["a b", "1ab", "", "  ", "-", "x_y", "toolongprefix"])
def test_prefix_rejects_malformed(bad):
    with pytest.raises(ValidationError):
        BacklogBlock(id_prefix=bad)


# --- id_hex_width: accept + reject (AC1-EDGE) -----------------------------


@pytest.mark.parametrize("w", [4, 5, 6, 7, 8])
def test_hex_width_accepts_in_range(w):
    assert BacklogBlock(id_hex_width=w).id_hex_width == w


@pytest.mark.parametrize("bad", [0, 3, 9, 40, -1])
def test_hex_width_rejects_out_of_range(bad):
    with pytest.raises(ValidationError):
        BacklogBlock(id_hex_width=bad)


def test_hex_width_rejects_non_int():
    with pytest.raises(ValidationError):
        BacklogBlock(id_hex_width="four")
