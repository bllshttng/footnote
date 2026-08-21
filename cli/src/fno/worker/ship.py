"""fno worker ship - idempotent PR creation.

- Calls `gh pr list --head <branch>` first to detect an existing PR.
- If found, updates artifact with existing PR number (no duplicate).
- If not found, calls `gh pr create`.
- Does NOT arm auto-merge: that moved to `fno-agents finalize` (x-1951).
- Writes .fno/artifacts/ship-{session_id}.md.
- Emits fno event emit --type pr_created/pr_exists.
- Sets state field artifact_shipped=true (via fno do state set).
"""
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path
from typing import Any, Optional

import yaml


def _read_state(state_path: Path) -> dict[str, Any]:
    """Read YAML frontmatter from a state file."""
    text = state_path.read_text(encoding="utf-8")
    if not text.startswith("---"):
        return {}
    rest = text[3:]
    if rest.startswith("\n"):
        rest = rest[1:]
    end = rest.find("\n---")
    if end == -1:
        return {}
    return yaml.safe_load(rest[:end]) or {}


def _read_graph_node_id(state_path: Path) -> Optional[str]:
    """The backlog node id, appended to the manifest BODY by
    init-target-state.sh (below the frontmatter _read_state parses). Returns
    None when absent or ``null`` so the caller skips the node<->PR stamp.
    """
    try:
        for line in state_path.read_text(encoding="utf-8").splitlines():
            m = re.match(r"^\s*graph_node_id:\s*(.*?)\s*$", line)
            if m:
                raw = m.group(1).strip().strip('"').strip("'")
                return raw if raw and raw != "null" else None
    except OSError:
        return None
    return None


def _extract_pr_number(url_or_output: str) -> Optional[int]:
    """Extract PR number from a GitHub URL or plain number string."""
    m = re.search(r"/pull/(\d+)", url_or_output)
    if m:
        return int(m.group(1))
    # Maybe just a number
    stripped = url_or_output.strip()
    if stripped.isdigit():
        return int(stripped)
    return None


def _get_current_branch() -> str:
    """Return the current git branch name. Raises on git failure.

    Returning a 'HEAD' sentinel string on failure (as the prior implementation
    did) caused `gh pr list --head HEAD` to match nothing and silently create
    a duplicate PR under whatever branch gh defaults to. Fail-loud instead.
    """
    result = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"git rev-parse failed (exit {result.returncode}): "
            f"{result.stderr.strip()[:200]}. Cannot ship without a real branch."
        )
    branch = result.stdout.strip()
    if not branch or branch == "HEAD":
        # Detached HEAD state or empty - not safe to assume gh will DWIM.
        raise RuntimeError(
            "current branch is detached or empty; cannot ship from detached HEAD."
        )
    return branch


def ship(
    *,
    state_path: Path,
    title: str,
    body: str,
    artifacts_dir: Optional[Path] = None,
    base_branch: str = "main",
) -> dict[str, Any]:
    """Create or detect a PR idempotently, then write the ship artifact.

    Args:
        state_path: Path to target-state.md.
        title: PR title.
        body: PR body.
        artifacts_dir: Where to write the ship artifact (default: .fno/artifacts).
        base_branch: Target branch for the PR.

    Returns:
        {
            "action": "pr_created" | "pr_exists",
            "pr_number": int,
            "pr_url": str,
        }
    """
    state_path = Path(state_path)
    state = _read_state(state_path)
    session_id = state.get("session_id", "unknown-session")

    # Incarnation fence (x-eea5 followup): a losing incarnation - one whose
    # session:<uuid> single-writer claim another incarnation holds - must not
    # create a PR. Same read-only, fail-closed semantics as fno do pr merge. Runs
    # before any gh/git call so a fenced incarnation publishes nothing.
    from fno.claims.incarnation import incarnation_fence_blocks, resolve_fence_session_uuid

    try:
        _fence_uuid = resolve_fence_session_uuid(state_path.parent.parent)
        _blocked, _fence_reason = incarnation_fence_blocks(_fence_uuid)
    except Exception as _exc:  # noqa: BLE001 - a fence-CODE crash fails OPEN (proceed + log); the helper already fail-closes on unreadable/corrupted state
        import sys

        print(f"worker.ship: incarnation-fence check crashed ({_exc}); proceeding", file=sys.stderr)
        _blocked, _fence_reason = False, ""
    if _blocked:
        return {"action": "blocked", "error": _fence_reason}

    if artifacts_dir is None:
        artifacts_dir = state_path.parent / "artifacts"
    artifacts_dir = Path(artifacts_dir)
    artifacts_dir.mkdir(parents=True, exist_ok=True)

    # Step 1: check for existing PR
    branch = _get_current_branch()
    list_result = subprocess.run(
        ["gh", "pr", "list", "--head", branch, "--json", "number,url,state"],
        capture_output=True,
        text=True,
    )

    existing_prs = []
    if list_result.returncode == 0 and list_result.stdout.strip():
        try:
            existing_prs = json.loads(list_result.stdout)
        except json.JSONDecodeError as exc:
            # A malformed `gh pr list` response leaves us in UNKNOWN state:
            # we cannot tell whether a PR already exists. Treating this as
            # "no PR exists" (prior behavior) would violate idempotency by
            # creating a duplicate. Fail loudly so the caller can retry.
            return {
                "action": "error",
                "error": (
                    f"malformed JSON from `gh pr list --head {branch}`: "
                    f"{exc.msg}. Cannot determine PR state safely; refusing "
                    f"to create PR to avoid duplicate. Stdout: "
                    f"{list_result.stdout[:200]}"
                ),
                "branch": branch,
            }

    from fno.pr._preflight import check_verification_evidence, local_verification_required

    evidence_required, _policy_reason = local_verification_required(
        cwd=str(Path.cwd()), base_ref=f"origin/{base_branch}"
    )
    if evidence_required:
        evidence = check_verification_evidence(allow_equivalent=True)
        if not evidence["satisfied"]:
            return {
                "action": "error",
                "error": (
                    "verification evidence refused ship: no full/passed "
                    "verification receipt for HEAD, and no earlier receipt "
                    "whose patches match it. Run scripts/ci/preflight.sh "
                    "(required by config.preflight.required = true). "
                    f"(mode={evidence['mode']} result={evidence['result']})"
                ),
                "branch": branch,
            }

    if existing_prs:
        # Use existing PR - idempotent
        pr = existing_prs[0]
        pr_number = pr.get("number")
        pr_url = pr.get("url", f"https://github.com/pull/{pr_number}")
        action = "pr_exists"
    else:
        # Stale-base guard: a branch cut from a stale local HEAD ships a PR full
        # of phantom deletions. Refuse before gh pr create (the same check the
        # /pr create router runs; bypass FNO_PR_BASE_OK=stale-acknowledged).
        from fno.pr._preflight import check_stale_base

        base_code, base_msg = check_stale_base(base=f"origin/{base_branch}")
        if base_msg and base_code == 0:
            # Fail-open path: the guard was SKIPPED (fetch flake / git missing).
            # Say so - a silently-skipped guard must not read as a clean pass.
            import sys

            print(f"worker.ship: {base_msg}", file=sys.stderr)
        if base_code != 0:
            return {
                "action": "error",
                "error": base_msg or "stale base: refused to open PR from a stale base",
                "branch": branch,
            }
        # Claim this session's node in the exact trailer BEFORE gh sees the
        # body. Unconditional: it is a no-op with nothing to claim and
        # idempotent on a body that already claims them.
        #
        # The MANIFEST id leads, and the branch is only a fallback. The branch
        # name is a guess that the graph then has to confirm, while
        # `_read_graph_node_id` is what this session actually claimed - and it
        # was already being read 50 lines below, for the PR that this same call
        # was labelling from the branch. On a reused or handed-off worktree the
        # branch still carries a previous node, so the trailer closed something
        # this PR does not ship. Passing it as `extra_ids` also keeps
        # `contained_in` descendants reachable, which a branch-derived id drops.
        from fno.pr.closure import ensure_closure_trailer

        manifest_node = _read_graph_node_id(state_path)
        body = ensure_closure_trailer(
            body, branch, extra_ids=[manifest_node] if manifest_node else None
        )
        # Create new PR
        create_result = subprocess.run(
            [
                "gh", "pr", "create",
                "--title", title,
                "--body", body,
                "--base", base_branch,
            ],
            capture_output=True,
            text=True,
        )
        if create_result.returncode != 0:
            return {
                "action": "error",
                "error": create_result.stderr.strip(),
                "exit_code": create_result.returncode,
            }
        pr_url = create_result.stdout.strip()
        pr_number = _extract_pr_number(pr_url)
        action = "pr_created"

    # Step 2: write ship artifact
    artifact_path = artifacts_dir / f"ship-{session_id}.md"
    artifact_content = (
        f"---\n"
        f"session_id: {session_id}\n"
        f"phase: ship\n"
        f"pr_number: {pr_number}\n"
        f"pr_url: {pr_url}\n"
        f"---\n"
        f"# Ship Artifact\n\n"
        f"PR_NUMBER: {pr_number}\n"
        f"PR_URL: {pr_url}\n"
        f"ACTION: {action}\n"
    )
    # Atomic write: the ship artifact is factor-2 of the two-factor gate check.
    # A partial write from a crash or concurrent access would be indistinguishable
    # from a forged artifact. atomic_write uses filelock + tempfile + os.replace.
    from fno.state.io import atomic_write
    atomic_write(artifact_path, artifact_content)

    # Step 2.5: stamp the backlog node <-> PR link (x-a166). Without this the
    # node's pr_number stays null through the whole PR review window, so the
    # _has_unmerged_open_pr selection guard and `fno backlog reconcile` cannot
    # see the in-flight/merged PR - leaving only the 2h PID claim to guard the
    # node, which lapses and lets the dispatcher re-spawn a finished node.
    # Best-effort + idempotent (re-stamping the same PR is a no-op); a stamp
    # failure logs but never fails the ship.
    node_id = _read_graph_node_id(state_path)
    if node_id and pr_number:
        stamp = subprocess.run(
            ["fno", "backlog", "update", node_id,
             "--pr-number", str(pr_number), "--pr-url", pr_url],
            capture_output=True,
            text=True,
        )
        if stamp.returncode != 0:
            import sys
            print(
                f"worker.ship: node<->PR stamp failed for {node_id} "
                f"PR {pr_number}: {(stamp.stderr or stamp.stdout).strip()[:200]}",
                file=sys.stderr,
            )
        # Step 2.6 used to stamp ship provenance here with an explicit
        # `session add --phase ship`. The `update --pr-number` call above now
        # owns that stamp (it fires on the pr_number unset->set transition with
        # the same ambient identity), so a second explicit stamp would only
        # double-fire.

    # Arming auto-merge here would pre-authorize a merge before any gate ran, so
    # it moved to `fno-agents finalize` (see the module docstring). The
    # `auto_merge_armed` / `auto_merge_error` keys went with it rather than
    # staying permanently false, which would read as "not armed" instead of "not
    # armed here"; finalize's `session_finalized` event carries the fact now.
    return {
        "action": action,
        "pr_number": pr_number,
        "pr_url": pr_url,
        "artifact_path": str(artifact_path),
        "session_id": session_id,
    }
