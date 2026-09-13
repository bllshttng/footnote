"""The one decision site for writing a provider quota lock: it decides
one is owed, attribution stays with the caller, and it refuses to guess."""

from __future__ import annotations

from typing import Optional


def record_quota_lock(
    account_id: Optional[str],
    text: Optional[str],
    *,
    resets_at: Optional[float] = None,
) -> Optional[str]:
    """Write the cooldown for a refusal already attributed to an account.

    Returns the account id written, or None. None and ``"default"`` are
    refused, never resolved to the active account.
    """
    if not account_id or account_id == "default" or not text:
        return None
    from fno.adapters.providers.error_taxonomy import classify_error, reset_epoch_from
    from fno.adapters.providers.runtime_state import (
        record_reset_timezone, update_provider_health,
    )

    rule = classify_error(None, text)
    if rule is None:
        return None
    if resets_at is None:
        resets_at = reset_epoch_from(text, record_reset_timezone(account_id))
    update_provider_health(account_id, rule, resets_at=resets_at)
    return account_id
