"""Hermetic isolation for the tests living under ``cli/src/fno/``.

This is a SEPARATE pytest root from ``cli/tests/``, which is the trap: a scrub
applied over there protects nothing here. A targeted ``pytest cli/src/fno/...``
reaches target_cli.py and the carveout/done/log readers with the live session's
markers still set, and resolves the identity of the session running the tests.

Both trees now call the same ``fno.hermetic.neutralise``, so they cannot drift
on what counts as ambient - which is the failure that produced three specimens
through three different channels on 2026-08-11.

Applied at module load rather than as a fixture, because a fixture is too late
for anything that reads a marker at import time.

Tests that intentionally exercise the global-fallback path (e.g.
``test_global_active_combo_falls_back_when_no_project_override``) opt out
per-test with ``monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH", raising=False)``
before redirecting ``HOME``; that still works, since neutralise sets the pin in
``os.environ`` exactly as the retired autouse fixture did.
"""
from __future__ import annotations

import os
import tempfile
from pathlib import Path

from fno.hermetic import neutralise

_SANDBOX = tempfile.mkdtemp(prefix="fno-src-test-sandbox-")
_hermetic_env = neutralise(os.environ, Path(_SANDBOX))
os.environ.clear()
os.environ.update(_hermetic_env)


def pytest_sessionfinish(session, exitstatus) -> None:  # noqa: ANN001
    import shutil

    shutil.rmtree(_SANDBOX, ignore_errors=True)
