"""Honest context-catalog observer for project-supplied artifacts.

Builds ``ContextReference`` observations from a generic identifier map in
config (``[context.artifacts]``). Nothing here knows a function's vocabulary:
the caller hands in an opaque identifier -> {path, sensitivity} map and gets
back one honest reference per entry. A readable file carries its real sha256
and byte size; an unreadable one becomes ``readable=False`` plus a naming
reason. No entry ever claims to have read a file it did not.

The catalog never mints a ``CapabilityFact`` and never grants anything; it is
an observation layer that the (untouched) resolver consults.
"""

from __future__ import annotations

import hashlib
from collections.abc import Mapping
from datetime import datetime
from pathlib import Path

from fno.paths import resolve_repo_root
from fno.roles.models import ContextKind, ContextReference, Sensitivity

__all__ = ["build_artifact_catalog", "catalog_revision"]


def _artifact_path(spec: object) -> str:
    if hasattr(spec, "path"):
        return str(spec.path)
    if isinstance(spec, Mapping):
        value = spec.get("path")
        if value is not None:
            return str(value)
    raise TypeError(f"artifact spec must carry a 'path', got {type(spec).__name__}")


def _artifact_sensitivity(spec: object) -> Sensitivity:
    raw: object = None
    if hasattr(spec, "sensitivity"):
        raw = getattr(spec, "sensitivity")
    elif isinstance(spec, Mapping):
        raw = spec.get("sensitivity", "internal")
    if raw is None:
        raw = "internal"
    return Sensitivity(str(raw))


def catalog_revision(artifacts: Mapping[str, object]) -> str:
    """A stable revision for a configured artifact map.

    Derived from identifiers and their configured paths/sensitivities (not file
    contents), so it is stable across runs that read the same files and changes
    only when the configuration changes. The operator passes it to
    ``fno roles resolve --snapshot`` so the catalog and the resolution agree on
    one alignment token.
    """
    payload = "|".join(
        f"{identifier}:{_artifact_path(spec)}:{_artifact_sensitivity(spec).value}"
        for identifier, spec in sorted(artifacts.items())
    )
    return "context:" + hashlib.sha256(payload.encode("utf-8")).hexdigest()[:16]


def build_artifact_catalog(
    artifacts: Mapping[str, object],
    *,
    snapshot_revision: str,
    clock: datetime,
) -> tuple[ContextReference, ...]:
    """Emit one honest ``ContextReference`` per configured artifact identifier.

    ``snapshot_revision`` is the alignment token shared with the resolution it
    feeds: the resolver rejects any reference whose ``snapshot_revision``
    differs, so a caller resolves roles against the same revision it stamped
    here. ``artifacts`` is the generic ``[context.artifacts]`` map; values may
    be ``ArtifactConfig`` models (from loaded settings) or plain dicts.
    """
    if clock.tzinfo is None:
        raise ValueError("clock must be timezone-aware")
    references: list[ContextReference] = []
    for identifier, spec in artifacts.items():
        sensitivity = _artifact_sensitivity(spec)
        raw_path = Path(_artifact_path(spec)).expanduser()
        # Anchor relative paths to the repo root, not the process cwd, so the
        # same configuration resolves the same file wherever the command runs.
        resolved = raw_path if raw_path.is_absolute() else resolve_repo_root() / raw_path
        try:
            data = resolved.read_bytes()
        except OSError as exc:
            # Absent, permission-denied, or a directory: never fabricate the
            # file's bytes or a digest. Flag unreadable with a reason naming
            # the path, and carry no content_digest/content_revision - an
            # honest observation of an unreadable file claims no content.
            references.append(
                ContextReference(
                    kind=ContextKind.ARTIFACT,
                    identifier=identifier,
                    provenance=str(resolved),
                    snapshot_revision=snapshot_revision,
                    sensitivity=sensitivity,
                    byte_size=0,
                    readable=False,
                    unavailable_reason=f"{resolved}: {exc.__class__.__name__}",
                )
            )
            continue
        digest = hashlib.sha256(data).hexdigest()
        references.append(
            ContextReference(
                kind=ContextKind.ARTIFACT,
                identifier=identifier,
                provenance=str(resolved),
                content_digest=digest,
                content_revision=digest,
                snapshot_revision=snapshot_revision,
                sensitivity=sensitivity,
                byte_size=len(data),
                readable=True,
            )
        )
    return tuple(references)
