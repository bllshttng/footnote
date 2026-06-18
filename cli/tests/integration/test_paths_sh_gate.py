"""Integration tests for fno paths emit-shell and fno paths verify commands.

Task 2.5 of plan 2026-05-14-path-config-impl.

Tests cover:
- fno paths verify exits 0 on matching paths.sh
- fno paths verify exits non-zero on mutated content
- Content hash is used (not mtime)
- Atomic write uses tmp + rename (not truncate)

An autouse fixture pins FNO_REPO_ROOT to tmp_path
(feedback_abi_repo_root_leaks_between_tests memory entry).
"""
from __future__ import annotations

import time
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()
_ENV = {"COLUMNS": "240", "NO_COLOR": "1", "TERM": "dumb"}


# ---------------------------------------------------------------------------
# Autouse fixture: pin FNO_REPO_ROOT and clear caches before each test
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Isolate each test: reset caches and pin repo root + settings."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass


def _set_settings(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, content: str) -> None:
    """Write a settings.yaml and wire it via FNO_CONFIG."""
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))


def _generate_paths_sh(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Generate a fresh paths.sh in tmp_path and return its path.

    Uses use_defaults=True so the generated file matches what the verify command
    (also defaults-mode) expects. This simulates the checked-in paths.sh flow.
    """
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    paths_sh = tmp_path / "paths.sh"

    from fno.setup.emit_shell import emit_paths_sh
    from fno.state.io import atomic_write

    content = emit_paths_sh(use_defaults=True)
    paths_sh.parent.mkdir(parents=True, exist_ok=True)
    atomic_write(paths_sh, content)
    return paths_sh


# ---------------------------------------------------------------------------
# AC2-HP: fno paths verify exits 0 against a freshly-generated paths.sh
# ---------------------------------------------------------------------------


def test_paths_verify_exits_0_on_fresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: `fno paths verify` exits 0 when paths.sh matches schema hash."""
    paths_sh = _generate_paths_sh(tmp_path, monkeypatch)

    result = runner.invoke(app, ["paths", "verify", str(paths_sh)], env=_ENV)
    assert result.exit_code == 0, (
        f"Expected exit 0 on fresh paths.sh, got {result.exit_code}.\nOutput:\n{result.output}"
    )
    assert "sync" in result.output.lower() or "match" in result.output.lower() or "ok" in result.output.lower(), (
        f"Expected success message in output, got: {result.output}"
    )


# ---------------------------------------------------------------------------
# AC2-ERR: fno paths verify exits non-zero on mutated content
# ---------------------------------------------------------------------------


def test_paths_verify_exits_nonzero_on_mutation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-ERR: `fno paths verify` exits non-zero when paths.sh is mutated."""
    paths_sh = _generate_paths_sh(tmp_path, monkeypatch)

    # Mutate the file
    original = paths_sh.read_text(encoding="utf-8")
    paths_sh.write_text(original + "\n# INJECTED_MUTATION\n", encoding="utf-8")

    result = runner.invoke(app, ["paths", "verify", str(paths_sh)], env=_ENV)
    assert result.exit_code != 0, (
        f"Expected non-zero exit on mutated paths.sh, got {result.exit_code}.\nOutput:\n{result.output}"
    )
    # Output should mention the diff or the regen command
    assert any(
        keyword in result.output.lower()
        for keyword in ("hash", "mismatch", "differ", "emit-shell", "regenerate", "regen")
    ), f"Expected diff/regen hint in output, got: {result.output}"


# ---------------------------------------------------------------------------
# AC2-EDGE: Content hash is used, not mtime
# ---------------------------------------------------------------------------


def test_paths_verify_uses_content_hash_not_mtime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-EDGE: Verify uses content hash; writing the same content twice passes."""
    paths_sh = _generate_paths_sh(tmp_path, monkeypatch)

    # Read the content and write it back (same content, later mtime)
    content = paths_sh.read_text(encoding="utf-8")
    time.sleep(0.01)  # ensure mtime changes if resolution allows
    paths_sh.write_text(content, encoding="utf-8")

    result = runner.invoke(app, ["paths", "verify", str(paths_sh)], env=_ENV)
    assert result.exit_code == 0, (
        f"Expected exit 0 when content is unchanged (same content, newer mtime), "
        f"got {result.exit_code}.\nOutput:\n{result.output}"
    )


# ---------------------------------------------------------------------------
# AC2-EDGE: atomic_write uses tmp + rename (not truncate)
# ---------------------------------------------------------------------------


def test_atomic_write_uses_tmp_rename(tmp_path: Path) -> None:
    """AC2-EDGE: atomic_write writes to a tmpfile then renames (POSIX atomic)."""
    from fno.state.io import atomic_write

    target = tmp_path / "output.sh"
    content = "#!/usr/bin/env bash\nexport FOO=bar\n"
    atomic_write(target, content)

    # File must exist with the exact content
    assert target.exists(), "output file must exist after atomic_write"
    assert target.read_text(encoding="utf-8") == content

    # No .tmp remnant should remain
    tmp_remnants = list(tmp_path.glob(".output.sh.*.tmp"))
    assert not tmp_remnants, f"Unexpected .tmp remnants: {tmp_remnants}"


# ---------------------------------------------------------------------------
# AC2-HP: fno paths emit-shell --output writes a sourceable file
# ---------------------------------------------------------------------------


def test_paths_emit_shell_writes_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: `fno paths emit-shell --output PATH` writes the stub to the given path."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    out = tmp_path / "generated.sh"

    result = runner.invoke(
        app,
        ["paths", "emit-shell", "--output", str(out)],
        env=_ENV,
    )
    assert result.exit_code == 0, (
        f"Expected exit 0, got {result.exit_code}.\nOutput:\n{result.output}"
    )
    assert out.exists(), "output file must be created"
    content = out.read_text(encoding="utf-8")
    assert "STATE_DIR" in content
    assert "GRAPH_JSON_PATH" in content


# ---------------------------------------------------------------------------
# AC2-HP: Roundtrip: emit then verify passes
# ---------------------------------------------------------------------------


def test_emit_then_verify_roundtrip(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: Emit followed immediately by verify exits 0 (roundtrip)."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    out = tmp_path / "paths.sh"

    emit_result = runner.invoke(
        app,
        ["paths", "emit-shell", "--output", str(out)],
        env=_ENV,
    )
    assert emit_result.exit_code == 0, f"emit-shell failed: {emit_result.output}"

    verify_result = runner.invoke(app, ["paths", "verify", str(out)], env=_ENV)
    assert verify_result.exit_code == 0, (
        f"verify failed after fresh emit: {verify_result.output}"
    )
