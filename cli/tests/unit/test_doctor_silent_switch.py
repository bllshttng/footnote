"""Tests for `fno doctor` silent-switch legibility (x-8cd5 Wave 6).

The rule, both directions: any default-off switch that can silently produce
inaction owes a doctor line; any default-on switch that can silently take an
irreversible action owes one too. Advisory only, never changes the exit code.
"""
from __future__ import annotations

import types

import pytest
from typer.testing import CliRunner

from fno import doctor
from fno.cli import app

runner = CliRunner()


def _fake_settings(
    *,
    active_backlog: bool = False,
    think_spawn: bool = False,
    auto_merge: bool = False,
    grant: str = "none",
) -> types.SimpleNamespace:
    return types.SimpleNamespace(
        active_backlog=types.SimpleNamespace(enabled=active_backlog),
        think_spawn=types.SimpleNamespace(enabled=think_spawn),
        auto_merge=types.SimpleNamespace(enabled=auto_merge, grant=grant),
    )


def _patch_silent(
    monkeypatch: pytest.MonkeyPatch,
    *,
    settings: types.SimpleNamespace,
    missions: int = 0,
    armed: dict[str, int] | int = 0,
) -> None:
    # `armed` accepts the real dict-by-source shape or a plain count (tests that
    # do not care about the breakdown).
    armed_value = armed if isinstance(armed, dict) else {"config": armed}
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    monkeypatch.setattr(doctor, "_mission_active_count", lambda: missions)
    monkeypatch.setattr(doctor, "_auto_merge_armed_manifests", lambda: armed_value)
    monkeypatch.setattr(doctor, "_read_posture_stamp", lambda: None)


# ---------------------------------------------------------------------------
# Collector: both directions
# ---------------------------------------------------------------------------


def test_drain_off_with_missions_named_as_inaction(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False),
        missions=5,
    )
    report = doctor._silent_switch_report()
    sw = [f for f in report["findings"] if f["switch"] == "active_backlog.enabled"]
    assert len(sw) == 1
    assert sw[0]["direction"] == "inaction"
    assert sw[0]["count"] == 5
    assert sw[0]["command"] == "fno config set active_backlog.enabled true"


def test_drain_off_with_no_missions_is_not_inaction(monkeypatch: pytest.MonkeyPatch) -> None:
    """A default-off switch with nothing to drain is a default, not inaction."""
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False),
        missions=0,
    )
    report = doctor._silent_switch_report()
    assert not any(f["switch"] == "active_backlog.enabled" for f in report["findings"])


def test_think_spawn_off_named_as_inaction(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(monkeypatch, settings=_fake_settings(think_spawn=False))
    report = doctor._silent_switch_report()
    ts = [f for f in report["findings"] if f["switch"] == "think_spawn.enabled"]
    assert len(ts) == 1
    assert ts[0]["direction"] == "inaction"
    assert "think_spawn.enabled true" in ts[0]["command"]


def test_auto_merge_armed_named_as_irreversible(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(auto_merge=True, grant="dispatch"),
        armed={"config": 2, "env-target-auto-merge": 1},
    )
    report = doctor._silent_switch_report()
    irreversible = [f for f in report["findings"] if f["direction"] == "irreversible"]
    switches = {f["switch"] for f in irreversible}
    assert "auto_merge.enabled" in switches
    assert "auto_merge.grant" in switches
    assert "auto_merge_approved (worktree manifests)" in switches
    by_sw = {f["switch"]: f for f in irreversible}
    assert by_sw["auto_merge_approved (worktree manifests)"]["count"] == 3
    assert "2 config" in by_sw["auto_merge_approved (worktree manifests)"]["count_label"]
    assert "1 env-target-auto-merge" in by_sw["auto_merge_approved (worktree manifests)"]["count_label"]
    # Every irreversible finding carries a disarm command (x-4be1: the grant
    # disarms to the 'none' literal, not a bool). The command must SET AN OFF
    # VALUE, not merely be any config-set: an arming command here would tell the
    # operator to grant unattended merge while disarming it.
    for f in irreversible:
        assert f["command"].startswith("fno config set")
        assert f["command"].endswith((" false", " none"))


def test_armed_unknown_manifests_name_stale_plugin_cache_as_cause(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """x-4be1: an `unknown` source count is answerable. When the deployed
    claude plugin cache is PROVEN stale, the armed-manifest finding carries the
    cause; any other cache verdict stays a bare count (never guess an origin).

    x-2486: the cause names `claude plugin update`, the verb that owns
    installed_plugins.json, and explicitly rules out `fno doctor update`. The old
    prescription was measured against the artifact and left it byte-identical
    on a 15-minute exit-0 run, so naming it here sent readers to a no-op."""
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(auto_merge=True, grant="dispatch"),
        armed={"unknown": 3},
    )
    monkeypatch.setattr(
        doctor,
        "_plugin_cache_report",
        lambda: {
            "status": "stale",
            "sha": "a8f3c5537ed55",
            "installed_at": "2026-08-13T04:48:50.501Z",
        },
    )
    report = doctor._silent_switch_report()
    by_sw = {f["switch"]: f for f in report["findings"]}
    finding = by_sw["auto_merge_approved (worktree manifests)"]
    assert finding["cause"] == (
        "deployed plugin cache is stale (a8f3c5537ed5, 2026-08-13); likely "
        "predates the auto_merge_source writer. Fix: claude plugin "
        "update fno@footnote (not fno doctor update), then restart"
    )


def test_armed_unknown_manifests_no_cause_when_cache_not_proven_stale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A fresh or unknown cache verdict must not fabricate a cause: a genuinely
    pre-provenance manifest stays a bare `unknown` count."""
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(auto_merge=True, grant="dispatch"),
        armed={"unknown": 3},
    )
    monkeypatch.setattr(
        doctor, "_plugin_cache_report", lambda: {"status": "unknown", "sha": None}
    )
    report = doctor._silent_switch_report()
    by_sw = {f["switch"]: f for f in report["findings"]}
    assert "cause" not in by_sw["auto_merge_approved (worktree manifests)"]


def test_armed_manifests_not_irreversible_when_kill_switch_off(monkeypatch: pytest.MonkeyPatch) -> None:
    """A manifest's auto_merge_approved is inert while auto_merge.enabled is off.

    The sanctioned merge path checks that switch first, so no green PR can merge
    unattended no matter how many manifests carry approval. Counting them as an
    active irreversible risk in that state is the false alarm this guards.
    """
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=True, think_spawn=True, auto_merge=False),
        armed=4,
    )
    report = doctor._silent_switch_report()
    assert not any(
        f["switch"] == "auto_merge_approved (worktree manifests)"
        for f in report["findings"]
    )
    assert not any(f["direction"] == "irreversible" for f in report["findings"])


def test_both_directions_when_drain_off_and_merge_armed(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False, auto_merge=True, grant="dispatch"),
        missions=2,
        armed=1,
    )
    report = doctor._silent_switch_report()
    directions = {f["direction"] for f in report["findings"]}
    assert directions == {"inaction", "irreversible"}


# ---------------------------------------------------------------------------
# Behavioural: the doctor command names the switch in its output
# ---------------------------------------------------------------------------


def _stub_quiet_machine(monkeypatch: pytest.MonkeyPatch) -> None:
    """Silence the expensive doctor collectors so the verdict is 'fresh' and the
    silent-switch lines are the interesting output."""
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: None)
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(doctor, "_python_content_drift", lambda source: 0)
    monkeypatch.setattr(
        doctor, "_rust_report", lambda: {"binary": None, "revision": None}
    )
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)
    monkeypatch.setattr(
        doctor, "_groom_health", lambda: {"state": "ran", "hours": 3.0, "stale": False, "agent_installed": True}
    )
    monkeypatch.setattr(doctor, "_launch_agent_failures", lambda: {"applicable": True, "dead": []})


def test_doctor_names_drain_off_with_missions(monkeypatch: pytest.MonkeyPatch) -> None:
    """Verify behaviourally: drain disabled + >=1 mission active -> doctor names it."""
    _stub_quiet_machine(monkeypatch)
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False, think_spawn=False, auto_merge=True, grant="dispatch"),
        missions=5,
        armed=31,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, result.output
    out = result.output
    assert "active_backlog.enabled is OFF but 5 mission_active epic(s) queued waiting" in out
    assert "fno config set active_backlog.enabled true" in out
    assert "think_spawn.enabled is OFF" in out
    # Both directions in one report.
    assert "auto_merge.enabled is ARMED" in out
    assert "auto_merge.grant is ARMED" in out
    assert "31 manifest(s)" in out
    # Advisory: does not change the exit code.
    assert "up to date" in out or "status unknown" in out


# ---------------------------------------------------------------------------
# Pair check (x-0888): auto_merge armed vs the merge gate's own lane predicate
# ---------------------------------------------------------------------------


def _fake_review(
    *,
    self_review_required: bool = False,
    peers: list | None = None,
    peer_identity: str | None = None,
) -> types.SimpleNamespace:
    return types.SimpleNamespace(
        self_review_required=self_review_required,
        required_bots=None,
        optional_apps=None,
        reviewers=None,
        peers=peers or [],
        peer_identity=peer_identity,
    )


def _patch_gap(
    monkeypatch: pytest.MonkeyPatch,
    *,
    settings: types.SimpleNamespace,
    source,
    lane: bool,
    review: types.SimpleNamespace | None = None,
    armed: dict[str, int] | int = 0,
) -> list[dict]:
    """Stub the gap check's collaborators; return the lane-predicate calls so
    tests can pin the reuse contract (doctor asks the gate, never recounts)."""
    armed_value = armed if isinstance(armed, dict) else {"config": armed}
    calls: list[dict] = []

    def fake_lane(repo, pr_number=0, code_payload=False):
        calls.append({"repo": repo, "pr_number": pr_number, "code_payload": code_payload})
        return lane

    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    monkeypatch.setattr(doctor, "_resolve_source", lambda _source: source)
    monkeypatch.setattr("fno.pr._merge._review_lane_configured", fake_lane)
    if review is not None:
        monkeypatch.setattr(
            "fno.config.load_settings_for_repo",
            lambda _path: types.SimpleNamespace(review=review),
        )
    monkeypatch.setattr(doctor, "_auto_merge_armed_manifests", lambda: armed_value)
    return calls


def test_gap_fires_on_armed_with_zero_lanes(
    monkeypatch: pytest.MonkeyPatch, tmp_path,
) -> None:
    calls = _patch_gap(
        monkeypatch,
        settings=_fake_settings(auto_merge=True),
        source=tmp_path,
        lane=False,
        review=_fake_review(self_review_required=False),
        armed={"config": 16, "unknown": 3},
    )
    gap = doctor._auto_merge_review_gap()
    # Reuse contract: the verdict is the merge gate's own predicate, asked
    # about a hypothetical code payload - never a doctor-side lane recount.
    assert calls == [{"repo": str(tmp_path), "pr_number": 0, "code_payload": True}]
    assert gap is not None
    assert gap["keys"] == [
        "review.self_review_required=False",
        "review.required_bots=[]",
        "review.reviewers=[]",
        "no identity-free review.peers",
    ]
    assert gap["manifests"] == 19
    assert "16 config" in gap["breakdown"]
    # An unknown source is its own answer, never folded into a default.
    assert "3 unknown" in gap["breakdown"]
    assert gap["remedy"].startswith("fno config set review.self_review_required true")


def test_gap_names_peer_identity_when_it_released_the_floor(
    monkeypatch: pytest.MonkeyPatch, tmp_path,
) -> None:
    _patch_gap(
        monkeypatch,
        settings=_fake_settings(auto_merge=True),
        source=tmp_path,
        lane=False,
        review=_fake_review(self_review_required=True, peer_identity="fno-peer-bot"),
        armed=0,
    )
    gap = doctor._auto_merge_review_gap()
    assert gap is not None
    assert "review.self_review_required=True" in gap["keys"]
    assert "review.peer_identity=fno-peer-bot" in gap["keys"]
    # Zero armed manifests is still reported: the config-level risk stands.
    assert gap["manifests"] == 0


def test_gap_silent_when_a_lane_covers_or_the_switch_is_off(
    monkeypatch: pytest.MonkeyPatch, tmp_path,
) -> None:
    _patch_gap(
        monkeypatch,
        settings=_fake_settings(auto_merge=True),
        source=tmp_path,
        lane=True,
        review=_fake_review(self_review_required=True),
    )
    assert doctor._auto_merge_review_gap() is None
    # Kill switch off: manifests and lanes are inert, matching the ARMED lines.
    _patch_gap(
        monkeypatch,
        settings=_fake_settings(auto_merge=False),
        source=tmp_path,
        lane=False,
        review=_fake_review(self_review_required=False),
    )
    assert doctor._auto_merge_review_gap() is None
    # No source checkout: no merge path to warn about.
    _patch_gap(
        monkeypatch,
        settings=_fake_settings(auto_merge=True),
        source=None,
        lane=False,
        review=_fake_review(self_review_required=False),
    )
    assert doctor._auto_merge_review_gap() is None


def test_doctor_names_armed_auto_merge_with_zero_review_lanes(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_quiet_machine(monkeypatch)
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(auto_merge=True, grant="dispatch"),
        armed={"config": 16, "unknown": 3},
    )
    monkeypatch.setattr(
        doctor,
        "_auto_merge_review_gap",
        lambda armed_manifests=None: {
            "keys": [
                "review.self_review_required=False",
                "review.required_bots=[]",
                "review.reviewers=[]",
                "no identity-free review.peers",
            ],
            "manifests": 19,
            "breakdown": "16 config, 3 unknown",
            "remedy": (
                "fno config set review.self_review_required true (or name a lane "
                "via review.required_bots / review.reviewers)"
            ),
        },
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, result.output
    out = result.output
    assert "auto_merge is ARMED with zero review lanes" in out
    assert "review.self_review_required=False" in out
    assert "19 armed manifest(s): 16 config, 3 unknown" in out
    assert "fno config set review.self_review_required true" in out
