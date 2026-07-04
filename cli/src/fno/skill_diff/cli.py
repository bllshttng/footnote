"""``fno skill-diff`` - the skill-diff proposer loop.

`tick`      one loop iteration: scan observer events -> propose a cited diff PR,
            file a no-diff-helps node, or close a run as a no-op. Level-gated
            (report = dry-run and default; assisted = actually open the PR).
`reconcile` AC10-FR backstop: find merged proposer PRs with no eval-closed
            receipt and report/schedule the re-eval.
"""
from __future__ import annotations

import logging
import subprocess
import sys
from pathlib import Path
from typing import Optional

import typer

from fno import paths
from fno.config import load_settings
from fno.loops import loop_level, loops_paused
from fno.skill_diff import engine, guards, synthesize

_LOG = logging.getLogger(__name__)

skill_diff_app = typer.Typer(
    name="skill-diff",
    no_args_is_help=True,
    help="Skill-diff proposer: observer failure patterns -> cited SKILL.md diff -> PR.",
)


@skill_diff_app.callback()
def _callback() -> None:
    """No-op: keeps Typer from collapsing the multi-command sub-app."""


# --------------------------------------------------------------------------- #
# plumbing
# --------------------------------------------------------------------------- #

def _events_paths() -> list[Path]:
    """Project log first (canonical), then the global log (best-effort)."""
    project = paths.project_log("events.jsonl")
    glob = paths.ledger_json().parent / "events.jsonl"
    return [project] if project == glob else [project, glob]


def _emit(type_name: str, data: dict) -> bool:
    """Emit through the house envelope+validator. Returns canonical-log success."""
    from fno.events import _build, append_event

    event = _build(type_name, "skill_diff", data)
    canonical_ok = False
    for i, p in enumerate(_events_paths()):
        try:
            append_event(event, p)
            if i == 0:
                canonical_ok = True
        except Exception as exc:  # noqa: BLE001 - a bad global log never fails the tick
            print(f"skill-diff: emit {type_name} to {p} failed: {exc}", file=sys.stderr)
    return canonical_ok


def _emit_tick(name: str, level: str, outcome: str) -> None:
    _emit("loop_tick", {"name": name, "level": level, "outcome": outcome})


def _skill_dir(skill_id: str) -> Path:
    """fno:blueprint -> <repo>/skills/blueprint."""
    short = skill_id.split(":", 1)[-1]
    return paths.resolve_repo_root() / "skills" / short


def _read_skill_files(skill_id: str) -> dict[str, str]:
    """SKILL.md only (bounded evidence posture - a whole references/ tree would
    blow the synthesis prompt)."""
    md = _skill_dir(skill_id) / "SKILL.md"
    try:
        return {str(md.relative_to(paths.resolve_repo_root())): md.read_text(encoding="utf-8")}
    except OSError:
        return {}


def _blob_hash(rel_path: str) -> str:
    """git blob hash of the working-tree file, or 'unknown' when unavailable
    (best-effort skill-version identity: an absent hash is never a blocker)."""
    try:
        out = subprocess.run(
            ["git", "hash-object", rel_path],
            cwd=paths.resolve_repo_root(),
            capture_output=True,
            text=True,
            check=True,
        )
        return out.stdout.strip() or "unknown"
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def _project_names() -> list[str]:
    """Every project name in the workspace work-map (redaction guard input, A3)."""
    names: list[str] = []
    try:
        work = load_settings().work
        for ws in work.workspaces.values():
            for proj in ws.projects:
                if proj.name:
                    names.append(proj.name)
    except Exception as exc:  # noqa: BLE001 - a malformed work-map must not uncap redaction
        _LOG.warning("skill-diff: could not read work-map for redaction names: %s", exc)
    return names


def _file_no_diff_node(skill_id: str, run_id: str, reason: str) -> Optional[str]:
    """File a backlog node for an architectural finding (no-diff-helps path).

    Filed WITH implementation guidance so it is a real follow-up, not a bare
    idea. Returns the node id, or None on failure (retried next tick - AC7's
    "degrade, don't crash").
    """
    title = f"skill-diff: {skill_id} failure looks architectural (run {run_id})"
    details = (
        f"The skill-diff proposer hit its local-maxima ceiling for `{skill_id}`: the top "
        f"failure dimension resisted proposals across the window ({reason}). Diffing that "
        "dimension again is chasing a local maximum. Needs a design look, not another wording "
        "tweak - the failure is likely architectural. See observer run {run_id}'s failure ranking."
    ).format(run_id=run_id)
    try:
        out = subprocess.run(
            ["fno", "backlog", "idea", title, "-d", details, "-p", "p2", "-t", "feature"],
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        _LOG.warning("skill-diff: filing no-diff-helps node failed: %s", exc)
        return None
    # `idea` prints the new node id somewhere in its receipt; grab the first token
    # matching the id shape.
    import re

    m = re.search(r"\b[a-z]+-[0-9a-f]{3,}\b", out.stdout)
    return m.group(0) if m else None


def _apply_and_open_pr(
    *, skill_id: str, run_id: str, hunks: list[dict], body: str, cited: list[str]
) -> tuple[Optional[int], str]:
    """assisted-level: apply hunks, branch, commit, push, gh pr create.

    Never merges, never touches a default branch directly.
    Returns (pr_number|None, branch). Raises on any git/gh failure so the caller
    takes no-action rather than reporting a phantom PR.
    """
    root = paths.resolve_repo_root().resolve()
    skills_root = (root / "skills").resolve()
    short_run = run_id.split("-")[-1][:12]
    branch = f"skill-diff/{skill_id.split(':')[-1]}-{short_run}"
    touched: list[str] = []
    for h in hunks:
        # h["file"] comes from untrusted LLM output: resolve it and refuse any
        # path that escapes the skills/ tree (traversal / absolute-path guard).
        f = (root / h["file"]).resolve()
        if not (f == skills_root or skills_root in f.parents):
            raise RuntimeError(f"hunk path escapes skills/: {h['file']}")
        old, new = h.get("old_text", ""), h.get("new_text", "")
        current = f.read_text(encoding="utf-8") if f.exists() else ""
        if old:
            if old not in current:
                raise RuntimeError(f"hunk old_text not found in {h['file']} (file drifted)")
            current = current.replace(old, new, 1)
        elif current:
            current = current.rstrip("\n") + "\n" + new.rstrip("\n") + "\n"
        else:
            # New/empty file: no leading blank line.
            current = new.rstrip("\n") + "\n"
        f.write_text(current, encoding="utf-8")
        touched.append(str(f.relative_to(root)))

    def git(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, check=True)

    commit_msg = f"docs(skill): {skill_id} diff from observer run {short_run}\n\ncites: {', '.join(cited)}"
    # Redaction guard over BOTH the PR body and the commit message (A3): the two
    # surfaces check-no-internal-refs CI never sees.
    hits = guards.redaction_violations(body + "\n" + commit_msg, _project_names())
    if hits:
        raise RuntimeError(f"redaction guard refused open: {hits}")

    git("checkout", "-B", branch)  # -B: reset if the branch exists (idempotent retry)
    git("add", *touched)
    git("commit", "-m", commit_msg)
    git("push", "-u", "origin", branch)

    body_file = paths.resolve_repo_root() / ".fno" / f".skill-diff-body-{short_run}.md"
    body_file.parent.mkdir(parents=True, exist_ok=True)
    body_file.write_text(body, encoding="utf-8")
    try:
        pr = subprocess.run(
            ["gh", "pr", "create", "--title", f"skill-diff: {skill_id} (run {short_run})",
             "--body-file", str(body_file)],
            cwd=root, capture_output=True, text=True, check=True,
        )
    finally:
        body_file.unlink(missing_ok=True)
    import re

    m = re.search(r"/pull/(\d+)", pr.stdout)
    return (int(m.group(1)) if m else None), branch


# --------------------------------------------------------------------------- #
# tick
# --------------------------------------------------------------------------- #

@skill_diff_app.command("tick")
def tick(
    skill: str = typer.Option(..., "--skill", help="blueprint | review (the pilot skill)."),
) -> None:
    """One proposer iteration over the newest unprocessed observer run.

    States on stdout, one word each: paused | no-work | noop | no-diff-helps |
    report | proposed. Never exits 0 silently with no state word.
    """
    name = engine.LOOP_NAME
    if loops_paused():
        print("paused")
        _emit_tick(name, loop_level(name), "paused")
        return

    skill_id = f"fno:{skill}" if ":" not in skill else skill
    events = engine.read_events_tolerant(_events_paths()[0])
    runs = engine.unprocessed_runs(events, skill_id)
    if not runs:
        print("no-work")
        _emit_tick(name, loop_level(name), "no-work")
        return
    run_id = runs[0]

    # AC6-EDGE: an all-pass (or empty) run is a no-op - nothing to fix.
    if not engine.has_actionable_findings(events, run_id):
        reason = "zero_findings" if not engine.findings_for_run(events, run_id) else "zero_failures"
        _emit("skill_diff_noop", {"run_id": run_id, "skill_id": skill_id, "reason": reason})
        _emit_tick(name, loop_level(name), "noop")
        print(f"noop {run_id} ({reason})")
        return

    # AC7-EDGE: local-maxima ceiling -> file a node, do not diff again.
    if engine.local_maxima_tripped(events, skill_id, run_id):
        node = _file_no_diff_node(skill_id, run_id, f"top dimension unchanged x{engine.LOCAL_MAXIMA_WINDOW}")
        _emit("skill_diff_no_diff_helps",
              {"run_id": run_id, "skill_id": skill_id, "reason": "local_maxima", "filed_node_id": node})
        _emit_tick(name, loop_level(name), "no-diff-helps")
        print(f"no-diff-helps {run_id} (local_maxima, node={node})")
        return

    level = loop_level(name)
    ranking = engine.failure_ranking(events, run_id)
    findings = engine.findings_for_run(events, run_id)

    # report (default): dry-run. Show what it WOULD process and
    # stop - no synthesis spend, no PR. Manually graduated to assisted.
    if level != "assisted":
        top = ranking[0]["dimension"] if ranking else "(none)"
        print(f"report {run_id} skill={skill_id} findings={len(findings)} top_failure={top} "
              f"(level={level}; set config.loops.{name}.level=assisted to open PRs)")
        _emit_tick(name, level, "report")
        return

    # assisted: synthesize -> guards -> PR (or no-diff-helps).
    rc = engine.run_complete_event(events, run_id)
    version_observed = (rc and (rc.get("data") or {}).get("skill_version")) or "unknown"
    skill_files = _read_skill_files(skill_id)
    prompt = synthesize.build_prompt(
        skill_id=skill_id, skill_files=skill_files, findings=findings, ranking=ranking,
        history=engine.prior_proposed(events, skill_id), additive_threshold=guards.ADDITIVE_LINE_THRESHOLD,
    )
    try:
        proposal = synthesize.synthesize(prompt)
    except synthesize.ProposalParseError as exc:
        # A synthesis crash/timeout is a tool fault, not a skill verdict: take no
        # action this tick, leave the run_id unprocessed, retry next tick.
        print(f"report {run_id} (synthesis failed, no action: {exc})")
        _emit_tick(name, level, "report")
        return

    if proposal.verdict == "no_diff_helps":
        node = _file_no_diff_node(skill_id, run_id, proposal.no_diff_reason or "synth")
        _emit("skill_diff_no_diff_helps",
              {"run_id": run_id, "skill_id": skill_id, "reason": "synth_no_diff", "filed_node_id": node})
        _emit_tick(name, level, "no-diff-helps")
        print(f"no-diff-helps {run_id} (synth, node={node})")
        return

    kept, dropped = guards.filter_cited_hunks(proposal.hunks)
    if not kept:
        # AC2-ERR: every hunk uncited -> no PR, take the no-diff-helps path.
        node = _file_no_diff_node(skill_id, run_id, "all hunks uncited")
        _emit("skill_diff_no_diff_helps",
              {"run_id": run_id, "skill_id": skill_id, "reason": "all_hunks_uncited", "filed_node_id": node})
        _emit_tick(name, level, "no-diff-helps")
        print(f"no-diff-helps {run_id} (all {len(dropped)} hunks uncited, node={node})")
        return

    # AC4-UI: additive-only over threshold requires justification.
    justification = proposal.justification
    if guards.additive_only_needs_justification(kept) and not (justification or "").strip():
        node = _file_no_diff_node(skill_id, run_id, "additive-only without justification")
        _emit("skill_diff_no_diff_helps",
              {"run_id": run_id, "skill_id": skill_id, "reason": "synth_no_diff", "filed_node_id": node})
        _emit_tick(name, level, "no-diff-helps")
        print(f"no-diff-helps {run_id} (additive-only, no justification)")
        return

    bloat = guards.bloat_flag(engine.prior_proposed(events, skill_id))
    added, removed = guards.count_lines(kept)
    cited = sorted({c for h in kept for c in h["cited_finding_ids"]})
    rel = next(iter(skill_files), f"skills/{skill}/SKILL.md")
    version_against = _blob_hash(rel)
    body = engine.build_pr_body(
        run_id=run_id, skill_id=skill_id, hunks=kept, justification=justification, bloat=bloat,
        version_observed=version_observed, version_against=version_against,
        is_review_skill=(skill == "review"),
    )
    try:
        pr_number, branch = _apply_and_open_pr(
            skill_id=skill_id, run_id=run_id, hunks=kept, body=body, cited=cited
        )
    except (RuntimeError, subprocess.CalledProcessError, OSError) as exc:
        # includes the redaction refusal (A3): record it, take no PR.
        reason = "redaction_refused" if "redaction" in str(exc) else "synth_no_diff"
        _emit("skill_diff_no_diff_helps",
              {"run_id": run_id, "skill_id": skill_id, "reason": reason, "filed_node_id": None})
        _emit_tick(name, level, "no-diff-helps")
        print(f"no-diff-helps {run_id} (open refused: {exc})")
        return

    _emit("skill_diff_proposed", {
        "run_id": run_id, "skill_id": skill_id,
        "skill_version_observed": version_observed, "skill_version_proposed_against": version_against,
        "pr_number": pr_number, "branch": branch, "cited_finding_ids": cited,
        "added_lines": added, "removed_lines": removed,
        "justification": justification,
    })
    _emit_tick(name, level, "proposed")
    print(f"proposed {run_id} PR#{pr_number} branch={branch} +{added}/-{removed} cited={cited}")


# --------------------------------------------------------------------------- #
# reconcile (AC10-FR backstop)
# --------------------------------------------------------------------------- #

@skill_diff_app.command("reconcile")
def reconcile() -> None:
    """Detect merged proposer PRs with no eval-closed receipt (AC10-FR).

    Report-only: lists proposer PRs that merged but never produced a
    ``skill_diff_eval_closed`` event, so the eval-after-merge loop can be
    re-scheduled. Never retries silently forever - it just surfaces the gap.
    """
    events = engine.read_events_tolerant(_events_paths()[0])
    proposed = {
        (e.get("data") or {}).get("pr_number"): (e.get("data") or {})
        for e in events
        if e.get("type") == "skill_diff_proposed" and (e.get("data") or {}).get("pr_number")
    }
    closed = {
        (e.get("data") or {}).get("pr_number")
        for e in events
        if e.get("type") == "skill_diff_eval_closed"
    }
    open_gaps = [pr for pr in proposed if pr not in closed]
    if not open_gaps:
        print("reconcile: no un-closed proposer PRs")
        return
    for pr in open_gaps:
        d = proposed[pr]
        merged = _pr_merged(pr)
        state = "merged" if merged else "not-yet-merged"
        print(f"reconcile: PR#{pr} ({d.get('skill_id')}, run {d.get('run_id')}) {state}, no eval-closed")


def _pr_merged(pr_number: int) -> Optional[bool]:
    """Best-effort merge check; None when gh is unavailable (offline)."""
    try:
        out = subprocess.run(
            ["gh", "pr", "view", str(pr_number), "--json", "state", "-q", ".state"],
            cwd=paths.resolve_repo_root(), capture_output=True, text=True, check=True,
        )
        return out.stdout.strip() == "MERGED"
    except (OSError, subprocess.CalledProcessError):
        return None
