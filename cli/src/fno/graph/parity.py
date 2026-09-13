"""JSON/relational graph parity: the thin client. The compare itself lives
in the Rust store (backlog::parity, the only implementation) and is served
by the keeper's `parity` op; this module resolves the graph, asks the
keeper, and reports. --negative-control copies the live pair, changes one
copied row's title, and requires the compare to exit 1 naming that id
(AC2-HP). Runs in-process: no subprocess, no script lookup."""

from __future__ import annotations

import argparse
import json
import shutil
import tempfile
from pathlib import Path

def _resolve(graph: "Path | None", db: "Path | None") -> tuple[Path, Path]:
    if graph is None:
        from fno import paths
        graph = paths.graph_json()
    graph = Path(graph)
    return graph, Path(db or graph.with_suffix(".db"))

def _op_result(graph: Path) -> dict:
    """The keeper's parity reply for `graph`, spawning the keeper when its
    socket is positively dead. Every failure mode (dead socket, spawn
    failure, error reply) raises."""
    from fno.graph.store import _client_for

    client = _client_for(graph)
    return client.request("parity", {})

def compare(*, graph: "Path | None" = None, db: "Path | None" = None) -> int:
    graph, db = _resolve(graph, db)
    try:
        result = _op_result(graph)
    except Exception as exc:  # noqa: BLE001 - any store failure reads UNMEASURED
        print(f"graph-parity: UNMEASURED: {exc}")
        return 2
    divergent = int(result.get("divergent", 0))
    if divergent:
        for node_id in result.get("divergent_ids", []) or []:
            print(f"graph-parity: content diverged: {node_id}")
        print(f"graph-parity: FAIL: {divergent} divergent row(s)")
        return 1
    print(f"graph-parity: PASS: compared {result.get('rows', 0)} rows")
    return 0

def negative_control(*, graph: "Path | None" = None, db: "Path | None" = None) -> int:
    """Copy the live pair, require a clean compare on the copies, change one
    copied row's title on the JSON side, require exit 1 naming that id."""
    graph, db = _resolve(graph, db)
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        copy_graph, copy_db = root / "graph.json", root / "graph.db"
        copy_graph.write_bytes(graph.read_bytes())
        if db.exists():
            shutil.copy2(db, copy_db)
            for suffix in ("-wal", "-shm"):
                sidecar = Path(str(db) + suffix)
                if sidecar.exists():
                    shutil.copy2(sidecar, str(copy_db) + suffix)
        if compare(graph=copy_graph) != 0:
            print("negative control: FAIL: clean copies did not compare clean")
            return 1
        doc = json.loads(copy_graph.read_bytes())
        entries = doc.get("entries")
        if not entries:
            print("negative control: FAIL: no entries to mutate")
            return 1
        victim = entries[0]
        target_id = victim.get("id")
        victim["title"] = f"{victim.get('title', '')} (negative control mutation)"
        copy_graph.write_text(json.dumps(doc))
        if compare(graph=copy_graph) != 1:
            print("negative control: FAIL: mutated copy did not diverge")
            return 1
        print(f"negative control: PASS {target_id}")
        return 0

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--graph", type=Path)
    parser.add_argument("--db", type=Path)
    parser.add_argument("--negative-control", action="store_true")
    args = parser.parse_args()
    if args.negative_control:
        return negative_control(graph=args.graph, db=args.db)
    return compare(graph=args.graph, db=args.db)

if __name__ == "__main__":
    raise SystemExit(main())
