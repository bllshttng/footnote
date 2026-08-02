"""Every smoke script that provisions a distribution and then runs it must scrub
``PYTHONPATH``, with a real statement, before it installs anything.

These scripts install fno into a throwaway venv / uv tool dir / cargo shim / brew
keg and then exercise it - some through ``python -c``, some through the
``fno-py`` console script. Either way an inherited ``PYTHONPATH=<repo>/cli/src``
prepends the source tree to ``sys.path``, so the assertions answer from a
checkout instead of the installed artifact and the smoke goes green on a
distribution that shipped nothing. ``scripts/ci/preflight.sh`` exports exactly
that, and it already produced two false wheel-defect reports against
``test_build.sh`` before that file got its own ``unset PYTHONPATH``.

Three deliberate design choices, each closing a way this guard could have been
decorative (the pitfall in AGENTS.md - a guard on one of N reachable paths):

1. **Every** ``*.sh`` in the dir must be classified, not just the ones matching
   an install-marker regex. The first draft classified on ``pip install`` plus
   ``python -c`` and covered ONE of the five real channels; the second still let
   a new script using ``sh "$INSTALLER"`` or ``pip3`` slip through unclassified.
   An unlisted file now fails, so a new smoke cannot inherit silence.
2. The scrub must be an **executable statement**, not the substring. A commented
   ``# unset PYTHONPATH`` satisfies a naive ``in`` check and does nothing.
3. The scrub must come **before the first install**, so it cannot be appended
   below the code it is supposed to protect.
"""
import re
from pathlib import Path

import pytest

SMOKE_DIR = Path(__file__).resolve().parents[2] / "tests" / "smoke"

#: An executable scrub that actually unsets the VARIABLE. Not a commented line,
#: not `unset -f PYTHONPATH` (that clears a function and leaves the variable
#: set), and not a trailing `# PYTHONPATH` on some other unset: PYTHONPATH has to
#: be a bare operand of the canonical command, optionally alongside other names.
SCRUB_RE = re.compile(
    r"^[ \t]*unset(?:[ \t]+-v)?"  # `unset` or its explicit `unset -v` form
    r"(?:[ \t]+[A-Za-z_][A-Za-z0-9_]*)*"  # other variables cleared in the same statement
    r"[ \t]+PYTHONPATH(?:[ \t;]|$)"  # PYTHONPATH itself, as an operand
)

#: Provisions fno somewhere and then RUNS it. The scrub is load-bearing in every
#: one, whether it inspects via `python -c` or a console script.
WHEEL_CHANNEL_SMOKES = {
    "brew_formula_smoke.sh",  # keg bin: `fno-py --version`
    "cargo_bootstrap_smoke.sh",  # cargo shim self-provisions, then runs
    "clean_machine_smoke.sh",  # fresh venv: `python -c "import fno..."`
    "fno_sh_smoke.sh",  # uv tool bin: `fno-py --version`
    "test_build.sh",  # throwaway venv: wheel contents + `python -c`
}

#: Everything else in the dir: these never install a distribution and then import
#: from it, so the scrub would be noise. Listed explicitly rather than inferred,
#: so adding a smoke script is a decision instead of a silent exemption.
NON_INSTALL_SMOKES = {
    "run-all.sh",
    "test-db-schema.sh",
    "test_done.sh",
    "test_find_new.sh",
    "test_help.sh",
    "test_inbox_cross_project.sh",
    "test_inbox_cross_project_e2e.sh",
    "test_inbox_drain_e2e.sh",
    "test_inbox_reply_thread.sh",
    "test_inbox_roundtrip.sh",
    "test_inbox_triage_subprocess.sh",
    "test_json_flag.sh",
    "test_migrate_inbox_path.sh",
    "test_postinstall.sh",  # greps the installer's TEXT; runs no installer
    "test_preflight_hermetic.sh",  # sets PYTHONPATH on purpose, installs nothing
    "test_skeleton.sh",
    "test_stop_hook_wake_log.sh",
    "test_unknown.sh",
    "test_wake_roundtrip.sh",
}

#: Where a wheel/formula/crate actually gets installed. Only used to locate the
#: FIRST install in a script already known to be a channel smoke, so it never
#: decides coverage - a miss here cannot silently exempt anything.
INSTALL_RE = re.compile(
    r"""pip["']?\s+install|uv\s+tool\s+install|brew\s+install|cargo\s+install"""
    r"""|FNO_(BOOTSTRAP|INSTALL)_WHEEL=|sh\s+["']?\$INSTALLER"""
)


def _lines(name: str) -> list[str]:
    return (SMOKE_DIR / name).read_text(encoding="utf-8").split("\n")


def _first_match(lines: list[str], pattern: re.Pattern[str]) -> int | None:
    """1-indexed line of the first non-comment match, or None."""
    for i, line in enumerate(lines, start=1):
        if line.lstrip().startswith("#"):
            continue
        if pattern.search(line):
            return i
    return None


@pytest.mark.parametrize(
    "line",
    [
        "unset PYTHONPATH",
        "unset PYTHONPATH 2>/dev/null || true",
        "  unset PYTHONPATH",
        "unset -v PYTHONPATH",
        "unset FNO_REPO_ROOT PYTHONPATH 2>/dev/null || true",
    ],
)
def test_scrub_regex_accepts_real_unsets(line: str) -> None:
    assert SCRUB_RE.match(line)


@pytest.mark.parametrize(
    "line",
    [
        "# unset PYTHONPATH",  # commented out: runs nothing
        "unset -f PYTHONPATH",  # clears a function; the variable survives
        "unset OTHER # PYTHONPATH",  # PYTHONPATH only named in a comment
        "unset PYTHONPATHX",  # a different variable
        'echo "unset PYTHONPATH"',  # printed, not executed
    ],
)
def test_scrub_regex_rejects_no_ops(line: str) -> None:
    assert not SCRUB_RE.match(line)


def test_every_script_is_classified() -> None:
    """Fail closed: an unclassified script gets no coverage and no warning."""
    present = {s.name for s in SMOKE_DIR.glob("*.sh")}
    classified = WHEEL_CHANNEL_SMOKES | NON_INSTALL_SMOKES
    unclassified = present - classified
    assert not unclassified, (
        f"{sorted(unclassified)} are not classified. If a script installs fno "
        "and then runs it, add it to WHEEL_CHANNEL_SMOKES and give it an "
        "`unset PYTHONPATH` before the install; otherwise add it to "
        "NON_INSTALL_SMOKES."
    )
    stale = classified - present
    assert not stale, (
        f"the registry names smoke scripts that no longer exist: {sorted(stale)}. "
        "Drop them, or fix the rename."
    )


@pytest.mark.parametrize("name", sorted(WHEEL_CHANNEL_SMOKES))
def test_scrub_is_executable_and_precedes_the_install(name: str) -> None:
    lines = _lines(name)
    scrub_line = _first_match(lines, SCRUB_RE)
    assert scrub_line is not None, (
        f"{name} installs fno and then runs it, but has no executable "
        "`unset PYTHONPATH` statement. An inherited PYTHONPATH "
        "(scripts/ci/preflight.sh exports <repo>/cli/src) puts the source tree "
        "ahead of the installed artifact on sys.path, so this smoke goes green "
        "on a distribution that shipped nothing. A commented-out line does not "
        "count."
    )
    install_line = _first_match(lines, INSTALL_RE)
    assert install_line is not None, (
        f"{name} is registered as a wheel-channel smoke but no install was "
        "found. Either it no longer installs anything (move it to "
        "NON_INSTALL_SMOKES) or INSTALL_RE needs its channel."
    )
    assert scrub_line < install_line, (
        f"{name} scrubs PYTHONPATH on line {scrub_line}, after the install on "
        f"line {install_line}. Hoist the scrub above it: a scrub that runs after "
        "the code it protects leaves that code exposed."
    )
