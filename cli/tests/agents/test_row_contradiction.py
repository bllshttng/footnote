"""The Python emitter asserts the shared row-contradiction truth table."""

import json
from pathlib import Path

from fno.agents.row_contradiction import project_row


_FIXTURE = Path(__file__).resolve().parents[3] / "schemas" / "agents-row-contradiction.json"


def test_shared_row_contradiction_fixture() -> None:
    fixture = json.loads(_FIXTURE.read_text())
    for case in fixture["cases"]:
        actual = project_row(case["row"], now=fixture["now"])
        for key, expected in case["expected"].items():
            assert actual[key] == expected, case["name"]
