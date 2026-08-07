"""`fno mail send --raw` (x-c24d Wave 3): fire a verb in a peer by injecting the
payload unwrapped at the prompt line. Covers the refusal rules (shape, multiline,
non-keystroke lane, unresolvable, self-send), the unwrapped inject, and the
never-durable invariant (AC18/AC30).
"""
from __future__ import annotations

import pytest
import typer

from fno.agents.registry import register_existing_session
from fno.paths_testing import use_tmpdir

SID_CLAUDE = "9a063cd3-69d4-415a-ada5-649b0164189c"
SID_CODEX = "019fc973-3401-4abc-9def-0123456789ab"


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


def _seed_claude(mailbox, monkeypatch):
    register_existing_session(
        provider="claude", session_id=SID_CLAUDE, cwd=str(mailbox), name="claudepeer"
    )
    injected = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda s, t, sender=None: injected.append((s, t, sender)) or True,
    )
    return injected


def test_raw_refuses_payload_without_leading_slash(mailbox, monkeypatch, capsys):
    from fno.mail.cli import _raw_send

    _seed_claude(mailbox, monkeypatch)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "code-review medium --fix", self_ok=False)
    assert exc.value.exit_code != 0
    assert "must start with /" in capsys.readouterr().err


def test_raw_refuses_multiline_payload(mailbox, monkeypatch, capsys):
    from fno.mail.cli import _raw_send

    _seed_claude(mailbox, monkeypatch)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "/compact\nfoo", self_ok=False)
    assert exc.value.exit_code != 0
    assert "single line" in capsys.readouterr().err


def test_raw_refuses_non_keystroke_codex_lane(mailbox, monkeypatch, capsys):
    from fno.mail.cli import _raw_send

    register_existing_session(
        provider="codex", session_id=SID_CODEX, cwd=str(mailbox), name="codexpeer"
    )
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda s, t: True)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review focus on concurrency", self_ok=False)
    assert exc.value.exit_code != 0
    err = capsys.readouterr().err
    assert "not a prompt-line keystroke path" in err
    assert "codex-daemon" in err


def test_raw_refuses_unresolvable_name(mailbox, monkeypatch, capsys):
    from fno.mail.cli import _raw_send

    with pytest.raises(typer.Exit) as exc:
        _raw_send("ghost", "/code-review", self_ok=False)
    assert exc.value.exit_code != 0
    assert "could not resolve" in capsys.readouterr().err


def test_raw_injects_unwrapped_on_claude_keystroke_lane(mailbox, monkeypatch, capsys):
    """AC13: the payload arrives UNWRAPPED (no <fno_mail> envelope) and the verb
    fires; AC18: write_new_thread is never reached on a clean delivery."""
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    durable = []
    monkeypatch.setattr(
        "fno.inbox.store.write_new_thread", lambda *a, **k: durable.append(a) or object()
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "/code-review medium --fix", self_ok=False)
    assert exc.value.exit_code == 0
    assert capsys.readouterr().out.strip() == "injected"
    assert injected == [(SID_CLAUDE, "/code-review medium --fix", None)]
    assert not durable, "AC18: --raw never writes durable on any transport result"


def test_raw_unconfirmed_never_durable(mailbox, monkeypatch, capsys):
    """AC29/AC30: a not-confirmed result is poll-budget exhaustion, not rejection
    (exit 0, never failure wording), and never queues durable."""
    from fno.mail.cli import _raw_send

    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude", lambda s, t, sender=None: False
    )
    register_existing_session(
        provider="claude", session_id=SID_CLAUDE, cwd=str(mailbox), name="claudepeer"
    )
    durable = []
    monkeypatch.setattr(
        "fno.inbox.store.write_new_thread", lambda *a, **k: durable.append(a) or object()
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "/code-review medium --fix", self_ok=False)
    assert exc.value.exit_code == 0
    out = capsys.readouterr().out
    assert "unconfirmed" in out
    assert "fail" not in out.lower()
    assert not durable, "AC30: unconfirmed never queues durable"


def test_raw_refuses_self_send_without_self_flag(mailbox, monkeypatch, capsys):
    """The self-send refusal is the PR's capability boundary: self-injection
    supplies the very user-shaped trigger a model-invocation refusal requires.
    Untested, an inverted `and not self_ok` ships green."""
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "/code-review medium --fix", self_ok=False)
    assert exc.value.exit_code != 0
    assert "self-injection" in capsys.readouterr().err
    assert not injected, "a refused self-send must not reach the transport"


def test_raw_self_flag_lifts_the_self_refusal(mailbox, monkeypatch, capsys):
    """--self is the documented opt-in for a verb carrying no model-invocation
    refusal (the /compact rescue)."""
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "/compact", self_ok=True)
    assert exc.value.exit_code == 0
    # AC27: a self-injection records sender == the recipient handle, so the
    # ledger identifies it permanently -- the only place it can be recorded, since
    # an unwrapped payload carries no `from` in the recipient transcript.
    assert injected == [(SID_CLAUDE, "/compact", SID_CLAUDE[-8:])]
    assert "/compact" in capsys.readouterr().out


def test_raw_self_refusal_fires_on_the_canonical_handle(mailbox, monkeypatch, capsys):
    """The king-mediated flow addresses by the 8-char canonical handle, not the
    full session id; the refusal must fire on that alias path too."""
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    with pytest.raises(typer.Exit) as exc:
        _raw_send(SID_CLAUDE[-8:], "/code-review", self_ok=False)
    assert exc.value.exit_code != 0
    assert "self-injection" in capsys.readouterr().err
    assert not injected


def test_raw_refuses_a_row_with_no_harness_session_id(mailbox, monkeypatch, capsys):
    """Fail closed: session_handle_tier returns None on an empty token, so a row
    with no harness_session_id would sail past the self-check even when it IS
    this session. No id, no soundness."""
    import fno.mail.cli as mail_cli
    from fno.agents.registry import AgentEntry
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    entry = AgentEntry(
        name="legacy", harness="claude", cwd=str(mailbox), log_path="", status="live"
    )
    monkeypatch.setattr(
        mail_cli, "_self_recipient", lambda *a, **k: None, raising=True
    )
    monkeypatch.setattr(
        "fno.agents.registry.resolve_agent",
        lambda n: type("R", (), {"entry": entry})(),
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("legacy", "/code-review", self_ok=False)
    assert exc.value.exit_code != 0
    assert "no harness_session_id" in capsys.readouterr().err
    assert not injected


def test_raw_injects_the_stripped_payload(mailbox, monkeypatch, capsys):
    """A leading space passes the slash check (which runs on the STRIPPED string)
    but defeats the REPL slash parser when injected raw -- and the receipt would
    still print `injected`. Inject what was validated."""
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "  /code-review medium --fix  ", self_ok=False)
    assert exc.value.exit_code == 0
    assert injected == [(SID_CLAUDE, "/code-review medium --fix", None)]
