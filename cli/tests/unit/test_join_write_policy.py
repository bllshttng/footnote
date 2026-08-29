"""Unit tests for the per-band join write policy (sandbox allowWrite layer).

``render_join_write_policy`` renders each band's OS-layer write allowlist from
the plan's own partition: items are ``(band, union of that band's task
surfaces)``, run through ``collision.partition`` at BAND grain. A band that
lands in a singleton group and owns at least one usable path is narrowed
(``verdict: enforced``); an overlapping band and an unevaluated one never are
(LD2: no parseable file list is its own verdict, never a silent pass).

Fixtures are real plan files so the tests exercise the actual parser, the same
way ``test_backlog_join.py`` does; attributes are resolved off the module
inside each test so a missing function fails as an AttributeError naming it,
not as a collection-time import error masking every test in the file.
"""

from __future__ import annotations

from pathlib import Path

from fno.backlog import advance


def _write_plan(tmp_path: Path, waves_yaml: str, tasks_yaml: str) -> Path:
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\ntitle: t\nstatus: ready\ndifficulty: high\n---\n\n"
        "## Execution Strategy\n\n"
        "```yaml\n"
        f"{waves_yaml}"
        f"{tasks_yaml}"
        "```\n"
    )
    return plan


DISJOINT_WAVES = """\
execution_mode: mixed
waves:
  - wave: 1
    mode: parallel
    difficulty: high
    tasks: ['1.1', '1.2']
  - wave: 2
    mode: parallel
    difficulty: medium
    tasks: ['2.1', '2.2']
"""

DISJOINT_TASKS = """\
tasks:
  - id: '1.1'
    title: a
    surface: ['src/high_a.py']
  - id: '1.2'
    title: b
    surface: ['src/high_b.py']
  - id: '2.1'
    title: c
    surface: ['cli/src/fno/med_a.py']
  - id: '2.2'
    title: d
    surface: ['cli/src/fno/med_b.py']
"""


def test_disjoint_bands_render_per_band_allowlist(tmp_path):
    """AC1-HP: each band's allow_write is its own surfaces + INFRA_WRITE_ROOTS."""
    plan = _write_plan(tmp_path, DISJOINT_WAVES, DISJOINT_TASKS)
    graph = advance._plan_task_graph(plan)
    policies = advance.render_join_write_policy(graph, ["high", "medium"])

    assert set(policies) == {"high", "medium"}
    high = policies["high"]
    assert high.verdict == "enforced"
    assert high.allow_write == (
        "src/high_a.py", "src/high_b.py", *advance.INFRA_WRITE_ROOTS
    )
    assert high.deny_edit == ("cli/src/fno/med_a.py", "cli/src/fno/med_b.py")
    medium = policies["medium"]
    assert medium.verdict == "enforced"
    assert medium.allow_write == (
        "cli/src/fno/med_a.py", "cli/src/fno/med_b.py", *advance.INFRA_WRITE_ROOTS
    )
    assert medium.deny_edit == ("src/high_a.py", "src/high_b.py")


OVERLAP_TASKS = """\
tasks:
  - id: '1.1'
    title: a
    surface: ['src/high_a.py', 'cli/src/fno/x.py']
  - id: '2.1'
    title: c
    surface: ['cli/src/fno/x.py']
"""


def test_overlapping_bands_refuse_to_narrow(tmp_path):
    """AC4-EDGE: two bands sharing a file both read overlapping, no allowlist."""
    plan = _write_plan(tmp_path, DISJOINT_WAVES, OVERLAP_TASKS)
    graph = advance._plan_task_graph(plan)
    policies = advance.render_join_write_policy(graph, ["high", "medium"])

    assert policies["high"].verdict == "overlapping"
    assert policies["medium"].verdict == "overlapping"
    assert policies["high"].allow_write is None
    assert policies["medium"].allow_write is None
    assert policies["high"].deny_edit is None


UNEVALUATED_TASKS = """\
tasks:
  - id: '1.1'
    title: a
    surface: ['src/high_a.py']
  - id: '2.1'
    title: c
"""


def test_unevaluated_band_is_never_jailed(tmp_path):
    """AC5-EDGE: a band whose tasks declare no surface gets no policy."""
    plan = _write_plan(tmp_path, DISJOINT_WAVES, UNEVALUATED_TASKS)
    graph = advance._plan_task_graph(plan)
    policies = advance.render_join_write_policy(graph, ["high", "medium"])

    assert policies["medium"].verdict == "unevaluated"
    assert policies["medium"].allow_write is None
    # The evaluated peer is still narrowed to exactly its own surfaces; its
    # deny_edit is empty because the unevaluated band owns nothing declarable.
    assert policies["high"].verdict == "enforced"
    assert policies["high"].allow_write == (
        "src/high_a.py", *advance.INFRA_WRITE_ROOTS
    )
    assert policies["high"].deny_edit == ()


def test_shapeless_band_reads_unevaluated(tmp_path):
    """An unbanded joiner ("") has no band->surface mapping to narrow."""
    plan = _write_plan(tmp_path, DISJOINT_WAVES, DISJOINT_TASKS)
    graph = advance._plan_task_graph(plan)
    policies = advance.render_join_write_policy(graph, [""])
    assert policies[""].verdict == "unevaluated"
    assert policies[""].allow_write is None


def test_task_graph_is_parsed_once_for_width_and_bands(tmp_path):
    """The extracted graph carries what the width walk and band list both read."""
    plan = _write_plan(tmp_path, DISJOINT_WAVES, DISJOINT_TASKS)
    graph = advance._plan_task_graph(plan)
    assert [mode for mode, _tasks in graph.waves] == ["parallel", "parallel"]
    assert graph.wave_bands == ["high", "medium"]
    assert graph.surfaces["2.1"] == ["cli/src/fno/med_a.py"]
    # The public path-taking signatures still answer off the same parse.
    assert advance._plan_parallel_width(plan) == 2
    assert advance._plan_wave_bands(plan) == ["high", "medium"]
