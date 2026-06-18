"""US4 CI drift guard tests (ab-acbde274) - `fno lint shellout-drift`.

Covers the five BDD acceptance criteria plus the precision regressions that
keep the scope decisions honest (cost/paths_cli/worktree exclusions).
"""
from __future__ import annotations

import shutil
import stat
import sys
from pathlib import Path

import pytest

from fno import lint_shellout_drift as g

_FNO_AVAILABLE = shutil.which("fno") is not None or (Path(sys.executable).parent / "fno").exists()

REPO_ROOT = Path(__file__).resolve().parents[3]


# --------------------------------------------------------------------------- #
# Fixture helpers
# --------------------------------------------------------------------------- #
def _make_module(scan_root: Path, relname: str, body: str) -> Path:
    path = scan_root / relname
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    return path


def _write_allowlist(tmp_path: Path, lines: list[str]) -> Path:
    al = tmp_path / "allow.txt"
    al.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return al


def _stub_fno(tmp_path: Path, name: str, script: str) -> list[str]:
    """Write an executable stub standing in for the `fno` CLI; return argv base."""
    p = tmp_path / name
    p.write_text(script, encoding="utf-8")
    p.chmod(p.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return [str(p)]


# A verb module that shells out to a resolve_repo_root()-rooted script (the
# canonical AC4-ERR shape).
BAD_VERB = '''
import subprocess
from fno.paths import resolve_repo_root

def go():
    script = resolve_repo_root() / "scripts" / "new.sh"
    subprocess.run(["bash", str(script)])
'''

# A module that bash-execs but roots the script at PLUGIN_ROOT via a PRIVATE
# _resolve_repo_root helper (the cost/_register.py shape) -> must NOT be flagged.
COST_LIKE = '''
import os, subprocess
from pathlib import Path

def _resolve_repo_root():
    return os.getcwd()

def emit():
    plugin_root = os.environ.get("PLUGIN_ROOT", "")
    events_sh = Path(plugin_root) / "scripts" / "lib" / "events.sh"
    subprocess.run(["bash", "-c", f"source {events_sh} && emit"])
'''

# A module that builds a resolve_repo_root()-rooted scripts/*.sh path but only
# WRITES/reads it (no bash exec) - the paths_cli.py shape -> must NOT be flagged.
CODEGEN_LIKE = '''
from fno.paths import resolve_repo_root

def emit():
    out = resolve_repo_root() / "scripts" / "lib" / "paths.sh"
    out.write_text("generated")
'''

# A module that bash-execs a script rooted at an INJECTED parameter (the
# worktree.py _run_setup_worktree_hook shape) -> must NOT be flagged.
PARAM_ROOTED = '''
import subprocess
from pathlib import Path

def run_hook(repo_root: Path):
    script = repo_root / "scripts" / "setup" / "setup-worktree.sh"
    if not script.exists():
        return -1
    subprocess.run(["bash", str(script)])
'''


# --------------------------------------------------------------------------- #
# AC4-HP: clean tree passes
# --------------------------------------------------------------------------- #
def test_ac4_hp_real_tree_scan_passes():
    """The real cli/src/fno tree has no un-allowlisted repo-root shell-outs."""
    report = g.run(do_degrade=False)
    assert report.exit_code == 0, "\n".join(report.lines)
    assert report.lines[0].startswith("shellout-drift: ok")


@pytest.mark.skipif(not _FNO_AVAILABLE, reason="fno CLI not on PATH or beside the interpreter")
def test_ac4_hp_real_tree_full_check_passes():
    """Full check (scan + live degrade proof) is green on the real tree."""
    report = g.run(do_degrade=True)
    assert report.exit_code == 0, "\n".join(report.lines)


# --------------------------------------------------------------------------- #
# AC4-ERR + AC4-UI: a new un-allowlisted shell-out fails with actionable output
# --------------------------------------------------------------------------- #
def test_ac4_err_new_shellout_fails(tmp_path):
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "badverb.py", BAD_VERB)
    al = _write_allowlist(tmp_path, ["# empty allowlist"])

    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)

    assert report.exit_code == 1, "\n".join(report.lines)
    joined = "\n".join(report.lines)
    assert "scripts/new.sh" in joined
    assert "cli/src/fno/badverb.py" in joined


def test_ac4_ui_output_is_actionable(tmp_path):
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "badverb.py", BAD_VERB)
    al = _write_allowlist(tmp_path, [])

    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    joined = "\n".join(report.lines)

    # file:line, the resolved relpath, and BOTH remediation paths.
    assert "badverb.py:" in joined  # file:line
    assert "scripts/new.sh" in joined  # resolved relpath
    assert "Remedy 1" in joined and "Remedy 2" in joined
    assert "eliminate the shell-out" in joined
    assert ".clone-only-scripts.txt" in joined


def test_allowlisted_shellout_passes(tmp_path):
    """The same shell-out passes once its script is on the allowlist."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "badverb.py", BAD_VERB)
    al = _write_allowlist(tmp_path, ["scripts/new.sh :: noop --flag"])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 0, "\n".join(report.lines)


# --------------------------------------------------------------------------- #
# AC4-EDGE: an allowlist entry whose verb does not degrade is rejected
# --------------------------------------------------------------------------- #
def test_ac4_edge_nondegrading_verb_rejected_exit_zero(tmp_path):
    stub = _stub_fno(tmp_path, "fake-fno", "#!/bin/sh\nexit 0\n")
    failures = g.degrade_proof({"scripts/x.sh": ["bogus"]}, fno_cmd=stub)
    assert len(failures) == 1
    assert "exited 0" in failures[0].reason


def test_ac4_edge_nondegrading_verb_rejected_127(tmp_path):
    stub = _stub_fno(tmp_path, "fake-fno", "#!/bin/sh\nexit 127\n")
    failures = g.degrade_proof({"scripts/x.sh": ["bogus"]}, fno_cmd=stub)
    assert len(failures) == 1
    assert "127" in failures[0].reason


def test_ac4_edge_silent_verb_rejected(tmp_path):
    stub = _stub_fno(tmp_path, "fake-fno", "#!/bin/sh\nexit 2\n")  # nonzero but no stderr
    failures = g.degrade_proof({"scripts/x.sh": ["bogus"]}, fno_cmd=stub)
    assert len(failures) == 1
    assert "silent" in failures[0].reason or "nothing to stderr" in failures[0].reason


def test_ac4_edge_traceback_verb_rejected(tmp_path):
    stub = _stub_fno(tmp_path, "fake-fno",
                     "#!/bin/sh\necho 'Traceback (most recent call last):' >&2\nexit 1\n")
    failures = g.degrade_proof({"scripts/x.sh": ["bogus"]}, fno_cmd=stub)
    assert len(failures) == 1
    assert "traceback" in failures[0].reason.lower()


def test_ac4_edge_good_degrade_accepted(tmp_path):
    """A verb that exits non-zero with an actionable stderr message passes."""
    stub = _stub_fno(tmp_path, "fake-fno",
                     "#!/bin/sh\necho 'needs the plugin; install it' >&2\nexit 2\n")
    failures = g.degrade_proof({"scripts/x.sh": ["bogus"]}, fno_cmd=stub)
    assert failures == []


# --------------------------------------------------------------------------- #
# AC4-FR: the guard fails closed on its own error
# --------------------------------------------------------------------------- #
def test_ac4_fr_parse_failure_is_red(tmp_path):
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "broken.py", "def f(:\n    pass\n")  # SyntaxError
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 3, "\n".join(report.lines)
    assert "fail-closed" in "\n".join(report.lines).lower()


def test_ac4_fr_malformed_allowlist_is_red(tmp_path):
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "ok.py", "x = 1\n")
    al = _write_allowlist(tmp_path, ["this-line-has-no-separator"])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 3
    assert "fail-closed" in "\n".join(report.lines).lower()


def test_ac4_fr_missing_allowlist_is_red(tmp_path):
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "ok.py", "x = 1\n")
    report = g.run(repo_root=tmp_path, scan_root=scan_root,
                   allowlist_path=tmp_path / "nope.txt", do_degrade=False)
    assert report.exit_code == 3


# --------------------------------------------------------------------------- #
# Precision regressions: the documented scope exclusions stay correct
# --------------------------------------------------------------------------- #
def test_cost_like_plugin_root_shellout_not_flagged(tmp_path):
    """PLUGIN_ROOT/_resolve_repo_root-rooted shell-out (cost/_register.py) is NOT
    flagged - it does not use the shared resolve_repo_root()."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "costlike.py", COST_LIKE)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 0, "\n".join(report.lines)


def test_codegen_like_not_flagged(tmp_path):
    """resolve_repo_root()-rooted scripts/*.sh that is WRITTEN, not bash-exec'd
    (paths_cli.py), is NOT flagged."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "codegenlike.py", CODEGEN_LIKE)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 0, "\n".join(report.lines)


def test_param_rooted_shellout_not_flagged(tmp_path):
    """A bash exec rooted at an injected param (worktree.py setup hook), with no
    shared-resolver call, is NOT flagged."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "paramrooted.py", PARAM_ROOTED)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 0, "\n".join(report.lines)


def test_scan_exclude_skips_in_repo_only_module(tmp_path):
    """A module on SCAN_EXCLUDE is not scanned even if it shells out."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    _make_module(scan_root, "evals/runner.py", BAD_VERB)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False, exclude={"cli/src/fno/evals/runner.py"})
    assert report.exit_code == 0, "\n".join(report.lines)


def test_segmented_and_fullstring_and_plugin_script_all_detected(tmp_path):
    """The three construction idioms are each caught."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    mod = '''
import subprocess
from fno.paths import resolve_repo_root, resolve_plugin_script

A = "scripts/lib/full.sh"           # full string
def go():
    seg = resolve_repo_root() / "scripts" / "lib" / "seg.sh"   # segmented
    p = resolve_plugin_script("hooks/helpers/plug.sh")          # plugin script
    subprocess.run(["bash", str(seg)])
'''
    _make_module(scan_root, "threeforms.py", mod)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    joined = "\n".join(report.lines)
    assert report.exit_code == 1
    assert "scripts/lib/full.sh" in joined
    assert "scripts/lib/seg.sh" in joined
    assert "hooks/helpers/plug.sh" in joined


def test_attribute_form_calls_detected(tmp_path):
    """Attribute-form resolver, plugin-script, and subprocess calls are caught
    (review: paths.resolve_repo_root(), paths.resolve_plugin_script(), no silent skip)."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    mod = '''
import subprocess
from fno import paths

def go():
    if paths.resolve_repo_root():
        p = paths.resolve_plugin_script("hooks/helpers/attr.sh")
        subprocess.run(["bash", str(p)])
'''
    _make_module(scan_root, "attrforms.py", mod)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 1, "\n".join(report.lines)
    assert "hooks/helpers/attr.sh" in "\n".join(report.lines)


def test_annassign_bare_import_and_pathlib_path_detected(tmp_path):
    """A typed `cmd: list[str] = ["bash",...]` + `from subprocess import run` +
    `pathlib.Path("scripts")` segmented join is caught (review: AnnAssign, bare
    import, attribute Path - all previously silent bypasses)."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    mod = '''
import pathlib
from subprocess import run
from fno.paths import resolve_repo_root

def go():
    base = resolve_repo_root()
    script = base / pathlib.Path("scripts") / "lib" / "ann.sh"
    cmd: list[str] = ["bash", str(script)]
    run(cmd)
'''
    _make_module(scan_root, "annforms.py", mod)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 1, "\n".join(report.lines)
    assert "scripts/lib/ann.sh" in "\n".join(report.lines)


def test_non_shell_subprocess_not_flagged(tmp_path):
    """Broadening subprocess detection to bare names must not flag a module that
    execs a NON-shell command (argv0 != bash/sh), even with a resolver + script string."""
    scan_root = tmp_path / "cli" / "src" / "fno"
    mod = '''
import subprocess
from fno.paths import resolve_repo_root

NOTE = "scripts/lib/never-exec.sh"

def go():
    resolve_repo_root()
    subprocess.run(["git", "status"])
'''
    _make_module(scan_root, "nonshell.py", mod)
    al = _write_allowlist(tmp_path, [])
    report = g.run(repo_root=tmp_path, scan_root=scan_root, allowlist_path=al,
                   do_degrade=False)
    assert report.exit_code == 0, "\n".join(report.lines)


def test_allowlist_lists_flock_pattern_and_drops_exception_caveat():
    """AC2-UI/AC2-HP (ab-fd017698): once `fno lint flock-pattern` conforms to the
    shared resolve_repo_root(), it is LISTED on the allowlist and the scope note
    no longer documents it as the private-rooted exception (the cv-ca99e324
    caveat is removed), so the allowlist does not lie."""
    text = (REPO_ROOT / g.ALLOWLIST_REL).read_text(encoding="utf-8")
    assert "scripts/lint-flock-pattern.sh :: lint flock-pattern" in text
    # the scope-note exception caveat + its carveout reference are gone
    assert "cv-ca99e324" not in text
    assert "out of scope" not in text


def test_real_allowlist_parses_and_matches_real_scan():
    """The shipped allowlist parses and exactly covers the real tree's shell-outs
    (no stale entries, no missing entries) under a scan-only run."""
    entries = g.parse_allowlist(REPO_ROOT / g.ALLOWLIST_REL)
    assert entries, "allowlist should be non-empty"
    # scan with the real allowlist -> no violations (covers all) ...
    violations = g.scan_tree(REPO_ROOT / g.SCAN_REL, set(entries), REPO_ROOT)
    assert violations == [], [f"{v.file}:{v.line} {v.relpath}" for v in violations]
    # ... and scan with an EMPTY allowlist -> every flagged relpath is one we
    # listed (no allowlist entry is stale / unmatched by the tree).
    all_flagged = {v.relpath for v in g.scan_tree(REPO_ROOT / g.SCAN_REL, set(), REPO_ROOT)}
    assert set(entries) == all_flagged, (
        f"allowlist drift: listed-but-not-found={set(entries) - all_flagged}, "
        f"found-but-not-listed={all_flagged - set(entries)}"
    )
