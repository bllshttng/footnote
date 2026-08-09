"""`fno mail send --raw` (x-c24d Wave 3): fire a verb in a peer by injecting the
payload unwrapped at the prompt line. Covers the refusal rules (shape, multiline,
non-keystroke lane, unresolvable, self-send), the unwrapped inject, and the
never-durable invariant (AC18/AC30).
"""
from __future__ import annotations

import pytest
import typer
from typer.testing import CliRunner

from fno.agents.registry import register_existing_session
from fno.cli import app
from fno.harness_identity import (
    HARNESS_SESSION_MARKERS,
    LEGACY_HARNESS_SESSION_MARKERS,
)
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


@pytest.fixture
def runner():
    return CliRunner()


# The canonical marker set, not a hand-maintained copy: a stale list here
# silently turns env-leak tests green for the wrong reason. Covers modern and
# legacy, since current_session_id() reads both.
_HARNESS_MARKERS = tuple(
    m for m, _ in (*HARNESS_SESSION_MARKERS, *LEGACY_HARNESS_SESSION_MARKERS)
)


def _clear_harness_markers(monkeypatch):
    for marker in _HARNESS_MARKERS:
        monkeypatch.delenv(marker, raising=False)


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
    """The self-send refusal is a redirect, not a prohibition: a caller who
    addressed this own session positionally is told the --to-self retry line.
    A refusal that stops reading as an instruction is the dead-end specimen 1
    hit, so pin the retry string."""
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "/code-review medium --fix", self_ok=False)
    assert exc.value.exit_code != 0
    err = capsys.readouterr().err
    assert "addressed this session" in err
    assert "--to-self --raw" in err, "refusal must name the retry line"
    assert not injected, "a refused self-send must not reach the transport"


def test_raw_self_ok_lifts_the_self_refusal(mailbox, monkeypatch, capsys):
    """self_ok (wired by --to-self) is the opt-in: a self-inject proceeds and
    records the sender, since an unwrapped payload carries no `from` in the
    recipient transcript (AC27)."""
    from fno.mail.cli import _raw_send

    injected = _seed_claude(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("claudepeer", "/compact", self_ok=True)
    assert exc.value.exit_code == 0
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
    assert "addressed this session" in capsys.readouterr().err
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


# --- --to-self (replaces the deleted --self): recipient derived from ambient
# identity, positional parks the payload, mirrors --to-project's shift. ---


def test_to_self_raw_derives_recipient_with_no_positional(runner, mailbox, monkeypatch):
    """`fno mail send '<payload>' --to-self --raw`: one positional (the payload),
    recipient derived from ambient identity, no <id> lookup. The self-ok path
    stamps the sender handle as sole provenance (AC27)."""
    injected = _seed_claude(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    res = runner.invoke(app, ["mail", "send", "/compact", "--to-self", "--raw"])
    assert res.exit_code == 0, res.output + (res.stderr or "")
    assert "injected" in res.output
    assert injected == [(SID_CLAUDE, "/compact", SID_CLAUDE[-8:])]


def test_to_self_no_ambient_identity_exits_2(runner, mailbox, monkeypatch):
    """No ambient harness identity -> exit 2, never a silent floor. Mirrors the
    --from-self fail-closed branch."""
    _clear_harness_markers(monkeypatch)
    res = runner.invoke(app, ["mail", "send", "/compact", "--to-self", "--raw"])
    assert res.exit_code == 2
    assert "no ambient harness identity" in (res.output + (res.stderr or ""))


def test_to_self_and_to_project_mutually_exclusive(runner, mailbox, monkeypatch):
    """--to-self and --to-project both claim the recipient; refuse rather than
    silently preferring one."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    res = runner.invoke(
        app, ["mail", "send", "/x", "--to-self", "--to-project", "web", "--raw"]
    )
    assert res.exit_code == 2
    assert "mutually exclusive" in (res.output + (res.stderr or ""))


def test_to_self_refuses_contaminated_identity(runner, mailbox, monkeypatch):
    """An inherited foreign marker (e.g. CODEX_THREAD_ID from a codex parent in a
    claude worker) makes the precedence resolver pick the PARENT session; --to-self
    must refuse rather than inject into the parent."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    monkeypatch.setenv("CODEX_THREAD_ID", SID_CODEX)
    res = runner.invoke(app, ["mail", "send", "/compact", "--to-self", "--raw"])
    assert res.exit_code == 2
    assert "multiple harness markers" in (res.output + (res.stderr or ""))


def test_raw_refuses_from_self(runner, mailbox, monkeypatch):
    """Regression: the raw branch returns before the from_self block, so --from-self
    was silently swallowed on the --raw path. Refuse the combination at exit 2
    naming the envelope reason - silently dropping a provenance flag is worse
    than rejecting it."""
    _seed_claude(mailbox, monkeypatch)
    res = runner.invoke(
        app, ["mail", "send", "claudepeer", "/x", "--from-self", "--raw"]
    )
    assert res.exit_code == 2
    assert "envelope" in (res.output + (res.stderr or ""))
