"""US7 (x-605c): `fno doctor` dead-letter visibility.

Two advisory findings, never blocking: (a) a claude env whose `drain-self`
SessionStart hook is not wired, (b) unread bus mail past a threshold addressed
to a handle with no live session.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone

import pytest

from fno import doctor
from fno.paths_testing import use_tmpdir


@pytest.fixture
def bus(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)


def _seed(to, body, ts, *, meta=None):
    from fno.bus.log import Envelope, append

    env = Envelope.new(from_="claude-cafe0001", to=to, kind="send", body=body, ts=ts, meta=meta)
    append(env)
    return env.id


def _write_hooks(path, *, with_drain):
    cmd = (
        "${CLAUDE_PLUGIN_ROOT}/hooks/inject-mail-drain-session-start.sh"
        if with_drain
        else "${CLAUDE_PLUGIN_ROOT}/hooks/some-other-hook.sh"
    )
    path.write_text(
        json.dumps({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}]}}),
        encoding="utf-8",
    )


# --------------------------------------------------------------------------
# (a) drain-self hook wiring
# --------------------------------------------------------------------------


def test_doctor_drain_hook_wired_true_false_none(tmp_path):
    wired = tmp_path / "wired.json"
    _write_hooks(wired, with_drain=True)
    assert doctor._drain_hook_wired(wired) is True

    unwired = tmp_path / "unwired.json"
    _write_hooks(unwired, with_drain=False)
    assert doctor._drain_hook_wired(unwired) is False

    # Unlocatable config -> None (advisory, don't guess).
    assert doctor._drain_hook_wired(tmp_path / "absent.json") is None


# --------------------------------------------------------------------------
# (b) stale unread bus mail to a dead handle
# --------------------------------------------------------------------------


def test_doctor_dead_letter_stale_unread_surfaces(bus):
    _seed("claude-deadbeef", "stranded", ts="2020-01-01T00:00:00Z")
    found = doctor._stale_dead_letters()
    assert [f["handle"] for f in found] == ["claude-deadbeef"]


@pytest.mark.parametrize("provider", ["claude", "codex", "gemini", "agy", "opencode"])
def test_doctor_dead_letter_covers_every_retired_provider_prefix(bus, provider):
    handle = f"{provider}-deadbeef"
    _seed(handle, "stranded", ts="2020-01-01T00:00:00Z")

    assert [f["handle"] for f in doctor._stale_dead_letters()] == [handle]


def test_doctor_dead_letter_fresh_mail_not_flagged(bus):
    now = datetime.now(tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    _seed("claude-deadbeef", "just sent", ts=now)
    assert doctor._stale_dead_letters() == []


def test_doctor_dead_letter_drained_handle_not_flagged(bus):
    # A recipient that genuinely drained advances its cursor; scan_unread then
    # returns nothing, so the thread never surfaces - liveness is not the gate,
    # the drain cursor is.
    from fno.bus.cursor import write_cursor

    mid = _seed("claude-deadbeef", "drained", ts="2020-01-01T00:00:00Z")
    write_cursor("claude-deadbeef", mid)
    assert doctor._stale_dead_letters() == []


def test_doctor_dead_letter_wedged_recipient_still_escalates(bus):
    # AC8-FR: a recipient whose drain never ran (cursor never advanced) still
    # escalates once past TTL, even if a roster would list it live. Roster
    # liveness is deliberately NOT consulted here.
    _seed("claude-deadbeef", "wedged", ts="2020-01-01T00:00:00Z")
    assert [f["handle"] for f in doctor._stale_dead_letters()] == ["claude-deadbeef"]


def test_doctor_dead_letter_project_recipient_never_flagged(bus):
    # A project-addressed durable note is not an a2a handle; never a dead letter.
    _seed("web", "project note", ts="2020-01-01T00:00:00Z")
    assert doctor._stale_dead_letters() == []


def test_doctor_dead_letter_bare_handle_surfaces(bus):
    # The bare short-id is the generated address now; if the scan only admitted
    # the prefixed form the diagnostic would go quiet for every new session.
    _seed("deadbeef", "stranded", ts="2020-01-01T00:00:00Z")
    assert [f["handle"] for f in doctor._stale_dead_letters()] == ["deadbeef"]


def test_doctor_dead_letter_all_hex_project_not_flagged(bus):
    # An all-hex project name matches the bare-handle shape, so to_kind is what
    # keeps a project broadcast from being reported as mail to a dead session.
    from fno.bus.log import Envelope, append

    append(Envelope.new(
        from_="claude-cafe0001", to="deadbeef", kind="send", body="project note",
        ts="2020-01-01T00:00:00Z", to_kind="project",
    ))
    assert doctor._stale_dead_letters() == []


def test_doctor_dead_letter_both_findings_surface(bus, tmp_path, monkeypatch):
    # The plan's verify: a stale unread envelope + a hooks config without
    # drain-self -> both findings surface in the report (advisory, never blocks).
    _seed("claude-deadbeef", "stranded", ts="2020-01-01T00:00:00Z")
    unwired = tmp_path / "hooks.json"
    _write_hooks(unwired, with_drain=False)
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(tmp_path))
    (tmp_path / "hooks").mkdir()
    _write_hooks(tmp_path / "hooks" / "hooks.json", with_drain=False)

    report = doctor._dead_letter_report()
    assert report["drain_hook_wired"] is False
    assert [f["handle"] for f in report["stale_unread"]] == ["claude-deadbeef"]


# --------------------------------------------------------------------------
# (c) per-owner TTL horizon (US6/US7): the stamp drives the verdict, not age
# --------------------------------------------------------------------------


def _fresh_ts(minutes_ago=1):
    from datetime import datetime, timedelta, timezone

    return (datetime.now(tz=timezone.utc) - timedelta(minutes=minutes_ago)).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


def test_dead_letter_owner_ttl_in_past_surfaces_immediately(bus):
    # A dead-letter's ttl_at is its birth, so a just-sent envelope surfaces at
    # once - the blanket 24h age would have kept it quiet.
    _seed(
        "claude-deadbeef",
        "born dead",
        ts=_fresh_ts(),
        meta={"owner": "dead-letter", "ttl_at": "2020-01-01T00:00:00Z"},
    )
    found = doctor._stale_dead_letters()
    assert [(f["handle"], f["owner"]) for f in found] == [("claude-deadbeef", "dead-letter")]


def test_wake_daemon_within_horizon_not_flagged(bus):
    # A wake-daemon thread inside its ttl_at horizon is not yet stranded, even
    # though its recipient is not live - the sweep waits the horizon out.
    from datetime import datetime, timedelta, timezone

    future = (datetime.now(tz=timezone.utc) + timedelta(hours=5)).strftime("%Y-%m-%dT%H:%M:%SZ")
    _seed(
        "claude-deadbeef",
        "still waiting",
        ts=_fresh_ts(),
        meta={"owner": "wake-daemon", "ttl_at": future},
    )
    assert doctor._stale_dead_letters() == []


def test_dead_letter_owner_stamped_named_recipient_surfaces(bus):
    # A registered-agent name like `alpha` is not a hex handle, but an
    # owner-stamped durable envelope carries its own ttl_at, so the sweep must
    # find it - hex-only recipient discovery used to miss named recipients
    # (dispatch_send writes its durable fallback to the registry name).
    _seed(
        "alpha",
        "stranded named recipient",
        ts=_fresh_ts(),
        meta={"owner": "wake-daemon", "ttl_at": "2020-01-01T00:00:00Z"},
    )
    found = doctor._stale_dead_letters()
    assert [(f["handle"], f["owner"]) for f in found] == [("alpha", "wake-daemon")]
