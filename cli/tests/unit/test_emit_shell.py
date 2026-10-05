"""Unit tests for fno.setup.emit_shell codegen.

Task 2.5 of plan 2026-05-14-path-config-impl.

All tests use tmp_path + monkeypatch isolation. An autouse fixture pins
FNO_REPO_ROOT to tmp_path so resolve_repo_root() is isolated
(feedback_fno_repo_root_leaks_between_tests memory entry).
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest
# Autouse fixture: pin FNO_REPO_ROOT and clear caches before each test
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Isolate each test: reset caches and pin repo root + settings."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    import fno.paths as paths_mod
    yield

# ---------------------------------------------------------------------------
# AC2-HP: Codegen is byte-deterministic within the same process
# ---------------------------------------------------------------------------


def test_is_project_relative_rejects_template_anywhere(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HIGH (Gemini): _is_project_relative returns False when '{' appears anywhere.

    A value like 'plans/{project}' contains a template var not at start;
    emitting it as '$REPO_ROOT/plans/{project}' produces unexpandable bash.
    The check must disqualify ANY occurrence of '{', not just at the start.
    """
    from fno.setup.emit_shell import _is_project_relative

    # Bare relative (no template vars) - should be True
    assert _is_project_relative(".fno/plans")
    assert _is_project_relative("plans")

    # Template var anywhere - must return False so emit falls through
    assert not _is_project_relative("plans/{project}")
    assert not _is_project_relative("{project}/plans")
    assert not _is_project_relative("some/path/{vault}/plans")

    # Absolute / home-relative / env-var - already false
    assert not _is_project_relative("/abs/path")
    assert not _is_project_relative("~/plans")
    assert not _is_project_relative("$SOME_VAR/plans")

def test_emit_paths_sh_defaults_match_checked_in_fixture() -> None:
    """The defaults emitter must reproduce the checked-in paths.sh byte for byte.

    The Rust verb generates the file now, and verify's schema hash has to agree
    with Rust output until the verify verb ports.
    """
    import fno
    from fno.setup.emit_shell import emit_paths_sh

    repo_root = Path(fno.__file__).resolve().parents[3]
    fixture = (repo_root / "scripts" / "lib" / "paths.sh").read_text(encoding="utf-8")
    assert emit_paths_sh(use_defaults=True) == fixture


