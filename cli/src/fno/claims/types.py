"""Pydantic models for the on-disk claim schema.

The Claim model serializes to YAML for human inspection. expires_at uses
null-vs-omit semantics: PID-liveness claims OMIT the key from YAML output
entirely (per the design doc's "Locked Decision #4"); TTL claims serialize
it as an integer epoch-ms.

ClaimState is the four-way classification used by status/list verbs.
"""
from __future__ import annotations

from enum import Enum
from typing import Any, Optional

from pydantic import BaseModel, ConfigDict, Field, field_validator


SCHEMA_VERSION = 1

# Raw key cap. The on-disk filename is `quote(key, safe="") + ".lock"`,
# which can expand non-ASCII / reserved chars up to ~3x. We validate
# the encoded filename length separately at acquire-time against
# MAX_ENCODED_FILENAME_BYTES, so MAX_KEY_LENGTH only needs to keep raw
# keys human-readable and prevent obvious denial-of-service strings.
MAX_KEY_LENGTH = 256

# Filesystem hard cap. ext4, HFS+, APFS all have 255-byte filename limits.
# acquire_claim rejects any key that would encode past this bound.
MAX_ENCODED_FILENAME_BYTES = 240  # 240 + ".lock" suffix = 245 bytes
MIN_TTL_MS = 60_000        # 1 minute
MAX_TTL_MS = 86_400_000    # 24 hours


class ClaimState(str, Enum):
    """Classification of a key's current state.

    - free: no claim file exists
    - live: claim exists and holder process is verifiably alive
    - stale: claim exists but holder is dead or expired (recoverable)
    - corrupted: claim file present but cannot be parsed
    """

    FREE = "free"
    LIVE = "live"
    STALE = "stale"
    CORRUPTED = "corrupted"


class Claim(BaseModel):
    """On-disk schema for a single claim file.

    Field meanings:
        schema_version: integer; forward-compat probe. Readers reject claims
            with version > SCHEMA_VERSION rather than guess.
        key: the lock subject (e.g. "node:ab-1234abcd"); URL-encoded when
            forming the file path.
        holder: the symbolic owner string (e.g. "target-session:<sid>").
        acquired_at: epoch-ms UTC when the claim was created.
        expires_at: epoch-ms UTC of TTL expiry. OMITTED from YAML for
            PID-liveness claims (the absence is meaningful; do not serialize
            as null).
        pid: holder process PID (host-local).
        host: socket.gethostname() at acquire time; cross-host claims are
            intentionally treated as opaque (see staleness.is_live).
        reason: optional human-readable context string.
        metadata: optional dict; treated opaquely.

    Bool-vs-string traps are not in play here because no field is a Literal.
    Forward-compatible reading via model_validate(extra="ignore") is set
    in ConfigDict so unknown fields from a future writer do not crash older
    readers.
    """

    model_config = ConfigDict(extra="ignore")

    schema_version: int = SCHEMA_VERSION
    key: str
    holder: str
    acquired_at: int = Field(description="epoch milliseconds, UTC")
    expires_at: Optional[int] = Field(default=None, description="epoch ms; absent => PID-liveness")
    pid: int
    host: str
    reason: Optional[str] = None
    metadata: dict[str, Any] = Field(default_factory=dict)

    @field_validator("key")
    @classmethod
    def _validate_key(cls, value: str) -> str:
        if not value:
            raise ValueError("claim key must be non-empty")
        if len(value) > MAX_KEY_LENGTH:
            raise ValueError(
                f"claim key length {len(value)} exceeds MAX_KEY_LENGTH={MAX_KEY_LENGTH}"
            )
        return value

    @field_validator("holder")
    @classmethod
    def _validate_holder(cls, value: str) -> str:
        if not value:
            raise ValueError("claim holder must be non-empty")
        return value

    @field_validator("schema_version")
    @classmethod
    def _validate_schema_version(cls, value: int) -> int:
        if value > SCHEMA_VERSION:
            raise ValueError(
                f"claim schema_version={value} > supported={SCHEMA_VERSION}; "
                f"refusing to read from a newer writer"
            )
        return value

    def to_yaml_dict(self) -> dict[str, Any]:
        """Return a dict ready for yaml.safe_dump.

        Critical: PID-liveness claims OMIT expires_at entirely (key absent),
        TTL claims serialize it as the integer epoch-ms. Never write
        ``expires_at: null`` — readers treat null and absent the same way
        but the design doc locks the writer side to absent-only.
        """
        out: dict[str, Any] = {
            "schema_version": self.schema_version,
            "key": self.key,
            "holder": self.holder,
            "acquired_at": self.acquired_at,
            "pid": self.pid,
            "host": self.host,
        }
        if self.expires_at is not None:
            out["expires_at"] = self.expires_at
        if self.reason is not None:
            out["reason"] = self.reason
        if self.metadata:
            out["metadata"] = self.metadata
        return out
