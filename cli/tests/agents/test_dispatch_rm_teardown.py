"""Per-harness session teardown in ``fno agents rm``.

``rm`` used to be registry-only for every non-claude harness, leaving the
harness's own session record behind. codex now gets a real teardown arm;
opencode stays registry-only on purpose (see its section below).

ACs:
- AC1-HP  : codex teardown drops the index entry, every other line survives
            byte-identical, registry row gone, transcripts untouched.
- AC1-ERR : teardown failure without --force preserves the registry row.
- AC2-ERR : --force drops the row and WARNs, naming the orphan record.
- AC1-EDGE: an already-absent harness record is idempotent success.
- AC2-EDGE: a malformed id refuses and preserves the row, rather than
            matching lines it should not (the silent-index-wipe guard).
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.agents import dispatch as dispatch_mod
from fno.agents.dispatch import DispatchAskError, rm_agent
from fno.agents.providers import codex as codex_mod
from fno.agents.providers import opencode as opencode_mod
from fno.agents.registry import AgentEntry, load_registry, update_registry

KEEP_ID = "aaaaaaaa-1111-2222-3333-444444444444"
GONE_ID = "bbbbbbbb-5555-6666-7777-888888888888"


@pytest.fixture
def isolated_state(tmp_path: Path, monkeypatch) -> Path:
    from fno import paths

    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(paths, "agents_registry_path", lambda: tmp_path / "registry.jsonl")
    monkeypatch.setattr(paths, "state_dir", lambda: tmp_path / "state")
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    return tmp_path


def _seed(name: str, *, harness: str, session_id: str, cwd: Path) -> None:
    update_registry(
        lambda entries: entries
        + [
            AgentEntry(
                name=name,
                harness=harness,
                cwd=str(cwd),
                log_path=str(cwd / f"{name}.log"),
                harness_session_id=session_id,
            )
        ]
    )


def _names() -> list[str]:
    return [e.name for e in load_registry()]


def _write_index(home: Path, ids: list[str]) -> Path:
    path = home / ".codex" / "session_index.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(f'{{"id":"{i}","cwd":"/tmp"}}\n' for i in ids), encoding="utf-8"
    )
    return path


# --------------------------------------------------------------------- codex


def test_codex_rm_drops_index_entry_and_spares_the_rest(isolated_state, capsys):
    """AC1-HP."""
    home = Path.home()
    index = _write_index(home, [KEEP_ID, GONE_ID])
    transcript = home / ".codex" / "sessions" / f"{GONE_ID}.jsonl"
    transcript.parent.mkdir(parents=True, exist_ok=True)
    transcript.write_text("rollout", encoding="utf-8")
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)

    rm_agent("worker")

    remaining = index.read_text(encoding="utf-8")
    assert GONE_ID not in remaining
    assert remaining == f'{{"id":"{KEEP_ID}","cwd":"/tmp"}}\n'
    assert _names() == []
    assert transcript.read_text(encoding="utf-8") == "rollout", "transcripts must survive"


def test_codex_rm_is_idempotent_when_entry_already_gone(isolated_state):
    """AC1-EDGE: a manually cleaned index must not wedge rm."""
    _write_index(Path.home(), [KEEP_ID])
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)

    rm_agent("worker")

    assert _names() == []


def test_codex_rm_with_no_index_file_still_removes_row(isolated_state):
    """Fresh codex install: no index on disk at all."""
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)

    rm_agent("worker")

    assert _names() == []


def test_codex_teardown_refuses_non_uuid_id(isolated_state):
    """AC2-EDGE: substring matching makes a loose id a whole-index wipe."""
    index = _write_index(Path.home(), [KEEP_ID, GONE_ID])
    before = index.read_text(encoding="utf-8")

    for bad in ("", "id", "not-a-uuid"):
        with pytest.raises(ValueError):
            codex_mod.remove_session_index_entry(bad)

    assert index.read_text(encoding="utf-8") == before


def test_codex_spares_a_row_that_merely_mentions_the_id(isolated_state):
    """The index carries a free-text thread_name, so substring matching bites.

    A session named after another session's uuid must survive its removal.
    """
    home = Path.home()
    index = home / ".codex" / "session_index.jsonl"
    index.parent.mkdir(parents=True, exist_ok=True)
    bystander = f'{{"id":"{KEEP_ID}","thread_name":"investigate {GONE_ID}"}}\n'
    index.write_text(
        bystander + f'{{"id":"{GONE_ID}","thread_name":"the target"}}\n',
        encoding="utf-8",
    )
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)

    rm_agent("worker")

    assert index.read_text(encoding="utf-8") == bystander


def test_codex_keeps_lines_it_cannot_parse(isolated_state):
    """Never remove what you do not understand."""
    home = Path.home()
    index = home / ".codex" / "session_index.jsonl"
    index.parent.mkdir(parents=True, exist_ok=True)
    index.write_text(
        f"not json at all {GONE_ID}\n" + f'{{"id":"{GONE_ID}"}}\n', encoding="utf-8"
    )
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)

    rm_agent("worker")

    assert index.read_text(encoding="utf-8") == f"not json at all {GONE_ID}\n"


def test_codex_rewrite_preserves_file_mode(isolated_state):
    index = _write_index(Path.home(), [KEEP_ID, GONE_ID])
    index.chmod(0o600)

    codex_mod.remove_session_index_entry(GONE_ID)

    assert index.stat().st_mode & 0o777 == 0o600


def test_codex_non_string_stored_id_refuses_cleanly(isolated_state):
    """A corrupt row must not surface as a TypeError traceback."""
    with pytest.raises(ValueError):
        codex_mod.remove_session_index_entry(123)


def test_rm_without_session_id_refuses_then_forces(isolated_state, capsys):
    """The harness record may exist; dropping the row silently orphans it."""
    _seed("worker", harness="codex", session_id="", cwd=isolated_state)

    with pytest.raises(DispatchAskError) as exc:
        rm_agent("worker")
    assert exc.value.exit_code == 12
    assert _names() == ["worker"]

    rm_agent("worker", force=True)
    assert _names() == []
    assert "WARN" in capsys.readouterr().err


def test_codex_rm_preserves_row_when_index_unwritable(isolated_state, monkeypatch):
    """AC1-ERR: ordering invariant holds for the codex arm."""
    _write_index(Path.home(), [GONE_ID])
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)
    monkeypatch.setattr(
        codex_mod,
        "remove_session_index_entry",
        lambda *a, **k: (_ for _ in ()).throw(OSError("read-only file system")),
    )

    with pytest.raises(DispatchAskError) as exc:
        rm_agent("worker")

    assert exc.value.exit_code == 1
    assert _names() == ["worker"]


def test_codex_rm_force_drops_row_and_warns(isolated_state, monkeypatch, capsys):
    """AC2-ERR: the orphan is named by BOTH harness and session id."""
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)
    monkeypatch.setattr(
        codex_mod,
        "remove_session_index_entry",
        lambda *a, **k: (_ for _ in ()).throw(OSError("boom")),
    )

    rm_agent("worker", force=True)

    err = capsys.readouterr().err
    assert "WARN" in err
    assert GONE_ID in err, "the orphan record must be named for manual cleanup"
    assert "codex" in err, "the orphan's harness must be named too"
    assert _names() == []


def test_forced_teardown_failure_emits_exactly_one_truthful_event(
    isolated_state, monkeypatch
):
    """The forced path must not emit twice, nor claim a mutation not yet made."""
    emitted: list[dict] = []
    monkeypatch.setattr(
        dispatch_mod.events, "emit", lambda kind, **kw: emitted.append({"kind": kind, **kw})
    )
    _seed("worker", harness="codex", session_id=GONE_ID, cwd=isolated_state)
    monkeypatch.setattr(
        codex_mod,
        "remove_session_index_entry",
        lambda *a, **k: (_ for _ in ()).throw(OSError("boom")),
    )

    rm_agent("worker", force=True)

    removed = [e for e in emitted if e["kind"] == "agent_removed"]
    assert len(removed) == 1, f"expected one agent_removed, got {len(removed)}"
    assert removed[0]["registry_changed"] is True
    assert "boom" in (removed[0].get("teardown_error") or "")


def test_codex_malformed_stored_id_preserves_row(isolated_state):
    """AC2-EDGE end-to-end: the refusal must reach the registry, not just the arm."""
    index = _write_index(Path.home(), [KEEP_ID])
    before = index.read_text(encoding="utf-8")
    _seed("worker", harness="codex", session_id="not-a-uuid", cwd=isolated_state)

    with pytest.raises(DispatchAskError) as exc:
        rm_agent("worker")

    assert exc.value.exit_code == 12
    assert _names() == ["worker"], "a refused teardown must not drop the row"
    assert index.read_text(encoding="utf-8") == before


def test_codex_rewrite_is_atomic_and_leaves_no_temp_file(isolated_state, monkeypatch):
    """The atomicity claim, mutation-tested: a failed rename must change nothing.

    Guards the temp-file cleanup too -- a direct in-place write, or a missing
    unlink, both show up here.
    """
    index = _write_index(Path.home(), [KEEP_ID, GONE_ID])
    before = index.read_text(encoding="utf-8")
    monkeypatch.setattr(
        codex_mod.os,
        "replace",
        lambda *a, **k: (_ for _ in ()).throw(OSError("rename failed")),
    )

    with pytest.raises(OSError):
        codex_mod.remove_session_index_entry(GONE_ID)

    assert index.read_text(encoding="utf-8") == before, "index must survive intact"
    assert list(index.parent.glob("*.fno-rm.*.tmp")) == [], "no stranded temp file"


# ------------------------------------------------------------------ opencode
#
# opencode is registry-only ON PURPOSE. `opencode session delete` is the
# store's only deletion verb and it takes the session's child sessions and
# every message row with it (`message.session_id` is ON DELETE CASCADE), so
# there is no record-only teardown to perform. `rm` must not destroy a
# conversation as a side effect of cleaning up a registry row.

SES = "ses_7f3a9b2c1d"


def test_opencode_rm_is_registry_only_and_says_so(isolated_state, capsys):
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    rm_agent("ocw")

    out = capsys.readouterr().out
    assert _names() == []
    assert SES in out, "the surviving session must be named"
    assert "opencode session delete" in out, "the escape hatch must be offered"


def test_opencode_rm_never_shells_out(isolated_state, monkeypatch):
    """The whole point: no deletion command may run for an opencode row."""
    def _explode(*a, **k):
        raise AssertionError("rm must not shell out for an opencode agent")

    monkeypatch.setattr(subprocess, "run", _explode)
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    rm_agent("ocw")

    assert _names() == []


def test_opencode_session_id_shape_guard():
    for good in (SES, "ses_abc123"):
        assert opencode_mod.is_session_id(good)
    for bad in ("", "abc", "ses_", "ses_x'; drop table session;--", None, 123):
        assert not opencode_mod.is_session_id(bad)


def test_unknown_harness_still_refuses(isolated_state):
    """The not-implemented arm survives for genuinely unsupported harnesses."""
    _seed("weird", harness="agy", session_id="x", cwd=isolated_state)

    with pytest.raises(DispatchAskError) as exc:
        rm_agent("weird")

    assert exc.value.exit_code == 2
    assert _names() == ["weird"]
