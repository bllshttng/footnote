"""The keystroke review lane asks the table before it types (x-a3e8).

A raw review verb dispatched at a harness whose row does not read
``features.review = native`` is refused by name, on both surfaces:
``--check`` answers not-injectable (exit 1) and the send refuses. The
codex-daemon RPC lane and every non-review payload are untouched.
"""
from __future__ import annotations

import pytest
import typer

from fno.agents.registry import AgentEntry
from fno.harness_identity import (
    HARNESS_SESSION_MARKERS,
    LEGACY_HARNESS_SESSION_MARKERS,
)
from fno.mail.cli import _raw_send
from fno.paths_testing import use_tmpdir

SID_AGY = "6f1a2b3c-0001-4000-8000-000000000001"

_HARNESS_MARKERS = tuple(
    m for m, _ in (*HARNESS_SESSION_MARKERS, *LEGACY_HARNESS_SESSION_MARKERS)
)


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


def _seed_harness(mailbox, monkeypatch, name: str, harness: str):
    """Seed a MUX-PANE keystroke recipient row for ``harness``. The keystroke
    lanes are the mux pane and claude's control.sock; a session-id-only row
    for any other harness reads <harness>-daemon, which is a non-keystroke
    lane with its own refusal. No prover pin: this seeds the RECIPIENT, and
    callers set whichever harness the SENDER is."""
    for marker in _HARNESS_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    entry = AgentEntry(
        name=name,
        harness=harness,
        harness_session_id=SID_AGY,
        cwd=str(mailbox),
        log_path="",
        status="live",
        mux={"session": "fno", "pane_id": 7},
    )
    monkeypatch.setattr(
        "fno.agents.registry.resolve_agent",
        lambda _name: type("R", (), {"entry": entry})(),
    )


def test_raw_review_on_an_unmeasured_review_row_is_refused_naming_the_row(
    mailbox, monkeypatch, capsys
):
    _seed_harness(mailbox, monkeypatch, "agypeer", "agy")
    with pytest.raises(typer.Exit) as refused:
        _raw_send("agypeer", "/review", self_ok=False)
    assert refused.value.exit_code != 0
    err = capsys.readouterr().err
    assert "features.review" in err
    assert "'unmeasured'" in err
    assert "harness probe agy" in err
    assert "--raw" in err, "the refusal teaches the text-delivery remedy"


def test_raw_review_check_answers_not_injectable_without_injecting(
    mailbox, monkeypatch, capsys
):
    _seed_harness(mailbox, monkeypatch, "agypeer", "agy")
    with pytest.raises(typer.Exit) as refused:
        _raw_send("agypeer", "/fno:review --comment", self_ok=False, check=True)
    assert refused.value.exit_code == 1
    out = capsys.readouterr().out
    assert out.startswith("not-injectable:")
    assert "'unmeasured'" in out


def test_the_gate_matches_every_review_spelling_and_only_review():
    from fno.mail.cli import _REVIEW_VERB_RE

    for verb in ("/review", "/code-review", "/fno:review", "/CODE-REVIEW", "/codex:review"):
        assert _REVIEW_VERB_RE.match(verb), verb
    for verb in ("/preview", "/compact", "/reviewx", "/model", "review", "/rev"):
        assert not _REVIEW_VERB_RE.match(verb), verb


def test_an_absent_review_row_refuses_in_absent_words(mailbox, monkeypatch, capsys):
    import fno.agents.harness_map as harness_map

    _seed_harness(mailbox, monkeypatch, "agypeer", "agy")
    monkeypatch.setattr(
        harness_map, "feature_claim", lambda name, key: "absent"
    )
    with pytest.raises(typer.Exit) as refused:
        _raw_send("agypeer", "/review", self_ok=False)
    assert refused.value.exit_code != 0
    err = capsys.readouterr().err
    assert "'absent'" in err
    assert "ships no review command" in err


def test_a_native_review_row_lets_the_send_proceed_unchanged(
    mailbox, monkeypatch, capsys
):
    _seed_harness(mailbox, monkeypatch, "codexpeer", "codex")
    pasted: list = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mux_pane_send",
        lambda *args, **kwargs: pasted.append(args) or True,
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.mail_inject_probe",
        lambda _session_id: (True, "injectable"),
    )
    with pytest.raises(typer.Exit) as sent:
        _raw_send("codexpeer", "/code-review medium --comment", self_ok=False)
    assert sent.value.exit_code == 0
    assert pasted, "a native row reaches the pane keystroke exactly as before"
