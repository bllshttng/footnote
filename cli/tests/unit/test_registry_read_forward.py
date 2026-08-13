"""A reader must not fail closed on a registry written by a newer writer.

``~/.fno/agents/registry.json`` is global to every agent on this machine, so a
process running ahead of the deployment raised the on-disk schema and every
deployed reader refused it at once. Mail died fleet-wide with no announcement,
and the symptom surfaced far from the cause.

The shape that fixes it is read forward, refuse to write, and say so out loud:

- READ a higher on-disk schema instead of raising, keeping the rows and fields
  this reader understands and ignoring the ones it does not.
- REFUSE to write while the on-disk schema is higher, because reading forward
  drops unknown fields in memory and a write from that state would erase rows
  the reader never saw.
- ANNOUNCE every degraded read on stderr, naming both versions. A silent
  read-forward makes a partial row indistinguishable from a complete one, so a
  routing or liveness decision taken on a truncated row would leave no trace.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.agents import registry as reg


def _write_raw(path: Path, version: int, agents: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"schema_version": version, "agents": agents}, indent=2),
        encoding="utf-8",
    )


def _row(name: str = "worker-1") -> dict:
    """A row this reader fully understands, as the current schema spells it."""
    from dataclasses import asdict

    entry = reg.AgentEntry(
        name=name,
        cwd="/Users/x/proj",
        log_path="/Users/x/proj/.fno/log",
        harness="claude",
        harness_session_id="9a063cd3-69d4-415a-ada5-649b0164189c",
    )
    return asdict(entry)


# --------------------------------------------------------------------------
# Read forward
# --------------------------------------------------------------------------


def test_higher_on_disk_schema_still_reads(tmp_path: Path) -> None:
    """The incident, reproduced: a writer one version ahead of this reader."""
    path = tmp_path / "registry.json"
    _write_raw(path, reg.SCHEMA_VERSION + 1, [_row()])

    entries = reg.load_registry(path)

    assert [e.name for e in entries] == ["worker-1"]


def test_unknown_field_from_a_newer_writer_is_ignored_not_fatal(tmp_path: Path) -> None:
    """A field this reader has never heard of must not brick the shared read."""
    path = tmp_path / "registry.json"
    row = _row()
    row["a_field_from_the_future"] = {"nested": True}
    _write_raw(path, reg.SCHEMA_VERSION + 1, [row])

    entries = reg.load_registry(path)

    assert [e.name for e in entries] == ["worker-1"]
    assert not hasattr(entries[0], "a_field_from_the_future")


def test_degraded_read_announces_both_versions(tmp_path: Path, capsys) -> None:
    """Silence is the trap. A partial row must never look like a complete one."""
    path = tmp_path / "registry.json"
    ahead = reg.SCHEMA_VERSION + 1
    _write_raw(path, ahead, [_row()])

    reg.load_registry(path)

    err = capsys.readouterr().err
    assert str(ahead) in err, f"on-disk schema not named on stderr: {err!r}"
    assert str(reg.SCHEMA_VERSION) in err, f"reader schema not named on stderr: {err!r}"


def test_every_degraded_read_announces_not_only_the_first(tmp_path: Path, capsys) -> None:
    """A once-per-process warning lets later decisions run silently on partial rows."""
    path = tmp_path / "registry.json"
    _write_raw(path, reg.SCHEMA_VERSION + 1, [_row()])

    reg.load_registry(path)
    capsys.readouterr()
    reg.load_registry(path)

    assert capsys.readouterr().err.strip(), "the second degraded read was silent"


def test_a_current_schema_read_stays_quiet(tmp_path: Path, capsys) -> None:
    """The warning has to mean something, so the normal path must not emit it."""
    path = tmp_path / "registry.json"
    _write_raw(path, reg.SCHEMA_VERSION, [_row()])

    reg.load_registry(path)

    assert capsys.readouterr().err == ""


# --------------------------------------------------------------------------
# Write closed
# --------------------------------------------------------------------------


def test_write_refuses_while_on_disk_schema_is_higher(tmp_path: Path) -> None:
    """The write block is what makes read-forward safe, not an extra precaution."""
    path = tmp_path / "registry.json"
    ahead = reg.SCHEMA_VERSION + 1
    row = _row()
    row["a_field_from_the_future"] = "keep me"
    _write_raw(path, ahead, [row])

    entries = reg.load_registry(path)
    with pytest.raises(reg.RegistryVersionError) as exc:
        reg.write_registry(entries, path)

    assert str(ahead) in str(exc.value)
    assert str(reg.SCHEMA_VERSION) in str(exc.value)


def test_a_refused_write_leaves_the_newer_file_untouched(tmp_path: Path) -> None:
    """The point of refusing is that the shared file survives intact."""
    path = tmp_path / "registry.json"
    row = _row()
    row["a_field_from_the_future"] = "keep me"
    _write_raw(path, reg.SCHEMA_VERSION + 1, [row])
    before = path.read_text(encoding="utf-8")

    # Load OUTSIDE the raises block: inside it, a load that raised would satisfy
    # the assertion without write_registry ever being reached.
    entries = reg.load_registry(path)
    with pytest.raises(reg.RegistryVersionError):
        reg.write_registry(entries, path)

    assert path.read_text(encoding="utf-8") == before


def test_write_still_works_at_the_current_schema(tmp_path: Path) -> None:
    """Read-forward must not cost the ordinary write path."""
    path = tmp_path / "registry.json"
    _write_raw(path, reg.SCHEMA_VERSION, [_row()])

    reg.write_registry(reg.load_registry(path), path)

    assert json.loads(path.read_text())["schema_version"] == reg.SCHEMA_VERSION


# --------------------------------------------------------------------------
# Still fail closed on genuine damage
# --------------------------------------------------------------------------


def test_malformed_json_still_raises(tmp_path: Path) -> None:
    """Read-forward covers a version gap, never a torn file."""
    path = tmp_path / "registry.json"
    path.write_text("{not json", encoding="utf-8")

    with pytest.raises(reg.RegistryVersionError):
        reg.load_registry(path)


def test_a_nonsense_schema_value_still_raises(tmp_path: Path) -> None:
    """A missing or non-integer version is damage, not a newer writer."""
    path = tmp_path / "registry.json"
    path.write_text(json.dumps({"schema_version": "thirteen", "agents": []}), encoding="utf-8")

    with pytest.raises(reg.RegistryVersionError):
        reg.load_registry(path)
