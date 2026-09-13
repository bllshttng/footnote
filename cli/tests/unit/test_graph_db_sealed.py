"""The graph database is sealed against Python. No module under cli/src/fno
may open graph.db with the stdlib sqlite3: the row store is owned by the
Rust backlog modules (ruling 4: each aggregate's module owns its tables),
and Python reaches it through the keeper API only. A new sqlite3 user
under cli/src/fno must carry an allowlist row naming what it opens - the
test fails with the file and line otherwise.
"""

from pathlib import Path
import re

SRC = Path(__file__).resolve().parents[2] / "src" / "fno"

# sqlite3 users that open their OWN databases, never graph.db. Every row is
# a debt: the day the store moves behind an API, the row goes away.
ALLOWLIST = {
    "approvals/store.py": "own approvals store",
    "agents/peek.py": "own read-state db",
    "agents/handle_collisions.py": "own collisions db",
    "agents/discover.py": "own discovery db",
    "graph/fts.py": "own fts5 cache index (graph.json.fts5), never graph.db",
}

# A file that touches the graph row store names it: the db path is derived
# beside the graph or spelled out.
GRAPH_DB_HINTS = re.compile(r"graph\.db|with_extension\([\"']db[\"']\)|database_path\(")

SQLITE_USE = re.compile(r"import sqlite3|sqlite3\.connect")


def _scan(root: Path):
    offenders = []
    for path in sorted(root.rglob("*.py")):
        rel = path.relative_to(root).as_posix()
        text = path.read_text()
        if not SQLITE_USE.search(text):
            continue
        if rel in ALLOWLIST:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if "sqlite3.connect" in line or "import sqlite3" in line:
                offenders.append(f"{rel}:{lineno}: sqlite3 use without an allowlist row")
    return offenders


def test_sealed_self_test_catches_a_violation():
    synthetic = Path("faux_module.py")
    synthetic.write_text(
        "import sqlite3\n"
        "conn = sqlite3.connect('graph.db')\n",
    )
    try:
        offenders = _scan(synthetic.parent)
    finally:
        synthetic.unlink()
    hits = [o for o in offenders if o.startswith("faux_module.py:")]
    assert hits, "the scanner must catch a planted sqlite3 graph.db user"
    print("graph db sealed self-test: PASS")


def test_no_python_module_opens_the_graph_db():
    offenders = _scan(SRC)
    assert not offenders, (
        "graph.db is Rust-owned (crates/fno-agents/src/backlog/); Python "
        "reaches it through the keeper API. Add a documented allowlist row "
        "only for a file that opens its OWN sqlite database:\n"
        + "\n".join(offenders)
    )
