"""Twice by hand is a verb.

The operator repaired a poisoned `registry.json` by hand on 2026-08-28, and a
surviving `.bak.poison-repair` sibling shows someone did the same on
2026-08-09. Both times the steps were identical: hold the lock, assert no row
carries data belonging to the newer schema, back up, drop the empty unknown
keys, set the integer, replace atomically.

The third step is the point. The 2026-08-28 rollback was lossless BY LUCK: all
33 rows happened to carry the v20 fields empty. One real value written five
minutes later and no safe rollback would have existed. This verb turns that
luck into an assertion.

Prevention is not retroactive, which is why this ships even though the write
guard drives its expected frequency toward zero: a worktree that already wrote
a raised version leaves a file only a hand edit clears.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.agents import registry as reg


def _row(name: str, **extra) -> dict:
    row = {
        "name": name,
        "cwd": "/Users/x/proj",
        "log_path": "/Users/x/proj/.fno/log",
        "harness": "claude",
        "harness_session_id": "9a063cd3-69d4-415a-ada5-649b0164189c",
    }
    row.update(extra)
    return row


def _poisoned(path: Path, version: int, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"schema_version": version, "agents": rows}, indent=2),
        encoding="utf-8",
    )


# --------------------------------------------------------------------------
# AC5-HP: the repair
# --------------------------------------------------------------------------


def test_apply_backs_up_restores_the_version_and_keeps_every_row(
    tmp_path: Path,
) -> None:
    path = tmp_path / "registry.json"
    ahead = reg.SCHEMA_VERSION + 1
    _poisoned(
        path,
        ahead,
        [
            _row("worker-1", a_field_from_the_future=None),
            _row("worker-2", a_field_from_the_future=""),
            _row("worker-3"),
        ],
    )

    plan = reg.repair_registry_schema(reg.SCHEMA_VERSION, path=path, apply=True)

    data = json.loads(path.read_text())
    assert data["schema_version"] == reg.SCHEMA_VERSION
    assert [a["name"] for a in data["agents"]] == ["worker-1", "worker-2", "worker-3"]
    assert all("a_field_from_the_future" not in a for a in data["agents"])
    assert plan.dropped == {
        "worker-1": ["a_field_from_the_future"],
        "worker-2": ["a_field_from_the_future"],
    }

    backups = list(tmp_path.glob("registry.json.bak.schema-repair-*"))
    assert len(backups) == 1
    assert json.loads(backups[0].read_text())["schema_version"] == ahead


def test_a_dry_run_reports_the_drop_and_writes_nothing(tmp_path: Path) -> None:
    path = tmp_path / "registry.json"
    _poisoned(
        path,
        reg.SCHEMA_VERSION + 1,
        [_row("worker-1", a_field_from_the_future=None)],
    )
    before = path.read_bytes()

    plan = reg.repair_registry_schema(reg.SCHEMA_VERSION, path=path)

    assert plan.dropped == {"worker-1": ["a_field_from_the_future"]}
    assert path.read_bytes() == before
    assert list(tmp_path.glob("*.bak.*")) == []


# --------------------------------------------------------------------------
# AC6-ERR: the assertion that makes it safe
# --------------------------------------------------------------------------


def test_a_row_carrying_real_newer_schema_data_refuses(tmp_path: Path) -> None:
    path = tmp_path / "registry.json"
    _poisoned(
        path,
        reg.SCHEMA_VERSION + 1,
        [
            _row("worker-1"),
            _row("worker-2", a_field_from_the_future="a real value"),
        ],
    )
    before = path.read_bytes()

    with pytest.raises(reg.RegistryRepairRefused) as excinfo:
        reg.repair_registry_schema(reg.SCHEMA_VERSION, path=path, apply=True)

    message = str(excinfo.value)
    assert "worker-2" in message
    assert "a_field_from_the_future" in message
    assert path.read_bytes() == before
    assert list(tmp_path.glob("*.bak.*")) == []


def test_duplicate_row_names_each_get_their_own_report_line(tmp_path: Path) -> None:
    """A poisoned file can carry duplicate names. Keyed by name alone, the
    second row's keys overwrite the first's and the preview names one row where
    two are dropping data."""
    path = tmp_path / "registry.json"
    _poisoned(
        path,
        reg.SCHEMA_VERSION + 1,
        [
            _row("twin", a_field_from_the_future=None),
            _row("twin", another_future_field=""),
        ],
    )

    plan = reg.repair_registry_schema(reg.SCHEMA_VERSION, path=path, apply=True)

    assert plan.dropped == {
        "twin": ["a_field_from_the_future"],
        "twin (row 1)": ["another_future_field"],
    }
    data = json.loads(path.read_text())
    assert len(data["agents"]) == 2
    assert all(len(a) == len(_row("twin")) for a in data["agents"])


@pytest.mark.parametrize("value", [0, False, [1], {"k": "v"}])
def test_a_falsy_but_real_value_is_still_data(tmp_path: Path, value) -> None:
    """``0`` and ``False`` are values a newer schema meant to store. Only
    ``None`` and an empty string, list, or object read as "never written"."""
    path = tmp_path / "registry.json"
    _poisoned(
        path,
        reg.SCHEMA_VERSION + 1,
        [_row("worker-1", a_field_from_the_future=value)],
    )

    with pytest.raises(reg.RegistryRepairRefused):
        reg.repair_registry_schema(reg.SCHEMA_VERSION, path=path, apply=True)


# --------------------------------------------------------------------------
# It refuses rather than guessing
# --------------------------------------------------------------------------


@pytest.mark.parametrize("on_disk_delta", [0, -1])
def test_a_version_not_strictly_above_the_target_refuses(
    tmp_path: Path, on_disk_delta: int
) -> None:
    path = tmp_path / "registry.json"
    _poisoned(path, reg.SCHEMA_VERSION + on_disk_delta, [_row("worker-1")])

    with pytest.raises(reg.RegistryRepairRefused):
        reg.repair_registry_schema(reg.SCHEMA_VERSION, path=path, apply=True)


@pytest.mark.parametrize("body", ["", "{", "[]", '{"schema_version": "20"}'])
def test_an_unparseable_or_untyped_file_refuses(tmp_path: Path, body: str) -> None:
    """A torn file is damage, not a version to roll back, and this verb never
    guesses at what the rows were."""
    path = tmp_path / "registry.json"
    path.write_text(body, encoding="utf-8")

    with pytest.raises(reg.RegistryRepairRefused):
        reg.repair_registry_schema(reg.SCHEMA_VERSION, path=path, apply=True)


def test_an_absent_file_refuses(tmp_path: Path) -> None:
    with pytest.raises(reg.RegistryRepairRefused):
        reg.repair_registry_schema(
            reg.SCHEMA_VERSION, path=tmp_path / "nope.json", apply=True
        )


# --------------------------------------------------------------------------
# The verb itself
# --------------------------------------------------------------------------


def test_the_cli_verb_is_registered_hidden_and_dry_runs_by_default(
    tmp_path: Path,
) -> None:
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    path = tmp_path / "registry.json"
    _poisoned(
        path,
        reg.SCHEMA_VERSION + 1,
        [_row("worker-1", a_field_from_the_future=None)],
    )
    before = path.read_bytes()

    result = CliRunner().invoke(
        agents_app,
        ["registry-repair", "--to", str(reg.SCHEMA_VERSION), "--path", str(path)],
    )

    assert result.exit_code == 0, result.output
    assert "worker-1" in result.output
    assert "a_field_from_the_future" in result.output
    assert path.read_bytes() == before

    applied = CliRunner().invoke(
        agents_app,
        [
            "registry-repair",
            "--to",
            str(reg.SCHEMA_VERSION),
            "--path",
            str(path),
            "--apply",
        ],
    )
    assert applied.exit_code == 0, applied.output
    assert json.loads(path.read_text())["schema_version"] == reg.SCHEMA_VERSION


def test_the_cli_verb_exits_non_zero_when_it_refuses(tmp_path: Path) -> None:
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    path = tmp_path / "registry.json"
    _poisoned(
        path,
        reg.SCHEMA_VERSION + 1,
        [_row("worker-1", a_field_from_the_future="real")],
    )

    result = CliRunner().invoke(
        agents_app,
        [
            "registry-repair",
            "--to",
            str(reg.SCHEMA_VERSION),
            "--path",
            str(path),
            "--apply",
        ],
    )

    assert result.exit_code != 0
    assert json.loads(path.read_text())["schema_version"] == reg.SCHEMA_VERSION + 1
