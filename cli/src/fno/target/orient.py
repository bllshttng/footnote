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
    status = str(entry.get("_status") or entry.get("status") or "").strip()
    pr = entry.get("pr_number")
    # `done` is terminal FIRST, before any PR-metadata branch: an advisory /
    # no-ship / manually-completed node is `done` without a PR, and must not
    # fall through to claim/fresh and misorient a resumed agent toward rework.
    if status == "done":
        return f"shipped (PR #{pr} merged)" if pr else "done (no PR)"
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


def _attended_line(manifest_raw: Optional[Dict[str, Any]]) -> str:
    if manifest_raw and "attended" in manifest_raw:
        val = str(manifest_raw["attended"]).strip().lower()
        return f"{val} (manifest)"
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
    """The must-have-reviewed bot list: None -> code default; [] -> no gate."""
    from fno.config import load_settings_for_repo

    bots = load_settings_for_repo(project_root).config.review.required_bots
    if bots is None:
        return ["chatgpt-codex-connector"]
    return list(bots)


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
    except Exception:  # noqa: BLE001 - degrade to the code default
        bots = ["chatgpt-codex-connector"]
    bots_str = ", ".join(bots) if bots else "none (PR + CI only)"
    line = f"PR + CI green + reviewed by [{bots_str}]"
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
            "boundary-reconcile", "done-when",
        ], lines
        by = {ln.label: ln.value for ln in lines}
        assert by["node"].startswith("fresh"), by
        assert by["attended"].startswith("true"), by
        assert "fno target start" in by["worktree"], by
        out = render(lines)
        assert "node:" in out and "done-when:" in out, out
    print("orient self-check OK")


if __name__ == "__main__":
    _self_check()
