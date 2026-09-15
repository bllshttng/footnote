"""`fno doctor evals run` CLI exit-code contract (no spawn / no worktree needed)."""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.evals.cli import evals_app
from fno.rust_binary import find_dev_binary

runner = CliRunner()

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


def _bank(tmp_path: Path, name: str, body: str) -> Path:
    d = tmp_path / "bank"
    d.mkdir(exist_ok=True)
    (d / name).write_text(body, encoding="utf-8")
    return d


def test_no_bank_exits_1(tmp_path: Path) -> None:
    res = runner.invoke(evals_app, ["run", "--bank", str(tmp_path / "nope")])
    assert res.exit_code == 1
    assert "no bank" in res.stdout.lower() or "no bank" in (res.stderr or "").lower()


def test_invalid_bank_task_exits_2(tmp_path: Path) -> None:
    d = _bank(tmp_path, "bad.yaml", "id: b\ntier: regression\ngrade: []\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d)])
    assert res.exit_code == 2


def test_empty_selection_exits_1(tmp_path: Path) -> None:
    d = _bank(tmp_path, "a.yaml",
              "id: a\ntier: regression\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d), "--task", "does-not-exist"])
    assert res.exit_code == 1


def test_bad_repeat_exits_1(tmp_path: Path) -> None:
    d = _bank(tmp_path, "a.yaml",
              "id: a\ntier: regression\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d), "--repeat", "0"])
    assert res.exit_code == 1


def test_prompt_task_defaults_provider(tmp_path: Path, monkeypatch) -> None:
    """codex P2: a prompt-bearing task with no --provider defaults to claude so
    the headless spawn is never provider-less. Assert run_task gets a provider."""
    import fno.evals.runner as runner_mod

    seen: dict = {}

    def fake_run_task(task, *, repeat, repo_root, worker_provider=None, **kw):
        seen["provider"] = worker_provider
        return [runner_mod.RunResult(task.id, task.tier, True, "", 0.0, 0)]

    # run_command does `from fno.evals.runner import run_task, sweep_orphans` at
    # call time, so patching the source module is what the import resolves.
    monkeypatch.setattr(runner_mod, "run_task", fake_run_task)
    monkeypatch.setattr(runner_mod, "sweep_orphans", lambda root: 0)

    d = _bank(tmp_path, "cap.yaml",
              "id: cap\ntier: capability\nprompt: do it\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d)])
    assert res.exit_code == 0
    assert seen.get("provider") == "claude"
    assert "defaulting" in res.stdout


# --- variant axis ---

def test_bad_variant_name_exits_1_before_bank_load(tmp_path: Path) -> None:
    # The bank dir does not exist: reaching "no bank" would mean validation
    # never ran; the variant refusal must come first.
    res = runner.invoke(evals_app, ["run", "--bank", str(tmp_path / "nope"),
                                    "--variant", "round2"])
    assert res.exit_code == 1
    out = res.stdout + (res.stderr or "")
    assert "must be 'baseline' or 'v<N>', got 'round2'" in out


def test_variant_without_ref_exits_1(tmp_path: Path) -> None:
    res = runner.invoke(evals_app, ["run", "--bank", str(tmp_path / "nope"),
                                    "--variant", "v1"])
    assert res.exit_code == 1
    assert "--ref" in res.stdout + (res.stderr or "")


def test_ref_with_baseline_variant_exits_1(tmp_path: Path) -> None:
    res = runner.invoke(evals_app, ["run", "--bank", str(tmp_path / "nope"),
                                    "--ref", "HEAD"])
    assert res.exit_code == 1
    assert "--variant" in res.stdout + (res.stderr or "")


def test_variant_flags_pass_through_to_run_task(tmp_path: Path, monkeypatch) -> None:
    import fno.evals.runner as runner_mod

    seen: dict = {}

    def fake_run_task(task, *, repeat, repo_root, worker_provider=None,
                      variant="baseline", variant_ref=None, **kw):
        seen["variant"] = variant
        seen["variant_ref"] = variant_ref
        return [runner_mod.RunResult(task.id, task.tier, True, "", 0.0, 0, variant)]

    monkeypatch.setattr(runner_mod, "run_task", fake_run_task)
    monkeypatch.setattr(runner_mod, "sweep_orphans", lambda root: 0)

    d = _bank(tmp_path, "a.yaml",
              "id: a\ntier: regression\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d),
                                    "--variant", "v2", "--ref", "HEAD"])
    assert res.exit_code == 0
    assert seen == {"variant": "v2", "variant_ref": "HEAD"}
    assert "a [v2]" in res.stdout  # the summary line names the round


@requires_rust
def test_report_compare_cli(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv("FNO_AGENTS_BIN", str(find_dev_binary()))
    hp = tmp_path / "h.jsonl"
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": True,
                         "variant": "baseline", "bank_rev": "aaa"})
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": False,
                         "variant": "baseline", "bank_rev": "aaa"})
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": True,
                         "variant": "v1", "bank_rev": "bbb"})
    res = runner.invoke(evals_app, ["report", "--history", str(hp), "--compare", "v1"])
    assert res.exit_code == 0  # a compare view never fires the alarm exit
    assert "baseline" in res.stdout and "v1" in res.stdout
    assert "improved" in res.stdout
    assert "git diff aaa bbb" in res.stdout


@requires_rust
def test_report_compare_json_cli(tmp_path: Path, monkeypatch) -> None:
    import json

    monkeypatch.setenv("FNO_AGENTS_BIN", str(find_dev_binary()))

    hp = tmp_path / "h.jsonl"
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": True,
                         "variant": "baseline"})
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": False,
                         "variant": "v1"})
    res = runner.invoke(evals_app, ["report", "--history", str(hp),
                                    "--compare", "v1", "--json"])
    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    assert payload["variant"] == "v1"
    assert payload["tasks"]["t"]["verdict"] == "regressed"


@requires_rust
def test_report_compare_rejects_bad_name(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv("FNO_AGENTS_BIN", str(find_dev_binary()))
    res = runner.invoke(evals_app, ["report", "--history", str(tmp_path / "h.jsonl"),
                                    "--compare", "round2"])
    out = res.stdout + (res.stderr or "")
    assert res.exit_code == 1
    assert "must be 'baseline' or 'v<N>', got 'round2'" in out


@requires_rust
def test_report_compare_honors_since(tmp_path: Path, monkeypatch) -> None:
    import json

    monkeypatch.setenv("FNO_AGENTS_BIN", str(find_dev_binary()))

    hp = tmp_path / "h.jsonl"
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": False,
                         "variant": "baseline"})
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": True,
                         "variant": "v1"})
    _history_append(hp, {"task_id": "t", "tier": "regression", "pass": True,
                         "variant": "v1"})
    res = runner.invoke(evals_app, ["report", "--history", str(hp),
                                    "--compare", "v1", "--since", "2", "--json"])
    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    # The window is the last 2 rows overall: two v1 rows, no baseline in it.
    assert payload["tasks"] == {}
    assert payload["missing_in_baseline"] == ["t"]


def _history_append(path: Path, row: dict) -> None:
    from fno.evals import history as _history

    _history.append_row(path, row)


# --- lane / cohort flags (x-fd52) ---

def test_unknown_lane_exits_1_before_bank_load(tmp_path: Path) -> None:
    res = runner.invoke(evals_app, ["run", "--bank", str(tmp_path / "nope"),
                                    "--lane", "no-such-lane"])
    assert res.exit_code == 1
    out = res.stdout + (res.stderr or "")
    assert "unknown lane" in out


def test_lane_and_cohort_flags_pass_through_to_run_task(tmp_path: Path, monkeypatch) -> None:
    import fno.evals.bank as bank_mod
    import fno.evals.runner as runner_mod

    from fno.route_resolve import InventoryRow

    seen: dict = {}
    monkeypatch.setattr(
        bank_mod, "resolve_lane",
        lambda name, **kw: InventoryRow(name=name, harness="codex", model="gpt-6-astra", effort="high"),
    )

    def fake_run_task(task, *, repeat, repo_root, worker_provider=None, **kw):
        seen["lane"] = kw.get("lane")
        seen["experiment_id"] = kw.get("experiment_id")
        seen["provider"] = worker_provider
        return [runner_mod.RunResult(task.id, task.tier, True, "", 0.0, 0)]

    monkeypatch.setattr(runner_mod, "run_task", fake_run_task)
    monkeypatch.setattr(runner_mod, "sweep_orphans", lambda root: 0)

    d = _bank(tmp_path, "cap.yaml",
              "id: cap\ntier: capability\nprompt: do it\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d), "--lane", "astra-high",
                                    "--cohort", "cohort-a"])
    assert res.exit_code == 0
    assert seen["lane"].name == "astra-high"
    assert seen["experiment_id"] == "cohort-a"
    # A lane is a complete coordinate; the provider default must not override it.
    assert seen["provider"] is None


def _macro_journal(path: Path, *, old: bool = False) -> None:
    ts = "2025-01-01T10:00:00Z" if old else "2026-09-12T10:00:00Z"
    rows = [
        {"ts": ts, "type": "loop_check_watch_idle", "data": {"session_id": "s1", "node_id": "n1", "reason": "ci"}},
        {"ts": ts, "type": "termination", "data": {"session_id": "s1", "node_id": "n1", "reason": "Budget"}},
        {"ts": ts, "type": "loop_check_watch_idle", "data": {"session_id": "s2", "node_id": "n2", "reason": "ci"}},
        {"ts": ts, "type": "termination", "data": {"session_id": "s2", "node_id": "n2", "reason": "Budget"}},
    ]
    path.write_text("\n".join(json.dumps(row) for row in rows) + "\n", encoding="utf-8")


def _capture_child_stdout(monkeypatch) -> list[str]:
    """The child binary inherits real stdout, which CliRunner cannot see;
    capture it and hand back the child outputs in call order."""
    import subprocess

    real_run = subprocess.run
    outputs: list[str] = []

    def run_capture(argv, check=False, **kw):
        proc = real_run(argv, check=check, capture_output=True, text=True)
        outputs.append(proc.stdout)
        return proc

    monkeypatch.setattr(subprocess, "run", run_capture)
    return outputs


@requires_rust
def test_macro_json_and_topic_drilldown(tmp_path: Path, monkeypatch) -> None:
    """The delegation end to end: the CLI shells the dev binary, which folds."""
    monkeypatch.setenv("FNO_AGENTS_BIN", str(find_dev_binary()))
    outputs = _capture_child_stdout(monkeypatch)
    journal = tmp_path / "events.jsonl"
    _macro_journal(journal)

    result = runner.invoke(evals_app, ["macro", "--events", str(journal), "--json"])

    assert result.exit_code == 0
    payload = json.loads(outputs[0])
    assert payload["leaderboard"][0]["pattern"] == "termination:Budget"

    drill = runner.invoke(evals_app, ["macro", "--events", str(journal),
                                      "--topic", "termination:Budget"])
    assert drill.exit_code == 0
    assert "s1" in outputs[1] and "s2" in outputs[1]
    assert "termination:Budget" in outputs[1]


@requires_rust
def test_macro_topic_error_and_empty_window(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv("FNO_AGENTS_BIN", str(find_dev_binary()))
    outputs = _capture_child_stdout(monkeypatch)
    journal = tmp_path / "events.jsonl"
    _macro_journal(journal)

    missing = runner.invoke(evals_app, ["macro", "--events", str(journal),
                                       "--topic", "nope:never"])
    assert missing.exit_code == 1
    assert "patterns present" in outputs[0]

    old_journal = tmp_path / "old-events.jsonl"
    _macro_journal(old_journal, old=True)
    empty = runner.invoke(evals_app, ["macro", "--events", str(old_journal),
                                      "--since", "1h"])
    assert empty.exit_code == 0
    assert "no events since" in outputs[1]
