"""Plan verification 7, second half: one stalled board is ONE fleet task.

The first half - that both king terminals reach the verb, over the same stalled
set - is `both_king_terminals_escalate_over_the_same_stalled_set` in
`crates/fno-agents/tests/loop_check.rs`. It drives the two real paths against a
mock `fno` and asserts the `--stalled` argument they produce. This file takes
that argument and asserts what the verb does with it.

After the fleet-task port, the fold behind `escalate` lives in
`crates/fno-agents/src/fleet_task.rs`; the Python channel is ONE transport
call. The fold's behavior (dedupe, supersede, close, lane scoping) is
characterized in Rust: `fleet_task_reconcile_parity.rs` and the `fleet_task`
unit tests. What stays testable here is the seam: the marker+key the renderer
receives, the lane+key payload the transport receives, and the refusals that
raise before either.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents.stale_escalate import dedupe_key
from fno.king.escalate import escalate
from fno.outstanding.core import read_open_questions

STALLED = ["undispatched:x-1234", "undispatched:x-5678"]


#: The per-session crown scopes the fake renderer resolves, mirroring the
#: crate's registry read. Channel tests install their map here.
SCOPES: "dict[str, str]" = {}


def _fake_render(
    ids, key, reason, *, live=None, unknown_reason=None, verdict=None, scope=None,
    session_id=None,
) -> dict:
    """The renderer runs in the fno-agents crate; the fake keeps its
    contract where the fold tests depend on it: the marker+key leads, and a
    session in SCOPES scopes both the marker and the needle key."""
    crown_scope = SCOPES.get(session_id or "")
    marker = f"king-escalation:{crown_scope}" if crown_scope else "king-escalation"
    return {
        "ok": True,
        "question": (
            f"[{marker}:{key}] The king stopped on {len(ids)} board "
            f"row(s) nothing is clearing: {', '.join(ids)}. "
            f"Reason given: {reason}. body"
        ),
        "mail": f"A crown under yours stopped on {len(ids)} rows. Reason given: {reason}.",
        "marker": marker,
    }


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )
    SCOPES.clear()


@pytest.fixture(autouse=True)
def crate_render_stub(monkeypatch: pytest.MonkeyPatch) -> None:
    """Stub the crate round-trip for every fold test, as
    test_outstanding.py stubs _law_match (x-ff27)."""
    monkeypatch.setattr("fno.king.escalate._render", _fake_render)


def _capture_transport(monkeypatch: pytest.MonkeyPatch, *, answer: dict) -> list:
    """Stub the fleet-task transport; returns the captured payloads."""
    import fno.rust_binary

    captured: list = []

    def fake_verb_call(verb, payload, *args, **kwargs):
        assert verb == "fleet-task", verb
        captured.append(payload)
        return dict(answer)

    monkeypatch.setattr(fno.rust_binary, "verb_call", fake_verb_call)
    return captured


def _run(root: Path, ids: list[str], reason: str = "NoProgress") -> tuple[str, str]:
    return escalate(ids, reason=reason, root=root, session_id="k-test", cwd=root)


def test_one_stalled_board_sends_one_task_payload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Both terminals escalating the same set key on the same dedupe key: the
    payload is identical whichever order the ids arrive in."""
    captured = _capture_transport(
        monkeypatch, answer={"outcome": "asked", "id": "ft-kingfeed"}
    )
    first_outcome, first_id = _run(tmp_path, STALLED)
    second_outcome, second_id = _run(tmp_path, list(reversed(STALLED)))

    assert first_outcome == "recorded"
    assert first_id == "ft-kingfeed"
    assert second_outcome == "duplicate"
    assert second_id == first_id
    assert len(captured) == 2
    assert captured[0]["key"] == captured[1]["key"] == dedupe_key(sorted(STALLED))
    assert captured[0]["lane"] == "king-escalation"
    assert captured[0]["empty"] is False
    # The rendered question text flows through as the task text.
    assert captured[0]["text"] == _fake_render(
        sorted(STALLED), dedupe_key(sorted(STALLED)), "NoProgress"
    )["question"]


def test_an_empty_refused_set_never_reaches_the_transport(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A refusal is the gate speaking (x-ff27): the renderer refuses an
    empty set, and the transport must never see one - reaching the fold with
    a refused set would read a refused board as a clean one."""
    captured = _capture_transport(
        monkeypatch, answer={"outcome": "asked", "id": "ft-shouldnot"}
    )
    monkeypatch.setattr(
        "fno.king.escalate._render",
        lambda *a, **k: {
            "ok": False,
            "message": "king escalation refused: the stalled set is empty",
        },
    )
    with pytest.raises(ValueError, match="king escalation refused"):
        _run(tmp_path, [])

    assert captured == []
    assert read_open_questions(tmp_path) == []


def test_the_key_ignores_order_and_repeats(tmp_path: Path) -> None:
    assert dedupe_key(["b", "a"]) == dedupe_key(["a", "b", "a"])
    assert dedupe_key(["a"]) != dedupe_key(["a", "b"])


def test_the_fold_renders_with_the_dedupe_key_of_its_ids(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The crate renders; this file pins the seam: escalate hands the
    renderer the deduped ids and the key the marker carries."""
    seen: dict = {}

    def rec(ids, key, reason, **k):
        seen.update(ids=ids, key=key, reason=reason)
        return _fake_render(ids, key, reason, **k)

    monkeypatch.setattr("fno.king.escalate._render", rec)
    _run(tmp_path, list(reversed(STALLED)))

    assert seen["ids"] == sorted(STALLED)
    assert seen["key"] == dedupe_key(STALLED)
    assert seen["reason"] == "NoProgress"


# ---------------------------------------------------------------------------
# AC4-HP (x-3ecf): king escalate resolves the presiding crown first
# ---------------------------------------------------------------------------


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry

    harness = kw.pop("harness", "claude")
    kw.setdefault("cwd", "/w")
    kw.setdefault("harness_session_id", f"{name}-session")
    return AgentEntry(name=name, log_path="", harness=harness, **kw)


def _prepare_court(monkeypatch, tmp_path: Path, rows) -> None:
    import json

    from fno import paths
    from fno.agents.registry import write_registry
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    write_registry(rows)
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(
        json.dumps({"entries": [{"id": "x-epic", "title": "x-epic", "slug": "x-epic",
                                 "type": "epic", "priority": "p2", "project": "fno",
                                 "status": "ready"}]}),
        encoding="utf-8",
    )


def test_ac4_hp_a_live_l1_crown_presides_over_its_epic_set(tmp_path: Path, monkeypatch) -> None:
    """An L2 crown over a scope an L1 crown's project contains: the L1
    king's own entry comes back, named."""
    from fno.king.escalate import resolve_presiding_king

    _prepare_court(
        monkeypatch,
        tmp_path,
        [
            _entry("l2-king", status="busy", crown_level=2, crown_scope="x-epic"),
            _entry("l1-king", status="busy", crown_level=1, crown_scope="fno"),
        ],
    )
    presiding = resolve_presiding_king("l2-king-session")
    assert presiding is not None
    assert presiding["holder"] == "l1-king"


def test_ac4_hp_no_higher_crown_reads_as_none(tmp_path: Path, monkeypatch) -> None:
    """The converse: an L1 crown (already the top rung reachable here) has
    nothing to escalate to, so the caller falls through to the operator."""
    from fno.king.escalate import resolve_presiding_king

    _prepare_court(
        monkeypatch, tmp_path, [_entry("l1-king", status="busy", crown_level=1, crown_scope="fno")]
    )
    assert resolve_presiding_king("l1-king-session") is None


def test_ac4_hp_an_unknown_session_falls_through_quietly(tmp_path: Path, monkeypatch) -> None:
    from fno.king.escalate import resolve_presiding_king

    _prepare_court(monkeypatch, tmp_path, [])
    assert resolve_presiding_king("no-such-session") is None


def test_ac4_hp_a_uuid_session_id_matches_case_insensitively(tmp_path: Path, monkeypatch) -> None:
    """A UUID-family id differing only in case is still the caller's own row
    (harness_identity.session_identity_key's own contract) - a raw string
    comparison here would silently read every such call as uncrowned."""
    from fno.king.escalate import resolve_presiding_king

    stored = "aaaa1111-bbbb-4ccc-8ddd-eeeeeeeeeeee"
    _prepare_court(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "l2-king",
                status="busy",
                crown_level=2,
                crown_scope="x-epic",
                harness_session_id=stored,
            ),
            _entry("l1-king", status="busy", crown_level=1, crown_scope="fno"),
        ],
    )
    presiding = resolve_presiding_king(stored.upper())
    assert presiding is not None
    assert presiding["holder"] == "l1-king"
    assert resolve_presiding_king(None) is None


def test_mail_presiding_king_is_false_with_no_fno_on_path(monkeypatch) -> None:
    from fno.king.escalate import mail_presiding_king

    monkeypatch.setattr("shutil.which", lambda _name: None)
    assert mail_presiding_king("l1-king", STALLED, "NoProgress") is False


def test_mail_presiding_king_true_only_on_a_zero_exit(monkeypatch) -> None:
    from fno.king.escalate import mail_presiding_king

    class _Proc:
        def __init__(self, code: int, stdout: str) -> None:
            self.returncode = code
            self.stdout = stdout

    monkeypatch.setattr("shutil.which", lambda _name: "/usr/bin/fno")
    monkeypatch.setattr(
        "subprocess.run",
        lambda *a, **k: _Proc(0, "msg-1 delivered (hosted)\n"),
    )
    assert mail_presiding_king("l1-king", STALLED, "NoProgress") is True

    monkeypatch.setattr(
        "subprocess.run",
        lambda *a, **k: _Proc(0, "msg-1 queued (durable) [live-miss]\n"),
    )
    assert mail_presiding_king("l1-king", STALLED, "NoProgress") is False

    monkeypatch.setattr(
        "subprocess.run",
        lambda *a, **k: _Proc(1, "msg-1 delivered (hosted)\n"),
    )
    assert mail_presiding_king("l1-king", STALLED, "NoProgress") is False


# ---------------------------------------------------------------------------
# The channel is keyed by the escalating king's crown scope
# ---------------------------------------------------------------------------


def _two_channels() -> None:
    SCOPES.update({"king-a-session": "fno", "king-b-session": "reaper"})


def _escalate_as(root: Path, session: str, ids: "list[str]") -> "tuple[str, str]":
    return escalate(ids, reason="NoProgress", root=root, session_id=session, cwd=root)


def test_two_reigning_kings_send_distinct_scoped_lanes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Two kings with different stuck sets reconcile in turn: each carries its
    own scoped lane, so neither ever touches the other's open tasks. The
    crown measured 8 asks and 7 mechanical supersedes in 41 minutes on
    2026-09-15 because the channel keyed on the marker alone."""
    _two_channels()
    captured = _capture_transport(
        monkeypatch, answer={"outcome": "asked", "id": "ft-scoped00"}
    )

    _escalate_as(tmp_path, "king-a-session", STALLED)
    _escalate_as(tmp_path, "king-b-session", ["unheld_progress:x-9"])

    lanes = [p["lane"] for p in captured]
    assert lanes == ["king-escalation:fno", "king-escalation:reaper"]


def test_a_scopeless_caller_stays_on_the_shared_marker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A scopeless caller stays on the legacy shared marker."""
    captured = _capture_transport(
        monkeypatch, answer={"outcome": "asked", "id": "ft-shared00"}
    )
    _escalate_as(tmp_path, "k-test", STALLED)
    assert captured[0]["lane"] == "king-escalation"


# --- the reign verdict rides the renderer (x-4d4f) ------------------------------


def test_escalate_threads_verdict_and_scope_to_the_renderer(tmp_path: Path, monkeypatch) -> None:
    """The verdict and its scope ride the payload into the renderer; the
    rendered words themselves are the crate's contract (king_escalation.rs)."""
    seen: dict = {}

    def _spy(ids, key, reason, *, live=None, unknown_reason=None, verdict=None, scope=None,
             session_id=None) -> dict:
        seen.update(
            ids=ids, key=key, reason=reason,
            live=live, unknown_reason=unknown_reason, verdict=verdict, scope=scope,
            session_id=session_id,
        )
        return _fake_render(ids, key, reason)

    monkeypatch.setattr("fno.king.escalate._render", _spy)

    escalate(
        ["reading:undelivered:x-a792"], reason="Budget", root=tmp_path,
        session_id="k-test", cwd=tmp_path,
        verdict="stalled undelivered 9", scope="x-a792",
    )
    assert seen["verdict"] == "stalled undelivered 9"
    assert seen["scope"] == "x-a792"
    assert seen["ids"] == ["reading:undelivered:x-a792"]


def test_escalate_raises_on_a_renderer_refusal(tmp_path: Path, monkeypatch) -> None:
    """``ok: false`` is a refusal, not a fallback: the caller raises while the
    channel is untouched - no task is filed from refused text."""
    _capture_transport(monkeypatch, answer={"outcome": "asked", "id": "ft-refused0"})

    def _refuse(ids, key, reason, **_kw) -> dict:
        return {"ok": False, "message": "king escalation refused: empty set"}

    monkeypatch.setattr("fno.king.escalate._render", _refuse)
    with pytest.raises(ValueError, match="refused"):
        escalate(STALLED, reason="Budget", root=tmp_path, session_id="k-test", cwd=tmp_path)
    assert read_open_questions(tmp_path) == []
