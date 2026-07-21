#!/usr/bin/env python3
"""Auto-register a target task completion into ledger.json.

Called by the stop hook after all gates pass and cost is calculated.
Reads target-state.md + git state to build the entry, appends to ledger.json,
and regenerates ledger.md.

Usage:
    python3 -m fno.cost._register <target-state-path> <session-id>
    python3 -m fno.cost._register .fno/target-state.md abc123-def456
"""

import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path

from fno import paths as _paths


def safe_number(val, as_type: str = "float", decimals: int = 2):
    """Convert a value to float or int safely. Returns None on failure."""
    if val is None:
        return None
    try:
        if as_type == "int":
            return int(float(val))
        return round(float(val), decimals)
    except (ValueError, TypeError):
        return None


def parse_target_state(state_path: str) -> dict:
    """Parse target-state.md YAML frontmatter into a dict."""
    result = {}
    with open(state_path) as f:
        content = f.read()

    # Extract frontmatter between --- markers
    parts = content.split("---")
    if len(parts) >= 3:
        frontmatter = parts[1]
    else:
        frontmatter = content

    for line in frontmatter.strip().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        match = re.match(r"^(\w[\w_]*):\s*(.*)", line)
        if match:
            key, value = match.group(1), match.group(2).strip()
            # Strip quotes
            if value.startswith('"') and value.endswith('"'):
                value = value[1:-1]
            elif value.startswith("'") and value.endswith("'"):
                value = value[1:-1]
            # Handle null/true/false
            if value == "null" or value == "":
                value = None
            elif value == "true":
                value = True
            elif value == "false":
                value = False
            result[key] = value

    # Also parse completion_gates section (not in frontmatter)
    gates = {}
    in_gates = False
    for line in content.splitlines():
        stripped = line.strip()
        if stripped == "completion_gates:":
            in_gates = True
            continue
        if in_gates:
            if not stripped or (not stripped.startswith("#") and ":" not in stripped):
                in_gates = False
                continue
            gate_match = re.match(r"(\w+):\s*(.*)", stripped)
            if gate_match:
                gates[gate_match.group(1)] = gate_match.group(2).strip()

    result["_gates"] = gates

    # init-target-state.sh writes graph_node_id BELOW the closing frontmatter
    # delimiter (it's appended to the file body, not the YAML block). Do a
    # second targeted scan so the entry-builder can populate it without
    # depending on placement. First match wins; null/empty stays None.
    # Validate the captured value matches the canonical id shape
    # (a `<prefix>-<4..8 lowercase hex>` token, legacy or configured). Keeps a
    # future markdown-prose line like `> graph_node_id: x (deprecated)` from
    # poisoning the
    # entry with a parenthetical-laden ID. Comment lines (`# ...`) are
    # already excluded by the leading-whitespace anchor `^\s*`.
    _GRAPH_NODE_ID_SHAPE = re.compile(r"^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$")
    if "graph_node_id" not in result or result.get("graph_node_id") is None:
        for line in content.splitlines():
            m = re.match(r"^\s*graph_node_id:\s*(.*?)\s*$", line)
            if not m:
                continue
            raw = m.group(1).strip()
            if raw.startswith('"') and raw.endswith('"'):
                raw = raw[1:-1]
            elif raw.startswith("'") and raw.endswith("'"):
                raw = raw[1:-1]
            if raw and raw != "null" and _GRAPH_NODE_ID_SHAPE.match(raw):
                result["graph_node_id"] = raw
            break

    return result


def git_cmd(*args: str) -> str:
    """Run a git command and return stripped output."""
    try:
        return subprocess.check_output(
            ["git"] + list(args), stderr=subprocess.DEVNULL, text=True
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def _pr_number_from_ship_artifact(root_path: str, session_id: str) -> int | None:
    """Read pr_number from the ship handoff artifact the target skill writes.

    The post-wedge immutable manifest (target-state.md) no longer carries
    ``pr_number``; the ship phase records it at
    ``{root}/.fno/artifacts/handoff/ship-{session_id}.md`` (ab-a933adf4).
    Worktree /target runs therefore lost the node->PR auto-link because
    build_entry only read the manifest. The artifact basename matches the
    manifest ``session_id`` exactly, so it is deterministically locatable.
    """
    if not root_path or not session_id:
        return None
    art = (
        Path(root_path)
        / ".fno"
        / "artifacts"
        / "handoff"
        / f"ship-{session_id}.md"
    )
    try:
        text = art.read_text()
    except OSError:
        return None
    for line in text.splitlines():
        m = re.match(r"^\s*pr_number:\s*(\d+)\s*$", line)
        if m:
            return int(m.group(1))
    return None


def _pr_number_from_gh(cwd: str) -> int | None:
    """Last-resort: ask gh for the current branch's PR number. Best-effort and
    silent - a no_ship run or a detached worktree simply yields None."""
    try:
        out = subprocess.check_output(
            ["gh", "pr", "view", "--json", "number", "-q", ".number"],
            cwd=cwd or None,
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    return int(out) if out.isdigit() else None


def derive_phases(state: dict) -> tuple[list[str], list[str]]:
    """Derive completed and skipped phases from gates and skip flags."""
    gates = state.get("_gates", {})

    phase_map = {
        "do": ("quality_check_passed", None),  # if we got to gates, do ran
        "review": ("quality_check_passed", None),
        "validate": ("output_validated", None),
        "ship": ("artifact_shipped", None),
        "external": ("external_review_passed", "no_external"),
        "browser": ("browser_testing_passed", "no_browser"),
        "docs": ("docs_generated", "no_docs"),
    }

    completed = []
    skipped = []

    # Think and plan: check input_type
    input_type = state.get("input_type", "idea")
    if input_type == "idea":
        completed.extend(["think", "plan"])
    else:
        # Plan input skips think/blueprint
        skipped.extend(["think", "plan"])

    for phase, (gate_key, skip_flag) in phase_map.items():
        gate_value = gates.get(gate_key, state.get(gate_key, "false"))
        skip_value = state.get(skip_flag) if skip_flag else False

        if gate_value == "true":
            completed.append(phase)
        elif gate_value == "skipped" or skip_value is True:
            skipped.append(phase)
        else:
            # Not completed and not skipped — omit from both
            pass

    return completed, skipped


def sum_plan_points(plan_path: str | None) -> int | None:
    """Sum estimated_points from plan phase files."""
    if not plan_path:
        return None
    plan_dir = Path(plan_path)
    if not plan_dir.is_dir():
        return None

    total = 0
    found_any = False
    for md_file in sorted(plan_dir.glob("*.md")):
        with open(md_file) as f:
            for line in f:
                match = re.match(r"^estimated_points:\s*(\d+)", line)
                if match:
                    total += int(match.group(1))
                    found_any = True
                    break

    return total if found_any else None


def _pr_url_for(
    pr_number: int | None, remote_url: str | None, cwd: str | None
) -> str | None:
    """The PR's GitHub url, or None when no repo slug can be resolved.

    A row that lands with a ``pr_number`` but no ``pr_url`` attributes no
    ownership at read time (see :mod:`fno.ledger_join`), and no later writer
    repairs it - ``upsert_ledger_pr`` returns early once a row carries the
    number. So the slug must be resolved as hard here as the read side resolves
    it: ``origin`` first, then the same ``gh repo view`` fallback
    :func:`resolve_current_repo_slug` uses, which covers a checkout whose GitHub
    remote simply is not named ``origin``.
    """
    if not pr_number:
        return None
    if remote_url:
        match = re.search(r"github\.com[:/](.+?)(?:\.git)?$", remote_url)
        if match:
            return f"https://github.com/{match.group(1)}/pull/{pr_number}"
    from fno.graph._reconcile import resolve_current_repo_slug

    slug = resolve_current_repo_slug(cwd)
    return f"https://github.com/{slug}/pull/{pr_number}" if slug else None


def build_entry(
    state: dict,
    session_id: str,
    termination_reason: str | None = None,
    cost_json: dict | None = None,
) -> dict:
    """Build a ledger.json entry from target-state and git.

    `termination_reason` (step 6, ab-f8e5f214) records WHY the session ended;
    `cost_json` carries session-cost.py output so cost lands without the
    immutable manifest having to hold it. Both are optional so the legacy
    stop-hook callers stay byte-identical when they pass neither.
    """
    # Git metadata
    branch = git_cmd("branch", "--show-current")
    remote_url = git_cmd("remote", "get-url", "origin")
    project = ""
    if remote_url:
        # Extract repo name: git@github.com:user/repo.git -> repo
        match = re.search(r"/([^/]+?)(?:\.git)?$", remote_url)
        if match:
            project = match.group(1)
    # root_path is the worktree top: project-local ledger writes, event
    # emission, and target-state.md reads all anchor here. Graph-node lookup
    # via _resolve_repo_root() stays on --git-common-dir (canonical repo)
    # so worktree branches still match graph entries written against the
    # canonical checkout. The two paths are equal in a non-worktree.
    worktree_top = git_cmd("rev-parse", "--show-toplevel")
    root_path = worktree_top or ""
    cwd = os.getcwd()

    # PR number (ab-a933adf4). The immutable manifest no longer carries
    # pr_number post-wedge, so a worktree /target run used to record pr=None and
    # never auto-link the node to its PR (reconcile then found "no drift" and the
    # node had to be closed by hand). Prefer the manifest (legacy/back-compat),
    # then the ship handoff artifact the target skill writes, then gh.
    pr_number = state.get("pr_number")
    if not (pr_number and str(pr_number).isdigit()):
        pr_number = _pr_number_from_ship_artifact(
            root_path, state.get("fno_id") or state.get("session_id") or ""
        )
    if not pr_number:
        pr_number = _pr_number_from_gh(cwd)
    pr_number = int(pr_number) if pr_number and str(pr_number).isdigit() else None

    pr_url = _pr_url_for(pr_number, remote_url, cwd)

    # Phases
    phases_completed, phases_skipped = derive_phases(state)

    # Plan points
    plan_path = state.get("plan_path") or state.get("plan_dir")
    points = sum_plan_points(plan_path)

    # Iterations
    iteration = state.get("iteration", 1)
    if isinstance(iteration, str) and iteration.isdigit():
        iteration = int(iteration)
    elif not isinstance(iteration, int):
        iteration = 1

    # Cost fields. The legacy stop-hook path reads them from target-state.md
    # (the LLM wrote them during pre-promise). Step 6's immutable manifest
    # carries no cost, so `finalize` passes session-cost.py's JSON via
    # --cost-json and these fields come straight from it. cost_json wins when
    # present; otherwise fall back to the manifest values (ab-f8e5f214).
    cj = cost_json if isinstance(cost_json, dict) else {}
    # Contract guard: a provider that breaches the tokens shape (list/str) must
    # not reach the `"total" in cj_tokens` / subscript logic below (gemini review).
    cj_tokens = cj.get("tokens") if isinstance(cj.get("tokens"), dict) else {}
    cost_usd = safe_number(cj["cost_usd"] if "cost_usd" in cj else state.get("total_cost"))
    tokens_total = safe_number(
        cj_tokens["total"] if "total" in cj_tokens else state.get("total_tokens"),
        as_type="int",
    )
    cache_read = safe_number(
        cj_tokens["cache_read"] if "cache_read" in cj_tokens else state.get("cache_read_tokens"),
        as_type="int",
    )
    duration = safe_number(
        cj["duration_minutes"] if "duration_minutes" in cj else state.get("duration_minutes"),
        decimals=1,
    )
    compactions = safe_number(
        cj["compactions"] if "compactions" in cj else state.get("compactions"),
        as_type="int",
    )
    model = cj.get("primary_model") or state.get("model")

    # Sessions
    # The caller passes `session_id` as the Claude transcript UUID (from
    # the stop hook's run_completion_accounting). State also carries a
    # stable target-minted session_id (immutable across accounting runs as
    # of gate-provenance phase 01b) plus an optional claude_transcript_id
    # field. Include every distinct identifier we have so cost lookups by
    # either ID keep working after the stop hook stops overwriting
    # session_id with the transcript UUID.
    target_sid = state.get("fno_id") or state.get("session_id") or ""
    # Current key is claude_session_id; old-key fallback for one release.
    claude_tid = state.get("claude_session_id") or state.get("claude_transcript_id") or ""
    _seen: set[str] = set()
    sessions: list[str] = []
    for candidate in (session_id, target_sid, claude_tid):
        if candidate and candidate not in _seen:
            sessions.append(candidate)
            _seen.add(candidate)

    # Completion summary path and text
    summary_path = os.environ.get("TARGET_SUMMARY_PATH")
    summary_text = None
    if summary_path and os.path.isfile(summary_path):
        try:
            with open(summary_path) as sf:
                content = sf.read()
            # Extract "What Was Built" section as summary text
            match = re.search(r"## What Was Built\s*\n\s*(.+?)(?:\n\n|\n##)", content, re.DOTALL)
            if match:
                summary_text = match.group(1).strip()[:200]
        except Exception as e:
            print(f"Warning: could not extract summary: {e}", file=sys.stderr)

    # Terminal status resolution. Prefer `status:` from target-state.md
    # (ABORTED | COMPLETE | etc.), fall back to "done" for the legacy path.
    # ABORTED state carries an `abort_reason:` field in the frontmatter; we
    # surface it as `reason` in the ledger entry and keep the env var
    # (TARGET_ABORT_REASON) as a belt-and-braces override from the stop hook.
    raw_status = state.get("status")
    # parse_target_state can return None/bool/str. Normalize to str for compare.
    raw_status_str = str(raw_status).upper() if raw_status is not None else ""
    if raw_status_str == "ABORTED":
        entry_status = "aborted"
    elif raw_status_str in {"COMPLETE", "", "IN_PROGRESS"}:
        # IN_PROGRESS is the default state; treat as "done" here because the
        # stop hook only calls us on terminal paths. Empty string covers
        # legacy state files without a status key.
        entry_status = "done"
    else:
        # Unknown status: record with status="unknown" and preserve the raw
        # value under `raw_status` (AC2-ERR: never drop the entry).
        entry_status = "unknown"

    abort_reason = (
        os.environ.get("TARGET_ABORT_REASON")
        or state.get("abort_reason")
        or None
    )
    if entry_status == "aborted" and not abort_reason:
        abort_reason = "unspecified"

    # Scalar identity fields make dedup and graph sync reliable. The
    # `sessions` list keeps every distinct ID we know about (transcript
    # UUID + target session_id) for backwards compat with older lookups.
    # `session_id` (scalar) is always the target-minted ID from state when
    # present - the stop hook's ensure_session_registered dedup uses this
    # so it can match an LLM-issued register-task call (which only knows
    # the target ID) against a later transcript-UUID lookup.
    # `graph_node_id` (scalar) lets _sync_to_graph skip the plan_path
    # path normalization entirely on the happy path (graph node was
    # registered via roadmap-tasks.py intake and the node ID was captured at
    # target init).
    scalar_session_id = state.get("fno_id") or state.get("session_id") or None
    scalar_graph_node_id = state.get("graph_node_id") or None

    entry = {
        "type": "execution",
        "status": entry_status,
        "title": state.get("input", "Untitled"),
        "summary": summary_text,
        "summary_path": summary_path,
        "plan_path": plan_path,
        "pr_number": pr_number,
        "pr_url": pr_url,
        "branch": branch or None,
        "project": project or None,
        "root_path": root_path or None,
        # worktree key kept (rendered as null) for legacy JSON readers that
        # still look it up; root_path is now the authoritative worktree path.
        "worktree": None,
        "started": state.get("created_at"),
        "completed": datetime.now().isoformat(),
        "duration_minutes": round(duration, 1) if duration else None,
        "iterations": iteration,
        "compactions": compactions,
        "cost_usd": round(cost_usd, 2) if cost_usd is not None else None,
        "tokens_total": tokens_total,
        "cache_read_tokens": cache_read,
        "model": model,
        # Dual-write for one release: new rows carry both keys (same value);
        # matchers accept either so old (session_id-only) rows stay matchable.
        "fno_id": scalar_session_id,
        "session_id": scalar_session_id,
        "graph_node_id": scalar_graph_node_id,
        "sessions": sessions,
        "phases_completed": phases_completed,
        "phases_skipped": phases_skipped,
        "points": points,
        "notes": None,
    }

    # Only include abort bookkeeping when the run actually aborted - keeps
    # done entries clean and doesn't churn the ledger schema for normal runs.
    if entry_status == "aborted":
        entry["reason"] = abort_reason
    if entry_status == "unknown":
        entry["raw_status"] = str(raw_status) if raw_status is not None else ""

    # Provider attribution: propagate from target-state.md when present.
    # Omit keys entirely when absent (consistent with cost.update() contract).
    # This is a Spec 2 write concern; Phase 04 only propagates what's already
    # in target-state.md frontmatter (set by the loop walker, not here).
    # provider_id: prefer the rotation-written provider_id; fall back to the
    # provider CLI family from target-state.md so EVERY terminal session leaves
    # a provider-attributed ledger row (US7, ab-f8e5f214 - the per-node paper
    # trail needs provider_id present even on standard, non-rotation runs).
    provider_id = state.get("provider_id") or state.get("provider")
    account_id = state.get("account_id")
    if provider_id is not None:
        entry["provider_id"] = provider_id
    if account_id is not None:
        entry["account_id"] = account_id

    # termination_reason (step 6): WHY this session ended (DonePRGreen |
    # DoneAdvisory | DoneBatched | Budget | NoProgress | Interrupted | Aborted
    # | delegated). Written by `fno-agents finalize` on every terminal exit;
    # omitted when a legacy caller does not supply it so old rows stay unchanged.
    if termination_reason:
        entry["termination_reason"] = termination_reason

    return entry


def _load_ledger_data(tasks_path: Path) -> dict:
    """Load a ledger.json into the canonical ``{"entries": [...]}`` shape.

    Caller must already hold the ledger flock. A missing file yields an empty
    ledger; a corrupt file or any shape that can't satisfy the append contract
    (non-dict, or a dict whose ``entries`` isn't a list) is backed up and reset.
    A bare-list legacy ledger is normalized into the wrapper. We recover rather
    than raise on purpose — a raised exception on the stop-hook completion path
    is exactly the silent-skip-registration failure this module exists to remove.
    """
    if not tasks_path.exists():
        return {"entries": []}
    try:
        data = json.loads(tasks_path.read_text())
    except json.JSONDecodeError:
        print(f"Warning: {tasks_path} corrupt, creating backup", file=sys.stderr)
        tasks_path.rename(tasks_path.with_suffix(".json.bak"))
        return {"entries": []}
    if isinstance(data, list):
        return {"entries": data}
    if not isinstance(data, dict) or not isinstance(data.get("entries"), list):
        print(f"Warning: {tasks_path} unexpected shape, creating backup", file=sys.stderr)
        tasks_path.rename(tasks_path.with_suffix(".json.bak"))
        return {"entries": []}
    return data


def _write_ledger_data(tasks_path: Path, data: dict) -> None:
    """Atomically replace tasks_path with data. Caller holds the ledger flock."""
    tmp_fd, tmp_path = tempfile.mkstemp(dir=tasks_path.parent, suffix=".tmp")
    try:
        with os.fdopen(tmp_fd, "w") as f:
            f.write(json.dumps(data, indent=2) + "\n")
        os.replace(tmp_path, tasks_path)
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


def append_to_tasks_json(tasks_path: Path, entry: dict) -> None:
    """Append entry to a ledger.json file atomically with flock."""
    tasks_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = Path("/tmp/abilities-ledger.lock")

    lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)

        data = _load_ledger_data(tasks_path)

        # Same-session race dedupe under flock. The four-tier hook
        # pre-check (ledger-dedup-lookup.py) handles legacy / cross-shape
        # collisions (e.g. transcript-UUID overlap with a legacy entry
        # whose scalar session_id is null). This inner dedup's only job
        # is rejecting a second write from the SAME target session that
        # raced past the pre-check between caller-process fork and our
        # flock acquisition. Match on the scalar id (fno_id-first, session_id
        # fallback for the one-release window); legacy null-id entries are NOT
        # considered.
        new_scalar = entry.get("fno_id") or entry.get("session_id")
        if new_scalar:
            for existing in data.get("entries", []):
                if (existing.get("fno_id") or existing.get("session_id")) == new_scalar:
                    print(
                        f"Skipping duplicate entry for target fno_id: {new_scalar}",
                        file=sys.stderr,
                    )
                    return

        # Collapse rule (x-88df): a full-fidelity row supersedes reconcile's
        # backstop floor for the same node. Before appending a row that carries a
        # graph_node_id, drop any existing `backstop: true` row with that id so a
        # node never carries both. The backstop has no fno_id, so finalize's
        # scalar dedup above can't catch it — this graph_node_id-keyed drop is the
        # only guard against a double-counted cost rollup.
        node_id = entry.get("graph_node_id")
        if node_id:
            data["entries"] = [
                e for e in data["entries"]
                if not (e.get("backstop") and e.get("graph_node_id") == node_id)
            ]

        data["entries"].append(entry)
        _write_ledger_data(tasks_path, data)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)

    print(f"Appended entry #{len(data['entries'])} to {tasks_path}", file=sys.stderr)


# Terminal reasons that mark a NON-delivery ledger row (a failed/aborted
# attempt). A resumed node can carry one of these from an earlier attempt whose
# shipping successor later lost its ledger write; the merged PR must never be
# stamped onto such a row (it would corrupt ship attribution keyed on
# termination_reason). upsert_ledger_pr stamps only a delivery-eligible row.
_NON_DELIVERY_TERMINALS = frozenset(
    {"Budget", "NoProgress", "Interrupted", "Aborted", "NoWork"}
)


def upsert_ledger_pr(
    node_id: str,
    pr_number: int,
    pr_url: str | None,
    project: str | None,
    merged_at: str | None,
) -> str:
    """Stamp or create a ledger row for a merged node, keyed on ``graph_node_id``.

    Reconcile-side backstop (x-88df Part 2) for the transcript-gone tail: the
    merge event knows ``(node, pr, project, merged_at)`` but no ``finalize`` ran.
    Under the SAME ``/tmp/abilities-ledger.lock`` flock the register path uses:

    - existing execution row with ``pr_number`` null -> stamp pr_number/pr_url
      WITHOUT touching its full-fidelity fields -> returns ``"stamped"``
    - existing execution row with a ``pr_number``     -> no-op -> ``"already-present"``
    - no row for the node                             -> minimal backstop row
      (``backstop: true``, ``termination_reason: reconcile-backstop``) -> ``"created"``

    A ``"created"`` backstop is dropped by :func:`append_to_tasks_json`'s collapse
    rule if a full finalize row later lands for the node.
    """
    ledger_path = _paths.ledger_json()
    ledger_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = Path("/tmp/abilities-ledger.lock")

    lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)

        data = _load_ledger_data(ledger_path)

        rows = [
            e for e in data["entries"]
            if e.get("type") == "execution" and e.get("graph_node_id") == node_id
        ]
        # Already correctly attributed to THIS merge.
        if any(e.get("pr_number") == pr_number for e in rows):
            return "already-present"
        # Stamp a DELIVERY row that finalized without resolving its PR - never a
        # failed attempt. A resumed node can carry an earlier Budget/NoProgress
        # row plus the real (ledger-lost) delivery; stamping the merged PR onto
        # the failed attempt would mis-key ship metrics, so those rows are
        # excluded and the delivery gets a fresh backstop instead.
        row = next(
            (
                e for e in rows
                if not e.get("pr_number")
                and e.get("termination_reason") not in _NON_DELIVERY_TERMINALS
            ),
            None,
        )
        if row is not None:
            # Stamp the PR only; the finalize record's cost/phases/completed
            # stay untouched (AC2-EDGE: the stamp adds the PR, never clobbers).
            row["pr_number"] = pr_number
            if pr_url:
                row["pr_url"] = pr_url
            outcome = "stamped"
        else:
            data["entries"].append({
                "type": "execution",
                "status": "done",
                "graph_node_id": node_id,
                "pr_number": pr_number,
                "pr_url": pr_url,
                "project": project,
                "completed": merged_at,
                "backstop": True,
                "termination_reason": "reconcile-backstop",
                "session_id": None,
            })
            outcome = "created"

        _write_ledger_data(ledger_path, data)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)

    return outcome


def render_tasks_md(tasks_json_path: Path, tasks_md_path: Path) -> None:
    """Render a ledger.md from a ledger.json file."""
    if not tasks_json_path.exists():
        return
    try:
        data = json.loads(tasks_json_path.read_text())
    except json.JSONDecodeError:
        return
    # Tolerate a legacy bare-list ledger (see append_to_tasks_json). Any
    # shape that doesn't yield a list of entries (non-dict, or a dict whose
    # `entries` isn't a list) is a no-op render rather than a crash, matching
    # the silent `return` on a JSONDecodeError above.
    if isinstance(data, list):
        entries = data
    elif isinstance(data, dict) and isinstance(data.get("entries"), list):
        entries = data["entries"]
    else:
        return

    # Render via the in-package _session_cost module (the former
    # session-cost.py, now a sibling in fno.cost). The .py ships in the wheel,
    # so the file-existence guard still distinguishes "renderer available" from
    # the project-local inline path.
    script_dir = Path(__file__).parent
    render_script = script_dir / "_session_cost.py"

    if tasks_json_path == _paths.ledger_json() and render_script.exists():
        # Global: use the canonical _session_cost --render renderer.
        subprocess.run(
            [sys.executable, str(render_script), "--render"],
            stderr=subprocess.DEVNULL,
        )
    else:
        # Project-local: inline render (lightweight, no external dep)
        lines = [
            "# Task Registry",
            "",
            f"> {len(entries)} tasks completed in this project.",
            f"> Source of truth: `ledger.json` - this file is a derived view.",
            "",
        ]
        for i, e in enumerate(entries):
            if i > 0:
                lines.append("---")
                lines.append("")
            pr = e.get("pr_number", "?")
            pr_url = e.get("pr_url")
            pr_link = f"[#{pr}]({pr_url})" if pr_url else f"#{pr}"
            cost = e.get("cost_usd")
            cost_str = f"${cost:.2f}" if cost is not None else "—"
            duration = e.get("duration_minutes")
            dur_str = f"{duration} min" if duration else "—"
            lines.append(f"### {e.get('title', 'Untitled')}")
            lines.append("")
            lines.append(f"PR: {pr_link} | Branch: `{e.get('branch', '—')}` | Cost: {cost_str} | Duration: {dur_str}")
            phases = e.get("phases_completed", [])
            lines.append(f"Phases: {', '.join(phases)}" if phases else "Phases: —")
            lines.append(f"Completed: {e.get('completed', '—')}")
            lines.append("")

        tasks_md_path.write_text("\n".join(lines) + "\n")
        print(f"Rendered {len(entries)} entries to {tasks_md_path}", file=sys.stderr)


def build_quick_entry(
    session_id: str,
    entry_type: str,
    title: str,
    plan_path: str | None = None,
    cost_json: dict | None = None,
) -> dict:
    """Build a ledger.json entry without target-state.md.

    Used by /think, /blueprint, /audit to register planning sessions.
    Cost data comes from session-cost.py JSON output.
    """
    branch = git_cmd("branch", "--show-current")
    remote_url = git_cmd("remote", "get-url", "origin")
    project = ""
    if remote_url:
        match = re.search(r"/([^/]+?)(?:\.git)?$", remote_url)
        if match:
            project = match.group(1)
    # See build_entry: root_path tracks the worktree top, not the canonical
    # repo. Graph-node lookup is the only consumer that still needs the
    # canonical path; it calls _resolve_repo_root() directly.
    worktree_top = git_cmd("rev-parse", "--show-toplevel")
    root_path = worktree_top or ""

    cost = cost_json or {}
    tokens = cost.get("tokens", {})

    return {
        "type": entry_type,
        "status": "done",
        "title": title,
        "summary": None,
        "summary_path": None,
        "plan_path": plan_path,
        "pr_number": None,
        "pr_url": None,
        "branch": branch or None,
        "project": project or None,
        "root_path": root_path or None,
        "worktree": None,
        "started": None,
        "completed": datetime.now().isoformat(),
        "duration_minutes": safe_number(cost.get("duration_minutes"), decimals=1),
        "iterations": None,
        "compactions": None,
        "cost_usd": safe_number(cost.get("cost_usd")),
        "tokens_total": tokens.get("total"),
        "cache_read_tokens": tokens.get("cache_read"),
        "model": cost.get("primary_model"),
        # Scalar session_id keeps the inner-flock dedup symmetric with
        # build_entry. Without it, a same-session quick-entry race would
        # fall through both layers (the four-tier hook pre-check still
        # catches transcript-UUID overlap, but the under-flock dedup
        # would silently no-op since entry.get("session_id") is None).
        "session_id": session_id or None,
        "sessions": [session_id] if session_id else [],
        "phases_completed": [],
        "phases_skipped": [],
        "points": None,
        "notes": None,
    }


def _resolve_repo_root() -> str:
    """Resolve the canonical repo root for plan_path normalization.

    Graph nodes store plan_path relative to the repo root. Ledger entries
    written by /target store plan_path absolute. To compare them we anchor
    the relative side at the repo root and absolutize. Use the main
    git common dir (not the worktree top) so worktree branches resolve
    against the same root as the canonical checkout - a worktree at
    .claude/worktrees/foo/internal/x shares its root with main repo's
    internal/x once both are absolutized.
    """
    common = git_cmd("rev-parse", "--path-format=absolute", "--git-common-dir")
    if common:
        return re.sub(r"/\.git/?$", "", common)
    top = git_cmd("rev-parse", "--show-toplevel")
    if top:
        return top
    return os.getcwd()


def _normalize_plan_path(plan_path: str | None, repo_root: str) -> str | None:
    """Anchor a plan_path at repo_root and normalize it for comparison.

    Returns an absolute, os.path.normpath'd path, or None if plan_path is
    falsy. Both ledger-side (absolute) and graph-side (relative) paths
    pass through here so the comparison is symmetric.
    """
    if not plan_path:
        return None
    s = str(plan_path)
    if not os.path.isabs(s):
        s = os.path.join(repo_root, s)
    return os.path.normpath(s)


def _match_graph_node(
    entries: list[dict],
    entry: dict,
    repo_root: str | None = None,
) -> dict | None:
    """Find the graph node that corresponds to a ledger entry.

    Two-tier lookup. The graph_node_id tier covers the happy path - when
    the plan was registered via `roadmap-tasks.py intake` and target captured
    the node ID at init, the entry carries it forward and the lookup is
    O(N) on a single field with no path massaging. The plan_path tier is
    the fallback for entries that didn't carry an ID (manual register-task
    calls, pre-graph_node_id ledger backfills) and resolves the well-known
    failure mode of relative-vs-absolute path mismatch by anchoring both
    sides at the repo root before comparing.

    Returns None when nothing matches; that's a benign no-op in
    _sync_to_graph (e.g., a plan that was never adopted to the graph).
    """
    if not entries:
        return None

    node_id = entry.get("graph_node_id")
    if node_id:
        for n in entries:
            if n.get("id") == node_id:
                return n

    plan_path = entry.get("plan_path")
    if not plan_path:
        return None

    root = repo_root or _resolve_repo_root()
    target = _normalize_plan_path(plan_path, root)
    if not target:
        return None

    for n in entries:
        node_plan = n.get("plan_path")
        if not node_plan:
            continue
        if _normalize_plan_path(node_plan, root) == target:
            return n
    return None


def _sync_to_graph(entry: dict) -> None:
    """Propagate a completed execution entry to graph.json via roadmap-tasks.py.

    When a target session completes, the ledger captures the execution record
    but the feature graph (adoption/roadmap) stays stale unless the caller
    remembers to run `roadmap-tasks.py update`. That manual step was the
    same class of failure as the ledger gap we just fixed - soft guidance
    to the LLM, silently skipped under pressure. This automates the happy
    path so a plan registered via `roadmap-tasks.py intake` flips to completed
    as soon as its execution lands in the ledger.

    Only fires when the entry has plan_path AND pr_number - those are the
    two fields the graph node needs to transition to "merged" or "pr-open"
    status. If roadmap-tasks.py isn't installed, or no graph node matches
    the plan_path, this is a silent no-op (stderr gets a one-liner; stdout
    stays clean so the ledger entry print still parses).
    """
    plan_path = entry.get("plan_path")
    pr_number = entry.get("pr_number")
    if not plan_path or not pr_number:
        return

    # This module lives at cli/src/fno/cost/_register.py, so the repo root is
    # parents[4]; scripts/roadmap-tasks.py is the (in-clone) graph writer. In
    # the installed wheel there is no scripts/ tree, so the exists() guard
    # below skips the best-effort graph sync (the ledger append still landed).
    repo_root = Path(__file__).resolve().parents[4]
    roadmap_script = repo_root / "scripts" / "roadmap-tasks.py"
    graph_path = Path.home() / ".fno" / "graph.json"
    if not roadmap_script.exists() or not graph_path.exists():
        return

    # Find the matching graph node. Read graph.json directly (roadmap-tasks.py
    # has no JSON-dump subcommand); the write path still goes through
    # roadmap-tasks.py so mutations use its flock + kanban rendering.
    try:
        raw = json.loads(graph_path.read_text())
    except (json.JSONDecodeError, OSError):
        return
    # graph.json wraps entries in {"entries": [...]}. Be lenient in case
    # an older version on disk is a bare list.
    entries = raw.get("entries", []) if isinstance(raw, dict) else (raw if isinstance(raw, list) else [])

    node = _match_graph_node(entries, entry)
    if not node:
        return

    node_id = node.get("id")
    if not node_id:
        return

    # sys.executable, not bare "python3": the graph sync must run under the same
    # interpreter as this process (which has fno + its deps), matching the
    # render + finalize interpreter fix. A bare PATH python3 could lack pydantic
    # and fail the graph sync with ModuleNotFoundError.
    update_args = [
        sys.executable, str(roadmap_script), "update", node_id,
        "--completed",
        "--pr-number", str(pr_number),
    ]
    pr_url = entry.get("pr_url")
    if pr_url:
        update_args += ["--pr-url", pr_url]

    try:
        result = subprocess.run(
            update_args, capture_output=True, text=True, timeout=10, check=False,
            cwd=str(repo_root),
        )
        if result.returncode == 0:
            print(
                f"Synced graph node {node_id} (completed, PR #{pr_number})",
                file=sys.stderr,
            )
        else:
            err = (result.stderr or result.stdout or "").strip()
            print(
                f"Warning: graph sync rc={result.returncode} for {node_id}: {err}",
                file=sys.stderr,
            )
    except (subprocess.TimeoutExpired, OSError) as exc:
        print(f"Warning: graph sync failed for {node_id}: {exc}", file=sys.stderr)


def _emit_ledger_transition(entry: dict) -> None:
    """Emit a phase_transition event for the ledger_updated gate.

    Gate-provenance phase 02: after a successful ledger append, signal
    the stop hook's future verify_provenance check that THIS session
    genuinely registered (not just flipped the state flag). Reads the
    provenance_nonce from the session's state file via the entry's
    root_path. Silent no-op when nonce/events helper is unavailable -
    the ledger write itself already succeeded, so missing telemetry
    should not fail the caller. See gate-provenance plan phase 02.
    """
    import shlex
    import subprocess

    root_path = entry.get("root_path")
    if not root_path:
        return
    state_path = Path(root_path) / ".fno" / "target-state.md"
    if not state_path.is_file():
        return
    nonce = ""
    try:
        for line in state_path.read_text().splitlines():
            if line.startswith("provenance_nonce:"):
                nonce = line.split(":", 1)[1].strip()
                break
    except OSError:
        return
    if not nonce or nonce == "null":
        return

    # Locate events.sh. PLUGIN_ROOT env wins; otherwise walk up from this file
    # to the repo root and reach scripts/lib/events.sh. This module lives at
    # cli/src/fno/cost/_register.py, so the repo root is parents[4]. (In the
    # installed wheel there is no scripts/ tree, so the is_file() guard below
    # skips the best-effort emit - the ledger append already succeeded.)
    plugin_root = os.environ.get("CLAUDE_PLUGIN_ROOT")
    if plugin_root:
        events_sh = Path(plugin_root) / "scripts" / "lib" / "events.sh"
    else:
        events_sh = Path(__file__).resolve().parents[4] / "scripts" / "lib" / "events.sh"
    if not events_sh.is_file():
        return

    # Use the target session_id (scalar, set at line 322 from state.get("session_id")),
    # NOT sessions[0] which is the Claude transcript UUID. The stop hook's
    # verify_provenance greps events.jsonl by the target session_id read from
    # target-state.md (`STATE_SESSION_ID`), so emitting with the transcript
    # UUID makes the event invisible to the ledger_updated gate. See
    # scripts/lib/emit-gate-transition.sh:55-56 for the working comparison
    # case that reads from the same source.
    session_id = entry.get("session_id") or ""
    if not session_id:
        # Without a target session_id the emitted event would carry session_id=""
        # and verify_provenance would never match it - silently turning the
        # ledger_updated gate into a no-op. Surface this as a warning and skip
        # the emit so the failure is visible (the gate will trip with
        # no_transition_for_gate, which is the intended diagnostic path).
        print(
            "Warning: _emit_ledger_transition skipped - entry missing target "
            "session_id; ledger_updated gate event NOT emitted",
            file=sys.stderr,
        )
        return
    payload = json.dumps({
        "session_id": session_id,
        "gate": "ledger_updated",
        "phase": "register",
        "nonce": nonce,
        "pr_number": entry.get("pr_number"),
    })
    cmd = f'source {shlex.quote(str(events_sh))} && emit_event_raw "phase_transition" {shlex.quote(payload)}'
    env = os.environ.copy()
    env["EVENTS_FILE"] = str(Path(root_path) / ".fno" / "events.jsonl")
    try:
        subprocess.run(
            ["bash", "-c", cmd],
            check=False,
            timeout=5,
            env=env,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        # bash missing or hang - non-fatal (the ledger append already succeeded)
        pass


def register_entry(entry: dict) -> None:
    """Write entry to the global ledger.json and render ledger.md.

    The ledger is cross-project by definition (one row per terminal session
    across every repo), so there is a single writer path: the global
    ``paths.ledger_json()``. The former project-local dual-write (a stray
    ``<root_path>/.fno/ledger.json``) was the split-brain that corrupted
    node-level joins; it is removed.
    """
    ledger_path = _paths.ledger_json()
    append_to_tasks_json(ledger_path, entry)
    render_tasks_md(ledger_path, ledger_path.with_suffix(".md"))

    # Graph sync (best-effort - never blocks the ledger write)
    _sync_to_graph(entry)

    # 4. Phase-transition event (gate-provenance phase 02, ledger_updated gate)
    _emit_ledger_transition(entry)

    print(json.dumps(entry, indent=2))


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Register a task in ledger.json")
    parser.add_argument("state_or_session", nargs="?", help="target-state.md path (legacy) or session ID (with --type)")
    parser.add_argument("session_id", nargs="?", help="Session ID (legacy positional)")
    parser.add_argument("--type", dest="entry_type", help="Entry type: execution, think, plan, audit")
    parser.add_argument("--title", help="Task title (required with --type)")
    parser.add_argument("--session", help="Session ID")
    parser.add_argument("--plan-path", help="Plan path to link to")
    parser.add_argument("--cost-json", help="JSON string from session-cost.py")
    parser.add_argument(
        "--termination-reason",
        dest="termination_reason",
        help="Terminal reason recorded on the ledger row (step 6, ab-f8e5f214)",
    )

    args = parser.parse_args()

    # Quick mode: --type flag provided
    if args.entry_type:
        session_id = args.session or args.state_or_session or ""
        title = args.title or f"Untitled {args.entry_type} session"
        cost_json = None
        if args.cost_json:
            try:
                cost_json = json.loads(args.cost_json)
            except json.JSONDecodeError:
                print(f"Warning: invalid --cost-json, ignoring", file=sys.stderr)

        entry = build_quick_entry(
            session_id=session_id,
            entry_type=args.entry_type,
            title=title,
            plan_path=args.plan_path,
            cost_json=cost_json,
        )
        register_entry(entry)
        return

    # Legacy mode: positional args (target-state-path session-id)
    if not args.state_or_session or not args.session_id:
        parser.print_usage()
        print("Error: provide <target-state-path> <session-id> or use --type", file=sys.stderr)
        sys.exit(1)

    state_path = args.state_or_session
    session_id = args.session_id

    if not os.path.exists(state_path):
        print(f"Error: {state_path} not found", file=sys.stderr)
        sys.exit(1)

    # Step 6: the legacy path also accepts --cost-json (cost without the
    # immutable manifest holding it) and --termination-reason (why it ended).
    cost_json = None
    if args.cost_json:
        try:
            cost_json = json.loads(args.cost_json)
        except json.JSONDecodeError:
            print("Warning: invalid --cost-json, ignoring", file=sys.stderr)

    state = parse_target_state(state_path)
    entry = build_entry(
        state,
        session_id,
        termination_reason=args.termination_reason,
        cost_json=cost_json,
    )
    register_entry(entry)


if __name__ == "__main__":
    main()
