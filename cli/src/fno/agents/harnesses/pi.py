"""fno.agents.harnesses.pi - pi's two lanes, and the create serialiser.

pi is a DUAL-LANE harness, and which lane a session gets is a choice fno makes
at exec rather than a property of pi:

    rpc lane    ``pi --mode rpc``. Strict JSONL over stdin and stdout, typed
                events out, LF the only record delimiter, an ``id`` correlating
                request to response. This is the DRIVING lane: spawn, ask, and
                mail injection, the last of which rides pi's own ``steer``
                command instead of typed keystrokes.
    pane lane   plain interactive ``pi`` hosted in a mux pane, showing pi's
                real TUI. This is the WATCHING lane, and the only lane where
                ``/login <provider>`` can be run at all.

They are mutually exclusive PER PROCESS, chosen at exec, and never per session.
A TUI opened on a session an rpc driver is holding JOINS it: measured
2026-08-28, the TUI rendered the rpc session's own turns and the session-file
count for that id stayed at ONE. So the pane is a view onto a live rpc session.

**A pi session is the pair (cwd, session_id), never the id alone.** The store is
cwd-scoped, so the same id in two worktrees is two different sessions and a
resume from the canonical checkout cannot see one started in a worktree.

The three things this module exists to get right
------------------------------------------------

**1. Appends are safe. Creates are not.** Joining an existing session
concurrently was measured four times: the second process exits 0 in about five
seconds and its user turn hangs off the holder's last assistant message by
``parentId``, one file and one linear chain. Creating one concurrently is
UNSAFE AND SILENT: four simultaneous creates on one id produced four session
files 49ms apart, every process exiting 0, every process printing "creating a
new session with that id", and every file internally perfect. Then a later
resume of that id picked the OLDEST, wrote no fifth file, emitted no warning,
and left three sessions holding real work unreachable by the only handle fno
has for them. Every component succeeded and the answer was wrong.

So :func:`create_decision` serialises the CREATE DECISION and nothing else. It
does not lock appends, because appends were measured safe and a lock there
would re-introduce a mechanism that was already refuted.

**2. rpc mode exits on stdin EOF, mid-turn, with status 0.** Feeding a prompt
from a file yielded five events and stopped at the user's own ``message_end``;
the assistant never spoke and the exit code still read success.
:class:`PiRpcSession` holds stdin open for the life of the session and settles
on the POSITIVE ``agent_settled`` event. Nothing here reads a clean exit as
proof that a turn completed.

**3. ``--provider openai-codex`` without ``--model`` does not resolve to
gpt-5.5.** It falls through to a Bedrock model and fails with "Token is
expired. To refresh this SSO session run 'aws sso login'", which names AWS and
misdirects completely. Every argv builder below passes both.

Credentials are operator-owned. ``pi auth`` is read-only (three verbs, none of
which writes); only the TUI's ``/login <provider>`` writes one, and that is an
operator action. Never synthesize a credential.
"""
from __future__ import annotations

import json
import os
import subprocess
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator, Optional, Sequence

from fno.agents.dispatch import DispatchAskError

# pi's provider and model for this fleet. Both are always passed: see trap 3 in
# the module docstring. Env overrides so a different subscription needs no
# code change.
PI_DEFAULT_PROVIDER = "openai-codex"
PI_DEFAULT_MODEL = "gpt-5.5"

# The typed event that IS pi's ready signal on the rpc lane. Emitted after the
# full session-level run settles, with no automatic retry, compaction retry, or
# queued continuation left. Asserting this positive marker is the only sound
# completion read on this harness.
SETTLED_EVENT = "agent_settled"

# How long the create claim is held.
#
# The ruling on this node set 30s, justified by measurement: pi reaches
# session-id adoption in 0.64s (that IS the create-decision span), and a full
# create through the first session file took 5.81s, 4.96s and 4.94s across
# three runs. **The claim primitive refuses anything under a minute**
# (`fno.claims.types.MIN_TTL_MS`), so 30s is not available and this is the
# floor rather than a chosen value. Say so rather than rounding silently: a
# reader who finds 60s here and 30s on the node deserves to know which one the
# code could actually take.
#
# The ruling's REASON survives the floor, which is why taking it is safe. 60s
# is about ten times the slowest measured create, and the leak it bounds is a
# crashed create holding one session id unusable for at most a minute.
#
# The scope is the create decision ONLY, never the session lifetime, and the
# two claim modes are why. A PID-liveness claim dies with its holder and is
# reapable. An explicit-TTL claim survives a crash for the whole TTL. A spawn
# lane that forks and exits must use a TTL, because the default anchors
# liveness to a process that goes away, so the TTL path is the one that gets
# used and it is the one that leaks: a long TTL taken for a session lifetime
# makes that id unusable until it expires. Keeping the scope short is what
# keeps the TTL small enough to be harmless.
CREATE_CLAIM_TTL_MS = 60_000


def pi_provider() -> str:
    """The provider fno passes to pi; ``FNO_PI_PROVIDER`` wins."""
    return os.environ.get("FNO_PI_PROVIDER") or PI_DEFAULT_PROVIDER


def pi_model() -> str:
    """The model fno passes to pi; ``FNO_PI_MODEL`` wins."""
    return os.environ.get("FNO_PI_MODEL") or PI_DEFAULT_MODEL


def pi_sessions_root() -> Path:
    """pi's session store root, honouring a relocated ``PI_HOME``."""
    home = os.environ.get("PI_HOME") or os.path.join(
        os.environ.get("HOME", ""), ".pi"
    )
    return Path(home) / "agent" / "sessions"


def encode_cwd(cwd: Path | str) -> str:
    """pi's on-disk encoding of a working directory.

    Every path separator becomes a single ``-``, and the result is fenced with
    ``--`` at both ends. Derived from three live directories rather than from
    pi's source::

        /Users/bb16/code/footnote/footnote  -> --Users-bb16-code-footnote-footnote--
        /private/tmp                        -> --private-tmp--
        /Users/bb16/.claude/jobs/../piprobe -> --Users-bb16-.claude-jobs-..-piprobe--

    A dot inside a component survives unchanged, as ``.claude`` above shows.

    Mirrors ``encode_cwd`` in ``crates/fno-agents/src/pi.rs``; a parity test
    pins the two.
    """
    raw = str(cwd)
    return "--" + raw.lstrip("/").replace("/", "-") + "--"


def session_dir(cwd: Path | str) -> Path:
    """The cwd-scoped directory pi keeps ``cwd``'s sessions in."""
    return pi_sessions_root() / encode_cwd(cwd)


@dataclass(frozen=True)
class SessionLookup:
    """What a lookup of one ``(cwd, session_id)`` pair found on disk.

    ``state`` is one of ``"unknown"``, ``"none"``, ``"one"``, ``"duplicate"``.

    ``"unknown"`` is a first-class outcome and never collapses into ``"none"``.
    Two different facts produce an empty answer and they call for opposite
    actions: a session directory that cannot be read proves nothing, while a
    readable directory holding no file for this id is a real ``"none"`` - and
    still is not proof the session is absent.

    A pi session's file materialises at the FIRST TURN ATTEMPT, not at create.
    A live rpc session held twelve seconds with no prompt sent leaves the
    directory empty. A turn that ATTEMPTS and fails still writes one: a run
    that died on an expired provider token left a five-row file whose assistant
    content is an empty array. So ``"none"`` means "no turn attempted yet",
    never "no session", and the instrument that covers that blind window is
    fno's own claim registry, which records at acquire rather than at output.
    """

    state: str
    directory: Path
    files: tuple[Path, ...] = ()
    reason: str = ""


def lookup_sessions(cwd: Path | str, session_id: str) -> SessionLookup:
    """Read the session files for one ``(cwd, session_id)`` pair, oldest first.

    Ordering is by FILENAME, which carries an ISO-8601 timestamp prefix
    (``<ISO>_<session-id>.jsonl``), so a lexicographic sort is chronological
    and needs no stat call and no parse.

    Ranking by CONTENT is forbidden, and this function deliberately gives a
    caller no means to do it. An empty assistant ``content`` array marks a turn
    that was ATTEMPTED AND FAILED, not an idle or empty session, so preferring
    the fuller file discards the one that errored, which is usually the one a
    human needs to read.
    """
    directory = session_dir(cwd)
    suffix = f"_{session_id}.jsonl"
    try:
        names = sorted(
            entry for entry in os.listdir(directory) if entry.endswith(suffix)
        )
    except OSError as exc:
        return SessionLookup(state="unknown", directory=directory, reason=str(exc))
    files = tuple(directory / name for name in names)
    if not files:
        return SessionLookup(state="none", directory=directory)
    if len(files) == 1:
        return SessionLookup(state="one", directory=directory, files=files)
    return SessionLookup(state="duplicate", directory=directory, files=files)


def duplicate_resume_refusal(
    cwd: Path | str, session_id: str, lookup: SessionLookup
) -> Optional[str]:
    """The refusal a resume owes an ambiguous id, or ``None`` when there is
    nothing ambiguous to refuse.

    It names EVERY session found, with its timestamp, and selects none. Naming
    only the one being resumed is the codex short-id precedent, where a refusal
    that named the victim's own row steered a worker to a wrong conclusion.

    pi's own behaviour is the defect this refuses to inherit: it picks the
    oldest file, prints nothing, and leaves the rest unreachable by this id.

    This is a SECOND defect and it outlives the create fix, because duplicates
    can pre-exist from an earlier run, a crash, or a pi someone ran by hand. No
    claim taken today can retroactively serialise one already on disk.
    """
    if lookup.state != "duplicate":
        return None
    lines = [
        f"pi session id {session_id!r} in {cwd} resolves to {len(lookup.files)} "
        "sessions, so this resume is refused rather than guessing. pi itself "
        "would pick the oldest and say nothing, leaving the others unreachable "
        "by this id. Every one of them, oldest first:"
    ]
    for path in lookup.files:
        stamp = path.name.split("_", 1)[0]
        lines.append(f"  {stamp}  {path}")
    lines.append(
        "None was selected. Do not rank these by content: an empty assistant "
        "content array marks a turn that was attempted and FAILED, so the "
        "emptier file is often the one worth reading. Resume one by its file "
        "path with `pi --session <path>`."
    )
    return "\n".join(lines)


def create_claim_key(cwd: Path | str, session_id: str) -> str:
    """The claim key that serialises the CREATE decision for one pi session.

    **This is a SESSION-ID key, not a node key.** The standing rule that
    ``fno agents claim acquire`` is never called by hand is about NODE claims,
    where ``target init`` already claims the node and a manual acquire creates a
    double claim. This key lives in a different key space, is taken by the
    spawn lane rather than by a person, and is released in the same operation.

    The cwd is IN the key because pi's session lookup is cwd-scoped: the same
    id in two worktrees is two different sessions and must not contend.
    """
    return f"pi-session:{cwd}:{session_id}"


class PiCreateHeld(DispatchAskError):
    """Another process is creating this exact pi session, and it is named.

    A refusal that names the holder is what a caller needs. A timeout naming
    only a clock is the diagnostic seam this whole lane exists to avoid.

    It subclasses ``DispatchAskError`` deliberately: the spawn CLI catches that
    type and prints its message with a taxonomy exit code. A bare ``Exception``
    would escape as a traceback, so the one refusal this lane exists to deliver
    would be the one a caller never reads.
    """

    def __init__(self, key: str, holder: str, pid: Optional[int], host: str) -> None:
        self.key = key
        self.holder = holder
        self.pid = pid
        self.host = host
        super().__init__(
            f"pi session create for {key!r} is held by {holder} "
            f"(pid={pid}, host={host}). This is a create, and creates are "
            "serialised. Wait for the holder to finish, then JOIN the session "
            "it made.",
            exit_code=1,
        )


@dataclass
class CreateDecision:
    """The outcome of :func:`create_decision`.

    ``role`` is ``"create"`` for the one winner and ``"join"`` for everyone
    else. A loser is not an error: joining an existing session is the half that
    was measured safe, so the loser simply waits for the winner and then
    addresses the same session.
    """

    role: str
    key: str
    holder: str
    lookup: Optional[SessionLookup] = None
    notes: list[str] = field(default_factory=list)


@contextmanager
def create_decision(
    cwd: Path | str,
    session_id: str,
    holder: str,
    *,
    ttl_ms: int = CREATE_CLAIM_TTL_MS,
    wait_timeout_s: float = 60.0,
    poll_s: float = 0.25,
    claims_root: Optional[Path] = None,
) -> Iterator[CreateDecision]:
    """Serialise the CREATE decision for one pi session, and only that.

    The winner acquires ``pi-session:<cwd>:<id>``, creates the pi session, and
    releases as soon as the session exists. A loser reads the named holder,
    waits for the release, and then JOINS.

    **The atomicity has to live in fno's registry, not in pi's filesystem.**
    During the race pi's session files do not exist yet at all: a session's
    file appears at the first turn ATTEMPT, so a file count reads zero for any
    number of racing creates. A claim is written at acquire, so the window the
    file instrument cannot see is exactly the window the claim covers. That is
    why this is the whole fix rather than half of one, and why counting files
    here would be atomicity on the wrong resource.

    **On expiry the reading degrades to UNKNOWN and is re-checked, never to
    free.** A claim that vanishes is not evidence that no create is running; it
    is evidence this reading is stale. So the loser re-acquires, and the
    acquire itself - not any inference from an absence - decides who creates.

    The context manager releases the claim on exit, including on an exception,
    so a failed create never leaves the id locked for the rest of the TTL.
    """
    from fno.claims.core import ClaimHeldByOther, acquire_claim, release_claim

    key = create_claim_key(cwd, session_id)
    deadline = time.monotonic() + wait_timeout_s
    notes: list[str] = []
    while True:
        try:
            acquire_claim(
                key,
                holder,
                reason="pi session create (create decision only, released at first turn)",
                ttl_ms=ttl_ms,
                metadata={"cwd": str(cwd), "session_id": session_id, "harness": "pi"},
                root=claims_root,
            )
        except ClaimHeldByOther as held:
            if time.monotonic() >= deadline:
                raise PiCreateHeld(key, held.holder, held.pid, held.host) from held
            # The loser waits rather than erroring: joining is the safe half,
            # and the only thing it is waiting for is the session to exist.
            notes.append(f"waited on create holder {held.holder}")
            time.sleep(poll_s)
            continue
        break

    # Re-read the store on EVERY acquire, not only after a wait. Whether this
    # process contended is not the question; whether the session already exists
    # is. A racer whose first attempt happens to land after the winner released
    # never sees contention at all, and gating the check on "did I wait" made
    # exactly that racer a second creator.
    lookup = lookup_sessions(cwd, session_id)
    decision = CreateDecision(
        role="create", key=key, holder=holder, lookup=lookup, notes=notes
    )
    if lookup.state in {"one", "duplicate"}:
        decision.role = "join"
    elif lookup.state == "unknown":
        # An unreadable store is not evidence of an absent session, so this
        # degrades to unknown and creates under the claim it already holds
        # rather than reading the absence as "free". The claim, not this
        # reading, is what makes that safe.
        decision.notes.append(
            f"session store unreadable ({lookup.reason}); creating under the claim, "
            "because an unreadable store is not evidence of an absent session"
        )
    try:
        yield decision
    finally:
        release_claim(key, holder, root=claims_root)


#: How long :func:`await_session_created` waits for pi to make its create
#: decision. Bounded by measurement: pi reaches session-id adoption at 0.64s and
#: writes its first session file at 5.81s, 4.96s and 4.94s across three runs.
#: 15s is roughly 2.5x the slowest, and it sits well inside the claim's own TTL
#: so the wait can never outlive the claim it is protecting.
CREATE_SETTLE_TIMEOUT_S = 15.0


def await_session_created(
    cwd: Path | str,
    session_id: str,
    *,
    timeout_s: float = CREATE_SETTLE_TIMEOUT_S,
    poll_s: float = 0.2,
) -> SessionLookup:
    """Wait, inside the create claim, until pi's session provably exists.

    The claim is worthless without this. `mux pane run` returns as soon as the
    PANE exists, in tens of milliseconds, while pi reaches session-id adoption
    at 0.64s and writes its first session file at about 5s. A claim released on
    the pane therefore covers a window in which pi has not yet decided
    anything, and two racers on one id would both pass through it.

    Returns the final reading rather than raising. A timeout is NOT a failure
    here: the blind window is real, and a session whose first turn has not been
    attempted writes no file at all. The honest outcome for that is
    ``"none"``, which the caller records as a create it could not confirm. What
    this function must never do is report an unconfirmed create as a confirmed
    one.
    """
    deadline = time.monotonic() + timeout_s
    lookup = lookup_sessions(cwd, session_id)
    while lookup.state not in {"one", "duplicate"} and time.monotonic() < deadline:
        time.sleep(poll_s)
        lookup = lookup_sessions(cwd, session_id)
    return lookup


def rpc_argv(
    session_id: str,
    *,
    provider: Optional[str] = None,
    model: Optional[str] = None,
) -> list[str]:
    """The DRIVING lane's argv: ``pi --mode rpc`` on a caller-assigned id."""
    return [
        "pi",
        "--mode",
        "rpc",
        "--session-id",
        session_id,
        "--provider",
        provider or pi_provider(),
        "--model",
        model or pi_model(),
    ]


def attach_argv(
    session_id: str,
    *,
    provider: Optional[str] = None,
    model: Optional[str] = None,
) -> list[str]:
    """The WATCHING lane's argv: pi's own TUI on the same session id.

    An EXEC target, never a proxy, and the same shape the codex viewport
    established: the pane hosts a real vendor process and fno draws nothing.
    What differs is that pi needs no daemon and no socket - naming the same id
    in the same cwd is the whole attach.

    This is a JOIN and it is only safe as one. Running it against an id that
    does not exist yet is a CREATE, and creates are the unserialised half.

    Mirrors ``pi_attach_argv`` in ``crates/fno-agents/src/pi.rs``; a parity
    test pins the two.
    """
    return [
        "pi",
        "--session-id",
        session_id,
        "--provider",
        provider or pi_provider(),
        "--model",
        model or pi_model(),
    ]


def iter_jsonl(chunks: Iterator[bytes]) -> Iterator[dict[str, Any]]:
    """Decode pi's rpc stream under STRICT JSONL framing.

    LF is the only record delimiter, and an optional trailing CR is stripped.
    pi's own protocol notes call out that Node's ``readline`` is not compliant
    here because it also splits on U+2028 and U+2029, which are legal inside a
    JSON string; a reader built on Python's text-mode line iteration has the
    same defect, so this splits bytes on ``b"\\n"`` itself.

    A line that does not parse is skipped rather than raised: pi may print a
    human-readable notice (the "creating a new session with that id" line) on
    the same stream, and one such line must not abort a live turn.
    """
    buffer = b""
    for chunk in chunks:
        buffer += chunk
        while b"\n" in buffer:
            raw, buffer = buffer.split(b"\n", 1)
            raw = raw.rstrip(b"\r")
            if not raw.strip():
                continue
            try:
                event = json.loads(raw.decode("utf-8", "replace"))
            except (ValueError, UnicodeDecodeError):
                continue
            if isinstance(event, dict):
                yield event


def prompt_command(
    message: str, *, msg_id: Optional[str] = None, streaming: Optional[str] = None
) -> dict[str, Any]:
    """A ``prompt`` command. ``streaming`` is ``"steer"`` or ``"followUp"``.

    If the agent is already streaming and no ``streamingBehavior`` is given, pi
    returns an error rather than queueing, so a caller injecting mid-turn must
    name one.
    """
    command: dict[str, Any] = {"type": "prompt", "message": message}
    if msg_id:
        command["id"] = msg_id
    if streaming:
        command["streamingBehavior"] = streaming
    return command


def steer_command(message: str, *, msg_id: Optional[str] = None) -> dict[str, Any]:
    """A ``steer`` command: fno's mail injection, in pi's own vocabulary.

    pi delivers a steer after the current assistant turn finishes its tool
    calls and BEFORE the next LLM call. That is exactly the semantics fno's
    mail injection wants, and what typing into a pane only approximates. So
    mail to a pi rpc worker is a steer, and the pane lane takes no fno-typed
    payload at all: the bracketed-paste hazard is designed out rather than
    mitigated.
    """
    command: dict[str, Any] = {"type": "steer", "message": message}
    if msg_id:
        command["id"] = msg_id
    return command


def receipt_for_response(response: dict[str, Any]) -> str:
    """Describe a command response WITHOUT overclaiming.

    ``success: true`` means the prompt was accepted, queued, or handled
    immediately. Failures after acceptance arrive through the event stream, not
    as a second response for the same request id. So an accepted steer is a
    receipt about ACCEPTANCE, never about the agent having acted on it, and
    saying otherwise is the receipt-can-lie shape.
    """
    command = response.get("command", "command")
    if response.get("success"):
        return f"{command} accepted by pi (queued or handled; not yet acted on)"
    error = response.get("error") or "no reason given"
    return f"{command} REFUSED by pi before acceptance: {error}"


class PiRpcSession:
    """A live ``pi --mode rpc`` process, with stdin held open.

    **Holding stdin open is the whole contract.** rpc mode exits on stdin EOF,
    mid-turn, with status 0: a prompt fed from a file yielded five events and
    stopped at the user's own ``message_end``, the assistant never spoke, and
    the exit code still read success. So this class never closes stdin as a way
    of ending a turn, and :meth:`run_turn` settles on the POSITIVE
    ``agent_settled`` event rather than on the process going away.
    """

    def __init__(
        self,
        session_id: str,
        cwd: Path | str,
        *,
        provider: Optional[str] = None,
        model: Optional[str] = None,
        argv: Optional[Sequence[str]] = None,
        env: Optional[dict[str, str]] = None,
    ) -> None:
        self.session_id = session_id
        self.cwd = Path(cwd)
        self.argv = list(argv or rpc_argv(session_id, provider=provider, model=model))
        self._env = env
        self.proc: Optional[subprocess.Popen] = None
        #: The ONE event iterator for this session's lifetime. It must not be
        #: rebuilt per turn: `iter_jsonl` keeps its partial-record buffer in the
        #: generator, and a 64KB read routinely returns `agent_settled` plus the
        #: first bytes of the next record. A per-turn generator drops that tail,
        #: so the following turn resumes mid-record, its first line fails to
        #: parse, and it is skipped silently - losing events, and hanging the
        #: turn outright when the discarded tail held its own settle marker.
        self._events: Optional[Iterator[dict[str, Any]]] = None
        #: pi's human-readable notices go to stderr ("creating a new session
        #: with that id", provider retries, the tmux extended-keys warning).
        #: Nothing here needs them, but an undrained PIPE deadlocks the child
        #: once the OS buffer fills: pi blocks on write, stops emitting stdout,
        #: and `run_turn` waits forever for a marker that cannot arrive. A
        #: reader thread keeps the pipe moving and keeps the text available.
        self._stderr: list[str] = []
        self._stderr_thread: Optional[threading.Thread] = None
        #: Per-turn request id. The protocol's `id` correlates a request to its
        #: response, so reusing one constant would attribute every later turn's
        #: response, error, or refusal to the first turn.
        self._turn = 0

    def __enter__(self) -> "PiRpcSession":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def start(self) -> "PiRpcSession":
        self.proc = subprocess.Popen(
            self.argv,
            cwd=str(self.cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=self._env,
        )
        stderr = self.proc.stderr
        if stderr is not None:
            def _drain() -> None:
                for line in iter(stderr.readline, b""):
                    self._stderr.append(line.decode("utf-8", "replace").rstrip("\n"))

            self._stderr_thread = threading.Thread(target=_drain, daemon=True)
            self._stderr_thread.start()
        return self

    @property
    def stderr_text(self) -> str:
        """Everything pi has written to stderr so far, for a diagnostic."""
        return "\n".join(self._stderr)

    def send(self, command: dict[str, Any]) -> None:
        """Write one JSONL command. Never closes stdin."""
        if self.proc is None or self.proc.stdin is None:
            raise RuntimeError("pi rpc session is not started")
        self.proc.stdin.write((json.dumps(command) + "\n").encode("utf-8"))
        self.proc.stdin.flush()

    def events(self) -> Iterator[dict[str, Any]]:
        """Every typed event and response, in order, across every turn.

        The iterator is built ONCE and reused. See ``_events`` for why: the
        framing buffer belongs to the session, not to a turn.
        """
        if self.proc is None or self.proc.stdout is None:
            raise RuntimeError("pi rpc session is not started")
        if self._events is not None:
            return self._events
        stdout = self.proc.stdout

        def _chunks() -> Iterator[bytes]:
            while True:
                chunk = stdout.read1(65536) if hasattr(stdout, "read1") else stdout.read(65536)
                if not chunk:
                    return
                yield chunk

        self._events = iter_jsonl(_chunks())
        return self._events

    def next_msg_id(self) -> str:
        """The next per-turn request id. Never a constant: the protocol's
        ``id`` is what correlates a response to its request."""
        self._turn += 1
        return f"fno-{self._turn}"

    def run_turn(self, message: str, *, msg_id: Optional[str] = None) -> list[dict[str, Any]]:
        """Send one prompt and collect events until ``agent_settled``.

        Returns every event seen, settled event included. If the stream ends
        WITHOUT ``agent_settled``, this raises: an absence has three
        explanations and only one of them is the outcome, so a turn that merely
        stopped producing output is never reported as a turn that completed.
        """
        self.send(prompt_command(message, msg_id=msg_id or self.next_msg_id()))
        seen: list[dict[str, Any]] = []
        for event in self.events():
            seen.append(event)
            if event.get("type") == SETTLED_EVENT:
                return seen
        raise RuntimeError(
            f"pi rpc stream ended without {SETTLED_EVENT!r} after {len(seen)} events. "
            "rpc mode exits on stdin EOF mid-turn with status 0, so this is NOT a "
            f"completed turn and the exit code does not say otherwise. pi's stderr: "
            f"{self.stderr_text or '<empty>'}"
        )

    def steer(self, message: str, *, msg_id: Optional[str] = None) -> None:
        """Inject mail mid-turn, over pi's native steering command."""
        self.send(steer_command(message, msg_id=msg_id))

    def close(self) -> None:
        """End the session deliberately, by closing stdin and reaping.

        Closing stdin is the ONLY place an EOF is sent, and it is sent as an
        intent to end rather than as a way to finish a turn.
        """
        if self.proc is None:
            return
        if self.proc.stdin is not None:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
        # Close the read ends too: without this every session leaks two file
        # descriptors, which a long-lived driver notices before anything else
        # does.
        for stream in (self.proc.stdout, self.proc.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass
        self._events = None
