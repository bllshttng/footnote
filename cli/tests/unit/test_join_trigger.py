"""The read-only probes target init asks before firing join.

``width`` must answer with a real measurement or nothing: an unmeasured
plan must not read as width 1, because "nothing to join" and "could not
measure" are different refusals and the caller treats them differently.
``armed`` must route through the canonical auto-continue resolver, because
the autonomy master switch outranks the env override and a bash
re-implementation of that precedence is how the two drift apart.
"""
from __future__ import annotations

import types

from fno.backlog.join_trigger import main

_PLAN = """---
title: width probe fixture
status: design
created: 2026-09-02
difficulty: medium
node: x-widthprobe
---

# Width probe fixture

## Acceptance Criteria

### AC1-HP: The thing works

**Given** a plan
**When** width is measured
**Then** one wave of two unblocked tasks measures two.

## Execution Strategy

```yaml
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    name: One
    difficulty: medium
    tasks: ['1.1', '1.2']
tasks:
  - id: '1.1'
    title: A
    surface: [a.py]
    verify: pytest -q
    acceptance: [AC1-HP]
    blocked_by: []
  - id: '1.2'
    title: B
    surface: [b.py]
    verify: pytest -q
    acceptance: [AC1-HP]
    blocked_by: []
```
"""


def _write_plan(tmp_path):
    plan = tmp_path / "plan.md"
    plan.write_text(_PLAN, encoding="utf-8")
    return plan


def test_width_measures_one_wave_two_unblocked_tasks_as_two(tmp_path, capsys):
    plan = _write_plan(tmp_path)
    assert main(["width", str(plan)]) == 0
    assert capsys.readouterr().out.strip() == "2"


def test_width_unreadable_plan_answers_nothing(capsys, tmp_path):
    assert main(["width", str(tmp_path / "absent.md")]) == 1
    assert capsys.readouterr().out == ""


def test_width_bad_usage_answers_with_usage(capsys):
    assert main([]) == 2
    assert "usage" in capsys.readouterr().err


def _arm_master(monkeypatch, enabled: bool) -> None:
    monkeypatch.setattr(
        "fno.config.autonomy_master_enabled", lambda root=None: enabled
    )


def test_armed_false_when_autonomy_master_is_off(monkeypatch, capsys):
    # The panic switch outranks the env override by design, so even an
    # explicit FNO_AUTO_CONTINUE=1 stays off with rank autonomy.
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")
    _arm_master(monkeypatch, False)
    assert main(["armed", ""]) == 0
    assert capsys.readouterr().out.strip() == "armed=false rank=autonomy"


def test_armed_env_override_wins_over_config(monkeypatch, capsys):
    _arm_master(monkeypatch, True)
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "0")

    def _fake_settings(root):
        return types.SimpleNamespace(
            auto_continue=types.SimpleNamespace(enabled=True)
        )

    monkeypatch.setattr("fno.config.load_settings_for_repo", _fake_settings)
    assert main(["armed", "/anywhere"]) == 0
    assert capsys.readouterr().out.strip() == "armed=false rank=env"


def test_armed_config_rank_supplies_the_answer(monkeypatch, capsys):
    _arm_master(monkeypatch, True)
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)

    def _fake_settings(root):
        return types.SimpleNamespace(
            auto_continue=types.SimpleNamespace(enabled=True)
        )

    monkeypatch.setattr("fno.config.load_settings_for_repo", _fake_settings)
    assert main(["armed", "/anywhere"]) == 0
    assert capsys.readouterr().out.strip() == "armed=true rank=config"
