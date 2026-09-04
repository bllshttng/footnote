"""Is the fleet watchdog lane on, and how far does it go?

Two questions ``recovery.watchdog`` spelled as one word, so "is this armed"
had to be asked as ``!= "off"``. Separate fields here; `coerce_legacy` keeps
every config already written parsing unchanged. Its own module because
``config/__init__.py`` is over the shrink-only file budget.
"""
from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict


class WatchdogBlock(BaseModel):
    """The lane's four settings. The per-key text an operator reads is
    `FIELD_META` in `registry.py`; a copy here would drift from it. One rule
    belongs beside the fields: ``reap`` is the only lane that ships off,
    because it runs ``stop`` then ``rm`` and deletes the session's worktree.
    A wrong wake can be undone and a wrong reap cannot.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    mode: Literal["report", "wake", "handoff"] = "report"
    mail_to: str = ""
    reap: bool = False


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
    for legacy, key in (("watchdog_mail_to", "mail_to"), ("watchdog_reap", "reap")):
        if legacy in data and key not in block:
            block[key] = data[legacy]
    if block or isinstance(flat, str):
        data = {**data, "watchdog": block}
    return data
