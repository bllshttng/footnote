"""Per-provider runtime state for exponential-backoff failover.

Plan A of provider failover hardening (ab-6534a78a). Distinct from the
phase-scoped ``failover-state.json`` (storm-cap, no-swap-back): this
file owns per-provider exponential backoff state that survives target
spawns within a megawalk campaign.

State path: ``.fno/provider-runtime-state.json`` (project-local).
Override via ``FNO_RUNTIME_STATE_PATH`` env var (used by tests).

Concurrency: writes serialize via filelock (which dispatches to
``fcntl.flock`` on Unix) on a sidecar lockfile. Reads acquire the same
lock for a brief shared-style window via ``read_text`` after the write
side has committed via ``os.replace``.

TTL: ProviderHealth entries are dropped lazily on next read when
``last_error_at`` is older than ``PROVIDER_HEALTH_TTL_SECONDS`` (1h).
Lazy clear writes the file as a side effect of read, so callers MUST
NOT assume read is side-effect-free.

Plan B (Spec 4) will add ``combo_cursors: dict[str, ComboCursor]`` to
the same top-level state dataclass; ``schema_version: int`` is bumped
when that lands.
"""
from __future__ import annotations

import dataclasses
import json
import logging
import os
import tempfile
import time
from pathlib import Path
from typing import Any

import filelock

from fno.adapters.providers.error_taxonomy import ErrorRule

logger = logging.getLogger(__name__)


BASE_BACKOFF_MS = 2_000
MAX_BACKOFF_MS = 5 * 60 * 1000
MAX_BACKOFF_LEVEL = 15
PROVIDER_HEALTH_TTL_SECONDS = 60 * 60
COMBO_CURSOR_TTL_SECONDS = 24 * 60 * 60  # Plan B (Spec 4): cursor goes stale after 24h
RUNTIME_STATE_PATH = ".fno/provider-runtime-state.json"
LOCK_TIMEOUT_SECONDS = 5
SCHEMA_VERSION = 2  # Plan B: combo_cursors added; v1 files migrate transparently on read


@dataclasses.dataclass(frozen=True)
class ProviderHealth:
    """Per-provider backoff state.

    ``backoff_level`` is 0 immediately after a successful call (see
    reset_provider_health) and increments by 1 on each consecutive
    backoff-class error, capped at ``MAX_BACKOFF_LEVEL`` (15).

    ``model_locks`` (Plan A1, ab-7fe3cdaf) is a dict mapping
    model identifier -> unix-epoch-seconds cooldown expiry. It lets a
    quota error on one model lock only that model while leaving the
    provider record (and its other models) usable. Provider-level
    ``rate_limited_until`` and per-model ``model_locks`` are written
    independently: ``update_provider_health(model=X)`` writes only
    ``model_locks[X]``; ``update_provider_health(model=None)`` writes
    only ``rate_limited_until``. The TTL applies at the record level -
    when ``last_error_at`` ages out, every ``model_locks`` entry under
    the same record goes with it.
    """

    provider_id: str
    backoff_level: int = 0
    rate_limited_until: float | None = None
    last_error_at: float | None = None
    model_locks: dict[str, float] = dataclasses.field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.provider_id:
            raise ValueError(
                "ProviderHealth.provider_id must be a non-empty string"
            )
        if not 0 <= self.backoff_level <= MAX_BACKOFF_LEVEL:
            raise ValueError(
                f"ProviderHealth.backoff_level={self.backoff_level} "
                f"out of [0, {MAX_BACKOFF_LEVEL}]"
            )
        for model_id, until_ts in self.model_locks.items():
            if not isinstance(model_id, str) or not model_id:
                raise ValueError(
                    "ProviderHealth.model_locks keys must be non-empty "
                    f"strings, got {model_id!r}"
                )
            if not isinstance(until_ts, (int, float)) or until_ts <= 0:
                raise ValueError(
                    "ProviderHealth.model_locks[%r] must be a positive "
                    "timestamp, got %r" % (model_id, until_ts)
                )


@dataclasses.dataclass(frozen=True)
class ComboCursor:
    """Per-combo round-robin cursor state.

    Plan B (Spec 4, ab-0e5a921e). One entry per round-robin combo, keyed by
    combo name in ``ProviderRuntimeState.combo_cursors``. The cursor sticks
    on ``cursor_index`` for ``sticky_limit`` consecutive ``advance_cursor``
    calls before rolling to the next index.

    ``providers_hash`` invalidates the cursor when the user edits the combo
    mid-session (add/remove/reorder providers): on a hash mismatch the cursor
    resets to (idx=0, count=1) cleanly, with no stored intermediate state.

    ``last_rotated_at`` is the unix epoch seconds of the last advance and
    drives the 24h TTL: a quiescent combo's cursor is dropped on the next
    locked write and a future advance starts fresh at idx=0.
    """

    combo_name: str
    cursor_index: int
    consecutive_use_count: int
    providers_hash: str
    last_rotated_at: float

    def __post_init__(self) -> None:
        if not self.combo_name:
            raise ValueError("ComboCursor.combo_name must be a non-empty string")
        if self.cursor_index < 0:
            raise ValueError(
                f"ComboCursor.cursor_index={self.cursor_index} must be >= 0"
            )
        if self.consecutive_use_count < 0:
            raise ValueError(
                f"ComboCursor.consecutive_use_count="
                f"{self.consecutive_use_count} must be >= 0"
            )
        if not self.providers_hash:
            raise ValueError("ComboCursor.providers_hash must be a non-empty string")


@dataclasses.dataclass(frozen=True)
class ProviderRuntimeState:
    """Top-level runtime state for the provider rotation substrate.

    Plan A populates ``provider_health`` (per-provider exponential backoff).
    Plan B adds ``combo_cursors`` (per-combo round-robin position). v1 files
    parse cleanly with an empty ``combo_cursors`` dict; the next write
    rewrites the file as v2.
    """

    provider_health: dict[str, ProviderHealth]
    combo_cursors: dict[str, ComboCursor] = dataclasses.field(default_factory=dict)
    schema_version: int = SCHEMA_VERSION


def _resolve_state_path() -> Path:
    """Return the active runtime-state path, honoring env override."""
    override = os.environ.get("FNO_RUNTIME_STATE_PATH")
    if override:
        return Path(override)
    return Path(RUNTIME_STATE_PATH)


def _lock_path(state_path: Path) -> Path:
    """Sidecar lock path for runtime_state writes.

    Uses a distinct suffix (`.update.lock`) so it never collides with
    `state.io.atomic_write`'s internal lock at `path + ".lock"`. The two
    paths are independent contention boundaries; sharing them would
    self-deadlock when the outer + inner locks both target this file.
    """
    return Path(str(state_path) + ".update.lock")


def _write_state_atomic(state_path: Path, content: str) -> None:
    """Atomic write under tempfile + os.replace.

    Caller is responsible for holding the outer lock; this helper does
    not acquire one (avoids re-acquiring the same path lock).
    """
    state_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            dir=state_path.parent,
            prefix=f".{state_path.name}.",
            suffix=".tmp",
            delete=False,
            encoding="utf-8",
        ) as tmp:
            tmp.write(content)
            tmp_path = Path(tmp.name)
        os.replace(tmp_path, state_path)
        tmp_path = None
    finally:
        if tmp_path is not None and tmp_path.exists():
            tmp_path.unlink(missing_ok=True)


def _compute_exponential_cooldown_ms(level: int) -> int:
    """Pure: BASE * 2 ** level, capped at MAX. Defensive against >MAX_LEVEL."""
    capped_level = min(max(0, level), MAX_BACKOFF_LEVEL)
    raw = BASE_BACKOFF_MS * (2 ** capped_level)
    return min(raw, MAX_BACKOFF_MS)


def _serialize_state(state: ProviderRuntimeState) -> str:
    payload = {
        "schema_version": state.schema_version,
        "provider_health": {
            pid: dataclasses.asdict(h) for pid, h in state.provider_health.items()
        },
        "combo_cursors": {
            name: dataclasses.asdict(c) for name, c in state.combo_cursors.items()
        },
    }
    return json.dumps(payload, indent=2, sort_keys=True)


def _parse_cursors_payload(raw: dict[str, Any]) -> dict[str, ComboCursor]:
    """Best-effort parse of combo_cursors block. Drops malformed entries.

    Treats absence-of-providers_hash, missing required fields, and bad types
    as "skip silently". Same invariant as ``_parse_state_payload``: a
    partially-corrupt file degrades to "missing entry", never raises.
    """
    cursors: dict[str, ComboCursor] = {}
    block = raw.get("combo_cursors") or {}
    if not isinstance(block, dict):
        return cursors
    for name, entry in block.items():
        if not isinstance(entry, dict):
            logger.warning(
                "runtime_state: dropping malformed combo_cursor for %r (not a dict)",
                name,
            )
            continue
        # All four runtime fields must be present and well-typed; legacy
        # entries from a future schema cleanup that lack providers_hash
        # are dropped here so callers see "no cursor" and start fresh.
        try:
            cursors[name] = ComboCursor(
                combo_name=str(name),
                cursor_index=int(entry["cursor_index"]),
                consecutive_use_count=int(entry["consecutive_use_count"]),
                providers_hash=str(entry["providers_hash"]),
                last_rotated_at=float(entry["last_rotated_at"]),
            )
        except (KeyError, TypeError, ValueError) as exc:
            logger.warning(
                "runtime_state: dropping malformed combo_cursor for %r: %s",
                name,
                exc,
            )
    return cursors


def _drop_stale_cursors(
    cursors: dict[str, ComboCursor], now: float
) -> tuple[dict[str, ComboCursor], int]:
    """Return (kept_entries, dropped_count) using COMBO_CURSOR_TTL_SECONDS."""
    cutoff = now - COMBO_CURSOR_TTL_SECONDS
    kept: dict[str, ComboCursor] = {}
    dropped = 0
    for name, c in cursors.items():
        if c.last_rotated_at < cutoff:
            dropped += 1
            continue
        kept[name] = c
    return kept, dropped


def _parse_state_payload(raw: dict[str, Any]) -> dict[str, ProviderHealth]:
    """Best-effort parse of the on-disk payload into ProviderHealth entries.

    Drops entries whose shape we don't recognize (logs warning); never
    raises so that a partially-corrupt file degrades to "empty state".
    """
    health_map: dict[str, ProviderHealth] = {}
    block = raw.get("provider_health") or {}
    if not isinstance(block, dict):
        return health_map
    for pid, entry in block.items():
        if not isinstance(entry, dict):
            logger.warning(
                "runtime_state: dropping malformed entry for %r (not a dict)",
                pid,
            )
            continue
        try:
            # Clamp backoff_level to the valid range. A hand-edited or
            # legacy file with an out-of-range integer is repaired in
            # memory rather than dropped; the next write rewrites disk
            # with the clamped value.
            raw_level = int(entry.get("backoff_level", 0))
            clamped_level = max(0, min(raw_level, MAX_BACKOFF_LEVEL))
            if raw_level != clamped_level:
                logger.warning(
                    "runtime_state: clamped backoff_level %d -> %d for %r",
                    raw_level, clamped_level, pid,
                )
            raw_locks = entry.get("model_locks")
            model_locks: dict[str, float] = {}
            if raw_locks is not None and not isinstance(raw_locks, dict):
                # Falsy non-dict (e.g. JSON `[]`) and truthy non-dict
                # (e.g. JSON `[1, 2]`) both reach this branch. Earlier
                # `or {}` short-circuit silently rewrote falsy non-dict
                # to `{}` without logging, so this branch is the single
                # warning site for any non-dict on-disk shape.
                logger.warning(
                    "runtime_state: model_locks on %r is not a dict "
                    "(type=%s); treating as empty",
                    pid, type(raw_locks).__name__,
                )
                raw_locks = {}
            elif raw_locks is None:
                raw_locks = {}
            for m_id, m_ts in raw_locks.items():
                if not isinstance(m_id, str) or not m_id:
                    logger.warning(
                        "runtime_state: dropping model_lock with bad "
                        "key %r on provider %r",
                        m_id, pid,
                    )
                    continue
                try:
                    ts = float(m_ts)
                except (TypeError, ValueError):
                    logger.warning(
                        "runtime_state: dropping model_lock %r=%r on "
                        "provider %r: not numeric",
                        m_id, m_ts, pid,
                    )
                    continue
                if ts <= 0:
                    logger.warning(
                        "runtime_state: dropping model_lock %r on "
                        "provider %r: non-positive timestamp %r",
                        m_id, pid, ts,
                    )
                    continue
                model_locks[m_id] = ts
            health_map[pid] = ProviderHealth(
                provider_id=str(pid),
                backoff_level=clamped_level,
                rate_limited_until=(
                    float(entry["rate_limited_until"])
                    if entry.get("rate_limited_until") is not None
                    else None
                ),
                last_error_at=(
                    float(entry["last_error_at"])
                    if entry.get("last_error_at") is not None
                    else None
                ),
                model_locks=model_locks,
            )
        except (TypeError, ValueError) as exc:
            logger.warning(
                "runtime_state: dropping malformed entry for %r: %s", pid, exc
            )
    return health_map


def _read_disk_payload(state_path: Path) -> dict[str, Any] | None:
    """Read raw JSON dict from disk, or None if absent / unreadable.

    Empty file and JSON-decode error both return None (treated as
    empty state by callers). The corrupt file is preserved so a
    post-mortem can inspect it - the Failure Modes block locks this.
    """
    if not state_path.is_file():
        return None
    try:
        text = state_path.read_text(encoding="utf-8")
    except OSError as exc:
        logger.warning("runtime_state: read failed at %s: %s", state_path, exc)
        return None
    if not text.strip():
        return None
    try:
        raw = json.loads(text)
    except json.JSONDecodeError as exc:
        logger.warning(
            "runtime_state: JSON parse failed at %s: %s. Treating as empty.",
            state_path,
            exc,
        )
        return None
    if not isinstance(raw, dict):
        logger.warning(
            "runtime_state: top-level payload is not a dict at %s; treating as empty.",
            state_path,
        )
        return None
    return raw


def _drop_stale(
    health: dict[str, ProviderHealth], now: float
) -> tuple[dict[str, ProviderHealth], int]:
    """Return (kept_entries, dropped_count)."""
    cutoff = now - PROVIDER_HEALTH_TTL_SECONDS
    kept: dict[str, ProviderHealth] = {}
    dropped = 0
    for pid, h in health.items():
        if h.last_error_at is not None and h.last_error_at < cutoff:
            dropped += 1
            continue
        kept[pid] = h
    return kept, dropped


def read_state(now: float | None = None) -> ProviderRuntimeState:
    """Load runtime state from disk, dropping stale entries in memory.

    Returns an empty state when the file is missing, empty, or
    malformed. Stale entries (``last_error_at`` older than
    ``PROVIDER_HEALTH_TTL_SECONDS``) are filtered from the returned
    state but NOT rewritten to disk - that would be a lock-free write
    that could clobber a concurrent ``update_provider_health`` /
    ``reset_provider_health`` mutation. Disk cleanup happens in those
    write paths instead, which already hold the exclusive lock.

    This means a long-quiet provider's stale entry can persist on disk
    until the next write touches the file. That is acceptable: the
    returned in-memory state never exposes the stale entry to callers,
    and the file is bounded by the active provider count.
    """
    if now is None:
        now = time.time()
    state_path = _resolve_state_path()
    raw = _read_disk_payload(state_path)
    if raw is None:
        return ProviderRuntimeState(provider_health={})

    health_map = _parse_state_payload(raw)
    kept, _dropped = _drop_stale(health_map, now)
    cursors = _parse_cursors_payload(raw)
    cursors_kept, _ = _drop_stale_cursors(cursors, now)
    schema_version = int(raw.get("schema_version", SCHEMA_VERSION))
    return ProviderRuntimeState(
        provider_health=kept,
        combo_cursors=cursors_kept,
        schema_version=schema_version,
    )


def _next_health(
    current: ProviderHealth,
    rule: ErrorRule,
    now: float,
    model: str | None = None,
) -> ProviderHealth:
    """Compute the next ProviderHealth given an ErrorRule + current state.

    For a backoff rule, the level increments by 1 (capped at
    MAX_BACKOFF_LEVEL) and the cooldown is computed from the OLD level:
    1st hit (level 0 -> 1) -> BASE * 2^0 = 2000ms; 2nd hit (1 -> 2) ->
    BASE * 2^1 = 4000ms. This matches AC2.1-HP's "+2000ms on first 429"
    contract. The cap fires at level 15 (BASE * 2^14 already exceeds
    MAX_BACKOFF_MS so subsequent hits stay at the cap).

    When ``model`` is provided (Plan A1, ab-7fe3cdaf) the cooldown is
    written to ``model_locks[model]`` and ``rate_limited_until`` is
    preserved untouched. The cooldown ramp (``backoff_level``) is per
    provider regardless of model so a second 429 on a sibling model
    feels the longer step. When ``model`` is None, behavior is the
    Plan A baseline (write ``rate_limited_until`` only).
    """
    if rule.backoff:
        old_level = current.backoff_level
        next_level = min(old_level + 1, MAX_BACKOFF_LEVEL)
        cooldown_ms = _compute_exponential_cooldown_ms(old_level)
    else:
        # Fixed-cooldown rule: do NOT increment backoff_level.
        next_level = current.backoff_level
        cooldown_ms = rule.cooldown_ms or 0
    until_ts = now + (cooldown_ms / 1000.0)
    if model is not None:
        new_locks = dict(current.model_locks)
        new_locks[model] = until_ts
        return ProviderHealth(
            provider_id=current.provider_id,
            backoff_level=next_level,
            rate_limited_until=current.rate_limited_until,
            last_error_at=now,
            model_locks=new_locks,
        )
    # Defensive copy on the provider-level path matches the model
    # branch above. Frozen dataclass prevents reassignment but does
    # not seal the inner dict, so sharing the reference would alias
    # the new instance's model_locks with the old. Cheap copy keeps
    # the two branches structurally symmetric.
    return ProviderHealth(
        provider_id=current.provider_id,
        backoff_level=next_level,
        rate_limited_until=until_ts,
        last_error_at=now,
        model_locks=dict(current.model_locks),
    )


def update_provider_health(
    provider_id: str,
    rule: ErrorRule,
    model: str | None = None,
    now: float | None = None,
) -> ProviderHealth:
    """Atomically increment backoff state for ``provider_id``.

    Returns the freshly written ``ProviderHealth``. On lock-contention
    timeout (>``LOCK_TIMEOUT_SECONDS``), logs a warning, skips the
    write, and returns the last-known-good ``ProviderHealth`` (or a
    zero state if no prior entry exists).

    When ``model`` is provided (Plan A1, ab-7fe3cdaf), the cooldown is
    written to ``model_locks[model]`` and ``rate_limited_until`` is
    untouched (Locked Decision 2: model-locks-only when model is
    known). The provider-level ``backoff_level`` still increments
    because the cooldown ramp is a per-provider property regardless of
    which model errored (Locked Decision 5). When ``model`` is None,
    behavior matches the Plan A baseline.

    Does NOT swallow programmer errors: ValueError on a malformed
    ErrorRule, TypeError from a future refactor mismatch, etc.,
    propagate to the caller so they surface in CI. IO failures
    (OSError, JSONDecodeError) and lock contention (filelock.Timeout)
    are caught locally and degrade to the last-known-good fallback.
    """
    if now is None:
        now = time.time()
    state_path = _resolve_state_path()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = _lock_path(state_path)

    try:
        with filelock.FileLock(str(lock_path), timeout=LOCK_TIMEOUT_SECONDS):
            raw = _read_disk_payload(state_path)
            if raw is None:
                health_map: dict[str, ProviderHealth] = {}
                schema_version = SCHEMA_VERSION
            else:
                health_map = _parse_state_payload(raw)
                schema_version = int(raw.get("schema_version", SCHEMA_VERSION))

            # Drop stale entries while we hold the lock so disk cleanup
            # happens incrementally on every write rather than via a
            # racy lock-free path in read_state.
            health_map, _dropped = _drop_stale(health_map, now)
            cursors = _parse_cursors_payload(raw or {})
            cursors, _ = _drop_stale_cursors(cursors, now)

            current = health_map.get(
                provider_id, ProviderHealth(provider_id=provider_id)
            )
            new_health = _next_health(current, rule, now, model=model)
            health_map[provider_id] = new_health
            new_state = ProviderRuntimeState(
                provider_health=health_map,
                combo_cursors=cursors,
                schema_version=schema_version,
            )
            _write_state_atomic(state_path, _serialize_state(new_state))
            return new_health
    except filelock.Timeout:
        # AC2.7-ERR: log warning, skip write, return last-known-good.
        logger.warning(
            "runtime_state: lock contention >%.1fs at %s for provider %r; "
            "skipping write and returning last-known-good.",
            LOCK_TIMEOUT_SECONDS,
            state_path,
            provider_id,
        )
        # Read without acquiring the lock - eventual consistency is OK
        # for the read-only fallback path.
        raw = _read_disk_payload(state_path)
        if raw is not None:
            health_map = _parse_state_payload(raw)
            if provider_id in health_map:
                return health_map[provider_id]
        return ProviderHealth(provider_id=provider_id)


def reset_provider_health(
    provider_id: str,
    now: float | None = None,
) -> None:
    """Clear backoff state for ``provider_id``.

    Idempotent: removing an absent entry is a no-op. Acquires the same
    fcntl lock as ``update_provider_health`` so a concurrent increment
    cannot land between read and write.
    """
    state_path = _resolve_state_path()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = _lock_path(state_path)

    if now is None:
        now = time.time()
    try:
        with filelock.FileLock(str(lock_path), timeout=LOCK_TIMEOUT_SECONDS):
            raw = _read_disk_payload(state_path)
            if raw is None:
                return  # nothing to reset
            health_map = _parse_state_payload(raw)
            # Drop stale entries opportunistically under the lock; same
            # rationale as in update_provider_health.
            health_map, _dropped = _drop_stale(health_map, now)
            cursors = _parse_cursors_payload(raw)
            cursors, _ = _drop_stale_cursors(cursors, now)
            if provider_id not in health_map and not _dropped:
                # No-op fast path: nothing to do for this provider AND no
                # health entries needed cleanup. Skip the cursor write so
                # we don't churn the file just because cursors are present.
                return
            health_map.pop(provider_id, None)
            schema_version = int(raw.get("schema_version", SCHEMA_VERSION))
            new_state = ProviderRuntimeState(
                provider_health=health_map,
                combo_cursors=cursors,
                schema_version=schema_version,
            )
            _write_state_atomic(state_path, _serialize_state(new_state))
    except filelock.Timeout:
        logger.warning(
            "runtime_state: lock contention on reset for provider %r; "
            "skipping (entry remains until next successful reset or TTL).",
            provider_id,
        )


def is_in_cooldown(
    provider_id: str,
    model: str | None = None,
    now: float | None = None,
) -> bool:
    """Lock-free read: is ``provider_id`` currently in cooldown?

    Two-level lookup (Plan A1, ab-7fe3cdaf):

    1. If ``model`` is provided and ``model_locks[model] > now``, True.
    2. If ``rate_limited_until > now``, True (provider-level lock).
    3. Otherwise False.

    The model-specific check fires first so a provider-wide ``rate_limited_until``
    that happens to also cover the queried model still returns True
    via the provider-level branch even if no entry exists for that
    model. A model-specific lock is a strictly finer signal: when set
    it blocks queries for that exact model and any sibling model on
    the same record is only affected if the provider-level lock is
    also set.

    Eventual consistency is acceptable here: a stale "yes" makes us
    skip a provider that may have recovered (cheap), and a stale "no"
    makes us hit a provider that's still cooling off (the next call
    will re-classify and update state).
    """
    if now is None:
        now = time.time()
    state_path = _resolve_state_path()
    raw = _read_disk_payload(state_path)
    if raw is None:
        return False
    health_map = _parse_state_payload(raw)
    health = health_map.get(provider_id)
    if health is None:
        return False
    if model is not None:
        until_ts = health.model_locks.get(model)
        if until_ts is not None and until_ts > now:
            return True
    if health.rate_limited_until is not None and health.rate_limited_until > now:
        return True
    return False


# ---------------------------------------------------------------------------
# Combo cursor: read (lock-free, eventual consistency) + advance (locked).
#
# Plan B (Spec 4, ab-0e5a921e). Mirrors the post-#228 invariant from the
# health side: lazy cleanup of stale or hash-mismatched entries happens
# ONLY in locked write paths. read_cursor never writes; it returns None
# when the entry is stale, hash-mismatched, or absent. Cleanup of the
# stale on-disk record happens on the next advance_cursor for that combo
# (or piggybacks on update_provider_health / reset_provider_health).
# ---------------------------------------------------------------------------


def read_cursor(
    combo_name: str,
    providers_hash: str,
    now: float | None = None,
) -> ComboCursor | None:
    """Lock-free read: return the cursor for ``combo_name`` if valid.

    Returns None when:
      * No on-disk entry exists for this combo,
      * The stored ``providers_hash`` mismatches the input (combo edited),
      * The stored entry is stale (>``COMBO_CURSOR_TTL_SECONDS``),
      * The on-disk entry is malformed (missing fields, bad types).

    Eventual consistency is acceptable here: the cursor is a hint for
    rotation order; ``advance_cursor`` is the source of truth and reseats
    the cursor under the lock when the read-side observation was wrong.
    """
    if now is None:
        now = time.time()
    state_path = _resolve_state_path()
    raw = _read_disk_payload(state_path)
    if raw is None:
        return None
    cursors = _parse_cursors_payload(raw)
    cursor = cursors.get(combo_name)
    if cursor is None:
        return None
    if cursor.providers_hash != providers_hash:
        return None
    if cursor.last_rotated_at < now - COMBO_CURSOR_TTL_SECONDS:
        return None
    return cursor


def _next_cursor(
    prev: ComboCursor | None,
    sticky_limit: int,
    providers_hash: str,
    providers_count: int,
    combo_name: str,
    now: float,
) -> ComboCursor:
    """Pure: compute the next cursor state given the previous one.

    Math (matches AC2.1-HP and 9router's getRotatedModels at combo.js:36-65):
      - Fresh (no prev, hash mismatch, or stale): return (idx=0, count=1).
      - prev.count < sticky_limit: stay on idx, count += 1.
      - prev.count >= sticky_limit: advance idx by 1 modulo N, count = 1.

    The single-provider-combo case is short-circuited by the caller
    (``advance_cursor`` returns idx=0 forever); this helper still handles
    it correctly via the modulo because (0+1) % 1 == 0.
    """
    fresh = (
        prev is None
        or prev.providers_hash != providers_hash
        or prev.last_rotated_at < now - COMBO_CURSOR_TTL_SECONDS
    )
    if fresh:
        return ComboCursor(
            combo_name=combo_name,
            cursor_index=0,
            consecutive_use_count=1,
            providers_hash=providers_hash,
            last_rotated_at=now,
        )
    if prev.consecutive_use_count < sticky_limit:
        return ComboCursor(
            combo_name=combo_name,
            cursor_index=prev.cursor_index,
            consecutive_use_count=prev.consecutive_use_count + 1,
            providers_hash=providers_hash,
            last_rotated_at=now,
        )
    # sticky exhausted: advance index, reset count to 1 (we're using the
    # new index NOW, not at a future call).
    return ComboCursor(
        combo_name=combo_name,
        cursor_index=(prev.cursor_index + 1) % providers_count,
        consecutive_use_count=1,
        providers_hash=providers_hash,
        last_rotated_at=now,
    )


def advance_cursor(
    combo_name: str,
    sticky_limit: int,
    providers_hash: str,
    providers_count: int,
    now: float | None = None,
) -> ComboCursor:
    """Atomically bump and return the cursor for ``combo_name``.

    On lock-contention timeout, logs a warning and falls back to a
    locally-computed (lock-free) cursor: this keeps dispatch progressing
    rather than freezing the loop, at the cost of a possible duplicate
    rotation across racing processes (acceptable - the cursor is a hint).

    The returned cursor's ``cursor_index`` is what the caller should use
    for the rotation immediately following this call (see
    rotation.get_rotated_providers).
    """
    if now is None:
        now = time.time()
    if providers_count < 1:
        raise ValueError(
            f"advance_cursor: providers_count must be >= 1, got {providers_count}"
        )
    state_path = _resolve_state_path()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = _lock_path(state_path)

    try:
        with filelock.FileLock(str(lock_path), timeout=LOCK_TIMEOUT_SECONDS):
            raw = _read_disk_payload(state_path) or {}
            health_map = _parse_state_payload(raw)
            health_map, _ = _drop_stale(health_map, now)
            cursors = _parse_cursors_payload(raw)
            cursors, _ = _drop_stale_cursors(cursors, now)

            new_cursor = _next_cursor(
                cursors.get(combo_name),
                sticky_limit=sticky_limit,
                providers_hash=providers_hash,
                providers_count=providers_count,
                combo_name=combo_name,
                now=now,
            )
            cursors[combo_name] = new_cursor
            schema_version = SCHEMA_VERSION  # always write current version
            new_state = ProviderRuntimeState(
                provider_health=health_map,
                combo_cursors=cursors,
                schema_version=schema_version,
            )
            _write_state_atomic(state_path, _serialize_state(new_state))
            return new_cursor
    except filelock.Timeout:
        logger.warning(
            "runtime_state: lock contention >%.1fs at %s for combo %r; "
            "returning lock-free cursor (may duplicate rotation).",
            LOCK_TIMEOUT_SECONDS,
            state_path,
            combo_name,
        )
        prev = read_cursor(combo_name, providers_hash, now=now)
        return _next_cursor(
            prev,
            sticky_limit=sticky_limit,
            providers_hash=providers_hash,
            providers_count=providers_count,
            combo_name=combo_name,
            now=now,
        )
