"""Failover controller: swap orchestration + per-phase counter state.

Phase 03 of provider rotation failover (ab-9728b70b). The controller
owns the read-mutate-write of settings.yaml during a swap and the
per-phase state that bounds swap behavior. v0 ships:

- Storm-cap (task 3.1): max swaps per phase. Default 5.
- No-swap-back (task 3.3): once swapped from foo to bar, foo is
  ineligible for the rest of the phase. Cheap v0 hysteresis without a
  health-check loop.
- Queue-exhausted fall-through: when no eligible candidate remains,
  callers handle attended (BLOCKED all_providers_exhausted) vs
  unattended (sleep + restart) per Spec 1's locked decision #2.

State persists at ``.fno/failover-state.json`` across calls
within the same phase. Phase boundaries are detected by the
``phase_id`` constructor arg (caller-provided, derived from
target-state.md's ``session_id`` + ``current_phase``). When the stored
phase_id differs from the current one, state resets implicitly.
"""
from __future__ import annotations

import dataclasses
import enum
import fcntl
import json
import logging
import os
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


from fno.adapters.providers.error_taxonomy import (
    NormalizedError,
    classify_error,
)
from fno.adapters.providers.loader import (
    _extract_providers_block,
    _read_parsed,
    atomic_mutate_settings,
)
from fno.adapters.providers.runtime_state import (
    reset_provider_health,
    update_provider_health,
)

logger = logging.getLogger(__name__)


DEFAULT_MAX_SWAPS_PER_PHASE = 5


class SwapDecision(str, enum.Enum):
    """Outcome of an ``attempt_swap`` call."""

    SWAPPED = "swapped"
    BLOCKED_THRASH = "blocked_thrash"
    QUEUE_EXHAUSTED = "queue_exhausted"
    NO_SWAP_NEEDED = "no_swap_needed"  # error did not trigger swap


@dataclasses.dataclass(frozen=True)
class SwapResult:
    decision: SwapDecision
    new_provider_id: str | None = None
    reason: str | None = None


@dataclasses.dataclass
class FailoverState:
    """Per-phase counter state.

    Persisted to disk so a swap inside one target subprocess survives the
    next subprocess (the dispatch layer can be split across phases).
    Reset on phase boundary - see ``FailoverController.snapshot_state``.
    """

    phase_id: str
    swaps_this_phase: int = 0
    last_swap_from: str | None = None
    last_swap_at_iso: str | None = None


def _state_lock_path(state_path: Path) -> Path:
    """Sidecar lock file path for failover-state.json.

    We use a dedicated lock per state file (not a shared lock with
    settings.yaml) because the two files have independent contention
    boundaries: settings.yaml is mutated whenever ``active`` flips,
    failover-state.json is mutated only when a swap actually happens.
    Sharing locks would needlessly serialize unrelated paths.
    """
    return Path(str(state_path) + ".lock")


def _read_state(state_path: Path, phase_id: str) -> FailoverState:
    if not state_path.is_file():
        return FailoverState(phase_id=phase_id)
    # Hold LOCK_SH for the read so we never observe a half-written file
    # mid-rename. atomic write below releases LOCK_EX after os.replace.
    lock_path = _state_lock_path(state_path)
    state_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with open(lock_path, "a") as lock_f:
            fcntl.flock(lock_f.fileno(), fcntl.LOCK_SH)
            try:
                text = state_path.read_text(encoding="utf-8")
            finally:
                fcntl.flock(lock_f.fileno(), fcntl.LOCK_UN)
    except OSError as exc:
        logger.warning(
            "failover-state.json read failed at %s: %s. Counter resets to 0.",
            state_path, exc,
        )
        return FailoverState(phase_id=phase_id)
    try:
        raw = json.loads(text)
    except json.JSONDecodeError as exc:
        # Corrupt state file silently resets the storm-cap counter, which
        # would defeat the safety net the file exists to enforce. Log so
        # the regression is visible in post-mortem; the failsafe (start
        # fresh) is still the right behavior.
        logger.warning(
            "failover-state.json unreadable at %s: %s. Counter resets to 0.",
            state_path, exc,
        )
        return FailoverState(phase_id=phase_id)
    if not isinstance(raw, dict):
        logger.warning(
            "failover-state.json at %s is not a dict (got %s). Resetting.",
            state_path, type(raw).__name__,
        )
        return FailoverState(phase_id=phase_id)
    stored_phase = raw.get("phase_id")
    if stored_phase != phase_id:
        # Phase boundary: discard prior state, start fresh for this phase.
        return FailoverState(phase_id=phase_id)
    # Floor swaps_this_phase at 0 so a hand-edited or corrupt negative
    # value can't disable the storm-cap by making the >= comparison
    # permanently false.
    raw_swaps = raw.get("swaps_this_phase", 0)
    try:
        swaps = max(0, int(raw_swaps))
    except (TypeError, ValueError):
        logger.warning(
            "failover-state.json swaps_this_phase=%r is not int-convertible; "
            "treating as 0.", raw_swaps,
        )
        swaps = 0
    return FailoverState(
        phase_id=phase_id,
        swaps_this_phase=swaps,
        last_swap_from=raw.get("last_swap_from"),
        last_swap_at_iso=raw.get("last_swap_at_iso"),
    )


def _write_state(state_path: Path, state: FailoverState) -> None:
    """Atomically persist FailoverState under LOCK_EX + tempfile + rename.

    Concurrent ``attempt_swap`` calls would otherwise race: two writers
    using ``write_text`` truncate before writing, so a reader can
    observe a zero-length file or one writer's update can land between
    another's truncate and write. The lock + atomic-replace pattern
    matches fno.state.io.atomic_write semantics.
    """
    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = _state_lock_path(state_path)
    content = json.dumps(dataclasses.asdict(state), indent=2)
    with open(lock_path, "a") as lock_f:
        fcntl.flock(lock_f.fileno(), fcntl.LOCK_EX)
        try:
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
        finally:
            fcntl.flock(lock_f.fileno(), fcntl.LOCK_UN)


def _read_max_swaps_per_phase(settings_path: Path) -> int:
    """Read config.providers.failover.max_swaps_per_phase from settings.yaml.

    Falls back to ``DEFAULT_MAX_SWAPS_PER_PHASE`` when missing. Hand-walks
    the YAML to avoid pulling the full pydantic model for one number.
    """
    if not settings_path.is_file():
        return DEFAULT_MAX_SWAPS_PER_PHASE
    data = _read_parsed(settings_path)
    providers = _extract_providers_block(data) or {}
    block = providers.get("failover", {})
    if not isinstance(block, dict):
        return DEFAULT_MAX_SWAPS_PER_PHASE
    raw = block.get("max_swaps_per_phase")
    try:
        return int(raw) if raw is not None else DEFAULT_MAX_SWAPS_PER_PHASE
    except (TypeError, ValueError):
        return DEFAULT_MAX_SWAPS_PER_PHASE


def _next_eligible_provider(
    *,
    settings_path: Path,
    exclude: list[str],
) -> str | None:
    """Return the next eligible provider id, ordered by ``priority`` then by
    record id. Excludes any id in ``exclude``. Returns None if no
    candidate remains.

    Reads ``settings_path`` directly rather than going through
    ``load_providers`` because the controller is constructed with an
    explicit path; we don't want the project-local-vs-global discovery
    to override the caller's choice.
    """
    if not settings_path.is_file():
        return None
    data = _read_parsed(settings_path)
    providers = _extract_providers_block(data) or {}
    raw_records = providers.get("records") or []
    candidates = [
        r for r in raw_records
        if isinstance(r, dict) and r.get("id") and r["id"] not in exclude
    ]
    if not candidates:
        return None

    def _priority_safe(r: dict[str, Any]) -> int:
        # Default priority is 100 (matches ProviderRecord default). This
        # function bypasses pydantic validation for speed; a non-numeric
        # `priority` in YAML must NOT crash the failover process. Fall
        # back to the default and warn so a typo is visible.
        raw = r.get("priority", 100)
        try:
            return int(raw)
        except (TypeError, ValueError):
            logger.warning(
                "non-numeric priority %r on provider %s; using default 100",
                raw, r.get("id"),
            )
            return 100

    candidates.sort(key=lambda r: (_priority_safe(r), r["id"]))
    return candidates[0]["id"]


def _write_blocked_reason(target_state_path: Path, reason: str) -> bool:
    """Append blocked_reason: <reason> to target-state.md frontmatter.

    Idempotent: if the file already has the reason, returns True without
    writing. The stop hook owns BLOCKED status writes; this only sets
    the discriminator field used by the typed-blocker safety-net
    detector.

    Returns:
        True if the discriminator is now present in the file (either
        already there or freshly written). False if the file is absent
        or its frontmatter doesn't match the expected `---\\n...\\n---\\n`
        shape - the caller can react to that signal instead of the
        previous silent no-op which let the loop continue with no
        observability.
    """
    if not target_state_path.is_file():
        logger.warning(
            "target-state.md missing at %s; cannot write blocked_reason=%s. "
            "Stop hook will not see the failover-thrash discriminator.",
            target_state_path, reason,
        )
        return False
    text = target_state_path.read_text(encoding="utf-8")
    line = f"blocked_reason: {reason}"
    if line in text:
        return True
    # Insert before the closing --- of the frontmatter
    if not text.startswith("---\n"):
        logger.warning(
            "target-state.md at %s missing leading frontmatter delimiter; "
            "cannot write blocked_reason=%s.",
            target_state_path, reason,
        )
        return False
    end_idx = text.find("\n---\n", 4)
    if end_idx <= 0:
        logger.warning(
            "target-state.md at %s missing closing frontmatter delimiter; "
            "cannot write blocked_reason=%s.",
            target_state_path, reason,
        )
        return False
    # Replace any existing `blocked_reason:` line in the frontmatter
    # rather than letting two coexist (typed-blocker spec keeps one
    # discriminator at a time, but a defensive replace avoids YAML key
    # duplication if a different detector tripped earlier).
    fm_head = text[:end_idx]
    fm_lines = fm_head.split("\n")
    replaced = False
    for i, ln in enumerate(fm_lines):
        if ln.startswith("blocked_reason:"):
            fm_lines[i] = line
            replaced = True
            break
    if replaced:
        new_text = "\n".join(fm_lines) + text[end_idx:]
    else:
        new_text = fm_head + "\n" + line + text[end_idx:]
    target_state_path.write_text(new_text, encoding="utf-8")
    return True


class FailoverController:
    """Owns swap orchestration and the per-phase counter."""

    def __init__(
        self,
        *,
        settings_path: Path,
        state_path: Path,
        phase_id: str,
        target_state_path: Path | None = None,
    ) -> None:
        self._settings_path = Path(settings_path)
        self._state_path = Path(state_path)
        self._phase_id = phase_id
        self._target_state_path = target_state_path
        self._state = _read_state(self._state_path, phase_id)

    @property
    def max_swaps_per_phase(self) -> int:
        return _read_max_swaps_per_phase(self._settings_path)

    def snapshot_state(self) -> FailoverState:
        """Return a copy of the current in-memory state."""
        return dataclasses.replace(self._state)

    def attempt_swap(
        self,
        *,
        current_provider_id: str,
        error: NormalizedError,
    ) -> SwapResult:
        """Decide whether to swap and, if so, perform it.

        Args:
            current_provider_id: provider that produced ``error``.
            error: normalized error from ``error_taxonomy.normalize``.

        Returns:
            ``SwapResult`` with one of:
            - ``SWAPPED``: settings.yaml mutated, counter incremented.
            - ``BLOCKED_THRASH``: cap reached this phase; settings
              unchanged. ``target_state_path`` (if provided) gets
              ``blocked_reason: stuck:failover_thrash``.
            - ``QUEUE_EXHAUSTED``: every eligible candidate excluded
              (queue empty after applying no-swap-back).
            - ``NO_SWAP_NEEDED``: error did not trigger swap.
        """
        if not error.triggers_swap:
            return SwapResult(decision=SwapDecision.NO_SWAP_NEEDED,
                              reason="error_class_not_swap_trigger")

        # Update per-provider backoff state (Plan A, ab-6534a78a). The
        # runtime-state write is supplementary to the swap decision: if
        # classify_error returns a rule, we update; otherwise the
        # existing storm-cap path runs unchanged. update_provider_health
        # is documented as "never raises", so the catch below is narrow
        # to specific IO failures only - a TypeError/AttributeError
        # introduced by a future refactor must surface in CI rather than
        # be hidden.
        #
        # Plan A1 (ab-7fe3cdaf): when ``error.model`` is set, the lock
        # is written to ``model_locks[model]`` instead of
        # ``rate_limited_until``. When None (existing call sites),
        # behavior matches Plan A baseline (provider-level lock).
        rule = classify_error(error.raw_status, error.body_excerpt)
        if rule is not None:
            try:
                update_provider_health(
                    current_provider_id, rule, model=error.model,
                )
            except (OSError, json.JSONDecodeError) as exc:
                logger.warning(
                    "failover: runtime_state update failed for provider %s: %s",
                    current_provider_id, exc,
                )

        # Storm-cap (task 3.1): are we already at the cap?
        max_swaps = self.max_swaps_per_phase
        if self._state.swaps_this_phase >= max_swaps:
            if self._target_state_path is not None:
                _write_blocked_reason(self._target_state_path, "stuck:failover_thrash")
            return SwapResult(decision=SwapDecision.BLOCKED_THRASH,
                              reason=f"swaps_this_phase >= {max_swaps}")

        # No-swap-back (task 3.3): exclude current AND last_swap_from.
        exclude = [current_provider_id]
        if self._state.last_swap_from is not None:
            exclude.append(self._state.last_swap_from)

        candidate = _next_eligible_provider(
            settings_path=self._settings_path, exclude=exclude,
        )
        if candidate is None:
            return SwapResult(decision=SwapDecision.QUEUE_EXHAUSTED,
                              reason="no_eligible_provider")

        # Mutate config.toml to flip active under exclusive lock. Flat shape:
        # providers is top-level (atomic_mutate_settings flattens any legacy
        # wrapper on write, so mutating the flat block preserves the records).
        def _mutator(d: dict[str, Any]) -> dict[str, Any]:
            providers_block = d.setdefault("providers", {})
            providers_block["active"] = candidate
            return d

        atomic_mutate_settings(_mutator, settings_path=self._settings_path)

        # Update and persist counter state.
        self._state.swaps_this_phase += 1
        self._state.last_swap_from = current_provider_id
        self._state.last_swap_at_iso = datetime.now(timezone.utc).isoformat()
        _write_state(self._state_path, self._state)

        return SwapResult(decision=SwapDecision.SWAPPED,
                          new_provider_id=candidate)


def record_success(provider_id: str) -> None:
    """Reset per-provider exponential backoff after a successful call.

    Plan A (ab-6534a78a) public API: callers (sigma_dispatch, the loop
    runner, future cooldown-aware code) invoke this when a provider call
    returns 2xx. Calling this is OPTIONAL today - the runtime_state's
    1h TTL covers stale entries - but RECOMMENDED so the backoff_level
    drops to 0 immediately on recovery rather than after the TTL.

    Idempotent: resetting a provider with no prior entry is a no-op.
    Failures in the runtime_state IO layer are swallowed and logged so
    a success path is never derailed by state-file IO problems. The
    catch is narrow to specific IO classes - a TypeError or
    AttributeError introduced by a future refactor must surface rather
    than be hidden.
    """
    try:
        reset_provider_health(provider_id)
    except (OSError, json.JSONDecodeError) as exc:
        logger.warning(
            "failover: record_success(%r) runtime_state reset failed: %s",
            provider_id, exc,
        )
