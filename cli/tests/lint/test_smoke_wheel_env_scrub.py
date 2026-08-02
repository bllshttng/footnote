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

VAR_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

#: The runtime proof that the scrub took effect. Checked for separately because
#: it, not the lint, is what catches a scrub that parsed fine and did nothing.
ASSERT_RE = re.compile(r'^\[ -z "\$\{PYTHONPATH:-\}" \]')


def is_scrub(line: str) -> bool:
    """True for an unconditional, top-level `unset` that clears the VARIABLE.

    Token-parsed rather than regex-matched, because each rejected form below was
    a real review finding and a pattern loose enough to accept one tends to
    accept the rest:

    * indented -> inside a conditional, loop, subshell, or heredoc body, so it
      may never run in the shell that performs the install
    * ``unset -f PYTHONPATH`` -> clears a function; the variable survives
    * ``unset OTHER # PYTHONPATH`` -> only named in a comment
    * ``unset PYTHONPATH 2>/dev/null || true`` -> a failed unset (readonly var)
      is swallowed and the smoke proceeds contaminated
    """
    if line != line.lstrip():
        return False
    tokens = line.split()
    if not tokens or tokens[0] != "unset":
        return False
    args = tokens[1:]
    if args[:1] == ["-v"]:  # the explicit variable form; `-f` is a function
        args = args[1:]
    if not args or not all(VAR_NAME.fullmatch(a) for a in args):
        return False
    return "PYTHONPATH" in args

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
        "unset -v PYTHONPATH",
        "unset FNO_REPO_ROOT PYTHONPATH",
    ],
)
def test_is_scrub_accepts_real_unsets(line: str) -> None:
    assert is_scrub(line)


@pytest.mark.parametrize(
    "line",
    [
        "# unset PYTHONPATH",  # commented out: runs nothing
        "unset -f PYTHONPATH",  # clears a function; the variable survives
        "unset OTHER # PYTHONPATH",  # PYTHONPATH only named in a comment
        "unset PYTHONPATHX",  # a different variable
        'echo "unset PYTHONPATH"',  # printed, not executed
        "  unset PYTHONPATH",  # indented: inside a block, may never run
        "unset PYTHONPATH 2>/dev/null || true",  # a readonly-var failure is swallowed
    ],
)
def test_is_scrub_rejects_no_ops(line: str) -> None:
    assert not is_scrub(line)


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
    scrub_line = next((i for i, ln in enumerate(lines, start=1) if is_scrub(ln)), None)
    assert scrub_line is not None, (
        f"{name} installs fno and then runs it, but has no unconditional "
        "top-level `unset PYTHONPATH`. An inherited PYTHONPATH "
        "(scripts/ci/preflight.sh exports <repo>/cli/src) puts the source tree "
        "ahead of the installed artifact on sys.path, so this smoke goes green "
        "on a distribution that shipped nothing. See is_scrub for the forms that "
        "look like a scrub and are not."
    )
    assert_line = _first_match(lines, ASSERT_RE)
    assert assert_line is not None, (
        f'{name} never verifies the scrub took: add `[ -z "${{PYTHONPATH:-}}" ] '
        '|| {{ echo ...; exit 1; }}` under the unset. The static check here can '
        "only prove the line is present and well-formed; the runtime check is "
        "what catches an unset that parsed fine and cleared nothing (a readonly "
        "variable, or a scrub that ran in some other shell)."
    )
    install_line = _first_match(lines, INSTALL_RE)
    assert install_line is not None, (
        f"{name} is registered as a wheel-channel smoke but no install was "
        "found. Either it no longer installs anything (move it to "
        "NON_INSTALL_SMOKES) or INSTALL_RE needs its channel."
    )
    assert scrub_line < assert_line < install_line, (
        f"{name} orders these wrong: scrub on line {scrub_line}, verification on "
        f"line {assert_line}, install on line {install_line}. Required order is "
        "scrub, then verify, then install - anything that runs before the scrub "
        "is exposed, and a verification below the install proves nothing about it."
    )
