"""Tests for the standalone verb census - scripts/ci/verb-census.py.

Two layers of positive control, because a zero and a broken instrument look
identical and the blueprint's census broke once on a hyphen-rejecting
lookbehind:

  - corpus controls (run_census on a fixture + the real repo via --check-controls)
    prove the walk fires for known-live verbs in each of the three matching
    regimes (Python-front, Rust-front, single-token).
  - pattern unit assertions are the SHARP guard for the hyphen-bug class: the
    corpus controls cannot deterministically catch it, because a Rust-front verb
    also has bare-form prose mentions that a buggy pattern still finds. The unit
    test asserts the real pattern matches the hyphenated front door AND that the
    buggy variant does not, so it fails loud if the lookbehind regresses.
"""
from __future__ import annotations

import importlib.util
import re
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "scripts" / "ci" / "verb-census.py"


def _load():
    spec = importlib.util.spec_from_file_location("verb_census", SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


vc = _load()


# --------------------------------------------------------------------------- #
# Bucket classification
# --------------------------------------------------------------------------- #

@pytest.mark.parametrize("rel,expected", [
    ("docs/guides/foo.md", "docs"),
    ("skills/target/SKILL.md", "agentsurface"),
    ("agents/scout.md", "agentsurface"),
    ("commands/fno-me.md", "agentsurface"),
    ("hooks/session-start.sh", "machinery"),
    ("scripts/ci/x.sh", "machinery"),
    ("crates/fno/src/main.rs", "machinery"),
    ("crates/fno-agents/tests/foo.rs", "tests"),       # tests dir beats crates
    ("cli/tests/unit/test_x.py", "tests"),             # tests dir beats cli/src
    ("cli/src/fno/test_cmd.py", "impl"),               # test_ name but no tests dir
    ("cli/src/fno/config_cli.py", "impl"),
    ("README.md", "other"),
    ("tests/hooks/foo.sh", "tests"),
])
def test_classify_bucket(rel, expected):
    assert vc.classify_bucket(rel) == expected


# --------------------------------------------------------------------------- #
# Baseline / curriculum parsing
# --------------------------------------------------------------------------- #

def test_load_verbs_strips_flags_comments_and_duplicates(tmp_path):
    base = tmp_path / "verb-baseline.txt"
    base.write_text(
        "# comment\n"
        "\n"
        "agents adopt\n"
        "agents ask !--provider\n"     # hidden flag stripped
        "backlog advance !--continuation\n"
        "agents adopt\n"               # duplicate dropped
    )
    assert vc.load_verbs(base) == ["agents adopt", "agents ask", "backlog advance"]


def test_load_curriculum_flags_unknown_verb(tmp_path):
    verbs = {"backlog get", "mail send"}
    curr = tmp_path / "curriculum.txt"
    curr.write_text("# header\nbacklog get\nmail send\nbacklog nope\n")
    taught, unknown = vc.load_curriculum(curr, verbs)
    assert taught == {"backlog get", "mail send"}
    assert unknown == ["backlog nope"]


# --------------------------------------------------------------------------- #
# Pattern compilation - the matching regimes
# --------------------------------------------------------------------------- #

def test_multitoken_matches_all_invocation_shapes():
    pat = vc.compile_pattern("backlog get")
    # shell / prose
    assert pat.search("run `fno backlog get ab-1`")
    assert pat.search("via fno-py backlog get")
    # python subprocess list
    assert pat.search('cmd = ["backlog", "get", x]')
    # rust arg array
    assert pat.search('args = ["backlog", "get"]')
    # not a substring of a larger word
    assert not pat.search("mybacklog gettysburg")


def test_rust_front_door_matches_despite_preceding_hyphen():
    """The load-bearing guard for the blueprint's hyphen-lookbehind bug.

    `agents loop-check` is reached through the Rust front door `fno-agents
    loop-check`, where `agents` is preceded by a hyphen. A lookbehind that
    rejected `-` (the bug) reported every such caller as dead. The real pattern
    must match; the buggy variant must not, which is what makes this assertion a
    control that fails when the bug returns.
    """
    pat = vc.compile_pattern("agents loop-check")
    assert pat.search("fno-agents loop-check --json")
    assert pat.search("shim that wraps fno-agents loop-check")
    buggy = re.compile(rf"(?<![-\w]){re.escape('agents')}{vc.SEP}{re.escape('loop-check')}(?!\w)")
    assert not buggy.search("fno-agents loop-check --json"), (
        "buggy hyphen-rejecting lookbehind matched the Rust front door; "
        "this control would not catch a regression of the blueprint bug"
    )


def test_single_token_requires_front_door():
    pat = vc.compile_pattern("whoami")
    assert pat.search("run `fno whoami`")
    assert pat.search('["fno-py", "whoami"]')
    # bare token in prose is NOT a caller (would match half the repo otherwise)
    assert not pat.search("a nice readme about whoami usage")
    # front door is token-bounded: not a prefix of a longer word
    assert not pat.search("fnoteworthy whoami")


# --------------------------------------------------------------------------- #
# Verdict logic
# --------------------------------------------------------------------------- #

def _b(**kw):
    base = {b: 0 for b in vc.BUCKET_ORDER}
    base.update(kw)
    return base


@pytest.mark.parametrize("buckets,expected", [
    (_b(docs=1), "KEEP-DOC-GAP"),
    (_b(docs=1, impl=3), "KEEP-DOC-GAP"),                 # docs wins
    (_b(agentsurface=1), "KEEP-INTERNAL-skill"),
    (_b(machinery=2), "KEEP-INTERNAL-machinery"),
    (_b(machinery=1, impl=2, tests=1), "KEEP-INTERNAL-machinery"),
    (_b(impl=1), "CUT-3"),
    (_b(impl=1, tests=3, other=1), "CUT-3"),              # impl beats tests
    (_b(tests=1), "CUT-2"),
    (_b(), "CUT-1"),                                       # no caller anywhere
    (_b(other=2), "CUT-1"),                                # other never rescues
])
def test_decide_verdict(buckets, expected):
    assert vc.decide_verdict(buckets) == expected


# --------------------------------------------------------------------------- #
# Census on a fixture
# --------------------------------------------------------------------------- #

def _fixture(tmp_path):
    (tmp_path / "scripts" / "ci").mkdir(parents=True)
    (tmp_path / "scripts" / "ci" / "verb-baseline.txt").write_text(
        "backlog get\nmail send\nagents loop-check\nwhoami\ndead verb\n"
    )
    (tmp_path / "scripts" / "ci" / "verb-census.py").write_text("# self\n")
    (tmp_path / "docs").mkdir()
    (tmp_path / "docs" / "g.md").write_text("use `fno backlog get` to read a node")
    (tmp_path / "skills").mkdir()
    (tmp_path / "skills" / "x.md").write_text("invoke `fno mail send` to post")
    (tmp_path / "hooks").mkdir()
    (tmp_path / "hooks" / "h.sh").write_text("fno-agents loop-check decides stop")
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "whoami.py").write_text(
        'subprocess.run(["fno", "whoami"])\n'
    )
    # 'dead verb' appears nowhere -> CUT-1
    return tmp_path


def test_census_fixture_buckets_and_verdicts(tmp_path):
    root = _fixture(tmp_path)
    verbs = vc.load_verbs(root / "scripts" / "ci" / "verb-baseline.txt")
    recs = {r["verb"]: r for r in vc.run_census(verbs, root)}
    assert recs["backlog get"]["verdict"] == "KEEP-DOC-GAP"
    assert recs["mail send"]["verdict"] == "KEEP-INTERNAL-skill"
    assert recs["agents loop-check"]["verdict"] == "KEEP-INTERNAL-machinery"
    assert recs["whoami"]["verdict"] == "CUT-3"           # only cli/src self-ref
    assert recs["dead verb"]["verdict"] == "CUT-1"        # no caller anywhere
    # the census must not count its own data files as callers
    assert recs["backlog get"]["first_caller"] != "scripts/ci/verb-baseline.txt"


def test_control_failures_on_bogus_and_on_real(tmp_path):
    root = _fixture(tmp_path)
    verbs = vc.load_verbs(root / "scripts" / "ci" / "verb-baseline.txt")
    recs = vc.run_census(verbs, root)
    # 'dead verb' has no outside-impl caller -> a control on it fails
    failed = vc.control_failures(recs, ["backlog get", "dead verb"])
    assert len(failed) == 1 and "dead verb" in failed[0]
    # a verb not in the baseline is a misconfigured control
    assert vc.control_failures(recs, ["does-not-exist"])


# --------------------------------------------------------------------------- #
# Real repo - the broad corpus control (CI-feasible: scans only 12 verbs)
# --------------------------------------------------------------------------- #

def test_real_repo_positive_controls():
    """The 12 controls must each be found outside impl on the real repo.

    Covers all three matching regimes: Python-front (backlog get), Rust-front
    (agents loop-check - the hyphen-bug class), and single-token (whoami). Runs
    the focused --check-controls path (~6s, 12 patterns), not the full census.
    """
    proc = subprocess.run(
        [sys.executable, str(SCRIPT), "--check-controls"],
        cwd=REPO_ROOT, capture_output=True, text=True,
    )
    assert proc.returncode == 0, proc.stderr
    assert "all 12 found outside impl" in proc.stdout
