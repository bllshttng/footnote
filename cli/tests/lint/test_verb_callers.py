"""Tests for the --curriculum complement layer of scripts/diagnostics/verb-callers.py.

verb-callers.py is the canonical caller-finding instrument (its iter_corpus
include-list is what avoids the skills/target rg-glob pitfall). These tests
cover only the curriculum layer added on top; the sweep, controls, and
skills/target-safe walk are exercised by the tool's own --self-check.
"""
from __future__ import annotations

import importlib.util
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "scripts" / "diagnostics" / "verb-callers.py"


def _load():
    spec = importlib.util.spec_from_file_location("verb_callers", SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


vc = _load()


def test_load_curriculum_strips_comments_and_flags_unknown(tmp_path):
    leaves = {"backlog get", "mail send", "agents loop-check"}
    curr = tmp_path / "curriculum.txt"
    curr.write_text(
        "# full-line comment\n"
        "backlog get\n"
        "mail send # trailing comment\n"
        "backlog nope\n"          # not a real leaf
        "\n"
    )
    taught, unknown = vc.load_curriculum(curr, leaves)
    assert taught == {"backlog get", "mail send"}
    assert unknown == ["backlog nope"]


def test_load_curriculum_empty_file(tmp_path):
    curr = tmp_path / "curriculum.txt"
    curr.write_text("# only comments\n\n")
    taught, unknown = vc.load_curriculum(curr, {"backlog get"})
    assert taught == set() and unknown == []


def test_curriculum_end_to_end_reports_complement():
    """The real curriculum partitions the live surface, with cull candidates.

    Slow (runs the corpus sweep); verifies the feature integrates with the
    canonical sweep + controls and that the checked-in curriculum is valid.

    Asserts the PARTITION (taught + untaught == the baseline, every count
    positive), not a snapshot of the numbers. It used to pin `complement
    (untaught): 277`, which made a verb cut - the thing this tool exists to
    support - fail a test that had nothing to say about the cut. A partition
    that stops adding up is a real defect; a complement that shrank is the
    tool working.
    """
    proc = subprocess.run(
        [sys.executable, str(SCRIPT),
         "--curriculum", str(REPO_ROOT / "scripts" / "ci" / "curriculum.txt")],
        cwd=REPO_ROOT, capture_output=True, text=True,
    )
    assert proc.returncode == 0, proc.stderr

    def _n(label: str) -> int:
        m = re.search(rf"^{re.escape(label)}: (\d+)$", proc.stdout, re.M)
        assert m, f"missing {label!r} line in:\n{proc.stdout}"
        return int(m.group(1))

    leaves = _n("baseline leaves")
    taught = _n("taught (curriculum)")
    untaught = _n("complement (untaught)")
    assert leaves > 0 and taught > 0 and untaught > 0
    assert taught + untaught == leaves, "curriculum must partition the live surface"
    assert "cull candidates in complement" in proc.stdout


def test_rust_argv_sweep_credits_the_array_not_the_command(tmp_path):
    """The fourth shell shape, and the two ways of getting its filter wrong.

    A Rust shell-out puts the verb inside an argv array with no binary token
    beside it, so the whitespace sweep credits nothing. The array is therefore
    the signal. Two directions matter and both are asserted here: an ENUMERATED
    foreign literal is skipped, and an UNKNOWN literal is credited, because a
    "skip anything that is not fno" rule deletes a live verb the day a wrapper
    lands under a new name.
    """
    src = tmp_path / "crates" / "fno" / "src"
    src.mkdir(parents=True)
    (src / "sites.rs").write_text(
        'Command::new(fno_bin()).args(["plan", "fidelity", "--json"]);\n'
        # the qualifier shape that defeated keying on Command::new
        "tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())\n"
        '    .args(["needs", "--json"]);\n'
        'Command::new("git").args(["notify"]);\n'
        'Command::new("candidate_fno").args(["mail", "send"]);\n'
    )
    (tmp_path / "crates" / "fno" / "target").mkdir()
    (tmp_path / "crates" / "fno" / "target" / "build.rs").write_text(
        'Command::new(fno_bin()).args(["backlog", "done"]);\n'
    )

    counts = vc.sweep_rust_argv(
        tmp_path, {"plan fidelity", "agents needs", "notify", "mail send", "backlog done"}
    )
    assert counts["plan fidelity"] == 1
    # the array reads ["needs"]; the leaf carries the `agents` the argv omits
    assert counts["agents needs"] == 1
    assert counts["notify"] == 0, "an enumerated foreign literal must be skipped"
    assert counts["mail send"] == 1, "an UNKNOWN literal must be credited, never skipped"
    assert counts["backlog done"] == 0, "crates/*/target is build output, not source"


def test_rust_argv_controls_refuse_to_emit_a_list_when_the_sweep_breaks(tmp_path):
    """The RED path, pinned. A green-only test cannot tell a working gate from a dead one.

    Runs a COPY of the script with the argv-array pattern replaced by one that
    matches nothing, against the real corpus. The four controls must fail, the
    run must exit 2, and no dead set may be printed - an emitted list is the
    failure mode this control set exists to prevent.
    """
    broken = tmp_path / "verb-callers.py"
    text = SCRIPT.read_text()
    needle = 're.compile(r"\\.args\\(\\s*&?\\s*\\[([^\\]]*)\\]", re.S)'
    assert needle in text, "the argv-array pattern moved; update this test"
    broken.write_text(
        text.replace(needle, 're.compile(r"\\.NO_SUCH_CALL\\(\\[([^\\]]*)\\]", re.S)')
    )

    proc = subprocess.run(
        [sys.executable, str(broken), "--dead"],
        cwd=REPO_ROOT, capture_output=True, text=True,
    )
    assert proc.returncode == 2, f"a failed control must exit 2, got {proc.returncode}"
    out = proc.stdout + proc.stderr
    assert "no list emitted" in out, out
    for leaf in vc.RUST_ARGV_CONTROLS:
        assert f"rust-argv/{leaf}" in out, f"{leaf} not named in the refusal:\n{out}"
    assert "dead:" not in out, f"a broken sweep must emit NO candidate list:\n{out}"


def test_rust_argv_controls_are_real_leaves():
    """A control naming a verb that no longer exists cannot fail for the right reason.

    It would fail on every run, get read as noise, and end up deleted or its
    floor dropped to zero - which is how a control set stops defending anything.
    """
    leaves = set(vc.load_leaves(REPO_ROOT))
    missing = sorted(set(vc.RUST_ARGV_CONTROLS) - leaves)
    assert not missing, f"rust-argv controls name non-leaves: {missing}"


def test_curriculum_with_self_check_runs_self_check():
    """--self-check takes precedence over --curriculum.

    Before the reorder, --curriculum returned early and --self-check was
    dropped: the operator got a cull list and exit 0 with no diagnostics. Now
    --self-check runs regardless of --curriculum, consistent with its
    precedence over --summary, --zero, and the default table.
    """
    proc = subprocess.run(
        [sys.executable, str(SCRIPT), "--curriculum",
         str(REPO_ROOT / "scripts" / "ci" / "curriculum.txt"), "--self-check"],
        cwd=REPO_ROOT, capture_output=True, text=True,
    )
    # self-check diagnostics present; the cull-list header is not.
    assert "controls:" in proc.stdout, proc.stdout
    assert "cull candidates in complement" not in proc.stdout, proc.stdout
