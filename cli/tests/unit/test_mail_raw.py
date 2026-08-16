"""`fno mail send --raw` (x-c24d Wave 3): fire a verb in a peer by injecting the
payload unwrapped at the prompt line. Covers the refusal rules (shape, multiline,
non-keystroke lane, unresolvable, self-send), the unwrapped inject, and the
never-durable invariant (AC18/AC30).
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

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


def _seed_codex_app_server(mailbox, monkeypatch):
    import fno.mail.cli as mail_cli
    from fno.agents.registry import AgentEntry

    entry = AgentEntry(
        name="codexpeer",
        harness="codex",
        harness_session_id=SID_CODEX,
        cwd=str(mailbox),
        log_path="",
        status="live",
    )
    monkeypatch.setattr(
        "fno.agents.registry.resolve_agent",
        lambda _name: type("R", (), {"entry": entry})(),
    )
    monkeypatch.setattr(
        mail_cli, "_codex_default_review_base", lambda _cwd: "origin/main"
    )
    return entry


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


@pytest.mark.parametrize("verb", ["/review", "/code-review"])
def test_raw_routes_exact_review_verbs_to_codex_review_start(
    mailbox, monkeypatch, capsys, verb
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda session, target, **_audit: calls.append((session, target))
        or {
            "delivered": True,
            "turn_id": "turn-1",
            "review_thread_id": "review-1",
        },
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", verb, self_ok=False)
    assert exc.value.exit_code == 0
    assert calls == [(SID_CODEX, "baseBranch:origin/main")]
    assert capsys.readouterr().out.strip() == (
        "review/start target=baseBranch:origin/main delivery=inline "
        "turn=turn-1 review_thread=review-1"
    )


@pytest.mark.parametrize(
    ("payload", "target"),
    [
        ("/review --base origin/main", "baseBranch:origin/main"),
        ("/review --uncommitted", "uncommittedChanges"),
        ("/review deadbee", "commit:deadbee"),
        ("/review custom:focus on concurrency", "custom:focus on concurrency"),
    ],
)
def test_raw_maps_explicit_codex_review_targets(
    mailbox, monkeypatch, capsys, payload, target
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda session, resolved_target, **_audit: calls.append(
            (session, resolved_target)
        )
        or {
            "delivered": True,
            "turn_id": "turn-2",
            "review_thread_id": "review-2",
        },
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", payload, self_ok=False)
    assert exc.value.exit_code == 0
    assert calls == [(SID_CODEX, target)]
    assert f"target={target}" in capsys.readouterr().out


def test_raw_bare_codex_review_requires_a_resolvable_default_base(
    mailbox, monkeypatch, capsys
):
    import fno.mail.cli as mail_cli
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(
        mail_cli, "_codex_default_review_base", lambda _cwd: None, raising=False
    )
    calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: calls.append(True)
        or {
            "delivered": True,
            "turn_id": "turn-unbased",
            "review_thread_id": "review-unbased",
        },
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=False)
    assert exc.value.exit_code == 2
    assert "--base" in capsys.readouterr().err
    assert not calls


def test_codex_default_review_base_reads_origin_head(tmp_path):
    import fno.mail.cli as mail_cli

    subprocess.run(["git", "init", "-q", str(tmp_path)], check=True)
    subprocess.run(
        [
            "git",
            "-C",
            str(tmp_path),
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
        check=True,
    )
    helper = getattr(mail_cli, "_codex_default_review_base", None)
    assert callable(helper), "bare Codex reviews need a default-base resolver"
    assert helper(str(tmp_path)) == "origin/trunk"


def test_raw_body_cap_runs_before_codex_review_start(
    mailbox, monkeypatch, capsys
):
    import fno.mail.cli as mail_cli
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(mail_cli, "_BODY_WARN_BYTES", 0)
    monkeypatch.setattr(mail_cli, "_BODY_REFUSE_BYTES", 20)
    calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: calls.append(True),
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review custom:" + "x" * 32, self_ok=False)
    assert exc.value.exit_code == 1
    assert "mail body is" in capsys.readouterr().err
    assert not calls


def test_raw_codex_review_passes_sender_and_original_payload_to_transport_audit(
    mailbox, monkeypatch, capsys
):
    from fno.mail.cli import _raw_send

    entry = _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    calls = []

    def review_start(session, target, **audit):
        calls.append((session, target, audit))
        return {
            "delivered": True,
            "turn_id": "turn-audit",
            "review_thread_id": "review-audit",
        }

    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        review_start,
    )
    python_events = []
    monkeypatch.setattr(
        "fno.events.append_event",
        lambda event, *_a, **_k: python_events.append(event),
    )
    payload = "/code-review focus on concurrency"
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", payload, self_ok=False)
    assert exc.value.exit_code == 0
    assert calls == [
        (
            SID_CODEX,
            "uncommittedChanges",
            {
                "audit_payload": payload,
                "audit_sender": SID_CLAUDE[:8],
                "audit_target_cwd": entry.cwd,
            },
        )
    ]
    assert not python_events, "the Rust transport owns the sole audit event"
    assert "unrecognized remainder ignored" in capsys.readouterr().out


def test_raw_codex_review_uses_process_owned_sender_with_inherited_marker(
    mailbox, monkeypatch
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setenv("CODEX_THREAD_ID", SID_CODEX)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", SID_CLAUDE)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda: "claude"
    )
    calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **audit: calls.append(audit)
        or {
            "delivered": True,
            "turn_id": "turn-owned",
            "review_thread_id": "review-owned",
        },
    )

    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=True)

    assert exc.value.exit_code == 0
    assert calls[0]["audit_sender"] == SID_CLAUDE[:8]


def test_raw_unparsed_codex_review_remainder_defaults_without_custom_rewrite(
    mailbox, monkeypatch, capsys
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda session, target, **_audit: calls.append((session, target))
        or {
            "delivered": True,
            "turn_id": "turn-3",
            "review_thread_id": "review-3",
        },
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review focus on concurrency", self_ok=False)
    assert exc.value.exit_code == 0
    assert calls == [(SID_CODEX, "uncommittedChanges")]
    out = capsys.readouterr().out
    assert "target=uncommittedChanges" in out
    assert "unrecognized remainder ignored" in out
    assert "custom:" not in out


@pytest.mark.parametrize("payload", ["/compact", "/reviewboard"])
def test_raw_refuses_non_review_payload_on_codex_daemon(
    mailbox, monkeypatch, capsys, payload
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    called = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: called.append(True),
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", payload, self_ok=False)
    assert exc.value.exit_code != 0
    err = capsys.readouterr().err
    assert "codex app-server thread" in err
    assert "has no prompt line" in err
    assert "turn/start" in err
    assert "review/start" in err
    assert "drop --raw" in err
    assert "mux pane" in err
    assert not called


def test_raw_codex_review_no_daemon_names_start_command(mailbox, monkeypatch, capsys):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: {"delivered": False, "reason": "no-daemon"},
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=False)
    assert exc.value.exit_code != 0
    err = capsys.readouterr().err
    assert "no-daemon" in err
    assert "codex app-server daemon start" in err
    assert "not a prompt-line keystroke path" not in err


@pytest.mark.parametrize("reason", ["io-error", "handshake-failed"])
def test_raw_codex_review_surfaces_rpc_failure_token(
    mailbox, monkeypatch, capsys, reason
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: {"delivered": False, "reason": reason},
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=False)
    assert exc.value.exit_code != 0
    assert reason in capsys.readouterr().err


def test_raw_codex_review_not_confirmed_warns_against_retry(
    mailbox, monkeypatch, capsys
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: {"delivered": False, "reason": "not-confirmed"},
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=False)
    assert exc.value.exit_code == 0
    captured = capsys.readouterr()
    assert "not-confirmed" in captured.err
    assert "may already be running" in captured.err
    assert "do not retry blindly" in captured.err
    assert "refused:" not in captured.err


@pytest.mark.parametrize(
    "payload",
    [
        "/review --base",
        "/review --base --uncommitted",
        "/review --base origin/main extra",
    ],
)
def test_raw_malformed_codex_base_refuses_instead_of_rescoping(
    mailbox, monkeypatch, capsys, payload
):
    """A named base is an explicit scope request: a malformed form must refuse,
    never fall through to uncommittedChanges (which silently reviews a
    different diff than the one asked for)."""
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    called = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: called.append(True),
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", payload, self_ok=False)
    assert exc.value.exit_code == 2
    err = capsys.readouterr().err
    assert "--base" in err
    assert not called


def test_raw_check_codex_review_answers_the_send_refusals(
    mailbox, monkeypatch, capsys
):
    """"--check cannot say yes where the send says no: an unresolvable target
    or a missing binary must answer not-injectable, not injectable."""
    import fno.mail.cli as mail_cli
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: Path("/bin/fno-agents")
    )
    monkeypatch.setattr(mail_cli, "_codex_default_review_base", lambda _cwd: None)

    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=False, check=True)
    assert exc.value.exit_code == 1
    out = capsys.readouterr().out
    assert "not-injectable" in out
    assert "--base" in out

    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: None)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review --uncommitted", self_ok=False, check=True)
    assert exc.value.exit_code == 1
    assert "fno doctor" in capsys.readouterr().out

    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: Path("/bin/fno-agents")
    )
    monkeypatch.setattr(
        mail_cli, "_codex_default_review_base", lambda _cwd: "origin/main"
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=False, check=True)
    assert exc.value.exit_code == 0
    assert "injectable" in capsys.readouterr().out


def test_raw_check_non_review_verb_on_codex_daemon_still_not_injectable(
    mailbox, monkeypatch, capsys
):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/compact", self_ok=False, check=True)
    assert exc.value.exit_code == 1
    assert "no prompt line" in capsys.readouterr().out


def test_review_start_codex_flags_stale_deployed_binary(monkeypatch):
    from fno.agents import dispatch

    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: Path("/bin/fno-agents")
    )
    monkeypatch.setattr(
        dispatch.subprocess,
        "run",
        lambda argv, **kwargs: subprocess.CompletedProcess(
            argv, 2, stdout="", stderr="review-start: unknown flag: --audit-payload"
        ),
    )
    assert dispatch._review_start_codex(SID_CODEX, "uncommittedChanges") == {
        "delivered": False,
        "reason": "stale-binary",
    }


def test_raw_codex_review_stale_binary_names_doctor(mailbox, monkeypatch, capsys):
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: {"delivered": False, "reason": "stale-binary"},
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review", self_ok=False)
    assert exc.value.exit_code == 2
    err = capsys.readouterr().err
    assert "stale-binary" in err
    assert "fno doctor" in err


def test_raw_body_cap_under_check_is_a_usage_exit(mailbox, monkeypatch, capsys):
    import fno.mail.cli as mail_cli
    from fno.mail.cli import _raw_send

    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setattr(mail_cli, "_BODY_WARN_BYTES", 0)
    monkeypatch.setattr(mail_cli, "_BODY_REFUSE_BYTES", 20)
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexpeer", "/review custom:" + "x" * 32, self_ok=False, check=True)
    # Exit 1 is the not-injectable verdict's code; an over-cap payload is a
    # malformed call, so under --check it must stay a usage error at exit 2.
    assert exc.value.exit_code == 2
    assert "mail body is" in capsys.readouterr().err


@pytest.mark.parametrize("check", [False, True])
def test_raw_generic_daemon_lane_keeps_its_refusal(mailbox, monkeypatch, capsys, check):
    """The non-codex daemon lanes (gemini, opencode, unknown) never gained the
    review/start exception; their generic refusal keeps coverage on both the
    send and the --check form."""
    from fno.agents.registry import AgentEntry
    from fno.mail.cli import _raw_send

    entry = AgentEntry(
        name="gempeer",
        harness="gemini",
        harness_session_id="g-1234",
        cwd=str(mailbox),
        log_path="",
        status="live",
    )
    monkeypatch.setattr(
        "fno.agents.registry.resolve_agent",
        lambda _name: type("R", (), {"entry": entry})(),
    )
    review_calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: review_calls.append(True),
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("gempeer", "/review", self_ok=False, check=check)
    assert exc.value.exit_code != 0
    captured = capsys.readouterr()
    assert not review_calls
    if check:
        assert "not-injectable" in captured.out
        assert "gemini-daemon" in captured.out
    else:
        assert "not a prompt-line keystroke path" in captured.err
        assert "gemini-daemon" in captured.err
    from fno.agents.registry import AgentEntry
    from fno.mail.cli import _raw_send

    entry = AgentEntry(
        name="codexmux",
        harness="codex",
        harness_session_id=SID_CODEX,
        cwd=str(mailbox),
        log_path="",
        status="live",
        mux={"session": "fno", "pane_id": 7},
    )
    monkeypatch.setattr(
        "fno.agents.registry.resolve_agent",
        lambda _name: type("R", (), {"entry": entry})(),
    )
    injected = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mux_pane_send",
        lambda resolved, payload, guarded=True, sender=None, confirm=False: (
            injected.append((resolved, payload, sender))
        )
        or True,
    )
    review_calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda *_a, **_k: review_calls.append(True),
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send("codexmux", "/compact", self_ok=False)
    assert exc.value.exit_code == 0
    assert injected == [(entry, "/compact", None)]
    assert not review_calls
    assert "injected" in capsys.readouterr().out


def test_review_start_codex_uses_structured_binary_argv(monkeypatch):
    from fno.agents import dispatch

    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: Path("/bin/fno-agents")
    )
    seen = {}

    def fake_run(argv, **kwargs):
        seen.update(argv=argv, kwargs=kwargs)
        return subprocess.CompletedProcess(
            argv,
            0,
            stdout=json.dumps(
                {
                    "delivered": True,
                    "turn_id": "turn-4",
                    "review_thread_id": "review-4",
                }
            ),
            stderr="",
        )

    monkeypatch.setattr(dispatch.subprocess, "run", fake_run)
    receipt = dispatch._review_start_codex(
        SID_CODEX,
        "baseBranch:origin/main",
        audit_payload="/review --base origin/main",
        audit_sender="sender-1",
        audit_target_cwd="/repo",
    )
    assert receipt["delivered"] is True
    assert seen["argv"] == [
        "/bin/fno-agents",
        "review-start",
        "--session",
        SID_CODEX,
        "--target",
        "baseBranch:origin/main",
        "--delivery",
        "inline",
        "--audit-payload",
        "/review --base origin/main",
        "--audit-sender",
        "sender-1",
        "--audit-target-cwd",
        "/repo",
    ]
    assert seen["kwargs"]["timeout"] == dispatch._MAIL_INJECT_TIMEOUT_S


def test_review_start_codex_preserves_no_daemon_reason(monkeypatch):
    from fno.agents import dispatch

    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: Path("/bin/fno-agents")
    )
    monkeypatch.setattr(
        dispatch.subprocess,
        "run",
        lambda argv, **kwargs: subprocess.CompletedProcess(
            argv,
            1,
            stdout='{"delivered":false,"reason":"no-daemon"}',
            stderr="",
        ),
    )
    assert dispatch._review_start_codex(SID_CODEX, "uncommittedChanges") == {
        "delivered": False,
        "reason": "no-daemon",
    }


@pytest.mark.parametrize(
    ("returncode", "receipt", "reason"),
    [
        (0, {"delivered": "false"}, "rpc-error"),
        (0, {"delivered": True, "turn_id": "turn-only"}, "not-confirmed"),
        (
            1,
            {
                "delivered": True,
                "turn_id": "turn-1",
                "review_thread_id": "review-1",
            },
            "not-confirmed",
        ),
    ],
)
def test_review_start_codex_rejects_malformed_success_receipts(
    monkeypatch, returncode, receipt, reason
):
    from fno.agents import dispatch

    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: Path("/bin/fno-agents")
    )
    monkeypatch.setattr(
        dispatch.subprocess,
        "run",
        lambda argv, **kwargs: subprocess.CompletedProcess(
            argv,
            returncode,
            stdout=json.dumps(receipt),
            stderr="",
        ),
    )
    assert dispatch._review_start_codex(SID_CODEX, "uncommittedChanges") == {
        "delivered": False,
        "reason": reason,
    }


def test_review_start_codex_child_timeout_is_not_confirmed(monkeypatch):
    from fno.agents import dispatch

    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: Path("/bin/fno-agents")
    )
    monkeypatch.setattr(
        dispatch.subprocess,
        "run",
        lambda *_a, **_k: (_ for _ in ()).throw(
            subprocess.TimeoutExpired("fno-agents", 20)
        ),
    )
    assert dispatch._review_start_codex(SID_CODEX, "uncommittedChanges") == {
        "delivered": False,
        "reason": "not-confirmed",
    }


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
        _raw_send("claudepeer", "/code-review <level> --comment --fix", self_ok=False)
    assert exc.value.exit_code == 0
    assert capsys.readouterr().out.strip() == "injected"
    assert injected == [(SID_CLAUDE, "/code-review <level> --comment --fix", None)]
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
        _raw_send("claudepeer", "/code-review <level> --comment --fix", self_ok=False)
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
        _raw_send("claudepeer", "/code-review <level> --comment --fix", self_ok=False)
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
    assert injected == [(SID_CLAUDE, "/compact", SID_CLAUDE[:8])]
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
        _raw_send("claudepeer", "  /code-review <level> --comment --fix  ", self_ok=False)
    assert exc.value.exit_code == 0
    assert injected == [(SID_CLAUDE, "/code-review <level> --comment --fix", None)]


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
    assert injected == [(SID_CLAUDE, "/compact", SID_CLAUDE[:8])]


def test_to_self_raw_routes_codex_review_to_ambient_thread(
    runner, mailbox, monkeypatch
):
    _seed_codex_app_server(mailbox, monkeypatch)
    monkeypatch.setenv("CODEX_THREAD_ID", SID_CODEX)
    calls = []
    monkeypatch.setattr(
        "fno.agents.dispatch._review_start_codex",
        lambda session, target, **_audit: calls.append((session, target))
        or {
            "delivered": True,
            "turn_id": "turn-self",
            "review_thread_id": "review-self",
        },
    )
    res = runner.invoke(app, ["mail", "send", "/review", "--to-self", "--raw"])
    assert res.exit_code == 0, res.output + (res.stderr or "")
    assert calls == [(SID_CODEX, "baseBranch:origin/main")]
    assert "review/start target=baseBranch:origin/main" in res.output


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
