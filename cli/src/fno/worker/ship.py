"""fno worker ship - idempotent PR creation.

- Calls `gh pr list --head <branch>` first to detect an existing PR.
- If found, updates artifact with existing PR number (no duplicate).
- If not found, calls `gh pr create`.
- When auto_merge_approved=true, also calls `gh pr merge --auto --merge`.
- Writes .fno/artifacts/ship-{session_id}.md.
- Emits fno event emit --type pr_created/pr_exists.
- Sets state field artifact_shipped=true (via fno state set).
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
            "auto_merge_armed": bool,
        }
    """
    state_path = Path(state_path)
    state = _read_state(state_path)
    session_id = state.get("session_id", "unknown-session")
    auto_merge_approved = state.get("auto_merge_approved", False)

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
        if base_code != 0:
            return {
                "action": "error",
                "error": base_msg or "stale base: refused to open PR from a stale base",
                "branch": branch,
            }
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

    # Step 3: arm auto-merge if approved
    auto_merge_armed = False
    auto_merge_error: Optional[str] = None
    if auto_merge_approved and pr_number:
        merge_result = subprocess.run(
            ["gh", "pr", "merge", str(pr_number), "--auto", "--merge"],
            capture_output=True,
            text=True,
        )
        auto_merge_armed = merge_result.returncode == 0
        if not auto_merge_armed:
            # Surface gh's stderr so the caller can distinguish "user opted out"
            # from "opt-in happened but failed to arm" (e.g. branch protection,
            # stale gh auth, unmergeable state).
            auto_merge_error = (merge_result.stderr or merge_result.stdout).strip()[:500]
            import sys
            print(
                f"worker.ship: auto-merge arm failed for PR {pr_number}: {auto_merge_error}",
                file=sys.stderr,
            )

    result = {
        "action": action,
        "pr_number": pr_number,
        "pr_url": pr_url,
        "auto_merge_armed": auto_merge_armed,
        "artifact_path": str(artifact_path),
        "session_id": session_id,
    }
    if auto_merge_approved and not auto_merge_armed and auto_merge_error:
        # Only include error when opt-in was attempted and failed, so callers
        # can distinguish "not requested" (auto_merge_armed=false, no error)
        # from "requested but failed" (auto_merge_armed=false, error present).
        result["auto_merge_error"] = auto_merge_error
    return result
