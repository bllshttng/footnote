"""fno do pr ritual - the mechanical core of the post-merge ritual (x-bbde).

One idempotent verb runs the ~90% of ``skills/pr/references/merged.md`` that is
pure CLI orchestration. Per leg it shells the existing fno verb, captures the
exit code, and prints one receipt line::

    step=<name> status=<ok|skipped|failed> detail=<...>

The judgment residue (deferral-node triage + parking-lot prose) is either done
inline by an attended session (the skill body) or spawned as ONE headless
one-shot when this verb runs autonomous. Never a bg thread (epic Locked
Decision 9).

Why a verb, not the skill: the ritual wore a full LLM session for a ~117k-token
birth payload to run a sequence of fno verbs, and its bash snippets carried a
proven zsh/ugrep silent-misfire class (x-f47f: ``${VAR:+...}`` word-splitting,
empty-alternation grep). Python builds each argv explicitly, so that whole
portability class is structurally gone and every leg's failure is loud - there
is no ``|| true`` anywhere (Locked Decision 5).

Absorbed bugs, each verified in this PR's tests:

- x-c4ff - only real verbs are called (``skill-diff reconcile``, ``pr
  sync-canonical`` both exist); no dangling references survive.
- x-fb99 - ``parking_lot_path`` is resolved against the CANONICAL root, never a
  worktree cwd that may carry a stale override.
- x-adf9 - canonical-sync pipes are closed + timeouted (see
  ``_sync_canonical._default_shell_runner``) so a trailing ``fno agents restart``
  detached daemon cannot hold the pipe and wedge the ritual.
- x-0d66 - the advance leg runs bounded with streamed progress lines instead of
  hanging silent for minutes.

The detector/dispatch cutover is the separate blocked child x-a35a; this verb
honors the existing dedup apparatus (markers + claims) unchanged.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

import typer

from fno._subprocess_util import fno_py_cmd
from fno.config import load_settings_for_repo
from fno.pr._proc import Result, ToolMissing, run as _run

# Cross-runner mutex (Step 0.5): a global TTL claim so an attended `/fno:pr
# merged` and an auto-dispatched worker cannot run the destructive middle
# concurrently. 15m bounds a run that finishes in 1-3 min; the TTL is the
# crash backstop.
_CLAIM_TTL = "15m"
# x-0d66: bound the advance leg. advance dispatches successors inline and can
# spend minutes with no output; a bounded run with progress lines surfaces
# partial-dispatch state instead of wedging the ritual. Killing mid-dispatch is
# safe: advance's `dispatch:<id>` reservations are TTL claims that survive the
# kill, so a re-run dedups rather than orphaning a half-wave.
_ADVANCE_TIMEOUT_S = 180.0
# Per-leg default bound so a single hung verb cannot wedge the whole ritual.
_LEG_TIMEOUT_S = 120.0
# Headless judgment worker: reads the merged diff + updates the backlog, which
# routinely exceeds a minute. Passed to `fno agents spawn --timeout` and matched
# by the outer subprocess bound so the worker is not killed early (codex P2).
_JUDGMENT_TIMEOUT_S = 600.0

_OK = "ok"
_SKIPPED = "skipped"
_FAILED = "failed"
# Distinct from _SKIPPED: the work is still owed, and a later sweep does it. A
# leg that emitted `skipped` with instructions attached read as "nothing to do
# here" while quietly handing the operator a command nobody ran.
_DEFERRED = "deferred"


@dataclass
class Receipt:
    step: str
    status: str
    detail: str = ""

    def line(self) -> str:
        d = self.detail.replace("\n", " ").strip()
        if len(d) > 200:
            d = d[:197] + "..."
        return f"step={self.step} status={self.status} detail={d}"


@dataclass
class _Ctx:
    """Per-PR context resolved ONCE (the bash re-derived it in every block)."""

    pr: int
    autonomous: bool
    canon: Optional[Path]
    settings: object
    pm: object  # config.post_merge (PostMergeBlock)
    project: str
    lane_project: str
    parking_lot: Optional[Path]
    holder: str
    owns_claim: bool = False
    node_ids: list = field(default_factory=list)
    receipts: list = field(default_factory=list)


def _git_text(argv: list[str], cwd: Path) -> str:
    try:
        r = _run(["git", *argv], cwd=str(cwd), timeout=15.0)
    except ToolMissing:
        return ""
    return r.stdout.strip() if r.ok else ""


def _canonical_root(cwd: Path) -> Optional[Path]:
    """The CANONICAL checkout, never the worktree this may run from.

    A lane worktree can carry a per-worktree ``parking_lot_path`` override and a
    real ``internal/`` dir; resolving config + paths against it would strand the
    durable parking-lot write in a file archive-worktree.sh deletes (x-fb99,
    x-071c). ``--git-common-dir`` points at the shared ``.git`` whose parent IS
    canonical; it may be relative, so resolve before taking the parent.
    """
    gcd = _git_text(["rev-parse", "--git-common-dir"], cwd)
    if gcd:
        p = Path(gcd)
        if not p.is_absolute():
            p = (cwd / p).resolve()
        if p.exists():
            return p.parent
    top = _git_text(["rev-parse", "--show-toplevel"], cwd)
    return Path(top) if top else None


def _worktree_root(cwd: Path) -> Path:
    """Resolve the checkout containing ``cwd`` for worktree-local config."""
    top = _git_text(["rev-parse", "--show-toplevel"], cwd)
    return Path(top).resolve() if top else Path(cwd).resolve()


# Anchored so a lookalike host or a github.com path segment cannot match: the
# scheme and user@ are optional, then `github.com` must be the host (optionally
# :port), then : or / . This is the exact semantics of the bash scan it replaces
# (AC9b resolves every GitHub remote form; AC9c rejects every lookalike).
_ORIGIN_SLUG_RE = re.compile(
    r"^(?:[a-z][a-z0-9+.-]*://)?"
    r"(?:[^/@]*@)?"
    r"github\.com(?::[0-9]+)?[:/]"
    r"(.+)$"
)


def _parse_origin_slug(url: str) -> Optional[str]:
    """``owner/repo`` from a GitHub remote URL, or None for a non-GitHub origin.

    Ported verbatim from the post-merge scan's bash sed: lowercase, strip an
    optional scheme and user@, require ``github.com`` as the host, take the
    path, strip trailing ``/`` and a trailing ``.git``, and require exactly
    ``owner/repo`` (a deeper path is rejected rather than guessed).
    """
    m = _ORIGIN_SLUG_RE.match((url or "").strip().lower())
    if not m:
        return None
    rest = m.group(1).rstrip("/")
    if rest.endswith(".git"):
        rest = rest[:-4]
    if rest.count("/") != 1:
        return None
    owner, repo = rest.split("/")
    return rest if owner and repo else None


def _scan_nodes(entries, pr: int, slug: Optional[str]) -> list[str]:
    """Graph-derived node ids whose pr_url matches this PR's repo.

    Repo-scoped because pr_number is unique only within a repo (cross-project
    graph): a foreign repo sharing the number is excluded. A url-less or
    non-string pr_url is skipped, never fatal (a corrupt entry cannot drop the
    legitimate nodes after it). Pure so the harness ACs test it directly.
    """
    if not slug:
        return []
    needle = f"/{slug.lower()}/pull/"
    out: list[str] = []
    for e in entries or []:
        if not isinstance(e, dict) or e.get("pr_number") != pr:
            continue
        url = e.get("pr_url")
        if not isinstance(url, str) or needle not in url.lower():
            continue
        nid = e.get("id")
        if nid and nid not in out:
            out.append(nid)
    return out


def _session_holder() -> str:
    """Runner-unique, stable-per-runner mutex holder (Step 0.5).

    A shared constant would read as an idempotent re-acquire and silently
    defeat the mutex; session-keyed holders keep two racing runners distinct.
    """
    from fno.harness_identity import current_session_id

    sid = current_session_id() or ""
    if not sid:
        try:
            r = _run([*fno_py_cmd(), "agents", "claim", "session-pid"], timeout=10.0)
            if r.ok:
                sid = r.stdout.strip()
        except (ToolMissing, subprocess.SubprocessError):
            sid = ""
    if not sid:
        sid = str(os.getpid())
    return f"postmerge:pr-holder:{sid}"


def _resolve_pr(pr: Optional[int], runner: Callable) -> Optional[int]:
    if pr:
        return int(pr)
    try:
        r = runner(
            ["gh", "pr", "list", "--state", "merged", "--json", "number,mergedAt",
             "--limit", "1"],
            timeout=30.0,
        )
    except ToolMissing:
        return None
    if not r.ok:
        return None
    try:
        rows = json.loads(r.stdout or "[]")
    except json.JSONDecodeError:
        return None
    if not isinstance(rows, list) or not rows:
        return None
    n = rows[-1].get("number") if isinstance(rows[-1], dict) else None
    return int(n) if n else None


def _parking_lot_path(canon: Optional[Path], pm) -> Optional[Path]:
    rel = getattr(pm, "parking_lot_path", None)
    if not rel or not canon:
        return None
    # Defense-in-depth: the schema rejects absolute / '..' paths, but a stale
    # installed fno might not, so backstop before joining onto canonical.
    if rel.startswith(("/", "~")) or f"/{rel}/".find("/../") >= 0:
        return None
    return canon / rel


class Ritual:
    """Runs the mechanical legs, emitting one receipt per leg."""

    # Class-level default so an object built without __init__ still has it.
    # Tests here construct via __new__ (the `_bare()` helper), and a guard that
    # only works on the constructed path is the decorative kind. False is the
    # honest default: no number was supplied.
    pr_supplied: bool = False
    # Same class-level-default reason: `_bare()`-style tests skip __init__, and
    # a cache that only exists on the constructed path is the decorative kind.
    _merge_state: Optional[tuple] = None

    def __init__(self, pr: Optional[int], autonomous: bool, cwd: Path,
                 runner: Callable = _run) -> None:
        self.runner = runner
        self.cwd = Path(cwd)
        canon = _canonical_root(self.cwd)
        settings = load_settings_for_repo(canon) if canon else None
        # SettingsModel exposes post_merge/project directly - there is no
        # `.config` wrapper attribute, so the old `getattr(settings, "config")`
        # was always None and the config leg failed on every run (x-bbde verb
        # shipped non-functional). The model IS the config root.
        cfg = settings
        pm = getattr(cfg, "post_merge", None) if cfg else None
        project = getattr(getattr(cfg, "project", None), "id", "") or "" if cfg else ""
        lane_settings = None
        try:
            lane_settings = load_settings_for_repo(_worktree_root(self.cwd))
        except Exception:  # noqa: BLE001 - a worktree with no config degrades to canon read
            lane_settings = settings
        lane_cfg = lane_settings if lane_settings else cfg
        lane_project = getattr(getattr(lane_cfg, "project", None), "id", "") or "" if lane_cfg else ""
        self.ctx = _Ctx(
            pr=0,
            autonomous=autonomous,
            canon=canon,
            settings=settings,
            pm=pm,
            project=project,
            lane_project=lane_project,
            parking_lot=_parking_lot_path(canon, pm),
            holder=_session_holder(),
        )
        # Where the number came from is part of the receipt. Omit it and
        # `_resolve_pr` takes the repo's most recent merge, which on a night
        # with four merges is very likely not the caller's. A silent pick
        # decides which worktree gets archived.
        self.pr_supplied = bool(pr)
        resolved = _resolve_pr(pr, runner)
        if resolved:
            self.ctx.pr = resolved

    # -- seams -------------------------------------------------------------

    def _sh(self, argv: list[str], *, cwd: Optional[Path] = None,
            timeout: float = _LEG_TIMEOUT_S) -> Result:
        """Shell an fno verb via the PATH-robust fno-py prefix."""
        return self.runner([*fno_py_cmd(), *argv], cwd=str(cwd or self.canon), timeout=timeout)

    def _gh(self, argv: list[str], *, timeout: float = 30.0) -> Result:
        return self.runner(["gh", *argv], cwd=str(self.canon or self.cwd), timeout=timeout)

    @property
    def canon(self) -> Path:
        return self.ctx.canon or self.cwd

    # -- receipts ----------------------------------------------------------

    def _emit(self, step: str, status: str, detail: str = "") -> None:
        rec = Receipt(step, status, detail)
        self.ctx.receipts.append(rec)
        typer.echo(rec.line())

    def _leg(self, step: str, argv: list[str], *, cwd: Optional[Path] = None,
             timeout: float = _LEG_TIMEOUT_S) -> Result:
        """Run one best-effort leg; map exit code to ok/failed, never raise.

        A non-zero exit is the load-bearing signal (x-f47f): the receipt names
        the leg and the verb exits non-zero, so a failed leg is never readable
        as a no-op.
        """
        try:
            r = self._sh(argv, cwd=cwd, timeout=timeout)
        except subprocess.TimeoutExpired:
            self._emit(step, _FAILED, "timeout")
            return Result(124, "", "timeout")
        except (ToolMissing, subprocess.SubprocessError) as exc:
            self._emit(step, _FAILED, f"spawn-error: {exc}")
            return Result(127, "", str(exc))
        tail = (r.stdout or r.stderr or "").strip().splitlines()
        detail = tail[-1] if tail else f"exit={r.returncode}"
        self._emit(step, _OK if r.ok else _FAILED,
                   detail if r.ok else f"exit={r.returncode} {detail}".strip())
        return r

    # -- the legs ----------------------------------------------------------

    def acquire_mutex(self) -> bool:
        """Step 0.5: refuse if another runner owns this PR's ritual."""
        key = f"reconcile:pr-{self.ctx.pr}"
        try:
            r = self.runner([*fno_py_cmd(), "agents", "claim", "acquire", key,
                             "--holder", self.ctx.holder, "--ttl", _CLAIM_TTL],
                            timeout=15.0)
        except (ToolMissing, subprocess.SubprocessError) as exc:
            # Fail open: a claims hiccup must not wedge the ritual (the marker
            # is the backstop). Proceed without the mutex.
            self._emit("mutex", _SKIPPED, f"acquire-error: {exc}")
            return True
        if r.returncode == 1:
            self._emit("mutex", _SKIPPED, "already-held by another runner")
            return False  # the other runner owns the ritual; stop here
        if not r.ok:
            self._emit("mutex", _SKIPPED, f"acquire exit={r.returncode}; fail-open")
            return True
        self.ctx.owns_claim = True
        self._emit("mutex", _OK, "acquired")
        return True

    def release_mutex(self) -> None:
        if not self.ctx.owns_claim:
            return
        key = f"reconcile:pr-{self.ctx.pr}"
        try:
            self.runner([*fno_py_cmd(), "agents", "claim", "release", key,
                         "--holder", self.ctx.holder], timeout=15.0)
        except (ToolMissing, subprocess.SubprocessError):
            pass  # TTL reaps it
        self.ctx.owns_claim = False

    def leg_stamp(self) -> None:
        """Step 2: close the merged node(s), reconcile plan status, stamp ship."""
        # --pr-number (x-59a6): bind every node this PR's exact Backlog-Closure
        # trailer names to the PR BEFORE the drift scan the rest of reconcile
        # already ran unconditionally - a PR naming several nodes (only one of
        # which ever got individually stamped at creation) now closes all of
        # them here, not just the primary. --repo is passed explicitly when
        # resolvable so a same-numbered PR in another repo can never be
        # mismatched; reconcile's own repo scoping still fails closed on a
        # bare-number match when this resolution comes up empty (never left
        # to cwd inference to guess).
        argv = ["backlog", "reconcile", "--pr-number", str(self.ctx.pr), "--json"]
        from fno.graph._reconcile import resolve_current_repo_slug

        repo = resolve_current_repo_slug(str(self.canon))
        if repo:
            argv += ["--repo", repo]
        else:
            typer.echo(
                f"warning: could not resolve this repo's slug for PR #{self.ctx.pr}; "
                "reconcile will scope to exact trailer claims only, skipping any "
                "bare-number backfill match",
                err=True,
            )
        try:
            r = self._sh(argv)
        except subprocess.TimeoutExpired:
            self._emit("reconcile", _FAILED, "timeout")
            return
        except (ToolMissing, subprocess.SubprocessError) as exc:
            self._emit("reconcile", _FAILED, f"spawn-error: {exc}")
            return
        if r.ok:
            try:
                obj = json.loads(r.stdout or "{}")
                closed = [c.get("node_id") for c in (obj.get("closed") or [])
                          if isinstance(c, dict) and c.get("node_id")]
                self.ctx.node_ids.extend(closed)
                held = [h.get("node_id") for h in (obj.get("promise_unmet") or [])
                        if isinstance(h, dict) and h.get("node_id")]
                errs = len(obj.get("contained_errors") or [])
                sync_obj = obj.get("sync_catchup") or {}
                sync_outcome = sync_obj.get("outcome") if isinstance(sync_obj, dict) else None
                closure_refused = obj.get("closure_refused")
                if held:
                    # Held open, not clean: the PR merged but the promise gate
                    # refused the close (x-40be). status=ok detail=closed=0
                    # covered "held seven nodes open"; deferred keeps the work
                    # visibly owed and names who holds it.
                    ids = ", ".join(str(h) for h in held[:5])
                    more = f" +{len(held) - 5}" if len(held) > 5 else ""
                    self._emit(
                        "reconcile",
                        _DEFERRED,
                        f"closed={len(closed)} held_open={len(held)}: {ids}{more}",
                    )
                elif closure_refused:
                    # The trailer-claimed nodes (if any) never got bound - a
                    # flaky gh query, cross-repo mismatch, or unmerged PR
                    # refusal. Never silently readable as "no-drift": that
                    # would mask a real closure failure until some later
                    # unrelated sweep happens to rediscover it (round-7 review).
                    self._emit(
                        "reconcile", _DEFERRED, f"closure binding refused: {closure_refused}",
                    )
                elif errs or sync_outcome == "failed":
                    # reconcile exits 0 here (only unresolvable PRs exit 4), so
                    # the leg is not failed - but cascade-close failures and a
                    # failed canonical sync must not read as a clean run.
                    self._emit(
                        "reconcile",
                        _OK,
                        f"closed={len(closed)} contained-errors={errs} "
                        f"sync={sync_outcome or 'not-run'}",
                    )
                elif closed or (obj.get("candidates") or []):
                    self._emit("reconcile", _OK, f"closed={len(closed)}")
                elif any(obj.get(k) for k in ("contained_closed", "healed_epics", "reverted")) or sync_outcome in ("synced", "marked"):
                    # Drift was found and healed without a new close: contained
                    # nodes / stranded epics / revert stamps / a canonical
                    # catch-up sync. NOT no-drift - the run mutated state.
                    self._emit("reconcile", _OK, "healed-only")
                else:
                    self._emit("reconcile", _OK, "no-drift")
            except json.JSONDecodeError:
                detail = "no-op" if not (r.stdout or "").strip() else "non-json"
                self._emit("reconcile", _OK, detail)
        else:
            self._emit("reconcile", _FAILED, f"exit={r.returncode}")
        # Step 2a: plan frontmatter. Idempotent.
        self._leg("plan-reconcile", ["do", "plan", "reconcile-status", "--apply"])
        # Ship provenance used to be stamped here (`session add --phase ship` on
        # the PR). It recorded the ritual session, not the implementer, and only
        # fired on the `fno do pr merged` path. The ship row now lands at
        # `fno backlog update --pr-number` (the PR-link choke point), which every
        # shipped node passes through regardless of how it merged.

    def leg_harvest(self) -> None:
        argv = ["retro", "run", "--pr-number", str(self.ctx.pr)]
        if self.ctx.autonomous:
            argv.append("--keep-going")
        self._leg("retro", argv)

    def leg_advance(self) -> None:
        """Step 3b: merge-triggered next dispatch, bounded + progress (x-0d66)."""
        argv = ["backlog", "advance", "-J", "--verbose"]
        # x-59a6: no --closed here. `leg_stamp`'s reconcile call already ran
        # `_advance`/`advance_dependents` per CLOSED RECORD for every node this
        # PR's trailer bound, not only the first - `--closed` takes a SINGLE
        # id, so passing `self.ctx.node_ids[0]` on a multi-node PR would key
        # this leg's own dependents check off one node and silently miss the
        # others' dependents. This leg is a plain next-selection pass; the
        # per-node dependents dispatch already happened at close time.
        if self.ctx.lane_project:
            argv += ["--project", self.ctx.lane_project]
        self._stream("advance", argv, _ADVANCE_TIMEOUT_S)

    def leg_skill_diff(self) -> None:
        """Step 3c: close the skill-diff loop. x-c4ff: the real verb exists."""
        self._leg(
            "skill-diff",
            ["doctor", "skill-diff", "reconcile", "--pr-number", str(self.ctx.pr)],
        )

    def leg_sync_canonical(self) -> None:
        """Step 3d: x-adf9 fix lives in _sync_canonical._default_shell_runner."""
        if not getattr(self.ctx.pm, "sync_command", None):
            self._emit("sync-canonical", _SKIPPED, "not configured")
            return
        self._leg("sync-canonical", ["do", "pr", "sync-canonical", "--pr-number", str(self.ctx.pr)],
                  timeout=900.0)

    def _merged_state(self) -> tuple[Optional[str], Optional[str]]:
        """(state, headRefName) from ONE gh call, or (None, None) if unreadable.

        Memoized: two legs ask, and one PR view per ritual is enough. The state
        can only travel toward MERGED during a run, so a cached OPEN refuses
        more, never less.

        `state` is the whole point. This call already existed and read only the
        branch, so an OPEN PR resolved a branch, found its worktree, and handed
        it to the archive script - whose own checks (clean, pushed, no live
        session) a worker who finished and now waits on review passes cleanly.

        The merge is not self-evident on every trigger. The pr-watch daemon
        fires on a gh-backed `state == MERGED`, but `fno do pr merged <n>` reaches
        `_resolve_pr`, which returns a caller-supplied number unchecked. On that
        path the merge is an ARGUMENT, so the guard belongs here at the leg,
        where a removal actually happens, not at resolve.
        """
        if self._merge_state is None:
            self._merge_state = self._merged_state_read()
        return self._merge_state

    def _merged_state_read(self) -> tuple[Optional[str], Optional[str]]:
        try:
            meta = self._gh(["pr", "view", str(self.ctx.pr),
                             "--json", "state,headRefName"])
        except (ToolMissing, subprocess.SubprocessError):
            return (None, None)
        if not meta.ok:
            return (None, None)
        try:
            obj = json.loads(meta.stdout or "{}")
        except json.JSONDecodeError:
            return (None, None)
        return (obj.get("state") or None, obj.get("headRefName") or None)

    def _refuse_unless_merged(self, step: str) -> bool:
        """True when gh confirms MERGED. Emits the refusal itself otherwise.

        Fails CLOSED: an unreadable state refuses too. "I could not check"
        must never spend the same as "I checked and it is merged".
        """
        state, _ = self._merged_state()
        if state == "MERGED":
            return True
        detail = f"not-merged (state={state})" if state else "merge-state unreadable"
        self._emit(step, _SKIPPED, detail)
        return False

    def leg_archive(self) -> None:
        """Step 4: best-effort worktree archive; defer when run from inside it."""
        state, branch = self._merged_state()
        if state is None:
            self._emit("archive", _SKIPPED, "gh-unavailable")
            return
        if state != "MERGED":
            self._emit("archive", _SKIPPED, f"not-merged (state={state})")
            return
        if not branch:
            self._emit("archive", _SKIPPED, "no-branch")
            return
        wt = self._find_worktree(branch)
        if not wt:
            self._emit("archive", _SKIPPED, f"no worktree for {branch}")
            return
        if Path(wt).resolve() == self.cwd.resolve():
            # Never self-remove. This is the DOMINANT path, because the worker
            # that merged its own PR is standing in its own worktree - which is
            # why the freshest merged worktrees were the ones that survived.
            # `deferred` says the work is still owed; `skipped` plus a command
            # in the detail line read as done and nobody ever ran it.
            self._emit("archive", _DEFERRED, "sweep-will-reap")
            return
        script = self.canon / "scripts" / "setup" / "archive-worktree.sh"
        if not script.exists():
            self._emit("archive", _SKIPPED, "archive-worktree.sh missing")
            return
        try:
            r = self.runner(["bash", str(script), str(wt), "--yes"],
                            cwd=str(self.canon), timeout=120.0)
        except subprocess.TimeoutExpired:
            self._emit("archive", _FAILED, "timeout")
            return
        except (ToolMissing, subprocess.SubprocessError) as exc:
            self._emit("archive", _FAILED, f"spawn-error: {exc}")
            return
        self._emit("archive", _OK if r.ok else _FAILED,
                   "archived" if r.ok else f"exit={r.returncode} (worktree left in place)")

    def _find_worktree(self, branch: str) -> Optional[str]:
        out = _git_text(["worktree", "list", "--porcelain"], self.canon)
        canonical = str(self.canon.resolve())
        for block in out.split("\n\n"):
            path = ""
            for line in block.splitlines():
                if line.startswith("worktree "):
                    path = line[len("worktree "):]
            if not path or path == canonical:
                continue
            hb = _git_text(["rev-parse", "--abbrev-ref", "HEAD"], Path(path))
            if hb == branch:
                return path
        return None

    # -- judgment ----------------------------------------------------------

    def _deferral_nodes(self) -> int:
        """Step 3e input (a): this PR's deferral-born nodes still open."""
        try:
            r = self._sh(["backlog", "find", "deferred from PR", "-J"])
        except (ToolMissing, subprocess.SubprocessError):
            return 0
        if not r.ok:
            return 0
        try:
            rows = json.loads(r.stdout or "[]")
        except json.JSONDecodeError:
            return 0
        needle = f"#{self.ctx.pr}"
        count = 0
        for row in rows:
            if not isinstance(row, dict):
                continue
            if str(row.get("status", "")).lower() in ("done", "superseded", "cancelled"):
                continue
            details = str(row.get("details") or "")
            parent = str(row.get("parent") or "")
            if needle in details or (self.ctx.node_ids and parent in self.ctx.node_ids):
                count += 1
        return count

    def _diff_stat(self) -> tuple[int, int]:
        """Step 6 input (b): (changed_files, additions+deletions)."""
        try:
            r = self._gh(["pr", "view", str(self.ctx.pr),
                          "--json", "additions,deletions,changedFiles"])
        except (ToolMissing, subprocess.SubprocessError):
            return (0, 0)
        if not r.ok:
            return (0, 0)
        try:
            obj = json.loads(r.stdout or "{}")
        except json.JSONDecodeError:
            return (0, 0)
        files = int(obj.get("changedFiles") or 0)
        lines = int(obj.get("additions") or 0) + int(obj.get("deletions") or 0)
        return (files, lines)

    def leg_judgment(self) -> None:
        """Compute judgment inputs; spawn one headless leg (autonomous only)."""
        deferred = self._deferral_nodes()
        files, lines = self._diff_stat()
        pl_set = bool(self.ctx.parking_lot)
        # The bar gates on diff size alone, not parking-lot availability: a
        # maintainer-only item has a default home (.fno/tasks/user.md) even
        # without a shared lot, so the dedicated destination must stay reachable
        # on the normal no-deferrals path rather than dead unless a lot is set.
        above_bar = files >= 1 and lines >= 10
        inputs = (f"deferred={deferred} files={files} lines={lines} "
                  f"parking_lot={'set' if pl_set else 'unset'} bar="
                  f"{'above' if above_bar else 'below'}")
        has_inputs = deferred > 0 or above_bar
        if not self.ctx.autonomous:
            # An attended session does the judgment itself (skill body).
            self._emit("judgment", _OK, f"deferred-to-skill (attended); {inputs}")
            return
        if not has_inputs:
            reason = "no-inputs" if not above_bar and deferred == 0 else "diff-below-bar"
            self._emit("judgment", _SKIPPED, f"reason={reason}; {inputs}")
            return
        spawned = self._spawn_judgment(deferred, files, lines)
        if spawned:
            self._emit("judgment", _OK, f"spawned headless; {inputs}")
        else:
            self._emit("judgment", _FAILED, f"spawn-failed; {inputs}")

    def _spawn_judgment(self, deferred: int, files: int, lines: int) -> bool:
        """ONE headless one-shot carrying only the two judgment steps.

        ``agents spawn`` takes ONE positional - the MESSAGE - and the agent name
        rides ``--name``; a second positional is refused ("takes one positional;
        the agent name moved to --name"). So the prompt is the sole positional
        and ``judgment-pr-<n>`` is passed via ``--name``. (Before the axis
        redesign the grammar was ``[name] [message]`` and the two were swapped
        positionals; a stale two-positional call fails closed here, which is
        exactly how the redesign's refusal caught this leg.) The headless worker
        reads a diff and updates the backlog, which routinely exceeds a minute,
        so it gets spawn's own ``--timeout`` and the outer bound matches it
        rather than killing the worker early.
        """
        prompt = self._judgment_prompt(deferred, files, lines)
        argv = [*fno_py_cmd(), "agents", "spawn", "--substrate", "headless",
                "--timeout", str(int(_JUDGMENT_TIMEOUT_S)),
                "-c", str(self.canon)]
        # Un-routed site: no --role, so config.post_merge.model IS the model.
        # Deliberately NOT `--role post-merge` - that role auto-routes to the
        # secondary provider, and this leg renders a judgment.
        # --harness pins the binary WITH the model (the groom worker's pairing):
        # the value is a claude model id, and an explicit --model bypasses the
        # provider-scoping that would otherwise drop it, so a codex-ambient
        # session would hand the claude id to codex and 400.
        model = getattr(self.ctx.pm, "model", None)
        if model:
            argv += ["--harness", "claude", "--model", model]
        # Behind `--` (fno's own click parser honors it, verified both
        # directions): a leading-flag seed must be the prompt positional.
        argv += ["--name", f"judgment-pr-{self.ctx.pr}", "--", prompt]
        try:
            r = self.runner(argv, timeout=_JUDGMENT_TIMEOUT_S + 60.0)
        except (ToolMissing, subprocess.SubprocessError):
            return False
        return r.ok

    def _judgment_prompt(self, deferred: int, files: int, lines: int) -> str:
        pr = self.ctx.pr
        marker = getattr(self.ctx.pm, "maintainer_marker", None) or ""
        if self.ctx.parking_lot is not None:
            # Shared/vault destination: narrative AND maintainer-only items both
            # live in the parking-lot section, the latter tagged only when a
            # marker is configured (the discriminator is earned by a shared lot).
            dest = str(self.ctx.parking_lot)
            tag = f" tagged `{marker}`" if marker else ""
            content = (f"narrative plus maintainer-only items (a "
                       f"decision/sign-off/manual setup){tag}")
        else:
            # Default: no shared lot, so ONLY maintainer-only items get a home
            # (a dedicated repo-local file). Narrative has no shared-lot home, so
            # it must NOT go in this file - file real follow-up work as a backlog
            # node instead. A dedicated file needs no discriminator tag.
            dest = ".fno/tasks/user.md (under the canonical repo root)"
            content = ("maintainer-only items only (a decision/sign-off/manual "
                       "setup), no tag - do NOT write narrative here")
        return (
            f"Post-merge judgment for PR #{pr} (autonomous; do not prompt). "
            f"(a) Triage {deferred} deferral-born node(s) filed from this PR: "
            f"run `fno backlog find 'deferred from PR #{pr}'` and for each open "
            f"node decide promote/keep/defer/supersede via the matching "
            f"`fno backlog` verb, logging undecided ones. "
            f"(b) Follow-up capture: read `gh pr diff {pr}`, and if the diff "
            f"(files={files}, lines={lines}) carries genuine follow-up context, "
            f"append ONE dated section keyed `<!-- post-merge:pr-{pr} -->` to "
            f"{dest} via an append-only redirect (never overwrite): {content}. "
            f"Skip if below the bar. "
            f"Self-end when done; never spawn further work."
        )

    # -- row reap ----------------------------------------------------------

    def _resolve_origin_slug(self) -> Optional[str]:
        """``owner/repo`` for this checkout: git remote first, then gh fallback.

        git remote needs no network, so an offline run still scopes the scan
        instead of skipping it. gh covers a checkout whose GitHub remote is not
        named ``origin``. An unresolvable slug skips the union wholesale (under-
        reaping is recoverable; reaping another repo's row is not).
        """
        try:
            r = self.runner(["git", "remote", "get-url", "origin"],
                            cwd=str(self.canon), timeout=15.0)
            if r.ok:
                slug = _parse_origin_slug(r.stdout.strip())
                if slug:
                    return slug
        except (ToolMissing, subprocess.SubprocessError):
            pass
        try:
            r = self.runner(["gh", "repo", "view", "--json", "nameWithOwner",
                             "-q", ".nameWithOwner"], cwd=str(self.canon), timeout=30.0)
            if r.ok:
                s = r.stdout.strip().lower()
                if s.count("/") == 1 and all(p for p in s.split("/")):
                    return s
        except (ToolMissing, subprocess.SubprocessError):
            pass
        return None

    def _recover_node_for_pr(self) -> list[str]:
        """Sidecar-derived node id(s) for this PR when reconcile closed nothing.

        The dominant path closes + stamps the node at the ship gate, so
        ``backlog reconcile`` no-ops and ``.closed[]`` is empty. Recover the
        PR's node from the sidecar store (repo-scoped; PR links are
        footnote-owned ship evidence, so the scan works on any tracker
        backend) so the row reap still finds it - the replaced bash Step 2 did
        this scan inline (codex P2).
        """
        slug = self._resolve_origin_slug()
        if not slug:
            return []
        try:
            from fno.tracker import sidecar as sidecar_store

            rows = [
                {"id": nid, "pr_number": sc.pr_number, "pr_url": sc.pr_url}
                for nid, sc in sidecar_store.load_all().items()
            ]
        except Exception:  # noqa: BLE001 - unreadable store degrades to no recovery
            return []
        return _scan_nodes(rows, self.ctx.pr, slug)

    def leg_reap_rows(self) -> None:
        """Step 8a: reap the merged node's lingering build-worker rows."""
        # Same guard as leg_archive, for the same reason: this leg works from
        # node ids and checked merge state nowhere, so an asserted merge on the
        # attended path reached a removal unverified.
        if not self._refuse_unless_merged("reap-rows"):
            return
        if not self.ctx.node_ids:
            # Dominant path: the ship gate already closed the node, so
            # reconcile's .closed[] was empty. Recover it from the graph
            # (repo-scoped pr_number match) so the row reap still runs -
            # mirroring the replaced bash Step 2 scan (codex P2).
            self.ctx.node_ids = self._recover_node_for_pr()
        if not self.ctx.node_ids:
            self._emit("reap-rows", _SKIPPED, "no closed node ids")
            return
        reap_on = _truthy(getattr(self.ctx.pm, "self_reap", False))
        rows = self._dead_target_rows()
        if not rows:
            self._emit("reap-rows", _OK, "no lingering rows")
            return
        if not reap_on:
            cmds = "; ".join(f"fno agents stop {n} && fno agents rm {n}" for n in rows)
            self._emit("reap-rows", _OK,
                       f"{len(rows)} row(s) linger; self_reap off - clear: {cmds}")
            return
        # STOP before RM - `agents rm` on a live agent orphans its supervisor.
        removed = 0
        for name in rows:
            try:
                self.runner([*fno_py_cmd(), "agents", "stop", name], timeout=30.0)
                r = self.runner([*fno_py_cmd(), "agents", "rm", name], timeout=30.0)
                if r.ok:
                    removed += 1
            except (ToolMissing, subprocess.SubprocessError):
                pass
        self._emit("reap-rows", _OK, f"reaped {removed}/{len(rows)}")

    def _dead_target_rows(self) -> list[str]:
        ids = {str(n) for n in self.ctx.node_ids}
        try:
            r = self._sh(["agents", "list", "--json"])
        except (ToolMissing, subprocess.SubprocessError):
            return []
        if not r.ok:
            return []
        try:
            obj = json.loads(r.stdout or "{}")
        except json.JSONDecodeError:
            return []
        out = []
        for a in obj.get("agents") or []:
            if not isinstance(a, dict):
                continue
            name = a.get("name") or ""
            if not name.startswith("target-"):
                continue
            if a.get("status") == "live":
                continue
            # target-<node>-<slug>: reap only rows for nodes this ritual closed.
            if not any(name.startswith(f"target-{nid}-") for nid in ids):
                continue
            out.append(name)
        return out

    # -- streaming leg (x-0d66) -------------------------------------------

    def _stream(self, step: str, argv: list[str], timeout: float) -> None:
        """Run a verb, echoing each stdout line as progress; bounded by timeout.

        The bound is enforced DURING the read (x-0d66), not just on ``wait()``:
        a hung verb with no output never closes its pipe, so a blocking read loop
        would wedge before ``wait`` ever ran. ``select`` with the remaining
        deadline lets us re-check the bound every tick and kill a silent hang.
        """
        import os
        import select
        import time

        try:
            proc = subprocess.Popen([*fno_py_cmd(), *argv], cwd=str(self.canon),
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        except FileNotFoundError as exc:
            self._emit(step, _FAILED, f"spawn-error: {exc}")
            return
        assert proc.stdout is not None  # PIPE was requested above
        fd = proc.stdout.fileno()
        deadline = time.monotonic() + timeout
        progress = 0
        pending = b""
        timed_out = False
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    timed_out = True
                    break
                ready, _, _ = select.select([fd], [], [], min(remaining, 0.5))
                if not ready:
                    continue  # still running, no output this tick; re-check deadline
                chunk = os.read(fd, 65536)
                if not chunk:
                    break  # EOF: process exited and pipe drained
                pending += chunk
                while b"\n" in pending:
                    raw, pending = pending.split(b"\n", 1)
                    line = raw.decode("utf-8", "replace").rstrip()
                    if line:
                        typer.echo(f"  {step}: {line}")
                        progress += 1
        finally:
            if proc.poll() is None:
                proc.kill()
            rc = proc.wait()
        if timed_out:
            self._emit(step, _FAILED,
                       f"timeout after {int(timeout)}s; {progress} progress lines")
            return
        self._emit(step, _OK if rc == 0 else _FAILED,
                   f"exit={rc}" + (f" ({progress} progress lines)" if progress else ""))

    # -- driver ------------------------------------------------------------

    def run(self) -> int:
        if not self.ctx.pr:
            self._emit("resolve", _FAILED, "no merged PR found; pass a PR number")
            return 1
        if not self.ctx.pm:
            self._emit("config", _FAILED, "post_merge config unreadable (not a repo / bad config)")
            return 1
        if not _truthy(getattr(self.ctx.pm, "enabled", True)):
            # config.post_merge.enabled is the ritual's off switch; a repo that
            # sets it false must not acquire the mutex or run any leg (codex P1).
            self._emit("config", _SKIPPED, "post_merge.enabled is false; ritual disabled")
            return 0
        from fno.config import autonomy_master_enabled

        if not autonomy_master_enabled(self.canon):
            # x-aaaf wave 3: the master panic switch outranks post_merge.enabled
            # too - checked separately so the receipt names WHICH one fired.
            self._emit("config", _SKIPPED, "autonomy.enabled is false; ritual disabled")
            return 0
        self._emit("resolve", _OK, "pr={} source={}".format(
            self.ctx.pr, "caller" if self.pr_supplied else "most-recent-merge"))
        if not self.acquire_mutex():
            return 0  # another runner owns it
        try:
            self.leg_stamp()
            self.leg_harvest()
            self.leg_advance()
            self.leg_skill_diff()
            self.leg_sync_canonical()
            self.leg_archive()
            self.leg_judgment()
            self.leg_reap_rows()
        finally:
            self.release_mutex()
        failed = [r.step for r in self.ctx.receipts if r.status == _FAILED]
        return 1 if failed else 0


def _truthy(v: object) -> bool:
    if isinstance(v, bool):
        return v
    return str(v).strip().lower() in ("true", "1", "yes", "on")


def run_ritual(pr: Optional[int], autonomous: bool, cwd: Optional[Path] = None,
               runner: Callable = _run) -> int:
    """Entry point for the `fno do pr ritual` command. Returns shell exit code."""
    ritual = Ritual(pr, autonomous, Path(cwd or Path.cwd()), runner=runner)
    return ritual.run()
