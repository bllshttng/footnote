"""Tests for the docs that ship in the public tree.

The programmatic ``fno.target()`` / ``fno.run_loop()`` Python API was removed
by the control-plane collapse; drive work via ``/target`` in Claude Code.
These tests pin the docs that remain and guard against the removed API
quietly coming back as a documented-but-broken surface.
"""
import pathlib

import fno

REPO_ROOT = pathlib.Path(__file__).parents[3]  # cli/tests/unit -> cli/tests -> cli -> repo root


def test_auth_md_exists():
    auth_doc = REPO_ROOT / "docs" / "auth.md"
    assert auth_doc.exists(), f"expected {auth_doc} to exist"


def test_auth_md_mentions_anthropic():
    auth_doc = REPO_ROOT / "docs" / "auth.md"
    assert "ANTHROPIC_API_KEY" in auth_doc.read_text(encoding="utf-8")


def test_removed_python_api_stays_removed():
    """The removed fno.target() / run_loop() callable API must not be re-exposed.

    Note: ``fno.target`` is also a real subpackage, so when the full suite
    imports it (e.g. ``fno.target.test_blast`` does ``from fno.target import
    blast``) the attribute resolves to that module rather than raising. Assert
    the removed *callable* is gone, not that the attribute is absent.
    """
    for name in ("target", "run_loop"):
        assert not callable(getattr(fno, name, None)), (
            f"fno.{name} must not be a re-exposed callable API"
        )


def test_removed_python_api_doc_not_resurrected():
    """docs/python-api.md was removed with the API; it must not return."""
    assert not (REPO_ROOT / "docs" / "python-api.md").exists()
