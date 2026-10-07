"""Seed the registry and claims tables the way production writes them.

``registry.json`` and the claims dir are migration fences once the native
store opens, so a test that writes them as files after that point fails.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Optional


def seed_registry(rows: list[dict[str, Any]], path: Optional[Path] = None, **document: Any) -> None:
    """Replace the registry document with ``rows`` (plus any top-level keys)."""
    from fno import paths
    from fno.agents.registry_door import commit_registry_document, read_registry_document

    target = Path(path) if path is not None else Path(paths.agents_registry_path())
    target.parent.mkdir(parents=True, exist_ok=True)
    current, revision = read_registry_document(target)
    payload = {**current, **document, "agents": rows}
    commit_registry_document(target, payload, revision)


def seed_claim(key: str, holder: str = "test-holder", **kwargs: Any):
    """Acquire ``key`` through the public claims API and return the Claim."""
    from fno.claims import acquire_claim

    return acquire_claim(key, holder, **kwargs)


def seed_legacy_registry(entries: list[Any], path: Path) -> Path:
    """Write ``entries`` as the pre-table ``registry.json`` the import reads.

    The table door refuses rows that collide on identity, but a legacy file
    can still hold them; this is the only way to seed that shape. It must run
    before anything opens the store at ``path``.
    """
    import json
    from dataclasses import asdict

    from fno.agents.registry import SCHEMA_VERSION

    rows = [asdict(e) if not isinstance(e, dict) else e for e in entries]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"schema_version": SCHEMA_VERSION, "agents": rows}))
    return path


def update_claim(key: str, root: Optional[Path] = None, **columns: Any) -> None:
    """Rewrite columns of an existing claims-table row, e.g. back-date it.

    The public API cannot age a claim, so a test that needs an expired or
    pid-less row edits the table the claim verbs read. ``expires_at=1``
    makes the row expired.
    """
    import json
    import sqlite3

    from fno.claims import native_claims_root
    from fno.claims.io import claims_dir

    state = claims_dir(root if root is not None else native_claims_root(key)).parent
    stores = list(state.rglob("graph.db"))
    assert stores, f"no graph.db under {state}"
    # Keep the row valid: a pid and pid_unavailable are exclusive, and each
    # shape has its own schema version.
    if columns.get("pid_unavailable"):
        columns.setdefault("schema_version", 2)
    elif columns.get("pid") is not None:
        columns.setdefault("pid_unavailable", 0)
        columns.setdefault("schema_version", 1)
    if "metadata" in columns and not isinstance(columns["metadata"], str):
        columns["metadata"] = json.dumps(columns["metadata"])
    sets = ", ".join(f"{name} = ?" for name in columns)
    for db in stores:
        with sqlite3.connect(db) as connection:
            hit = connection.execute(
                f"UPDATE claims SET {sets} WHERE key = ?", [*columns.values(), key]
            ).rowcount
        if hit:
            return
    raise AssertionError(f"no claims row for {key} under {state}")


def read_claim_row(key: str, root: Optional[Path] = None) -> dict[str, Any]:
    """Return the raw claims-table row for ``key`` (metadata decoded)."""
    import json
    import sqlite3

    from fno.claims import native_claims_root
    from fno.claims.io import claims_dir

    state = claims_dir(root if root is not None else native_claims_root(key)).parent
    for db in state.rglob("graph.db"):
        with sqlite3.connect(db) as connection:
            connection.row_factory = sqlite3.Row
            try:
                row = connection.execute("SELECT * FROM claims WHERE key = ?", [key]).fetchone()
            except sqlite3.OperationalError:
                continue
        if row is not None:
            out = dict(row)
            out["metadata"] = json.loads(out.get("metadata") or "{}")
            return out
    raise AssertionError(f"no claims row for {key} under {state}")
