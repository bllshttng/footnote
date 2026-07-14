"""fno plan migrate-keys - collapse synonym frontmatter keys to the canonical set.

Line-level, byte-preserving key rename over the plans dir (x-f34f US7). The
plan==PR==node schema declares one canonical key per axis; historical plans
carry synonyms that lie to a single-schema reader:

    graph_node_id -> node
    created_at    -> created
    depends_on    -> blocked_by
    kind          -> type

and ``claims`` is dropped where its value byte-equals ``node`` (a pure
duplicate). Two guards keep the rename honest:

- A key is renamed ONLY when its canonical target is ABSENT in the same
  frontmatter (never create a duplicate key); if both are present the legacy
  key is kept and the file is listed for manual review.
- A ``claims`` that DIFFERS from ``node`` is preserved untouched and flagged -
  the migration never guesses which of two distinct ids is right.

``deliverable_type`` is deliberately NOT collapsed into ``type``: they are
different axes (node type includes task/epic; deliverable type includes
investigation) and both have live readers. Idempotent: a re-run over migrated
plans changes nothing. Only the frontmatter block is touched; the body is
byte-for-byte intact (no YAML round-trip).
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from fno.plan._stamp import _atomic_write

# Leading ---\n ... \n--- . Non-greedy so the FIRST block wins even if the body
# carries a --- rule. Mirrors reconcile_status._FRONT_RE.
_FRONT_RE = re.compile(r"\A(---\n)(?P<fm>.*?)(\n---)(?P<rest>.*)\Z", re.DOTALL)
_KEY_RE = re.compile(
    r"^(?P<indent>[ \t]*)(?P<key>[A-Za-z0-9_]+)(?P<sep>[ \t]*:)(?P<val>.*)$"
)

# Legacy synonym -> canonical key. deliverable_type is intentionally absent.
RENAME: dict[str, str] = {
    "graph_node_id": "node",
    "created_at": "created",
    "depends_on": "blocked_by",
    "kind": "type",
}


def _scalar(raw: str) -> str:
    raw = raw.strip()
    if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in ("'", '"'):
        return raw[1:-1]
    return raw


def _top_level_keys(fm: str) -> set[str]:
    keys: set[str] = set()
    for line in fm.splitlines():
        m = _KEY_RE.match(line)
        if m and not m.group("indent"):
            keys.add(m.group("key"))
    return keys


def migrate_text(text: str) -> tuple[Optional[str], list[str]]:
    """Return (new_text, notes). new_text is None when nothing changed.

    notes describe every rename / drop / kept-for-review decision, so the CLI
    receipt is a faithful account of what the migration did to each file.
    """
    m = _FRONT_RE.match(text)
    if not m:
        return None, []
    fm = m.group("fm")
    present = _top_level_keys(fm)

    # Node identity resolves from `node` OR its legacy `graph_node_id` (they
    # collapse to the same key), so a file carrying graph_node_id + claims drops
    # the duplicate claims in the SAME pass as the rename - single-pass idempotent.
    node_val: Optional[str] = None
    for wanted in ("node", "graph_node_id"):
        for line in fm.splitlines():
            km = _KEY_RE.match(line)
            if km and not km.group("indent") and km.group("key") == wanted:
                node_val = _scalar(km.group("val"))
                break
        if node_val is not None:
            break

    out: list[str] = []
    notes: list[str] = []
    changed = False
    for line in fm.splitlines():
        km = _KEY_RE.match(line)
        if not km or km.group("indent"):
            out.append(line)
            continue
        key = km.group("key")
        target = RENAME.get(key)
        if target is not None:
            if target in present:
                notes.append(f"kept {key} ({target} also present) - review")
                out.append(line)
            else:
                out.append(f"{km.group('indent')}{target}{km.group('sep')}{km.group('val')}")
                present.discard(key)
                present.add(target)
                changed = True
                notes.append(f"{key} -> {target}")
            continue
        if key == "claims":
            cval = _scalar(km.group("val"))
            if node_val is not None and cval == node_val:
                changed = True
                notes.append("dropped claims (== node)")
                continue  # drop the duplicate line
            if node_val is not None and cval != node_val:
                notes.append("kept claims (differs from node) - review")
            out.append(line)
            continue
        out.append(line)

    if not changed:
        return None, notes
    return m.group(1) + "\n".join(out) + m.group(3) + m.group("rest"), notes


@dataclass
class MigrateResult:
    migrated: int = 0
    skipped: int = 0
    review: int = 0  # files with a kept legacy key needing a human decision
    changes: list[tuple[str, list[str]]] = field(default_factory=list)  # (path, notes)
    review_files: list[tuple[str, list[str]]] = field(default_factory=list)  # (path, review notes)
    warnings: list[str] = field(default_factory=list)

    def summary(self) -> str:
        return f"{self.migrated} migrated, {self.review} need review, {self.skipped} skipped"


def migrate(plans_dir: Path, *, apply: bool = False) -> MigrateResult:
    """Scan every ``*.md`` in *plans_dir*, collapse synonym keys byte-preservingly."""
    res = MigrateResult()
    if not plans_dir.is_dir():
        res.warnings.append(f"plans dir not found: {plans_dir}")
        return res

    for path in sorted(plans_dir.glob("*.md")):
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            res.skipped += 1
            res.warnings.append(f"skip (unreadable): {path.name}: {exc}")
            continue

        new_text, notes = migrate_text(text)
        review_notes = [n for n in notes if "review" in n]
        if review_notes:
            # Record the path so the receipt can name which files a human must
            # reconcile - a bare count is useless without the list.
            res.review += 1
            res.review_files.append((str(path), review_notes))
        if new_text is None:
            res.skipped += 1
            continue

        res.migrated += 1
        res.changes.append((str(path), notes))
        if apply:
            # Atomic (tmp + os.replace): an interrupted migration never leaves a
            # plan half-written, so a re-run stays idempotent and recoverable.
            _atomic_write(path, new_text)

    return res
