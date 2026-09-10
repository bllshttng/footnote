"""A fresh claude thread spawn with no message is refused before any worker launches.

Claude starts such a session with no prompt: its job state reads
`needs: send a prompt to start`, and the row holds a worker slot for nothing.
"""
from __future__ import annotations

import pytest

from fno.agents import rust_runtime
from fno.agents.spawn_defaults import seedless_thread_refusal

SEEDED_FORM = "fno agents spawn '/fno:target x-1' --name w --node x-1 --substrate thread"
RESUME = "0a6e775f-1111-2222-3333-444444444444"


def test_the_refusal_names_the_seeded_form():
    text = seedless_thread_refusal("claude", "thread", "  ", name="w", node="x-1")
    assert text is not None
    assert SEEDED_FORM in text
    assert "No worker launched" in text


@pytest.mark.parametrize(
    "message,extra",
    [("/fno:target x-1", {}), ("", {"resume": RESUME}), ("", {"crown": True})],
)
def test_a_seed_a_resume_or_a_crown_is_not_refused(message, extra):
    assert seedless_thread_refusal("claude", "thread", message, **extra) is None


@pytest.mark.parametrize(
    "harness,substrate", [("codex", "thread"), ("claude", "pane"), ("claude", "headless")]
)
def test_only_a_claude_thread_is_judged(harness, substrate):
    assert seedless_thread_refusal(harness, substrate, "") is None


def test_the_seam_refuses_an_explicit_thread_substrate(capsys):
    with pytest.raises(SystemExit) as exc:
        rust_runtime._refuse_seedless_thread_spawn(
            ["spawn", "--name", "w", "--node", "x-1", "--harness", "claude", "--substrate", "thread"]
        )
    assert exc.value.code == 2
    assert SEEDED_FORM in capsys.readouterr().err


def test_the_seam_leaves_a_resume_alone():
    rust_runtime._refuse_seedless_thread_spawn(
        ["spawn", "--name", "w", "--harness", "claude", "--substrate", "bg", "--resume", RESUME]
    )


def test_cmd_spawn_refuses_a_defaulted_thread_before_dispatch(tmp_path, monkeypatch):
    # No --substrate: the seam leaves it to cmd_spawn, which resolves the
    # default (a thread for claude) and must refuse before dispatch runs.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    from fno.agents import dispatch as dispatch_mod
    from fno.agents import mux_spawn, spawn_gate

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: tmp_path)
    monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda n, s, p: {})
    monkeypatch.setattr("fno.agents.harness_map.thread_seatable", lambda h: True)

    class FakeGuard:
        def release(self):
            pass

    monkeypatch.setattr(spawn_gate, "run_gate", lambda name, substrate, **kw: FakeGuard())
    dispatched: list[dict] = []
    monkeypatch.setattr(dispatch_mod, "dispatch_spawn", lambda **kw: dispatched.append(kw))

    res = CliRunner().invoke(
        agents_cli.agents_app, ["spawn", "--name", "w", "--node", "x-1", "--harness", "claude"]
    )
    assert res.exit_code == 2, res.output
    assert "/fno:target x-1" in res.output, res.output
    assert dispatched == []
