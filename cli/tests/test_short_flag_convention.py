"""Short-flag convention enforcement (ab-3ff64151 Phase 1, ab-e893ba6e Phase 2,
ab-a04f3f1a Phase 3).

The design (``internal/fno/design/2026-06-03-fno-cli-short-flags.md``)
locks a single short-flag scheme for the ``fno`` Typer CLI:

* UPPERCASE letters are a small fixed GLOBAL register that means the same
  thing on every command: ``-J --json``, ``-A --all``, ``-F --force``,
  ``-N --dry-run``, ``-R --reason``, ``-Y --yolo``. They are reserved and
  never carry a per-command meaning.
* lowercase letters are per-command value flags (may differ per command).
* The 7 shorts that shipped before this convention must never change
  (``-A -I -b -n -f -m -o``).

This test is the *source of truth* for the convention (per the design's
"the collision test is the source of truth"). It is a static AST scan over
every ``typer.Option`` declaration in ``fno`` source, so it needs no
runtime import of the (lazily loaded) command tree and cannot be fooled by
deferred sub-apps.

Covers ACs 1 (Python side), 2 (global register), 3 (no collisions + pins).
The Rust-path side of AC1 and AC4/AC5 live in the Rust crate tests and in
``tests/agents/test_cmd_ask.py``.
"""
from __future__ import annotations

import ast
from collections import defaultdict
from pathlib import Path

import pytest

# --------------------------------------------------------------------------- #
# The convention, declared once.
# --------------------------------------------------------------------------- #

#: GLOBAL register: short -> the ONE long flag it is allowed to mean anywhere.
GLOBAL_REGISTER: dict[str, str] = {
    "-J": "--json",
    "-A": "--all",
    "-F": "--force",
    "-N": "--dry-run",
    "-R": "--reason",
    "-Y": "--yolo",
}
#: Inverse: a command that declares this long MUST carry this short (AC2 positive).
GLOBAL_LONG_TO_SHORT: dict[str, str] = {v: k for k, v in GLOBAL_REGISTER.items()}

#: The 7 shorts that predate the convention and must never change (AC3).
#: Lowercase letters are per-command under the convention, so each pair is
#: pinned to its HOME declaration site (file suffix); another command may
#: legally reuse the letter for a different long (e.g. ``-b --body`` on
#: ``inbox send`` vs ``-b --blocked`` on ``backlog pick``). The uppercase
#: pair (``-I``) stays codebase-exclusive like the global register.
PRE_EXISTING_SHORTS: dict[str, tuple[str, str]] = {
    "-A": ("--all", ""),  # exclusivity enforced by the global-register tests
    "-I": ("--ideas", "graph/cli.py"),
    "-b": ("--blocked", "graph/cli.py"),
    "-n": ("--tail", "agents/cli.py"),
    "-f": ("--follow", "agents/cli.py"),
    "-m": ("--note", "done/cli.py"),
    "-o": ("--output", "codemap_cli/cli.py"),
}

#: Uppercase shorts outside the global register: still exclusive, i.e. the letter
#: maps to exactly one long codebase-wide. ``-H``/``-P`` are the spawn axis pair:
#: uppercase because the lowercase letters mean something else there (``-h`` is
#: help, ``-p`` is headless, mirroring the harnesses' own one-shot short).
EXCLUSIVE_PRE_EXISTING: dict[str, str] = {
    "-I": "--ideas",
    "-H": "--harness",
    "-P": "--provider",
}

#: Phase 2 (US2, ab-e893ba6e): the per-command lowercase map from the design
#: table. Key: (file suffix, enclosing function). Value: long -> short pairs
#: that command MUST declare. Purely additive - long flags are unchanged.
PHASE2_LOWERCASE_MAP: dict[tuple[str, str], dict[str, str]] = {
    ("graph/cli.py", "cmd_add"): {
        "--priority": "-p", "--cwd": "-c", "--details": "-d", "--type": "-t",
    },
    # cmd_idea was unified with cmd_add (x-9ac9): it now shares add's full option
    # set, so -d rides --details (with --description as the no-short alias),
    # matching cmd_add above instead of idea's old --description/-d primary.
    ("graph/cli.py", "cmd_idea"): {
        "--details": "-d", "--priority": "-p", "--cwd": "-c",
    },
    ("graph/cli.py", "cmd_intake"): {"--title": "-t", "--priority": "-p"},
    ("graph/cli.py", "cmd_update"): {
        "--priority": "-p", "--cwd": "-c", "--title": "-t",
    },
    ("graph/cli.py", "cmd_next"): {"--project": "-p"},
    ("graph/cli.py", "cmd_ready"): {"--project": "-p"},
    ("graph/cli.py", "cmd_find"): {
        "--project": "-p", "--status": "-s", "--domain": "-d",
    },
    ("backlog/capture.py", "cmd_add"): {
        "--source": "-s", "--where": "-w", "--priority": "-p",
    },
    # cmd_send moved from agents/cli.py to mail/cli.py in ab-cee91152 (messaging
    # extracted into the dedicated `fno mail` namespace); its lowercase shorts
    # moved with it (-k --kind, -b --body, plus the pre-existing -p/-c).
    ("mail/cli.py", "cmd_send"): {
        "--kind": "-k", "--body": "-b", "--provider": "-p", "--cwd": "-c",
    },
    ("providers/cli.py", "add_provider"): {
        "--cli": "-c", "--auth": "-a", "--scope": "-s", "--priority": "-p",
    },
    # gates/cli.py entries removed: the `fno gate` sub-app was deleted by the
    # control-plane collapse wedge (ab-d0337fbc).
    ("events/cli.py", "emit"): {"--type": "-t", "--data": "-d", "--source": "-s"},
    # Phase 3 reordered done's multi-name decl canonical-first, so the scan's
    # primary long is now --pr-number (the --pr spelling remains accepted).
    ("done/cli.py", "done_command"): {"--pr-number": "-p", "--link": "-l"},
    ("done/cli.py", "_cli_callback"): {"--pr-number": "-p", "--link": "-l"},
    ("carveout/cli.py", "add"): {"--kind": "-k", "--priority": "-p"},
}

#: Phase 3 (US3, ab-a04f3f1a): two-spelling canonicalization. Key: (file
#: suffix, enclosing function). Value: list of (canonical, legacy) long-flag
#: pairs that command must declare. The canonical spelling is a normal
#: visible option; the legacy spelling is a SEPARATE hidden=True option (the
#: "hidden deprecated alias") merged in the command body via
#: ``fno._flag_aliases.merge_deprecated_alias``.
TWO_SPELLING_SITES: dict[tuple[str, str], list[tuple[str, str]]] = {
    # ("fno/cli.py", "loop") and ("gates/cli.py", "check") removed:
    # `fno loop` became a flagless supersession stub and the `fno gate`
    # sub-app was deleted by the control-plane collapse wedge (ab-d0337fbc).
    ("fno/cli.py", "review"): [("--session-id", "--session")],
    ("graph/cli.py", "cmd_cost"): [("--session-id", "--session")],
    ("worker/cli.py", "review"): [("--session-id", "--session")],
    ("worker/cli.py", "external"): [("--pr-number", "--pr")],
    ("reality_check/cli.py", "gh"): [("--pr-number", "--pr")],
    ("retro/cli.py", "run"): [
        ("--session-id", "--session"),
        ("--pr-number", "--pr"),
    ],
}

#: Visible --session/--pr declarations exempt from the canonicalization
#: invariant. None currently: every visible long must use the canonical
#: --session-id/--pr-number spelling (with a hidden short alias).
TWO_SPELLING_EXEMPTIONS: set[tuple[str, str, str]] = set()

SRC_ROOT = Path(__file__).resolve().parents[1] / "src" / "fno"


# --------------------------------------------------------------------------- #
# AST scan.
# --------------------------------------------------------------------------- #

class Decl:
    """One ``typer.Option(...)`` declaration."""

    __slots__ = ("long", "longs", "shorts", "file", "func", "lineno", "hidden")

    def __init__(
        self,
        long: str | None,
        longs: list[str],
        shorts: list[str],
        file: str,
        func: str,
        lineno: int,
        hidden: bool,
    ):
        self.long = long
        self.longs = longs
        self.shorts = shorts
        self.file = file
        self.func = func
        self.lineno = lineno
        self.hidden = hidden

    @property
    def where(self) -> str:
        return f"{self.file}:{self.lineno} ({self.func})"


def _norm_long(flag: str) -> str:
    """``--blocked/--no-blocked`` -> ``--blocked``; the secondary is not a name."""
    return flag.split("/", 1)[0]


def _scan() -> list[Decl]:
    decls: list[Decl] = []
    for path in sorted(SRC_ROOT.rglob("*.py")):
        rel = path.relative_to(SRC_ROOT.parent.parent)
        if "test" in path.name:
            continue
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        parents: dict[ast.AST, ast.AST] = {}
        for node in ast.walk(tree):
            for child in ast.iter_child_nodes(node):
                parents[child] = node

        def enclosing_func(n: ast.AST) -> str:
            cur: ast.AST | None = n
            while cur is not None:
                cur = parents.get(cur)
                if isinstance(cur, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    return cur.name
            return "<module>"

        for node in ast.walk(tree):
            if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)):
                continue
            if node.func.attr != "Option":
                continue
            flags = [
                a.value
                for a in node.args
                if isinstance(a, ast.Constant)
                and isinstance(a.value, str)
                and a.value.startswith("-")
            ]
            longs = [_norm_long(f) for f in flags if f.startswith("--")]
            shorts = [f for f in flags if not f.startswith("--")]
            long = longs[0] if longs else None
            hidden = any(
                kw.arg == "hidden"
                and isinstance(kw.value, ast.Constant)
                and kw.value.value is True
                for kw in node.keywords
            )
            decls.append(
                Decl(long, longs, shorts, str(rel), enclosing_func(node), node.lineno, hidden)
            )
    return decls


@pytest.fixture(scope="module")
def decls() -> list[Decl]:
    found = _scan()
    assert found, f"no typer.Option declarations found under {SRC_ROOT}"
    return found


# --------------------------------------------------------------------------- #
# AC2 - global register.
# --------------------------------------------------------------------------- #

def test_global_register_never_carries_a_local_meaning(decls: list[Decl]) -> None:
    """AC2 negative invariant: -J -A -F -N -R -Y map ONLY to their global long."""
    offenders: list[str] = []
    for d in decls:
        for s in d.shorts:
            if s in GLOBAL_REGISTER and d.long != GLOBAL_REGISTER[s]:
                offenders.append(f"{s} -> {d.long} at {d.where} (reserved for {GLOBAL_REGISTER[s]})")
    assert not offenders, "global-register letters reused for a local meaning:\n" + "\n".join(offenders)


def test_every_global_long_carries_its_global_short(decls: list[Decl]) -> None:
    """AC2 positive: any option declaring a global long has the matching short."""
    missing: list[str] = []
    for d in decls:
        if d.long in GLOBAL_LONG_TO_SHORT:
            want = GLOBAL_LONG_TO_SHORT[d.long]
            if want not in d.shorts:
                missing.append(f"{d.long} missing {want} at {d.where}")
    assert not missing, "global longs without their reserved short:\n" + "\n".join(missing)


# --------------------------------------------------------------------------- #
# AC3 - no collisions + pinned pre-existing shorts.
# --------------------------------------------------------------------------- #

def test_no_short_collision_within_a_command(decls: list[Decl]) -> None:
    """AC3: no single command declares the same short for two different longs."""
    per_cmd: dict[tuple[str, str], dict[str, set[str]]] = defaultdict(lambda: defaultdict(set))
    for d in decls:
        for s in d.shorts:
            per_cmd[(d.file, d.func)][s].add(d.long or "?")
    collisions: list[str] = []
    for (file, func), short_map in per_cmd.items():
        for short, longs in short_map.items():
            if len(longs) > 1:
                collisions.append(f"{file} ({func}): {short} -> {sorted(longs)}")
    assert not collisions, "same short letter on two options of one command:\n" + "\n".join(collisions)


def test_pre_existing_shorts_unchanged(decls: list[Decl]) -> None:
    """AC3: the 7 pre-existing (short, long) pairs still exist at their home.

    Lowercase shorts are per-command under the convention, so this pins each
    pair to its home file rather than demanding codebase-wide exclusivity
    (Phase 2 legally adds ``-b --body`` on ``inbox send`` while ``backlog
    pick`` keeps ``-b --blocked``). Same-command ambiguity is impossible -
    ``test_no_short_collision_within_a_command`` guards it.
    """
    for short, (long, home) in PRE_EXISTING_SHORTS.items():
        hits = [
            d for d in decls
            if short in d.shorts and d.long == long
            and (not home or d.file.endswith(home))
        ]
        assert hits, f"pre-existing short {short} ({long}) vanished from {home or 'the codebase'}"


def test_exclusive_pre_existing_uppercase(decls: list[Decl]) -> None:
    """AC3: legacy uppercase shorts outside the global register stay exclusive."""
    for short, long in EXCLUSIVE_PRE_EXISTING.items():
        observed = {d.long for d in decls if short in d.shorts and d.long}
        assert observed == {long}, (
            f"legacy uppercase short {short} must map only to {long}, found {sorted(observed)}"
        )


# --------------------------------------------------------------------------- #
# AC1 (Python path) - the phone-critical command.
# --------------------------------------------------------------------------- #

def test_agents_ask_has_phone_shorts(decls: list[Decl]) -> None:
    """AC1: `agents ask` declares -p/-c/-t for provider/cwd/timeout (Python side)."""
    ask = {d.long: d.shorts for d in decls if d.file.endswith("agents/cli.py") and d.func == "cmd_ask"}
    assert ask, "could not find cmd_ask options in agents/cli.py"
    expected = {"--provider": "-p", "--cwd": "-c", "--timeout": "-t"}
    for long, short in expected.items():
        assert long in ask, f"agents ask is missing {long}"
        assert short in ask[long], f"agents ask {long} must carry {short}, has {ask[long]}"


# --------------------------------------------------------------------------- #
# Phase 2 (US2) - the per-command lowercase map.
# --------------------------------------------------------------------------- #

def test_phase2_commands_carry_their_lowercase_shorts(decls: list[Decl]) -> None:
    """US2: every Phase 2 command declares the design-table lowercase shorts."""
    missing: list[str] = []
    for (file_suffix, func), expected in PHASE2_LOWERCASE_MAP.items():
        cmd = {
            d.long: d.shorts
            for d in decls
            if d.file.endswith(file_suffix) and d.func == func
        }
        if not cmd:
            missing.append(f"no typer.Option decls found for {func} in {file_suffix}")
            continue
        for long, short in expected.items():
            if long not in cmd:
                missing.append(f"{file_suffix} ({func}): missing {long}")
            elif short not in cmd[long]:
                missing.append(
                    f"{file_suffix} ({func}): {long} must carry {short}, has {cmd[long]}"
                )
    assert not missing, "Phase 2 lowercase map not satisfied:\n" + "\n".join(missing)


# --------------------------------------------------------------------------- #
# Phase 3 (US3) - two-spelling canonicalization (AC5 declaration shape).
# --------------------------------------------------------------------------- #

def test_two_spelling_sites_declare_canonical_and_hidden_legacy(decls: list[Decl]) -> None:
    """US3: each drift site declares the canonical long + a hidden legacy alias."""
    problems: list[str] = []
    for (file_suffix, func), pairs in TWO_SPELLING_SITES.items():
        cmd_decls = [d for d in decls if d.file.endswith(file_suffix) and d.func == func]
        if not cmd_decls:
            problems.append(f"no typer.Option decls found for {func} in {file_suffix}")
            continue
        for canonical, legacy in pairs:
            canon = [d for d in cmd_decls if d.long == canonical]
            if not canon:
                problems.append(f"{file_suffix} ({func}): missing canonical {canonical}")
            elif any(d.hidden for d in canon):
                problems.append(
                    f"{file_suffix} ({func}): canonical {canonical} must be visible, not hidden"
                )
            leg = [d for d in cmd_decls if d.long == legacy]
            if not leg:
                problems.append(
                    f"{file_suffix} ({func}): missing hidden legacy alias {legacy}"
                )
            elif not all(d.hidden for d in leg):
                problems.append(
                    f"{file_suffix} ({func}): legacy {legacy} must be hidden=True"
                )
    assert not problems, "Phase 3 two-spelling map not satisfied:\n" + "\n".join(problems)


def test_no_visible_deprecated_spelling_anywhere(decls: list[Decl]) -> None:
    """US3 negative invariant: --session/--pr never appear as a VISIBLE primary
    long anywhere in the CLI, except the documented exemptions. New commands
    must declare --session-id/--pr-number (the canonical spellings)."""
    offenders: list[str] = []
    for d in decls:
        if d.long not in {"--session", "--pr"} or d.hidden:
            continue
        exempt = any(
            d.file.endswith(suffix) and d.func == func and d.long == long
            for (suffix, func, long) in TWO_SPELLING_EXEMPTIONS
        )
        if not exempt:
            offenders.append(f"{d.long} at {d.where}")
    assert not offenders, (
        "deprecated spellings declared as visible primary longs "
        "(use --session-id/--pr-number + hidden alias):\n" + "\n".join(offenders)
    )


def test_done_lists_canonical_spelling_first(decls: list[Decl]) -> None:
    """US3: done's multi-name option lists --pr-number before --pr so help and
    the AST scan both see the canonical spelling as primary."""
    hits = [
        d for d in decls
        if d.file.endswith("done/cli.py") and "--pr-number" in d.longs and "--pr" in d.longs
    ]
    assert hits, "done/cli.py no longer declares the --pr-number/--pr multi-name option"
    for d in hits:
        assert d.longs.index("--pr-number") < d.longs.index("--pr"), (
            f"--pr-number must precede --pr at {d.where}, has {d.longs}"
        )
