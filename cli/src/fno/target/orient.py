"""``fno target status`` -- resolved orientation report (x-a7be, change A).

A cold or compacted agent reconstructs its situation -- node lifecycle,
attended state, worktree path, repo test command, plan delta, done-condition --
from scattered ``fno`` / ``git`` / ``gh`` calls plus per-agent memory, the layer
that does not cross to OSS users or weaker models. This builds that situation
ONCE as a resolved fact block.

Contract (the invariants the report must keep):
  * Strictly READ-ONLY. Never mutates the graph, the manifest, or a claim.
  * Each line resolves INDEPENDENTLY. An unresolvable line prints ``unknown``
    plus the single command that resolves it -- never a stack trace, never an
    abort. A degraded ``gh``/``git``/graph never blocks the whole report.

This is the introspection family (``fno whoami`` / ``fno status``), reusing
``load_agent_context`` for the manifest read rather than a parallel surface.
"""
from __future__ import annotations

import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional

from fno.plan.reconcile import reconcile_plan


@dataclass(frozen=True)
class OrientLine:
    label: str
    value: str


# --- git helpers (self-contained so orient never imports target_cli; that
#     module imports orient for the `status` command + init print) -----------

def _git_out(cwd: Path, *args: str) -> Optional[str]:
    try:
        proc = subprocess.run(
            ["git", "-C", str(cwd), *args], capture_output=True, text=True
        )
    except (OSError, ValueError):
        return None
    if proc.returncode != 0 or not proc.stdout.strip():
        return None
    return proc.stdout.strip()


def _is_linked_worktree(cwd: Path) -> bool:
    """True if ``cwd`` is inside a git LINKED worktree (git-dir != common-dir).

    Mirrors target_cli's location verdict in pure git terms: a linked worktree
    means we are already isolated.
    """
    gdir = _git_out(cwd, "rev-parse", "--git-dir")
    common = _git_out(cwd, "rev-parse", "--git-common-dir")
    if not gdir or not common:
        return False

    def _abs(p: str) -> Path:
        path = Path(p)
        return (path if path.is_absolute() else cwd / path).resolve()

    return _abs(gdir) != _abs(common)


# --- per-line resolvers (each fail-safe to `unknown ... | resolve: <cmd>`) ---

def _graph_entry(node_id: str, project_root: Path) -> Optional[Dict[str, Any]]:
    """The graph entry for ``node_id``, or None when absent. Raises on a real
    graph load error so the node line can degrade distinctly (not-in-graph vs
    unreadable)."""
    from fno.graph.load import load_graph
    from fno.paths import graph_json

    data = load_graph(graph_json())
    entries = data if isinstance(data, list) else []
    low = node_id.lower()
    for e in entries:
        if isinstance(e, dict) and str(e.get("id", "")).lower() == low:
            return e
    return None


def _node_line(
    node_id: Optional[str],
    project_root: Path,
    manifest_raw: Optional[Dict[str, Any]] = None,
) -> str:
    if not node_id:
        return "fresh (no node bound)"
    resolve = f"resolve: fno backlog get {node_id}"
    try:
        entry = _graph_entry(node_id, project_root)
    except Exception as exc:  # noqa: BLE001 - degrade, never abort the report
        return f"unknown (graph unreadable: {exc}) | {resolve}"
    if entry is None:
        return f"unknown (not in graph) | {resolve}"
    status = str(entry.get("status") or "").strip()
    pr = entry.get("pr_number")
    # `done` is terminal FIRST, before any PR-metadata branch: an advisory /
    # no-ship / manually-completed node is `done` without a PR, and must not
    # fall through to claim/fresh and misorient a resumed agent toward rework.
    if status == "done":
        if not pr:
            return "done (no PR)"
        # Only `merge_status` evidences a merge. Deriving "merged" from
        # done + pr_number asserted a merge nothing had checked, so a node
        # closed early read as shipped while the PR was still open (x-47a3).
        if entry.get("merge_status") == "merged":
            return f"shipped (PR #{pr} merged)"
        return f"shipped (PR #{pr}, awaiting merge)"
    if pr:
        return f"half-done (PR #{pr})"
    # In-progress: the current manifest itself holds this node's claim. (A
    # foreign worker's claim is not reliably a file on disk -- `fno target init`
    # already refused the loser, so graph status orients them; this surfaces the
    # holder we DO know.)
    raw = manifest_raw or {}
    if str(raw.get("target_claim_key") or "") == f"node:{node_id}":
        holder = str(raw.get("target_claim_holder") or "this session")
        return f"in-progress (claim: {holder})"
    if status == "blocked":
        return "blocked (open dependency)"
    return f"fresh ({status or 'ready'})"


# --- live-manifest predicate (x-4af4) ---------------------------------------
#
# ONE liveness truth, two consumers: `_attended_line` (so a DEAD manifest reads
# attended, restoring /think's question flow) and the session-start GC hook
# (which shells `fno target status --json` and archives a DEAD manifest). The
# hook must NOT re-implement pid/claim logic in bash -- it reads `manifest-live`.


def _pid_alive(pid_val: Any) -> bool:
    """Best-effort: is ``pid_val`` a running process on THIS host?

    Biased toward LIVE on any uncertainty: a false-live costs one autonomous
    /think, a false-dead would archive a still-running session's manifest.
    """
    try:
        pid = int(str(pid_val).strip())
    except (TypeError, ValueError):
        return False
    if pid <= 0:
        return False
    try:
        import psutil

        return bool(psutil.pid_exists(pid))
    except Exception:  # noqa: BLE001 - psutil missing/erroring -> os.kill fallback
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except (PermissionError, OSError):
            return True  # exists-but-not-ours / uncertain -> biased live


def _claim_state(claim_key: str) -> Optional[str]:
    """The claim lockfile state for ``claim_key`` (free|live|suspect|stale|
    corrupted), or None on a read error. None means "claim signal unavailable"
    -- the caller must NOT treat it as confirmed-dead."""
    try:
        from fno.claims.core import claim_status
        from fno.claims.io import claims_root_for

        # node:/dispatch:/... keys live at the GLOBAL claims root, not the
        # per-repo default; route there (the same helper `fno claim status` uses)
        # or a node claim always reads `free` from a worktree checkout.
        state = claim_status(claim_key, root=claims_root_for(claim_key)).get("state")
        return str(state or "") or None
    except Exception:  # noqa: BLE001 - unreadable claim -> None (not confirmed dead)
        return None


def _manifest_liveness(manifest_raw: Optional[Dict[str, Any]]) -> tuple[str, str]:
    """``(state, reason)`` where state is ``live`` | ``dead`` | ``none``.

    The node claim is the ONLY durable liveness signal (x-ba4b: session-pid
    anchored + TTL-protected). ``owner_pid`` is the TRANSIENT ``fno target init``
    wrapper pid (init-target-state.sh:525) that dies seconds after init, so it can
    only ever PROVE life (a live pid), never death. DEAD is asserted solely from a
    claim confirmed absent/expired:

      * claim held (live/suspect)          -> LIVE
      * claim absent/expired (free/stale)  -> DEAD (the durable anchor is gone)
      * claim unreadable (corrupted/error) -> LIVE (cannot confirm death)
      * NO claim key -> LIVE unless owner_pid still proves life

    The no-claim-key bias is load-bearing: a live NON-node target (free-text or a
    plan input writes graph_node_id:null and no claim) has a dead transient
    owner_pid post-init, so concluding DEAD from owner_pid there would archive a
    running session and flip /think to attended mid-run. With no durable death
    signal we bias LIVE (a false-live costs one autonomous /think; a false-dead
    archives a live session).
    """
    raw = manifest_raw or {}
    if not raw:
        return "none", "no manifest"

    claim_key = str(raw.get("target_claim_key") or "").strip()
    if claim_key:
        state = _claim_state(claim_key)
        if state in {"live", "suspect"}:
            return "live", f"claim {claim_key} {state}"
        if state in {"free", "stale"}:
            return "dead", f"claim {claim_key} {state}"
        # corrupted / unreadable -> claim signal unavailable, cannot confirm death
        return "live", f"claim {claim_key} unreadable (biased live)"

    # No recorded claim key: owner_pid can only PROVE life (it is transient, so a
    # dead/absent one is not proof of death - could be a live non-node target).
    if _pid_alive(raw.get("owner_pid")):
        return "live", "owner_pid alive"
    return "live", "no claim key; owner_pid transient (biased live)"


def _authority_granted(raw: Optional[Dict[str, Any]]) -> bool:
    """Authority fails CLOSED: it requires a LIVE CLAIM, and nothing else.

    Two properties have to hold at once, and only a claim delivers both. The
    grant must be live now, and it must stay readable after this process exits.
    ``owner_pid`` gives the first without the second: it is alive for every
    session at init time, claimless ones included, so a pid-based check reads
    granted at init and then silently evaporates minutes later - the operator
    walks away believing they have a grant they no longer hold.

    ``_manifest_liveness``'s bias toward live is right for ``attended`` (worst
    case you get asked) and wrong here, where a stale grant silently un-prompts
    every session that reads it (x-4af4: a defunct manifest once auto-locked an
    attended /think for ten days). So: no claim, no authority - which is also
    why a free-text run cannot hold one.
    """
    if not raw or str(raw.get("authority", "")).strip().lower() != "full":
        return False
    claim_key = str(raw.get("target_claim_key") or "").strip()
    return bool(claim_key) and _claim_state(claim_key) in {"live", "suspect"}


def _attended_line(manifest_raw: Optional[Dict[str, Any]]) -> str:
    state, reason = _manifest_liveness(manifest_raw)
    # A DEAD manifest (x-4af4) means the owning session is gone -- resolve to
    # ATTENDED regardless of the stale stamped value, and NAME it so the posture
    # is not silently changed (the original bug was a silent autonomous switch).
    # This branch also denies a dead manifest's authority grant.
    if state == "dead":
        return f"true (dead manifest: {reason}; attended)"
    if manifest_raw and "attended" in manifest_raw:
        val = str(manifest_raw["attended"]).strip().lower()
        line = f"{val} (manifest, live: {reason})"
        if _authority_granted(manifest_raw):
            line += "; authority: full (beastmode)"
        return line
    # No manifest yet: resolve from the substrate, mirroring init-target-state.sh
    # and the spawn_think precedent -- FNO_AGENT_SELF (injected into EVERY spawned
    # worker) is the reliable "not an operator at the keyboard" signal.
    if (
        os.environ.get("FNO_AGENT_SELF")
        or os.environ.get("FNO_BG")
        or os.environ.get("TARGET_UNATTENDED") == "1"
    ):
        return "false (substrate: spawned/bg worker)"
    return "true (substrate: operator session)"


def _manifest_live_line(manifest_raw: Optional[Dict[str, Any]]) -> str:
    """The machine-read liveness field the session-start GC keys on. A ``dead``
    value carries the archive command (the module's "unknown line names its one
    resolving command" idiom)."""
    state, reason = _manifest_liveness(manifest_raw)
    if state == "dead":
        return (
            f"dead ({reason}) | archive: "
            "fno state archive --path .fno/target-state.md --type target"
        )
    if state == "none":
        return "none (no manifest)"
    return f"live ({reason})"


def _worktree_line(project_root: Path, node_id: Optional[str]) -> str:
    try:
        if _is_linked_worktree(project_root):
            return str(project_root)
    except Exception as exc:  # noqa: BLE001
        return f"unknown (git error: {exc}) | resolve: git rev-parse --git-dir"
    hint = node_id or "<node>"
    return f"on canonical main -- create with: fno target start {hint}"


def _tests_line(project_root: Path) -> str:
    """The repo's test command(s), detected from project markers."""
    cmds: List[str] = []
    if (project_root / "pyproject.toml").exists() or (
        project_root / "cli" / "pyproject.toml"
    ).exists():
        cmds.append("pytest")
    if (project_root / "Cargo.toml").exists() or (
        project_root / "crates"
    ).is_dir():
        cmds.append("cargo test")
    if (project_root / "package.json").exists():
        cmds.append("npm test")
    if not cmds:
        return "unknown | resolve: set your repo's test command"
    return " | ".join(cmds)


def _required_bots(project_root: Path) -> List[str]:
    """The must-have-reviewed login list: None/[] -> no gate (cv-6537099f).

    Reads config.review.github_apps (the legacy required_bots aliases it). The
    effective default matches the Rust loop-check: absent == [] == no review
    gate (PR + CI only), not the old ["chatgpt-codex-connector"].
    """
    from fno.config import load_settings_for_repo

    bots = load_settings_for_repo(project_root).review.github_apps
    return list(bots) if bots else []


def _optional_bots(project_root: Path) -> List[str]:
    """Honored-if-present reviewer logins (config.review.optional_apps)."""
    from fno.config import load_settings_for_repo

    return list(load_settings_for_repo(project_root).review.optional_apps)


def _done_when_line(manifest_raw: Optional[Dict[str, Any]], project_root: Path) -> str:
    raw = manifest_raw or {}

    # `or ""` would collapse a YAML-parsed bool False to "" -- read the value
    # straight so `attended: false` / `no_ship: false` are detected correctly.
    def _is(key: str, want: str) -> bool:
        return str(raw.get(key)).strip().lower() == want

    if _is("no_ship", "true") or _is("advisory", "true"):
        return "advisory: written + eval-green (no PR)"
    try:
        bots = _required_bots(project_root)
        optional = _optional_bots(project_root)
    except Exception:  # noqa: BLE001 - degrade to the no-gate default
        bots, optional = [], []
    bots_str = ", ".join(bots) if bots else "none (PR + CI only)"
    line = f"PR + CI green + reviewed by [{bots_str}]"
    if optional:
        line += f" (optional if present: [{', '.join(optional)}])"
    if _is("attended", "false"):
        line += "; bg -> hand off the merge"
    return line


def _plan_line(plan_path: Optional[str], project_root: Path) -> str:
    if not plan_path:
        return "none (no plan bound)"
    return reconcile_plan(plan_path, project_root).summary()


def _render_boundary(verdicts: list) -> str:
    """Collapse per-blocker verdicts to one line. STALE > unknown > reconciled >
    fresh -- a single stale blocker is the actionable signal Step 0 keys on."""
    if not verdicts:
        # empty covers both "no blockers" and "blockers all skipped as not-stale"
        return "fresh (no landed blocker to reconcile)"
    stale = [v for v in verdicts if v.verdict == "stale"]
    unknown = [v for v in verdicts if v.verdict == "unknown"]
    reconciled = [v for v in verdicts if v.verdict == "reconciled"]
    if stale:
        clauses = [
            f"{v.blocker_id} ("
            f"{('PR #' + str(v.pr_number)) if v.pr_number else 'no PR'}"
            f"{', merged ' + v.completed_at[:10] if v.completed_at else ''})"
            for v in stale
        ]
        return "STALE vs " + ", ".join(clauses) + " - Step 0 required"
    if unknown:
        return "unknown (" + "; ".join(f"{v.blocker_id}: {v.reason}" for v in unknown) + ")"
    if reconciled:
        return "reconciled (" + ", ".join(f"{v.blocker_id} marker present" for v in reconciled) + ")"
    return "fresh (no done blocker newer than plan)"


def _boundary_line(
    node_id: Optional[str], plan_path: Optional[str], project_root: Path
) -> str:
    """Boundary-reconcile verdict for the report (x-d0ad). Advisory: the /target
    spine's Step 0 is what mandates acting on STALE. Never raises."""
    if not node_id:
        return "fresh (no node bound)"
    try:
        entry = _graph_entry(node_id, project_root)
    except Exception as exc:  # noqa: BLE001 - degrade, never abort the report
        return f"unknown (graph unreadable: {exc})"
    if entry is None:
        return "unknown (not in graph)"
    try:
        from fno.graph.load import load_graph
        from fno.paths import graph_json
        from fno.plan.boundary import boundary_reconcile

        verdicts = boundary_reconcile(entry, plan_path, load_graph(graph_json()))
        return _render_boundary(verdicts)
    except Exception as exc:  # noqa: BLE001 - render inside the try so the
        # module's "each line resolves independently, never abort" contract holds
        # even if _render_boundary itself raises on malformed verdict data.
        return f"unknown ({exc})"


# --- assembly + render -------------------------------------------------------

def build_report(
    project_root: Path,
    *,
    node_id: Optional[str] = None,
    plan_path: Optional[str] = None,
    manifest_raw: Optional[Dict[str, Any]] = None,
) -> List[OrientLine]:
    """Resolve all seven orientation lines. Read-only; never raises."""
    return [
        OrientLine("node", _node_line(node_id, project_root, manifest_raw)),
        OrientLine("attended", _attended_line(manifest_raw)),
        OrientLine("worktree", _worktree_line(project_root, node_id)),
        OrientLine("tests", _tests_line(project_root)),
        OrientLine("plan", _plan_line(plan_path, project_root)),
        OrientLine("boundary-reconcile", _boundary_line(node_id, plan_path, project_root)),
        OrientLine("manifest-live", _manifest_live_line(manifest_raw)),
        OrientLine("done-when", _done_when_line(manifest_raw, project_root)),
    ]


def render(lines: List[OrientLine]) -> str:
    width = max((len(ln.label) for ln in lines), default=0) + 1  # +1 for ':'
    return "\n".join(f"{(ln.label + ':'):<{width + 1}} {ln.value}" for ln in lines)


def load_orientation(
    project_root: Path,
    *,
    node_id: Optional[str] = None,
    plan_path: Optional[str] = None,
) -> List[OrientLine]:
    """Build the report by reading the session manifest (best-effort).

    Resolves node_id / plan_path / manifest_raw from ``target-state.md`` when it
    exists; degrades to a manifest-less report (substrate-resolved attended, no
    node) otherwise. Explicit ``node_id`` / ``plan_path`` override the manifest
    values (for ``fno target status <node>``). Never raises.
    """
    manifest_raw = _read_manifest(project_root)
    if node_id is None:
        nid = str((manifest_raw or {}).get("graph_node_id") or "").strip()
        if nid and nid != "null":
            node_id = nid
    if plan_path is None:
        pp = str((manifest_raw or {}).get("plan_path") or "").strip().strip("\"'")
        if pp and pp != "null":
            plan_path = pp
    return build_report(
        project_root, node_id=node_id, plan_path=plan_path, manifest_raw=manifest_raw
    )


# Body keys appended below the frontmatter (init-target-state.sh writes them as
# `key: value` lines, NOT YAML frontmatter), so load_agent_context (frontmatter
# only) never sees them. Mirror _maybe_dispatch_work_start's regex read.
_BODY_KEYS = ("graph_node_id", "target_claim_key", "target_claim_holder")


def _read_manifest(project_root: Path) -> Optional[Dict[str, Any]]:
    """Merged session manifest: frontmatter (via load_agent_context) + the body
    `key: value` lines that carry graph_node_id / target_claim_*. None when no
    manifest exists. Never raises."""
    raw: Optional[Dict[str, Any]] = None
    try:
        from fno.agent.state import load_agent_context

        # Pin to project_root so the frontmatter read and the body read below
        # resolve the SAME manifest (load_agent_context otherwise detects the
        # root from cwd, which can differ under FNO_REPO_ROOT / a subdirectory).
        ctx = load_agent_context(project_root_override=project_root)
        if ctx.session is not None:
            raw = dict(ctx.session.raw)
    except Exception:  # noqa: BLE001 - no/unreadable manifest is fine
        pass
    manifest = project_root / ".fno" / "target-state.md"
    try:
        text = manifest.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return raw
    for key in _BODY_KEYS:
        if raw and raw.get(key):
            continue
        m = re.search(rf"^{key}\s*:\s*(.+)$", text, re.MULTILINE)
        if m:
            val = m.group(1).strip().strip("\"'")
            if val and val != "null":
                raw = raw or {}
                raw[key] = val
    return raw


def _self_check() -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        # manifest-less, no node -> a fresh, operator-session report
        os.environ.pop("FNO_AGENT_SELF", None)
        os.environ.pop("FNO_BG", None)
        os.environ.pop("TARGET_UNATTENDED", None)
        lines = build_report(root, node_id=None, plan_path=None, manifest_raw=None)
        assert [ln.label for ln in lines] == [
            "node", "attended", "worktree", "tests", "plan",
            "boundary-reconcile", "manifest-live", "done-when",
        ], lines
        by = {ln.label: ln.value for ln in lines}
        assert by["node"].startswith("fresh"), by
        assert by["attended"].startswith("true"), by
        assert by["manifest-live"].startswith("none"), by
        assert "fno target start" in by["worktree"], by
        out = render(lines)
        assert "node:" in out and "done-when:" in out, out
    print("orient self-check OK")


if __name__ == "__main__":
    _self_check()
