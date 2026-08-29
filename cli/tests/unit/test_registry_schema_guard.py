"""A source-run fno must not raise the shared registry's schema.

x-d07d made the blast survivable: a reader one version behind degrades and
announces instead of failing closed, and that worked on 2026-08-28. It left the
fuse in place. This is the fuse.

The causing worker said it in its own words the same hour: "my mail and
review-request calls ran the worktree CLI against the real HOME." No test ran.
So the boundary that has to hold is not test versus production, it is worktree
SOURCE versus deployed STATE, and it has to hold on the most common verb on the
fleet.

Three conditions gate the refusal, and each one is load-bearing:

- the target IS the process-global registry (a named store is nobody's shared
  state, so a redirected checkout keeps its normal write - that is the escape
  hatch, and it works by moving the target rather than by silencing the check);
- this process runs from a source checkout, decided on the RUNNING ARTIFACT's
  own path and never on the cwd, because a deployed fno invoked from inside a
  worktree is still deployed and must keep its normal write;
- the on-disk version is a readable int strictly BELOW this one. An absent or
  unparseable file is not a raise, mirroring the reasoning already written into
  the sibling guard.
"""
from __future__ import annotations

import json
from dataclasses import asdict
from pathlib import Path

import pytest

from fno import paths
from fno.agents import registry as reg


def _entry(name: str = "worker-1") -> reg.AgentEntry:
    return reg.AgentEntry(
        name=name,
        cwd="/Users/x/proj",
        log_path="/Users/x/proj/.fno/log",
        harness="claude",
        harness_session_id="9a063cd3-69d4-415a-ada5-649b0164189c",
    )


def _row(name: str = "worker-1") -> dict:
    return asdict(_entry(name))


def _write_raw(path: Path, version: int, agents: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"schema_version": version, "agents": agents}, indent=2),
        encoding="utf-8",
    )


@pytest.fixture
def shared(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A registry that IS the process-global one, living outside any checkout."""
    target = tmp_path / "fno-home" / "agents" / "registry.json"
    monkeypatch.setattr(paths, "agents_registry_path", lambda: target)
    return target


@pytest.fixture
def from_source(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """This process reports itself as running from a checkout at ``<tmp>/src``."""
    root = tmp_path / "src"
    root.mkdir(parents=True, exist_ok=True)
    (root / ".git").write_text("gitdir: elsewhere\n", encoding="utf-8")
    monkeypatch.setattr(reg, "_running_from_source", lambda: root)
    return root


@pytest.fixture
def deployed(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(reg, "_running_from_source", lambda: None)


# --------------------------------------------------------------------------
# AC1-HP: the refusal, on the path mail actually takes
# --------------------------------------------------------------------------


def test_source_ahead_write_to_the_shared_registry_is_refused(
    shared: Path, from_source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _write_raw(shared, reg.SCHEMA_VERSION, [_row()])
    before = shared.read_bytes()
    monkeypatch.setattr(reg, "SCHEMA_VERSION", reg.SCHEMA_VERSION + 1)

    with pytest.raises(reg.RegistryVersionError):
        reg.write_registry([_entry()])

    assert shared.read_bytes() == before


def test_the_guard_fires_through_update_registry_the_way_mail_reaches_it(
    shared: Path, from_source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The regression this test exists for.

    ``fno.mail.hold`` calls ``update_registry(_updater)`` with no path, and
    ``update_registry`` resolves the default ITSELF before calling
    ``write_registry(entries, path=target)``. So a guard keyed on "the caller
    passed no explicit path" is permanently false on the one path that caused
    the outage. Key on the RESOLVED target instead.
    """
    _write_raw(shared, reg.SCHEMA_VERSION, [_row()])
    before = shared.read_bytes()
    monkeypatch.setattr(reg, "SCHEMA_VERSION", reg.SCHEMA_VERSION + 1)

    with pytest.raises(reg.RegistryVersionError):
        reg.update_registry(lambda entries: entries)

    assert shared.read_bytes() == before


def test_the_refusal_names_both_versions_the_source_path_and_both_exits(
    shared: Path, from_source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Two separate readers misdiagnosed this. The message is the diagnosis."""
    on_disk = reg.SCHEMA_VERSION
    _write_raw(shared, on_disk, [_row()])
    monkeypatch.setattr(reg, "SCHEMA_VERSION", on_disk + 1)

    with pytest.raises(reg.RegistryVersionError) as excinfo:
        reg.write_registry([_entry()])

    message = str(excinfo.value)
    assert f"schema_version={on_disk}" in message
    assert f"schema_version={on_disk + 1}" in message
    assert str(shared) in message
    assert str(from_source) in message
    assert "running from source" in message
    assert "fno doctor update" in message
    assert "config.paths.agents_registry_path" in message
    assert "FNO_AGENTS_HOME" in message


# --------------------------------------------------------------------------
# AC2-HP: the legitimate upgrade path survives
# --------------------------------------------------------------------------


def test_a_deployed_fno_may_still_raise_the_schema(
    shared: Path, deployed: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Or the next real deploy could never raise the schema at all."""
    on_disk = reg.SCHEMA_VERSION
    _write_raw(shared, on_disk, [_row()])
    monkeypatch.setattr(reg, "SCHEMA_VERSION", on_disk + 1)

    reg.write_registry([_entry()])

    assert json.loads(shared.read_text())["schema_version"] == on_disk + 1


# --------------------------------------------------------------------------
# AC3-HP: the escape hatch moves the target, it does not silence the check
# --------------------------------------------------------------------------


def test_a_checkout_local_registry_bumps_freely(
    from_source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    inside = from_source / "worktree-state" / "registry.json"
    monkeypatch.setattr(paths, "agents_registry_path", lambda: inside)
    on_disk = reg.SCHEMA_VERSION
    _write_raw(inside, on_disk, [_row()])
    monkeypatch.setattr(reg, "SCHEMA_VERSION", on_disk + 1)

    reg.write_registry([_entry()])

    assert json.loads(inside.read_text())["schema_version"] == on_disk + 1


def test_a_named_store_is_not_the_shared_one(
    tmp_path: Path, shared: Path, from_source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Every test and every deliberate non-default store names its own file."""
    named = tmp_path / "somewhere-else" / "registry.json"
    on_disk = reg.SCHEMA_VERSION
    _write_raw(named, on_disk, [_row()])
    monkeypatch.setattr(reg, "SCHEMA_VERSION", on_disk + 1)

    reg.write_registry([_entry()], path=named)

    assert json.loads(named.read_text())["schema_version"] == on_disk + 1


# --------------------------------------------------------------------------
# AC7-EDGE: an absent file is not a raise
# --------------------------------------------------------------------------


@pytest.mark.parametrize("body", [None, "", "{", "[]", '{"schema_version": "19"}'])
def test_absent_empty_or_unparseable_does_not_fire_the_guard(
    shared: Path, from_source: Path, monkeypatch: pytest.MonkeyPatch, body
) -> None:
    """Refusing here would leave a torn registry unrepairable by the very
    command meant to rewrite it."""
    shared.parent.mkdir(parents=True, exist_ok=True)
    if body is not None:
        shared.write_text(body, encoding="utf-8")
    bumped = reg.SCHEMA_VERSION + 1
    monkeypatch.setattr(reg, "SCHEMA_VERSION", bumped)

    reg.write_registry([_entry()])

    assert json.loads(shared.read_text())["schema_version"] == bumped


def test_an_equal_on_disk_version_is_not_a_bump(shared: Path, from_source: Path) -> None:
    """The ordinary case: source and deployment agree. Every write on this
    machine takes this branch, so it must stay free of a refusal."""
    _write_raw(shared, reg.SCHEMA_VERSION, [_row()])

    reg.write_registry([_entry("worker-2")])

    data = json.loads(shared.read_text())
    assert data["schema_version"] == reg.SCHEMA_VERSION
    assert [a["name"] for a in data["agents"]] == ["worker-2"]
