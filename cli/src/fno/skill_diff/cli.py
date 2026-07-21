"""``fno skill-diff`` - the skill-diff proposer loop.

`tick`      one loop iteration: scan observer events -> propose a cited diff PR,
            file a no-diff-helps node, or close a run as a no-op. Level-gated
            (report = dry-run and default; assisted = actually open the PR).
`reconcile` AC10-FR loop-closer: find merged proposer PRs with no eval-closed
            receipt, replay the targeted corpus items at the merge commit, fold a
            before/after delta, and emit skill_diff_eval_closed. `--pr-number`
            scopes to one PR (merge-trigger); bare sweeps (periodic backstop).
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


class RedactionRefused(RuntimeError):
    """A redaction hit refused the PR open. Distinct from a transient git/gh
    error so the caller can make it terminal (a content problem won't fix
    itself) while transient failures leave the run unprocessed for retry."""


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


def _no_diff_helps(name: str, level: str, run_id: str, skill_id: str, reason: str) -> None:
    """Terminal no-diff-helps path: file a backlog node, then emit the terminal
    event ONLY if the node was filed. If filing fails (network hiccup), emit no
    terminal event so the run stays unprocessed and the next tick retries - the
    plan's degrade-not-crash rule. Without this guard a failed `fno backlog idea`
    would still mark the run handled, making the advertised retry impossible.
    """
    node = _file_no_diff_node(skill_id, run_id, reason)
    if node is None:
        _emit_tick(name, level, "no-diff-helps-defer")
        print(f"no-diff-helps {run_id} ({reason}) - node filing failed, will retry next tick")
        return
    _emit("skill_diff_no_diff_helps",
          {"run_id": run_id, "skill_id": skill_id, "reason": reason, "filed_node_id": node})
    _emit_tick(name, level, "no-diff-helps")
    print(f"no-diff-helps {run_id} ({reason}, node={node})")


def _apply_and_open_pr(
    *, skill_id: str, run_id: str, hunks: list[dict], body: str, cited: list[str]
) -> tuple[Optional[int], str]:
    """assisted-level: apply hunks, branch, commit, push, gh pr create.

    Never merges, never touches a default branch directly.
    Returns (pr_number|None, branch). Raises on any git/gh failure so the caller
    takes no-action rather than reporting a phantom PR.
    """
    root = paths.resolve_repo_root().resolve()
    skill_root = _skill_dir(skill_id).resolve()
    short_run = run_id.split("-")[-1][:12]
    branch = f"skill-diff/{skill_id.split(':')[-1]}-{short_run}"

    # Redaction guard (A3) over EVERY surface that gets committed or pushed: the
    # PR body, the commit message, AND the hunk content itself. The first two are
    # GitHub metadata check-no-internal-refs CI never sees; the third lands in a
    # skills/ file that CI also does not scan. Refuse before writing anything.
    commit_msg = f"docs(skill): {skill_id} diff from observer run {short_run}\n\ncites: {', '.join(cited)}"
    scan_text = "\n".join([body, commit_msg] + [h.get("new_text", "") for h in hunks])
    hits = guards.redaction_violations(scan_text, _project_names())
    if hits:
        raise RedactionRefused(f"redaction guard refused open: {hits}")

    touched: list[str] = []
    for h in hunks:
        # h["file"] comes from untrusted LLM output: resolve it and refuse any
        # path outside the TARGET skill's own directory, and any non-markdown
        # file (skill content is markdown - a .py/.sh path is never legitimate).
        f = (root / h["file"]).resolve()
        if not (f == skill_root or skill_root in f.parents) or f.suffix != ".md":
            raise RuntimeError(f"hunk path not a .md under {skill_id}'s dir: {h['file']}")
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
    if not engine.has_actionable_findings(events, run_id, skill_id):
        reason = "zero_findings" if not engine.findings_for_run(events, run_id, skill_id) else "zero_failures"
        _emit("skill_diff_noop", {"run_id": run_id, "skill_id": skill_id, "reason": reason})
        _emit_tick(name, loop_level(name), "noop")
        print(f"noop {run_id} ({reason})")
        return

    # AC7-EDGE: local-maxima ceiling -> file a node, do not diff again.
    if engine.local_maxima_tripped(events, skill_id, run_id):
        _no_diff_helps(name, loop_level(name), run_id, skill_id, "local_maxima")
        return

    level = loop_level(name)
    ranking = engine.failure_ranking(events, run_id, skill_id)
    findings = engine.findings_for_run(events, run_id, skill_id)

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
        _no_diff_helps(name, level, run_id, skill_id, "synth_no_diff")
        return

    kept, dropped = guards.filter_cited_hunks(proposal.hunks)
    if not kept:
        # AC2-ERR: every hunk uncited -> no PR, take the no-diff-helps path.
        _no_diff_helps(name, level, run_id, skill_id, "all_hunks_uncited")
        return

    # AC4-UI: additive-only over threshold requires justification.
    justification = proposal.justification
    if guards.additive_only_needs_justification(kept) and not (justification or "").strip():
        _no_diff_helps(name, level, run_id, skill_id, "additive_only_no_justification")
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
    except RedactionRefused as exc:
        # A leak in the synthesized content is a hard, terminal refusal (retrying
        # would re-run synthesis on the same corpus and likely leak again).
        _emit("skill_diff_no_diff_helps",
              {"run_id": run_id, "skill_id": skill_id, "reason": "redaction_refused", "filed_node_id": None})
        _emit_tick(name, level, "no-diff-helps")
        print(f"no-diff-helps {run_id} (redaction refused: {exc})")
        return
    except (RuntimeError, subprocess.CalledProcessError, OSError) as exc:
        # A transient git/gh failure (dirty tree, push reject, network) must NOT
        # become terminal: emit no terminal event so the run stays unprocessed
        # and the next tick retries.
        _emit_tick(name, level, "open-failed")
        print(f"open-failed {run_id} (transient, will retry next tick: {exc})")
        return

    proposed_ok = _emit("skill_diff_proposed", {
        "run_id": run_id, "skill_id": skill_id,
        "skill_version_observed": version_observed, "skill_version_proposed_against": version_against,
        "pr_number": pr_number, "branch": branch, "cited_finding_ids": cited,
        "added_lines": added, "removed_lines": removed,
        "justification": justification,
    })
    if not proposed_ok:
        # The proposed event IS the idempotency record. If the canonical log
        # append failed after the PR opened, the next tick can't see this run as
        # handled and could open a second PR - surface it loudly for the operator
        # rather than swallowing it (the PR is already up; it can't be un-opened).
        print(f"CRITICAL {run_id}: PR#{pr_number} opened but skill_diff_proposed "
              f"failed to log canonically - reconcile before the next tick to avoid a duplicate PR",
              file=sys.stderr)
    _emit_tick(name, level, "proposed")
    print(f"proposed {run_id} PR#{pr_number} branch={branch} +{added}/-{removed} cited={cited}")


# --------------------------------------------------------------------------- #
# reconcile (AC10-FR backstop)
# --------------------------------------------------------------------------- #

@skill_diff_app.command("reconcile")
def reconcile(
    pr_number: Optional[int] = typer.Option(
        None, "--pr-number", "--pr",
        help="Scope to one merged proposer PR (the fast merge-trigger path); "
             "omit for a full sweep of every un-closed proposer PR.",
    ),
) -> None:
    """Close the skill-diff loop: re-eval merged proposer PRs, emit the receipt (AC10-FR).

    Detect-AND-run. For each merged proposer PR with no ``skill_diff_eval_closed``
    receipt, replay the exact corpus items the diff targeted at the merge commit,
    fold a before/after ``score_delta`` on the top failure dimension, and emit
    ``skill_diff_eval_closed``. Two callers key idempotency on the receipt's
    presence, so firing both (merge-trigger + periodic backstop) never
    double-emits: ``--pr-number`` scopes to the just-merged PR; the bare form
    sweeps. One status line per PR, never a silent exit-0.
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
    if pr_number is not None:
        if pr_number not in proposed:
            print(f"reconcile: PR#{pr_number} is not a known proposer PR (no skill_diff_proposed event)")
            return
        if pr_number in closed:
            print(f"reconcile: PR#{pr_number} already has an eval-closed receipt (no-op)")
            return
        open_gaps = [pr_number]
    else:
        open_gaps = [pr for pr in proposed if pr is not None and pr not in closed]
        if not open_gaps:
            print("reconcile: no un-closed proposer PRs")
            return

    for pr in open_gaps:
        print(f"reconcile: {_reeval_pr(pr, proposed[pr], events)}")


# Dimensions a /blueprint replay can re-score structurally (Review Amendment A1:
# replay sets include_shipped_outcome=False, so shipped_outcome is never emitted).
# If the top failure dimension is not one of these, there is no structural
# before/after to compute - the deferred outcome horizon owns it (Locked
# Decision 1), so reconcile closes it as outcome-pending rather than replaying.
_REPLAYABLE_DIMS = {"structural_validity", "collision_free"}


def _emit_receipt(pr: int, skill_id: str, run_id_before: str,
                  run_id_after: Optional[str], score_delta: Optional[int]) -> bool:
    """Emit the close receipt; return the canonical-log success flag so the caller
    never reports a close it did not durably record."""
    return _emit("skill_diff_eval_closed", {
        "pr_number": pr, "skill_id": skill_id, "run_id_before": run_id_before,
        "run_id_after": run_id_after, "score_delta": score_delta,
    })


def _already_closed(pr: int) -> bool:
    """A skill_diff_eval_closed for this PR already landed. A cheap re-read right
    before emitting narrows the merge-trigger vs periodic-sweep race (both can read
    'unclosed', replay, and append). Locked Decision 3 chose idempotency-on-presence
    over a lock, so this shrinks - does not eliminate - a simultaneous double-emit;
    it is the plan-faithful guard, not a claim."""
    for e in engine.read_events_tolerant(_events_paths()[0]):
        if e.get("type") == "skill_diff_eval_closed" and (e.get("data") or {}).get("pr_number") == pr:
            return True
    return False


def _replay_set(events: list[dict], run_id_before: str, skill_id: str, top_dim: Optional[str]) -> list[str]:
    """The corpus items the diff was supposed to fix: the before run's failing
    findings on the top dimension only (Locked Decision 4). Order-preserving
    dedup - the smallest honest measurement and the cost bound."""
    if not top_dim:
        return []
    seen: set[str] = set()
    out: list[str] = []
    for f in engine.findings_for_run(events, run_id_before, skill_id):  # tool_fault already excluded
        if f.get("dimension") == top_dim and f.get("verdict") == "fail":
            cid = f.get("corpus_item_id")
            if cid and cid not in seen:
                seen.add(cid)
                out.append(cid)
    return out


def _reeval_pr(pr: int, proposed: dict, events: list[dict]) -> str:
    """Re-eval one merged-unclosed proposer PR; return its one-line report status.

    Emits ``skill_diff_eval_closed`` on success. Emits NOTHING (leaving the PR
    detectable as unclosed for the next tick) on: not-yet-merged, gh-offline,
    paused, a replay that produced no comparable top-dim verdict (AC3-ERR), or a
    canonical-log emit failure - all degrade-not-crash. A non-replayable top
    dimension (e.g. shipped_outcome) closes as outcome-pending (null delta, no
    replay). The after-run's ``skill_eval_run_complete`` (with ``skill_ref``) is
    emitted by the observer replays themselves - reconcile only folds the delta on
    the top dimension both sides.
    """
    skill_id = proposed.get("skill_id") or "unknown"
    run_id_before = proposed.get("run_id")

    # /review proposals have no historical thread state to score finding_precision
    # against (Review Amendment A2): log-and-skip, no replay, no receipt.
    if skill_id == "fno:review":
        return f"PR#{pr} ({skill_id}) outcome-pending (review-skill)"

    merged = _pr_merged(pr)
    if merged is False:
        return f"PR#{pr} ({skill_id}, run {run_id_before}) not-yet-merged, no eval-closed"
    if merged is None:
        # gh unreachable -> merge state unconfirmable. Never re-eval a PR we can't
        # confirm merged: it could still be open, and replaying it against
        # origin/main would score a diff that never landed. Leave it unclosed for
        # a later tick when gh is back (AC3-ERR). Distinct from `is False` so an
        # offline blip is not misreported as a genuinely open PR.
        return f"PR#{pr} ({skill_id}) merge status unknown (gh offline), left unclosed for retry"

    # Pause gate BEFORE any replay spend (AC8-FR); replay honors it too, but we
    # never even spawn while paused.
    if loops_paused():
        return f"PR#{pr} ({skill_id}) paused, left unclosed for a later tick"

    if not run_id_before:
        return f"PR#{pr} ({skill_id}) malformed proposed event (no run_id), skipped"

    top = engine.top_dimension(events, run_id_before, skill_id)
    replay_set = _replay_set(events, run_id_before, skill_id, top)

    # AC5-EDGE: nothing failing on the top dimension -> replay nothing; close with
    # a null after-run and a zero delta.
    if not replay_set:
        if _already_closed(pr):
            return f"PR#{pr} ({skill_id}) already closed by a concurrent reconcile (no-op)"
        if not _emit_receipt(pr, skill_id, run_id_before, None, 0):
            return f"PR#{pr} ({skill_id}) receipt emit failed (canonical log), left unclosed for retry"
        return f"PR#{pr} ({skill_id}) re-evaluated: no failing items on {top or 'top'} dim, delta=0"

    # The top failure dimension is not one replay can structurally re-score (e.g.
    # shipped_outcome). There is no structural before/after here - the deferred
    # outcome horizon owns it (Locked Decision 1). Close as outcome-pending (null
    # delta, no replay) so it neither spins the replay fleet forever nor reports a
    # fabricated structural win on a dimension that was never re-measured.
    if top not in _REPLAYABLE_DIMS:
        if _already_closed(pr):
            return f"PR#{pr} ({skill_id}) already closed by a concurrent reconcile (no-op)"
        if not _emit_receipt(pr, skill_id, run_id_before, None, None):
            return f"PR#{pr} ({skill_id}) receipt emit failed (canonical log), left unclosed for retry"
        return f"PR#{pr} ({skill_id}) outcome-pending: top dim {top} not structurally replayable (delta=null)"

    merge_sha = _merge_sha(pr) or "origin/main"  # record which ref actually ran
    run_id_after = _mint_run_id(skill_id)
    for corpus_item in replay_set:
        _run_replay(corpus_item, merge_sha, run_id_after)

    # Re-read and scope the fold to the TOP dimension both sides: a replay emits a
    # finding on EVERY structural dimension, but the delta is only honest on the
    # dimension the diff targeted (Locked Decision 7).
    events_after = engine.read_events_tolerant(_events_paths()[0])
    top_after = [
        f for f in engine.findings_for_run(events_after, run_id_after, skill_id)  # tool_fault excluded
        if f.get("dimension") == top
    ]

    # AC3-ERR: the replay produced no comparable top-dimension verdict - every
    # spawn tool-faulted, or the top dim came back unscorable. Emit no receipt; the
    # PR stays unclosed so the next tick retries (never a fabricated delta).
    if not top_after:
        return f"PR#{pr} ({skill_id}) replay produced no {top} verdict, left unclosed for retry"

    # before_fail = the targeted slice size (every replay-set item failed the top
    # dim before, by construction); after_fail = how many still fail. Same corpus
    # IDs both sides, so delta = the items the merged diff actually fixed.
    before_fail = len(replay_set)
    after_fail = sum(1 for f in top_after if f.get("verdict") == "fail")
    score_delta = before_fail - after_fail
    if _already_closed(pr):
        return f"PR#{pr} ({skill_id}) already closed by a concurrent reconcile (no-op)"
    if not _emit_receipt(pr, skill_id, run_id_before, run_id_after, score_delta):
        return f"PR#{pr} ({skill_id}) receipt emit failed (canonical log), left unclosed for retry"
    return (f"PR#{pr} ({skill_id}) re-evaluated: delta={score_delta} "
            f"(before={before_fail} after={after_fail}, ref={merge_sha[:12]})")


def _mint_run_id(skill_id: str) -> str:
    """A fresh after-run id sharing the observer's ``obs-<skill_id>-<UTC>`` shape
    but no lineage with run_id_before."""
    from fno.observer.cli import _mint_run_id as _observer_mint

    return _observer_mint(skill_id)


def _run_replay(corpus_item: str, skill_ref: str, run_id_after: str) -> int:
    """Spawn one observer replay of a corpus item at the candidate ref, under the
    shared after-run id. The heavy machinery (isolated worktree, claim, detective
    isolation scan, per-item scoring, its own skill_ref-tagged run_complete) is
    the observer's; reconcile only loops it. Injectable seam for tests; returns
    the subprocess rc (non-zero == that item did not score)."""
    try:
        p = subprocess.run(
            ["fno", "observer", "replay", "--skill", "blueprint",
             "--corpus-item", corpus_item, "--skill-ref", skill_ref, "--run-id", run_id_after],
            cwd=paths.resolve_repo_root(), capture_output=True, text=True, timeout=900,
        )
        if p.returncode != 0:
            _LOG.warning("skill-diff: replay of %s failed (rc=%d): %s",
                         corpus_item, p.returncode, (p.stderr or "").strip()[:300])
        return p.returncode
    except (OSError, subprocess.TimeoutExpired) as exc:
        _LOG.warning("skill-diff: replay of %s failed: %s", corpus_item, exc)
        return 1


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


def _merge_sha(pr_number: int) -> Optional[str]:
    """Best-effort merge commit SHA to pin the candidate ref; None when gh is
    unavailable (caller falls back to origin/main and records that)."""
    try:
        out = subprocess.run(
            ["gh", "pr", "view", str(pr_number), "--json", "mergeCommit", "-q", ".mergeCommit.oid"],
            cwd=paths.resolve_repo_root(), capture_output=True, text=True, check=True,
        )
        # A null mergeCommit (not-yet-populated squash race) prints the literal
        # "null" via jq -q; treat it as unrecoverable so the caller falls back to
        # origin/main rather than `git worktree add null`.
        sha = out.stdout.strip()
        return sha if sha and sha != "null" else None
    except (OSError, subprocess.CalledProcessError):
        return None
