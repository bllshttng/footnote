"""The stale-row question lane: reconcile one ``[watchdog-stale:*]`` operator
question to the set the sweep actually measured. A row past the wake ceiling
is the needs-human bucket - no action lane may take it, so the only honest
surface is a human's. The generic ``reconcile_channel`` (in
``stale_escalate.py``, the shared fold) serves every report-only question
lane (the friction lane rides it too). Deliberately NOT
the report path: the AC9 census guards ``stale_escalate.py`` against
session-bookkeeping vocabulary (PR 1227 measured the stale ask as noise).
"""
from __future__ import annotations

import re
from pathlib import Path

from fno.agents.stale_escalate import reconcile_channel

STALE_MARKER = "watchdog-stale"
#: The reaper-hold lane (x-e3cc): escalated holds ask on the same durable
#: question channel the stale lane uses.
HOLD_MARKER = "reap-hold"

_AGE_H_RE = re.compile(r"(\d+)h old")


def oldest_h(bases: "list[str]") -> "int | None":
    ages = [int(m.group(1)) for b in bases for m in (_AGE_H_RE.search(b),) if m]
    return max(ages) if ages else None


def reconcile_stale(stale_pairs, *, root: Path, session_id: "str | None",
                    cwd: Path) -> "tuple[str, str]":
    """The stale lane's question: rows past the wake ceiling, oldest age
    named (see :func:`reconcile_channel`)."""
    shown = [
        f"{v.name} [node {_row.node or 'unknown'}]: {v.basis}"
        for v, _row in stale_pairs
    ]
    oldest = oldest_h([v.basis or "" for v, _row in stale_pairs])
    age_clause = f", oldest {oldest}h" if oldest is not None else ""
    return reconcile_channel(
        stale_pairs, root=root, session_id=session_id, cwd=cwd,
        marker=STALE_MARKER, subject="stale",
        identities=[f"stale:{v.row_id}" for v, _row in stale_pairs],
        question=lambda key: (
            f"[{STALE_MARKER}:{key}] The fleet watchdog holds "
            f"{len(stale_pairs)} stale row(s) no lane will act on{age_clause}. "
            "Nothing in the sweep clears these; each needs a human to reap "
            "it or resume it. Rows: " + "; ".join(shown)
        ),
        ask=lambda _key: (
            f"triage {len(stale_pairs)} stale watchdog row(s){age_clause}: "
            "fno agents watchdog --only stale"
        ),
    )


def reconcile_holds(holds, *, root: Path, session_id: "str | None",
                    cwd: Path) -> "tuple[str, str]":
    """The reap-hold lane's question: holds past agents.hold_escalate_after_s
    (see :func:`reconcile_channel`). Identities are ``hold:<id>:<reason>``,
    so a hold that changes reason asks again. The ask is the first row's
    release command."""
    if not holds:
        # An empty set closes: the channel decides, never the caller.
        return reconcile_channel(
            [], root=root, session_id=session_id, cwd=cwd,
            marker=HOLD_MARKER, subject="reap-hold",
            identities=[],
            question=lambda key: "",
            ask=lambda _key: "",
        )
    shown = []
    for h in holds:
        age = h.get("age_s")
        age_text = "unmeasured" if age is None else f"{age}s"
        shown.append(
            f"{h['id']} held {age_text} under {h['reason']}: {h['detail']}"
        )
    ask_cmd = f"fno agents reap --release {holds[0]['id']}"
    return reconcile_channel(
        holds, root=root, session_id=session_id, cwd=cwd,
        marker=HOLD_MARKER, subject="reap-hold",
        identities=[f"hold:{h['id']}:{h['reason']}" for h in holds],
        question=lambda key: (
            f"[{HOLD_MARKER}:{key}] The reaper holds "
            f"{len(holds)} row(s) past agents.hold_escalate_after_s. "
            "Each hold is correct and none clears on its own. Rows: "
            + "; ".join(shown) + f" -> {ask_cmd}"
        ),
        ask=lambda _key: ask_cmd,
    )


def escalated_holds():
    """The escalated holds from one reap dry-run read; ``None`` when the
    read fails, so the channel closes nothing on an unreadable instrument."""
    import json

    from fno.agents import retirement as retirement_mod

    try:
        summary = json.loads(retirement_mod._default_runner())
    except Exception:  # noqa: BLE001 - an unreadable instrument asks nothing
        return None
    return [h for h in summary.get("holds", []) if h.get("escalated")]


def run(*, json_out: bool) -> None:
    """The hidden stale-escalate verb's whole body, beside the fold it drives."""
    import json

    from fno.agents import watchdog as wd
    from fno.carveout.core import resolve_carveout_root, resolve_session_id

    root = resolve_carveout_root()
    try:
        from fno.paths import resolve_repo_root

        session_id = resolve_session_id(resolve_repo_root())
    except Exception:  # noqa: BLE001 - an unbound ask still records
        session_id = None
    cwd = Path.cwd()

    # The hold channel (x-e3cc) runs even when the watchdog sweep refused:
    # the reaper's clock is a different instrument, and its question must
    # not vanish behind the watchdog's own refusal.
    holds = escalated_holds()
    if holds is None:
        hold_count, hold_outcome, hold_qid = 0, "refused", ""
    else:
        hold_outcome, hold_qid = reconcile_holds(
            holds, root=root, session_id=session_id, cwd=cwd,
        )
        hold_count = len(holds)

    payload, rows = wd.run_sweep()
    if payload.get("refused"):
        outcome, qid, stale_count, oldest = "refused", "", 0, 0
    else:
        stale_pairs = [
            (wd.Verdict(**data), row)
            for data, row in zip(payload["verdicts"], rows)
            if data["verdict"] == wd.STALE
        ]
        outcome, qid = reconcile_stale(
            stale_pairs,
            root=root,
            session_id=session_id,
            cwd=cwd,
        )
        stale_count = len(stale_pairs)
        oldest = oldest_h([v.basis or "" for v, _row in stale_pairs]) or 0

    summary = (
        f"Summary: {stale_count} stale, outcome {outcome}, oldest {oldest}h; "
        f"{hold_count} escalated hold(s), outcome {hold_outcome}"
    )
    if json_out:
        print(json.dumps({
            "outcome": outcome,
            "question_id": qid,
            "stale_count": stale_count,
            "oldest_h": oldest,
            "hold_count": hold_count,
            "hold_outcome": hold_outcome,
            "hold_question_id": hold_qid,
            "summary": summary,
        }), flush=True)
    else:
        print(summary, flush=True)
