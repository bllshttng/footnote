#!/usr/bin/env python3
"""One-off: claim merged footnote PRs that no ledger row accounts for.

Over a rename (``fno`` -> ``footnote``, one GitHub repo, continuous PR
numbers) many merged PRs never got an execution row - autonomous bg threads
ship without a session-URL trailer, so finalize's ledger write never fires. The
scoreboard then reports those PRs as work that never happened.

This walks BACK from what landed: the merged-PR set is derived from
``git log --merges origin/main`` (offline, reproducible, and each merge commit
carries the PR number, the merge date, and the source branch in one line). A PR
is UNCLAIMED when no ledger row for this repo names its number. For each
unclaimed PR it appends a minimal execution row::

    {type, project, pr_number, pr_url, merged_at, status, backfilled: true,
     graph_node_id?, session_id?}

Node id: the source branch's node-id token (``feature/x-9608`` -> ``x-9608``),
kept only when it resolves to EXACTLY ONE existing graph node (read-only against
graph.json - the existence check is the disambiguation guard, never a guess).
Session provenance is copied from that node's ``sessions``/``session_id`` when
present; its absence is expected and fine.

Secondary pass: stamp ``pr_number`` onto existing same-repo rows that carry a
``pr_url`` but a null ``pr_number`` (recovered from the URL).

Idempotent: a PR already claimed (including a row this script wrote) is skipped,
so a re-run changes nothing. Counts are computed live, not hardcoded. Writes are
atomic under the ledger flock (refuses on contention). Dry-run by default. This
NEVER touches graph.json.

Usage:
    python3 scripts/metrics/backfill-ledger-pr-claims.py            # dry-run
    python3 scripts/metrics/backfill-ledger-pr-claims.py --apply    # write
    python3 scripts/metrics/backfill-ledger-pr-claims.py --days 30
    python3 scripts/metrics/backfill-ledger-pr-claims.py --self-test
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

LEDGER_LOCK_PATH = Path("/tmp/fno-ledger.lock")

# A node-id-shaped token: short alpha(+num) prefix, dash, 4-8 hex. Same shape as
# backfill-ledger-node-id.py; the exact-match-against-graph guard rejects any
# coincidental hit that is not a real node.
_TOKEN = re.compile(r"\b([a-z][a-z0-9]{0,9}-[0-9a-f]{4,8})\b")

# "Merge pull request #NNN from owner/branch"
_MERGE = re.compile(r"Merge pull request #(\d+) from \S+?/(\S+)")

# A ledger row's PR belongs to this repo iff its pr_url points at footnote or its
# pre-rename name fno (same repo); regready/readyrule are different repos.
_REPO_URL = re.compile(r"github\.com/[^/]+/(?:footnote|fno)/pull/(\d+)")
_SAME_REPO_PROJECTS = {"footnote", "fno", "fno"}

# A merged PR is a delivered node. The scoreboard fold windows every row on
# `completed` and counts a row as shipped only when `termination_reason` is in
# its _SHIPPED_TERMINALS allowlist (DonePRGreen | DoneAdvisory | DoneBatched);
# a row missing either field is invisible to the scoreboard. So each backfilled
# row must carry `completed` (= the merge time) and the delivered terminal.
_SHIPPED_REASON = "DonePRGreen"


def _entries(data: object) -> list[dict]:
    if isinstance(data, dict) and isinstance(data.get("entries"), list):
        return data["entries"]
    if isinstance(data, list):
        return data
    return []


def _atomic_write_json(path: Path, data: object) -> None:
    directory = path.parent
    directory.mkdir(parents=True, exist_ok=True)
    tmp_path: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", dir=str(directory), prefix=f".{path.name}.",
            suffix=".tmp", delete=False, encoding="utf-8",
        ) as tmp:
            tmp_path = tmp.name
            tmp.write(json.dumps(data, indent=2))
            tmp.flush()
            os.fsync(tmp.fileno())
        os.replace(tmp_path, path)
        tmp_path = None
    finally:
        if tmp_path is not None:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass


def pr_of(row: dict) -> int | None:
    """The PR number a row names, from pr_number, legacy pr, or the pr_url tail."""
    n = row.get("pr_number") or row.get("pr")
    if n is None and row.get("pr_url"):
        try:
            n = int(str(row["pr_url"]).rstrip("/").split("/")[-1])
        except ValueError:
            n = None
    try:
        return int(n)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return None


def _same_repo(row: dict) -> bool:
    url = str(row.get("pr_url") or "")
    if _REPO_URL.search(url):
        return True
    if "regready" in url or "readyrule" in url:
        return False
    return row.get("project") in _SAME_REPO_PROJECTS


def claimed_prs(rows: list[dict]) -> set[int]:
    """PR numbers already claimed by a row for THIS repo (footnote/fno)."""
    out: set[int] = set()
    for r in rows:
        if not isinstance(r, dict) or not _same_repo(r):
            continue
        n = pr_of(r)
        if n is not None:
            out.add(n)
    return out


def parse_merges(log_lines: list[str]) -> list[dict]:
    """Parse `git log --merges --pretty=%cI\\t%s` lines into merged-PR records.

    Returns [{"pr": int, "merged_at": iso, "branch": str}], newest first as git
    emits them, deduped on PR number (first occurrence wins).
    """
    out: list[dict] = []
    seen: set[int] = set()
    for line in log_lines:
        if "\t" not in line:
            continue
        merged_at, subject = line.split("\t", 1)
        m = _MERGE.search(subject)
        if not m:
            continue
        pr = int(m.group(1))
        if pr in seen:
            continue
        seen.add(pr)
        out.append({"pr": pr, "merged_at": merged_at.strip(), "branch": m.group(2).strip()})
    return out


def resolve_node(branch: str, node_ids: set[str]) -> str | None:
    """The one existing graph node named by the branch, else None (never guess)."""
    hits = {t for t in _TOKEN.findall(branch or "") if t in node_ids}
    return next(iter(hits)) if len(hits) == 1 else None


def _node_session(node: dict | None) -> str | None:
    if not node:
        return None
    if node.get("session_id"):
        return str(node["session_id"])
    sessions = node.get("sessions")
    if isinstance(sessions, list) and sessions:
        first = sessions[0]
        if isinstance(first, dict):
            return str(first.get("session_id") or first.get("id") or "") or None
        return str(first) or None
    return None


def git_merge_lines(repo_dir: Path, days: int) -> list[str]:
    since = (datetime.now(timezone.utc) - timedelta(days=days)).isoformat()
    out = subprocess.run(
        ["git", "-C", str(repo_dir), "log", "origin/main", "--merges",
         f"--since={since}", "--pretty=%cI%x09%s"],
        capture_output=True, text=True, check=True,
    ).stdout
    return out.splitlines()


def repo_slug(repo_dir: Path) -> str:
    """owner/repo from origin, for constructing pr_url. Falls back to footnote."""
    try:
        url = subprocess.run(
            ["git", "-C", str(repo_dir), "remote", "get-url", "origin"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    except subprocess.CalledProcessError:
        return "bllshttng/footnote"
    m = re.search(r"[:/]([^/:]+/[^/]+?)(?:\.git)?$", url)
    return m.group(1) if m else "bllshttng/footnote"


def _ensure_outcome_fields(row: dict, merged_at: str) -> bool:
    """Give a backfilled row the two fields the scoreboard folds on.

    `completed` is the timestamp every fold window reads; `termination_reason`
    is what marks the row as a delivered ship. Returns True iff it changed
    anything (so an upgrade pass over already-written rows stays idempotent).
    """
    changed = False
    if not row.get("completed") and merged_at:
        row["completed"] = merged_at
        changed = True
    if not row.get("termination_reason"):
        row["termination_reason"] = _SHIPPED_REASON
        changed = True
    return changed


def build_rows(merges: list[dict], claimed: set[int], node_ids: set[str],
               nodes_by_id: dict[str, dict], slug: str, project: str) -> list[dict]:
    """One minimal execution row per unclaimed merged PR."""
    rows: list[dict] = []
    for m in merges:
        if m["pr"] in claimed:
            continue
        node_id = resolve_node(m["branch"], node_ids)
        row = {
            "type": "execution",
            "project": project,
            "pr_number": m["pr"],
            "pr_url": f"https://github.com/{slug}/pull/{m['pr']}",
            "merged_at": m["merged_at"],
            "ts": m["merged_at"],
            "status": "merged",
            "branch": m["branch"],
            "backfilled": True,
        }
        _ensure_outcome_fields(row, m["merged_at"])
        if node_id:
            row["graph_node_id"] = node_id
            sess = _node_session(nodes_by_id.get(node_id))
            if sess:
                row["session_id"] = sess
        rows.append(row)
    return rows


def backfill(ledger_path: Path, merges: list[dict], node_ids: set[str],
             nodes_by_id: dict[str, dict], slug: str, project: str,
             apply: bool) -> int:
    data = json.loads(ledger_path.read_text(encoding="utf-8"))
    # Bind the mutation target to the LIVE list on disk. _entries returns a
    # fresh [] when data is a dict lacking a valid "entries" list; extending
    # that detached list would silently write nothing. Get-or-create instead.
    if isinstance(data, dict):
        if not isinstance(data.get("entries"), list):
            data["entries"] = []
        rows = data["entries"]
    elif isinstance(data, list):
        rows = data
    else:
        data = []
        rows = data

    merged_prs = {m["pr"] for m in merges}
    claimed = claimed_prs(rows)
    before_covered = len(merged_prs & claimed)

    new_rows = build_rows(merges, claimed, node_ids, nodes_by_id, slug, project)
    with_node = sum(1 for r in new_rows if r.get("graph_node_id"))

    # Secondary: recover pr_number on same-repo rows that only carry a pr_url.
    stamped = 0
    for r in rows:
        if not isinstance(r, dict) or not _same_repo(r):
            continue
        if r.get("pr_number") is not None:
            continue
        n = pr_of(r)
        if n is not None:
            r["pr_number"] = n
            stamped += 1

    # Heal already-written backfill rows that predate the outcome fields, so a
    # prior run's rows become visible to the scoreboard on the next --apply.
    upgraded = 0
    for r in rows:
        if not isinstance(r, dict) or not r.get("backfilled"):
            continue
        if _ensure_outcome_fields(r, str(r.get("merged_at") or r.get("ts") or "")):
            upgraded += 1

    print(f"merged PRs in window:        {len(merged_prs)}")
    print(f"  already claimed (before):  {before_covered}")
    print(f"  unclaimed -> new rows:     {len(new_rows)} "
          f"({with_node} with node id, {len(new_rows) - with_node} without)")
    print(f"pr_number stamped on url-only rows: {stamped}")
    print(f"outcome fields healed on prior backfill rows: {upgraded}")
    after_covered = before_covered + len(new_rows)
    print(f"coverage: {before_covered}/{len(merged_prs)} -> "
          f"{after_covered}/{len(merged_prs)}")

    if not apply:
        print("\n[dry-run] pass --apply to write.")
        return 0
    if not new_rows and not stamped and not upgraded:
        print("nothing to write.")
        return 0

    rows.extend(new_rows)
    backup = ledger_path.with_suffix(".json.pre-prclaims-backfill.bak")
    backup.write_text(ledger_path.read_text(encoding="utf-8"), encoding="utf-8")
    _atomic_write_json(ledger_path, data)
    print(f"wrote {ledger_path} (backup: {backup})")
    return 0


def _self_test() -> int:
    log = [
        "2026-07-18T14:11:52-07:00\tMerge pull request #460 from bllshttng/feature/x-9608",
        "2026-07-18T14:11:02-07:00\tMerge pull request #458 from bllshttng/feature/spawn-x-9c5f",
        "2026-07-17T00:00:00-07:00\tMerge pull request #300 from bllshttng/fix/register-doc",
        "2026-07-16T00:00:00-07:00\tnot a merge subject",
        "2026-07-18T14:11:52-07:00\tMerge pull request #460 from bllshttng/feature/x-9608",  # dup
    ]
    merges = parse_merges(log)
    assert [m["pr"] for m in merges] == [460, 458, 300], merges  # deduped, non-merge dropped
    assert merges[0]["branch"] == "feature/x-9608"

    node_ids = {"x-9608", "x-9c5f", "x-dead"}
    assert resolve_node("feature/x-9608", node_ids) == "x-9608"
    assert resolve_node("feature/spawn-x-9c5f", node_ids) == "x-9c5f"  # token, not "spawn-x"
    assert resolve_node("fix/register-doc", node_ids) is None          # no token
    assert resolve_node("feature/x-ffff", node_ids) is None            # token not a real node

    rows = [
        {"project": "footnote", "pr_number": 460},
        {"pr_url": "https://github.com/bllshttng/fno/pull/300"},   # same repo, no number
        {"pr_url": "https://github.com/bllshttng/regready/pull/458"},    # different repo
        {"project": "readyrule-web", "pr_number": 458},                 # different repo
    ]
    claimed = claimed_prs(rows)
    assert claimed == {460, 300}, claimed  # 458 belongs to other repos, not claimed here

    nodes_by_id = {"x-9c5f": {"session_id": "sess-abc"}}
    new = build_rows(merges, claimed, node_ids, nodes_by_id, "bllshttng/footnote", "footnote")
    assert [r["pr_number"] for r in new] == [458], new  # 460 & 300 claimed; only 458 remains
    r = new[0]
    assert r["graph_node_id"] == "x-9c5f" and r["session_id"] == "sess-abc"
    assert r["backfilled"] is True and r["status"] == "merged"
    assert r["pr_url"] == "https://github.com/bllshttng/footnote/pull/458"
    # scoreboard-visible: windows on `completed`, ships on `termination_reason`.
    assert r["completed"] == r["merged_at"]
    assert r["termination_reason"] == _SHIPPED_REASON

    # idempotency: once 458 is claimed, a re-run yields no new rows.
    assert build_rows(merges, claimed | {458}, node_ids, nodes_by_id,
                      "bllshttng/footnote", "footnote") == []

    # heal pass: a prior-run row missing the outcome fields is upgraded once,
    # then stable.
    old = {"backfilled": True, "merged_at": "2026-07-18T00:00:00-07:00"}
    assert _ensure_outcome_fields(old, old["merged_at"]) is True
    assert old["completed"] == old["merged_at"] and old["termination_reason"] == _SHIPPED_REASON
    assert _ensure_outcome_fields(old, old["merged_at"]) is False  # idempotent
    print("self-test: OK")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ledger", type=Path, default=None,
                    help="Ledger path. Default: paths.ledger_json() (global).")
    ap.add_argument("--graph", type=Path, default=None,
                    help="Graph path (read-only). Default: paths.graph_json().")
    ap.add_argument("--repo-dir", type=Path, default=Path.cwd(),
                    help="Repo whose merged PRs are claimed. Default: cwd.")
    ap.add_argument("--project", default="footnote",
                    help="project field on new rows. Default: footnote.")
    ap.add_argument("--days", type=int, default=14, help="Merge window. Default 14.")
    ap.add_argument("--apply", action="store_true", help="Write (default: dry-run).")
    ap.add_argument("--self-test", action="store_true",
                    help="Run in-process assertions and exit (no I/O).")
    args = ap.parse_args()

    if args.self_test:
        return _self_test()

    ledger_path, graph_path = args.ledger, args.graph
    if ledger_path is None or graph_path is None:
        from fno import paths  # only needed to resolve a default path
        ledger_path = ledger_path or paths.ledger_json()
        graph_path = graph_path or paths.graph_json()
    ledger_path = Path(ledger_path).resolve()

    graph = json.loads(Path(graph_path).read_text(encoding="utf-8"))
    node_ids: set[str] = set()
    nodes_by_id: dict[str, dict] = {}
    for e in _entries(graph):
        if isinstance(e, dict) and (nid := e.get("id")):
            node_ids.add(nid)
            nodes_by_id[nid] = e

    merges = parse_merges(git_merge_lines(args.repo_dir, args.days))
    slug = repo_slug(args.repo_dir)
    print(f"repo: {slug} | graph nodes: {len(node_ids)} | ledger: {ledger_path}")

    lock_fd = os.open(str(LEDGER_LOCK_PATH), os.O_CREAT | os.O_RDWR)
    try:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("ledger lock contended; refusing. Re-run when idle.", file=sys.stderr)
            return 2
        return backfill(ledger_path, merges, node_ids, nodes_by_id, slug,
                        args.project, args.apply)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


if __name__ == "__main__":
    sys.exit(main())
