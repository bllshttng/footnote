"""Hash comparison gate for scripts/lib/paths.sh.

Schema is canonical; paths.sh is generated.
If hashes differ, the CI gate fails the PR.
"""
from __future__ import annotations

import hashlib
from pathlib import Path

from fno.setup.emit_shell import emit_paths_sh


def schema_derived_hash() -> str:
    """Return the SHA-256 hex digest of what emit_paths_sh(use_defaults=True) produces.

    Uses defaults-only mode so the hash is machine-stable regardless of any
    user settings.yaml overrides. This matches the checked-in paths.sh which
    is generated with use_defaults=True.
    """
    return hashlib.sha256(emit_paths_sh(use_defaults=True).encode("utf-8")).hexdigest()


def checked_in_hash(paths_sh: Path) -> str:
    """Return the SHA-256 hex digest of the checked-in paths.sh file."""
    return hashlib.sha256(paths_sh.read_bytes()).hexdigest()


def verify(paths_sh: Path) -> tuple[bool, str, str]:
    """Compare schema-derived hash to checked-in file hash.

    Returns:
        A tuple of (ok, derived_hash, checked_in_hash) where ok is True
        if the hashes match.
    """
    derived = schema_derived_hash()
    checked = checked_in_hash(paths_sh)
    return derived == checked, derived, checked
