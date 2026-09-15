"""Owned ambient identity resolution for core and runtime callers."""

from __future__ import annotations

import os
import re
from dataclasses import replace
from pathlib import Path
from typing import Callable, Mapping, Optional, Tuple, Union

from fno.harness_identity import (
    live_thread_row_for_cwd,
    parse_canonical_identity,
    present_harness_markers,
    resolve_attester_identity,
    resolve_owned_identity,
    session_identity_key,
)


def resolve_self_identity(
    env: Optional[Mapping[str, str]] = None,
    *,
    collide: Optional[
        Callable[[str, str, Optional[Tuple[str, str]]], Optional[str]]
    ] = None,
    witness: Optional[Callable[[str], frozenset]] = None,
):
    """Resolve the harness identity this process can prove it owns.

    The prover is the process-tree walk, and it is the only prover for an
    AMBIENT marker: the nearest harness ancestor separates a marker this
    session minted from one it merely inherited. A self-set marker such as
    ``CLAUDECODE`` is never a prover - it survives a fork, so a codex session
    under a shell that ran claude inherits it, and env alone cannot tell the
    two cases apart (see
    :data:`fno.harness_identity.SELF_SET_HARNESS_MARKERS`). When the walk has
    no answer, resolution refuses rather than guesses, except for the
    uncontended single-family elimination; a walk that cannot tell is
    "cannot tell" (``None``), never a contradiction (``False``) - only a walk
    that found a DIFFERENT harness contradicts (: returning False on a
    silent walk refused every spawned worker whose ancestry the sandbox
    hides).

    The spawn record is a separate, narrower source that fills a session id
    ancestry structurally cannot supply: a codex thread worker owns no
    process, and the daemon-written registry row keyed by this process's cwd
    is the only per-worker identity the lane has. See
    :func:`_fill_spawn_record`, which runs after the walk and never
    overwrites a proven session id.

    ``collide(harness, session_id, own_pair) -> owner | None`` reports a live
    registry row owning an id. ``own_pair`` is this process's own
    ``(harness, session_id)`` pair, or None when nothing proves an id: a row
    agreeing with the pair on both halves is the caller's OWN row and never
    contention. The id half must come from the STAMP or a witness, never from
    the ambient marker under test - that pair would assert exactly what the
    marker claims, and a leaked marker meeting its owner's live row would
    read as self (round-1 P1). A name_only worker resolves when the attester
    witnesses its marker from ancestry, or the rollout witness sees its id in
    a live fd, and fails closed otherwise. The agreement check stays in the
    registry; this layer computes the pair and hands it over.

    ``witness(harness) -> frozenset[session_id]`` names the session ids a live
    rollout fd witnesses for this process (see
    ``fno.agents.codex_rollout.codex_rollout_witness``). The id comes from the
    fd, not the marker under test, so completing a name_only stamp's pair with
    it is not circular.
    """
    from fno.claims.session_pid import resolve_session_harness

    true_harness = resolve_session_harness()
    canonical = parse_canonical_identity(env)

    def collide_with(pair: Optional[Tuple[str, str]]) -> Callable[[str, str], Optional[str]]:
        # The 3-arg collide (with own_pair) is THIS resolver's contract; the
        # shared resolve_owned_identity takes the 2-arg shape.
        def _collide(harness: str, session_id: str) -> Optional[str]:
            if collide is None:
                return None
            return collide(harness, session_id, pair)

        return _collide

    if canonical.disposition not in {"complete", "name_only"}:
        fallback_prove = (
            None if true_harness is None else (lambda harness, sid: harness == true_harness)
        )
        # No collide here: a session with no stamp at all (a hand-started
        # joined session) resolves by the uncontended single-family
        # elimination, exactly as the registry's own SessionStart
        # registration expects - colliding would read that session's OWN
        # registered row as contention and refuse every crown grantor,
        # whoami and --from-self for it. The fail-closed collide lives in
        # the stamped branches below, where an attester can still witness
        # self.
        return _fill_spawn_record(resolve_owned_identity(env, prove=fallback_prove))

    try:
        attested_session_id, attester_witness = resolve_attester_identity(env)
    except Exception:
        attested_session_id, attester_witness = "", ""
    canonical_session_id = canonical.session_id or attested_session_id
    canonical_proven = bool(
        true_harness
        and canonical.harness == true_harness
        and attester_witness == "process"
        and canonical_session_id
        and attested_session_id
        and session_identity_key(canonical_session_id)
        == session_identity_key(attested_session_id)
    )

    # a name_only codex stamp carries no id and codex never carries
    # CODEX_THREAD_ID in its own env, so the attester cannot complete the
    # pair. A marker value the rollout witness sees in a live fd IS this
    # process's id; the thread id wins (CODEX_SESSION_ID is the ROOT session).
    witnessed_value: Optional[str] = None
    if (
        witness is not None
        and not canonical_proven
        and canonical.disposition == "name_only"
        and true_harness
        and canonical.harness == true_harness
    ):
        environ_w = os.environ if env is None else env
        seen = {session_identity_key(s) for s in witness(true_harness)}
        thread_value = (environ_w.get("CODEX_THREAD_ID") or "").strip()
        if thread_value and session_identity_key(thread_value) in seen:
            witnessed_value = thread_value
        else:
            witnessed = [
                value
                for _marker, harness, value in present_harness_markers(environ_w)
                if harness == true_harness and session_identity_key(value) in seen
            ]
            if len(witnessed) == 1:
                witnessed_value = witnessed[0]
        if witnessed_value:
            canonical_session_id = witnessed_value
            canonical_proven = True

    def prove(harness: str, session_id: str) -> Optional[bool]:
        if true_harness is None:
            return None
        if harness != true_harness:
            return False
        if not canonical_proven:
            return None
        return session_identity_key(session_id) == session_identity_key(canonical_session_id)

    # The stamp-declared pair: a COMPLETE stamp names the id independently of
    # the markers under test (spawn writes the stamp and the row in one act),
    # which is the non-circular ground. A name_only stamp names only
    # the family and completes NO pair - the ambient marker would be
    # self-attesting, and own_pair stays None so the collider keeps its full
    # ambient-leak strength there.
    own_pair: Optional[Tuple[str, str]] = None
    if canonical.harness and canonical.session_id:
        own_pair = (
            canonical.harness.strip().lower(),
            session_identity_key(canonical.session_id),
        )
    elif witnessed_value and true_harness:
        own_pair = (true_harness, session_identity_key(witnessed_value))

    return _fill_spawn_record(
        resolve_owned_identity(
            env,
            prove=prove,
            collide=None if canonical_proven else collide_with(own_pair),
        )
    )


def _fill_spawn_record(owned):
    """Fill a session id the walk could not supply from the cwd-keyed spawn
    record.

    Guards, in order: a resolved session id short-circuits before the read, a
    fail-closed disposition is never overwritten, and a process carrying ANY
    other family's marker never adopts - cwd is shared by bystanders.
    """
    if owned.session_id or owned.disposition in {"invalid", "contradiction"}:
        return owned
    row = live_thread_row_for_cwd(os.getcwd())
    if row is None:
        return owned
    harness, session_id = row
    if owned.harness and owned.harness != harness:
        return owned
    marker_families = {h for _m, h, _v in owned.markers_present}
    if marker_families - {harness}:
        return owned
    return replace(
        owned, harness=harness, session_id=session_id, disposition="spawn_record"
    )


#: Manifest body/frontmatter fields that carry an identity every fno process
#: in the worktree can read. Read directly rather than through
#: ``fno.target.manifest`` - claims sits at the bottom of the stack and must
#: not import the target layer.
_MANIFEST_IDENTITY_FIELDS = (
    "harness_session_id",
    "claude_session_id",
    "session_id",
    "fno_id",
)

#: Dispositions of :class:`fno.harness_identity.OwnedHarnessIdentity` whose
#: session id is PROVEN by this process's own ancestry. Every other
#: disposition with an id present is an inherited marker, and an inherited
#: marker matching the worktree manifest is a shared anchor, not a self
#: (``spawn_record`` stays absent: see :func:`_fill_spawn_record`).
_PROVEN_DISPOSITIONS = frozenset({"canonical", "proven"})


def _manifest_identity_values(project_root: Optional[Path]) -> frozenset:
    """The manifest's identity values, found by walking UP from CWD.

    The caller may run from a subdirectory of the worktree, so a bare
    ``cwd/.fno`` read would silently miss the manifest and wave a shared
    anchor through. The walk stops at the first ``.fno/target-state.md``, and
    never climbs past a repository root (the ``.git`` marker) - a stray
    manifest ABOVE the project must not anchor lookups inside it. Worktree
    roots carry ``.git`` as a file, plain repos as a directory, so the marker
    check accepts both. No git subprocess: claims stays at the bottom of the
    stack.
    """
    start = Path(project_root) if project_root else Path.cwd()
    for directory in (start, *start.parents):
        manifest = directory / ".fno" / "target-state.md"
        try:
            text = manifest.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            text = None
        if text is not None:
            values = []
            for field in _MANIFEST_IDENTITY_FIELDS:
                m = re.search(rf"^{field}\s*:\s*(.+)$", text, re.MULTILINE)
                if m:
                    val = m.group(1).strip().strip("\"'")
                    if val and val != "null":
                        values.append(val)
            return frozenset(values)
        if (directory / ".git").exists():
            break
    return frozenset()


def resolve_task_holder(
    env: Optional[Mapping[str, str]] = None,
    *,
    project_root: Optional[Union[str, Path]] = None,
) -> Tuple[Optional[str], str]:
    """Resolve the holder for a task-grain claim, or name why it cannot.

    Returns ``(holder, "")`` or ``(None, refusal_reason)``; the caller turns a
    refusal into an exit-4 identity failure. Three identities are acceptable:

    1. ``FNO_WORKER_NAME``: the roster name ``fno agents spawn`` exports into
       the worker it launches. A spawned worker can prove the name is its own
       because its parent minted it specifically for this child, and two
       siblings in one worktree carry two names - the collapse this resolver
       exists to break.
    2. The roster name bound to this session id in the agents registry (the
       spawn-time name<->session binding; see
       :func:`_roster_name_for_session`). The env export in 1 cannot reach a
       daemon-forked worker, whose serving session inherits the daemon's env;
       the registry row survives that fork and proves the same fact.
    3. The ambient session id, when this process PROVES it (process-tree
       ancestry) or the marker is at least not the worktree manifest's shared
       value. The manifest is read by every fno process in the directory, so
       an identity that only matches it is a shared anchor: refusing names the
       fix (``--owner`` or spawn through the roster) instead of attributing a
       stranger's work.
    """
    environ = os.environ if env is None else env
    name = (environ.get("FNO_WORKER_NAME") or "").strip()
    if name:
        return name, ""
    ident = resolve_self_identity(env)
    if not ident.session_id or not ident.harness:
        return None, "no provable session identity"
    if ident.disposition not in _PROVEN_DISPOSITIONS:
        root = Path(project_root) if project_root else None
        if ident.session_id in _manifest_identity_values(root):
            return None, (
                "the only provable identity is the worktree manifest's shared "
                "session id, which every fno process in this directory reads"
            )
    roster = _roster_name_for_session(ident.session_id)
    if roster:
        return roster, ""
    return ident.session_id, ""


def _roster_name_for_session(session_id: str) -> str:
    """The roster name bound to this harness session id, or ``""``.

    ``fno agents spawn`` writes the name->harness_session_id binding into the
    registry at spawn time, so a row naming this exact session id proves the
    name is this worker's own - the same guarantee the ``FNO_WORKER_NAME``
    export was meant to give. The env write cannot reach a daemon-forked
    worker: the serving session inherits the claude daemon's env, never the
    spawning process's, so a spawned worker's holder degraded to a
    raw session id (live join proof, 2026-08-27). The registry row survives
    that fork. Best-effort by design: an unreadable registry, a missing row,
    or an ambiguous session id (two names) answers "" and the caller keeps
    the previous resolution unchanged.

    The registry FILE is read directly (paths -> json) rather than through
    ``fno.agents.registry``: that module is L5 runtime and this resolver is
    L1 core, a new edge the boundary gate refuses. The coupled shape is the
    documented store layout - ``{"schema_version": N, "agents": [row]}`` with
    the session-id fields per row - and the read degrades to "" on anything
    else, so an L5-side schema change cannot crash identity resolution.
    """
    sid = (session_id or "").strip()
    if not sid:
        return ""
    try:
        import json

        from fno.paths import agents_registry_path

        raw = json.loads(agents_registry_path().read_text(encoding="utf-8"))
    except Exception:  # noqa: BLE001 - identity must degrade, never crash
        return ""
    rows = raw.get("agents") if isinstance(raw, dict) else None
    if not isinstance(rows, list):
        return ""
    names = {
        str(row["name"])
        for row in rows
        if isinstance(row, dict) and row.get("name")
        and sid in {
            str(row.get(k) or "").strip()
            for k in ("harness_session_id", "cc_session_id", "session_id")
        }
    }
    if len(names) == 1:
        return next(iter(names))
    return ""

