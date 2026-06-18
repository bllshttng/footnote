"""Tests for credential staging module.

Phase 03 of the provider rotation substrate (ab-256f6b6e).
Covers AC03.1-HP, AC03.2-ERR, AC03.4-FR, AC03.5-EDGE.
"""
from __future__ import annotations

import os
from pathlib import Path

import pytest

from fno.adapters.providers.model import ProviderRecord, ProviderStagingError
from fno.adapters.providers.staging import stage, unstage, verify_staged


@pytest.fixture()
def creds_source(tmp_path: Path) -> Path:
    """A real directory that acts as the canonical credentials source."""
    source = tmp_path / "canonical-creds"
    source.mkdir()
    # Put a dummy file inside so presence checks are realistic.
    (source / ".credentials.json").write_text('{"token": "dummy"}')
    return source


@pytest.fixture()
def oauth_record(creds_source: Path) -> ProviderRecord:
    return ProviderRecord(
        id="claude-max-secondary",
        name="Claude Max Secondary",
        cli="claude",
        auth="oauth_dir",
        credentials_source=creds_source,
    )


@pytest.fixture()
def gemini_record(creds_source: Path) -> ProviderRecord:
    return ProviderRecord(
        id="gemini-pro-a",
        name="Gemini Pro A",
        cli="gemini",
        auth="oauth_dir",
        credentials_source=creds_source,
    )


@pytest.fixture()
def api_key_record() -> ProviderRecord:
    return ProviderRecord(
        id="anthropic-api-via-openclaw",
        name="Anthropic API via OpenClaw",
        cli="openclaw",
        auth="api_key",
        env={"ANTHROPIC_API_KEY": "${KEYCHAIN:anthropic-api-key-default}"},
    )


@pytest.fixture()
def staging_root(tmp_path: Path) -> Path:
    root = tmp_path / "providers"
    root.mkdir()
    return root


# ---------------------------------------------------------------------------
# AC03.1-HP: Stage + dispatch round-trip for oauth_dir (staging arm)
# ---------------------------------------------------------------------------

def test_stage_oauth_dir_creates_claude_symlink(
    oauth_record: ProviderRecord, staging_root: Path
) -> None:
    """stage() creates <root>/<id>/.claude symlink pointing at credentials_source."""
    result = stage(oauth_record, root=staging_root)
    assert result == staging_root / oauth_record.id
    link = staging_root / oauth_record.id / ".claude"
    assert link.is_symlink(), "Expected .claude to be a symlink"
    assert Path(os.readlink(link)) == oauth_record.credentials_source


def test_stage_oauth_dir_gemini_creates_home_symlink(
    gemini_record: ProviderRecord, staging_root: Path
) -> None:
    """stage() for gemini creates <root>/<id>/home/.gemini symlink."""
    stage(gemini_record, root=staging_root)
    link = staging_root / gemini_record.id / "home" / ".gemini"
    assert link.is_symlink(), "Expected home/.gemini to be a symlink"
    assert Path(os.readlink(link)) == gemini_record.credentials_source


def test_stage_api_key_creates_marker_dir_no_symlinks(
    api_key_record: ProviderRecord, staging_root: Path
) -> None:
    """stage() for api_key creates marker dir; no symlinks created."""
    result = stage(api_key_record, root=staging_root)
    assert result == staging_root / api_key_record.id
    assert result.is_dir()
    symlinks = [p for p in result.rglob("*") if p.is_symlink()]
    assert symlinks == [], f"Unexpected symlinks for api_key record: {symlinks}"


# ---------------------------------------------------------------------------
# AC03.5-EDGE: Idempotent staging
# ---------------------------------------------------------------------------

def test_stage_is_idempotent(
    oauth_record: ProviderRecord, staging_root: Path
) -> None:
    """Calling stage() twice with the same record is a no-op (no error)."""
    result1 = stage(oauth_record, root=staging_root)
    result2 = stage(oauth_record, root=staging_root)
    assert result1 == result2
    link = staging_root / oauth_record.id / ".claude"
    assert link.is_symlink()
    # Only one symlink should exist (idempotent, not doubled).
    assert Path(os.readlink(link)) == oauth_record.credentials_source


# ---------------------------------------------------------------------------
# AC03.2-ERR: Colliding symlink raises ProviderStagingError
# ---------------------------------------------------------------------------

def test_stage_colliding_symlink_raises(
    oauth_record: ProviderRecord, staging_root: Path, tmp_path: Path
) -> None:
    """stage() with existing symlink pointing elsewhere raises ProviderStagingError."""
    # First stage correctly.
    stage(oauth_record, root=staging_root)
    link = staging_root / oauth_record.id / ".claude"
    assert link.is_symlink()
    # Now remove and replace with a different-target symlink.
    link.unlink()
    different_target = tmp_path / "other-creds"
    different_target.mkdir()
    link.symlink_to(different_target)

    # Build a new record with the same id but same credentials_source.
    # Staging must detect the mismatch.
    with pytest.raises(ProviderStagingError, match="different target"):
        stage(oauth_record, root=staging_root)


def test_stage_missing_credentials_source_raises(
    staging_root: Path,
) -> None:
    """stage() where credentials_source does not exist raises ProviderStagingError."""
    ghost_path = Path("/tmp/this-path-absolutely-does-not-exist-abc123xyz")
    record = ProviderRecord(
        id="bad-creds",
        name="Bad Creds",
        cli="claude",
        auth="oauth_dir",
        credentials_source=ghost_path,
    )
    with pytest.raises(ProviderStagingError) as exc_info:
        stage(record, root=staging_root)
    assert str(ghost_path) in str(exc_info.value)


# ---------------------------------------------------------------------------
# verify_staged / unstage
# ---------------------------------------------------------------------------

def test_verify_staged_returns_true_after_stage(
    oauth_record: ProviderRecord, staging_root: Path
) -> None:
    """verify_staged() returns True for a properly staged oauth_dir record."""
    stage(oauth_record, root=staging_root)
    assert verify_staged(oauth_record, root=staging_root) is True


def test_verify_staged_returns_false_after_unstage(
    oauth_record: ProviderRecord, staging_root: Path
) -> None:
    """verify_staged() returns False after unstage() removes the dir."""
    stage(oauth_record, root=staging_root)
    unstage(oauth_record, root=staging_root)
    assert verify_staged(oauth_record, root=staging_root) is False


def test_unstage_is_noop_when_not_staged(
    oauth_record: ProviderRecord, staging_root: Path
) -> None:
    """unstage() is a no-op when the record was never staged."""
    # Should not raise.
    unstage(oauth_record, root=staging_root)


def test_verify_staged_api_key_marker_dir(
    api_key_record: ProviderRecord, staging_root: Path
) -> None:
    """verify_staged() returns True for api_key when marker dir exists."""
    stage(api_key_record, root=staging_root)
    assert verify_staged(api_key_record, root=staging_root) is True
    unstage(api_key_record, root=staging_root)
    assert verify_staged(api_key_record, root=staging_root) is False


# ---------------------------------------------------------------------------
# Relative symlink idempotency (Gemini Code Assist MEDIUM finding PR #199)
# ---------------------------------------------------------------------------

def test_stage_idempotent_with_relative_symlink(
    creds_source: Path, staging_root: Path
) -> None:
    """stage() must accept an existing relative symlink that resolves to the
    same credentials_source - it should be treated as an idempotent no-op,
    not raise ProviderStagingError."""
    record = ProviderRecord(
        id="claude-relative-test",
        name="Claude Relative Test",
        cli="claude",
        auth="oauth_dir",
        credentials_source=creds_source,
    )
    # Set up the account dir as stage() would.
    account_dir = staging_root / record.id
    account_dir.mkdir(parents=True, exist_ok=True)
    link = account_dir / ".claude"

    # Create a RELATIVE symlink: "../canonical-creds" relative to the account_dir.
    # This is equivalent to creds_source, but stored as a relative path on disk.
    rel_target = os.path.relpath(creds_source, link.parent)
    link.symlink_to(rel_target)

    # stage() must recognise this as pointing to the right place and not raise.
    result = stage(record, root=staging_root)
    assert result == account_dir
    # Symlink must remain unchanged (idempotent - not replaced with absolute).
    assert os.readlink(link) == rel_target
