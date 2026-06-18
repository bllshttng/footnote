"""Megatron Phase 3 Task 3.3: fno megatron CLI tests."""
from __future__ import annotations

import textwrap
from pathlib import Path

import pytest
from typer.testing import CliRunner


# ---------------------------------------------------------------------------
# Task 4.2: fno megatron retro
# ---------------------------------------------------------------------------


def _make_retro_fleet_dir(
    base: Path,
    slug: str,
    mission_id: str,
    status: str = "complete",
    write_artifact: bool = True,
) -> Path:
    """Set up a minimal fleet dir for retro tests."""
    fleet = base / slug
    fleet.mkdir(parents=True)
    state = fleet / "state.md"
    state.write_text(
        f"---\n"
        f"mission_id: {mission_id}\n"
        f"status: {status}\n"
        f"created_at: 2026-05-13T10:00:00Z\n"
        f"sent_msg_ids: {{}}\n"
        f"received_completes: []\n"
        f"---\n",
        encoding="utf-8",
    )
    if write_artifact:
        artifact = fleet / f"mission-complete-{mission_id}.md"
        artifact.write_text(
            f"---\nmission_id: {mission_id}\nstatus: {status}\n---\n\n"
            f"# Mission complete: {mission_id}\n\nSome forensic body.\n",
            encoding="utf-8",
        )
    return fleet


def test_retro_happy_path(tmp_path, monkeypatch):
    """AC1-HP: retro prints artifact to stdout, exits 0 for a complete mission."""
    from fno.megatron import cli as cli_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)

    mission_id = "ab-retro001"
    fleet_dir = _make_retro_fleet_dir(fleet_root, "2026-05-13-retro-test", mission_id)

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["retro", mission_id])
    assert result.exit_code == 0, result.output
    assert "Mission complete" in result.output
    assert mission_id in result.output


def test_retro_incomplete_mission(tmp_path, monkeypatch):
    """AC2-ERR: retro exits 4 when mission status is not 'complete'."""
    from fno.megatron import cli as cli_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)

    mission_id = "ab-retro002"
    _make_retro_fleet_dir(
        fleet_root,
        "2026-05-13-retro-incomplete",
        mission_id,
        status="running",
        write_artifact=False,
    )

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["retro", mission_id])
    assert result.exit_code == 4
    combined = result.output + (result.stderr if hasattr(result, "stderr") else "")
    assert "running" in combined


def test_retro_unknown_mission(tmp_path, monkeypatch):
    """AC3-ERR: retro exits 2 when no fleet dir exists for the given mission id."""
    from fno.megatron import cli as cli_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["retro", "ab-no-such-mission"])
    assert result.exit_code == 2
    combined = result.output + (result.stderr if hasattr(result, "stderr") else "")
    assert "not found" in combined


def _make_fleet_dir(home: Path, slug: str, mission_id: str, status: str = "running") -> Path:
    fleet = home / ".fno" / "fleet" / slug
    fleet.mkdir(parents=True)
    state = fleet / "state.md"
    state.write_text(
        f"---\n"
        f"mission_id: {mission_id}\n"
        f"status: {status}\n"
        f"created_at: 2026-05-06T13:00:00Z\n"
        f"sent_msg_ids: {{}}\n"
        f"received_completes: []\n"
        f"---\n",
        encoding="utf-8",
    )
    manifest = fleet / "00-INDEX.md"
    manifest.write_text(
        textwrap.dedent(
            f"""
            ---
            mission_type: fleet
            mission_id: {mission_id}
            waves:
              - wave: 1
                mode: sequential
                projects:
                  - name: backend
                    body: "x"
            ---
            """
        ).lstrip(),
        encoding="utf-8",
    )
    return fleet


def test_cli_status_shows_progress(tmp_path, monkeypatch):
    from fno.megatron.cli import app as megatron_app

    monkeypatch.setenv("HOME", str(tmp_path))
    _make_fleet_dir(tmp_path, "test-mission", "ab-cli0001")

    runner = CliRunner()
    result = runner.invoke(megatron_app, ["status", "ab-cli0001"])
    assert result.exit_code == 0, result.output
    assert "ab-cli0001" in result.output
    assert "running" in result.output


def test_cli_status_unknown_mission_errors(tmp_path, monkeypatch):
    from fno.megatron.cli import app as megatron_app

    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / ".fno" / "fleet").mkdir(parents=True)

    runner = CliRunner()
    result = runner.invoke(megatron_app, ["status", "ab-nope"])
    assert result.exit_code == 2
    assert "not found" in (result.output + (result.stderr if hasattr(result, "stderr") else ""))


def test_cli_cancel_writes_sentinel_and_status(tmp_path, monkeypatch):
    from fno.megatron import read_state
    from fno.megatron.cli import app as megatron_app

    monkeypatch.setenv("HOME", str(tmp_path))
    fleet = _make_fleet_dir(tmp_path, "cancel-mission", "ab-cnc0001")

    runner = CliRunner()
    result = runner.invoke(megatron_app, ["cancel", "ab-cnc0001"])
    assert result.exit_code == 0, result.output

    sentinel = fleet / ".cancelled"
    assert sentinel.exists(), "cancel sentinel must be created"
    state = read_state(fleet / "state.md")
    assert state.status == "cancelled"


def test_cli_cancel_against_failed_mission_does_not_raise(tmp_path, monkeypatch):
    """BUG-MT-002: TERMINAL_STATUSES drift.

    `fno megatron cancel <failed-mission>` previously hardcoded the
    terminal-status tuple to ("complete", "cancelled") and omitted
    "failed". The cancel command would touch the sentinel and then call
    update_status("cancelled"), which raised MissionStateRegression
    because failed -> cancelled is forbidden in _ALLOWED_TRANSITIONS.
    Fix: gate on TERMINAL_STATUSES (the single source of truth).
    """
    from fno.megatron import read_state
    from fno.megatron.cli import app as megatron_app

    monkeypatch.setenv("HOME", str(tmp_path))
    fleet = _make_fleet_dir(tmp_path, "failed-mission", "ab-fld0001", status="failed")

    runner = CliRunner()
    result = runner.invoke(megatron_app, ["cancel", "ab-fld0001"])
    # Should exit cleanly; sentinel still touched; status stays "failed".
    assert result.exit_code == 0, result.output
    assert (fleet / ".cancelled").exists(), "sentinel still written for failed mission"
    state = read_state(fleet / "state.md")
    assert state.status == "failed", "must not transition failed -> cancelled"


def test_cli_list_shows_missions_table(tmp_path, monkeypatch):
    from fno.megatron.cli import app as megatron_app

    monkeypatch.setenv("HOME", str(tmp_path))
    _make_fleet_dir(tmp_path, "alpha-mission", "ab-aaaa", status="running")
    _make_fleet_dir(tmp_path, "bravo-mission", "ab-bbbb", status="paused")

    runner = CliRunner()
    result = runner.invoke(megatron_app, ["list"])
    assert result.exit_code == 0, result.output
    assert "ab-aaaa" in result.output
    assert "ab-bbbb" in result.output
    assert "running" in result.output
    assert "paused" in result.output


def test_cli_run_execs_unified_loop(tmp_path, monkeypatch):
    """`fno megatron run` is a strangler front door: it execs the Rust loop
    (`fno-agents loop run --driver megatron --mission <id>`) instead of the
    deleted Python commander poll loop (group 3, ab-9fd662c6)."""
    import subprocess

    from fno.megatron import cli as cli_mod

    monkeypatch.setenv("HOME", str(tmp_path))
    _make_fleet_dir(tmp_path, "run-mission", "ab-run0001")

    fake_bin = tmp_path / "fno-agents"
    monkeypatch.setattr(cli_mod, "_resolve_loop_binary", lambda: fake_bin)

    recorded: dict = {}
    real_run = subprocess.run

    class _Done:
        returncode = 0

    def fake_run(cmd, *args, **kwargs):
        # Only intercept the loop launch; unrelated subprocess users
        # (e.g. resolve_repo_root's git rev-parse) pass through.
        if cmd and cmd[0] == str(fake_bin):
            recorded["cmd"] = cmd
            return _Done()
        return real_run(cmd, *args, **kwargs)

    monkeypatch.setattr(subprocess, "run", fake_run)

    runner = CliRunner()
    result = runner.invoke(
        cli_mod.app,
        ["run", "ab-run0001", "--max-iterations", "3", "--poll-interval", "0.001"],
    )

    assert result.exit_code == 0, result.output
    assert recorded["cmd"][0] == str(fake_bin)
    assert recorded["cmd"][1:5] == ["loop", "run", "--driver", "megatron"]
    assert "--mission" in recorded["cmd"] and "ab-run0001" in recorded["cmd"]
    assert "--max-iterations" in recorded["cmd"] and "3" in recorded["cmd"]
    # codex P2: the wrapper resolves and passes the driver-lib dir so the
    # documented command works from any cwd (test runs inside a checkout,
    # so the source-relative candidate resolves).
    assert "--driver-lib-dir" in recorded["cmd"]
    # The deprecated --poll-interval is accepted with a notice, never passed on.
    assert "--poll-interval" not in recorded["cmd"]
    assert "Mission ab-run0001 complete." in result.output


def test_cli_run_maps_paused_exit_code(tmp_path, monkeypatch):
    """Rust exit 4 (paused) surfaces as exit 4 with an actionable message."""
    import subprocess

    from fno.megatron import cli as cli_mod

    monkeypatch.setenv("HOME", str(tmp_path))
    _make_fleet_dir(tmp_path, "run-mission", "ab-run0002")
    fake_bin = tmp_path / "fno-agents"
    monkeypatch.setattr(cli_mod, "_resolve_loop_binary", lambda: fake_bin)
    real_run = subprocess.run

    class _Paused:
        returncode = 4

    def fake_run(cmd, *args, **kwargs):
        if cmd and cmd[0] == str(fake_bin):
            return _Paused()
        return real_run(cmd, *args, **kwargs)

    monkeypatch.setattr(subprocess, "run", fake_run)

    result = CliRunner().invoke(cli_mod.app, ["run", "ab-run0002"])

    assert result.exit_code == 4
    combined = result.output + (result.stderr or "")
    assert "paused" in combined


def test_cli_run_missing_binary_is_actionable(tmp_path, monkeypatch):
    """No fno-agents binary -> exit 2 with the install hint, no crash."""
    from fno.megatron import cli as cli_mod

    monkeypatch.setenv("HOME", str(tmp_path))
    _make_fleet_dir(tmp_path, "run-mission", "ab-run0003")
    monkeypatch.setattr(cli_mod, "_resolve_loop_binary", lambda: None)

    result = CliRunner().invoke(cli_mod.app, ["run", "ab-run0003"])

    assert result.exit_code == 2
    combined = result.output + (result.stderr or "")
    assert "fno update --rust" in combined


# ---------------------------------------------------------------------------
# Task: fno megatron reconcile (ab-b80df463)
# ---------------------------------------------------------------------------


def _make_reconcile_fleet_dir(
    base: Path,
    slug: str,
    mission_id: str,
    *,
    projects: list[str] = ("alpha", "beta"),
    waves: int = 1,
    seed_completions_for: list[tuple[int, str]] | None = None,
) -> Path:
    """Set up a fleet dir with manifest + state for reconcile tests."""
    fleet = base / slug
    fleet.mkdir(parents=True)

    wave_blocks = []
    for w in range(1, waves + 1):
        proj_lines = "\n".join(f"      - {{name: {p}, body: x}}" for p in projects)
        wave_blocks.append(f"  - wave: {w}\n    mode: parallel\n    projects:\n{proj_lines}")
    waves_yaml = "\n".join(wave_blocks)

    (fleet / "00-INDEX.md").write_text(
        f"---\nmission_type: fleet\nmission_id: {mission_id}\nwaves:\n{waves_yaml}\n---\n",
        encoding="utf-8",
    )
    (fleet / "state.md").write_text(
        f"---\n"
        f"mission_id: {mission_id}\n"
        f"status: running\n"
        f"created_at: 2026-05-13T10:00:00Z\n"
        f"sent_msg_ids: {{}}\n"
        f"received_completes: []\n"
        f"---\n",
        encoding="utf-8",
    )

    for (wave, project) in seed_completions_for or []:
        d = fleet / "completions" / f"wave-{wave}"
        d.mkdir(parents=True, exist_ok=True)
        (d / f"{project}.json").write_text(f'{{"project":"{project}","wave":{wave},"mission_id":"x"}}')

    return fleet


def test_reconcile_unknown_mission_exits_2(tmp_path, monkeypatch):
    """AC1-ERR: unknown mission id -> exit 2."""
    from fno.megatron import cli as cli_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["reconcile", "ab-nope0001"])
    assert result.exit_code == 2


def test_reconcile_clean_mission_exits_0(tmp_path, monkeypatch):
    """AC5-HP: all completions present -> exit 0, prose says 'No drift'."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)

    mission_id = "ab-rcclean1"
    _make_reconcile_fleet_dir(
        fleet_root,
        "2026-05-13-rc-clean",
        mission_id,
        projects=["alpha", "beta"],
        seed_completions_for=[(1, "alpha"), (1, "beta")],
    )

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["reconcile", mission_id])
    assert result.exit_code == 0, result.output
    assert "No drift detected." in result.output


def test_reconcile_drift_exits_4(tmp_path, monkeypatch):
    """AC1-HP: drift detected -> exit 4 in read-only mode."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)
    # Stub the network call so no gh shell-out happens.
    monkeypatch.setattr(rc_mod, "query_pr_state", lambda p, b: [])

    mission_id = "ab-rcdrift1"
    _make_reconcile_fleet_dir(
        fleet_root,
        "2026-05-13-rc-drift",
        mission_id,
        projects=["alpha", "beta"],
        seed_completions_for=[(1, "alpha")],  # missing beta
    )

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["reconcile", mission_id])
    assert result.exit_code == 4, result.output
    assert "beta" in result.output
    assert "missing-no-pr" in result.output


def test_reconcile_backfill_resolves_drift(tmp_path, monkeypatch):
    """AC2-HP: --backfill writes the missing JSON for a merged PR and exits 0."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)

    def stub_query(project, branch):
        if project == "beta":
            return [
                rc_mod.PrState(
                    number=42,
                    url="https://github.com/x/y/pull/42",
                    state="MERGED",
                    merged_at="2026-05-13T20:00:00Z",
                    merge_commit_sha="abc1234",
                )
            ]
        return []

    monkeypatch.setattr(rc_mod, "query_pr_state", stub_query)

    mission_id = "ab-rcback01"
    fleet_dir = _make_reconcile_fleet_dir(
        fleet_root,
        "2026-05-13-rc-backfill",
        mission_id,
        projects=["alpha", "beta"],
        seed_completions_for=[(1, "alpha")],
    )

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["reconcile", mission_id, "--backfill"])
    assert result.exit_code == 0, result.output

    backfilled = fleet_dir / "completions" / "wave-1" / "beta.json"
    assert backfilled.exists()
    import json as _json

    payload = _json.loads(backfilled.read_text())
    assert payload["source"] == "reconcile-backfill"
    assert payload["commit_sha"] == "abc1234"


def test_reconcile_json_output_is_parseable(tmp_path, monkeypatch):
    """--json emits structured JSON to stdout."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod
    import json as _json

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)
    monkeypatch.setattr(rc_mod, "query_pr_state", lambda p, b: [])

    mission_id = "ab-rcjson01"
    _make_reconcile_fleet_dir(
        fleet_root,
        "2026-05-13-rc-json",
        mission_id,
        projects=["alpha"],
        seed_completions_for=[],
    )

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["reconcile", mission_id, "--json"])
    # Exit 4 because there's drift, but stdout must still be JSON.
    assert result.exit_code == 4
    parsed = _json.loads(result.output)
    assert parsed["mission_id"] == mission_id
    assert isinstance(parsed["drift"], list)


def test_reconcile_pr_zero_rejected_with_helpful_error(tmp_path, monkeypatch):
    """--pr 0 is invalid (1-indexed)."""
    from fno.megatron import cli as cli_mod

    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: tmp_path)
    runner = CliRunner()
    result = runner.invoke(
        cli_mod.app, ["reconcile", "ab-anything", "--backfill", "--pr", "0"]
    )
    assert result.exit_code == 2
    combined = result.output + (result.stderr if hasattr(result, "stderr") else "")
    assert "1-indexed" in combined


def test_reconcile_backfill_ambiguous_without_pr_emits_actionable_skip(
    tmp_path, monkeypatch
):
    """Ambiguous record (>1 candidate) without --pr surfaces a clear skip reason."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)

    def stub_query(project, branch):
        return [
            rc_mod.PrState(
                number=1,
                url="https://github.com/x/y/pull/1",
                state="MERGED",
                merged_at="t1",
                merge_commit_sha="a",
            ),
            rc_mod.PrState(
                number=2,
                url="https://github.com/x/y/pull/2",
                state="MERGED",
                merged_at="t2",
                merge_commit_sha="b",
            ),
        ]

    monkeypatch.setattr(rc_mod, "query_pr_state", stub_query)

    mission_id = "ab-rcamb01"
    _make_reconcile_fleet_dir(
        fleet_root,
        "2026-05-13-rc-ambig",
        mission_id,
        projects=["alpha"],
        seed_completions_for=[],
    )

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["reconcile", mission_id, "--backfill"])
    assert result.exit_code == 4
    assert "pass --pr" in result.output


def test_reconcile_backfill_ambiguous_with_pr_writes_chosen(tmp_path, monkeypatch):
    """--pr 2 picks the second candidate of an ambiguous record."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod
    import json as _json

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)

    def stub_query(project, branch):
        return [
            rc_mod.PrState(
                number=1,
                url="https://github.com/x/y/pull/1",
                state="MERGED",
                merged_at="t1",
                merge_commit_sha="first",
            ),
            rc_mod.PrState(
                number=2,
                url="https://github.com/x/y/pull/2",
                state="MERGED",
                merged_at="t2",
                merge_commit_sha="second",
            ),
        ]

    monkeypatch.setattr(rc_mod, "query_pr_state", stub_query)

    mission_id = "ab-rcamb02"
    fleet_dir = _make_reconcile_fleet_dir(
        fleet_root,
        "2026-05-13-rc-ambig2",
        mission_id,
        projects=["alpha"],
        seed_completions_for=[],
    )

    runner = CliRunner()
    result = runner.invoke(
        cli_mod.app,
        ["reconcile", mission_id, "--backfill", "--pr", "2"],
    )
    assert result.exit_code == 0, result.output

    backfilled = fleet_dir / "completions" / "wave-1" / "alpha.json"
    assert backfilled.exists()
    payload = _json.loads(backfilled.read_text())
    assert payload["commit_sha"] == "second"
    assert payload["pr_url"] == "https://github.com/x/y/pull/2"


def test_reconcile_exits_3_when_gh_missing(tmp_path, monkeypatch):
    """Scan-level auth failure: gh CLI not on PATH -> exit 3 (distinct from per-record query failure)."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)
    # Make `gh` look absent from the CLI's perspective.
    import shutil as _shutil
    monkeypatch.setattr(_shutil, "which", lambda name: None if name == "gh" else "/usr/bin/" + name)

    mission_id = "ab-rcnogh1"
    _make_reconcile_fleet_dir(
        fleet_root, "2026-05-13-rc-nogh", mission_id, projects=["alpha"],
    )

    runner = CliRunner()
    result = runner.invoke(cli_mod.app, ["reconcile", mission_id])
    assert result.exit_code == 3, result.output


def test_reconcile_pr_choice_ignored_for_single_candidate_records(tmp_path, monkeypatch):
    """--pr 2 must succeed when a record has only one candidate (no out-of-range)."""
    import fno.megatron.cli as cli_mod
    import fno.megatron.reconcile as rc_mod
    import json as _json

    fleet_root = tmp_path / "fleet"
    fleet_root.mkdir()
    monkeypatch.setattr(cli_mod, "_fleet_root", lambda: fleet_root)
    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)

    def stub_query(project, branch, **kwargs):
        return [
            rc_mod.PrState(
                number=99, url="u", state="MERGED",
                merged_at="2026-05-14T07:00:00Z", merge_commit_sha="only",
            )
        ]

    monkeypatch.setattr(rc_mod, "query_pr_state", stub_query)

    mission_id = "ab-rcsngl1"
    fleet_dir = _make_reconcile_fleet_dir(
        fleet_root, "2026-05-13-rc-single", mission_id, projects=["alpha"],
    )

    runner = CliRunner()
    result = runner.invoke(
        cli_mod.app, ["reconcile", mission_id, "--backfill", "--pr", "2"],
    )
    assert result.exit_code == 0, result.output
    payload = _json.loads(
        (fleet_dir / "completions" / "wave-1" / "alpha.json").read_text()
    )
    assert payload["commit_sha"] == "only"
