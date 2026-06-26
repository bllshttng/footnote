"""Regression: parse_short_id must read claude's --bg receipt with or without
ANSI color. claude >= 2.1.191 colorizes the short-id even over a non-TTY pipe
(`backgrounded · \\x1b[36m<id>\\x1b[39m · <name>`), which silently broke
`fno agents spawn` on the live round-trip (x-7060). Surfaced by the AC1 proof.
"""
from __future__ import annotations

import pytest

from fno.agents.providers.claude import ProviderParseError, parse_short_id


def test_parse_short_id_plain():
    assert parse_short_id("backgrounded · ad3f5e65 · relay-a\n") == "ad3f5e65"


def test_parse_short_id_strips_ansi_color():
    # Exactly what claude 2.1.191 emits over a captured (non-TTY) pipe.
    colored = "backgrounded · \x1b[36mad3f5e65\x1b[39m · relay-a\n"
    assert parse_short_id(colored) == "ad3f5e65"


def test_parse_short_id_rejects_garbage():
    with pytest.raises(ProviderParseError):
        parse_short_id("not a backgrounded receipt\n")
