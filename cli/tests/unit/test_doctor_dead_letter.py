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


def _seed(to, body, ts):
    from fno.bus.log import Envelope, append

    append(Envelope.new(from_="claude-cafe0001", to=to, kind="send", body=body, ts=ts))


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
    found = doctor._stale_dead_letters(live_handles=set())
    assert [f["handle"] for f in found] == ["claude-deadbeef"]


def test_doctor_dead_letter_fresh_mail_not_flagged(bus):
    now = datetime.now(tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    _seed("claude-deadbeef", "just sent", ts=now)
    assert doctor._stale_dead_letters(live_handles=set()) == []


def test_doctor_dead_letter_live_handle_not_flagged(bus):
    _seed("claude-deadbeef", "old but alive", ts="2020-01-01T00:00:00Z")
    # The recipient IS live -> not a dead letter.
    assert doctor._stale_dead_letters(live_handles={"claude-deadbeef"}) == []


def test_doctor_dead_letter_project_recipient_never_flagged(bus):
    # A project-addressed durable note is not an a2a handle; never a dead letter.
    _seed("web", "project note", ts="2020-01-01T00:00:00Z")
    assert doctor._stale_dead_letters(live_handles=set()) == []


def test_doctor_dead_letter_bare_handle_surfaces(bus):
    # The bare short-id is the generated address now; if the scan only admitted
    # the prefixed form the diagnostic would go quiet for every new session.
    _seed("deadbeef", "stranded", ts="2020-01-01T00:00:00Z")
    assert [f["handle"] for f in doctor._stale_dead_letters(live_handles=set())] == ["deadbeef"]


def test_doctor_dead_letter_all_hex_project_not_flagged(bus):
    # An all-hex project name matches the bare-handle shape, so to_kind is what
    # keeps a project broadcast from being reported as mail to a dead session.
    from fno.bus.log import Envelope, append

    append(Envelope.new(
        from_="claude-cafe0001", to="deadbeef", kind="send", body="project note",
        ts="2020-01-01T00:00:00Z", to_kind="project",
    ))
    assert doctor._stale_dead_letters(live_handles=set()) == []


def test_doctor_dead_letter_both_findings_surface(bus, tmp_path, monkeypatch):
    # The plan's verify: a stale unread envelope + a hooks config without
    # drain-self -> both findings surface in the report (advisory, never blocks).
    _seed("claude-deadbeef", "stranded", ts="2020-01-01T00:00:00Z")
    unwired = tmp_path / "hooks.json"
    _write_hooks(unwired, with_drain=False)
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(tmp_path))
    (tmp_path / "hooks").mkdir()
    _write_hooks(tmp_path / "hooks" / "hooks.json", with_drain=False)
    monkeypatch.setattr(doctor, "_live_a2a_handles", lambda: set())

    report = doctor._dead_letter_report()
    assert report["drain_hook_wired"] is False
    assert [f["handle"] for f in report["stale_unread"]] == ["claude-deadbeef"]
