"""One inventory of the ambient state a test can read, and two operations on it.

The problem this exists to solve: CI runs with a clean ``HOME``. A test that
READS the developer's ambient state therefore passes in CI forever, because the
value it reads is always absent there; a test that DEPENDS on ambient state
fails in CI and passes locally. Neither direction is caught by the thing we
trust to catch things.

Three specimens on 2026-08-11 leaked through three different channels (an env
var the command itself resolved, the config chain, and the real carve-out
ledger). Each got a per-test pin. That is not the fix, and the measurement says
why: ``fno`` source reads 124 distinct ``FNO_*`` names while the pytest conftest
pinned five of them by name. An allowlist facing a surface that size loses the
moment someone adds the next var.

So the rule here is inverted. Ambient state is neutralised as a CLASS
(deny-by-default), and the small set of names that must survive is explicit,
justified, and short enough to read.

Two functions:

``neutralise(env, sandbox)``
    What a hermetic child process gets. Applied at ``test_cmd._child_env``,
    the one process boundary all four test trees cross (pytest over
    ``cli/tests``, pytest over ``cli/src``, every ``bash tests/*.sh`` smoke
    step, and ``cargo``), and again at both conftests' module load so a bare
    ``pytest`` is covered too.

``poison(env, fixtures)``
    What ``fno test smoke --ambient dirty`` feeds the RUNNER. ``_child_env``
    then neutralises that poisoned parent to build the child env. If this
    inventory is complete the child is identical to the clean lane and the
    dirty lane is green; if a name is missing it survives into the child, a
    test reads a ``fno-poison-*`` sentinel, and it fails naming itself.

Docs: ``docs/architecture/test-hermeticity.md``.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Mapping, Optional

from fno.harness_identity import AMBIENT_IDENTITY_ENV

__all__ = [
    "AMBIENT_LEAK_CANARY",
    "ambient_names",
    "classify",
    "neutralise",
    "poison",
]


# Every ``FNO_*`` and ``TARGET_*`` name is ambient until proven otherwise.
# This is the whole point: a 125th FNO_ var added to source neutralises without
# editing this file. TARGET_* earns its place the same way - a live /target
# session exports TARGET_INPUT / TARGET_PLAN_PATH / TARGET_SIZE / TARGET_UNATTENDED
# and more, so a suite run from inside one inherits that session's parameters.
_AMBIENT_PREFIXES = ("FNO_", "TARGET_")

# Non-prefixed ambient names, measured rather than guessed: source reads 49
# distinct non-FNO_ env names, and ``test_ambient_surface.py`` fails when a new
# one appears in neither this tuple nor _ENVIRONMENT below. That test is what
# stops this list becoming the next allowlist that loses to the fifth thing
# nobody thought of.
#
# Session identity is IMPORTED from harness_identity, never retyped - that tuple
# is already derived from HARNESS_SESSION_MARKERS because a hand-maintained copy
# had already lost CLAUDE_SESSION_ID once.
_AMBIENT_NAMES: tuple[str, ...] = (
    *AMBIENT_IDENTITY_ENV,
    # Harness config roots. resolve_plugin_script takes the plugin roots as
    # authoritative, so a suite run inside a live session resolves the
    # DEVELOPER's checkout as the plugin payload and silently overrules a
    # fixture that built its own tree.
    "CLAUDE_PLUGIN_ROOT",
    "CODEX_PLUGIN_ROOT",
    "CLAUDE_CONFIG_DIR",  # the account-alias channel; picks which bill is paid
    "CODEX_HOME",  # 12 reads in source; a real per-developer setting
    "GEMINI_PROJECT_DIR",
    "GEMINI_SANDBOX",
    "CLAUDE_CLI",
    "CLI",  # legacy harness selector; CLI=codex flips harness resolution
    "CLAUDE_CODE_STOP_HOOK_BLOCK_CAP",
    # State-path overrides. Each one relocates a store a test then reads.
    "EVENTS_FILE",
    "STATE_FILE",
    "POSTMORTEMS_DIR",
    "POSTMORTEM_CORRECTIONS_LOG",
    "WORKTREE_STATUS_REGISTRY",
    "TASK_LOCK_TTL_HOURS",
    "POST_MERGE_NONINTERACTIVE",
    "MCP_CHANNEL_INBOUND_POKE",
    # Credentials, connections and provider ROUTE. No test in this suite should
    # reach a real provider or database; if one does, it must fail rather than
    # succeed against the developer's account.
    #
    # The route vars are here despite not being read through env::var in source:
    # they are inherited by anything the suite spawns, so a developer shell that
    # exports ANTHROPIC_BASE_URL / ANTHROPIC_MODEL silently redirects a spawned
    # child to a different provider than the one the test names. That is not
    # hypothetical - a session on 2026-08-11 carried ANTHROPIC_MODEL set to a
    # non-Anthropic model while the base URL was unset.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "DATABASE_URL",
    # Shell-prompt config the mux integration reads.
    "STARSHIP_CONFIG",
    "ZDOTDIR",
    # Git identity and config location. Re-pinned into the sandbox below; see
    # _git_pins for what leaks through ~/.gitconfig in both directions.
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
)

# Read by source but NOT ambient state: the process needs these to run, and
# neutralising them would break the runner rather than isolate it. Listed
# explicitly so ``test_ambient_surface.py`` can tell "deliberately kept" from
# "nobody has looked at it yet".
_ENVIRONMENT: tuple[str, ...] = (
    "PATH",
    "PWD",
    "SHELL",
    "USER",
    "USERNAME",
    "TERM",
    "COLORTERM",
    "PYTHONPATH",
    "PYTEST_CURRENT_TEST",
    "CI",  # CI-gated behaviour is deliberate; the graph tripwire keys on it
    "INVOCATION_ID",
    "CRON_JOB",
    "HOME",  # pinned into the sandbox below rather than dropped
    "USERPROFILE",
    "XDG_STATE_HOME",  # pinned into the sandbox below
    "XDG_CACHE_HOME",  # a cache, preserved at its real value
    "CARGO_HOME",  # ditto
    # A unix socket path, not a source of answers. Sandboxing it risks the
    # 108-byte sockaddr limit under a long pytest tmpdir, which would break the
    # mux tests for no isolation gain.
    "XDG_RUNTIME_DIR",
    # Where every test builds its own fixture. Neutralising it would relocate
    # the sandbox itself, and it is read here only to widen the config ceiling
    # so a test's OWN config stays findable.
    "TMPDIR",
)

# Set deliberately by a CI workflow, not inherited from a developer's shell.
# This is the ONE place a runner-configured FNO_* var is exempted; a step that
# needs a new one adds it here with a comment naming the workflow line, and
# until then it is dropped and the step fails loudly rather than reading a
# developer's value. (Vars set inline in a smoke step's own command, e.g.
# ``FNO_CLAIMS_COMPAT_REQUIRED=1 uv run pytest ...``, are set inside the child
# and never travel this path.)
_RUNNER_PASSTHROUGH = (
    "FNO_REAL_CODEX_PLUGIN_TEST",  # .github/workflows/cli-ci.yml
    "FNO_RUST_FRONT",  # .github/workflows/cli-ci.yml, via $GITHUB_ENV
)

# Toolchain CACHES, not state fno reads. Sandboxing HOME relocates them, which
# turns a four-minute suite into a full rebuild, so they are resolved at their
# REAL values before the swap and re-exported. This list is the one
# hand-maintained part of the module; it is short on purpose, and a miss shows
# up as a slow suite rather than as a wrong answer.
_CACHE_NAMES = (
    "CARGO_HOME",
    "RUSTUP_HOME",
    "UV_CACHE_DIR",
    "PIP_CACHE_DIR",
    "XDG_CACHE_HOME",
    "npm_config_cache",
)

# Cache defaults live under the real home, so an unset var still has to be
# pinned explicitly - otherwise the sandboxed HOME silently redirects it.
_CACHE_DEFAULTS = {
    "CARGO_HOME": ".cargo",
    "RUSTUP_HOME": ".rustup",
    "XDG_CACHE_HOME": ".cache",
}

# XDG config/data/state are sandboxed (they are state); XDG_CACHE_HOME is not
# (it is a cache) and is handled above.
_XDG_SANDBOXED = ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME")

# A deliberately UNCOVERED name: it carries no ``FNO_`` prefix, is not a session
# marker, and is not a plugin root, so nothing in this module scrubs it. That is
# the point. It simulates the channel this inventory has not thought of yet, and
# the canary test that reads it is the dirty lane's positive control: green
# clean, RED dirty. A lane never proven able to go red is an absence-only
# success condition, which cannot tell "no leaks" from "the instrument never
# ran".
#
# It deliberately does NOT start with FNO_: that prefix is swept as a class
# above, so an FNO_-named canary would be scrubbed like any other ambient var
# and would disarm the control it exists to provide.
AMBIENT_LEAK_CANARY = "AMBIENT_LEAK_CANARY"


def _is_ambient(name: str) -> bool:
    # An explicit keep always wins over a prefix sweep, so a future TARGET_/FNO_
    # name the runner genuinely needs can be exempted in one place.
    if name in _RUNNER_PASSTHROUGH or name in _ENVIRONMENT:
        return False
    if name in _AMBIENT_NAMES:
        return True
    return any(name.startswith(p) for p in _AMBIENT_PREFIXES)


def ambient_names(env: Optional[Mapping[str, str]] = None) -> tuple[str, ...]:
    """Names in ``env`` this module considers ambient. Sorted, for reporting."""
    src = os.environ if env is None else env
    return tuple(sorted(n for n in src if _is_ambient(n)))


def classify(name: str) -> str:
    """``"ambient"``, ``"environment"``, or ``"unclassified"``.

    ``test_ambient_surface.py`` walks every env read in source and fails on an
    ``"unclassified"`` result. That is the guard: a new env var cannot be added
    to source without someone deciding, in this file, whether a test may see the
    developer's value for it. Nobody has to remember - CI asks.
    """
    if name in _RUNNER_PASSTHROUGH or name in _ENVIRONMENT:
        return "environment"
    if _is_ambient(name):
        return "ambient"
    return "unclassified"


def _passwd_home() -> str:
    """The real home from the passwd database, NOT from ``$HOME``.

    Load-bearing for the lane invariant: the dirty lane poisons ``$HOME``, so
    deriving a cache default from the environment would make ``CARGO_HOME``
    differ between lanes and report a leak that is really just lane skew.
    The passwd entry is the same in both lanes.
    """
    try:
        import pwd  # POSIX only

        return pwd.getpwuid(os.getuid()).pw_dir
    except Exception:
        return os.path.expanduser("~")


def _real_caches(src: Mapping[str, str]) -> dict:
    """Cache locations resolved against the REAL home, before HOME is swapped."""
    home = _passwd_home()
    out = {}
    for name in _CACHE_NAMES:
        value = src.get(name) or (
            os.path.join(home, _CACHE_DEFAULTS[name]) if name in _CACHE_DEFAULTS else ""
        )
        if value:
            out[name] = value
    return out


def neutralise(
    env: Optional[Mapping[str, str]] = None,
    sandbox: Optional[Path] = None,
) -> dict:
    """Return ``env`` with every ambient channel neutralised against ``sandbox``.

    Drops the ambient surface wholesale, then re-establishes the pins from
    ``sandbox`` rather than from the parent, so a poisoned parent and a clean
    parent produce the same result. That equality is the invariant the dirty
    lane rests on; ``test_hermetic.py`` asserts it directly.

    ``sandbox`` is created if absent. The caller owns its lifetime.
    """
    src = dict(os.environ if env is None else env)
    caches = _real_caches(src)

    out = {k: v for k, v in src.items() if not _is_ambient(k)}

    if sandbox is None:
        raise ValueError("neutralise() needs a sandbox directory to pin state into")
    sandbox = Path(sandbox)
    home = sandbox / "home"
    (home / ".fno").mkdir(parents=True, exist_ok=True)

    # State: HOME (POSIX) and USERPROFILE (Windows, which Path.home() reads).
    out["HOME"] = str(home)
    out["USERPROFILE"] = str(home)
    for name in _XDG_SANDBOXED:
        out[name] = str(sandbox / "xdg" / name.lower())

    # Config chain. The developer's FNO_CONFIG / FNO_GLOBAL_SETTINGS_PATH are
    # already gone with the rest of the FNO_* sweep; what remains is to stop
    # DISCOVERY finding their files.
    #
    # FNO_CONFIG is deliberately NOT re-pinned. Pinning it to a single path
    # overrides project-local discovery for every test, which breaks the
    # config-writing and worktree-policy suites for no isolation gain: the
    # sandboxed HOME already relocates ~/.fno/config.toml, and the ceiling below
    # bounds the rest of the chain.
    #
    # The ceiling is the part HOME cannot do. The candidate chain climbs to the
    # canonical checkout through ``git worktree list``, and git ignores HOME, so
    # without a ceiling a suite run from a real checkout reads that checkout's
    # config and local-red never equals CI-red.
    #
    # TMPDIR is in the ceiling because a test's OWN config has to be findable.
    # Shell harnesses build their fixture under `mktemp -d` and pytest builds
    # basetemp under the same root, so a ceiling of sandbox-only rejects the
    # config the test just wrote and the test reads defaults instead. The real
    # checkout and the real home are still outside every entry, which is the
    # thing the ceiling exists to exclude.
    ceiling = [str(sandbox), str(home)]
    tmpdir = os.environ.get("TMPDIR") or "/tmp"
    for candidate in (tmpdir, os.path.realpath(tmpdir)):
        if candidate not in ceiling:
            ceiling.append(candidate)
    out["FNO_CONFIG_SEARCH_ROOT"] = os.pathsep.join(ceiling)
    # FNO_GLOBAL_SETTINGS_PATH is scrubbed and NOT re-pinned either. The global
    # candidate is ``Path.home() / .fno / settings.yaml``, so the sandboxed HOME
    # above already relocates it - the old per-tree ``/dev/null`` pin was
    # standing in for a HOME redirect that tree did not have. Re-pinning it here
    # would additionally OVERRIDE the candidate for tests that monkeypatch HOME
    # to exercise the global-fallback path, which is a behaviour change dressed
    # as isolation.

    # FNO_REPO_ROOT is scrubbed with the rest of FNO_* but NOT re-pinned.
    # Pinning it points repo-root resolution at an empty sandbox, and a large
    # part of the suite legitimately resolves the real checkout to find a lint
    # script or the installed package. Unset is also exactly what CI has, which
    # is the state this module exists to reproduce.
    #
    # That leaves the carve-out ledger channel (specimen 3) open here, and it
    # has to be: ``_carveout_ledger_root`` resolves from the caller's CWD via
    # ``git worktree list``, so no environment variable can close it. It is
    # closed at the reader instead - see ``_hermetic_promise_carveout_gate`` in
    # cli/tests/conftest.py - and a shell test closes it by running from a
    # directory that is not a worktree.

    # Env-gated side effects that would reach the host machine. These are the
    # pins the pytest conftest carried inline; they live here now so the shell
    # and cargo trees get them too.
    out["FNO_THINK_SPAWN"] = "0"  # never spawn a real /think worker
    out["FNO_SPAWN_GATE"] = "0"  # never queue behind the host's live workers
    out["FNO_E2E"] = "1"  # arm idle-exit so an orphaned mux server reaps itself
    # The spawn seam publishes the computed writable-dir grant on the AMBIENT
    # process env, because os.execv is what carries it to the Rust client. In
    # a test tree that makes it leak: xdist reuses one process per worker, so a
    # test that reaches the seam pins its directories onto every later test in
    # that worker, and the codex-resume argv builders read it. Measured as five
    # cross-file failures that vanish when the files run alone. Scrub it for the
    # same reason the harness session markers are scrubbed - a hermetic run must
    # not inherit a live session's state.
    out.pop("FNO_WORKER_ADD_DIRS", None)
    # The operator needs panel folds the checkout journal, and repo-root
    # resolution cannot be sandboxed (see the FNO_REPO_ROOT note above), so an
    # unpathed append_event under test lands a production-shaped row on a live
    # operator surface. Six such fixture rows were measured in the panel on
    # 2026-08-17. Pin the journal itself instead of teaching either reader to
    # recognise test data.
    #
    # This env reaches the pytest, shell, and cargo trees, because all three come
    # through this function, and all three writers read it: the resolver above,
    # `scripts/lib/events.sh`, and `claim_events_path` in
    # crates/fno-agents/src/claims.rs. That last one is not optional. Python and
    # Rust share the claim journal AND its `.lock.d` mutex as a wire contract, so
    # a pin only one side honored would split the writers onto different files
    # and stop the mutex serialising them; the cross-impl merge gate catches it.
    #
    # Still outside the pin: the three loop-journal `ProjectJournalPath` sites in
    # fno-agents, which build their path by hand and consult no var.
    out["FNO_EVENTS_PATH"] = str(sandbox / "events.jsonl")

    out.update(caches)
    out.update(_git_pins(sandbox))
    out["FNO_TEST_HERMETIC"] = "1"  # receipt: this env came through neutralise()
    return out


# A synthetic global gitconfig. `~/.gitconfig` is an ambient channel HOME does
# not close, because git resolves it before HOME in some layouts and because
# nothing in the FNO_/TARGET_ sweep touches it. What leaks through it:
#
#   - the developer's user.name / user.email, which end up stamped on every
#     commit a test fixture makes (`git log` in a temp repo currently reads
#     the real author);
#   - init.defaultBranch, which renames the branch a test just created;
#   - core.* and alias.*, which can change what a plain `git` invocation does.
#
# It leaks in the other direction too: exactly one shell harness
# (scripts/tests/test_git_protection_hook.sh) commits without setting a local
# identity, so it passes on any machine with a ~/.gitconfig and has nothing to
# fall back on when there is none.
#
# Pinned to a WRITABLE sandbox file rather than /dev/null so a test that wants
# to write global git config still can. No shell harness uses
# `git config --global` today, but breaking that is not worth the byte saved.
_GIT_IDENTITY = """\
[user]
\tname = fno test
\temail = fno-test@localhost
[init]
\tdefaultBranch = main
[commit]
\tgpgsign = false
"""


def _git_pins(sandbox: Path) -> dict:
    gitconfig = sandbox / "gitconfig"
    if not gitconfig.exists():
        gitconfig.write_text(_GIT_IDENTITY, encoding="utf-8")
    return {
        "GIT_CONFIG_GLOBAL": str(gitconfig),
        "GIT_CONFIG_SYSTEM": os.devnull,
    }


def poison(env: Optional[Mapping[str, str]] = None, fixtures: Optional[Path] = None) -> dict:
    """Return ``env`` with synthetic ambient state set, for the dirty lane.

    Values are sentinels (``fno-poison-*``) rather than plausible-looking data:
    a leak has to fail loudly, and a realistic value might quietly pass and
    teach nobody anything.

    The profile is derived from the three 2026-08-11 specimens, not imagined:
    a routed-codex config (specimen 2), a global settings file (specimen 2's
    sibling channel), an unharvested deferred carve-out (specimen 3), and live
    session identity markers (specimen 1).
    """
    src = dict(os.environ if env is None else env)
    if fixtures is None:
        raise ValueError("poison() needs the ambient-poison fixture directory")
    fixtures = Path(fixtures)

    out = dict(src)
    # Specimen 1: identity the command itself resolves.
    for marker in AMBIENT_IDENTITY_ENV:
        out[marker] = f"fno-poison-session-{marker.lower()}"
    out["CLAUDE_PLUGIN_ROOT"] = str(fixtures / "poison-plugin-root")
    out["FNO_NODE"] = "x-poison"
    out["FNO_SLUG"] = "fno-poison-slug"
    out["FNO_PLAN"] = str(fixtures / "fno-poison-plan.md")

    # Specimen 2: the config chain.
    out["FNO_CONFIG"] = str(fixtures / "config.toml")
    out["FNO_GLOBAL_SETTINGS_PATH"] = str(fixtures / "settings.yaml")
    out.pop("FNO_CONFIG_SEARCH_ROOT", None)  # unbounded, as a real dev box is

    # Specimen 3: the real state ledger.
    out["HOME"] = str(fixtures / "home")
    out["USERPROFILE"] = str(fixtures / "home")
    out["FNO_REPO_ROOT"] = str(fixtures / "repo")

    # The simulated missed channel. See AMBIENT_LEAK_CANARY.
    out[AMBIENT_LEAK_CANARY] = "fno-poison-canary"
    return out
