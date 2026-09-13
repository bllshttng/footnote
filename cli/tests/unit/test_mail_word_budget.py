"""Control lane word ledger (x-3700 Wave 2, reshaped by x-34c1).

An ordinary send charges no rolling ledger: rule 7 is its only word gate, and
the acceptance test proves three same-pair sends inside one window all deliver
with no ledger file at all. The `control:` lane keeps its own 60-word rolling
ledger, and every control assertion here is a positive marker.
"""
from __future__ import annotations

import json
import time

import pytest
import typer

from fno import style
from fno.mail import budget


@pytest.fixture(autouse=True)
def isolated_bus(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_BUS_DIR", str(tmp_path / "bus"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    yield


def words(n: int) -> str:
    """A body of exactly ``n`` masked words, verified through Rule 7's counter."""
    text = " ".join(f"word{i}" for i in range(n))
    assert style.word_count(text) == n
    return text


def control_send(sender: str, recipient: str, n: int, msg_id: str, **keys):
    return budget.reserve_control(
        sender=sender,
        recipient=recipient,
        words=n,
        msg_id=msg_id,
        **keys,
    )


# --- one count, three callers -----------------------------------------------

def test_rule_seven_and_budget_share_one_count():
    body = "Ship the fix. See `cli/src/fno/mail/budget.py` and --flag now."
    # Rule 7 reports the same number in its own violation detail.
    long_body = " ".join([body] * 40)
    violations = style.check(long_body, surface="mail")
    seven = [v for v in violations if v.rule == 7]
    assert seven, "rule 7 must fire on a body over the cap"
    assert str(style.word_count(long_body)) in seven[0].detail


def test_masking_holds_for_the_budget():
    # A pasted log masks to near nothing; the cap covers prose, not a dump.
    assert style.word_count("```\n" + "\n".join(str(i) for i in range(200)) + "\n```") == 0


def test_identifier_masking_preserves_snake_case():
    assert style._mask_inline("foo_bar foo_bar_baz") == "x x"


# --- AC1-HP: an ordinary pair window does not exist -------------------------

def test_three_79_word_sends_deliver_without_a_ledger(
    monkeypatch,
    tmp_path,
    stubbed_pane,
    capsys,
):
    """The flipped burst acceptance, all three lanes: three 79-word ordinary
    sends from one sender to one recipient inside one window all deliver, and
    no ``<bus_dir>/word-budget/`` file is ever written."""
    from fno import paths
    from fno.agents.discover import DiscoveredSession
    from fno.agents.registry import AgentEntry, write_registry
    from fno.mail import cli

    recipient = DiscoveredSession(
        session_id="11111111-2222-3333-4444-555566667777",
        short_id="11111111",
        handle="11111111",
        pid=0,
        cwd="/tmp",
        project=None,
        status=None,
        agent="codex",
        truth_state="working",
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_codex",
        lambda _sid, body, **_kwargs: (body, False)[1],
    )
    body = words(79)
    for _ in range(3):
        cli._name_lane_send(body, from_name="sender", resolved=recipient)
        assert "queued (durable)" in capsys.readouterr().out

    registry = tmp_path / "agents.json"
    monkeypatch.setattr(paths, "agents_registry_path", lambda: registry)
    write_registry(
        [
            AgentEntry(
                name="worker",
                harness="claude",
                harness_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                short_id="aaaaaaaa",
                cwd=str(tmp_path),
                log_path="",
                status="idle",
            )
        ],
        registry,
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._registered_family1_state",
        lambda _entry: "sleeping",
    )
    from fno.agents.dispatch import dispatch_send

    for _ in range(3):
        result = dispatch_send(
            "worker", body, provider=None, cwd=tmp_path, from_name="sender"
        )
        assert result.delivery == "durable"

    for _ in range(3):
        prepared = _pane_prepare(_clean_body(45))
        assert prepared.exit_code == 0, prepared.output
        assert "</fno_mail>" in prepared.output

    assert not (paths.bus_dir() / "word-budget").exists(), (
        "an ordinary send charges no rolling ledger"
    )


def test_ordinary_81_word_body_still_breaks_rule_seven(capsys):
    """AC1-EDGE: the per-message cap is the only ordinary word gate, unchanged.
    The style gate fires in the send verb, before any lane is chosen."""
    from fno.mail import cli

    with pytest.raises(typer.Exit) as raised:
        cli._enforce_style(_clean_body(81))
    assert raised.value.exit_code == 1
    assert "81" in capsys.readouterr().err


# --- control lane: operational control rides its own window ----------------

def test_is_control_reads_the_first_non_blank_line_only():
    assert budget.is_control("control: stop all spawns now")
    assert budget.is_control("CONTROL: stop all spawns now"), "one capital letter bypasses an exact-case check"
    assert budget.is_control("  \n control: lift the hold")
    assert not budget.is_control("we need a control: review of this")
    assert not budget.is_control("status report\ncontrol: mentioned below")
    assert not budget.is_control("")


def test_control_body_over_the_cap_refuses_with_the_lane_marker(monkeypatch, capsys):
    """AC1-ERR: one 61-word control body refuses naming the lane; two 40-word
    control bodies refuse on the second."""
    from fno.agents.discover import DiscoveredSession
    from fno.mail import cli

    recipient = DiscoveredSession(
        session_id="11111111-2222-3333-4444-555566667777",
        short_id="11111111",
        handle="11111111",
        pid=0,
        cwd="/tmp",
        project=None,
        status=None,
        agent="codex",
        truth_state="working",
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_codex",
        lambda _sid, _body, **_kwargs: False,
    )
    with pytest.raises(typer.Exit) as raised:
        cli._name_lane_send("control: " + words(60), from_name="sender", resolved=recipient)
    assert raised.value.exit_code == 1
    assert "refused: control word budget" in capsys.readouterr().err

    cli._name_lane_send("control: " + words(39), from_name="sender", resolved=recipient)
    capsys.readouterr()
    with pytest.raises(typer.Exit) as raised2:
        cli._name_lane_send("control: " + words(39), from_name="sender", resolved=recipient)
    assert raised2.value.exit_code == 1
    refusal = capsys.readouterr().err
    assert "refused: control word budget" in refusal
    assert "running=40 current=40 projected=80 cap=60 window=10m" in refusal


def test_control_lane_caps_itself_and_the_ordinary_lane_writes_nothing():
    res = budget.reserve_control(sender="a", recipient="b", words=50, msg_id="msg-ctl-1")
    assert res.pair == "control:a -> b"

    with pytest.raises(budget.BudgetRefused) as exc:
        budget.reserve_control(sender="a", recipient="b", words=11, msg_id="msg-ctl-2")
    assert exc.value.cap == budget.CONTROL_CAP
    assert exc.value.pair == "control:a -> b"

    # The control ledger holds only the control traffic; an ordinary send
    # reserves nothing, so the ordinary pair file never comes into existence.
    assert not budget._ledger_path("a -> b").exists()


def test_control_window_resets_on_an_inbound_reply():
    budget.reserve_control(sender="a", recipient="b", words=55, msg_id="msg-ctl-1")
    reset_id = _inbound("a", "b", "msg-in-ctl")
    second = budget.reserve_control(sender="a", recipient="b", words=55, msg_id="msg-ctl-2")
    assert second.running_before == 0
    assert second.reset_by == reset_id


def test_control_lane_keys_colliding_codex_siblings_separately():
    budget.reserve_control(
        sender="king", recipient="01a0370b", words=55, msg_id="ctl-a",
        recipient_key="aaaa1111-2222-3333-4444-555566667777",
    )
    second = budget.reserve_control(
        sender="king", recipient="01a0370b", words=55, msg_id="ctl-b",
        recipient_key="bbbb1111-2222-3333-4444-555566667777",
    )
    assert second.running_before == 0, "distinct full-id keys charge separate control windows"


def test_control_body_skips_the_style_check(monkeypatch):
    from fno import style
    from fno.mail import cli

    def _violation(text, **_kw):
        return [style.Violation(rule=1, sentence_index=0, sentence=text, detail="flagged")]

    monkeypatch.setattr(style, "check", _violation)
    with pytest.raises(typer.Exit):
        cli._enforce_style(words(10))
    cli._enforce_style("control: HOLD all spawns now. Load 219 on 12 cores.")


# --- the control ledger ------------------------------------------------------

def _inbound(
    sender: str,
    recipient: str,
    msg_id: str,
    *,
    kind: str = "send",
) -> str:
    """Write one bus envelope FROM the recipient TO the sender."""
    from fno.bus.log import Envelope, append

    append(
        Envelope.new(
            id=msg_id,
            from_=recipient,
            to=sender,
            kind=kind,
            body="ok",
            word_count=1,
        )
    )
    return msg_id


def test_reservation_names_the_canonical_pair():
    res = control_send("canon-sender", "canon-recipient", 10, "msg-pair")
    assert res.pair == "control:canon-sender -> canon-recipient"
    path = budget._ledger_path(res.pair)
    stored = json.loads(path.read_text())
    assert stored["pair"] == "control:canon-sender -> canon-recipient"


def test_concurrent_sends_serialize_on_the_pair_ledger():
    import threading

    results: list = []

    def attempt(msg_id: str) -> None:
        try:
            results.append(("ok", control_send("a", "b", 50, msg_id)))
        except budget.BudgetRefused as exc:
            results.append(("refused", exc))

    threads = [threading.Thread(target=attempt, args=(f"msg-c{i}",)) for i in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    kinds = sorted(k for k, _ in results)
    assert kinds == ["ok", "refused"]
    refusal = next(v for k, v in results if k == "refused")
    assert refusal.marker() == (
        "running=50 current=50 projected=100 cap=60 window=10m"
    )
    stored = json.loads(budget._ledger_path("control:a -> b").read_text())
    assert len(stored["entries"]) == 1, "exactly one live reservation for the pair"


def test_job_lane_durable_failure_releases_the_control_reservation(monkeypatch):
    from fno.mail import job_lane
    from fno.mail.job_address import JobHolder

    job = JobHolder(
        node_id="work-1234",
        address="node:work-1234",
        state="live",
        session_id="bbbbbbbb-cccc-dddd-eeee-ffffffffffff",
        harness="claude",
    )
    monkeypatch.setattr(
        "fno.mail.job_address.resolve_job_address", lambda _token: job
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda _sid, _body, **_kwargs: False,
    )
    monkeypatch.setattr(
        "fno.inbox.store.write_new_thread",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(OSError("disk full")),
    )
    body = "control: " + words(50)

    with pytest.raises(typer.Exit) as failed:
        job_lane.job_lane_send(body, "node:work-1234", from_name="sender")
    assert failed.value.exit_code == 12

    retry = control_send("sender", "node:work-1234", 50, "msg-retry")
    assert retry.running_before == 0, "the proven non-delivery released its words"


# --- window expiry and release --------------------------------------------

def test_entries_older_than_the_window_are_pruned():
    res = control_send("a", "b", 55, "msg-old")
    path = budget._ledger_path(res.pair)
    stored = json.loads(path.read_text())
    stored["entries"][0]["ts"] = time.time() - budget.WINDOW_SECONDS - 1
    path.write_text(json.dumps(stored))
    fresh = control_send("a", "b", 55, "msg-new")
    assert fresh.running_before == 0


def test_release_gives_back_a_proven_non_delivery():
    res = control_send("a", "b", 55, "msg-fail")
    budget.release(res)
    again = control_send("a", "b", 55, "msg-retry")
    assert again.running_before == 0


def test_release_of_a_none_reservation_is_a_no_op():
    budget.release(None)


def test_an_empty_ledger_file_is_removed():
    res = control_send("a", "b", 10, "msg-solo")
    budget.release(res)
    assert not budget._ledger_path(res.pair).exists()


# --- fail closed ----------------------------------------------------------

def test_a_malformed_active_ledger_refuses_rather_than_resetting():
    res = control_send("a", "b", 55, "msg-one")
    path = budget._ledger_path(res.pair)
    path.write_text("{ not json")
    with pytest.raises(budget.BudgetUnavailable) as exc:
        control_send("a", "b", 1, "msg-two")
    assert "control:a -> b" in str(exc.value)
    assert str(path) in str(exc.value), "the refusal names the recovery path"


# --- the pane lane (x-4268): same style gate, control bodies only ----------

def _pane_prepare(body: str, *extra: str):
    from typer.testing import CliRunner

    from fno.mail.cli import mail_app

    return CliRunner().invoke(
        mail_app,
        ["pane-prepare", "--session-id", "s", "--pane", "3", *extra],
        input=body,
    )


@pytest.fixture
def stubbed_pane(monkeypatch):
    """Pin the pane transport's environment probes; see the bridge test file."""
    from fno.mail.pane_transport import PaneIdentity

    monkeypatch.setattr(
        "fno.mail.pane_transport.resolve_pane_harness", lambda s, p: "claude"
    )
    monkeypatch.setattr(
        "fno.mail.pane_transport.resolve_pane_recipient",
        lambda s, p: "recip-3333",
    )
    monkeypatch.setattr("fno.mail.pane_transport.prompt_refusal", lambda **_kw: None)
    monkeypatch.setattr("fno.agents.self_stamp.stamp_from", lambda _n: "sender-4444")
    monkeypatch.setattr(
        "fno.mail.pane_transport.resolve_pane_identity",
        lambda s, p: PaneIdentity(
            name="worker",
            fno_id="33333333-2222-1111-4444-555566667777",
            session_id="33333333-2222-1111-4444-555566667777",
            handle="recip-3333",
        ),
    )
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_session_id",
        lambda: "44444444-5555-6666-7777-888899990000",
    )
    return None


def _clean_body(n_words: int) -> str:
    """A style-clean body of exactly ``n_words`` words: 5-word sentences."""
    words = [f"w{i}" for i in range(n_words)]
    sentences = [
        " ".join(words[i : i + 5]) + "." for i in range(0, n_words, 5)
    ]
    text = " ".join(sentences)
    assert style.word_count(text) == n_words
    return text
