"""Verb-surface ratchet: hold the REAL verb count down against a checked-in
baseline.

``fno doctor lint menu-caps`` caps what ``fno --help`` ADVERTISES. It does not cap what
EXISTS, so the real surface grew to hundreds of leaves while the menu stayed at
9. This module is the missing counterweight: a checked-in baseline of every
invocable leaf verb (``scripts/ci/verb-baseline.txt``) and a diff that fails when
the live surface and the baseline disagree.

Shape copied from ``scripts/ci/company-boundary-baseline.txt`` (a human-readable,
diffable, checked-in set with a stated removal rule) rather than
the former LOC ratchet (a numeric delta): a verb list is small, enumerable,
and reviewable line by line.

Both binaries. ``fno`` is a Rust front (``crates/fno``) that owns ``mux`` and
``version`` and forwards everything else to this Python CLI. A ratchet built on
Python introspection alone reproduces the exact defect it exists to fix: the
``fno help --all`` listing silently omits the whole Rust surface. The generator
reaches the Rust front and FAILS CLOSED (named error, no baseline written) when
it cannot, so the ratchet can never pass by reporting only half the surface.
That fail-closed reach is the guard-on-one-of-N-paths trap from the pitfalls
corpus: build the ratchet in a way that would have caught the thing that made the
ratchet necessary.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Optional, cast

from fno.paths import resolve_repo_root


class VerbRatchetError(Exception):
    """Raised when the surface cannot be enumerated truthfully (fail-closed)."""


BASELINE_REL = Path("scripts") / "ci" / "verb-baseline.txt"
COLLAPSE_MAP_REL = Path("scripts") / "ci" / "verb-collapse-map.tsv"
COLLAPSE_FLAGS_REL = Path("scripts") / "ci" / "verb-collapse-flags.txt"

# Two people hit these gates cold within an hour and both reverse-engineered the
# row format by reading the file. The refusal said what to do and not how, so it
# now carries the shape. Runtime text cannot drift from the behaviour that
# raises it, which a doc can.
COLLAPSE_MAP_ROW_HELP = (
    "Append one tab-separated row per new action, e.g.\n"
    "  agents orphans\tT1\tagents orphans\t10\t\n"
    "  current-leaf          the action exactly as a user types it after `fno`\n"
    "  tier                  T1 collapses to an argument of its group, KEEP stays "
    "a distinct subcommand (T2 and T3 are legal and currently unused)\n"
    "  post-collapse-typing  what a user types after the collapse, equal to "
    "current-leaf on every T1 row\n"
    "  refs                  how many times the corpus outside cli/src names this "
    "leaf, swept by `python3 scripts/diagnostics/verb-callers.py`\n"
    "  reason-if-not-T1      required on every non-T1 row, left empty on T1\n"
    "Then bump the row count pinned in cli/tests/unit/test_verb_collapse_map.py, "
    "and regenerate scripts/ci/verb-baseline.txt last."
)
COLLAPSE_FLAGS_ROW_HELP = (
    "One line per hidden option on an argument-dispatched action, e.g.\n"
    "  agents discovered-json !--provider\n"
    "  <action> !<option>    the action as a user types it after `fno`, then `!`, "
    "then the long option\n"
    "code-only means the option exists and the file omits it: add that line. "
    "file-only means the file lists an option the code dropped: delete that line."
)

# The Rust front's leaf tree is READ FROM ITS DISPATCHERS, not listed here.
#
# It used to be a hand-typed tuple checked against a hand-typed usage string,
# with an exemption set for whatever the usage string omitted. Both sides were
# written by the same person in the same sitting and omitted the same verbs, so
# the gate could not fail for the reason it existed: five live verbs (``mux
# layout apply``, ``mux layout graft``, ``mux pane split``, ``mux pane break``,
# ``mux block annotate``) sat unbaselined behind a green check.
#
# Two independent readings now have to agree, and neither is a Python constant:
#
#   SOURCE   the match arms and equality guards in the dispatchers themselves
#            (:func:`scan_rust_source`). Adding an arm changes this set.
#   BINARY   the alternation the live front prints when it refuses a bogus verb
#            (:func:`probe_rust_families`), e.g. ``(ls|create|rename|join)``.
#
# Disagreement in EITHER direction is a hard failure. A new arm whose usage
# string was not updated fails as "dispatches a verb it does not advertise"; an
# arm the scan cannot see fails as "advertises a verb the scan missed". The only
# way to green is to change the code and the baseline together, which is the
# property the ratchet was supposed to have all along.
def _repo_root() -> Path:
    # Reuse the canonical cached, FNO_REPO_ROOT-aware resolver rather than a
    # third divergent git rev-parse (lint_cli.py already has one duplicate;
    # fno.paths.resolve_repo_root is the single source).
    return resolve_repo_root()


def baseline_path(repo_root: Optional[Path] = None) -> Path:
    return (repo_root or _repo_root()) / BASELINE_REL


def _hidden_option_tokens(cmd) -> list[str]:
    """Hidden Option names on a leaf command, as ``!--flag`` baseline tokens.

    Visible options appear in ``--help`` (already reviewed at the menu); only
    ``.hidden`` options are the ungated axis this ratchet guards. Each is emitted
    as its first opt string (the long form) so one option is one token and the
    baseline line stays readable. ``_flag_aliases`` makes every deprecated alias
    its own hidden Option, so one rule covers hidden flags and aliases both.
    """
    tokens: list[str] = []
    for param in getattr(cmd, "params", []):
        if getattr(param, "hidden", False):
            opts = [o for o in getattr(param, "opts", []) if o.startswith("-")]
            if opts:
                tokens.append("!" + opts[0])
    return sorted(tokens)


def _format_leaf(path: str, cmd) -> str:
    """A baseline leaf line: the verb path plus its hidden options, if any."""
    toks = _hidden_option_tokens(cmd)
    return f"{path} {' '.join(toks)}" if toks else path


def _iter_group_leaves(group, ctx, prefix: str, depth: int = 0):
    """Yield ``(path, cmd)`` for each leaf under a Click group.

    Mirrors :func:`_group_leaves` but yields the command object alongside its
    path, so a report tool reads help text and callback source off the same walk
    instead of re-traversing the registry. The collapse rule matches: a group
    with no resolvable children yields its own ``(path, group)`` pair. The depth
    cap yields ``(prefix, None)`` (``_format_leaf`` renders ``None`` as a bare
    path), but the cap never fires at fno's group depth.
    """
    import click

    if depth > 8:  # ponytail: safety cap; fno groups never nest this deep
        yield prefix, None
        return
    if getattr(group, "_fno_collapsed_dispatcher", False):
        yield prefix, group
    # A NESTED group that runs without a subcommand is itself an action, and its
    # callback can carry hidden options no leaf repeats. Yield it so those flags
    # stay inside the ratchet instead of dropping out the moment the group gains
    # its first subcommand. Scoped to nested groups because the collapse map
    # models a top-level group as its actions, never as a row of its own, so a
    # bare top-level row has no legal shape there.
    elif (
        " " in prefix
        and getattr(group, "invoke_without_command", False)
        and _hidden_option_tokens(group)
    ):
        yield prefix, group
    for name in group.list_commands(ctx):
        sub = group.get_command(ctx, name)
        if sub is None:
            continue
        path = f"{prefix} {name}"
        if hasattr(sub, "list_commands"):
            child_ctx = click.Context(sub, info_name=name, parent=ctx)
            yielded = False
            for leaf_path, leaf_cmd in _iter_group_leaves(sub, child_ctx, path, depth + 1):
                yield leaf_path, leaf_cmd
                yielded = True
            if not yielded:
                yield path, sub
        else:
            yield path, sub


def _group_leaves(group, ctx, prefix: str, depth: int = 0) -> list[str]:
    """Recurse a Click group to its leaf command paths (hidden included).

    A group with no resolvable children collapses to its own path, so an empty
    or all-stub group still counts as one invocable verb. Hidden options on a
    leaf ride along as ``!--flag`` tokens on that leaf's line.
    """
    return [_format_leaf(path, cmd) for path, cmd in _iter_group_leaves(group, ctx, prefix, depth)]


def _assert_python_source_matches_repo() -> None:
    """Refuse when the imported ``fno`` package is not the repo's own source.

    The Python half enumerates by IMPORTING ``fno.cli`` in this interpreter,
    while the baseline is written to ``resolve_repo_root()``. Invoked as a bare
    ``fno``, those are two different trees: the surface comes from the INSTALLED
    package and the file comes from the checkout. ``--update`` then reports
    "regenerated ... (N leaves)" over a byte-identical file - a success line for
    work it did not do - and ``check()`` compares one tree's surface against
    another tree's baseline.

    This is the Python-side twin of the reachability check
    :func:`enumerate_rust_leaves` already performs. That half refuses to emit a
    half-true baseline when it cannot reach the Rust front; this half had no
    equivalent, so a stale deployed ``fno`` produced a confident wrong answer.

    Guarding the ENUMERATOR rather than ``--update`` is deliberate: ``check()``
    reaches the same code, so a guard on the writer alone would be a guard on
    one of two paths - the exact shape this module's docstring says it exists to
    avoid.

    An editable install resolves into the checkout and passes. CI runs via
    ``uv run --project cli fno-py``, so it passes too.
    """
    import fno

    pkg_dir = Path(fno.__file__).resolve().parent
    expected = (_repo_root() / "cli" / "src" / "fno").resolve()
    if pkg_dir != expected:
        raise VerbRatchetError(
            "verb-ratchet: the imported fno package is not this checkout's source, "
            "so the surface enumerated here does not describe the baseline being "
            f"read or written.\n  imported: {pkg_dir}\n  expected: {expected}\n"
            "  A bare `fno` runs the INSTALLED package; the baseline belongs to the "
            "checkout. Re-run as:  uv run --project cli fno-py doctor lint verb-ratchet "
            "[--update]\n  (Refusing rather than guessing: this silently emitted a "
            "byte-identical baseline plus a success line, which is how a new verb "
            "reached CI unbaselined.)"
        )


def iter_python_leaves():
    """Yield ``(path, cmd)`` for every fno-py leaf verb, visible and hidden.

    The ``(path, cmd)`` pairs are what :func:`enumerate_python_leaves` formats
    into baseline strings; a report tool reads ``cmd.help`` and the callback's
    source location off the same walk instead of re-traversing the registry.

    Import and dedup rules mirror :func:`enumerate_python_leaves` exactly: an
    unimportable module is a hard failure, Typer alias pairs dedup by import
    target, and plain-function entries are wrapped so their hidden options are
    not missed.
    """
    import importlib

    import click
    import typer
    import typer.main

    _assert_python_source_matches_repo()

    from fno._lazy_group import collapse_click_group
    from fno.cli import LAZY_SUBCOMMANDS, _EAGER_COMMAND_HELP, app as _root_app

    collapsed_groups = {
        name
        for name, entry in LAZY_SUBCOMMANDS.items()
        if len(entry) == 3 and "collapse_keep" in entry[2]
    }
    mapped_actions = {
        action
        for line in (_repo_root() / COLLAPSE_MAP_REL).read_text().splitlines()[1:]
        if line.strip()
        for action in [line.split("\t", 1)[0]]
        if action.split()[0] in collapsed_groups
    }
    live_actions: set[str] = set()
    live_action_flags: set[str] = set()

    # Eager inline commands (help, cost, review) are seeded on the main app, not
    # LAZY_SUBCOMMANDS, so resolve each from the root Click tree rather than emit
    # a bare name: a bare name would miss hidden options (review carries --sigma-*).
    _root_cmd = cast(click.Group, typer.main.get_command(_root_app))
    _root_ctx = click.Context(_root_cmd)
    for n in _EAGER_COMMAND_HELP:
        yield n, _root_cmd.get_command(_root_ctx, n)

    seen_groups: set[str] = set()
    for name, entry in LAZY_SUBCOMMANDS.items():
        import_path = entry[0]
        options = entry[2] if len(entry) == 3 else {}
        module_path, _, attr = import_path.rpartition(":")
        try:
            module = importlib.import_module(module_path)
        except Exception as exc:  # noqa: BLE001 - report the unimportable entry
            raise VerbRatchetError(
                f"verb-ratchet: Python verb {name!r} import failed "
                f"({import_path}): {exc}. An unimportable verb is a hard "
                f"failure, not a skip: a verb must not leave the baseline by "
                f"breaking. Fix the import or remove the LAZY_SUBCOMMANDS entry."
            ) from exc
        obj = getattr(module, attr, None)
        if obj is None:
            raise VerbRatchetError(
                f"verb-ratchet: Python verb {name!r} resolved to None at "
                f"{import_path}; the named attribute is missing."
            )
        if isinstance(obj, typer.Typer):
            # Two names bound to the same Typer app are a true alias pair; the
            # subtree is emitted once under the first name.
            if import_path in seen_groups:
                continue
            seen_groups.add(import_path)
            cmd = typer.main.get_command(obj)
            collapse_keep = options.get("collapse_keep")
            if collapse_keep is not None:
                uncollapsed_ctx = click.Context(cmd, info_name=name)
                for path, sub in _iter_group_leaves(cmd, uncollapsed_ctx, name):
                    live_actions.add(path)
                    live_action_flags.update(
                        f"{path} {flag}" for flag in _hidden_option_tokens(sub)
                    )
                cmd = collapse_click_group(cmd, keep=set(collapse_keep))
            if hasattr(cmd, "list_commands"):
                ctx = click.Context(cmd, info_name=name)
                yielded = False
                for path, sub in _iter_group_leaves(cmd, ctx, name):
                    yield path, sub
                    yielded = True
                if not yielded:
                    yield name, cmd
            else:
                yield name, cmd
        elif isinstance(obj, click.Command):
            yield name, obj
        else:
            # Plain function with Typer-style params: the live CLI wraps it as a
            # single-command Typer app (cli/_lazy_group.py). A bare function has
            # no .params, so mirror that wrap or its hidden options (doctor
            # --context-*) are silently missed.
            sub = typer.Typer(add_completion=False)
            sub.command(name=name)(obj)
            yield name, typer.main.get_command(sub)

    if live_actions != mapped_actions:
        raise VerbRatchetError(
            "verb-ratchet: collapsed action inventory drifted from "
            "scripts/ci/verb-collapse-map.tsv; "
            f"code-only={sorted(live_actions - mapped_actions)}, "
            f"map-only={sorted(mapped_actions - live_actions)}. Allocate every new "
            "action in the map before regenerating the registered-leaf baseline.\n"
            + COLLAPSE_MAP_ROW_HELP
        )
    mapped_action_flags = {
        line.strip()
        for line in (_repo_root() / COLLAPSE_FLAGS_REL).read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    if live_action_flags != mapped_action_flags:
        raise VerbRatchetError(
            "verb-ratchet: collapsed action hidden-option inventory drifted from "
            "scripts/ci/verb-collapse-flags.txt; "
            f"code-only={sorted(live_action_flags - mapped_action_flags)}, "
            f"file-only={sorted(mapped_action_flags - live_action_flags)}.\n"
            + COLLAPSE_FLAGS_ROW_HELP
        )


def enumerate_python_leaves() -> list[str]:
    """Every leaf verb the fno-py registry exposes, visible and hidden.

    Mirrors the introspection ``fno doctor lint menu-caps`` uses but recurses to leaves
    instead of stopping at group names, and includes hidden commands. An entry
    whose module will not import is a HARD failure here, not the skip menu-caps
    does: a verb must not leave the baseline by breaking.

    Dedup is by import target ONLY for Typer groups, where two names sharing one
    app would otherwise double-emit the whole subtree (the former ``graph`` ->
    ``backlog`` alias shape). A single command that shares its import target
    with another name is two distinct invocable verbs and both appear.

    Plain-function entries (``doctor``) and eager inline commands (``review``)
    are resolved to real Click commands, not bare names, so their hidden options
    ride into the baseline like any group leaf.
    """
    return sorted({_format_leaf(path, cmd) for path, cmd in iter_python_leaves()})


def _strip_line_comments(text: str) -> list[str]:
    """Source lines with ``//`` comments removed.

    Doc comments narrate verbs in prose (```prune` exists today``) and the
    catch-all arm is often explained a few lines above itself. Scanning them
    proposes verbs that no arm dispatches.
    """
    return [line.split("//")[0] for line in text.splitlines()]


def _braced_region(text: str, marker: str) -> str:
    """Return the balanced braced region beginning at ``marker``."""
    start = text.find(marker)
    opening = text.find("{", start)
    if start < 0 or opening < 0:
        raise VerbRatchetError(
            f"verb-ratchet: could not find fno-agents dispatch marker {marker!r}"
        )
    depth = 0
    for index in range(opening, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[opening : index + 1]
    raise VerbRatchetError(f"verb-ratchet: unbalanced fno-agents dispatch marker {marker!r}")


def scan_fno_agents_source(repo_root: Optional[Path] = None) -> set[str]:
    """Action tokens read from the fno-agents dispatch control flow."""
    root = repo_root or _repo_root()
    text = "\n".join(_strip_line_comments((root / FNO_AGENTS_SOURCE).read_text(encoding="utf-8")))
    run = _braced_region(text, "async fn run(args:")
    actions = set(re.findall(r'\bif verb == "([a-z][a-z0-9-]*)"', run))
    for alternatives in re.findall(r"matches!\(\s*verb\s*,([^)]*)\)", run):
        actions.update(re.findall(r'"([a-z][a-z0-9-]*)"', alternatives))

    retired = _braced_region(text, "fn retired_verb_pointer(verb:")
    actions.update(_ARM_RE.findall(retired))
    request = _braced_region(text, "let method = match verb")
    request_lines = _strip_line_comments(request)
    actions.update(_arms_at_top_level(request_lines, 0, len(request_lines)))

    actions.update({"help", "version", "--emit-schema"})
    if not {"claim", "detect"} <= actions:
        raise VerbRatchetError(
            "verb-ratchet: fno-agents source scan missed hidden `claim` or `detect`; "
            "a silent exemption is the defect this scan exists to prevent"
        )
    return actions


def probe_fno_agents_actions(binary: Path) -> set[str]:
    """Action tokens from the live binary's named unknown-verb refusal."""
    result = _run_front(binary, ["__fno_verb_probe__"])
    output = (result.stdout or "") + (result.stderr or "")
    marker = "__fno_verb_probe__"
    # Both refusals name the probed BINARY. This gate reads a compiled artifact
    # while every other check in the lint reads source, so a binary built before
    # the current checkout fails here and sends the reader to the source scan
    # that is fine. Rebuild is the first thing to try, and the path says which.
    if marker not in output:
        raise VerbRatchetError(
            f"verb-ratchet: {binary} did not refuse the positive probe by name; "
            "if it predates this checkout, rebuild it first (cargo build)"
        )
    match = re.search(r"\(expected ([^)]+)\)", output[output.index(marker) :])
    if match is None:
        raise VerbRatchetError(
            f"verb-ratchet: {binary} refused without reporting its expected actions; "
            "if it predates this checkout, rebuild it first (cargo build)"
        )
    return {value.strip() for value in match.group(1).split("|") if value.strip()}


def _locate_fno_agents_front() -> Optional[Path]:
    override = os.environ.get("FNO_AGENTS_FRONT", "").strip()
    if override:
        return Path(override)
    worktree = _repo_root() / "crates" / "fno-agents" / "target" / "debug" / "fno-agents"
    if worktree.is_file() and os.access(worktree, os.X_OK):
        return worktree
    found = shutil.which("fno-agents")
    return Path(found) if found else None


def _enclosing_block_start(lines: list[str], idx: int) -> int:
    """Index of the line that opens the innermost block containing ``idx``."""
    depth = 0
    for i in range(idx, -1, -1):
        for ch in reversed(lines[i]):
            if ch == "}":
                depth += 1
            elif ch == "{":
                if depth == 0:
                    return i
                depth -= 1
    return 0


def _arms_at_top_level(lines: list[str], lo: int, hi: int) -> set[str]:
    """Verb literals dispatched directly by the match opening at ``lo``.

    Depth-scoped on purpose. A flat scan of the enclosing function also picked
    up ``"split"`` and ``"workspace"`` from the ``pane run`` flag parser nested
    inside it, which are flag spellings rather than verbs.
    """
    verbs: set[str] = set()
    depth = 0
    for i in range(lo, hi):
        line = lines[i]
        if depth == 0:
            verbs.update(_ARM_RE.findall(line))
        depth += line.count("{") - line.count("}")
        if i == lo:
            depth = max(depth - 1, 0)  # discount the match's own opening brace
    return verbs


NATIVE_TREE_REL = "scripts/ci/native-command-tree.txt"
INVENTORY_REGENERATE = (
    "cargo run --quiet --manifest-path crates/fno/Cargo.toml "
    "--example native_command_tree > scripts/ci/native-command-tree.txt"
)


FNO_AGENTS_SOURCE = Path("crates") / "fno-agents" / "src" / "bin" / "client.rs"
_ARM_RE = re.compile(r'(?:Some\(\s*)?"([a-z][a-z0-9-]*)"\s*(?:\)\s*)?(?:\||=>)')


def read_native_inventory(repo_root: Optional[Path] = None) -> list[list[str]]:
    """The generated native command tree's data rows (path, kind, ...).

    ``scripts/ci/native-command-tree.txt`` is generated from the one clap
    declaration in ``crates/fno`` (the file's header names the regenerate
    command), and a freshness test in ``cli_args.rs`` fails ``cargo test``
    when the artifact lags the tree - so the ratchet reads one generated
    owner instead of re-deriving the tree from Rust source text.
    """
    path = (repo_root or _repo_root()) / NATIVE_TREE_REL
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise VerbRatchetError(
            f"verb-ratchet: cannot read the native command inventory {path}: {exc}. "
            f"Regenerate it from the typed tree: {INVENTORY_REGENERATE}"
        ) from exc
    rows = [
        line.split("\t")
        for line in raw.splitlines()
        if line and not line.startswith("#")
    ]
    if not rows:
        raise VerbRatchetError(
            f"verb-ratchet: the native command inventory {path} is empty. "
            f"Regenerate it from the typed tree: {INVENTORY_REGENERATE}"
        )
    return rows


def _run_front(binary: Path, args: list[str]):
    """Run the Rust front, converting reachability failures to VerbRatchetError.

    A timeout (hung front), a missing executable (vanished between ``which`` and
    exec), or any OS-level spawn failure is a fail-closed reachability break, not
    a traceback: AC4 wants a named error, and the caller must not write a baseline.
    """
    try:
        return subprocess.run(
            [str(binary), *args],
            capture_output=True,
            text=True,
            timeout=20,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
        raise VerbRatchetError(
            f"verb-ratchet: Rust front unreachable - `fno {' '.join(args)}` "
            f"failed: {exc.__class__.__name__}: {exc}"
        ) from exc


def enumerate_rust_leaves(repo_root: Optional[Path] = None) -> list[str]:
    """The Rust front's leaves, read from the generated native inventory.

    Every declared root other than ``mux`` is a leaf of its own (``version``);
    ``mux`` registers as one leaf, its families being argument dispatch. The
    fno-agents half keeps the independent source/binary cross-check.
    """
    rows = read_native_inventory(repo_root)
    roots = {
        row[0].split()[0] for row in rows if row and row[0] and row[0] != "(root)"
    }
    if "mux" not in roots or "version" not in roots:
        raise VerbRatchetError(
            "verb-ratchet: the native command inventory is missing the mux/version "
            f"roots (found {sorted(roots)}); the artifact may predate the typed "
            f"tree. Regenerate: {INVENTORY_REGENERATE}"
        )
    leaves = {"mux", "fno-agents"}
    leaves |= roots - {"mux"}
    return sorted(leaves)


def enumerate_all_leaves() -> list[str]:
    """Every invocable leaf verb across both binaries, sorted and deduped.

    Raises :class:`VerbRatchetError` if either half cannot be truthfully
    enumerated, so the caller writes no baseline on failure.
    """
    combined = enumerate_python_leaves() + enumerate_rust_leaves()
    return sorted(set(combined))


_HEADER = """\
# Known verb surface held by the CI ratchet.
# Every invocable leaf verb, one per line, sorted: the fno-py registry (visible
# AND hidden, recursed to leaves) plus the Rust front's mux + version surface.
# Remove or add a line only in the same PR that removes or adds the verb.
#
# ALLOCATE THE ACTION FIRST. A new action must have a row in
# scripts/ci/verb-collapse-map.tsv before this file can regenerate, or the lint
# refuses with `collapsed action inventory drifted ... code-only=[...]`. Copy a
# neighbouring row: tier T1 keeps the pre-collapse typing, and the refs column
# comes from the sweep in scripts/diagnostics/verb-callers.py, not from a guess.
# The map's contract test pins the inventory count, so that number moves in the
# same PR. Two separate branches hit this within one hour on 2026-08-13, which
# is why it is written here rather than learned twice.
#
# To regenerate after an intentional change:
#   uv run --project cli fno-py doctor lint verb-ratchet --update
# then commit this file in that PR. Run it that way, not as a bare `fno`: a bare
# `fno` enumerates the INSTALLED package while writing this checkout's file, and
# a verb that exists only in source is missed. The lint refuses that combination
# rather than emitting a byte-identical file plus a success line.
# A deliberate addition carries a PR-body line:
#   verb-exception: <rationale>
# (an explicit rationale keeps the exception reviewable).
# Two PRs that each add a verb both edit this file; the merge conflict is the
# intended review moment, not tooling noise.
#
# A leaf carrying hidden options lists them inline as `!--flag` tokens
# (e.g. `mail send !--provider`); leaves with none stay single-token. Hidden options
# and deprecated aliases are the ungated axis this ratchet guards, so a new
# hidden option on an existing verb carries its own PR-body line:
#   flag-exception: <rationale>
# (same shape as `verb-exception:`; removals are free and need no line).
#
# Scope: the hidden-option coverage is fno-py only. The Rust front's VERBS are
# read from its dispatchers and cross-checked against the live binary, but its
# FLAGS are not enumerated, so a hidden flag on a mux verb is invisible to this
# gate. The lint output names that boundary on every green run.
"""


def generate(leaves: list[str]) -> str:
    return _HEADER + "\n".join(leaves) + "\n"


def parse_baseline(text: str) -> list[str]:
    """Non-comment, non-blank lines of a baseline file."""
    return [
        line.strip()
        for line in text.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


def _split_leaf(line: str) -> tuple[str, frozenset[str]]:
    """Split a baseline line into ``(verb path, hidden-option tokens)``.

    Verb tokens never start with ``!``; hidden options are emitted as ``!--flag``
    by :func:`_format_leaf`, so the split is unambiguous. A bare leaf (no hidden
    options) returns an empty flag set.
    """
    parts = line.split()
    path = " ".join(p for p in parts if not p.startswith("!"))
    flags = frozenset(p for p in parts if p.startswith("!"))
    return path, flags


@dataclass
class CheckReport:
    ok: bool
    message: str


def check() -> CheckReport:
    """Diff the live surface against the checked-in baseline.

    Separates two drift kinds so each carries its own escape hatch: an added
    VERB needs a ``verb-exception:`` line, an added HIDDEN OPTION on an existing
    verb needs a ``flag-exception:`` line. Removals (verbs or options) are free;
    they pass once the baseline is regenerated and need no exception line.

    The hidden-option coverage is fno-py only. The Rust front's verbs ARE read
    from source now, but its flags are not enumerated, so a clap
    ``hide = true`` option on a mux verb is invisible here. The ok line states
    that boundary so it is visible in CI output, not only in the source.
    """
    live_lines = enumerate_all_leaves()  # raises VerbRatchetError on failure
    live: dict[str, tuple[frozenset[str], str]] = {}
    for line in live_lines:
        lpath, lflags = _split_leaf(line)
        live[lpath] = (lflags, line)

    path = baseline_path()
    baseline_text = path.read_text(encoding="utf-8") if path.exists() else ""
    baseline: dict[str, tuple[frozenset[str], str]] = {}
    for line in parse_baseline(baseline_text):
        bpath, bflags = _split_leaf(line)
        baseline[bpath] = (bflags, line)

    added_verbs = [live[p][1] for p in sorted(set(live) - set(baseline))]
    removed_verbs = [baseline[p][1] for p in sorted(set(baseline) - set(live))]
    added_flags: list[str] = []
    removed_flags: list[str] = []
    for p in sorted(set(live) & set(baseline)):
        lf = live[p][0]
        bf = baseline[p][0]
        added_flags.extend(f"{p} {f}" for f in sorted(lf - bf))
        removed_flags.extend(f"{p} {f}" for f in sorted(bf - lf))

    n_hidden = sum(len(v[0]) for v in live.values())
    if not added_verbs and not removed_verbs and not added_flags and not removed_flags:
        return CheckReport(
            ok=True,
            message=(
                f"verb-ratchet: ok ({len(live)} leaves, {n_hidden} hidden "
                f"option{'s' if n_hidden != 1 else ''} - fno-py only; the Rust front's "
                f"verbs are read from source, its flags are not)"
            ),
        )

    parts = ["verb-ratchet: FAIL - live surface drifted from scripts/ci/verb-baseline.txt"]
    if added_verbs:
        parts.append("  Added (in the code, not the baseline): " + ", ".join(added_verbs))
        parts.append("    A verb cannot be added silently. Regenerate with")
        parts.append("    `uv run --project cli fno-py doctor lint verb-ratchet --update`,")
        parts.append("    commit this file in the PR,")
        parts.append("    and add a PR-body line:  verb-exception: <rationale>")
    if added_flags:
        parts.append(
            "  Added hidden options (in the code, not the baseline): " + ", ".join(added_flags)
        )
        parts.append("    A hidden option cannot be added silently. Regenerate with")
        parts.append("    `uv run --project cli fno-py doctor lint verb-ratchet --update`,")
        parts.append("    commit this file in the PR,")
        parts.append("    and add a PR-body line:  flag-exception: <rationale>")
    if removed_verbs:
        parts.append("  Removed (in the baseline, not the code): " + ", ".join(removed_verbs))
        parts.append(
            "    Regenerate with `uv run --project cli fno-py doctor lint verb-ratchet "
            "--update` and commit the baseline."
        )
    if removed_flags:
        parts.append(
            "  Removed hidden options (in the baseline, not the code): " + ", ".join(removed_flags)
        )
        parts.append(
            "    Regenerate with `uv run --project cli fno-py doctor lint verb-ratchet "
            "--update` and commit the baseline."
        )
    parts.append("  If two PRs each add a verb or hidden option, both edit this file; the")
    parts.append("  merge conflict is the intended review moment, not tooling noise.")
    return CheckReport(ok=False, message="\n".join(parts))
