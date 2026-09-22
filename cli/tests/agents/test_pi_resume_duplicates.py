"""The reader half: a resume whose id resolves to more than one session.

This defect OUTLIVES the create serialiser, which is why it is a separate
lane rather than a corollary. Duplicates can pre-exist from an earlier run, a
crash, or a pi someone started by hand, and no claim taken today can
retroactively serialise one already sitting on disk.

pi's own behaviour is what is refused here: it picks the OLDEST file, writes no
new one, prints nothing, and gives no hint that other sessions carry that id.
"""
from __future__ import annotations

import sys
from pathlib import Path

import pytest

_TEST_DIR = Path(__file__).resolve().parent
_SRC = _TEST_DIR.parents[1] / "src"
if str(_SRC) not in sys.path:
    sys.path.insert(0, str(_SRC))

from fno.agents.harnesses.pi import (  # noqa: E402
    duplicate_resume_refusal,
    encode_cwd,
    lookup_sessions,
    session_dir,
)

CWD = "/repo/worktrees/pi-dupes"
SESSION_ID = "fno-race-0001"
STAMPS = (
    "2026-08-28T20-58-10-768Z",
    "2026-08-28T20-58-10-799Z",
    "2026-08-28T20-58-10-810Z",
    "2026-08-28T20-58-10-817Z",
)


@pytest.fixture
def store(tmp_path, monkeypatch):
    monkeypatch.setenv("PI_HOME", str(tmp_path / "pi-home"))
    directory = session_dir(CWD)
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def _seed(directory: Path, stamp: str, *, body: str) -> Path:
    path = directory / f"{stamp}_{SESSION_ID}.jsonl"
    path.write_text(body)
    return path


def test_AC6_HP_a_duplicate_id_refuses_and_names_both_with_timestamps(store):
    """AC6-HP: two sessions on one id, both named, neither selected."""
    _seed(store, STAMPS[0], body='{"role": "assistant", "content": ["BRAVO"]}\n')
    _seed(store, STAMPS[1], body='{"role": "assistant", "content": ["DELTA"]}\n')

    lookup = lookup_sessions(CWD, SESSION_ID)
    assert lookup.state == "duplicate", lookup
    message = duplicate_resume_refusal(CWD, SESSION_ID, lookup)
    assert message is not None, "a duplicate id must refuse"
    assert STAMPS[0] in message, message
    assert STAMPS[1] in message, message
    assert "None was selected" in message, message


def test_AC6_EDGE_the_refusal_names_the_other_sessions_not_only_this_one(store):
    """AC6-EDGE: every duplicate is named, not just the one being resumed.

    The codex short-id precedent is exact: a refusal that named only the
    victim's OWN row steered a worker to a wrong conclusion about which session
    was the problem.
    """
    for stamp in STAMPS:
        _seed(store, stamp, body='{"role": "user"}\n')
    lookup = lookup_sessions(CWD, SESSION_ID)
    message = duplicate_resume_refusal(CWD, SESSION_ID, lookup)
    assert message is not None
    for stamp in STAMPS:
        assert stamp in message, f"{stamp} missing from refusal:\n{message}"
    assert "4 sessions" in message, message


def test_AC6_FAIL_an_empty_assistant_array_is_never_ranked_as_the_lesser(store):
    """AC6-FAIL: duplicates whose turns all FAILED still refuse, and no file is
    preferred for being fuller.

    An empty assistant ``content`` array does NOT mean an idle or empty
    session. It means a turn was ATTEMPTED and failed - a run that died on an
    expired provider token left exactly that shape. Ranking by content would
    preferentially discard the session that errored, which is usually the one
    someone needs to read.
    """
    empty = _seed(store, STAMPS[0], body='{"role": "assistant", "content": []}\n')
    full = _seed(
        store, STAMPS[1], body='{"role": "assistant", "content": ["lots of text"]}\n'
    )
    lookup = lookup_sessions(CWD, SESSION_ID)
    message = duplicate_resume_refusal(CWD, SESSION_ID, lookup)
    assert message is not None
    assert str(empty) in message and str(full) in message, message
    # Neither is chosen, and the refusal says why ranking by content is wrong.
    assert "None was selected" in message, message
    assert "FAILED" in message, message
    assert lookup.files == (empty, full), "oldest first, by filename timestamp"


def test_a_single_session_does_not_refuse(store):
    _seed(store, STAMPS[0], body='{"role": "user"}\n')
    lookup = lookup_sessions(CWD, SESSION_ID)
    assert lookup.state == "one"
    assert duplicate_resume_refusal(CWD, SESSION_ID, lookup) is None


def test_an_unreadable_store_reads_unknown_and_never_none(tmp_path, monkeypatch):
    """The absence with two explanations is never reported as the convenient one.

    A missing directory means this reading cannot see anything. Reporting it as
    "no duplicates" would be the absence-is-not-an-outcome trap, and it would
    certify exactly the state the create race produces.
    """
    monkeypatch.setenv("PI_HOME", str(tmp_path / "no-such-pi-home"))
    lookup = lookup_sessions(CWD, SESSION_ID)
    assert lookup.state == "unknown", lookup
    assert lookup.reason, "an unknown reading must say why it could not read"
    assert duplicate_resume_refusal(CWD, SESSION_ID, lookup) is None


def test_the_cwd_encoding_matches_three_observed_directories():
    """Pinned against real directories in a live ``~/.pi/agent/sessions``.

    Mirrors ``encode_cwd`` in ``crates/fno-agents/src/pi.rs``.
    """
    assert (
        encode_cwd("/Users/bb16/code/footnote/footnote")
        == "--Users-bb16-code-footnote-footnote--"
    )
    assert encode_cwd("/private/tmp") == "--private-tmp--"
    # A dot-prefixed component survives unchanged; only separators move.
    assert encode_cwd("/home/u/.local/tmp/piprobe") == "--home-u-.local-tmp-piprobe--"


def test_one_id_in_two_worktrees_is_two_sessions(tmp_path, monkeypatch):
    """The identity is the PAIR, so a resume from canonical cannot see a
    worktree's session - and must not silently look somewhere else."""
    monkeypatch.setenv("PI_HOME", str(tmp_path / "pi-home"))
    canonical = "/repo"
    worktree = "/repo/worktrees/feature"
    directory = session_dir(worktree)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / f"{STAMPS[0]}_{SESSION_ID}.jsonl").write_text("{}\n")

    assert lookup_sessions(worktree, SESSION_ID).state == "one"
    assert lookup_sessions(canonical, SESSION_ID).state == "unknown"
