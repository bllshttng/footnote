"""Tests for the mux-pane spawn back half (4a-G2, task 4.5).

``fno agents spawn --substrate pane`` hosts the agent as a mux pane via
``fno mux pane run`` and writes the registry row with the ``mux`` ref. The
mux subprocess is faked at the ``runner`` seam (the G1 e2e drives the real
socket); these tests pin the Python contract:

- AC1-HP  spawn -> `pane run --session --cwd -- env <mesh> <argv>`; row
          carries mux:{session, pane_id} + claude_session_uuid + child pid.
- AC1-ERR mux failure -> no half-created row, error names the mux session,
          no daemon fallback.
- AC1-FR  a claude argv carrying -p/--print is refused BEFORE any pane run.
- AC1-EDGE is Rust-side (pane run self-spawns the server; G1 e2e covers it).
- Routing: pane-substrate spawns never auto-route to the Rust client.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Optional

import pytest

from fno.paths_testing import use_tmpdir


class FakeRunner:
    """Record `fno mux ...` invocations; script the replies per verb."""

    def __init__(
        self,
        run_returncode: int = 0,
        run_stdout: str = "7\n",
        run_stderr: str = "",
        ls_stdout: Optional[str] = None,
        db_stdout: str = "",
    ) -> None:
        self.calls: list[list[str]] = []
        self.run_returncode = run_returncode
        self.run_stdout = run_stdout
        self.run_stderr = run_stderr
        self.ls_stdout = ls_stdout
        self.db_stdout = db_stdout

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        # The opencode spawn path reads opencode's session store through this
        # same seam (x-830c); default empty output = "no session captured", the
        # live-only row every non-opencode test already expects.
        if argv[:2] == ["opencode", "db"]:
            return subprocess.CompletedProcess(argv, 0, self.db_stdout, "")
        if argv[1:4] == ["mux", "pane", "run"]:
            return subprocess.CompletedProcess(
                argv, self.run_returncode, self.run_stdout, self.run_stderr
            )
        if argv[1:4] == ["mux", "pane", "ls"]:
            out = self.ls_stdout
            if out is None:
                out = json.dumps(
                    [{"pane_id": 7, "squad_id": 1, "tab_id": 1, "cwd": "/w", "child_pid": 4242}]
                )
            return subprocess.CompletedProcess(argv, 0, out, "")
        raise AssertionError(f"unexpected fno invocation: {argv}")


def _spawn(monkeypatch, tmp_path, **kwargs):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    from fno.agents.mux_spawn import dispatch_spawn_pane

    runner = kwargs.pop("runner", FakeRunner())
    result = dispatch_spawn_pane(
        name=kwargs.pop("name", "peer"),
        message=kwargs.pop("message", "hello"),
        provider=kwargs.pop("provider", "claude"),
        cwd=kwargs.pop("cwd", tmp_path),
        runner=runner,
        **kwargs,
    )
    return result, runner


def test_opencode_spawn_stamps_the_captured_session_id(
    tmp_path: Path, monkeypatch
) -> None:
    """x-830c: a unique store match lands on the row as harness_session_id.

    This is what makes the opencode resume lane reachable at all - the mapping
    and the resume argv both key on this field.
    """
    from fno.agents.registry import load_registry

    ses = "ses_09679f284ffeJv7NdBAoLQLnLZ"
    result, _ = _spawn(
        monkeypatch, tmp_path,
        provider="opencode",
        runner=FakeRunner(db_stdout=f"id\n{ses}\n"),
    )
    rows = load_registry()
    assert [r.harness_session_id for r in rows] == [ses]
    # US8: opencode resumes off harness_session_id, so short_id stays empty -
    # the jobId population is claude-only. Assert BOTH the row and the
    # receipt-facing result: a regressed claude guard would otherwise hand out a
    # non-hex `ses_0967` result handle while the row-only check still passed.
    assert rows[0].short_id == ""
    assert result.short_id == ""


def test_opencode_spawn_never_claims_another_rows_session_id(
    tmp_path: Path, monkeypatch
) -> None:
    """Two panes racing in one cwd must not both stamp the same session id.

    The second pane's session may not exist yet when both backfills query, so
    each sees the SAME lone candidate and the ambiguity rule cannot fire. The
    loser drops to live-only rather than pointing resume at the other pane.
    """
    from fno.agents.registry import load_registry

    ses = "ses_09679f284ffeJv7NdBAoLQLnLZ"
    _spawn(
        monkeypatch, tmp_path, name="oc-a",
        provider="opencode", runner=FakeRunner(db_stdout=f"{ses}\n"),
    )
    # Second pane, same cwd, backfill returns the SAME id (the race).
    _spawn(
        monkeypatch, tmp_path, name="oc-b",
        provider="opencode", runner=FakeRunner(db_stdout=f"{ses}\n"),
    )
    rows = {r.name: r.harness_session_id for r in load_registry()}
    assert rows["oc-a"] == ses, "the first pane to land owns the id"
    assert rows["oc-b"] is None, "the loser must not share the id"


def test_opencode_spawn_without_capture_stays_live_only(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-FR: a missed capture leaves the row exactly as it was before x-830c.

    The pane itself is unaffected - the spawn still succeeds and reports its
    pane id; only resume is unavailable until an id is captured.
    """
    from fno.agents.registry import load_registry

    result, _ = _spawn(
        monkeypatch, tmp_path, provider="opencode", runner=FakeRunner(db_stdout=""),
    )
    assert result.pane_id == 7
    rows = load_registry()
    assert [r.harness_session_id for r in rows] == [None]


def test_ac1_hp_spawn_pane_runs_mux_and_writes_mux_ref_row(
    tmp_path: Path, monkeypatch
) -> None:
    result, runner = _spawn(monkeypatch, tmp_path)

    # The hosting call is the G1 script API with the resolved session + cwd.
    run_call = runner.calls[0]
    assert run_call[1:4] == ["mux", "pane", "run"]
    assert "--claim" in run_call  # agent panes opt into the writer claim
    assert run_call[run_call.index("--session") + 1] == "main"
    assert run_call[run_call.index("--cwd") + 1] == str(tmp_path)
    # Mesh identity rides the env(1) wrapper after `--`.
    tail = run_call[run_call.index("--") + 1 :]
    assert tail[0] == "env"
    assert "FNO_AGENT_SELF=peer" in tail
    assert "FNO_AGENT_PROVIDER=claude" in tail
    assert "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1" in tail
    # The provider argv is interactive claude with the pinned session id.
    claude_at = tail.index("claude")
    assert tail[claude_at + 1] == "--session-id"
    assert tail[claude_at + 2] == result.session_uuid
    assert tail[claude_at + 3] == "hello"

    assert result.pane_id == 7
    assert result.session == "main"
    assert result.child_pid == 4242

    from fno.agents.registry import load_registry

    rows = load_registry()
    assert len(rows) == 1
    row = rows[0]
    assert row.mux == {"session": "main", "pane_id": 7}
    assert row.harness_session_id == result.session_uuid
    assert row.pid == 4242
    assert row.status == "live"
    # The row keeps short_id empty: it is the worker/bg transport slot, and a mux
    # row holds exactly one live ref (validate_single_live_ref).
    assert row.short_id == ""
    # US8: the receipt-facing result carries claude's 8-hex jobId so the king can
    # mail the pane straight from the spawn receipt...
    assert result.short_id == result.session_uuid[:8]
    # ...and that handle resolves back to this exact row via the derived_short
    # rule (harness_session_id[:8]), proving the receipt handle is addressable.
    from fno.agents.registry import resolve_agent_in

    resolved = resolve_agent_in(rows, result.short_id)
    assert resolved.entry.name == "peer"
    assert resolved.matched_by == "derived_short"


def test_ac1_hp_session_resolution_env_beats_default(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_SESSION", "work")
    from fno.agents.mux_spawn import dispatch_spawn_pane

    runner = FakeRunner()
    result = dispatch_spawn_pane(
        name="peer", message="", provider="claude", cwd=tmp_path, runner=runner
    )
    assert result.session == "work"
    run_call = runner.calls[0]
    assert run_call[run_call.index("--session") + 1] == "work"
    # An explicit session beats the env.
    runner2 = FakeRunner()
    result2 = dispatch_spawn_pane(
        name="peer2",
        message="",
        provider="claude",
        cwd=tmp_path,
        session="other",
        runner=runner2,
    )
    assert result2.session == "other"


def test_ac1_err_mux_failure_leaves_no_row_and_names_session(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError) as exc_info:
        _spawn(
            monkeypatch,
            tmp_path,
            runner=FakeRunner(run_returncode=1, run_stdout="", run_stderr="no pty"),
        )
    msg = str(exc_info.value)
    assert "'main'" in msg, f"error must name the mux session: {msg}"
    assert "no pty" in msg
    assert "fallback" in msg  # explicitly no daemon-PTY fallback

    from fno.agents.registry import load_registry

    assert load_registry() == [], "a failed spawn must not leave a half-created row"


def test_collision_refused_before_any_pane_spawn(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [AgentEntry(name="peer", harness="claude", cwd="/p", log_path="/l")]
    )
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="already exists") as exc_info:
        dispatch_spawn_pane(
            name="peer", message="", provider="claude", cwd=tmp_path, runner=runner
        )
    assert exc_info.value.exit_code == 2
    assert runner.calls == [], "collision must refuse before any mux subprocess"


def test_ac1_fr_billing_guard_refuses_print_argv_before_pane(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.mux_spawn as mux_spawn

    # The builder never emits -p by construction; force it to prove the guard
    # sits between argv resolution and the pane spawn.
    monkeypatch.setattr(
        mux_spawn, "build_pane_argv", lambda *a, **k: ["claude", "-p", "hi"]
    )
    runner = FakeRunner()
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="-p/--print"):
        mux_spawn.dispatch_spawn_pane(
            name="peer", message="hi", provider="claude", cwd=tmp_path, runner=runner
        )
    assert runner.calls == [], "the guard must fire BEFORE any pane exists"

    # The predicate itself, both spellings.
    assert not mux_spawn.claude_argv_is_interactive(["claude", "-p"])
    assert not mux_spawn.claude_argv_is_interactive(["claude", "--print", "x"])
    assert mux_spawn.claude_argv_is_interactive(["claude", "--session-id", "u", "msg"])


def test_build_pane_argv_provider_forms(tmp_path: Path) -> None:
    from fno.agents.mux_spawn import build_pane_argv

    claude = build_pane_argv("claude", "task", tmp_path, False, "uuid-1")
    assert claude == ["claude", "--session-id", "uuid-1", "task"]

    codex = build_pane_argv("codex", "task", tmp_path, False, None)
    assert codex[:3] == ["codex", "-C", str(tmp_path)]
    assert "--sandbox" in codex and codex[-1] == "task"
    codex_yolo = build_pane_argv("codex", "", tmp_path, True, None)
    assert "--dangerously-bypass-approvals-and-sandbox" in codex_yolo

    gemini = build_pane_argv("gemini", "task", tmp_path, False, None)
    assert gemini[:2] == ["gemini", "--skip-trust"]
    assert "-i" in gemini
    # Bare interactive session: no -i without a message.
    assert "-i" not in build_pane_argv("gemini", "", tmp_path, False, None)

    # x-8f7f US1: agy is never-prompt, stateless (no --session-id), message as
    # trailing positional; never `-p` (that is agy's headless/print form).
    agy = build_pane_argv("agy", "task", tmp_path, False, "ignored-uuid")
    assert agy == ["agy", "--dangerously-skip-permissions", "task"]
    assert "-p" not in agy and "--session-id" not in agy
    assert build_pane_argv("agy", "", tmp_path, False, None) == [
        "agy",
        "--dangerously-skip-permissions",
    ]

    # x-51f6 US2: bare `opencode` is the TUI; the message rides --prompt (the
    # positional is a PROJECT PATH, not a prompt), --auto only under yolo,
    # and never the headless `run` subcommand.
    # x-c772: opencode is always launched with a model (the z-ai/glm-5.2 default).
    opencode = build_pane_argv("opencode", "task", tmp_path, False, "ignored")
    assert opencode == ["opencode", "--prompt", "task", "--model", "z-ai/glm-5.2"]
    assert build_pane_argv("opencode", "", tmp_path, False, None) == [
        "opencode",
        "--model",
        "z-ai/glm-5.2",
    ]
    opencode_yolo = build_pane_argv("opencode", "task", tmp_path, True, None)
    assert opencode_yolo == [
        "opencode",
        "--prompt",
        "task",
        "--model",
        "z-ai/glm-5.2",
        "--auto",
    ]
    assert "run" not in opencode and "--session-id" not in opencode


def test_build_pane_argv_forwards_model(tmp_path: Path) -> None:
    # x-c772: an explicit --model reaches every pane provider's TUI flag
    # (opencode included, now that it is spawnable). Exact passthrough; opencode
    # uses the provider/model form and always carries a model (z-ai/glm-5.2 default).
    from fno.agents.mux_spawn import _PER_HARNESS_DEFAULT_MODEL, build_pane_argv

    cases = [
        ("claude", "u", "opus"),
        ("codex", None, "gpt-5.5"),
        ("gemini", None, "gemini-3-pro"),
        ("agy", None, "some-model"),
        ("opencode", None, "anthropic/claude-opus-4-8"),
    ]
    for provider, sid, model in cases:
        argv = build_pane_argv(provider, "t", tmp_path, False, sid, model)
        assert argv[argv.index("--model") + 1] == model, provider

    # claude/codex/gemini/agy: None/empty model -> no --model flag.
    for p in ("claude", "codex", "gemini", "agy"):
        assert "--model" not in build_pane_argv(p, "t", tmp_path, False, None, None)
        assert "--model" not in build_pane_argv(p, "t", tmp_path, False, None, "")

    # opencode ALWAYS carries a model: None/empty falls back to the default.
    for m in (None, ""):
        argv = build_pane_argv("opencode", "t", tmp_path, False, None, m)
        assert argv[argv.index("--model") + 1] == _PER_HARNESS_DEFAULT_MODEL["opencode"]


def test_opencode_default_is_a_table_lookup(tmp_path: Path, monkeypatch) -> None:
    # AC7-EDGE: opencode's default reads from the provider-keyed table, not a
    # hardcoded branch. Retargeting the entry retargets the injected argv;
    # removing it injects no --model at all; an explicit --model still wins.
    import fno.agents.mux_spawn as ms
    from fno.agents.mux_spawn import build_pane_argv

    # retarget: a sentinel entry flows straight through to argv
    monkeypatch.setattr(ms, "_PER_HARNESS_DEFAULT_MODEL", {"opencode": "sentinel/x"})
    argv = build_pane_argv("opencode", "t", tmp_path, False, None, None)
    assert argv[argv.index("--model") + 1] == "sentinel/x"

    # remove the entry: no --model injected (a hardcoded branch would still add one)
    monkeypatch.setattr(ms, "_PER_HARNESS_DEFAULT_MODEL", {})
    assert "--model" not in build_pane_argv("opencode", "t", tmp_path, False, None, None)

    # explicit --model overrides the (now empty) table
    argv = build_pane_argv("opencode", "t", tmp_path, False, None, "mine/y")
    assert argv[argv.index("--model") + 1] == "mine/y"


def test_build_pane_argv_forwards_tier3_flags(tmp_path: Path) -> None:
    # x-b6e2: --add-dir/--agent/--tools/--deny-tools map to claude's own
    # spellings in a fixed order; codex/agy map only --add-dir; opencode maps
    # only --agent. Fixed order enforces the Rust/Python parity contract.
    from fno.agents.mux_spawn import build_pane_argv

    claude = build_pane_argv(
        "claude", "t", tmp_path, False, "u",
        add_dir="/work", agent="reviewer", tools="Read,Edit", deny_tools="Bash",
    )
    # tokens present, in order.
    for a, b in [("--add-dir", "/work"), ("--agent", "reviewer"),
                 ("--allowedTools", "Read,Edit"), ("--disallowedTools", "Bash")]:
        assert claude[claude.index(a) + 1] == b
    assert claude.index("--add-dir") < claude.index("--agent") < \
        claude.index("--allowedTools") < claude.index("--disallowedTools")

    codex = build_pane_argv("codex", "t", tmp_path, False, None, add_dir="/extra")
    assert codex[codex.index("--add-dir") + 1] == "/extra"
    agy = build_pane_argv("agy", "t", tmp_path, False, None, add_dir="/extra")
    assert agy[agy.index("--add-dir") + 1] == "/extra"
    opencode = build_pane_argv("opencode", "t", tmp_path, False, None, agent="build")
    assert opencode[opencode.index("--agent") + 1] == "build"


def test_build_pane_argv_tier3_fails_closed(tmp_path: Path) -> None:
    # x-b6e2: a no-equivalent (provider, flag) cell raises BEFORE any spawn.
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import build_pane_argv

    closed = [
        ("codex", {"agent": "x"}),
        ("codex", {"tools": "Read"}),
        ("agy", {"deny_tools": "Bash"}),
        ("opencode", {"add_dir": "/w"}),
        ("gemini", {"add_dir": "/w"}),
        ("gemini", {"agent": "x"}),
    ]
    for provider, kw in closed:
        with pytest.raises(DispatchAskError):
            build_pane_argv(provider, "t", tmp_path, False, None, **kw)

    # AC2-ERR: an EMPTY value is unset, not a stray token - it must NOT trip the
    # fail-closed guard even on a no-equivalent provider (gemini review finding).
    for provider, kw in closed:
        argv = build_pane_argv(provider, "t", tmp_path, False, None, **{k: "" for k in kw})
        assert "--add-dir" not in argv and "--agent" not in argv


def test_pane_hostable_set_stays_in_sync_with_build_pane_argv(tmp_path: Path) -> None:
    """x-8f7f: PANE_HOSTABLE_PROVIDERS is the pane gate's source of truth and MUST
    match build_pane_argv's branches exactly - every listed provider builds argv,
    and a readable-but-argvless provider (opencode, staged inert until x-51f6) does
    NOT. This is the enforcement the borrowed READABLE_PROVIDERS list lacked."""
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import PANE_HOSTABLE_PROVIDERS, build_pane_argv
    from fno.agents.providers import READABLE_PROVIDERS

    for provider in PANE_HOSTABLE_PROVIDERS:
        argv = build_pane_argv(provider, "", tmp_path, False, None)
        assert argv and argv[0] == provider

    # opencode graduated from readable-but-argvless to pane-hostable (x-51f6);
    # the two sets coincide again until the next staged provider. Any future
    # readable-but-argvless provider must keep raising at build_pane_argv.
    assert "opencode" in PANE_HOSTABLE_PROVIDERS
    for readable in READABLE_PROVIDERS:
        if readable not in PANE_HOSTABLE_PROVIDERS:
            with pytest.raises(DispatchAskError, match="no interactive pane form"):
                build_pane_argv(readable, "", tmp_path, False, None)


def test_ac1_host_pane_gate_admits_hosted_rejects_unhosted(
    tmp_path: Path, monkeypatch
) -> None:
    """x-8f7f US1/US3 (+ x-51f6): the pane gate is PANE_HOSTABLE_PROVIDERS.
    agy and opencode (pane-hostable) are admitted and produce mux-hosted rows;
    a genuinely-unhosted CLI is rejected at the gate before any subprocess."""
    from fno.agents.dispatch import DispatchAskError

    # agy spawns a real (faked) mux pane -> a row lands.
    result, runner = _spawn(monkeypatch, tmp_path, provider="agy")
    assert result.provider == "agy"
    assert runner.calls[0][1:4] == ["mux", "pane", "run"]

    # opencode is pane-hostable since x-51f6 -> a row lands too.
    oc_result, oc_runner = _spawn(monkeypatch, tmp_path, provider="opencode", name="oc")
    assert oc_result.provider == "opencode"
    assert oc_runner.calls[0][1:4] == ["mux", "pane", "run"]

    # Registry-state assertion (not just the mocked call shape): a well-formed
    # row actually landed for both, mirroring the rigor of
    # test_ac1_hp_spawn_pane_runs_mux_and_writes_mux_ref_row's claude checks.
    from fno.agents.registry import load_registry

    rows = {row.name: row for row in load_registry()}
    assert set(rows) == {"peer", "oc"}
    agy_row = rows["peer"]
    assert agy_row.harness == "agy"
    assert agy_row.mux == {"session": "main", "pane_id": 7}  # FakeRunner default
    assert agy_row.status == "live"
    oc_row = rows["oc"]
    assert oc_row.harness == "opencode"
    assert oc_row.mux == {"session": "main", "pane_id": 7}
    assert oc_row.status == "live"

    # aider is not pane-hostable -> refused before any mux subprocess.
    with pytest.raises(DispatchAskError, match="unknown provider 'aider'"):
        _spawn(monkeypatch, tmp_path, provider="aider", name="ai")


def test_unparseable_pane_id_is_a_loud_error(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="unparseable"):
        _spawn(
            monkeypatch,
            tmp_path,
            runner=FakeRunner(run_stdout="not-a-pane-id\n"),
        )
    from fno.agents.registry import load_registry

    assert load_registry() == []


def test_child_pid_lookup_is_best_effort(tmp_path: Path, monkeypatch) -> None:
    # A broken `pane ls` must not fail the spawn: pid stays None.
    result, _ = _spawn(
        monkeypatch, tmp_path, runner=FakeRunner(ls_stdout="not json")
    )
    assert result.child_pid is None
    from fno.agents.registry import load_registry

    assert load_registry()[0].pid is None


def test_routing_pane_substrate_spawn_stays_python() -> None:
    """4a-G2 routing carve-out: a pane spawn (explicit or default) never
    auto-routes to the Rust client; bg/headless spawns still do."""
    from fno.agents.rust_runtime import _is_pane_substrate_spawn

    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer", "--substrate", "pane"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer", "--substrate=pane"])
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "p", "--substrate", "bg"])
    assert not _is_pane_substrate_spawn(
        "spawn", ["spawn", "p", "--substrate=headless"]
    )
    # x-c772: --headless / --once is the headless shortcut -> never a pane. `-H`
    # was reassigned to --harness (x-6de8), so `-H codex` is a default-pane spawn.
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "p", "--headless"])
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "p", "--once"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "p", "-H", "codex"])
    assert not _is_pane_substrate_spawn("ask", ["ask", "peer", "hi"])
    # The scan stops at --argv: payload tokens cannot masquerade as our flag.
    assert _is_pane_substrate_spawn(
        "spawn", ["spawn", "p", "--argv", "--substrate", "bg"]
    )


def test_routing_provenance_bearing_spawn_stays_python() -> None:
    """x-84a8: a spawn carrying --node/--slug/--plan is Python-only (the Rust
    client cannot parse them), even on a bg substrate that would otherwise route
    to the binary. Covers the /agent spawn.sh forward AND a direct CLI call."""
    from fno.agents.rust_runtime import _is_provenance_bearing_spawn

    assert _is_provenance_bearing_spawn("spawn", ["spawn", "p", "--node", "x-84a8"])
    assert _is_provenance_bearing_spawn("spawn", ["spawn", "p", "--node=x-84a8"])
    assert _is_provenance_bearing_spawn(
        "spawn", ["spawn", "p", "--substrate", "bg", "--slug", "s"]
    )
    assert _is_provenance_bearing_spawn("spawn", ["spawn", "p", "--plan", "a.md"])
    assert not _is_provenance_bearing_spawn("spawn", ["spawn", "p"])
    assert not _is_provenance_bearing_spawn("ask", ["ask", "p", "--node", "x"])


def test_provenance_vars_ride_wrapper_for_node_driven(
    tmp_path: Path, monkeypatch
) -> None:
    """x-84a8 AC(happy): a node-driven pane spawn exports FNO_NODE/SLUG/PLAN
    into the pane env alongside the mesh identity."""
    _, runner = _spawn(
        monkeypatch,
        tmp_path,
        provenance={"FNO_NODE": "x-84a8", "FNO_SLUG": "pane-prov", "FNO_PLAN": "p.md"},
    )
    tail = runner.calls[0][runner.calls[0].index("--") + 1 :]
    assert "FNO_NODE=x-84a8" in tail
    assert "FNO_SLUG=pane-prov" in tail
    assert "FNO_PLAN=p.md" in tail


def test_ad_hoc_spawn_exports_no_provenance(tmp_path: Path, monkeypatch) -> None:
    """x-84a8 AC(edge): an ad-hoc spawn (no node) exports no FNO_NODE/SLUG/PLAN,
    and no empty-string variants.

    Asserted against ASSIGNMENTS (`KEY=`), not bare tokens: the wrapper now also
    emits `-u KEY` to clear an inherited value, whose operand is the bare key.
    Unsetting satisfies this AC more strongly than omitting would.
    """
    _, runner = _spawn(monkeypatch, tmp_path)  # default: no provenance
    tail = runner.calls[0][runner.calls[0].index("--") + 1 :]
    assert not any(
        t.startswith(("FNO_NODE=", "FNO_SLUG=", "FNO_PLAN=")) for t in tail
    )


def test_resolve_provenance_branches(tmp_path: Path, monkeypatch) -> None:
    """resolve_provenance: explicit slug/plan skip the graph read; a linked plan
    yields FNO_PLAN, an empty one drops it; no node -> {}."""
    use_tmpdir(monkeypatch, tmp_path)  # empty graph, so any read misses
    from fno.agents.mux_spawn import resolve_provenance

    # No node -> nothing (the ad-hoc edge case at the resolver level).
    assert resolve_provenance(None) == {}

    # Explicit slug+plan: no graph needed, all three present.
    assert resolve_provenance("x-1", "the-slug", "plan.md") == {
        "FNO_NODE": "x-1",
        "FNO_SLUG": "the-slug",
        "FNO_PLAN": "plan.md",
    }

    # An unlinked plan (empty string) is dropped, slug kept.
    assert resolve_provenance("x-2", "s2", "") == {"FNO_NODE": "x-2", "FNO_SLUG": "s2"}

    # Unknown node + empty graph degrades to the node id alone (no raise).
    assert resolve_provenance("x-missing") == {"FNO_NODE": "x-missing"}


def test_cmd_spawn_node_flag_resolves_and_passes_provenance(
    tmp_path: Path, monkeypatch
) -> None:
    """x-84a8: `fno agents spawn --node ... --slug ... --plan ...` resolves the
    provenance map and hands it to dispatch_spawn_pane."""
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.mux_spawn import MuxSpawnResult

    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return MuxSpawnResult(
            name=kwargs["name"], provider=kwargs["provider"], session="main",
            pane_id=1, child_pid=None, session_uuid="u",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")

    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude",
         "--node", "x-84a8", "--slug", "s", "--plan", "p.md"],
    )
    assert res.exit_code == 0, res.output
    assert captured["provenance"] == {
        "FNO_NODE": "x-84a8", "FNO_SLUG": "s", "FNO_PLAN": "p.md",
    }


def test_cmd_spawn_pane_receipt_shape(tmp_path: Path, monkeypatch) -> None:
    """The CLI receipt is one JSON line, a superset of the daemon-spawn shape
    ({"name","short_id","provider","status"}) plus the mux fields."""
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    from fno.agents.mux_spawn import MuxSpawnResult

    def fake_dispatch(**kwargs):
        return MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="main",
            pane_id=9,
            child_pid=111,
            session_uuid="u-1",
        )

    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    # x-85fe: pin canonical == caller so this node-less spawn does NOT move to
    # the canonical root (AC1-EDGE no-op) -- the receipt/redirect note would
    # otherwise drift when run from a linked worktree. This test checks the pane
    # receipt shape, not the cwd move.
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())

    runner = CliRunner()
    result = runner.invoke(agents_cli.agents_app, ["spawn", "peer", "--harness", "claude"])
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[-1])
    assert receipt == {
        "name": "peer",
        "short_id": "",
        "provider": "claude",
        "provider_source": "explicit",  # dispatch-provider provenance
        "status": "live",
        "mux_session": "main",
        "pane_id": 9,
    }


# ---------------------------------------------------------------------------
# x-3e38 pane placement: squad/split on the outer pane-run transport
# ---------------------------------------------------------------------------


def test_placement_directives_ride_outer_pane_run_before_separator(
    tmp_path: Path, monkeypatch
) -> None:
    # AC1-HP/AC2-HP: placement rides the OUTER pane-run argv, before the `--`
    # fencing the provider argv; build_pane_argv stays placement-blind.
    _result, runner = _spawn(monkeypatch, tmp_path, squad="review", split="left")
    run_call = runner.calls[0]
    sep = run_call.index("--")
    outer = run_call[:sep]
    assert outer[outer.index("squad") + 1] == "review"
    assert outer[outer.index("split") + 1] == "left"
    tail = run_call[sep + 1 :]
    assert "squad" not in tail and "split" not in tail
    assert "claude" in tail  # provider argv unchanged


def test_placement_omitted_leaves_pane_run_argv_unchanged(
    tmp_path: Path, monkeypatch
) -> None:
    # AC4-EDGE: no placement -> exactly the current one-new-tab argv.
    _result, runner = _spawn(monkeypatch, tmp_path)
    run_call = runner.calls[0]
    sep = run_call.index("--")
    assert "squad" not in run_call[:sep] and "split" not in run_call[:sep]


def test_cmd_spawn_placement_rejected_on_bg_substrate(tmp_path: Path, monkeypatch) -> None:
    # AC4-ERR: pane geometry flags fail closed on a substrate with no pane tree,
    # before any spawn.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "bg", "-x", "left"],
    )
    assert res.exit_code == 2, res.output
    assert "--workspace/-s and --split/-x apply only to --substrate pane" in res.output


def test_cmd_spawn_rejects_bad_split_value(tmp_path: Path, monkeypatch) -> None:
    # AC4-ERR: an out-of-vocabulary direction is refused at the CLI boundary.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "-x", "diagonal"],
    )
    assert res.exit_code == 2, res.output
    assert "left, right, up, or down" in res.output


def test_cmd_spawn_rejects_blank_squad_before_dispatch(tmp_path: Path, monkeypatch) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "-s", ""],
    )
    assert res.exit_code == 2, res.output
    assert "--workspace/-s needs a nonblank workspace name" in res.output


@pytest.mark.parametrize(
    "placement_args",
    [
        ["--workspace", "review", "--split", "right"],
        ["--squad", "review", "--split", "right"],
        ["-s", "review", "-x", "right"],
    ],
)
def test_cmd_spawn_pane_threads_placement_to_dispatch(
    tmp_path: Path, monkeypatch, placement_args: list[str]
) -> None:
    # AC1-HP/AC2-HP: long and mobile aliases reach dispatch_spawn_pane.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.mux_spawn import MuxSpawnResult

    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="main",
            pane_id=1,
            child_pid=None,
            session_uuid="u",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", *placement_args],
    )
    assert res.exit_code == 0, res.output
    assert captured["squad"] == "review"
    assert captured["split"] == "right"


# ---------------------------------------------------------------------------
# _mesh_env_wrapper: a routed pane scrubs the parent's Anthropic creds (x-db50)
# ---------------------------------------------------------------------------


def test_mesh_env_wrapper_routed_pane_scrubs_anthropic_creds(monkeypatch):
    """A routed role must prefix `env -u ANTHROPIC_API_KEY -u
    CLAUDE_CODE_OAUTH_TOKEN` so a parent API key / subscription OAuth token
    cannot override the routed AUTH_TOKEN."""
    from fno.agents import mux_spawn
    from fno.agents import model_routing

    monkeypatch.setattr(
        model_routing,
        "resolve_route",
        lambda role, **kw: {
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "zk",
            "ANTHROPIC_MODEL": "glm-5.2",
        },
    )
    wrapped = mux_spawn._mesh_env_wrapper("w", "claude", "coordinate", ["claude"])
    assert wrapped[0] == "env"
    # -u flags precede any KEY=VAL assignment (env parses options first).
    assert "-u" in wrapped
    ui = wrapped.index("-u")
    unset_region = wrapped[ui : ui + 4]
    assert "ANTHROPIC_API_KEY" in unset_region
    assert "CLAUDE_CODE_OAUTH_TOKEN" in unset_region
    first_assign = next(i for i, t in enumerate(wrapped) if "=" in t)
    assert ui < first_assign  # unsets before assignments
    assert "ANTHROPIC_AUTH_TOKEN=zk" in wrapped


def test_mesh_env_wrapper_unrouted_pane_adds_no_unset(monkeypatch):
    """No role -> no route -> no AUTH scrub.

    Narrowed from "no `-u` at all": the wrapper now always clears the
    provenance triple, so a pane cannot inherit its spawner's node. The claim
    this test exists to make is about credentials, which are still untouched.
    """
    from fno.agents import mux_spawn

    wrapped = mux_spawn._mesh_env_wrapper("w", "claude", None, ["claude"])
    assert "ANTHROPIC_API_KEY" not in wrapped
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in wrapped
    assert wrapped[0] == "env"


def test_mesh_env_wrapper_role_without_key_adds_no_unset(monkeypatch):
    """A routed role that resolves to None (no key) must not scrub creds either.

    Narrowed alongside the unrouted case: the provenance clear is unconditional,
    the credential scrub is not.
    """
    from fno.agents import mux_spawn
    from fno.agents import model_routing

    monkeypatch.setattr(model_routing, "resolve_route", lambda role, **kw: None)
    wrapped = mux_spawn._mesh_env_wrapper("w", "claude", "coordinate", ["claude"])
    assert "ANTHROPIC_API_KEY" not in wrapped
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in wrapped


def test_mesh_env_wrapper_clears_inherited_provenance(monkeypatch):
    """A pane must not inherit its spawner's node.

    dispatch_spawn_pane hands the ambient environment to the self-spawning mux
    process, so without an explicit clear an ad-hoc pane carries the server's
    FNO_NODE and a plan-less child carries its FNO_PLAN - which ambient origin
    capture then persists into every node that pane files.
    """
    from fno.agents import mux_spawn

    adhoc = mux_spawn._mesh_env_wrapper("w", "claude", None, ["claude"])
    for key in mux_spawn.PROVENANCE_KEYS:
        assert ["-u", key] == adhoc[adhoc.index(key) - 1 : adhoc.index(key) + 1]

    # A resolved key is set, not cleared; the unresolved rest are still cleared.
    bound = mux_spawn._mesh_env_wrapper(
        "w", "claude", None, ["claude"], provenance={"FNO_NODE": "x-aaaa"}
    )
    assert "FNO_NODE=x-aaaa" in bound
    assert "-u" in bound and "FNO_PLAN" in bound
    assert "FNO_NODE" not in bound[: bound.index("FNO_NODE=x-aaaa")]
