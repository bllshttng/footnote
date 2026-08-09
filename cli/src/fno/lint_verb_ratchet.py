"""Verb-surface ratchet: hold the REAL verb count down against a checked-in
baseline.

``fno lint menu-caps`` caps what ``fno --help`` ADVERTISES. It does not cap what
EXISTS, so the real surface grew to hundreds of leaves while the menu stayed at
9. This module is the missing counterweight: a checked-in baseline of every
invocable leaf verb (``scripts/ci/verb-baseline.txt``) and a diff that fails when
the live surface and the baseline disagree.

Shape copied from ``scripts/ci/company-boundary-baseline.txt`` (a human-readable,
diffable, checked-in set with a stated removal rule) rather than
``scripts/ci/loc-ratchet.sh`` (a numeric delta): a verb list is small, enumerable,
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

import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from fno.paths import resolve_repo_root


class VerbRatchetError(Exception):
    """Raised when the surface cannot be enumerated truthfully (fail-closed)."""


BASELINE_REL = Path("scripts") / "ci" / "verb-baseline.txt"

# The complete known leaf tree of the Rust front (``crates/fno``), sourced from
# ``crates/fno/src/main.rs::decide_role`` and ``crates/fno/src/mux_cli.rs``.
# The advertised subset (what ``fno mux --help`` prints) is verified against the
# live binary at lint time; the hidden verbs below (``mux tab``, ``mux layout``,
# ``mux where``) are constant-only because the Rust front exposes no structured
# introspection and its own usage string omits them - the same "incomplete help"
# defect this node is about, one layer down. Adding a Rust verb is a deliberate
# two-file change: the Rust match arm AND this tuple (plus the baseline) - the
# same review moment adding a Python verb to LAZY_SUBCOMMANDS is.
RUST_FRONT_VERBS: tuple[str, ...] = (
    "mux attach",
    "mux block pipe",
    "mux doctor",
    "mux kill-server",
    "mux layout get",
    "mux ls",
    "mux pane claim",
    "mux pane kill",
    "mux pane ls",
    "mux pane read",
    "mux pane release",
    "mux pane run",
    "mux pane send",
    "mux pane wait",
    "mux serve",
    "mux server",
    "mux shell-init",
    "mux tab create",
    "mux tab join",
    "mux tab ls",
    "mux tab rename",
    "mux where",
    "mux workspace prune",
    "version",
)

# Verbs the Rust usage string omits by design; everything else in
# RUST_FRONT_VERBS must appear in `fno mux --help`, and `enumerate_rust_leaves`
# checks BOTH directions so a Rust verb added to or dropped from the help string
# without a matching edit here (or in RUST_FRONT_VERBS) fails the ratchet.
_RUST_USAGE_HIDDEN = frozenset(
    {
        "mux layout get",
        "mux tab create",
        "mux tab join",
        "mux tab ls",
        "mux tab rename",
        "mux where",
    }
)


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


def _group_leaves(group, ctx, prefix: str, depth: int = 0) -> list[str]:
    """Recurse a Click group to its leaf command paths (hidden included).

    A group with no resolvable children collapses to its own path, so an empty
    or all-stub group still counts as one invocable verb. Hidden options on a
    leaf ride along as ``!--flag`` tokens on that leaf's line.
    """
    import click

    if depth > 8:  # ponytail: safety cap; fno groups never nest this deep
        return [prefix]
    leaves: list[str] = []
    for name in group.list_commands(ctx):
        sub = group.get_command(ctx, name)
        if sub is None:
            continue
        path = f"{prefix} {name}"
        if hasattr(sub, "list_commands"):
            child_ctx = click.Context(sub, info_name=name, parent=ctx)
            children = _group_leaves(sub, child_ctx, path, depth + 1)
            leaves.extend(children if children else [_format_leaf(path, sub)])
        else:
            leaves.append(_format_leaf(path, sub))
    return leaves


def enumerate_python_leaves() -> list[str]:
    """Every leaf verb the fno-py registry exposes, visible and hidden.

    Mirrors the introspection ``fno lint menu-caps`` uses but recurses to leaves
    instead of stopping at group names, and includes hidden commands. An entry
    whose module will not import is a HARD failure here, not the skip menu-caps
    does: a verb must not leave the baseline by breaking.

    Dedup is by import target ONLY for Typer groups, where two names sharing one
    app would otherwise double-emit the whole subtree (the former ``graph`` ->
    ``backlog`` alias shape). A single command that shares its import target
    with another name (``update`` / ``upgrade``) is two distinct invocable verbs
    and both appear.
    """
    import importlib

    import click
    import typer
    import typer.main

    from fno.cli import LAZY_SUBCOMMANDS, _EAGER_COMMAND_HELP

    leaves: list[str] = list(_EAGER_COMMAND_HELP)  # help, cost, review (inline)

    seen_groups: set[str] = set()
    for name, entry in LAZY_SUBCOMMANDS.items():
        import_path = entry[0]
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
            if hasattr(cmd, "list_commands"):
                ctx = click.Context(cmd, info_name=name)
                children = _group_leaves(cmd, ctx, name)
                leaves.extend(children if children else [_format_leaf(name, cmd)])
            else:
                leaves.append(_format_leaf(name, cmd))
        else:
            leaves.append(_format_leaf(name, obj))
    return sorted(set(leaves))


def _locate_rust_front() -> Optional[Path]:
    """The Rust front binary, by explicit path first and then PATH.

    ``FNO_RUST_FRONT`` exists for environments that HAVE the front but must not
    put it on PATH: the CI smoke job builds it for this gate alone, and a bare
    ``fno`` there would shadow the Python entry point every other smoke step
    resolves. An unset variable keeps the ordinary PATH lookup; a set-but-wrong
    one still reaches the reachability probe below and fails closed.
    """
    override = os.environ.get("FNO_RUST_FRONT", "").strip()
    if override:
        return Path(override)
    found = shutil.which("fno")
    return Path(found) if found else None


def _parse_mux_usage(usage: str) -> list[str]:
    """Parse the ``fno mux --help`` usage string into advertised leaf verbs.

    The grammar is ``fno <verb...> [opts] | fno <verb...> <a|b|c> ...``; a token
    starting with ``[``/``<``/``-`` ends the verb path, and a ``|`` inside a path
    token (``mux pane ls|read|...``) fans out to one leaf per alternative. A
    parse that returns nothing it should have is itself a failure the caller
    catches via the ``mux pane`` anchor.
    """
    leaves: list[str] = []
    idx = usage.find("fno ")
    text = usage[idx:] if idx >= 0 else usage
    for seg in text.split(" | "):
        seg = seg.strip()
        if seg.startswith("fno "):
            seg = seg[4:]
        path: list[str] = []
        expanded = False
        for tok in seg.split():
            if not tok or tok[0] in "[<-." or tok.startswith("-"):
                break
            if "|" in tok:
                for alt in tok.split("|"):
                    if alt:
                        leaves.append(" ".join([*path, alt]))
                expanded = True
                break
            path.append(tok)
        if path and not expanded:
            leaves.append(" ".join(path))
    return sorted(set(leaves))


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


def enumerate_rust_leaves() -> list[str]:
    """The Rust front's leaf verbs, reaching the binary to fail closed.

    Verifies reachability (``fno version --json`` parseable) and that the
    advertised mux surface is a subset of :data:`RUST_FRONT_VERBS` (so a Rust
    addition is caught), then returns the full known Rust leaf tree.
    """
    binary = _locate_rust_front()
    if binary is None:
        raise VerbRatchetError(
            "verb-ratchet: Rust front not reachable - `fno` is not on PATH. "
            "The ratchet covers BOTH binaries and refuses to emit a Python-only "
            "baseline: that would repeat the defect it exists to fix (the help "
            "surface omitting mux). Install the Rust front, or run where it is."
        )
    version = _run_front(binary, ["version", "--json"])
    if version.returncode != 0:
        raise VerbRatchetError(
            f"verb-ratchet: Rust front unreachable - `fno version --json` "
            f"exited {version.returncode}: {version.stderr.strip()[:200]}"
        )
    try:
        parsed = json.loads(version.stdout)
        rev = parsed.get("git_rev") if isinstance(parsed, dict) else None
    except (ValueError, TypeError):
        rev = None
    if not rev:
        # `fno version` is Rust-owned; fno-py has no `version` command. A shim
        # that errors or forwards to Python fails here -> fail closed.
        raise VerbRatchetError(
            "verb-ratchet: `fno version --json` returned no git_rev; the `fno` "
            f"on PATH is not the Rust front. Output: {version.stdout.strip()[:200]}"
        )
    mux = _run_front(binary, ["mux", "--help"])
    # MuxUsage prints the usage to stderr and exits 2; that is the happy path.
    usage = (mux.stdout + mux.stderr).strip()
    if "mux pane" not in usage:
        raise VerbRatchetError(
            f"verb-ratchet: Rust front does not own mux - `fno mux --help` "
            f"lacks the `mux pane` anchor (rc={mux.returncode})."
        )
    advertised = set(_parse_mux_usage(usage))
    unknown = sorted(advertised - set(RUST_FRONT_VERBS))
    if unknown:
        raise VerbRatchetError(
            "verb-ratchet: the Rust front advertises verb(s) not listed in "
            "RUST_FRONT_VERBS in lint_verb_ratchet.py: "
            + ", ".join(unknown)
            + ". Add them to RUST_FRONT_VERBS and regenerate the baseline."
        )
    # Reverse check: every non-hidden constant verb must still be advertised.
    # Without it, a maintainer who drops a Rust match arm + its usage line but
    # leaves the verb in RUST_FRONT_VERBS gets a green ratchet over a dead entry,
    # the guard-on-one-of-N-paths shape this module exists to catch. _RUST_USAGE_HIDDEN
    # are the verbs the usage string omits by design, so they are exempt.
    dropped = sorted((set(RUST_FRONT_VERBS) - _RUST_USAGE_HIDDEN) - advertised)
    if dropped:
        raise VerbRatchetError(
            "verb-ratchet: RUST_FRONT_VERBS lists advertised verb(s) the Rust "
            "front no longer shows in `fno mux --help`: "
            + ", ".join(dropped)
            + ". Remove them from RUST_FRONT_VERBS (or add to _RUST_USAGE_HIDDEN "
            "if newly hidden) and regenerate the baseline."
        )
    return sorted(set(RUST_FRONT_VERBS))


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
# To regenerate after an intentional change: `fno lint verb-ratchet --update`,
# then commit this file in that PR. A deliberate addition carries a PR-body line:
#   verb-exception: <rationale>
# (mirrors `loc-exception:` so contributors learn one idiom, not two).
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
# Scope: the hidden-option coverage is fno-py only. The Rust front is
# constant-listed in lint_verb_ratchet.py (RUST_FRONT_VERBS), not introspected,
# so a clap `hide = true` option on it is invisible to this gate - and to
# `fno mux --help`, which is how it stays hidden. The lint output names this
# boundary on every green run.
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

    The hidden-option coverage is fno-py only - the Rust front is
    constant-listed (:data:`RUST_FRONT_VERBS`), not introspected, so a clap
    ``hide = true`` option on it is invisible here. The ok line states that
    boundary so it is visible in CI output, not only in the source.
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
                f"option{'s' if n_hidden != 1 else ''} - fno-py only; the Rust front "
                f"is constant-listed and not introspected)"
            ),
        )

    parts = ["verb-ratchet: FAIL - live surface drifted from scripts/ci/verb-baseline.txt"]
    if added_verbs:
        parts.append("  Added (in the code, not the baseline): " + ", ".join(added_verbs))
        parts.append("    A verb cannot be added silently. Regenerate with")
        parts.append("    `fno lint verb-ratchet --update`, commit this file in the PR,")
        parts.append("    and add a PR-body line:  verb-exception: <rationale>")
    if added_flags:
        parts.append("  Added hidden options (in the code, not the baseline): " + ", ".join(added_flags))
        parts.append("    A hidden option cannot be added silently. Regenerate with")
        parts.append("    `fno lint verb-ratchet --update`, commit this file in the PR,")
        parts.append("    and add a PR-body line:  flag-exception: <rationale>")
    if removed_verbs:
        parts.append("  Removed (in the baseline, not the code): " + ", ".join(removed_verbs))
        parts.append(
            "    Regenerate with `fno lint verb-ratchet --update` and commit the baseline."
        )
    if removed_flags:
        parts.append("  Removed hidden options (in the baseline, not the code): " + ", ".join(removed_flags))
        parts.append(
            "    Regenerate with `fno lint verb-ratchet --update` and commit the baseline."
        )
    parts.append("  If two PRs each add a verb or hidden option, both edit this file; the")
    parts.append("  merge conflict is the intended review moment, not tooling noise.")
    return CheckReport(ok=False, message="\n".join(parts))
