"""Config-sourced spawn defaults, injected argv-level at the dispatch seam.

Every `fno agents spawn` passes this seam before the Rust/Python routing
fork (Locked Decision 9). Per field: explicit CLI flag > lane >
`agents.profiles.<verb>` (per-harness overlays inside) > `agents.defaults` >
built-in. A bare scalar `model` is scoped to the config `provider`'s harness,
never the ambient one; an explicit `-m/--model` always wins; a resolved
`--role` lane owns the model.
"""
from __future__ import annotations

import json
import random
import re
import sys
from typing import IO, Callable, List, Mapping, Optional, Sequence, Set, Tuple

# Flags that consume the FOLLOWING token. Scanning for our three flags skips a
# value flag's value so a value that looks like `--model` / `--effort` can never
# masquerade as one of ours. Mirrors client.rs VALUE_FLAGS + the short aliases
# typer exposes on the spawn verb.
_VALUE_FLAGS = frozenset(
    {
        "--provider", "-P", "--harness", "-H", "--model", "-m", "--effort",
        "--from", "--cwd", "-c",
        "--message", "--session-id", "--cc-session-id", "--channel-id", "--status",
        "--from-name", "--timeout", "-t", "--mode", "--substrate", "--permission-mode",
        "--output-format", "--monitor", "--tab",
        "--node",
    }
)

# The harness axis (the CLI binary) is --harness/-H only: --provider/-P names the
# model VENDOR, a different axis that never picks a binary, so it must not feed
# the provider-aware default scan.
_HARNESS_FLAGS = ("--harness", "-H")
_MODEL_FLAGS = ("--model", "-m")
_EFFORT_FLAGS = ("--effort",)


def _scan(args: Sequence[str]) -> Tuple[bool, Optional[str], bool, bool]:
    """One pass over a spawn argv (verb already stripped by the caller).

    Returns ``(harness_present, harness_value, model_present, effort_present)``.
    The scanned flag is ``-H/--harness``: this pass reads the HARNESS axis and
    never touches ``-P/--provider``, which the caller reads separately.
    Handles both `--flag value` and `--flag=value`; stops at the `--argv`
    payload boundary and at a bare `--` passthrough fence (: fenced
    tokens are the harness's own flags, never fno's); skips a value flag's value.
    """
    harness_present = model_present = effort_present = False
    harness_value: Optional[str] = None
    it = iter(args)
    for a in it:
        if a == "--argv" or a == "--":
            break
        key, eq, val = a.partition("=")
        if key in _HARNESS_FLAGS:
            harness_present = True
            harness_value = val if eq else next(it, None)
        elif key in _MODEL_FLAGS:
            model_present = True
            if not eq:
                next(it, None)
        elif key in _EFFORT_FLAGS:
            effort_present = True
            if not eq:
                next(it, None)
        elif key in _VALUE_FLAGS and not eq:
            next(it, None)  # skip this flag's value so it can't be misread
    return harness_present, harness_value, model_present, effort_present


# --------------------------------------------------------------------------- #
# Spawn argv normalization: three ergonomic cuts, one argv->argv pass.
#
# Runs at the front door (inside compose_spawn_argv, BEFORE config injection
# and BEFORE the runtime route/fork), so by the time either runtime parser sees
# the argv it is canonical: an explicit NAME, long-form `--resume <full-uuid>`,
# long-form `--substrate <s>`. Neither parser learns a new vocabulary.
# --------------------------------------------------------------------------- #

_SUBSTRATES = ("pane", "thread", "headless", "bg")

# Flags on `spawn` that consume the following token. Needed to tell a flag's
# VALUE apart from a positional when scanning for the NAME / substrate token. A
# missing entry would misread that flag's value as a positional (e.g. a Rust-path
# `--message bg` mis-parsed as a substrate token), so this unions the shared
# `_VALUE_FLAGS` (--message, --session-id, --from, --status, ...) with the
# spawn-only value options.
_SPAWN_VALUE_FLAGS = _VALUE_FLAGS | frozenset(
    {
        "--role", "--resume", "-r", "--add-dir", "--agent", "--tools",
        "--deny-tools", "--workspace", "--squad", "-s", "--split", "-x", "--tab",
        "--pane",
        "--node", "--node-reason", "--slug", "--plan", "--name", "--recorded-provider",
        # --route/--account/--crown were absent, so their VALUES read as positionals:
        # a nameless `spawn --route zai,glm-5.2` registered an agent named "zai,glm-5.2".
        # Kept in lockstep with cmd_spawn (test_spawn_value_flags_cover_every_value_option).
        "--route", "--account", "--promote", "--crown", "-k", "--dispatch-account",
        # --at's value (current|<pane>) must not read as a positional.
        "--at",
        # --portal's index is a value, never a prompt word.
        "--portal",
        # the sessions-row phase names a phase, not a prompt word.
        "--session-phase",
        # The join call site's per-worker policy file: its PATH is a value,
        # never a prompt word.
        "--sandbox-write-policy",
        # A retry budget is a duration, never a prompt word.
        "--wait",
        # A prompt-file PATH is never a prompt word.
        "--prompt-file",
        # The dispatch-next porcelain's pinned lane name is a value.
        "--mux-session",
    }
)


def extract_existing_pane(args: Sequence[str]) -> tuple[List[str], Optional[int]]:
    """Remove fno's control-plane pane target before Typer parses."""
    index = 0
    while index < len(args):
        token = args[index]
        if token in ("--argv", "--"):
            return list(args), None
        key, equals, raw = token.partition("=")
        if key == "--pane":
            if not equals:
                raw = args[index + 1] if index + 1 < len(args) else ""
            try:
                pane = int(raw)
            except ValueError as exc:
                raise ValueError(f"--pane needs an integer pane id, got {raw!r}") from exc
            out = list(args)
            del out[index:index + (1 if equals else 2)]
            return out, pane
        index += 1 + (not equals and token in _SPAWN_VALUE_FLAGS)
    return list(args), None

# Tokens that pin the substrate explicitly (a positional substrate word conflicts
# with any of these -> exit 2). `--headless`/`-p` and `-o/--once` mean headless;
# `-H` selects the harness and `-P` the vendor, so both are value flags here.
_EXPLICIT_SUBSTRATE_BOOLS = ("--headless", "-p", "-o", "--once")

#: A claude thread spawned with no message starts with no prompt: claude's job
#: state reads `needs: send a prompt to start` and the row holds a worker slot
#: for nothing. Printed by the seam (explicit substrate, both runtimes) and by
#: cmd_spawn (resolved substrate).
SEEDLESS_THREAD_REFUSAL = (
    "refusing a claude thread spawn with no message. Claude starts that session "
    "with no prompt, it waits for one forever, and its row holds a worker slot. "
    "Put the work in the message: fno agents spawn '/fno:target {node}' "
    "--name {name} --node {node} --substrate thread. A resume needs no message: "
    "pass --resume <uuid>. No worker launched."
)


def seedless_thread_refusal(
    harness: Optional[str],
    substrate: Optional[str],
    message: Optional[str],
    *,
    resume: Optional[str] = None,
    crown: bool = False,
    name: Optional[str] = None,
    node: Optional[str] = None,
) -> Optional[str]:
    """The refusal text for a fresh claude thread spawn with no message, else None.

    A resume continues a transcript and a crown spawn gets the reign verb typed
    later in dispatch, so neither needs a message here.
    """
    if harness != "claude" or substrate not in ("thread", "bg"):
        return None
    if (message or "").strip() or resume or crown:
        return None
    return SEEDLESS_THREAD_REFUSAL.format(name=name or "<name>", node=node or "<node>")

_UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
_SHORT_ID_RE = re.compile(r"^[0-9a-f]{8}$")

# Two-word slug lists for a nameless spawn (docker/heroku pattern). Curated
# lowercase-ascii, unambiguous read aloud; no external dependency, no config.
_SLUG_ADJ = (
    "amber", "brave", "calm", "clever", "coral", "cosmic", "crisp", "dapper",
    "eager", "fabled", "gentle", "glossy", "golden", "hardy", "jolly", "keen",
    "lively", "lucid", "mellow", "merry", "nimble", "noble", "plucky", "quiet",
    "rapid", "ruddy", "sage", "sleek", "snug", "spry", "stout", "sunny",
    "swift", "tidy", "vivid", "warm", "witty", "zesty", "bold", "bright",
)
_SLUG_NOUN = (
    "otter", "falcon", "willow", "cedar", "comet", "ember", "harbor", "meadow",
    "pebble", "quartz", "river", "summit", "thicket", "vale", "walrus", "yak",
    "badger", "bison", "cobra", "crane", "dingo", "eagle", "ferret", "gecko",
    "heron", "ibis", "jaguar", "koala", "lemur", "marten", "newt", "osprey",
    "puffin", "raven", "shrew", "tapir", "urchin", "viper", "wombat", "finch",
)


def placement_refusal(
    *,
    substrate: str,
    once: bool,
    squad: Optional[str],
    split: Optional[str],
    at: Optional[str],
    tab: Optional[str],
    bounded_placement: bool,
) -> Optional[str]:
    """The pane placement contract as one named refusal, or None when
    the combination is legal. Portal placement is Rust-owned (the runtime that
    runs the spawn validates its own flags); this seam no longer reads a
    --portal value."""
    placement_requested = (
        bounded_placement
        or squad is not None
        or split is not None
        or at is not None
        or tab is not None
    )
    if squad is not None and not squad.strip():
        return "--workspace/-s needs a nonblank workspace name"
    if bounded_placement and substrate == "bg":
        return "--bounded-placement selects its own tab for a pane; a thread cannot be bounded"
    if at is not None and substrate == "bg":
        return "--at applies only to --substrate pane (a thread has no calling pane)"
    if placement_requested and (substrate != "pane" or once):
        return (
            "--workspace/-s, --split/-x, --at, and --tab apply only to --substrate pane "
            "(bg/headless have no pane geometry)"
        )
    if split is not None and split not in ("left", "right", "up", "down"):
        return f"--split/-x must be left, right, up, or down (got {split!r})"
    if tab is not None and not tab.strip():
        return "--tab needs a nonblank selector or pane-group name"
    if bounded_placement and any(value is not None for value in (split, at, tab)):
        return (
            "--bounded-placement selects its own stable tab and cannot be combined "
            "with --split, --at, or --tab"
        )
    if at is not None:
        # `--at current` is the exact-anchor spelling (the mux CLI resolves it
        # from FNO_PANE, strict Refuse fallback). A numeric anchor is a
        # low-level `mux pane run` concern, never exposed here.
        if at != "current":
            return "--at must be `current` (the exact-anchor spelling)"
        if split is None:
            return "--at requires --split (the side to place on)"
    return None


def _has_explicit_substrate(toks: Sequence[str]) -> Optional[str]:
    """Return the substrate value if pinned by an explicit flag, else None.

    Stops at the ``--argv`` payload boundary and at a bare ``--`` seed fence
    like the other spawn scans: fenced tokens are prompt text, never flags.
    """
    it = iter(toks)
    for t in it:
        if t in ("--argv", "--"):
            break
        if t in _EXPLICIT_SUBSTRATE_BOOLS:
            return "headless"
        if t == "--substrate":
            return next(it, "")
        if t.startswith("--substrate="):
            return t.split("=", 1)[1]
    return None


def _positional_indices(toks: Sequence[str]) -> List[int]:
    """Indices of positional tokens (NAME, MESSAGE), skipping flags + their values.

    Stops at a bare ``--`` fence: the first fenced token still lands in the
    MESSAGE (click fills positionals in order), and the rest are the
    provider passthrough - provider tokens, never prompt positionals to refuse.
    """
    idxs: List[int] = []
    i = 0
    n = len(toks)
    while i < n:
        t = toks[i]
        if t == "--argv" or t == "--":
            break
        if t.startswith("-"):
            if "=" not in t and t in _SPAWN_VALUE_FLAGS:
                i += 2  # skip the flag and its value
                continue
            i += 1
            continue
        idxs.append(i)
        i += 1
    return idxs


def _demote_thread_uncarried_passthrough(toks: List[str], err: IO[str]) -> None:
    """Rewrite an explicit or injected thread/bg substrate to pane when the
    harness's thread lane cannot carry the fenced `--` tokens. Runs on
    operator argv and again after config injection, covering the Rust-routed
    lane the Python resolver never sees; the daemon-side harness_args parser
    stays the trust-boundary backstop. Headless keeps its tokens: the
    one-shot lanes carry them.
    """
    fence = next((i for i, t in enumerate(toks) if t == "--"), None)
    if fence is None:
        return
    if _has_explicit_substrate(toks) not in ("thread", "bg"):
        return
    from fno.agents.harness_map import thread_uncarried

    harness = _flag_value(toks, "--harness", "-H") or "claude"
    uncarried = thread_uncarried(harness, {}, toks[fence + 1 :])
    if uncarried is None:
        return
    for i, t in enumerate(toks[:fence]):
        if t == "--substrate" and i + 1 < len(toks):
            toks[i + 1] = "pane"
            break
        if t.startswith("--substrate="):
            toks[i] = "--substrate=pane"
            break
    print(
        f"fno agents spawn: substrate: pane (the {harness} thread lane "
        f"has no carrier for {uncarried})",
        file=err,
    )


def _mint_slug(existing: Set[str], rng: random.Random, err: IO[str]) -> str:
    """Best-effort collision-avoided ``adjective-noun`` slug.

    Regenerates up to 5 times on a registry hit; the flocked downstream check
    remains authoritative. Raises SystemExit(2) only if all 5 attempts collide.
    """
    for _ in range(5):
        slug = f"{rng.choice(_SLUG_ADJ)}-{rng.choice(_SLUG_NOUN)}"
        if slug not in existing:
            return slug
    print(
        "fno agents spawn: could not find a free auto-name after 5 tries; "
        "pass one explicitly",
        file=err,
    )
    raise SystemExit(2)


def _head_flag_value(head: Sequence[str], flags: Tuple[str, ...]) -> Optional[str]:
    """Read one flag's value from a pre-fence head (``--flag v``, ``--flag=v``,
    ``-f v``, ``-f v`` attached). Skips every other value flag's value so it
    cannot misread one, mirroring ``_scan``. First occurrence wins."""
    it = iter(head)
    for a in it:
        if a in ("--argv", "--"):
            break
        key, eq, val = a.partition("=")
        if key in flags:
            return val if eq else next(it, None) or None
        # Short attached form: -m<value> (no long-flag starts with a single dash).
        if (
            not eq
            and len(a) > 2
            and not a.startswith("--")
            and a[:2] in flags
        ):
            return a[2:]
        if key in _SPAWN_VALUE_FLAGS and not eq:
            next(it, None)
    return None


def _node_slug_from_graph(node: str) -> Tuple[Optional[str], Optional[str]]:
    """Best-effort graph read of a node's canonical id and slug.

    Returns ``(node_id, slug)``; a slug input normalizes to the id, like
    ``resolve_provenance``. Raises whatever the graph read raises - the CALLER
    decides the fallback, because a spawn must never die on a naming lookup.
    """
    from fno.graph.load import load_graph

    for rec in load_graph():
        if rec.get("id") == node or rec.get("slug") == node:
            return rec.get("id") or node, rec.get("slug") or None
    return node, None


def _mint_node_name(
    node: str,
    slug_flag: Optional[str],
    model: Optional[str],
    existing: Optional[Set[str]] = None,
) -> Optional[str]:
    """The ``t-<hex>-<slug>-<model>`` mint for a node-driven spawn.

    Routes through :func:`fno.agents.naming.dispatch_agent_name` - the single
    owner of the 64-char budget - with no source (a manual launch) and the
    model tag as the name's final segment. The mint is
    deterministic, so a name already taken by a live worker gets a ``-2``,
    ``-3``... suffix (the same collision-avoidance the adjective-noun mint
    retries for): a re-spawn on one node must not turn into a refusal. Any
    failure (graph read, budget, contract) returns ``None`` so pass 3 falls
    back to the adjective-noun mint: a spawn must never die on a naming lookup.
    """
    from fno.agents.naming import AgentNameError, dispatch_agent_name

    try:
        node_id, slug = _node_slug_from_graph(node)
    except Exception:
        return None
    if slug_flag:
        slug = slug_flag
    try:
        name = dispatch_agent_name(
            None, "t", node_id or node, slug=slug, model=model
        )
    except AgentNameError:
        return None
    if existing and name in existing:
        for n in range(2, 6):
            if len(name) + len(str(n)) + 1 > 64:
                return None
            if f"{name}-{n}" not in existing:
                return f"{name}-{n}"
        return None
    return name


def _read_registry_rows() -> list[object]:
    """Registry rows shared by name minting and profile-lane selection."""
    try:
        from fno.agents.registry import load_registry

        return list(load_registry())
    except Exception:
        return []


def normalize_spawn_args(
    args: Sequence[str],
    *,
    resolver: Optional[Callable[[str], Optional[str]]] = None,
    existing_names: Optional[Set[str]] = None,
    rng: Optional[random.Random] = None,
    stderr: Optional[IO[str]] = None,
) -> List[str]:
    """Canonicalize a ``spawn`` argv (verb at index 0); pure argv -> argv.

    Three passes (each sees the previous pass's output):

    1. A trailing positional that exact-matches ``pane|bg|headless`` becomes
       ``--substrate <token>`` (unless an explicit substrate is present -> exit 2).
    2. ``-r`` is the short flag for ``--resume``; its value may be a full uuid or
       an 8-hex short-id (resolved to the uuid; unresolvable/malformed -> exit 2).
       ``--resume`` with no substrate defaults the substrate to ``thread``.
    3. The single positional is the MESSAGE; the name rides ``--name`` and is
       minted (``adjective-noun``) when omitted. A second positional -> exit 2.

    Non-``spawn`` verbs and ``spawn --help`` pass through unchanged. Read-only
    (registry names, session resolver); writes no state.
    """
    out = list(args)
    if not out or out[0] != "spawn":
        return out
    for a in out[1:]:
        if a == "--argv":
            break
        if a in ("-h", "--help"):
            return out

    err = stderr if stderr is not None else sys.stderr
    # Split off the `--argv` provider payload: every pass operates on the fno-arg
    # HEAD only, and derived flags are appended before the payload, so a payload
    # token (e.g. the child command's own `--resume`) is never scanned or rewritten.
    body = out[1:]
    if "--argv" in body:
        cut = body.index("--argv")
        toks, payload = body[:cut], body[cut:]
    else:
        toks, payload = body, []

    # Pass 1: trailing substrate token.
    positions = _positional_indices(toks)
    if positions:
        last = positions[-1]
        tok = toks[last]
        if tok in _SUBSTRATES:
            explicit = _has_explicit_substrate(toks)
            if explicit is not None:
                print(
                    f"fno agents spawn: substrate given twice: positional {tok!r} "
                    f"and --substrate {explicit!r}",
                    file=err,
                )
                raise SystemExit(2)
            del toks[last]
            toks += ["--substrate", tok]

    # Pass 2: -r / --resume id widening + implied bg. The scan sees only the
    # pre-fence head: a fenced `--resume`/`-r` is the provider's flag,
    # and reading it here would append an implied `--substrate bg` under a
    # passthrough fence - or exit 2 on the provider's short-flag value.
    _fence = next((i for i, t in enumerate(toks) if t == "--"), None)
    head_toks = toks if _fence is None else toks[:_fence]
    resume_idxs = [
        i for i, t in enumerate(head_toks)
        if t in ("-r", "--resume")
        or t.startswith("--resume=")
        or (t.startswith("-r") and len(t) > 2)  # -r=ID and the Click -rID attached form
    ]
    if len(resume_idxs) > 1:
        print("fno agents spawn: resume given twice (-r / --resume)", file=err)
        raise SystemExit(2)
    if resume_idxs:
        i = resume_idxs[0]
        flag = toks[i]
        if "=" in flag:
            raw_value: Optional[str] = flag.split("=", 1)[1]
            value_at = None
        elif flag.startswith("-r") and len(flag) > 2:
            # Click attached short: -r<id>. Value is the rest of the token.
            raw_value = flag[2:]
            value_at = None
        else:
            value_at = i + 1
            raw_value = toks[value_at] if value_at < len(toks) else None
        if not raw_value or raw_value.startswith("-"):
            print("fno agents spawn: -r/--resume needs a session uuid or 8-hex short-id", file=err)
            raise SystemExit(2)
        low = raw_value.lower()
        resolved: Optional[str]
        if _UUID_RE.match(low):
            resolved = low
        elif _SHORT_ID_RE.match(low):
            resolve = resolver if resolver is not None else _default_resolver
            resolved = resolve(low)
            if not resolved:
                print(f"fno agents spawn: cannot resolve short-id {raw_value!r} to a session uuid", file=err)
                raise SystemExit(2)
        else:
            print(
                f"fno agents spawn: -r/--resume value {raw_value!r} is neither a "
                "full session uuid (8-4-4-4-12) nor an 8-hex short-id",
                file=err,
            )
            raise SystemExit(2)
        # Rewrite in place to the canonical `--resume <uuid>` form. Leave an
        # already-canonical `--resume <lowercase-uuid>` untouched so a fully
        # explicit argv passes through byte-identically (AC1-EDGE).
        if value_at is None:
            toks[i] = f"--resume={resolved}"
        elif not (flag == "--resume" and raw_value == resolved):
            toks[i] = "--resume"
            toks[value_at] = resolved
        # `--resume` implies thread; the splice lands BEFORE any `--` fence, never past it.
        if _has_explicit_substrate(toks) is None:
            cut = _fence if _fence is not None else len(toks)
            toks = toks[:cut] + ["--substrate", "thread"] + toks[cut:]

    # A thread lane carries only what its contract row maps: a spawn pinning
    # thread/bg with an unmapped fenced token demotes to the pane here, on
    # both runtimes (the Rust-routed lane never reaches the CLI's resolver).
    fence = next((i for i, t in enumerate(toks) if t == "--"), None)
    if fence is not None:
        _demote_thread_uncarried_passthrough(toks, err)

    # Pass 3: the NAME axis. `spawn` takes ONE positional and it is the MESSAGE;
    # the agent name is a handle the caller rarely picks, so it is minted unless
    # --name says otherwise. Canonicalize to `--name <n> <message>` so both
    # parsers read the same shape. A second positional is refused rather than
    # guessed at: under the old `<name> <message>` grammar it would silently
    # register an agent named after the prompt. The mint probe scans only the
    # pre-fence head: a passthrough `--name` is the PROVIDER's flag (claude's
    # session display name) and must not suppress fno's own name mint.
    head = toks if fence is None else toks[:fence]
    if not any(t == "--name" or t.startswith("--name=") for t in head):
        names = (
            existing_names
            if existing_names is not None
            else {str(getattr(e, "name", "")) for e in _read_registry_rows()}
        )
        # a node-driven spawn carries what an operator remembers. The
        # name is the registry row's ONLY node carrier, so a nodeless mint makes
        # the row unfindable by node or slug. Mint ``t-<node>-<slug>-<model>``
        # through the canonical owner when --node is present; any lookup
        # failure falls back to the adjective-noun mint, never a refusal.
        node = _head_flag_value(head, ("--node",))
        minted: Optional[str] = None
        if node:
            slug_flag = _head_flag_value(head, ("--slug",))
            model = _head_flag_value(head, _MODEL_FLAGS)
            if not model:
                route = _head_flag_value(head, ("--route",))
                # Canonical route spelling is provider/model (comma is
                # legacy); take the model half of either.
                model = (
                    route.replace(",", "/").rsplit("/", 1)[1].strip()
                    if route and ("/" in route or "," in route)
                    else None
                )
            minted = _mint_node_name(node, slug_flag, model, names)
        name = minted or _mint_slug(names, rng if rng is not None else random.Random(), err)
        toks = ["--name", name, *toks]
    extra = _positional_indices(toks)
    if len(extra) > 1:
        print(
            "fno agents spawn: takes one positional (the prompt); got "
            f"{len(extra)} ({', '.join(repr(toks[i]) for i in extra)}). The agent "
            "name moved to --name, so pass `--name <n> \"<prompt>\"`.",
            file=err,
        )
        raise SystemExit(2)

    return ["spawn", *toks, *payload]


def _default_resolver(short_id: str) -> Optional[str]:
    """Resolve an 8-hex claude short-id to its full session uuid (bg sessions).

    Uses the bounded-retry lane so a short-id issued while claude is still writing
    the session entry is not rejected on a transient miss (blueprint Concurrency).
    """
    try:
        from fno.agents.harnesses.claude import resolve_session_uuid_at_spawn

        return resolve_session_uuid_at_spawn(short_id)
    except Exception:
        return None


# --------------------------------------------------------------------------- #
# Per-verb profile resolution: a pure string rule over the seed's first
# token selects `config.agents.profiles.<verb>`, layered over `agents.defaults`.
# No content-based inference of any kind - only an explicit leading slash-verb.
# --------------------------------------------------------------------------- #

# Keep the old spelling on the canonical profile key for one release.
_VERB_ALIASES = {"do": "execute"}
# King work walks the crown slot whichever verb opens its seed.
_CROWN_VERBS = frozenset({"lead", "reign", "fno-me"})

# The one built-in answer to "what permission mode does an unattended worker
# get". Formerly config.agents.spawn_permission_mode's default; a constant now,
# because a second config key answering the same question is what let a bare
# mesh spawn land in auto while three other paths were pinned.
SPAWN_PERMISSION_BUILTIN = "bypassPermissions"


def _seed_slot(toks: Sequence[str]) -> Optional[tuple[int, str]]:
    """Where the MESSAGE seed lives: ``(index, form)`` into ``toks``, or
    None. One scan so the reader and the seam rewrite agree on the slot.
    Same rules as :func:`_seed_of`: the ``--argv`` boundary ends the head,
    and a bare ``--`` fence wins only in the legacy no-message idiom
    ."""
    i = 0
    while i < len(toks):
        t = toks[i]
        if t == "--argv":
            break
        if t == "--":
            head_pos = _positional_indices(toks[:i])
            if head_pos:
                return head_pos[0], "positional"
            if i + 1 < len(toks):
                return i + 1, "fenced"
            return None
        if t == "--message":
            if i + 1 < len(toks):
                return i + 1, "message"
            return None
        if t.startswith("--message="):
            return i, "message_eq"
        i += 1
    pos = _positional_indices(toks)
    if pos:
        return pos[0], "positional"
    return None


def _seed_of(toks: Sequence[str]) -> Optional[str]:
    """The MESSAGE seed: the ``--message`` value, else the sole positional
    (the name rides ``--name``); a thin reader over :func:`_seed_slot`, so
    reading and rewriting share one scan."""
    slot = _seed_slot(toks)
    if slot is None:
        return None
    i, form = slot
    text = toks[i]
    if form == "message_eq":
        return text.split("=", 1)[1]
    return text


def _role_of(toks: Sequence[str]) -> Optional[str]:
    """The ``--role`` value if present, else None. Stops at the ``--argv``
    payload boundary like the other spawn scans."""
    toks = list(toks)
    for i, t in _spawn_tokens(toks):
        if t == "--role":
            return toks[i + 1] if i + 1 < len(toks) else None
        if t.startswith("--role="):
            return t.split("=", 1)[1]
    return None


def _role_resolves(role: str, settings: object, env: Optional[Mapping[str, str]]) -> bool:
    """Whether ``role`` resolves to a real spawn route (a non-None env overlay).

    resolve_route is fail-SAFE, so this is True only when the role is routed AND
    its provider is configured with an anthropic endpoint AND a key is present.
    Any resolution error degrades to False (never bricks the spawn)."""
    from fno.agents.model_routing import resolve_route

    try:
        return resolve_route(role, settings=settings, env=env) is not None  # type: ignore[arg-type]
    except Exception:
        return False


def _has_permission_mode(toks: Sequence[str]) -> bool:
    """Whether the permission control is pinned, up to the ``--argv`` boundary
    and a bare ``--`` fence (: a fenced ``--permission-mode`` is the
    provider's flag, not fno's, and must not suppress a config default).
    ``--yolo``/``-Y`` count: they are the same knob as ``--permission-mode`` and
    are mutually exclusive with it downstream, so a config value injected
    alongside an explicit ``--yolo`` would exit 2 (explicit intent must win)."""
    for t in toks:
        if t == "--argv" or t == "--":
            break
        if (
            t in ("--permission-mode", "--yolo", "-Y")
            or t.startswith("--permission-mode=")
        ):
            return True
    return False


def _spawn_tokens(toks: Sequence[str]):
    """Yield ``(index, token)`` from ``toks`` up to the ``--argv`` boundary and
    any bare ``--`` seed fence (fenced tokens are prompt text), skipping any
    token that is another value-flag's consumed VALUE - so a literal occurrence
    of one flag can never be misread out of a different flag's value (e.g.
    ``--session-id --route`` names a session id of ``--route``, not a
    ``--route`` flag)."""
    it = enumerate(toks)
    for i, t in it:
        if t in ("--argv", "--"):
            break
        yield i, t
        if t in _SPAWN_VALUE_FLAGS and "=" not in t:
            next(it, None)


def _flag_present(toks: Sequence[str], flag: str) -> bool:
    """Whether a value ``flag`` appears as ``--flag`` or ``--flag=...`` in toks,
    up to the ``--argv`` payload boundary."""
    for _, t in _spawn_tokens(toks):
        if t == flag or t.startswith(flag + "="):
            return True
    return False


def _flag_value(toks: Sequence[str], *flags: str) -> Optional[str]:
    """Value of the first of ``flags`` found as ``--flag value``,
    ``--flag=value``, or - for a 2-char short flag like ``-P`` - the glued
    ``-Pvalue`` form typer/click also accepts, else None. Stops at the
    ``--argv`` payload boundary."""
    toks = list(toks)
    for i, t in _spawn_tokens(toks):
        for f in flags:
            if t == f:
                return toks[i + 1] if i + 1 < len(toks) else None
            if t.startswith(f + "="):
                return t.split("=", 1)[1]
            if len(f) == 2 and f[1] != "-" and t != f and t.startswith(f):
                return t[len(f):]
    return None


def _substrate_compatible(substrate: str, provider: str) -> bool:
    """A config-sourced substrate must be a KNOWN value AND honored by the
    resolved provider. The vocabulary lives in Rust
    (crates/fno-agents/src/effort_surface.rs), so this is a transport bridge;
    the ``provider == "claude"`` arm answers for an unavailable owner."""
    if substrate not in _SUBSTRATES:
        return False
    if substrate == "bg":
        substrate = "thread"
    if substrate != "thread":
        return True
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        answer = verb_call(
            "spawn-overlay",
            {"kind": "compat", "harness": provider, "substrate": substrate},
            VerbUnavailable,
        )
    except VerbUnavailable:
        return provider == "claude"
    return bool((answer.get("substrate") or {}).get("compatible"))


def _overlay_payload(obj: object) -> dict:
    """One spawn-defaults block as the verb's JSON view. ``model_dump()`` is
    the pydantic shape (extra=allow keeps smuggled keys visible); getattr is
    the test-fixture shape."""
    dump = getattr(obj, "model_dump", None)
    if callable(dump):
        return dump()
    out: dict = {}
    for k in (
        "provider", "model", "effort", "substrate", "permission_mode",
        "route", "account", "pane_group",
    ):
        out[k] = getattr(obj, k, "") or ""
    harness = getattr(obj, "harness", None)
    if isinstance(harness, Mapping):
        out["harness"] = {
            h: (b.model_dump() if callable(getattr(b, "model_dump", None)) else dict(b))
            for h, b in harness.items()
        }
    return out


# A model string's implied vendor, by prefix or tier word. A pure string
# opinion and never a routing input: the warning it drives is advisory, because
# the pairing is legal and --model is deliberate passthrough (cli.py).
def resolve_lane_vendor(
    argv: Sequence[str],
    env: Optional[Mapping[str, str]] = None,
    *,
    harness: Optional[str] = None,
) -> Optional[str]:
    """The model vendor a final spawn argv bills: route > provider > harness.
    The vocabulary and the judgment live in the spawn-overlay verb; this shim
    resolves the harness-side inputs (explicit arg, then -H, then dispatch
    inference from env) and reads the answer."""
    from fno.agents.spawn_overlay_client import (
        SpawnOverlayUnavailable,
        spawn_overlay_call,
    )

    toks = [str(t) for t in (list(argv)[1:] if argv else [])]
    env_harness = None
    if not (harness and str(harness).strip()):
        harness = _flag_value(toks, "--harness", "-H")
    if not (harness and str(harness).strip()):
        try:
            from fno.dispatch_flags import resolve_dispatch_harness

            env_harness = resolve_dispatch_harness(None, env=env)[0]
        except Exception:
            env_harness = "claude"
    try:
        return spawn_overlay_call(
            {
                "kind": "lane-vendor",
                "argv_tail": toks,
                "argv_head": argv[0] if argv else None,
                "harness": harness,
                "env_harness": env_harness,
            }
        ).get("vendor")
    except SpawnOverlayUnavailable:
        # No binary: no vendor opinion (the documented no-opinion answer).
        return None


def compose_spawn_argv(
    args: Sequence[str],
    *,
    env: Optional[Mapping[str, str]] = None,
    stderr: Optional[IO[str]] = None,
    apply_permission_builtin: bool = True,
    node_verb: Optional[str] = None,
) -> List[str]:
    """Return ``args`` with config spawn-defaults injected where absent.

    The composition lives in Rust (crates/fno-agents/src/spawn_compose.rs);
    this is the transport: normalize, project the argv scan, one verb call,
    apply the answer. Raises SystemExit on the verb's refusals (exit 2, or
    78 with the exhausted payload on stdout). The old seam's degrade-open
    stance survives as the unavailable-owner arm: a missing binary must not
    brick an otherwise valid spawn, so it prints one named line and returns
    the normalized argv.
    """
    out = list(args)
    if not out or out[0] != "spawn":
        return out
    # `spawn --help`/`-h` must always render help, even under a broken config.
    # Stop at the --argv boundary so a payload's own --help is not consumed.
    for a in out[1:]:
        if a == "--argv":
            break
        if a in ("-h", "--help"):
            return out

    err = stderr if stderr is not None else sys.stderr
    # Ergonomic normalization runs FIRST: the substrate-token / -r /
    # autogen-name rewrites consider only operator-supplied argv, so config
    # defaults injected by the verb never fight the token form.
    out = normalize_spawn_args(out, stderr=err)
    tail = out[1:]

    scan = _scan_projection(tail)
    facts: dict = {"role_resolves": None, "role_protected": None}
    if scan["role"]:
        try:
            from fno.agents.model_routing import PROTECTED_ROLES

            if scan["role"].strip().lower() in PROTECTED_ROLES:
                facts["role_protected"] = scan["role"].strip().lower()
        except Exception:  # noqa: BLE001 - the floor is advisory, never fatal
            pass

    payload = {
        "kind": "compose",
        "argv": list(out),
        "node_verb": node_verb,
        "env_node": (env or {}).get("FNO_NODE") or None,
        "permission_builtin": SPAWN_PERMISSION_BUILTIN if apply_permission_builtin else None,
        "scan": scan,
        "facts": facts,
        "verbose": "--verbose" in out[1 : next((i for i, t in enumerate(out) if t in ("--", "--argv")), len(out))],
    }
    try:
        from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call

        answer = spawn_overlay_call(payload, timeout=90)
    except SpawnOverlayUnavailable as exc:
        print(
            f"fno agents spawn: config defaults skipped (spawn-overlay unavailable: {exc})",
            file=err,
        )
        return out

    # The billing gate declared it needs the role answer (config model, free
    # axis, a role named): resolve ONCE and ask again. Common case: one verb
    # call, zero seam resolves; cmd_spawn's own resolve is the one.
    if answer.get("role_gate_needed"):
        facts["role_resolves"] = _role_resolves(scan["role"], None, env)
        payload["facts"] = facts
        answer = spawn_overlay_call(payload, timeout=90)

    for line in answer.get("stderr") or []:
        print(line, file=err)
    exit_code = int(answer.get("exit") or 0)
    if exit_code:
        if answer.get("stdout"):
            print(json.dumps(answer["stdout"]))
        raise SystemExit(exit_code)
    for event in answer.get("events") or []:
        from fno.agents import events

        events.emit("model_vendor_mismatch", **event)
    answer_argv = [str(t) for t in answer.get("argv") or out]
    # Injection can pin the substrate the operator left open, and the
    # Rust-routed lane never reaches the Python CLI's own refusal, so the
    # post-injection demote re-runs on the composed argv.
    if answer.get("injected"):
        _demote_thread_uncarried_passthrough(answer_argv, err)
    return answer_argv


def _scan_projection(tail: Sequence[str]) -> dict:
    """The scan projection the transport sends, one scanner call each. The
    scanners answer what the OPERATOR typed; the verb owns every config
    decision over the result."""
    has_harness, explicit_harness, has_model, has_effort = _scan(tail)
    explicit_vendor = _flag_value(tail, "--provider", "-P")
    explicit_route = _flag_present(tail, "--route")
    role = _role_of(tail)
    has_permission = _has_permission_mode(tail)
    permission_value = _flag_value(tail, "--permission-mode")
    if not permission_value and has_permission:
        # --yolo/-Y are the same knob as --permission-mode; the verb must see
        # them or it can hand a yolo spawn a harness the gate refuses.
        permission_value = "yolo"
    slot = _seed_slot(tail)
    return {
        "has_harness": has_harness,
        "explicit_harness": explicit_harness,
        "has_model": has_model,
        "has_effort": has_effort,
        "explicit_vendor": explicit_vendor,
        "explicit_route": explicit_route,
        "route_value": _flag_value(tail, "--route") if explicit_route else None,
        "model_value": _flag_value(tail, "--model", "-m") if has_model else None,
        "role": role,
        "explicit_substrate": _has_explicit_substrate(tail),
        "permission_value": permission_value,
        "has_permission": has_permission,
        "flag_node": _flag_value(tail, "--node"),
        "seed": _seed_of(tail),
        "seed_index": (slot[0] + 1) if slot else None,
        "seed_form": slot[1] if slot else None,
        "account_flag_present": _flag_present(tail, "--account"),
        "tab_flag_present": _flag_present(tail, "--tab"),
        "name": _flag_value(tail, "--name"),
        "positional_present": bool(_positional_indices(tail)),
        "spawn_tokens": [str(t) for t in tail],
    }


def spawn_seam_marker() -> str:
    """The marker token both bridges carry, straight after the spawn verb."""
    return f"--defaults-applied={routing_enforcement_state()}"


def routing_enforcement_state(settings: object = None) -> str:
    """The marker verdict: the config-blind binary's only record of the
    seam's decision. Read through the verb's policy mode; read failure
    degrades open, and a strict seam refuses upstream."""
    del settings
    try:
        from fno.route_resolve import _routing_enforced

        return "enforced" if _routing_enforced() else "unenforced"
    except Exception:  # noqa: BLE001 - unknown reads as legacy, never as strict
        return "unenforced"

# --------------------------------------------------------------------------- #
# The CLI's substrate posture gates, moved here from cmd_spawn (file budget):
# the value gate + bg alias, and the monitor gate.


def resolve_spawn_gates(substrate, monitor, *, once, harness):
    """Validate the substrate/monitor posture; canonicalize ``thread``->``bg``.

    Exit 2 on a value outside the closed set or a monitor combination without
    support (exactly claude+zai on a pane). The deprecated ``bg`` spelling
    warns and still works. Portal gates live on the Rust lane only: the
    Python parser must not advertise a flag whose placement it cannot run.
    """
    if substrate not in ("pane", "thread", "bg", "headless"):
        print(
            f"--substrate must be one of: pane, thread, headless (got {substrate})",
            file=sys.stderr,
        )
        raise SystemExit(2)
    if substrate == "bg":
        print("substrate 'bg' was retired; use --substrate thread", file=sys.stderr)
        raise SystemExit(2)
    if substrate == "thread":
        substrate = "bg"
    if monitor is not None and monitor != "happy":
        print(f"--monitor must be 'happy' (got {monitor!r})", file=sys.stderr)
        raise SystemExit(2)
    if monitor == "happy" and (substrate != "pane" or once):
        print(
            "--monitor happy is pane-only; bg and headless workers do not pass "
            "the happy launcher seam",
            file=sys.stderr,
        )
        raise SystemExit(2)
    if monitor == "happy" and harness != "claude":
        print(
            f"--monitor happy requires the claude harness; got harness {harness!r}",
            file=sys.stderr,
        )
        raise SystemExit(2)
    return substrate


