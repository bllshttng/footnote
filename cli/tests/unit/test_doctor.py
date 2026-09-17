"""Unit tests for `fno doctor` (ab-5a1fc285 + ab-a78c9731).

Covers US1 (detection: fresh / stale / unknown / json / no-source / probe-error),
US3-adjacent --fix behavior (delegates to `fno doctor update`, honors the IN_PROGRESS
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
import os
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import doctor
from fno.cli import app

# Forces the real `update_command` into the lazily-built `doctor update` Click
# command NOW, before any test below monkeypatches `update.update_command`.
# `doctor_cli.py` binds that reference once, at its own first import, so a
# monkeypatch active during a LATER first import bakes the fake into the
# command for the rest of this worker process - a cross-test poison, not a
# module attribute a fresh read would pick up.
import fno.doctor_cli  # noqa: F401

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
    deployed_config_keys: frozenset[str] | None = frozenset({"backlog.id_prefix"}),
    source_config_keys: frozenset[str] | None = frozenset({"backlog.id_prefix"}),
    content_drift: int | None = 0,
    source_checkout_sync: dict | None = None,
    components: list[dict] | None = None,
) -> None:
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: src)
    # Content-drift ground truth. Default 0 = "check ran, byte-identical" so a
    # fresh-marker test stays fresh; drift/indeterminate tests set >0 or None.
    # Never hashes the real installed package (hermetic).
    monkeypatch.setattr(doctor, "_python_content_drift", lambda source: content_drift)
    monkeypatch.setattr(doctor, "_source_rev", lambda source: source_rev)
    monkeypatch.setattr(doctor, "_read_marker", lambda: marker)
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: capture_present)
    # Config-schema surfaces (x-6c5b): default to EQUAL keysets so existing tests
    # exercise no drift; drift tests pass differing sets explicitly.
    monkeypatch.setattr(doctor, "_deployed_config_keys", lambda: deployed_config_keys)
    monkeypatch.setattr(doctor, "_source_config_keys", lambda source: source_config_keys)
    monkeypatch.setattr(
        doctor,
        "_source_checkout_sync",
        lambda source: source_checkout_sync or {
            "status": "current",
            "behind": 0,
            "source_head": "abc123",
            "remote_head": "abc123",
            "detail": "",
        },
    )
    # Post ab-716cd330 `revision` carries the binary's self-reported crates/ rev
    # (the verdict driver), not the installed-rust-rev marker. `rust_marker` is
    # the value the resolved binary reports here.
    monkeypatch.setattr(
        doctor,
        "_rust_report",
        lambda: {"binary": rust_binary, "revision": rust_marker},
    )
    monkeypatch.setattr(doctor, "_daemon_drift_warning", lambda: None)
    monkeypatch.setattr(doctor, "_read_rust_marker", lambda: rust_marker)
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: rust_source_rev)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: cargo_bin_present)
    # Component convergence (deployed-shape probes + native verdict): default
    # to "no evidence" so existing verdict tests keep their exact surface;
    # component tests pass rows explicitly.
    monkeypatch.setattr(
        doctor, "_component_convergence", lambda src, rust, marker, drift: components or []
    )
    # Agent health (x-1c7b): default to a quiet, healthy machine. Left unstubbed
    # these shell out to the real `launchctl` and read the real claims root, so
    # every verdict test would inherit the developer's own dead agents.
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "ran", "hours": 3.0, "stale": False, "agent_installed": True},
    )
    monkeypatch.setattr(
        doctor, "_launch_agent_failures", lambda: {"applicable": True, "dead": []}
    )
    # mux-server freshness probe shells out to `fno mux ls --json` and only
    # short-circuits when no mux is running. A developer machine with a live mux
    # (or a sibling bg session) trips the subprocess tripwire below for a reason
    # unrelated to the verdict under test, so stub it like the other probes.
    from fno import update

    monkeypatch.setattr(update, "stale_mux_servers", lambda: [])
    # Control-plane arm staleness shells out to the real `fno-agents status
    # --json`. A developer machine with genuinely stale arms leaks STALE lines
    # into full-output assertions, so stub it like the other probes.
    monkeypatch.setattr(
        doctor,
        "_control_plane_arms_report",
        lambda: {"stale": [], "unknown_reason": None},
    )
    # Evals demand: build_report reads the real evals history through the
    # summary seam; pin it fresh and quiet so a developer machine's stale bank
    # cannot leak a STALE line into full-output assertions. Eval-specific tests
    # patch the seam again via _patch_evals_summary.
    monkeypatch.setattr("fno.paths.evals_history", lambda: Path("/evals/h.jsonl"))
    monkeypatch.setattr(
        "fno.evals.report.evals_health_summary",
        lambda _path, **_kw: {
            "regression_pass_rate": 1.0,
            "flake_count": 0,
            "regression_alarm": [],
            "age_days": 1.0,
            "stale": False,
            "never_ran": False,
        },
    )


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
    assert "fno doctor update" in result.stdout


def _fake_native_sync(payload):
    """Capture the sync argv and answer with the given native payload."""
    captured: dict = {}

    def fake(subcommand, extra=None, runner=None, input_text=None):
        captured["sub"] = subcommand
        captured["extra"] = list(extra or [])
        return payload

    return fake, captured


def test_source_checkout_sync_maps_native_payload(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    from fno import update

    source = tmp_path / "cli"
    source.mkdir()
    payload = {
        "status": "behind",
        "behind": 117,
        "source_head": "local",
        "remote_head": "remote",
        "detail": "",
    }
    fake, captured = _fake_native_sync(payload)
    monkeypatch.setattr(update, "_source_pin_call", fake)

    report = doctor._source_checkout_sync(source)

    assert report == {"status": "behind", "behind": 117, "source_head": "local", "remote_head": "remote", "detail": ""}
    assert captured["sub"] == "sync"
    assert captured["extra"] == ["--source", str(source)]


def test_source_checkout_sync_degrades_without_native_answer(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    from fno import update

    source = tmp_path / "cli"
    source.mkdir()
    fake, _captured = _fake_native_sync(None)
    monkeypatch.setattr(update, "_source_pin_call", fake)

    report = doctor._source_checkout_sync(source)

    assert report["status"] == "unknown"
    assert report["behind"] is None
    assert "source-pin helper" in report["detail"]


def test_source_checkout_behind_is_a_doctor_blocker() -> None:
    blockers = doctor._blockers(
        {
            "status": "fresh",
            "source_checkout_sync": {
                "status": "behind",
                "behind": 117,
                "source_head": "local",
                "remote_head": "remote",
            },
        }
    )

    assert len(blockers) == 1
    assert "117 commits behind origin/main" in blockers[0]
    assert "freshness is relative" in blockers[0]


def test_source_checkout_behind_makes_doctor_exit_nonzero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        source_checkout_sync={
            "status": "behind",
            "behind": 117,
            "source_head": "local",
            "remote_head": "remote",
            "detail": "",
        },
    )

    result = runner.invoke(app, ["doctor", "--json"])

    assert result.exit_code == 1
    payload = json.loads(result.stdout)
    assert payload["status"] == "fresh"
    assert payload["source_checkout_sync"]["status"] == "behind"
    assert payload["source_checkout_sync"]["behind"] == 117
    assert "source checkout" in result.stderr


def test_build_report_checks_source_sync_after_post_merge_refresh(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    events: list[str] = []
    source = Path("/src")

    monkeypatch.setattr(doctor, "_resolve_source", lambda _: source)
    monkeypatch.setattr(doctor, "_source_rev", lambda _: "source")
    monkeypatch.setattr(doctor, "_read_marker", lambda: "source")
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(doctor, "_rust_report", lambda: {"binary": None, "revision": None})
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda _: None)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)
    monkeypatch.setattr(doctor, "_deployed_config_keys", lambda: frozenset())
    monkeypatch.setattr(doctor, "_source_config_keys", lambda _: frozenset())
    monkeypatch.setattr(doctor, "_python_content_drift", lambda _: 0)
    monkeypatch.setattr(doctor, "_verdict", lambda **_: {"status": "fresh"})
    monkeypatch.setattr(
        doctor,
        "_post_merge_sync_health",
        lambda: events.append("post-merge") or {},
    )
    monkeypatch.setattr(
        doctor,
        "_source_checkout_sync",
        lambda _: events.append("source-sync") or {"status": "current", "behind": 0},
    )
    for name in (
        "_mux_front_door_report",
        "_daemon_drift_warning",
        "_orphan_report",
        "_pr_watch_liveness",
        "_fd_limit_report",
        "_dead_letter_report",
        "_codex_app_server_report",
        "_managed_block_report",
        "_harness_surface_report",
        "_plugin_hooks_launch_report",
        "_pre_push_hook_report",
        "_groom_health",
        "_launch_agent_failures",
        "_plugin_cache_report",
        "_silent_switch_report",
        "_auto_merge_review_gap",
    ):
        monkeypatch.setattr(doctor, name, lambda *args, **kwargs: {})
    monkeypatch.setattr(doctor, "_auto_merge_armed_manifests", lambda: [])

    from fno import update

    monkeypatch.setattr(update, "stale_mux_servers", lambda: [])

    doctor.build_report(source)

    assert events == ["post-merge", "source-sync"]


def test_source_checkout_behind_refuses_fix_before_repair(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="source",
        marker="installed",
        capture_present="present",
        source_checkout_sync={
            "status": "behind",
            "behind": 117,
            "source_head": "source",
            "remote_head": "remote",
            "detail": "",
        },
    )
    monkeypatch.setattr(doctor, "_post_merge_sync_health", lambda: {})

    from fno import update

    def tripwire(*args, **kwargs):
        raise AssertionError("repair must not run from a stale source checkout")

    monkeypatch.setattr(update, "update_command", tripwire)

    result = runner.invoke(app, ["doctor", "--fix"])

    assert result.exit_code == 1
    assert "behind origin/main" in result.stdout
    assert "refused" in result.stderr


# ---------------------------------------------------------------------------
# x-3248 Change 5: per-harness surface freshness / dedupe
# ---------------------------------------------------------------------------

_MARKETPLACE_LIST_ONE = """\
MARKETPLACE             ROOT
openai-bundled          /Users/x/.codex/.tmp/bundled-marketplaces/openai-bundled
footnote-local          /Users/x/code/footnote/footnote
"""

_MARKETPLACE_LIST_DUP = """\
MARKETPLACE             ROOT
footnote-local          /Users/x/code/footnote/footnote
footnote                /Users/x/.codex/.tmp/footnote-clone
"""


def test_codex_marketplace_duplicates_pure_parser() -> None:
    # A single legitimate registration is not a duplicate; the header row and
    # foreign marketplaces never match.
    assert doctor._codex_marketplace_duplicates(_MARKETPLACE_LIST_ONE) == []
    assert doctor._codex_marketplace_duplicates("") == []
    # Two footnote rows -> both names reported.
    assert doctor._codex_marketplace_duplicates(_MARKETPLACE_LIST_DUP) == [
        "footnote-local",
        "footnote",
    ]


def test_ac5_doctor_names_codex_duplicate_and_dedupe_action(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC5: codex footnote registered twice -> doctor names the duplicate and
    the dedupe action instead of staying silent."""
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor, "_harness_surface_report",
        lambda: {"codex_marketplace_duplicates": ["footnote-local", "footnote"]},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "registered 2 times" in result.stdout
    assert "footnote-local, footnote" in result.stdout
    assert "codex plugin marketplace remove" in result.stdout


def test_doctor_reports_stale_opencode_plugin(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor, "_harness_surface_report", lambda: {"opencode": "stale"}
    )
    result = runner.invoke(app, ["doctor"])
    assert "opencode footnote plugin is STALE" in result.stdout
    assert "fno config setup" in result.stdout


def test_doctor_reports_stale_surface_via_door(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    """A stale receipt (version drift) becomes a named opencode advisory."""
    (tmp_path / "oc" / "plugins").mkdir(parents=True)
    monkeypatch.setenv("OPENCODE_CONFIG_DIR", str(tmp_path / "oc"))
    monkeypatch.setattr(
        "fno.rust_binary.call_binary_json",
        lambda verb, args, **kw: (
            None,
            {
                "status": "stale",
                "version": "0.3.1",
                "source_version": "0.3.2",
                "missing": [],
            },
        ),
    )
    report = doctor._harness_surface_report()
    assert "STALE" in report["opencode"]
    assert "0.3.2" in report["opencode"]


def test_doctor_main_run_points_at_codex_hooks_dual(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC4: a plain `fno doctor` (not just --codex-hooks) surfaces codex hooks
    dual-representation and points at the heal verb."""
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor, "_harness_surface_report", lambda: {"codex_hooks_dual": True}
    )
    result = runner.invoke(app, ["doctor"])
    assert "codex hooks load from both" in result.stdout
    assert "--migrate-legacy-hooks-json" in result.stdout


def test_doctor_quiet_when_surfaces_healthy(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(doctor, "_harness_surface_report", lambda: {})
    result = runner.invoke(app, ["doctor"])
    assert "opencode" not in result.stdout
    assert "marketplace" not in result.stdout


def test_harness_surface_is_quiet_when_codex_and_footnote_state_are_absent(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(doctor.shutil, "which", lambda name: None if name == "codex" else "/bin/tool")
    monkeypatch.setenv("CODEX_HOME", str(tmp_path / "codex-home"))
    monkeypatch.setattr(
        "fno.setup.codex_plugin.inspect_freshness",
        lambda: pytest.fail("plugin inspection should not run without Codex or Footnote state"),
    )
    monkeypatch.setattr(doctor, "_codex_hooks_report", lambda: {})
    monkeypatch.setenv("OPENCODE_CONFIG_DIR", str(tmp_path / "no-opencode"))

    assert "codex_plugin" not in doctor._harness_surface_report()


def test_doctor_reports_plugin_drift_without_staling_cli(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    plugin = {
        "status": "stale",
        "issue": "payload-drift",
        "channel": "dev",
        "source_version": "0.3.0",
        "cache_version": "0.3.0",
        "source_digest": "a" * 64,
        "cache_digest": "b" * 64,
        "enabled_plugin_ids": ["fno@footnote"],
        "remedy": "fno config plugin install codex --force",
    }
    monkeypatch.setattr(
        doctor, "_harness_surface_report", lambda: {"codex_plugin": plugin}
    )

    result = runner.invoke(app, ["doctor", "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["status"] == "fresh"
    assert payload["harness_surface"]["codex_plugin"] == plugin
    assert "codex plugin: STALE" in result.stderr
    assert "fno config plugin install codex --force" in result.stderr


def test_doctor_reports_ambiguous_duplicate_state_without_freshness(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor,
        "_harness_surface_report",
        lambda: {
            "codex_plugin": {
                "status": "conflict",
                "issue": "ambiguous-duplicate-state",
                "enabled_plugin_ids": ["fno@footnote", "fno@footnote-dev"],
                "remedy": "fno config plugin install codex --force",
            }
        },
    )

    result = runner.invoke(app, ["doctor"])

    assert result.exit_code == 0
    assert "codex plugin: CONFLICT" in result.stdout
    assert "fno@footnote, fno@footnote-dev" in result.stdout
    assert "codex plugin: fresh" not in result.stdout.lower()


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


def test_daemon_drift_probe_uses_installed_status_and_relays_canonical_warning(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno import rust_binary

    warning = (
        "fno agents: the running daemon (pid 91627) is an older build than the installed "
        "binary; run `fno agents restart` to pick up the new build (it restarts the "
        "daemon only and keeps PTY workers)."
    )
    calls: list[tuple[list[str], dict]] = []
    monkeypatch.setattr(
        rust_binary, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
    )

    def fake_run(cmd, **kwargs):
        calls.append((cmd, kwargs))
        return type(
            "Completed",
            (),
            {
                "returncode": 0,
                "stdout": '{"drift": "drifted", "daemon": {"pid": 91627}}',
                "stderr": warning,
            },
        )()

    monkeypatch.setattr(doctor.subprocess, "run", fake_run)
    assert doctor._daemon_drift_warning() == warning
    assert calls == [
        (
            ["/cargo/bin/fno-agents", "status", "--json"],
            {"capture_output": True, "text": True, "check": False, "timeout": 5},
        )
    ]


def test_daemon_drift_probe_appends_measured_process_age(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Process age rides beside the artifact verdict - a long-lived daemon
    on pre-fix code must read as lag, not as an unqualified fresh."""
    from fno import rust_binary

    warning = (
        "fno agents: the running daemon (pid 30324) is an older build than the installed "
        "binary; run `fno agents restart` to pick up the new build (it restarts the "
        "daemon only and keeps PTY workers)."
    )
    monkeypatch.setattr(
        rust_binary, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
    )
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda *args, **kwargs: type(
            "Completed",
            (),
            {
                "returncode": 0,
                "stdout": json.dumps(
                    {"drift": "drifted", "daemon": {"pid": 30324, "uptime_secs": 2241}}
                ),
                "stderr": warning,
            },
        )(),
    )
    result = doctor._daemon_drift_warning()
    assert result is not None
    assert result.startswith(warning)
    assert "(daemon up 37m; running its startup build, not this one)" in result


def test_daemon_drift_probe_gates_on_structured_drift_field(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The stderr sentence alone is not the verdict; the JSON ``drift`` field is."""
    from fno import rust_binary

    warning = (
        "fno agents: the running daemon (pid 7) is an older build than the installed "
        "binary; run `fno agents restart` to pick up the new build (it restarts the "
        "daemon only and keeps PTY workers)."
    )
    monkeypatch.setattr(
        rust_binary, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
    )
    for drift_value in ("fresh", "unknown", None):
        payload = {"daemon": {"pid": 7}}
        if drift_value is not None:
            payload["drift"] = drift_value
        monkeypatch.setattr(
            doctor.subprocess,
            "run",
            lambda *args, _p=payload, **kwargs: type(
                "Completed",
                (),
                {
                    "returncode": 0,
                    "stdout": json.dumps(_p),
                    "stderr": warning,
                },
            )(),
        )
        assert doctor._daemon_drift_warning() is None, drift_value


@pytest.mark.parametrize(
    "stderr,expected",
    [
        (
            "fno agents: the running daemon (pid 7) is an older build than the installed "
            "binary; run `fno agents restart` to pick up the new build (it restarts the "
            "daemon only and keeps PTY workers).",
            "relayed",
        ),
        (
            "fno agents: the running daemon (pid 7) is an older build than the installed "
            "binary; `fno agents restart` fixes it but restarts every worker on the shared "
            "daemon, so it is an operator action - surface it to the operator instead of "
            "running it from an agent session.",
            "relayed",
        ),
        (
            "fno agents: something else entirely",
            None,
        ),
    ],
)
def test_daemon_drift_warning_regex_matches_remedy_tails(
    monkeypatch: pytest.MonkeyPatch,
    stderr: str,
    expected: str | None,
) -> None:
    """The relay filter matches the stable state prefix, not the remedy tail;
    the wording may change without silencing the relay that triggers the restart."""
    from fno import rust_binary

    monkeypatch.setattr(
        rust_binary, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
    )
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda *args, **kwargs: type(
            "Completed",
            (),
            {"returncode": 0, "stdout": '{"drift": "drifted", "daemon": {"pid": 7}}', "stderr": stderr},
        )(),
    )
    warning = doctor._daemon_drift_warning()
    if expected is None:
        assert warning is None
    else:
        assert warning == stderr


def test_daemon_drift_probe_uses_forced_runtime_binary(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno import rust_binary

    warning = (
        "fno agents: the running daemon is an older build than the installed binary; "
        "run `fno agents restart` to pick up the new build (it restarts the daemon only "
        "and keeps PTY workers)."
    )
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "rust")
    monkeypatch.setenv("FNO_AGENTS_BIN", "/custom/fno-agents")
    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: Path("/custom/fno-agents"))
    monkeypatch.setattr(
        rust_binary,
        "resolve_installed_binary",
        lambda: (_ for _ in ()).throw(AssertionError("default resolver must not run")),
    )
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda cmd, **kwargs: type(
            "Completed",
            (),
            {"returncode": 0, "stdout": '{"drift": "drifted", "daemon": {}}', "stderr": warning},
        )(),
    )

    assert doctor._daemon_drift_warning() == warning


@pytest.mark.parametrize(
    "returncode,stdout,stderr",
    [
        (0, '{"daemon": {"pid": 7}}', ""),
        (13, "", "fno-agents: daemon not running"),
        (
            0,
            "not json",
            "fno agents: the running daemon (pid 7) is an older build than the installed "
            "binary; run `fno agents restart` to pick up the new build (it restarts the "
            "daemon only and keeps PTY workers).",
        ),
        (1, '{"daemon": {"pid": 7}}', "fno agents: transport failed"),
        (
            0,
            '{"drift": "fresh", "daemon": {"pid": 7}}',
            "fno agents: the running daemon (pid 7) is an older build than the installed "
            "binary; run `fno agents restart` to pick up the new build (it restarts the "
            "daemon only and keeps PTY workers).",
        ),
    ],
)
def test_daemon_drift_probe_fails_silent_without_proven_status(
    monkeypatch: pytest.MonkeyPatch,
    returncode: int,
    stdout: str,
    stderr: str,
) -> None:
    from fno import rust_binary

    monkeypatch.setattr(
        rust_binary, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
    )
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda *args, **kwargs: type(
            "Completed", (), {"returncode": returncode, "stdout": stdout, "stderr": stderr}
        )(),
    )
    assert doctor._daemon_drift_warning() is None


def test_doctor_reports_measured_daemon_drift_without_changing_verdict(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
    )
    warning = (
        "fno agents: the running daemon is an older build than the installed binary; "
        "run `fno agents restart` to pick up the new build (it restarts the daemon only "
        "and keeps PTY workers)."
    )
    monkeypatch.setattr(doctor, "_daemon_drift_warning", lambda: warning)

    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "fno doctor: note: " + warning in result.stdout

    payload = json.loads(runner.invoke(app, ["doctor", "--json"]).stdout)
    assert payload["status"] == "fresh"
    assert payload["daemon_drift"] == warning


def test_daemon_drift_never_prints_unqualified_fresh_component_verdict(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Test clause: with an artifact newer than the running process, doctor
    reports the lag AND the summary line stops reading as bare
    "N/N fresh" - the exact false evidence the incident shipped."""
    warning = (
        "fno agents: the running daemon (pid 30324) is an older build than the installed "
        "binary; run `fno agents restart` to pick up the new build (it restarts the "
        "daemon only and keeps PTY workers)."
    )
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="def",
        rust_source_rev="def",
        cargo_bin_present=True,
        components=[
            {"component": "fno", "status": "fresh"},
            {"component": "fno-agents-daemon", "status": "fresh"},
        ],
    )
    # _stub_signals defaults the probe to None; the drift case overrides it.
    monkeypatch.setattr(doctor, "_daemon_drift_warning", lambda: warning)

    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "fno doctor: note: " + warning in result.stdout
    assert "2/2 fresh on disk; a daemon drift note follows." in result.stdout
    assert "2/2 fresh (" not in result.stdout


# ---------------------------------------------------------------------------
# Python config-schema drift
# ---------------------------------------------------------------------------


def test_config_schema_drift_reports_stale(monkeypatch: pytest.MonkeyPatch) -> None:
    """Deployed FIELD_META missing a key the source defines => stale, exit nonzero,
    names a missing key + remediation. Revs match, so this is caught by the schema
    fingerprint alone (the exact stale-uv-tool symptom)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # rev + verb both look fresh...
        capture_present="present",
        deployed_config_keys=frozenset({"project.id"}),  # ...but the config schema is behind
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "config schema is STALE" in result.stdout
    assert "backlog.id_prefix" in result.stdout
    assert "fno doctor update" in result.stdout


def test_config_schema_drift_shows_rev_delta_when_also_rev_behind(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Overlap case (schema-behind AND rev-behind): the config message leads but the
    rev delta is still surfaced, not dropped."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",  # rev-behind too
        capture_present="present",
        deployed_config_keys=frozenset({"project.id"}),
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "config schema is STALE" in result.stdout
    assert "oldsha" in result.stdout and "newsha" in result.stdout


def test_config_schema_in_sync_is_silent(monkeypatch: pytest.MonkeyPatch) -> None:
    """Matching keysets on an otherwise-fresh install stay silent (no false positive)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        deployed_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" in result.stdout
    assert "config schema" not in result.stdout


def test_config_deployed_ahead_of_source_not_stale(monkeypatch: pytest.MonkeyPatch) -> None:
    """A deployed CLI with MORE keys than source is not drift (don't cry wolf)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        deployed_config_keys=frozenset({"project.id", "backlog.id_prefix", "new.key"}),
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "config schema is STALE" not in result.stdout


_FLAT_REGISTRY = (
    "from dataclasses import dataclass\n"
    "@dataclass\n"
    "class Meta:\n"
    "    doc: str\n"
    'FIELD_META: dict[str, Meta] = {\n'
    '    "project.id": Meta("x"),\n'
    '    "backlog.id_prefix": Meta("y"),\n'
    "}\n"
)


def test_parse_field_meta_keys_flat() -> None:
    """A flat literal of constant string keys parses to the exact keyset."""
    assert doctor._parse_field_meta_keys(_FLAT_REGISTRY) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )


def test_parse_field_meta_keys_spread_returns_none() -> None:
    """A `**spread` (or computed key) can't be read completely => None, never a
    truncated set that would risk a false 'fresh'."""
    spread = 'BASE = {"a.b": 1}\nFIELD_META = {**BASE, "backlog.id_prefix": 2}\n'
    assert doctor._parse_field_meta_keys(spread) is None
    computed = 'K = "x"\nFIELD_META = {K: 1}\n'
    assert doctor._parse_field_meta_keys(computed) is None


def test_parse_field_meta_keys_split_annotation_then_assign() -> None:
    """A bare annotation followed by a separate dict assignment still parses: the
    valueless AnnAssign is skipped, not treated as an unreadable form."""
    split = (
        "FIELD_META: dict[str, int]\n"
        'FIELD_META = {"project.id": 1, "backlog.id_prefix": 2}\n'
    )
    assert doctor._parse_field_meta_keys(split) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )


def test_parse_field_meta_keys_broken_or_absent_returns_none() -> None:
    """Unparseable text or no FIELD_META => None."""
    assert doctor._parse_field_meta_keys("FIELD_META = {  # truncated\n") is None
    assert doctor._parse_field_meta_keys("x = 1\n") is None


def _init_git_source(root: Path, registry_text: str) -> None:
    """Commit a registry.py into a throwaway git repo laid out like the cli source."""
    import subprocess

    reg = root / "src" / "fno" / "config" / "registry.py"
    reg.parent.mkdir(parents=True)
    reg.write_text(registry_text, encoding="utf-8")
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@t",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@t",
    }
    for cmd in (["init", "-q"], ["add", "-A"], ["commit", "-qm", "init"]):
        subprocess.run(["git", "-C", str(root), *cmd], check=True, env=env)


def test_source_config_keys_reads_committed_head(tmp_path: Path) -> None:
    """The source keyset comes from committed HEAD, NOT the dirty working tree: an
    uncommitted edit must not leak into the verdict (matches _source_rev semantics)."""
    _init_git_source(tmp_path, _FLAT_REGISTRY)
    assert doctor._source_config_keys(tmp_path) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )
    # Add a key in the WORKING TREE only (no commit). HEAD is unchanged, so the
    # committed keyset must not include it - else a dirty checkout false-STALEs.
    reg = tmp_path / "src" / "fno" / "config" / "registry.py"
    reg.write_text(
        _FLAT_REGISTRY.replace("}\n", '    "batch.enabled": Meta("z"),\n}\n'),
        encoding="utf-8",
    )
    assert doctor._source_config_keys(tmp_path) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )


def test_source_config_keys_fails_open_on_missing_or_non_git(tmp_path: Path) -> None:
    """None source or a non-git dir => None (skip the check, never crash doctor)."""
    assert doctor._source_config_keys(None) is None
    assert doctor._source_config_keys(tmp_path) is None  # not a git repo


def test_deployed_config_keys_reflects_real_field_meta() -> None:
    """The deployed surface is the real in-process FIELD_META (includes the sentinel key)."""
    keys = doctor._deployed_config_keys()
    assert keys is not None
    assert "backlog.id_prefix" in keys


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
        # Config drift: source defines a key deployed lacks, revs match -> python stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              deployed_config_keys=frozenset({"a"}), source_config_keys=frozenset({"a", "b"})),
         "stale", True, False),
        # Config drift proven even when the python status would otherwise be unknown.
        (dict(source_resolved=True, source_rev="a", marker=None, capture_present="present",
              deployed_config_keys=frozenset({"a"}), source_config_keys=frozenset({"a", "b"})),
         "stale", True, False),
        # Config match -> no drift, stays fresh.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              deployed_config_keys=frozenset({"a", "b"}), source_config_keys=frozenset({"a", "b"})),
         "fresh", False, False),
        # Config partial evidence (source keyset unknown) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              deployed_config_keys=frozenset({"a"}), source_config_keys=None), "fresh", False, False),
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
    # Should mention revision unknown / no marker / seed it via fno doctor update.
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
            "fix": "fno do pr watch install", "loaded": True, "last_tick": None,
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


def _wedged_pr_watch(monkeypatch) -> None:
    monkeypatch.setattr(
        doctor,
        "_pr_watch_liveness",
        lambda: {
            "enabled": True, "verdict": "wedged",
            "detail": "last tick 25s ago but each of the last 3 ticks ended broken",
            "fix": "fno do pr watch refresh", "loaded": True,
            "last_tick": "2026-09-11T00:00:00Z", "interval_seconds": 600,
        },
    )


def test_fix_refreshes_wedged_pr_watch_instead_of_healing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A wedged watermark is fresh, so the tick already runs: the cure is the
    plist re-render (refresh_watcher), never the plain bounce."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _wedged_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    refresh_calls: list = []

    def _fail_heal(**kw):
        raise AssertionError("wedged must re-render the plist, not bounce it")

    monkeypatch.setattr(
        pw, "refresh_watcher",
        lambda **kw: refresh_calls.append(kw) or ("re-rendered and bounced x", 0),
    )
    monkeypatch.setattr(pw, "heal_watcher", _fail_heal)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0  # advisory: never flips the exit
    assert len(refresh_calls) == 1
    assert "pr-watch refresh" in result.stderr


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


def test_wedged_pr_watch_reports_in_human_output(monkeypatch: pytest.MonkeyPatch) -> None:
    """A plain `doctor` names the wedged verdict and the refresh fix; silence
    here would read as a clean bill while the watcher delivers nothing."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _wedged_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    monkeypatch.setattr(
        pw, "refresh_watcher",
        lambda **kw: pytest.fail("plain doctor must not refresh; only --fix does"),
    )

    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "pr-watch wedged" in result.stdout
    assert "fno do pr watch refresh" in result.stdout


def test_ac3_hp_fix_delegates_to_update(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC3-HP: --fix on a stale Python install delegates to `fno doctor update`."""
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
    object and does NOT delegate to `fno doctor update` (which prints to stdout)."""
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
    # Same reasoning as the tripwire test below: the control-plane arms readout
    # (`fno-agents status --json`) is a legit advisory subprocess, and the claim
    # door may legitimately put a binary in this test's PATH. Stub it so the
    # readout's environmental arm state cannot masquerade as the rust-staleness
    # contradiction this test pins.
    monkeypatch.setattr(
        doctor, "_control_plane_arms_report", lambda: {"arms": [], "stale": [], "unknown_reason": None}
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


def test_doctor_prints_the_reader_line_for_red_arms(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """x-d7dc: a red (stale or failing) arm prints the line the Rust reader
    rendered, exactly once; an ok arm in the same payload is never named.
    """
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
    )
    red_line = (
        "active_backlog    STALE      never skip=never via=daemon "
        "cause=stale_daemon (daemon predates the installed build; run fno agents restart)"
    )
    red_row = {"arm": "active_backlog", "stale": True, "failing": False, "line": red_line}
    ok_row = {"arm": "stop_hook", "stale": False, "failing": False,
              "line": "stop_hook        ok         never"}
    monkeypatch.setattr(
        doctor,
        "_control_plane_arms_report",
        lambda: {"arms": [red_row, ok_row], "red": [red_row], "unknown_reason": None},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, f"exit code {result.exit_code}, output: {result.stdout}{result.stderr}"
    combined = result.stdout + result.stderr
    expected = f"fno doctor: control-plane arm {red_line}"
    assert expected in combined, f"missing owned line. Got:\n{combined}"
    assert combined.count(red_line) == 1, "the red line must print exactly once"
    assert "stop_hook" not in combined, "an ok arm must never be named"


def test_doctor_falls_back_to_the_sentence_for_rows_without_line(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """x-d7dc: a red row with no `line` (an older binary) prints the old
    sentence instead of failing."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor,
        "_control_plane_arms_report",
        lambda: {"red": [{"arm": "reap", "stale": True, "age_s": 4600,
                          "interval_s": 60, "skip_reason": "never"}],
                 "unknown_reason": None},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, f"exit code {result.exit_code}, output: {result.stdout}{result.stderr}"
    combined = result.stdout + result.stderr
    assert "control-plane arm reap is STALE" in combined, f"Got:\n{combined}"
    assert "last tick 4600s ago" in combined, f"Got:\n{combined}"
    assert "interval 60s" in combined, f"Got:\n{combined}"
    assert "skip: never" in combined, f"Got:\n{combined}"


def test_doctor_falls_back_to_unobserved_wording_for_an_unobserved_row(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """x-6484: a line-less attention row whose producer evidence reads
    unobserved falls back to UNOBSERVED wording, never the STALE sentence."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor,
        "_control_plane_arms_report",
        lambda: {"red": [{"arm": "king_wake", "stale": False, "age_s": None,
                          "interval_s": 900, "skip_reason": "never",
                          "producer_evidence": "unobserved"}],
                 "unknown_reason": None},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, f"exit code {result.exit_code}, output: {result.stdout}{result.stderr}"
    combined = result.stdout + result.stderr
    assert "control-plane arm king_wake is UNOBSERVED" in combined, f"Got:\n{combined}"
    assert "no producer receipt in the journals" in combined, f"Got:\n{combined}"
    assert "is STALE" not in combined, f"Got:\n{combined}"


def test_control_plane_arms_report_consumes_the_rust_attention_set(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """x-6484: `_control_plane_arms_report` passes the Rust-owned
    `arms_attention` rows through as `red` and never re-derives the verdict
    from the legacy `stale`/`failing` booleans: a stale row left out of the
    Rust selection stays unreported.
    """
    from fno import rust_binary

    observed_fresh = {"arm": "watchdog", "stale": False, "failing": False,
                      "producer_evidence": "observed", "line": "watchdog ok"}
    stale_left_out = {"arm": "reap", "stale": True, "failing": False,
                      "producer_evidence": "observed"}
    unobserved = {"arm": "king_wake", "stale": False, "failing": False,
                  "producer_evidence": "unobserved",
                  "line": "king_wake         UNOBSERVED     never via=launchd"}
    payload = json.dumps({"arms": [observed_fresh, stale_left_out, unobserved],
                          "arms_attention": [unobserved]})

    class _FakeResult:
        stdout = payload

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: Path("/bin/fno-agents"))
    monkeypatch.setattr(
        doctor.subprocess, "run",
        lambda *a, **kw: _FakeResult(),
    )
    report = doctor._control_plane_arms_report()
    assert report["unknown_reason"] is None
    assert report["red"] == [unobserved], f"Got: {report}"


def test_control_plane_arms_report_unknown_without_the_attention_set(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """x-6484 AC3: an older status payload carrying only `arms[]` (no
    `arms_attention`) reads unknown - never derived from legacy booleans."""
    from fno import rust_binary

    class _FakeResult:
        stdout = json.dumps({"arms": [{"arm": "reap", "stale": True}]})

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: Path("/bin/fno-agents"))
    monkeypatch.setattr(
        doctor.subprocess, "run",
        lambda *a, **kw: _FakeResult(),
    )
    report = doctor._control_plane_arms_report()
    assert report["red"] == []
    assert report["unknown_reason"] is not None
    assert "arms_attention" in report["unknown_reason"]


def test_doctor_prints_the_unobserved_row_the_reader_rendered(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """x-6484: an unobserved arm in the canonical attention set prints the
    reader's own UNOBSERVED line, once; an observed fresh arm is not named."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
    )
    unobserved_line = (
        "king_wake         UNOBSERVED     never via=launchd:sh.fno.pr-watcher"
    )
    monkeypatch.setattr(
        doctor,
        "_control_plane_arms_report",
        lambda: {"red": [{"arm": "king_wake", "stale": False, "failing": False,
                          "producer_evidence": "unobserved", "line": unobserved_line}],
                 "unknown_reason": None},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, f"exit code {result.exit_code}, output: {result.stdout}{result.stderr}"
    combined = result.stdout + result.stderr
    assert f"fno doctor: control-plane arm {unobserved_line}" in combined, f"Got:\n{combined}"
    assert combined.count("UNOBSERVED") == 1, "the unobserved line must print exactly once"


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
    # Same reasoning for the per-harness surface probe (`codex plugin marketplace
    # list`) - a legit advisory subprocess, not raw cargo.
    monkeypatch.setattr(doctor, "_harness_surface_report", lambda: {})
    # Same reasoning for the control-plane arms readout (`fno-agents status
    # --json`): a legit advisory subprocess, stubbed so the tripwire isolates
    # cargo, not the readout.
    monkeypatch.setattr(
        doctor, "_control_plane_arms_report", lambda: {"arms": [], "stale": [], "unknown_reason": None}
    )
    # Same reasoning for the plugin-roots probe (`fno-agents plugin-install
    # --check`): a legit advisory subprocess on machines with a cargo bin,
    # stubbed so the tripwire isolates cargo, not the probe.
    monkeypatch.setattr(
        doctor, "_plugin_cache_report", lambda: {"status": "unknown"}
    )

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
    from fno import rust_binary

    monkeypatch.setattr(
        rust_binary, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
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
    from fno import rust_binary

    monkeypatch.setattr(
        rust_binary, "resolve_installed_binary", lambda: Path("/bundled/fno-agents")
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


# ---------------------------------------------------------------------------
# Content drift: ground-truth Python freshness (catches a lying installed-rev
# marker after a cache-hit reinstall).
# ---------------------------------------------------------------------------


def test_content_drift_overrides_fresh_marker(monkeypatch: pytest.MonkeyPatch) -> None:
    """The regression: marker == source HEAD (rev check says fresh) but installed
    bytes differ -> STALE, exit nonzero, message names the file count. This is the
    month-old-install-behind-a-HEAD-marker case."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # marker agrees with HEAD - the lie
        capture_present="present",
        content_drift=3,  # but 3 .py files on disk differ
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 1
    assert "STALE" in result.stdout
    assert "3 .py file" in result.stdout


def test_content_drift_zero_stays_fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    """0 differing files is byte-identical -> never flips a fresh verdict."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        content_drift=0,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" in result.stdout


def test_content_indeterminate_downgrades_fresh_to_unknown(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An undeterminable content check (None) must NOT leave a marker-only fresh
    standing: the marker can lie about a cache-hit reinstall, so downgrade to
    unknown. It never flips to stale (no positive drift proven)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # marker matches -> rev check alone would say fresh
        capture_present="present",
        content_drift=None,  # but the ground-truth check could not run
    )
    result = runner.invoke(app, ["doctor", "--json"])
    payload = json.loads(result.stdout)
    assert payload["status"] == "unknown"
    assert payload["content_stale"] is False
    assert payload["content_indeterminate"] is True
    assert payload["content_drift_count"] is None


def test_content_indeterminate_does_not_downgrade_stale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A None content check only touches a would-be fresh; a verdict already proven
    stale by another signal (a missing verb) stays stale, not unknown."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="missing",  # proves stale independently
        content_drift=None,
    )
    result = runner.invoke(app, ["doctor", "--json"])
    payload = json.loads(result.stdout)
    assert payload["status"] == "stale"


def test_python_content_drift_counts_differing_py_files(tmp_path: Path) -> None:
    """_python_content_drift fingerprints installed vs source/src/fno and counts
    only files whose bytes differ; identical files do not count."""
    inst = tmp_path / "installed" / "fno"
    src = tmp_path / "source"
    src_pkg = src / "src" / "fno"
    inst.mkdir(parents=True)
    src_pkg.mkdir(parents=True)
    (inst / "same.py").write_text("x = 1\n")
    (src_pkg / "same.py").write_text("x = 1\n")
    (inst / "drift.py").write_text("old = True\n")
    (src_pkg / "drift.py").write_text("old = False\n")  # differs
    (src_pkg / "added.py").write_text("new = 1\n")  # only in source

    import fno as _fno_pkg

    # Point _installed_pkg_dir at our fake installed tree.
    orig_file = _fno_pkg.__file__
    try:
        _fno_pkg.__file__ = str(inst / "__init__.py")
        assert doctor._python_content_drift(src) == 2  # drift.py + added.py
    finally:
        _fno_pkg.__file__ = orig_file


def test_python_content_drift_none_when_source_missing(tmp_path: Path) -> None:
    """No source/src/fno dir -> None (skip), not a false 0 or a crash."""
    assert doctor._python_content_drift(tmp_path / "nonexistent") is None


def test_python_content_drift_none_when_source_arg_none() -> None:
    assert doctor._python_content_drift(None) is None


# ---------------------------------------------------------------------------
# Agent health (x-1c7b): four grooming surfaces shipped and never ran, every
# time because nothing reported the silence.
# ---------------------------------------------------------------------------


def _fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )


def test_dead_launch_agent_is_named_with_its_exit_and_reddens_doctor(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-ERR: an installed-but-failing agent must not be a quiet line."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_launch_agent_failures",
        lambda: {"applicable": True, "dead": [{"label": "sh.fno.pr-watcher", "exit": 78}]},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 1, "a dead agent must fail the exit code, not just print"
    assert "sh.fno.pr-watcher" in result.stdout
    assert "78" in result.stdout


def test_missing_launchctl_degrades_without_crying_wolf(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-ERR: no launchctl (Linux) must never read as a dead agent."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor, "_launch_agent_failures", lambda: {"applicable": False, "dead": []}
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "not applicable" in result.stdout
    assert "last exited" not in result.stdout


def test_never_run_grooming_reads_differently_from_stale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-UI: "never" names the install remedy and prints no hour count."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    # The remedy is platform-specific, so pin it: a Linux CI runner gets the
    # cron advice and would otherwise fail a macOS-shaped assertion.
    monkeypatch.setattr(doctor.sys, "platform", "darwin")
    result = runner.invoke(app, ["doctor"])
    assert "NEVER run" in result.stdout
    assert "--install-agent" in result.stdout
    assert "h ago" not in result.stdout
    assert result.exit_code == 0, "a fresh install has legitimately never groomed"


def test_stale_grooming_reports_the_age(monkeypatch: pytest.MonkeyPatch) -> None:
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "ran", "hours": 96.0, "stale": True, "agent_installed": True},
    )
    result = runner.invoke(app, ["doctor"])
    assert "96h ago" in result.stdout
    assert "NEVER" not in result.stdout


def test_fix_installs_the_groom_agent_when_nothing_schedules_it(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    monkeypatch.setattr(doctor.sys, "platform", "darwin")  # the install is launchd-only
    calls: list = []
    monkeypatch.setattr(
        "fno.backlog.groom.install_groom_agent",
        lambda **kw: calls.append(kw) or {"status": "installed", "detail": "ok"},
    )
    result = runner.invoke(app, ["doctor", "--fix"])
    assert len(calls) == 1
    assert "--fix groom agent: installed" in result.stderr


def test_fix_skips_the_install_when_the_agent_is_already_there(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": True},
    )

    # Pinned to darwin so this proves the already-installed guard, not the
    # platform guard - off launchd it would pass without exercising anything.
    monkeypatch.setattr(doctor.sys, "platform", "darwin")

    def _boom(**kw):
        raise AssertionError("must not reinstall an agent that is already installed")

    monkeypatch.setattr("fno.backlog.groom.install_groom_agent", _boom)
    runner.invoke(app, ["doctor", "--fix"])


def test_agent_scan_parses_last_exit_not_current_state(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A `-` in the PID column is normal for a periodic job; only col 2 counts."""
    import subprocess as sp

    monkeypatch.setattr(doctor.sys, "platform", "darwin")
    monkeypatch.setattr(doctor.shutil, "which", lambda name: "/bin/launchctl")
    listing = (
        "PID\tStatus\tLabel\n"
        "-\t0\tsh.fno.groom\n"
        "-\t78\tsh.fno.pr-watcher\n"
        "412\t0\tsh.fno.mux\n"
        "-\t127\tcom.other.thing\n"
        "-\t-\tsh.fno.idle\n"
    )
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda *a, **kw: sp.CompletedProcess(a[0], 0, listing, ""),
    )
    report = doctor._launch_agent_failures()
    assert report["applicable"] is True
    assert report["dead"] == [{"label": "sh.fno.pr-watcher", "exit": 78}], (
        "only nonzero-exit sh.fno.* labels count; foreign labels and `-` do not"
    )


def test_never_run_remedy_is_platform_appropriate(monkeypatch: pytest.MonkeyPatch) -> None:
    """--install-agent is launchd-only; off darwin it would report `unsupported`."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    monkeypatch.setattr(doctor.sys, "platform", "linux")
    result = runner.invoke(app, ["doctor"])
    assert "NEVER run" in result.stdout
    assert "--install-agent" not in result.stdout, "that flag does nothing off launchd"
    assert "docs/backlog-usage.md" in result.stdout


def test_fix_does_not_attempt_a_launchd_install_off_darwin(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An unguarded call would warn `unsupported` on every --fix, unactionably."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    monkeypatch.setattr(doctor.sys, "platform", "linux")

    def _boom(**kw):
        raise AssertionError("must not attempt a launchd install off darwin")

    monkeypatch.setattr("fno.backlog.groom.install_groom_agent", _boom)
    result = runner.invoke(app, ["doctor", "--fix"])
    assert "groom agent" not in result.stderr


def test_doctor_codex_app_server_absent_names_fix(monkeypatch):
    """AC7: a plain `fno doctor` reports an absent codex app-server daemon with
    the start command, the bootstrap alternative, and the restart ordering."""
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor,
        "_codex_app_server_report",
        lambda: {"present": False, "socket_path": "/tmp/missing.sock"},
    )
    result = runner.invoke(app, ["doctor"])
    assert "codex app-server daemon not running" in result.stdout
    assert "codex app-server daemon start" in result.stdout
    assert "bootstrap" in result.stdout
    assert "restart" in result.stdout


def test_doctor_codex_app_server_present_is_quiet(monkeypatch):
    """AC7 negative: when the daemon socket is present, no advisory line fires."""
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor,
        "_codex_app_server_report",
        lambda: {"present": True, "socket_path": "/tmp/here.sock"},
    )
    result = runner.invoke(app, ["doctor"])
    assert "codex app-server daemon not running" not in result.stdout


def test_doctor_codex_app_server_report_respects_codex_home(tmp_path, monkeypatch):
    """The report keys the socket off $CODEX_HOME, so a fresh home reads absent."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path / "codex-home"))
    report = doctor._codex_app_server_report()
    assert report["present"] is False
    assert report["socket_path"].endswith("app-server-control/app-server-control.sock")


def _short_codex_home(monkeypatch):
    """A CODEX_HOME short enough for the 104-char AF_UNIX bind limit (pytest's
    tmp_path on macOS is not; dir="/tmp" escapes the long TMPDIR). Caller
    cleans up the returned parent."""
    import tempfile

    home = tempfile.mkdtemp(prefix="fno-x571-", dir="/tmp")
    monkeypatch.setenv("CODEX_HOME", home)
    socket_dir = Path(home) / "app-server-control"
    socket_dir.mkdir(parents=True)
    return socket_dir / "app-server-control.sock", home


def test_codex_app_server_report_stale_file_reads_absent(tmp_path, monkeypatch):
    """A unix socket file survives its process: a regular file at the socket
    path must read absent, never present, however long it sits on disk."""
    import shutil

    monkeypatch.setattr(doctor, "_CODEX_APP_SERVER_PROBE_TIMEOUT_S", 0.5)
    sock_path, home = _short_codex_home(monkeypatch)
    try:
        sock_path.write_bytes(b"")
        assert doctor._codex_app_server_report()["present"] is False
    finally:
        shutil.rmtree(home, ignore_errors=True)


def test_codex_app_server_report_dead_socket_inode_reads_absent(tmp_path, monkeypatch):
    """The measured specimen: a real socket inode whose creating process is
    gone. The file exists, nothing listens, the probe reads absent."""
    import shutil
    import socket as _socket

    monkeypatch.setattr(doctor, "_CODEX_APP_SERVER_PROBE_TIMEOUT_S", 0.5)
    sock_path, home = _short_codex_home(monkeypatch)
    try:
        listener = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        listener.bind(str(sock_path))
        listener.close()  # inode remains on disk; no process listens
        assert sock_path.exists()
        assert doctor._codex_app_server_report()["present"] is False
    finally:
        shutil.rmtree(home, ignore_errors=True)


def test_codex_app_server_report_live_listener_reads_present(tmp_path, monkeypatch):
    """Positive control: a live listener on the control socket reads present."""
    import shutil
    import socket as _socket

    monkeypatch.setattr(doctor, "_CODEX_APP_SERVER_PROBE_TIMEOUT_S", 0.5)
    sock_path, home = _short_codex_home(monkeypatch)
    listener = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    listener.bind(str(sock_path))
    listener.listen(1)
    try:
        assert doctor._codex_app_server_report()["present"] is True
    finally:
        listener.close()
        shutil.rmtree(home, ignore_errors=True)


# ---------------------------------------------------------------------------
# Deployed fno plugin roots freshness: the Rust verb enumerates every root
# (marketplace stage, registry installPath, orphan copies) and byte-checks
# each against source HEAD. Same fresh|stale|unknown vocabulary; stale only
# on proven evidence.
# ---------------------------------------------------------------------------


def test_plugin_cache_no_source_is_unknown(tmp_path, monkeypatch):
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: "fno-agents-stub")
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: None)
    report = doctor._plugin_cache_report()
    assert report["status"] == "unknown"
    assert "no source checkout" in (report.get("detail") or "")


def _root_verdict(path: str, status: str, live: bool, **extra) -> dict:
    drift = extra.get("differing_count", 0) + extra.get("missing_count", 0)
    note = f" ({drift} file(s) differ from source HEAD)" if drift else ""
    if status == "stale" and not live:
        note += ". Fix: cd /src && fno config plugin install claude removes the stale second copy"
    blocker = None
    if status == "stale":
        blocker = (
            f"plugin root {path} ({'live' if live else 'second copy'}) differs from "
            f"source HEAD in {drift} file(s) (e.g. {(extra.get('sample') or ['?'])[0]}). "
            "Fix: cd /src && fno config plugin install claude"
        )
    verdict = {
        "path": path,
        "origin": "marketplace" if live else "registry",
        "live": live,
        "kind": "stage",
        "status": status,
        "source": "/src",
        "source_head": "a" * 40,
        "differing_count": 0,
        "missing_count": 0,
        "sample": [],
        "remedy": "cd /src && fno config plugin install claude",
        "detail": None,
        "note": note,
        "blocker": blocker,
    }
    verdict.update(extra)
    return verdict


def _roots_stdout(roots: list[dict], detail: str | None = None) -> str:
    live = next((r for r in roots if r["live"]), None)
    rank = {"fresh": 0, "absent": 0, "unknown": 1, "stale": 2}
    worst = max(roots, key=lambda r: rank.get(r["status"], 1), default=None)
    return json.dumps(
        {
            "status": (worst or {"status": "unknown"})["status"],
            "sha": (live or {}).get("sha"),
            "installed_at": None,
            "kind": "stage" if (live or not roots) else None,
            "stage": (live or {}).get("path"),
            "remedy": (live or {}).get("remedy"),
            "detail": detail,
            "roots": roots,
        }
    )


def test_plugin_cache_multi_root_folds_worst_and_names_cache(tmp_path, monkeypatch):
    """Two roots with the non-live one stale: the fold keeps both under
    roots, reads status stale, points the flat keys at the live root, and
    the blocker names the stale second copy."""
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: tmp_path)
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: "fno-agents-stub")
    monkeypatch.setattr(
        doctor,
        "_run_stage_check",
        lambda argv: (
            3,
            _roots_stdout(
                [
                    _root_verdict("/stage/fno", "fresh", True),
                    _root_verdict(
                        "/claude/plugins/cache/footnote/fno/0.3.2",
                        "stale",
                        False,
                        differing_count=1422,
                        sample=["hooks/king-delegation-guard.sh"],
                    ),
                ]
            ),
            "",
        ),
    )

    report = doctor._plugin_cache_report()

    assert report["status"] == "stale"
    assert report["kind"] == "stage"
    assert report["stage"] == "/stage/fno"
    assert len(report["roots"]) == 2
    assert any("/claude/plugins/cache/footnote" in r["path"] for r in report["roots"])
    assert "fno config plugin install claude" in report["remedy"]
    blockers = doctor._blockers({"plugin_cache": report})
    assert any("second copy" in b and "1422 file(s)" in b for b in blockers)
    assert any("hooks/king-delegation-guard.sh" in b for b in blockers)


def test_plugin_cache_stage_check_transport_failure_is_unknown(tmp_path, monkeypatch):
    """A failed or timed-out stage probe is unknown with the reason in
    detail, adds no blocker, and reports no root as fresh."""
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: tmp_path)
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: "fno-agents-stub")
    monkeypatch.setattr(doctor, "_run_stage_check", lambda argv: (1, "", "boom"))

    report = doctor._plugin_cache_report()

    assert report["kind"] == "stage"
    assert report["status"] == "unknown"
    assert "boom" in (report.get("detail") or "")
    assert report["roots"] == []
    assert doctor._blockers({"plugin_cache": report}) == []


def test_plugin_cache_stage_check_needs_a_cargo_binary(tmp_path, monkeypatch):
    """No cargo fno-agents on the machine -> unknown naming the gap, never a
    false fresh (CI runners carry no ~/.cargo/bin)."""
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: None)

    report = doctor._plugin_cache_report()

    assert report["kind"] == "stage"
    assert report["status"] == "unknown"
    assert "no cargo fno-agents binary" in (report.get("detail") or "")


# --- x-2486: the stale-cache line must name a command that can perform the fix ---
#
# `fno update` was measured against this artifact: 15m07s, exit 0,
# installed_plugins.json byte-identical. update.py names ~/.claude/plugins only
# as a SOURCE to install FROM. The registry belongs to claude, which mints its
# own gitCommitSha, so the verb that can refresh it is `claude plugin update`.
# These assert the RELATIONSHIP (the prescribed verb owns the artifact), not the
# sentence: a string-only test re-encodes the same guess it is meant to catch.


def test_stale_plugin_cache_prescribes_the_verb_that_owns_the_artifact(
    capsys: pytest.CaptureFixture[str],
) -> None:
    result = {
        "status": "fresh",
        "rust_stale": False,
        "missing_verbs": [],
        "python_stale": False,
        "plugin_cache": {
            "status": "stale",
            "sha": "a8f3c5537ed55b3ce0926fec21a3f35021078539",
            "installed_at": "2026-08-13T04:48:50.501Z",
        },
    }
    rust = {"binary": "/cargo/bin/fno-agents", "revision": "abc", "binary_rev": "abc"}
    doctor._emit_human(result, Path("/src"), rust, err=False, cargo_present=True)
    line = next(
        ln for ln in capsys.readouterr().out.splitlines() if "plugin cache" in ln
    )
    # The prescribed verb owns installed_plugins.json.
    assert "claude plugin update" in line
    # And the retired prescription is named as NOT the fix, because it shipped
    # for long enough that a reader has already run it.
    assert "does NOT" in line
    assert "restart" in line


def test_fresh_plugin_cache_prescribes_nothing(
    capsys: pytest.CaptureFixture[str],
) -> None:
    result = {
        "status": "fresh",
        "rust_stale": False,
        "missing_verbs": [],
        "python_stale": False,
        "plugin_cache": {"status": "fresh", "sha": "abc", "installed_at": "x"},
    }
    rust = {"binary": "/cargo/bin/fno-agents", "revision": "abc", "binary_rev": "abc"}
    doctor._emit_human(result, Path("/src"), rust, err=False, cargo_present=True)
    line = next(
        ln for ln in capsys.readouterr().out.splitlines() if "plugin cache" in ln
    )
    assert "claude plugin update" not in line
    assert "fno doctor update" not in line


def _write_skill_file(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)


def test_plugin_file_fresh_reports_equal_digests(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source = tmp_path / "source"
    active = tmp_path / "active" / "skills" / "review" / "SKILL.md"
    _write_skill_file(source / "skills" / "review" / "SKILL.md", b"same")
    _write_skill_file(active, b"same")
    monkeypatch.setattr(doctor, "_resolve_source", lambda _source: source)

    result = runner.invoke(app, ["doctor", "plugin-file", str(active)])

    assert result.exit_code == 0, result.output
    assert "PLUGIN_FILE_FRESH" in result.stdout
    assert "active_sha256=" in result.stdout
    assert "source_sha256=" in result.stdout
    assert "skills/review/SKILL.md" in result.stdout


def test_plugin_file_stale_names_claude_refresh_and_digests(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source = tmp_path / "source"
    active = (
        tmp_path
        / ".claude"
        / "plugins"
        / "cache"
        / "footnote"
        / "fno"
        / "0.3.1"
        / "skills"
        / "review"
        / "SKILL.md"
    )
    _write_skill_file(source / "skills" / "review" / "SKILL.md", b"source")
    _write_skill_file(active, b"deployed")
    monkeypatch.setattr(doctor, "_resolve_source", lambda _source: source)

    result = runner.invoke(app, ["doctor", "plugin-file", str(active)])

    assert result.exit_code == 3, result.output
    assert "PLUGIN_FILE_STALE" in result.stdout
    assert "active_sha256=" in result.stdout
    assert "source_sha256=" in result.stdout
    assert "claude plugin update fno@footnote" in result.stdout
    assert "fno doctor update" in result.stdout
    assert "does NOT refresh" in result.stdout
    assert "restart" in result.stdout


def test_plugin_file_unknown_is_not_fresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    active = tmp_path / "active" / "skills" / "review" / "SKILL.md"
    _write_skill_file(active, b"deployed")
    monkeypatch.setattr(doctor, "_resolve_source", lambda _source: None)

    result = runner.invoke(app, ["doctor", "plugin-file", str(active)])

    assert result.exit_code == 4, result.output
    assert "PLUGIN_FILE_UNKNOWN" in result.stdout
    assert "active_sha256" not in result.stdout


def test_plugin_file_json_contains_positive_record(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source = tmp_path / "source"
    active = tmp_path / "active" / "skills" / "review" / "SKILL.md"
    _write_skill_file(source / "skills" / "review" / "SKILL.md", b"source")
    _write_skill_file(active, b"deployed")
    monkeypatch.setattr(doctor, "_resolve_source", lambda _source: source)

    result = runner.invoke(
        app, ["doctor", "plugin-file", str(active), "--json"]
    )

    assert result.exit_code == 3, result.output
    payload = json.loads(result.stdout)
    assert payload["record"] == "PLUGIN_FILE_STALE"
    assert payload["relative_path"] == "skills/review/SKILL.md"
    assert payload["active_digest"] != payload["source_digest"]


# The second prescription site (the silent-switch cause line) is already covered
# by test_doctor_silent_switch.py::test_armed_unknown_manifests_name_stale_plugin_cache_as_cause,
# which owns the armed-manifest fixture this branch needs. Asserting it here too
# would be a second implementation of one check.


# ---------------------------------------------------------------------------
# Evals demand (x-ab72): staleness row for the eval bank
# ---------------------------------------------------------------------------


def _patch_evals_summary(
    monkeypatch: pytest.MonkeyPatch, summary: dict | None
) -> None:
    """Pin the summary seam: a fake history path + a canned summary.

    The lazy imports resolve at call time from their source modules, so
    patching there reaches the assembly without going through the real
    ``~/.fno`` history.
    """
    monkeypatch.setattr("fno.paths.evals_history", lambda: Path("/evals/h.jsonl"))
    monkeypatch.setattr(
        "fno.evals.report.evals_health_summary", lambda _path, **_kw: summary
    )


def test_doctor_renders_evals_stale_row(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    _patch_evals_summary(
        monkeypatch,
        {
            "regression_pass_rate": 1.0,
            "flake_count": 0,
            "regression_alarm": [],
            "age_days": 9.0,
            "stale": True,
            "never_ran": False,
        },
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "evals STALE" in result.stdout
    assert "9d old" in result.stdout
    assert "fno doctor evals run --tier regression" in result.stdout


def test_doctor_renders_evals_regressing_row(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    _patch_evals_summary(
        monkeypatch,
        {
            "regression_pass_rate": 0.33,
            "flake_count": 0,
            "regression_alarm": ["r"],
            "regressed": ["r"],
            "window_days": 7,
            "age_days": 1.0,
            "stale": False,
            "never_ran": False,
        },
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "evals REGRESSING" in result.stdout
    assert "r dropped" in result.stdout
    assert "prior 7d window" in result.stdout
    assert "fno doctor evals trend" in result.stdout


def test_doctor_renders_evals_unknown_row_without_history(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    _patch_evals_summary(monkeypatch, None)
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "evals UNKNOWN" in result.stdout


def test_doctor_renders_evals_unknown_row_when_never_ran(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    _patch_evals_summary(
        monkeypatch,
        {
            "regression_pass_rate": None,
            "flake_count": 0,
            "regression_alarm": [],
            "age_days": None,
            "stale": False,
            "never_ran": True,
        },
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "evals UNKNOWN" in result.stdout


def test_doctor_stays_silent_when_evals_fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    _patch_evals_summary(
        monkeypatch,
        {
            "regression_pass_rate": 1.0,
            "flake_count": 0,
            "regression_alarm": [],
            "age_days": 1.0,
            "stale": False,
            "never_ran": False,
        },
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "evals" not in result.stdout


# ---------------------------------------------------------------------------
# Deployed-component convergence (verdict widening + rendering)
# ---------------------------------------------------------------------------


def test_verdict_widens_rust_stale_from_a_proven_stale_sibling() -> None:
    """A fresh client beside a stale daemon is invisible to the client's own
    rev check; the component verdict proves the sibling and gates rust_stale."""
    result = doctor._verdict(
        source_resolved=True,
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_installed_rev="aaa",
        rust_source_rev="aaa",
        cargo_bin_present=True,
        component_statuses=[
            ("fno-agents", "fresh"),
            ("fno-agents-daemon", "stale"),
            ("fno-agents-worker", "fresh"),
            ("fno", "fresh"),
            ("python-tool", "fresh"),
        ],
    )
    assert result["rust_stale"] is True
    assert result["status"] == "stale"


def test_verdict_unknown_components_never_gate_rust_stale() -> None:
    """Unknown is not proven stale: an unprobed component never widens the
    gate (it renders with its named instrument instead)."""
    result = doctor._verdict(
        source_resolved=True,
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_installed_rev="aaa",
        rust_source_rev="aaa",
        cargo_bin_present=True,
        component_statuses=[("fno-agents-worker", "unknown")],
    )
    assert result["rust_stale"] is False
    assert result["status"] == "fresh"


def test_ac3_hp_doctor_shows_unknown_with_the_named_instrument(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC3-HP: one component cannot be probed -> doctor renders Unknown with
    the named instrument and does not collapse it into fresh or missing."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        components=[
            {"component": "fno-agents", "status": "fresh"},
            {"component": "fno-agents-worker", "status": "unknown",
             "detail": "hung on `version --json` (>20s)",
             "line": "component fno-agents-worker: unknown (no revision reported,"
                     " expected abc1234abcd); hung on `version --json` (>20s)"},
            {"component": "python-tool", "status": "fresh"},
        ],
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, result.stdout
    assert "component fno-agents-worker: unknown" in result.stdout
    assert "hung on `version --json`" in result.stdout


def test_doctor_component_summary_line_when_all_fresh(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Every component fresh -> one positive summary line naming them all."""
    rows = [
        {"component": "python-tool", "status": "fresh"},
        {"component": "fno", "status": "fresh"},
        {"component": "fno-agents", "status": "fresh"},
        {"component": "fno-agents-daemon", "status": "fresh"},
        {"component": "fno-agents-worker", "status": "fresh"},
    ]
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        components=rows,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, result.stdout
    assert "components: 5/5 fresh (python-tool, fno, fno-agents, fno-agents-daemon, fno-agents-worker)." in result.stdout


def test_doctor_component_stale_renders_repair_and_gates_exit(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A proven-stale sibling renders its repair command and the doctor exits
    nonzero (stale is actionable, not advisory)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        components=[
            {"component": "fno-agents-daemon", "status": "stale",
             "observed_rev": "0" * 40, "expected_rev": "a" * 40,
             "repair": "cargo install --path /src/crates/fno-agents --bins",
             "line": "component fno-agents-daemon: stale (rev 000000000000,"
                     " expected aaaaaaaaaaaa); repair: cargo install --path"
                     " /src/crates/fno-agents --bins"},
            {"component": "fno-agents", "status": "fresh"},
        ],
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "component fno-agents-daemon: stale" in result.stdout
    assert "repair: cargo install --path /src/crates/fno-agents --bins" in result.stdout
