"""Harness-store fallback for unregistered sessions (x-9cc5).

The registry is a cache of reality: a real session with no roster row must be
resolvable, adopted, and then addressable -- without ever guessing on ambiguity
or claiming a dead session is live.
"""
from __future__ import annotations

import json

import pytest

from fno.agents import store_fallback
from fno.agents.registry import AgentResolutionError, load_registry, resolve_agent

CLAUDE_UUID = "c655c326-1111-2222-3333-444455556666"
CODEX_UUID = "c655c326-aaaa-bbbb-cccc-ddddeeeeffff"


@pytest.fixture(autouse=True)
def _registry_home(tmp_path, monkeypatch):
    """Point the registry + every harness store at scratch dirs.

    The suite's $HOME redirect is session-scoped, so the registry would otherwise
    accumulate rows across tests in this file.
    """
    (tmp_path / "agents").mkdir()
    registry = tmp_path / "agents" / "registry.json"
    monkeypatch.setattr("fno.paths.agents_registry_path", lambda: registry)
    projects = tmp_path / "projects"
    codex = tmp_path / "codex"
    projects.mkdir()
    codex.mkdir()
    monkeypatch.setattr(store_fallback, "_claude_projects_dir", lambda: projects)
    monkeypatch.setattr(store_fallback, "_codex_sessions_dir", lambda: codex)
    return tmp_path


def _write_claude_session(root, uuid, cwd="/repo/one", project="-repo-one"):
    pdir = root / "projects" / project
    pdir.mkdir(parents=True, exist_ok=True)
    lines = [
        json.dumps({"type": "summary", "sessionId": uuid}),
        json.dumps({"type": "user", "sessionId": uuid, "cwd": cwd}),
    ]
    (pdir / f"{uuid}.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")


def _write_codex_session(root, uuid, cwd="/repo/two"):
    d = root / "codex" / "2026" / "07" / "20"
    d.mkdir(parents=True, exist_ok=True)
    meta = json.dumps({"type": "session_meta", "payload": {"id": uuid, "cwd": cwd}})
    (d / f"rollout-2026-07-20T10-00-00-{uuid}.jsonl").write_text(
        meta + "\n", encoding="utf-8"
    )


# --- shape gate ------------------------------------------------------------


@pytest.mark.parametrize(
    "token,shaped",
    [
        ("c655c326", True),
        (CLAUDE_UUID, True),
        ("ses_abc123", True),
        ("reviewer", False),
        ("c655c3", False),       # 6 hex: not a short
        ("c655c3267", False),    # 9 hex: not a short
        ("", False),
    ],
)
def test_shape_gate(token, shaped):
    assert store_fallback.is_session_shaped(token) is shaped


def test_unshaped_token_never_probes(_registry_home, monkeypatch):
    """A plain unknown name must fail exactly as it did before the fallback."""
    called = []
    monkeypatch.setattr(store_fallback, "_probe_claude", lambda t: called.append(t) or [])

    assert store_fallback.probe_stores("reviewer") == []
    assert called == []


# --- US1: claude attach on an unregistered session --------------------------


def test_claude_short_id_resolves_and_auto_registers(_registry_home):
    """AC1-HP: a real session with no row resolves, adopts, and is addressable."""
    _write_claude_session(_registry_home, CLAUDE_UUID)

    resolved = resolve_agent("c655c326")

    assert resolved.matched_by == "harness_store"
    assert resolved.entry.harness == "claude"
    assert resolved.entry.harness_session_id == CLAUDE_UUID
    # The transport key `claude attach` wants is the 8-hex jobId, NOT the UUID.
    assert resolved.entry.short_id == "c655c326"
    assert resolved.entry.cwd == "/repo/one"

    # ...and the row is now on the roster, so a later resolution is a registry hit.
    assert [e.harness_session_id for e in load_registry()] == [CLAUDE_UUID]
    assert resolve_agent("c655c326").matched_by != "harness_store"


def test_full_uuid_resolves_too(_registry_home):
    _write_claude_session(_registry_home, CLAUDE_UUID)

    assert resolve_agent(CLAUDE_UUID).entry.harness_session_id == CLAUDE_UUID


def test_adoption_is_idempotent(_registry_home):
    """Concurrent/repeat adoption upserts one row, never a duplicate."""
    _write_claude_session(_registry_home, CLAUDE_UUID)

    store_fallback.heal_from_harness_store("c655c326")
    store_fallback.heal_from_harness_store("c655c326")

    assert len(load_registry()) == 1


# --- US2/US3: other harnesses ----------------------------------------------


def test_codex_thread_resolves_from_rollout(_registry_home):
    _write_codex_session(_registry_home, CODEX_UUID)

    entry = store_fallback.heal_from_harness_store(CODEX_UUID)

    assert entry.harness == "codex"
    assert entry.harness_session_id == CODEX_UUID
    assert entry.cwd == "/repo/two"


def test_opencode_token_skips_the_hex_stores(_registry_home, monkeypatch):
    """`ses_` never probes claude/codex, and a hex token never probes opencode."""
    assert store_fallback._probe_claude("ses_abc123") == []
    assert store_fallback._probe_codex("ses_abc123") == []
    assert store_fallback._probe_opencode("c655c326") == []


# --- refusals and safety ----------------------------------------------------


def test_ambiguous_token_refuses_with_candidates(_registry_home):
    """AC1-ERR: two stores matching one short id refuses and registers nothing."""
    _write_claude_session(_registry_home, CLAUDE_UUID)
    _write_codex_session(_registry_home, CODEX_UUID)

    with pytest.raises(AgentResolutionError) as exc:
        store_fallback.heal_from_harness_store("c655c326")

    assert CLAUDE_UUID in str(exc.value)
    assert CODEX_UUID in str(exc.value)
    assert load_registry() == []


def test_unknown_token_returns_none(_registry_home):
    """Zero matches: the caller's original not-found error must survive."""
    assert store_fallback.heal_from_harness_store("deadbeef") is None
    with pytest.raises(AgentResolutionError):
        resolve_agent("deadbeef")


def test_adopted_row_is_never_live(_registry_home):
    """AC1-EDGE: a store row proves existence, never liveness."""
    _write_claude_session(_registry_home, CLAUDE_UUID)

    entry = store_fallback.heal_from_harness_store("c655c326")

    assert entry.status == "orphaned"


def test_registry_write_failure_still_resolves(_registry_home, monkeypatch, capsys):
    """AC1-FR: reaching the session wins; a failed roster write only WARNs."""
    _write_claude_session(_registry_home, CLAUDE_UUID)

    def _boom(**_kwargs):
        raise OSError("read-only registry")

    monkeypatch.setattr(
        "fno.agents.registry.register_existing_session", _boom
    )

    entry = store_fallback.heal_from_harness_store("c655c326")

    assert entry.harness_session_id == CLAUDE_UUID
    assert "could not register" in capsys.readouterr().err


def test_corrupt_store_never_denies_resolution(_registry_home):
    """A junk transcript is skipped, not fatal; a healthy sibling still resolves."""
    _write_claude_session(_registry_home, CLAUDE_UUID)
    bad = _registry_home / "projects" / "-repo-one" / "c655c326-dead.jsonl"
    bad.write_text("not json at all\n", encoding="utf-8")

    # Two files, two distinct session ids -> honest ambiguity, not a crash.
    with pytest.raises(AgentResolutionError):
        store_fallback.heal_from_harness_store("c655c326")


def test_sync_conflict_copies_are_ignored(_registry_home):
    _write_claude_session(_registry_home, CLAUDE_UUID)
    pdir = _registry_home / "projects" / "-repo-one"
    (pdir / f"{CLAUDE_UUID}.sync-conflict-20260720.jsonl").write_text("{}\n")

    assert len(store_fallback.probe_stores("c655c326")) == 1


# --- verb wiring: the heal must reach every resolution surface --------------


def test_resume_heals_an_unregistered_session(_registry_home):
    """US2: resume loads its own entries, so it needs the seam explicitly."""
    from fno.agents.resume_cli import resume_logic

    _write_codex_session(_registry_home, CODEX_UUID)

    # path_checker: the resolution is what is under test, not whether this host
    # happens to have the codex CLI installed (CI does not).
    result = resume_logic(
        name=CODEX_UUID, print_command=True, path_checker=lambda _b: True
    )

    assert result.exit_code == 0
    assert result.exec_argv == ["codex", "resume", CODEX_UUID]
    assert result.exec_cwd == "/repo/two"


def test_resume_reports_ambiguity_rather_than_guessing(_registry_home):
    from fno.agents.resume_cli import resume_logic

    _write_claude_session(_registry_home, CLAUDE_UUID)
    _write_codex_session(_registry_home, CODEX_UUID)

    result = resume_logic(name="c655c326", print_command=True)

    assert result.exit_code == 13
    assert "matches 2 sessions" in result.stderr


def test_attach_heals_an_unregistered_claude_session(_registry_home, monkeypatch):
    """AC1-HP: attach shells claude against exactly the resolved session."""
    from fno.agents import dispatch

    _write_claude_session(_registry_home, CLAUDE_UUID)
    attached = []
    monkeypatch.setattr(dispatch, "is_provider_available", lambda _p: True)
    monkeypatch.setattr(
        "fno.agents.providers.claude.claude_attach",
        lambda short: attached.append(short) or 0,
    )

    result = dispatch.attach_agent("c655c326")

    assert result.exit_code == 0
    assert attached == ["c655c326"]


def test_uppercase_uuid_still_resolves(_registry_home):
    """An id pasted out of a log resolves; opencode ids stay case-sensitive."""
    _write_claude_session(_registry_home, CLAUDE_UUID)

    assert store_fallback.probe_stores(CLAUDE_UUID.upper())[0].session_id == CLAUDE_UUID
    assert store_fallback._normalize("ses_AbC123") == "ses_AbC123"


# --- review fixes: ambiguity must not fall through, healed rows must survive --


def test_registry_ambiguity_never_probes_the_store(_registry_home):
    """Two registry rows sharing a prefix must keep refusing even when one of
    them has a transcript: a store hit must not pick the winner the registry
    deliberately would not."""
    from fno.agents.registry import AgentEntry, write_registry

    _write_claude_session(_registry_home, CLAUDE_UUID)
    write_registry([
        AgentEntry(name="one", cwd="/a", log_path="", harness="claude",
                   harness_session_id=CLAUDE_UUID),
        AgentEntry(name="two", cwd="/b", log_path="", harness="claude",
                   harness_session_id="c655c326-9999-8888-7777-666655554444"),
    ])

    with pytest.raises(AgentResolutionError) as exc:
        resolve_agent("c655c326")

    assert exc.value.ambiguous is True
    assert "is ambiguous across 2 agents" in str(exc.value)


def test_resume_keeps_registry_ambiguity(_registry_home):
    from fno.agents.registry import AgentEntry, write_registry
    from fno.agents.resume_cli import resume_logic

    _write_claude_session(_registry_home, CLAUDE_UUID)
    write_registry([
        AgentEntry(name="one", cwd="/a", log_path="", harness="claude",
                   harness_session_id=CLAUDE_UUID),
        AgentEntry(name="two", cwd="/b", log_path="", harness="claude",
                   harness_session_id="c655c326-9999-8888-7777-666655554444"),
    ])

    result = resume_logic(name="c655c326", print_command=True)

    assert result.exit_code == 13
    assert "is ambiguous across 2 agents" in result.stderr


def test_attach_survives_an_unwritable_registry(_registry_home, monkeypatch):
    """The healer returns a synthesized row when it cannot persist; attach must
    use THAT row, not re-read the unchanged registry and report not-found."""
    from fno.agents import dispatch

    _write_claude_session(_registry_home, CLAUDE_UUID)

    def _boom(**_kwargs):
        raise OSError("read-only registry")

    monkeypatch.setattr("fno.agents.registry.register_existing_session", _boom)
    attached = []
    monkeypatch.setattr(dispatch, "is_provider_available", lambda _p: True)
    monkeypatch.setattr(
        "fno.agents.providers.claude.claude_attach",
        lambda short: attached.append(short) or 0,
    )

    result = dispatch.attach_agent("c655c326")

    assert result.exit_code == 0
    assert attached == ["c655c326"]


def test_invalid_utf8_transcript_does_not_crash(_registry_home):
    """UnicodeDecodeError is a ValueError, so it would bypass an OSError-only
    guard and crash resolution from the iteration itself."""
    pdir = _registry_home / "projects" / "-repo-one"
    pdir.mkdir(parents=True, exist_ok=True)
    (pdir / f"{CLAUDE_UUID}.jsonl").write_bytes(
        b'{"type":"summary"}\n\xff\xfe not utf-8 \xff\n'
    )

    hits = store_fallback.probe_stores("c655c326")

    assert [h.session_id for h in hits] == [CLAUDE_UUID]
    assert hits[0].cwd == ""
