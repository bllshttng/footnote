"""Tests for `fno target start` - the one-verb cold-start (x-d91b).

Covers the pure name sanitizer plus the four command branches with the
subprocess + setup-hook stubbed so no real worktree/state is created:
  * already-isolated -> no-op, nothing spawned (Boundary).
  * happy path -> ensure + setup-hook + init, receipt `node=claimed`.
  * existing manifest -> idempotent skip, init NOT re-run (Invariant).
  * ensure failure -> loud non-zero, init never reached (Errors).
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from typer.testing import CliRunner

from fno import target_cli
from fno.target_cli import _wt_name, target_app

runner = CliRunner()


# ----------------------------- pure sanitizer ----------------------------- #
def test_wt_name_node_id_roundtrips():
    assert _wt_name("x-d91b") == "x-d91b"


def test_wt_name_slugifies_free_text():
    assert _wt_name("Fix the Login Bug!") == "fix-the-login-bug"


def test_wt_name_never_empty():
    assert _wt_name("///") == "target"


def test_wt_name_bounded():
    assert len(_wt_name("a" * 200)) == 60


def test_wt_name_no_trailing_hyphen_after_truncation():
    # Truncation lands on the hyphen at index 59 -> must be stripped (gemini #114).
    out = _wt_name("a" * 59 + "-bug")
    assert not out.endswith("-")
    assert out == "a" * 59


# --------------------------- slug -> id resolution ------------------------ #
def test_resolve_node_id_upgrades_slug(monkeypatch):
    from types import SimpleNamespace

    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr("fno.graph.load.load_graph", lambda p: [])
    monkeypatch.setattr(
        "fno.graph.fuzzy.resolve_node",
        lambda q, e: SimpleNamespace(kind="exact", id="ab-1a2b3c4d"),
    )
    assert target_cli._resolve_node_id("dashless-spawn") == "ab-1a2b3c4d"


def test_resolve_node_id_freetext_fallthrough(monkeypatch):
    from types import SimpleNamespace

    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr("fno.graph.load.load_graph", lambda p: [])
    monkeypatch.setattr(
        "fno.graph.fuzzy.resolve_node",
        lambda q, e: SimpleNamespace(kind="none", id=None),
    )
    assert target_cli._resolve_node_id("fix the login bug") == "fix the login bug"


# ------------------------------- no-op branch ----------------------------- #
def test_already_isolated_is_noop(monkeypatch):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda nid: None)
    spawned = []
    monkeypatch.setattr(
        target_cli.subprocess, "run", lambda *a, **k: spawned.append(a) or None
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "already isolated" in result.stdout
    assert spawned == []  # nothing created


# --------------------------- happy path + idempotency --------------------- #
def _wire_happy(monkeypatch, wt_path: Path, *, manifest_exists: bool):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli, "_git_out", lambda cwd, *a: "/canonical/repo"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda r, w: (0, "")
    )
    if manifest_exists:
        (wt_path / ".fno").mkdir(parents=True, exist_ok=True)
        (wt_path / ".fno" / "target-state.md").write_text("session_id: x\n")

    init_calls = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt_path), stderr="")
        if "init" in args:
            init_calls.append(kwargs.get("cwd"))
            return subprocess.CompletedProcess(args, 0)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    return init_calls


def test_happy_path_claims_and_prints_receipt(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=False)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert f"worktree={wt}" in result.stdout
    assert "base=origin/main" in result.stdout
    assert "node=claimed" in result.stdout
    # init ran exactly once, from inside the worktree (binds owner_cwd).
    assert init_calls == [str(wt)]


def test_start_forwards_model_provider_to_init(monkeypatch, tmp_path):
    """--model/--provider ride through to the composed `fno target init` call."""
    wt = tmp_path / "wt"
    wt.mkdir()
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    monkeypatch.setattr("fno.worktree._run_setup_worktree_hook", lambda r, w: (0, ""))

    init_args = {}

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt), stderr="")
        if "init" in args:
            init_args["args"] = list(args)
            return subprocess.CompletedProcess(args, 0)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    result = runner.invoke(
        target_app, ["start", "x-d91b", "--model", "glm-4.7", "--provider", "codex"]
    )
    assert result.exit_code == 0, result.stdout
    a = init_args["args"]
    assert a[a.index("--model") + 1] == "glm-4.7"
    assert a[a.index("--provider") + 1] == "codex"


def test_existing_manifest_is_idempotent(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=True)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "node=already-claimed" in result.stdout
    assert init_calls == []  # invariant: never double-claim


def test_ensure_failure_is_loud_and_skips_init(monkeypatch, tmp_path):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    init_calls = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 1, stdout="", stderr="boom")
        if "init" in args:
            init_calls.append(True)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert init_calls == []  # never proceed past a failed ensure


# --------------------------- tier projection (x-d7a7) --------------------- #
def _wire_start(monkeypatch, wt: Path):
    """Stub the four seams `start` shells so only model threading is exercised.

    Returns the list `start` builds as the `fno target init` argv (captured).
    """
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    monkeypatch.setattr("fno.worktree._run_setup_worktree_hook", lambda r, w: (0, ""))
    init_args: list[str] = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt), stderr="")
        if "init" in args:
            init_args.extend(args)
            return subprocess.CompletedProcess(args, 0)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    return init_args


def test_start_bare_tiered_node_threads_resolved_model(monkeypatch, tmp_path):
    """AC1-HP: bare start on a tiered node carries the resolved model + source."""
    wt = tmp_path / "wt"
    wt.mkdir()
    init_args = _wire_start(monkeypatch, wt)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None: ("claude-sonnet-5", "task-tier(medium)"),
    )
    result = runner.invoke(target_app, ["start", "x-d7a7"])
    assert result.exit_code == 0, result.stdout
    assert init_args[init_args.index("--model") + 1] == "claude-sonnet-5"
    assert "model=claude-sonnet-5 (task-tier(medium))" in result.stdout


def test_start_explicit_model_wins_over_tier(monkeypatch, tmp_path):
    """AC1-EDGE: an explicit -m wins and the node is never loaded to read a tier."""
    wt = tmp_path / "wt"
    wt.mkdir()
    init_args = _wire_start(monkeypatch, wt)

    def _boom(nid):  # node lookup must NOT run when -m is explicit (short-circuit)
        raise AssertionError("node loaded despite explicit -m")

    monkeypatch.setattr(target_cli, "_find_node", _boom)
    result = runner.invoke(target_app, ["start", "x-d7a7", "-m", "glm-4.7"])
    assert result.exit_code == 0, result.stdout
    assert init_args[init_args.index("--model") + 1] == "glm-4.7"
    assert "model=glm-4.7 (explicit)" in result.stdout


def test_start_untiered_node_forwards_no_model(monkeypatch, tmp_path):
    """Invariant: a node with no pin/tier -> no --model, receipt byte-identical."""
    wt = tmp_path / "wt"
    wt.mkdir()
    init_args = _wire_start(monkeypatch, wt)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None: (None, "provider-default"),
    )
    result = runner.invoke(target_app, ["start", "x-d7a7"])
    assert result.exit_code == 0, result.stdout
    assert "--model" not in init_args
    assert "model=" not in result.stdout
    assert result.stdout.rstrip().endswith("node=claimed")


def test_resolve_node_model_degrades_on_error(monkeypatch):
    """AC1-ERR: any load/resolve error -> (None, provider-default), never raises."""

    def _raise(_p):
        raise RuntimeError("snapshot unreadable")

    monkeypatch.setattr("fno.graph.load.load_graph", _raise)
    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    assert target_cli._resolve_node_model("x-d7a7") == (None, "provider-default")


def test_resolve_node_model_error_preserves_explicit(monkeypatch):
    """A resolve error with an explicit -m degrades to that value, not the default."""

    def _raise(**_kw):
        raise RuntimeError("router boom")

    monkeypatch.setattr("fno.route_resolve.resolve_dispatch_model", _raise)
    assert target_cli._resolve_node_model("x-d7a7", explicit="glm-4.7") == (
        "glm-4.7",
        "explicit",
    )


def test_resolve_node_model_uses_route_resolve(monkeypatch):
    """The helper reads the node's model/model_tier and defers to route_resolve."""
    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda p: [{"id": "x-d7a7", "model_tier": "high"}],
    )
    seen = {}

    def fake_resolve(*, task_model, task_tier, **_kw):
        seen["task_model"] = task_model
        seen["task_tier"] = task_tier
        return "high-model", "task-tier(high)", ["tier(high)"]

    monkeypatch.setattr(
        "fno.route_resolve.resolve_dispatch_model", fake_resolve
    )
    assert target_cli._resolve_node_model("x-d7a7") == ("high-model", "task-tier(high)")
    assert seen == {"task_model": None, "task_tier": "high"}


def test_resolve_node_model_scopes_by_provider(monkeypatch):
    """The seam scopes tier resolution by the provider it is handed (x-da6e)."""
    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda p: [{"id": "x-d7a7", "model_tier": "medium"}],
    )
    seen = {}

    def fake_resolve(*, provider, **_kw):
        seen["provider"] = provider
        return "claude-sonnet-5", "task-tier(medium)", ["tier(medium)"]

    monkeypatch.setattr("fno.route_resolve.resolve_dispatch_model", fake_resolve)
    target_cli._resolve_node_model("x-d7a7", provider="claude")
    assert seen == {"provider": "claude"}


def test_resolve_model_command_prints_model(monkeypatch):
    """`fno target resolve-model` prints the resolved model for bash dispatchers."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None: ("claude-sonnet-5", "task-tier(medium)"),
    )
    result = runner.invoke(target_app, ["resolve-model", "x-d7a7"])
    assert result.exit_code == 0
    assert result.stdout.strip() == "claude-sonnet-5"


def test_resolve_model_command_empty_when_no_model(monkeypatch):
    """No pin/tier -> prints nothing (caller uses the provider default)."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None: (None, "provider-default"),
    )
    result = runner.invoke(target_app, ["resolve-model", "x-d7a7"])
    assert result.exit_code == 0
    assert result.stdout.strip() == ""


def test_resolve_model_provider_filter_drops_cross_harness(monkeypatch):
    """--provider claude drops a tier that resolved to a codex model (bg is claude-only)."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None: ("gpt-5.4", "task-tier(medium)"),
    )
    # gpt-5.4 maps to the codex harness in the real REACHABILITY table.
    result = runner.invoke(
        target_app, ["resolve-model", "x-d7a7", "--provider", "claude"]
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == ""  # dropped -> caller uses the provider default


def test_resolve_model_provider_filter_keeps_same_harness(monkeypatch):
    """--provider claude keeps a claude-reachable tier model."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None: ("claude-sonnet-5", "task-tier(medium)"),
    )
    result = runner.invoke(
        target_app, ["resolve-model", "x-d7a7", "--provider", "claude"]
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == "claude-sonnet-5"


def test_model_reachable_by_conservative_on_unknown(monkeypatch):
    """An unknown model is treated as reachable (guard only drops CONFIRMED mismatches)."""
    assert target_cli._model_reachable_by("gpt-5.4", "claude") is False
    assert target_cli._model_reachable_by("claude-sonnet-5", "claude") is True
    assert target_cli._model_reachable_by("some-unmapped-model", "claude") is True


# ================= ownership guard: refuse a foreign live session (x-84fc) =====
# _foreign_live_holder unit tests -------------------------------------------- #
def _wire_claim(monkeypatch, status, *, own_pid=None):
    monkeypatch.setattr("fno.claims.core.claim_status", lambda key, root=None: status)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: own_pid
    )


def test_foreign_live_holder_free_returns_none(monkeypatch):
    # AC2-ERR: a free claim -> proceed.
    _wire_claim(monkeypatch, {"key": "node:N", "state": "free"})
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_dead_returns_none(monkeypatch):
    # AC2-ERR: stale/dead is not live/suspect -> proceed unchanged.
    _wire_claim(
        monkeypatch,
        {"key": "node:N", "state": "stale", "holder": "target-session:A"},
    )
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_different_live_returns_info(monkeypatch):
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=123)  # own pid != holder pid
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_suspect_cross_host_returns_info(monkeypatch):
    # A suspect holder (live-on-another-host) folds into refuse.
    status = {
        "key": "node:N", "state": "suspect",
        "holder": "target-session:A", "pid": 999, "host": "other",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_lapsed_ttl_still_live(monkeypatch):
    # AC1-EDGE: classify() returns "live" from the durable pid even with TTL
    # lapsed -> the guard still surfaces the holder (park, not "idle").
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_ours_by_tsid(monkeypatch):
    # AC1-ERR: same-session identity by TARGET_SESSION_ID -> not foreign.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:X", "pid": 1, "host": "h",
    }
    _wire_claim(monkeypatch, status)
    monkeypatch.setenv("TARGET_SESSION_ID", "X")
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_ours_by_codex_thread(monkeypatch):
    # Codex parity: a codex session's claim owner is its CODEX_THREAD_ID (no
    # TARGET_SESSION_ID), so a same-thread re-run must NOT be seen as foreign.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:thread-abc", "pid": 1, "host": "h",
    }
    _wire_claim(monkeypatch, status)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-abc")
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_different_codex_thread_is_foreign(monkeypatch):
    # A DIFFERENT codex thread's live claim is still foreign -> refuse.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:thread-OTHER", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-abc")
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_ours_by_pid_host(monkeypatch):
    # Bare interactive re-run: durable pid + host match -> not foreign.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:Z", "pid": 555, "host": "myhost",
    }
    _wire_claim(monkeypatch, status, own_pid=555)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setattr(target_cli.socket, "gethostname", lambda: "myhost")
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_probe_error_degrades_none(monkeypatch):
    # AC1-FR: claim_status raising -> None (never blocks a legit start).
    def boom(key, root=None):
        raise RuntimeError("corrupt claim file")

    monkeypatch.setattr("fno.claims.core.claim_status", boom)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_uncapturable_pid_parks(monkeypatch):
    # AC2-FR: own pid None + no TSID + foreign live -> return info (park).
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_gethostname_raises_parks(monkeypatch):
    # Contract: never raises. socket.gethostname() can OSError in a sandbox ->
    # own identity is uncapturable -> a foreign live claim parks (AC2-FR).
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 555, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=555)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)

    def boom():
        raise OSError("no hostname in sandbox")

    monkeypatch.setattr(target_cli.socket, "gethostname", boom)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_freetext_reads_free(monkeypatch):
    # AC2-EDGE: a free-text arg keys node:<text>, reads free, never false-refuses.
    seen = {}

    def status(key, root=None):
        seen["key"] = key
        return {"key": key, "state": "free"}

    monkeypatch.setattr("fno.claims.core.claim_status", status)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    assert target_cli._foreign_live_holder("fix the login bug") is None
    assert seen["key"] == "node:fix the login bug"


# manifest-present exit (Site B) --------------------------------------------- #
def test_manifest_present_foreign_holder_refuses(monkeypatch, tmp_path):
    # AC1-HP: foreign live holder at the manifest-present exit -> park, exit 1.
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(
        target_cli, "_foreign_live_holder",
        lambda nid: {"holder": "target-session:A", "pid": 4321, "host": "boxA"},
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert "target-session:A" in result.output
    assert "pid=4321" in result.output
    assert "boxA" in result.output
    assert "node=already-claimed" not in result.output
    assert init_calls == []  # never proceeds into a shared worktree


def test_manifest_present_own_rerun_proceeds(monkeypatch, tmp_path):
    # AC1-ERR: guard returns None (ours/dead) -> today's already-claimed receipt.
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda nid: None)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "node=already-claimed" in result.output


# already-isolated exit (Site A) --------------------------------------------- #
def test_already_isolated_foreign_holder_refuses(monkeypatch):
    # AC2-HP: cwd is a foreign live session's worktree -> park, exit 1.
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli, "_foreign_live_holder",
        lambda nid: {"holder": "target-session:A", "pid": 4321, "host": "boxA"},
    )
    spawned = []
    monkeypatch.setattr(
        target_cli.subprocess, "run", lambda *a, **k: spawned.append(a) or None
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert "target-session:A" in result.output
    assert "already isolated" not in result.output
    assert spawned == []  # never created/entered anything


# shared park-message printer (Site 1.4) ------------------------------------- #
def test_park_message_names_holder_pid_host_worktree(capsys):
    info = {"holder": "target-session:A", "pid": 4321, "host": "boxA"}
    target_cli._print_foreign_holder_park("x-84fc", info, Path("/wt/x-84fc"))
    err = capsys.readouterr().err
    assert "x-84fc" in err  # node id
    assert "target-session:A" in err  # holder
    assert "pid=4321" in err  # pid
    assert "boxA" in err  # host
    assert "/wt/x-84fc" in err  # worktree path
