"""The claims-backed lease lane for merge-gating configuration opt-outs.

Core-layer code by necessity, not by accident: the lease instrument is a
claim, and the values it guards live in config files. ``fno.config`` may not
import the claims layer, so the revocation guard, the write-time lease, and
the reaper restore all live here, and ``fno.config.writer`` refuses
merge-gating keys unless this module hands it its lease operations. Read-time
revocation (``fno.config`` loads this module through importlib) is the safety
guarantee; nothing here weakens it.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path
from typing import Any, Optional

from fno.claims import (
    CLAIM_UNAVAILABLE,
    ClaimState,
    ClaimCorrupted,
    ClaimGoneAway,
    ClaimHeldByOther,
    ClaimValidationError,
    acquire_claim,
    claim_status,
    release_claim,
)
from fno.claims.io import claims_dir, claims_root_for
from fno.config.optouts import (
    MERGE_GATING_OPTOUT_DEFAULTS,
    MERGE_GATING_OPTOUTS,
    _drop_raw_leaf,
    _raw_leaf,
)
from fno.config.writer import (
    ConfigSetError,
    _deep_set,
    _deep_unset,
    _is_optout_value,
    _locked_update,
    _storage_parts,
    _stored_leaf,
    set_config_values as _writer_set_config_values,
    unset_config_value as _writer_unset_config_value,
)
from fno.harness_identity import resolve_attester_identity

_LOG = logging.getLogger(__name__)

_OPT_OUT_CLAIM_PREFIX = "config-optout:"


def _resolve_optout_holder() -> str:
    """Return the session identity that is allowed to hold an opt-out lease."""
    session_id, _witness = resolve_attester_identity()
    if not session_id:
        raise ConfigSetError(
            "merge-gating opt-out requires a resolved attester session"
        )
    return session_id


def _optout_claim_key(key: str) -> str:
    return f"{_OPT_OUT_CLAIM_PREFIX}{key}"


def _optout_status(key: str) -> dict[str, Any]:
    claim_key = _optout_claim_key(key)
    return claim_status(claim_key, root=claims_root_for(claim_key))


def _release_optout_claim(key: str, holder: str) -> None:
    release_claim(
        _optout_claim_key(key),
        holder,
        root=claims_root_for(_optout_claim_key(key)),
    )


def _optout_ttl_ms(data: dict[str, Any]) -> int:
    present, value = _stored_leaf(data, ["review", "optout_ttl_minutes"])
    minutes = value if present else 60
    if isinstance(minutes, bool) or not isinstance(minutes, int):
        raise ConfigSetError(
            "review.optout_ttl_minutes must be an integer between 1 and 1440",
            2,
        )
    return minutes * 60 * 1000


def _lease_dict(claim: Any) -> dict[str, Any]:
    return {
        "holder": claim.holder,
        "acquired_at": claim.acquired_at,
        "expires_at": claim.expires_at,
    }


# ---------------------------------------------------------------------------
# read-time revocation
# ---------------------------------------------------------------------------


def _claim_state(key: str) -> str:
    """Read the opt-out instrument, distinguishing unreadable from absent."""
    root = claims_root_for(f"config-optout:{key}")
    try:
        directory = claims_dir(root)
        try:
            directory.stat()
        except FileNotFoundError:
            pass
        except OSError:
            return "unreadable"
        else:
            try:
                with os.scandir(directory):
                    pass
            except OSError:
                return "unreadable"
        return str(
            claim_status(f"config-optout:{key}", root=root).get(
                "state", "unreadable"
            )
        )
    except Exception:  # noqa: BLE001 - unreadable instrument revokes the opt-out
        return "unreadable"


def revoke_unbacked_optouts(raw: dict[str, Any]) -> dict[str, Any]:
    """Drop merge-gating opt-outs unless their global claim is LIVE.

    This is deliberately a read-time guard. A failed or delayed reaper leaves
    the file value inert, so a fresh process still requires the gate.
    """
    for key, optout in MERGE_GATING_OPTOUTS.items():
        present, value = _raw_leaf(raw, key)
        # Explicit presence matters for optional_apps: absent means the model's
        # built-in list, while [] is the actual opt-out.
        if not present or value != optout:
            continue
        state = _claim_state(key)
        if state == ClaimState.LIVE.value:
            continue
        _drop_raw_leaf(raw, key)
        _LOG.warning(
            "merge-gating opt-out %s revoked: claim instrument is %s; "
            "resolved default is %r",
            key,
            state,
            MERGE_GATING_OPTOUT_DEFAULTS[key],
        )
    return raw


# ---------------------------------------------------------------------------
# write-time lease
# ---------------------------------------------------------------------------


def prepare_set_leases(
    *,
    order: list[str],
    parts_by_key: dict[str, list[str]],
    existing: dict[str, Any],
    data: dict[str, Any],
    target: Path,
    scope: str,
) -> dict[str, Any]:
    """Acquire, keep, or schedule release of one lease per merge-gating key.

    Runs inside writer's file lock so the config value and its lease cannot
    drift apart. Returns the (possibly unchanged) merged data plus the lease
    bookkeeping writer folds into its results and rollback paths.
    """
    leases: dict[str, dict[str, Any]] = {}
    newly_acquired: list[tuple[str, str]] = []
    releases: list[tuple[str, str]] = []
    try:
        _prepare_set_leases_locked(
            order, parts_by_key, existing, data, target, scope,
            leases, newly_acquired, releases,
        )
    except Exception:
        # A mid-batch refusal must not strand the leases of the keys that
        # already acquired; writer's own rollback only sees claims from a
        # prepare that returned.
        rollback_set_leases(newly_acquired)
        raise
    return {
        "data": data,
        "leases": leases,
        "newly_acquired": newly_acquired,
        "releases": releases,
    }


def _prepare_set_leases_locked(
    order: list[str],
    parts_by_key: dict[str, list[str]],
    existing: dict[str, Any],
    data: dict[str, Any],
    target: Path,
    scope: str,
    leases: dict[str, dict[str, Any]],
    newly_acquired: list[tuple[str, str]],
    releases: list[tuple[str, str]],
) -> None:
    for key in order:
        if key not in MERGE_GATING_OPTOUTS:
            continue
        before_present, before = _stored_leaf(existing, parts_by_key[key])
        after_present, after = _stored_leaf(data, parts_by_key[key])
        before_active = _is_optout_value(key, before_present, before)
        after_active = _is_optout_value(key, after_present, after)
        status = _optout_status(key)
        state = status.get("state")
        claim_key = _optout_claim_key(key)
        root = claims_root_for(claim_key)

        if after_active:
            holder = _resolve_optout_holder()
            existing_holder = status.get("holder")
            metadata = {
                "config_key": key,
                "config_path": str(
                    Path(os.path.realpath(target)) if target.is_symlink() else target
                ),
                "scope": scope,
                "prior_present": before_present and not before_active,
                "prior_value": before if before_present and not before_active else None,
            }
            # An idempotent refresh must preserve the original value that
            # the lease is responsible for restoring - but only when the
            # old lease described THIS file. A takeover across a scope
            # change must not point the future restore at the previous
            # file and leave the new one's opt-out value unrestored.
            old_metadata = status.get("metadata")
            old_path = (
                old_metadata.get("config_path")
                if isinstance(old_metadata, dict)
                else None
            )
            if (
                isinstance(old_metadata, dict)
                and old_metadata.get("config_key") == key
                and old_path == metadata["config_path"]
            ):
                if state in {"live", "suspect", "stale"}:
                    metadata = old_metadata
            try:
                claim = acquire_claim(
                    claim_key,
                    holder,
                    reason="merge-gating opt-out",
                    ttl_ms=_optout_ttl_ms(data),
                    metadata=metadata,
                    root=root,
                )
            except ClaimHeldByOther as exc:
                raise ConfigSetError(str(exc), 1) from exc
            except CLAIM_UNAVAILABLE as exc:
                raise ConfigSetError(
                    f"cannot acquire {claim_key}: {exc}", 1
                ) from exc
            except (ClaimCorrupted, ClaimGoneAway, ClaimValidationError, OSError) as exc:
                raise ConfigSetError(
                    f"cannot acquire {claim_key}: {exc}", 1
                ) from exc
            leases[key] = _lease_dict(claim)
            if not (
                state in {"live", "suspect"} and existing_holder == holder
            ):
                newly_acquired.append((key, holder))
        elif state in {"live", "suspect"}:
            holder = _resolve_optout_holder()
            if status.get("holder") != holder:
                raise ConfigSetError(
                    f"claim {claim_key!r} held by {status.get('holder')}; "
                    "only its holder may release the opt-out",
                    1,
                )
            releases.append((key, holder))


def rollback_set_leases(newly_acquired: list[tuple[str, str]]) -> None:
    """Release leases acquired for a write that did not land."""
    for key, holder in newly_acquired:
        _release_optout_claim(key, holder)


def finalize_set_leases(releases: list[tuple[str, str]]) -> None:
    """Release the leases of opt-outs a successful write turned off."""
    for key, holder in releases:
        _release_optout_claim(key, holder)


def plan_unset_release(key: str) -> Optional[str]:
    """Return the holder whose lease must release when ``key`` is unset."""
    status = _optout_status(key)
    if status.get("state") in {"live", "suspect"}:
        holder = _resolve_optout_holder()
        claim_key = _optout_claim_key(key)
        if status.get("holder") != holder:
            raise ConfigSetError(
                f"claim {claim_key!r} held by {status.get('holder')}; "
                "only its holder may release the opt-out",
                1,
            )
        return holder
    return None


def release_optout(key: str, holder: str) -> None:
    _release_optout_claim(key, holder)


class _LeaseOps:
    """The lease operations handed to writer as ``lease_ops``."""

    prepare_set_leases = staticmethod(prepare_set_leases)
    rollback_set_leases = staticmethod(rollback_set_leases)
    finalize_set_leases = staticmethod(finalize_set_leases)
    plan_unset_release = staticmethod(plan_unset_release)
    release_optout = staticmethod(release_optout)


_LEASE_OPS = _LeaseOps()


# ---------------------------------------------------------------------------
# reaper restore
# ---------------------------------------------------------------------------


def _restore_reaped_optout(claim: Any) -> Optional[Path]:
    """Restore the value recorded by a retired ``config-optout`` claim."""
    key = str(getattr(claim, "key", ""))
    if not key.startswith(_OPT_OUT_CLAIM_PREFIX):
        return None
    metadata = getattr(claim, "metadata", {})
    if not isinstance(metadata, dict):
        raise ConfigSetError(f"{key} has invalid restore metadata")
    config_key = metadata.get("config_key")
    config_path = metadata.get("config_path")
    if not isinstance(config_key, str) or config_key not in MERGE_GATING_OPTOUTS:
        raise ConfigSetError(f"{key} has invalid config_key restore metadata")
    if not isinstance(config_path, str) or not config_path:
        raise ConfigSetError(f"{key} has no config_path restore metadata")

    target = Path(os.path.realpath(config_path))
    parts = _storage_parts(config_key.split("."))
    prior_present = bool(metadata.get("prior_present", False))
    prior_value = metadata.get("prior_value")

    def _restore(existing: dict[str, Any]) -> dict[str, Any]:
        present, value = _stored_leaf(existing, parts)
        if not _is_optout_value(config_key, present, value):
            return existing
        if prior_present:
            return _deep_set(existing, parts, prior_value)
        return _deep_unset(existing, parts)[0]

    if not target.exists() and not prior_present:
        return None
    return _locked_update(target, _restore)


def restore_reaped_optouts(sink: list[Any]) -> list[tuple[str, str]]:
    """Restore every claim a reaper collected in ``optout_sink``.

    The reaper stays config-free, so the callers that hold the sink call this
    right after the sweep and fold the failures into their own ``reap_failed``
    reporting. Read-time revocation is the safety guarantee; this is cleanup
    for the human-facing file and must never be allowed to turn a failed
    restore into an honored opt-out.
    """
    failures: list[tuple[str, str]] = []
    for claim in sink:
        try:
            _restore_reaped_optout(claim)
        except Exception as exc:  # noqa: BLE001 - keep restoring the rest
            failures.append(
                (str(getattr(claim, "key", "")), f"opt-out restore failed: {exc}")
            )
    return failures


# ---------------------------------------------------------------------------
# public entry points (the claims lane over writer's atomic set/unset)
# ---------------------------------------------------------------------------


def set_config_values(
    items: list[tuple[str, str]],
    *,
    scope: str = "global",
    repo_root: Optional[Path] = None,
    lock_timeout: Optional[float] = None,
) -> list[Any]:
    """``fno.config.writer.set_config_values`` with the lease lane bound.

    Required for merge-gating keys; identical to the writer for everything
    else (the ops only engage when a merge-gating key is touched).
    """
    return _writer_set_config_values(
        items,
        scope=scope,
        repo_root=repo_root,
        lock_timeout=lock_timeout,
        lease_ops=_LEASE_OPS,
    )


def set_config_value(
    key: str,
    value: str,
    *,
    scope: str = "global",
    repo_root: Optional[Path] = None,
    lock_timeout: Optional[float] = None,
) -> Any:
    return set_config_values(
        [(key, value)],
        scope=scope,
        repo_root=repo_root,
        lock_timeout=lock_timeout,
    )[0]


def unset_config_value(
    key: str,
    *,
    scope: str = "global",
    repo_root: Optional[Path] = None,
) -> Any:
    """``fno.config.writer.unset_config_value`` with the lease lane bound."""
    return _writer_unset_config_value(
        key,
        scope=scope,
        repo_root=repo_root,
        lease_ops=_LEASE_OPS,
    )
