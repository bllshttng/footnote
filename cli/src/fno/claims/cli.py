"""fno claim - Typer surface for the six work-claim verbs.

Exit codes:
    0  success
    1  ClaimHeldByOther (caller should retry later)
    2  validation / input error
    3  ClaimCorrupted or ClaimGoneAway (race during operation)
    4  HolderMismatch (release/refresh wrong holder)

The structured output uses --json on each verb. Without --json, output is a
human-friendly summary on stdout; errors always go to stderr.
"""
from __future__ import annotations

import json
import re
from typing import Optional

import typer

from .core import (
    ClaimCorrupted,
    ClaimGoneAway,
    ClaimHeldByOther,
    ClaimValidationError,
    HolderMismatch,
    acquire_claim,
    claim_status,
    force_release_claim,
    list_claims,
    refresh_claim,
    release_claim,
)


cli = typer.Typer(
    name="claim",
    help="Work-claim coordination primitive",
    no_args_is_help=True,
)


_TTL_PATTERN = re.compile(r"^\s*(\d+)\s*([smh]?)\s*$", re.IGNORECASE)


def _parse_ttl(value: Optional[str]) -> Optional[int]:
    """Convert "1h" / "30m" / "3600s" / "5000" into milliseconds.

    Empty string and None return None (caller decides default).
    Plain digits are interpreted as seconds, matching ``sleep`` convention.
    """
    if value is None or value == "":
        return None
    m = _TTL_PATTERN.match(value)
    if not m:
        raise typer.BadParameter(f"invalid TTL format: {value!r} (use '30m', '1h', '3600s')")
    n = int(m.group(1))
    unit = m.group(2).lower()
    if unit == "s" or unit == "":
        return n * 1000
    if unit == "m":
        return n * 60_000
    if unit == "h":
        return n * 3_600_000
    raise typer.BadParameter(f"unknown TTL unit: {unit!r}")


def _parse_metadata(value: str) -> dict:
    if not value:
        return {}
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError as exc:
        raise typer.BadParameter(f"--metadata is not valid JSON: {exc}")
    if not isinstance(parsed, dict):
        raise typer.BadParameter("--metadata must be a JSON object")
    return parsed


def _node_aware_root(key: str):
    """Resolve the claims root for a key (delegates to the shared helper).

    Global-id kinds (``node:``/``dispatch:``/``reconcile:``) route to the global
    ``~/.fno/claims`` so operator commands work without the env var
    (ab-fcf9cec5); repo-local keys keep the cwd/env default. See
    :func:`fno.claims.io.claims_root_for` for the single source of truth.
    """
    from .io import claims_root_for

    return claims_root_for(key)


@cli.command()
def acquire(
    key: str = typer.Argument(..., help="Lock key, e.g. node:ab-1234abcd"),
    holder: str = typer.Option(..., "--holder", help="Symbolic owner string"),
    reason: str = typer.Option("", "--reason", "-R", help="Optional rationale recorded in audit"),
    ttl: str = typer.Option("", "--ttl", help="TTL expression like 30m, 1h, 3600s (omit => PID-liveness)"),
    metadata: str = typer.Option("{}", "--metadata", help="JSON object passed verbatim"),
    pid: Optional[int] = typer.Option(
        None,
        "--pid",
        help=(
            "PID-liveness anchor (omit => nearest agent session, else this process's "
            "PID). Pin the claim to a LONG-LIVED owner (e.g. a stream worker) rather "
            "than the transient acquiring process so PID-liveness does not mark it "
            "stale instantly."
        ),
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON to stdout"),
    verbose: bool = typer.Option(False, "--verbose", help="More detail on stderr"),
) -> None:
    """Acquire a claim on KEY for HOLDER. Idempotent re-acquire if HOLDER matches."""
    # ponytail: an omitted --pid used to anchor to the TRANSIENT acquiring process
    # (a one-shot `fno claim acquire` from a shell dies ~1s later, so the claim went
    # instantly STALE -- the footgun). Default instead to the durable session
    # (nearest claude ancestor) when one exists; degrade to the prior os.getpid()
    # default when not (standalone use, no agent session). Reuses the exact walk
    # init-target-state.sh already runs via `fno claim session-pid`.
    if pid is None:
        try:
            from .session_pid import resolve_session_pid
            pid = resolve_session_pid()
        except Exception:
            pid = None  # degrade to acquire_claim's os.getpid() default
    try:
        claim = acquire_claim(
            key=key,
            holder=holder,
            reason=reason or None,
            ttl_ms=_parse_ttl(ttl),
            metadata=_parse_metadata(metadata),
            pid=pid,
            root=_node_aware_root(key),
        )
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)
    except ClaimHeldByOther as exc:
        typer.echo(
            f"claim {key!r} held by {exc.holder} (pid={exc.pid}, host={exc.host})",
            err=True,
        )
        raise typer.Exit(code=1)
    except (ClaimCorrupted, ClaimGoneAway) as exc:
        typer.echo(f"transient error: {exc}", err=True)
        raise typer.Exit(code=3)

    if json_output:
        typer.echo(json.dumps(claim.to_yaml_dict()))
    else:
        typer.echo(f"acquired: {key} (holder={holder}, pid={claim.pid})")


@cli.command()
def release(
    key: str = typer.Argument(...),
    holder: str = typer.Option(..., "--holder"),
    strict: bool = typer.Option(False, "--strict", help="Raise if holder does not match"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Release a claim we own. Silent success if already released."""
    try:
        release_claim(key=key, holder=holder, strict=strict, root=_node_aware_root(key))
    except HolderMismatch as exc:
        typer.echo(f"holder mismatch: {exc}", err=True)
        raise typer.Exit(code=4)
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)
    except (ClaimCorrupted, ClaimGoneAway) as exc:
        typer.echo(f"transient error: {exc}", err=True)
        raise typer.Exit(code=3)

    if json_output:
        typer.echo(json.dumps({"key": key, "released": True}))
    else:
        typer.echo(f"released: {key}")


@cli.command()
def refresh(
    key: str = typer.Argument(...),
    holder: str = typer.Option(..., "--holder"),
    ttl: str = typer.Option("", "--ttl"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Extend a TTL claim's expires_at. No-op for PID-liveness claims."""
    try:
        result = refresh_claim(key=key, holder=holder, ttl_ms=_parse_ttl(ttl), root=_node_aware_root(key))
    except HolderMismatch as exc:
        typer.echo(f"holder mismatch: {exc}", err=True)
        raise typer.Exit(code=4)
    except ClaimGoneAway as exc:
        typer.echo(f"claim missing: {exc}", err=True)
        raise typer.Exit(code=3)
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)
    except ClaimCorrupted as exc:
        typer.echo(f"corrupted claim: {exc}", err=True)
        raise typer.Exit(code=3)

    if result is None:
        if json_output:
            typer.echo(json.dumps({"key": key, "refreshed": False, "reason": "pid_liveness"}))
        else:
            typer.echo(f"no-op for PID-liveness claim: {key}")
        return

    if json_output:
        typer.echo(json.dumps(result.to_yaml_dict()))
    else:
        typer.echo(f"refreshed: {key} (new expires_at={result.expires_at})")


@cli.command()
def status(
    key: str = typer.Argument(...),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Inspect a single claim. Exit code reflects state for scripting."""
    info = claim_status(key=key, root=_node_aware_root(key))
    if json_output:
        typer.echo(json.dumps(info))
    else:
        typer.echo(json.dumps(info, indent=2))


@cli.command(name="list")
def list_cmd(
    prefix: str = typer.Option("", "--prefix", help="Filter keys starting with this prefix"),
    include_stale: bool = typer.Option(False, "--include-stale"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Enumerate claims under the claims directory."""
    results = list_claims(
        prefix=prefix or None,
        include_stale=include_stale,
        root=_node_aware_root(prefix),
    )
    if json_output:
        typer.echo(json.dumps(results))
    else:
        if not results:
            typer.echo("no claims")
            return
        for r in results:
            typer.echo(
                f"{r['state']:9} {r['key']:32} holder={r.get('holder', '-')} "
                f"pid={r.get('pid', '-')} host={r.get('host', '-')}"
            )


@cli.command(name="session-pid")
def session_pid(
    from_pid: Optional[int] = typer.Option(
        None,
        "--from-pid",
        help="Start the ancestor walk here (default: this process's parent).",
    ),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Resolve the durable session pid (nearest ``claude`` ancestor) for the
    hybrid liveness pid-arm. Prints the pid on stdout, or nothing when
    uncapturable (the caller degrades to TTL-only liveness). Always exit 0 -
    a missing pid is a safe degrade, not an error (ab-cc5553f2)."""
    from .session_pid import resolve_session_pid

    pid = resolve_session_pid(from_pid=from_pid)
    if json_output:
        typer.echo(json.dumps({"session_pid": pid}))
    elif pid is not None:
        typer.echo(str(pid))
    # else: emit nothing on stdout so `$(fno claim session-pid)` is empty.


@cli.command(name="force-release")
def force_release(
    key: str = typer.Argument(...),
    reason: str = typer.Option(..., "--reason", "-R", help="Required: audit rationale"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Administratively drop a claim regardless of owner. Archived to .expired/."""
    try:
        force_release_claim(key=key, reason=reason, root=_node_aware_root(key))
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)

    if json_output:
        typer.echo(json.dumps({"key": key, "force_released": True, "reason": reason}))
    else:
        typer.echo(f"force-released: {key}")


__all__ = ["cli"]
