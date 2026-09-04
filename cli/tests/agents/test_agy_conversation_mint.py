"""agy mints its conversation id through one print-mode turn.

The envelope is the ONLY surface that returns the id, so every way that read
can fail has to raise rather than hand the keeper a name it made up. The live
shape asserted here is the one measured 2026-09-03 on agy 1.1.24.
"""
from __future__ import annotations

import json
import subprocess

import pytest

from fno.agents.harnesses import agy


def _completed(returncode: int = 0, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(
        args=["agy"], returncode=returncode, stdout=stdout, stderr=stderr
    )


ENVELOPE = {
    "conversation_id": "c5661b28-bcba-4690-8b2e-4a4a88541e8c",
    "status": "SUCCESS",
    "response": "OK\n",
    "duration_seconds": 1.497642,
    "num_turns": 1,
}


def test_mint_reads_the_conversation_id_and_pins_the_argv(monkeypatch, tmp_path):
    seen: dict = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["cwd"] = kwargs.get("cwd")
        return _completed(stdout=json.dumps(ENVELOPE))

    monkeypatch.setattr(agy.subprocess, "run", fake_run)

    assert agy.create_conversation(tmp_path) == ENVELOPE["conversation_id"]
    # `-p` is agy's print form and the only one that returns an id; the JSON
    # output format is what carries it; the bypass keeps an unattended mint
    # from wedging on a tool approval.
    assert seen["argv"][:2] == ["agy", "-p"]
    assert "--output-format" in seen["argv"] and "json" in seen["argv"]
    assert "--dangerously-skip-permissions" in seen["argv"]
    assert seen["cwd"] == str(tmp_path)


@pytest.mark.parametrize(
    "completed, fragment",
    [
        (_completed(returncode=1, stderr="not signed in"), "exited 1"),
        (_completed(stdout="Welcome to the Antigravity CLI"), "was not JSON"),
        (_completed(stdout=json.dumps({"status": "SUCCESS"})), "no conversation_id"),
        (
            _completed(stdout=json.dumps({"conversation_id": "c5661b28"})),
            "not a full UUID",
        ),
    ],
)
def test_every_unreadable_mint_refuses(monkeypatch, tmp_path, completed, fragment):
    monkeypatch.setattr(agy.subprocess, "run", lambda *a, **k: completed)

    with pytest.raises(agy.AgySessionError) as caught:
        agy.create_conversation(tmp_path)
    assert fragment in str(caught.value)


def test_a_timed_out_mint_refuses_rather_than_inventing_an_id(monkeypatch, tmp_path):
    def fake_run(*a, **k):
        raise subprocess.TimeoutExpired(cmd="agy", timeout=1.0)

    monkeypatch.setattr(agy.subprocess, "run", fake_run)

    with pytest.raises(agy.AgySessionError) as caught:
        agy.create_conversation(tmp_path, timeout_s=1.0)
    assert "minted no conversation id" in str(caught.value)


def test_store_path_names_agys_own_conversation_db():
    path = agy.conversation_store_path(ENVELOPE["conversation_id"])
    assert path.name == f"{ENVELOPE['conversation_id']}.db"
    assert path.parent.parts[-3:] == (".gemini", "antigravity-cli", "conversations")
