"""``fno agents lead`` - board reads and session init for the lead loop."""
from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import NoReturn, Optional, cast

import typer

from fno.lead.lead_faq import faq_app

lead_app = typer.Typer(
    name="lead",
    help="The lead's board: what still needs doing, and the session manifest for its loop.",
    no_args_is_help=True,
)


#: Per-queue render cap for the human board view (was board.DEFAULT_MAX_ROWS;
#: the count in the payload stays whole, only the rendered rows are cut).
DEFAULT_MAX_ROWS = 25


def _default_max_rows() -> int:
    return DEFAULT_MAX_ROWS


def _refuse(msg: str) -> NoReturn:
    typer.echo(msg, err=True)
    raise typer.Exit(2)


def _emit_cancel_signal(path: Path, scope: str) -> None:
    """Record a lead cancel after the sentinel is safely on disk."""
    try:
        from fno.events import _build, append_event

        append_event(
            _build(
                "cancel_signal_set",
                "agents",
                {"lane": "lead", "path": str(path), "scope": scope, "reason": "operator"},
            ),
            events_path=path.parent.parent / "events.jsonl",
        )
    except Exception as exc:  # noqa: BLE001 - the sentinel remains authoritative
        typer.echo(f"lead: WARNING: cancel signal event not emitted: {exc}", err=True)


@lead_app.command("init")
def init_cmd(
    scope_values: list[str] = typer.Option(
        ...,
        "--scope",
        help="What this lead was promoted over. Repeat for a set of epics.",
    ),
    harness_session_id: str = typer.Option(
        "", "--harness-session-id", help="The lead's own harness session id."
    ),
    max_iterations: int = typer.Option(
        40, "--max-iterations", help="Iteration ceiling before the loop stops on Budget."
    ),
    respawn_ceiling: int = typer.Option(
        4,
        "--respawn-ceiling",
        help="Lead sessions the walk may respawn before it terminates on Budget.",
    ),
    force: bool = typer.Option(
        False, "--force", "-F", help="Replace an existing manifest."
    ),
) -> None:
    """Write this role scope's manifest, which the lead loop arms read.

    Write-once, like the target manifest; without it the stop hook allows exit
    silently (correct for a session nobody promoted). A successor init needs
    --force only when the predecessor died without stepping_down.
    """
    from fno.lead.state import (
        LeadManifestExists,
        lead_loop_enabled,
        lead_manifest_path,
        write_manifest,
    )

    # The ONE chokepoint for `config.lead.enabled`: every arm (hook shim,
    # loop-check, LeadQueue) arms on the manifest's existence, so gating the
    # manifest gates all three. The version this replaces gated zero paths.
    if not lead_loop_enabled():
        typer.echo(
            "lead: config.lead.enabled is false, so no lead is promoted. "
            "Enable it with `fno config set lead.enabled true`.",
            err=True,
        )
        raise typer.Exit(3)

    # An id-less manifest matches nobody, so it gates every session or none;
    # refuse to write a role that cannot be attributed.
    if not harness_session_id.strip():
        typer.echo(
            "lead: --harness-session-id is required. The stop hook gates the "
            "session the manifest NAMES, so an unattributable manifest roles "
            "nobody and risks holding unrelated sessions open.",
            err=True,
        )
        raise typer.Exit(2)

    # Every spelling of one role must arm the SAME file: canonicalize the set
    # AND resolve aliases, or `--scope a` arms leads/a.md while row-keyed
    # readers resolve leads/alpha.md from row.role_scope - and the
    # unpromoted-row warning stays silent, comparing through the same
    # normalization.
    from fno.agents.role import _canonical_members, canonical_scope, split_scope

    scope = canonical_scope(scope_values)
    members = split_scope(scope)
    if not members:
        typer.echo(
            "lead: --scope needs a role scope: name an epic or a project.",
            err=True,
        )
        raise typer.Exit(2)
    scope = canonical_scope(list(_canonical_members(scope)))
    from fno.rust_binary import call_binary_json
    admit = ["readiness", "--scope", scope, "--session", harness_session_id, "--ensure-goal"]
    error, _ = call_binary_json("loop", admit)
    if error:
        typer.echo(f"lead: refusing role admission for {scope!r}: {error}", err=True)
        raise typer.Exit(2)

    try:
        manifest_path = lead_manifest_path(scope)
    except ValueError as exc:
        # A path-unsafe scope member ('..' or a '/') is a refusal, not a
        # traceback: the safety check lives in lead_manifest_path.
        typer.echo(str(exc), err=True)
        raise typer.Exit(2) from exc

    try:
        fields = write_manifest(
            manifest_path,
            scope=scope,
            harness_session_id=harness_session_id,
            max_iterations=max_iterations,
            respawn_ceiling=respawn_ceiling,
            force=force,
        )
        manifest_path.with_suffix(".cancelled").unlink(missing_ok=True)
    except LeadManifestExists as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(1) from exc
    typer.echo(f"lead: manifest written: {manifest_path}")
    typer.echo(f"fno_id: {fields['fno_id']}")
    typer.echo(f"scope:  {fields['scope']}")
    _print_settled_children(scope)
    _warn_unpromoted_row(scope)


def _print_settled_children(scope: str) -> None:
    """Print the role scope's settled children as titles, or nothing.

    A role lands on an epic whose history predates the session, and every
    board read a lead makes filters to open rows, so the done children that
    record what the epic already established are invisible exactly when they
    matter most. Titles only: no search over details, no similarity
    score. One local graph read; nothing prints when none exist or the graph
    cannot be read.
    """
    from fno.agents.role import _graph_index, split_scope

    by_id = _graph_index()
    if by_id is None:
        return
    lines = []
    for member in split_scope(scope):
        for entry in by_id.values():
            if entry.get("parent") != member:
                continue
            if entry.get("status") not in ("done", "superseded"):
                continue
            title = entry.get("title") or entry.get("id") or ""
            lines.append(f"- {title} ({entry.get('id')})")
    if not lines:
        return
    typer.echo()
    typer.echo(
        "Settled findings under this role (done or superseded children; "
        "read before the first check-in, never re-derive):"
    )
    for line in lines:
        typer.echo(line)


def _warn_unpromoted_row(scope: str) -> None:
    """Warn when the manifest is armed but the row carries no role.

    Arming and authority are separate (only `fno agents role` stamps the row),
    but the three row-keyed readers - `lead done`, `lead manifest-path`, the
    post-compact reinject hook - fail CLOSED and QUIETLY with no role, and a
    role over OTHER territory is as absent to them as none. Warn, never
    refuse: the loop arms on the FILE, which works, and warning keeps a
    legitimate arm-then-role ordering usable. An unresolvable row is an
    unanswered question, not a finding.
    """
    from fno.agents.role import (
        AGENT_UNREGISTERED,
        REGISTRY_UNREADABLE,
        calling_agent_row,
        role_scope_matches,
        role_reading,
    )

    try:
        row = calling_agent_row()
    except Exception:  # noqa: BLE001 - a warning never breaks a written manifest
        return
    if row is REGISTRY_UNREADABLE or row is AGENT_UNREGISTERED or row is None:
        typer.echo(
            "lead: warning: manifest armed, but this session resolves to no "
            "registry row, so its role cannot be checked. Run `/fno-me` to "
            "register, then have an attended shell run "
            f"`fno agents org promote <handle> --scope {scope}`.",
            err=True,
        )
        return
    handle = getattr(row, "name", "") or "<handle>"
    reading = role_reading(row)
    if reading is not None and role_scope_matches(reading.get("scope"), scope):
        return

    # Identical consequence for a missing and a mismatched role: the readers
    # key on role_scope, so other territory is as absent as none.
    consequence = (
        "`fno agents lead done` will refuse, `fno agents lead manifest-path` "
        "will exit non-zero without printing a path so the stop hook leaves "
        "LEAD_STATE_FILE unset, and the post-compact lead brief will never "
        "arrive."
    )
    if reading is None:
        typer.echo(
            f"lead: warning: manifest armed for {scope!r}, but this row "
            f"carries NO role, so {consequence} Ask an attended shell to run "
            f"`fno agents org promote {handle} --scope {scope}`.",
            err=True,
        )
        return
    typer.echo(
        f"lead: warning: manifest armed for {scope!r}, but this row's role is "
        f"{reading['label']!r}, which is not that scope. These readers key on "
        f"an EXACT role_scope, so a role over wider territory does not "
        f"satisfy them any more than a role over unrelated territory: "
        f"{consequence} Ask an attended shell to re-scope it with "
        f"`fno agents org promote {handle} --scope {scope}`.",
        err=True,
    )


@lead_app.command("done")
def done_cmd(
    scope: str = typer.Option(
        "", "--scope", help="Role scope to expire. Default: this session's own role."
    ),
) -> None:
    """Expire this role: vacate the row and clear the scope manifest.

    The departure half of the lifecycle: a lead ending its term calls it so a
    successor arms without --force. A lead that dies without it leaves an inert
    manifest (the registry row is authority).
    """
    from dataclasses import replace as _replace

    from fno.agents.role import (
        AGENT_UNREGISTERED,
        REGISTRY_UNREADABLE,
        _canonical_members,
        _derived_level,
        _locked_identity,
        calling_agent_row,
        canonical_scope,
        role_answers_to,
        emit_role_vacated,
    )
    from fno.agents.team import _graph_index, find_presiding_role
    from fno.agents.registry import TERMINAL_STATUSES as _TERMINAL_ROW_STATUSES
    from fno.agents.registry import load_registry, update_registry
    from fno.lead.state import lead_manifest_path, parse_manifest, remove_lead_manifest
    from fno.lead.state import _owner_state_root

    if scope.strip():
        # Rows store the canonical form, so the compares below must not
        # depend on member order or aliases.
        scope = canonical_scope(list(_canonical_members(scope)))
    caller = calling_agent_row()
    if caller is REGISTRY_UNREADABLE or caller is AGENT_UNREGISTERED:
        typer.echo(
            "lead: cannot verify the caller's role: this session carries an "
            "agent identity the registry does not resolve to a row, and "
            "expiring a role requires a holder. Run /fno-me or retry, or "
            "pass --scope from an attended shell.",
            err=True,
        )
        raise typer.Exit(2)

    if caller is None:
        if not scope.strip():
            typer.echo(
                "lead: an attended shell holds no role of its own; pass "
                "--scope <territory> to expire that role.",
                err=True,
            )
            raise typer.Exit(2)
        # One member of a set role is not a role: vacating nothing and
        # clearing no manifest would still print a false expiry receipt.
        try:
            rows = load_registry()
        except Exception as exc:  # noqa: BLE001 - named, never swallowed
            typer.echo(f"lead: role expire failed: {exc}", err=True)
            raise typer.Exit(1) from exc
        live = [row for row in rows if row.status not in _TERMINAL_ROW_STATUSES]
        if not any(row.role_scope == scope for row in live):
            for row in live:
                if role_answers_to(row.role_scope, scope):
                    typer.echo(
                        f"lead: refusing to expire {scope!r}: it is one member of "
                        f"{row.name}'s role over {row.role_scope!r}. Expire the "
                        f"whole role with --scope {row.role_scope}, or narrow it "
                        f"with fno agents org promote {row.name} --scope <the remaining epics>.",
                        err=True,
                    )
                    raise typer.Exit(2)
        # A named scope may still have a LIVE lead; expiring only the manifest
        # would disarm its stop-hook floor while the row reads promoted. The
        # vacate closure below resolves the holder under the lock; no live
        # holder is orphan cleanup and clears the file alone.
        holder_name = ""
    else:
        own = getattr(caller, "role_scope", None)
        if not scope.strip():
            if not own:
                typer.echo(
                    "lead: this session holds no role, so there is nothing "
                    "to expire.",
                    err=True,
                )
                raise typer.Exit(2)
            scope = own
        elif own != scope:
            rows = load_registry()
            mine = [r for r in rows if getattr(r, "role_scope", None) == scope]
            roles = [
                {"holder": r.name, "level": r.role_level, "scope": r.role_scope}
                for r in rows
                if r.status not in _TERMINAL_ROW_STATUSES and r.role_level is not None
            ]
            target_level = next((r.role_level for r in mine if r.role_level is not None), None)
            presider = find_presiding_role(
                scope, target_level or _derived_level(scope), roles, _graph_index()
            )
            has_live = any(r.status not in _TERMINAL_ROW_STATUSES for r in mine)
            member = any(role_answers_to(r.role_scope, scope) and r.role_scope != scope
                         for r in rows)
            if has_live or member or (presider or {}).get("holder") != caller.name:
                typer.echo(
                    f"lead: refusing to expire {scope!r}: this session's role is "
                    f"{own!r}, and an agent expires only its own role. Call "
                    "`fno agents lead done` with no --scope, or use an attended "
                    "shell for another territory.",
                    err=True,
                )
                raise typer.Exit(2)
        holder_name = "" if own != scope else caller.name

    # Snapshot the manifest's OWN session id BEFORE vacating: removal compares
    # it under the manifest lock, so a successor promoted mid-vacate survives.
    # Read from the file, never the row, so a resumed lead expires the manifest
    # it was armed with.
    owner_root = _owner_state_root(getattr(caller, "cwd", None))
    try:
        expired_manifest_session = (
            parse_manifest(
                lead_manifest_path(scope, state_root=owner_root)
            ).get("harness_session_id") or None
        )
    except (OSError, ValueError):
        expired_manifest_session = None

    # Vacate the row BEFORE the file, under the registry lock: a scope that
    # moved to a successor mid-call is refused here instead of disarming the
    # successor's manifest below (same order as the succession path).
    vacated = holder_name is None
    vacated_rows: list = []
    if holder_name is not None:
        attended_named = holder_name == ""

        def _vacate(rows: list) -> list:
            nonlocal vacated, vacated_rows
            if attended_named and caller is not None:
                if any(r.role_scope == scope
                       and r.status not in _TERMINAL_ROW_STATUSES for r in rows):
                    raise ValueError("a live holder promoted mid-expiry")
                return rows
            # The caller's row, matched by name AND session; a rebound name refuses.
            holder_row = None if attended_named else _locked_identity(rows, caller)
            for index, row in enumerate(rows):
                if attended_named:
                    # Attended + named scope: vacate whatever live row holds
                    # it; an orphaned scope clears the manifest alone below.
                    if (
                        row.role_scope == scope
                        and row.status not in _TERMINAL_ROW_STATUSES
                    ):
                        vacated_rows.append(row)
                        rows[index] = _replace(
                            row,
                            role_level=None,
                            role_scope=None,
                            role_grantor=None,
                        )
                        vacated = True
                elif holder_row is not None and row is holder_row and row.role_scope == scope:
                    vacated_rows.append(row)
                    rows[index] = _replace(
                        row, role_level=None, role_scope=None, role_grantor=None
                    )
                    vacated = True
                    break
            return rows

        try:
            update_registry(_vacate)
        except Exception as exc:  # noqa: BLE001 - named, never swallowed
            typer.echo(f"lead: role expire failed: {exc}", err=True)
            raise typer.Exit(1) from exc
        if not vacated and not attended_named:
            typer.echo(
                f"lead: refusing to expire {scope!r}: this row no longer holds "
                "it (the role moved or was already vacated), so the manifest "
                "on disk may belong to a successor. Re-read with "
                "`fno agents team` before expiring anything.",
                err=True,
            )
            raise typer.Exit(1)
        for vacated_row in vacated_rows:
            # The row write is the authority: this fires even if the manifest
            # removal below then fails, and never on the refusal above. One
            # event per vacated row, so a split role records every holder.
            emit_role_vacated(
                scope=scope, level=vacated_row.role_level, holder=vacated_row.name,
                holder_session=vacated_row.harness_session_id,
                grantor=vacated_row.role_grantor,
                cause="expired" if attended_named else "stepped_down",
            )

    # The session-id snapshot guards the successor race under the manifest
    # lock (ownership was proven by the locked vacate above). False means the
    # file is no longer the manifest this expiry targeted.
    if not remove_lead_manifest(
        scope, expected_harness_session_id=expired_manifest_session, state_root=owner_root
    ):
        typer.echo(
            f"lead: row vacated, but the manifest for {scope!r} could not be "
            "removed: the file no longer names the session this expiry "
            "snapshotted, so a successor promoted mid-expiry likely owns it "
            "now. Re-read with `fno agents team` before touching anything.",
            err=True,
        )
        raise typer.Exit(1)
    if not vacated:
        # No live holder: the manifest clear is the whole vacate.
        emit_role_vacated(
            scope=scope, level=None, holder=None,
            holder_session=expired_manifest_session,
            grantor=None, cause="orphan_manifest",
        )
    typer.echo(f"lead: role expired: {scope}")
    typer.echo(
        "row role: vacated; manifest: cleared"
        if vacated
        else "row role: no live holder found; manifest: cleared"
    )


@lead_app.command("cancel")
def cancel_cmd(
    scope: str = typer.Option(..., "--scope", help="Role scope whose walk to cancel."),
    clear: bool = typer.Option(False, "--clear", help="Clear the lead cancel signal."),
) -> None:
    """Set or clear the cancel signal beside a canonical role manifest."""
    from fno.lead.state import lead_manifest_path

    manifest = lead_manifest_path(scope)
    if not manifest.is_file():
        typer.echo(
            f"lead: refusing cancel for {scope!r}; no lead manifest at canonical path "
            f"{manifest}",
            err=True,
        )
        raise typer.Exit(1)
    sentinel = manifest.with_suffix(".cancelled")
    try:
        if clear:
            sentinel.unlink(missing_ok=True)
            typer.echo(f"lead: cancel signal cleared: {sentinel}")
        else:
            sentinel.touch()
            _emit_cancel_signal(sentinel, scope)
            typer.echo(f"lead: cancel signal set: {sentinel}")
    except OSError as exc:
        typer.echo(f"lead: could not update cancel signal {sentinel}: {exc}", err=True)
        raise typer.Exit(1) from exc


def _own_role_argv(verb: str, scope: str) -> tuple[list[str], str]:
    """The caller ladder every ``term-*`` verb needs: resolve the caller's
    own role, refuse an unregistered caller or a foreign scope, resolve the
    binary, and return the argv prefix (binary, verb, --scope, --root, and
    --session when known) plus the resolved scope. Shared by ``shape`` and
    ``term`` so the ladder is written once (law d-b6cc1a2a: new logic in
    crates, this shell only relays).
    """
    from fno.agents.role import (
        AGENT_UNREGISTERED,
        REGISTRY_UNREADABLE,
        calling_agent_row,
    )
    from fno.lead.state import _owner_state_root
    from fno.rust_binary import resolve_binary

    caller = calling_agent_row()
    if caller is REGISTRY_UNREADABLE or caller is AGENT_UNREGISTERED:
        _refuse(
            "lead: cannot resolve the caller's role: this session carries an "
            "agent identity the registry does not resolve to a row. Run /fno-me "
            "or retry."
        )
    if caller is None:
        _refuse(
            "lead: an attended shell holds no role; the promoted session "
            "declares its own term from inside it."
        )
    own = getattr(caller, "role_scope", None)
    if not own:
        _refuse("lead: this session holds no role, so there is no term to declare on.")
    if scope.strip() and scope != own:
        _refuse(
            f"lead: refusing to act on {scope!r}: this session's role is "
            f"{own!r}, and a holder declares only its own term."
        )
    session_id = (
        getattr(caller, "harness_session_id", None)
        or getattr(caller, "cc_session_id", None)
        or ""
    )
    binary = resolve_binary()
    if binary is None:
        _refuse(
            "lead: the fno-agents binary was not found, and the write lives "
            "there. Reinstall fno, run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN."
        )
    root = _owner_state_root(getattr(caller, "cwd", None))
    argv = [str(binary), verb, "--scope", own,
            "--root", str(root.parent if root.name == ".fno" else root)]
    if session_id:
        argv += ["--session", session_id]
    return argv, own


@lead_app.command("shape")
def shape_cmd(
    shape: str = typer.Argument(..., help="pass or team."),
    scope: str = typer.Option(
        "", "--scope", help="Role scope to reshape. Default: this session's own role."
    ),
) -> None:
    """Declare this term's shape: a pure pass, or a team holding workers.

    The field the Stop nudge reads; declare ``team`` at the first worker. The
    write lives in Rust; this shell keeps the caller ladder (Python self-stamp).
    """
    import subprocess

    from fno._subprocess_util import propagate_returncode

    if shape not in ("pass", "team"):
        _refuse("lead: shape must be 'pass' or 'team'.")
    argv, own = _own_role_argv("lead-shape", scope)
    argv += ["--shape", shape]
    result = subprocess.run(argv, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        typer.echo(f"lead: {result.stderr.strip()}", err=True)
        raise typer.Exit(code=propagate_returncode(result.returncode))
    typer.echo(f"lead: shape declared: {result.stdout.strip()}")
    typer.echo(f"scope:  {own}")


@lead_app.command(
    "term",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def term_cmd(ctx: typer.Context) -> None:
    """Declare or extend this term's term: the bound past which the Stop
    hook demands a handoff receipt (``--succeed``) or a written ``--reason``.

    Usage: ``lead term <spec> [--reason TEXT] [--scope SCOPE]``, spec being
    ``span:<N>[smhd]`` or ``compactions:<N>``. Undeclared reads a 96h
    default. Flags pass through as raw args (the flag-registry ratchet bars
    new ``typer.Option`` calls in this tree; a new verb belongs in crates),
    so this shell scans them by hand rather than declaring them. The write
    lives in Rust; this shell keeps the caller ladder (Python self-stamp).
    """
    import re
    import subprocess

    from fno._subprocess_util import propagate_returncode

    passed = list(ctx.args)
    positional = [a for a in passed if not a.startswith("-")]
    if not positional:
        _refuse("lead: term needs a spec: span:<N>[smhd] or compactions:<N>.")
    spec = positional[0]
    reason = next(
        (passed[i + 1] for i, t in enumerate(passed) if t == "--reason" and i + 1 < len(passed)),
        "",
    )
    scope = next(
        (passed[i + 1] for i, t in enumerate(passed) if t == "--scope" and i + 1 < len(passed)),
        "",
    )
    if not re.fullmatch(r"span:[0-9]+[smhd]|compactions:[0-9]+", spec.strip()):
        _refuse(f"lead: bad term spec {spec!r}; legal forms: span:<N>[smhd], compactions:<N>")
    argv, own = _own_role_argv("lead-shape", scope)
    argv += ["--term", spec]
    if reason.strip():
        argv += ["--reason", reason]
    result = subprocess.run(argv, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        typer.echo(f"lead: {result.stderr.strip()}", err=True)
        raise typer.Exit(code=propagate_returncode(result.returncode))
    typer.echo(f"lead: term declared: {result.stdout.strip()}")
    typer.echo(f"scope:  {own}")


@lead_app.command("manifest-path", hidden=True)
def manifest_path_cmd(
    harness_session_id: str = typer.Option(..., "--harness-session-id"),
    harness: str = typer.Option("", "--harness"),
    state_root: Optional[Path] = typer.Option(None, "--state-root"),
) -> None:
    """Print this live promoted session's existing scope manifest path."""
    from fno.lead.state import resolve_lead_manifest_path

    path, reason = resolve_lead_manifest_path(
        harness_session_id,
        harness or None,
        state_root=state_root,
    )
    if path is None:
        typer.echo(f"lead manifest-path: {reason}", err=True)
        raise typer.Exit(1)
    typer.echo(path)


def history_cmd(
    scope: str = typer.Option(
        "", "--scope", help="Role scope to read. Default: this session's own role."
    ),
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the full JSON payload."),
) -> None:
    """Read this role's recorded term: its check-ins, newest first, verbatim.

    ``fno agents team -n`` answers who rules NOW; this answers what
    happened across the term. Contract: docs/architecture/lead.md.
    """
    from fno.lead.history import run_native
    from fno.paths import event_journals

    code, out, err = run_native(event_journals(), scope, as_json)
    if out:
        typer.echo(out.rstrip("\n"))
    if err:
        typer.echo(err.rstrip("\n"), err=True)
    raise typer.Exit(code)


def checkin_cmd(ctx: typer.Context) -> None:
    """Run the term check-in body: gather, print, diff, journal.

    Flags pass through to the native ``lead-checkin`` beat: [--scope
    <scope>] [--no-emit] [--json]. The beat resolves the caller's role
    scope, level and board state natively; this shell supplies the paths
    Python owns. The beat never decides.
    """
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.paths import (
        event_journals,
        graph_json,
        handoffs_dir,
        lead_faqs_dir,
        project_events_json,
    )
    from fno.rust_binary import resolve_binary

    passed = list(ctx.args)
    binary = resolve_binary()
    if binary is None:
        _refuse(
            "lead: the fno-agents binary was not found, and the check-in beat "
            "runs there. Reinstall fno, run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN."
        )
    argv = [
        str(binary),
        "lead-checkin",
        *passed,
        "--graph",
        str(graph_json()),
        "--handoffs-dir",
        str(handoffs_dir()),
        "--faqs-dir",
        str(lead_faqs_dir()),
        "--emit-path",
        str(project_events_json()),
    ]
    for path in event_journals():
        argv += ["--events-path", str(path)]
    proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    if proc.stdout:
        typer.echo(proc.stdout.rstrip("\n"))
    if proc.stderr:
        typer.echo(proc.stderr.rstrip("\n"), err=True)
    raise typer.Exit(code=propagate_returncode(proc.returncode))


def verdict_cmd(
    scope: str = typer.Argument("", help="Role scope. Default: this session's own."),
) -> None:
    """Read the term tenure verdict; the native renderer's words pass through."""
    from fno.lead.history import verdict_read
    from fno.paths import event_journals

    code, out, err = verdict_read(event_journals(), scope or None, as_json=False)
    typer.echo(out, nl=False)
    if code != 0:
        _refuse(f"lead verdict unreadable: {err.strip()}")


def ledger_cmd(
    out: Optional[Path] = typer.Option(
        None, "--out", help="Write the page here instead of <state_dir>/term.html."
    ),
) -> None:
    """Render the term ledger page: every role, its territory, its nodes.

    The page assembly is the native ``lead-rundown`` verb; this shell resolves
    the team and the paths. Contract: docs/architecture/lead.md.
    """
    from fno.lead.ledger import build_ledger_data, write_ledger

    try:
        path = write_ledger(build_ledger_data(), out)
    except RuntimeError as exc:
        _refuse(f"lead: {exc}")
    # The daemon's team_ledger arm greps this exact prefix
    # (crates/fno-agents/src/rundown.rs run_ledger); keep the two in step.
    typer.echo(f"lead ledger: {path}")


@lead_app.command("board")
def board_cmd(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the board payload."),
    max_rows: int = typer.Option(
        _default_max_rows(), "--max-rows", help="Rows rendered per queue."
    ),
    last_run: bool = typer.Option(
        False,
        "--last-run",
        help="Instead of reading the board, ask whether a lead walk terminated recently.",
    ),
    since: str = typer.Option("24h", "--since", help="Window for --last-run (e.g. 24h, 90m, 7d)."),
    budget_ms: Optional[int] = typer.Option(
        None,
        "--budget-ms",
        help="Whole-board budget in milliseconds. Every per-source slice is "
        "derived from this one total; a caller that enforces an outer timer "
        "(the stop gate) passes that same bound in, so the board returns a "
        "payload naming what it could not read instead of being killed.",
    ),
    state: Optional[Path] = typer.Option(
        None,
        "--state",
        help="Lead manifest whose scope bounds the board. Defaults to this "
        "session's own role; pass one explicitly to read outside it.",
    ),
) -> None:
    """Report every queue that would keep a lead working.

    The collector is the Rust runtime (d-e11b2b3e): this command is a
    shell over `fno-agents board` - it resolves the binary, passes the whole
    budget and the manifest through, and renders or emits the returned payload.
    A bare call from a promoted session defaults to that role's manifest (the
    registry row plus the file, never presence alone, is the authority);
    anything else reads the fleet-wide board. Exits non-zero when any queue
    could not be read: an unreadable queue is not an empty one.
    """
    import subprocess

    from fno.rust_binary import resolve_binary

    if last_run:
        from fno.lead.state import last_run_is_fresh, parse_window
        from fno.paths import project_events_json

        try:
            window_s = parse_window(since)
        except ValueError as exc:
            typer.echo(str(exc), err=True)
            raise typer.Exit(2) from exc
        fresh = last_run_is_fresh(project_events_json(), since_s=window_s)
        typer.echo(f"last lead walk within {since}: {'yes' if fresh else 'no'}")
        raise typer.Exit(0 if fresh else 1)

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno inbox board: the fno-agents binary was not found, and the board reads "
            "through the Rust runtime. Reinstall fno, run `fno doctor update --rust`, "
            "or set FNO_AGENTS_BIN.",
            err=True,
        )
        raise typer.Exit(code=2)

    if state is None:
        # A lead reads its own board, so the role it already holds is the
        # scope it means. An unreadable, terminal, or unpromoted reading
        # resolves nothing and the board stays fleet-wide, exactly as before.
        from fno.agents.role import calling_agent_row
        from fno.lead.state import resolve_lead_manifest_path

        caller = calling_agent_row()
        session_id = (
            getattr(caller, "harness_session_id", None)
            or getattr(caller, "cc_session_id", None)
            or ""
        )
        if session_id:
            resolved, _ = resolve_lead_manifest_path(
                session_id, getattr(caller, "harness", None)
            )
            if resolved is not None:
                state = resolved

    cmd = [str(binary), "board", "--json"]
    if budget_ms is not None:
        cmd += ["--budget-ms", str(budget_ms)]
    if state is not None:
        cmd += ["--state", str(state)]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    # The board prints a full payload even when queues are unreadable (its exit
    # code carries that); parse the stdout regardless, as the stop gate does.
    try:
        board = json.loads(proc.stdout or "")
    except ValueError as exc:
        detail = (proc.stderr or proc.stdout or "").strip()[:300]
        typer.echo(f"lead: board unreadable (exit {proc.returncode}): {detail or exc}", err=True)
        raise typer.Exit(code=1) from exc
    if as_json:
        typer.echo(json.dumps(board, indent=2))
    else:
        _render(board, max_rows)
    raise typer.Exit(cast(int, board.get("exit_code", 1)))


@lead_app.command("escalate")
def escalate_cmd(
    scope_arg: str = typer.Argument("", help="Role scope for the verdict read."),
    stalled: str = typer.Option(
        "", "--stalled", help="Comma-separated board rows nothing is clearing."
    ),
    reason: str = typer.Option(
        "NoProgress", "--reason", "-R", help="The terminal reason that triggered this."
    ),
) -> None:
    """Escalate to the presiding role or operator with work pending (AC4-HP)."""
    from fno.carveout.core import resolve_carveout_root, resolve_session_id
    from fno.lead.escalate import escalate, mail_presiding_lead, resolve_presiding_lead
    from fno.lead.state import term_state
    from fno.paths import resolve_repo_root

    ids = [part.strip() for part in stalled.split(",") if part.strip()]
    try:
        session_id = resolve_session_id(resolve_repo_root())
    except Exception:  # noqa: BLE001 - an unresolvable session never blocks the ask
        session_id = None
    # The caller's own liveness, read not asserted: unknown reads as dead
    # in the renderer, with the reason named.
    try:
        state = term_state(session_id=session_id)
        live, unknown_reason = state.live, state.unknown_reason
    except Exception as exc:  # noqa: BLE001 - escalation must still fire
        live, unknown_reason = None, f"term_state unreadable: {exc}"
    # The term verdict: the native owner resolves everything; a
    # failed read still records, "verdict unreadable".
    summary = None
    verdict_scope = None
    try:
        from fno.lead.history import HistoryUnreadable, verdict_read
        from fno.paths import event_journals

        code, out, err = verdict_read(event_journals(), scope_arg or None, as_json=True)
        if code != 0:
            raise HistoryUnreadable(f"native exit {code}: {(err or out).strip()[:200]}")
        payload = json.loads(out or "{}")
        summary, verdict_scope = payload.get("summary"), payload.get("scope")
    except Exception as exc:  # noqa: BLE001 - escalation never goes silent
        summary = f"unreadable {exc}"
    presiding = resolve_presiding_lead(session_id)
    try:
        outcome, qid = escalate(
            ids,
            reason=reason,
            root=resolve_carveout_root(),
            session_id=session_id,
            cwd=Path.cwd(),
            live=live,
            unknown_reason=unknown_reason,
            verdict=summary,
            scope=verdict_scope or None,
        )
    except Exception as exc:  # noqa: BLE001 - named, never swallowed
        typer.echo(f"lead: escalation failed: {exc}", err=True)
        raise typer.Exit(1) from exc
    holder = presiding["holder"] if presiding is not None else None
    mailed = holder is not None and mail_presiding_lead(holder, ids, reason)
    target = f"lead:{holder}" if mailed else f"operator:{qid}"
    note = f", mailed presiding lead {holder}" if mailed else ""
    typer.echo(f"lead: {outcome}{note} {qid}", err=True)
    typer.echo(target)


@lead_app.command("drain")
def drain_cmd(
    scope: str = typer.Argument(..., help="The role scope to count."),
) -> None:
    """Is the role drained: how many scope nodes are not done or superseded.

    The walk's termination reads this, never a queue: a row with a driver
    leaves the actionable board while its work is unshipped, so the board
    answers assignment and this answers delivery. An unreadable graph exits
    nonzero on purpose - a walk that cannot see the scope must not read the
    failure as drained.
    """
    from fno.graph.store import GraphCorruptError, GraphUnreadableError, StoreUnavailable
    from fno.lead import drain_cache
    from fno.lead.scope import scope_undelivered
    from fno.tracker import active_backend_name
    from fno.tracker.metadata import ExternalMetadataUnavailable
    from fno.tracker.metadata import _graph_store_path, read_entries

    path = _graph_store_path()
    ident = drain_cache.graph_ident(path) if active_backend_name() == "graph" else None
    if ident is not None:
        cached = drain_cache.load(scope, ident)
        if cached is not None:
            typer.echo(
                json.dumps({"scope": scope, "undelivered": cached, "cached": True})
            )
            return
    try:
        entries = read_entries("lead drain", strict=True)
        undelivered = scope_undelivered(scope, entries)
    except (
        ExternalMetadataUnavailable, GraphCorruptError, GraphUnreadableError, StoreUnavailable, ValueError
    ) as exc:
        typer.echo(f"lead: drain for {scope!r} unreadable: {exc}", err=True)
        raise typer.Exit(1) from exc
    # A moved post-read identity describes bytes the count never saw - except
    # first contact: the read itself materializes the db, which moves the
    # identity from the seed-file stat to the store version with no write in
    # between, so that flip is the count's own key.
    post = drain_cache.graph_ident(path)
    materialized = (
        ident is not None and ident[0] == "json" and post is not None and post[0] == "sqlite"
    )
    if post is not None and (post == ident or materialized):
        drain_cache.store(scope, post, undelivered)
    typer.echo(json.dumps({"scope": scope, "undelivered": undelivered}))


def _render(board: dict, max_rows: int) -> None:
    typer.echo(f"actionable: {board['actionable']}")
    for q in board["queues"]:
        if q["status"] == "unreadable":
            typer.echo(f"  {q['name']:<20} UNREADABLE  {q['error']}")
        elif q["status"] == "over_budget":
            typer.echo(f"  {q['name']:<20} NOT READ    {q['error']}")
        else:
            mark = "*" if q["actionable"] and q["count"] else " "
            verb = f"  -> {q['verb']}" if q.get("verb") else ""
            note = f"  ({q['note']})" if q["note"] and q["count"] else ""
            typer.echo(f" {mark}{q['name']:<20} {q['count']}{verb}{note}")
            for row in q["rows"][:max_rows]:
                typer.echo(f"      {row}")
            hidden = max(0, len(q["rows"]) - max_rows)
            if hidden:
                typer.echo(f"      ... {hidden} more not shown")
        typer.echo(f"      source: {q['source']}")
    for warning in board["warnings"]:
        typer.echo(f"warning: {warning}", err=True)


agents_lead_app = typer.Typer(
    name="lead",
    help="The lead session manifest and escalation controls.",
    no_args_is_help=True,
)
agents_lead_app.command("init")(init_cmd)
agents_lead_app.command("done")(done_cmd)
agents_lead_app.command("cancel")(cancel_cmd)
agents_lead_app.command("escalate")(escalate_cmd)
agents_lead_app.command("drain")(drain_cmd)
agents_lead_app.command("shape")(shape_cmd)
agents_lead_app.command(
    "term",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)(term_cmd)
# The stop hooks resolve the role manifest through this hidden verb: the
# deprecated `fno lead` spelling once missed the verb_moves fold and burned
# every stop's unavailable-retries. The hooks now name `agents lead` directly.
agents_lead_app.command("manifest-path", hidden=True)(manifest_path_cmd)
# Here only, like the faq typer: the retired bare `fno lead` menu stays capped.
agents_lead_app.command("history")(history_cmd)
agents_lead_app.command(
    "checkin",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)(checkin_cmd)
agents_lead_app.command("verdict")(verdict_cmd)
agents_lead_app.command("ledger")(ledger_cmd)
agents_lead_app.add_typer(faq_app, name="faq")


def main() -> None:  # pragma: no cover - console-script shim
    sys.exit(lead_app())
