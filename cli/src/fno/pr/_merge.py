"""In-package port of ``scripts/lib/pr-merge.sh`` (ab-d4c98550, US1).

Skill-agnostic PR merge wrapper. Shells to ``gh``/``git`` and preserves the
caller-facing contract verbatim:

- Stdout: one JSON line ``{pr, outcome, reason, strategy, invoker}`` on
  merged/queued/skipped; failures print the same JSON shape to STDERR.
- Exit codes: 0 merged|queued, 1 failed (incl. bad args), 2 skipped
  (auto_merge disabled / invoker not allowed), 127 gh not installed.
- The footnote-canonical merge guards (config.auto_merge gating + invoker
  allowlist) and the worktree server-side-recovery fallback are preserved.
- Post-merge followups (memory-pass + triage sentinels, the session_satisfied
  event, the per-PR artifact consolidation) fire for merged AND queued,
  best-effort: a followup failure never changes the already-emitted outcome.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import sys
from typing import List, Optional, Sequence

from fno.pr._proc import ToolMissing, run

_VALID_INVOKERS = {"target", "megawalk"}
_PR_RE = re.compile(r"^[1-9][0-9]*$")


def _emit(pr: int, outcome: str, reason: str, strategy: str, invoker: str, *, err: bool) -> None:
    """Print the JSON line. ``err`` routes to stderr (failure cases)."""
    obj = {
        "pr": pr,
        "outcome": outcome,
        "reason": reason,
        "strategy": strategy,
        "invoker": invoker,
    }
    line = json.dumps(obj, separators=(",", ":")) + "\n"
    (sys.stderr if err else sys.stdout).write(line)
    (sys.stderr if err else sys.stdout).flush()


def _gh(args: Sequence[str], cwd: str):
    return run(["gh", *args], cwd=cwd)


def _git(args: Sequence[str], cwd: str):
    return run(["git", *args], cwd=cwd)


def _load_auto_merge():
    from fno.config import load_settings

    return load_settings().config.auto_merge


def _repo_state_dir(cwd: str) -> str:
    res = _git(["rev-parse", "--show-toplevel"], cwd)
    root = res.stdout.strip() if res.ok and res.stdout.strip() else cwd
    return os.path.join(root, ".fno")


def _read_state_field(state_file: str, field: str) -> str:
    """Read ``field:`` from frontmatter, dequoting a matched pair (parser parity).

    Strips only a MATCHED surrounding quote pair (not a naive unbalanced
    strip that could mangle a value starting/ending with a quote; gemini on
    PR #524).
    """
    try:
        with open(state_file, "r", encoding="utf-8") as fh:
            for ln in fh:
                if ln.startswith(field + ":"):
                    val = ln[len(field) + 1:].strip()
                    if len(val) >= 2 and (
                        (val[0] == '"' and val[-1] == '"')
                        or (val[0] == "'" and val[-1] == "'")
                    ):
                        return val[1:-1]
                    return val
    except OSError:
        pass
    return ""


# ---------------------------------------------------------------------------
# Post-merge side-effects (best-effort; never change the merge outcome)
# ---------------------------------------------------------------------------


def _sync_graph_merge_status(merge_status: str, pr_number: int) -> None:
    """Set merge_status on the graph node carrying this pr_number (best-effort)."""
    try:
        from fno.paths import graph_json
        from fno.graph.store import locked_mutate_graph

        path = graph_json()
        if not path.exists():
            return

        def _mut(entries: List[dict]) -> List[dict]:
            for e in entries:
                if e.get("pr_number") == pr_number:
                    e["merge_status"] = merge_status
                    break
            return entries

        locked_mutate_graph(path, _mut)
    except (Exception, SystemExit):
        # Silent no-op on ANY failure (no graph, store error): the bash
        # `|| true`-guarded this, and it must never block the merge outcome.
        # SystemExit is included because locked_mutate_graph calls sys.exit(1)
        # on a corrupt graph.json - a bare `except Exception` would let that
        # abort the merge outcome before the post-merge followups run (codex
        # P2 on PR #524). KeyboardInterrupt is deliberately NOT swallowed.
        pass


def _emit_session_satisfied(pr_url: str, state_dir: str) -> None:
    """Emit a session_satisfied{source:pr_merge} event (best-effort)."""
    state_file = os.path.join(state_dir, "target-state.md")
    if not os.path.isfile(state_file):
        return
    sid = _read_state_field(state_file, "session_id")
    if not sid or sid == "null":
        return
    try:
        with open(state_file, "rb") as fh:
            gate_hash = hashlib.md5(fh.read()).hexdigest()
    except OSError:
        return
    if not gate_hash:
        return
    try:
        from pathlib import Path

        from fno.events import append_event, session_satisfied

        event = session_satisfied(
            trigger="pr_merge",
            reason="pr_merged",
            session_id=sid,
            gate_state_hash=gate_hash,
            evidence_url=pr_url or None,
            source="target",
        )
        append_event(event, events_path=Path(state_dir) / "events.jsonl")
    except Exception as exc:  # noqa: BLE001 - best-effort, surface a diagnostic
        sys.stderr.write(
            f"pr-merge: session_satisfied emit failed ({exc}); merge outcome unaffected\n"
        )


def _run_post_merge_followups(pr_number: int, strategy: str, cwd: str) -> None:
    state_dir = _repo_state_dir(cwd)
    state_file = os.path.join(state_dir, "target-state.md")

    # Memory-pass sentinel.
    try:
        with open(os.path.join(state_dir, ".memory-pass-pending"), "w", encoding="utf-8") as fh:
            fh.write(f"{pr_number}\n")
    except OSError:
        pass

    # Retro-triage fast-path sentinel.
    try:
        mode = "interactive"
        if os.path.isfile(os.path.join(state_dir, "megawalk-state.md")) or os.environ.get(
            "TARGET_MISSION_ID"
        ):
            mode = "autonomous"
        plan_path = _read_state_field(state_file, "plan_path")
        session_id = _read_state_field(state_file, "session_id")
        pr_url = ""
        res = _gh(["pr", "view", str(pr_number), "--json", "url", "-q", ".url"], cwd)
        if res.ok:
            pr_url = res.stdout.strip()
        sentinel = {
            "pr_number": pr_number,
            "pr_url": pr_url,
            "mode": mode,
            "plan_path": plan_path,
            "session_id": session_id,
        }
        with open(os.path.join(state_dir, ".triage-pending"), "w", encoding="utf-8") as fh:
            fh.write(json.dumps(sentinel, separators=(",", ":")))
    except Exception:
        pass

    # Auto-complete signal.
    try:
        pr_url = ""
        res = _gh(["pr", "view", str(pr_number), "--json", "url", "-q", ".url"], cwd)
        if res.ok:
            pr_url = res.stdout.strip()
        _emit_session_satisfied(pr_url, state_dir)
    except Exception:
        pass

    # Per-PR artifact consolidation (best-effort; degrades cleanly when the
    # script is absent, e.g. a bare pip install). The consolidator lives in the
    # PLUGIN tree (`<plugin>/scripts/lib/consolidate-artifacts.sh`), not the
    # target repo, so resolve the plugin root first - else `fno pr merge` run in
    # a footnote-managed target repo would silently skip it every time (codex P2
    # on PR #524). CLAUDE_PLUGIN_ROOT / FNO_REPO_ROOT are read directly (a
    # PRIVATE resolution, not the shared resolve_repo_root/resolve_plugin_script
    # names), so this stays out of scope for the shellout-drift guard (flock-
    # pattern carveout cv-ca99e324 posture); os.path.dirname(state_dir) is the
    # last-resort fallback (correct in the dogfooded footnote repo itself).
    try:
        plugin_root = (
            os.environ.get("CLAUDE_PLUGIN_ROOT")
            or os.environ.get("FNO_REPO_ROOT")
            or os.path.dirname(state_dir)
        )
        script = os.path.join(plugin_root, "scripts", "lib", "consolidate-artifacts.sh")
        if os.path.isfile(script):
            env = dict(os.environ, PR_NUMBER=str(pr_number))
            run(["bash", script], cwd=cwd, env=env)
    except Exception:
        sys.stderr.write(
            "pr-merge: artifact consolidation failed; merge outcome unaffected\n"
        )


# ---------------------------------------------------------------------------
# Main flow
# ---------------------------------------------------------------------------


def run_merge(argv: Sequence[str], cwd: Optional[str] = None) -> int:
    repo = cwd or os.getcwd()
    invoker = ""
    pr_raw = ""
    for arg in argv:
        if arg.startswith("--invoker="):
            invoker = arg[len("--invoker="):]
        elif arg[:1].isdigit():
            pr_raw = arg
        else:
            sys.stderr.write(f"Error: unknown arg '{arg}'\n")
            return 1

    if not invoker:
        sys.stderr.write("--invoker=<target|megawalk> required\n")
        return 1
    if not pr_raw:
        sys.stderr.write("pr_number required\n")
        return 1

    # Invoker whitelist (injection guard).
    if invoker not in _VALID_INVOKERS:
        sys.stderr.write(
            f"Error: invalid --invoker '{invoker}'. Must be target|megawalk.\n"
        )
        return 1

    # pr_number must be a positive integer.
    if not _PR_RE.match(pr_raw):
        _emit(0, "failed", f"invalid pr number: {pr_raw}", "none", invoker, err=True)
        return 1
    pr_number = int(pr_raw)

    # (0) Stub-manifest hold: a `contract`-tier dependent's PR must not merge
    # while it carries an unreconciled stub-manifest (mocks would ship). Checked
    # BEFORE the auto_merge gate so auto-merge cannot bypass it (AC7-EDGE), and
    # it no-ops for every non-contract PR so the default `hard` path is unchanged
    # (AC6-EDGE).
    try:
        from fno.stub_manifest import unreconciled_manifest_for_pr

        # Resolve the repo top-level: manifests are written under the PROJECT
        # root's `.fno/`, so a merge invoked from a subdirectory must not look
        # under that subdir (codex P2). Falls back to `repo` if git can't say.
        top = _git(["rev-parse", "--show-toplevel"], repo)
        root = top.stdout.strip() if top.ok and top.stdout.strip() else repo
        held = unreconciled_manifest_for_pr(pr_number, root)
    except Exception:
        held = None  # never let the guard's own failure block a normal merge
    if held:
        if held.get("_malformed"):
            detail = "malformed stub-manifest (cannot prove stubs are gone)"
        else:
            detail = f"unreconciled stub-manifest ({len(held.get('stubs', []))} stub(s))"
        _emit(
            pr_number,
            "held",
            f"contract dependent {held.get('_node')} carries a {detail}; "
            "reconcile before merge",
            "none",
            invoker,
            err=False,
        )
        return 2

    # (1) Short-circuit if disabled or invoker not allowed.
    auto_merge = _load_auto_merge()
    if not auto_merge.is_allowed_for(invoker):
        _emit(
            pr_number,
            "skipped",
            f"auto_merge disabled or invoker '{invoker}' not in allowed_invokers",
            "none",
            invoker,
            err=False,
        )
        return 2

    # (2) gh must be installed.
    if shutil.which("gh") is None:
        _emit(pr_number, "failed", "gh CLI not installed", "none", invoker, err=True)
        return 127

    # (3) Build command.
    strategy = auto_merge.merge_strategy
    cmd: List[str] = ["pr", "merge", str(pr_number), f"--{strategy}"]
    if auto_merge.delete_branch_on_merge:
        cmd.append("--delete-branch")
    if auto_merge.require_checks_pass:
        cmd.append("--auto")

    # (4) Run + classify.
    try:
        res = _gh(cmd, repo)
    except ToolMissing:
        _emit(pr_number, "failed", "gh CLI not installed", "none", invoker, err=True)
        return 127

    output = (res.stdout or "") + (res.stderr or "")
    if res.ok:
        if re.search(r"will be automatically merged", output, re.IGNORECASE):
            _emit(pr_number, "queued", "awaiting required checks", strategy, invoker, err=False)
            _sync_graph_merge_status("queued", pr_number)
        else:
            _emit(pr_number, "merged", "merged immediately", strategy, invoker, err=False)
            _sync_graph_merge_status("merged", pr_number)
        _run_post_merge_followups(pr_number, strategy, repo)
        return 0

    # Failure path. A worktree-local post-merge step can fail even though the
    # SERVER-SIDE merge already landed (recurring PR #393/#395 bite).
    first_line = output.splitlines()[0][:200] if output.strip() else ""
    if re.search(r"is already used by worktree|already checked out", output, re.IGNORECASE):
        # (a) Server-side merge already landed -> cosmetic local failure.
        merged_at = ""
        view = _gh(["pr", "view", str(pr_number), "--json", "mergedAt", "-q", ".mergedAt"], repo)
        if view.ok:
            merged_at = view.stdout.strip()
        if merged_at and merged_at != "null":
            _git(["fetch", "origin"], repo)
            _emit(
                pr_number,
                "merged",
                "merged server-side; local post-merge step skipped in worktree",
                strategy,
                invoker,
                err=False,
            )
            _sync_graph_merge_status("merged", pr_number)
            _run_post_merge_followups(pr_number, strategy, repo)
            return 0
        # (b) Not merged yet -> merge SERVER-SIDE via the API (no local checkout).
        api = _gh(
            [
                "api",
                "--method",
                "PUT",
                f"repos/{{owner}}/{{repo}}/pulls/{pr_number}/merge",
                "-f",
                f"merge_method={strategy}",
            ],
            repo,
        )
        if api.ok:
            _git(["fetch", "origin"], repo)
            _emit(
                pr_number,
                "merged",
                "merged server-side (worktree fallback)",
                strategy,
                invoker,
                err=False,
            )
            _sync_graph_merge_status("merged", pr_number)
            _run_post_merge_followups(pr_number, strategy, repo)
            return 0

    # Unrecovered failure: classify and report.
    reason = first_line
    if re.search(r"protected", output, re.IGNORECASE):
        reason = "branch protected"
    elif re.search(r"not mergeable", output, re.IGNORECASE):
        reason = "not mergeable (conflicts or base changed)"
    elif re.search(r"required review", output, re.IGNORECASE):
        reason = "required review pending"
    _emit(pr_number, "failed", reason, strategy, invoker, err=True)
    _sync_graph_merge_status("failed", pr_number)
    return 1
