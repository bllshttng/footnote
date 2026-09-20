"""Status-fanout config models (``config.status_fanout``, ``config.status_sinks[]``).

Extracted from the config monolith to keep that file under its line
budget, and re-exported from ``fno.config`` so every importer stays unchanged.
"""

from __future__ import annotations

import re
from typing import Optional

from pydantic import (
    BaseModel,
    ConfigDict,
    Field,
    field_validator,
    model_validator,
)

_STATUS_SINK_TYPES = ("json-webhook", "text-webhook", "backlog-progress")

# The protocol-family envelope whitelist minus `data` (a nested object, not an
# equality target). Sourced from events/schema.yaml at validation time so it
# cannot drift; this literal is the fallback for a smoke venv where the schema
# file is absent.
_MATCH_KEYS_FALLBACK = frozenset(
    {"ts", "v", "type", "source", "from", "model", "host",
     "project", "node", "task", "run", "parent", "outcome"}
)


def _status_sink_match_keys() -> frozenset[str]:
    try:
        from fno.events import PROTOCOL_ENVELOPE_ALLOWED

        allowed = set(PROTOCOL_ENVELOPE_ALLOWED)
        if allowed:
            return frozenset(allowed - {"data"})
    except Exception:
        pass
    return _MATCH_KEYS_FALLBACK


class StatusFanoutConfig(BaseModel):
    """Fanout dispatcher tuning (config.status_fanout).

    Non-positive tuning values fail loud at config load (matching this file's
    convention) rather than reaching ``urllib`` as a negative timeout - which
    raises deep inside a per-sink try/except and silently drops every event.
    """

    model_config = ConfigDict(extra="ignore")

    interval_secs: int = Field(default=5, ge=1)
    http_timeout_secs: int = Field(default=5, ge=1)
    retries: int = Field(default=2, ge=0)


class StatusSinkConfig(BaseModel):
    """One status sink (``config.status_sinks[]``).

    Semantic errors (unknown type, out-of-whitelist ``match`` key, both/neither
    of ``url``/``url_env``, duplicate ``name`` across the list) RAISE at config
    load - a misconfigured sink fails loud rather than silently not delivering.
    A container-level type mismatch (a scalar where the list belongs) still
    fails safe to ``[]`` in ``ConfigBlock`` so a typo never bricks settings load.
    """

    model_config = ConfigDict(extra="ignore")

    name: str
    type: str
    events: list[str] = Field(default_factory=list)
    match: dict[str, str] = Field(default_factory=dict)
    url: Optional[str] = None
    url_env: Optional[str] = None
    template: Optional[str] = None
    field: str = "content"
    # text-webhook: POST the rendered template verbatim as text/plain (ntfy
    # topic-URL shape) instead of a ``{field: rendered}`` JSON envelope.
    raw_body: bool = False
    cloudevents: bool = False
    enabled: bool = True

    @field_validator("name")
    @classmethod
    def _safe_name(cls, v: str) -> str:
        # name keys the cursor/errors filenames (`<name>.cursor`), so it must be
        # a bare filesystem-safe token - no separators, no leading dot - to keep
        # a sink from ever resolving a path outside `.fno/status-sinks/`.
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", v):
            raise ValueError(
                f"status sink name {v!r} must match [A-Za-z0-9][A-Za-z0-9._-]* "
                "(it keys the cursor filename; no path separators or leading dot)"
            )
        return v

    @field_validator("type")
    @classmethod
    def _known_type(cls, v: str) -> str:
        if v not in _STATUS_SINK_TYPES:
            raise ValueError(
                f"unknown status sink type {v!r} "
                f"(allowed: {', '.join(_STATUS_SINK_TYPES)})"
            )
        return v

    @field_validator("match")
    @classmethod
    def _match_keys_in_whitelist(cls, v: dict[str, str]) -> dict[str, str]:
        allowed = _status_sink_match_keys()
        bad = [k for k in v if k not in allowed]
        if bad:
            raise ValueError(
                f"status sink match key(s) {bad} not in the protocol envelope "
                f"whitelist (allowed: {', '.join(sorted(allowed))})"
            )
        return v

    @model_validator(mode="after")
    def _url_xor_url_env(self) -> "StatusSinkConfig":
        if self.type in ("json-webhook", "text-webhook"):
            if bool(self.url) == bool(self.url_env):
                raise ValueError(
                    f"status sink {self.name!r} ({self.type}) requires exactly "
                    f"one of url / url_env"
                )
        return self


class ReachMeRow(BaseModel):
    """One ``[[reach_me]]`` row (where questions reach the user). The Rust
    attention arm reads the same key; this model exists so the setup wizard
    can ask and ``fno config set`` can write. Unknown keys stay allowed."""

    model_config = ConfigDict(extra="allow")

    name: str = ""
    type: str = "md"
    path: str
    tag: str = "#fno"
    settle_secs: int = 120
    ready_only: bool = False
