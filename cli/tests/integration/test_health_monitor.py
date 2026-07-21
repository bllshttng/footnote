"""Integration tests for the backlog health monitor.

Covers:
- ``load_config`` defaults + project/user merge
- ``evaluate_thresholds`` per-key breach detection
- ``Breach.severity`` scaling (info / warn / alert)
- ``dispatch_notifications`` surface routing + throttling
- ``append_history`` JSONL write + retention pruning
- ``fno backlog triage health --check`` exit-code semantics
- ``fno backlog triage health --check --quiet`` loop-safe output
- ``fno backlog triage trend`` history readout
"""
from __future__ import annotations

import json
from datetime import datetime, timezone, timedelta
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    """Redirect ~/.fno/graph.json + the in-process global home dir to a
    temp tree so each test starts from an empty backlog and an empty home.

    Mirrors the fixture pattern in test_collision.py (its tmp_graph also
    flips internal module-level Path constants via
    fno.graph._constants).
    """
    import fno.graph._constants as gc

    home = tmp_path / "home"
    home.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HOME", str(home))

    abilities_dir = home / ".fno"
    abilities_dir.mkdir(parents=True, exist_ok=True)

    # GRAPH_JSON is computed at import time from Path.home(); monkeypatch
    # it to the temp path so reads/writes land in the test sandbox.
    monkeypatch.setattr(gc, "GRAPH_JSON", abilities_dir / "graph.json")

    return abilities_dir


def _write_idea_nodes(graph_path: Path, n: int) -> None:
    """Seed the graph with N idea-status nodes (no plan_path = derives to idea)."""
    from datetime import datetime, timezone
    entries = []
    for i in range(n):
        entries.append(
            {
                "id": f"ab-test{i:04x}",
                "title": f"idea {i}",
                "type": "feature",
                "project": "test-proj",
                "cwd": str(graph_path.parent),
                "priority": "p2",
                "domain": "code",
                "blocked_by": [],
                "session_id": None,
                "claimed_at": None,
                "completed_at": None,
                "has_brief": False,
                "compacted": False,
                "plan_path": None,  # no plan_path -> derives to status: idea
                "pr_number": None,
                "pr_url": None,
                "merge_status": None,
                "created_at": datetime.now(timezone.utc).isoformat(),
                "status": "idea",
                "collisions_acknowledged": [],
                "supersedes": [],
                "superseded_by": None,
                "source_kind": "organic",
            }
        )
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(json.dumps({"entries": entries}, indent=2))


# ---------------------------------------------------------------------------
# load_config
# ---------------------------------------------------------------------------


def test_load_config_defaults(tmp_path):
    """Missing settings files yield the documented defaults."""
    from fno.health_monitor import load_config, DEFAULT_CONFIG

    out = load_config(
        project_settings=tmp_path / "missing-project.yaml",
        user_settings=tmp_path / "missing-user.yaml",
    )
    assert out["enabled"] == DEFAULT_CONFIG["enabled"]
    assert out["thresholds"]["idea_pile_depth"] == DEFAULT_CONFIG["thresholds"]["idea_pile_depth"]
    assert out["thresholds"]["stale_ready_days"] == DEFAULT_CONFIG["thresholds"]["stale_ready_days"]
    assert out["notifications"]["surfaces"] == DEFAULT_CONFIG["notifications"]["surfaces"]
    assert out["history"]["enabled"] == DEFAULT_CONFIG["history"]["enabled"]


def test_load_config_project_overrides_user(tmp_path):
    """Project settings beat user settings beat defaults."""
    from fno.health_monitor import load_config

    user = tmp_path / "user.yaml"
    user.write_text(
        "config:\n"
        "  health_monitor:\n"
        "    thresholds:\n"
        "      idea_pile_depth: 50\n"
        "      stale_ready_days: 60\n"
        "    notifications:\n"
        "      surfaces: [terminal, webhook]\n"
        "      webhook_url: https://example.com/user-hook\n"
    )
    project = tmp_path / "project.yaml"
    project.write_text(
        "config:\n"
        "  health_monitor:\n"
        "    thresholds:\n"
        "      idea_pile_depth: 10\n"
        "    notifications:\n"
        "      throttle_minutes: 5\n"
    )
    out = load_config(project_settings=project, user_settings=user)
    # project wins where present
    assert out["thresholds"]["idea_pile_depth"] == 10
    # user fills in keys project omits
    assert out["thresholds"]["stale_ready_days"] == 60
    # notification surfaces: project omits, user provides
    assert out["notifications"]["surfaces"] == ["terminal", "webhook"]
    # project's throttle overrides default
    assert out["notifications"]["throttle_minutes"] == 5
    # user's webhook_url survives the merge
    assert out["notifications"]["webhook_url"] == "https://example.com/user-hook"


def test_load_config_disabled_short_circuits(tmp_path):
    """enabled: false is preserved through the merge."""
    from fno.health_monitor import load_config

    user = tmp_path / "user.yaml"
    user.write_text(
        "config:\n"
        "  health_monitor:\n"
        "    enabled: false\n"
    )
    out = load_config(project_settings=tmp_path / "missing.yaml", user_settings=user)
    assert out["enabled"] is False


# ---------------------------------------------------------------------------
# evaluate_thresholds
# ---------------------------------------------------------------------------


def _healthy_report() -> dict:
    """A report dict that breaches no thresholds at default config."""
    return {
        "scope": "project 'test'",
        "idea_pile_depth": 0,
        "stale_ready_nodes": [],
        "failure_prone_nodes": [],
        "collisions": [],
        "acknowledged_resolved": [],
        "totals": {
            "pending": 0,
            "ideas": 0,
            "stale": 0,
            "failure_prone": 0,
            "collisions": 0,
            "acknowledged_resolved": 0,
        },
    }


def test_evaluate_no_breaches_when_healthy():
    from fno.health_monitor import evaluate_thresholds

    breaches = evaluate_thresholds(_healthy_report())
    assert breaches == []


def test_evaluate_idea_pile_breach():
    from fno.health_monitor import evaluate_thresholds

    report = _healthy_report()
    report["idea_pile_depth"] = 30  # default threshold = 25
    breaches = evaluate_thresholds(report)
    assert len(breaches) == 1
    assert breaches[0].key == "idea_pile_depth"
    assert breaches[0].actual == 30
    assert breaches[0].threshold == 25
    assert breaches[0].severity in ("info", "warn", "alert")


def test_evaluate_stale_ready_breach():
    from fno.health_monitor import evaluate_thresholds

    report = _healthy_report()
    report["stale_ready_nodes"] = [
        {"id": "ab-1", "title": "old", "age_days": 45},
        {"id": "ab-2", "title": "older", "age_days": 60},
    ]
    breaches = evaluate_thresholds(report)
    keys = [b.key for b in breaches]
    assert "stale_ready_nodes" in keys


def test_evaluate_failure_prone_breach():
    from fno.health_monitor import evaluate_thresholds

    report = _healthy_report()
    report["failure_prone_nodes"] = [
        {"id": f"ab-{i}", "title": "x", "attempts": 3, "burned_usd": 1.0}
        for i in range(4)  # default threshold = 2
    ]
    breaches = evaluate_thresholds(report)
    keys = [b.key for b in breaches]
    assert "failure_prone_nodes" in keys


def test_evaluate_collision_breach():
    from fno.health_monitor import evaluate_thresholds

    report = _healthy_report()
    report["collisions"] = [
        {"between": [f"ab-a{i}", f"ab-b{i}"], "shared_files": ["x"], "severity": "medium"}
        for i in range(5)  # default threshold = 3
    ]
    breaches = evaluate_thresholds(report)
    keys = [b.key for b in breaches]
    assert "collisions" in keys


def test_evaluate_severity_scaling():
    """Severity scales with overshoot magnitude: 1.5x = info, 2x = warn, 5x = alert."""
    from fno.health_monitor import evaluate_thresholds

    def severity_for_actual(actual: int) -> str:
        rep = _healthy_report()
        rep["idea_pile_depth"] = actual
        breaches = evaluate_thresholds(rep)
        assert len(breaches) == 1
        return breaches[0].severity

    # Threshold is 25.
    assert severity_for_actual(30) == "info"   # 1.2x -> info
    assert severity_for_actual(50) == "warn"   # 2.0x -> warn
    assert severity_for_actual(125) == "alert"  # 5.0x -> alert


def test_evaluate_disabled_returns_empty():
    """When config.enabled is false, no breaches even with bad data."""
    from fno.health_monitor import evaluate_thresholds

    report = _healthy_report()
    report["idea_pile_depth"] = 1000
    cfg = {
        "enabled": False,
        "thresholds": {
            "idea_pile_depth": 25,
            "stale_ready_days": 30,
            "failure_prone_attempts": 2,
            "collision_count": 3,
            "collision_severity_min": "medium",
        },
    }
    assert evaluate_thresholds(report, config=cfg) == []


# ---------------------------------------------------------------------------
# append_history
# ---------------------------------------------------------------------------


def test_history_appended_each_run(tmp_path):
    from fno.health_monitor import append_history

    history = tmp_path / "history.jsonl"
    append_history(_healthy_report(), breaches=[], history_path=history)
    append_history(_healthy_report(), breaches=[], history_path=history)

    lines = history.read_text().strip().splitlines()
    assert len(lines) == 2
    for line in lines:
        entry = json.loads(line)
        assert "timestamp" in entry
        assert "report" in entry
        assert entry["breaches"] == []


def test_history_pruning_drops_old_entries(tmp_path):
    """Entries older than retain_days are pruned on append."""
    from fno.health_monitor import append_history

    history = tmp_path / "history.jsonl"
    old_ts = (datetime.now(timezone.utc) - timedelta(days=100)).isoformat()
    history.write_text(
        json.dumps({"timestamp": old_ts, "report": {}, "breaches": []}) + "\n"
    )
    append_history(
        _healthy_report(),
        breaches=[],
        history_path=history,
        retain_days=90,
    )
    lines = history.read_text().strip().splitlines()
    assert len(lines) == 1  # old entry pruned, new one written
    entry = json.loads(lines[0])
    # The new entry is the survivor.
    assert entry["timestamp"] != old_ts


def test_history_handles_unparseable_existing_lines(tmp_path):
    """Malformed lines in history.jsonl don't crash the append step."""
    from fno.health_monitor import append_history

    history = tmp_path / "history.jsonl"
    history.write_text("this is not json\n{partial: oops\n")
    # Should not raise; should produce exactly one valid line at the end.
    append_history(_healthy_report(), breaches=[], history_path=history)
    lines = history.read_text().strip().splitlines()
    # Last line must be valid JSON.
    json.loads(lines[-1])


# ---------------------------------------------------------------------------
# dispatch_notifications
# ---------------------------------------------------------------------------


def test_dispatch_terminal_writes_stderr(tmp_path, capsys):
    """terminal surface prints breach summary to stderr."""
    from fno.health_monitor import dispatch_notifications, Breach

    breach = Breach(
        key="idea_pile_depth",
        actual=30,
        threshold=25,
        severity="info",
        message="idea pile at 30 (threshold 25)",
    )
    cfg = {
        "enabled": True,
        "notifications": {
            "surfaces": ["terminal"],
            "discord_channel": None,
            "webhook_url": None,
            "throttle_minutes": 60,
        },
    }
    dispatch_notifications(
        _healthy_report(),
        [breach],
        config=cfg,
        throttle_path=tmp_path / "throttle.json",
        alert_log_path=tmp_path / "alerts.log",
    )
    captured = capsys.readouterr()
    assert "idea_pile_depth" in captured.err


def test_dispatch_log_only_writes_file(tmp_path, capsys):
    """log_only surface produces no terminal output but writes to alerts log."""
    from fno.health_monitor import dispatch_notifications, Breach

    breach = Breach(
        key="idea_pile_depth",
        actual=30,
        threshold=25,
        severity="info",
        message="x",
    )
    cfg = {
        "enabled": True,
        "notifications": {
            "surfaces": ["log_only"],
            "discord_channel": None,
            "webhook_url": None,
            "throttle_minutes": 60,
        },
    }
    alert_log = tmp_path / "alerts.log"
    dispatch_notifications(
        _healthy_report(),
        [breach],
        config=cfg,
        throttle_path=tmp_path / "throttle.json",
        alert_log_path=alert_log,
    )
    captured = capsys.readouterr()
    # log_only must not write the breach to stderr; tightened from the
    # original `or` form which passed vacuously when stderr was empty.
    assert "idea_pile_depth" not in captured.err
    assert alert_log.exists()
    assert "idea_pile_depth" in alert_log.read_text()


def test_dispatch_throttle_suppresses_duplicate(tmp_path, capsys):
    """Two breaches with the same key within throttle_minutes produce one alert."""
    from fno.health_monitor import dispatch_notifications, Breach

    breach = Breach(
        key="idea_pile_depth",
        actual=30,
        threshold=25,
        severity="info",  # info respects throttle; alert always fires
        message="x",
    )
    cfg = {
        "enabled": True,
        "notifications": {
            "surfaces": ["log_only"],
            "discord_channel": None,
            "webhook_url": None,
            "throttle_minutes": 60,
        },
    }
    alert_log = tmp_path / "alerts.log"
    throttle = tmp_path / "throttle.json"
    dispatch_notifications(
        _healthy_report(), [breach],
        config=cfg, throttle_path=throttle, alert_log_path=alert_log,
    )
    dispatch_notifications(
        _healthy_report(), [breach],
        config=cfg, throttle_path=throttle, alert_log_path=alert_log,
    )
    # Only one notification should have been written.
    assert alert_log.read_text().count("idea_pile_depth") == 1


def test_dispatch_throttle_does_not_suppress_alert_severity(tmp_path):
    """severity=alert bypasses throttle (alerts always fire)."""
    from fno.health_monitor import dispatch_notifications, Breach

    breach = Breach(
        key="idea_pile_depth",
        actual=200,  # 8x threshold -> alert
        threshold=25,
        severity="alert",
        message="x",
    )
    cfg = {
        "enabled": True,
        "notifications": {
            "surfaces": ["log_only"],
            "discord_channel": None,
            "webhook_url": None,
            "throttle_minutes": 60,
        },
    }
    alert_log = tmp_path / "alerts.log"
    throttle = tmp_path / "throttle.json"
    dispatch_notifications(
        _healthy_report(), [breach],
        config=cfg, throttle_path=throttle, alert_log_path=alert_log,
    )
    dispatch_notifications(
        _healthy_report(), [breach],
        config=cfg, throttle_path=throttle, alert_log_path=alert_log,
    )
    # Both alerts wrote, because severity=alert is not throttled.
    assert alert_log.read_text().count("idea_pile_depth") == 2


# ---------------------------------------------------------------------------
# CLI: fno backlog triage health --check
# ---------------------------------------------------------------------------


def test_check_mode_exit_0_when_healthy(tmp_graph):
    """--check on empty backlog exits 0."""
    result = runner.invoke(app, ["backlog", "triage", "health", "--check", "--quiet"])
    assert result.exit_code == 0, result.output + result.stderr


def test_quiet_mode_no_output_when_healthy(tmp_graph):
    """--quiet on healthy backlog produces no stdout."""
    result = runner.invoke(app, ["backlog", "triage", "health", "--check", "--quiet"])
    assert result.exit_code == 0
    # No stdout when healthy + quiet (stderr is fine for "loaded config" diagnostics)
    assert result.stdout.strip() == ""


def test_check_mode_exit_4_on_breach(tmp_graph, monkeypatch):
    """Seed enough idea nodes to breach default threshold; --check exits 4."""
    # Default idea_pile_depth threshold is 25. Seed 30 idea nodes.
    _write_idea_nodes(tmp_graph / "graph.json", n=30)
    result = runner.invoke(
        app,
        ["backlog", "triage", "health", "--check", "--all"],
    )
    assert result.exit_code == 4, (
        f"expected exit 4 on breach, got {result.exit_code}\n"
        f"stdout: {result.stdout}"
    )


def test_trend_verb_prints_summary_when_history_empty(tmp_graph, tmp_path, monkeypatch):
    """trend prints a friendly message when no history exists yet."""
    # Point the trend verb at an empty history file via env override
    monkeypatch.setenv("FNO_HEALTH_HISTORY", str(tmp_path / "empty.jsonl"))
    result = runner.invoke(app, ["backlog", "triage", "trend"])
    assert result.exit_code == 0
    # Tightened from the original triple-or form: must contain the actual
    # user-facing message so a silent crash that swallows stdout would
    # still fail the test.
    assert "no history yet" in result.output.lower()


def test_check_to_history_to_throttle_to_trend_journey(tmp_graph, tmp_path, monkeypatch):
    """End-to-end wiring: --check breach -> history append -> throttle write
    -> repeat --check still breaches but throttle suppresses dispatch ->
    trend reads both entries.

    Catches wiring bugs the per-function tests miss: e.g. cmd_health
    forgetting to pass throttle_path or skipping append_history.
    """
    # Seed graph so --check breaches.
    _write_idea_nodes(tmp_graph / "graph.json", n=30)

    history_path = tmp_path / "hist.jsonl"
    monkeypatch.setenv("FNO_HEALTH_HISTORY", str(history_path))

    # Point the throttle/alert paths at the temp tree so the journey is
    # observable. We seed a project settings.yaml that turns on log_only
    # so the alert log is the dispatch sink we can read back.
    settings = tmp_graph / "settings.yaml"
    settings.write_text(
        f"config:\n"
        f"  health_monitor:\n"
        f"    enabled: true\n"
        f"    notifications:\n"
        f"      surfaces: [log_only]\n"
        f"    history:\n"
        f"      enabled: true\n"
        f"      path: {history_path}\n"
        f"      retain_days: 90\n"
    )
    # The CLI loads project settings from `.fno/settings.yaml` in
    # CWD; switch to the tmp home so that resolves correctly.
    monkeypatch.chdir(tmp_graph.parent)
    (tmp_graph.parent / ".fno").mkdir(exist_ok=True)
    (tmp_graph.parent / ".fno" / "settings.yaml").write_text(settings.read_text())

    # First check: breach, history written, alert dispatched.
    r1 = runner.invoke(app, ["backlog", "triage", "health", "--check", "--all"])
    assert r1.exit_code == 4, r1.output
    assert history_path.exists()
    history_lines_1 = history_path.read_text().strip().splitlines()
    assert len(history_lines_1) == 1
    entry1 = json.loads(history_lines_1[0])
    assert entry1["report"]["idea_pile_depth"] >= 30
    assert any(b["key"] == "idea_pile_depth" for b in entry1["breaches"])

    # Second check: still breach. History appends a second line. The
    # alert log (from log_only) should NOT have grown - throttle
    # suppresses the second info-severity dispatch.
    alert_log = Path.home() / ".fno" / "health-alerts.log"
    log_size_before = alert_log.stat().st_size if alert_log.exists() else 0
    r2 = runner.invoke(app, ["backlog", "triage", "health", "--check", "--all"])
    assert r2.exit_code == 4, r2.output
    history_lines_2 = history_path.read_text().strip().splitlines()
    assert len(history_lines_2) == 2  # history is not throttled
    if alert_log.exists():
        # Throttle should keep the log file from growing on the second
        # info-severity dispatch within the throttle window.
        assert alert_log.stat().st_size == log_size_before

    # Trend: reads the populated history.
    r3 = runner.invoke(app, ["backlog", "triage", "trend", "--days", "1"])
    assert r3.exit_code == 0
    assert "idea_pile_depth" in r3.output


def test_dispatch_webhook_posts_correct_shape(tmp_path, monkeypatch):
    """webhook surface POSTs a JSON payload with the documented shape."""
    from fno.health_monitor import dispatch_notifications, Breach
    import urllib.request

    captured: dict = {}

    class FakeResp:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

    def fake_urlopen(req, timeout=None):
        captured["url"] = req.full_url
        captured["body"] = req.data.decode("utf-8")
        captured["headers"] = dict(req.headers)
        return FakeResp()

    monkeypatch.setattr(urllib.request, "urlopen", fake_urlopen)

    breach = Breach(
        key="idea_pile_depth",
        actual=30,
        threshold=25,
        severity="info",
        message="x",
    )
    cfg = {
        "enabled": True,
        "notifications": {
            "surfaces": ["webhook"],
            "discord_channel": None,
            "webhook_url": "https://example.com/hook",
            "throttle_minutes": 60,
        },
    }
    dispatch_notifications(
        _healthy_report(), [breach],
        config=cfg,
        throttle_path=tmp_path / "throttle.json",
        alert_log_path=tmp_path / "alerts.log",
    )
    assert captured["url"] == "https://example.com/hook"
    payload = json.loads(captured["body"])
    assert payload["scope"] == _healthy_report()["scope"]
    assert payload["breaches"][0]["key"] == "idea_pile_depth"
    assert "timestamp" in payload
    # Content-Type header set so receiving services can parse correctly.
    assert any("application/json" in v.lower() for v in captured["headers"].values())


def test_load_config_malformed_yaml_falls_back_to_defaults(tmp_path, caplog):
    """Broken project YAML degrades to defaults with a logged warning,
    not a crash (config is loaded through the model, which warns via the
    logger rather than printing to stderr)."""
    import logging

    from fno.health_monitor import load_config, DEFAULT_CONFIG

    project = tmp_path / "settings.yaml"
    project.write_text("config:\n  health_monitor:\n    not: valid: yaml: [\n")
    with caplog.at_level(logging.WARNING, logger="fno.config"):
        out = load_config(project_settings=project, user_settings=tmp_path / "missing.yaml")
    # Defaults applied across the board.
    assert out["enabled"] == DEFAULT_CONFIG["enabled"]
    assert out["thresholds"]["idea_pile_depth"] == DEFAULT_CONFIG["thresholds"]["idea_pile_depth"]
    # Operator-visible warning about the unparseable file.
    assert any("failed to parse" in r.message.lower() for r in caplog.records)


def test_breach_post_init_rejects_invalid_severity():
    """Direct Breach(...) with bogus severity raises immediately."""
    from fno.health_monitor import Breach

    with pytest.raises(ValueError, match="severity"):
        Breach(
            key="idea_pile_depth",
            actual=30,
            threshold=25,
            severity="critical",  # not in the Severity literal set
            message="x",
        )


def test_breach_post_init_rejects_negative_actual():
    from fno.health_monitor import Breach

    with pytest.raises(ValueError, match="non-negative"):
        Breach(
            key="idea_pile_depth",
            actual=-5,
            threshold=25,
            severity="info",
            message="x",
        )


# ---------------------------------------------------------------------------
# project_cwd_mismatch health metric (US3, Task 1.3)
# ---------------------------------------------------------------------------


def _make_pending_node(
    node_id: str,
    *,
    project: str,
    cwd: str,
    completed_at=None,
) -> dict:
    """Build a minimal pending (or done if completed_at set) backlog node."""
    from datetime import datetime, timezone

    return {
        "id": node_id,
        "title": f"node {node_id}",
        "type": "feature",
        "project": project,
        "cwd": cwd,
        "priority": "p2",
        "domain": "code",
        "blocked_by": [],
        "session_id": None,
        "claimed_at": None,
        "completed_at": completed_at,
        "has_brief": False,
        "compacted": False,
        "plan_path": "internal/fno/plans/test-plan.md",
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "status": "done" if completed_at else "ready",
        "collisions_acknowledged": [],
        "supersedes": [],
        "superseded_by": None,
        "source_kind": "organic",
    }


def _write_nodes(graph_path: Path, nodes: list[dict]) -> None:
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(json.dumps({"entries": nodes}, indent=2))


def test_health_mismatch_ac3_hp(tmp_graph, monkeypatch):
    """AC3-HP: pending node with mapped project and cwd != work-map root is
    counted in project_cwd_mismatch and listed in project_cwd_mismatch_nodes."""
    import os

    mapped_root = "/real/project/root"
    wrong_cwd = "/wrong/cwd"

    node = _make_pending_node("ab-mismatch01", project="my-proj", cwd=wrong_cwd)
    _write_nodes(tmp_graph / "graph.json", [node])

    # Patch project_root_from_settings at its import location so cmd_health's
    # lazy import inside the function body is intercepted.
    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda proj: mapped_root if proj == "my-proj" else None,
    )

    result = runner.invoke(app, ["backlog", "triage", "health", "--json", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert data["project_cwd_mismatch"] >= 1
    assert "ab-mismatch01" in data["project_cwd_mismatch_nodes"]


def test_health_mismatch_ac3_err_unmapped_not_counted(tmp_graph, monkeypatch):
    """AC3-ERR: pending node whose project has no work-map entry is not counted."""
    node = _make_pending_node(
        "ab-unmapped01", project="unknown-proj", cwd="/some/cwd"
    )
    _write_nodes(tmp_graph / "graph.json", [node])

    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda proj: None,  # no mapping for any project
    )

    result = runner.invoke(app, ["backlog", "triage", "health", "--json", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert data["project_cwd_mismatch"] == 0
    assert data["project_cwd_mismatch_nodes"] == []


def test_health_mismatch_ac3_ui_zero_when_clean(tmp_graph, monkeypatch):
    """AC3-UI: when all pending nodes agree with the work-map, report contains
    explicit project_cwd_mismatch: 0 (not absent)."""
    import os

    mapped_root = os.path.abspath("/the/same/root")
    node = _make_pending_node(
        "ab-clean01", project="clean-proj", cwd=mapped_root
    )
    _write_nodes(tmp_graph / "graph.json", [node])

    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda proj: mapped_root if proj == "clean-proj" else None,
    )

    result = runner.invoke(app, ["backlog", "triage", "health", "--json", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert "project_cwd_mismatch" in data
    assert data["project_cwd_mismatch"] == 0


def test_health_mismatch_ac3_edge_done_not_counted(tmp_graph, monkeypatch):
    """AC3-EDGE: a done node (completed_at set) with historical cwd mismatch
    is NOT counted in project_cwd_mismatch."""
    from datetime import datetime, timezone

    mapped_root = "/real/root"
    wrong_cwd = "/totally/wrong"

    done_node = _make_pending_node(
        "ab-done01",
        project="my-proj",
        cwd=wrong_cwd,
        completed_at=datetime.now(timezone.utc).isoformat(),
    )
    _write_nodes(tmp_graph / "graph.json", [done_node])

    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda proj: mapped_root if proj == "my-proj" else None,
    )

    result = runner.invoke(app, ["backlog", "triage", "health", "--json", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert data["project_cwd_mismatch"] == 0


def test_health_mismatch_normalization_tilde(tmp_graph, monkeypatch):
    """Normalization edge: cwd stored as ~/x matches abspath expanded root."""
    import os

    home = tmp_graph.parent  # monkeypatched HOME from the tmp_graph fixture
    mapped_root = os.path.join(str(home), "myproject")
    tilde_cwd = "~/myproject"

    node = _make_pending_node("ab-tilde01", project="tilde-proj", cwd=tilde_cwd)
    _write_nodes(tmp_graph / "graph.json", [node])

    # Override HOME so expanduser resolves correctly
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda proj: mapped_root if proj == "tilde-proj" else None,
    )

    result = runner.invoke(app, ["backlog", "triage", "health", "--json", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    # ~/myproject normalized == mapped_root -> no mismatch
    assert data["project_cwd_mismatch"] == 0


def test_health_mismatch_ac3_fr_evaluate_thresholds_breach():
    """AC3-FR: evaluate_thresholds reports breach when project_cwd_mismatch=1
    and threshold=0; no breach when project_cwd_mismatch=0."""
    from fno.health_monitor import evaluate_thresholds

    # 1 mismatch -> breach
    report_with_mismatch = _healthy_report()
    report_with_mismatch["project_cwd_mismatch"] = 1
    report_with_mismatch["project_cwd_mismatch_nodes"] = ["ab-test0001"]
    report_with_mismatch["totals"]["project_cwd_mismatch"] = 1

    cfg = {
        "enabled": True,
        "thresholds": {
            "idea_pile_depth": 25,
            "stale_ready_days": 30,
            "failure_prone_attempts": 2,
            "collision_count": 3,
            "project_cwd_mismatch": 0,
        },
    }
    breaches = evaluate_thresholds(report_with_mismatch, config=cfg)
    breach_keys = [b.key for b in breaches]
    assert "project_cwd_mismatch" in breach_keys
    mismatch_breach = next(b for b in breaches if b.key == "project_cwd_mismatch")
    assert mismatch_breach.actual == 1
    assert mismatch_breach.threshold == 0
    assert mismatch_breach.severity == "alert"  # zero threshold -> full severity

    # 0 mismatches -> no breach
    report_clean = _healthy_report()
    report_clean["project_cwd_mismatch"] = 0
    report_clean["project_cwd_mismatch_nodes"] = []
    report_clean["totals"]["project_cwd_mismatch"] = 0

    breaches_clean = evaluate_thresholds(report_clean, config=cfg)
    assert not any(b.key == "project_cwd_mismatch" for b in breaches_clean)


def test_health_mismatch_check_exit4(tmp_graph, monkeypatch):
    """--check exits 4 when project_cwd_mismatch breaches threshold=0."""
    import os

    mapped_root = "/real/root"
    wrong_cwd = "/wrong/cwd"

    node = _make_pending_node("ab-check01", project="check-proj", cwd=wrong_cwd)
    _write_nodes(tmp_graph / "graph.json", [node])

    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda proj: mapped_root if proj == "check-proj" else None,
    )

    # Write a project settings that sets project_cwd_mismatch threshold to 0
    settings_text = (
        "config:\n"
        "  health_monitor:\n"
        "    enabled: true\n"
        "    thresholds:\n"
        "      project_cwd_mismatch: 0\n"
        "    notifications:\n"
        "      surfaces: [log_only]\n"
    )
    abilities_dir = tmp_graph.parent / ".fno"
    abilities_dir.mkdir(exist_ok=True)
    (abilities_dir / "settings.yaml").write_text(settings_text)
    monkeypatch.chdir(tmp_graph.parent)

    result = runner.invoke(
        app, ["backlog", "triage", "health", "--check", "--all"]
    )
    assert result.exit_code == 4, (
        f"expected exit 4 on project_cwd_mismatch breach, got {result.exit_code}\n"
        f"stdout: {result.output}"
    )
