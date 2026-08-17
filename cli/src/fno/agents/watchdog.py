"""fno.agents.watchdog - the external fleet watchdog (x-55c3).

Runs OUTSIDE every session (a manual verb, or a leg on the pr_watch tick) and
decides, per fleet row: wake it, reroute it, reap it, or leave it. The decision
is made from TRANSCRIPT truth keyed by session id; the two stores (the fno
registry, ``claude agents --json``) are hints. Measured 2026-08-15: 8 roster
rows claimed ``working`` while their transcripts had not moved in 30+ minutes,
``claude agents --json`` inverted live/dead on a capped lane, and a bulk reap
that trusted a session's ``done`` state as terminal killed live sessions (a
``done`` row means the turn finished and the session is RESUMABLE). The
transcript was right every single time.

The classifier is one pure function over injected inputs (no subprocess inside
it), so tests need no live fleet. Mechanisms delegate: the wake lane calls
``fno agents resume`` (x-c136) and then confirms the message by CONTENT in the
recipient transcript - a state field reading ``working`` was caught claiming a
wake that never landed. Reroute reuses ``fno.recovery._redispatch`` (stop
FIRST, then respawn; skipping the stop wakes a duplicate when the window
opens). Reap refuses on a dirty worktree and leans on ``claude rm``'s own
refusal rather than forcing past it.

Two traps a stranger inherits (both measured by hand on 2026-08-15):
  - node identity joins on the recorded ``node:<id>`` claim holder / worktree
    manifest, NEVER on a name regex: eight auto-named workers read as
    nobody-on-this-node and were nearly double-dispatched.
  - a wake is confirmed by transcript content, never by a state field.
"""
from __future__ import annotations

import json
import re
import subprocess
from collections import namedtuple
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

Verdict = namedtuple("Verdict", "row_id name state verdict basis action")
Row = namedtuple("Row", "row_id name state node cwd delivery_policy")
#: ``records`` is [(epoch_s_or_None, text)] newest-last; ``tail_text`` is the
#: flattened join of those texts. No transcript resolving -> None (ghost),
#: which is a different fact from a resolved-but-quiet transcript.
TailFacts = namedtuple("TailFacts", "records last_event_epoch tail_text")

GHOST = "ghost"
REAP = "reap"
REROUTE = "reroute"
WAKE = "wake"
LEAVE = "leave"

#: States that make a transcript-less row a ghost: the row claims a live-ish
#: session whose recorded id resolves to nothing. A ``stopped`` row with no
#: transcript is not a ghost - stopped is already the operator's answer.
_GHOST_STATES = frozenset({"working", "blocked"})
_WAKE_STATES = frozenset({"blocked", "stopped"})

# The 429 marker and its reset stamp. Reset stamps ride the provider error
# text in Singapore local time (UTC+8): "02:48:21 SGT" is 18:48:21Z. Two
# sessions launched at 18:45 and 18:46 took a 429 they would not have taken
# three minutes later, so waking inside a closed window costs a real turn -
# which is why an UNPARSEABLE stamp classifies leave, never wake.
_RATE_MARK_RE = re.compile(r"\b429\b|rate[ _-]?limit", re.IGNORECASE)
_RESET_STAMP_RE = re.compile(r"(\d{1,2}):(\d{2}):(\d{2})\s*(?:SGT|UTC\+8)")
_SGT_OFFSET_S = 8 * 3600
_TAIL_BYTES = 64 * 1024
_TAIL_RECORDS = 15

#: The bare resume word (x-e21e): a bus-only row is woken with this and never
#: a message payload - a wake is an attach and a neutral resume, not a paste.
WAKE_MESSAGE = "continue"


# ---------------------------------------------------------------------------
# Rate-limit window math (pure)
# ---------------------------------------------------------------------------

def parse_sgt_stamp(
    hour: int, minute: int, second: int, now_s: float
) -> Optional[float]:
    """``HH:MM:SS SGT`` -> UTC epoch seconds, or None when out of range.

    A time-only stamp carries no date, so the day is the one of ``now`` rolled
    to the NEAREST candidate (a stamp 23h in the past is tomorrow's window, not
    yesterday's). A bad clock value (25:00) is None, never a guess.
    """
    if not (0 <= hour <= 23 and 0 <= minute <= 59 and 0 <= second <= 59):
        return None
    now_dt = datetime.fromtimestamp(now_s, tz=timezone.utc)
    base = now_dt.replace(hour=hour, minute=minute, second=second, microsecond=0)
    candidates = [
        base.timestamp() - _SGT_OFFSET_S + day * 86400 for day in (-1, 0, 1)
    ]
    return min(candidates, key=lambda t: abs(t - now_s))


def rate_limit_window(
    tail_text: str, now_s: float
) -> tuple[str, Optional[float], str]:
    """``("none"|"live"|"passed"|"unknown", reset_epoch, stamp_str)``.

    ``none``: no 429 marker in the tail. ``live``/``passed``: a 429 whose reset
    stamp sits in the future / past. ``unknown``: a 429 whose stamp cannot be
    found or parsed - fail safe, the caller must not wake on it.
    """
    if not _RATE_MARK_RE.search(tail_text):
        return "none", None, ""
    m = _RESET_STAMP_RE.search(tail_text)
    if m is None:
        return "unknown", None, ""
    stamp = m.group(0)
    epoch = parse_sgt_stamp(int(m.group(1)), int(m.group(2)), int(m.group(3)), now_s)
    if epoch is None:
        return "unknown", None, stamp
    return ("live" if epoch > now_s else "passed"), epoch, stamp


# ---------------------------------------------------------------------------
# The classifier (pure)
# ---------------------------------------------------------------------------

def verdicts(
    rows: list[Row],
    *,
    transcript_for: Callable[[str], Optional[TailFacts]],
    claim_for: Callable[[str], dict],
    node_state_for: Callable[[str], Optional[dict]],
    now_s: float,
) -> list[Verdict]:
    """One verdict per row, in table precedence (ghost > reap > reroute >
    wake > leave). Each basis string names the measurement that decided it, so
    a reader can falsify the call. ``claim_for(node)`` returns the
    ``node:<id>`` claim view (``{"state", "holder"}``); ``node_state_for``
    returns the graph entry (``{"status", ...}``) or None."""
    out: list[Verdict] = []
    for row in rows:
        out.append(
            _verdict_one(
                row,
                transcript_for=transcript_for,
                claim_for=claim_for,
                node_state_for=node_state_for,
                now_s=now_s,
            )
        )
    return out


def _mins(now_s: float, epoch: Optional[float]) -> Optional[int]:
    if epoch is None:
        return None
    return max(0, int((now_s - epoch) / 60))


def _age_clause(now_s: float, epoch: Optional[float]) -> str:
    n = _mins(now_s, epoch)
    return f"{n}m" if n is not None else "unknown"


def _verdict_one(
    row: Row,
    *,
    transcript_for: Callable[[str], Optional[TailFacts]],
    claim_for: Callable[[str], dict],
    node_state_for: Callable[[str], Optional[dict]],
    now_s: float,
) -> Verdict:
    facts = transcript_for(row.row_id)

    # ghost: the row claims working/blocked but its recorded id resolves to no
    # transcript - a wake that failed to attach, fell back to spawning, minted
    # a new id, and left this row claiming a session that does not exist. It
    # outranks reap because a row with no transcript cannot be safely stopped.
    if facts is None and row.state in _GHOST_STATES:
        return Verdict(row.row_id, row.name, row.state, GHOST,
                       f"no transcript for {row.row_id}", "report")

    # reap: the DELIVERABLE is settled (node done / claim held live by another
    # session), never the session's own state - a `done` row is resumable and
    # a reap keyed on it killed live sessions on 2026-08-15.
    if row.node:
        entry = node_state_for(row.node)
        if entry is not None and entry.get("status") == "done":
            return Verdict(row.row_id, row.name, row.state, REAP,
                           f"node {row.node} done", "stop+rm")
        claim = claim_for(row.node)
        holder_sid = _holder_session(claim.get("holder"))
        if claim.get("state") == "live" and holder_sid and holder_sid != row.row_id:
            return Verdict(row.row_id, row.name, row.state, REAP,
                           f"claim held by {holder_sid}", "stop+rm")

    window, reset_epoch, stamp = ("none", None, "")
    if facts is not None:
        window, reset_epoch, stamp = rate_limit_window(facts.tail_text, now_s)

    # reroute: blocked on a 429 whose window has NOT opened. Waking bounces
    # (proved twice by hand); the session must be stopped before the window
    # opens or it wakes into a duplicate.
    if row.state == "blocked" and window == "live":
        reset_utc = datetime.fromtimestamp(reset_epoch, tz=timezone.utc)
        return Verdict(
            row.row_id, row.name, row.state, REROUTE,
            f"429 resets {reset_utc.strftime('%H:%M:%SZ')}, "
            f"{_mins(reset_epoch, now_s)}m out",
            "redispatch",
        )

    # wake: blocked or stopped, a transcript exists, and no live 429 window.
    # An unknown window (stamp missing or unparseable) is NOT wakeable.
    if row.state in _WAKE_STATES and facts is not None:
        age = _age_clause(now_s, facts.last_event_epoch)
        if window == "unknown":
            return Verdict(row.row_id, row.name, row.state, LEAVE,
                           f"429 present, reset window unknown "
                           f"({stamp or 'no stamp'})", "none")
        clause = ("last 429 window passed" if window == "passed"
                  else "no 429 in tail")
        return Verdict(row.row_id, row.name, row.state, WAKE,
                       f"{row.state} {age}, {clause}", "resume")

    # leave: everything else, including every healthy injectable row - the
    # watchdog never competes with the normal inject path.
    basis = (
        f"reachable, last turn {_age_clause(now_s, facts.last_event_epoch)} ago"
        if facts is not None
        else f"no transcript, state {row.state}"
    )
    return Verdict(row.row_id, row.name, row.state, LEAVE, basis, "none")


def _holder_session(holder: Optional[str]) -> Optional[str]:
    """``target-session:<uuid>`` -> ``<uuid>`` (truth_status._session_from_holder
    semantics; a foreign holder shape returns None and condemns nothing)."""
    prefix = "target-session:"
    if holder and holder.startswith(prefix):
        sid = holder[len(prefix):]
        return sid or None
    return None


# ---------------------------------------------------------------------------
# Real I/O seams (every one injectable; the classifier above stays pure)
# ---------------------------------------------------------------------------

def tail_facts(session_id: str, cwd: str) -> Optional[TailFacts]:
    """Resolve a session's transcript and tail-read it. Never raises.

    Reuses the provenance resolver (content-aware across every project dir -
    the dir name is the LAUNCH cwd, never derivable from a repo or worktree
    name). A missing transcript is None, which the classifier renders as a
    fact (ghost / unknown-age), never as fresh. A 64KB tail read costs ~0.9ms
    per row measured over 130 rows, so sweeping the fleet is cheap.
    """
    from fno.provenance.observed import resolve_transcript_path

    try:
        path = resolve_transcript_path("claude", session_id, cwd)
    except Exception:  # noqa: BLE001 - a broken resolver is "no transcript"
        return None
    if path is None:
        return None
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            fh.seek(max(0, size - _TAIL_BYTES))
            chunk = fh.read()
    except OSError:
        return None
    lines = chunk.decode("utf-8", "replace").splitlines()
    if size > _TAIL_BYTES and lines:
        lines = lines[1:]  # a mid-file seek lands inside a line; drop it
    records: list[tuple[Optional[float], str]] = []
    for line in lines:
        try:
            e = json.loads(line)
        except Exception:  # noqa: BLE001 - a torn/foreign line is not data
            continue
        epoch: Optional[float] = None
        ts = e.get("timestamp")
        if ts:
            try:
                epoch = datetime.fromisoformat(
                    str(ts).replace("Z", "+00:00")
                ).timestamp()
            except ValueError:
                epoch = None
        records.append((epoch, _record_text(e)))
    records = records[-_TAIL_RECORDS:]
    last_epoch = next((t for t, _ in reversed(records) if t is not None), None)
    return TailFacts(records, last_epoch, " ".join(t for _, t in records))


def _record_text(e: dict) -> str:
    """Flattened text of one transcript record (message bodies and top-level
    system text - a provider 429 lands in either)."""
    msg = e.get("message")
    parts: list[str] = []
    content = msg.get("content") if isinstance(msg, dict) else None
    if isinstance(content, str):
        parts = [content]
    elif isinstance(content, list):
        parts = [
            p.get("text", "")
            for p in content
            if isinstance(p, dict) and p.get("type") == "text"
        ]
    if not parts and isinstance(e.get("text"), str):
        parts = [e["text"]]
    return " ".join(" ".join(parts).split())


def fleet_rows() -> tuple[list[Row], list[str]]:
    """Enumerate the fleet from ``claude agents --json --all`` joined to the
    registry for recorded identity. Node identity comes from the worktree
    MANIFEST (runtime-recorded), never from a name regex."""
    from fno.agents.harnesses.claude import claude_agents_rows
    from fno.agents.registry import load_registry
    from fno.recovery import _node_id_from_worktree

    raw, warnings = claude_agents_rows()
    by_sid: dict[str, Any] = {}
    try:
        for entry in load_registry():
            if getattr(entry, "harness", None) != "claude":
                continue
            sid = (
                getattr(entry, "harness_session_id", None)
                or getattr(entry, "cc_session_id", None)
            )
            if sid:
                by_sid[str(sid)] = entry
    except Exception:  # noqa: BLE001 - registry read miss degrades to claude rows
        by_sid = {}
    out: list[Row] = []
    for r in raw:
        sid = str(r.get("sessionId") or r.get("id") or "")
        if not sid:
            continue
        entry = by_sid.get(sid)
        cwd = str(r.get("cwd") or getattr(entry, "cwd", "") or "")
        node = _node_id_from_worktree(cwd) if cwd else None
        out.append(Row(
            row_id=sid,
            name=str(getattr(entry, "name", None) or r.get("name") or sid),
            state=str(r.get("state") or ""),
            node=node,
            cwd=cwd,
            delivery_policy=getattr(entry, "delivery_policy", None),
        ))
    return out, warnings


def _claim_view(node: str) -> dict:
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    key = f"node:{node}"
    try:
        return claim_status(key, root=claims_root_for(key))
    except Exception:  # noqa: BLE001 - an unreadable claim condemns nothing
        return {}


def _graph_index() -> dict[str, dict]:
    from fno.graph.load import load_graph

    try:
        entries = load_graph()
    except Exception:  # noqa: BLE001 - graph miss degrades to "no node state"
        return {}
    return {
        str(e.get("id")): e for e in entries if isinstance(e, dict) and e.get("id")
    }


def run_sweep(
    *,
    now_s: Optional[float] = None,
    rows_provider: Optional[Callable[[], tuple[list[Row], list[str]]]] = None,
    transcript_fn: Optional[Callable[[str], Optional[TailFacts]]] = None,
    claim_fn: Optional[Callable[[str], dict]] = None,
    graph_fn: Optional[Callable[[], dict[str, dict]]] = None,
) -> tuple[dict, list[Row]]:
    """Build the real seams and classify the whole fleet once. Returns
    ``(payload, rows)`` - the payload is the ``--json`` shape
    (``{"generated_at", "verdicts", "counts", "warnings"}``) and the rows ride
    along index-aligned with the verdicts so an apply lane can reach each
    row's cwd. Read-only - actions live in :func:`apply_verdict`."""
    now_s = now_s if now_s is not None else datetime.now(timezone.utc).timestamp()
    rows, warnings = (rows_provider or fleet_rows)()
    cwd_by_sid = {r.row_id: r.cwd for r in rows}
    if transcript_fn is None:
        def transcript_fn(sid: str) -> Optional[TailFacts]:
            return tail_facts(sid, cwd_by_sid.get(sid, ""))
    claim_fn = claim_fn or _claim_view
    if graph_fn is None:
        index = _graph_index()

        def graph_fn() -> dict[str, dict]:
            return index
    vs = verdicts(
        rows,
        transcript_for=transcript_fn,
        claim_for=claim_fn,
        node_state_for=lambda node: graph_fn().get(node),
        now_s=now_s,
    )
    counts: dict[str, int] = {}
    for v in vs:
        counts[v.verdict] = counts.get(v.verdict, 0) + 1
    payload = {
        "generated_at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "verdicts": [v._asdict() for v in vs],
        "counts": counts,
        "warnings": warnings,
    }
    return payload, rows


def emit_event(kind: str, data: dict) -> None:
    """Best-effort schema-validated event on the global events.jsonl (the same
    path the pr_watch tick writes). A miss is swallowed: telemetry never breaks
    a sweep."""
    try:
        from fno import paths
        from fno.events import _build, append_event

        append_event(
            _build(kind, "daemon", data), paths.state_dir() / "events.jsonl"
        )
    except Exception:  # noqa: BLE001 - telemetry must never break the sweep
        pass


def sweep_path() -> Path:
    from fno import paths

    return paths.state_dir() / "watchdog-sweep.json"


def write_sweep_file(
    source: str, counts: dict, now_s: float, signature: str = ""
) -> None:
    """Freshness evidence for the done probe: one small state file per sweep,
    best-effort (an unwritable state root must never break a tick). The
    ``signature`` of the non-leave verdict set rides along so the mail lane
    can skip a digest that says exactly what the last one said."""
    try:
        path = sweep_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps({
                "source": source,
                "at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
                    "%Y-%m-%dT%H:%M:%SZ"
                ),
                "counts": counts,
                "signature": signature,
            }),
            encoding="utf-8",
        )
    except OSError:
        pass


def _last_signature() -> str:
    try:
        return str(json.loads(sweep_path().read_text(encoding="utf-8")).get("signature") or "")
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return ""


def verdict_signature(payload: dict) -> str:
    """Stable identity of the non-leave verdict set (row_id:verdict, sorted).
    Two sweeps that agree on the fleet produce one signature, so the mail lane
    speaks only on change - a row stuck for a day reads once, not 72 times."""
    parts = sorted(
        f"{v['row_id']}:{v['verdict']}"
        for v in payload["verdicts"]
        if v["verdict"] != LEAVE
    )
    return ";".join(parts)


def digest_text(payload: dict, limit: int = 8) -> str:
    """One-screen digest of a sweep, house-style (one physical line per
    paragraph). The basis rides along so the king can falsify each call."""
    total = len(payload["verdicts"])
    counts = " ".join(f"{k}={v}" for k, v in sorted(payload["counts"].items()))
    lines = [f"fleet watchdog swept {total} rows. {counts}"]
    non_leave = [x for x in payload["verdicts"] if x["verdict"] != LEAVE]
    for v in non_leave[:limit]:
        lines.append(f"{v['verdict']} {v['name']}: {v['basis']}")
    more = len(non_leave) - limit
    if more > 0:
        lines.append(f"{more} more row(s) not shown.")
    return "\n".join(lines)


def mail_digest(
    payload: dict, to: str, *, runner: Callable = subprocess.run
) -> tuple[bool, str]:
    """Push the verdict to a mail handle (push, not pull: a verdict the king
    has to remember to fetch goes unread). Skipped without comment when the
    non-leave set is unchanged since the last sweep. A ``project:<slug>``
    recipient addresses the project mailbox instead of one agent."""
    if not to:
        return False, "no recipient configured"
    non_leave = [v for v in payload["verdicts"] if v["verdict"] != LEAVE]
    if not non_leave:
        return True, "all rows leave, nothing to say"
    if verdict_signature(payload) == _last_signature():
        return True, "unchanged since the last sweep, not mailed"
    argv = [*_fno(), "mail", "send"]
    if to.startswith("project:"):
        argv += ["--to-project", to[len("project:"):]]
    else:
        argv.append(to)
    argv.append(digest_text(payload))
    try:
        proc = runner(argv, capture_output=True, text=True, timeout=60, check=False)
    except (OSError, subprocess.SubprocessError) as exc:
        return False, f"mail send failed: {exc}"
    ok = proc.returncode == 0
    return ok, (proc.stdout or proc.stderr or "").strip()


# ---------------------------------------------------------------------------
# Apply lanes (the watchdog owns the decision, never the mechanism)
# ---------------------------------------------------------------------------

def _fno() -> list[str]:
    from fno import _subprocess_util

    return [*_subprocess_util.fno_py_cmd()]


def worktree_refusal(cwd: str) -> Optional[str]:
    """Why this worktree may NOT be reaped, or None when it is clean. Named so
    the refusal is actionable: unstaged changes and unpushed commits are work
    that exists nowhere else. A branch with no upstream reads as unpushed -
    a worktree whose commits are not on any remote is exactly that."""
    if not cwd:
        return "no recorded cwd"
    try:
        dirty = subprocess.run(
            ["git", "-C", cwd, "status", "--porcelain"],
            capture_output=True, text=True, timeout=30, check=False,
        )
        n_dirty = len([l for l in (dirty.stdout or "").splitlines() if l.strip()])
        if n_dirty:
            return f"{n_dirty} uncommitted change(s) in {cwd}"
        unpushed = subprocess.run(
            ["git", "-C", cwd, "rev-list", "--count", "@{upstream}..HEAD"],
            capture_output=True, text=True, timeout=30, check=False,
        )
        if unpushed.returncode != 0:
            return f"no upstream to compare in {cwd}"
        n = (unpushed.stdout or "").strip()
        if n and n != "0":
            return f"{n} unpushed commit(s) in {cwd}"
    except (OSError, subprocess.SubprocessError) as exc:
        return f"worktree check failed: {exc}"
    return None


def confirm_wake_landed(
    row_id: str, cwd: str, message: str, before_epoch: Optional[float]
) -> bool:
    """The message must appear in the recipient transcript AFTER the pre-wake
    marker. The state field is not evidence: ``wake.sh`` printed
    ``working -> working`` for both a message that landed and one that did not
    (the same content-not-state contract as mail_inject's confirm_content_after)."""
    facts = tail_facts(row_id, cwd)
    if facts is None:
        return False
    for epoch, text in facts.records:
        if message not in text:
            continue
        if before_epoch is None or epoch is None or epoch > before_epoch:
            # before_epoch None (no parseable pre-wake stamp) degrades to a
            # presence check: still content, never a state field.
            return True
    return False


#: Which verdicts each apply level may execute. ``wake`` is the one lane that
#: cannot destroy work, so bare ``--apply`` stops there; reap and reroute both
#: stop a session, so they need ``--apply=all``. ghost NEVER auto-acts: the
#: remedy is a respawn under a new id, which is the operator's call.
LANES = {"wake": frozenset({WAKE}), "all": frozenset({WAKE, REROUTE, REAP})}


def apply_verdict(
    v: Verdict,
    *,
    lanes: str,
    cwd: str = "",
    runner=subprocess.run,
    redispatch_fn: Optional[Callable[[Any], bool]] = None,
) -> tuple[str, str]:
    """Execute one verdict inside ``lanes`` ("wake" | "all"). Returns
    ``(outcome, detail)`` with outcome in applied | refused | reported.
    Mechanisms delegate: resume (which verifies the state move and holds its
    own single-writer claim), recovery._redispatch for reroute, stop + rm for
    reap - rm is never forced, ``claude rm``'s own refusal on a dirty worktree
    is a safety feature this lane leans on rather than bypasses."""
    if v.verdict not in LANES.get(lanes, frozenset()):
        return "reported", f"{v.verdict} outside {lanes} lane"
    try:
        if v.verdict == WAKE:
            return _apply_wake(v, cwd=cwd, runner=runner)
        if v.verdict == REROUTE:
            return _apply_reroute(v, cwd=cwd, redispatch_fn=redispatch_fn)
        if v.verdict == REAP:
            return _apply_reap(v, cwd=cwd, runner=runner)
    except (OSError, subprocess.SubprocessError) as exc:
        return "refused", f"{v.verdict} action failed: {exc}"
    return "reported", f"{v.verdict} has no auto-action"


def _apply_wake(v: Verdict, *, cwd: str, runner: Callable) -> tuple[str, str]:
    before = tail_facts(v.row_id, cwd)
    before_epoch = before.last_event_epoch if before is not None else None
    proc = runner(
        [*_fno(), "agents", "resume", v.row_id, "--message", WAKE_MESSAGE],
        capture_output=True, text=True, timeout=180, check=False,
    )
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "").strip().splitlines()
        return "refused", f"resume exit {proc.returncode}: {tail[-1] if tail else ''}"
    if not confirm_wake_landed(v.row_id, cwd, WAKE_MESSAGE, before_epoch):
        return (
            "refused",
            f"resume reported success but {WAKE_MESSAGE!r} is not in the "
            f"transcript after the wake",
        )
    return "applied", f"woke {v.name}; message confirmed in transcript"


def _apply_reroute(
    v: Verdict, *, cwd: str, redispatch_fn: Optional[Callable[[Any], bool]]
) -> tuple[str, str]:
    from fno.recovery import Candidate, _redispatch

    if not cwd:
        return "refused", "reroute refused: no recorded worktree to respawn into"
    fn = redispatch_fn or _redispatch
    candidate = Candidate(
        short_id=v.row_id[:8], sock_path="", jobs_dir=None,
        cwd=cwd, name=v.name,
    )
    if fn(candidate):
        return "applied", f"stopped and respawned via redispatch ({v.basis})"
    return "refused", f"redispatch declined ({v.basis}); session left as-is"


def _apply_reap(v: Verdict, *, cwd: str, runner: Callable) -> tuple[str, str]:
    refusal = worktree_refusal(cwd)
    if refusal:
        return "refused", f"reap refused: {refusal}"
    stopped = runner(
        [*_fno(), "agents", "stop", v.row_id],
        capture_output=True, text=True, timeout=60, check=False,
    )
    if stopped.returncode != 0:
        return "refused", f"stop exit {stopped.returncode}: {(stopped.stderr or '').strip()[:200]}"
    removed = runner(
        [*_fno(), "agents", "rm", v.row_id],
        capture_output=True, text=True, timeout=60, check=False,
    )
    if removed.returncode != 0:
        return (
            "refused",
            f"rm exit {removed.returncode} (registry row kept): "
            f"{(removed.stderr or '').strip()[:200]}",
        )
    return "applied", f"stopped and removed ({v.basis})"

