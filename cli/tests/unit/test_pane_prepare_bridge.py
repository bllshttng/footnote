"""The Rust-to-Python renderer bridge, asserted as a contract.

`fno mux pane send` (Rust) shells to `fno agents mail pane-prepare` (Python) for every
non-raw send, and FAILS CLOSED when that call cannot run. Fail-closed is right,
and it is also why this needs a test: rename the command or the flag and every
existing test still passes while non-raw pane sends refuse at runtime, quietly,
because a refusal is what the design asks for when the hop is unavailable.

So this reads the argv the Rust source actually builds and asserts the Python
CLI accepts it. It is a positive marker on the exact tokens that cross the
boundary, not a check that something somewhere still works.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.mail.cli import mail_app

REPO = Path(__file__).resolve().parents[3]
MUX_CLI = REPO / "crates" / "fno" / "src" / "mux_cli.rs"


def _bridge_argv() -> list[str]:
    """The literal argv `prepare_pane_bytes` passes to the child."""
    src = MUX_CLI.read_text(encoding="utf-8")
    block = re.search(
        r"fn prepare_pane_bytes.*?\.args\(\[(.*?)\]\)", src, re.S
    )
    assert block, "prepare_pane_bytes no longer builds a literal .args([...])"
    return re.findall(r'"([^"]+)"', block.group(1))


def _declared_options() -> set[str]:
    """Every option string `agents mail pane-prepare` declares."""
    from typer.main import get_command

    group = get_command(mail_app)
    prepare_cmd = group.commands["pane-prepare"]  # type: ignore[attr-defined]
    opts: set[str] = set()
    for param in prepare_cmd.params:
        opts.update(getattr(param, "opts", []) or [])
        opts.update(getattr(param, "secondary_opts", []) or [])
    assert opts, "pane-prepare declares no options; the introspection broke"
    return opts


def test_the_rust_bridge_names_a_command_this_cli_has():
    argv = _bridge_argv()
    # The x-6233 fold shape: `agents mail pane-prepare` plus flags.
    assert argv[:3] == ["agents", "mail", "pane-prepare"], argv

    result = CliRunner().invoke(mail_app, ["pane-prepare", "--help"])
    assert result.exit_code == 0, (
        f"the Rust bridge shells to `agents mail pane-prepare`, which this "
        f"CLI no longer accepts: {result.output}"
    )


@pytest.mark.parametrize("flag", ["--session-id", "--pane"])
def test_the_rust_bridge_flags_still_exist(flag):
    """Each flag the bridge passes must be one the command still takes.

    `--session-id` in particular was renamed once already, with `--session`
    kept as a hidden deprecated alias. A second rename that forgot the Rust
    caller would refuse every non-raw send and no test would say so.
    """
    assert flag in _bridge_argv(), f"{flag} is no longer in the bridge argv"
    # Introspect the parameters, never the rendered help. Typer draws help in a
    # Rich box that wraps to the terminal width, so a narrower CI terminal
    # splits a long flag across lines and a substring search reports it missing.
    # The parameter list is what the command actually accepts, and it does not
    # depend on how wide the screen is.
    assert flag in _declared_options(), (
        f"`agents mail pane-prepare` no longer accepts {flag}; the Rust bridge passes it"
    )


def test_the_rust_bridge_passes_style_exception_to_the_child():
    """The conditional bridge flag: present in `prepare_pane_bytes`, accepted
    by the child. It rides `.arg(...)` beside the literal `.args([...])`, so it
    needs its own source assertion rather than `_bridge_argv`."""
    src = MUX_CLI.read_text(encoding="utf-8")
    body = re.search(r"fn prepare_pane_bytes.*?\n\}", src, re.S)
    assert body, "prepare_pane_bytes no longer exists"
    assert 'arg("--style-exception")' in body.group(0), (
        "the Rust bridge stopped passing --style-exception to the renderer"
    )
    assert "--style-exception" in _declared_options(), (
        "`mail pane-prepare` no longer accepts --style-exception"
    )


# -- the gates on the enveloped body (x-4268) + the empty-payload refusal ----

@pytest.fixture(autouse=True)
def _isolated_bus(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_BUS_DIR", str(tmp_path / "bus"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))


@pytest.fixture
def stubbed_transport(monkeypatch):
    """Pin the transport's environment probes so the CLI bridge runs clean.

    Patched at their module attributes, not at the CLI's imports: the command
    imports them at call time, so it picks up these stubs.
    """
    from fno.mail.pane_transport import PaneIdentity

    monkeypatch.setattr(
        "fno.mail.pane_transport.resolve_pane_harness", lambda s, p: "claude"
    )
    monkeypatch.setattr(
        "fno.mail.pane_transport.resolve_pane_recipient",
        lambda s, p: "recip-1111",
    )
    monkeypatch.setattr("fno.mail.pane_transport.prompt_refusal", lambda **_kw: None)
    monkeypatch.setattr("fno.agents.self_stamp.stamp_from", lambda _n: "sender-2222")
    monkeypatch.setattr(
        "fno.mail.pane_transport.resolve_pane_identity",
        lambda s, p: PaneIdentity(
            name="worker",
            fno_id="11111111-2222-3333-4444-555566667777",
            session_id="11111111-2222-3333-4444-555566667777",
            handle="recip-1111",
        ),
    )
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_session_id",
        lambda: "99999999-8888-7777-6666-555544443333",
    )
    return None


def _prepare(body: str, *extra: str):
    return CliRunner().invoke(
        mail_app,
        ["pane-prepare", "--session-id", "s", "--pane", "3", *extra],
        input=body,
    )


def test_default_pane_body_violating_style_refuses_before_envelope(stubbed_transport):
    """The measured bypass: identical prose refused by mail used to pass here."""
    # One 26-word sentence: rule 1 caps a sentence at 25 words.
    body = (
        "the refusal says use --raw for a bare submit keystroke and the next "
        "command rejects that form because the arity guard still demands a "
        "text flag"
    )
    result = _prepare(body)
    assert result.exit_code == 1, result.output
    assert "style" in result.output.lower(), result.output
    assert "rule 1" in result.output, result.output
    assert "</fno_mail>" not in result.output, "a refused body must not render"


def test_style_exception_flag_lets_the_body_through_and_charges_it(
    stubbed_transport, monkeypatch
):
    """LD3: the exception permits the overage but still records the count, and
    the ledger identity matches the envelope (LD2). The ledger key carries the
    FULL session ids, both ends: an eight-hex handle collides for codex
    siblings spawned inside one ~65s clock bucket."""
    captured: dict = {}
    import fno.mail.budget as budget_mod

    real_reserve = budget_mod.reserve

    def _capture(**kwargs):
        captured.update(kwargs)
        return real_reserve(**kwargs)

    monkeypatch.setattr("fno.mail.budget.reserve", _capture)
    body = (
        "the refusal says use --raw for a bare submit keystroke and the next "
        "command rejects that form because the arity guard still demands a "
        "text flag"
    )
    result = _prepare(body, "--style-exception", "quoted operator text")
    assert result.exit_code == 0, result.output
    assert "</fno_mail>" in result.output
    # The envelope's identity is the ledger's identity: one id, one pair.
    envelope_id = re.search(r'id="([^"]+)"', result.output)
    assert envelope_id, result.output
    assert captured["msg_id"] == envelope_id.group(1)
    assert captured["sender"] == "sender-2222"
    assert captured["recipient"] == "recip-1111"
    # The KEY is the full ids; the display pair and the envelope stay handles.
    assert captured["sender_key"] == "99999999-8888-7777-6666-555544443333"
    assert captured["recipient_key"] == "11111111-2222-3333-4444-555566667777"
    assert captured["enforce"] is False
    assert captured["words"] > 0


def test_identity_capture_pins_the_gate_not_a_re_resolve(stubbed_transport, monkeypatch):
    """A pane reassigned between resolve and gate must refuse, not render an
    envelope addressed to the old occupant: the CLI threads its captured
    name/fno_id into prepare as the gate's expected identity."""
    seen: dict = {}
    from fno.mail import pane_transport

    real_prepare = pane_transport.prepare

    def _capture(text, **kwargs):
        seen.update(kwargs)
        return real_prepare(text, **kwargs)

    monkeypatch.setattr(pane_transport, "prepare", _capture)
    result = _prepare("the build is green and the tests pass.")
    assert result.exit_code == 0, result.output
    assert seen["expected_name"] == "worker"
    assert seen["expected_fno_id"] == "11111111-2222-3333-4444-555566667777"
    assert seen["to"] == "recip-1111"


def test_empty_enveloped_body_keeps_the_attribution_refusal(stubbed_transport):
    """x-3081's boundary: the new Rust `--raw --submit` exception must not
    relax the Python attribution refusal, which stays exit 3 with its marker
    and now names the full working form."""
    result = _prepare("  \n")
    assert result.exit_code == 3, result.output
    assert "empty payload: there is nothing to attribute" in result.output
    assert "--raw --submit" in result.output
    assert "</fno_mail>" not in result.output
