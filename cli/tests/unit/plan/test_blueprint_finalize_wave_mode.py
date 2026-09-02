"""finalize() computes each wave's ``mode`` from the surfaces it can see.

The mint writes one wave with ``mode: sequential`` and every task surface
empty, because at mint time there is nothing else to compare against and no
surface to read. Finalize is the first point where surfaces are filled, so it
is where the intra-wave fan-out decision belongs: disjoint non-empty surfaces
run parallel, a shared path or any empty surface stays sequential.

This buys fan-out inside a wave and nothing else. Wave ``mode`` never widens
the plan's join width - the width function adds derived edges only when a
wave is parallel, so flipping to parallel can only narrow width - and these
tests pin that this change does not claim otherwise.
"""
from __future__ import annotations

import importlib.util
import re
import sys
from collections import OrderedDict
from pathlib import Path

import yaml

_REPO_ROOT = Path(__file__).resolve().parents[4]
_SCRIPT_PATH = _REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"


def _load_mutate_module():
    spec = importlib.util.spec_from_file_location("mutate_doc_wave_mode", _SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutate_doc_wave_mode"] = module
    spec.loader.exec_module(module)
    return module


_mutate_doc = _load_mutate_module()


def _doc(waves: str, tasks: str) -> str:
    return f"""---
title: wave mode at finalize
status: design
created: 2026-09-02
difficulty: medium
node: x-wavemode
---

# Wave mode at finalize

## Acceptance Criteria

### AC1-HP: The thing works

**Given** a finalized plan
**When** a worker reads the wave
**Then** the mode matches its surfaces.

## Execution Strategy

```yaml
execution_mode: mixed
waves:
{waves}tasks:
{tasks}```
"""


def _finalize(tmp_path: Path, waves: str, tasks: str):
    doc = tmp_path / "plan.md"
    doc.write_text(_doc(waves, tasks), encoding="utf-8")
    rc, out = _mutate_doc.finalize(doc, no_emit=True)
    return rc, out


def _wave_modes(out: str) -> dict:
    block = re.search(r"```yaml\n(.*?)```", out, re.S).group(1)
    return {w["wave"]: w["mode"] for w in yaml.safe_load(block)["waves"]}


_WAVES_2 = """  - wave: 1
    mode: sequential
    name: One
    difficulty: medium
    tasks: ['1.1', '1.2']
  - wave: 2
    mode: sequential
    name: Two
    difficulty: medium
    tasks: ['2.1']
"""


def _tasks(*surfaces_12: str) -> str:
    """Task 1.1 always carries alpha.py; 1.2 carries what is given."""
    s12 = f"    surface: {surfaces_12[0]}\n" if surfaces_12 else "    surface: []\n"
    return f"""  - id: '1.1'
    title: A
    surface: [cli/src/alpha.py]
    verify: pytest cli/tests/unit/plan -q
    acceptance: [AC1-HP]
  - id: '1.2'
    title: B
{s12}    verify: pytest cli/tests/unit/plan -q
    acceptance: [AC1-HP]
  - id: '2.1'
    title: C
    surface: [cli/src/gamma.py]
    verify: pytest cli/tests/unit/plan -q
    acceptance: [AC1-HP]
"""


def test_finalize_flips_disjoint_surface_waves_to_parallel(tmp_path):
    """AC9: every task surfaces a file and no two waves-mates share one."""
    rc, out = _finalize(tmp_path, _WAVES_2, _tasks("[cli/src/beta.py]"))
    assert rc == 0, out
    assert _wave_modes(out) == {1: "parallel", 2: "parallel"}


def test_finalize_keeps_wave_sequential_when_any_surface_is_empty(tmp_path):
    """AC10: an empty surface means fan-out was never assessed; the whole
    wave stays sequential rather than guessing.

    The decision is a pure function because finalize's own gate refuses an
    empty-surface plan outright ("task surface must name at least one
    file"), so the doc-level path can never exercise this branch - but the
    mint's default tasks all carry empty surfaces, and any other caller of
    the rule (a dry-run, a future lint) gets the honest answer, not a
    guess."""
    assert _mutate_doc._wave_mode_from_surfaces([]) == "sequential"
    assert _mutate_doc._wave_mode_from_surfaces([set(), {"a.py"}]) == "sequential"


def test_finalize_keeps_wave_sequential_when_surfaces_share_a_path(tmp_path):
    """AC10: two tasks editing one file must not run concurrently."""
    rc, out = _finalize(tmp_path, _WAVES_2, _tasks("[cli/src/alpha.py, cli/src/beta.py]"))
    assert rc == 0, out
    assert _wave_modes(out) == {1: "sequential", 2: "parallel"}


def test_mint_still_writes_sequential(tmp_path):
    """AC10's other half: the mint is unchanged. Its single wave carries no
    surfaces, so sequential is both what it writes and what finalize would
    compute - the pin stops a future mint edit from assuming finalize cleans
    up after it."""
    block = _mutate_doc._build_execution_strategy(OrderedDict(), "medium")
    assert "mode: sequential" in block
