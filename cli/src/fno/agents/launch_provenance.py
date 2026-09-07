"""Which account a spawn launched on, and WHO chose it.

One vocabulary for the launch-account fact every spawn surface records: the
row column (registry v26), the stderr line, and the receipt. Sources are the
shared words from ``fno.agents.spawn_flag_owners``. The defect this closes:
a receipt named the account but not WHO chose it, so a config injection
read as a caller decision.
"""

from __future__ import annotations

import json
import os
from typing import Optional

from fno.agents.spawn_flag_owners import CALLER, CONFIG

#: The env carrier across the exec seam: the seam that injects a picked
#: ``--account`` sets it to ``config`` so a flag-shaped read stays honest.
LAUNCH_ACCOUNT_SOURCE_ENV = "FNO_LAUNCH_ACCOUNT_SOURCE"

__all__ = [
    "CALLER",
    "CONFIG",
    "LAUNCH_ACCOUNT_SOURCE_ENV",
    "bg_account_field",
    "receipt_account_fields",
    "row_launch_source",
    "seam_launch_source",
]


def seam_launch_source(launch_account: Optional[str]) -> Optional[str]:
    """A typed flag is a caller; the carrier names the pick's ``config``."""
    if launch_account is None:
        return None
    if os.environ.get(LAUNCH_ACCOUNT_SOURCE_ENV) == CONFIG:
        return CONFIG
    return CALLER


def row_launch_source(source: Optional[str], account_value: Optional[str]) -> Optional[str]:
    """A row stamped ``default`` has no concrete account to attribute."""
    if account_value == "default":
        return None
    return source


def receipt_account_fields(
    result_launch_account: Optional[str],
    receipt_source: Optional[str],
    flag_account: Optional[str] = None,
) -> tuple[Optional[str], Optional[str]]:
    """The receipt's ``(account, account_source)`` pair; the caller's flag is
    the fallback so a back half that predates the field still reports the pin."""
    if result_launch_account not in (None, "default"):
        return result_launch_account, receipt_source
    return flag_account, receipt_source


def bg_account_field(result, account: Optional[str]) -> str:
    """The bg receipt's account fragment; source rides the account."""
    if getattr(result, "launch_account_source", None) is not None:
        return (
            f', "account": {json.dumps(result.launch_account)}, '
            f'"account_source": {json.dumps(result.launch_account_source)}'
        )
    return f', "account": {json.dumps(account)}' if account else ""
