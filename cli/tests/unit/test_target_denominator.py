"""`fno target init --deliverables N` stamps the scope denominator (x-cbab, AC1).

A plan-less code run historically had no denominator, so "shipped 1 of 4" was
inexpressible rather than unreported. `--deliverables N` declares the count into
the immutable manifest; omitting it leaves the field ABSENT (the unmeasurable
state the denominator gate keys on), never 0 (which would invert the gate).

AC1-DENOM  `--deliverables 3` writes `deliverables: 3`. Omitting leaves it absent.
AC1-PARSE  zero/negative is refused at parse, before any state is written.
AC1-PLUMB  the flag reaches the bash manifest writer via TARGET_DELIVERABLES.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest
import yaml
from typer.testing import CliRunner

_REPO_ROOT = Path(__file__).resolve().parents[3]
_INIT_SCRIPT = _REPO_ROOT / "hooks" / "helpers" / "init-target-state.sh"

# Bare-env `fno` stub: the asserted manifest value is bash-computed, so the CLI
# round-trips the hook makes are startup-cost no-ops here. Exit codes mirror a
# bare env (no graph/config): config/backlog -> 1, paths/worktree -> 0, claim -> 0.
_FNO_STUB = """\
#!/usr/bin/env bash
case "$1" in
  config) exit 1 ;;
  backlog) exit 1 ;;
  paths) exit 0 ;;
  worktree) exit 0 ;;
  claim)
    case "$2" in
      status|session-pid|acquire) exit 0 ;;
      *) exit 1 ;;
    esac ;;
  *) exit 1 ;;
esac
"""


def _run_init_script(tmp_path: Path, extra_env: dict[str, str]) -> subprocess.CompletedProcess:
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Test plan\n")
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    fno = bin_dir / "fno"
    fno.write_text(_FNO_STUB)
    fno.chmod(0o755)
    env = {
        "HOME": str(tmp_path),
        "PATH": f"{bin_dir}{os.pathsep}" + os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin"),
        "TARGET_START": "1",
        "TARGET_INPUT": str(plan_file),
        "TARGET_AUTO_MERGE": "false",
    }
    env.update(extra_env)
    return subprocess.run(
        ["bash", str(_INIT_SCRIPT)], cwd=str(tmp_path), env=env,
        capture_output=True, text=True, timeout=90,
    )


def _frontmatter(state_file: Path) -> dict:
    lines = state_file.read_text().splitlines()
    assert lines[0].strip() == "---", f"No opening --- in {state_file}"
    end = next(i for i, ln in enumerate(lines[1:], 1) if ln.strip() == "---")
    return yaml.safe_load("\n".join(lines[1:end]))


def test_deliverables_flag_stamps_the_manifest(tmp_path: Path):
    """AC1-DENOM: TARGET_DELIVERABLES=3 lands as `deliverables: 3`."""
    proc = _run_init_script(tmp_path, {"TARGET_DELIVERABLES": "3"})
    assert proc.returncode == 0, f"stderr: {proc.stderr[:500]}"
    fm = _frontmatter(tmp_path / ".fno" / "target-state.md")
    assert fm.get("deliverables") == 3


def test_omitting_deliverables_leaves_the_field_absent(tmp_path: Path):
    """AC1-DENOM: absence, not 0. A stamped 0 would invert the gate; absence is
    the unmeasurable state the denominator gate and the ratio measurement key on."""
    proc = _run_init_script(tmp_path, {})
    assert proc.returncode == 0, f"stderr: {proc.stderr[:500]}"
    fm = _frontmatter(tmp_path / ".fno" / "target-state.md")
    assert "deliverables" not in fm, fm


def _invoke_init(args: list[str], monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
    # No reviewers/apps configured and an absent plugin root: the deliverables
    # validation runs before the script is ever resolved, so the refusal surfaces
    # as the gate's own exit 2, not the missing-script message.
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "absent.yaml"))
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(tmp_path / "empty-plugin-root"))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    from fno.config import load_settings
    from fno.cli import app

    load_settings.cache_clear()
    return CliRunner().invoke(app, ["target", "init", "--input", "some-feature", *args])


@pytest.mark.parametrize("bad", ["0", "-1", "-3"])
def test_non_positive_deliverables_refused_at_parse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, bad: str
):
    """AC1-PARSE: 0/negative is refused before any state is written."""
    r = _invoke_init(["--deliverables", bad], monkeypatch, tmp_path)
    assert r.exit_code == 2
    assert "positive integer" in r.output


def test_deliverables_flag_reaches_the_manifest_writer_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC1-PLUMB: the Python flag is carried to the bash writer as
    TARGET_DELIVERABLES (the deterministic carrier, mirroring TARGET_SIZE)."""
    captured: dict = {}

    class _Proc:
        returncode = 0

    def _fake_run(cmd, **kwargs):
        if "env" in kwargs:
            captured["env"] = kwargs["env"]
        return _Proc()

    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "absent.yaml"))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setattr("fno.target_cli.subprocess.run", _fake_run)
    monkeypatch.setattr("fno.target_cli._print_orientation_report", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_dispatch_work_start", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_reconcile_lane_slot", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_check_resume_receipt", lambda *a, **k: None)
    from fno.config import load_settings
    from fno.cli import app

    load_settings.cache_clear()
    r = CliRunner().invoke(
        app, ["target", "init", "--input", "some-feature", "--deliverables", "4"]
    )
    assert r.exit_code == 0, r.output
    assert captured["env"].get("TARGET_DELIVERABLES") == "4"


def test_omitting_deliverables_does_not_set_the_env_carrier(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The carrier is absent when the flag is, so the bash writer emits no line.
    Asserting a positive marker (env key unset) rather than an absence would pass
    against a bug that always stamps a default."""
    captured: dict = {}

    class _Proc:
        returncode = 0

    def _fake_run(cmd, **kwargs):
        if "env" in kwargs:
            captured["env"] = kwargs["env"]
        return _Proc()

    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "absent.yaml"))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "GEMINI_SESSION_ID", "TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setattr("fno.target_cli.subprocess.run", _fake_run)
    monkeypatch.setattr("fno.target_cli._print_orientation_report", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_dispatch_work_start", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_reconcile_lane_slot", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_check_resume_receipt", lambda *a, **k: None)
    from fno.config import load_settings
    from fno.cli import app

    load_settings.cache_clear()
    r = CliRunner().invoke(app, ["target", "init", "--input", "some-feature"])
    assert r.exit_code == 0, r.output
    assert "TARGET_DELIVERABLES" not in captured["env"]


# --- AC2-REFUSE: the denominator gate ----------------------------------------
#
# Unit tests pin the predicate's conditions; the wiring test pins that init
# echoes the message, exits 2 (init's refusal contract - REVIEW_GATE_REFUSED=9
# is the check-review-gate verb's code, re-stamped from this 2), and writes no
# manifest.

from fno.target.denominator import absent_denominator_refusal  # noqa: E402

_CODE_NODE = {
    "id": "x-test", "domain": "code", "slug": "do-the-thing",
    "title": "do the thing", "details": "build it out", "plan_path": None,
}


def test_refusal_message_for_a_planless_code_node():
    msg = absent_denominator_refusal(node=_CODE_NODE, plan_path="", deliverables=None)
    assert msg is not None
    assert "no scope denominator" in msg
    assert "/fno:blueprint" in msg
    assert "--deliverables" in msg


def test_no_refusal_when_a_plan_is_bound():
    assert absent_denominator_refusal(
        node=_CODE_NODE, plan_path="/x/plan.md", deliverables=None
    ) is None


def test_no_refusal_when_deliverables_declared():
    # The cheap N=1 exit satisfies the denominator; it is never a hole.
    assert absent_denominator_refusal(
        node=_CODE_NODE, plan_path="", deliverables=1
    ) is None


def test_no_refusal_for_a_non_code_node():
    docs = {**_CODE_NODE, "domain": "docs"}
    assert absent_denominator_refusal(
        node=docs, plan_path="", deliverables=None
    ) is None


def test_no_refusal_for_a_free_text_idea():
    # An idea input resolves no node and makes its own denominator via /blueprint.
    assert absent_denominator_refusal(
        node=None, plan_path="", deliverables=None
    ) is None


def test_enumerated_node_withdraws_the_deliverables_exit():
    enum_node = {
        **_CODE_NODE,
        "title": "four bands",
        "details": "(1) county; (2) state; (3) peers; (4) portfolio",
    }
    msg = absent_denominator_refusal(
        node=enum_node, plan_path="", deliverables=None
    )
    assert msg is not None
    assert "withdrawn" in msg


def test_enumerated_node_refuses_a_declared_count():
    # A singular node keeps the cheap --deliverables exit (the test above this
    # one). An enumerated node forfeits it: its own prose declares several
    # deliverables, so a count would ship one of N behind a falsifiable claim.
    enum_node = {
        **_CODE_NODE,
        "title": "four bands",
        "details": "(1) county; (2) state; (3) peers; (4) portfolio",
    }
    msg = absent_denominator_refusal(
        node=enum_node, plan_path="", deliverables=1
    )
    assert msg is not None
    assert "withdrawn" in msg


def _invoke_init_node(monkeypatch: pytest.MonkeyPatch, node: dict, deliverables: bool):
    """Drive the real `target init` with a resolved node, without running the
    bootstrap script. The refusal sits after the script-resolution check, so the
    real plugin root must resolve (no empty-root trick); subprocess.run is
    stubbed so the script never actually runs on the proceeding path."""
    monkeypatch.setattr("fno.target_cli._resolve_dispatch_node", lambda *a, **k: node)
    monkeypatch.setattr("fno.target_cli.subprocess.run", lambda *a, **k: type("_P", (), {"returncode": 0})())
    for helper in (
        "_print_orientation_report", "_maybe_dispatch_work_start",
        "_maybe_reconcile_lane_slot", "_maybe_check_resume_receipt",
    ):
        monkeypatch.setattr(f"fno.target_cli.{helper}", lambda *a, **k: None)
    monkeypatch.setenv("FNO_CONFIG", str(Path("/nonexistent-config-xcbab.yaml")))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    from fno.config import load_settings
    from fno.cli import app

    load_settings.cache_clear()
    args = ["target", "init", "--input", "x-test"]
    if deliverables:
        args += ["--deliverables", "1"]
    return CliRunner().invoke(app, args)


def test_init_refuses_a_planless_code_node_and_names_both_exits(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    r = _invoke_init_node(monkeypatch, _CODE_NODE, deliverables=False)
    assert r.exit_code == 2
    assert "no scope denominator" in r.output
    assert "/fno:blueprint" in r.output
    assert "--deliverables" in r.output


def test_init_writes_no_manifest_when_it_refuses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    repo = tmp_path / "repo"
    (repo / ".fno").mkdir(parents=True)
    monkeypatch.chdir(repo)
    r = _invoke_init_node(monkeypatch, _CODE_NODE, deliverables=False)
    assert r.exit_code == 2
    assert not (repo / ".fno" / "target-state.md").exists()


def test_init_proceeds_when_deliverables_satisfies_the_gate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The same node with --deliverables is NOT refused (the cheap exit works)."""
    r = _invoke_init_node(monkeypatch, _CODE_NODE, deliverables=True)
    assert r.exit_code == 0, r.output
    assert "no scope denominator" not in r.output


def test_init_never_refuses_a_node_with_a_bound_plan(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """A node whose plan back-fills is never refused for lacking one. The
    back-fill reads the real graph (empty here), so simulate by giving the node
    a plan_path the back-fill would have set: pass --plan-path directly."""
    monkeypatch.setattr("fno.target_cli.subprocess.run", lambda *a, **k: type("_P", (), {"returncode": 0})())
    for helper in ("_print_orientation_report", "_maybe_dispatch_work_start",
                   "_maybe_reconcile_lane_slot", "_maybe_check_resume_receipt"):
        monkeypatch.setattr(f"fno.target_cli.{helper}", lambda *a, **k: None)
    monkeypatch.setenv("FNO_CONFIG", str(Path("/nonexistent-config-xcbab.yaml")))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    from fno.config import load_settings
    from fno.cli import app

    load_settings.cache_clear()
    r = CliRunner().invoke(app, ["target", "init", "--plan-path", str(tmp_path / "plan.md")])
    assert r.exit_code == 0, r.output
    assert "no scope denominator" not in r.output
