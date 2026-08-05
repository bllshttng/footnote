"""Shared pytest fixtures for tests living under ``cli/src/fno/``.

The tests in ``src/fno/`` package directories (e.g. ``adapters/providers/
test_loader.py``, ``test_dispatch_target.py``) accept a ``repo_root=tmp_path``
parameter to isolate their project-local settings. The corresponding global
candidate ``~/.fno/settings.yaml`` is read directly via ``Path.home()``
and falls outside ``tmp_path``, so a contributor with their own global
provider config will see those tests fail locally while CI (clean home dir)
sees them pass.

This autouse fixture pins ``FNO_GLOBAL_SETTINGS_PATH=/dev/null`` for every
test under ``src/fno/`` so the global candidate is a no-op file. Tests
that intentionally exercise the global-fallback path (e.g.
``test_global_active_combo_falls_back_when_no_project_override``) opt out
locally by calling ``monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH",
raising=False)`` before redirecting ``HOME``.

The fixture is intentionally scoped to ``src/fno/`` only; the tests
under ``cli/tests/`` have their own conftest with a different isolation
strategy (lru_cache clearing for ``load_settings``).
"""
from __future__ import annotations

import pytest

from fno.harness_identity import scrub_ambient_identity

# Ambient session identity, scrubbed at module load like the cli/tests/ tree
# does. This tree is a SEPARATE pytest root with its own conftest, so the scrub
# over there protects nothing here: a targeted `pytest cli/src/fno/...` reaches
# target_cli.py and the carveout/done/log readers with the live session's
# markers still set, and resolves the identity of the session running the tests.
# A fixture would be too late for anything reading a marker at import time.
scrub_ambient_identity()


@pytest.fixture(autouse=True)
def _pin_global_settings_to_devnull(monkeypatch: pytest.MonkeyPatch) -> None:
    """Force ``FNO_GLOBAL_SETTINGS_PATH=/dev/null`` for every test in this tree.

    See the module docstring for rationale. Tests that want to read from a
    monkeypatched HOME's settings.yaml must explicitly ``monkeypatch.delenv``
    this variable to restore the default ``Path.home() / .fno /
    settings.yaml`` resolution.
    """
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
