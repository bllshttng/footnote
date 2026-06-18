import pytest


@pytest.fixture(autouse=True)
def _reset_gate_coercion_warned():
    """Snapshot and restore the module-level legacy-coercion seen-set.

    The seen-set deduplicates DeprecationWarning emissions to once per
    (field, value) per process. Across tests sharing a pytest worker,
    a prior test that warned for (clean_passed, "passed") would silence
    the warning in a later test that asserts pytest.warns(DeprecationWarning)
    for the same pair. The autouse fixture clears the set per-test and
    restores it after so test order is irrelevant.
    """
    from fno.schemas import target as _target
    snapshot = set(_target._GATE_COERCION_WARNED)
    _target._GATE_COERCION_WARNED.clear()
    try:
        yield
    finally:
        _target._GATE_COERCION_WARNED.clear()
        _target._GATE_COERCION_WARNED.update(snapshot)
