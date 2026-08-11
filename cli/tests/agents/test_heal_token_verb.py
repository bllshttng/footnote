"""`fno agents heal-token` -- the registry-miss healer as a shellable verb (x-da8c).

The Rust lifecycle verbs (logs/attach/resume) resolve against the registry file
only, so a real session with no roster row is refused there while `fno mail`
reaches it. This verb exposes the ONE x-9cc5 healer so Rust heals through the
same probe instead of growing a second one; these tests pin the exit-code
contract that shellout depends on (0 + row JSON / 13 miss / 3 ambiguous).
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.agents import discover
from fno.agents.cli import (
    HEAL_TOKEN_AMBIGUOUS_EXIT,
    HEAL_TOKEN_MISS_EXIT,
    HEAL_TOKEN_UNAVAILABLE_EXIT,
)
from fno.agents.registry import load_registry
from fno.cli import app

CLAUDE_UUID = "c655c326-1111-2222-3333-444455556666"
TWIN_UUID = "c655c326-9999-8888-7777-666655554444"


@pytest.fixture(autouse=True)
def _scratch_stores(tmp_path, monkeypatch):
    (tmp_path / "agents").mkdir()
    registry = tmp_path / "agents" / "registry.json"
    monkeypatch.setattr("fno.paths.agents_registry_path", lambda: registry)
    for name, env in (("projects", discover.PROJECTS_DIR_ENV),
                      ("codex", discover.CODEX_SESSIONS_DIR_ENV)):
        (tmp_path / name).mkdir()
        monkeypatch.setenv(env, str(tmp_path / name))
    # Run from a cwd that is NOT a known project: project confinement (defect 1)
    # is N/A outside a project, so these adoption-mechanic tests proceed as
    # before. The confined path (same-project adopt, foreign refuse) is covered
    # by test_store_fallback_confinement.py.
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _write_claude_session(root, uuid, cwd="/repo/one", project="-repo-one"):
    pdir = root / "projects" / project
    pdir.mkdir(parents=True, exist_ok=True)
    (pdir / f"{uuid}.jsonl").write_text(
        json.dumps({"type": "summary"}) + "\n"
        + json.dumps({"type": "user", "cwd": cwd}) + "\n",
        encoding="utf-8",
    )


def _write_codex_session(root, uuid, cwd="/repo/two"):
    d = root / "codex" / "2026" / "07" / "20"
    d.mkdir(parents=True, exist_ok=True)
    (d / f"rollout-2026-07-20T10-00-00-{uuid}.jsonl").write_text(
        json.dumps({"type": "session_meta", "payload": {"id": uuid, "cwd": cwd}}) + "\n",
        encoding="utf-8",
    )


def _run(*args):
    return CliRunner().invoke(app, ["agents", "heal-token", *args])


def test_adopts_stored_session_and_prints_row(_scratch_stores):
    _write_claude_session(_scratch_stores, CLAUDE_UUID)

    res = _run("c655c326")

    assert res.exit_code == 0
    row = json.loads(res.stdout.strip().splitlines()[-1])
    assert row["harness"] == "claude"
    assert row["harness_session_id"] == CLAUDE_UUID
    assert row["short_id"] == "c655c326"
    assert row["cwd"] == "/repo/one"
    # Never live: store membership proves the session exists, not that it runs.
    assert row["status"] == "orphaned"
    # ...and the row is now in the roster, so every later verb agrees.
    assert [e.harness_session_id for e in load_registry()] == [CLAUDE_UUID]


def test_miss_exits_13_with_no_row(_scratch_stores):
    res = _run("deadbeef")

    assert res.exit_code == HEAL_TOKEN_MISS_EXIT
    assert res.stdout.strip() == ""
    assert load_registry() == []


def test_name_shaped_token_never_probes(_scratch_stores):
    """The shape gate: a plain unknown name must miss without touching a store."""
    _write_claude_session(_scratch_stores, CLAUDE_UUID)

    res = _run("reviewer")

    assert res.exit_code == HEAL_TOKEN_MISS_EXIT
    assert load_registry() == []


def test_ambiguous_token_refuses_with_every_candidate(_scratch_stores):
    _write_claude_session(_scratch_stores, CLAUDE_UUID)
    _write_codex_session(_scratch_stores, TWIN_UUID)

    res = _run("c655c326")

    assert res.exit_code == HEAL_TOKEN_AMBIGUOUS_EXIT
    assert CLAUDE_UUID in res.stderr and TWIN_UUID in res.stderr
    # Nothing adopted: an ambiguous token is refused, never guessed.
    assert load_registry() == []


def test_all_sources_refuses_registry_and_store_collision_without_adopting(
    _scratch_stores,
):
    from fno.agents.registry import AgentEntry, write_registry

    registered_id = "aaaaaaaa-1111-2222-3333-444455556666"
    write_registry([
        AgentEntry(
            name="c655c326",
            cwd="/registered",
            log_path="",
            harness="claude",
            harness_session_id=registered_id,
        )
    ])
    _write_claude_session(_scratch_stores, CLAUDE_UUID)
    _write_codex_session(_scratch_stores, TWIN_UUID)

    res = _run("c655c326", "--all-sources")

    assert res.exit_code == HEAL_TOKEN_AMBIGUOUS_EXIT
    assert registered_id in res.stderr
    assert CLAUDE_UUID in res.stderr and TWIN_UUID in res.stderr
    assert [e.harness_session_id for e in load_registry()] == [registered_id]


def test_all_sources_reports_unreadable_registry_as_unavailable(_scratch_stores):
    from fno.agents.registry import SCHEMA_VERSION

    registry = _scratch_stores / "agents" / "registry.json"
    registry.write_text(
        json.dumps({"schema_version": SCHEMA_VERSION + 1, "agents": []}),
        encoding="utf-8",
    )

    res = _run("deadbeef", "--all-sources")

    assert res.exit_code == HEAL_TOKEN_UNAVAILABLE_EXIT
    assert "registry unreadable" in res.stderr


def test_verb_stays_off_the_rust_routing_set():
    """The recursion guard: Rust shells this out, so it must never route back."""
    from fno.agents.rust_runtime import AUTO_ROUTE_VERBS, RUST_CLIENT_VERBS

    assert "heal-token" not in RUST_CLIENT_VERBS
    assert "heal-token" not in AUTO_ROUTE_VERBS
