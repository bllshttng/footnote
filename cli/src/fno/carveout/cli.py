"""`fno carveout` - capture left-out work to a session ledger.

Machine-first surface (Locked Decision #3: a CLI verb, not a transcript tag):
the new carve-out id prints to stdout; warnings and errors go to stderr;
exit codes are predictable (0 ok / 2 invalid args / 1 write failure).
"""
from __future__ import annotations

import json
import re
from typing import List

import typer

from fno.carveout.core import (
    DESCRIPTION_CAP,
    VALID_KINDS,
    CarveoutError,
    add_carveout,
)

carveout_app = typer.Typer(
    no_args_is_help=True,
    help=(
        "Capture left-out work (deferred decisions, out-of-scope bugs, data "
        "backfills the merged PR enables) to .fno/carveouts.jsonl. "
        "Advisory: call it the moment you leave work undone; the retro-triage "
        "harvest at merge turns deferred/oos-bug into backlog nodes (deduped, "
        "classified), while `backfill` is handled by /fno:pr merged."
    ),
)

_PRIORITY_RE = re.compile(r"^p[0-3]$")


@carveout_app.command("add")
def add(
    description: str = typer.Argument(
        ...,
        help="What was left undone, and why. Truncated past "
        f"{DESCRIPTION_CAP} chars (never rejected).",
    ),
    kind: str = typer.Option(
        ...,
        "--kind",
        "-k",
        help="deferred (blocked on an open question) | oos-bug (out-of-scope bug) "
        "| backfill (a data backfill the merged PR enables).",
    ),
    need: str = typer.Option(
        None,
        "--need",
        help="The dependency the work is blocked on: an open question (deferred) "
        "or a precondition (backfill).",
    ),
    priority: str = typer.Option(
        None,
        "--priority",
        "-p",
        help="Priority hint pN (p0-p3) the harvested node should inherit; default p3 at triage.",
    ),
) -> None:
    """Record one deferred decision, out-of-scope bug, or data backfill for later triage."""
    if kind not in VALID_KINDS:
        typer.echo(
            f"carveout: invalid --kind '{kind}' "
            f"(expected one of: {', '.join(VALID_KINDS)})",
            err=True,
        )
        raise typer.Exit(2)

    if priority is not None and not _PRIORITY_RE.match(priority):
        typer.echo(
            f"carveout: invalid --priority '{priority}' (expected p0, p1, p2 or p3)",
            err=True,
        )
        raise typer.Exit(2)

    # Deferred import keeps cli.py module load cheap (resolve_repo_root touches git).
    from fno.carveout.core import resolve_carveout_root
    from fno.paths import resolve_repo_root

    # Session id comes from the LIVE worktree's target-state.md; the ledger is
    # written under the CANONICAL root so a carve-out captured inside a linked
    # worktree survives that worktree's archival (ab-44408b6e).
    session_root = resolve_repo_root()
    storage_root = resolve_carveout_root()
    try:
        cv, unscoped = add_carveout(
            session_root,
            kind=kind,
            description=description,
            need=need,
            priority=priority,
            storage_root=storage_root,
        )
    except CarveoutError as exc:
        typer.echo(f"carveout: failed to record carve-out: {exc}", err=True)
        raise typer.Exit(1)

    if unscoped:
        typer.echo(
            "carveout: no active session; carve-out recorded unscoped", err=True
        )

    # stdout carries the value: the new carve-out id.
    typer.echo(cv.id)


@carveout_app.command("list")
def list_carveouts(
    kind: str = typer.Option(
        None,
        "--kind",
        "-k",
        help=f"Filter to one kind ({' | '.join(VALID_KINDS)}). Omit to list all.",
    ),
    session_id: List[str] = typer.Option(
        None,
        "--session-id",
        help="Filter to carve-out(s) recorded under this session id. Repeatable; "
        "/pr merged passes the merged PR's owning session(s) so it never touches "
        "another session's backfill.",
    ),
    as_json: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit one JSON object per line (JSONL) instead of a human summary.",
    ),
) -> None:
    """List recorded carve-outs (read-only), optionally filtered by --kind / --session-id.

    Reads the CANONICAL ledger (the same one `add` writes to), so it works from
    a linked worktree. A missing ledger is not an error: prints nothing, exits 0.
    Powers /fno:pr merged's backfill slot via
    `--kind backfill --session-id <sid> --json`.
    """
    if kind is not None and kind not in VALID_KINDS:
        typer.echo(
            f"carveout: invalid --kind '{kind}' "
            f"(expected one of: {', '.join(VALID_KINDS)})",
            err=True,
        )
        raise typer.Exit(2)

    from fno.carveout.core import read_carveouts, resolve_carveout_root

    # Typer gives [] for an unset repeatable option; pass None so "no filter"
    # is distinct from "filter to the empty set".
    sessions = session_id or None
    try:
        rows = read_carveouts(resolve_carveout_root(), kind=kind, session_ids=sessions)
    except CarveoutError as exc:
        # A present-but-unreadable ledger is a FAILED read, not "no carve-outs":
        # surface it loud (exit 1) like `add`, so /pr merged never treats an
        # unreadable ledger as "no backfills to run".
        typer.echo(f"carveout: failed to read carve-outs: {exc}", err=True)
        raise typer.Exit(1)
    for r in rows:
        if as_json:
            typer.echo(json.dumps(r, separators=(",", ":")))
        else:
            # `... or default` (not `.get(k, default)`) so an explicit JSON null
            # value renders as the placeholder, not the string "None".
            need = r.get("need")
            first = (str(r.get("description") or "").splitlines() or [""])[0]
            suffix = f"  (need: {need})" if need else ""
            cid = r.get("id") or "?"
            kind_val = r.get("kind") or "?"
            typer.echo(f"{cid} [{kind_val}] {first}{suffix}")


@carveout_app.command("resolve")
def resolve_carveouts(
    ids: List[str] = typer.Argument(
        ...,
        help="Carve-out id(s) to remove from the ledger (e.g. cv-ab12cd34).",
    ),
) -> None:
    """Remove handled carve-out(s) from the ledger.

    Used by /fno:pr merged's backfill slot once a backfill is run or filed
    as a backlog node, so a later run never re-offers the same entry. Idempotent:
    an id not present is a silent no-op. Prints the count actually removed.
    """
    from fno.carveout.core import consume_carveouts, resolve_carveout_root

    # Dedupe (order-preserving) so a repeated id does not inflate the requested
    # count and trip a false shortfall warning - consume_carveouts dedupes
    # internally, so removed is by unique id.
    unique_ids = list(dict.fromkeys(ids))
    removed = consume_carveouts(resolve_carveout_root(), unique_ids)
    if removed < len(unique_ids):
        # consume_carveouts returns the count actually removed and is best-effort
        # (a lock timeout or unwritable ledger also returns a low count). A
        # shortfall must be visible so a locked-ledger failure is not mistaken
        # for "already resolved" - else /pr merged re-offers a handled backfill.
        # Exit stays 0 (an absent id is a legitimate idempotent no-op); the
        # signal is on stderr (mirrors retro/cli.py's removed<want warning).
        typer.echo(
            f"carveout: resolved {removed} of {len(unique_ids)} requested id(s); "
            "remainder absent or ledger unwritable",
            err=True,
        )
    typer.echo(f"resolved {removed} carve-out(s)")
