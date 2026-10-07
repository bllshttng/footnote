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

from fno.lead.lane import read_lane
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


def _is_promoted() -> bool:
    """True only when this session's registry row carries a role.

    ``FNO_AGENT_SELF`` (tier 1 of ``resolve_self``) answers with no
    session-id dependency, which is what lets a spawned lead resolve this at
    session start before the harness has written anything else. Any
    exception degrades to "not promoted" and never fails the command:
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
        # READ-ONLY (LD5): resolves the role holder to display it.
        ident = resolve_harness_identity()
        result = resolve_self(
            env=os.environ,
            registry=registry,
            session_uuid=ident.session_id,
            harness=ident.harness,
        )
    except Exception:  # noqa: BLE001 - an unresolved role is not an error here
        return False
    return result.role is not None


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

    block = render(outstanding, session_id=_session_id(), promoted=_is_promoted())
    if block:
        typer.echo(block, nl=False)


def _law_rows() -> "list[dict]":
    """Live law rows the Rust intake matches against (d-0fa92eb9: no agent
    asks a question the operator already settled). ``lane`` is the verdict of
    the same authority rule `backlog decide-retract` enforces, so the ask
    gate reads the retraction constraint instead of re-deriving it."""
    from fno.rust_binary import call_front_json

    answer = call_front_json(
        {"mode": "decisions", "argv": ["--lane", "law", "--state", "live", "--json"]}
    )
    rows = answer.get("decisions") or []
    return [
        {
            key: row.get(key)
            for key in ("decision_id", "subject", "decision", "ts", "lane")
        }
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
    from datetime import datetime, timezone

    from fno.claims.self_identity import resolve_self_identity
    from fno.harness_identity import canonical_handle
    from fno.paths import project_log, questions_jsonl
    from fno.rust_binary import VerbUnavailable, verb_call
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
    payload = {
        "question": question, "ask": ask, "options": option, "blocks": blocks,
        "node": node, "subject": subject, "session_id": _session_id(),
        "cwd": str(Path.cwd()), "asker": asker, "laws": laws,
        "storage_root": str(_storage_root()),
        "index_path": str(questions_jsonl()),
        "journal_path": str(project_log("events.jsonl")),
        "display_name": display_name(),
    }
    try:
        answer = verb_call("question-intake", payload)
    except VerbUnavailable:
        # No native door on this install: record the question in the project
        # journal in the door's own envelope shape. A missing binary never
        # loses an operator question.
        import secrets

        qid = f"q-{secrets.token_hex(4)}"
        event = {
            "ts": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
            "type": "operator_question",
            "source": "target",
            "data": {
                "question_id": qid,
                "question": question,
                "ask": ask,
                "options": option,
                "blocks": blocks,
                "node": node,
                "subject": subject,
                "session_id": payload["session_id"],
                "cwd": payload["cwd"],
                "asker": asker,
                "display_name": payload["display_name"],
            },
        }
        journal = project_log("events.jsonl")
        journal.parent.mkdir(parents=True, exist_ok=True)
        with journal.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(event) + "\n")
        typer.echo(
            f"outstanding: the question-intake door is unavailable on this install; "
            f"recorded {qid} in the project journal. NOT VISIBLE: without the door's "
            f"readiness gate this row can land not-ready and the mux will not show it "
            f"(unknowns, reversible, meanwhile, per-option What happens next). "
            f"Fix the fno-agents install and ask again. Clear it once answered: "
            f"fno outstanding clear {qid}",
            err=True,
        )
        typer.echo(qid)
        return
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
        help="How the answerer was entitled to answer: 'operator', 'role', "
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
    from fno import events, paths, rust_binary
    from fno.outstanding.deliver import deliver_answer
    from types import SimpleNamespace

    authority_sources = ("operator", "role", "agent", "beastmode")
    if authority is not None and authority not in authority_sources:
        typer.echo(f"outstanding: --authority '{authority}' is not one of {', '.join(authority_sources)}. Nothing was closed.", err=True)
        raise typer.Exit(2)
    if answer is not None and len(answer) > events.QUESTION_CAP:
        typer.echo(f"outstanding: recorded truncated: the answer is {len(answer)} characters, the event stores {events.QUESTION_CAP}.", err=True)
    provenance = {}
    if answer is not None:
        # One resolver, one law: the native decide door resolves provenance
        # (the deleted Python engine's three states, fail-closed third).
        from fno.rust_binary import VerbUnavailable, call_front_json

        try:
            answer_meta = call_front_json(
                {"mode": "resolve-provenance", "authority": authority, "origin": origin}
            )
        except VerbUnavailable as exc:
            typer.echo(
                f"outstanding: refused: {exc}. Nothing was closed; all "
                f"{len(question_ids)} question(s) stay open.",
                err=True,
            )
            raise typer.Exit(3)
        kind = answer_meta.get("refusal_kind")
        if kind:
            refusal = answer_meta.get("refusal") or "the provenance resolver refused"
            remedy = {
                "unknown-origin": "",
                "unattributed": "This process has no session identity and no terminal, so it is not an agent and has no chat to compose in. Run it from an attended terminal, or from a real agent session, and answer again.",
                "authority": "An agent answers as agent or role. The superuser lane is not an agent's to claim. Drop --authority operator and answer again.",
                "origin-authority": f"The refusal is about the claimed --origin {answer_meta.get('origin')!r}, not about who you are. Drop --origin (or drop --authority operator) and answer again.",
            }.get(kind, "")
            typer.echo(f"outstanding: refused: {refusal}. Nothing was closed; all {len(question_ids)} question(s) stay open." + (f"\n{remedy}" if remedy else ""), err=True)
            raise typer.Exit(3)
        provenance = answer_meta
    result = rust_binary.verb_call("question-clear", {"ids": question_ids, "answer": answer, "cap": events.QUESTION_CAP, "provenance": provenance, "closed_by": _session_id(), "journal_path": str(paths.project_log("events.jsonl")), "index_path": str(paths.questions_jsonl()), "decisions_path": str(paths.decisions_jsonl()), "graph": str(paths.graph_json()), "repo_root": str(Path.cwd())})
    typer.echo("\n".join(result.get("lines") or ()))
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
