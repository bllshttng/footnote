"""Harness-store fallback for unregistered sessions (x-9cc5).

The registry is a cache of reality: a real session with no roster row must be
resolvable, adopted, and then addressable -- without ever guessing on ambiguity
or claiming a dead session is live.
"""
from __future__ import annotations

import json
import os

import pytest

from fno.agents import discover, store_fallback
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
    # Through the real env seams discover owns, so the test exercises the same
    # resolution path production does.
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(codex))
    # Run from a cwd that is NOT a known project: project confinement (defect 1)
    # is N/A outside a project, so these adoption/ambiguity-mechanic tests
    # proceed as before. The confined path (same-project adopt, foreign refuse)
    # is covered by test_store_fallback_confinement.py.
    monkeypatch.chdir(tmp_path)
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


def _write_turn_named_codex_session(root, uuid, cwd="/repo/two"):
    d = root / "codex" / "2026" / "07" / "20"
    d.mkdir(parents=True, exist_ok=True)
    meta = json.dumps({"type": "session_meta", "payload": {"id": uuid, "cwd": cwd}})
    (d / "rollout-2026-07-20T10-00-00-turn_12345.jsonl").write_text(
        meta + "\n", encoding="utf-8"
    )


def test_default_codex_sessions_dir_honors_codex_home(
    monkeypatch, tmp_path
):
    monkeypatch.delenv(discover.CODEX_SESSIONS_DIR_ENV)
    monkeypatch.setenv("CODEX_HOME", str(tmp_path / "custom-codex"))

    assert discover.default_codex_sessions_dir() == tmp_path / "custom-codex" / "sessions"


# --- shape gate ------------------------------------------------------------


@pytest.mark.parametrize(
    "token,shaped",
    [
        ("c655c326", True),
        (CLAUDE_UUID, True),
        ("ses_abc123", True),
        ("reviewer", True),  # a random OpenCode tail may be alphabetic
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

    assert store_fallback.probe_stores("friendly-name") == []
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


def test_unreadable_store_refuses_short_token_resolution(monkeypatch):
    """An errored probe is unknown coverage, never proof of no collision."""

    def _unreadable(_token):
        raise OSError("store permission denied")

    monkeypatch.setattr(store_fallback, "_PROBES", (_unreadable,))

    with pytest.raises(AgentResolutionError, match="could not be checked") as exc:
        store_fallback.complete_store_hits("deadbeef")

    assert exc.value.ambiguous is True
    assert "unreadable" in str(exc.value)


def test_one_hit_plus_unreadable_store_is_not_unique(monkeypatch):
    """A candidate cannot win while another source may hide its collision."""
    hit = store_fallback.StoreHit(
        "codex", "019fb417-1111-7222-8333-4444deadbeef", "/repo"
    )

    def _hit(_token):
        return [hit]

    def _unreadable(_token):
        raise OSError("store permission denied")

    monkeypatch.setattr(store_fallback, "_PROBES", (_hit, _unreadable))

    with pytest.raises(AgentResolutionError, match="could not be checked") as exc:
        store_fallback.complete_store_hits("deadbeef")

    assert hit.session_id in str(exc.value)


def test_real_unreadable_directory_refuses_partial_store_answer(
    _registry_home, monkeypatch
):
    """Filesystem traversal errors are coverage failures, not empty directories."""
    matching = "019fb417-1111-7222-8333-4444deadbeef"
    _write_codex_session(_registry_home, matching)
    blocked = _registry_home / "projects" / "blocked"
    blocked.mkdir()
    (blocked / "unrelated.jsonl").write_text("{}\n")
    blocked.chmod(0)
    try:
        if os.access(blocked, os.R_OK | os.X_OK):
            pytest.skip("test user can still enumerate mode-000 directories")
        with pytest.raises(AgentResolutionError, match="claude") as exc:
            store_fallback.complete_store_hits("deadbeef")
    finally:
        blocked.chmod(0o700)

    assert matching in str(exc.value)


def test_turn_named_codex_rollout_resolves_from_session_metadata(_registry_home):
    session_id = "019fb417-1111-7222-8333-4444deadbeef"
    _write_turn_named_codex_session(_registry_home, session_id)

    for token in (session_id, "deadbeef", "019fb417"):
        hits = store_fallback.complete_store_hits(token)
        assert [(hit.harness, hit.session_id) for hit in hits] == [
            ("codex", session_id)
        ]


def test_turn_named_codex_rollout_participates_in_registry_collision(
    _registry_home,
):
    from fno.agents.registry import AgentEntry, write_registry

    store_session = "019fb417-1111-7222-8333-4444deadbeef"
    _write_turn_named_codex_session(_registry_home, store_session)
    write_registry([
        AgentEntry(
            name="deadbeef",
            harness="claude",
            harness_session_id="aaaaaaaa-1111-2222-3333-444455556666",
            cwd="/registered",
            log_path="",
        )
    ])

    with pytest.raises(AgentResolutionError, match=store_session):
        resolve_agent("deadbeef")


def test_registry_hit_and_store_only_session_share_one_ambiguity_namespace(
    _registry_home,
):
    """A registry name must not hide a distinct store-only session handle."""
    from fno.agents.registry import AgentEntry, write_registry

    registered_id = "aaaaaaaa-1111-2222-3333-444455556666"
    store_only_id = "bbbbbbbb-1111-2222-3333-0000deadbeef"
    write_registry([
        AgentEntry(
            name="deadbeef",
            cwd="/registered",
            log_path="",
            harness="claude",
            harness_session_id=registered_id,
        )
    ])
    _write_codex_session(_registry_home, store_only_id)

    with pytest.raises(AgentResolutionError) as exc:
        resolve_agent("deadbeef")

    assert exc.value.ambiguous is True
    assert registered_id in str(exc.value)
    assert store_only_id in str(exc.value)
    assert [e.harness_session_id for e in load_registry()] == [registered_id]


def test_registry_hit_refuses_when_any_harness_store_is_unreadable(
    _registry_home, monkeypatch
):
    """A partial store census cannot prove that a registry short is unique."""
    from fno.agents.registry import AgentEntry, write_registry

    registered_id = "aaaaaaaa-1111-2222-3333-444455556666"
    write_registry([
        AgentEntry(
            name="deadbeef",
            cwd="/registered",
            log_path="",
            harness="claude",
            harness_session_id=registered_id,
        )
    ])

    def unreadable_codex(_token):
        raise OSError("codex store unreadable")

    monkeypatch.setattr(
        store_fallback,
        "_PROBES",
        (store_fallback._probe_claude, unreadable_codex, store_fallback._probe_opencode),
    )

    with pytest.raises(AgentResolutionError, match="could not be checked") as exc:
        resolve_agent("deadbeef")

    assert exc.value.ambiguous is True
    assert "unreadable_codex" in str(exc.value)


def test_store_only_hit_refuses_when_another_harness_store_is_unreadable(
    _registry_home, monkeypatch
):
    """A unique partial hit is still an unsafe guess on the registry-miss path."""
    hit = store_fallback.StoreHit(
        "claude",
        "aaaaaaaa-1111-2222-3333-4444deadbeef",
        "/repo/one",
    )

    def one_hit(_token):
        return [hit]

    def unreadable_codex(_token):
        raise OSError("codex store unreadable")

    monkeypatch.setattr(
        store_fallback,
        "_PROBES",
        (one_hit, unreadable_codex, store_fallback._probe_opencode),
    )

    with pytest.raises(AgentResolutionError, match="could not be checked") as exc:
        store_fallback.heal_from_harness_store("deadbeef")

    assert exc.value.ambiguous is True
    assert "unreadable_codex" in str(exc.value)
    assert load_registry() == []


def test_registry_and_store_sighting_of_same_session_deduplicate(_registry_home):
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="same-worker",
            cwd="/repo/one",
            log_path="",
            harness="claude",
            harness_session_id=CLAUDE_UUID,
            short_id="c655c326",
        )
    ])
    _write_claude_session(_registry_home, CLAUDE_UUID)

    resolved = resolve_agent("c655c326")

    assert resolved.entry.name == "same-worker"
    assert len(load_registry()) == 1


def test_resume_refuses_registry_hit_that_collides_with_store_session(
    _registry_home,
):
    from fno.agents.registry import AgentEntry, write_registry
    from fno.agents.resume_cli import resume_logic

    registered_id = "aaaaaaaa-1111-2222-3333-444455556666"
    store_only_id = "bbbbbbbb-1111-2222-3333-0000deadbeef"
    write_registry([
        AgentEntry(
            name="deadbeef",
            cwd="/registered",
            log_path="",
            harness="codex",
            harness_session_id=registered_id,
        )
    ])
    _write_codex_session(_registry_home, store_only_id)

    result = resume_logic(name="deadbeef", print_command=True)

    assert result.exit_code == 13
    assert registered_id in result.stderr
    assert store_only_id in result.stderr


@pytest.mark.parametrize("verb", ["stop", "rm"])
def test_lifecycle_refuses_registry_name_that_collides_with_store_session(
    _registry_home, verb
):
    from fno.agents import dispatch
    from fno.agents.registry import AgentEntry, write_registry

    registered_id = "aaaaaaaa-1111-2222-3333-444455556666"
    store_only_id = "bbbbbbbb-1111-2222-3333-0000deadbeef"
    write_registry([
        AgentEntry(
            name="deadbeef",
            cwd="/registered",
            log_path="",
            harness="codex",
            harness_session_id=registered_id,
        )
    ])
    _write_codex_session(_registry_home, store_only_id)

    with pytest.raises(dispatch.DispatchAskError) as exc:
        getattr(dispatch, f"{verb}_agent")("deadbeef")

    assert exc.value.exit_code == 2
    assert registered_id in str(exc.value)
    assert store_only_id in str(exc.value)


def test_attach_refuses_registry_name_that_collides_with_store_session(
    _registry_home, monkeypatch
):
    from fno.agents import dispatch
    from fno.agents.registry import AgentEntry, write_registry

    registered_id = "aaaaaaaa-1111-2222-3333-444455556666"
    store_only_id = "bbbbbbbb-1111-2222-3333-0000deadbeef"
    write_registry([
        AgentEntry(
            name="deadbeef",
            cwd="/registered",
            log_path="",
            harness="claude",
            harness_session_id=registered_id,
            short_id="aaaaaaaa",
        )
    ])
    _write_codex_session(_registry_home, store_only_id)
    attached = []
    monkeypatch.setattr(
        "fno.agents.providers.claude.claude_attach",
        lambda short: attached.append(short) or 0,
    )

    with pytest.raises(dispatch.DispatchAskError) as exc:
        dispatch.attach_agent("deadbeef")

    assert exc.value.exit_code == 2
    assert registered_id in str(exc.value)
    assert store_only_id in str(exc.value)
    assert attached == []


def test_dispatch_send_refuses_registry_name_colliding_with_store_session(
    _registry_home, monkeypatch
):
    """Directed send shares the resolver and never exact-name guesses."""
    from fno.agents import dispatch
    from fno.agents.registry import AgentEntry, write_registry

    registered_id = "aaaaaaaa-1111-2222-3333-444455556666"
    store_only_id = "bbbbbbbb-1111-2222-3333-0000deadbeef"
    write_registry([
        AgentEntry(
            name="deadbeef",
            cwd="/registered",
            log_path="",
            harness="claude",
            harness_session_id=registered_id,
            short_id="aaaaaaaa",
        )
    ])
    _write_codex_session(_registry_home, store_only_id)
    delivered = []
    monkeypatch.setattr(
        dispatch,
        "_mail_inject_claude",
        lambda *_args: delivered.append(True) or True,
    )

    with pytest.raises(dispatch.DispatchAskError) as exc:
        dispatch.dispatch_send(
            name="deadbeef",
            message="do not misroute",
            provider=None,
            cwd=_registry_home,
        )

    assert exc.value.exit_code == 2
    assert registered_id in str(exc.value)
    assert store_only_id in str(exc.value)
    assert delivered == []


def test_registry_full_id_and_unshaped_name_keep_store_fast_paths(
    _registry_home, monkeypatch
):
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="billing-worker",
            cwd="/repo/one",
            log_path="",
            harness="claude",
            harness_session_id=CLAUDE_UUID,
        )
    ])

    def _unexpected_probe(_token):
        raise AssertionError("fast-path token reached harness stores")

    monkeypatch.setattr(store_fallback, "probe_stores", _unexpected_probe)

    assert resolve_agent(CLAUDE_UUID).entry.name == "billing-worker"
    assert resolve_agent("billing-worker").entry.name == "billing-worker"


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


def test_registration_identity_collision_is_never_degraded_to_a_synthesized_row(
    _registry_home,
):
    """A designed ambiguity refusal is not a best-effort registry write failure."""
    from fno.agents.registry import AgentEntry, write_registry

    existing_id = "aaaaaaaa-1111-2222-3333-444455556666"
    store_only_id = "deadbeef-1111-7222-8333-444400000000"
    write_registry([
        AgentEntry(
            name="deadbeef",
            cwd="/registered",
            log_path="",
            harness="claude",
            harness_session_id=existing_id,
        )
    ])
    _write_codex_session(_registry_home, store_only_id)

    with pytest.raises(AgentResolutionError, match="canonical handle") as exc:
        store_fallback.heal_from_harness_store("deadbeef")

    assert exc.value.ambiguous is True
    assert [entry.harness_session_id for entry in load_registry()] == [existing_id]


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


def test_adoption_never_demotes_a_live_row(_registry_home):
    """A registration landing between the resolver's miss and the healer's
    locked upsert must survive: overwriting it with the healer's weaker
    'orphaned' would drop a running agent out of live routing."""
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    _write_claude_session(_registry_home, CLAUDE_UUID)
    write_registry([
        AgentEntry(name="racer", cwd="/live", log_path="", harness="claude",
                   harness_session_id=CLAUDE_UUID, status="live"),
    ])

    store_fallback.heal_from_harness_store("c655c326")

    assert load_registry()[0].status == "live"


def test_explicitless_registration_still_refreshes_status(_registry_home):
    """`/fno-me`'s re-register (no explicit status) keeps its old behavior."""
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        register_existing_session,
        write_registry,
    )

    write_registry([
        AgentEntry(name="racer", cwd="/live", log_path="", harness="claude",
                   harness_session_id=CLAUDE_UUID, status="live"),
    ])

    register_existing_session(
        provider="claude", session_id=CLAUDE_UUID, cwd="/live"
    )

    assert load_registry()[0].status == "idle"


def test_canonical_tail_resolves_codex_uuidv7_store(tmp_path, monkeypatch):
    sid = "019fb417-1111-7222-8333-444455556666"
    root = tmp_path / "codex" / "2026" / "07" / "30"
    root.mkdir(parents=True)
    rollout = root / f"rollout-2026-07-30T00-00-00-{sid}.jsonl"
    rollout.write_text(
        json.dumps({"type": "session_meta", "payload": {"id": sid, "cwd": "/x"}})
        + "\n"
    )
    monkeypatch.setenv("FNO_CODEX_SESSIONS_DIR", str(tmp_path / "codex"))
    hits = store_fallback.probe_stores("55556666")
    assert [(hit.harness, hit.session_id) for hit in hits] == [("codex", sid)]


def test_canonical_tail_resolves_mixed_case_opencode_store(tmp_path, monkeypatch):
    import sqlite3

    storage = tmp_path / "opencode" / "storage"
    storage.mkdir(parents=True)
    db = storage.parent / "opencode.db"
    con = sqlite3.connect(db)
    con.execute("CREATE TABLE session (id TEXT, directory TEXT)")
    sid = "ses_7f3a9b2cAbCd1234"
    con.execute("INSERT INTO session VALUES (?, ?)", (sid, "/x"))
    con.commit()
    con.close()
    monkeypatch.setenv("FNO_OPENCODE_STORAGE_DIR", str(storage))
    hits = store_fallback.probe_stores("AbCd1234")
    assert [hit.session_id for hit in hits] == [sid]
    assert store_fallback.probe_stores("abcd1234") == []


def test_legacy_opencode_tail_participates_in_cross_store_ambiguity(
    tmp_path, monkeypatch
):
    """A host without opencode.db still contributes its legacy JSON sessions."""
    from fno.agents.registry import AgentEntry, write_registry

    storage = tmp_path / "opencode" / "storage"
    session_dir = storage / "session" / "project"
    session_dir.mkdir(parents=True)
    opencode_sid = "ses_7f3a9b2cAbCd1234"
    (session_dir / f"{opencode_sid}.json").write_text(
        json.dumps({"id": opencode_sid, "directory": "/opencode"})
    )
    monkeypatch.setenv("FNO_OPENCODE_STORAGE_DIR", str(storage))
    registry_sid = "aaaaaaaa-1111-2222-3333-4444abcd1234"
    write_registry([
        AgentEntry(
            name="codex-worker",
            harness="codex",
            harness_session_id=registry_sid,
            cwd="/codex",
            log_path="",
        )
    ])

    with pytest.raises(AgentResolutionError, match="ambiguous across 2 sessions") as exc:
        resolve_agent("AbCd1234")

    assert registry_sid in str(exc.value)
    assert opencode_sid in str(exc.value)


def test_canonical_tail_collision_in_store_fallback_is_ambiguous(tmp_path, monkeypatch):
    from fno.agents.registry import AgentResolutionError

    root = tmp_path / "codex" / "2026" / "07" / "30"
    root.mkdir(parents=True)
    for index in (1, 2):
        sid = f"019fb417-1111-7222-8333-{index:04d}deadbeef"
        path = root / f"rollout-{index}-{sid}.jsonl"
        path.write_text(
            json.dumps({"type": "session_meta", "payload": {"id": sid, "cwd": "/x"}})
            + "\n"
        )
    monkeypatch.setenv("FNO_CODEX_SESSIONS_DIR", str(tmp_path / "codex"))
    with pytest.raises(AgentResolutionError, match="matches 2 sessions"):
        store_fallback.heal_from_harness_store(
            "deadbeef", registry_path=tmp_path / "registry.json"
        )


def test_canonical_tail_and_legacy_prefix_in_store_fallback_are_ambiguous(
    _registry_home,
):
    legacy_sid = "deadbeef-1111-7222-8333-444455556666"
    canonical_sid = "019fb417-1111-7222-8333-4444deadbeef"
    _write_codex_session(_registry_home, legacy_sid)
    _write_codex_session(_registry_home, canonical_sid)
    with pytest.raises(AgentResolutionError, match="matches 2 sessions") as exc:
        store_fallback.heal_from_harness_store("deadbeef")
    assert legacy_sid in str(exc.value)
    assert canonical_sid in str(exc.value)
    assert load_registry() == []


def test_codex_tail_probe_parses_every_rollout_before_identity_filter(
    tmp_path, monkeypatch
):
    """Turn-ID filenames require metadata-first filtering for complete coverage."""
    from fno.agents import discover

    root = tmp_path / "codex" / "2026" / "07" / "30"
    root.mkdir(parents=True)
    sid = "019fb417-1111-7222-8333-444455556666"
    wanted = root / f"rollout-now-{sid}.jsonl"
    noise = root / "rollout-now-aaaaaaaa-bbbb-7ccc-8ddd-eeeeffffffff.jsonl"
    wanted.write_text("{}\n")
    noise.write_text("{}\n")
    seen = []

    def meta(path):
        seen.append(path.name)
        if path == wanted:
            return sid, "/x"
        return "aaaaaaaa-bbbb-7ccc-8ddd-eeeeffffffff", "/noise"

    monkeypatch.setenv("FNO_CODEX_SESSIONS_DIR", str(tmp_path / "codex"))
    monkeypatch.setattr(discover, "_codex_meta", meta)
    assert [hit.session_id for hit in store_fallback.probe_stores("55556666")] == [sid]
    assert set(seen) == {wanted.name, noise.name}
