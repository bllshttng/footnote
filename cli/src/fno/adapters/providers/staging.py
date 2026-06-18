"""Credential staging for provider rotation substrate.

Phase 03 of the provider rotation substrate (ab-256f6b6e).

Each provider gets a directory at <root>/<id>/. The shape inside depends on
the CLI kind:

    claude       -> <id>/.claude              symlink to credentials_source
    gemini       -> <id>/home/.gemini         symlink to credentials_source
    codex        -> <id>/home/.codex          symlink to credentials_source
    openclaw     -> <id>/home/.openclaw       symlink to credentials_source
    hermes       -> <id>/home/.hermes         symlink to credentials_source

For api_key auth, staging is a no-op beyond creating the marker dir (env vars
only; nothing on disk that needs to be isolated).
"""
from __future__ import annotations

import os
import shutil
from pathlib import Path

from fno.adapters.providers.model import (
    ProviderRecord,
    ProviderStagingError,
)


def _default_providers_root() -> Path:
    """Return the providers directory, using paths accessor when available.

    Broad except is intentional: this is documented as fail-open. A
    malformed settings.yaml that raises ValidationError from
    paths.state_dir() should not block staging operations from running
    against the default location.
    """
    try:
        from fno import paths as _paths
        return _paths.state_dir() / "providers"
    except Exception:
        return Path.home() / ".fno" / "providers"


def _symlink_path(record: ProviderRecord, account_dir: Path) -> tuple[Path, Path]:
    """Return (symlink_path, parent_of_symlink) for this record's CLI kind.

    The symlink *itself* lives at symlink_path. For CLIs that use a HOME
    override, the intermediate `home/` directory is created automatically.
    """
    cli = record.cli
    if cli == "claude":
        return account_dir / ".claude", account_dir
    else:
        # gemini, codex, openclaw, hermes all use HOME override.
        home_dir = account_dir / "home"
        link = home_dir / f".{cli}"
        return link, home_dir


def stage(
    record: ProviderRecord,
    root: Path | None = None,
) -> Path:
    """Create the per-account directory layout and return its path.

    For oauth_dir auth: creates root/<id>/ with the appropriate inner dir
    symlinked to record.credentials_source. Idempotent: if the symlink
    already exists and points at the right target, no-op. If it exists
    but points elsewhere, raises ProviderStagingError.

    For api_key auth: creates root/<id>/ as an empty marker dir and
    returns the path; no symlinks.

    Raises:
        ProviderStagingError: if credentials_source does not exist for
            oauth_dir, or if an existing symlink points to a different
            target (likely user error or corruption).
    """
    if root is None:
        root = _default_providers_root()
    account_dir = root / record.id
    account_dir.mkdir(parents=True, exist_ok=True)

    if record.auth == "api_key":
        return account_dir

    # oauth_dir path: validate credentials_source exists first.
    assert record.credentials_source is not None  # enforced by model validator
    if not record.credentials_source.exists():
        raise ProviderStagingError(
            f"credentials_source does not exist: {record.credentials_source}"
        )

    link, link_parent = _symlink_path(record, account_dir)
    link_parent.mkdir(parents=True, exist_ok=True)

    if link.is_symlink():
        existing_target = Path(os.readlink(link))
        # os.readlink may return a relative path; resolve it against the
        # link's parent so the comparison works regardless of how the symlink
        # was created (absolute vs. relative).
        if not existing_target.is_absolute():
            existing_target = (link.parent / existing_target).resolve()
        if existing_target == record.credentials_source:
            # Already correct - idempotent no-op.
            return account_dir
        raise ProviderStagingError(
            f"Existing symlink at {link} points to a different target "
            f"({existing_target!r} != {record.credentials_source!r}). "
            "Remove it manually or call unstage() first."
        )

    link.symlink_to(record.credentials_source)
    return account_dir


def unstage(
    record: ProviderRecord,
    root: Path | None = None,
) -> None:
    """Remove the per-account directory. No-op if not staged."""
    if root is None:
        root = _default_providers_root()
    account_dir = root / record.id
    if account_dir.exists() or account_dir.is_symlink():
        shutil.rmtree(account_dir)


def verify_staged(
    record: ProviderRecord,
    root: Path | None = None,
) -> bool:
    """Check that a staged record's filesystem layout is intact.

    For oauth_dir: symlink exists and its target is readable.
    For api_key: marker dir exists.
    """
    if root is None:
        root = _default_providers_root()
    account_dir = root / record.id
    if record.auth == "api_key":
        return account_dir.is_dir()

    link, _ = _symlink_path(record, account_dir)
    return link.is_symlink() and link.exists()
