"""``fno agents king faq add`` - the recipe-becomes-verb for a king's FAQ.

Three kings hand-wrote entries in the king FAQ directory before this verb
existed: a question a reader types, the answer that survived contact, a
specimen, and the change that retires the entry. Nothing kept that convention
- no verb wrote it, so it stayed three files. This module is the verb.

An entry with no exit is refused: a workaround with no plan to stop needing
it is not durable, it is a habit. The hook side (``king-postcompact-reinject.sh``)
reads these entries back for a matching crown scope; nothing here writes to
that hook or to the registry - the two are unaware of each other beyond the
directory they share.
"""
from __future__ import annotations

import secrets
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import typer

faq_app = typer.Typer(
    name="faq",
    help="The king FAQ: durable answers a crowned session leaves for the next one.",
    no_args_is_help=True,
)


#: Per-crown cap on how many FAQ entries the reinject hook re-teaches. The
#: hook fires on every compaction, so an unbounded scope makes the context
#: cost grow with how long a king has reigned rather than staying flat.
DEFAULT_MAX_ENTRIES = 20


def _entry_scope(text: str) -> str:
    """The ``scope:`` frontmatter value, or "" if absent or unparseable."""
    if not text.startswith("---\n"):
        return ""
    end = text.find("\n---", 4)
    if end == -1:
        return ""
    for line in text[4:end].splitlines():
        if line.startswith("scope:"):
            return line.split(":", 1)[1].strip()
    return ""


def entries_for_scope(
    scope: str, *, faqs_dir: Optional[Path] = None, max_entries: int = DEFAULT_MAX_ENTRIES
) -> list[str]:
    """This crown scope's FAQ entries, oldest first, capped at ``max_entries``.

    Degrade-only: a missing or unreadable directory yields an empty list,
    never an exception - the reinject hook that calls this must never block
    a compaction over a broken FAQ store.
    """
    from fno.paths import king_faqs_dir

    directory = faqs_dir if faqs_dir is not None else king_faqs_dir()
    try:
        paths = sorted(directory.glob("king-*.md"))
    except OSError:
        return []
    matched = []
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        if _entry_scope(text) == scope:
            matched.append(text.strip())
    return matched[-max_entries:]


def _slug(value: str, *, max_len: int = 24) -> str:
    out = []
    prev_dash = False
    for ch in value.lower():
        if ch.isalnum():
            out.append(ch)
            prev_dash = False
        elif not prev_dash:
            out.append("-")
            prev_dash = True
    slug = "".join(out).strip("-")
    return slug[:max_len] or "faq"


def write_faq_entry(
    *,
    question: str,
    answer: str,
    specimen: str,
    exit_: str,
    scope: str,
    king: str,
    session: str,
    faqs_dir: Optional[Path] = None,
) -> Path:
    """Write one FAQ entry and return its path.

    The caller has already refused a missing ``exit_``; this function trusts
    its arguments and only handles placement and formatting.
    """
    from fno.paths import king_faqs_dir

    directory = faqs_dir if faqs_dir is not None else king_faqs_dir()
    directory.mkdir(parents=True, exist_ok=True)
    created = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    path = directory / f"king-{_slug(scope)}-{secrets.token_hex(4)}.md"
    body = (
        "---\n"
        f"created: {created}\n"
        f"king: {king}\n"
        f"session: {session}\n"
        f"scope: {scope}\n"
        "---\n\n"
        f"# {question}\n\n"
        "## Answer\n\n"
        f"{answer}\n\n"
        "## Specimen\n\n"
        f"{specimen}\n\n"
        "## Exit\n\n"
        f"{exit_}\n"
    )
    path.write_text(body, encoding="utf-8")
    return path


@faq_app.command("add")
def add_cmd(
    question: str = typer.Option(..., "--question", help="The question a reader types."),
    answer: str = typer.Option(..., "--answer", help="The answer that survived contact."),
    specimen: str = typer.Option(..., "--specimen", help="Where this happened: a node or PR, and a date."),
    exit_: Optional[str] = typer.Option(
        None,
        "--exit",
        help="The change that will retire this entry. Required.",
    ),
    scope: Optional[str] = typer.Option(
        None,
        "--scope",
        help="Crown scope this entry belongs to. Defaults to this session's own held crown.",
    ),
) -> None:
    """Write one entry into the king FAQ directory and print its path."""
    if not exit_ or not exit_.strip():
        typer.echo(
            "fno agents king faq add: refused, no --exit. An entry with no exit "
            "is a workaround with no plan to stop needing it - name the change "
            "that will retire this entry.",
            err=True,
        )
        raise typer.Exit(code=2)

    from fno.agents.crown import current_crown
    from fno.agents.self_stamp import resolve_self_handle, resolve_self_session_id

    resolved_scope = scope
    if not resolved_scope:
        crown = current_crown()
        resolved_scope = crown["scope"] if crown else None
    if not resolved_scope:
        typer.echo(
            "fno agents king faq add: refused, no crown held and no --scope given. "
            "Pass --scope explicitly.",
            err=True,
        )
        raise typer.Exit(code=2)

    path = write_faq_entry(
        question=question,
        answer=answer,
        specimen=specimen,
        exit_=exit_,
        scope=resolved_scope,
        king=resolve_self_handle() or "unknown",
        session=resolve_self_session_id() or "unknown",
    )
    typer.echo(str(path))


@faq_app.command("list")
def list_cmd(
    scope: str = typer.Option(..., "--scope", help="Crown scope to list entries for."),
    max_entries: int = typer.Option(
        DEFAULT_MAX_ENTRIES, "--max-entries", help="Cap on entries emitted."
    ),
) -> None:
    """Print this crown scope's FAQ entries, each separated by a rule.

    Silent (empty output, exit 0) when the directory is missing, unreadable,
    or has no entry for this scope - the reinject hook that shells out to this
    reads an empty result the same way it reads a missing directory.
    """
    for entry in entries_for_scope(scope, max_entries=max_entries):
        typer.echo(entry)
        typer.echo("---")
