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
    BACKFILL_KIND,
    DESCRIPTION_CAP,
    VALID_KINDS,
    VALID_SEVERITIES,
    CarveoutError,
    add_carveout,
)

carveout_app = typer.Typer(
    no_args_is_help=True,
    help=(
        "LAST RESORT for work too big to land in this PR. Fix what you find "
        "instead: a problem you can fix here gets fixed here, in this PR, as "
        "its own commit - SIZE is the only justification for filing instead. "
        "Records to .fno/carveouts.jsonl; the retro-triage harvest at merge "
        "turns deferred/oos-bug into backlog nodes (deduped, classified), "
        "while `backfill` is handled by /fno:pr merged. That harvest is "
        "manual: `fno retro sweep-carveouts --apply` is the only thing that "
        "clears the ledger, so every row you file is a chore for a human. "
        "See `fno outstanding` for what has piled up."
    ),
)

_PRIORITY_RE = re.compile(r"^p[0-3]$")


@carveout_app.command(
    "add",
    epilog="Paired verbs: `fno carveout update <id>` corrects one in place "
    "(the id survives); `fno carveout resolve <id>` retires it.",
)
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
    scope: str = typer.Option(
        None,
        "--scope",
        help="The crown scope this carve-out discharges (e.g. an epic id). Lets a "
        "structured match - the king orphan check reads this field rather than "
        "grepping the description free text.",
    ),
    severity: str = typer.Option(
        None,
        "--severity",
        help="How much this left-out item matters: critical|high|medium|low, "
        "routed to priority p0..p3 at harvest. Defaults to p3 (today's behavior). "
        "A carveout filed to satisfy the fidelity gate is stamped high by "
        "provenance, not chosen here.",
    ),
) -> None:
    """Last resort: record work too big for this PR. Anything smaller, fix here instead."""
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

    if severity is not None and severity not in VALID_SEVERITIES:
        typer.echo(
            f"carveout: invalid --severity '{severity}' "
            f"(expected one of: {', '.join(VALID_SEVERITIES)})",
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
            scope=scope,
            severity=severity,
            storage_root=storage_root,
        )
    except CarveoutError as exc:
        typer.echo(f"carveout: failed to record carve-out: {exc}", err=True)
        raise typer.Exit(1)

    if unscoped:
        typer.echo(
            "carveout: no active session; carve-out recorded unscoped", err=True
        )

    # The size test on stderr, never stdout: machine consumers read the id.
    typer.echo(
        "carveout: filed as a last resort - SIZE is the only justification. "
        "If this was small enough to fix in this PR, fix it here and "
        "`fno carveout resolve` this row. Clears only via "
        "`fno retro sweep-carveouts --apply`, which nobody runs unprompted.",
        err=True,
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
    pr_number: int = typer.Option(
        None,
        "--pr-number",
        help="Resolve the merged PR's owning session(s) from ledger.json and "
        "filter to them (replaces the ritual's jq+grep pipeline). Mutually "
        "exclusive with --session-id.",
    ),
    all_sessions: bool = typer.Option(
        False,
        "--all",
        "-A",
        help="Read the WHOLE ledger instead of just this session's rows. Needed "
        "by any consumer that folds across sessions (the king orphan check "
        "does; it filters by .scope over every row).",
    ),
    as_json: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit one JSON object per line (JSONL) instead of a human summary. "
        "With --pr-number, emits ONE object carrying sessions_resolved + carveouts.",
    ),
) -> None:
    """List recorded carve-outs (read-only), filtered by --kind / --session-id / --pr-number.

    Reads the CANONICAL ledger (the same one `add` writes to), so it works from
    a linked worktree. A missing ledger is not an error: prints nothing, exits 0.
    Powers /fno:pr merged's backfill slot via
    `--kind backfill --pr-number <n> --json`, whose `sessions_resolved` (and,
    when empty, `reason`) drives the consume-vs-read-only branch - so that
    branch keys on verb output, never on an empty shell variable of unknowable
    provenance (x-f47f US2).
    """
    if kind is not None and kind not in VALID_KINDS:
        typer.echo(
            f"carveout: invalid --kind '{kind}' "
            f"(expected one of: {', '.join(VALID_KINDS)})",
            err=True,
        )
        raise typer.Exit(2)

    if pr_number is not None and session_id:
        typer.echo(
            "carveout: pass --pr-number or --session-id, not both "
            "(--pr-number resolves the sessions itself).",
            err=True,
        )
        raise typer.Exit(2)

    from fno.carveout.core import read_carveouts, resolve_carveout_root

    reason = None
    if pr_number is not None:
        from fno.graph._reconcile import resolve_current_repo_slug
        from fno.ledger_join import resolve_pr_sessions
        from fno.paths import ledger_json

        session_id, reason = resolve_pr_sessions(
            ledger_json(), pr_number, resolve_current_repo_slug()
        )

    # Typer gives [] for an unset repeatable option; pass None so "no filter"
    # is distinct from "filter to the empty set".
    sessions = session_id or None

    # Default scope is THIS session. The unscoped read was the default while
    # the safe path was opt-in, so an agent pointed at `carveout list` saw a
    # month of every other session's rows and could not tell which were its
    # own. --all restores the global read for the consumers that need it.
    unscoped_reason = None
    include_unscoped = False
    if pr_number is None and not sessions and not all_sessions:
        from fno.carveout.core import resolve_session_id
        from fno.paths import resolve_repo_root

        current = resolve_session_id(resolve_repo_root())
        if current:
            sessions = [current]
            # Ownerless rows ride along: `add` mints them whenever no session
            # resolves, and dropping them here would hide them from every
            # session forever (13 of 39 rows on this repo's ledger).
            include_unscoped = True
        else:
            # Never a silent empty: a scoped read that returns nothing when no
            # session resolves cannot be told apart from a genuinely clear
            # ledger. Print every row AND say why.
            unscoped_reason = "no active session"

    if pr_number is not None and not sessions:
        # Unresolved ownership is NOT "filter to nothing": the caller must see
        # every carve-out read-only plus the reason, and consume none.
        sessions = None
    try:
        rows = read_carveouts(
            resolve_carveout_root(),
            kind=kind,
            session_ids=sessions,
            include_unscoped=include_unscoped,
        )
    except CarveoutError as exc:
        # A present-but-unreadable ledger is a FAILED read, not "no carve-outs":
        # surface it loud (exit 1) like `add`, so /pr merged never treats an
        # unreadable ledger as "no backfills to run".
        typer.echo(f"carveout: failed to read carve-outs: {exc}", err=True)
        raise typer.Exit(1)

    if pr_number is not None:
        resolved = list(sessions or [])
        if as_json:
            typer.echo(json.dumps({
                "pr_number": pr_number,
                "sessions_resolved": resolved,
                "reason": reason,
                "consumable": bool(resolved),
                "carveouts": rows,
            }, separators=(",", ":")))
            return
        if reason:
            # Never silent: the read-only branch must be able to state WHY.
            typer.echo(f"carveout: {reason}; listing read-only, not consumable", err=True)

    if unscoped_reason is not None:
        # A positive banner naming the count and the reason, so the reader can
        # tell "everyone's rows" from "mine, and there are none".
        typer.echo(
            f"carveout: {unscoped_reason}; listing all {len(rows)} row(s) "
            f"across every session. Pass --all to ask for this explicitly.",
            err=True,
        )

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


@carveout_app.command(
    "update",
    epilog="Reverses nothing - it CORRECTS. To retire a row instead, "
    "`fno carveout resolve <id> --reason \"...\"`.",
)
def update(
    cv_id: str = typer.Argument(..., help="Carve-out id to edit (e.g. cv-ab12cd34)."),
    description: str = typer.Option(
        None,
        "--description",
        "-d",
        help="Replacement text. Truncated past "
        f"{DESCRIPTION_CAP} chars, like `add`.",
    ),
    kind: str = typer.Option(
        None,
        "--kind",
        "-k",
        help=f"Reclassify: {' | '.join(VALID_KINDS)}.",
    ),
    need: str = typer.Option(None, "--need", help="Replacement dependency text."),
    priority: str = typer.Option(
        None, "--priority", "-p", help="Replacement priority hint (p0-p3)."
    ),
    scope: str = typer.Option(None, "--scope", help="Replacement crown scope."),
) -> None:
    """Correct a carve-out in place, keeping its id.

    Fixing the wording of a carve-out used to mean ``resolve`` then ``add``.
    That changed the id, so every id already quoted in a PR body or a mail
    became a dead pointer, and it was lossy: two writes, and a failure between
    them left the ledger with neither the old row nor the new one. That happened
    live, and the replacement id had to be chased through three messages.

    Only the options you pass are replaced; the rest of the row is untouched.

    Refuses rather than guessing:
      - no field given (exit 2) - a no-op that prints a success line is exactly
        the lie this verb exists to stop
      - an id that is not on the ledger (exit 1) - never creates it, since that
        would resurrect a row ``/pr merged`` already consumed, under a later
        PR's number
      - an empty ``--description`` (exit 2) - an empty carve-out is a lost one
      - an unreadable or unwritable ledger (exit 1) - a failed edit must not
        report as a clean no-op while the old wording sits on disk
    """
    from fno.carveout.core import (
        CarveoutNotFound,
        resolve_carveout_root,
        update_carveout,
    )

    if all(v is None for v in (description, kind, need, priority, scope)):
        typer.echo(
            "carveout: nothing to update; pass at least one of --description / "
            "--kind / --need / --priority / --scope",
            err=True,
        )
        raise typer.Exit(2)

    if description is not None and not description.strip():
        typer.echo("carveout: --description cannot be blank", err=True)
        raise typer.Exit(2)

    if kind is not None and kind not in VALID_KINDS:
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

    root = resolve_carveout_root()
    # Read the old kind BEFORE the rewrite so the warning below can name what
    # actually changed rather than asserting a transition that may not have
    # happened.
    old_kind = None
    if kind is not None:
        from fno.carveout.core import read_carveouts

        try:
            old_kind = next(
                (
                    str(r.get("kind"))
                    for r in read_carveouts(root)
                    if str(r.get("id")) == cv_id
                ),
                None,
            )
        except CarveoutError:
            old_kind = None

    try:
        rec = update_carveout(
            root,
            cv_id,
            description=description,
            kind=kind,
            need=need,
            priority=priority,
            scope=scope,
        )
    except CarveoutNotFound as exc:
        typer.echo(f"carveout: {exc}", err=True)
        raise typer.Exit(1)
    except CarveoutError as exc:
        typer.echo(f"carveout: failed to update {cv_id}: {exc}", err=True)
        raise typer.Exit(1)

    # A warning, not a refusal. Crossing the backfill boundary hands the row to
    # a different consumer (/pr merged's backfill slot vs the generic retro
    # harvest), which is worth saying out loud - but refusing a legitimate
    # reclassification would be worse than permitting it.
    if old_kind is not None and kind is not None and old_kind != kind:
        crossed = BACKFILL_KIND in (old_kind, kind)
        if crossed:
            typer.echo(
                f"carveout: {cv_id} moved {old_kind} -> {kind}; it now belongs to "
                f"{'/fno:pr merged' if kind == BACKFILL_KIND else 'the retro-triage harvest'} "
                f"instead",
                err=True,
            )

    if rec.get("truncated"):
        typer.echo(
            f"carveout: description truncated at {DESCRIPTION_CAP} chars", err=True
        )

    # Same machine-first stdout contract as `add`: the id, which is the value
    # this verb exists to preserve.
    typer.echo(cv_id)


@carveout_app.command("resolve")
def resolve_carveouts(
    ids: List[str] = typer.Argument(
        ...,
        help="Carve-out id(s) to remove from the ledger (e.g. cv-ab12cd34).",
    ),
    reason: str = typer.Option(
        None,
        "--reason",
        "-R",
        help="Why these rows are being retired without a node. Recorded as a "
        "`carveout_resolved` event - the only trace a row leaves when it is "
        "removed without becoming tracked work.",
    ),
) -> None:
    """Remove handled carve-out(s) from the ledger.

    Used by /fno:pr merged's backfill slot once a backfill is run or filed
    as a backlog node, so a later run never re-offers the same entry. Idempotent:
    an id not present is a silent no-op. Prints the count actually removed.

    ``--reason`` exists for the row that should be retired WITHOUT filing
    anything: test residue, a duplicate of work already tracked elsewhere, a
    carve-out overtaken by events. Without the reason that same removal is
    indistinguishable from dropping the work on the floor.

    This docstring used to justify that flag by asserting the backlog had no
    delete verb, so a junk node was permanent. That was FALSE: ``fno backlog
    remove`` has always existed, and ``fno backlog reopen`` now reverses a
    close. The claim was read by an agent, which repeated it to an operator as
    the load-bearing reason for a ruling; another project kept 23 nodes it
    believed un-file-able. Corrected here rather than deleted, because the
    failure mode is worth naming: prose in a docstring is consulted as fact.

    To CORRECT a carve-out rather than retire it, use ``fno carveout update``,
    which preserves the id. Resolving and re-adding changes the id and loses
    the content if the second step fails.
    """
    from fno.carveout.core import (
        consume_carveouts,
        read_carveouts,
        resolve_carveout_root,
    )

    # Dedupe (order-preserving) so a repeated id does not inflate the requested
    # count and trip a false shortfall warning - consume_carveouts dedupes
    # internally, so removed is by unique id.
    unique_ids = list(dict.fromkeys(ids))
    root = resolve_carveout_root()
    # Which requested ids are actually ON the ledger, read BEFORE the removal.
    # consume_carveouts returns only a count, so without this the event below
    # would name every id the operator typed - asserting that an absent id was
    # retired for that reason while it sits on some other ledger, or nowhere.
    # An unreadable ledger degrades to "cannot attribute" rather than guessing.
    try:
        present = {
            str(r.get("id"))
            for r in read_carveouts(root)
            if str(r.get("id")) in set(unique_ids)
        }
    except Exception:
        present = set()
    removed = consume_carveouts(root, unique_ids)
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
    # `removed > 0`, not just `reason`: consume_carveouts is best-effort and
    # returns 0 on a lock timeout or an unwritable ledger, so emitting there
    # would record a retirement of rows still sitting on the ledger.
    if reason and removed:
        # Best-effort: the rows are already gone, so a failed emit must not read
        # as a failed resolve. It must not be silent either - an unrecorded
        # reasoned removal is the untraceable drop the flag exists to prevent.
        try:
            from fno.events import _build, append_event
            from fno.paths import project_log

            # Explicit events_path, rooted where the LEDGER lives. Without it
            # append_event writes to a relative ./.fno/events.jsonl under the
            # caller's cwd: run from a subdirectory or a linked worktree, the
            # row is removed from the canonical ledger while its only trace
            # lands somewhere that does not survive the worktree's archival.
            append_event(
                _build(
                    "carveout_resolved",
                    # "backlog", not "carveout": the envelope source is a closed
                    # enum and there is no carveout member. A wrong value fails
                    # validation at emit time, which is how this was caught.
                    "backlog",
                    {
                        # The ids actually retired, not every id requested.
                        "carveout_ids": ",".join(
                            i for i in unique_ids if i in present
                        ) or ",".join(unique_ids),
                        "reason": reason,
                        "removed": removed,
                    },
                ),
                events_path=project_log(
                    "events.jsonl", project_root=resolve_carveout_root()
                ),
            )
        except Exception as exc:
            typer.echo(
                f"carveout: resolved, but the reason was NOT recorded "
                f"({type(exc).__name__}: {exc}); re-record it by hand",
                err=True,
            )
    typer.echo(f"resolved {removed} carve-out(s)")
