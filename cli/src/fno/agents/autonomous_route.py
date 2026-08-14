"""One quota-aware route decision, shared by every autonomous launcher.

Before this module the two autonomous target launchers disagreed: ``backlog
advance`` could walk the active combo and land on another harness, while
``fno dispatch`` could only defer or decline-to-defer on the Claude account
pool. So the all-accounts-walled case terminated in a wait of up to a week
while an idle Codex subscription sat there.

:func:`select_autonomous_route` is the single seam. It returns one of four
actions and, for ``cutover``, the complete destination tuple (record, harness,
account overlay) a spawn needs. It is pure policy: it never spawns, claims, or
mutates anything.

Precedence is explicit invocation pin > node pin > configured dispatch harness
> quota policy > built-in harness default. ``pinned`` collapses the first two
for the caller: a pinned launch may still defer, but never gets rerouted -
quota policy must not silently change a billing or harness choice a human made.
"""

from __future__ import annotations

import dataclasses
from pathlib import Path
from typing import Optional

__all__ = ["AutonomousRoute", "launch_is_pinned", "select_autonomous_route"]


def launch_is_pinned(
    node: Optional[dict] = None,
    *,
    provider: Optional[str] = None,
    model: Optional[str] = None,
    account: Optional[str] = None,
    node_cwd: Optional[str] = None,
    honors_config_harness: bool = True,
) -> bool:
    """True when this launch carries explicit intent quota policy must not move.

    One implementation for both autonomous launchers, so they cannot drift on
    what counts as a pin. Precedence is explicit invocation pin > node pin >
    configured dispatch harness > quota policy, and each of those outranks an
    automatic cutover.

    The model pin matters as much as the provider one: a cutover swaps the
    harness, so forwarding a claude-only model to codex launches a worker that
    cannot start. An unreadable config pins nothing - fail-open, the same stance
    as the rest of the quota path.
    """
    if any((v or "").strip() for v in (provider, model, account)):
        return True
    if node and any(
        str(node.get(k) or "").strip() for k in ("provider", "model", "harness")
    ):
        return True
    if not honors_config_harness:
        # A launcher that hardcodes its harness does not honor this setting, so
        # letting it pin would suppress a cutover to protect a choice the launch
        # never makes. Only a launcher that actually reads the setting may pin on
        # it.
        return False
    try:
        from fno.config import load_settings, load_settings_for_repo

        s = load_settings_for_repo(Path(node_cwd)) if node_cwd else load_settings()
        return bool((s.dispatch.harness or "").strip())
    except Exception:  # noqa: BLE001 - unreadable config pins nothing
        return False


@dataclasses.dataclass(frozen=True)
class AutonomousRoute:
    """The route one autonomous launch attempt resolved to.

    ``action``:
      - ``stay``            - launch here, quota is fine (or policy is off).
      - ``defer``           - hold the node; ``retry_at`` is the reset.
      - ``cutover``         - launch on ``record_id``/``harness`` instead.
      - ``unknown-proceed`` - the probe had no data; absence is not trouble.

    The destination fields are set together or not at all: a ``cutover`` that
    could not resolve BOTH a harness and an account overlay is never returned,
    because passing only one produces a wrong-billing or wrong-binary launch.

    ``defer_fallback`` is what a ``cutover`` degrades to if the caller cannot
    stage the destination after all: True when the window was binding enough to
    hold work (EXHAUSTED), False when it was only the proactive LOW case, where
    launching here is still better than waiting.
    """

    action: str
    reason: str
    source_record: str = ""
    record_id: Optional[str] = None
    harness: Optional[str] = None
    account_env: Optional[dict] = None
    retry_at: Optional[float] = None
    window: Optional[str] = None
    defer_fallback: bool = False


def _cutover_low_after_minutes(node_cwd: Optional[str]) -> int:
    try:
        from fno.config import load_settings, load_settings_for_repo

        s = load_settings_for_repo(Path(node_cwd)) if node_cwd else load_settings()
        return int(s.dispatch.cutover_low_after_minutes or 0)
    except Exception:  # noqa: BLE001 - unreadable config -> proactive cutover off
        return 0


def _select_destination(
    node_cwd: Optional[str], exhausted_provider: str
) -> Optional[tuple[str, str, dict]]:
    """Pick the next healthy record from the active combo, or None.

    Returns ``(record_id, harness, account_env)``, or ``None`` for the defer
    floor: not configured for failover, no active COMBO to walk (a bare active
    provider is not a combo), the whole combo exhausted, an unresolvable
    harness, an unstageable account, or ANY read error. Rerouting never
    guesses - every non-clean path degrades to defer.

    The three parts stage a spawn correctly (a combo member is a provider
    RECORD id, NOT a harness): ``harness`` (the record's ``cli``) becomes
    ``--provider`` (validated against the harness set, so a record id must
    never go there) and drives the resolver's substrate; ``account_env``
    (``dispatch_env`` of the record: ``CLAUDE_CONFIG_DIR`` / ``base_url`` / api
    key) selects the ACCOUNT via the spawn subprocess env. A cross-harness
    cutover (claude->codex) rides the ``harness`` change.
    """
    try:
        from fno.config import load_settings, load_settings_for_repo

        s = load_settings_for_repo(Path(node_cwd)) if node_cwd else load_settings()
        if (s.dispatch.on_exhaustion or "defer") != "failover":
            return None
    except Exception:  # noqa: BLE001 - unreadable config -> defer floor
        return None
    try:
        from fno.adapters.providers.dispatch import dispatch_env
        from fno.adapters.providers.loader import load_combos, load_providers
        from fno.adapters.providers.rotation import next_healthy_provider
        from fno.agents.dispatch_target import resolve_dispatch_target

        # The active combo via the settings_combo rung (env={} so no TARGET_COMBO
        # pin leaks in; a synthetic agent name has no per-agent pin). A bare active
        # PROVIDER (no combo) resolves to combo_name=None -> defer (nothing to walk).
        # One repo root for every read below. Resolving settings against the node
        # while loading the combo and record set from the CALLER's cwd would pick
        # a destination out of the wrong project's registry - the dispatcher runs
        # from wherever it happens to be, not from the node's checkout.
        root = Path(node_cwd) if node_cwd else None
        target = resolve_dispatch_target(
            "advance-failover",
            repo_root=root,
            env={},
        )
        if not target.combo_name:
            return None
        combo = load_combos(repo_root=root).get(target.combo_name)
        if combo is None:
            return None
        pid = next_healthy_provider(
            combo, exclude={exhausted_provider}, repo_root=root
        )
        if pid is None:
            return None
        rec = load_providers(repo_root=root).by_id.get(pid)
        harness = (getattr(rec, "harness", "") or "").strip()
        if not harness:
            return None  # a record with no harness cannot pick a --provider
        # Stage the account env; an unstaged/unresolvable account -> defer (never
        # spawn onto a broken account).
        account_env = dispatch_env(pid, repo_root=root)
        return (pid, harness, account_env)
    except Exception:  # noqa: BLE001 - never dispatch onto an unresolved provider
        return None


def _healthy_alternate_exists(node_cwd: Optional[str] = None) -> bool:
    """True when launch-time account picking is armed AND has a live account.

    Best-effort and conservative: anything unreadable, or picking disarmed,
    returns False so the defer stands. Only a positive answer - an account the
    spawn seam could actually launch on - suppresses it.

    ``pick_account`` is scoped to the PROCESS cwd, so for a node in another
    project it would answer from the dispatcher's registry rather than the
    node's. Rather than suppress a defer on evidence from the wrong repository,
    a cross-project node answers False and the defer stands.
    """
    try:
        from pathlib import Path as _Path

        if node_cwd and _Path(node_cwd).resolve() != _Path.cwd().resolve():
            return False
        from fno.adapters.providers.cli import pick_account

        return pick_account(if_armed=True).account is not None
    except Exception:  # noqa: BLE001 - a probe must never block a dispatch
        return False


def _emit_quota_rotation_declined(
    node_id: Optional[str], provider: str, reason: str,
) -> None:
    """Emit the single quota_rotation_declined decision event. Non-fatal,
    mirrors ``_emit_quota_deferred`` (``dispatch.py``). The counterpart to
    that event: this one fires when the launch went out on UNKNOWN headroom
    rather than being held, so a healthy system and an absent probe stop
    looking identical in the journal."""
    try:
        import time as _time

        from fno.events import _build, append_event

        data: dict = {"provider": provider, "reason": reason}
        if node_id:
            data["node_id"] = node_id
        try:
            from fno.adapters.providers.runtime_state import read_usage

            snap = read_usage(provider, ttl_seconds=float("inf"))
            if snap is not None:
                data["snapshot_age_s"] = _time.time() - snap.probed_at
        except Exception:  # noqa: BLE001 - the age is a nice-to-have, not the event
            pass
        append_event(_build("quota_rotation_declined", "backlog", data))
    except Exception:  # noqa: BLE001 - a telemetry write must never block dispatch
        pass


def select_autonomous_route(
    *,
    provider_id: str,
    priority: Optional[str] = None,
    pinned: bool = False,
    node_cwd: Optional[str] = None,
    now: Optional[float] = None,
    node_id: Optional[str] = None,
) -> AutonomousRoute:
    """Resolve one autonomous launch's route from one quota probe.

    ``pinned`` is any explicit harness / provider / account / model / node
    route pin: it forbids automatic replacement, never the existing defer.
    ``node_id`` is only for the ``quota_rotation_declined`` telemetry event
    (Task 3, x-8183) - it never changes the routing decision.
    """
    from fno.adapters.providers.runtime_state import (
        HeadroomState,
        evaluate_quota_signal,
    )

    sig = evaluate_quota_signal(
        provider_id,
        priority=priority,
        cutover_low_after_minutes=_cutover_low_after_minutes(node_cwd),
        now=now,
        repo_root=Path(node_cwd) if node_cwd else None,
    )
    window = sig.state.value
    if sig.cutover and not pinned:
        dest = _select_destination(node_cwd, sig.provider_id)
        if dest is not None:
            record_id, harness, account_env = dest
            return AutonomousRoute(
                "cutover",
                f"{window.lower()}-cutover",
                source_record=sig.provider_id,
                record_id=record_id,
                harness=harness,
                account_env=account_env,
                retry_at=sig.resets_at,
                window=window,
                defer_fallback=sig.defer,
            )
    if sig.defer:
        # Deferring is the floor, not the answer. Before holding work, check the
        # OTHER reroute: launch-time account picking. Holding because the active
        # account is walled while a sibling account has headroom is the stall
        # this whole path exists to delete. A pinned launch skips it - picking is
        # a reroute, and a pin forbids reroutes, not defers. This lives HERE and
        # not in one caller because the two launchers must reach the same
        # verdict; when only `fno dispatch` had it, identical fixtures deferred
        # on one path and launched on the other.
        if not pinned and _healthy_alternate_exists(node_cwd):
            return AutonomousRoute(
                "stay", "alternate-account-available",
                source_record=sig.provider_id, window=window,
            )
        return AutonomousRoute(
            "defer",
            "pinned" if (pinned and sig.cutover) else sig.reason,
            source_record=sig.provider_id,
            retry_at=sig.resets_at,
            window=window,
        )
    if sig.state is HeadroomState.UNKNOWN:
        _emit_quota_rotation_declined(node_id, sig.provider_id, sig.reason)
        return AutonomousRoute(
            "unknown-proceed", sig.reason, source_record=sig.provider_id, window=window
        )
    return AutonomousRoute(
        "stay", sig.reason, source_record=sig.provider_id, window=window
    )
