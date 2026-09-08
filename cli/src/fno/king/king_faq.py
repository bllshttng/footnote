"""``fno agents king faq add`` / ``list`` - the king FAQ recipe becomes a verb."""
from __future__ import annotations

import secrets
from datetime import datetime, timezone
from pathlib import Path

import typer

from fno.agents.naming import slug_component

faq_app = typer.Typer(name="faq", help="Durable answers a crowned session leaves behind.", no_args_is_help=True)

#: Cap on entries the reinject hook re-teaches per crown, so context cost stays flat.
DEFAULT_MAX_ENTRIES = 20


def entries_for_scope(scope: str, *, faqs_dir: Path | None = None, max_entries: int = DEFAULT_MAX_ENTRIES) -> list[str]:
    """This scope's FAQ entries, oldest (by mtime) first, capped. Degrades to []."""
    from fno.paths import king_faqs_dir
    from fno.plan._stamp import parse_frontmatter

    directory = faqs_dir if faqs_dir is not None else king_faqs_dir()
    try:
        paths = sorted(directory.glob("king-*.md"), key=lambda p: p.stat().st_mtime)
    except OSError:
        return []
    matched = []
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
            fields, _raw, _rest = parse_frontmatter(text)
        except (OSError, ValueError):
            continue
        if fields.get("scope") == scope:
            matched.append(text.strip())
    return matched[-max_entries:]


def write_faq_entry(
    *, question: str, answer: str, specimen: str, exit_: str, scope: str,
    king: str, session: str, faqs_dir: Path | None = None,
) -> Path:
    """Write one FAQ entry (caller has already refused a missing exit_) and return its path."""
    from fno.paths import king_faqs_dir

    directory = faqs_dir if faqs_dir is not None else king_faqs_dir()
    directory.mkdir(parents=True, exist_ok=True)
    created = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    path = directory / f"king-{slug_component(scope, cap=24) or 'faq'}-{secrets.token_hex(4)}.md"
    path.write_text(
        f"---\ncreated: {created}\nking: {king}\nsession: {session}\nscope: {scope}\n---\n\n"
        f"# {question}\n\n## Answer\n\n{answer}\n\n## Specimen\n\n{specimen}\n\n## Exit\n\n{exit_}\n",
        encoding="utf-8",
    )
    return path


@faq_app.command("add")
def add_cmd(
    question: str = typer.Option(..., "--question"),
    answer: str = typer.Option(..., "--answer"),
    specimen: str = typer.Option(..., "--specimen", help="Where this happened: a node or PR, and a date."),
    exit_: str | None = typer.Option(None, "--exit", help="The change that will retire this entry. Required."),
    scope: str | None = typer.Option(None, "--scope", help="Defaults to this session's own held crown."),
) -> None:
    """Write one entry into the king FAQ directory and print its path."""
    if not exit_ or not exit_.strip():
        typer.echo("refused, no --exit: a workaround with no plan to stop needing it.", err=True)
        raise typer.Exit(2)

    from fno.agents.crown import current_crown
    from fno.agents.self_stamp import resolve_self_handle, resolve_self_session_id
    resolved_scope = scope or ((current_crown() or {}).get("scope"))
    if not resolved_scope:
        typer.echo("refused, no crown held and no --scope given.", err=True)
        raise typer.Exit(2)
    path = write_faq_entry(
        question=question, answer=answer, specimen=specimen, exit_=exit_, scope=resolved_scope,
        king=resolve_self_handle() or "unknown", session=resolve_self_session_id() or "unknown",
    )
    typer.echo(str(path))


@faq_app.command("list")
def list_cmd(
    scope: str = typer.Option(..., "--scope"),
    max_entries: int = typer.Option(DEFAULT_MAX_ENTRIES, "--max-entries"),
) -> None:
    """Print this scope's FAQ entries, each followed by a rule. Silent if none."""
    for entry in entries_for_scope(scope, max_entries=max_entries):
        typer.echo(entry)
        typer.echo("---")
