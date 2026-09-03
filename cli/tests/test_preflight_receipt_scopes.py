"""The optional preflight leg scope set stays in step with preflight.sh.

A leg added to scripts/ci/preflight.sh mints receipts carrying its scope.
`_trusted_preflight_producer` discards a receipt whose scope set is not a
subset of BASE | OPTIONAL, quietly: the receipt is dropped, not rejected, so
`check_verification_evidence` fails and preflight.required installs refuse to
merge. tracker-gates:fno broke this the day it landed; file-budget:fno was
registered with it on purpose. These tests pin the registration.
"""

from fno.pr._preflight import (
    _PREFLIGHT_BASE_SCOPE,
    _PREFLIGHT_GATE_SCOPE,
    _PREFLIGHT_OPTIONAL_SCOPE,
    _trusted_preflight_producer,
)


def _event(scopes: list[str]) -> dict:
    return {
        "source": "target",
        "data": {
            "command": ["scripts/ci/preflight.sh"],
            "producer": {"kind": "preflight", "id": "host:x"},
            "environment": {"host": "host", "runner": "scripts/ci/preflight.sh"},
            "scope": list(scopes),
        },
    }


def _base_plus(scope: str) -> list[str]:
    return sorted(_PREFLIGHT_BASE_SCOPE) + [scope]


def test_optional_scopes_are_registered():
    # Every leg preflight.sh can record must live in the gate scope, or its
    # receipts are discarded on preflight.required installs.
    assert "file-budget:fno" in _PREFLIGHT_OPTIONAL_SCOPE
    assert "tracker-gates:fno" in _PREFLIGHT_OPTIONAL_SCOPE
    assert "squads-leak-guard:fno" in _PREFLIGHT_OPTIONAL_SCOPE
    assert _PREFLIGHT_GATE_SCOPE == _PREFLIGHT_BASE_SCOPE | _PREFLIGHT_OPTIONAL_SCOPE


def test_receipt_carrying_file_budget_scope_is_trusted():
    assert _trusted_preflight_producer(_event(_base_plus("file-budget:fno"))) is True


def test_receipt_without_new_leg_still_trusted():
    assert _trusted_preflight_producer(_event(sorted(_PREFLIGHT_BASE_SCOPE))) is True


def test_unknown_scope_still_discarded():
    assert _trusted_preflight_producer(_event(_base_plus("no-such-leg:fno"))) is False
