"""Config-sourced spawn defaults, injected argv-level at the dispatch seam.

Every `fno agents spawn` / `/agent spawn` passes the Python dispatch seam
(`rust_runtime.make_context`) before the Rust/Python routing fork. Injecting
`config.agents.defaults` field-by-field on argv HERE covers pane, bg, headless,
and the Rust route with zero Rust changes (Locked Decision 9).

Precedence per field: explicit CLI flag > `agents.profiles.<verb>` > `agents.
defaults` > built-in. The profile layer (x-3d5b) is the same block keyed by the
seed's leading slash-verb, merged over defaults field-wise before injection.
Fields resolve independently, with ONE exception: the `model` default is provider-
scoped. A bare scalar `model` with no `provider` is scoped to the harness it was
written for - the config `provider`, else the builtin default (claude), NOT the
ambient harness (whose shape the model may not match). A spawn that resolves to a
DIFFERENT harness (an explicit `-H codex`, OR a codex-ambient session, over a
claude-shaped `model`) leaves the model to that harness rather than forcing an
incompatible one. An explicit `-m/--model` always wins. The profile layer
applies to every spawn carrying a slash-verb seed, including autonomous dispatch
(`/target`, `/blueprint`): a stage that has not pinned a field inherits it from
`profiles.<verb>` then `agents.defaults`, so a coordinate set in the stage table
reaches an autonomous worker. An explicit flag always wins, and a `--role` whose
lane resolves owns the model, so autonomous dispatch is never rerouted on a field
it pinned (harness, substrate).
"""
from __future__ import annotations

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
        "--output-format", "--monitor",
    }
)

# The harness axis (the CLI binary) is --harness/-H only: --provider/-P names the
# model VENDOR, a different axis that never picks a binary, so it must not feed
# the provider-aware default scan.
_PROVIDER_FLAGS = ("--harness", "-H")
_MODEL_FLAGS = ("--model", "-m")
_EFFORT_FLAGS = ("--effort",)


def _scan(args: Sequence[str]) -> Tuple[bool, Optional[str], bool, bool]:
    """One pass over a spawn argv (verb already stripped by the caller).

    Returns ``(provider_present, provider_value, model_present, effort_present)``.
    Handles both `--flag value` and `--flag=value`; stops at the `--argv`
    payload boundary and at a bare `--` passthrough fence (x-1caa: fenced
    tokens are the provider's flags, never fno's); skips a value flag's value.
    """
    provider_present = model_present = effort_present = False
    provider_value: Optional[str] = None
    it = iter(args)
    for a in it:
        if a == "--argv" or a == "--":
            break
        key, eq, val = a.partition("=")
        if key in _PROVIDER_FLAGS:
            provider_present = True
            provider_value = val if eq else next(it, None)
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
    return provider_present, provider_value, model_present, effort_present


# --------------------------------------------------------------------------- #
# Spawn argv normalization (x-f76e): three ergonomic cuts, one argv->argv pass.
#
# Runs at the front door (inside inject_spawn_defaults, BEFORE config injection
# and BEFORE the runtime route/fork), so by the time either runtime parser sees
# the argv it is canonical: an explicit NAME, long-form `--resume <full-uuid>`,
# long-form `--substrate <s>`. Neither parser learns a new vocabulary.
# --------------------------------------------------------------------------- #

_SUBSTRATES = ("pane", "bg", "headless")

# Flags on `spawn` that consume the following token. Needed to tell a flag's
# VALUE apart from a positional when scanning for the NAME / substrate token. A
# missing entry would misread that flag's value as a positional (e.g. a Rust-path
# `--message bg` mis-parsed as a substrate token), so this unions the shared
# `_VALUE_FLAGS` (--message, --session-id, --from, --status, ...) with the
# spawn-only value options.
_SPAWN_VALUE_FLAGS = _VALUE_FLAGS | frozenset(
    {
        "--role", "--resume", "-r", "--add-dir", "--agent", "--tools",
        "--deny-tools", "--workspace", "--squad", "-s", "--split", "-x",
        "--node", "--slug", "--plan", "--name",
        # x-6de8: --route/--account/--crown were absent, so their VALUES read as
        # positionals: a nameless `spawn --route zai,glm-5.2` registered an agent
        # named "zai,glm-5.2". Kept in lockstep with cmd_spawn's value options
        # (test_spawn_value_flags_cover_every_value_option pins the two together).
        "--route", "--account", "--crown", "-k", "--dispatch-account",
        # x-6928: --at's value (current|<pane>) must not read as a positional.
        "--at",
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
    if _has_explicit_substrate(toks) not in ("bg", "headless"):
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


def _read_registry_names() -> Set[str]:
    """Live worker names for the autogen pre-check. Best-effort: {} on any error."""
    try:
        from fno.agents.registry import load_registry

        return {e.name for e in load_registry()}
    except Exception:
        return set()


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
        names = existing_names if existing_names is not None else _read_registry_names()
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


def _profile_key(seed: Optional[str]) -> Optional[str]:
    """Derive the profile key from a seed's first token by a pure string rule:
    must start with ``/`` and contain no further ``/`` (an absolute path never
    matches); strip the ``/`` and an optional ``fno:`` namespace; the remainder
    must be lowercase ``^[a-z0-9][a-z0-9_-]*$``. No key -> no profile layer."""
    if not seed:
        return None
    parts = seed.split()
    if not parts:
        return None
    tok = parts[0]
    if not tok.startswith("/") or "/" in tok[1:]:
        return None
    rest = tok[1:]
    if rest.startswith("fno:"):
        rest = rest[len("fno:"):]
    return rest if _PROFILE_KEY_RE.match(rest) else None


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


def _substrate_compatible(substrate: str, provider: str) -> bool:
    """A config-sourced substrate must be a KNOWN value AND honored by the
    resolved provider. ``bg`` is claude-only; ``pane``/``headless`` are universal.
    An unknown value (or ``bg`` on a non-bg provider) degrades open (warn, skip) -
    never injected to fail at the spawn parser (both exit 2 there otherwise)."""
    if substrate not in _SUBSTRATES:
        return False
    if substrate != "bg":
        return True
    try:
        from fno.agents.harness_map import capabilities

        return bool(capabilities(provider)["bg"])
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


# A model string's implied vendor, by prefix or tier word. A pure string
# opinion and never a routing input: the warning it drives is advisory, because
# the pairing is legal and --model is deliberate passthrough (cli.py).
_MODEL_VENDOR_HINTS: Tuple[Tuple[str, str], ...] = (
    ("glm-", "zai"),
    ("gpt-", "openai"),
    ("deepseek", "deepseek"),
    ("gemini-", "google"),
    ("claude-", "anthropic"),
)
_MODEL_WORD_VENDORS = {"opus": "anthropic", "sonnet": "anthropic", "haiku": "anthropic"}

# The vendor a harness's own primary lane bills when no route/vendor was named.
# opencode is operator-configured, so it holds no opinion here.
_HARNESS_DEFAULT_VENDOR = {
    "claude": "anthropic",
    "codex": "openai",
    "gemini": "google",
    "agy": "google",
}


def _implied_vendor(model: Optional[str]) -> Optional[str]:
    if not model:
        return None
    for candidate in (model.lower(), model.lower().rsplit("/", 1)[-1]):
        for prefix, vendor in _MODEL_VENDOR_HINTS:
            if candidate.startswith(prefix):
                return vendor
        base = candidate.split("[", 1)[0].split("-", 1)[0]
        word_vendor: Optional[str] = _MODEL_WORD_VENDORS.get(base)
        if word_vendor:
            return word_vendor
    return None


def resolve_lane_vendor(
    argv: Sequence[str],
    env: Optional[Mapping[str, str]] = None,
    *,
    harness: Optional[str] = None,
) -> Optional[str]:
    """Resolve the model vendor carried by a final spawn argv."""
    values = list(argv)
    tokens = values[1:] if values else []
    explicit_route = _flag_value(tokens, "--route")
    if explicit_route:
        return explicit_route.replace(",", "/").split("/", 1)[0].strip().lower() or None
    explicit_vendor = _flag_value(tokens, "--provider", "-P")
    if explicit_vendor:
        return explicit_vendor.strip().lower() or None
    resolved_harness = (
        harness or _flag_value(tokens, "--harness", "-H") or ""
    ).strip().lower()
    if not resolved_harness and values and values[0] in _HARNESS_DEFAULT_VENDOR:
        resolved_harness = values[0]
    if not resolved_harness:
        try:
            from fno.dispatch_flags import resolve_dispatch_provider

            resolved_harness = resolve_dispatch_provider(None, env=env)[0]
        except Exception:
            resolved_harness = "claude"
    lane = _HARNESS_DEFAULT_VENDOR.get(resolved_harness)
    if lane:
        return lane
    if resolved_harness == "opencode":
        return _implied_vendor(_flag_value(tokens, "--model", "-m"))
    return None


def _warn_model_vendor_mismatch(
    argv: Sequence[str], err: IO[str], env: Optional[Mapping[str, str]] = None
) -> None:
    """Advisory warning when a model string implies a vendor the resolved lane
    does not match (a glm-* model with no zai route, a gpt-* under a claude
    harness). Warn, never refuse: the pairing is legal and passthrough is
    deliberate. Names BOTH sides, because a warning naming only the mismatch
    does not tell the caller which half to change. Runs on the FINAL argv so
    explicit flags and injected defaults are judged together. An explicit
    --route suppresses the warning: the caller named the lane AND the model,
    which is a deliberate override, not the misroute this catches."""
    toks = list(argv[1:])
    model = _flag_value(toks, "--model", "-m")
    implied = _implied_vendor(model)
    if not implied:
        return
    if _flag_value(toks, "--route") is not None:
        # Explicit route: a deliberate lane choice beside a deliberate model.
        # (A config route is never injected beside an explicit model, so a
        # --route in the final argv is always operator-typed.)
        return
    lane = resolve_lane_vendor(argv, env=env)
    if not lane or lane == implied:
        return
    from fno.agents import events

    events.emit(
        "model_vendor_mismatch",
        model=model,
        implied_vendor=implied,
        resolved_vendor=lane,
    )
    print(
        f"fno agents spawn: --model {model!r} implies vendor {implied}, but the "
        f"resolved lane is {lane}; the model rides that lane's CLI as-is. Name "
        f"the vendor with -P {implied} to route it.",
        file=err,
    )


def inject_spawn_defaults(
    args: Sequence[str],
    *,
    settings: object = None,
    env: Optional[Mapping[str, str]] = None,
    stderr: Optional[IO[str]] = None,
) -> List[str]:
    """Return ``args`` with config spawn-defaults injected where absent.

    Fields resolve field-wise from the merged view `agents.profiles.<verb>` (the
    seed's leading slash-verb, x-3d5b) over `agents.defaults`, so an explicit CLI
    flag > profile > defaults > built-in. Only acts on a `spawn` verb
    (``args[0] == "spawn"``). Returns the input unchanged for any other verb, or
    when the config load fails (a bad config must never brick spawning). Raises
    ``SystemExit(2)`` on an unknown config provider (AC5-ERR). Config-sourced
    effort/substrate/permission_mode degrade open on an incompatible resolved
    provider (warn, skip); an explicit flag stays fail-closed downstream.
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
    verb = _profile_key(_seed_of(out[1:]))
    profile = (getattr(agents, "profiles", None) or {}).get(verb) if verb else None

    def field(name: str) -> Tuple[str, Optional[str]]:
        """Effective value + source rung for a field: profile > defaults."""
        if profile is not None:
            pv = (getattr(profile, name, "") or "").strip()
            if pv:
                return pv, f"agents.profiles.{verb}"
        dv = (getattr(defaults, name, "") or "").strip()
        if dv:
            return dv, "agents.defaults"
        return "", None

    cfg_provider, provider_rung = field("provider")
    cfg_model, model_rung = field("model")
    cfg_effort, effort_rung = field("effort")
    cfg_substrate, substrate_rung = field("substrate")
    cfg_permission, permission_rung = field("permission_mode")
    cfg_route, route_rung = field("route")
    cfg_account, account_rung = field("account")
    if not (
        cfg_provider or cfg_model or cfg_effort or cfg_substrate or cfg_permission
        or cfg_route or cfg_account
    ):
        _warn_model_vendor_mismatch(out, err, env)
        return out

    # A spawn carrying --role whose lane resolves to a real route is billed on
    # that route: the route owns the model via env (ANTHROPIC_*), so a config
    # --model sourced here would collide with it. This is the five-opus-workers
    # defect - a worker believed cheap and billed expensive - so the model
    # branch below skips injection when resolve_route returns a real route.
    role = _role_of(out[1:])

    has_provider, explicit_provider, has_model, has_effort = _scan(out[1:])

    inject: List[str] = []
    # (axis, value, source) - source is f"{rung}.{field}"; axis is the
    # coordinate the field actually feeds (provider feeds "harness", not
    # "provider" - see docs/architecture/axis-vocabulary.md).
    from_config: List[Tuple[str, str, str]] = []

    # Lazy resolved-target provider for the substrate/permission compatibility
    # checks: explicit -p > merged config provider > harness inference.
    _resolved: dict = {}

    def resolved_provider() -> Optional[str]:
        if "v" not in _resolved:
            if explicit_provider and explicit_provider.strip():
                _resolved["v"] = explicit_provider.strip()
            elif cfg_provider:
                _resolved["v"] = cfg_provider
            else:
                try:
                    from fno.dispatch_flags import resolve_dispatch_provider

                    _resolved["v"] = resolve_dispatch_provider(None, env=env)[0]
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
    explicit_vendor = _flag_value(out[1:], "--provider", "-P")
    explicit_vendor_present = explicit_vendor is not None
    explicit_route = _flag_present(out[1:], "--route")

    if cfg_provider and not has_provider:
        from fno.agents.harnesses import READABLE_PROVIDERS

        if cfg_provider not in READABLE_PROVIDERS:
            print(
                f"fno agents spawn: config.{provider_rung}.provider = "
                f"{cfg_provider!r} is not a known provider; valid: "
                f"{', '.join(READABLE_PROVIDERS)}",
                file=err,
            )
            raise SystemExit(2)

        # AC2-HP: the profile is about to fill the HARNESS axis (nothing on
        # the caller's argv set it - has_provider is False, or we would not
        # be here). If the caller's argv is already route-shaped, that route
        # only the claude harness can carry, and a non-claude profile harness
        # makes it unusable. This is not a precedence bug (no explicit flag is
        # overwritten); it is a cross-axis collision, so say everything: the
        # config path, the value, the axis it set, and the caller's own flags
        # in the caller's own spelling.
        route_shaped = explicit_route or (explicit_vendor_present and explicit_model_present)
        if route_shaped and cfg_provider != "claude":
            if explicit_route:
                caller_spelling = f"--route {_flag_value(out[1:], '--route')}"
            else:
                model_val = _flag_value(out[1:], "--model", "-m")
                caller_spelling = f"-P {explicit_vendor} --model {model_val}"
            print(
                f"fno agents spawn: config.{provider_rung}.provider = {cfg_provider!r} "
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

        inject += ["--harness", cfg_provider]
        from_config.append(("harness", cfg_provider, f"{provider_rung}.provider"))  # type: ignore[arg-type]

    # route / account (ruling 4): two new fields beside the legacy provider.
    # route carries vendor/model as vendor/model and is forwarded as --route, so
    # it inherits the flag's fail-closed resolution - an unknown vendor or a
    # missing key refuses the spawn rather than silently billing the primary,
    # which is the invisible-billing shape this node exists to kill. account
    # forwards --account.
    route_injected = False
    if cfg_route and not explicit_route and not explicit_model_present and not explicit_vendor_present:
        inject += ["--route", cfg_route]
        route_injected = True
        from_config.append(("route", cfg_route, f"{route_rung}.route"))  # type: ignore[arg-type]
    # route_present covers BOTH ways --route ends up in the final argv: injected
    # from config just above, or already explicit on the caller's argv. Gating
    # the model-suppression below on route_injected alone missed the explicit
    # case - an operator-typed `--route zai/... ` with no `-m` still fell through
    # to the config-model branch and injected `--model opus` alongside it, the
    # exact route+model collision (five-opus-workers defect) this field exists
    # to prevent.
    route_present = route_injected or explicit_route
    # Accounts are Claude-only (cmd_spawn rejects --account on any other
    # harness), so a configured account must not follow an explicit non-Claude
    # harness - e.g. an autonomous Claude-to-Codex quota cutover (-H codex)
    # would otherwise carry a Claude account into a spawn that can't use it and
    # abort instead of cutting over.
    if cfg_account and not _flag_present(out[1:], "--account"):
        prov = resolved_provider()
        if prov == "claude":
            inject += ["--account", cfg_account]
            from_config.append(("account", cfg_account, f"{account_rung}.account"))  # type: ignore[arg-type]
        else:
            # AC9-UI: config-sourced routing is never invisible - a substrate/
            # permission skip already warns here, so account must too rather than
            # silently dropping the pin on a Claude-to-Codex cutover.
            print(
                f"fno agents spawn: account skipped (accounts are claude-only, "
                f"resolved provider {prov!r}); {account_rung}.account "
                f"{cfg_account!r} ignored",
                file=err,
            )

    if cfg_model and not has_model:
        # The config model is suppressed when something else already owns the
        # model: an injected route (route carries vendor/model), or a --role that
        # resolves to a real route (the route owns the model via env). Either
        # would collide with a config --model. An explicit -m already won via
        # has_model, which short-circuited this whole branch.
        if route_present:
            print(
                f"fno agents spawn: --route owns the model; not injecting "
                f"{model_rung}.model {cfg_model!r}",
                file=err,
            )
        elif explicit_vendor_present:
            # A bare explicit -P/--provider (vendor, no -m) already names the
            # vendor half of a route; cmd_spawn pairs it with whatever --model
            # reaches it. Injecting the config model here would pair a DIFFERENT
            # vendor's model behind the explicit vendor (e.g. -P zai + injected
            # --model opus -> route "zai/opus", an anthropic model at a zai
            # endpoint) - the same invisible-billing shape the route/vendor
            # collision guards above exist to kill, just on the model path
            # instead of the route path.
            print(
                f"fno agents spawn: --provider {explicit_vendor!r} names a "
                f"vendor; not injecting {model_rung}.model {cfg_model!r} "
                "(add --model yourself to complete the route)",
                file=err,
            )
        elif role and _role_resolves(role, settings, env):
            # resolve_route is fail-SAFE: a protected role, a disabled block, an
            # unconfigured lane, or a missing key all return None (spawn falls
            # back to the primary model, where the config default still applies).
            # Only a REAL route owns the model, so only then do we skip.
            print(
                f"fno agents spawn: --role {role!r} resolves to a route; leaving "
                f"model to the route (not injecting {model_rung}.model "
                f"{cfg_model!r})",
                file=err,
            )
        else:
            # A provider-less config model is scoped to the harness it was written
            # for, but nothing on disk records which harness that was. Scope it to
            # the HOME provider - the config provider, else the builtin default
            # (claude, the same fallback resolve_dispatch_provider uses) - NOT the
            # ambient harness. Inject only when the spawn's resolved TARGET equals
            # that home: a codex spawn (explicit `-p codex` OR a codex-ambient
            # session) must not inherit a claude model (it 400s after the round-trip);
            # an explicit --model stays the supported cross-harness override. This
            # never maps a model value to a provider (no catalog); it only scopes an
            # UNqualified default the way the rest of dispatch scopes one.
            from fno.dispatch_flags import resolve_dispatch_provider

            home = cfg_provider or "claude"
            try:
                if explicit_provider and explicit_provider.strip():
                    target: Optional[str] = explicit_provider.strip()
                elif cfg_provider:
                    target = cfg_provider
                else:
                    target = resolve_dispatch_provider(None, env=env)[0]
            except Exception:
                # Degrade open (AC5-FR): a resolution raise must never brick a
                # spawn that would otherwise work. No target => no basis to inject.
                print(
                    "fno agents spawn: provider resolution failed; "
                    "leaving model to the harness",
                    file=err,
                )
                target = None
            if target and target == home:
                inject += ["--model", cfg_model]
                from_config.append(("model", cfg_model, f"{model_rung}.model"))  # type: ignore[arg-type]
            elif target:
                print(
                    f"fno agents spawn: config model {cfg_model!r} is scoped to "
                    f"{home}; spawn resolves {target}, leaving model to the harness "
                    f"(bind {model_rung}.provider to apply it cross-harness)",
                    file=err,
                )

    if cfg_effort and not has_effort:
        # Effort surface depends on the RESOLVED provider: an explicit -p flag,
        # else the config provider, else harness inference / builtin claude.
        eff_provider = (explicit_provider or "").strip() or cfg_provider
        if not eff_provider:
            from fno.dispatch_flags import resolve_dispatch_provider

            # `None` = no explicit provider, so resolve_dispatch_provider does
            # harness inference (env-based via infer_invoking_harness) then the
            # builtin claude. Its first arg is the explicit provider STRING, not
            # argv, and inference reads env markers, not command-line args.
            eff_provider, _ = resolve_dispatch_provider(None, env=env)
        from fno.agents.mux_spawn import _EFFORT_ALLOWED

        allowed = _EFFORT_ALLOWED.get(eff_provider)
        if allowed and cfg_effort in allowed:
            inject += ["--effort", cfg_effort]
            from_config.append(("effort", cfg_effort, f"{effort_rung}.effort"))  # type: ignore[arg-type]
        else:
            # Config-sourced effort degrades open on BOTH a no-surface provider
            # AND a value the resolved provider can't map (e.g. codex + "max"):
            # an ambient default must never hard-fail a bare spawn. An explicit
            # --effort keeps x-a0e0's fail-closed exit 2 (has_effort short-circuits
            # this whole branch).
            reason = (
                f"no {eff_provider} effort surface"
                if not allowed
                else f"{eff_provider} does not support effort {cfg_effort!r}"
            )
            print(
                f"fno agents spawn: effort skipped ({reason}); "
                f"{effort_rung}.effort = {cfg_effort!r} ignored",
                file=err,
            )

    # Substrate (x-3d5b): inject when no explicit substrate is pinned (flag,
    # positional token, --headless/-o, or resume-implied bg - all post-normalize).
    # A config-sourced value that is unknown, or incompatible with the resolved
    # provider, degrades open (warn, skip) rather than failing at the spawn parser.
    explicit_substrate = _has_explicit_substrate(out[1:])
    injected_substrate: Optional[str] = None
    if cfg_substrate and explicit_substrate is None:
        prov = resolved_provider()
        if prov and _substrate_compatible(cfg_substrate, prov):
            inject += ["--substrate", cfg_substrate]
            injected_substrate = cfg_substrate
            from_config.append(("substrate", cfg_substrate, f"{substrate_rung}.substrate"))  # type: ignore[arg-type]
        else:
            if not prov:
                reason = "provider resolution failed"
            elif cfg_substrate not in _SUBSTRATES:
                reason = f"unknown substrate (valid: {', '.join(_SUBSTRATES)})"
            else:
                reason = f"{prov} does not support substrate {cfg_substrate!r} (bg is claude-only)"
            print(
                f"fno agents spawn: substrate skipped ({reason}); "
                f"{substrate_rung}.substrate = {cfg_substrate!r} ignored",
                file=err,
            )

    # Permission mode (x-3d5b): same shape as substrate, but the compatibility
    # check depends on the EFFECTIVE substrate (explicit pin > this-run injection >
    # per-provider default), because a non-claude bg/headless lane refuses a
    # mapped --permission-mode. An explicit --permission-mode/--yolo keeps the
    # fail-closed behavior (has_permission short-circuits this branch).
    if cfg_permission and not _has_permission_mode(out[1:]):
        prov = resolved_provider()
        # The effective substrate this spawn resolves to: an explicit pin, else a
        # config value injected this run, else the `fno agents spawn` default -
        # PANE (cli.py, not the autonomous-dispatch substrate_default, which picks
        # headless for non-claude and would wrongly skip a pane-mappable mode).
        eff_substrate = explicit_substrate or injected_substrate or "pane"
        if prov and _permission_mappable(prov, cfg_permission, eff_substrate):
            inject += ["--permission-mode", cfg_permission]
            from_config.append(("permission_mode", cfg_permission, f"{permission_rung}.permission_mode"))  # type: ignore[arg-type]
        else:
            reason = (
                f"{prov} cannot map permission mode {cfg_permission!r} on substrate {eff_substrate!r}"
                if prov
                else "provider resolution failed"
            )
            print(
                f"fno agents spawn: permission-mode skipped ({reason}); "
                f"{permission_rung}.permission_mode = {cfg_permission!r} ignored",
                file=err,
            )

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
        # x-1caa: injection can pin the substrate the operator left open, and
        # the Rust-routed lane never reaches the Python CLI's own refusal - so
        # the off-pane passthrough gate re-runs on the final argv, not just the
        # operator's.
        _refuse_off_pane_passthrough(out[1:], err)
    _warn_model_vendor_mismatch(out, err, env)
    return out


# ---------------------------------------------------------------------------
# The fallback chain: where a refused node goes next
# ---------------------------------------------------------------------------

#: Sizes that resolve to their own chain. Anything else reads `default`.
_CHAIN_SIZES = ("S", "M", "L")

_CHAIN_KEYS = frozenset({"S", "M", "L", "default"})
# The harness axis is the BINARY (docs/architecture/axis-vocabulary.md).
# `opencode` is legally both a harness and a provider, so never infer the axis
# from the value.
_CHAIN_HARNESSES = frozenset({"claude", "codex", "agy", "opencode"})


class FallbackConfigError(ValueError):
    """A fallback chain that cannot be trusted to name a vendor."""


def validate_fallback(table: object) -> dict:
    """Return the chain table as links, or raise naming the offending key.

    Called on the failover path, never at config load. Unlike
    ``_coerce_profiles``, which degrades a typo to no-profiles so a bad config
    cannot brick spawning, this REFUSES: a chain is read only when a provider
    has already refused, and degrading open there spawns a worker at an
    unintended vendor and bills it. Refusing leaves the worker alive and the
    node claimed, which is a bounded stop.

    Raising here rather than in the config model is deliberate. A field
    validator would fail ``load_settings()`` process-wide, so one typo would
    make every ``fno`` command raise and stop the pr-watch tick at its settings
    phase - taking down the daemon that runs the failover, which is a strictly
    worse outcome than the one being prevented.
    """
    from fno.config import SpawnDefaultsBlock

    if not isinstance(table, dict):
        raise FallbackConfigError(
            "config.agents.fallback must be a table keyed by size "
            f"(S|M|L|default); got {type(table).__name__}"
        )
    out: dict[str, list] = {}
    for size, chain in table.items():
        if size not in _CHAIN_KEYS:
            raise FallbackConfigError(
                f"config.agents.fallback.{size}: unknown size key; "
                f"expected one of {'|'.join(sorted(_CHAIN_KEYS))}"
            )
        if not isinstance(chain, list):
            raise FallbackConfigError(
                f"config.agents.fallback.{size} must be a list of links; "
                f"got {type(chain).__name__}"
            )
        links = []
        for i, link in enumerate(chain):
            if isinstance(link, SpawnDefaultsBlock):
                links.append(link)
                continue
            if not isinstance(link, dict):
                raise FallbackConfigError(
                    f"config.agents.fallback.{size}[{i}] must be a table of "
                    f"axis fields; got {type(link).__name__}"
                )
            # A link is spelled with the CORRECT axis word, `harness` (the
            # binary). SpawnDefaultsBlock's own field is the legacy `provider`,
            # which its comment records as meaning harness. Both spellings
            # read and `harness` wins. The value is checked either way, because
            # `extra="ignore"` would otherwise drop a typo'd harness silently
            # and the link would spawn on the ambient binary.
            link = dict(link)
            harness = str(
                link.pop("harness", "") or link.get("provider", "") or ""
            ).strip()
            if harness and harness not in _CHAIN_HARNESSES:
                raise FallbackConfigError(
                    f"config.agents.fallback.{size}[{i}].harness={harness!r} "
                    f"is not a known harness "
                    f"({'|'.join(sorted(_CHAIN_HARNESSES))})"
                )
            if harness:
                link["provider"] = harness
            links.append(SpawnDefaultsBlock(**link))
        out[size] = links
    return out


def link_id(link) -> str:
    """A stable ``harness/model`` name for one chain link.

    Used as the walk's memory key, so a node never re-dispatches onto a link it
    has already spent. Two links that differ only in effort are the SAME
    destination for that purpose: retrying the same vendor at a different
    reasoning setting does not answer a cap.

    ``account`` DOES participate, because it names a different bill and a
    different meter. Two links on one harness and model that differ only by
    account are the operator's second credential, and folding them together
    would spend the first and then skip the second as already tried.
    """
    harness = (getattr(link, "provider", "") or "").strip() or "?"
    model = (getattr(link, "model", "") or "").strip()
    route = (getattr(link, "route", "") or "").strip()
    account = (getattr(link, "account", "") or "").strip()
    base = f"{harness}/{model or route or 'default'}"
    return f"{base}@{account}" if account else base


def _harness_records(harness: str, repo_root=None):
    """Account records whose harness is ``harness``, or [] when unreadable.

    Rooted at ``repo_root`` because the recovery roster is global and a
    candidate can belong to another project. A same-id record in the
    dispatcher's own project must never answer for a foreign worker.
    """
    try:
        from fno.adapters.providers.loader import load_providers

        from pathlib import Path as _Path

        cfg = load_providers(repo_root=_Path(repo_root) if repo_root else None)
        return [r for r in cfg.records if r.harness == harness]
    except Exception:  # noqa: BLE001 - an unreadable config means UNKNOWN, not exhausted
        return []


def link_is_exhausted(link, *, now: Optional[float] = None, repo_root=None) -> bool:
    """True only when every account this link can land on is KNOWN exhausted.

    This is the check task 1.2 made real. Before the harvested reset, a claude
    record's lock expired seconds after the cap and a z.ai record had no probe
    at all, so every link looked eligible and the chain routed straight back
    into the provider that had just refused.

    UNKNOWN stays eligible, matching the invariant `rotation.py` already
    enforces: unknown is not exhausted. So does an unreadable config, no
    matching record, and any error on the way - the failure mode of guessing
    "exhausted" is holding a node that could have run.
    """
    from fno.adapters.providers.runtime_state import HeadroomState, headroom

    account = (getattr(link, "account", "") or "").strip()
    harness = (getattr(link, "provider", "") or "").strip()
    ids = (
        [account] if account
        else [r.id for r in _harness_records(harness, repo_root)]
    )
    if not ids:
        return False
    try:
        from pathlib import Path as _Path

        root = _Path(repo_root) if repo_root else None
        verdicts = [headroom(pid, now=now, repo_root=root) for pid in ids]
    except Exception:  # noqa: BLE001 - a failed read is UNKNOWN, never exhausted
        return False
    return all(v.state is HeadroomState.EXHAUSTED for v in verdicts)


def resolve_fallback_chain(
    size: Optional[str],
    *,
    exclude: Sequence[str] = (),
    settings: object = None,
    now: Optional[float] = None,
    repo_root=None,
) -> List[object]:
    """The eligible fallback links for a node of ``size``, in order.

    ``size`` is the node's existing ``--size S|M|L``, which is already the
    operator's own simple-versus-complex split; an absent or unrecognised size
    reads the ``default`` chain. ``exclude`` carries the links this node has
    already spent, by :func:`link_id`.

    Returns [] for three different situations that share one correct action -
    spawn nothing: no chain configured (the pre-existing behavior), every link
    already tried, and every remaining link's own provider known exhausted.
    Routing into a known-capped provider is worse than holding, so an
    all-exhausted chain deliberately does NOT fall back to link zero.

    Raises whatever the config validator raises on a malformed chain. That is
    the point: a chain is read on the failover path only, and degrading open
    there spawns a worker at an unintended vendor and bills it.
    """
    if settings is None:
        # Rooted at the CANDIDATE's project, not the daemon's cwd. The recovery
        # roster is global, so a foreign worker resolving the dispatcher's
        # chain would spawn on a vendor its own project never authorized.
        if repo_root:
            from pathlib import Path as _Path

            from fno.config import load_settings_for_repo

            settings = load_settings_for_repo(_Path(repo_root))
        else:
            from fno.config import load_settings

            settings = load_settings()
    table = validate_fallback(
        getattr(settings.agents, "fallback", None) or {}  # type: ignore[attr-defined]
    )
    key = size if size in _CHAIN_SIZES else "default"
    chain = table.get(key) or table.get("default") or []
    spent = set(exclude)
    return [
        link for link in chain
        if link_id(link) not in spent
        and not link_is_exhausted(link, now=now, repo_root=repo_root)
    ]


def link_to_spawn_flags(link) -> List[str]:
    """One chain link as spawn flags, in the codebase's existing axis spelling.

    No new axis vocabulary: harness is ``-H``, the vendor/model route is
    ``--route``, and model, effort, substrate, permission-mode and account keep
    their own flags. ``--substrate bg`` is claude-only in the Rust client, so a
    codex link that left substrate unset resolves to a pane rather than being
    handed a substrate its harness rejects.
    """
    harness = (getattr(link, "provider", "") or "").strip()
    out: List[str] = []
    if harness:
        out += ["-H", harness]
    for flag, field in (
        ("-m", "model"),
        ("--effort", "effort"),
        ("--permission-mode", "permission_mode"),
        ("--route", "route"),
        ("--account", "account"),
    ):
        val = (getattr(link, field, "") or "").strip()
        if val:
            out += [flag, val]
    substrate = (getattr(link, "substrate", "") or "").strip()
    if not substrate:
        substrate = "bg" if harness == "claude" else "pane"
    out += ["--substrate", substrate]
    return out
