"""Hidden deprecated flag aliases (short-flags Phase 3, ab-a04f3f1a).

The two-spelling unification in the short-flag design
(``internal/fno/design/2026-06-03-fno-cli-short-flags.md``)
canonicalizes ``--session-id`` and ``--pr-number``. The old spellings
(``--session``, ``--pr``) survive as hidden deprecated aliases: each is a
SEPARATE ``typer.Option(..., hidden=True)`` whose value is folded into the
canonical parameter at the top of the command body via
``merge_deprecated_alias``. Hiding the alias keeps ``--help`` and the
``fno help shorthands`` legend unambiguous while old call sites keep
working unchanged.
"""
from __future__ import annotations

from typing import Optional, TypeVar

import click
import typer
from typer.models import OptionInfo

T = TypeVar("T")


def _coerce(value):
    """Map Typer's declaration-default sentinel to ``None``.

    When a Typer command function is called DIRECTLY (tests and internal
    callers bypass Click), unfilled keyword params hold their
    ``typer.Option(...)`` declaration default - an :class:`OptionInfo`
    object, not the ``None`` Click would deliver. Treat it as not passed
    so direct calls keep pre-alias semantics.
    """
    return None if isinstance(value, OptionInfo) else value


def _passed(value: object) -> bool:
    """True when the option was actually given on the command line.

    ``None`` is Click's not-passed sentinel for scalar options. The
    ``!= ()`` guard covers Click's ``multiple=True`` unset sentinel (an
    empty tuple); ``!= []`` is the defensive twin for callers whose
    repeatable option is ``list``-typed. Falsy-but-real values (``0``)
    count as passed.
    """
    return value is not None and value != () and value != []


def merge_deprecated_alias(
    canonical: Optional[T],
    legacy: Optional[T],
    *,
    canonical_flag: str,
    legacy_flag: str,
) -> Optional[T]:
    """Fold a hidden deprecated alias into its canonical option value.

    Returns the canonical value when only it was passed (or neither was);
    returns the legacy value with a stderr deprecation warning when only
    the alias was passed. Both-passed is ambiguous user input and refused
    as a :class:`click.UsageError` (exit 2, matching Click's own bad-flag
    exit code) - even when the two values happen to agree.
    """
    canonical = _coerce(canonical)
    legacy = _coerce(legacy)
    if not _passed(legacy):
        return canonical
    if _passed(canonical):
        raise click.UsageError(
            f"pass either {canonical_flag} or {legacy_flag} (deprecated), not both"
        )
    typer.echo(
        f"warning: {legacy_flag} is deprecated; use {canonical_flag} instead. "
        "The alias will be removed in a future release.",
        err=True,
    )
    return legacy


#: The axis-split tombstone (x-bab1). The harness axis (the CLI binary) was
#: ``--provider/-p`` on every verb except spawn; it is ``--harness/-H`` everywhere
#: now. A model vendor routes ONLY at spawn, so the retired spelling is a hidden
#: long-only option whose sole behavior is to exit 2 with this map - a tombstone,
#: not an alias (one spelling, one meaning). Removed at 0.4.0.
PROVIDER_AXIS_TOMBSTONE = (
    "--provider was split at the axis rename: the CLI binary is --harness/-H; "
    "a model vendor is only routable at spawn "
    "(`fno agents spawn --provider <vendor> --model <m>`). Removed at 0.4.0."
)


def refuse_retired_provider(value: object) -> None:
    """Exit 2 with the axis map when the retired ``--provider`` is passed.

    Pairs with a hidden ``_provider_tombstone: typer.Option(None, "--provider",
    hidden=True)`` parameter on each migrated verb. Call it at the top of the
    command body; it is a no-op when the flag was not given.
    """
    if _passed(_coerce(value)):
        typer.echo(PROVIDER_AXIS_TOMBSTONE, err=True)
        raise typer.Exit(code=2)
