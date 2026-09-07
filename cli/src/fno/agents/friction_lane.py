"""The friction lane: reconcile ONE ``[watchdog-friction:*]`` operator
question to the contended + polling_settled set the sweep measured, riding
the stale lane's channel. Both verdicts are report-only, so a human is the
only lane that clears them; one row per finding rebuilds the
17-percent-signal queue the needs-fold cleanup emptied."""
from __future__ import annotations

from pathlib import Path

FRICTION_MARKER = "watchdog-friction"


def reconcile_friction(pairs, *, root: Path, session_id: "str | None",
                       cwd: Path) -> "tuple[str, str]":
    """The friction lane's question: contended + polling_settled rows.
    See :func:`fno.agents.stale_lane.reconcile_channel` for the fold's
    contract; ``pairs`` is the (Verdict, Row) set the caller filtered out of
    a real ``run_sweep``."""
    from fno.agents.stale_lane import reconcile_channel

    shown = [
        f"{v.name} [node {_row.node or 'unknown'}]: {v.basis}"
        for v, _row in pairs
    ]
    return reconcile_channel(
        pairs, root=root, session_id=session_id, cwd=cwd,
        marker=FRICTION_MARKER, subject="friction",
        identities=[f"{v.verdict}:{v.row_id}" for v, _row in pairs],
        question=lambda key: (
            f"[{FRICTION_MARKER}:{key}] The fleet watchdog holds "
            f"{len(pairs)} contention/polling row(s) no lane will act on. "
            "Each needs a human to separate the sessions or stop the "
            "polling. Rows: " + "; ".join(shown)
        ),
        ask=lambda _key: (
            f"triage {len(pairs)} friction row(s): "
            "fno agents watchdog --only contended; fno agents watchdog "
            "--only polling_settled"
        ),
    )


def run(*, json_out: bool) -> None:
    """The hidden verb's whole body, beside the fold it drives."""
    import json

    from fno.agents import watchdog as wd
    from fno.carveout.core import resolve_carveout_root, resolve_session_id

    payload, rows = wd.run_sweep()
    if payload.get("refused"):
        outcome, qid, count = "refused", "", 0
    else:
        pairs = [
            (wd.Verdict(**data), row)
            for data, row in zip(payload["verdicts"], rows)
            if data["verdict"] in (wd.CONTENDED, wd.POLLING_SETTLED)
        ]
        try:
            from fno.paths import resolve_repo_root

            session_id = resolve_session_id(resolve_repo_root())
        except Exception:  # noqa: BLE001 - an unbound ask still records
            session_id = None
        outcome, qid = reconcile_friction(
            pairs,
            root=resolve_carveout_root(),
            session_id=session_id,
            cwd=Path.cwd(),
        )
        count = len(pairs)
    summary = f"Summary: {count} friction, outcome {outcome}"
    if json_out:
        print(json.dumps({
            "outcome": outcome,
            "question_id": qid,
            "friction_count": count,
            "summary": summary,
        }), flush=True)
    else:
        print(summary, flush=True)
