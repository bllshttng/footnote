"""Blast-radius size-modulation tests for `fno target init` (task 1.2, x-518f).

Two layers:
  * ``_modulate_size`` - the pure floor-up / cautious-down / fail-safe decision
    (the full AC matrix: AC1-HP, AC2-HP, AC2-EDGE, AC2-FR, unknown).
  * ``init`` integration - the env ``TARGET_SIZE`` actually handed to the bash
    writer is the modulated value, with the bash exec + script-resolve stubbed
    so no real state is written (AC2-HP floor over explicit S, AC1-HP downgrade,
    AC2-EDGE explicit respected, AC1-FR disabled byte-identical, AC1-ERR/EDGE
    fail-safe).
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import target_cli
from fno.config import BlastConfig
from fno.target_cli import _modulate_size, target_app

runner = CliRunner()


# ----------------------------- pure decision ------------------------------ #
def test_modulate_low_unpinned_downgrades_to_s():
    # AC1-HP
    eff, note = _modulate_size(
        "low", size_explicit=False, operator_size=None, downgrade=True
    )
    assert eff == "S"
    assert note and "fast path S" in note


def test_modulate_high_floors_over_explicit_s():
    # AC2-HP: explicit S is raised to the M floor.
    eff, note = _modulate_size(
        "high",
        size_explicit=True,
        operator_size="S",
        downgrade=True,
        matched_paths=["scripts/lib/x.sh"],
    )
    assert eff == "M"
    assert note and "scripts/lib/x.sh" in note


def test_modulate_high_default_floors_to_m():
    eff, _ = _modulate_size(
        "high", size_explicit=False, operator_size=None, downgrade=True
    )
    assert eff == "M"


def test_modulate_high_explicit_l_stays_l():
    eff, _ = _modulate_size(
        "high", size_explicit=True, operator_size="L", downgrade=True
    )
    assert eff == "L"  # floor never lowers a higher ceremony


def test_modulate_low_explicit_is_respected():
    # AC2-EDGE: an explicit size is never downgraded.
    eff, note = _modulate_size(
        "low", size_explicit=True, operator_size="M", downgrade=True
    )
    assert eff is None and note is None


def test_modulate_safety_only_mode_no_downgrade():
    # AC2-FR: downgrade=False -> low never drops, high still floors.
    eff_low, _ = _modulate_size(
        "low", size_explicit=False, operator_size=None, downgrade=False
    )
    assert eff_low is None
    eff_high, _ = _modulate_size(
        "high", size_explicit=False, operator_size=None, downgrade=False
    )
    assert eff_high == "M"


def test_modulate_unknown_is_unchanged():
    eff, note = _modulate_size(
        "unknown", size_explicit=False, operator_size=None, downgrade=True
    )
    assert eff is None and note is None


# ----------------------------- init integration --------------------------- #
@pytest.fixture()
def stub_exec(monkeypatch, tmp_path):
    """Stub the script resolve + bash exec so init writes no real state.

    Returns a one-element list that captures the env dict passed to the fake
    ``subprocess.run`` so tests can assert on ``TARGET_SIZE``.
    """
    fake_script = tmp_path / "init-target-state.sh"
    fake_script.write_text("#!/usr/bin/env bash\n", encoding="utf-8")
    monkeypatch.setattr(target_cli, "_resolve_init_script", lambda: fake_script)

    captured: list[dict] = []

    class _Result:
        returncode = 0

    # Intercept ONLY the bash init-script call; pass everything else (the git
    # rev-parse that resolve_repo_root shells out for, reached transitively via
    # the blast read's graph_json) through to real subprocess. Patching the
    # global subprocess.run with a stub that breaks git made the suite
    # order-dependent (it only passed once another test had warmed
    # resolve_repo_root's lru_cache).
    real_run = target_cli.subprocess.run

    def _fake_run(cmd, *args, **kwargs):
        if (
            isinstance(cmd, (list, tuple))
            and len(cmd) >= 2
            and str(cmd[0]) == "bash"
            and str(cmd[1]) == str(fake_script)
        ):
            captured.append(dict(kwargs.get("env") or {}))
            return _Result()
        return real_run(cmd, *args, **kwargs)

    monkeypatch.setattr(target_cli.subprocess, "run", _fake_run)
    return captured


def _plan(tmp_path: Path, owner_rows: str) -> str:
    p = tmp_path / "plan.md"
    p.write_text(
        "# Plan\n\n## File Ownership Map\n\n| File | Action | Owner |\n|---|---|---|\n"
        + owner_rows,
        encoding="utf-8",
    )
    return str(p)


def _enable(monkeypatch, **kw):
    monkeypatch.setattr(target_cli, "_load_blast_cfg", lambda: BlastConfig(enabled=True, **kw))


def _invoke(args):
    return runner.invoke(target_app, ["init", *args])


def test_init_high_floors_over_explicit_s(stub_exec, monkeypatch, tmp_path):
    # AC2-HP: a control-plane path + explicit S -> manifest TARGET_SIZE=M.
    _enable(monkeypatch)
    plan = _plan(tmp_path, "| `db/migrations/2026.sql` | modify | 1.1 |\n")
    res = _invoke(["--plan-path", plan, "--size", "S"])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "M"


def test_init_low_unpinned_downgrades(stub_exec, monkeypatch, tmp_path):
    # AC1-HP: low blast + no size -> TARGET_SIZE=S.
    _enable(monkeypatch)
    plan = _plan(tmp_path, "| `src/widget.ts` | modify | 1.1 |\n")
    res = _invoke(["--plan-path", plan])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "S"


def test_init_low_explicit_respected(stub_exec, monkeypatch, tmp_path):
    # AC2-EDGE: explicit M on a low plan stays M.
    _enable(monkeypatch)
    plan = _plan(tmp_path, "| `src/widget.ts` | modify | 1.1 |\n")
    res = _invoke(["--plan-path", plan, "--size", "M"])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "M"


def test_init_disabled_is_byte_identical(stub_exec, monkeypatch, tmp_path):
    # AC1-FR: disabled -> no modulation. No size passed => TARGET_SIZE absent.
    monkeypatch.setattr(target_cli, "_load_blast_cfg", lambda: BlastConfig(enabled=False))
    plan = _plan(tmp_path, "| `db/migrations/2026.sql` | modify | 1.1 |\n")
    res = _invoke(["--plan-path", plan])
    assert res.exit_code == 0
    assert "TARGET_SIZE" not in stub_exec[0]


def test_init_empty_map_is_unchanged(stub_exec, monkeypatch, tmp_path):
    # AC1-EDGE: no parseable ownership map -> unknown -> explicit size untouched.
    _enable(monkeypatch)
    p = tmp_path / "plan.md"
    p.write_text("# Plan\n\nNo ownership map.\n", encoding="utf-8")
    res = _invoke(["--plan-path", str(p), "--size", "S"])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "S"


def test_init_node_input_is_covered(stub_exec, monkeypatch, tmp_path):
    # Locked Decision 2: a bare node-id --input (no --plan-path) resolves to the
    # node's plan and modulates (the megawalk/megatron path).
    plan = _plan(tmp_path, "| `db/migrations/x.sql` | modify | 1.1 |\n")
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda *a, **k: [{"id": "x-518f", "plan_path": plan}],
    )
    _enable(monkeypatch)
    res = _invoke(["--input", "x-518f", "--size", "S"])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "M"  # high blast floors S -> M


def test_init_modifier_prefixed_node_is_covered(stub_exec, monkeypatch, tmp_path):
    # A modifier-prefixed node input ("no-merge <id>", the auto-continue path)
    # IS covered: the id token resolves even though the arg carries modifiers.
    plan = _plan(tmp_path, "| `db/migrations/x.sql` | modify | 1.1 |\n")
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda *a, **k: [{"id": "x-518f", "plan_path": plan}],
    )
    _enable(monkeypatch)
    res = _invoke(["--input", "no-merge x-518f", "--size", "S"])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "M"  # high blast floors S -> M


def test_init_env_pinned_size_not_downgraded(stub_exec, monkeypatch, tmp_path):
    # An env-pinned TARGET_SIZE (no --size flag) is explicit: a low-blast plan
    # must NOT strip it down to S.
    monkeypatch.setenv("TARGET_SIZE", "L")
    plan = _plan(tmp_path, "| `src/widget.ts` | modify | 1.1 |\n")
    _enable(monkeypatch)
    res = _invoke(["--input", "feature", "--plan-path", plan])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "L"


def test_resolve_plan_for_blast_matrix(monkeypatch, tmp_path):
    from fno.target_cli import _resolve_plan_for_blast

    # explicit plan_path always wins, no graph load needed
    assert _resolve_plan_for_blast("a/b/plan.md", "x-518f") == "a/b/plan.md"

    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda *a, **k: [
            {"id": "x-518f", "plan_path": "p/blast.md"},
            {"id": "ab-other", "plan_path": "p/other.md"},
        ],
    )
    assert _resolve_plan_for_blast(None, "x-518f") == "p/blast.md"        # clean id
    assert _resolve_plan_for_blast(None, "no-merge x-518f") == "p/blast.md"  # modifier
    assert _resolve_plan_for_blast(None, "X-518F") == "p/blast.md"        # case-insensitive
    assert _resolve_plan_for_blast(None, "x-518f ab-other") is None       # ambiguous -> skip
    assert _resolve_plan_for_blast(None, "fix the auth bug") is None      # free text
    assert _resolve_plan_for_blast(None, "ab-unknown") is None            # no such id
    assert _resolve_plan_for_blast(None, "") is None                      # empty


def test_resolve_plan_for_blast_malformed_graph(monkeypatch):
    # Defensive: a non-list graph / non-dict entries never raise -> None.
    from fno.target_cli import _resolve_plan_for_blast

    monkeypatch.setattr("fno.graph.load.load_graph", lambda *a, **k: "not-a-list")
    assert _resolve_plan_for_blast(None, "x-518f") is None
    monkeypatch.setattr("fno.graph.load.load_graph", lambda *a, **k: ["junk", 42])
    assert _resolve_plan_for_blast(None, "x-518f") is None


def test_init_classifier_error_is_non_blocking(stub_exec, monkeypatch, tmp_path):
    # AC1-ERR: a classify() blow-up degrades to unknown, init still proceeds.
    _enable(monkeypatch)
    plan = _plan(tmp_path, "| `db/migrations/2026.sql` | modify | 1.1 |\n")

    def _boom(*a, **k):
        raise RuntimeError("classifier exploded")

    monkeypatch.setattr("fno.target.blast.classify", _boom)
    res = _invoke(["--plan-path", plan, "--size", "L"])
    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_SIZE") == "L"  # unchanged
