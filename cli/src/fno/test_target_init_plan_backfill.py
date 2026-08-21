"""Node plan_path -> manifest back-fill contract for `fno do target init` (x-39c0).

`fno do target start <node>` / `fno do target init --input <node>` used to write
``plan_path: ""`` to the immutable manifest even when the node had a plan bound,
because init-target-state.sh sources plan_path only from ``TARGET_PLAN_PATH``
(= ``--plan-path``). The orienter then reported ``plan: none`` for a node that
did have a plan, forcing a manual first-fill.

The fix (987b024d) shipped with no test, and it rides on
``_resolve_plan_for_blast`` - a helper NAMED for the blast-radius read. Scoping
that helper back to blast-only would silently revert x-39c0 with a green suite,
which is the "a guard on one of N reachable paths is decorative" trap in its
quietest form. These tests pin the back-fill as its own contract:
``test_backfill_survives_blast_disabled`` is the one that fails on that revert.

The dangling-pointer case is not cosmetic. Back-filling a plan that is not on
disk wedges the write-once manifest, while leaving it empty starts a planned
node without its plan. Initialization must refuse before writing either state.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import target_cli
from fno.config import BlastConfig
from fno.target_cli import _resolve_plan_pointer, target_app

runner = CliRunner()


@pytest.fixture
def stub_exec(monkeypatch, tmp_path):
    """Capture the env handed to the bash init writer; write no real state.

    Intercepts ONLY the bash init-script call and passes every other subprocess
    through, so the `git rev-parse` that resolve_repo_root shells out for still
    works (patching subprocess.run wholesale makes the suite order-dependent).
    """
    fake_script = tmp_path / "init-target-state.sh"
    fake_script.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
    monkeypatch.setattr(target_cli, "_resolve_init_script", lambda: fake_script)

    captured: list[dict] = []

    class _Result:
        returncode = 0

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


def _graph(monkeypatch, entries):
    monkeypatch.setattr("fno.graph.load.load_graph", lambda *a, **k: entries)


def _blast(monkeypatch, enabled: bool):
    monkeypatch.setattr(
        target_cli, "_load_blast_cfg", lambda: BlastConfig(enabled=enabled)
    )


def _real_plan(tmp_path: Path, name: str = "plan.md") -> str:
    p = tmp_path / name
    p.write_text("# Plan\n", encoding="utf-8")
    return str(p)


def _invoke(args):
    return runner.invoke(target_app, ["init", *args])


# --------------------------- the x-39c0 contract --------------------------- #
def test_node_input_backfills_bound_plan(stub_exec, monkeypatch, tmp_path):
    """A node input with a bound, on-disk plan reaches the manifest writer."""
    plan = _real_plan(tmp_path)
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": plan}])
    _blast(monkeypatch, True)

    res = _invoke(["--input", "x-39c0"])

    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_PLAN_PATH") == plan


def test_backfill_survives_blast_disabled(stub_exec, monkeypatch, tmp_path):
    """The back-fill is NOT a blast-radius side effect.

    Regression guard: `_resolve_plan_for_blast` is named for the blast read, and
    the blast read itself is gated on config.target.blast.enabled. If the
    back-fill is ever folded inside that gate, every default install (blast off)
    silently returns to `plan: none`. Explicitly asserts with blast disabled.
    """
    plan = _real_plan(tmp_path)
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": plan}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "x-39c0"])

    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_PLAN_PATH") == plan


def test_modifier_prefixed_node_input_backfills(stub_exec, monkeypatch, tmp_path):
    """`/target no-merge <id>` forwards the whole arg; the id token still wins."""
    plan = _real_plan(tmp_path)
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": plan}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "no-merge x-39c0"])

    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_PLAN_PATH") == plan


def test_anchored_plan_pointer_backfills(stub_exec, monkeypatch, tmp_path):
    """A `#group-N` anchored pointer is not mistaken for a missing file."""
    plan = _real_plan(tmp_path)
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": f"{plan}#group-2"}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "x-39c0"])

    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_PLAN_PATH") == f"{plan}#group-2"


def test_explicit_plan_path_wins_over_node(stub_exec, monkeypatch, tmp_path):
    """An operator-named --plan-path is never overridden by the node's."""
    node_plan = _real_plan(tmp_path, "node.md")
    explicit = _real_plan(tmp_path, "explicit.md")
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": node_plan}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "x-39c0", "--plan-path", explicit])

    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_PLAN_PATH") == explicit


def test_missing_explicit_plan_path_refuses_before_manifest_write(
    stub_exec, monkeypatch, tmp_path
):
    """A typo in --plan-path refuses instead of writing a broken pointer."""
    missing = str(tmp_path / "nope.md")
    _graph(monkeypatch, [])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "some feature", "--plan-path", missing])

    assert res.exit_code == 2
    assert stub_exec == []
    assert "REFUSING" in res.stderr
    assert "explicit --plan-path" in res.stderr
    assert "does not resolve to a readable plan" in res.stderr


def test_free_text_input_leaves_plan_empty(stub_exec, monkeypatch, tmp_path):
    """Idea-first runs stay empty for the blueprint-then-first-fill flow.

    The node's plan must be a REAL file: with a non-existent one the existence
    gate drops the pointer no matter what, so the test would pass even if a
    free-text description mis-resolved to a node - the exact regression it
    exists to catch. An on-disk plan makes token matching the only thing
    keeping this empty.
    """
    plan = _real_plan(tmp_path)
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": plan}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "fix the login redirect bug"])

    assert res.exit_code == 0
    assert "TARGET_PLAN_PATH" not in stub_exec[0]


def test_dangling_node_plan_refuses_before_manifest_write(
    stub_exec, monkeypatch, tmp_path
):
    """A bound-but-missing plan refuses instead of starting planless."""
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": str(tmp_path / "gone.md")}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "x-39c0"])

    assert res.exit_code == 2
    assert stub_exec == []
    assert "REFUSING" in res.stderr
    assert "does not resolve to a readable plan" in res.stderr
    assert "fno backlog update" in res.stderr


def test_unreadable_node_plan_refuses_before_manifest_write(
    stub_exec, monkeypatch, tmp_path
):
    plan = _real_plan(tmp_path)
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": plan}])
    _blast(monkeypatch, False)

    def _unreadable(path):
        raise PermissionError("permission denied")

    monkeypatch.setattr("fno.target.blast.resolve_plan_index", _unreadable)

    res = _invoke(["--input", "x-39c0"])

    assert res.exit_code == 2
    assert stub_exec == []
    assert "REFUSING" in res.stderr
    assert "does not resolve to a readable plan" in res.stderr


def test_ambiguous_node_tokens_leave_plan_empty(stub_exec, monkeypatch, tmp_path):
    """Two node ids in one input: refuse to guess which plan to bind."""
    a = _real_plan(tmp_path, "a.md")
    b = _real_plan(tmp_path, "b.md")
    _graph(
        monkeypatch,
        [{"id": "x-39c0", "plan_path": a}, {"id": "ab-other", "plan_path": b}],
    )
    _blast(monkeypatch, False)

    res = _invoke(["--input", "x-39c0 ab-other"])

    assert res.exit_code == 0
    assert "TARGET_PLAN_PATH" not in stub_exec[0]


def test_node_without_plan_leaves_plan_empty(stub_exec, monkeypatch):
    """A bare idea node (no plan bound) is the legitimate empty case."""
    _graph(monkeypatch, [{"id": "x-39c0", "status": "idea"}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "x-39c0"])

    assert res.exit_code == 0
    assert "TARGET_PLAN_PATH" not in stub_exec[0]


# -------------------------- the pointer resolver --------------------------- #
def test_resolve_plan_pointer_matrix(tmp_path):
    real = tmp_path / "plan.md"
    real.write_text("# Plan\n", encoding="utf-8")

    assert _resolve_plan_pointer(str(real)) == str(real)
    # anchor stripped for the stat, preserved on the way out
    assert _resolve_plan_pointer(f"{real}#group-3") == f"{real}#group-3"
    assert _resolve_plan_pointer(str(tmp_path / "gone.md")) is None
    assert _resolve_plan_pointer("") is None
    assert _resolve_plan_pointer("#group-1") is None  # anchor only, no file


def test_resolve_plan_pointer_accepts_folder_plan(tmp_path):
    """A folder plan resolves: the gate is existence, never is_file.

    footnote plans are routinely directories whose entry point is 00-INDEX.md,
    so an is_file() "tightening" would silently drop the back-fill for every one
    of them with a fully green suite.
    """
    folder = tmp_path / "plan-folder"
    folder.mkdir()
    (folder / "00-INDEX.md").write_text("# Index\n", encoding="utf-8")

    assert _resolve_plan_pointer(str(folder)) == str(folder)


def test_resolve_plan_pointer_relative_uses_repo_root(tmp_path, monkeypatch):
    """A relative pointer resolves against the repo root, not the process cwd.

    The graph is global, so a relative plan_path in it is repo-root-relative by
    convention. Statting against the process cwd would report a plan that is on
    disk as missing whenever init runs from a subdirectory, dropping a valid
    binding and printing a false diagnosis. init-target-state.sh does not anchor
    relative paths either (its plan stats are bare), so normalizing to absolute
    here is what keeps the shell correct too.
    """
    root = tmp_path / "repo"
    (root / "internal" / "plans").mkdir(parents=True)
    (root / "internal" / "plans" / "p.md").write_text("# Plan\n", encoding="utf-8")
    monkeypatch.setattr(target_cli, "_repo_root_or_none", lambda: str(root))

    sub = root / "cli"
    sub.mkdir()
    monkeypatch.chdir(sub)  # a cwd where the relative pointer does NOT resolve

    got = _resolve_plan_pointer("internal/plans/p.md")

    assert got == str(root / "internal" / "plans" / "p.md")


def test_resolve_plan_pointer_returns_expanded_tilde(tmp_path, monkeypatch):
    """A `~` pointer is bound EXPANDED, never raw.

    bash does not tilde-expand a variable's value, so init-target-state.sh and
    every downstream plan read would see a raw `~/...` string as missing. Storing
    the raw form would pass this gate and then wedge the write-once manifest -
    the exact harm the gate exists to prevent - so the resolver must hand back
    the path it actually proved.
    """
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / "plan.md").write_text("# Plan\n", encoding="utf-8")

    got = _resolve_plan_pointer("~/plan.md")

    assert got == str(tmp_path / "plan.md")
    assert "~" not in got
    assert _resolve_plan_pointer("~/absent.md") is None


def test_tilde_node_plan_reaches_writer_expanded(stub_exec, monkeypatch, tmp_path):
    """End-to-end: the env handed to the bash writer carries no raw `~`."""
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / "plan.md").write_text("# Plan\n", encoding="utf-8")
    _graph(monkeypatch, [{"id": "x-39c0", "plan_path": "~/plan.md"}])
    _blast(monkeypatch, False)

    res = _invoke(["--input", "x-39c0"])

    assert res.exit_code == 0
    assert stub_exec[0].get("TARGET_PLAN_PATH") == str(tmp_path / "plan.md")


def test_resolve_plan_pointer_fails_closed(monkeypatch):
    """Under an unreadable path, return the refusal signal.

    Fail-CLOSED on purpose: an unverified pointer must never land in a
    write-once field or let a planned node continue with an empty field.
    """
    def _boom(self):
        raise OSError("filesystem unavailable")

    monkeypatch.setattr(Path, "exists", _boom)
    assert _resolve_plan_pointer("some/plan.md") is None
