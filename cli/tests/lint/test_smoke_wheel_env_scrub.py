"""Every smoke script that provisions a distribution and then runs it must scrub
``PYTHONPATH``.

These scripts install fno into a throwaway venv / uv tool dir / keg and then
exercise it - some through ``python -c``, some through the ``fno-py`` console
script or the cargo shim. Either way an inherited ``PYTHONPATH=<repo>/cli/src``
prepends the source tree to ``sys.path``, so the assertions answer from a
checkout instead of the installed artifact and the smoke goes green on a
distribution that shipped nothing. ``scripts/ci/preflight.sh`` exports exactly
that, and it already produced two false wheel-defect reports against
``test_build.sh`` before that file got its own ``unset PYTHONPATH``.

Deliberately an explicit registry rather than a shape-matching heuristic. The
first draft of this lint classified on ``pip install`` plus ``python -c``, which
silently covered ONE of the five real channels - a guard on one of N reachable
paths is decorative, which is the exact failure it was written to prevent. An
allowlist cannot drift: ``test_new_script_is_classified`` fails until a human
puts each new install-channel smoke in one bucket or the other.
"""
import re
from pathlib import Path

import pytest

SMOKE_DIR = Path(__file__).resolve().parents[2] / "tests" / "smoke"

SCRUB = "unset PYTHONPATH"

#: Scripts that install fno somewhere and then RUN it. The scrub is load-bearing
#: in every one of them, whether they inspect via `python -c` or a console script.
WHEEL_CHANNEL_SMOKES = {
    "brew_formula_smoke.sh",  # keg bin: `fno-py --version`
    "cargo_bootstrap_smoke.sh",  # cargo shim self-provisions, then runs
    "clean_machine_smoke.sh",  # fresh venv: `python -c "import fno..."`
    "fno_sh_smoke.sh",  # uv tool bin: `fno-py --version`
    "test_build.sh",  # throwaway venv: wheel contents + `python -c`
}

#: Scripts that only MENTION an install channel inside a text assertion (they
#: grep the installer's source, never run one). Nothing imports fno, so the
#: scrub would be noise.
TEXT_ONLY_SMOKES = {
    "test_postinstall.sh",  # greps .claude-plugin/postinstall.sh for needles
}

#: Catches any script referencing an install channel, so a newly added one
#: cannot slip past unclassified. Broader than the buckets on purpose.
INSTALL_MARKER_RE = re.compile(
    r"""pip["']?\s+install|uv\s+tool\s+install|brew\s+install|cargo\s+install"""
    r"""|FNO_(BOOTSTRAP|INSTALL)_WHEEL"""
)


def _scripts_touching_an_install_channel() -> set[str]:
    return {
        s.name
        for s in SMOKE_DIR.glob("*.sh")
        if INSTALL_MARKER_RE.search(s.read_text(encoding="utf-8"))
    }


def test_registry_matches_reality() -> None:
    """A registry naming a file that no longer exists protects nothing."""
    present = {s.name for s in SMOKE_DIR.glob("*.sh")}
    missing = (WHEEL_CHANNEL_SMOKES | TEXT_ONLY_SMOKES) - present
    assert not missing, (
        f"registry names smoke scripts that no longer exist: {sorted(missing)}. "
        "Drop them, or fix the rename."
    )


def test_new_script_is_classified() -> None:
    """The anti-drift guard: every install-channel smoke lands in a bucket.

    Without this, adding a sixth channel silently inherits no coverage - which
    is how the first version of this lint ended up protecting one file.
    """
    unclassified = _scripts_touching_an_install_channel() - (
        WHEEL_CHANNEL_SMOKES | TEXT_ONLY_SMOKES
    )
    assert not unclassified, (
        f"{sorted(unclassified)} reference an install channel but are in neither "
        "WHEEL_CHANNEL_SMOKES nor TEXT_ONLY_SMOKES. If the script runs what it "
        f"installs, add it to the former and give it `{SCRUB}`; if it only greps "
        "an installer's text, add it to the latter."
    )


@pytest.mark.parametrize("name", sorted(WHEEL_CHANNEL_SMOKES))
def test_scrubs_pythonpath(name: str) -> None:
    body = (SMOKE_DIR / name).read_text(encoding="utf-8")
    assert SCRUB in body, (
        f"{name} installs fno and then runs it, but never runs `{SCRUB}`. An "
        "inherited PYTHONPATH (scripts/ci/preflight.sh exports <repo>/cli/src) "
        "puts the source tree ahead of the installed artifact on sys.path, so "
        "this smoke goes green on a distribution that shipped nothing."
    )
