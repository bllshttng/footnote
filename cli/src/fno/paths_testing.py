"""Test fixture helper for isolating path-config state.

Usage:
    def test_foo(tmp_path, monkeypatch):
        use_tmpdir(monkeypatch, tmp_path)
        # All paths.X() now resolve under tmp_path; no real state touched.

Import: from fno.paths_testing import use_tmpdir
"""
from __future__ import annotations

import os
from pathlib import Path


def use_tmpdir(monkeypatch: object, tmp_path: Path) -> Path:
    """Point state_dir and settings file at tmp_path.

    Writes a minimal settings.yaml so paths.X() resolves cleanly, then sets
    ``FNO_CONFIG`` at the tmp file. The settings cache keys on its
    declaration (``fno.config._settings_key``), so every reader - including
    modules that bound ``load_settings`` at import time - resolves the tmp
    root with no function swap and no cache clearing.

    Returns the path to the tmp settings file for further customization
    (caller can overwrite it before calling paths.X()).
    """
    tmp_state = tmp_path / ".fno"
    tmp_state.mkdir(exist_ok=True)
    settings = tmp_state / "settings.yaml"
    settings.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {str(tmp_state)}/\n",
        encoding="utf-8",
    )
    sentinel = tmp_state / ".path-migration-done"
    sentinel.touch()

    # Wire the env var so load_settings() finds the tmp file
    monkeypatch.setenv("FNO_CONFIG", str(settings))  # type: ignore[attr-defined]

    # Calling this fixture IS a root declaration, so say so. It covers the lane
    # that reproduces a test by importing its module outside pytest, where no
    # conftest ran and nothing else stamps the pin.
    if os.environ.get("FNO_TEST_HERMETIC") is None:
        monkeypatch.setenv("FNO_TEST_HERMETIC", "1")  # type: ignore[attr-defined]

    _assert_state_landed(tmp_state)

    return settings


def _assert_state_landed(tmp_state: Path) -> None:
    """Refuse loudly when the declared root did not actually take.

    One assertion, inherited by every caller. The crown family resolved
    ``graph_json`` past this fixture and overwrote the operator's live graph.
    Silence was the whole defect, so this is a receipt, not a comment.
    """
    from fno import paths

    def landed(name: str) -> tuple[bool, str]:
        try:
            resolved = Path(getattr(paths, name)())
        except Exception as exc:  # a refused fence is a failed receipt too
            return False, f"<{type(exc).__name__}: {exc}>"
        return resolved == tmp_state or tmp_state in resolved.parents, str(resolved)

    checked = {name: landed(name) for name in ("state_dir", "graph_json")}
    if all(ok for ok, _ in checked.values()):
        return
    resolved = {name: shown for name, (_, shown) in checked.items()}
    raise RuntimeError(
        "use_tmpdir: resolved state escaped the fixture root. "
        f"state_dir={resolved['state_dir']} graph_json={resolved['graph_json']} "
        f"tmp_state={tmp_state}"
    )
