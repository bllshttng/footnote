"""Every smoke script that inspects an INSTALLED wheel must scrub PYTHONPATH.

These scripts build/install a wheel into a throwaway venv and then assert on
what that venv can import. An inherited ``PYTHONPATH=<repo>/cli/src`` redirects
every such import to the source tree, so the assertions pass on a wheel that
ships nothing. ``scripts/ci/preflight.sh`` exports exactly that, and it has
already produced false wheel-defect reports twice against
``test_build.sh`` (fixed by its own ``unset PYTHONPATH``) - a guard on one
script while its siblings stay unscrubbed is decorative, so this pins the
whole class rather than the one file that was reported.
"""
import re
from pathlib import Path

import pytest

SMOKE_DIR = Path(__file__).resolve().parents[2] / "tests" / "smoke"

#: Marks a script that installs a wheel. Both invocation shapes in use here are
#: interpreter-relative (`"$VENV/bin/pip" install`), so a literal "pip install"
#: substring silently misses them - match across the closing quote instead.
INSTALL_RE = re.compile(r"""pip["']?\s+install""")
#: ...but only flag it when it reads back through that installed interpreter,
#: which is the step an inherited PYTHONPATH hijacks. Same quote problem:
#: `"$BIN/python" -c` must match as readily as a bare `python3 -c`.
IMPORT_RE = re.compile(r"""python[0-9.]*["']?\s+-c""")


def _wheel_inspecting_scripts() -> list[Path]:
    hits = []
    for script in sorted(SMOKE_DIR.glob("*.sh")):
        body = script.read_text(encoding="utf-8")
        if INSTALL_RE.search(body) and IMPORT_RE.search(body):
            hits.append(script)
    return hits


def test_the_probe_finds_something() -> None:
    """A zero-hit sweep is a claim, not a pass: fail if the probe found nothing."""
    assert _wheel_inspecting_scripts(), (
        f"no wheel-inspecting smoke scripts found under {SMOKE_DIR}; the marker "
        "heuristic has drifted and this lint is silently covering nothing"
    )


@pytest.mark.parametrize(
    "script", _wheel_inspecting_scripts(), ids=lambda p: p.name
)
def test_scrubs_pythonpath(script: Path) -> None:
    body = script.read_text(encoding="utf-8")
    assert "unset PYTHONPATH" in body, (
        f"{script.name} installs a wheel and imports from it, but never runs "
        "`unset PYTHONPATH`. An inherited PYTHONPATH (preflight.sh exports "
        "<repo>/cli/src) redirects those imports to the source tree, so this "
        "smoke goes green on a wheel that ships nothing."
    )
