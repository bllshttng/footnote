"""fno do target CLI - discoverable bootstrap for /fno:target sessions.

Exposes:
    fno do target init --input <text|ab-id> [--plan-path <path>]

Why this exists (Change 3 of the worktree-binding plan): the canonical
bootstrap lives at ``hooks/helpers/init-target-state.sh`` - outside the
skill dir and named without a path in SKILL.md. Agents looking in the skill
dir fail to find it and substitute the discoverable-but-wrong ``fno do state
init``, which writes a stub the stop hook then archives (often in a loop).
A discoverable verb that REFUSES to write a stub closes that substitution at
the source for every CLI (Claude / Codex / Gemini), since ``fno`` is the
shared dependency.

The verb is a thin wrapper: it sets ``TARGET_START=1`` plus the input env the
init script reads (``TARGET_INPUT`` / ``TARGET_PLAN_PATH``) and execs the
canonical script, propagating its exit code. It records nothing itself - the
init script owns the owner_cwd worktree binding and all state.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

import typer

from fno.paths import resolve_plugin_script
from fno.tombstones import tombstone_group_cls

target_app = typer.Typer(
    name="target",
    help="Target session bootstrap (records input/plan_path + owner_cwd binding).",
    add_completion=False,
    cls=tombstone_group_cls("target"),
)

_INIT_RELPATH = "hooks/helpers/init-target-state.sh"


def _resolve_init_script() -> Path:
    """Locate the canonical init script from the PLUGIN root, not the cwd repo.

    The init script ships with the fno plugin, not with arbitrary user
    projects. Resolving it via ``resolve_repo_root()`` (the active project)
    means ``fno do target init`` would fail in normal plugin usage - running
    ``fno`` inside a target project where ``hooks/helpers/`` does not exist
    (Codex P1 on #337). Since this verb is the mandatory bootstrap path, it
    resolves from the plugin install first.

    Order:
      1. ``CLAUDE_PLUGIN_ROOT`` - set by Claude Code in plugin context.
      2. ``FNO_REPO_ROOT`` - explicit override / test hook.
         Both env hints are authoritative: if set, the script is expected
         there and we do NOT fall through to guessing, so a misconfigured
         root surfaces loudly via the caller's ``is_file()`` check.
      3. Package-relative: the repo root is three parents above this file
         (``cli/src/fno/target_cli.py`` -> repo root) for a
         source/editable install where ``hooks/`` sits beside ``cli/``.
      4. ``resolve_repo_root()`` - last resort (running inside fno repo).
    """
    # Delegates to the shared resolver (env hint -> package-relative ->
    # persisted ~/.fno/install/plugin-root pointer -> repo) so `fno do target init`
    # finds the script from any project without a hand-set FNO_REPO_ROOT.
    return resolve_plugin_script(_INIT_RELPATH)


def _refuse_unsatisfiable_reviewers() -> None:
    """Refuse a `config.review.reviewers` gate nothing here can satisfy.

    The gate is fail-closed, so a reviewer that cannot run in this harness and
    substrate wedges the session at the stop gate AFTER the work is done. That
    is where PR #618 lost fifteen turns. Resolving it here converts the wedge
    into a refusal before anything is built. Read-only; no state is written.

    An unreadable config degrades to a no-op so a repo that configures no
    reviewer is never blocked by this check failing - but it SAYS SO. Silence
    was the original wedge: a typo'd reviewer name raises out of the config
    validator, and swallowing that skipped the refusal entirely while the Rust
    stop gate went live on a name no attestation can ever match.

    Resolution errors are NOT swallowed. Once a reviewer is known to be
    configured, guessing "available" is the wedge.
    """
    try:
        from fno.config import load_settings

        review = load_settings().review
        reviewers = list(review.reviewers or [])
        registry = review.reviewer_registry
        peers = list(review.peers or [])
        peer_identity = review.peer_identity
    except Exception as exc:  # noqa: BLE001 - report, never block bootstrap
        # "settings", not "config.review.reviewers": load_settings validates the
        # whole model, so the fault may be in an unrelated block. Naming the key
        # we did not verify is the misattribution this PR exists to delete.
        typer.echo(
            f"WARN target init: review capability check skipped "
            f"(settings unreadable: {exc})",
            err=True,
        )
        return
    if not reviewers and not peers:
        return

    from dataclasses import asdict

    from fno.config import resolvable_reviewers
    from fno.rust_binary import verb_call

    answer = verb_call(
        ["review-level", "capability", "reviewers"],
        {
            "reviewers": reviewers,
            "descriptors": {
                name: asdict(d) for name, d in resolvable_reviewers(registry).items()
            },
            "peers": peers,
            "peer_identity": peer_identity,
        },
    )
    for row in [r for r in answer.get("reviewers", []) if r.get("status") == "unverifiable"]:
        typer.echo(f"note target init: {row['line']}", err=True)
    for row in [r for r in answer.get("local_peers", []) if r.get("status") == "unverifiable"]:
        typer.echo(f"note target init: {row['line']}", err=True)
    messages = [
        message
        for message in (
            answer.get("reviewers_refusal"),
            answer.get("local_peers_refusal"),
        )
        if message
    ]
    if messages:
        typer.echo("\n\n".join(messages), err=True)
        raise typer.Exit(code=2)


def _refuse_unreachable_github_apps() -> None:
    """Refuse a `config.review.github_apps` login that cannot review here.

    Mirror of `_refuse_unsatisfiable_reviewers` on the App-bot axis: a required
    App that footnote does not recognize AND has never acted in this repository
    is a typo or an uninstalled App, and today it wedges the session at the
    budget ceiling instead of failing loud at init. This moves that discovery to
    the front of the run, in the same vocabulary and fail-open on any probe it
    could not run. Read-only; no state is written.
    """
    try:
        from fno.config import load_settings

        review = load_settings().review
        apps = list(review.github_apps or [])
        nudge_logins = tuple((review.nudge or {}).keys())
    except Exception as exc:  # noqa: BLE001 - report, never block bootstrap
        typer.echo(
            f"WARN target init: github_apps capability check skipped "
            f"(settings unreadable: {exc})",
            err=True,
        )
        return
    if not apps:
        return

    from fno.rust_binary import verb_call

    answer = verb_call(
        ["review-level", "capability", "github-apps"],
        {"apps": apps, "nudge_logins": list(nudge_logins)},
        timeout=70,
    )
    for row in [r for r in answer.get("verdicts", []) if r.get("status") == "unverifiable"]:
        typer.echo(f"note target init: {row['line']}", err=True)
    message = answer.get("refusal")
    if message:
        typer.echo(message, err=True)
        raise typer.Exit(code=2)


_SIZE_ORDER = {"S": 0, "M": 1, "L": 2}


def _load_blast_cfg():
    """Load config.target.blast, fail-safe to a disabled default block.

    A malformed settings file (or any load error) degrades to a default
    ``BlastConfig`` (enabled=False) so a config typo can never make the blast
    read raise. Mirrors the ``agents_headless_yolo`` try/except idiom.
    """
    from fno.config import BlastConfig

    try:
        from fno.config import load_settings

        return load_settings().target.blast
    except Exception:
        return BlastConfig()


def _repo_root_or_none() -> Optional[str]:
    """Best-effort repo root for path normalization; None on any failure."""
    try:
        from fno.paths import resolve_repo_root

        return str(resolve_repo_root())
    except Exception:
        return None


_TARGET_NODE_TOKEN_RE = re.compile(r"^[a-z][a-z0-9]{0,7}-?[0-9a-f]{4,8}$", re.IGNORECASE)
_SOURCE_PR_URL_RE = re.compile(
    r"https?://github\.com/([^/\s]+)/([^/\s]+)/pull/(\d+)(?:[#?\s)]|$)",
    re.IGNORECASE,
)
_GH_PREFLIGHT_TIMEOUT_S = 30.0


def _graph_entries_or_none() -> Optional[list]:
    """One graph read a resolver and the hold gate share (round-12 finding 10).

    ``load_graph`` is not free, and init()/start() were paying it twice per
    invocation - once in the node resolver, again inside
    ``_refuse_dispatch_hold``'s own read. Returns ``None`` on any read
    failure so the CALLER decides the failure policy: the hold gates refuse
    when the input names a node (an unreadable graph must not silently skip
    the check, the fail-open the resolver used to swallow), while the
    containment check keeps its documented fail-open.
    """
    try:
        from fno.graph.load import load_graph
        from fno.paths import graph_json

        graph = load_graph(graph_json())
    except Exception:  # noqa: BLE001 - policy belongs to the caller
        return None
    if not isinstance(graph, list):
        return None
    return [entry for entry in graph if isinstance(entry, dict)]


def _input_may_name_a_node(text: Optional[str]) -> bool:
    """True when input shape can name a backlog node: canonical or compact
    prefix/hex, bare hex, or kebab slug. Spaced free text and single words
    cannot name one, so an unreadable graph stays proceed-able for them."""
    if not text:
        return False
    for tok in text.split():
        if _TARGET_NODE_TOKEN_RE.fullmatch(tok):
            return True
        if re.fullmatch(r"[0-9a-fA-F]{4,8}", tok) or re.fullmatch(
            r"[a-z0-9]+(-[a-z0-9]+)+", tok
        ):
            return True
    return False


def _refuse_unreadable_graph_hold(what: str) -> None:
    """A named dispatch against an unreadable graph refuses (round-12
    finding 10): the hold state cannot be read, and failing to read it must
    not read as unheld. Free-text inputs never reach this."""
    typer.echo(
        f"fno do target {what}: dispatch-hold-invalid: backlog graph is "
        "unreadable; refusing to assume unheld. Nothing was claimed. Fix or "
        "restore graph.json and retry.",
        err=True,
    )
    raise typer.Exit(code=2)


def _resolve_dispatch_node(
    input_: Optional[str],
    plan_path: Optional[str],
    entries: Optional[list] = None,
) -> Optional[dict]:
    """Resolve only an exact node/plan reference for the retro gate.

    Free-text target inputs return ``None`` without reading GitHub. Exact
    matching keeps ordinary target init behavior cheap and avoids title/fuzzy
    guesses at the claim boundary. ``entries`` lets a caller hand in the graph
    it already read (the hold gate reads it once and shares - round-12
    finding 10); ``None`` reads it here as before.
    """
    tokens = (input_ or "").split()
    node_tokens = {tok.lower() for tok in tokens if _TARGET_NODE_TOKEN_RE.fullmatch(tok)}
    if entries is None:
        entries = _graph_entries_or_none()
        if entries is None:
            return None

    if node_tokens:
        matches = [
            entry
            for entry in entries
            if str(entry.get("id", "")).lower() in node_tokens
        ]
        return matches[0] if len(matches) == 1 else None

    if not plan_path:
        return None
    try:
        wanted = Path(plan_path).expanduser().resolve()
    except OSError:
        return None
    matches = []
    for entry in entries:
        candidate = entry.get("plan_path")
        if not isinstance(candidate, str) or not candidate:
            continue
        try:
            if Path(candidate).expanduser().resolve() == wanted:
                matches.append(entry)
        except OSError:
            continue
    if len(matches) > 1:
        # A plan shared by a delivery unit and its contained children is the
        # LEGAL shape, not an ambiguity: exactly one of those nodes
        # owns the PR. Resolving it to None here treated the normal contained
        # graph as unresolvable, so `--plan-path <shared plan>` sailed past the
        # containment redirect and the retro dedup gate alike. Narrow to the
        # delivery unit; a genuinely ambiguous set (two non-contained holders,
        # which change 1.1 now refuses to create) still returns None.
        units = [e for e in matches if not e.get("contained_in")]
        if len(units) == 1:
            return units[0]
        if not units:
            # Every holder is contained (the owner was superseded, deleted, or
            # had its plan_path edited away). The first narrowing missed this:
            # it fell back to the len==1 rule and returned None, so a
            # `--plan-path` bootstrap of such a plan skipped the redirect
            # entirely - the exact second-PR state the guard exists to stop.
            # When they all name ONE owner the destination is unambiguous, so
            # hand back any of them and let the redirect route to that owner.
            owners = {e.get("contained_in") for e in matches}
            if len(owners) == 1:
                return matches[0]
    return matches[0] if len(matches) == 1 else None


def _redirect_if_contained(node: Optional[dict]) -> None:
    """Route a named contained node to the delivery unit that owns its PR.

    A node carrying ``contained_in`` ships inside another node's PR,
    so initializing a session on it would claim it, build it, and open a SECOND
    PR for one plan - the double-binding this invariant exists to prevent.

    A REDIRECT, not a refusal. Naming a node is consent and this does not
    overrule it; the operator asked for work that exists, and the answer is
    which node owns it. Exits before ``_retro_dispatch_preflight`` and before
    the shell bootstrap, so nothing is claimed and no manifest is written.

    This is the NAMED half of the guard. ``selection_guards`` covers autonomous
    selection only - by its own docstring, an explicitly-named node dispatches
    from any rung - so without this an operator typing the id walks straight
    past it, and the autonomous guard alone would be decorative.
    """
    if not isinstance(node, dict):
        return
    owner = node.get("contained_in")
    if not isinstance(owner, str) or not owner:
        return
    nid = node.get("id") or "that node"
    # Route on the owner's LIVE state. After the merge cascade closes the
    # children the owner is usually done, and `/fno:target <done node>` is a
    # dead end - the operator reads the redirect as broken rather than as
    # "this already shipped".
    owner_node = _find_node(owner)
    if owner_node is None:
        # The owner is gone entirely (removed before the release landed, or a
        # hand-edited graph). Routing to it is a dead end with no remedy, and
        # the reconcile heal deliberately skips a missing owner - so say what
        # to do instead of naming a node the graph does not have.
        typer.echo(
            f"fno do target init: {nid} is marked as shipping inside {owner}, but "
            f"no node {owner} exists.\n"
            f"That containment is stale and nothing will clear it automatically. "
            f"Clear it with `fno backlog update {nid} parent=null`, then "
            "dispatch this node on its own. Nothing was claimed.",
            err=True,
        )
        raise typer.Exit(code=2)
    # A dead owner will never ship, so routing there is worse than useless -
    # it sends the operator to a node nothing will build. Named separately from
    # the shipped case because the remedy differs: this one is stale
    # containment that wants clearing, not work that already landed.
    if owner_node is not None and (
        owner_node.get("superseded_by") or owner_node.get("deferred_at")
    ) and not owner_node.get("completed_at"):
        _state = "superseded" if owner_node.get("superseded_by") else "deferred"
        typer.echo(
            f"fno do target init: {nid} is marked as shipping inside {owner}, but "
            f"{owner} is {_state} and will never ship it.\n"
            f"That containment is stale. Clear it with `fno backlog update {nid} "
            "parent=null`, then dispatch this node on its own. Nothing was claimed.",
            err=True,
        )
        raise typer.Exit(code=2)
    if owner_node is not None and owner_node.get("completed_at"):
        shipped = owner_node.get("pr_number")
        where = f" in PR #{shipped}" if shipped else ""
        typer.echo(
            f"fno do target init: {nid} already shipped inside {owner}{where}, "
            f"which is done - there is nothing left to build here.\n"
            f"{nid} was folded into {owner} as a contained node, so it has no "
            "PR of its own and is not separately costed. Nothing was claimed.",
            err=True,
        )
        raise typer.Exit(code=2)
    typer.echo(
        f"fno do target init: {nid} ships inside {owner}'s PR; "
        f"run `/fno:target {owner}`.\n"
        f"{nid} was folded into {owner} as a contained node, so it has no PR of "
        "its own and is not separately costed. Nothing was claimed.",
        err=True,
    )
    raise typer.Exit(code=2)


def _refuse_dispatch_hold(node: Optional[dict], entries: Optional[list] = None) -> None:
    """Refuse a named dispatch when its plan ancestry carries a hold.

    ``entries`` is the caller's already-read graph rows (round-12 finding 10:
    init/start resolve the node and hold-check it off ONE read). ``None`` -
    only for callers with no resolution step of their own - reads here.
    """
    if not isinstance(node, dict):
        return
    from fno.graph.ladder import DispatchHoldState, dispatch_hold_verdict
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        # A hold is a footnote-graph-resident concept: under an external
        # tracker backend this repo's graph.json is not the delivery record
        # of truth, so there is no plan hold to enforce here.
        return

    if entries is None:
        from fno.graph.api import wire_rows
        from fno.paths import graph_json

        try:
            entries = wire_rows(path=graph_json())
        except Exception as exc:  # noqa: BLE001 - an unreadable hold world refuses
            typer.echo(
                f"fno do target: dispatch-hold-invalid:{node.get('id', 'unknown')}: "
                f"backlog graph is unreadable ({exc}); refusing to assume unheld.",
                err=True,
            )
            raise typer.Exit(code=2)
    by_id = {
        entry.get("id"): entry
        for entry in entries
        if isinstance(entry, dict) and entry.get("id")
    }
    verdict = dispatch_hold_verdict(node, by_id)
    if verdict is None:
        return
    hold = verdict.hold
    if hold.state is DispatchHoldState.INVALID:
        # The missing-file sub-case (round-12 finding 6) has no declaration
        # to fix - its remedy is the same repair the dangling-pointer gate
        # below names, so say that instead of pointing at a declaration that
        # does not exist.
        _missing = "missing under its existing plan root" in (hold.detail or "")
        _remedy = (
            "Repair the node with: fno backlog update <id> plan_path=<path>, "
            "or restore the plan file."
            if _missing
            else "fix or remove the declaration, then retry."
        )
        typer.echo(
            f"fno do target: {verdict.guard_reason}: {hold.detail}. "
            "The plan cannot prove that dispatch is unheld; "
            f"{_remedy} Nothing was claimed.",
            err=True,
        )
        raise typer.Exit(code=2)
    typer.echo(
        f"fno do target: dispatch held by plan {verdict.owner_id}\n"
        f"  reason: {hold.reason}\n"
        f"  set_by: {hold.set_by}\n"
        f"  release_when: {hold.release_when}\n"
        f"  review_on: {hold.review_on}\n"
        + (
            f"  exempt: {hold.exempt}\n"
            if hold.exempt
            else ""
        )
        +
        "Remove dispatch_hold only after the release condition is confirmed. "
        "Nothing was claimed.",
        err=True,
    )
    raise typer.Exit(code=2)


def _source_pr_repo(node: dict, source_pr: int) -> Optional[str]:
    """Extract the source PR repo from the retro node's canonical permalink."""
    from fno.graph._reconcile import repo_slug_from_url

    values = [node.get("source_pr_url"), node.get("pr_url"), node.get("details")]
    for value in values:
        match = _SOURCE_PR_URL_RE.search(str(value or ""))
        if match and int(match.group(3)) == source_pr:
            url = match.group(0).rstrip("#? )\t\r\n")
            return repo_slug_from_url(url)
    return None


def _evidence_region_files(node: dict) -> list[str]:
    """Read file paths from an enriched ``git:merged-region:<path>`` packet."""
    paths: list[str] = []

    def visit(value: Any) -> None:
        if isinstance(value, dict):
            items = value.get("items")
            if items is not None:
                visit(items)
            for key, child in value.items():
                if isinstance(key, str) and key.startswith("git:merged-region:"):
                    path = key.removeprefix("git:merged-region:").strip()
                    if path and path not in paths:
                        paths.append(path)
                elif key != "items":
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    for key in ("evidence", "validity_evidence", "evidence_packet"):
        visit(node.get(key))
    return paths


_RETRO_SOURCE_TIMEOUT_S = 5.0
_RETRO_REGION_WINDOW = 8
_RETRO_REGION_MAX_BYTES = 1200


def _fetch_retro_comment(
    source_pr: int, finding_hash: str, root: str, *, repo: Optional[str] = None
) -> Optional[dict]:
    """PR ``source_pr``'s inline comment whose body hash-joins ``finding_hash``,
    or ``None`` on any failure."""
    from fno.retro.dedup import content_hash

    path = (
        f"repos/{repo}/pulls/{source_pr}/comments"
        if repo
        else f"repos/:owner/:repo/pulls/{source_pr}/comments"
    )
    try:
        proc = subprocess.run(
            ["gh", "api", path, "--paginate", "--slurp"],
            capture_output=True,
            text=True,
            cwd=root,
            timeout=_RETRO_SOURCE_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        raw = json.loads(proc.stdout) if proc.stdout.strip() else []
    except (json.JSONDecodeError, ValueError):
        return None
    # --slurp wraps pages as [[page1...],[page2...]]; flatten defensively.
    flat: list[dict] = []
    if isinstance(raw, list):
        for elem in raw:
            if isinstance(elem, list):
                flat.extend(x for x in elem if isinstance(x, dict))
            elif isinstance(elem, dict):
                flat.append(elem)
    for c in flat:
        if content_hash(str(c.get("body", ""))) == finding_hash:
            return c
    return None


def _read_merged_region(root: str, path: str, line: object) -> str:
    """Bounded excerpt of ``path`` around ``line`` in the merged tree:
    ``git show HEAD:<path>``, else a read that stays inside ``root``; "" when
    neither resolves. ``path`` comes from a GitHub comment, so an absolute or
    parent-escaping path is refused before any read."""
    if not isinstance(root, str) or not isinstance(path, str):
        return ""
    norm = os.path.normpath(path)
    if os.path.isabs(norm) or norm == ".." or norm.startswith(".." + os.sep):
        return ""
    content: Optional[str] = None
    try:
        proc = subprocess.run(
            ["git", "-C", root, "show", f"HEAD:{path}"],
            capture_output=True,
            text=True,
            timeout=_RETRO_SOURCE_TIMEOUT_S,
        )
        if proc.returncode == 0:
            content = proc.stdout
    except (OSError, subprocess.SubprocessError):
        content = None
    if content is None:
        target = os.path.abspath(os.path.join(root, path))
        try:
            if os.path.commonpath([root, target]) != root or not os.path.exists(target):
                return ""
        except ValueError:
            return ""
        try:
            with open(target, encoding="utf-8", errors="replace") as fh:
                content = fh.read()
        except OSError:
            return ""
    file_lines = content.splitlines()
    if not file_lines:
        return ""
    anchor = min(line if isinstance(line, int) and line >= 1 else 1, len(file_lines))
    lo = max(0, anchor - 1 - _RETRO_REGION_WINDOW)
    hi = min(len(file_lines), anchor + _RETRO_REGION_WINDOW)
    return "\n".join(file_lines[lo:hi])[:_RETRO_REGION_MAX_BYTES]


def _fetch_dispatch_region_file(
    node: dict, source_pr: int, finding_hash: str, repo: str
) -> Optional[str]:
    """Best-effort live enrichment for nodes whose packet is not persisted."""
    root = node.get("cwd")
    if not isinstance(root, str) or not os.path.isdir(root):
        return None
    try:
        comment = _fetch_retro_comment(
            source_pr, finding_hash, root, repo=repo
        )
        if not comment:
            return None
        path = comment.get("path")
        if not isinstance(path, str) or not path:
            return None
        line = comment.get("line") or comment.get("original_line")
        if not _read_merged_region(root, path, line):
            return None
        return path
    except Exception:  # noqa: BLE001 - Tier 2 is advisory and fail-open
        return None


GhRunner = Callable[[list[str]], tuple[int, str, str]]


def _default_preflight_gh_runner(args: list[str]) -> tuple[int, str, str]:
    try:
        proc = subprocess.run(
            ["gh", *args],
            capture_output=True,
            text=True,
            check=False,
            timeout=_GH_PREFLIGHT_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return 127, "", str(exc)
    return proc.returncode, proc.stdout, proc.stderr


def _preflight_gh_json(
    args: list[str], runner: GhRunner
) -> Any:
    rc, out, err = runner(args)
    if rc != 0:
        raise RuntimeError(err.strip()[:160] or f"gh exited {rc}")
    try:
        return json.loads(out) if out.strip() else None
    except (json.JSONDecodeError, ValueError) as exc:
        raise RuntimeError(f"invalid JSON: {exc}") from exc


def _parse_github_time(value: object) -> Optional[datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)


def _force_supersede_dispatch_node(node_id: str, reason: str) -> bool:
    proc = subprocess.run(
        ["fno", "backlog", "done", node_id, "--note", reason],
        check=False,
    )
    return proc.returncode == 0


def _retro_dispatch_preflight(
    node: Optional[dict],
    *,
    beastmode: bool = False,
    unattended: bool = False,
    scan_fn: Optional[Callable[..., list]] = None,
    gh_runner: Optional[GhRunner] = None,
    supersede_fn: Optional[Callable[[str, str], bool]] = None,
) -> None:
    """Run the two-tier retro dedup probe before target init claims a node."""
    if not node:
        return

    from fno.retro.dedup import _TRAILER_RE

    if node.get("completed_at") or node.get("superseded_by"):
        return
    # The full machine trailer `land` writes, never prose that mentions it.
    trailer = _TRAILER_RE.search(str(node.get("details") or ""))
    if trailer is None:
        return
    source_pr = None if trailer.group("pr") == "None" else int(trailer.group("pr"))
    finding_hash = trailer.group("hash")
    if source_pr is None:
        typer.echo(
            "WARN target init: Tier 1 skipped (retro node has source_pr=None); proceeding.",
            err=True,
        )
        typer.echo(
            "WARN target init: Tier 2 skipped (retro node has no source PR); proceeding.",
            err=True,
        )
        return

    repo = _source_pr_repo(node, source_pr)
    if not repo:
        typer.echo(
            f"WARN target init: Tier 1 skipped (source PR #{source_pr} repo is unavailable); proceeding.",
            err=True,
        )
        typer.echo(
            "WARN target init: Tier 2 skipped (source PR repo is unavailable); proceeding.",
            err=True,
        )
        return

    from fno.retro.reconcile_findings import scan_addressed_findings

    scan = scan_fn or scan_addressed_findings
    scan_warnings: list[str] = []
    try:
        addressed = scan(
            [node], include_planned=True, warnings=scan_warnings
        )
    except Exception as exc:  # noqa: BLE001 - never close on uncertainty
        addressed = []
        scan_warnings.append(str(exc))

    if addressed:
        finding = addressed[0]
        reason = (
            f"retro finding addressed on source PR #{source_pr} ({finding.signal}); "
            "target dispatch preflight"
        )
        typer.echo(
            f"WARN target init: refusing {node.get('id')} - finding already addressed "
            f"on source PR #{source_pr} ({finding.signal}); superseding node.",
            err=True,
        )
        close = supersede_fn or _force_supersede_dispatch_node
        if not close(str(node.get("id")), reason):
            typer.echo(
                f"WARN target init: could not supersede {node.get('id')}; refusing dispatch.",
                err=True,
            )
        raise typer.Exit(code=3)
    if scan_warnings:
        typer.echo(
            f"WARN target init: Tier 1 skipped (source PR #{source_pr} state unavailable); proceeding.",
            err=True,
        )

    files = _evidence_region_files(node)
    if not files:
        live_file = _fetch_dispatch_region_file(node, source_pr, finding_hash, repo)
        if live_file:
            files = [live_file]
    if not files:
        typer.echo(
            "WARN target init: Tier 2 skipped (enriched merged-file-region unavailable); proceeding.",
            err=True,
        )
        return

    runner = gh_runner or _default_preflight_gh_runner
    try:
        source = _preflight_gh_json(
            ["pr", "view", str(source_pr), "--repo", repo, "--json", "mergedAt"],
            runner,
        )
        source_merged = _parse_github_time(
            source.get("mergedAt") if isinstance(source, dict) else None
        )
        if source_merged is None:
            raise RuntimeError("source PR has no parseable mergedAt")
        merged = _preflight_gh_json(
            [
                "pr", "list", "--repo", repo, "--state", "merged", "--limit", "100",
                "--json", "number,title,mergedAt,files",
            ],
            runner,
        )
        if not isinstance(merged, list):
            raise RuntimeError("merged PR list was not an array")
    except Exception as exc:  # noqa: BLE001 - Tier 2 is advisory
        typer.echo(
            f"WARN target init: Tier 2 skipped (sibling PR probe failed: {exc}); proceeding.",
            err=True,
        )
        return

    finding_files = {path.lstrip("./") for path in files}
    overlaps: list[tuple[int, str, list[str]]] = []
    for pr in merged:
        if not isinstance(pr, dict):
            continue
        merged_at = _parse_github_time(pr.get("mergedAt"))
        if merged_at is None or merged_at <= source_merged:
            continue
        changed = []
        for item in pr.get("files") or []:
            path = item.get("path") if isinstance(item, dict) else item
            if isinstance(path, str) and path.lstrip("./") in finding_files:
                changed.append(path)
        if changed and isinstance(pr.get("number"), int):
            overlaps.append((pr["number"], str(pr.get("title") or ""), changed))

    if not overlaps:
        return
    for number, title, changed in overlaps:
        typer.echo(
            f"WARN target init: Tier 2 sibling PR #{number} {title!r} overlaps "
            f"finding files: {', '.join(changed)}.",
            err=True,
        )

    # Tier 2 is deliberately advisory: overlap is best-effort sibling evidence,
    # never a dispatch gate or a reason to mutate the backlog node.
    typer.echo(
        "WARN target init: Tier 2 overlap is advisory; proceeding with dispatch.",
        err=True,
    )


@target_app.command("blast-check")
def blast_check(
    plan: str = typer.Argument(
        ..., help="Path to the plan whose File Ownership Map to classify"
    ),
    quiet: bool = typer.Option(
        False, "--quiet", help="Print only the bare verdict token (high|low|unknown)"
    ),
) -> None:
    """Classify a plan's touched surface as high / low / unknown blast radius.

    `fno do target blast-check` runs in the fno-agents binary now: this
    command is a thin forward and the classifier is the native
    `target_blast.rs` door, the same leg the init modulation reads.
    """
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno do target blast-check: the classifier is native, but no "
            "fno-agents binary resolved (set FNO_AGENTS_BIN or install "
            "fno-agents)",
            err=True,
        )
        raise typer.Exit(code=1)
    cmd = [str(binary), "target-blast-check", plan]
    if quiet:
        cmd.append("--quiet")
    raise typer.Exit(code=subprocess.run(cmd).returncode)


# Exit code meaning "this gate refused", distinct from every code Click itself
# can produce. Click uses 2 for UsageError, so a STALE installed `fno` without
# this verb exits 2 with "No such command" - indistinguishable from a refusal if
# the gate also spoke 2, which would turn every outdated CLI into a hard refusal
# of the mandatory bootstrap. Anything Click can emit on its own must never mean
# "refused".
REVIEW_GATE_REFUSED = 9


@target_app.command("check-review-gate", hidden=True)
def check_review_gate() -> None:
    """Run both review-capability refusals for a non-Python caller.

    Exists so `hooks/helpers/init-target-state.sh` can reach the ONE
    implementation of these verdicts from bash. SKILL.md documents running that
    script directly when `fno` is unavailable, and on that path a typo'd
    `config.review.github_apps` login used to start a full run and surface only
    at the stop gate - a guard on one of two reachable paths is decorative.

    Read-only; writes no state and takes no arguments. Exit 0 = both axes clear
    (or nothing configured); exit `REVIEW_GATE_REFUSED` = refused, message
    already on stderr. The caller treats every OTHER non-zero as a broken gate
    rather than a failed one, so the tables behind this stay Python and are
    never restated in shell.
    """
    try:
        _refuse_unsatisfiable_reviewers()
        _refuse_unreachable_github_apps()
    except typer.Exit as exc:
        # The helpers speak `init`'s vocabulary: 2 and only 2 means refused.
        # Re-stamp that one code as this verb's, so the shell caller can tell a
        # refusal from a usage error. Anything else propagates untouched - the
        # caller reads a code it does not recognize as a BROKEN gate and
        # proceeds, so widening this to "any non-zero refuses" would turn a
        # future unrelated failure into a hard bootstrap refusal.
        if exc.exit_code != 2:
            raise
        raise typer.Exit(code=REVIEW_GATE_REFUSED) from None


@target_app.command("review-invocation", hidden=True)
def review_invocation_cmd(
    harness: Optional[str] = typer.Option(
        None,
        "--harness",
        help="Harness to render for (default: the ambient session's). Callers "
        "that already resolved a harness pass it so the render matches their view.",
    ),
) -> None:
    """Print the sized self-review invocation for this session.

    Stdout is exactly one line: the invocation the worker should run, level
    sized from the branch's diff via ``diff_review_level`` (never a hardcoded
    level). The refusal surfaces a held worker reads (the stop gate's coverage
    reason, the merge guard, the CI status) embed this output; prose cannot
    carry it because ``check-review-invocation-single-source.sh`` bans any
    concrete level outside the builder. A worker or lead can also run this
    verb directly.

    Read-only; writes no state. A named door refusal (the fno-agents binary
    missing or failed) prints the refusal on stderr and exits 1: machinery
    reading this output embeds it verbatim, so a fabricated placeholder would
    lie about a gate that cannot answer."""
    from fno.rust_binary import VerbUnavailable, verb_call

    argv = ["review-level", "capability", "invocation", "--root", str(Path.cwd())]
    if harness:
        argv += ["--harness", harness]
    try:
        answer = verb_call(argv, {})
    except VerbUnavailable as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(code=1) from None
    typer.echo(answer["invocation"])


def _read_pr_metadata(pr_number: int, cwd: Path) -> dict[str, Any]:
    """Read the PR identity required before a native self-review can fire.

    The REST lane (`fetch_pr_info_rest`), never `gh pr view`: the fno gh
    proxy routes that spelling through the GraphQL broker, which refuses
    discretionary reads once the shared quota hits its reserve - exactly
    when a fleet is busiest - and the ship step would stall on a read the
    REST endpoint serves freely.
    """
    from fno.pr._rest import fetch_pr_info_rest

    info, reason = fetch_pr_info_rest(str(pr_number), cwd=str(cwd))
    if info is None:
        raise RuntimeError(
            f"cannot read PR {pr_number}: {(reason or 'REST read failed')[:240]}"
        )
    return {
        "number": info.get("pr", pr_number),
        "headRefOid": info.get("head_sha") or "",
        "headRefName": info.get("head_ref") or "",
        "baseRefName": info.get("base_ref") or "",
    }


def _resolve_self_review_identity() -> tuple[str, str]:
    """Return the owned harness and full session id for the self-send lane."""
    from fno.agents.self_stamp import resolve_self_identity

    identity = resolve_self_identity()
    harness = (identity.harness or "").strip()
    session_id = (identity.session_id or "").strip()
    if not harness or not session_id:
        raise RuntimeError("no owned harness identity is available for self-review")
    return harness, session_id


def _send_self_review_payload(
    *, payload: str, harness: str, session_id: str
) -> dict[str, Any]:
    """Type the self-review payload through the Rust raw-send leaf."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        return {
            "outcome": "refused",
            "transport": harness,
            "reason": (
                "the fno-agents binary was not found; reinstall fno or run "
                "`fno doctor update --rust`, or set FNO_AGENTS_BIN to its path"
            ),
        }
    # The FULL session id, never canonical_handle's head-8: a codex id
    # is UUIDv7, so its first eight are a 65.536-second timestamp
    # bucket that every same-minute sibling shares, and resolution
    # fails closed on the shared short exactly when a fleet is busy.
    result = subprocess.run(
        [
            str(binary),
            "mail",
            "send",
            session_id,
            payload,
            "--raw",
            "--self-ok",
            "--review-request",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        reason = (result.stderr or result.stdout).strip()
        return {"outcome": "refused", "transport": harness, "reason": reason}
    receipt = result.stdout.strip()
    if receipt.startswith("queued"):
        transport = "mux-pane"
        outcome = "queued"
    elif receipt.startswith("started"):
        transport = "mux-pane"
        outcome = "started"
    elif receipt.startswith("injected"):
        transport = "prompt-line"
        outcome = "started"
    else:
        transport = harness
        outcome = "unconfirmed"
    return {"outcome": outcome, "transport": transport}


def _self_review_refusal(
    pr_number: Optional[int], reason: str, *, head_sha: str = "", base_branch: str = ""
) -> dict[str, Any]:
    """Build one machine-readable refusal without ever entering the send lane."""
    return {
        "outcome": "refused",
        "pr": pr_number,
        "head_sha": head_sha,
        "base_branch": base_branch,
        "reason": reason,
    }


def _hold_branch_under_review(
    cwd: Path, *, head_sha: str, session_id: str, receipt: dict[str, Any], branch: str
) -> None:
    """Register that a review of ``branch`` at ``head_sha`` is RUNNING.

    The requester side, so every pipeline review is held without a reviewer
    remembering. On PR 1575 the merge fired mid-review and discarded eight
    findings, one HIGH. The hold existed and nothing took it. ``branch`` is
    what the caller resolved: the local branch on the pre-push form, the PR's
    own head ref on the post-push form - a local alias with a matching sha
    keys a hold the merge guard, which reads GitHub, never looks up. The cwd
    is never read for one : a hold on a guessed branch reports
    protection of a PR it is not protecting.

    Best-effort throughout. An unconfirmed send takes nothing. A refused
    invocation takes nothing, since it emits no attestation and nothing would
    release the hold. A lockfile failure never turns a sent review into a
    refusal. Counts no round, gates on no origin (d-0fa92eb9, d-777e7d1f).
    """
    if str(receipt.get("outcome") or "") in {"refused", "unconfirmed"}:
        return
    try:
        from fno.pr._review_hold import acquire_review_hold, review_invocation_refusal

        if not branch or branch == "HEAD":
            return
        if review_invocation_refusal(branch, head_sha, cwd=str(cwd)):
            return
        holder = f"review-session:{session_id or 'unknown'}"
        acquire_review_hold(branch, head=head_sha, holder=holder, verb="/fno:review")
    except Exception:  # noqa: BLE001 - see docstring
        pass


@target_app.command("request-self-review", hidden=True)
def request_self_review_cmd(
    pr_number: Optional[int] = typer.Option(
        None,
        "--pr-number",
        "--pr",
        help="PR number to review (the post-push form). Omit to review the "
        "local branch and HEAD before any push.",
    ),
) -> None:
    """Request one explicit final-head review through the raw self lane."""
    from fno.rust_binary import VerbUnavailable, verb_call

    cwd = Path.cwd()
    head_sha = _git_out(cwd, "rev-parse", "HEAD") or ""
    base_branch = ""
    branch = ""
    hold_branch = ""
    metadata: dict[str, Any] = {}
    try:
        if not head_sha:
            raise RuntimeError("cannot resolve local HEAD")
        if pr_number is not None and pr_number <= 0:
            raise RuntimeError("PR number must be positive")
        if pr_number is None:
            # The default pre-push form: pin the LOCAL branch and HEAD against
            # the origin base, so the round runs before any push and the PR
            # opens already reviewed. The base resolves the same way the emit
            # script resolves it, so the review and its attestation name one
            # base; there is deliberately no gh call here - no PR exists, and
            # a network call would fail a local request when GitHub is slow.
            branch = (_git_out(cwd, "rev-parse", "--abbrev-ref", "HEAD") or "").strip()
            if not branch or branch == "HEAD":
                raise RuntimeError(
                    "no branch to review; check out the feature branch"
                )
            hold_branch = branch
            base_branch = (
                _git_out(cwd, "symbolic-ref", "--short", "refs/remotes/origin/HEAD")
                or ""
            ).strip()
            if base_branch.startswith("origin/"):
                base_branch = base_branch[len("origin/") :]
            if not base_branch:
                base_branch = "main"
        else:
            metadata = _read_pr_metadata(pr_number, cwd)
            pr_head = str(metadata.get("headRefOid") or "").strip()
            base_branch = str(metadata.get("baseRefName") or "").strip()
            # The hold keys on the PR's OWN head ref, since a local alias with
            # a matching sha keys one the merge guard never looks up. Apart
            # from `branch`, the payload target the renderer refuses beside a
            # pr_number.
            hold_branch = str(metadata.get("headRefName") or "").strip()
            if not pr_head:
                raise RuntimeError("PR has no headRefOid")
            if head_sha.lower() != pr_head.lower():
                raise RuntimeError(
                    f"local HEAD {head_sha} does not match PR head {pr_head}; refusing review"
                )
            if not base_branch:
                raise RuntimeError("PR baseRefName is unresolved; refusing review")
        harness, session_id = _resolve_self_review_identity()
        argv = ["review-level", "capability", "invocation", "--root", str(cwd)]
        if harness:
            argv += ["--harness", harness]
        if pr_number is not None:
            argv += ["--pr-number", str(pr_number)]
        if branch:
            argv += ["--branch", branch]
        argv += ["--head-sha", head_sha, "--base-branch", base_branch]
        try:
            payload = verb_call(argv, {})["invocation"]
        except VerbUnavailable as exc:
            # A named refusal, never a fabricated string: the receipt keeps
            # the door's full detail under a prefix that names the surface.
            raise RuntimeError(f"review invocation door refused: {exc}") from None
        receipt = _send_self_review_payload(
            payload=payload, harness=harness, session_id=session_id
        )
        _hold_branch_under_review(
            cwd, head_sha=head_sha, session_id=session_id, receipt=receipt, branch=hold_branch
        )
    except (RuntimeError, ValueError) as exc:
        receipt = _self_review_refusal(
            pr_number,
            str(exc),
            head_sha=head_sha,
            base_branch=base_branch,
        )
    else:
        receipt = {
            **receipt,
            "pr": pr_number,
            "branch": branch or None,
            "head_sha": head_sha,
            "base_branch": base_branch,
            "payload": payload,
        }

    typer.echo(json.dumps(receipt, sort_keys=True))
    if receipt.get("outcome") in {"refused", "unconfirmed"}:
        raise typer.Exit(code=2)


@target_app.command("resolve-owned-identity", hidden=True)
def resolve_owned_identity_cmd() -> None:
    """Resolve the harness identity this process can PROVE it owns.

    The init hook has no Python and must stamp ``harness``/``harness_session_id``
    on the manifest; precedence alone launders an inherited marker into
    ownership, so it calls this verb instead. Returns the proven (or
    collision-surviving) identity plus any collision detected, as ``KEY=value``
    lines on stdout; the caller treats a stale ``fno`` without this verb as
    'unavailable' rather than bricking init.

    Resolution routes through :func:`fno.claims.self_identity.resolve_self_identity`,
    the one owned-identity implementation every caller shares.

    Read-only; writes no state. Always exits 0 - it is a resolver, not a gate.
    """
    from fno.agents.registry import row_owning_session_id
    from fno.claims.self_identity import resolve_self_identity

    # The hook's claude session id may arrive as TARGET_TRANSCRIPT_ID (a footnote
    # hook input) when CLAUDE_CODE_SESSION_ID is not in the env; honor it as the
    # claude marker so a claude hook that exposes its id only through hook input
    # can still prove and stamp its identity.
    env = dict(os.environ)
    if not (env.get("CLAUDE_CODE_SESSION_ID") or "").strip() and (
        env.get("TARGET_TRANSCRIPT_ID") or ""
    ).strip():
        env["CLAUDE_CODE_SESSION_ID"] = env["TARGET_TRANSCRIPT_ID"]

    def _collide(harness: str, sid: str, own_pair: Optional[tuple[str, str]]) -> Optional[str]:
        # own_pair arrives from claims.self_identity (None when it could not);
        # the registry applies the agreement check, so this site never answers
        # the own-row question itself.
        from fno.claims.session_pid import resolve_session_harness

        return row_owning_session_id(
            sid, self_binding=own_pair, walk_harness=resolve_session_harness()
        )

    # Same injection seam self_stamp uses.
    from fno.agents.self_stamp import runtime_identity_witness

    owned = resolve_self_identity(env, collide=_collide, witness=runtime_identity_witness)
    # AC5-CON: record any non-trivial resolution (a refused collision or a
    # non-single disposition) so a future leak is reconstructable from the event
    # log alone. A single-family resolve can still carry a refused collision, so
    # emit on rejected too.
    if owned.disposition in {"proven", "ambiguous", "spawn_record"} or owned.rejected:
        from fno.agents.events import emit_identity_resolution

        emit_identity_resolution(owned)
    rejected = owned.rejected[0] if owned.rejected else {}
    typer.echo(f"HARNESS={owned.harness or ''}")
    typer.echo(f"SESSION_ID={owned.session_id or ''}")
    typer.echo(f"DISPOSITION={owned.disposition}")
    typer.echo(f"COLLISION={rejected.get('owner', '')}")
    typer.echo(f"COLLISION_ID={rejected.get('session_id', '')}")
    typer.echo(f"COLLISION_MARKER={rejected.get('marker', '')}")


@target_app.command("check-contained", hidden=True)
def check_contained() -> None:
    """Refuse a contained node for a non-Python caller (task 1.3b).

    The bash twin of the redirect ``init`` performs in-process. SKILL.md
    documents running ``hooks/helpers/init-target-state.sh`` directly when the
    wrapper is unavailable, and that script acquires the node claim and writes
    the manifest - so without this, the documented fallback bootstraps a
    contained node and opens the second PR for one plan that this whole
    invariant exists to prevent. A guard on one of two reachable paths is
    decorative; the same reasoning as ``check-review-gate`` above.

    Reads the node from ``TARGET_INPUT`` / ``TARGET_PLAN_PATH``, which is how
    the script already receives its input, so the shell caller passes nothing
    and cannot pass the wrong thing.

    Read-only; writes no state. Exit 0 = not contained, not resolvable, or
    nothing to check; exit ``REVIEW_GATE_REFUSED`` = redirected, message already
    on stderr. Every other non-zero means a BROKEN gate, and the caller
    proceeds - a stale `fno` without this verb exits 2 as a Click usage error
    and must not hard-refuse every direct bootstrap.
    """
    # Read THROUGH THE STORE KEEPER (formerly: under the graph flock, codex
    # P1). The flock totalized this read against decompose's write because a
    # file read could land between decompose's read and its atomic replace.
    # With the ported store that window no longer exists: decompose's commit
    # is a keeper transaction and every read is served under the keeper's
    # gate, so a reader sees either the pre-commit or the post-commit graph,
    # never a torn one. No lock to take, and both orderings stay safe:
    #
    #   read first  -> we see no containment and proceed; decompose's begin
    #                  then sees our now-live claim, and refuses.
    #   commit first -> we read the committed containment and refuse.
    from fno.graph.store import GraphLockTimeout, StoreUnavailable

    try:
        node = _resolve_dispatch_node(
            os.environ.get("TARGET_INPUT"), os.environ.get("TARGET_PLAN_PATH")
        )
    except (StoreUnavailable, GraphLockTimeout, OSError) as _store_exc:
        # Say so. The keeper gate is what makes this check unraceable, so
        # losing it silently while still exiting 0 lets every caller read the
        # result as "checked, and it is fine" - the same swallow this gate has
        # already been fixed for twice. Still fail-open: an unreachable store
        # must not block every bootstrap, and the pre-claim check plus
        # decompose's own live-claim refusal both still stand.
        typer.echo(
            f"fno do target check-contained: the graph store is unavailable "
            f"({_store_exc}); the containment check DID NOT RUN. Proceeding.",
            err=True,
        )
        return
    try:
        _redirect_if_contained(node)
    except typer.Exit as exc:
        if exc.exit_code != 2:
            raise
        raise typer.Exit(code=REVIEW_GATE_REFUSED) from None


@target_app.command("check-dispatch-hold", hidden=True)
def check_dispatch_hold() -> None:
    """Fail-closed hold check for the direct shell bootstrap path."""
    _input = os.environ.get("TARGET_INPUT")
    _plan = os.environ.get("TARGET_PLAN_PATH")
    try:
        entries = _graph_entries_or_none()
        if entries is None and (_input_may_name_a_node(_input) or _plan):
            _refuse_unreadable_graph_hold("check-dispatch-hold")
        node = _resolve_dispatch_node(_input, _plan, entries=entries or [])
        _refuse_dispatch_hold(node, entries=entries)
    except typer.Exit as exc:
        if exc.exit_code != 2:
            raise
        raise typer.Exit(code=REVIEW_GATE_REFUSED) from None


@target_app.command("denominator-ratio", hidden=True)
def denominator_ratio(
    since_days: int = typer.Option(
        28, "--since-days", help="Events lookback window in days."
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the ratio as JSON."),
) -> None:
    """The deliverables-1 ratio : is the cheap --deliverables exit a bypass?

    Reads ``target_denominator`` events. Of declared-denominator inits (plan or
    deliverables), the fraction that used ``--deliverables 1``: past roughly 80
    percent the exit is reflexive and ``enumerated_scope`` needs widening. A
    stamped 1 still beats an absent denominator, so this measures drift, not
    correctness.
    """
    import json as _json
    from datetime import datetime, timedelta

    from fno.paths import resolve_repo_root
    from fno.scoreboard.fold import read_jsonl_events

    events = read_jsonl_events(
        [resolve_repo_root() / ".fno" / "events.jsonl"], {"target_denominator"}
    )
    cutoff = datetime.now() - timedelta(days=since_days)

    def _ts(e):
        raw = e.get("ts")
        # naive-local compare; events are written naive-local (fold._parse_ts).
        try:
            return datetime.fromisoformat(str(raw).replace("Z", ""))
        except (ValueError, TypeError):
            return None

    counts = {"plan": 0, "deliverables": 0, "derived": 0, "none": 0}
    ones = 0
    for e in events:
        if _ts(e) is None or _ts(e) < cutoff:
            continue
        raw = e.get("data")
        data = raw if isinstance(raw, dict) else {}
        d = data.get("denominator")
        if d in counts:
            counts[d] += 1
        if d == "deliverables" and data.get("count") == 1:
            ones += 1

    declared = counts["plan"] + counts["deliverables"]
    ratio = round(100 * ones / declared, 1) if declared else None
    verdict = (
        "no declared-denominator inits in window"
        if declared == 0
        else ("bypass: deliverables:1 dominates; widen enumerated_scope" if (ratio or 0) >= 80 else "healthy")
    )
    out = {
        "deliverables_1_ratio_pct": ratio,
        "deliverables_1": ones,
        "plan_backed": counts["plan"],
        "deliverables_declared": counts["deliverables"],
        "deliverables_derived": counts["derived"],
        "none": counts["none"],
        "since_days": since_days,
        "verdict": verdict,
    }
    if json_out:
        typer.echo(_json.dumps(out, indent=2))
        return
    typer.echo(
        f"deliverables:1 ratio: {ratio if ratio is not None else 'n/a'}%  "
        f"({ones} of {declared} declared; plan={counts['plan']}, "
        f"deliverables={counts['deliverables']}, derived={counts['derived']}, "
        f"none={counts['none']})  {verdict}"
    )


@target_app.command("status")
def status(
    node: Optional[str] = typer.Argument(
        None, help="Node id to orient on (default: read from the session manifest)."
    ),
    plan_path: Optional[str] = typer.Option(
        None, "--plan-path", help="Plan to reconcile (default: manifest plan_path)."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the report as a JSON object."
    ),
) -> None:
    """Resolved orientation report: node, attended, worktree, tests, plan, done-when.

    Strictly read-only -- never mutates the graph, manifest, or a claim. Each
    line resolves independently; an unresolvable line prints `unknown` plus the
    one command that resolves it. Re-run bare after compaction to re-orient.
    """
    from fno.target.orient import load_orientation, render

    try:
        from fno.paths import resolve_repo_root

        root = resolve_repo_root()
    except Exception:  # noqa: BLE001 - degrade to cwd when not in a repo
        root = Path.cwd()
    lines = load_orientation(root, node_id=node, plan_path=plan_path)
    if json_output:
        typer.echo(json.dumps({ln.label: ln.value for ln in lines}, indent=2))
    else:
        typer.echo(render(lines))


def _resolve_node_id(node: str, entries: Optional[list] = None) -> str:
    """Resolve a slug / bare-hex input to its canonical backlog node id.

    ``fno do target init`` only derives the node id (and thus the node claim) from
    an exact graph id or ``--plan-path``; a documented slug forwarded raw would
    write ``graph_node_id: null`` and skip the claim, so another worker could
    grab the same card (codex PR #114). Resolving here means a slug input still
    claims its node. A non-match (free-text feature input) returns the arg
    unchanged - init accepts text - so this only ever upgrades a resolvable id,
    never blocks one. Best-effort: any load/resolve error falls through to raw.
    ``entries`` lets a caller hand in the graph rows it already read, so a
    verb that resolves AND gates off the graph pays one read, not two
    (round-12 finding 10); ``None`` reads it here as before.
    """
    try:
        if entries is None:
            from fno.graph.load import load_graph
            from fno.paths import graph_json

            data = load_graph(graph_json())
            entries = data if isinstance(data, list) else []
        from fno.graph.fuzzy import resolve_node

        match = resolve_node(node, entries)
        node_id = getattr(match, "id", None)
        if getattr(match, "kind", "none") == "exact" and node_id:
            return node_id
    except Exception:  # noqa: BLE001 - best-effort; the raw arg still works
        pass
    return node


def _find_node(node_id: str) -> Optional[dict]:
    """The graph node dict for an exact id, or None (best-effort, never raises)."""
    try:
        from fno.graph.store import _readback_row
        from fno.paths import graph_json

        row = _readback_row(graph_json(), node_id)[0]
        return row if row is not None and not row.get("archived_at") else None
    except Exception:  # noqa: BLE001 - best-effort; caller degrades to default
        return None


def _resolve_node_model(
    node_id: str,
    *,
    explicit: Optional[str] = None,
    provider: Optional[str] = None,
    include_difficulty: bool = False,
) -> tuple[Optional[str], str]:
    """``(model, decision_source)`` for a node's ``model`` pin.

    The single Python projection of ``route_resolve`` at the ``target start`` seam
    (the same precedence ``advance.py`` uses), so pin resolution lives in exactly
    ONE place. An explicit ``-m`` wins without loading the node. ``provider`` scopes
    resolution to the spawn harness so a pick never crosses harnesses;
    None defaults to ``claude`` (the bg spawn default -- a bg worker is
    always claude regardless of the invoking harness, so scoping by the ambient
    harness would mis-resolve; Locked 3 intent is the incident bg-default lane).
    ``model`` is None -> the spawn path uses the provider default. Strictly
    non-fatal: any error degrades to the explicit value or the provider default,
    so a dispatch never fails because of the routing layer (inherited Locked 10).

    ``difficulty`` is deliberately NOT resolved here by default: advance's
    dispatch seams defer it to the capacity grid, and a static pin at start
    would suppress the same grid for a bare ``target start``. A lane whose
    spawn argv pins ``--harness`` stands that grid down and has no receiving
    end - the bash dispatch lane (``resolve-model``) - so it passes
    ``include_difficulty=True`` and resolves the band itself (the resolution
    ``model_tier`` gave it before the field retired).
    """
    try:
        from fno import route_resolve

        node = None if explicit else _find_node(node_id)
        model, source, _chain = route_resolve.resolve_dispatch_model(
            explicit=explicit,
            task_model=(node or {}).get("model"),
            task_difficulty=(
                (node or {}).get("difficulty") if include_difficulty else None
            ),
            provider=provider or "claude",
        )
        return model, source
    except Exception:  # noqa: BLE001 - routing degrades, never blocks a dispatch
        return explicit, "explicit" if explicit else "provider-default"


def _model_reachable_by(model: str, provider: str) -> bool:
    """True if ``model`` maps to the ``provider`` harness (per the benchmark map).

    A tier resolves the cheapest model clearing its floor across ALL harnesses, so
    it can pick a model mapped to a different provider than the spawn lane uses.
    Best-effort: an unknown model or any lookup error is treated as reachable so
    the guard only ever DROPS a confirmed cross-harness pick, never a valid one.
    """
    try:
        from fno.adapters.providers import benchmarks as bm

        reach = bm.reachable(model)
        return reach is None or reach[0] == provider
    except Exception:  # noqa: BLE001 - never block a dispatch on a lookup error
        return True


@target_app.command("resolve-model")
def resolve_model(
    node: str = typer.Argument(..., help="Backlog node id/slug."),
    harness: Optional[str] = typer.Option(
        None,
        "--harness",
        help="Only print the model if it is reachable by this harness. A tier "
        "can resolve to a model mapped to another harness (e.g. a codex gpt-*); "
        "a spawn lane whose harness is fixed (bg: claude or opencode) passes "
        "its harness here so a cross-harness pick degrades to the harness "
        "default instead of an invalid `<harness> --model <foreign-model>` "
        "spawn.",
    ),
    _provider_tombstone: Optional[str] = typer.Option(
        None, "--provider", hidden=True,
        help="Retired: the harness axis is --harness.",
    ),
) -> None:
    """Print the dispatch model a node resolves to (its ``model`` pin).

    The one Python projection of ``route_resolve`` for bash dispatchers
    (``dispatch-node.sh``), so a pinned node's worker spawns on the pin
    without bash ever reimplementing resolution. Prints the resolved model on
    stdout, or nothing when the node has no pin (the caller uses the harness
    default). Never fails a dispatch: any error prints nothing.
    """
    from fno._flag_aliases import refuse_retired_provider

    refuse_retired_provider(_provider_tombstone)

    # include_difficulty: the bash dispatch lane pins --harness in its spawn
    # argv, which stands the compose's grid down - there is no grid
    # receiving end here, so the band resolves statically (the resolution
    # model_tier gave this lane before the field retired).
    model, _source = _resolve_node_model(
        _resolve_node_id(node), provider=harness, include_difficulty=True
    )
    if model and harness and not _model_reachable_by(model, harness):
        return  # a pin resolves unfiltered; the guard drops a cross-harness pin
    if model:
        typer.echo(model)


def _git_out(cwd: Path, *args: str) -> Optional[str]:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args], capture_output=True, text=True
    )
    if proc.returncode != 0 or not proc.stdout.strip():
        return None
    return proc.stdout.strip()


