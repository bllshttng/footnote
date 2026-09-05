"""Is the fleet watchdog lane on, and how far does it go?

Two questions ``recovery.watchdog`` spelled as one word, so "is this armed"
had to be asked as ``!= "off"``. Separate fields here; `coerce_legacy` keeps
every config already written parsing unchanged. Its own module because
``config/__init__.py`` is over the shrink-only file budget, which is also why
the ``retire_grace_s`` legacy lift lives here.
"""
from __future__ import annotations

import sys
from typing import Literal

from pydantic import BaseModel, ConfigDict


class WatchdogBlock(BaseModel):
    """The lane's settings. The per-key text an operator reads is
    `FIELD_META` in `registry.py`; a copy here would drift from it.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    mode: Literal["report", "wake", "handoff"] = "report"
    mail_to: str = ""


def coerce_legacy(data: object) -> object:
    """A ``recovery`` block written before the split still parses, unchanged.
    ``"off"`` becomes ``enabled=false`` keeping the DEFAULT mode, so turning
    the lane back on does not also silently pick a depth; a key already in the
    nested form wins over its legacy sibling."""
    if not isinstance(data, dict):
        return data
    flat = data.get("watchdog")
    if isinstance(flat, WatchdogBlock):
        # An already-built block beside a legacy sibling: seed from it, or the
        # sibling replaces the whole block and silently disarms the lane.
        block = flat.model_dump()
    else:
        block = dict(flat) if isinstance(flat, dict) else {}
    if isinstance(flat, str):
        word = flat.strip().lower()
        block["enabled"] = word not in {"", "off"}
        if block["enabled"]:
            block["mode"] = word
    if "watchdog_mail_to" in data and "mail_to" not in block:
        block["mail_to"] = data["watchdog_mail_to"]
    if block or isinstance(flat, str):
        data = {**data, "watchdog": block}
    return data


def lift_retire_grace(data: object) -> object:
    """Lift a legacy ``recovery.retire_grace_s`` onto ``agents.retire_grace_s``.

    The key moved to the agents block when retirement became the daemon
    sweep's question (x-c672); the daemon reads ``agents.retire_grace_s``
    (agents_config.rs). A config still carrying the recovery spelling parses,
    answers under the new key, and prints ONE line naming the legacy key.
    The new spelling wins when both are present.
    """
    if not isinstance(data, dict):
        return data
    recovery = data.get("recovery")
    if not isinstance(recovery, dict) or "retire_grace_s" not in recovery:
        return data
    agents = data.get("agents")
    agents = dict(agents) if isinstance(agents, dict) else {}
    if "retire_grace_s" not in agents:
        agents["retire_grace_s"] = recovery["retire_grace_s"]
        print(
            "fno config: recovery.retire_grace_s moved to agents.retire_grace_s; "
            "using the legacy value (x-c672)",
            file=sys.stderr,
        )
    recovery = {k: v for k, v in recovery.items() if k != "retire_grace_s"}
    return {**data, "agents": agents, "recovery": recovery}
