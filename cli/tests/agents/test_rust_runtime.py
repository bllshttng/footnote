"""Tests for the opt-in Rust runtime path (Phase 6 W6).

Covers gating (``FNO_AGENTS_RUNTIME``), binary resolution order, argv
forwarding, the missing-binary error, and that ``fno agents`` short-circuits
to the binary only when opted in. The actual ``os.execv`` is always stubbed so
the test process is never replaced.
"""
from __future__ import annotations

import stat
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.agents import rust_runtime as rr


@pytest.fixture(autouse=True)
def _isolated_registry(monkeypatch, tmp_path_factory):
    """Default the agents registry to a missing path so tests don't pick up
    the developer's real ``~/.fno/agents/registry.json``. The routing
    tests stub ``route_to_rust`` so the Rust client (which reads the registry to
    decide create-vs-resume) never runs, but isolating the path keeps the suite
    deterministic on a dev machine with real registered agents.

    Tests that need a specific registry override this with their own
    ``monkeypatch.setattr(_paths, "agents_registry_path", ...)`` -- the
    test-local setattr wins because monkeypatch records overrides in order.
    """
    import fno.paths as _paths

    missing = tmp_path_factory.mktemp("registry-isolated") / "missing.json"
    monkeypatch.setattr(_paths, "agents_registry_path", lambda: missing)


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _make_exe(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\nexit 0\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


# --------------------------------------------------------------------------- #
# rust_runtime_enabled
# --------------------------------------------------------------------------- #

@pytest.mark.parametrize(
    "value,expected",
    [("rust", True), ("RUST", True), (" rust ", True), ("rs", False),
     ("python", False), ("", False), ("1", False)],
)
def test_runtime_enabled_gating(monkeypatch, value, expected) -> None:
    monkeypatch.setenv(rr.RUNTIME_ENV, value)
    assert rr.rust_runtime_enabled() is expected


def test_runtime_disabled_when_unset(monkeypatch) -> None:
    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    assert rr.rust_runtime_enabled() is False


# --------------------------------------------------------------------------- #
# runtime_mode: rust | python | auto
# --------------------------------------------------------------------------- #

@pytest.mark.parametrize(
    "value,expected",
    [
        ("rust", "rust"), ("RUST", "rust"), (" rust ", "rust"),
        ("python", "python"), ("PYTHON", "python"), (" python ", "python"),
        # Unset / empty / unrecognized all collapse to the default: auto.
        ("", "auto"), ("rs", "auto"), ("1", "auto"), ("py", "auto"),
    ],
)
def test_runtime_mode_resolution(monkeypatch, value, expected) -> None:
    monkeypatch.setenv(rr.RUNTIME_ENV, value)
    assert rr.runtime_mode() == expected


def test_runtime_mode_auto_when_unset(monkeypatch) -> None:
    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    assert rr.runtime_mode() == "auto"


# --------------------------------------------------------------------------- #
# resolve_installed_binary: bundled -> sibling -> PATH, NEVER cargo dev
# --------------------------------------------------------------------------- #

def test_installed_resolve_prefers_bundled(monkeypatch, tmp_path) -> None:
    bundled = _make_exe(tmp_path / "bundled" / rr.BINARY_NAME)
    monkeypatch.setattr(rr, "_bundled_binary", lambda: bundled)
    monkeypatch.setattr(rr, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rr, "_path_binary", lambda: None)
    assert rr.resolve_installed_binary() == bundled


def test_installed_resolve_excludes_cargo_dev(monkeypatch, tmp_path) -> None:
    """A cargo dev artifact must NOT satisfy the installed-only resolver: a dev
    checkout stays on Python by default (the test process is never replaced)."""
    dev = _make_exe(tmp_path / "target" / "release" / rr.BINARY_NAME)
    monkeypatch.setattr(rr, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rr, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rr, "_path_binary", lambda: None)
    # Even if the cargo dev finder would resolve, the installed-only path ignores it.
    monkeypatch.setattr(rr, "_cargo_dev_binary", lambda: dev)
    assert rr.resolve_installed_binary() is None


# --------------------------------------------------------------------------- #
# route_to_rust: pre-resolved binary skips _resolve (auto happy path)
# --------------------------------------------------------------------------- #

def test_route_uses_preresolved_binary(monkeypatch, tmp_path) -> None:
    """When `binary=` is passed, _resolve is never consulted."""
    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    calls: list = []

    def fake_exec(path, argv):
        calls.append((path, list(argv)))
        raise SystemExit(0)

    def resolve_must_not_run() -> None:
        raise AssertionError("pre-resolved binary must skip _resolve")

    with pytest.raises(SystemExit) as exc:
        rr.route_to_rust(
            ["list"], binary=binary, _exec=fake_exec, _resolve=resolve_must_not_run
        )
    assert exc.value.code == 0
    assert calls == [(str(binary), [str(binary), "list"])]


# --------------------------------------------------------------------------- #
# resolve_binary resolution order: bundled -> PATH -> cargo dev
# --------------------------------------------------------------------------- #

def test_resolve_prefers_bundled(monkeypatch, tmp_path) -> None:
    bundled = _make_exe(tmp_path / "bundled" / rr.BINARY_NAME)
    on_path = _make_exe(tmp_path / "path" / rr.BINARY_NAME)
    monkeypatch.setattr(rr, "_bundled_binary", lambda: bundled)
    monkeypatch.setattr(rr, "_path_binary", lambda: on_path)
    assert rr.resolve_binary() == bundled


def test_resolve_falls_back_to_sibling(monkeypatch, tmp_path) -> None:
    sibling = _make_exe(tmp_path / "venvbin" / rr.BINARY_NAME)
    monkeypatch.setattr(rr, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rr, "_sibling_binary", lambda: sibling)
    monkeypatch.setattr(rr, "_path_binary", lambda: None)
    monkeypatch.setattr(rr, "_cargo_dev_binary", lambda: None)
    assert rr.resolve_binary() == sibling


def test_resolve_falls_back_to_path(monkeypatch, tmp_path) -> None:
    on_path = _make_exe(tmp_path / "path" / rr.BINARY_NAME)
    monkeypatch.setattr(rr, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rr, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rr, "_path_binary", lambda: on_path)
    monkeypatch.setattr(rr, "_cargo_dev_binary", lambda: None)
    assert rr.resolve_binary() == on_path


def test_resolve_falls_back_to_cargo_dev(monkeypatch, tmp_path) -> None:
    dev = _make_exe(tmp_path / "target" / "release" / rr.BINARY_NAME)
    monkeypatch.setattr(rr, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rr, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rr, "_path_binary", lambda: None)
    monkeypatch.setattr(rr, "_cargo_dev_binary", lambda: dev)
    assert rr.resolve_binary() == dev


def test_resolve_returns_none_when_absent(monkeypatch) -> None:
    monkeypatch.setattr(rr, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rr, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rr, "_path_binary", lambda: None)
    monkeypatch.setattr(rr, "_cargo_dev_binary", lambda: None)
    assert rr.resolve_binary() is None


def test_sibling_binary_finds_next_to_launcher(monkeypatch, tmp_path) -> None:
    """_sibling_binary resolves the co-installed binary via the launcher dir."""
    bindir = tmp_path / "venvbin"
    launcher = _make_exe(bindir / "fno")
    sibling = _make_exe(bindir / rr.BINARY_NAME)
    monkeypatch.setattr(rr.sys, "argv", [str(launcher), "agents", "ask"])
    assert rr._sibling_binary() == sibling


def test_sibling_binary_none_when_absent(monkeypatch, tmp_path) -> None:
    launcher = _make_exe(tmp_path / "venvbin" / "fno")  # no fno-agents beside it
    monkeypatch.setattr(rr.sys, "argv", [str(launcher)])
    assert rr._sibling_binary() is None


def test_path_binary_uses_which(monkeypatch, tmp_path) -> None:
    target = _make_exe(tmp_path / rr.BINARY_NAME)
    monkeypatch.setattr(rr.shutil, "which", lambda name: str(target) if name == rr.BINARY_NAME else None)
    assert rr._path_binary() == target


# --------------------------------------------------------------------------- #
# route_to_rust: argv forwarding + missing-binary error
# --------------------------------------------------------------------------- #

def test_route_forwards_verb_and_args(monkeypatch, tmp_path) -> None:
    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    calls: list = []

    def fake_exec(path, argv):
        calls.append((path, list(argv)))
        raise SystemExit(0)  # mimic process replacement terminating the test path

    with pytest.raises(SystemExit) as exc:
        rr.route_to_rust(
            ["ask", "worker-A", "hello", "--provider", "codex"],
            _exec=fake_exec,
            _resolve=lambda: binary,
        )
    assert exc.value.code == 0
    assert calls == [(str(binary), [str(binary), "ask", "worker-A", "hello", "--provider", "codex"])]


def test_route_missing_binary_exits_127(capsys) -> None:
    with pytest.raises(SystemExit) as exc:
        rr.route_to_rust(["list"], _resolve=lambda: None)
    assert exc.value.code == rr.BIN_NOT_FOUND_EXIT
    err = capsys.readouterr().err
    assert rr.BINARY_NAME in err
    assert "not found" in err


def test_route_exec_failure_exits_127(capsys, tmp_path) -> None:
    """A resolved binary whose exec raises OSError fails legibly, not as a traceback."""
    binary = _make_exe(tmp_path / rr.BINARY_NAME)

    def boom_exec(path, argv):
        raise OSError("Exec format error")

    with pytest.raises(SystemExit) as exc:
        rr.route_to_rust(["ask", "x", "hi"], _exec=boom_exec, _resolve=lambda: binary)
    assert exc.value.code == rr.BIN_NOT_FOUND_EXIT
    err = capsys.readouterr().err
    assert "failed to exec" in err
    assert "Exec format error" in err


# --------------------------------------------------------------------------- #
# Integration: fno agents short-circuits ONLY when opted in
# --------------------------------------------------------------------------- #

def test_agents_group_execs_when_opted_in(monkeypatch) -> None:
    """With the env set, `fno agents ask ...` execs the binary with raw argv."""
    from fno.cli import app

    captured: list = []

    def fake_route(args):
        captured.append(list(args))
        raise SystemExit(0)

    monkeypatch.setenv(rr.RUNTIME_ENV, "rust")
    monkeypatch.setattr(rr, "route_to_rust", fake_route)
    result = CliRunner().invoke(app, ["agents", "ask", "worker-A", "hi", "--provider", "codex"])
    assert result.exit_code == 0
    assert captured == [["ask", "worker-A", "hi", "--provider", "codex"]]


def test_spawn_seam_injects_config_defaults(monkeypatch) -> None:
    """x-de9d US8: the seam injects config.agents.defaults into a bare spawn
    argv before the route, so the Rust route sees the config provider."""
    from fno.cli import app
    import fno.config as _config

    class _D:
        provider, model, effort = "codex", "gpt-5.6-sol", ""

    class _S:
        agents = type("A", (), {"defaults": _D()})()

    monkeypatch.setattr(_config, "load_settings", lambda: _S())

    captured: list = []

    def fake_route(args, **kw):
        captured.append(list(args))
        raise SystemExit(0)

    # bg substrate keeps the spawn on the Rust route (pane would force Python).
    monkeypatch.setenv(rr.RUNTIME_ENV, "rust")
    monkeypatch.setattr(rr, "route_to_rust", fake_route)
    result = CliRunner().invoke(
        app, ["agents", "spawn", "worker-A", "hi", "--substrate", "bg"]
    )
    assert result.exit_code == 0
    argv = captured[0]
    assert argv[0] == "spawn"
    assert "--provider" in argv and argv[argv.index("--provider") + 1] == "codex"
    assert "--model" in argv and argv[argv.index("--model") + 1] == "gpt-5.6-sol"
    # positionals still resolvable after the injected flags
    assert argv[-2:] == ["worker-A", "hi"] or "worker-A" in argv


def test_agents_help_falls_through_when_opted_in(monkeypatch) -> None:
    """`fno agents --help` stays on the Python help even with the env set."""
    from fno.cli import app

    def boom(args):
        raise AssertionError("should not exec for --help")

    monkeypatch.setenv(rr.RUNTIME_ENV, "rust")
    monkeypatch.setattr(rr, "route_to_rust", boom)
    result = CliRunner().invoke(app, ["agents", "--help"])
    assert result.exit_code == 0
    assert "agent" in result.output.lower()


def test_agents_default_path_untouched_when_unset(monkeypatch) -> None:
    """A bare ``agents --help`` stays on the Python group help in every mode."""
    from fno.cli import app

    def boom(args, **kw):
        raise AssertionError("group --help must not exec the Rust binary")

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "route_to_rust", boom)
    # --help is enough to exercise the make_context path without a real spawn.
    result = CliRunner().invoke(app, ["agents", "--help"])
    assert result.exit_code == 0


# --------------------------------------------------------------------------- #
# Default flip: auto mode routes only the daemon-native verbs (no Python
# contract to regress) to an *installed* binary; verbs Python owns stay on
# Python. `<verb> [--help]` exercises the routing decision in make_context
# without running a real command body or a spawn (route_to_rust is stubbed).
# A `called[]` list records routing so fall-through cases assert on routing,
# not on whatever exit code the Python dispatch happens to produce.
# --------------------------------------------------------------------------- #

def test_auto_routes_daemon_native_verb_when_installed(monkeypatch, tmp_path) -> None:
    """Default (auto): a Rust-only verb (no Python contract) routes to the binary."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    captured: list = []

    def fake_route(args, **kw):
        captured.append((list(args), kw.get("binary")))
        raise SystemExit(99)

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", fake_route)
    # `status` exists only in the Rust client, so routing it regresses nothing.
    result = CliRunner().invoke(app, ["agents", "status"])
    assert result.exit_code == 99
    assert captured == [(["status"], binary)]


def test_shared_verbs_and_ask_all_auto_route() -> None:
    """Since ab-73da4ac2 every shared verb AND ``ask`` (all providers) auto-route.
    The prior carve-outs are all gone: stop/rm (Task 2.1), list/reconcile (Task
    3.1), and ``ask`` (the last holdout — claude ab-cc926b4e, codex ab-0429c6e1,
    gemini ab-73da4ac2).

    Messaging (send/inbox/ack) moved out of ``fno agents`` into ``fno mail``
    (ab-cee91152), so they are no longer agents verbs at all - not in
    ``PYTHON_AGENT_VERBS`` nor ``AUTO_ROUTE_VERBS``.
    """
    for v in ("stop", "rm", "list", "reconcile", "ask"):
        assert v in rr.AUTO_ROUTE_VERBS, f"{v} must auto-route"
        assert v not in rr.PYTHON_AGENT_VERBS, f"{v} must not be in PYTHON_AGENT_VERBS"
    # send/inbox/ack are no longer agents verbs (moved to `fno mail`).
    for v in ("send", "inbox", "ack"):
        assert v not in rr.PYTHON_AGENT_VERBS, f"{v} moved to fno mail; not an agents verb"
        assert v not in rr.AUTO_ROUTE_VERBS, f"{v} moved to fno mail; not an agents verb"


def test_auto_falls_back_to_python_without_installed_binary(monkeypatch) -> None:
    """Default (auto): an auto-routable verb with no installed binary does not route."""
    from fno.cli import app

    called: list = []

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: None)
    monkeypatch.setattr(rr, "route_to_rust", lambda args, **kw: called.append(list(args)))
    CliRunner().invoke(app, ["agents", "status"])
    assert called == []


def test_ask_help_forwards_to_binary(monkeypatch, tmp_path) -> None:
    """Default (auto): `ask <verb> --help` forwards to the binary (it owns the
    verb's help) now that ask auto-routes — there are no Python-only verbs left.
    (A bare `agents -h/--help` still falls through to the Python group help; that
    is the `args[0] in (-h, --help)` guard, covered separately.)"""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    called: list = []

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", lambda args, **kw: called.append(list(args)))
    CliRunner().invoke(app, ["agents", "ask", "--help"])
    assert called == [["ask", "--help"]]


def test_python_mode_forces_python_for_daemon_native_verb(monkeypatch, tmp_path) -> None:
    """=python suppresses routing even for an auto-routable verb with a binary."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    called: list = []

    monkeypatch.setenv(rr.RUNTIME_ENV, "python")
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", lambda args, **kw: called.append(list(args)))
    CliRunner().invoke(app, ["agents", "status"])
    assert called == []


def test_rust_mode_forces_even_shared_verb(monkeypatch) -> None:
    """=rust forces the binary for every verb, including the shared/Python-only
    ones (the caller explicitly accepts the Rust surface; the binary owns the
    flag/output behavior)."""
    from fno.cli import app

    captured: list = []

    def fake_route(args, **kw):
        captured.append(list(args))
        raise SystemExit(99)

    monkeypatch.setenv(rr.RUNTIME_ENV, "rust")
    monkeypatch.setattr(rr, "route_to_rust", fake_route)
    result = CliRunner().invoke(app, ["agents", "list", "worker-A"])
    assert result.exit_code == 99
    assert captured == [["list", "worker-A"]]


# --------------------------------------------------------------------------- #
# Verb-set invariants + cross-source drift guards.
# --------------------------------------------------------------------------- #

def test_auto_route_verbs_have_no_python_contract() -> None:
    """AUTO_ROUTE_VERBS = Rust verbs MINUS the verbs Python implements: every
    auto-routed verb is daemon-native, so routing it cannot regress a Python
    flag/stdout contract.

    Since ab-73da4ac2 ``PYTHON_AGENT_VERBS`` is empty (the ``ask`` holdout was
    the last carve-out — claude ab-cc926b4e, codex ab-0429c6e1, gemini
    ab-73da4ac2), so AUTO_ROUTE_VERBS equals RUST_CLIENT_VERBS and that identity
    is the whole routing contract. Every shared verb plus ``ask`` auto-routes.
    """
    assert rr.AUTO_ROUTE_VERBS == rr.RUST_CLIENT_VERBS - rr.PYTHON_AGENT_VERBS
    assert rr.AUTO_ROUTE_VERBS.isdisjoint(rr.PYTHON_AGENT_VERBS)
    # PYTHON_AGENT_VERBS is empty, so the set difference is the identity.
    assert rr.AUTO_ROUTE_VERBS == rr.RUST_CLIENT_VERBS
    for parity in ("stop", "rm", "list", "reconcile", "ask"):
        assert parity in rr.AUTO_ROUTE_VERBS, f"{parity} must auto-route"


def test_python_agent_verbs_match_registered_commands() -> None:
    """PYTHON_AGENT_VERBS mirrors the Python-owned @agents_app.command registrations.

    Every Python ``@agents_app.command`` is preserved as fallback dispatch for
    ``FNO_AGENTS_RUNTIME=python`` mode and no-binary environments, but since
    ab-73da4ac2 NONE are carved out of auto-routing: ``PYTHON_AGENT_VERBS`` is
    empty. ``ask`` was the last holdout and now auto-routes for every provider,
    so it joins the rust-parity set. The guard remains so a NEW Python-only verb
    (one without a Rust client port) can't silently escape into auto-routing —
    it would land in ``python_owned`` and fail the empty-set assertion below.
    """
    from fno.agents.cli import agents_app

    registered = {cmd.name for cmd in agents_app.registered_commands}
    # Every registered command has a Rust client port and auto-routes; they
    # remain registered as the =python fallback but are no longer carved out.
    # ``ask`` joined this set with the gemini port + unconditional flip.
    rust_parity_verbs = {
        "stop",
        "rm",
        "list",
        "reconcile",
        "drive-authority",
        "trace",
        "ping",
        "resume",
        "attach",
        "logs",
        "ask",
        # Task 1.2: spawn gains a Python implementation (--once / claude plain
        # spawn) but stays in RUST_CLIENT_VERBS + AUTO_ROUTE_VERBS so the
        # daemon PTY worker path (codex/gemini without --once) still auto-routes
        # to Rust. It therefore belongs in this parity set: registered AND has
        # a Rust client port.
        "spawn",
    }
    python_owned = registered - rust_parity_verbs
    assert python_owned == set(rr.PYTHON_AGENT_VERBS), (
        "PYTHON_AGENT_VERBS is out of sync with the agents_app commands.\n"
        f"  only registered (excl. rust-parity): {sorted(python_owned - set(rr.PYTHON_AGENT_VERBS))}\n"
        f"  only in PYTHON_AGENT_VERBS: {sorted(set(rr.PYTHON_AGENT_VERBS) - registered)}"
    )
    # All rust-parity verbs must auto-route (not be in PYTHON_AGENT_VERBS).
    for v in rust_parity_verbs:
        assert v not in rr.PYTHON_AGENT_VERBS, f"{v} should not be in PYTHON_AGENT_VERBS after parity"
        assert v in rr.AUTO_ROUTE_VERBS, f"{v} must be in AUTO_ROUTE_VERBS after parity"


def _find_repo_root(start: Path) -> Path | None:
    """Walk up from ``start`` to the first ancestor containing ``crates/`` (the
    workspace root). Robust to the test file moving between ``cli/tests/...``
    depths -- unlike a fixed ``parents[N]`` index."""
    for parent in [start, *start.parents]:
        if (parent / "crates" / "fno-agents" / "src" / "bin" / "client.rs").is_file():
            return parent
    return None


def test_rust_client_verbs_match_client_rs() -> None:
    """RUST_CLIENT_VERBS mirrors the routable verbs in client.rs.

    Parses the Rust client source and compares its dispatchable verbs to the
    Python-side allowlist so a new verb on either side fails CI instead of
    silently mis-routing. Skipped when the crate source is absent (e.g. an
    installed sdist test env without the workspace).
    """
    import re

    repo_root = _find_repo_root(Path(__file__).resolve().parent)
    if repo_root is None:
        pytest.skip("crates/fno-agents/src/bin/client.rs not present in this checkout")
    client_rs = repo_root / "crates" / "fno-agents" / "src" / "bin" / "client.rs"

    # Strip `//` line comments before matching so a commented-out or
    # illustrative `"x" =>` in a comment can't inject a phantom verb (gemini).
    # client.rs has no string literal containing `//`, so a plain split is safe
    # here; revisit if that ever changes.
    src = "\n".join(line.split("//", 1)[0] for line in client_rs.read_text().splitlines())
    # build_request match arms: `"verb" =>`  (drive flags are `--…`, excluded by
    # [a-z] start). Scope the scan to build_request's body: since Task 1.3a the
    # file also contains a PROVIDER match (`"claude" =>` inside maybe_run_spawn)
    # whose arms are not verbs and must not leak into this set.
    build_request_body = src.split("fn build_request", 1)[1].split("\nfn ", 1)[0]
    arms = set(re.findall(r'"([a-z][a-z0-9-]*)"\s*=>', build_request_body))
    # Verbs dispatched before build_request via `verb == "…"` (drive, status; the
    # `--emit-schema` flag starts with `-` so it is excluded by the same anchor).
    specials = set(re.findall(r'verb == "([a-z][a-z0-9-]*)"', src))
    routable = arms | specials

    assert routable == set(rr.RUST_CLIENT_VERBS), (
        "RUST_CLIENT_VERBS is out of sync with client.rs routable verbs.\n"
        f"  only in client.rs: {sorted(routable - set(rr.RUST_CLIENT_VERBS))}\n"
        f"  only in RUST_CLIENT_VERBS: {sorted(set(rr.RUST_CLIENT_VERBS) - routable)}"
    )


def test_harness_markers_match_client_rs() -> None:
    """Rust HARNESS_MARKERS mirrors Python HARNESS_SESSION_MARKERS, order included.

    The Rust unit test only checks the Rust const against a hard-coded Rust
    literal, so a drift on the Python side leaves both suites green while the two
    runtimes infer different harnesses. This reads the actual Rust source and
    asserts the (env_var, harness) sequences equal Python's canonical table --
    order is load-bearing (it is the priority list). Skipped when the crate
    source is absent (installed sdist test env without the workspace).
    """
    import re

    from fno.harness_identity import HARNESS_SESSION_MARKERS

    repo_root = _find_repo_root(Path(__file__).resolve().parent)
    if repo_root is None:
        pytest.skip("crates/fno-agents/src/bin/client.rs not present in this checkout")
    client_rs = repo_root / "crates" / "fno-agents" / "src" / "bin" / "client.rs"

    src = client_rs.read_text()
    block = re.search(r"const HARNESS_MARKERS[^=]*=\s*&\[(.*?)\];", src, re.DOTALL)
    assert block, "HARNESS_MARKERS const not found in client.rs"
    rust_pairs = re.findall(r'\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*\)', block.group(1))

    assert rust_pairs == [tuple(p) for p in HARNESS_SESSION_MARKERS], (
        "HARNESS_MARKERS is out of sync between Rust and Python.\n"
        f"  client.rs: {rust_pairs}\n"
        f"  harness_identity.HARNESS_SESSION_MARKERS: {list(HARNESS_SESSION_MARKERS)}"
    )


# --------------------------------------------------------------------------- #
# Unconditional `ask` auto-routing (ab-73da4ac2).
#
# Since the gemini client-side ask port landed, `ask` is in AUTO_ROUTE_VERBS for
# EVERY provider — the Rust client owns create/resume + the unresolvable-create
# exit-2 surface. There is no provider-conditional branch and no make_context
# registry lookup anymore; the only gate is "installed binary present".
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("provider", ["claude", "codex", "gemini"])
def test_ask_auto_routes_for_every_provider(monkeypatch, tmp_path, provider) -> None:
    """`fno agents ask <name> <msg> --provider <p>` routes to the installed Rust
    binary under the default auto runtime for claude, codex, AND gemini — the
    unconditional flip; gemini no longer stays on Python."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    captured: list = []

    def fake_route(args, **kw):
        captured.append((list(args), kw.get("binary")))
        raise SystemExit(99)

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", fake_route)
    result = CliRunner().invoke(app, ["agents", "ask", "newagent", "hi", "--provider", provider])
    assert result.exit_code == 99
    assert captured == [(["ask", "newagent", "hi", "--provider", provider], binary)]


def test_ask_routes_to_rust_without_provider_flag(monkeypatch, tmp_path) -> None:
    """A bare `ask <name> <msg>` (no `--provider`) also auto-routes: make_context
    no longer resolves the provider itself; the Rust client decides create vs
    resume and, when unresolvable, surfaces Python's exit-2 error itself."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    captured: list = []

    def fake_route(args, **kw):
        captured.append((list(args), kw.get("binary")))
        raise SystemExit(99)

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", fake_route)
    result = CliRunner().invoke(app, ["agents", "ask", "ghost", "hi"])
    assert result.exit_code == 99
    assert captured == [(["ask", "ghost", "hi"], binary)]


def test_ask_falls_back_to_python_without_installed_binary(monkeypatch) -> None:
    """With no installed binary, `ask` falls through to the mature Python
    dispatch (the no-binary fallback is intact for every provider)."""
    from fno.cli import app
    from fno.agents import dispatch as dispatch_mod

    called: list = []
    dispatched: list = []

    def fake_dispatch_ask(**kwargs):
        # Stub the Python dispatch so the test verifies the fallback is
        # SELECTED without EXECUTING a real provider call. Letting the real
        # dispatch_ask run shells `claude --bg`/codex/gemini and spawns a
        # live "newagent" session on every test run (the leak this guards).
        dispatched.append(kwargs)
        return SimpleNamespace(kind="create", short_id="stub", reply=None)

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: None)
    monkeypatch.setattr(rr, "route_to_rust", lambda args, **kw: called.append(list(args)))
    monkeypatch.setattr(dispatch_mod, "dispatch_ask", fake_dispatch_ask)
    CliRunner().invoke(app, ["agents", "ask", "newagent", "hi", "--provider", "gemini"])
    assert called == []
    assert [d["name"] for d in dispatched] == ["newagent"]


@pytest.mark.parametrize("provider", ["claude", "codex", "gemini"])
def test_python_mode_forces_python_for_ask(monkeypatch, tmp_path, provider) -> None:
    """`FNO_AGENTS_RUNTIME=python` keeps `ask` on the Python dispatch for every
    provider even with a binary present (the fallback must stay intact)."""
    from fno.cli import app
    from fno.agents import dispatch as dispatch_mod

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    called: list = []
    dispatched: list = []

    def fake_dispatch_ask(**kwargs):
        # Stub the Python dispatch so the test verifies python-mode is
        # SELECTED without EXECUTING a real provider call. Letting the real
        # dispatch_ask run shells `claude --bg`/codex/gemini and spawns a
        # live "any" session on every test run (the leak this guards).
        dispatched.append(kwargs)
        return SimpleNamespace(kind="create", short_id="stub", reply=None)

    monkeypatch.setenv(rr.RUNTIME_ENV, "python")
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", lambda args, **kw: called.append(list(args)))
    monkeypatch.setattr(dispatch_mod, "dispatch_ask", fake_dispatch_ask)
    CliRunner().invoke(app, ["agents", "ask", "any", "hi", "--provider", provider])
    assert called == [], f"=python must keep ask on Python for {provider}; got: {called}"
    assert [d["name"] for d in dispatched] == ["any"], (
        f"=python must reach the Python dispatch for {provider}; got: {dispatched}"
    )


# --------------------------------------------------------------------------- #
# Help completeness for the Rust-only verbs.
#
# A bare ``fno agents --help`` always renders the *Python* group help (it never
# execs the binary), so without the RUST_ONLY_VERB_HELP injection it would omit
# every Rust-only verb (spawn/status/trace/*-channel) and an
# agent reading the help could not discover them. These tests pin the listing,
# the no-drift guard, and the legible fallback when the verb is reached without
# an installed binary (instead of a bare "No such command").
# --------------------------------------------------------------------------- #

def test_agents_help_lists_every_rust_only_verb() -> None:
    """`fno agents --help` lists all Rust-only verbs with their descriptions."""
    from fno.cli import app

    result = CliRunner().invoke(app, ["agents", "--help"])
    assert result.exit_code == 0
    for verb in rr.RUST_ONLY_VERB_HELP:
        assert verb in result.output, f"`fno agents --help` is missing the Rust-only verb {verb!r}"
    # A representative rust-only verb must be discoverable.
    assert "trace" in result.output


def test_rust_only_verb_help_covers_unregistered_verbs() -> None:
    """RUST_ONLY_VERB_HELP keys == RUST_CLIENT_VERBS minus Python-registered verbs.

    This is the drift guard that keeps the help complete forever: a future
    Rust-only verb added to RUST_CLIENT_VERBS (and client.rs, per
    test_rust_client_verbs_match_client_rs) cannot land without a help entry,
    so it can never silently vanish from ``fno agents --help`` again.
    """
    from fno.agents.cli import agents_app

    registered = {cmd.name for cmd in agents_app.registered_commands}
    expected = set(rr.RUST_CLIENT_VERBS) - registered
    assert set(rr.RUST_ONLY_VERB_HELP) == expected, (
        "RUST_ONLY_VERB_HELP is out of sync with the Rust-only verb set.\n"
        f"  missing a help entry: {sorted(expected - set(rr.RUST_ONLY_VERB_HELP))}\n"
        f"  help entry for a non-Rust-only verb: {sorted(set(rr.RUST_ONLY_VERB_HELP) - expected)}"
    )


def test_rust_only_verbs_not_registered_as_python_commands() -> None:
    """The help injection must NOT add the Rust-only verbs to the Typer registry.

    ``registered_commands`` is the source of truth for "has a Python
    implementation" (test_python_agent_verbs_match_registered_commands relies on
    it); the injection lives in the Click group's list_commands/get_command, so
    these verbs surface in help without polluting that set.
    """
    from fno.agents.cli import agents_app

    registered = {cmd.name for cmd in agents_app.registered_commands}
    assert registered.isdisjoint(rr.RUST_ONLY_VERB_HELP)


def test_rust_only_verb_python_mode_emits_legible_message(monkeypatch) -> None:
    """`=python` for a Rust-only verb errors with guidance, not "No such command"."""
    from fno.cli import app

    monkeypatch.setenv(rr.RUNTIME_ENV, "python")
    result = CliRunner().invoke(app, ["agents", "restart"])
    assert result.exit_code == rr.BIN_NOT_FOUND_EXIT
    assert "no Python implementation" in result.output
    assert "No such command" not in result.output


def test_rust_only_verb_no_binary_emits_install_hint(monkeypatch) -> None:
    """Auto mode + no installed binary for a Rust-only verb points at the install path."""
    from fno.cli import app

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: None)
    result = CliRunner().invoke(app, ["agents", "restart"])
    assert result.exit_code == rr.BIN_NOT_FOUND_EXIT
    assert "Rust runtime" in result.output
    assert "No such command" not in result.output


def test_rust_only_verb_routes_when_installed(monkeypatch, tmp_path) -> None:
    """A Rust-only verb still auto-routes to the binary; the placeholder body is
    never reached when an installed binary is present (routing is untouched)."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    captured: list = []

    def fake_route(args, **kw):
        captured.append((list(args), kw.get("binary")))
        raise SystemExit(99)

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", fake_route)
    result = CliRunner().invoke(app, ["agents", "trace"])
    assert result.exit_code == 99
    assert captured == [(["trace"], binary)]


def test_retired_verb_emits_mux_pointer(monkeypatch, tmp_path) -> None:
    """A verb retired at G4 (grid/drive/host/promote) prints a one-line mux
    pointer and exits non-zero, even with an installed binary in auto mode -- it
    is not auto-routed and not a silent no-op (AC5-EDGE)."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    for verb in rr.RETIRED_VERB_POINTERS:
        result = CliRunner().invoke(app, ["agents", verb])
        assert result.exit_code == 2, f"{verb} should exit non-zero"
        assert "retired at G4" in result.output, f"{verb} should point at the mux"
        assert "No such command" not in result.output


# --------------------------------------------------------------------------- #
# Role-bearing spawn (x-d2fe): --role is Python-only, never routed to the binary
# (the Rust client cannot parse it). `--help` exercises the make_context routing
# decision without running a real spawn.
# --------------------------------------------------------------------------- #


def test_role_bearing_spawn_not_routed_to_rust(monkeypatch, tmp_path) -> None:
    """`spawn ... --role <r>` falls through to Python even with a binary present."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    called: list = []

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", lambda args, **kw: called.append(list(args)))
    CliRunner().invoke(app, ["agents", "spawn", "--role", "consolidate", "--help"])
    assert called == [], "a --role spawn must not route to the Rust binary"


def test_role_bearing_spawn_not_routed_in_forced_rust_mode(monkeypatch, tmp_path) -> None:
    """Even FNO_AGENTS_RUNTIME=rust must not route a --role spawn to the binary."""
    from fno.cli import app

    called: list = []
    monkeypatch.setenv(rr.RUNTIME_ENV, "rust")
    monkeypatch.setattr(rr, "route_to_rust", lambda args, **kw: called.append(list(args)))
    CliRunner().invoke(app, ["agents", "spawn", "--role=tidy", "--help"])
    assert called == []


def test_plain_spawn_stays_python_bg_spawn_auto_routes(monkeypatch, tmp_path) -> None:
    """4a-G2 routing: a plain spawn (default = pane substrate) is Python-owned
    (the mux back half) and must NOT route to the binary; a --substrate bg
    spawn still auto-routes."""
    from fno.cli import app

    binary = _make_exe(tmp_path / rr.BINARY_NAME)
    captured: list = []

    def fake_route(args, **kw):
        captured.append(list(args))
        raise SystemExit(99)

    monkeypatch.delenv(rr.RUNTIME_ENV, raising=False)
    monkeypatch.setattr(rr, "resolve_installed_binary", lambda: binary)
    monkeypatch.setattr(rr, "route_to_rust", fake_route)

    # Plain spawn: stays Python (the pane back half). It will fail inside the
    # Python dispatch (no mux in this test env), but must never exec the binary.
    CliRunner().invoke(app, ["agents", "spawn", "worker", "--provider", "claude"])
    assert captured == [], "a pane-substrate spawn must not route to the binary"

    # bg substrate: still the binary's lane.
    result = CliRunner().invoke(
        app,
        ["agents", "spawn", "worker", "--provider", "claude", "--substrate", "bg"],
    )
    assert result.exit_code == 99
    assert captured == [
        ["spawn", "worker", "--provider", "claude", "--substrate", "bg"]
    ]


def test_is_role_bearing_spawn_predicate() -> None:
    assert rr._is_role_bearing_spawn("spawn", ["spawn", "w", "--role", "tidy"])
    assert rr._is_role_bearing_spawn("spawn", ["spawn", "--role=orient"])
    # x-c772: -r is the mobile short for --role; must also stay Python-only.
    assert rr._is_role_bearing_spawn("spawn", ["spawn", "w", "-r", "tidy"])
    assert not rr._is_role_bearing_spawn("spawn", ["spawn", "w", "--provider", "claude"])
    assert not rr._is_role_bearing_spawn("ask", ["ask", "--role", "tidy"])
