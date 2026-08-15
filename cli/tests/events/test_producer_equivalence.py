"""Every producer named at SKILL.md:112 actually produces (x-e97b).

skills/review/SKILL.md:112 lists sigma, peer, code-review, and declare as
peer producers of the SAME head-pinned `review_attestation` event. Before
this PR, that sentence was true in prose and false in code: `code-review`
was listed but had no producer, so a clean `/code-review` pass with flags
still read `uncovered`, and six workers in one night had to go find
`emit-attestation.sh` by hand.

This test is deliberately mechanical about "producer": it parses the
producer NAMES out of that exact sentence, so a fifth name added there
without a working real-world trigger fails here rather than shipping
silently, the way `code-review` did. For each name it drives the REAL
clean-pass surface (not a hand-authored event line) and asserts the
DESTINATION — a `review_attestation` event in `.fno/events.jsonl`, and for
`code-review` also what `fno-agents review-coverage` reads back from that
same destination — not just that some script printed a tag.
"""
from __future__ import annotations

import json
import os
import re
import stat
import subprocess
from pathlib import Path

import pytest

_REPO_ROOT = Path(__file__).resolve().parents[3]
_SKILL_MD = _REPO_ROOT / "skills" / "review" / "SKILL.md"
_EMIT = _REPO_ROOT / "skills" / "review" / "scripts" / "emit-attestation.sh"
_CONSUME_PEER = _REPO_ROOT / "skills" / "review" / "scripts" / "consume-peer-verdict.sh"
_HOOK = _REPO_ROOT / "hooks" / "code-review-attest.sh"


def _producer_names() -> list[str]:
    """Parse the producer list straight out of the sentence at SKILL.md:112.

    Pinned to the sentence's own wording ("are local review producers"), not
    to a line number, so the list moves with the doc. A name added to that
    sentence with no matching entry in _SURFACES below fails loudly instead
    of the test quietly covering fewer producers than the doc claims.
    """
    text = _SKILL_MD.read_text()
    m = re.search(
        r"^`([^`]+)`, `([^`]+)`, `([^`]+)`, and `([^`]+)` are local review producers\.",
        text,
        re.MULTILINE,
    )
    assert m, "SKILL.md producer sentence not found or changed shape - update this parser"
    return list(m.groups())


def _temp_git_repo(tmp_path: Path) -> Path:
    sub = tmp_path / "repo"
    sub.mkdir()
    for args in (
        ["git", "init", "-q"],
        ["git", "config", "user.email", "t@t.t"],
        ["git", "config", "user.name", "t"],
        ["git", "commit", "-q", "--allow-empty", "-m", "init"],
    ):
        subprocess.run(args, cwd=sub, check=True)
    return sub


def _head(repo: Path) -> str:
    return subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True, check=True
    ).stdout.strip()


def _events(repo: Path) -> list[dict]:
    path = repo / ".fno" / "events.jsonl"
    if not path.exists():
        return []
    return [json.loads(ln) for ln in path.read_text().splitlines() if ln.strip()]


def _base_env() -> dict:
    return {**os.environ, "FNO": "fno-py"}


def _drive_sigma(repo: Path) -> None:
    r = subprocess.run(
        ["bash", str(_EMIT), "sigma"], cwd=repo, env=_base_env(), capture_output=True, text=True
    )
    assert r.returncode == 0, r.stderr


def _drive_declare(repo: Path) -> None:
    r = subprocess.run(
        ["bash", str(_EMIT), "declare"], cwd=repo, env=_base_env(), capture_output=True, text=True
    )
    assert r.returncode == 0, r.stderr


def _drive_peer(repo: Path) -> None:
    verdict_file = repo / "peer-verdict.txt"
    verdict_file.write_text(
        "some review prose\n"
        + 'fno-peer-verdict: {"verdict": "clean", "blocking_findings": 0}\n'
    )
    r = subprocess.run(
        ["bash", str(_CONSUME_PEER), str(verdict_file)],
        cwd=repo, env=_base_env(), capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr


def _drive_code_review(repo: Path) -> None:
    """Feed the SubagentStop trigger the exact clean-pass shape the harness
    actually produces for a Skill-tool self-invocation of `/code-review`.

    `Skill(skill="code-review", ...)` runs the skill as a FORKED subagent
    (confirmed live by running this PR's own sized `/code-review` self-review,
    x-e97b): inside a fork the skill's own instructions forbid
    calling ReportFindings ("this review's output contract is the JSON
    block above"), so the PostToolUse(ReportFindings) surface below is
    reachable on some invocation shapes but NOT this one - the one the
    ship-and-promise self-review instruction actually drives. This is the
    primary driver; test_report_findings_path_also_emits below covers the
    other reachable path.
    """
    payload = json.dumps(
        {
            "hook_event_name": "SubagentStop",
            "description": "/code-review <level> --comment --fix",
            "last_assistant_message": "## Review findings\n\n```json\n[]\n```\n",
            "cwd": str(repo),
        }
    )
    r = subprocess.run(
        ["bash", str(_HOOK)],
        input=payload, cwd=repo, env=_base_env(), capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr


def test_report_findings_path_also_emits(tmp_path: Path) -> None:
    """The OTHER reachable path: a foreground pass that does call
    ReportFindings directly. A guard on only one of the two paths that can
    produce a clean code-review verdict is decorative (AGENTS.md pitfalls
    corpus) - this is the second path, not a duplicate of the SubagentStop
    driver above."""
    repo = _temp_git_repo(tmp_path)
    head = _head(repo)
    payload = json.dumps(
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "ReportFindings",
            "tool_input": {"findings": []},
            "cwd": str(repo),
        }
    )
    r = subprocess.run(
        ["bash", str(_HOOK)],
        input=payload, cwd=repo, env=_base_env(), capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    events = [e for e in _events(repo) if e.get("type") == "review_attestation"]
    assert len(events) == 1, events
    assert events[0]["data"]["reviewer"] == "code-review"
    assert events[0]["data"]["head_sha"] == head


def test_subagent_stop_ignores_a_non_code_review_description(tmp_path: Path) -> None:
    """A SubagentStop from an unrelated subagent must never attest - neither
    identification signal (description, or the code-review skill's own
    "## Review findings" heading) is present here."""
    repo = _temp_git_repo(tmp_path)
    payload = json.dumps(
        {
            "hook_event_name": "SubagentStop",
            "description": "some other subagent task",
            "last_assistant_message": "```json\n[]\n```",
            "cwd": str(repo),
        }
    )
    r = subprocess.run(
        ["bash", str(_HOOK)],
        input=payload, cwd=repo, env=_base_env(), capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    assert _events(repo) == []


def test_subagent_stop_identifies_by_message_shape_alone(tmp_path: Path) -> None:
    """A second self-review of this exact PR (x-e97b) found the description
    field was a guess: Claude Code's docs confirm only a generic `agent_type`
    field, not a code-review-specific one. The content shape - the skill's
    own "## Review findings" heading, observed verbatim across two live
    self-reviews of this PR - must identify a clean pass on its own, with no
    description match at all."""
    repo = _temp_git_repo(tmp_path)
    head = _head(repo)
    payload = json.dumps(
        {
            "hook_event_name": "SubagentStop",
            "description": "a generic fork type, not a skill name",
            "last_assistant_message": "## Review findings\n\n```json\n[]\n```\n",
            "cwd": str(repo),
        }
    )
    r = subprocess.run(
        ["bash", str(_HOOK)],
        input=payload, cwd=repo, env=_base_env(), capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    events = [e for e in _events(repo) if e.get("type") == "review_attestation"]
    assert len(events) == 1, events
    assert events[0]["data"]["head_sha"] == head


def test_subagent_stop_with_findings_emits_nothing(tmp_path: Path) -> None:
    repo = _temp_git_repo(tmp_path)
    payload = json.dumps(
        {
            "hook_event_name": "SubagentStop",
            "description": "/code-review <level> --comment --fix",
            "last_assistant_message": (
                '## Review findings\n\n```json\n[{"file": "a.py", "summary": "x", '
                '"failure_scenario": "y"}]\n```\n'
            ),
            "cwd": str(repo),
        }
    )
    r = subprocess.run(
        ["bash", str(_HOOK)],
        input=payload, cwd=repo, env=_base_env(), capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    assert _events(repo) == []


_SURFACES = {
    "sigma": _drive_sigma,
    "peer": _drive_peer,
    "code-review": _drive_code_review,
    "declare": _drive_declare,
}


@pytest.mark.parametrize("reviewer", _producer_names())
def test_producer_emits_review_attestation_with_no_second_call(
    reviewer: str, tmp_path: Path
) -> None:
    driver = _SURFACES.get(reviewer)
    assert driver is not None, (
        f"'{reviewer}' is named a producer at SKILL.md:112 but this test has no "
        "real-world driver for it - add one to _SURFACES, do not just widen the doc"
    )
    repo = _temp_git_repo(tmp_path)
    head = _head(repo)

    driver(repo)

    events = [e for e in _events(repo) if e.get("type") == "review_attestation"]
    assert len(events) == 1, (
        f"expected exactly one review_attestation from a single clean pass, got {events}"
    )
    data = events[0]["data"]
    assert data["reviewer"] == reviewer
    assert data["verdict"] == "pass"
    assert data["head_sha"] == head


def test_a_non_empty_findings_report_emits_nothing() -> None:
    """The re-report-after-fixes shape (findings non-empty) must not attest:
    the fixes are about to move HEAD, and an attestation pinned before that
    move is dead on arrival anyway (sigma's own rule, mirrored here)."""
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        repo = _temp_git_repo(Path(td))
        payload = json.dumps(
            {
                "hook_event_name": "PostToolUse",
                "tool_name": "ReportFindings",
                "tool_input": {
                    "findings": [{"file": "a.py", "summary": "x", "failure_scenario": "y"}]
                },
                "cwd": str(repo),
            }
        )
        r = subprocess.run(
            ["bash", str(_HOOK)],
            input=payload, cwd=repo, env=_base_env(), capture_output=True, text=True,
        )
        assert r.returncode == 0, r.stderr
        assert _events(repo) == []


def test_a_different_tool_report_emits_nothing() -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        repo = _temp_git_repo(Path(td))
        payload = json.dumps(
            {
                "hook_event_name": "PostToolUse",
                "tool_name": "Read",
                "tool_input": {},
                "cwd": str(repo),
            }
        )
        r = subprocess.run(
            ["bash", str(_HOOK)],
            input=payload, cwd=repo, env=_base_env(), capture_output=True, text=True,
        )
        assert r.returncode == 0, r.stderr
        assert _events(repo) == []


# ── resolver leg: what the gate actually reads back ─────────────────────────


def _fno_agents_bin() -> Path | None:
    env = os.environ.get("FNO_AGENTS_BIN", "")
    if env:
        p = Path(env)
        return p if p.exists() else None
    for profile in ("debug", "release"):
        p = _REPO_ROOT / "crates" / "fno-agents" / "target" / profile / "fno-agents"
        if p.exists():
            return p
    return None


_FNO_AGENTS_BIN = _fno_agents_bin()


def _make_script(dir_: Path, name: str, body: str) -> Path:
    p = dir_ / name
    p.write_text(f"#!/usr/bin/env bash\n{body}\n")
    p.chmod(p.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    return p


@pytest.mark.skipif(
    _FNO_AGENTS_BIN is None,
    reason="fno-agents binary not built (cargo build -p fno-agents); set FNO_AGENTS_BIN",
)
def test_resolver_reads_the_code_review_attestation_at_head(tmp_path: Path) -> None:
    """Pin the DESTINATION, not the tag: prove `fno-agents review-coverage` -
    the exact resolver the reviewers gate reads - reports this PR's own
    producer as `reviewed_count > 0` at HEAD, not merely that a JSON line
    with the right shape was written somewhere."""
    repo = _temp_git_repo(tmp_path)
    head = _head(repo)
    _drive_code_review(repo)

    bins = tmp_path / "bins"
    bins.mkdir()
    gh = _make_script(
        bins,
        "gh",
        f"""
set -euo pipefail
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"{head}","mergeable":"MERGEABLE","baseRefName":"main"}}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{{"name":"ci","state":"SUCCESS","bucket":"pass"}}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then echo '[]'; exit 0; fi
if echo "$*" | grep -q "reviews"; then echo '{{"reviews":[],"comments":[]}}'; exit 0; fi
exit 1
""",
    )
    git = _make_script(
        bins,
        "git",
        """
case "$*" in
  *--raw*) exit 1 ;;
  *) echo "$FAKE_HEAD" ;;
esac
""",
    )

    env = {
        **os.environ,
        "FAKE_HEAD": head,
    }
    r = subprocess.run(
        [
            str(_FNO_AGENTS_BIN), "review-coverage",
            "--cwd", str(repo),
            "--pr", "1",
            "--head", head,
            "--gh-bin", str(gh),
            "--git-bin", str(git),
            "--global-settings", "/nonexistent/global-settings.yaml",
            "--author-harness", "none",
        ],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    data = json.loads(r.stdout)
    assert data["reviewed_count"] >= 1, data
    names = [v["name"] for v in data["verdicts"]]
    assert "code-review" in names, data
