"""fno pr sync-canonical - post-merge canonical-checkout sync (x-47be).

Pure-mechanical, fail-open. After a PR merges, bring the CANONICAL checkout and
its installed tooling up to the merged HEAD by running the project's configured
``config.post_merge.sync_command`` (footnote example:
``git checkout main && git pull origin main && fno update && fno restart``).

Load-bearing constraints, each with its own guard below:

- **Location.** The sync ALWAYS targets the canonical checkout, even when
  invoked from a worktree cwd - a worktree cannot ``git checkout main`` without
  hijacking its own branch. The resolved root is guarded by an origin-slug
  match against the PR's repo before any command runs.
- **Files from GitHub, not local git.** At gate time the canonical has NOT
  pulled the merge (the pull is *inside* ``sync_command``), so the merged file
  list and SHA come from ``gh pr view``, never a local ``git diff``.
- **Exactly-once per merge SHA.** A ``.fno/post-merge-synced/<sha>`` marker
  (written only on success) is the cross-session record; a single-flight claim
  serializes concurrent runs so two ``fno restart``s never overlap.

Every outcome prints exactly one status line (AC1-UI); no outcome is silent.
Failure is non-fatal to callers (reconcile/ritual): a non-zero exit withholds
the marker so the next reconcile retries.
"""
from __future__ import annotations

import fnmatch
import re
from pathlib import Path
from typing import Any, Callable, Optional

import typer

from fno.pr._proc import Result, ToolMissing, run as _run

# Single-flight claim TTL: long enough to cover a slow sync (build + restart),
# short enough that a crashed holder's lock recovers within one coffee break.
_SYNC_CLAIM_TTL_MS = 30 * 60 * 1000

_REMOTE_SLUG_RE = re.compile(
    r"(?:github\.com[:/])([^/]+)/(.+?)(?:\.git)?/?$"
)


def _origin_slug(canonical: Path, runner: Callable[..., Result]) -> Optional[str]:
    """``owner/repo`` from the canonical checkout's origin remote, or None.

    Handles both ``git@github.com:owner/repo.git`` and
    ``https://github.com/owner/repo(.git)`` forms.
    """
    try:
        res = runner(["git", "remote", "get-url", "origin"], cwd=str(canonical))
    except ToolMissing:
        return None
    if not res.ok:
        return None
    m = _REMOTE_SLUG_RE.search(res.stdout.strip())
    return f"{m.group(1)}/{m.group(2)}" if m else None


def _synced_marker(canonical: Path, sha: str) -> Path:
    return canonical / ".fno" / "post-merge-synced" / sha


def run_sync_canonical(
    pr_number: int,
    *,
    settings: Any = None,
    canonical_root: Optional[Path] = None,
    runner: Callable[..., Result] = _run,
    gh_json: Optional[Callable[[list[str], Optional[str]], dict[str, Any]]] = None,
    shell_runner: Optional[Callable[[str, str], int]] = None,
) -> int:
    """Run the canonical-sync for a merged PR. Returns a process exit code.

    Seams (``settings`` / ``canonical_root`` / ``runner`` / ``gh_json`` /
    ``shell_runner``) let unit tests exercise every branch without shelling out.
    """
    from fno.config import load_settings

    if settings is None:
        settings = load_settings()
    pm = settings.post_merge

    # 1. Unset command -> clean no-op (opt-in).
    if not (pm.sync_command or "").strip():
        typer.echo("post-merge sync: not configured")
        return 0

    # 2. Resolve the canonical checkout (targets canonical even from a worktree).
    if canonical_root is None:
        from fno.paths import resolve_canonical_repo_root

        canonical_root = resolve_canonical_repo_root()
    canonical = Path(canonical_root)

    origin = _origin_slug(canonical, runner)
    if origin is None:
        typer.echo(
            f"post-merge sync: canonical {canonical} has no resolvable origin; skipping",
            err=True,
        )
        return 0

    # 3. Read merge SHA + files from GitHub (the canonical has not pulled yet).
    if gh_json is None:
        gh_json = _default_gh_json
    try:
        row = gh_json(
            ["pr", "view", str(pr_number), "--repo", origin,
             "--json", "state,mergeCommit,files,url"],
            str(canonical),
        )
    except ToolMissing:
        typer.echo("post-merge sync: gh not found on PATH; skipping", err=True)
        return 0
    except _GhError as exc:
        typer.echo(f"post-merge sync: gh pr view #{pr_number} failed: {exc}", err=True)
        return 1  # no marker; next reconcile retries

    state = row.get("state")
    if state != "MERGED":
        typer.echo(f"post-merge sync: PR #{pr_number} not merged (state={state}); skipping")
        return 0

    # Wrong-repo guard: the PR's own url must sit in the resolved canonical's
    # repo before we let sync_command run `git checkout main` there.
    pr_slug = _slug_from_pr_url(row.get("url"))
    if pr_slug and pr_slug != origin:
        typer.echo(
            f"post-merge sync: PR repo {pr_slug} != canonical origin {origin}; "
            f"refusing to sync the wrong repo",
            err=True,
        )
        return 0

    sha = (row.get("mergeCommit") or {}).get("oid")
    if not sha:
        typer.echo(f"post-merge sync: PR #{pr_number} has no merge commit yet; skipping")
        return 0

    # 4. Dedup by merge SHA (cross-session).
    marker = _synced_marker(canonical, sha)
    if marker.exists():
        typer.echo(f"post-merge sync: already synced {sha[:12]}")
        return 0

    # 5. Single-flight lock (canonical-scoped, TTL-live).
    from fno import claims

    lock_key = f"post-merge-sync:{sha}"
    holder = f"sync-canonical:{pr_number}"
    try:
        claims.acquire_claim(
            lock_key, holder, ttl_ms=_SYNC_CLAIM_TTL_MS, reason="post-merge canonical sync"
        )
    except claims.ClaimHeldByOther:
        typer.echo(f"post-merge sync: in progress elsewhere for {sha[:12]}; skipping")
        return 0

    try:
        # Re-check under the lock (double-checked): a loser that read the marker
        # as absent before the winner wrote it must not re-run sync_command
        # after the winner releases.
        if marker.exists():
            typer.echo(f"post-merge sync: already synced {sha[:12]}")
            return 0

        # 6. Path-gate (globs computed from the GitHub file list, not local git).
        files = [f.get("path", "") for f in (row.get("files") or []) if isinstance(f, dict)]
        globs = list(pm.sync_paths or [])
        if globs and not _any_match(files, globs):
            _write_marker(marker)
            typer.echo(
                f"post-merge sync: skipped - no buildable change "
                f"({len(files)} files, none matched {globs}); marked {sha[:12]}"
            )
            return 0

        # 7. Run sync_command in the canonical via a login shell so uv/cargo/npm
        #    on the shell-rc PATH resolve (a bare `bash -c` would miss them).
        cmd = pm.sync_command
        typer.echo(f"post-merge sync: running in {canonical} for {sha[:12]}")
        if shell_runner is None:
            shell_runner = _default_shell_runner
        rc = shell_runner(cmd, str(canonical))
        if rc == 0:
            _write_marker(marker)
            typer.echo(f"post-merge sync: synced {sha[:12]}")
            return 0
        typer.echo(f"post-merge sync: failed (exit {rc}); marker withheld, will retry", err=True)
        return rc
    finally:
        try:
            claims.release_claim(lock_key, holder)
        except Exception:
            pass  # lock is TTL-bounded; a failed release recovers on its own


def _any_match(files: list[str], globs: list[str]) -> bool:
    return any(fnmatch.fnmatch(f, g) for f in files for g in globs)


def _write_marker(marker: Path) -> None:
    # Best-effort: the sync already ran, so a marker failure only makes the next
    # sweep re-run (safe direction). mkdir is inside the guard too, keeping the
    # whole write fail-open rather than letting an ENOENT/EACCES escape. A
    # failure is signalled so the operator can see WHY the sync re-runs each
    # sweep (a persistently-unwritable .fno) rather than it looking like normal.
    try:
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.touch(exist_ok=True)
    except OSError as exc:
        typer.echo(
            f"post-merge sync: marker write failed ({exc}); will re-run next sweep",
            err=True,
        )


_PR_URL_SLUG_RE = re.compile(r"github\.com/([^/]+)/([^/]+)/pull/\d+")


def _slug_from_pr_url(url: Optional[str]) -> Optional[str]:
    if not url:
        return None
    m = _PR_URL_SLUG_RE.search(url)
    return f"{m.group(1)}/{m.group(2)}" if m else None


class _GhError(Exception):
    pass


def _default_gh_json(args: list[str], cwd: Optional[str]) -> dict[str, Any]:
    """Run ``gh <args>`` in ``cwd`` and parse a single JSON object."""
    import json

    res = _run(["gh", *args], cwd=cwd)
    if not res.ok:
        raise _GhError((res.stderr or res.stdout or "").strip()[:200])
    try:
        obj = json.loads(res.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise _GhError(f"non-JSON gh output: {exc}") from exc
    return obj if isinstance(obj, dict) else {}


def _default_shell_runner(command: str, cwd: str) -> int:
    """Run ``command`` via a login shell in ``cwd``, streaming to the terminal."""
    import subprocess

    proc = subprocess.run(["bash", "-lc", command], cwd=cwd)
    return proc.returncode
