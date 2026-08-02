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

Every requirement below exists because an earlier draft of this file failed open
in exactly that way - the decorative-guard pitfall in AGENTS.md, found in the
lint written to prevent it:

1. **Every** ``*.sh`` in the dir must be classified. Matching on ``pip install``
   plus ``python -c`` covered ONE of the five real channels; broadening the
   markers still missed a script calling ``sh "$INSTALLER"`` or ``pip3``. An
   unlisted file now fails, so a new smoke cannot inherit silence.
2. The scrub must be a real ``unset`` of the VARIABLE (``is_scrub``), not a
   substring: ``unset -f PYTHONPATH``, a trailing ``# PYTHONPATH`` comment, and
   a commented-out line all satisfy a naive check and clear nothing.
3. It must run **unconditionally in the script's own shell**
   (``top_level_lines``). Column zero does not prove that, since shell needs no
   indentation, and a subshell's unset dies with the subshell.
4. It must be followed by a check that **aborts** when the variable survived
   (``is_verification``). These scripts use ``set -uo pipefail`` without ``-e``,
   so a test with no exit - or one in a comment or a subshell - installs anyway.
5. Scrub, then verify, then install, in that order.

The static checks stop at what static checks can do. The runtime verification in
each script is what catches a scrub that parsed fine and cleared nothing.
"""
import re
from pathlib import Path

import pytest

SMOKE_DIR = Path(__file__).resolve().parents[2] / "tests" / "smoke"

VAR_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

#: The runtime proof that the scrub took effect. Checked for separately because
#: it, not the lint, is what catches a scrub that parsed fine and did nothing.
TEST_RE = re.compile(r'^\[ -z "\$\{PYTHONPATH:-\}" \].*\|\|')
#: The `exit` is required and must be the CURRENT shell's: these scripts run
#: under `set -uo pipefail` WITHOUT `-e`, so a bare test, one ending `|| true`,
#: and `|| (exit 1)` (which exits only the subshell) all install anyway.
EXIT_RE = re.compile(r"\bexit [1-9]")
SUBSHELL_EXIT_RE = re.compile(r"\(\s*exit\b")


def strip_comment(line: str) -> str:
    """Drop a trailing `#` comment, respecting quotes.

    Needed because a comment can otherwise supply the token a check is looking
    for - `|| echo warning # exit 1` reads as an exit and terminates nothing.
    """
    quote: str | None = None
    for i, ch in enumerate(line):
        if quote:
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif ch == "#" and (i == 0 or line[i - 1].isspace()):
            return line[:i]
    return line


def blank_quoted(line: str) -> str:
    """Replace quoted spans with spaces, keeping length and structure.

    A quoted `exit 1` is a string being printed, not a command being run:
    `|| echo "please exit 1"` prints advice and installs anyway.
    """
    out = []
    quote: str | None = None
    for ch in line:
        if quote:
            out.append(" ")
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            out.append(" ")
        else:
            out.append(ch)
    return "".join(out)


def is_verification(line: str) -> bool:
    """True for a check that aborts THIS shell when PYTHONPATH survived."""
    code = strip_comment(line)
    if not TEST_RE.match(code) or SUBSHELL_EXIT_RE.search(code):
        return False
    # The exit must be a command, so look for it with strings blanked out.
    return bool(EXIT_RE.search(blank_quoted(code)))

#: Lines that open a shell block, and the ones that close it. Used to reject a
#: scrub nested inside a conditional, loop, function, or heredoc, where it may
#: never run in the shell that performs the install. Deliberately a small
#: structural tracker and not a shell parser: it recognises the block forms this
#: repo's smoke scripts actually use, and `test_tracker_sees_through_*` pins the
#: cases that matter. A construct it cannot see is reported as nested (the safe
#: direction - a false alarm names a line, a false pass protects nothing).
#: A bare `(` opening a multiline subshell counts too: a scrub inside one clears
#: only the child's copy, leaving the parent - which runs the install - dirty.
OPENS_RE = re.compile(r"^\s*(if|while|until|for|case|select)\b|\{\s*$|\(\)\s*\{|\(\s*$")
CLOSES_RE = re.compile(r"^\s*(fi|done|esac|\}|\))\b|^\s*\)\s*$")
HEREDOC_RE = re.compile(r"<<-?\s*[\"']?([A-Za-z_][A-Za-z0-9_]*)[\"']?")


def top_level_lines(lines: list[str]) -> set[int]:
    """1-indexed line numbers that run unconditionally in the script's own shell.

    Excludes anything inside a block or a heredoc body. Column zero is NOT
    evidence of top level - shell needs no indentation - which is why this walks
    the structure instead of measuring whitespace.
    """
    top: set[int] = set()
    depth = 0
    heredoc: str | None = None
    for i, line in enumerate(lines, start=1):
        if heredoc is not None:
            if line.strip() == heredoc:
                heredoc = None
            continue
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if CLOSES_RE.match(line):
            depth = max(0, depth - 1)
            continue
        if depth == 0 and not OPENS_RE.search(line):
            top.add(i)
        if OPENS_RE.search(line):
            depth += 1
        found = HEREDOC_RE.search(line)
        if found:
            heredoc = found.group(1)
    return top


def is_scrub(line: str) -> bool:
    """True for an unconditional, top-level `unset` that clears the VARIABLE.

    Token-parsed rather than regex-matched, because each rejected form below was
    a real review finding and a pattern loose enough to accept one tends to
    accept the rest:

    Whether the line actually runs unconditionally is a separate question, and a
    structural one - see ``top_level_lines``.

    * ``unset -f PYTHONPATH`` -> clears a function; the variable survives
    * ``unset OTHER # PYTHONPATH`` -> only named in a comment
    * ``unset PYTHONPATH 2>/dev/null || true`` -> a failed unset (readonly var)
      is swallowed and the smoke proceeds contaminated
    """
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
        "unset PYTHONPATH 2>/dev/null || true",  # a readonly-var failure is swallowed
    ],
)
def test_is_scrub_rejects_no_ops(line: str) -> None:
    assert not is_scrub(line)


def test_tracker_sees_through_column_zero_nesting() -> None:
    """Column zero is not top level: the reason this walks structure at all."""
    script = [
        "unset PYTHONPATH",  # 1: genuinely top level
        "if [ -n \"$SOMETHING\" ]; then",  # 2
        "unset PYTHONPATH",  # 3: column zero, still conditional
        "fi",  # 4
        "cat <<EOF",  # 5
        "unset PYTHONPATH",  # 6: column zero, inside a heredoc body
        "EOF",  # 7
        "pip install ./x.whl",  # 8
    ]
    top = top_level_lines(script)
    assert 1 in top
    assert 3 not in top
    assert 6 not in top
    assert 8 in top


def test_tracker_reopens_after_a_block_closes() -> None:
    """Depth must come back down, or everything after the first block is hidden."""
    script = ["if x; then", "y", "fi", "unset PYTHONPATH"]
    assert 4 in top_level_lines(script)


def test_tracker_sees_select_loops() -> None:
    """`select` skips its body outright when stdin is at EOF."""
    script = ["select x in a b; do", "unset PYTHONPATH", "done", "pip install ./x.whl"]
    top = top_level_lines(script)
    assert 2 not in top
    assert 4 in top


def test_tracker_sees_multiline_subshells() -> None:
    """A subshell clears only its own copy; the parent still installs dirty."""
    script = ["(", "unset PYTHONPATH", ")", "pip install ./x.whl"]
    top = top_level_lines(script)
    assert 2 not in top
    assert 4 in top


@pytest.mark.parametrize(
    "line",
    [
        '[ -z "${PYTHONPATH:-}" ] || { echo "..."; exit 1; }',
        '[ -z "${PYTHONPATH:-}" ] || exit 1',
    ],
)
def test_verification_must_exit(line: str) -> None:
    assert is_verification(line)


@pytest.mark.parametrize(
    "line",
    [
        '[ -z "${PYTHONPATH:-}" ]',  # reports nothing, stops nothing (no set -e)
        '[ -z "${PYTHONPATH:-}" ] || true',  # explicitly swallows the failure
        '[ -z "${PYTHONPATH:-}" ] || echo "oh well"',  # warns, then installs anyway
        '[ -z "${PYTHONPATH:-}" ] || echo warn # exit 1',  # the exit is in a comment
        '[ -z "${PYTHONPATH:-}" ] || (exit 1)',  # exits the subshell, not the script
        '[ -z "${PYTHONPATH:-}" ] || echo "please exit 1"',  # prints the word, runs nothing
    ],
)
def test_verification_without_an_exit_is_rejected(line: str) -> None:
    assert not is_verification(line)


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
    top = top_level_lines(lines)
    scrub_line = next(
        (i for i, ln in enumerate(lines, start=1) if i in top and is_scrub(ln)), None
    )
    assert scrub_line is not None, (
        f"{name} installs fno and then runs it, but has no unconditional "
        "top-level `unset PYTHONPATH` (one nested in a block, function, or "
        "heredoc does not count - it may never run in the shell that installs). "
        "An inherited PYTHONPATH "
        "(scripts/ci/preflight.sh exports <repo>/cli/src) puts the source tree "
        "ahead of the installed artifact on sys.path, so this smoke goes green "
        "on a distribution that shipped nothing. See is_scrub for the forms that "
        "look like a scrub and are not."
    )
    assert_line = next(
        (i for i, ln in enumerate(lines, start=1) if i in top and is_verification(ln)),
        None,
    )
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
