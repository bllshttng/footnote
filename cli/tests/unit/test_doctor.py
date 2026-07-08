"""Unit tests for `fno doctor` (ab-5a1fc285 + ab-a78c9731).

Covers US1 (detection: fresh / stale / unknown / json / no-source / probe-error),
US3-adjacent --fix behavior (delegates to `fno update`, honors the IN_PROGRESS
guard), and US2 (rust staleness fold-in: full evidence mismatch -> stale,
partial evidence -> not stale, --fix rust-only leg runs the refresh helper,
never shells out to cargo directly).

The signal collectors (_resolve_source, _source_rev, _read_marker,
_probe_installed_verb, _rust_report, _read_rust_marker, _rust_source_rev,
_cargo_bin) are module-level so each test stubs them for a hermetic,
network-free verdict.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import doctor
from fno.cli import app

runner = CliRunner()


def _stub_signals(
    monkeypatch: pytest.MonkeyPatch,
    *,
    src: Path | None,
    source_rev: str | None,
    marker: str | None,
    capture_present: str,  # ProbeResult: "present" | "missing" | "unknown"
    rust_binary: str | None = None,
    rust_marker: str | None = None,
    rust_source_rev: str | None = None,
    cargo_bin_present: bool = False,
) -> None:
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: src)
    monkeypatch.setattr(doctor, "_source_rev", lambda source: source_rev)
    monkeypatch.setattr(doctor, "_read_marker", lambda: marker)
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: capture_present)
    # Post ab-716cd330 `revision` carries the binary's self-reported crates/ rev
    # (the verdict driver), not the installed-rust-rev marker. `rust_marker` is
    # the value the resolved binary reports here.
    monkeypatch.setattr(
        doctor,
        "_rust_report",
        lambda: {"binary": rust_binary, "revision": rust_marker},
    )
    monkeypatch.setattr(doctor, "_read_rust_marker", lambda: rust_marker)
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: rust_source_rev)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: cargo_bin_present)


# ---------------------------------------------------------------------------
# US1: detection
# ---------------------------------------------------------------------------


def test_ac1_hp_fresh_install_reports_healthy(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: marker == source HEAD => fresh, exit 0."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" in result.stdout


def test_ac1_err_missing_verb_reports_skew_and_exits_nonzero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-ERR: capability probe proves a missing verb => stale, exit nonzero, names the verb + remediation."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # marker even matches, but the probe wins
        capture_present="missing",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "missing: backlog capture" in result.stdout
    assert "fno update" in result.stdout


def test_ac1_err_rev_behind_reports_stale(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR variant: marker behind source HEAD => stale, exit nonzero."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "behind" in result.stdout


def test_ac1_ui_json_is_single_object_on_stdout(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-UI: --json emits one JSON object on stdout; human text to stderr."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
    )
    result = runner.invoke(app, ["doctor", "--json"])
    assert result.exit_code != 0  # stale
    # stdout must be a single parseable JSON object with the contract fields.
    payload = json.loads(result.stdout.strip())
    assert payload["status"] == "stale"
    assert payload["python_stale"] is True
    assert payload["rust_stale"] is False
    assert payload["missing_verbs"] == []
    assert payload["source_rev"] == "newsha"
    assert payload["installed_rev"] == "oldsha"
    assert payload["rust_binary"] == "/cargo/bin/fno-agents"
    # Human/metadata text is on stderr, not mixed into the JSON stdout.
    assert "fno doctor" in result.stderr


def test_ac1_edge_no_source_degrades_to_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-EDGE: no resolvable source => unknown, exit 0 (cannot prove stale)."""
    _stub_signals(
        monkeypatch,
        src=None,
        source_rev=None,
        marker="abc123",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "no source checkout to compare against" in result.stdout


def test_ac1_fr_rev_probe_error_still_produces_verdict(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-FR: git rev undeterminable => revision signal degrades, verdict still produced, no traceback."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev=None,  # git rev-parse failed
        marker="abc123",
        capture_present="present",  # capability probe still ran and found nothing missing
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert result.exception is None
    assert "unknown" in result.stdout


def test_marker_absent_is_not_false_fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    """Boundary: a missing marker must not report 'fresh' even with source resolvable."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker=None,  # pre-marker install
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" not in result.stdout
    assert "unknown" in result.stdout


def test_rust_binary_always_reported(monkeypatch: pytest.MonkeyPatch) -> None:
    """The resolved fno-agents binary is always reported; 'undeterminable' is NOT pinned."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/wheel/_bin/fno-agents",
    )
    result = runner.invoke(app, ["doctor"])
    assert "/wheel/_bin/fno-agents" in result.stdout


# ---------------------------------------------------------------------------
# _verdict: pure decision matrix
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "kwargs,expected_status,expected_python_stale,expected_rust_stale",
    [
        # Existing rows: no rust evidence passed - rust_stale must stay False.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present"), "fresh", False, False),
        (dict(source_resolved=True, source_rev="a", marker="b", capture_present="present"), "stale", True, False),
        (dict(source_resolved=True, source_rev="a", marker=None, capture_present="present"), "unknown", False, False),
        (dict(source_resolved=False, source_rev=None, marker="a", capture_present="present"), "unknown", False, False),
        (dict(source_resolved=True, source_rev=None, marker="a", capture_present="present"), "unknown", False, False),
        # Probe proves missing verb => stale regardless of source/marker.
        (dict(source_resolved=False, source_rev=None, marker=None, capture_present="missing"), "stale", True, False),
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="missing"), "stale", True, False),
        # Rust fold-in: full evidence mismatch + python fresh -> rust_stale True, status stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="bbb", cargo_bin_present=True), "stale", False, True),
        # Rust fold-in: full evidence match + python fresh -> rust_stale False, status fresh.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="aaa", cargo_bin_present=True), "fresh", False, False),
        # Rust fold-in: partial evidence (no cargo bin) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="bbb", cargo_bin_present=False), "fresh", False, False),
        # Rust fold-in: partial evidence (marker None) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev=None, rust_source_rev="bbb", cargo_bin_present=True), "fresh", False, False),
        # Rust fold-in: partial evidence (rust_source_rev None) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev=None, cargo_bin_present=True), "fresh", False, False),
        # Rust fold-in: python stale + rust stale -> status stale.
        (dict(source_resolved=True, source_rev="a", marker="b", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="bbb", cargo_bin_present=True), "stale", True, True),
    ],
)
def test_verdict_matrix(kwargs, expected_status, expected_python_stale, expected_rust_stale) -> None:
    v = doctor._verdict(**kwargs)
    assert v["status"] == expected_status
    assert v["python_stale"] is expected_python_stale
    assert v["rust_stale"] is expected_rust_stale


# ---------------------------------------------------------------------------
# AC2: rust staleness detection
# ---------------------------------------------------------------------------


def test_ac2_hp_rust_stale_json_payload(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC2-HP: cargo bin + marker aaa + rust_source_rev bbb + python fresh -> --json shows rust_stale: true, status stale, exit 1."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    result = runner.invoke(app, ["doctor", "--json"])
    assert result.exit_code == 1
    payload = json.loads(result.stdout.strip())
    assert payload["rust_stale"] is True
    assert payload["rust_installed_rev"] == "aaa"
    assert payload["rust_source_rev"] == "bbb"
    assert payload["status"] == "stale"
    assert payload["python_stale"] is False


@pytest.mark.parametrize(
    "rust_marker,rust_source_rev,cargo_bin_present,python_status,expected_exit",
    [
        # No cargo bin - not stale.
        (None, "bbb", False, "fresh", 0),
        # Marker None - not stale (unknown).
        (None, "bbb", True, "fresh", 0),
        # rust_source_rev None - not stale.
        ("aaa", None, True, "fresh", 0),
        # python unknown + rust evidence gap -> still exit 0 unknown.
        (None, None, False, "unknown", 0),
    ],
)
def test_ac2_err_degrade_matrix(
    monkeypatch: pytest.MonkeyPatch,
    rust_marker: str | None,
    rust_source_rev: str | None,
    cargo_bin_present: bool,
    python_status: str,
    expected_exit: int,
) -> None:
    """AC2-ERR: incomplete rust evidence -> rust_stale false, exit 0."""
    # For python_status "unknown" use no source, for "fresh" use matching marker.
    if python_status == "unknown":
        src = None
        source_rev = None
        marker = None
        cp = "present"
    else:
        src = Path("/src")
        source_rev = "abc"
        marker = "abc"
        cp = "present"
    _stub_signals(
        monkeypatch,
        src=src,
        source_rev=source_rev,
        marker=marker,
        capture_present=cp,
        rust_marker=rust_marker,
        rust_source_rev=rust_source_rev,
        cargo_bin_present=cargo_bin_present,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == expected_exit
    payload_json_result = runner.invoke(app, ["doctor", "--json"])
    payload = json.loads(payload_json_result.stdout.strip())
    assert payload["rust_stale"] is False


def test_ac2_err_binary_present_marker_absent_explains(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC2-ERR: binary present but marker absent -> human output explains why revision is unknown."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker=None,  # no marker yet
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    # Should mention revision unknown / no marker / seed it via fno update.
    combined = result.stdout + result.stderr
    assert "revision unknown" in combined or "no installed-rust-rev marker" in combined


@pytest.mark.parametrize(
    "rust_marker,rust_source_rev,rust_binary,cargo_bin_present,expected_fragment",
    [
        # Not installed.
        (None, None, None, False, "not found"),
        # Fresh.
        ("aaa", "aaa", "/cargo/bin/fno-agents", True, "fresh"),
        # Stale.
        ("aaa", "bbb", "/cargo/bin/fno-agents", True, "STALE"),
        # Unknown: binary present, marker absent.
        (None, "bbb", "/cargo/bin/fno-agents", True, "unknown"),
    ],
)
def test_ac2_ui_rust_human_line_states(
    monkeypatch: pytest.MonkeyPatch,
    rust_marker: str | None,
    rust_source_rev: str | None,
    rust_binary: str | None,
    cargo_bin_present: bool,
    expected_fragment: str,
) -> None:
    """AC2-UI: four rust human-output states each produce their identifying line."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
        rust_binary=rust_binary,
        rust_marker=rust_marker,
        rust_source_rev=rust_source_rev,
        cargo_bin_present=cargo_bin_present,
    )
    result = runner.invoke(app, ["doctor"])
    combined = result.stdout + result.stderr
    assert expected_fragment in combined


# ---------------------------------------------------------------------------
# AC2-EDGE: --fix routing
# ---------------------------------------------------------------------------


def test_ac2_edge_rust_only_stale_fix_calls_refresh_not_update(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (a): rust-only stale + --fix -> refresh_rust_bins called, update_command NOT called, exit 0."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Fix C2: ensure the IN_PROGRESS guard does not fire so the fix proceeds
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    refresh_calls: list[dict] = []
    update_calls: list[dict] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append({"source": source, "force": force, "dry_run": dry_run})
        return "refreshed"

    def _fake_update(source=None, dry_run=False, force=False):
        update_calls.append({"source": source})

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert len(refresh_calls) == 1
    assert refresh_calls[0]["source"] == Path("/src")
    assert len(update_calls) == 0


def test_ac2_edge_rust_only_fix_fresh_outcome_exits_zero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (PR #438 Gemini): a concurrent refresh can land between the
    verdict read and the repair, making the helper return 'fresh'. The goal
    state is achieved, so --fix exits 0 - never a failure."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Fix C2: ensure the IN_PROGRESS guard does not fire so the fix proceeds
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    def _fake_refresh(source, *, force=False, dry_run=False):
        return "fresh"

    def _fake_update(source=None, dry_run=False, force=False):
        raise AssertionError("update_command must not run for rust-only --fix")

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert "already fresh" in result.stderr


def test_ac2_edge_rust_only_fix_no_marker_outcome_exits_one(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """ab-703f2ed2: cargo succeeded but the marker was not written (e.g.
    ~/.fno unwritable). The stale verdict cannot converge - the next
    doctor run still reports rust stale - so --fix must not claim success."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    def _fake_refresh(source, *, force=False, dry_run=False):
        return "refreshed-no-marker"

    def _fake_update(source=None, dry_run=False, force=False):
        raise AssertionError("update_command must not run for rust-only --fix")

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 1
    assert "will not converge" in result.stderr


def test_ac2_edge_python_and_rust_stale_fix_delegates_update_only(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (b): python stale + rust stale -> delegates to update_command, _refresh_rust_bins NOT called directly."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    refresh_calls: list[dict] = []
    update_calls: list[dict] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append({})
        return "refreshed"

    def _fake_update(source=None, dry_run=False, force=False):
        update_calls.append({"source": source})

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exception is None
    assert len(update_calls) == 1
    assert len(refresh_calls) == 0  # doctor did not call it directly; update_command owns it


def test_ac2_edge_fix_json_rust_stale_no_repair(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (c): --fix --json with rust-only stale -> no repair call, JSON on stdout, skip message on stderr."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    refresh_calls: list[dict] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append({})
        return "refreshed"

    def _fake_update(source=None, dry_run=False, force=False):
        pass

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--json", "--fix"])
    # stdout is still a single parseable JSON object.
    payload = json.loads(result.stdout.strip())
    assert payload["status"] == "stale"
    assert payload["rust_stale"] is True
    assert len(refresh_calls) == 0
    # The skip message appears on stderr.
    assert "--fix skipped under --json" in result.stderr


# ---------------------------------------------------------------------------
# AC2-FR: follow-up fresh run after successful rust-only fix
# ---------------------------------------------------------------------------


def test_ac2_fr_rust_only_fix_exits_zero_and_followup_is_fresh(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-FR: successful rust-only fix exits 0; re-run with matching markers -> fresh exit 0."""
    # First run: rust only stale.
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Fix C2: ensure the IN_PROGRESS guard does not fire
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    def _fake_refresh(source, *, force=False, dry_run=False):
        return "refreshed"

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0

    # Follow-up run: markers match -> fresh.
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="bbb",  # after fix, marker == source rev
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    result2 = runner.invoke(app, ["doctor"])
    assert result2.exit_code == 0
    combined = result2.stdout + result2.stderr
    assert "fresh" in combined


# ---------------------------------------------------------------------------
# --fix
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# x-8c3b: --fix acts on a dead pr-watch verdict (advisory, never flips exit)
# ---------------------------------------------------------------------------


def _dead_pr_watch(monkeypatch) -> None:
    monkeypatch.setattr(
        doctor,
        "_pr_watch_liveness",
        lambda: {
            "enabled": True, "verdict": "dead", "detail": "no tick recorded",
            "fix": "fno pr-watch install", "loaded": True, "last_tick": None,
        },
    )


def test_fix_heals_dead_pr_watch_on_fresh_binary(monkeypatch: pytest.MonkeyPatch) -> None:
    """A fresh binary with a dead watcher still heals it under --fix, exit 0."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _dead_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    heal_calls: list = []
    monkeypatch.setattr(pw, "heal_watcher", lambda **kw: heal_calls.append(kw) or ("bounced x", 0))

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0  # advisory: a dead watcher never flips the exit
    assert len(heal_calls) == 1
    assert "pr-watch heal" in result.stderr


def test_fix_json_skips_pr_watch_heal(monkeypatch: pytest.MonkeyPatch) -> None:
    """--json preserves the single-JSON-object stdout contract: no heal side-effect."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _dead_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    monkeypatch.setattr(pw, "heal_watcher", lambda **kw: pytest.fail("must not heal under --json"))

    result = runner.invoke(app, ["doctor", "--json", "--fix"])
    assert result.exit_code == 0
    # stdout is exactly one JSON object.
    json.loads(result.stdout.strip())


def test_no_fix_never_heals(monkeypatch: pytest.MonkeyPatch) -> None:
    """A plain `doctor` (no --fix) reports but never runs the bounce."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _dead_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    monkeypatch.setattr(pw, "heal_watcher", lambda **kw: pytest.fail("must not heal without --fix"))

    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "pr-watch enabled but not running" in result.stdout


def test_ac3_hp_fix_delegates_to_update(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC3-HP: --fix on a stale Python install delegates to `fno update`."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
    )
    calls: dict[str, object] = {}

    from fno import update

    def _fake_update(source=None, dry_run=False, force=False):  # noqa: ANN001
        calls["source"] = source
        calls["called"] = True

    monkeypatch.setattr(update, "update_command", _fake_update)
    result = runner.invoke(app, ["doctor", "--fix", "--source", "/src"])
    assert calls.get("called") is True
    assert str(calls["source"]) == "/src"
    assert result.exception is None


def test_ac3_edge_fix_respects_in_progress_guard(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC3-EDGE: --fix during an IN_PROGRESS target refuses (the update guard fires)."""
    # Stale via probe so --fix reaches the python_stale branch without needing source.
    _stub_signals(
        monkeypatch,
        src=None,
        source_rev=None,
        marker=None,
        capture_present="missing",
    )
    # An IN_PROGRESS target-state.md in the resolved repo root triggers update's guard.
    repo_root = tmp_path / "repo"
    (repo_root / ".fno").mkdir(parents=True)
    (repo_root / ".fno" / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\n---\n", encoding="utf-8"
    )
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))
    # Defensive: even if the guard were bypassed, never actually install.
    import fno.update as update_mod

    monkeypatch.setattr(update_mod.os, "execvp", lambda *a, **kw: None)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 1
    assert "refused" in (result.stderr + result.stdout)


def test_fix_nothing_to_do_when_fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    """--fix on a fresh install reports nothing to fix and exits 0."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert "nothing to fix" in result.stderr


def test_stale_missing_verb_without_source_says_behind_source_not_none(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Gemini review: a missing-verb stale verdict with no resolved source must
    print 'behind source', never 'behind None'."""
    _stub_signals(
        monkeypatch,
        src=None,            # no source resolved
        source_rev=None,
        marker=None,
        capture_present="missing",  # probe proves stale regardless of source
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "behind None" not in result.stdout
    assert "behind source" in result.stdout
    assert "missing: backlog capture" in result.stdout


def test_json_fix_does_not_pollute_stdout_or_delegate(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Codex review: `--json --fix` on a stale install keeps stdout a single JSON
    object and does NOT delegate to `fno update` (which prints to stdout)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",  # rev-mismatch stale
    )
    from fno import update

    called = {"update": False}

    def _fake_update(source=None, dry_run=False, force=False):  # noqa: ANN001
        called["update"] = True

    monkeypatch.setattr(update, "update_command", _fake_update)
    result = runner.invoke(app, ["doctor", "--json", "--fix"])
    assert result.exit_code != 0  # stale
    # stdout is still a single parseable JSON object - no update chatter.
    payload = json.loads(result.stdout.strip())
    assert payload["status"] == "stale"
    # update was NOT executed under --json (would have polluted stdout).
    assert called["update"] is False
    # The skip is explicit, on stderr.
    assert "--fix skipped under --json" in result.stderr


# ---------------------------------------------------------------------------
# Fix C2: doctor --fix rust-only branch honors the IN_PROGRESS guard
# ---------------------------------------------------------------------------


def test_ac2_edge_rust_only_fix_respects_in_progress_guard(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Fix C2: rust-only stale + --fix + IN_PROGRESS -> exit 1, "refused" in stderr,
    _refresh_rust_bins never called.

    The python_stale delegation path already inherits update's own guard. The
    rust-only branch called _refresh_rust_bins directly, bypassing the guard.
    """
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Simulate IN_PROGRESS
    monkeypatch.setattr(update, "_target_in_progress", lambda: True)

    # Tripwire: _refresh_rust_bins must NOT be called
    def _tripwire_refresh(source, *, force=False, dry_run=False):
        raise AssertionError("_refresh_rust_bins must not be called when IN_PROGRESS")

    monkeypatch.setattr(update, "_refresh_rust_bins", _tripwire_refresh)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 1
    assert "refused" in result.stderr
    assert "IN_PROGRESS" in result.stderr


# ---------------------------------------------------------------------------
# Fix C1: _emit_human never prints STALE for non-cargo binaries
# ---------------------------------------------------------------------------


def test_ac2_ui_non_cargo_binary_never_reports_stale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Fix C1: a bundled-wheel/PATH binary with a leftover marker mismatch must
    NOT print STALE. The JSON verdict has rust_stale: false (no cargo bin), so
    human output must not contradict it.

    cargo_bin_present=False + marker mismatch -> exit 0, output contains
    "not tracked", does NOT contain "STALE".
    """
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
        rust_binary="/wheel/_bin/fno-agents",
        rust_marker="aaa",       # mismatch evidence but unproven
        rust_source_rev="bbb",
        cargo_bin_present=False,  # no cargo bin -> rust_stale is False in verdict
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, f"exit code {result.exit_code}, output: {result.stdout}{result.stderr}"
    combined = result.stdout + result.stderr
    assert "STALE" not in combined, (
        f"Non-cargo binary must never produce STALE output. Got:\n{combined}"
    )
    assert "not tracked" in combined, (
        f"Expected 'not tracked' for non-cargo binary. Got:\n{combined}"
    )


def test_ac3_fr_fix_rust_only_stale_runs_refresh_never_raw_cargo(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC3-FR (new contract): rust-only stale --fix RUNS the refresh helper, never invokes
    cargo via raw subprocess directly from doctor.py.

    Tripwire: doctor.subprocess.run is wired to explode; only the helper (which is
    stubbed separately) may be called.
    """
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )

    # Tripwire: doctor must never call subprocess.run (which would mean raw cargo).
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda *a, **k: (_ for _ in ()).throw(AssertionError("doctor.py must never run cargo directly")),
    )
    # The advisory mux front-door probe (`fno mux ls`) is a legit doctor
    # subprocess, unrelated to the raw-cargo concern this tripwire guards; stub it
    # so the tripwire isolates cargo, not the probe.
    monkeypatch.setattr(doctor, "_probe_is_mux", lambda p: False)

    from fno import update

    refresh_calls: list[str] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append(str(source))
        return "refreshed"

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert len(refresh_calls) == 1
    assert refresh_calls[0] == str(Path("/src"))


# ---------------------------------------------------------------------------
# ab-24a59d50: binary self-reported git rev (build.rs embed)
# ---------------------------------------------------------------------------


def _fake_run(returncode: int, stdout: str):
    """Build a subprocess.run stub returning a fixed CompletedProcess."""
    import subprocess

    def _run(cmd, *args, **kwargs):
        return subprocess.CompletedProcess(cmd, returncode, stdout=stdout, stderr="")

    return _run


def test_binary_self_rev_returns_git_rev(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        doctor.subprocess, "run", _fake_run(0, '{"git_rev": "deadbeefcafe", "package": "0.1.0"}')
    )
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") == "deadbeefcafe"


def test_binary_self_rev_none_for_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    # A non-git build self-reports "unknown"; treat that as no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, '{"git_rev": "unknown"}'))
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_on_nonzero_exit(monkeypatch: pytest.MonkeyPatch) -> None:
    # An old binary lacking the `version` verb exits non-zero -> no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(2, ""))
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_on_malformed_json(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, "not json at all"))
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_on_oserror(monkeypatch: pytest.MonkeyPatch) -> None:
    def _boom(*args, **kwargs):
        raise OSError("no such binary")

    monkeypatch.setattr(doctor.subprocess, "run", _boom)
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_when_no_binary() -> None:
    # Skips the subprocess entirely when there is no resolved binary.
    assert doctor._binary_self_rev(None) is None


def test_rust_report_revision_from_cargo_binary_single_spawn(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # ab-716cd330: verdict `revision` = binary's crates/ subtree rev (marker-free).
    # gemini PR #491: when resolved == cargo binary, probe `version --json` ONCE.
    from fno.agents import rust_runtime

    monkeypatch.setattr(
        rust_runtime, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
    )
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: "/cargo/bin/fno-agents")
    calls: list[str | None] = []

    def fake_version_json(binary: str | None) -> dict:
        calls.append(binary)
        return {"git_rev": "feedface1234", "crates_rev": "cab5cab5cab5"}

    monkeypatch.setattr(doctor, "_binary_version_json", fake_version_json)
    report = doctor._rust_report()
    assert report["binary"] == "/cargo/bin/fno-agents"
    assert report["revision"] == "cab5cab5cab5"  # verdict driver = cargo crates rev
    assert report["binary_rev"] == "feedface1234"  # HEAD identity, informational
    assert calls == ["/cargo/bin/fno-agents"]  # single spawn when paths coincide


def test_rust_report_revision_comes_from_cargo_not_resolved(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # codex PR #491: a bundled sibling resolves, but the verdict rev must come
    # from the cargo binary the gate (_cargo_bin_present) + --fix target.
    from fno.agents import rust_runtime

    monkeypatch.setattr(
        rust_runtime, "resolve_installed_binary", lambda: Path("/bundled/fno-agents")
    )
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: "/cargo/bin/fno-agents")

    def fake_version_json(binary: str | None) -> dict:
        if binary == "/bundled/fno-agents":
            return {"git_rev": "bbbbbbbbbbbb", "crates_rev": "bundledcrates"}
        return {"git_rev": "cccccccccccc", "crates_rev": "cargocrates12"}

    monkeypatch.setattr(doctor, "_binary_version_json", fake_version_json)
    report = doctor._rust_report()
    assert report["binary"] == "/bundled/fno-agents"  # display = resolved binary
    assert report["binary_rev"] == "bbbbbbbbbbbb"  # informational = resolved HEAD
    assert report["revision"] == "cargocrates12"  # verdict = CARGO crates rev


# ---------------------------------------------------------------------------
# ab-716cd330: binary self-reported crates/ subtree rev (build.rs embed)
# ---------------------------------------------------------------------------


def test_binary_crates_rev_returns_crates_rev(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        _fake_run(0, '{"git_rev": "deadbeefcafe", "crates_rev": "cab5cab5cab5"}'),
    )
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") == "cab5cab5cab5"


def test_binary_crates_rev_none_for_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    # A non-git build self-reports "unknown"; treat that as no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, '{"crates_rev": "unknown"}'))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_when_field_absent(monkeypatch: pytest.MonkeyPatch) -> None:
    # A pre-ab-716cd330 binary has git_rev but no crates_rev -> no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, '{"git_rev": "deadbeefcafe"}'))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_on_nonzero_exit(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(2, ""))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_on_malformed_json(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, "not json at all"))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_when_no_binary() -> None:
    assert doctor._binary_crates_rev(None) is None


def test_emit_human_binary_rev_shown_as_build_provenance(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """AC1-UI + AC1-EDGE: git_rev (HEAD) is build provenance only, never
    compared to the crates/ source rev. A "fresh" verdict and the HEAD line must
    never contradict each other - even when HEAD has advanced past the last
    crates/ commit (python-only commits since), the exact false alarm from the
    incident where "rust bins fresh" printed beside a bogus rev mismatch.
    """
    result = {
        "status": "fresh",
        "rust_stale": False,
        "rust_installed_rev": "abc123abc123",  # crates_rev drives the verdict
        "rust_source_rev": "abc123abc123",  # crates/ subtree rev
        "missing_verbs": [],
        "python_stale": False,
    }
    # binary_rev = HEAD, DELIBERATELY newer than the crates/ rev (python-only commits).
    rust = {
        "binary": "/cargo/bin/fno-agents",
        "revision": "abc123abc123",
        "binary_rev": "deadbeef9999",
    }
    doctor._emit_human(result, Path("/src"), rust, err=False, cargo_present=True)
    out = capsys.readouterr().out
    # Fresh verdict AND the HEAD line coexist without contradiction.
    assert "rust bins fresh" in out
    assert "built at HEAD deadbeef9999" in out
    assert "build provenance" in out
    # The retired apples-to-oranges framing must be gone.
    assert "source crates/ rev" not in out
    assert "self-reports rev" not in out


# --- x-c267: mux front-door health (advisory) ---


@pytest.mark.parametrize(
    "mux, which_fno, probe, expected",
    [
        (None, None, False, "not-installed"),
        # `fno` on PATH but not a mux (probe says no) + no cargo mux -> not-installed
        (None, "/home/x/.local/bin/fno", False, "not-installed"),
        # custom --root mux: not at $CARGO_HOME/bin, but `fno` on PATH answers the
        # mux verb -> active (this is the case the old code mislabeled not-installed)
        (None, "/custom/root/bin/fno", True, "active"),
        # `fno` on PATH IS the cargo mux -> active (matched by path, probe not needed)
        ("/home/x/.cargo/bin/fno", "/home/x/.cargo/bin/fno", False, "active"),
        # cargo mux installed but a non-mux `fno` wins PATH -> shadowed
        ("/home/x/.cargo/bin/fno", "/home/x/.local/bin/fno", False, "shadowed"),
        # cargo mux installed but off PATH -> shadowed
        ("/home/x/.cargo/bin/fno", None, False, "shadowed"),
    ],
)
def test_mux_front_door_report_states(
    monkeypatch: pytest.MonkeyPatch, mux, which_fno, probe, expected
) -> None:
    """Front-door state: active when `fno` on PATH is the mux (== cargo binary OR
    answers the mux verb, catching custom --root); shadowed when a cargo mux
    exists but isn't the `fno` on PATH; not-installed otherwise."""
    monkeypatch.setattr(doctor, "_cargo_installed_mux", lambda: Path(mux) if mux else None)
    monkeypatch.setattr(doctor.shutil, "which", lambda name: which_fno)
    monkeypatch.setattr(doctor, "_probe_is_mux", lambda p: probe)
    report = doctor._mux_front_door_report()
    assert report["mux_front_door"] == expected
    assert report["mux_binary"] == (mux if mux else None)
    assert report["path_fno"] == which_fno


# ---------------------------------------------------------------------------
# Orphan-file report (Group 3 GC)
# ---------------------------------------------------------------------------


def test_orphan_report_empty_on_clean_machine(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(doctor.Path, "home", classmethod(lambda cls: tmp_path / "home"))
    (tmp_path / "project").mkdir()
    monkeypatch.chdir(tmp_path / "project")
    assert doctor._orphan_report() == []


def test_orphan_report_finds_leftover_files_in_both_dirs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    home = tmp_path / "home"
    project = tmp_path / "project"
    (home / ".fno").mkdir(parents=True)
    (project / ".fno").mkdir(parents=True)
    (home / ".fno" / "convo-signals.jsonl").write_text("")
    (home / ".fno" / "tasks.json").write_text("")
    (project / ".fno" / "convo-signals.jsonl").write_text("")

    monkeypatch.setattr(doctor.Path, "home", classmethod(lambda cls: home))
    monkeypatch.chdir(project)

    report = doctor._orphan_report()
    assert str(home / ".fno" / "convo-signals.jsonl") in report
    assert str(home / ".fno" / "tasks.json") in report
    assert str(project / ".fno" / "convo-signals.jsonl") in report
    assert len(report) == 3


def test_orphan_report_degrades_on_unresolvable_cwd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A deleted-out-from-under-us cwd (e.g. an archived worktree) must not
    crash the whole `fno doctor` invocation - just skip that dir."""
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    (home / ".fno" / "convo-signals.jsonl").write_text("")

    monkeypatch.setattr(doctor.Path, "home", classmethod(lambda cls: home))
    monkeypatch.setattr(
        doctor.Path, "cwd", classmethod(lambda cls: (_ for _ in ()).throw(OSError("gone")))
    )

    report = doctor._orphan_report()
    assert str(home / ".fno" / "convo-signals.jsonl") in report
