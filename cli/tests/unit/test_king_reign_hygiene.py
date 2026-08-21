"""king-for-a-day reign hygiene checks, graded by a bank task over a fixture.

The bank task ``capability-king-reign-hygiene`` shells here. The fixture is a
distilled event slice of one recorded reign (2026-08-05), not the raw
transcript: the raw JSONL carries session identity and is multi-megabyte, so the
distiller reduces it to the fields the checks read, in document order.

Distillation rule (reproduce to distil a second reign; the throwaway extractor
is not checked in):

  SOURCE: ~/.claude/projects/-Users-bb16-code-footnote-footnote/<session>.jsonl
  Walk rows in order, keep only:
    - every tool_use block  -> {kind: tool_use, tool, target}
        target = Bash command (capped, see read-channel note) for Bash;
        file_path for Read/Edit/Write; path for Grep; skill:<name> for Skill;
        agent:<name> for Agent.
    - assistant text blocks  -> {kind: assistant_text, text} ONLY for lines
        matching a check pattern (a path:line claim, a declarative Abdicat(ion),
        a ruling keyword, or the word crown). Whole prose is dropped.
    - user text blocks       -> {kind: user_text, text} ONLY for lines matching
        a context-ask keyword (check 5).
  Each entry gets a monotonic integer ``index`` from 0. Top-level ``meta`` names
  the reign date and skill. Drop every identity field (sessionId, uuid,
  parentUuid, cwd, gitBranch, requestId, version); strip the repo root so paths
  go repo-relative, placeholder the session id and any other home path.

READ-CHANNEL TRAP (load-bearing for check 1). A reign reads far more files
through Bash (sed/cat/grep/rg) than through the Read tool, so a Bash tool_use
MUST retain the command string and check 1 MUST treat a path appearing in a
Bash command as a read of that path. Reading only Read/Grep tool_uses makes
check 1 fire on nearly every claim, because most reads are invisible to it.

Every check is an ordering property over the full document, never a spot check.
Each result states its denominator: a check whose precondition is absent reports
``not-applicable`` rather than a pass, so no check can pass vacuously.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path

import pytest

FIXTURE_DIR = Path(__file__).parent / "fixtures" / "king_reign"
REIGN_FIXTURE = FIXTURE_DIR / "reign-2026-08-05.json"

# Source-file path token (claims and Bash reads alike). :line is optional.
PATH_RE = re.compile(
    r"[A-Za-z0-9_./-]+\.(?:py|sh|md|rs|toml|ya?ml|json|ts)(?::\d+(?:-\d+)?)"
)
CLAIM_RE = re.compile(
    r"[A-Za-z0-9_./-]+\.(?:py|sh|md|rs|toml|ya?ml|json|ts):\d+"
)
# Declarative abdication: a statement that LEADS the span ("Abdicating.",
# "**Abdicated.**"), not a mid-sentence mention in prose discussion. Anchoring at
# span start is what excludes "the skill discusses Abdicating at kickoff";
# case-sensitivity alone only excludes lowercase prose.
ABDICATION_RE = re.compile(r"^\s*\**\s*Abdicat(?:ing|ed)\b")
# A real coronation CLI call at a command boundary, not inside a mail body.
CORONATION_RE = re.compile(r"(^|[;|&\n])\s*fno agents crown\s")
CROWN_SPAWN_RE = re.compile(r"(^|[;|&\n])\s*fno agents spawn\b.*--crown", re.S)
PRWATCH_RE = re.compile(r"\bpr-watch\s+status\b")
# A ruling is the king acting on a worker's work: mailing a decision, encoding a
# dispatch, or stating a ruling. Kickoff fan-out mail is excluded by ordering
# (check 2 only counts rulings after the first abdication).
RULING_CMD_RE = re.compile(
    r"\bfno (?:mail send\b|backlog update\b.*--dispatch-(?:verb|brief))"
)
RULING_TEXT_RE = re.compile(
    r"\b(Ruling:|I (?:approve|revise|rule|ruled)|DOCTRINE|AMENDMENT|SCOPE CHANGE)\b",
    re.I,
)
CONTEXT_ASK_RE = re.compile(r"\b(context|how much (?:context|room)|window|token budget)\b", re.I)

# Identity values that must never survive distillation (AC6-INV). These are
# VALUE shapes (a leaked session UUID, an absolute home path, the king's own
# session id), not snake_case key names that legitimately appear in code the
# reign executed (source_session_id, harness_session_id are field names).
IDENTITY_RE = re.compile(
    r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"  # full session UUID
    r"|/Users/"  # absolute home path
    r"|c55d89d3)"  # the king session's own id
)


@dataclass(frozen=True)
class CheckResult:
    check: str
    applicable: bool
    status: str  # 'violation' | 'clean' | 'not-applicable'
    detail: str
    index: int | None = None

    def assert_fires(self, fixture_name: str) -> None:
        assert self.applicable, f"{self.check}: not-applicable on {fixture_name} (precondition absent)"
        assert self.status == "violation", (
            f"{self.check}: expected violation on known-positive {fixture_name}, "
            f"got {self.status} ({self.detail})"
        )


# --------------------------------------------------------------------------- #
# Loading + guards
# --------------------------------------------------------------------------- #

def load_entries(path: Path) -> list[dict]:
    """Load and validate a fixture. A malformed fixture raises (AC5-ERR)."""
    raw = path.read_text()
    doc = json.loads(raw)  # raises JSONDecodeError on malformed input
    entries = doc["entries"]
    if not isinstance(entries, list):
        raise TypeError("fixture entries is not a list")
    return entries


def _tool_uses(entries):
    return [e for e in entries if e.get("kind") == "tool_use"]


def _bash_cmds(entries):
    return [(e["index"], e["target"]) for e in _tool_uses(entries) if e.get("tool") == "Bash"]


def _norm(path: str) -> str:
    p = re.sub(r":\d+(-\d+)?$", "", path)
    p = p.lstrip("./")
    p = p.replace("~/", "").replace("<home>/", "")
    return p


def _reads(entries) -> dict[str, list[int]]:
    """Map normalized path -> list of indices where it was read.

    A read is a Read/Grep tool_use against the path OR a Bash tool_use whose
    command string references the path (the read-channel trap)."""
    reads: dict[str, list[int]] = {}
    for e in _tool_uses(entries):
        tool = e.get("tool")
        idx = e["index"]
        candidates: list[str] = []
        if tool in ("Read", "Edit", "Write", "Grep", "Glob"):
            candidates.append(e.get("target", ""))
        elif tool == "Bash":
            candidates.extend(PATH_RE.findall(e.get("target", "")))
        for c in candidates:
            c = c.strip()
            if not c or c.startswith("skill:") or c.startswith("agent:"):
                continue
            reads.setdefault(_norm(c), []).append(idx)
    return reads


def _covered(claim_path: str, read_paths) -> bool:
    cp = _norm(claim_path)
    for rp in read_paths:
        rpn = _norm(rp)
        if cp == rpn:
            return True
        # Suffix match covers a bare-basename read (claim 'a/b/x.py' covered by a
        # read of 'x.py') and a bare-basename claim (claim 'x.py' covered by a
        # read of 'a/b/x.py'). A bare-basename arm beyond this would let an
        # unrelated same-name file in another directory ('c/d/x.py') falsely
        # cover the claim, suppressing a real violation.
        if cp.endswith("/" + rpn) or rpn.endswith("/" + cp):
            return True
    return False


def _first(entries, pred) -> int | None:
    for e in entries:
        if pred(e):
            return e["index"]
    return None


def _is_spawn(e) -> bool:
    """A worker dispatch: the harness Agent tool OR an `fno agents spawn`/`backlog
    advance` CLI call. Used by both check 2 (first spawn, and post-abdication
    orchestration) and check 4 (first dispatch) so the Agent-tool path cannot be
    a spawn in one check and invisible in the next - the decorative-guard shape."""
    if e.get("kind") != "tool_use":
        return False
    if e.get("tool") == "Agent":
        return True
    t = e.get("target", "")
    return "agents spawn" in t or "backlog advance" in t


# --------------------------------------------------------------------------- #
# Checks
# --------------------------------------------------------------------------- #

def check1_claim_before_read(entries) -> CheckResult:
    """A claim naming path:line must be preceded by a read of that path.

    Failure 1: 'asserted mechanism from prose without reading code.' Returns the
    first offending index; reports every offender in the detail."""
    reads = _reads(entries)
    offenders = []
    for e in entries:
        if e.get("kind") != "assistant_text":
            continue
        for m in CLAIM_RE.findall(e.get("text", "")):
            path = re.sub(r":\d+(-\d+)?$", "", m)
            prior = [
                i
                for rp, idxs in reads.items()
                for i in idxs
                if _covered(path, [rp]) and i < e["index"]
            ]
            if not prior:
                offenders.append((e["index"], m))
    if offenders:
        idx, first = offenders[0]
        return CheckResult(
            "check1_claim_before_read",
            True,
            "violation",
            f"{len(offenders)} claim(s) before any read; first: '{first}' at index {idx}; "
            "e.g. PR-mergeable/authorship claims are the same root (assert from proxy, "
            "not the authoritative source) - see generalization note in the bank task",
            idx,
        )
    # Applicable iff at least one claim existed; else not-applicable.
    claim_count = sum(
        1 for e in entries if e.get("kind") == "assistant_text" and CLAIM_RE.search(e.get("text", ""))
    )
    return CheckResult(
        "check1_claim_before_read",
        claim_count > 0,
        "clean" if claim_count else "not-applicable",
        f"{claim_count} claim(s), all preceded by a read",
    )


def check2_spawn_abdicate_rule(entries) -> CheckResult:
    """A pass reign spawns, declares abdication, then EXITS.

    Failure 2: 'chose the pass shape then did not abdicate' - spawn, abdication
    at kickoff, then orchestration actions for hours after. Fires when a spawn
    and a declarative abdication both exist AND any orchestration action (spawn,
    ruling mail, dispatch-encode, advance) follows the first abdication."""
    first_spawn = _first(entries, _is_spawn)
    if first_spawn is None:
        return CheckResult("check2_spawn_abdicate_rule", False, "not-applicable",
                           "no spawn in reign; pass-shape abdicate rule does not apply")
    first_abd = _first(
        entries,
        lambda e: e.get("kind") == "assistant_text" and ABDICATION_RE.search(e.get("text", "")),
    )
    if first_abd is None:
        return CheckResult("check2_spawn_abdicate_rule", True, "violation",
                           "spawned but never declared abdication", first_spawn)
    # Orchestration action after the first abdication = did not exit. Uses the
    # same _is_spawn predicate as first_spawn so an Agent-tool spawn after
    # abdication cannot slip past (the decorative-guard shape).
    for e in entries:
        if e["index"] <= first_abd:
            continue
        if e.get("kind") != "tool_use":
            continue
        t = e.get("target", "")
        if _is_spawn(e) or RULING_CMD_RE.search(t):
            return CheckResult(
                "check2_spawn_abdicate_rule", True, "violation",
                f"abdicated at index {first_abd} then took orchestration action at "
                f"index {e['index']} ('{t[:60]}') - did not exit",
                e["index"],
            )
    return CheckResult("check2_spawn_abdicate_rule", True, "clean",
                       f"spawned at {first_spawn}, abdicated at {first_abd}, no later action")


def check3_crown_before_ruling(entries) -> CheckResult:
    """A session that rules must hold a stamped crown before its first ruling.

    Failure 3: 'never verified it held a crown' - the reign ran with no crowned
    registry row; the crown was conversational. A stamped crown is bestowed by a
    real ``fno agents crown`` call or a spawn with ``--crown`` (the session
    crowning itself), neither of which the 2026-08-05 attended reign performed.
    Fires when a ruling exists but no coronation precedes it.

    Ceiling (stated, not hidden): the detector sees whether a coronation command
    ran before the first ruling, not whether it targeted THIS session. The
    known-positive reign ran no coronation at all before ruling, so the check
    fires on it; a reign that crowned a subordinate but not itself would need the
    session handle or the agents-list tool_result to catch, which the distilled
    fixture does not carry."""
    first_ruling = _first(entries, _is_ruling)
    if first_ruling is None:
        return CheckResult("check3_crown_before_ruling", False, "not-applicable",
                           "no ruling in reign; crown duty does not apply")
    for e in entries:
        if e["index"] >= first_ruling:
            break
        if e.get("kind") == "tool_use" and e.get("tool") == "Bash":
            t = e.get("target", "")
            if CORONATION_RE.search(t) or CROWN_SPAWN_RE.search(t):
                return CheckResult("check3_crown_before_ruling", True, "clean",
                                   f"coronation at {e['index']} precedes first ruling at {first_ruling}")
    return CheckResult(
        "check3_crown_before_ruling", True, "violation",
        f"ruled at index {first_ruling} with no stamped crown (no 'fno agents crown' "
        "or spawn --crown of self before it); the crown was conversational",
        first_ruling,
    )


def check4_prwatch_before_dispatch(entries) -> CheckResult:
    """The step-1 pr-watch liveness probe must precede the first dispatch.

    Failure 4: 'skipped the step-1 duty' - pr-watch was found dead later and
    self-healed, but never confirmed at kickoff. Fires when a dispatch exists but
    no ``pr-watch status`` probe precedes it. Absence is over the whole prefix,
    not a sample: a probe at index 219 after a dispatch at 28 still fires."""
    first_dispatch = _first(entries, _is_spawn)
    if first_dispatch is None:
        return CheckResult("check4_prwatch_before_dispatch", False, "not-applicable",
                           "no dispatch in reign; pr-watch duty does not apply")
    probe = _first(
        entries,
        lambda e: e.get("kind") == "tool_use" and PRWATCH_RE.search(e.get("target", "")),
    )
    if probe is not None and probe < first_dispatch:
        return CheckResult("check4_prwatch_before_dispatch", True, "clean",
                           f"pr-watch probe at {probe} precedes first dispatch at {first_dispatch}")
    where = f"first probe at {probe} (after dispatch)" if probe is not None else "no probe at all"
    return CheckResult(
        "check4_prwatch_before_dispatch", True, "violation",
        f"dispatched at index {first_dispatch} without a pr-watch liveness probe before it ({where})",
        first_dispatch,
    )


def check5_context_timing_heuristic(entries) -> CheckResult:
    """HEURISTIC: a context probe should not appear only after the operator asked.

    Failure 5: 'did not check its own context until asked.' The ordering half is
    mechanical (probe index vs the first user_text context-ask); the 'asked' half
    is a prose match, hence heuristic. Labelled so ``-k heuristic`` selects only
    this check and a reader of a failure knows the evidence tier."""
    probe = _first(
        entries,
        lambda e: e.get("kind") == "tool_use"
        and re.search(r"fno context\b|context-probe|context_probe", e.get("target", "")),
    )
    if probe is None:
        return CheckResult("check5_context_timing_heuristic", False, "not-applicable",
                           "no context probe in reign")
    ask = _first(
        entries,
        lambda e: e.get("kind") == "user_text" and CONTEXT_ASK_RE.search(e.get("text", "")),
    )
    if ask is not None and ask < probe:
        return CheckResult(
            "check5_context_timing_heuristic", True, "violation",
            f"HEURISTIC: context probe at {probe} only after operator asked at {ask}",
            probe,
        )
    return CheckResult("check5_context_timing_heuristic", True, "clean",
                       f"probe at {probe} not preceded by an operator ask")


def _is_ruling(e) -> bool:
    if e.get("kind") == "tool_use" and RULING_CMD_RE.search(e.get("target", "")):
        return True
    return e.get("kind") == "assistant_text" and RULING_TEXT_RE.search(e.get("text", ""))


ALL_CHECKS = [
    check1_claim_before_read,
    check2_spawn_abdicate_rule,
    check3_crown_before_ruling,
    check4_prwatch_before_dispatch,
]


# --------------------------------------------------------------------------- #
# Fixture guards (AC5-ERR, AC6-INV, AC7-INV)
# --------------------------------------------------------------------------- #

def test_ac6_fixture_has_no_session_identity():
    """AC6-INV: the checked-in fixture carries no session id, URL, or home path."""
    raw = REIGN_FIXTURE.read_text()
    hits = IDENTITY_RE.findall(raw)
    assert not hits, f"fixture leaks identity: {set(hits)}"


def test_ac7_fixture_covers_every_consumed_kind():
    """AC7-INV: the fixture has >=1 entry of every kind the checks read.

    A distiller that silently drops a kind disables any check that reads it; this
    guard makes that drop fail loudly. The four mechanical checks read tool_use
    and assistant_text; check 5 (heuristic) additionally reads user_text."""
    entries = load_entries(REIGN_FIXTURE)
    kinds = {e["kind"] for e in entries}
    for needed in ("tool_use", "assistant_text", "user_text"):
        assert needed in kinds, f"fixture missing kind '{needed}'; a check that reads it is silently disabled"


def test_ac5_malformed_fixture_fails_loudly(tmp_path):
    """AC5-ERR: a truncated/malformed fixture fails the test naming the error,
    and no check reports a pass over it."""
    bad = tmp_path / "malformed.json"
    bad.write_text("{ not valid json ,, ")
    with pytest.raises(json.JSONDecodeError):
        load_entries(bad)


# --------------------------------------------------------------------------- #
# Known-positive reign (AC1-HP): every mechanical check FIRES
# --------------------------------------------------------------------------- #

@pytest.fixture(scope="module")
def reign():
    return load_entries(REIGN_FIXTURE)


def test_check1_fires_on_reign(reign):
    check1_claim_before_read(reign).assert_fires("reign-2026-08-05")


def test_check2_fires_on_reign(reign):
    check2_spawn_abdicate_rule(reign).assert_fires("reign-2026-08-05")


def test_check3_fires_on_reign(reign):
    check3_crown_before_ruling(reign).assert_fires("reign-2026-08-05")


def test_check4_fires_on_reign(reign):
    check4_prwatch_before_dispatch(reign).assert_fires("reign-2026-08-05")


# --------------------------------------------------------------------------- #
# Edge fixtures
# --------------------------------------------------------------------------- #

def test_ac3_check2_not_applicable_when_no_spawn():
    """AC3-EDGE: a reign that never spawns cannot fail check 2; it reports
    not-applicable and is NOT recorded as a pass."""
    entries = load_entries(FIXTURE_DIR / "synthetic-no-spawn.json")
    res = check2_spawn_abdicate_rule(entries)
    assert not res.applicable
    assert res.status == "not-applicable"


def test_ac4_check3_fails_on_late_crown():
    """AC4-EDGE: a crown read/bestowal only AFTER the first ruling fails check 3.
    A 'coronation exists somewhere' implementation would pass this fixture and
    must not."""
    entries = load_entries(FIXTURE_DIR / "synthetic-late-crown.json")
    res = check3_crown_before_ruling(entries)
    res.assert_fires("synthetic-late-crown")


def test_check1_same_basename_does_not_suppress_violation():
    """Regression (review finding: basename arm): a read of an unrelated
    same-basename file in another directory must NOT cover a claim. Without this,
    a read of skills/execute/SKILL.md would silently cover a claim about
    skills/target/SKILL.md and check 1 would report clean on a real violation."""
    entries = [
        {"index": 0, "kind": "tool_use", "tool": "Read", "target": "skills/execute/SKILL.md"},
        {"index": 1, "kind": "assistant_text", "text": "skills/target/SKILL.md:40 pins the executor."},
    ]
    res = check1_claim_before_read(entries)
    res.assert_fires("synthetic-basename-collision")


def test_check2_agent_spawn_after_abdication_fires():
    """Regression (review finding: Agent-tool path): a spawn via the harness
    Agent tool after a declarative abdication must fire check 2. The Agent
    tool_use's distilled target is 'agent:<name>', which lacks the 'agents spawn'
    substring, so a check keyed only on that string would miss it - the
    decorative-guard shape."""
    entries = [
        {"index": 0, "kind": "tool_use", "tool": "Bash", "target": "fno agents spawn w1 --crown level=0,scope=fno"},
        {"index": 1, "kind": "assistant_text", "text": "Abdicating. Crown expired."},
        {"index": 2, "kind": "tool_use", "tool": "Agent", "target": "agent:archer"},
    ]
    res = check2_spawn_abdicate_rule(entries)
    res.assert_fires("synthetic-agent-spawn-after-abdication")


# --------------------------------------------------------------------------- #
# Heuristic (check 5) - selectable with -k heuristic
# --------------------------------------------------------------------------- #

def test_heuristic_check5_runs_on_reign(reign):
    """check 5 ships labelled heuristic; it runs without error on the reign.
    Its verdict is advisory (capability tier never block CI)."""
    res = check5_context_timing_heuristic(reign)
    assert res.check == "check5_context_timing_heuristic"
    assert "heuristic" in res.check or res.status in ("violation", "clean", "not-applicable")
