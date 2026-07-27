"""AC13-INV: the axis tombstones keep their message and lose their expiry date.

The `--provider` tombstones are not deprecated aliases waiting to expire. Each
is a hidden option whose only behavior is to exit 2 with the axis map, and that
message is worth keeping indefinitely - deleting the option would trade a
message naming `--harness/-H` for Click's bare "No such option: --provider".

So the date goes and the message stays. These two tests pin both halves.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import typer

from fno._flag_aliases import PROVIDER_AXIS_TOMBSTONE, refuse_retired_provider

_SRC = Path(__file__).resolve().parents[3] / "cli" / "src" / "fno"


def test_no_runtime_string_promises_a_0_4_0_removal():
    """0.3.x is the long-term home, so a 0.4.0 date is a promise that will not
    be kept. No runtime help string may carry one."""
    sources = list(_SRC.rglob("*.py"))
    # A scan pointed at the wrong directory finds nothing and passes for the
    # wrong reason; anchor it so a moved test file fails loudly instead.
    assert len(sources) > 100, f"scan found only {len(sources)} files under {_SRC}"
    offenders = [
        f"{path.relative_to(_SRC)}:{n}"
        for path in sources
        for n, line in enumerate(path.read_text().splitlines(), 1)
        if "Removed at 0.4.0" in line
    ]
    assert offenders == [], f"runtime text promises a 0.4.0 removal: {offenders}"


def test_tombstone_still_names_the_harness_axis():
    """The message itself is correct and unchanged: it must keep naming the
    flag that replaced it, or the refusal stops teaching anything."""
    assert "--harness/-H" in PROVIDER_AXIS_TOMBSTONE
    assert "0.4.0" not in PROVIDER_AXIS_TOMBSTONE


def test_retired_provider_exits_2():
    with pytest.raises(typer.Exit) as excinfo:
        refuse_retired_provider("claude")
    assert excinfo.value.exit_code == 2


def test_retired_provider_is_a_noop_when_absent():
    """The tombstone must not fire on verbs where the flag was simply omitted."""
    assert refuse_retired_provider(None) is None
