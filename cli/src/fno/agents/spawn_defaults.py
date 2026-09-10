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
    payload boundary and at a bare `--` passthrough fence (x-1caa: fenced
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
# Spawn argv normalization (x-f76e): three ergonomic cuts, one argv->argv pass.
#
# Runs at the front door (inside inject_spawn_defaults, BEFORE config injection
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
        "--node", "--slug", "--plan", "--name", "--recorded-provider",
        # x-6de8: --route/--account/--crown were absent, so their VALUES read as
        # positionals: a nameless `spawn --route zai,glm-5.2` registered an agent
        # named "zai,glm-5.2". Kept in lockstep with cmd_spawn's value options
        # (test_spawn_value_flags_cover_every_value_option pins the two together).
        "--route", "--account", "--crown", "-k", "--dispatch-account",
        # x-6928: --at's value (current|<pane>) must not read as a positional.
        "--at",
        # x-9b60: --portal's index is a value, never a prompt word.
        "--portal",
        # x-4342: the sessions-row phase names a phase, not a prompt word.
        "--session-phase",
        # The join call site's per-worker policy file: its PATH is a value,
        # never a prompt word.
        "--sandbox-write-policy",
    }
)

# Tokens that pin the substrate explicitly (a positional substrate word conflicts
# with any of these -> exit 2). `--headless`/`-p` and `-o/--once` mean headless;
# `-H` selects the harness and `-P` the vendor, so both are value flags here.
_EXPLICIT_SUBSTRATE_BOOLS = ("--headless", "-p", "-o", "--once")

#: x-1caa: the pane-only `--` passthrough refusal body, shared by this seam
#: (explicit-flag substrate, pre-config-injection, covers the Rust-routed lane)
#: and the Python CLI lane (resolved substrate incl. config defaults). One
#: string, two triggers - reword it here, not per lane.
PASSTHROUGH_PANE_ONLY = (
    "passthrough after -- is pane-only; the "
    "bg/headless argv builders carry none of the pane's provider "
    "refusals, so the tokens cannot be forwarded. Use --substrate pane "
    "(the default) or drop them."
)

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
    """The pane placement contract (x-3e38) as one named refusal, or None when
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
    MESSAGE (click fills positionals in order), and the rest are the x-1caa
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


def _refuse_off_pane_passthrough(toks: Sequence[str], err: IO[str]) -> None:
    """Refuse `--` passthrough tokens on an explicit bg/headless substrate
    (x-1caa AC7): those argv builders carry none of the pane's provider
    refusals, so forwarding there would be a second, unguarded surface.

    Passthrough is fenced tokens in EITHER shape: more than one token after
    the fence, or any fenced token beside a pre-fence positional message (the
    legacy flag-shaped-seed idiom is exactly ONE fenced token with NO message
    before the fence). Runs at the seam on operator argv and again after
    config injection, so a substrate that arrived by config default - which
    reroutes to the Rust lane before the Python CLI's own refusal can run -
    is refused here too.
    """
    fence = next((i for i, t in enumerate(toks) if t == "--"), None)
    if fence is None:
        return
    if _has_explicit_substrate(toks) not in ("thread", "bg", "headless"):
        return
    if len(toks) - fence - 1 > 1 or _positional_indices(toks[:fence]):
        print(f"fno agents spawn: {PASSTHROUGH_PANE_ONLY}", file=err)
        raise SystemExit(2)


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


def _model_tag(model: Optional[str]) -> Optional[str]:
    """The short per-model name tag: lowercase, every non-alphanumeric stripped
    (``glm-5.2`` -> ``glm52``). Kept short by hand rather than via
    ``slug_component`` so the tag never spends the name budget on hyphens."""
    if not model:
        return None
    return re.sub(r"[^a-z0-9]", "", model.lower()) or None


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
    """The ``t-<node>-<slug>-<model>`` mint for a node-driven spawn (x-b80d).

    Routes through :func:`fno.agents.naming.agent_name` - the single owner of
    the 64-char budget - with the model tag as discriminator. The mint is
    deterministic, so a name already taken by a live worker gets a ``-2``,
    ``-3``... suffix (the same collision-avoidance the adjective-noun mint
    retries for): a re-spawn on one node must not turn into a refusal. Any
    failure (graph read, budget, contract) returns ``None`` so pass 3 falls
    back to the adjective-noun mint: a spawn must never die on a naming lookup.
    """
    from fno.agents.naming import AgentNameError, agent_name

    try:
        node_id, slug = _node_slug_from_graph(node)
    except Exception:
        return None
    if slug_flag:
        slug = slug_flag
    try:
        name = agent_name(
            "t", node_id or node, slug=slug, discriminator=_model_tag(model)
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
       ``--resume`` with no substrate defaults the substrate to ``bg``.
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
    # pre-fence head (x-1caa): a fenced `--resume`/`-r` is the provider's flag,
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
        # `--resume` is bg-only: default the substrate when none was pinned.
        # Print the implied choice so the routing decision is never silent
        # (blueprint Silent-Failure-Hunter / Locked Decision 4). The flag pair
        # splices BEFORE any bare `--` fence (x-1caa): appended past it, click
        # reads it as passthrough positionals and the implied lane is lost.
        if _has_explicit_substrate(toks) is None:
            cut = _fence if _fence is not None else len(toks)
            toks = toks[:cut] + ["--substrate", "bg"] + toks[cut:]
            print("fno agents spawn: substrate: bg (implied by --resume)", file=err)

    # x-1caa: a bare `--` fence carries provider passthrough (the first fenced
    # token is the MESSAGE only in the legacy no-message idiom; click fills
    # positionals in order). The pane substrate splices those tokens into the
    # provider argv behind the composed-argv refusals; bg/headless build argv
    # in Rust with none of those guards, so forwarding there would be a second,
    # unguarded surface. Refuse here - this seam is the one front door both
    # runtimes share - rather than dropping the tokens or corrupting the seed.
    # A single fenced token with NO message before the fence stays the legacy
    # flag-shaped-seed idiom, untouched.
    fence = next((i for i, t in enumerate(toks) if t == "--"), None)
    if fence is not None:
        _refuse_off_pane_passthrough(toks, err)

    # Pass 3: the NAME axis. `spawn` takes ONE positional and it is the MESSAGE;
    # the agent name is a handle the caller rarely picks, so it is minted unless
    # --name says otherwise. Canonicalize to `--name <n> <message>` so both
    # parsers read the same shape. A second positional is refused rather than
    # guessed at: under the old `<name> <message>` grammar it would silently
    # register an agent named after the prompt. The mint probe scans only the
    # pre-fence head: a passthrough `--name` is the PROVIDER's flag (claude's
    # session display name) and must not suppress fno's own name mint (x-1caa).
    head = toks if fence is None else toks[:fence]
    if not any(t == "--name" or t.startswith("--name=") for t in head):
        names = (
            existing_names
            if existing_names is not None
            else {str(getattr(e, "name", "")) for e in _read_registry_rows()}
        )
        # x-b80d: a node-driven spawn carries what an operator remembers. The
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
# Per-verb profile resolution (x-3d5b): a pure string rule over the seed's first
# token selects `config.agents.profiles.<verb>`, layered over `agents.defaults`.
# No content-based inference of any kind - only an explicit leading slash-verb.
# --------------------------------------------------------------------------- #

_PROFILE_KEY_RE = re.compile(r"^[a-z0-9][a-z0-9_-]*$")
# Keep the old spelling on the canonical profile key for one release.
_VERB_ALIASES = {"do": "execute"}

# The one built-in answer to "what permission mode does an unattended worker
# get". Formerly config.agents.spawn_permission_mode's default; a constant now,
# because a second config key answering the same question is what let a bare
# mesh spawn land in auto while three other paths were pinned (x-7198).
SPAWN_PERMISSION_BUILTIN = "bypassPermissions"


def _seed_of(toks: Sequence[str]) -> Optional[str]:
    """The MESSAGE seed: the ``--message`` value, else the sole positional (the
    name rides ``--name``). A bare ``--`` fence makes the first token after it
    the seed - even when flag-shaped - ONLY in the legacy no-message idiom; a
    positional message before the fence outranks the fenced tail (x-1caa).
    Stops at the ``--argv`` payload boundary."""
    i = 0
    while i < len(toks):
        t = toks[i]
        if t == "--argv":
            break
        if t == "--":
            # x-1caa: a positional MESSAGE before the fence outranks the fenced
            # tail (click fills positionals in order); the first fenced token
            # is the seed only in the legacy no-message idiom. Reading the
            # fenced token here silently dropped the profile layer.
            head_pos = _positional_indices(toks[:i])
            if head_pos:
                return toks[head_pos[0]]
            return toks[i + 1] if i + 1 < len(toks) else None
        if t == "--message":
            return toks[i + 1] if i + 1 < len(toks) else None
        if t.startswith("--message="):
            return t.split("=", 1)[1]
        i += 1
    pos = _positional_indices(toks)
    return toks[pos[0]] if pos else None


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


def is_verb_seed(seed: Optional[str]) -> bool:
    """Whether ``seed``'s first token is a leading slash-verb, by a pure
    string rule: must start with ``/`` and contain no further ``/`` (an
    absolute path never matches); strip the ``/`` and an optional ``fno:``
    namespace; the remainder must be lowercase ``^[a-z0-9][a-z0-9_-]*$``.

    Shared by ``_profile_key`` (which profile row a spawn's seed selects) and
    the permission-mode built-in rung (x-7198): a slash-verb seed is
    fire-and-forget work, a seedless or prose seed is a conversation. The
    attended/unattended axis is DECLARED, never inferred (the response-time
    instrument was retracted: fno mail is injected as user-shaped text, so no
    measurement can tell operator chatter from fleet chatter)."""
    if not seed:
        return False
    parts = seed.split()
    if not parts:
        return False
    tok = parts[0]
    if not tok.startswith("/") or "/" in tok[1:]:
        return False
    rest = tok[1:]
    if rest.startswith("fno:"):
        rest = rest[len("fno:"):]
    return bool(_PROFILE_KEY_RE.match(rest))


def _profile_key(seed: Optional[str]) -> Optional[str]:
    """Derive the profile key from a seed's first token (see ``is_verb_seed``).

    A seed with no leading slash-verb - every king seed, and a seedless spawn -
    resolves to the literal key ``crown`` instead of None, so
    ``[agents.profiles.crown]`` reaches a crown spawn exactly like every other
    stage row."""
    if not is_verb_seed(seed):
        return "crown"
    rest = seed.split()[0][1:]  # type: ignore[union-attr]
    if rest.startswith("fno:"):
        rest = rest[len("fno:"):]
    return _VERB_ALIASES.get(rest, rest)


def _has_permission_mode(toks: Sequence[str]) -> bool:
    """Whether the permission control is pinned, up to the ``--argv`` boundary
    and a bare ``--`` fence (x-1caa: a fenced ``--permission-mode`` is the
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


def _grid_node(toks: Sequence[str], env: Optional[Mapping[str, str]] = None) -> Optional[dict]:
    """Read the node's difficulty and priority for the dispatch grid."""
    node_id = _flag_value(toks, "--node") or (env or {}).get("FNO_NODE")
    if not node_id:
        return None
    try:
        from fno.tracker.metadata import read_entries

        entries = read_entries("spawn_defaults._grid_node")
        for entry in entries:
            if isinstance(entry, Mapping) and entry.get("id") == node_id:
                return dict(entry)
    except Exception:  # noqa: BLE001 - the grid is advisory
        return None
    return None


def _substrate_compatible(substrate: str, provider: str) -> bool:
    """A config-sourced substrate must be a KNOWN value AND honored by the
    resolved provider. ``thread`` requires the harness's journey-proven fno
    driver (its spawn claim reads native); ``pane``/``headless`` are universal.
    ``bg`` is accepted as a deprecated alias for ``thread``.
    An unknown value (or ``thread`` on a non-thread provider) degrades open (warn, skip) -
    never injected to fail at the spawn parser (both exit 2 there otherwise)."""
    if substrate not in _SUBSTRATES:
        return False
    if substrate == "bg":
        substrate = "thread"
    if substrate != "thread":
        return True
    try:
        from fno.agents.harness_map import thread_seatable

        return thread_seatable(provider)
    except Exception:
        return provider == "claude"


def _permission_mappable(provider: str, mode: str, substrate: Optional[str]) -> bool:
    """Whether the resolved (provider, substrate) can honor a mapped
    permission-mode. Mirrors the spawn parser's own gate: claude honors it on
    every substrate; a non-claude provider maps it ONLY on the pane lane
    (bg/headless hardcode their own bypass and exit 2 on ``--permission-mode``).
    A config value that would be refused there degrades open (warn, skip)."""
    if provider == "claude":
        return True
    if substrate != "pane":
        return False
    try:
        from fno.agents.mux_spawn import permission_pane_tokens

        permission_pane_tokens(provider, mode)
        return True
    except Exception:
        return False


_LANE_FIELDS = frozenset(
    {"provider", "model", "effort", "substrate", "permission_mode", "route", "account", "pane_group"}
)


def _lane_value(lane: object, name: str) -> str:
    value = lane.get(name, "") if isinstance(lane, Mapping) else getattr(lane, name, "")
    return value.strip() if isinstance(value, str) else ""


def _overlays_present(defaults: object, profile: object, lane: object = None) -> bool:
    """A harness overlay table (or lane args) can carry the ONLY value this
    spawn injects, so an empty harness-blind scalar read must not end
    composition before the post-resolution reads run (x-8975)."""
    for obj in (defaults, profile):
        table = getattr(obj, "harness", None)
        if isinstance(table, Mapping) and table:
            return True
    if lane is not None:
        lv = (
            lane.get("args")
            if isinstance(lane, Mapping)
            else getattr(lane, "args", None)
        )
        if lv:
            return True
    return False


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


def _check_model_vendor_mismatch(
    argv: Sequence[str],
    err: IO[str],
    env: Optional[Mapping[str, str]] = None,
    *,
    model_source: Optional[str] = None,
) -> None:
    """Judge a model whose implied vendor the resolved lane does not match.

    WHAT THE CALLER TYPED AND WHAT CONFIG SUPPLIED ARE DIFFERENT FACTS, and
    ``model_source`` carries the config path when the model was INJECTED and
    None when the caller named it: a typed mismatch warns and proceeds (the
    passthrough is deliberate), an injected one REFUSES - nobody chose that
    pairing, and the worker it would start dies on its first inference.
    An explicit --route suppresses both. The judgment lives in the
    spawn-overlay verb; this shim keeps the Python side effects (event,
    stderr line, exit 2).
    """
    toks = [str(t) for t in argv[1:]]
    # Pure fast path: with no model named there is nothing to judge, and the
    # spawn must compose on installs with no fno-agents binary at all.
    if _flag_value(toks, "--model", "-m") is None:
        return
    # The lane's harness, resolved in the verb's precedence order: an explicit
    # -H flag wins, then dispatch inference from env (as a LATE input; the
    # argv head may still name the harness first).
    harness = _flag_value(toks, "--harness", "-H")
    env_harness = None
    if not (harness and str(harness).strip()):
        try:
            from fno.dispatch_flags import resolve_dispatch_harness

            env_harness = resolve_dispatch_harness(None, env=env)[0]
        except Exception:
            env_harness = "claude"
    from fno.agents.spawn_overlay_client import (
        SpawnOverlayUnavailable,
        spawn_overlay_call,
    )

    try:
        answer = spawn_overlay_call(
            {
                "kind": "model-vendor",
                "argv_tail": toks,
                "argv_head": argv[0] if argv else None,
                "harness": harness,
                "env_harness": env_harness,
                "model_source": model_source,
            }
        )
    except SpawnOverlayUnavailable as exc:
        # No binary: the check degrades open with a named line. The refusal
        # contract is kept wherever the verb actually ran.
        print(f"fno agents spawn: vendor check skipped ({exc})", file=err)
        return
    if answer.get("event"):
        from fno.agents import events

        events.emit("model_vendor_mismatch", **answer["event"])
    if answer.get("verdict") == "refuse":
        print(answer.get("message"), file=err)
        raise SystemExit(2)
    if answer.get("verdict") == "warn":
        print(answer.get("message"), file=err)


def inject_spawn_defaults(
    args: Sequence[str],
    *,
    settings: object = None,
    env: Optional[Mapping[str, str]] = None,
    stderr: Optional[IO[str]] = None,
    apply_permission_builtin: bool = True,
) -> List[str]:
    """Return ``args`` with config spawn-defaults injected where absent.

    Only acts on a `spawn` verb; returns the input unchanged for any other
    verb, or when the config load fails (a bad config must never brick
    spawning). Raises ``SystemExit(2)`` on an unknown config provider
    (AC5-ERR) and on a malformed slot; exits 78 on an on_exhausted=queue
    terminal. Config-sourced effort/substrate/permission_mode degrade open on
    an incompatible resolved provider (warn, skip); an explicit flag stays
    fail-closed downstream. ``apply_permission_builtin`` (default True) gates
    the ``SPAWN_PERMISSION_BUILTIN`` rung (x-7198); off for a probe that
    never launches (see ``retask.py``).
    """
    out = list(args)
    if not out or out[0] != "spawn":
        return out
    # `spawn --help`/`-h` must always render help, even under a broken config
    # (a bad provider would otherwise exit 2 here before help prints). Stop at
    # the --argv boundary so a payload's own --help is not consumed.
    for a in out[1:]:
        if a == "--argv":
            break
        if a in ("-h", "--help"):
            return out

    # Ergonomic normalization runs FIRST (x-f76e): the substrate-token / -r /
    # autogen-name rewrites consider only operator-supplied argv, so config
    # defaults injected below never fight the token form.
    out = normalize_spawn_args(out, stderr=stderr)
    err = stderr if stderr is not None else sys.stderr

    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:
            # A malformed config never bricks spawning (the ONE degrade-open
            # path). A successful load yields a valid SettingsModel whose
            # `.agents.defaults` always exists, so field access below is NOT
            # wrapped: a schema/wiring bug there must surface, not be masked
            # into an invisible no-op (AC5-FR).
            return out
    agents = settings.agents  # type: ignore[attr-defined]
    defaults = agents.defaults
    # Per-verb profile (x-3d5b): the seed's leading slash-verb selects a profile
    # layered OVER defaults, resolved field-wise into one effective view BEFORE
    # the injection below - so the provider-scoped model rule, effort degrade, and
    # unknown-provider refusal all run once, on the merged fields.
    seed = _seed_of(out[1:])
    verb = _profile_key(seed)
    profiles = getattr(agents, "profiles", None) or {}
    profile_verb = verb
    profile = profiles.get(verb) if verb else None
    if profile is None and verb:
        legacy_verb = next((old for old, new in _VERB_ALIASES.items() if new == verb), None)
        if legacy_verb:
            profile = profiles.get(legacy_verb)
            if profile is not None:
                profile_verb = legacy_verb
    # Overlay table guards (x-8975): the spawn-overlay verb owns them now -
    # an unknown harness name, or a ranking field inside an overlay, refuses
    # the composition from the same call that resolves the rungs. Scoped to
    # the rungs THIS spawn reads; an unrelated verb's typo must not block it.
    lane: Optional[object] = None
    lane_index: Optional[int] = None
    slot_candidate: Optional[dict] = None
    slot_chain: List[str] = []
    grid_node_entry: Optional[dict] = None
    grid_account_injected = False
    # Axis occupancy scanned ONCE, before the slot resolver: a lane named on
    # the command line changes the all-exhausted terminal (degrade, not
    # refuse), an occupied model axis stands the no-lanes grid down, and
    # -P/--provider counts as a named lane (it names the capped VENDOR).
    has_harness, explicit_harness, has_model, has_effort = _scan(out[1:])
    explicit_vendor = _flag_value(out[1:], "--provider", "-P")
    explicit_vendor_present = explicit_vendor is not None
    explicit_route = _flag_present(out[1:], "--route")
    role = _role_of(out[1:])
    _explicit_lane = bool(has_harness or explicit_route or explicit_vendor_present)
    explicit_substrate = _has_explicit_substrate(out[1:])
    explicit_permission_value = _flag_value(out[1:], "--permission-mode")
    if not explicit_permission_value and _has_permission_mode(out[1:]):
        # --yolo/-Y are the same knob as --permission-mode; the filter must
        # see them or it can hand a yolo spawn a harness the gate refuses.
        explicit_permission_value = "yolo"
    node_id_present = (
        _flag_value(out[1:], "--node") is not None or bool((env or {}).get("FNO_NODE"))
    )

    def _above_defaults(rung: Optional[str]) -> bool:
        return bool(rung) and rung != "agents.defaults"

    def _scalar_rung(name: str) -> Optional[str]:
        """Where a field's value would come from with no lane in play."""
        if profile is not None and (getattr(profile, name, "") or "").strip():
            return f"agents.profiles.{profile_verb}"
        if (getattr(defaults, name, "") or "").strip():
            return "agents.defaults"
        return None

    lanes_present = bool(
        profile is not None
        and (getattr(profile, "lanes", None) or getattr(profile, "by_difficulty", None))
    )
    # A profile LANE is atomic (occupies harness/model/effort); a bare FIELD
    # occupies only its axis. `model_occupied` is the NO-LANE view gating the
    # grid rung, which resolve_slot runs only when the verb declares no lanes.
    model_occupied = bool(
        has_model
        or explicit_vendor_present
        or explicit_route
        or _above_defaults(_scalar_rung("model"))
        or _above_defaults(_scalar_rung("route"))
    )
    slot_receipt: List[Tuple[str, str, str]] = []
    # (axis, value, rung, reason): a config-resolved axis this spawn did NOT get.
    suppressed: List[Tuple[str, str, str, str]] = []
    # The slot walk's structured extras (refusal, fingerprint), empty when unset.
    _slot_meta: dict = {}
    # Strict inventory: the resolver runs on EVERY spawn; a pin qualifies, never bypasses.
    enforced = routing_enforcement_state(settings) == "enforced"
    if lanes_present or not model_occupied or enforced:
        if not model_occupied or enforced:
            grid_node_entry = _grid_node(out[1:], env)
        capacity: Optional[dict[str, object]] = None
        _slot_inventory = None
        if lanes_present or grid_node_entry or enforced:
            try:
                from fno import route_resolve as _rr

                _slot_inventory = _rr.resolve_inventory()
                capacity = dict(_rr.runtime_capacity(inventory=_slot_inventory))
            except Exception:  # noqa: BLE001 - unknown capacity leaves defaults intact
                capacity = {}
        if capacity is not None:
            # Plan-presence, not plan quality: an unplanned target bills planning.
            grid_role: Optional[str] = None
            if grid_node_entry and verb == "target":
                grid_role = (
                    "execution"
                    if (grid_node_entry.get("plan_path") or "").strip()
                    else "planning"
                )
            elif verb in ("blueprint", "think"):
                grid_role = "planning"
            protected_name: Optional[str] = None
            try:
                from fno.agents.model_routing import PROTECTED_ROLES as _PROTECTED

                if role and role.strip().lower() in _PROTECTED:
                    protected_name = role.strip().lower()
            except Exception:  # noqa: BLE001 - the floor is advisory, never fatal
                protected_name = None
            try:
                from fno import route_resolve as _rr

                slot_candidate, slot_chain, _slot_verdict = _rr.resolve_slot(
                    profile_verb,
                    grid_node_entry,
                    capacity,
                    inventory=_slot_inventory,
                    settings=settings,
                    substrate=explicit_substrate,
                    permission_mode=explicit_permission_value,
                    constrain_harness=(explicit_harness or (
                        (getattr(profile, "provider", "") or "").strip() if profile is not None else ""
                    )).strip() or None,
                    role=grid_role,
                    protected_role=protected_name,
                    model_occupied=model_occupied,
                    explicit_model=has_model,
                    explicit_lane=_explicit_lane,
                    work_verb=verb,
                    explicit_model_value=_flag_value(out[1:], "--model", "-m") if has_model else None,
                    explicit_route_value=_flag_value(out[1:], "--route") if explicit_route else None,
                    explicit_vendor_value=(
                        (explicit_vendor or "").strip() or None if explicit_vendor_present else None
                    ),
                    meta=_slot_meta,
                )
            except Exception as _exc:  # noqa: BLE001 - legacy degrades; strict refuses
                if enforced:
                    print(
                        f"fno agents spawn: routing decision unavailable "
                        f"({_exc}); strict routing refuses without a complete decision",
                        file=err,
                    )
                    print("fno agents spawn: refusing; no worker launched", file=err)
                    raise SystemExit(2)
                slot_candidate = None
                slot_chain = []
        # Chain notes print and bill; the terminal rules.
        for _line in slot_chain:
            if _line.startswith(("slot skip", "slot note", "slot demote")):
                print(f"fno agents spawn: {_line}", file=err)
                suppressed.append(("slot", "", "", _line))

        def _refuse(msg: str) -> None:
            print(msg, file=err)
            print("fno agents spawn: refusing; no worker launched", file=err)
            raise SystemExit(2)

        if slot_chain:
            _terminal = slot_chain[-1]
            # Refusals arrive composed from the verb's refusal_terminal owner;
            # the exhausted queue rides its structured payload (exit 78).
            _refusal = _slot_meta.get("refusal") or {}
            if _refusal.get("text"):
                _refuse(f"fno agents spawn: {_refusal['text']}")
            if _slot_meta.get("exhausted"):
                print(json.dumps(_slot_meta["exhausted"]))
                raise SystemExit(78)
            if slot_candidate is not None and slot_candidate.get("lane_rung"):
                lane = slot_candidate["lane_fields"]
                lane_index = slot_candidate["lane_index"]
                slot_receipt.append(
                    ("slot", f"{slot_candidate['lane_rung']} {slot_candidate['lane']}", "routing")
                )
            elif slot_candidate is None and _terminal.startswith("slot="):
                slot_receipt.append(("slot", _terminal[len("slot="):], "routing"))
            else:
                # The no-lanes grid path: the terminal is the grid's own
                # vocabulary (no-inventory-declared, no-band-candidate, ...).
                slot_receipt.append(("grid", _terminal, "routing"))
    elif node_id_present:
        # The model axis is taken and the verb declares no lanes: the grid
        # stands down loudly rather than in silence.
        slot_receipt.append(("grid", "grid=model-axis-occupied", "routing"))

    # A lane is a COMPLETE coordinate: route/model stop at the lane (a codex
    # lane inheriting a zai route builds an argv cli.py refuses); postures fall through.
    _LANE_EXCLUSIVE = ("route", "model")

    def field(name: str) -> Tuple[str, Optional[str]]:
        """Effective value + source rung: lane > profile > defaults, harness-
        blind (the two harness rungs live in the verb's answer; this read is
        exact whenever no overlay table exists, which gates the verb path)."""
        if lane is not None and lane_index is not None:
            lv = _lane_value(lane, name)
            if lv:
                return lv, f"agents.profiles.{profile_verb}.lanes[{lane_index}]"
            if name in _LANE_EXCLUSIVE:
                return "", None
        if profile is not None:
            pv = (getattr(profile, name, "") or "").strip()
            if pv:
                return pv, f"agents.profiles.{profile_verb}"
        dv = (getattr(defaults, name, "") or "").strip()
        if dv:
            return dv, "agents.defaults"
        return "", None

    cfg_harness, provider_rung = field("provider")
    cfg_model, model_rung = field("model")
    cfg_effort, effort_rung = field("effort")
    cfg_substrate, substrate_rung = field("substrate")
    cfg_permission, permission_rung = field("permission_mode")
    if not cfg_permission and apply_permission_builtin and is_verb_seed(seed):
        cfg_permission, permission_rung = SPAWN_PERMISSION_BUILTIN, "builtin.autonomous"
    cfg_route, route_rung = field("route")
    cfg_account, account_rung = field("account")
    cfg_pane_group, pane_group_rung = field("pane_group")
    inject: List[str] = []
    # (axis, value, source) - source is f"{rung}.{field}"; axis is the
    # coordinate the field actually feeds (provider feeds "harness").
    from_config: List[Tuple[str, str, str]] = []
    _resolved: dict = {}

    effort_occupied = bool(
        has_effort or _above_defaults(_scalar_rung("effort")) or lane is not None
    )
    # A grid-branch candidate (no lane_rung) is an atomic harness/model/effort
    # TRIPLE injected below; a lane candidate feeds field() instead, one rung
    # per field with the seam's compatibility checks intact.
    grid_candidate: Optional[dict[str, str]] = (
        slot_candidate
        if slot_candidate is not None and not slot_candidate.get("lane_rung")
        else None
    )
    from_config.extend(slot_receipt)

    if not (
        cfg_harness or cfg_model or cfg_effort or cfg_substrate or cfg_permission
        or cfg_route or cfg_account or cfg_pane_group
    ) and grid_candidate is None and not from_config and not _overlays_present(
        defaults, profile, lane
    ):
        # No config field resolved at all, so any --model here was typed.
        _check_model_vendor_mismatch(out, err, env)
        _emit_defaults_applied(
            out, profile_verb, seed, locals(), from_config, suppressed,
        )
        return out

    # A spawn carrying --role whose lane resolves to a real route is billed on
    # that route: the route owns the model via env (ANTHROPIC_*), so a config
    # --model sourced here would collide with it. This is the five-opus-workers
    # defect - a worker believed cheap and billed expensive - so the model
    # branch below skips injection when resolve_route returns a real route.

    # A grid candidate is an atomic harness/model/effort TRIPLE. Mark the
    # occupied coordinates so lower default rungs cannot split or overwrite
    # the decision; effort joins the pair only when its axis was free and the
    # row's harness has an effort surface (the resolver omitted it otherwise).
    if grid_candidate is not None:
        inject += ["--harness", grid_candidate["harness"], "--model", grid_candidate["model"]]
        from_config.append(("grid", f"{grid_candidate['harness']}/{grid_candidate['model']}", "difficulty-grid"))
        has_harness = True
        has_model = True
        if grid_candidate.get("effort") and not effort_occupied:
            inject += ["--effort", grid_candidate["effort"]]
            from_config.append(("effort", grid_candidate["effort"], "difficulty-grid"))
            has_effort = True
        # The row's route rides beside the model it belongs to; a route or
        # vendor pinned on argv is never overwritten.
        if (
            grid_candidate.get("route")
            and not explicit_route
            and not explicit_vendor_present
        ):
            inject += ["--route", grid_candidate["route"]]
            from_config.append(("route", grid_candidate["route"], "difficulty-grid"))
        # The capacity pick read the row account's quota; claude-only at the CLI.
        if grid_candidate.get("account") and not _flag_present(out[1:], "--account"):
            if grid_candidate["harness"] == "claude":
                inject += ["--account", grid_candidate["account"]]
                from_config.append(
                    ("account", grid_candidate["account"], "difficulty-grid")
                )
                grid_account_injected = True
            else:
                print(
                    f"fno agents spawn: account skipped (claude-only, grid "
                    f"harness {grid_candidate['harness']!r}); "
                    f"{grid_candidate['account']!r} ignored",
                    file=err,
                )
        _resolved["v"] = grid_candidate["harness"]

    # Lazy resolved-target HARNESS for the substrate/permission compatibility
    # checks: explicit -H > the merged config `provider` field (which carries a
    # harness) > harness inference.
    def resolved_harness() -> Optional[str]:
        if "v" not in _resolved:
            if explicit_harness and explicit_harness.strip():
                _resolved["v"] = explicit_harness.strip()
            elif cfg_harness:
                _resolved["v"] = cfg_harness
            else:
                try:
                    from fno.dispatch_flags import resolve_dispatch_harness

                    _resolved["v"] = resolve_dispatch_harness(None, env=env)[0]
                except Exception:
                    _resolved["v"] = None
        return _resolved["v"]

    # Explicit-flag reads needed before the harness injection below: the
    # route-collision refusal (AC2-HP) must see whether the caller's argv is
    # already route-shaped (-P vendor --model m, or --route v) BEFORE a
    # config-sourced harness gets injected, so a refusal leaves the argv
    # untouched and nothing spawns. A bare explicit -m/--model with no -P
    # already names the model half of a route, so injecting a config route on
    # top would carry a DIFFERENT vendor's model behind the explicit one
    # (cmd_spawn does not reject that combination the way it rejects -P+-m
    # against --route, so a bare -m slipped through here and reached the
    # routed vendor's endpoint asking for a model it likely doesn't have -
    # the exact invisible-billing shape this field exists to kill). Also
    # covers -P/--provider + -m/--model, which carries the same two pieces of
    # information as --route and cmd_spawn rejects two route spellings
    # together. A bare explicit -P/--provider (vendor) with no -m is NOT yet
    # route-shaped (AC4-EDGE): cmd_spawn rejects vendor + --route together
    # ("two spellings of one route") BEFORE it ever reaches the "add --model"
    # check, so a config route or a route-collision refusal here would turn a
    # helpful "add --model" error into a confusing one on an argv the
    # operator never paired with a route at all.
    explicit_model_present = has_model
    if cfg_harness and not has_harness:
        from fno.agents.harnesses import READABLE_PROVIDERS

        if cfg_harness not in READABLE_PROVIDERS:
            # READABLE_PROVIDERS is a HARNESS roster despite its name (its own
            # rename crosses the Rust KNOWN_PROVIDERS parity pin), so the
            # refusal names the axis the value has to satisfy, not the roster's
            # spelling. The config key keeps its documented `provider` spelling.
            print(
                f"fno agents spawn: config.{provider_rung}.provider = "
                f"{cfg_harness!r} is not a known harness; valid: "
                f"{', '.join(READABLE_PROVIDERS)}",
                file=err,
            )
            raise SystemExit(2)

        # AC2-HP: the profile is about to fill the HARNESS axis (nothing on
        # the caller's argv set it - has_harness is False, or we would not
        # be here). If the caller's argv is already route-shaped, that route
        # only the claude harness can carry, and a non-claude profile harness
        # makes it unusable. This is not a precedence bug (no explicit flag is
        # overwritten); it is a cross-axis collision, so say everything: the
        # config path, the value, the axis it set, and the caller's own flags
        # in the caller's own spelling.
        route_shaped = explicit_route or (explicit_vendor_present and explicit_model_present)
        if route_shaped and cfg_harness != "claude":
            if explicit_route:
                caller_spelling = f"--route {_flag_value(out[1:], '--route')}"
            else:
                model_val = _flag_value(out[1:], "--model", "-m")
                caller_spelling = f"-P {explicit_vendor} --model {model_val}"
            print(
                f"fno agents spawn: config.{provider_rung}.provider = {cfg_harness!r} "
                "sets the HARNESS axis,\n"
                "and a route only the claude harness can carry is already on your "
                "command line\n"
                f"({caller_spelling}).\n"
                "Nothing you passed set the harness: -P names the model vendor, a "
                "different axis,\n"
                "so the profile filled it.\n"
                f"Pass -H claude to keep your route, or clear {provider_rung}.provider.",
                file=err,
            )
            raise SystemExit(2)

        inject += ["--harness", cfg_harness]
        from_config.append(("harness", cfg_harness, f"{provider_rung}.provider"))  # type: ignore[arg-type]

    # Billing axes (route/account/model, ruling 4): decided by the Rust owner
    # (crates/fno-agents/src/spawn_axes.rs): one round trip over the projected
    # facts returns injections, receipts and skip reasons verbatim.
    route_injected = False
    if cfg_route or cfg_account or cfg_model:
        from fno.agents.spawn_axes_client import SpawnAxesUnavailable, spawn_axes_call

        _role_route = False
        if cfg_model and not has_model and role:
            _role_route = bool(_role_resolves(role, settings, env))
        _target: Optional[str] = None
        _target_failed = False
        if cfg_model and not has_model and not explicit_route and not explicit_vendor_present and not _role_route:
            from fno.dispatch_flags import resolve_dispatch_harness

            try:
                _target = (
                    explicit_harness.strip()
                    if explicit_harness and explicit_harness.strip()
                    else cfg_harness or resolve_dispatch_harness(None, env=env)[0]
                )
            except Exception:
                # Degrade open (AC5-FR): no target, no basis to inject.
                _target = None
                _target_failed = True
        try:
            _axes = spawn_axes_call({
                "route": {"value": cfg_route, "rung": route_rung},
                "account": {"value": cfg_account, "rung": account_rung},
                "model": {"value": cfg_model, "rung": model_rung},
                "explicit_route": explicit_route,
                "explicit_vendor_present": explicit_vendor_present,
                "explicit_vendor": explicit_vendor or "",
                "explicit_model_present": explicit_model_present,
                "has_model": has_model,
                "grid_candidate_present": grid_candidate is not None,
                "slot_chain": slot_chain,
                "grid_account_injected": grid_account_injected,
                "account_flag_present": _flag_present(out[1:], "--account"),
                "prov": (resolved_harness() or "") if cfg_account else "",
                "role": role, "role_resolves": _role_route,
                "cfg_harness": cfg_harness or "",
                "harness_target": _target, "harness_target_failed": _target_failed,
            })
        except SpawnAxesUnavailable as _exc:
            # Degrade open (AC5-FR), the seam's own stance for config-sourced
            # fields: a missing owner must not brick an otherwise valid spawn.
            print(
                f"fno agents spawn: billing axes skipped (spawn-axes "
                f"unavailable: {_exc})",
                file=err,
            )
            _axes = {}
        for _line in _axes.get("messages") or []:
            print(_line, file=err)
        inject += [str(t) for _pair in _axes.get("inject") or [] for t in _pair]
        from_config.extend([tuple(e) for e in _axes.get("applied") or []])
        suppressed.extend([tuple(e) for e in _axes.get("suppressed") or []])
        route_injected = bool(_axes.get("route_injected"))

    # The spawn-overlay verb owns the harness-keyed rungs (x-8975): one
    # round-trip answers effort/substrate/permission plus the ONE bundle and
    # refuses a bad overlay. Gated on an overlay table (or lane args) being
    # present, so an overlay-free spawn pays zero subprocesses and the
    # harness-blind field() reads below answer it exactly.
    _overlay_answer: Optional[dict] = None
    if _overlays_present(defaults, profile, lane):
        from fno.agents.spawn_overlay_client import (
            SpawnOverlayUnavailable,
            spawn_overlay_call,
        )

        _lv = (
            lane.get("args") if isinstance(lane, Mapping) else getattr(lane, "args", None)
        ) if lane is not None else None
        try:
            _overlay_answer = spawn_overlay_call(
                {
                    "kind": "overlay",
                    "verb": profile_verb or "",
                    "harness": resolved_harness() or "",
                    "defaults": _overlay_payload(defaults) if defaults is not None else {},
                    "profile": _overlay_payload(profile) if profile is not None else None,
                    "lane": {"args": list(_lv)} if _lv else None,
                    "lane_index": lane_index,
                    "argv_tail": [str(t) for t in out[1:]],
                }
            )
        except SpawnOverlayUnavailable as exc:
            # Transport unavailable (no fno-agents binary): degrade open like
            # every config-sourced field here. Only the verb's own VERDICT
            # refusal below fails the spawn.
            print(
                f"fno agents spawn: harness-keyed defaults skipped ({exc})",
                file=err,
            )
        if _overlay_answer is not None and _overlay_answer.get("refusal"):
            print(_overlay_answer["refusal"], file=err)
            raise SystemExit(2)

    def _seamed(name: str) -> Tuple[str, Optional[str]]:
        """Lane first (a lane is a complete coordinate), then the harness-keyed
        answer, then the harness-blind scalars: field()'s order with the two
        harness rungs spliced in above it."""
        if lane is not None and lane_index is not None:
            lv = _lane_value(lane, name)
            if lv:
                return lv, f"agents.profiles.{profile_verb}.lanes[{lane_index}]"
        entry = (_overlay_answer or {}).get("effective", {}).get(name)
        if entry:
            return entry["value"], entry["rung"]
        return field(name)

    if not has_effort:
        # Effort surface depends on the RESOLVED HARNESS, not the vendor, so
        # the value is re-read through the harness rungs HERE, after the grid
        # or slot has settled the harness (x-8975) - a codex-keyed overlay
        # effort must win on codex while the scalar still answers claude.
        cfg_effort, effort_rung = _seamed("effort")

    # Mechanical axes ride the same owner; the pane-group stays here (its
    # placement judgment is the spawn-overlay verb's).
    explicit_substrate = _has_explicit_substrate(out[1:])
    _effort_reason: Optional[str] = None
    if cfg_effort:
        from fno.agents.mux_spawn import effort_tokens

        try:
            effort_tokens(resolved_harness() or "", cfg_effort)
        except Exception as _exc:
            _effort_reason = str(_exc)
    if explicit_substrate is None:
        cfg_substrate, substrate_rung = _seamed("substrate")
    else:
        cfg_substrate, substrate_rung = "", None
    _has_permission = _has_permission_mode(out[1:])
    if not _has_permission:
        # Re-read through the harness rungs (x-8975): an empty re-read keeps
        # the harness-blind read alive: that is the builtin.autonomous rung
        # (x-7198), which field() cannot see.
        h_permission, h_rung = _seamed("permission_mode")
        if h_permission:
            cfg_permission, permission_rung = h_permission, h_rung
    prov = resolved_harness() or ""
    _substrate_unknown = bool(cfg_substrate) and cfg_substrate not in _SUBSTRATES
    _substrate_ok = bool(prov) and bool(cfg_substrate) and _substrate_compatible(cfg_substrate, prov)
    _pane_tokens_ok = False
    if cfg_permission and prov and _permission_mappable(prov, cfg_permission, "pane"):
        _pane_tokens_ok = True
    _axes = {}
    if cfg_effort or cfg_substrate or cfg_permission:
        from fno.agents.spawn_axes_client import SpawnAxesUnavailable, spawn_axes_call

        try:
            _axes = spawn_axes_call({
                "effort": {"value": cfg_effort, "rung": effort_rung},
                "has_effort": has_effort, "effort_reason": _effort_reason or "",
                "substrate": {"value": cfg_substrate or "", "rung": substrate_rung},
                "explicit_substrate": explicit_substrate or "",
                "substrate_unknown": _substrate_unknown, "substrate_ok": _substrate_ok,
                "substrate_valid_list": ", ".join(_SUBSTRATES),
                "permission_mode": {"value": cfg_permission, "rung": permission_rung},
                "has_permission": _has_permission, "pane_tokens_ok": _pane_tokens_ok,
                "prov": prov,
            })
        except SpawnAxesUnavailable as _exc:
            print(
                f"fno agents spawn: mechanical axes skipped (spawn-axes "
                f"unavailable: {_exc})",
                file=err,
            )
            _axes = {}
    for _line in _axes.get("messages") or []:
        print(_line, file=err)
    inject += [str(t) for _pair in _axes.get("inject") or [] for t in _pair]
    from_config.extend([tuple(e) for e in _axes.get("applied") or []])
    suppressed.extend([tuple(e) for e in _axes.get("suppressed") or []])
    injected_substrate = _axes.get("injected_substrate") or None

    # Harness bundle (x-8975): the verb's ONE bundle answer (lane args >
    # profile harness overlay > defaults harness overlay, never concatenated)
    # lands behind the -- passthrough fence at the argv TAIL, so the caller's
    # own pre-fence tokens stay pre-fence; a boundary the caller already typed
    # displaces the configured bundle (the verb names it), and the off-pane
    # gate below re-reads the final argv, so a bundle on an explicit
    # bg/headless substrate is refused exactly like a typed one. The bundle
    # tokens ride the axes answer's bundle_inject (argv tail), never the head
    # inject.
    _bundle_inject: List[str] = []
    _pg_unavailable = ""
    _pg_answer: Optional[dict] = None
    _bundle_json = (_overlay_answer or {}).get("bundle")
    _positional_present = bool(_positional_indices(out[1:]))
    _axes = {}
    if cfg_pane_group or isinstance(_bundle_json, dict):
        from fno.agents.spawn_axes_client import SpawnAxesUnavailable, spawn_axes_call

        _pg_rung = f"{pane_group_rung}.pane_group"
        _pg_unavailable = ""
        if cfg_pane_group and not _flag_present(out[1:], "--tab"):
            # Placement judgment lives in the verb; degrade open on missing it.
            try:
                from fno.agents.spawn_overlay_client import (
                    SpawnOverlayUnavailable,
                    spawn_overlay_call,
                )

                _pg = spawn_overlay_call(
                    {
                        "kind": "pane-group",
                        "group": cfg_pane_group,
                        "rung": _pg_rung,
                        "eff_substrate": explicit_substrate or injected_substrate or "pane",
                        "argv_tail": [t for _, t in _spawn_tokens(out[1:])],
                    }
                )
                _pg_answer = _pg
            except SpawnOverlayUnavailable as _exc:
                _pg_unavailable = str(_exc)
        try:
            _axes = spawn_axes_call({
                "pane_group": {"value": cfg_pane_group, "rung": pane_group_rung},
                "tab_flag_present": _flag_present(out[1:], "--tab"),
                "pane_group_pg_rung": _pg_rung,
                "pane_group_unavailable": _pg_unavailable,
                "pane_group_answer": _pg_answer,
                "bundle": _bundle_json, "positional_present": _positional_present,
            })
        except SpawnAxesUnavailable as _exc:
            print(
                f"fno agents spawn: pane/bundle skipped (spawn-axes "
                f"unavailable: {_exc})",
                file=err,
            )
            _axes = {}
        for _line in _axes.get("messages") or []:
            print(_line, file=err)
        inject += [str(t) for _pair in _axes.get("inject") or [] for t in _pair]
        from_config.extend([tuple(e) for e in _axes.get("applied") or []])
        suppressed.extend([tuple(e) for e in _axes.get("suppressed") or []])
        _bundle_inject = [
            str(t) for _pair in _axes.get("bundle_inject") or [] for t in _pair
        ]
        if _axes.get("out_tail_append_empty"):
            out = [*out, ""]

    if from_config:
        # AC9-UI / AC1-HP: config-sourced routing is never invisible; name the
        # AXIS a field feeds (not the field name), its value, and the config
        # path - "provider=agents.profiles.target" reads as though a provider
        # was set to a profile, when what actually happened is harness=codex.
        print(
            "fno agents spawn: applied "
            + ", ".join(f"{a}={v} ({s})" for a, v, s in from_config),
            file=err,
        )
    if inject:
        out = [out[0], *inject, *out[1:]]
    if _bundle_inject:
        out = [*out, *_bundle_inject]
    if inject or _bundle_inject:
        # x-1caa: injection can pin the substrate the operator left open, and
        # the Rust-routed lane never reaches the Python CLI's own refusal - so
        # the off-pane passthrough gate re-runs on the final argv, not just the
        # operator's.
        _refuse_off_pane_passthrough(out[1:], err)
    # `from_config` is the record of what was actually INJECTED, so it is the
    # only honest answer to "did anyone choose this model?". Reading the config
    # value instead would refuse a typed model that merely happens to sit
    # beside a configured one.
    model_source = next(
        (source for axis, _value, source in from_config if axis == "model"), None
    )
    _check_model_vendor_mismatch(out, err, env, model_source=model_source)
    _emit_defaults_applied(
        out, profile_verb, seed, locals(),
        from_config, suppressed,
        fingerprint=_slot_meta.get("fingerprint", ""),
    )
    return out


def _emit_defaults_applied(
    out: Sequence[str],
    verb: Optional[str],
    seed: Optional[str],
    scope: dict,
    applied: Sequence[Tuple[str, str, str]],
    suppressed: Sequence[Tuple[str, str, str, str]],
    fingerprint: str = "",
) -> None:
    """Journal the spawn_defaults_applied receipt (contract:
    docs/architecture/role-based-model-routing.md). ``scope`` is the caller's
    locals(); the call never raises - a dead journal never bricks a launch."""
    try:
        import os

        pin = os.environ.get("FNO_EVENTS_PATH")
        if pin:
            path = pin
        else:
            from fno import paths

            path = str(paths.state_dir() / "events.jsonl")
        axes = {
            axis: (scope.get(value_key) or "", scope.get(rung_key))
            for axis, value_key, rung_key in (
                ("provider", "cfg_harness", "provider_rung"),
                ("model", "cfg_model", "model_rung"),
                ("effort", "cfg_effort", "effort_rung"),
                ("substrate", "cfg_substrate", "substrate_rung"),
                ("permission_mode", "cfg_permission", "permission_rung"),
                ("route", "cfg_route", "route_rung"),
                ("account", "cfg_account", "account_rung"),
                ("pane_group", "cfg_pane_group", "pane_group_rung"),
            )
        }
        from fno.route_slot_client import route_slot_call

        route_slot_call({
            "op": "journal",
            "path": path,
            "event": {
                "name": _flag_value(out[1:], "--name"),
                "verb": verb,
                "seed": seed,
                "fingerprint": fingerprint,
                "resolved": {axis: {"value": v, "rung": r} for axis, (v, r) in axes.items()},
                "applied": [list(entry) for entry in applied],
                "suppressed": [list(entry) for entry in suppressed],
            },
        })
    except Exception:  # noqa: BLE001 - a dead journal never bricks a spawn
        pass


def spawn_seam_marker() -> str:
    """The marker token both bridges carry, straight after the spawn verb."""
    return f"--defaults-applied={routing_enforcement_state()}"


def routing_enforcement_state(settings: object = None) -> str:
    """The marker verdict: the config-blind binary's only record of the
    seam's decision. Read failure degrades open; a strict seam refuses
    upstream."""
    try:
        from fno.route_resolve import _routing_enforced

        if settings is None:
            from fno.config import load_settings

            settings = load_settings()
        return "enforced" if _routing_enforced(settings) else "unenforced"
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
            f"--substrate must be one of: pane, thread, headless (bg is a deprecated alias; got {substrate})",
            file=sys.stderr,
        )
        raise SystemExit(2)
    if substrate == "bg":
        print(
            "warning: substrate value 'bg' is deprecated; use 'thread' instead; "
            "the alias will be removed after one release",
            file=sys.stderr,
        )
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


def place_default_view(worker: str) -> bool:
    """Open the built-in default view (portal 0) on a freshly seated thread.

    The bare spawn's view rides the SAME two-call seam a user names by hand
    (``fno mux thread <name> --portal 0``); the spawn parser itself never
    declares a portal flag. Best effort by contract: a failure prints a named
    note with the manual reach and returns False - the worker receipt keeps
    its own verdict.
    """
    import subprocess

    argv = ["fno", "mux", "thread", worker, "--portal", "0"]
    try:
        proc = subprocess.run(
            argv, capture_output=True, text=True, timeout=15, check=False
        )
    except (OSError, subprocess.SubprocessError) as exc:
        print(
            f"fno agents spawn: default view unavailable ({exc}); open it by "
            f"hand: fno mux thread {worker} --portal 0",
            file=sys.stderr,
        )
        return False
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip().splitlines()
        reason = detail[-1] if detail else f"exit {proc.returncode}"
        print(
            f"fno agents spawn: default view not placed ({reason}); open it "
            f"by hand: fno mux thread {worker} --portal 0",
            file=sys.stderr,
        )
        return False
    return True
