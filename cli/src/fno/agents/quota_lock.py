"""The one decision site for writing a provider quota lock.

``update_provider_health`` is the only writer of the cooldown, and this
module is the only place that decides one is owed. Attribution stays with
the caller: the function refuses to guess, so an unattributed refusal
cannot poison a healthy account by falling back to the active one.
"""

from __future__ import annotations

from typing import Optional

__all__ = ["record_quota_lock"]


def record_quota_lock(
    account_id: Optional[str],
    text: Optional[str],
    *,
    resets_at: Optional[float] = None,
) -> Optional[str]:
    """Write the provider cooldown for a refusal already attributed to an account.

    Returns the account id written, or None when nothing was written. The
    caller supplies its own account attribution; ``"default"`` (the spawn
    positively pinned nothing) and None are refused rather than resolved to
    the active account, which is how one dead worker's refusal reads as a
    verdict on an account it never used.
    """
    if not account_id or account_id == "default" or not text:
        return None
    from fno.adapters.providers.error_taxonomy import (
        classify_error,
        reset_epoch_from,
    )
    from fno.adapters.providers.runtime_state import (
        record_reset_timezone,
        update_provider_health,
    )

    rule = classify_error(None, text)
    if rule is None:
        return None
    update_provider_health(
        account_id, rule,
        resets_at=(resets_at if resets_at is not None
                   else reset_epoch_from(text, record_reset_timezone(account_id))),
    )
    return account_id
