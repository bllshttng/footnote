"""Megatron test isolation (ab-fc00ae39).

Megatron production emit sites (the loop, ``update_status``, the artifact
aggregator) resolve the ``.fno/events.jsonl`` sink via
``fno.paths.resolve_repo_root()`` (order: ``FNO_REPO_ROOT`` env ->
``git rev-parse --show-toplevel`` -> cwd). Pytest runs with cwd under the real
checkout, so a megatron test that drives an emit site without isolating the
resolver silently writes telemetry into the real repo's ``events.jsonl``
(~33 junk lines across this directory per run).

This autouse fixture isolates the resolver by ``chdir``-ing to a per-test tmp
directory. ``chdir`` (rather than setting ``FNO_REPO_ROOT``) is deliberate so
the guard composes with the two isolation styles already used in this
directory instead of fighting them:

  * Tests that isolate neither (the leakers) now resolve via the cwd fallback
    -> tmp, because ``git rev-parse`` fails outside a repo. No leak.
  * Tests that ``chdir`` to their own dir (e.g. the artifact e2e test) chdir
    again after this fixture; the last chdir wins, so their emit + read-back
    stay co-located. No conflict.
  * Tests that set their own ``FNO_REPO_ROOT`` (e.g. test_telemetry_anchor)
    win on the env var, which takes precedence over cwd. No conflict.

``resolve_repo_root`` is ``@cache``'d; the root ``cli/tests/conftest.py``
autouse clears it at every test's setup, and we clear again here so the chdir
is read on the next resolve, and once more on teardown.
"""
from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def _anchor_megatron_repo_root(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    from fno.paths import resolve_repo_root

    resolve_repo_root.cache_clear()
    yield
    resolve_repo_root.cache_clear()
