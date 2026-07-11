"""Born-with-why: context-carrying /think spawn at node birth (x-6a10).

This is the *mechanism* half of the node-provenance work. Its prerequisite
x-30f6 gave every backlog node its provenance *pointers* (``source_session_id``
+ ``source_harness`` + ``source_cwd`` + ``source_node_id``) captured ambiently
at birth, plus a claude transcript resolver
(:func:`fno.provenance.resolver.resolve_transcript`). Those pointers are inert
until something consumes them at birth. This module closes that loop.

When the node-birth path (``fno backlog idea``) persists a generated organic
node, :func:`maybe_spawn_think` evaluates a spawn decision *deterministically in
code* (Locked Decision 1: never LLM-volunteered, the ambient-capture principle
inherited from x-30f6) and, when armed:

  - **away** (the originating session is headless/autonomous): spawns a
    fire-and-forget ``/think`` background worker carrying the *resolved*
    transcript pointer (not a paraphrase), then stamps the node with the
    spawned think's session pointer.
  - **attended** (an operator is present): surfaces a single copy-pasteable
    ``/think <node-id>`` handoff line rather than auto-spawning.

The whole evaluation is opt-in (``config.think_spawn.enabled``, default OFF),
bounded (per-run blast-radius cap + at-most-once dedup token), and strictly
non-fatal: any failure resolves to ``think_skipped{reason}`` and the filing
pipeline continues. Exactly one decision event is emitted per evaluation once
the gate is on (a gate-off evaluation is a complete no-op: no event, no spawn).

Patterns are deliberately a sibling of :mod:`fno.backlog.advance`: same
single-decision-event discipline, same TTL bridge-token dedup, same
``fno agents spawn`` seam, same fail-safe-to-disabled config posture.
"""
from __future__ import annotations

import json
import logging
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from fno import _subprocess_util
from fno import route_resolve as _route_resolve
from fno.harness_identity import resolve_harness_identity
from fno.provenance.resolver import resolve_transcript

_LOG = logging.getLogger(__name__)

# Mirror advance.py / handoff.sh: a 3-minute TTL bridge token covers the
# spawn->worker-init boot window. TTL (not PID) liveness is mandatory so the
# reservation outlives this short-lived `fno backlog idea` process.
_DISPATCH_TTL_MS = 180_000  # 3m

_TRUTHY = frozenset({"1", "true", "yes", "on"})

# Highest-precedence explicit override (tests + force on/off). Mirrors
# advance.py's _ENV_OVERRIDE; the gate otherwise reads config.think_spawn.
_ENV_OVERRIDE = "FNO_THINK_SPAWN"
# Test/CI seam to pin presence without faking a manifest or tty.
_ENV_PRESENCE = "FNO_THINK_SPAWN_PRESENCE"
# Test/CI + force seam for the attended opt-in (B, x-5d51); mirrors _ENV_OVERRIDE.
# Otherwise read from config.think_spawn.attended (spawn|offer, default offer).
_ENV_ATTENDED = "FNO_THINK_SPAWN_ATTENDED"
# Explicit headless markers. A --bg worker may set FNO_BG, but the claude
# spawn path (providers/claude.py) injects FNO_AGENT_SELF into EVERY spawned
# worker and does NOT set FNO_BG - so a bg worker filing an idea before its
# target-state manifest exists would otherwise misclassify as attended (codex
# PR #9). FNO_AGENT_SELF is the reliable "I am a spawned agent, not an operator
# at the keyboard" signal.
_ENV_BG = "FNO_BG"
_ENV_AGENT_SELF = "FNO_AGENT_SELF"

# Decision-event kinds (registered in cli/src/fno/events/schema.yaml).
EVENT_SPAWNED = "think_spawned"
EVENT_OFFERED = "think_offered"
EVENT_SKIPPED = "think_skipped"
_EVENT_SOURCE = "backlog"

# (decision, event) pairs that are legal to construct. ``noop`` carries no
# event: a gate-off evaluation emits nothing at all (AC4-HP).
_VALID_DECISION_EVENTS = {
    ("spawned", EVENT_SPAWNED),
    ("offered", EVENT_OFFERED),
    ("skipped", EVENT_SKIPPED),
    ("noop", None),
}

# The discriminator `fno agents spawn` prints on a name collision (exit 2).
_SPAWN_ALREADY_EXISTS = "already exists"

# x-2c27: `fno agents spawn` enforces a 1-64 char agent name. The assembled
# provenance name can overflow even with per-component slugging, so it is capped.
_AGENT_NAME_MAX = 64

# A2 (x-122a): non-birth dispatch reasons. The default birth reason keeps A1
# byte-for-byte; the lifecycle reasons additionally require a RESOLVED transcript
# pointer (relevance filter, Locked Decision 3) so a context-free /think never
# fires on a high-volume lifecycle moment.
REASON_BIRTH = "birth"
REASON_WORK_START = "work-start"
REASON_RETRO = "retro"
# C (x-0a9c): the explicit conversational verb. NOT in _LIFECYCLE_REASONS: it is
# operator-invoked (one per explicit call, no firehose), so it does not need the
# relevance filter and may degrade to the stored triple like birth does.
REASON_CONVERSATIONAL = "conversational"
_LIFECYCLE_REASONS = frozenset({REASON_WORK_START, REASON_RETRO})


class SpawnAlreadyRunning(RuntimeError):
    """A peer dispatcher / live worker already owns this node's /think launch."""


class SpawnError(RuntimeError):
    """``fno agents spawn`` failed for a reason that leaves the node re-spawnable."""


@dataclass(frozen=True)
class ThinkSpawnResult:
    """Outcome of one maybe_spawn_think() evaluation.

    ``event`` is the single kind emitted (or None for the gate-off no-op).
    """

    decision: str  # "spawned" | "offered" | "skipped" | "noop"
    event: Optional[str]
    reason: Optional[str] = None
    node_id: Optional[str] = None
    presence: Optional[str] = None
    resolved: Optional[bool] = None
    think_session: Optional[str] = None
    offer_line: Optional[str] = None
    detail: Optional[str] = None

    def __post_init__(self) -> None:
        if (self.decision, self.event) not in _VALID_DECISION_EVENTS:
            raise ValueError(
                f"invalid ThinkSpawnResult (decision, event): "
                f"({self.decision!r}, {self.event!r})"
            )


@dataclass
class RunState:
    """Per-node-generation-run state for the blast-radius cap (AC4-EDGE).

    A single ``fno backlog idea`` files one node, so the default fresh state
    trivially satisfies the cap. A bulk path (e.g. a future decompose wiring)
    threads ONE RunState through all its births so the cap bounds the run.
    """

    spawned: int = 0
    truncation_logged: bool = False


@dataclass
class ThinkSeed:
    """An assembled /think seed: the spawn prompt + the attended offer line."""

    prompt: str  # multi-line; carries the transcript POINTER, never a paraphrase
    offer_line: str  # single copy-pasteable line (AC2-UI)
    resolved: bool  # did the origin transcript resolve to a real .jsonl?
    output_path: str = ""  # where the headless worker writes its /think doc (B, x-5d51)


# ---------------------------------------------------------------------------
# Gate
# ---------------------------------------------------------------------------


def _settings_for(project_root: Optional[Path]):
    """Load settings for the NODE's repo when known, else the ambient cwd.

    Honors ``project_root`` (gemini PR #9): in a multi-repo / cross-project run
    the gate must read the node's repo settings, not whatever repo the birth
    process happens to be cwd'd in. Falls back to the ambient ``load_settings``
    when no root is given (the cmd_idea path, which defaults project_root to cwd).
    """
    from fno.config import load_settings, load_settings_for_repo

    if project_root is not None:
        return load_settings_for_repo(Path(project_root))
    return load_settings()


def think_spawn_enabled(
    *,
    project_root: Optional[Path] = None,
    env: Optional[dict] = None,
) -> bool:
    """Resolve whether born-with-why /think spawn is armed.

    Precedence (highest first), mirroring advance.auto_continue_enabled:
      1. ``FNO_THINK_SPAWN`` env override (explicit force on/off).
      2. ``config.think_spawn.enabled`` from the node's repo settings
         (``project_root`` when given, else the ambient cwd; local>global).
      3. default False.

    Fail-safe (AC4-ERR): ANY exception reading settings degrades to False
    rather than raising into the node-birth pipeline.
    """
    environ = os.environ if env is None else env
    override = environ.get(_ENV_OVERRIDE)
    if override is not None:
        return override.strip().lower() in _TRUTHY

    try:
        return bool(_settings_for(project_root).think_spawn.enabled)
    except Exception as exc:  # noqa: BLE001 - fail-safe to disabled (AC4-ERR)
        _LOG.debug("think_spawn_enabled: settings read failed, defaulting off: %s", exc)
        return False


def _max_per_run(project_root: Optional[Path]) -> int:
    """The blast-radius cap from the node's repo config, fail-safe to 5."""
    try:
        return int(_settings_for(project_root).think_spawn.max_per_run)
    except Exception:  # noqa: BLE001
        return 5


def _daily_cap(project_root: Optional[Path]) -> int:
    """The per-install per-day ceiling from config, fail-safe to 20 (0 = off)."""
    try:
        return int(_settings_for(project_root).think_spawn.daily_cap)
    except Exception:  # noqa: BLE001
        return 20


# ---------------------------------------------------------------------------
# Per-day firehose ceiling (Locked Decision 3) - global across projects/nodes
# ---------------------------------------------------------------------------


def _daily_counter_path() -> Path:
    """``~/.fno/.think-spawn-daily.json`` - the global per-day spawn counter.

    Per-install (not per-project): the firehose guard bounds total bg /think
    sessions a day regardless of which repo or node triggered them. Resolved
    under ``global_claims_root()`` (the SAME ``$FNO_CLAIMS_ROOT``-honoring base
    as the dispatch dedup tokens) so the counter isolates with the claims in
    tests and travels with them in production.
    """
    from fno.claims.io import global_claims_root

    return global_claims_root() / ".fno" / ".think-spawn-daily.json"


def _today_str() -> str:
    from datetime import date

    return date.today().isoformat()


def _daily_count() -> int:
    """Today's spawn count, or 0 when the file is absent / stale / unreadable."""
    try:
        obj = json.loads(_daily_counter_path().read_text(encoding="utf-8"))
        if isinstance(obj, dict) and obj.get("date") == _today_str():
            return int(obj.get("count") or 0)
    except (OSError, ValueError, TypeError):
        pass
    return 0


def _bump_daily_count() -> None:
    """Increment today's spawn count (resetting on a new day). Best-effort.

    ponytail: read-modify-write, last-writer-wins on the count - a soft ceiling
    tolerates an off-by-one under a rare race. But the WRITE is atomic (tmp +
    os.replace) so a concurrent bump can never leave a torn/partial JSON file,
    which would read back as 0 and silently RESET the ceiling for the day. The
    per-(node,reason) dedup token bounds same-node storms; this bounds total
    daily volume. Swap in the claim lock if exact accounting ever matters.
    """
    try:
        p = _daily_counter_path()
        p.parent.mkdir(parents=True, exist_ok=True)
        count = _daily_count() + 1
        tmp = p.with_suffix(f".{os.getpid()}.tmp")
        tmp.write_text(json.dumps({"date": _today_str(), "count": count}), encoding="utf-8")
        os.replace(tmp, p)  # atomic rename: a reader sees the old OR new file, never a torn one
    except OSError as exc:  # noqa: BLE001 - never wedge a spawn on a counter write
        _LOG.debug("spawn_think: daily-count bump failed: %s", exc)


def _attended_mode(
    project_root: Optional[Path] = None,
    *,
    env: Optional[dict] = None,
) -> str:
    """Resolve the attended opt-in: ``spawn`` (real bg /think) or ``offer`` (B, x-5d51).

    Precedence mirrors think_spawn_enabled: ``FNO_THINK_SPAWN_ATTENDED`` override,
    then ``config.think_spawn.attended`` from the node's repo settings. Fail-safe
    to ``offer`` (byte-for-byte x-6a10): any unreadable/garbage value keeps the
    default stderr-handoff behavior, never an unintended auto-spawn (AC4-HP).
    """
    environ = os.environ if env is None else env
    # A PRESENT override is authoritative (mirrors think_spawn_enabled): a
    # set-but-garbage value resolves to ``offer`` here rather than leaking
    # through to a config ``attended: spawn`` (gemini PR #33). Only an ABSENT
    # override falls through to settings.
    override = environ.get(_ENV_ATTENDED)
    if override is not None:
        return "spawn" if override.strip().lower() == "spawn" else "offer"
    try:
        val = str(_settings_for(project_root).think_spawn.attended).strip().lower()
        return "spawn" if val == "spawn" else "offer"
    except Exception:  # noqa: BLE001 - fail-safe to the offer default
        return "offer"


# ---------------------------------------------------------------------------
# Presence classifier (attended vs away) - Locked Decision 3
# ---------------------------------------------------------------------------


def _scan_md_field(text: str, key: str) -> Optional[str]:
    """Pull a top-level ``key: value`` from a target-state manifest body.

    Mirrors graph.cli._scan_md_field: tolerant of quotes, returns None when
    absent. Kept local so this module does not import graph.cli (which would
    create an import cycle: graph.cli calls into this module).
    """
    m = re.search(rf"^{re.escape(key)}\s*:\s*(.+)$", text, re.MULTILINE)
    if not m:
        return None
    value = m.group(1).strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
        value = value[1:-1]
    return value


def _owned_manifest_attended(project_root: Path, environ: dict) -> Optional[bool]:
    """Return the ``attended`` flag of THIS session's target-state manifest.

    Ownership is proven exactly as graph.cli._session_provenance does it: the
    manifest's ``claude_transcript_id`` must equal this process's
    ``CLAUDE_CODE_SESSION_ID``, so a stale / foreign worktree manifest never
    leaks a presence verdict this session does not own. Returns None when there
    is no owned manifest (caller falls back to env signal).
    """
    identity = resolve_harness_identity(environ)
    if identity.harness != "claude" or not identity.session_id:
        return None
    sid = identity.session_id
    try:
        text = (project_root / ".fno" / "target-state.md").read_text(encoding="utf-8")
    except OSError:
        return None
    # Current key is claude_session_id; old-key fallback for one release.
    manifest_claude_sid = _scan_md_field(text, "claude_session_id") or _scan_md_field(
        text, "claude_transcript_id"
    )
    if manifest_claude_sid != sid:
        return None
    raw = _scan_md_field(text, "attended")
    if raw is None:
        return None
    return raw.strip().lower() in _TRUTHY


def classify_presence(
    *,
    project_root: Optional[Path] = None,
    env: Optional[dict] = None,
) -> str:
    """Classify the originating session as ``attended`` or ``away``.

    Primary signal (Locked Decision 3, dependency-free): the attended-vs-
    headless state of the *originating* session.
      1. ``FNO_THINK_SPAWN_PRESENCE`` test/CI override.
      2. A spawned/headless worker (``FNO_AGENT_SELF`` injected by the claude
         spawn path, or an explicit ``FNO_BG``) => away. This MUST precede the
         CLAUDE_CODE_SESSION_ID check below: a bg worker exposes that session id
         too, so without this a manifest-less bg worker would misclassify as
         attended (codex PR #9).
      3. This session's OWNED target-state manifest's ``attended`` flag.
      4. An interactive Claude or Codex session identity with no autonomous
         manifest => attended.
      5. Default => away (filed from a script/cron with no human present).

    tty probing is deliberately NOT primary (Domain Pitfall: false-positives
    inside tmux/CI, and ``fno backlog idea``'s own stdout is captured even in
    attended sessions); the explicit spawned-worker/manifest signal leads.
    """
    environ = os.environ if env is None else env

    override = (environ.get(_ENV_PRESENCE) or "").strip().lower()
    if override in ("attended", "away"):
        return override

    if (environ.get(_ENV_AGENT_SELF) or "").strip() or (
        environ.get(_ENV_BG) or ""
    ).strip().lower() in _TRUTHY:
        return "away"

    root = Path(project_root) if project_root is not None else Path.cwd()
    attended = _owned_manifest_attended(root, environ)
    if attended is not None:
        return "attended" if attended else "away"

    identity = resolve_harness_identity(environ)
    if identity.session_id and identity.harness in ("claude", "codex"):
        return "attended"

    return "away"


# ---------------------------------------------------------------------------
# Context assembler - resolve the pointer, never paraphrase (US1)
# ---------------------------------------------------------------------------


def assemble_seed(node: dict) -> ThinkSeed:
    """Build a /think seed carrying the *resolved* origin pointer.

    Resolves the node's x-30f6 provenance pointers to a real transcript path
    via :func:`resolve_transcript`. When resolved, the seed references the
    on-disk ``.jsonl`` (the pointer); when not (foreign harness / pruned file),
    it degrades to the stored ``(harness, session_id, cwd)`` triple with
    ``resolved=False`` (AC1-EDGE) - it NEVER paraphrases the why (the exact bug
    being fixed).
    """
    node_id = node.get("id") or "?"
    slug = node.get("slug") or ""
    title = node.get("title") or ""
    details = (node.get("details") or "").strip()
    source_node = node.get("source_node_id")

    res = resolve_transcript(
        node.get("source_harness"),
        node.get("source_session_id"),
        node.get("source_cwd"),
    )

    why_lines = []
    if res.resolved and res.transcript_path:
        why_lines.append(f"  origin transcript: {res.transcript_path}")
        if res.ambiguous:
            why_lines.append("  (note: session-id prefix matched multiple transcripts; this is the first)")
    else:
        why_lines.append(
            "  origin transcript UNRESOLVED "
            f"(reason: {res.reason or 'unknown'}); fall back to the stored pointer:"
        )
        why_lines.append(
            f"  origin session: {node.get('source_harness') or '?'}:"
            f"{node.get('source_session_id') or '?'} @ {node.get('source_cwd') or '?'}"
        )
    if source_node:
        why_lines.append(f"  origin node chain: {source_node}")

    # Give the headless worker a durable, known home for its output so the
    # /think doc is written where /blueprint mutates it in place (plans-dir,
    # date-slug named); best-effort - an unresolvable path just omits the line.
    output_path = _think_output_path(node_id, slug)

    prompt = (
        f"/think {node_id}\n\n"
        f"WHY THIS NODE EXISTS - read the originating context below for the full "
        f"reasoning that justified filing it. Do NOT work from the title alone; "
        f"the transcript holds the discovery / failure-mode / tradeoff that made "
        f"this worth filing.\n"
        + "\n".join(why_lines)
        + f"\n\nNode: {node_id} {('(' + slug + ')') if slug else ''}\n"
        f"Title: {title}\n"
        + (f"\nDetails:\n{details}\n" if details else "")
        + (
            f"\nWRITE YOUR /think OUTPUT to this exact path so node {node_id} can "
            f"point at it:\n  {output_path}\n"
            if output_path
            else ""
        )
    )

    # The offer line is a single copy-pasteable line (AC2-UI). When resolved we
    # append the transcript as a trailing comment; otherwise a bare line.
    if res.resolved and res.transcript_path:
        offer_line = f"/think {node_id}  # origin transcript: {res.transcript_path}"
    else:
        offer_line = f"/think {node_id}"

    return ThinkSeed(
        prompt=prompt, offer_line=offer_line, resolved=bool(res.resolved),
        output_path=output_path,
    )


def _plans_output_dir() -> Path:
    """The plans dir the /think doc lands in (x-ff83 W1); shared with W2's sweep.

    Delegates to :func:`fno.paths.plans_content_dir` (settings.local
    ``plansDirectory`` -> ``config.plans_dir``). Raises on an unresolvable
    root so the caller can fall back to briefs.
    """
    from fno.paths import plans_content_dir

    return plans_content_dir()


def _frontmatter_claims_node(path: Path, node_id: str) -> bool:
    """True iff ``path``'s YAML frontmatter ``claims:``/``graph_node_id:`` == node_id.

    Reads only the frontmatter block, never the body (a design doc often quotes
    an id in prose or a fenced example, which is not an authoritative claim).
    """
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return False
    if not text.startswith("---"):
        return False
    end = text.find("\n---", 3)
    fm = text[: end if end != -1 else 2000]
    for line in fm.splitlines():
        m = re.match(r"^(?:claims|graph_node_id):\s*(.+?)\s*$", line)
        if m and m.group(1).strip().strip("\"'") == node_id:
            return True
    return False


def _find_node_doc(pdir: Path, node_id: str) -> Optional[Path]:
    """The plans-dir doc that already belongs to ``node_id``, or None.

    A bounded plans-dir scan (never a graph walk). A file whose frontmatter
    *claims* the node wins over one that merely *ends* ``-<node_id>.md`` - the
    claim is the stronger signal (a pre-created roadmap stub is the doc's home).
    First match in sorted order is deterministic; two files claiming one node is
    pre-existing corruption, flagged for separate cleanup, not resolved here.
    """
    try:
        candidates = sorted(pdir.glob("????-??-??-*.md"))
    except OSError:
        return None
    by_name: Optional[Path] = None
    for f in candidates:
        if _frontmatter_claims_node(f, node_id):
            return f
        if by_name is None and f.name.endswith(f"-{node_id}.md"):
            by_name = f
    return by_name


def _think_output_path(node_id: str, slug: str = "") -> str:
    """Resolve where the headless /think worker writes its design doc (x-8af8).

    The filename ALWAYS ends ``-<node_id>.md`` so a roadmap base keyed on the
    node id can find it. Resolution is node-id-first: reuse a file that already
    claims the node, else a prior ``-<node_id>.md`` doc, else mint
    ``<plans-dir>/YYYY-MM-DD-<slug>-<node_id>.md`` (empty slug ->
    ``<date>-<node_id>.md``). Keying reuse on the stable node id, not the mutable
    slug, keeps a re-dispatch after a slug edit idempotent. Falls back to
    ``briefs_dir()/think-<node-id>.md`` with a visible warning (AC1-ERR) when the
    plans dir is unresolvable. Returns "" only if even briefs is unresolvable.
    """
    from datetime import datetime, timezone

    try:
        pdir = _plans_output_dir()
        # 1-2: reuse this node's existing doc (frontmatter claim, else a prior
        #      node-id-suffixed mint) rather than minting a duplicate.
        existing = _find_node_doc(pdir, node_id)
        if existing:
            return str(existing)
        # 3: mint. Empty slug degrades to <date>-<node_id>.md, never a dangling
        #    <date>--<node_id>.md.
        date = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        s = (slug or "").strip("-")
        tail = f"{s}-{node_id}" if s else node_id
        return str(pdir / f"{date}-{tail}.md")
    except Exception as exc:  # noqa: BLE001 - degrade to briefs, never wedge the spawn
        try:
            from fno.paths import briefs_dir

            print(
                f"warn: plans dir unresolvable ({exc}); /think doc falls back to "
                f"briefs/ - relocate before /blueprint",
                file=sys.stderr,
            )
            return str(briefs_dir() / f"think-{node_id}.md")
        except Exception:  # noqa: BLE001 - best-effort; an unresolved path is non-fatal
            return ""


# ---------------------------------------------------------------------------
# Spawn seam (subprocess to `fno agents spawn`; patched in unit tests)
# ---------------------------------------------------------------------------


def _name_slug(raw: Optional[str]) -> str:
    """Normalize a slug/title tail to a safe agent-name suffix.

    Mirrors advance._name_slug / dispatch-node.sh: lowercase, non-[a-z0-9-]
    runs -> hyphen, collapse repeats, strip, cut to 30, trim trailing hyphen.
    """
    if not raw:
        return ""
    s = re.sub(r"-+", "-", re.sub(r"[^a-z0-9-]", "-", raw.lower())).strip("-")
    return s[:30].rstrip("-")


def _worker_agent_name(node_id: str, node_slug: Optional[str], reason: str = REASON_BIRTH, invocation_suffix: Optional[str] = None) -> str:
    """Provenance-carrying bg worker name, scoped by trigger reason.

    Birth keeps ``think-<node-id>-<slug>`` byte-for-byte (A1). A LIFECYCLE
    trigger gets ``think-<node-id>-<reason>-<slug>`` so a node born + later
    worked + retro'd dispatches a DISTINCT worker per moment - the dedup token
    is reason-scoped, and ``fno agents spawn`` rejects a duplicate NAME, so the
    name must be reason-scoped too or the second lifecycle trigger collides and
    is wrongly skipped (codex P2).
    """
    base = f"think-{node_id}" if reason == REASON_BIRTH else f"think-{node_id}-{reason}"
    slug = _name_slug(node_slug)
    # An optional per-invocation discriminator (C/x-0a9c: the live session id) so a
    # REPEATABLE trigger (conversational) gets a DISTINCT name each call. Without
    # it `fno agents spawn` rejects the constant name and a later conversation can
    # never re-dispatch the node even after the dedup TTL expires (codex P2).
    suf = _name_slug(invocation_suffix)
    # x-2c27 (AC2-ERR): each component is slugged to 30, but the ASSEMBLED name
    # (think- + node-id + reason + slug + suffix) can exceed `fno agents spawn`'s
    # 1-64 char limit and fail with "name must be 1-64 chars". `base` (id + reason
    # scoping) and `suf` (the per-session uniqueness discriminator) are BOTH
    # load-bearing, so cap only the human-readable `slug` to make room - trimming
    # the assembled tail would shave the suffix and let two repeat dispatches
    # collide on name (codex P2).
    fixed = len(base) + (1 + len(suf) if suf else 0)  # +1 for the '-' before suf
    if slug:
        avail = _AGENT_NAME_MAX - fixed - 1  # -1 for the '-' before slug
        slug = slug[:avail].rstrip("-") if avail > 0 else ""
    name = "-".join(p for p in (base, slug, suf) if p)
    # Pathological backstop: base + suf alone overflow (a very long id/suffix) ->
    # cap the tail; the `think-<node-id>` lead still survives.
    if len(name) > _AGENT_NAME_MAX:
        name = name[:_AGENT_NAME_MAX].rstrip("-")
    return name


def _spawn_think_worker(
    node_id: str,
    prompt: str,
    node_cwd: Optional[str],
    node_slug: Optional[str],
    reason: str = REASON_BIRTH,
    invocation_suffix: Optional[str] = None,
    model: Optional[str] = None,
    provider: Optional[str] = None,
    permission_mode: Optional[str] = None,
) -> str:
    """Dispatch a fire-and-forget ``/think`` claude bg worker carrying the seed.

    Mirrors advance._spawn_worker: ``/think`` rides as the command prompt (NOT
    an env var), the agent is named ``think-<node-id>[-<reason>]-<slug>``, the
    cwd resolves to the node's recorded root (``--cwd``) or canonical main
    (``--fresh``). Returns the spawn receipt's short_id. Raises
    SpawnAlreadyRunning on a name-collision and SpawnError otherwise.
    """
    agent_name = _worker_agent_name(node_id, node_slug, reason, invocation_suffix)
    # x-2c27: a conversational /think handoff is a DETACHED thread, so route it
    # to the `claude --bg` substrate explicitly (the x-3ab8 default `pane` would
    # land an owned-PTY pane that stalls a fire-and-forget dispatch).
    # provider defaults to claude (the bg substrate is claude-only); a dispatch
    # flag overriding it rides through and fails loud downstream if the substrate
    # cannot host it, rather than being silently dropped.
    prov = (provider or "").strip() or "claude"
    cmd = [*_subprocess_util.fno_py_cmd(), "agents", "spawn", "--provider", prov, "--substrate", "bg"]
    if node_cwd:
        cmd += ["--cwd", node_cwd]
    else:
        cmd += ["--fresh"]
    # x-571f: a pinned node's /think worker also runs on the pin (US1 honors it
    # on the claude/bg arm). Empty/None = provider default, unchanged.
    if model:
        cmd += ["--model", model]
    # x-dfa4: an explicit permission_mode wins; else the autonomous-dispatcher
    # config default (config.agents.spawn_permission_mode). Both empty = unchanged.
    mode = (permission_mode or "").strip()
    if not mode:
        try:
            mode = (
                _settings_for(
                    Path(node_cwd) if node_cwd else None
                ).agents.spawn_permission_mode
                or ""
            ).strip()
        except Exception:  # noqa: BLE001 - fail-safe to unset (unchanged)
            mode = ""
    if mode:
        cmd += ["--permission-mode", mode]
    cmd += [agent_name, prompt]

    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if proc.returncode != 0:
        stderr = (proc.stderr or "").strip()
        if proc.returncode == 2 and _SPAWN_ALREADY_EXISTS in stderr:
            raise SpawnAlreadyRunning(f"agent {agent_name} already exists")
        raise SpawnError(
            f"fno agents spawn exited {proc.returncode}: "
            f"{(stderr or proc.stdout or '').strip()[:200]}"
        )
    short_id = _parse_short_id(proc.stdout or "")
    if not short_id:
        raise SpawnError(
            f"fno agents spawn exit 0 but no short_id receipt: "
            f"{(proc.stdout or proc.stderr or '').strip()[:200]}"
        )
    return short_id


def _parse_short_id(stdout: str) -> str:
    """Extract the spawn receipt's ``short_id`` from spawn stdout, robustly.

    The receipt may arrive as a compact single-line JSON object, a
    pretty-printed object (``"short_id"`` on its own line), or one line among
    banner/log noise. A naive per-line ``json.loads`` raises on every line of a
    pretty-printed object (gemini PR #9). So:
      1. Try parsing the WHOLE stdout as one JSON object (covers compact AND
         pretty-printed single objects).
      2. Fall back to a per-line scan that ignores non-JSON noise lines (parity
         with advance._spawn_worker, which guards against a log line that merely
         MENTIONS short_id).
    A bare regex is deliberately avoided: it would match a ``"short_id"`` inside
    an unrelated log line, whereas a real JSON parse cannot.
    """
    text = stdout or ""
    try:
        obj = json.loads(text)
        if isinstance(obj, dict) and obj.get("short_id"):
            return str(obj["short_id"])
    except json.JSONDecodeError:
        pass
    for line in text.splitlines():
        if '"short_id"' in line:
            try:
                sid = json.loads(line).get("short_id", "")
            except json.JSONDecodeError:
                continue
            if sid:
                return str(sid)
    return ""


# ---------------------------------------------------------------------------
# Claim helpers (dedup token) + event emission (mirror advance.py)
# ---------------------------------------------------------------------------


def _claim_is_live(key: str) -> bool:
    # A suspect claim (x-ba4b: TTL-unexpired, dead pid) counts as occupied too,
    # so a respawned worker's dispatch reservation still dedups.
    from fno.claims.core import claim_status

    try:
        return claim_status(key).get("state") in ("live", "suspect")
    except Exception:  # noqa: BLE001 - a probe error must not crash the birth hook
        return False


def _safe_release(key: str, holder: str) -> None:
    from fno.claims.core import release_claim

    try:
        release_claim(key, holder)
    except Exception as exc:  # noqa: BLE001
        _LOG.debug("spawn_think: dispatch-reservation release failed for %s: %s", key, exc)


def _events_path(project_root: Optional[Path]) -> Path:
    root = Path(project_root) if project_root is not None else Path.cwd()
    return root / ".fno" / "events.jsonl"


def _emit(kind: str, data: dict, events_path: Path) -> None:
    """Best-effort event emit. Never raises (non-fatal: never wedge node birth)."""
    try:
        from fno.events import _build, append_event

        append_event(_build(kind, _EVENT_SOURCE, data), events_path)
    except Exception as exc:  # noqa: BLE001
        print(f"spawn_think: WARNING: event emit failed ({kind}): {exc}", file=sys.stderr)


def _stamp_forward(
    node_id: str,
    think_session: str,
    project_root: Optional[Path],
    output_path: Optional[str] = None,
) -> None:
    """Stamp the node with its spawned /think session + output pointers (Discretion 5).

    Serialized under locked_mutate_graph so the forward pointer write cannot
    clobber a concurrent node update (Concurrency invariant). Best-effort: a
    stamp failure never unwinds an already-successful spawn. ``output_path`` (B,
    x-5d51) records where the headless worker writes its /think doc so the node
    points at the artifact, not just the session.
    """
    try:
        from fno.graph.cli import _graph_path
        from fno.graph.store import locked_mutate_graph

        def mutator(entries):
            for e in entries:
                if e.get("id") == node_id:
                    e["think_session_id"] = think_session
                    if output_path:
                        e["think_output_path"] = output_path
                    break
            return entries

        locked_mutate_graph(_graph_path(), mutator)
    except Exception as exc:  # noqa: BLE001
        _LOG.debug("spawn_think: forward stamp failed for %s: %s", node_id, exc)


# ---------------------------------------------------------------------------
# on_node_born() - the shared birth seam (v2 A1)
# ---------------------------------------------------------------------------


def on_node_born(
    node: dict,
    *,
    project_root: Optional[Path] = None,
    run_state: Optional[RunState] = None,
    graph_path: Optional[Path] = None,
    persisted: bool = False,
    quiet: bool = False,
) -> Optional[ThinkSpawnResult]:
    """Single post-persist birth hook: every node-creation path routes here.

    Before v2 only ``cmd_idea`` called :func:`maybe_spawn_think` inline, so a
    retro-harvest / intake / decompose birth carried no why forward (the
    x-7c38 / x-6e23 gap). This wrapper gives every birth path the SAME gated,
    bounded, non-fatal dispatch.

    Three responsibilities the callers must NOT each re-implement:

      * **Gate-first.** Resolve the gate before any other I/O so a default-OFF
        install pays nothing (no graph re-read, no settings churn beyond the
        single gate read).
      * **Durable re-read.** ``store.ensure_slugs`` may re-slug a node inside
        ``locked_mutate_graph``, so the seed + worker name must read the node
        back by id post-persist (Domain Pitfall: slug re-read after persist).
        Falls back to the passed-in node when the re-read can't find it. A
        caller that ALREADY holds the persisted, slugged node (decompose's
        ``by_id`` map, intake's re-read) passes ``persisted=True`` to skip the
        redundant read.
      * **Strictly non-fatal.** Any failure here resolves to ``None`` and never
        raises into the node-birth path that called it (additive, opt-in).

    Bulk paths (decompose children, a retro batch) thread ONE ``run_state`` so
    the blast-radius cap bounds the whole run, not each node.

    ``project_root`` is honored as-given and is NOT auto-derived from the node's
    cwd: ``maybe_spawn_think`` uses it for BOTH the settings gate AND presence
    classification, and presence must key off the *originating* session's cwd
    (where its ``target-state.md`` lives), which for a worktree-born node is the
    running cwd, not the node's durable canonical cwd. Defaulting to the node
    cwd would make an autonomous worktree session's away-manifest invisible and
    misclassify it as attended (codex P2). Left as ``None`` it inherits x-6a10's
    proven ambient behavior; a caller may still pass an explicit root to scope
    the gate.
    """
    try:
        node_id = (node or {}).get("id")

        # Gate-first: off => zero further I/O (the slug re-read below is wasted
        # work for the default-OFF install, which is every un-opted-in install).
        if not node_id or not think_spawn_enabled(project_root=project_root):
            return None

        if persisted:
            durable = node
        else:
            from fno.graph.cli import _graph_path
            from fno.graph.store import read_graph

            gp = graph_path if graph_path is not None else _graph_path()
            # ponytail: linear scan of the graph per born node. Bounded by the
            # blast cap (default 5) and gated OFF by default; callers holding the
            # durable node already pass persisted=True to skip this.
            durable = next(
                (e for e in read_graph(gp) if e.get("id") == node_id), node
            )
        return maybe_spawn_think(
            durable, project_root=project_root, run_state=run_state, quiet=quiet
        )
    except Exception as exc:  # noqa: BLE001 - additive; never wedge node birth
        _LOG.debug("on_node_born: non-fatal dispatch failure: %s", exc)
        return None


# ---------------------------------------------------------------------------
# A2 lifecycle wrappers (x-122a) - work-start + retro-at-done
# ---------------------------------------------------------------------------


def _subflag_on(name: str, project_root: Optional[Path]) -> bool:
    """Read a ``config.think_spawn.<name>`` bool sub-flag, fail-safe to False.

    A2 triggers gate on their OWN sub-flag IN ADDITION to ``enabled`` (Open
    Question 1: even when the layer is on, work-start/retro stay off until
    explicitly armed). Any settings-read error degrades to off.
    """
    try:
        return bool(getattr(_settings_for(project_root).think_spawn, name))
    except Exception:  # noqa: BLE001 - fail-safe to disabled
        return False


def _on_node_lifecycle(
    node: dict,
    *,
    reason: str,
    subflag: str,
    project_root: Optional[Path],
    run_state: Optional[RunState],
) -> Optional[ThinkSpawnResult]:
    """Shared A2 lifecycle dispatch: gate the sub-flag, then route to the core.

    Mirrors :func:`on_node_born` (gate-first, strictly non-fatal) but adds the
    per-trigger sub-flag gate and tags the dispatch with ``reason``. Callers
    already hold the persisted node (the claimed/closed node dict), so no graph
    re-read is needed.
    """
    try:
        node_id = (node or {}).get("id")
        # Gate-first: the layer must be enabled AND this trigger's sub-flag on.
        # Checking enabled here (not only the sub-flag) keeps the default-OFF
        # install from doing maybe_spawn_think's env/path setup just to noop.
        if (
            not node_id
            or not think_spawn_enabled(project_root=project_root)
            or not _subflag_on(subflag, project_root)
        ):
            return None
        return maybe_spawn_think(
            node, reason=reason, project_root=project_root, run_state=run_state
        )
    except Exception as exc:  # noqa: BLE001 - additive; never wedge the lifecycle op
        _LOG.debug("on_node_%s: non-fatal dispatch failure: %s", reason, exc)
        return None


def on_node_work_start(
    node: dict,
    *,
    project_root: Optional[Path] = None,
    run_state: Optional[RunState] = None,
) -> Optional[ThinkSpawnResult]:
    """A2: dispatch a ``work-start`` context /think when /target claims a node.

    Gated by ``config.think_spawn.on_work_start`` (default OFF even when the
    layer is enabled). Non-fatal: never blocks the claim it rides in on.
    """
    return _on_node_lifecycle(
        node, reason=REASON_WORK_START, subflag="on_work_start",
        project_root=project_root, run_state=run_state,
    )


def on_node_retro(
    node: dict,
    *,
    project_root: Optional[Path] = None,
    run_state: Optional[RunState] = None,
) -> Optional[ThinkSpawnResult]:
    """A2: dispatch a ``retro`` context /think when ``fno backlog done`` closes a node.

    Gated by ``config.think_spawn.on_retro`` (default OFF even when the layer is
    enabled). Non-fatal: never blocks the node close it rides in on.
    """
    return _on_node_lifecycle(
        node, reason=REASON_RETRO, subflag="on_retro",
        project_root=project_root, run_state=run_state,
    )


# ---------------------------------------------------------------------------
# dispatch_conversational() - the explicit conversational verb (v2 C, x-0a9c)
# ---------------------------------------------------------------------------


def dispatch_conversational(
    node: dict,
    *,
    session_id: Optional[str],
    cwd: Optional[str],
    harness: str = "claude",
    project_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    env: Optional[dict] = None,
) -> ThinkSpawnResult:
    """C (x-0a9c): explicit conversational /think dispatch for a named node.

    The operator, mid-conversation about an fno-touched node, invokes one verb
    and a bg /think picks it up with full LIVE context (US5/AC5-HP). Unlike the
    automatic A1/A2 triggers this:

      * carries the LIVE session's transcript pointer (``session_id``/``cwd``/
        ``harness``), NOT the node's stored birth origin - so the spawned think
        reads THIS conversation, not whatever first filed the node;
      * actually SPAWNS a bg /think even in an attended session - the explicit
        invocation IS the opt-in, so we do not degrade to a stderr offer line;
      * is not gated behind ``config.think_spawn.enabled`` - the verb is the
        per-invocation opt-in, so a default-OFF install still serves an explicit
        request.

    All three forcings reuse existing env seams rather than new ``maybe_spawn``
    branches (Locked Decision 6): ``FNO_THINK_SPAWN=1`` arms the gate and
    ``FNO_THINK_SPAWN_ATTENDED=spawn`` chooses a real spawn over the offer line.
    Every other guard is reused verbatim by routing through
    :func:`maybe_spawn_think`: the reason-scoped dedup TTL token, the per-day
    firehose ceiling, the forward stamp, the single decision event, and strict
    non-fatality. There is NO auto-grep heuristic - a dispatch happens only on
    this explicit call (AC5-FR).
    """
    environ = dict(os.environ if env is None else env)
    # Overlay the LIVE pointer so assemble_seed resolves THIS conversation's
    # transcript, never the node's birth origin (AC5-HP). An empty session_id
    # falls through to maybe_spawn_think's no-origin skip (the CLI verb rejects
    # that earlier with a clearer message).
    live_node = {
        **node,
        "source_harness": harness,
        "source_session_id": session_id,
        "source_cwd": cwd,
    }
    environ[_ENV_OVERRIDE] = "1"
    environ[_ENV_ATTENDED] = "spawn"
    # The live session id discriminates the dedup token + worker name so a LATER
    # conversation can re-dispatch the same node (the verb is repeatable, unlike
    # the once-per-moment birth/lifecycle triggers) - codex P2.
    suffix = (session_id or "").strip()[:8] or None
    return maybe_spawn_think(
        live_node,
        reason=REASON_CONVERSATIONAL,
        project_root=project_root,
        events_path=events_path,
        env=environ,
        invocation_suffix=suffix,
    )


# ---------------------------------------------------------------------------
# maybe_spawn_think() - the birth-hook decision matrix
# ---------------------------------------------------------------------------


def maybe_spawn_think(
    node: dict,
    *,
    reason: str = REASON_BIRTH,
    project_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    env: Optional[dict] = None,
    run_state: Optional[RunState] = None,
    invocation_suffix: Optional[str] = None,
    quiet: bool = False,
) -> ThinkSpawnResult:
    """Evaluate + execute the context /think spawn for a node at a trigger moment.

    ``reason`` names the trigger: ``birth`` (A1 default, byte-for-byte x-6a10),
    ``work-start`` or ``retro`` (A2 lifecycle, x-122a). It scopes the dedup token
    (``dispatch:think:<id>:<reason>`` so a node born + retro'd dispatches once per
    moment, not once total) and tags every decision event. Lifecycle reasons add
    a relevance filter (skip unless the transcript pointer resolves) so a
    context-free /think never fires on a high-volume lifecycle moment.

    Strictly non-fatal: any failure resolves to ``think_skipped{reason}`` and the
    host operation continues. Emits EXACTLY ONE decision event per evaluation once
    the gate is on; a gate-off evaluation is a complete no-op (no event - AC4-HP).
    """
    environ = os.environ if env is None else env
    ev_path = events_path if events_path is not None else _events_path(project_root)
    rs = run_state if run_state is not None else RunState()
    node_id = node.get("id")

    def skip(skip_reason: str, **extra) -> ThinkSpawnResult:
        # ``reason`` (event key) stays the SKIP reason for back-compat; the
        # trigger reason rides as ``trigger`` so no consumer of the existing
        # schema breaks (AC1-UI: one event, non-null reason+node_id).
        data: dict = {"reason": skip_reason, "trigger": reason}
        if node_id:
            data["node_id"] = node_id
        for k, v in extra.items():
            if v is not None:
                data[k] = v
        _emit(EVENT_SKIPPED, data, ev_path)
        return ThinkSpawnResult(
            "skipped", EVENT_SKIPPED, reason=skip_reason, node_id=node_id,
            presence=extra.get("presence"), resolved=extra.get("resolved"),
            detail=extra.get("detail"),
        )

    # 0. Gate. Off => complete no-op: no event, no spawn (AC4-HP).
    if not think_spawn_enabled(project_root=project_root, env=environ):
        return ThinkSpawnResult("noop", None, reason="disabled", node_id=node_id)

    # 1. Eligibility: a node must have a usable id.
    if not node_id:
        return skip("no-node-id")

    # 2. Eligibility: bulk roadmap/vision intake is excluded (Locked Decision 6).
    if node.get("roadmap_id") or node.get("vision_path"):
        return skip("bulk-intake")

    # 3. Eligibility: a node with no captured origin cannot carry a why
    #    (human-typed at a bare terminal lands here) -> skip{no-origin} (AC1-ERR).
    if not (node.get("source_session_id") or "").strip():
        return skip("no-origin")

    # 4. Blast-radius cap (AC4-EDGE): a bulk run over the cap skips the rest and
    #    logs the truncation (never silent).
    cap = _max_per_run(project_root)
    if rs.spawned >= cap:
        if not rs.truncation_logged:
            # quiet: same stream-pollution reason as the offer print below - a
            # bulk decompose --json over the cap must not leak this warning into
            # a captured JSON stream. Still mark it handled so the cap is enforced.
            if not quiet:
                print(
                    f"spawn_think: blast-radius cap reached ({cap}); "
                    f"skipping further /think spawns this run",
                    file=sys.stderr,
                )
            rs.truncation_logged = True
        return skip("cap-exceeded", detail=f"max_per_run={cap}")

    # 5. Presence + seed.
    presence = classify_presence(project_root=project_root, env=environ)
    seed = assemble_seed(node)

    # 5b. Relevance filter (A2, Locked Decision 3): a LIFECYCLE trigger fires only
    #     when the origin pointer resolves - a high-volume work-start/retro moment
    #     must not dispatch a context-free /think. Birth (A1) is unchanged: it
    #     still degrades to the stored (harness, sid, cwd) triple.
    if reason in _LIFECYCLE_REASONS and not seed.resolved:
        return skip("unresolved-pointer", presence=presence, resolved=False)

    # 6. Attended => offer a single handoff line by default; auto-spawn only when
    #    the operator opted in via config.think_spawn.attended: spawn (AC4-HP, B).
    #    Default 'offer' is byte-for-byte x-6a10.
    if presence == "attended" and _attended_mode(project_root, env=environ) == "offer":
        # quiet: a machine-mode caller (e.g. `decompose --json`) suppresses the
        # human-facing offer print so it can't pollute a captured JSON stream; the
        # durable EVENT_OFFERED below still fires, so the offer survives for
        # `fno backlog pick`.
        if not quiet:
            print(
                f"spawn_think: OFFER PENDING (nothing spawned). "
                f"Ask the operator whether to run `{seed.offer_line}` now, or skip.",
                file=sys.stderr,
            )
        _emit(
            EVENT_OFFERED,
            {"node_id": node_id, "trigger": reason, "presence": "attended",
             "resolved": seed.resolved, "offer_line": seed.offer_line},
            ev_path,
        )
        return ThinkSpawnResult(
            "offered", EVENT_OFFERED, node_id=node_id, presence="attended",
            resolved=seed.resolved, offer_line=seed.offer_line,
        )

    # 6b. Per-day firehose ceiling (A2, Locked Decision 3): bound total bg /think
    #     spawns per install per day. Checked only on the spawn path - an attended
    #     offer costs nothing. 0 disables the ceiling.
    day_cap = _daily_cap(project_root)
    if day_cap > 0 and _daily_count() >= day_cap:
        return skip("daily-cap", presence=presence, detail=f"daily_cap={day_cap}")

    # 7. Away (or attended-with-opt-in-spawn) => fire-and-forget bg /think. Dedup
    #    via a per-(node, reason) TTL bridge token so two triggers observing the
    #    SAME moment spawn at most one, while a node born + later retro'd still
    #    dispatches once per moment (AC4-FR).
    from fno.claims.core import ClaimHeldByOther, acquire_claim

    # The dedup token carries the same per-invocation discriminator as the worker
    # name (C/x-0a9c) so two DIFFERENT conversations dispatching the same node are
    # independent, while a retry from the SAME invocation is still deduped.
    dispatch_key = f"dispatch:think:{node_id}:{reason}"
    if invocation_suffix:
        dispatch_key = f"{dispatch_key}:{invocation_suffix}"
    holder = f"think-spawn:{os.getpid()}"
    if _claim_is_live(dispatch_key):
        return skip("already-claimed", presence=presence, node_id=node_id)
    try:
        acquire_claim(
            dispatch_key, holder, ttl_ms=_DISPATCH_TTL_MS,
            reason=f"context /think dispatch ({reason}) for {node_id}",
        )
    except ClaimHeldByOther:
        return skip("already-claimed", presence=presence)
    except Exception as exc:  # noqa: BLE001
        return skip("claim-error", presence=presence, detail=str(exc))

    node_cwd = node.get("_resolved_cwd") or node.get("cwd") or None
    node_slug = node.get("slug") or node.get("title")
    try:
        short_id = _spawn_think_worker(
            node_id,
            seed.prompt,
            node_cwd,
            node_slug,
            reason,
            invocation_suffix,
            model=_route_resolve.node_model(node, provider=node.get("provider")),
            provider=node.get("provider"),
        )
    except SpawnAlreadyRunning:
        _safe_release(dispatch_key, holder)
        return skip("already-claimed", presence=presence)
    except Exception as exc:  # noqa: BLE001 - AC3-ERR: spawn fail -> skip, no stamp
        _safe_release(dispatch_key, holder)
        return skip("spawn-failed", presence=presence, detail=str(exc))

    # 8. Loop-closing forward stamp: node now points forward to its /think thread
    #    AND the durable output path the worker writes to (B, x-5d51).
    rs.spawned += 1
    _bump_daily_count()
    _stamp_forward(node_id, short_id, project_root, output_path=seed.output_path or None)
    _emit(
        EVENT_SPAWNED,
        {"node_id": node_id, "trigger": reason, "think_session": short_id,
         "presence": presence, "resolved": seed.resolved,
         "output_path": seed.output_path or None,
         "agent_name": _worker_agent_name(node_id, node_slug, reason, invocation_suffix)},
        ev_path,
    )
    return ThinkSpawnResult(
        "spawned", EVENT_SPAWNED, node_id=node_id, presence=presence,
        resolved=seed.resolved, think_session=short_id,
    )
