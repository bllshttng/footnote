"""Per-harness session teardown in ``fno agents rm``.

``rm`` used to be registry-only for every non-claude harness, so a removed
codex/opencode session kept resurfacing in discovery. These cover the real
teardown arms.

ACs:
- AC1-HP  : codex teardown drops the index entry, every other line survives
            byte-identical, registry row gone, transcripts untouched.
- AC2-HP  : opencode teardown shells the supported delete verb, row gone.
- AC1-ERR : teardown failure without --force preserves the registry row.
- AC2-ERR : --force drops the row and WARNs, naming the orphan record.
- AC1-EDGE: an already-absent harness record is idempotent success.
- AC2-EDGE: a non-UUID codex id refuses rather than matching every line
            (the silent-index-wipe guard).
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

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

SES = "ses_7f3a9b2c1d"


class _Run:
    """Stand-in for subprocess.run capturing argv."""

    def __init__(self, returncode: int = 0, stdout: str = "", stderr: str = ""):
        self.returncode, self.stdout, self.stderr = returncode, stdout, stderr
        self.argv: list[str] | None = None

    def __call__(self, argv, **kwargs):
        self.argv = argv
        return self


def _with_opencode(monkeypatch, run: _Run) -> None:
    monkeypatch.setattr(opencode_mod, "_subprocess_run", run)
    monkeypatch.setattr("shutil.which", lambda name: "/usr/bin/opencode")


def test_opencode_rm_deletes_session_and_row(isolated_state, monkeypatch):
    """AC2-HP."""
    run = _Run(stdout=f"Session {SES} deleted")
    _with_opencode(monkeypatch, run)
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    rm_agent("ocw")

    assert run.argv == ["opencode", "--pure", "session", "delete", SES]
    assert _names() == []


def test_opencode_rm_missing_session_is_idempotent(isolated_state, monkeypatch):
    """AC1-EDGE: opencode reports an absent session as exit 1, not success."""
    _with_opencode(monkeypatch, _Run(returncode=1, stderr=f"Error: Session not found: {SES}"))
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    rm_agent("ocw")

    assert _names() == []


def test_opencode_rm_preserves_row_on_failure(isolated_state, monkeypatch):
    """AC1-ERR."""
    _with_opencode(monkeypatch, _Run(returncode=1, stderr="Error: database is locked"))
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    with pytest.raises(DispatchAskError):
        rm_agent("ocw")

    assert _names() == ["ocw"]


def test_opencode_rm_missing_binary_preserves_row(isolated_state, monkeypatch):
    """AC1-ERR: not-on-PATH mirrors claude's exit 14 family."""
    monkeypatch.setattr("shutil.which", lambda name: None)
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    with pytest.raises(DispatchAskError) as exc:
        rm_agent("ocw")

    assert exc.value.exit_code == 14
    assert _names() == ["ocw"]


def test_opencode_rm_timeout_preserves_row(isolated_state, monkeypatch):
    """A hung `opencode` must not hang rm, and must not drop the row."""
    def _boom(argv, **kwargs):
        raise subprocess.TimeoutExpired(argv, 30)

    monkeypatch.setattr(opencode_mod, "_subprocess_run", _boom)
    monkeypatch.setattr("shutil.which", lambda name: "/usr/bin/opencode")
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    with pytest.raises(DispatchAskError) as exc:
        rm_agent("ocw")

    assert exc.value.exit_code == 15
    assert _names() == ["ocw"]


def test_opencode_rm_force_drops_row_and_warns(isolated_state, monkeypatch, capsys):
    """AC2-ERR."""
    _with_opencode(monkeypatch, _Run(returncode=1, stderr="Error: database is locked"))
    _seed("ocw", harness="opencode", session_id=SES, cwd=isolated_state)

    rm_agent("ocw", force=True)

    err = capsys.readouterr().err
    assert "WARN" in err and SES in err
    assert "opencode" in err, "the orphan's harness must be named too"
    assert _names() == []


def test_opencode_session_delete_refuses_malformed_id(monkeypatch):
    """No shell metacharacter can reach the subprocess."""
    run = _Run()
    monkeypatch.setattr(opencode_mod, "_subprocess_run", run)

    for bad in ("", "abc", "ses_", "ses_x'; drop table session;--"):
        with pytest.raises(ValueError):
            opencode_mod.session_delete(bad)

    assert run.argv is None, "a malformed id must never reach the subprocess"


def test_unknown_harness_still_refuses(isolated_state):
    """The not-implemented arm survives for genuinely unsupported harnesses."""
    _seed("weird", harness="agy", session_id="x", cwd=isolated_state)

    with pytest.raises(DispatchAskError) as exc:
        rm_agent("weird")

    assert exc.value.exit_code == 2
    assert _names() == ["weird"]
