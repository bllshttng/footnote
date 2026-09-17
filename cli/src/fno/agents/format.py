"""fno.agents.format — the Python registry-row serializer for `fno agents`.

``serialize_entry`` turns an :class:`AgentEntry` into the row shape pinned by
``schemas/agents-list-row.json``. The served ``fno agents list`` renders in the
Rust client; there is no Python list renderer.
"""
from __future__ import annotations

from typing import Optional

from fno.agents.registry import AgentEntry
from fno.agents.row_contradiction import project_row
from fno.agents.session_truth import STALE_ATTENTION_S

# Basis values that are falsifiers rather than evidence: a positive
# measurement that the worker is gone, which no other reading outranks.
_FALSIFIER_BASES = {"process-gone", "pane-gone"}


def attention_rank(row: dict) -> int:
    """Evidence tier for one serialized row: 0 needs the operator most.

    Built only from fields that carry their evidence with them (``basis``,
    ``last_activity_age_s``) - never from ``status`` or a bare verdict word,
    both of which read healthy for a worker dead under two hours. Mirrors the
    daemon's ``attention_sort_key`` tier set; the shared fixture in
    ``schemas/agents-attention-order.json`` is what pins the two together.
    """
    basis = row.get("basis")
    if basis in _FALSIFIER_BASES or row.get("reachability") == "unreachable":
        return 5
    age = row.get("last_activity_age_s") or 0
    if basis == "transcript" and age >= STALE_ATTENTION_S:
        return 0
    if basis == "silent":
        return 1
    if basis == "no-evidence":
        return 2
    return 4


def attention_sort_key(row: dict) -> tuple:
    """Attention order: evidence tier, then longest-silent first, then name.

    An absent age counts as 0 (youngest): an absent reading has two
    explanations and a sort cannot tell them apart, so it must never float a
    row to the top.
    """
    age = row.get("last_activity_age_s") or 0
    return (attention_rank(row), -age, str(row.get("name") or ""))


def row_address(
    harness: Optional[str],
    harness_session_id: Optional[str],
    short_id: Optional[str] = None,
) -> Optional[str]:
    """The mailbox address a row can actually receive at.

    One derivation, reached by both lanes (registry rows via
    :func:`serialize_entry`, discovered rows via the table renderer) so the two
    cannot advertise different addresses for the same session. The Rust
    ``agent.list`` projection carries a parity-pinned mirror because it cannot
    import Python; ``schemas/agents-list-row.json`` is what keeps them honest.

    ``short_id`` is a fallback for claude ONLY, where the transport key IS the
    first eight. A codex or opencode ``short_id`` is a daemon worker key, so
    using it here would advertise a mailbox nothing drains - the exact failure
    this column exists to stop.
    """
    from fno.harness_identity import canonical_handle

    if harness_session_id:
        return canonical_handle(harness_session_id)
    if harness == "claude" and short_id:
        return short_id
    return None


def _dnd_label(entry: AgentEntry) -> Optional[str]:
    """This row's do-not-disturb state for the DND column, or None.

    None whenever mail flows right now, so a row whose hold already lapsed
    reads the same as a row that never had one. Both are states in which a
    message lands, and the column exists to answer that question, not to
    report a flag nobody cleaned up.

    A bus-only row with no clock reads ``held``, not blank. Mail to it really
    is being held, indefinitely, and a blank cell there would be the column
    lying about the one row it exists to describe. ``dnd_label`` derives that
    from the same ``lapsed`` the delivery gate uses, so the two agree by
    construction rather than by matching edits.
    """
    if getattr(entry, "delivery_policy", None) != "bus-only":
        return None
    try:
        from fno.mail import hold as _hold

        # The ENTRY, not `entry.name`. A codex row's name is a spawn label and
        # its clock sits under the canonical handle, so keying by the name read
        # a blank cell for a held codex session.
        return _hold.dnd_label(entry)
    except Exception:  # noqa: BLE001 - a render helper never breaks the listing
        # "?" and not None. None renders as "-", which is the cell a row with
        # no hold gets, so a failed read would tell the operator mail is
        # flowing to a session whose flag says it is held. An instrument that
        # declines to answer must not answer anyway.
        return "?"


def _model_substitution_marker(
    requested: Optional[str], observed_model: Optional[dict]
) -> Optional[dict]:
    """The row's substitution marker, or None on match-or-unknown.

    One shared verdict (`row_contradiction.model_substitution`) decides; this
    wrapper only shapes the positive marker the contract wants: BOTH values,
    or nothing. None is deliberately the unknown shape too - a row whose
    check has not answered must not read as clean.
    """
    from fno.agents.row_contradiction import model_substitution

    if model_substitution(requested, observed_model) == "substituted":
        return {
            "requested": requested,
            "observed": observed_model.get("model") if observed_model else None,
        }
    return None


def serialize_entry(
    entry: AgentEntry,
    live_status: Optional[str],
    observed_model: Optional[dict] = None,
    reachability: Optional[str] = None,
    basis: Optional[str] = None,
    progress: Optional[str] = None,
    progress_basis: Optional[str] = None,
    last_activity_age_s: Optional[int] = None,
    last_activity_basis: Optional[str] = None,
    last_event_at: Optional[str] = None,
    last_message: Optional[str] = None,
    status: Optional[str] = None,
    superseded_live_status: Optional[str] = None,
) -> dict:
    """Produce the canonical dict shape for one agent.

    Returns the same key set for every provider so JSON consumers can
    iterate a list of agents without per-provider branching (AC3-HP).
    The key set is pinned by ``schemas/agents-list-row.json``, which the
    Rust daemon's ``agent.list`` projection is asserted against too — this
    function is NOT what serves ``fno agents list``, so the two have drifted
    before and only the shared contract file keeps them honest.
    ``short_id`` is the provider transport key (claude jobId or daemon
    worker key; null when absent). ``session_id`` is the unified,
    provider-resolving resume-target id: ``short_id`` for claude, ``codex_session_id``
    for codex, ``gemini_session_id`` for gemini. It surfaces the codex
    resume UUID — the argument ``codex resume`` / ``fno agents resume``
    consume — which was previously stored but invisible in list output.

    ``live_status`` is the orthogonal "what is claude's supervisor saying
    right now" signal. It is ``None`` for non-Claude entries and for
    Claude entries when the ``claude agents --json`` shellout failed or
    omitted the entry.

    ``observed_model`` is the five-variant reading from
    :func:`fno.provenance.observed.observed_model` -- what the worker is
    ACTUALLY answering as, derived from its own transcript rather than from
    anything the spawn recorded. Defaulted rather than required so a caller
    that has no truth reading still produces the full key set; the default is
    the same ``no-transcript`` the resolver reports when it finds no file.
    """
    row = {
        "name": entry.name,
        # `harness` is the sole identity axis. `provider` beside it is the
        # v15+ model-vendor axis, stamped at spawn and never inferred from
        # harness. The pre-split alias that carried the HARNESS value under
        # this name stayed gone until, which left the list answering
        # null for a field the writer stored; `observed_model` below remains
        # the transcript-derived answer to what actually answered.
        "harness": entry.harness,
        "provider": entry.provider,
        # The selected reasoning-effort axis, recorded at spawn and passed
        # through verbatim. It is not a transcript observation and is not
        # inferred from harness, provider, or observed_model.
        "effort": entry.effort,
        # The worker's own session id in its harness's store. Distinct from
        # `session_id` (the resume-target id, which is the 8-hex jobId for
        # claude) and from `short_id` (the transport key).
        "harness_session_id": entry.harness_session_id,
        # The lane the row was spawned on ("pane"|"thread"|"headless"), read
        # from the registry record and never inferred from mux or thread_id:
        # a paneless pane row and a thread row would then read identically.
        "substrate": getattr(entry, "substrate", None),
        # The two identity axes, stated explicitly: `thread_id` is
        # the stable fno identity one worker keeps across succession, and
        # `current_session_id` is the address delivery follows NOW. They are
        # emitted separately so a renderer that sourced current identity
        # from the thread id, pane metadata, or the first predecessor fails
        # the positive assertion instead of passing silently.
        "thread_id": getattr(entry, "fno_id", None),
        "current_session_id": entry.harness_session_id,
        # The node this row works, already stamped in registry storage from
        # resolved spawn provenance. Never infer it from the row name.
        "node": entry.node,
        # Classified lineage: the succession chain A->B->... and the fork
        # edge of a parallel branch. Empty/None for a worker never re-minted
        # and never forked - the dominant case.
        "predecessor_session_ids": list(
            getattr(entry, "predecessor_session_ids", None) or []
        ),
        "forked_from_session_id": getattr(entry, "forked_from_session_id", None),
        "short_id": entry.short_id or None,
        "session_id": entry.session_id,
        # The one identifier in this row that mail can be sent to. Every other
        # one names something else: `name` is a spawn label, `short_id` is a
        # transport key and is null for most rows, `session_id` is a resume
        # target. A reader with no address column copies `name`, and a name-lane
        # durable write queues under a key no drain reads.
        "address": row_address(
            entry.harness, entry.harness_session_id, entry.short_id or None
        ),
        "cwd": entry.cwd,
        "created_at": entry.created_at,
        "last_message_at": entry.last_message_at,
        "last_message_at_basis": None,
        "last_reconciled_at": entry.last_reconciled_at,
        # The caller's rendered word when it has one (the reachability wire
        # vocabulary), else the registry's stored token. It has to arrive
        # BEFORE the projection, not be patched on after: the contradiction
        # rules read `status`, and a caller that patches then re-projects
        # loses `liveness_origin`, because `pid` is popped below.
        "status": status if status is not None else entry.status,
        "live_status": live_status,
        # Internal input to the shared projection rule (popped there, like
        # `pid`): the non-idle supervisor word a fired falsifier superseded.
        "superseded_live_status": superseded_live_status,
        # The model the worker is answering as, read from its transcript. A
        # spawn-recorded route would report the INTENDED model in exactly the
        # case an operator suspects a silent fallback; this cannot.
        "observed_model": observed_model or {"kind": "no-transcript"},
        # v23: the REQUEST verbatim beside the observation, so a
        # silent substitution is a one-line diff a reader makes from the list
        # alone. `requested_model` is write-once at birth; `model_substituted`
        # is derived HERE from the same observed payload this row already
        # carries, so a substitution that surfaces minutes after spawn is
        # visible on the next list read with no new probe. Unknown (either
        # side absent/unreadable) renders null - never a fabricated "match",
        # which is how silence passes for health.
        "requested_model": getattr(entry, "requested_model", None),
        "model_substituted": _model_substitution_marker(
            getattr(entry, "requested_model", None), observed_model
        ),
        # a field fno already modelled, consumed internally, and never
        # showed. `delivery_policy` (registry.py, schema v14) decides whether
        # mail to this row may ever paste into its prompt line - readable by
        # twelve call sites and invisible to the human deciding.
        # `dnd` is the derived half, so a do-not-disturb row shows when it ends
        # rather than a bare yes. Four values, all of which a consumer must
        # handle: a duration, `held` for no recorded expiry, `?` for a clock
        # that could not be read, and null when mail is flowing despite the
        # flag. The clock is read only for a row carrying the flag, so the
        # common case pays no file read at all.
        "delivery_policy": entry.delivery_policy,
        "dnd": _dnd_label(entry),
        "log_path": entry.log_path,
        # Crown (US9): a compact "L1 epic-x" descriptor + the raw fields, so a
        # minion can resolve who to escalate to and a second live crown over the
        # same scope is detectable. null for an uncrowned row.
        "crown": entry.crown_label,
        "crown_level": entry.crown_level,
        "crown_scope": entry.crown_scope,
        "crown_grantor": entry.crown_grantor,
        # The parent edge the orphan check keys on. Null is a real answer (an
        # ambiguous identity resolve records no lineage rather than a wrong
        # one): this worker is invisible to its spawner's orphan check.
        "spawned_by_session": getattr(entry, "spawned_by_session", None),
        "lineage_kind": getattr(entry, "lineage_kind", None),
        # How this session came to exist: "operator" for one a human started by
        # hand, "spawn" for a footnote-created worker, null for a row nothing
        # stamped. The reap lane REFUSES on "operator", so a human auditing that
        # refusal has to be able to read the field it turned on - and until
        # this projection did not emit it at all, which left the one
        # answer to "is somebody sitting in this?" visible to nobody.
        "origin": entry.origin,
        # The mux hosting ref ({session, pane_id}) for a pane-hosted row, else
        # null. Exposed so a caller can address the pane - e.g. close a handed-off
        # teammate with `fno mux pane kill <session>:<pane_id>` (a mux row's
        # short_id is empty, so `fno agents stop` refuses it).
        "mux": entry.mux,
        # The shared reachability verdict and the evidence it was reached from
        # (fno.agents.reachability). Emitted here rather than bolted onto the row
        # by the caller so the key set stays pinned by the shared contract file —
        # a key that exists only on the path that happened to answer is the drift
        # this contract was written to stop.
        "reachability": reachability,
        "basis": basis,
        # The orthogonal axis: reachability answers "can I reach this
        # process"; progress answers "is it advancing, awaiting the operator,
        # parked, or refused" -- a question a refused-but-reachable worker
        # needs answered separately (fno.agents.reachability.classify_progress).
        "progress": progress,
        "progress_basis": progress_basis,
        "last_activity_age_s": last_activity_age_s,
        # The instrument the age came from (`last-entry` | `mtime` |
        # `opencode-db`), or the resolver's reason word (`not-found` |
        # `no-records` | `resolver-error`) when it could not resolve the
        # handle at all - never a bare null: those words separate "no
        # transcript" from "the resolver crashed".
        "last_activity_basis": last_activity_basis,
        # text of the LAST turn. Both come from the same truth probe as
        # the age above, so a reader can see WHAT the worker last did and WHEN -
        # a `working` row whose stamp is hours old is the wedged-worker signal
        # no store could express. Null when the probe never answered: an absent
        # reading is never a fresh one.
        "last_event_at": last_event_at,
        "last_message": last_message,
        # Internal inputs to the shared projection rule; both are removed before
        # the row reaches the wire because pid identity is not a row verdict.
        # `pid` has to be here even though this serializer never publishes it:
        # the shared rule gates on it FIRST, so a row that omits it reads
        # `liveness_origin: null` with basis `pid-absent` forever, and the field
        # is dead on the very path `fno agents list` serves.
        "pid": entry.pid,
        "pid_start_time": entry.pid_start_time,
    }
    row = project_row(row)
    row.pop("pid_start_time", None)
    row.pop("pid", None)
    return row
