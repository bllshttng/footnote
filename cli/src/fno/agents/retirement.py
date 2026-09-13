"""The forwarding shim over the Rust retirement verdict (x-1379, x-70e1).

The policy no longer lives here: one decision is computed in the Rust GC
(``fno-agents reap --dry-run --json``), and this module only MAPS its
buckets onto the ``Retirement`` verdicts ``fno agents top`` renders. A
binary that is missing, slow or unreadable fails CLOSED - every row reads
not-retirable with the reason named - never as a clean zero.
"""

from __future__ import annotations

import json
import subprocess
from typing import Iterable, NamedTuple, Optional


class Retirement(NamedTuple):
    """The verdict for one worker row, with the basis it was resolved on."""

    node: Optional[str]  # the resolved node id, None when unresolvable
    node_basis: Optional[str]  # "graph" | None (the Rust side owns the join)
    retire: bool
    reason: str  # why, in both directions


def _default_runner() -> str:
    """Shell the Rust dry run and return its stdout."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise RuntimeError("no fno-agents binary installed")
    proc = subprocess.run(  # noqa: S603 - fixed argv, no shell
        [str(binary), "reap", "--dry-run", "--json"],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"reap --dry-run exited {proc.returncode}: {proc.stderr.strip()}")
    return proc.stdout


def _fmt_age(held_s: int) -> str:
    if held_s >= 3600:
        return f"{held_s // 3600}h{(held_s % 3600) // 60}m"
    if held_s >= 60:
        return f"{held_s // 60}m"
    return f"{held_s}s"


def _bucket_reasons(summary: dict) -> dict[str, Retirement]:
    """Map one sweep summary onto per-row verdicts. Every bucket the Rust
    renderer emits is named here, so a NEW bucket cannot silently read as
    not-retirable - an unknown bucket raises and the caller fails closed."""
    out: dict[str, Retirement] = {}
    for row in summary.get("retired", []):
        basis = row["basis"]
        # "every named node done: N1, N2" - the first named node is the one
        # the old graph join displayed; keep it in the NODE column.
        node = None
        if basis.startswith("every named node done:"):
            names = basis.split(":", 1)[1].strip()
            node = names.split(",")[0].strip() or None
        out[row["id"]] = Retirement(node, "graph", True, basis)
    for row in summary.get("kept_open_work", []):
        out[row["id"]] = Retirement(
            row["node"], "graph", False, f"status={row['status']}"
        )
    for row in summary.get("kept_open_do_row", []):
        out[row["id"]] = Retirement(row["node"], "graph", False, "open do row")
    for row in summary.get("kept_not_spawn", []):
        origin = row.get("reason") or "unknown"
        out[row["id"]] = Retirement(None, None, False, f"not a spawn row: origin {origin}")
    for ident in summary.get("kept_operator", []):
        out[ident] = Retirement(None, None, False, "operator row")
    for ident in summary.get("kept_crowned", []):
        out[ident] = Retirement(None, None, False, "crowned")
    for ident in summary.get("kept_no_provenance", []):
        out[ident] = Retirement(None, None, False, "no-node")
    for row in summary.get("kept_active", []):
        out[row["id"]] = Retirement(None, None, False, f"active: written {row['age_s']}s ago")
    for row in summary.get("kept_transcript_unresolved", []):
        # (x-1b90 change 3) The hold names its age; an old hold on done work
        # asks for a decision, and rm proves the death it prints.
        held_s = row.get("held_s", 0)
        reason = f"transcript unresolved for {_fmt_age(held_s)}"
        if row.get("nodes_done") and held_s > 6 * 3600:
            reason += f"; needs a decision: fno agents rm {row['id']}"
        out[row["id"]] = Retirement(None, None, False, reason)
    for row in summary.get("stop_refused", []):
        out[row["id"]] = Retirement(None, None, False, f"stop refused: {row['reason']}")
    for row in summary.get("kept_no_receipt", []):
        out[row["id"]] = Retirement(None, None, False, f"no receipt: {row['reason']}")
    # The hold clock (x-e3cc): every held row maps with its age and basis,
    # so a reader of this projection answers the same question the reap
    # report does. A row its own bucket already mapped keeps that verdict.
    for row in summary.get("holds", []):
        age = row.get("age_s")
        age_text = "unmeasured" if age is None else f"held {age}s"
        out.setdefault(
            row["id"],
            Retirement(
                None,
                None,
                False,
                f"{row['reason']}: {row['detail']}; {age_text}",
            ),
        )
    return out


def verdicts(
    rows: Iterable[tuple[str, Optional[str]]], runner=None
) -> dict:
    """``(name, node_field)`` roster -> ``{name: Retirement}``, one Rust read.

    ``runner`` is the injectable seam (returns the ``--json`` stdout); the
    default shells the installed binary. ANY failure is a named not-retirable
    verdict for every row: a projection that cannot be read is never a clean
    bill of health.
    """
    roster = list(rows)
    try:
        raw = (runner or _default_runner)()
        summary = json.loads(raw)
        mapped = _bucket_reasons(summary)
    except Exception as exc:  # noqa: BLE001 - fail closed, never act
        reason = f"rust-reap-unreadable: {exc}"
        return {name: Retirement(None, None, False, reason) for name, _ in roster}
    out: dict[str, Retirement] = {}
    for name, _node_field in roster:
        if name in mapped:
            out[name] = mapped[name]
        else:
            # The sweep never judged this row (an empty registry, a row
            # added between reads): not judged is not retirable.
            out[name] = Retirement(None, None, False, "not in sweep summary")
    return out
