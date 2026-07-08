"""Setup-wizard config-write path for the node-ID scheme (ab-bbfccb8f, T3.1).

The `/setup` wizard collects the prefix (required) + hex width (default 4) and
persists them via ``fno config set`` -> ``set_config_value``. These tests cover
that write path: AC1-HP (writes the chosen scheme), AC1-ERR (rejects an invalid
prefix, file untouched), AC1-EDGE (rejects an out-of-range width).

Filter: `uv run pytest cli/tests -k setup_id -q`
"""
from __future__ import annotations

import pytest
import tomllib

from fno.config.writer import ConfigSetError, set_config_value


def _read(tmp_path):
    return tomllib.loads((tmp_path / ".fno" / "config.toml").read_text())


def test_setup_id_writes_prefix_normalized(tmp_path):
    from fno.config import BacklogBlock

    set_config_value(
        "config.backlog.id_prefix", "xy", scope="project", repo_root=tmp_path
    )
    stored = _read(tmp_path)["backlog"]["id_prefix"]
    # The writer stores the accepted value; the field validator normalizes to a
    # trailing dash on read, so it round-trips to "xy-".
    assert BacklogBlock(id_prefix=stored).id_prefix == "xy-"


def test_setup_id_writes_hex_width(tmp_path):
    res = set_config_value(
        "config.backlog.id_hex_width", "4", scope="project", repo_root=tmp_path
    )
    assert res.value == 4
    assert _read(tmp_path)["backlog"]["id_hex_width"] == 4


@pytest.mark.parametrize("bad", ["cv-", "CV-", "tgt-", "a b", "x_y"])
def test_setup_id_rejects_invalid_prefix(tmp_path, bad):
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.backlog.id_prefix", bad, scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 2
    # AC1-FR/ERR: nothing written on a rejected value.
    assert not (tmp_path / ".fno" / "config.toml").exists()


@pytest.mark.parametrize("bad", ["0", "3", "9", "four"])
def test_setup_id_rejects_out_of_range_width(tmp_path, bad):
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.backlog.id_hex_width", bad, scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 2
    assert not (tmp_path / ".fno" / "config.toml").exists()
