"""`fno inbox outstanding` - read what is waiting on a human; ask and clear questions.

Machine-first, mirroring `fno backlog carveout`: stdout carries the value,
guidance goes to stderr, exit codes are predictable (0 ok / 1 failure).
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import List

import typer

from fno.king.lane import read_lane
from fno.outstanding.core import OutstandingError, collect, render
from fno.outstanding.mine import mine_app
from fno.user import display_name

outstanding_app = typer.Typer(
    help=(
        "What is outstanding FOR YOU: unharvested carve-outs, open operator "
        "questions, and undecided fu- captures folded across every project. "
        "Read-only by default; `ask` records a question so it survives the "
        "next turn, `clear` closes it once answered."
    ),
)
outstanding_app.add_typer(mine_app, name="mine")


def _storage_root() -> Path:
    """The canonical root that owns both stores (shared project state)."""
    from fno.outstanding.core import _canonical_checkout_for

    env_root = os.environ.get("FNO_REPO_ROOT")
    if env_root:
        return Path(env_root).resolve()

    cwd = Path.cwd()
    marker = cwd / ".git"
    if marker.is_dir() or marker.is_file():
        canonical = _canonical_checkout_for(cwd)
        if canonical != cwd or marker.is_dir():
            return canonical

    from fno.paths import resolve_repo_root

    root = resolve_repo_root()
    canonical = _canonical_checkout_for(root)
    if canonical != root or (root / ".git").is_dir():
        return canonical

    from fno.carveout.core import resolve_carveout_root

    return resolve_carveout_root()


def _session_id() -> "str | None":
    from fno.paths import resolve_repo_root

    try:
        lines = (resolve_repo_root() / ".fno" / "target-state.md").read_text(
            encoding="utf-8"
        ).splitlines()
    except Exception:  # noqa: BLE001 - an unresolvable session is not an error here
        lines = []
    fields: dict[str, str] = {}
    if lines and lines[0].strip() == "---":
        closed = False
        for line in lines[1:]:
            stripped = line.strip()
            if stripped == "---":
                closed = True
                break
            for key in ("fno_id", "session_id"):
                prefix = f"{key}:"
                if stripped.startswith(prefix):
                    value = stripped[len(prefix) :].strip().strip("\"'")
                    if value and value != "null":
                        fields.setdefault(key, value)
        if closed:
            session_id = fields.get("fno_id") or fields.get("session_id")
            if session_id:
                return session_id
    env_session = os.environ.get("CLAUDECODE_SESSION_ID")
    return env_session.strip() if env_session and env_session.strip() else None


def _is_crowned() -> bool:
    """True only when this session's registry row carries a crown.

    ``FNO_AGENT_SELF`` (tier 1 of ``resolve_self``) answers with no
    session-id dependency, which is what lets a spawned king resolve this at
    session start before the harness has written anything else. Any
    exception degrades to "not crowned" and never fails the command:
    `fno outstanding` runs on the session-start path under a bound, and
    degrading this way costs one missing action line, not a missing lane.
    """
    try:
        from fno.agents.registry import RegistryVersionError, load_registry
        from fno.agents.whoami import resolve_self
        from fno.harness_identity import resolve_harness_identity

        registry: list = []
        try:
            registry = load_registry()
        except RegistryVersionError:
            pass
        # READ-ONLY (LD5): resolves the crown holder to display it.
        ident = resolve_harness_identity()
        result = resolve_self(
            env=os.environ,
            registry=registry,
            session_uuid=ident.session_id,
            harness=ident.harness,
        )
    except Exception:  # noqa: BLE001 - an unresolved crown is not an error here
        return False
    return result.crown is not None


@outstanding_app.callback(invoke_without_command=True)
def report(
    ctx: typer.Context,
    as_json: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit one JSON object carrying both legs instead of the human block.",
    ),
) -> None:
    """Report unharvested carve-outs and open operator questions."""
    if ctx.invoked_subcommand is not None:
        return

    root = _storage_root()
    try:
        outstanding = collect(root, lane=read_lane())
    except OutstandingError as exc:
        # A present-but-unreadable store is a FAILED read, never "nothing
        # outstanding": reporting an empty queue here would tell the operator
        # the pile is clear when it is merely unreadable.
        typer.echo(f"outstanding: failed to read: {exc}", err=True)
        raise typer.Exit(1)

    if as_json:
        typer.echo(json.dumps(outstanding.as_dict(), separators=(",", ":")))
        return

    block = render(outstanding, session_id=_session_id(), crowned=_is_crowned())
    if block:
        typer.echo(block, nl=False)


def _law_rows() -> "list[dict]":
    """Live law rows the Rust intake matches against (d-0fa92eb9: no agent
    asks a question the operator already settled)."""
    from fno.decide import list_decisions

    _, rows, _damaged = list_decisions(None, limit=None, lane="law", state="live")
    return [
        {key: row.get(key) for key in ("decision_id", "subject", "decision", "ts")}
        for row in rows
    ]


@outstanding_app.command("ask")
def ask(
    question: str | None = typer.Argument(
        None, help="What you need the operator to decide or answer."
    ),
    question_file: Path | None = typer.Option(
        None,
        "--question-file",
        help="Read the question from a file ('-' = stdin); QUESTION_CAP truncation still applies.",
    ),
    ask: str = typer.Option(None, "--ask", help="One action that closes the question."),
    option: List[str] = typer.Option(
        [], "--option", help="A choice the operator can make; repeatable."
    ),
    blocks: List[str] = typer.Option(
        [], "--blocks", help="A backlog node blocked by the question; repeatable."
    ),
    node: str = typer.Option(
        None, "--node", help="Backlog node the question is about, when there is one."
    ),
    subject: str = typer.Option(
        None,
        "--subject",
        help="The decision subject this question is about; live law on it refuses the ask.",
    ),
) -> None:
    """Record a question for the operator so it survives the next turn.

    The leg is the Rust `question-intake` transport (law refusal, context
    parse, writes, receipt); this side keeps identity and the law-row read.
    """
    from fno.claims.self_identity import resolve_self_identity
    from fno.harness_identity import canonical_handle
    from fno.paths import project_log, questions_jsonl
    from fno.rust_binary import verb_call
    from fno.text_or_file import read_text_arg

    question = read_text_arg(question, question_file, what="the question")
    if not question:
        typer.echo(
            "error: provide the question - positionally or --question-file", err=True
        )
        raise typer.Exit(code=2)

    ident = resolve_self_identity()
    try:
        laws = _law_rows()
    except Exception as exc:  # noqa: BLE001 - fail open: record the question
        laws = []
        typer.echo(f"outstanding: live-law lookup failed ({exc}); recording anyway", err=True)
    asker = canonical_handle(ident.session_id) if ident.session_id and ident.harness else None
    answer = verb_call(
        "question-intake",
        {
            "question": question, "ask": ask, "options": option, "blocks": blocks,
            "node": node, "subject": subject, "session_id": _session_id(),
            "cwd": str(Path.cwd()), "asker": asker, "laws": laws,
            "storage_root": str(_storage_root()),
            "index_path": str(questions_jsonl()),
            "journal_path": str(project_log("events.jsonl")),
            "display_name": display_name(),
        },
    )
    # Every human word rides the answer's lines, composed Rust-side.
    for line in answer.get("lines") or ():
        typer.echo(line, err=True)
    if (code := answer.get("exit_code")) and code != 0:
        raise typer.Exit(code)
    typer.echo(answer["qid"])


@outstanding_app.command("clear")
def clear(
    question_ids: List[str] = typer.Argument(
        ..., help="Question id(s) to close (e.g. q-ab12cd34)."
    ),
    answer: str = typer.Option(
        None, "--answer", help="The answer, recorded so the decision outlives the session."
    ),
    authority: str = typer.Option(
        None,
        "--authority",
        help="How the answerer was entitled to answer: 'operator', 'crown', "
        "'agent', or 'beastmode'. Omit to claim none.",
    ),
    origin: str = typer.Option(
        None,
        "--origin",
        hidden=True,
        help="Carried mail origin used by the machine law gate.",
    ),
) -> None:
    """Close one or more open questions. Idempotent."""
    from fno import decide, events, paths, rust_binary
    from fno.outstanding.deliver import deliver_answer
    from types import SimpleNamespace

    if authority is not None and authority not in decide.AUTHORITY_SOURCES:
        typer.echo(f"outstanding: --authority '{authority}' is not one of {', '.join(decide.AUTHORITY_SOURCES)}. Nothing was closed.", err=True)
        raise typer.Exit(2)
    if answer is not None and len(answer) > events.QUESTION_CAP:
        typer.echo(f"outstanding: recorded truncated: the answer is {len(answer)} characters, the event stores {events.QUESTION_CAP}.", err=True)
    provenance = {}
    if answer is not None:
        try:
            origin = decide.enforce_origin_floor(origin)
            provenance = decide._resolve_decider(None, authority, origin=origin)._asdict()
            provenance["origin"] = origin
        except (decide.UnknownOriginError, decide.RefusedAuthorityError, decide.UnattributedAuthorityError) as exc:
            remedy = "" if isinstance(exc, decide.UnknownOriginError) else "This process has no session identity and no terminal, so it is not an agent and has no chat to compose in. Run it from an attended terminal, or from a real agent session, and answer again." if isinstance(exc, decide.UnattributedAuthorityError) else f"The refusal is about the claimed --origin {exc.origin!r}, not about who you are. Drop --origin (or drop --authority operator) and answer again." if exc.origin is not None else "An agent answers as agent or crown. The superuser lane is not an agent's to claim. Drop --authority operator and answer again."
            typer.echo(f"outstanding: refused: {exc}. Nothing was closed; all {len(question_ids)} question(s) stay open." + (f"\n{remedy}" if remedy else ""), err=True)
            raise typer.Exit(3)
    result = rust_binary.verb_call("question-clear", {"ids": question_ids, "answer": answer, "cap": events.QUESTION_CAP, "provenance": provenance, "closed_by": _session_id(), "journal_path": str(paths.project_log("events.jsonl")), "index_path": str(paths.questions_jsonl()), "decisions_path": str(paths.decisions_jsonl()), "graph": str(paths.graph_json()), "repo_root": str(Path.cwd())})
    typer.echo("\n".join(result.get("lines") or ()))
    for item in result.get("closed") or ():
        if item.get("node") and item.get("decision_event"):
            try:
                projected = decide._project(item["decision_event"])
            except (Exception, SystemExit) as exc:
                projected = (None, f"the graph projection failed ({exc!r})")
            if projected[0] is None:
                typer.echo(f"outstanding: {item['qid']} decision {item['decision_id']} is recorded but not on node {item['node']}: {projected[1]}", err=True)
    for item in result.get("deliveries") or ():
        typer.echo(deliver_answer(SimpleNamespace(id=item["qid"], question=item["question"], asker=item["asker"], session_id=item.get("session_id")), answer, item["decision_id"]), err=True)
    raise typer.Exit(result["exit_code"])


@outstanding_app.command("reindex")
def reindex() -> None:
    """Backfill the machine-wide question index from every project journal."""
    from fno.outstanding.core import reindex_questions

    try:
        result = reindex_questions(_storage_root())
    except OutstandingError as exc:
        typer.echo(f"outstanding: failed to read: {exc}", err=True)
        raise typer.Exit(1)
    except Exception as exc:  # noqa: BLE001 - a partial backfill is not success
        typer.echo(f"outstanding: failed to reindex questions: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(
        f"{result['added']} event(s) added; {result['already']} already indexed; "
        f"{result['total']} total"
    )
