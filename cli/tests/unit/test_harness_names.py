"""x-cec8: the harness-name set is platform-layer data, and importing the
platform layer no longer drags the runtime in at import time."""
from __future__ import annotations

import sys


def test_known_harnesses_matches_capability_table_keys():
    """Adding a harness is a coupled change: the name lands in KNOWN_HARNESSES
    and a capability dict lands in _HARNESS_CAPS, or the table's import-time
    assertion fires. Pin that contract so a one-sided add is caught here too."""
    from fno.agents.harness_map import _HARNESS_CAPS, known_harnesses
    from fno.harness_names import KNOWN_HARNESSES

    assert set(known_harnesses()) == set(KNOWN_HARNESSES)
    assert set(_HARNESS_CAPS) == set(KNOWN_HARNESSES)
    assert known_harnesses() == sorted(known_harnesses())


def test_importing_dispatch_flags_does_not_drag_the_runtime():
    """x-cec8 defect: importing fno.dispatch_flags used to eagerly import
    fno.agents (via harness_identity -> harness_map). The name set now lives at
    L0, so the platform layer imports no runtime module at import time."""
    for mod in [k for k in list(sys.modules) if k.startswith("fno.agents")]:
        del sys.modules[mod]
    import fno.dispatch_flags  # noqa: F401

    dragged = [k for k in sys.modules if k.startswith("fno.agents")]
    assert not dragged, f"dispatch_flags dragged the runtime at import: {dragged}"
