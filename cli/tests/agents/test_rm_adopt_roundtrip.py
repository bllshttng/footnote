"""The reap-to-adopt round trip, and the notices that name it.

The claim this node rests on is that `fno agents rm` forfeits a resume handle
REVERSIBLY: the harness session record goes, the transcript stays, and `fno
agents adopt` rebuilds the row from what is left. Every notice added here
prints that recovery, so a test that never checks it would ship a promise
nobody verified.

`adopt` itself is a Rust verb, so these tests pin the Python half its recovery
actually depends on: the store probe that `adopt`'s third resolution step runs
(`heal_token` -> `probe_stores`), which is what finds a reaped session by its
8-hex short id. That is the load-bearing link, and it is hermetic.

ACs:
- AC1-HP  : rm writes the session id and the literal adopt verb to stderr
            BEFORE any harness teardown is issued, and still completes.
- AC2-HP  : the transcript survives rm, and the store probe still resolves the
            short id afterwards -- the round trip adopt performs.
- AC1-ERR : an operator answering no at the TTY prompt stops the reap.
- AC2-ERR : --force never prompts, and still writes the warning.
- AC1-EDGE: a row with no resume handle at all gets no notice naming an empty
            one.
- AC1-UI  : peek's miss names the registry it read, says the transcript is on
            disk when it is, and prints the adopt command.
"""
from __future__ import annotations

import io
from pathlib import Path

import pytest

from fno.agents import rm_notice
from fno.agents.dispatch import rm_agent
from fno.agents.registry import AgentEntry, load_registry, update_registry

# A full claude session uuid and the 8-hex short id an operator actually peeks
# and adopts by. The short is the uuid's first eight, matching canonical_handle.
SESSION_ID = "0a6e775f-1111-2222-3333-444444444444"
SHORT_ID = "0a6e775f"


@pytest.fixture
def isolated_state(tmp_path: Path, monkeypatch) -> Path:
    from fno import paths

    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(paths, "agents_registry_path", lambda: tmp_path / "registry.jsonl")
    monkeypatch.setattr(paths, "state_dir", lambda: tmp_path / "state")
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    return tmp_path


def _seed_row(name: str = "reaped-worker", *, short_id: str = SHORT_ID) -> None:
    update_registry(
        lambda entries: entries
        + [
            AgentEntry(
                name=name,
                harness="claude",
                cwd="/tmp",
                log_path="/tmp/reaped-worker.log",
                harness_session_id=SESSION_ID,
                short_id=short_id,
            )
        ]
    )


def _seed_transcript(tmp_path: Path, monkeypatch) -> Path:
    """A claude transcript on disk, named by its session uuid as claude names it."""
    from fno.agents.discover import PROJECTS_DIR_ENV

    root = tmp_path / "projects" / "-tmp-project"
    root.mkdir(parents=True)
    path = root / f"{SESSION_ID}.jsonl"
    path.write_text(
        '{"type":"user","cwd":"/tmp","message":{"role":"user","content":"hi"}}\n',
        encoding="utf-8",
    )
    monkeypatch.setenv(PROJECTS_DIR_ENV, str(tmp_path / "projects"))
    return path


def _names() -> list[str]:
    return [e.name for e in load_registry()]


# --------------------------------------------------------------------------
# AC1-HP / AC2-ERR: the warning precedes the teardown
# --------------------------------------------------------------------------


def test_rm_warns_before_the_teardown_and_names_adopt(
    isolated_state, monkeypatch, capsys
):
    """AC1-HP: the notice is written BEFORE `claude rm` is invoked, not after.

    Ordering is the whole point: a warning printed after the shellout tells an
    operator about a loss they can no longer decline. The recorder below
    captures the stderr length at shellout time, so the assertion is on
    sequence, not merely on both things happening.
    """
    _seed_row()
    seen_before_shellout: dict[str, str] = {}

    import sys

    from fno.agents.harnesses import claude as claude_mod

    def fake_rm(short_id, *, timeout=30.0):
        seen_before_shellout["stderr"] = sys.stderr.getvalue()  # type: ignore[attr-defined]
        seen_before_shellout["short_id"] = short_id
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)
    monkeypatch.setattr("fno.agents.dispatch.is_provider_available", lambda _p: True)

    rm_agent("reaped-worker", force=True)

    # Positive markers, not an absence: assert the exact strings the recovery
    # needs, so a crashed writer that prints nothing cannot pass as a warning.
    warned = seen_before_shellout["stderr"]
    assert SHORT_ID in warned, warned
    assert "fno agents adopt" in warned, warned
    assert "resume handle" in warned, warned
    assert seen_before_shellout["short_id"] == SHORT_ID
    assert "reaped-worker" not in _names()


def test_rm_notice_omits_a_row_with_no_handle(isolated_state):
    """AC1-EDGE: no handle means no notice, never `fno agents adopt ` with a blank."""
    entry = AgentEntry(
        name="handle-less",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/x.log",
    )
    assert rm_notice.resume_handle_for(entry) is None


def test_rm_notice_prefers_short_id_then_session_id():
    """The id adopt takes back, in the order adopt resolves them."""
    with_short = AgentEntry(
        name="a",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/a.log",
        harness_session_id=SESSION_ID,
        short_id=SHORT_ID,
    )
    assert rm_notice.resume_handle_for(with_short) == SHORT_ID

    session_only = AgentEntry(
        name="b",
        harness="codex",
        cwd="/tmp",
        log_path="/tmp/b.log",
        harness_session_id=SESSION_ID,
    )
    assert rm_notice.resume_handle_for(session_only) == SESSION_ID


# --------------------------------------------------------------------------
# AC1-ERR / AC2-ERR: the confirmation gate
# --------------------------------------------------------------------------


class _FakeTTY(io.StringIO):
    def __init__(self, answer: str = "") -> None:
        super().__init__(answer)

    def isatty(self) -> bool:  # noqa: D102 - a TTY for the prompt's purposes
        return True


def test_declining_at_the_prompt_stops_the_reap(isolated_state):
    """AC1-ERR: anything but a yes is a no, and nothing is torn down."""
    _seed_row()
    err = _FakeTTY()
    assert rm_notice.warn_and_confirm(
        "reaped-worker", stderr=err, stdin=_FakeTTY("n\n")
    ) is False
    assert "fno agents adopt" in err.getvalue()


def test_accepting_at_the_prompt_proceeds(isolated_state):
    _seed_row()
    err = _FakeTTY()
    assert rm_notice.warn_and_confirm(
        "reaped-worker", stderr=err, stdin=_FakeTTY("y\n")
    ) is True


def test_force_never_prompts_but_still_warns(isolated_state):
    """AC2-ERR: --force skips the question, never the notice."""
    _seed_row()
    err = _FakeTTY()
    stdin = _FakeTTY("")  # a read here would return "" and refuse
    assert rm_notice.warn_and_confirm(
        "reaped-worker", force=True, stderr=err, stdin=stdin
    ) is True
    assert "fno agents adopt" in err.getvalue()
    assert "anyway?" not in err.getvalue()


def test_non_tty_proceeds_without_blocking(isolated_state):
    """An unattended sweep is warned and proceeds; a prompt there wedges the fleet."""
    _seed_row()
    err = io.StringIO()
    assert rm_notice.warn_and_confirm(
        "reaped-worker", stderr=err, stdin=io.StringIO("")
    ) is True
    assert "fno agents adopt" in err.getvalue()
    assert "anyway?" not in err.getvalue()


def test_a_prompt_nobody_can_read_is_never_asked(isolated_state):
    """The silent-hang guard: stdin is a TTY but stderr is redirected away.

    `dispatch-node.sh` and `spawn.sh` both run `fno agents rm ... >/dev/null
    2>&1` with stdin inherited. Prompting there would block forever on a
    question sent to /dev/null -- a silent hang produced by the guard meant to
    prevent a silent loss. The read below would return "" and refuse, so a
    regression here fails as a False, not as a timeout.
    """
    _seed_row()
    err = io.StringIO()  # NOT a tty: the prompt would be invisible
    assert rm_notice.warn_and_confirm(
        "reaped-worker", stderr=err, stdin=_FakeTTY("")
    ) is True
    assert "anyway?" not in err.getvalue()


# --------------------------------------------------------------------------
# AC2-HP: the round trip adopt actually performs
# --------------------------------------------------------------------------


def test_transcript_survives_rm_and_the_short_id_still_resolves(
    isolated_state, tmp_path, monkeypatch
):
    """AC2-HP: rm takes the row, not the conversation, and adopt can find it.

    This is the claim every notice in this change makes. `adopt`'s third
    resolution step is the harness store probe, so if the probe still answers
    for the short id after a reap, the advertised recovery is real. Asserting
    it here rather than trusting the string.
    """
    from fno.agents.store_fallback import probe_stores

    transcript = _seed_transcript(tmp_path, monkeypatch)
    _seed_row()

    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "claude_rm", lambda sid, *, timeout=30.0: (0, ""))
    monkeypatch.setattr("fno.agents.dispatch.is_provider_available", lambda _p: True)

    assert probe_stores(SHORT_ID), "precondition: the store resolves the short id"

    rm_agent("reaped-worker", force=True)

    assert "reaped-worker" not in _names(), "the registry row is gone"
    assert transcript.exists(), "the transcript is NOT what rm removes"
    hits = probe_stores(SHORT_ID)
    assert [h.session_id for h in hits] == [SESSION_ID], (
        "adopt's store probe must still answer for the short id after a reap; "
        "otherwise every `fno agents adopt` hint this change prints is a lie"
    )


# --------------------------------------------------------------------------
# AC1-UI: peek names the instrument
# --------------------------------------------------------------------------


def test_peek_miss_names_the_registry_and_the_adopt_command(tmp_path, monkeypatch):
    """A miss says which surface it read, and never reports an absence bare."""
    from fno.agents.peek import EXIT_NOT_FOUND, peek

    err = io.StringIO()
    rc = peek(
        "nobody-here",
        stdout=io.StringIO(),
        stderr=err,
        resolve=lambda _h: (None, []),
        projects_root=tmp_path / "empty",
        mux_lookup=lambda _h: None,
    )
    text = err.getvalue()
    assert rc == EXIT_NOT_FOUND
    assert "peer not found in the registry" in text, text
    assert "fno agents adopt nobody-here" in text, text
    # The exit code alone is not the assertion: a crashed binary also fails to
    # print a peer. The adopt hint is the positive marker.


def test_peek_miss_says_so_when_the_transcript_is_on_disk(tmp_path, monkeypatch):
    """The exact case that nearly cost 4.9M of context: row gone, conversation intact."""
    from fno.agents.peek import EXIT_NOT_FOUND, peek

    _seed_transcript(tmp_path, monkeypatch)

    err = io.StringIO()
    rc = peek(
        SHORT_ID,
        stdout=io.StringIO(),
        stderr=err,
        resolve=lambda _h: (None, []),
        projects_root=tmp_path / "projects",
        mux_lookup=lambda _h: None,
    )
    text = err.getvalue()
    assert rc == EXIT_NOT_FOUND
    assert "transcript IS on disk" in text, text
    # The hint names the FULL session id the store resolved, not the short the
    # operator typed: adopt takes either, and the resolved one is unambiguous.
    assert f"fno agents adopt {SESSION_ID}" in text, text
