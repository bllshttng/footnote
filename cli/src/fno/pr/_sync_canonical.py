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
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable, Optional

import typer

from fno.pr._proc import Result, ToolMissing, run as _run

# Single-flight claim TTL: long enough to cover a slow sync (build + restart),
# short enough that a crashed holder's lock recovers within one coffee break.
_SYNC_CLAIM_TTL_MS = 30 * 60 * 1000

# Bound every catch-up probe: this runs inside the pr-watch tick, and a hung gh
# would wedge the daemon this feature exists to work around. `timeout(1)` is
# absent on stock macOS, so the bound is _proc.run's own, never a shell wrapper.
_CATCHUP_PROBE_TIMEOUT_S = 30.0
# gh page size. The window filter is what actually bounds the sweep; this only
# caps the wire payload for a very busy week.
_CATCHUP_GH_LIMIT = 50

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
    # GitHub owner/repo are case-insensitive; compare lowercased so a casing
    # mismatch (Owner/Repo vs owner/repo) is not a false-positive refusal.
    if pr_slug and pr_slug.lower() != origin.lower():
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

    # Canonical-wide, NOT per-SHA. The claim's job is that two `fno restart`s
    # never overlap in one checkout, and a per-SHA key does not deliver that: a
    # catch-up for one merge and a merge-time sync for another take different
    # locks and pull, update, and restart concurrently. Exactly-once-per-SHA is
    # the marker's job, and it still is - separating the two is what lets this
    # key be the coarse one the non-overlap invariant actually needs.
    lock_key = "post-merge-sync"
    holder = f"sync-canonical:{pr_number}"
    try:
        claims.acquire_claim(
            lock_key, holder, ttl_ms=_SYNC_CLAIM_TTL_MS,
            reason="post-merge canonical sync", root=canonical,
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
            claims.release_claim(lock_key, holder, root=canonical)
        except Exception:
            pass  # lock is TTL-bounded; a failed release recovers on its own


# ---------------------------------------------------------------------------
# Catch-up sweep + staleness alarm
#
# The sync above fires only at merge-DETECTION time, so a merge nobody was alive
# to see is never synced. Everything below therefore reads only ground truth (gh,
# the marker dir, git's behind-count): the watcher state file and the events bus
# were both dead or lying during the outage this exists to survive.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SyncStaleness:
    """Outcome-keyed health of the canonical sync.

    ``state`` is ``fresh`` / ``stale`` / ``unknown`` rather than a bool: an
    unauthenticated gh must read as neither "fine" nor "alarm".
    """

    state: str
    markerless: tuple[dict, ...] = ()  # newest-first {number, sha, merged_at}
    behind: Optional[int] = None
    detail: str = ""


@dataclass(frozen=True)
class CatchupResult:
    """One catch-up sweep. ``outcome`` drives the tick's alarm decision."""

    outcome: str  # disabled | unknown | fresh | synced | marked | skipped | failed
    pr_number: Optional[int] = None
    swept: int = 0
    detail: str = ""
    # Whether the predicate found the canonical stale. The alarm is "detected AND
    # unresolved", so the tick needs the detection separately from the outcome: a
    # failed sync of a merge from two minutes ago is not yet an outage, and a
    # canonical proven behind with every marker present is one even though there
    # was nothing for the sweep to do.
    stale: bool = False


def _default_gh_list(canonical: Path, window_days: int) -> Optional[list[dict]]:
    """Merged PRs in the window, newest-first, or None when gh cannot answer.

    None (not an exception, not an empty list) is the "unknown" signal: an empty
    list means "nothing merged recently", and conflating the two would let a
    gh outage read as a clean bill of health.
    """
    import json

    try:
        res = _run(
            [
                "gh", "pr", "list", "--state", "merged",
                "--limit", str(_CATCHUP_GH_LIMIT),
                "--json", "number,mergedAt,mergeCommit",
            ],
            cwd=str(canonical),
            timeout=_CATCHUP_PROBE_TIMEOUT_S,
        )
    except Exception:  # noqa: BLE001 - ToolMissing, timeout, OSError all mean "unknown"
        return None
    if not res.ok:
        return None
    try:
        rows = json.loads(res.stdout or "[]")
    except json.JSONDecodeError:
        return None
    if not isinstance(rows, list):
        return None

    cutoff = datetime.now(timezone.utc) - timedelta(days=max(window_days, 0))
    out: list[dict] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        sha = (row.get("mergeCommit") or {}).get("oid")
        merged_at = _parse_iso(row.get("mergedAt"))
        if not sha or merged_at is None or merged_at < cutoff:
            continue
        out.append({"number": row.get("number"), "sha": sha, "merged_at": merged_at})
    out.sort(key=lambda r: r["merged_at"], reverse=True)
    return out


def _parse_iso(raw: object) -> Optional[datetime]:
    """Parse a gh timestamp, always tz-aware.

    A naive datetime here would raise TypeError on every comparison against the
    aware `now`, taking the whole leg down from one odd offset-less timestamp.
    """
    if not isinstance(raw, str) or not raw:
        return None
    try:
        dt = datetime.fromisoformat(raw.strip().replace("Z", "+00:00"))
    except ValueError:
        return None
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


def _behind_count(
    canonical: Path, runner: Callable[..., Result], *, fetch: bool = False
) -> Optional[int]:
    """Commits the default branch is behind its origin counterpart, or None.

    Without ``fetch`` the answer is only as fresh as the last fetch, which
    under-reports rather than inventing an alarm. That is wrong for the one case
    this number exists to catch: a merge that has aged out of the gh window
    leaves no markerless row, so a stale remote-tracking ref would read as zero
    behind and the outage would be invisible forever. Callers that report to a
    human (doctor) therefore fetch; the 5-minute tick does not.
    """
    if fetch:
        try:
            runner(
                ["git", "fetch", "--quiet", "origin"],
                cwd=str(canonical),
                timeout=_CATCHUP_PROBE_TIMEOUT_S,
            )
        except Exception:  # noqa: BLE001 - a failed fetch just means a staler answer
            pass
    try:
        head = runner(
            ["git", "symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            cwd=str(canonical),
            timeout=_CATCHUP_PROBE_TIMEOUT_S,
        )
    except Exception:  # noqa: BLE001 - a missing git is "unknown", not "behind 0"
        return None
    # A zero exit with empty stdout is not an answer: it would make `remote` an
    # empty string and the rev-list range malformed.
    remote = head.stdout.strip() if (head.ok and head.stdout.strip()) else "origin/main"
    local = remote.split("/", 1)[1] if "/" in remote else "main"
    try:
        res = runner(
            ["git", "rev-list", "--count", f"{local}..{remote}"],
            cwd=str(canonical),
            timeout=_CATCHUP_PROBE_TIMEOUT_S,
        )
    except Exception:  # noqa: BLE001
        return None
    if not res.ok:
        return None
    try:
        return int(res.stdout.strip())
    except ValueError:
        return None


def sync_staleness(
    *,
    settings: Any = None,
    canonical_root: Optional[Path] = None,
    runner: Callable[..., Result] = _run,
    gh_list: Optional[Callable[[Path, int], Optional[list[dict]]]] = None,
    fetch: bool = False,
) -> SyncStaleness:
    """Is the canonical checkout current with recently-merged PRs?

    Read-only, so it is safe for ``fno doctor`` to call regardless of
    ``post_merge.auto_run`` - reporting is not acting. ``fetch`` refreshes the
    remote-tracking ref first; see :func:`_behind_count` for why a human-facing
    caller wants it and the tick does not.
    """
    from fno.config import load_settings

    if settings is None:
        settings = load_settings()
    pm = settings.post_merge

    if canonical_root is None:
        from fno.paths import resolve_canonical_repo_root

        canonical_root = resolve_canonical_repo_root()
    canonical = Path(canonical_root)

    rows = (gh_list or _default_gh_list)(canonical, pm.catchup_window_days)
    if rows is None:
        return SyncStaleness("unknown", detail="gh unavailable or unauthenticated")

    markerless = tuple(
        r for r in rows if not _synced_marker(canonical, r["sha"]).exists()
    )
    behind = _behind_count(canonical, runner, fetch=fetch)

    # ANY markerless merge past the threshold is stale, not just the newest one.
    # Keying on the newest alone would call the older ones cosmetic on the theory
    # that a newer sync pulled past them - but a marker does not imply a pull (a
    # merge missing the sync_paths globs is marked without one), so the newest
    # being marked proves nothing about the merges behind it.
    now = datetime.now(timezone.utc)
    overdue = [
        r for r in markerless
        if (now - r["merged_at"]).total_seconds() / 3600 > pm.sync_stale_hours
    ]
    detail = ""
    stale = bool(overdue)
    if overdue:
        oldest = overdue[-1]  # markerless is newest-first
        age_h = (now - oldest["merged_at"]).total_seconds() / 3600
        detail = f"PR #{oldest['number']} merged {age_h:.0f}h ago, never synced"
        if len(overdue) > 1:
            detail += f" (+{len(overdue) - 1} more)"
    if behind:
        stale = True
        detail = (detail + "; " if detail else "") + f"local default branch {behind} behind origin"

    return SyncStaleness(
        "stale" if stale else "fresh", markerless, behind, detail
    )


def run_sync_catchup(
    *,
    settings: Any = None,
    canonical_root: Optional[Path] = None,
    runner: Callable[..., Result] = _run,
    gh_list: Optional[Callable[[Path, int], Optional[list[dict]]]] = None,
    sync: Optional[Callable[..., int]] = None,
) -> CatchupResult:
    """Sync the canonical for any merge the event-time triggers missed.

    Newest-only: one ``run_sync_canonical`` regardless of how many merges piled
    up, because a single pull brings HEAD current for all of them. The older
    swept SHAs are marker-stamped afterwards so they stop reading as stale - but
    ONLY once the newest SHA's marker proves the sync actually landed, so a
    claim-held skip or a failed ``fno update`` can never backdate a lie.
    """
    from fno.config import load_settings

    if settings is None:
        settings = load_settings()
    if not settings.post_merge.auto_run:
        return CatchupResult("disabled")

    if canonical_root is None:
        from fno.paths import resolve_canonical_repo_root

        canonical_root = resolve_canonical_repo_root()
    canonical = Path(canonical_root)

    st = sync_staleness(
        settings=settings, canonical_root=canonical, runner=runner, gh_list=gh_list
    )
    if st.state == "unknown":
        typer.echo(f"post-merge sync catch-up: {st.detail}; skipping", err=True)
        return CatchupResult("unknown", detail=st.detail)
    if not st.markerless:
        # Carry the staleness detail even here. Every marker can be present while
        # the canonical is still behind origin - that is what "the markers lie"
        # looks like, and it is the one state the sweep cannot act on, so the
        # least it can do is say so rather than report a flat "fresh".
        return CatchupResult("fresh", detail=st.detail, stale=st.state == "stale")

    newest = st.markerless[0]
    # Stamping the older merges is only sound if the newest one actually PULLED,
    # and neither the exit code nor the marker proves that: run_sync_canonical
    # returns 0 and writes a marker for a merge that misses the sync_paths globs
    # (a docs-only merge needs no build), having run nothing. Stamping older
    # merges off that would permanently mark real code merges as synced without
    # ever pulling them - the exact silent-skip this feature exists to end. So
    # the proof is whether sync_command's shell was entered, observed through
    # the shell_runner seam the function already exposes.
    pulled: list[int] = []

    def _tracking_shell(command: str, cwd: str) -> int:
        pulled.append(1)
        return _default_shell_runner(command, cwd)

    rc = (sync or run_sync_canonical)(
        newest["number"], settings=settings, canonical_root=canonical,
        shell_runner=_tracking_shell,
    )
    if rc != 0:
        typer.echo(
            f"post-merge sync catch-up: sync of PR #{newest['number']} failed "
            f"(exit {rc}); markers withheld, will retry",
            err=True,
        )
        return CatchupResult(
            "failed", newest["number"], detail=f"exit {rc}", stale=st.state == "stale"
        )

    if not _synced_marker(canonical, newest["sha"]).exists():
        return CatchupResult(
            "skipped", newest["number"],
            detail="sync declined (claim held or out of scope)",
            stale=st.state == "stale",
        )

    if not pulled:
        # The newest merge needed no sync, so it is marked but nothing was
        # pulled. The older merges keep their claim on the next sweep, which
        # will pick the newest REMAINING one and pull for real.
        return CatchupResult(
            "marked", newest["number"],
            detail="no buildable change; older merges still pending",
            stale=st.state == "stale",
        )

    swept = 0
    for row in st.markerless[1:]:
        marker = _synced_marker(canonical, row["sha"])
        if not marker.exists():
            _write_marker(marker)
            swept += 1
    typer.echo(
        f"post-merge sync catch-up: synced PR #{newest['number']}"
        + (f", stamped {swept} older merge(s)" if swept else "")
    )
    return CatchupResult("synced", newest["number"], swept)


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
