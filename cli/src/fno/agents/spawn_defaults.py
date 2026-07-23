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
DIFFERENT harness (an explicit `-p codex`, OR a codex-ambient session, over a
claude-shaped `model`) leaves the model to that harness rather than forcing an
incompatible one. An explicit `-m/--model` always wins. Scope is the operator-
initiated spawn surface only; autonomous dispatch computes its own routing and
reaches the seam as explicit flags, never displaced by these.
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
        "--provider", "-p", "--harness", "-H", "--model", "-m", "--effort",
        "--from", "--cwd", "-c",
        "--message", "--session-id", "--cc-session-id", "--channel-id", "--status",
        "--from-name", "--timeout", "-t", "--mode", "--substrate", "--permission-mode",
    }
)

# --harness/-H is the canonical CLI-binary axis; --provider/-p the deprecated
# alias (x-6de8). Both feed the same provider-aware default scan. -H was
# reassigned FROM headless (which is now --headless/--once/--substrate headless).
_PROVIDER_FLAGS = ("--provider", "-p", "--harness", "-H")
_MODEL_FLAGS = ("--model", "-m")
_EFFORT_FLAGS = ("--effort",)


def _scan(args: Sequence[str]) -> Tuple[bool, Optional[str], bool, bool]:
    """One pass over a spawn argv (verb already stripped by the caller).

    Returns ``(provider_present, provider_value, model_present, effort_present)``.
    Handles both `--flag value` and `--flag=value`; stops at the `--argv`
    payload boundary; skips a value flag's value token.
    """
    provider_present = model_present = effort_present = False
    provider_value: Optional[str] = None
    it = iter(args)
    for a in it:
        if a == "--argv":
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
        "--node", "--slug", "--plan",
    }
)

# Tokens that pin the substrate explicitly (a positional substrate word conflicts
# with any of these -> exit 2). `--headless` and `-o/--once` mean headless; `-H`
# was reassigned to --harness (x-6de8), so it is a value flag, not a substrate pin.
_EXPLICIT_SUBSTRATE_BOOLS = ("--headless", "-o", "--once")

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

    Stops at the ``--argv`` payload boundary like the other spawn scans.
    """
    it = iter(toks)
    for t in it:
        if t == "--argv":
            break
        if t in _EXPLICIT_SUBSTRATE_BOOLS:
            return "headless"
        if t == "--substrate":
            return next(it, "")
        if t.startswith("--substrate="):
            return t.split("=", 1)[1]
    return None


def _positional_indices(toks: Sequence[str]) -> List[int]:
    """Indices of positional tokens (NAME, MESSAGE), skipping flags + their values."""
    idxs: List[int] = []
    i = 0
    n = len(toks)
    while i < n:
        t = toks[i]
        if t == "--argv":
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
    3. A spawn with no NAME positional gets an autogen ``adjective-noun`` slug.

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

    # Pass 2: -r / --resume id widening + implied bg.
    resume_idxs = [
        i for i, t in enumerate(toks)
        if t in ("-r", "--resume") or t.startswith("--resume=") or t.startswith("-r=")
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
        # (blueprint Silent-Failure-Hunter / Locked Decision 4).
        if _has_explicit_substrate(toks) is None:
            toks += ["--substrate", "bg"]
            print("fno agents spawn: substrate: bg (implied by --resume)", file=err)

    # Pass 3: autogen name when no NAME positional remains.
    if not _positional_indices(toks):
        names = existing_names if existing_names is not None else _read_registry_names()
        slug = _mint_slug(names, rng if rng is not None else random.Random(), err)
        toks.insert(0, slug)

    return ["spawn", *toks, *payload]


def _default_resolver(short_id: str) -> Optional[str]:
    """Resolve an 8-hex claude short-id to its full session uuid (bg sessions).

    Uses the bounded-retry lane so a short-id issued while claude is still writing
    the session entry is not rejected on a transient miss (blueprint Concurrency).
    """
    try:
        from fno.agents.providers.claude import resolve_session_uuid_at_spawn

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
    """The MESSAGE seed: the ``--message`` value, else the 2nd positional (the
    first is NAME). Stops at the ``--argv`` payload boundary."""
    i = 0
    while i < len(toks):
        t = toks[i]
        if t == "--argv":
            break
        if t == "--message":
            return toks[i + 1] if i + 1 < len(toks) else None
        if t.startswith("--message="):
            return t.split("=", 1)[1]
        i += 1
    pos = _positional_indices(toks)
    return toks[pos[1]] if len(pos) >= 2 else None


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
    """Whether the permission control is pinned, up to the ``--argv`` boundary.
    ``--yolo``/``-Y`` count: they are the same knob as ``--permission-mode`` and
    are mutually exclusive with it downstream, so a config value injected
    alongside an explicit ``--yolo`` would exit 2 (explicit intent must win)."""
    for t in toks:
        if t == "--argv":
            break
        if (
            t in ("--permission-mode", "--yolo", "-Y")
            or t.startswith("--permission-mode=")
        ):
            return True
    return False


def _substrate_compatible(substrate: str, provider: str) -> bool:
    """A config-sourced substrate must be a KNOWN value AND honored by the
    resolved provider. ``bg`` is claude-only; ``pane``/``headless`` are universal.
    An unknown value (or ``bg`` on a non-bg provider) degrades open (warn, skip) -
    never injected to fail at the spawn parser (both exit 2 there otherwise)."""
    if substrate not in _SUBSTRATES:
        return False
    # `zai` is the routed-claude shorthand: routing is materialized only for the
    # bg/headless lanes (dispatch_spawn_pane takes no route_env). An ambient
    # `pane` default must NOT be injected onto a zai spawn -- once on the argv it
    # is indistinguishable from a user-typed flag, so it would defeat the
    # shorthand's bg default and trip the routed-pane refusal, making
    # `spawn --harness zai` unusable under a configured pane default (x-6de8).
    # Degrade open here; cmd_spawn then applies the bg default itself.
    if provider == "zai":
        return substrate in ("bg", "headless")
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
    if not (cfg_provider or cfg_model or cfg_effort or cfg_substrate or cfg_permission):
        return out

    err = stderr if stderr is not None else sys.stderr
    has_provider, explicit_provider, has_model, has_effort = _scan(out[1:])

    inject: List[str] = []
    from_config: List[Tuple[str, str]] = []

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
                    from fno.agents.provider_resolve import resolve_dispatch_provider

                    _resolved["v"] = resolve_dispatch_provider(None, env=env)[0]
                except Exception:
                    _resolved["v"] = None
        return _resolved["v"]

    if cfg_provider and not has_provider:
        from fno.agents.providers import READABLE_PROVIDERS

        if cfg_provider not in READABLE_PROVIDERS:
            print(
                f"fno agents spawn: config.{provider_rung}.provider = "
                f"{cfg_provider!r} is not a known provider; valid: "
                f"{', '.join(READABLE_PROVIDERS)}",
                file=err,
            )
            raise SystemExit(2)
        inject += ["--provider", cfg_provider]
        from_config.append(("provider", provider_rung))  # type: ignore[arg-type]

    if cfg_model and not has_model:
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
        from fno.agents.provider_resolve import resolve_dispatch_provider

        home = cfg_provider or "claude"
        try:
            if explicit_provider and explicit_provider.strip():
                target: Optional[str] = explicit_provider.strip()
            elif cfg_provider:
                target = cfg_provider
            else:
                target = resolve_dispatch_provider(None, env=env)[0]
        except Exception:
            # Degrade open (AC5-FR): a resolution raise must never brick a spawn
            # that would otherwise work. No target => no basis to inject.
            print(
                "fno agents spawn: provider resolution failed; "
                "leaving model to the harness",
                file=err,
            )
            target = None
        if target and target == home:
            inject += ["--model", cfg_model]
            from_config.append(("model", model_rung))  # type: ignore[arg-type]
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
            from fno.agents.provider_resolve import resolve_dispatch_provider

            # `None` = no explicit provider, so resolve_dispatch_provider does
            # harness inference (env-based via infer_invoking_harness) then the
            # builtin claude. Its first arg is the explicit provider STRING, not
            # argv, and inference reads env markers, not command-line args.
            eff_provider, _ = resolve_dispatch_provider(None, env=env)
        from fno.agents.mux_spawn import _EFFORT_ALLOWED

        allowed = _EFFORT_ALLOWED.get(eff_provider)
        if allowed and cfg_effort in allowed:
            inject += ["--effort", cfg_effort]
            from_config.append(("effort", effort_rung))  # type: ignore[arg-type]
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
    # positional token, -H/-o, or resume-implied bg - all visible post-normalize).
    # A config-sourced value that is unknown, or incompatible with the resolved
    # provider, degrades open (warn, skip) rather than failing at the spawn parser.
    explicit_substrate = _has_explicit_substrate(out[1:])
    injected_substrate: Optional[str] = None
    if cfg_substrate and explicit_substrate is None:
        prov = resolved_provider()
        if prov and _substrate_compatible(cfg_substrate, prov):
            inject += ["--substrate", cfg_substrate]
            injected_substrate = cfg_substrate
            from_config.append(("substrate", substrate_rung))  # type: ignore[arg-type]
        else:
            if not prov:
                reason = "provider resolution failed"
            elif cfg_substrate not in _SUBSTRATES:
                reason = f"unknown substrate (valid: {', '.join(_SUBSTRATES)})"
            elif prov == "zai":
                reason = (
                    f"routed zai reaches only bg/headless, not {cfg_substrate!r}; "
                    "defaulting to bg"
                )
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
            from_config.append(("permission_mode", permission_rung))  # type: ignore[arg-type]
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
        # AC9-UI: config-sourced routing is never invisible; name field + rung.
        print(
            "fno agents spawn: applied "
            + ", ".join(f"{f}={r}" for f, r in from_config),
            file=err,
        )
    if inject:
        out = [out[0], *inject, *out[1:]]
    return out
