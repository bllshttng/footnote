"""Task 1.2: spawn verb with --once / -o ephemeral one-shot lifecycle.

Acceptance criteria (operator-locked):

  AC2-HP: codex --once creates+exchanges, stdout=reply, exit 0, teardown receipt on
          stderr, registry has NO row afterward.
  AC2-ERR: provider create fails -> stderr has error, nonzero exit, registry empty.
  AC2-UI: stderr receipt identifies peer (name, provider, session_or_short_id).
  AC2-EDGE: pre-seeded name -> collision refuse exit 2, row untouched.
  AC2-FR: teardown fails after successful exchange -> stderr warning names peer +
          fno agents rm hint, exit 0, row still present.
  claude plain spawn: JSON receipt on stdout exact-match, registry row present with
          provider=claude.
  claude --once selects the ephemeral headless substrate.
  codex plain-spawn (no --once) refusal exit 13 (Python fallback, PTY daemon needed).
  CLI wiring: fno agents spawn registered; RUST_ONLY_VERB_HELP no longer lists spawn;
          help-parity test passes (implicitly via test_rust_only_verb_help_covers_unregistered_verbs).
"""
from __future__ import annotations

import json
import re
from pathlib import Path
from unittest.mock import MagicMock

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir
from fno.agents.harnesses import codex as codex_mod
from fno.agents.harnesses.codex import (
    CodexInvocationError,
    CodexResult,
)
from fno.agents.registry import (
    AgentEntry,
    load_registry,
    write_registry,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------



def _receipt_line(output: str) -> str:
    """The line that IS the JSON receipt. CliRunner mixes stderr into output,
    so a stderr notice (the inherited tier-remap drop warning) can precede or
    follow the receipt on machines carrying ANTHROPIC_DEFAULT_*_MODEL."""
    for line in output.splitlines():
        line = line.strip()
        if line.startswith("{"):
            return line
    raise AssertionError(f"no JSON receipt line in output: {output!r}")


def _make_runner() -> CliRunner:
    return CliRunner()


def _read_events(tmp_path: Path) -> list[dict]:
    from fno import paths
    events_log = paths.state_dir() / "events.jsonl"
    if not events_log.exists():
        return []
    return [
        json.loads(line)
        for line in events_log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _write_existing_entry(name: str, provider: str, session_id: str) -> None:
    """Seed the registry with one entry for collision tests."""
    write_registry([
        AgentEntry(
            name=name,
            harness=provider,
            cwd="/tmp",
            log_path="/tmp/a.log",
            harness_session_id=session_id if provider == "codex" else None,
            short_id=session_id if provider == "claude" else "",
        )
    ])


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def workdir(tmp_path, monkeypatch):
    """Isolated fno home with codex marked available on PATH."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text("#!/bin/sh\necho fake\n", encoding="utf-8")
    (bin_dir / "codex").chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


@pytest.fixture
def workdir_claude(tmp_path, monkeypatch):
    """Isolated fno home with claude marked available on PATH."""
    from tests.agents._fake_claude import install_fake_claude
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


@pytest.fixture
def fake_codex_create_once(monkeypatch):
    """Replace codex_mod.create with a mock returning a successful CodexResult."""
    mock = MagicMock(return_value=CodexResult(
        exit_code=0,
        session_id="codex-once-sid",
        last_msg="hello from codex",
        duration_ms=42,
    ))
    monkeypatch.setattr(codex_mod, "create", mock)
    return mock


# ---------------------------------------------------------------------------
# AC2-HP: codex --once happy path
# ---------------------------------------------------------------------------


def test_spawn_once_codex_happy_path(workdir, fake_codex_create_once, monkeypatch) -> None:
    """AC2-HP: codex --once creates+exchanges, reply on stdout, teardown on stderr,
    no registry row afterward, exit 0."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "tmp1", "-H", "codex", "--once", "summarize X"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"expected exit 0, got {result.exit_code}\noutput: {result.output}"
    # stdout+stderr combined is result.output in Typer CliRunner
    assert "hello from codex" in result.output
    # Teardown receipt in output
    assert "tmp1" in result.output
    assert "torn down" in result.output or "teardown" in result.output.lower()
    # Registry must have NO row for tmp1 after teardown
    entries = load_registry()
    assert not any(e.name == "tmp1" for e in entries), (
        f"Expected no tmp1 row after --once teardown, got: {entries}"
    )


def test_spawn_once_codex_normalizes_direct_plugin_command(
    workdir, fake_codex_create_once
) -> None:
    """The Python headless fallback is a direct-spawn choke point too."""
    from fno.agents.cli import agents_app

    result = _make_runner().invoke(
        agents_app,
        ["spawn", "--name", "tmp-skill", "-H", "codex", "--once", "/fno:target x-81ad"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    prompt = fake_codex_create_once.call_args.kwargs["prompt"]
    assert prompt.startswith("$fno:target x-81ad\n\n")
    assert prompt.count("<fno_relay_compression>") == 1


# ---------------------------------------------------------------------------
# AC2-ERR: provider create fails -> no registry entry, nonzero exit
# ---------------------------------------------------------------------------


def test_spawn_once_create_failure_no_registry_entry(workdir, monkeypatch) -> None:
    """AC2-ERR: codex create fails -> stderr has error, nonzero exit, no registry row."""
    monkeypatch.setattr(
        codex_mod, "create",
        MagicMock(side_effect=CodexInvocationError(1))
    )
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "tmp2", "-H", "codex", "--once", "hello"],
    )

    assert result.exit_code != 0, (
        f"expected nonzero exit on create failure, got {result.exit_code}\noutput: {result.output}"
    )
    entries = load_registry()
    assert not any(e.name == "tmp2" for e in entries), (
        "No registry row should exist after failed create"
    )


# ---------------------------------------------------------------------------
# AC2-UI: stderr receipt format (name, provider, session_or_short_id)
# ---------------------------------------------------------------------------


def test_spawn_once_receipt_format(workdir, fake_codex_create_once) -> None:
    """AC2-UI: teardown receipt on stderr identifies peer: name + provider/id."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "myagent", "-H", "codex", "--once", "do something"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0
    # Receipt (in combined output) must contain name and provider
    assert "myagent" in result.output
    assert "codex" in result.output


# ---------------------------------------------------------------------------
# AC2-EDGE: name collision refuses, existing row untouched
# ---------------------------------------------------------------------------


def test_spawn_collision_refuses(workdir) -> None:
    """AC2-EDGE: pre-seeded registry entry -> spawn refuses exit 2, row untouched."""
    _write_existing_entry("existing-agent", "codex", "oldses-123")

    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "existing-agent", "-H", "codex", "--once", "hello"],
    )

    assert result.exit_code == 2, (
        f"expected exit 2 for name collision, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    # Message must mention the name and hint
    assert "existing-agent" in result.output
    assert "rm" in result.output
    # Row must remain intact
    entries = load_registry()
    existing = next((e for e in entries if e.name == "existing-agent"), None)
    assert existing is not None, "existing row must not be deleted on collision"
    assert existing.harness_session_id == "oldses-123"


# ---------------------------------------------------------------------------
# AC2-FR: teardown failure after successful exchange
# ---------------------------------------------------------------------------


def test_spawn_once_teardown_failure(workdir, fake_codex_create_once, monkeypatch) -> None:
    """AC2-FR: teardown fails after successful exchange -> stderr warning, exit 0,
    row still present."""
    from fno.agents.cli import agents_app
    from fno.agents import dispatch as dispatch_mod

    # Monkeypatch update_registry to fail during teardown but succeed during create.
    original_update = dispatch_mod.update_registry
    call_count = [0]

    def _patched_update_registry(updater):
        call_count[0] += 1
        if call_count[0] > 1:
            # Second call is the teardown removal - make it fail
            raise OSError("simulated teardown failure")
        return original_update(updater)

    monkeypatch.setattr(dispatch_mod, "update_registry", _patched_update_registry)

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "teardown-victim", "-H", "codex", "--once", "hello"],
        catch_exceptions=False,
    )

    # Exchange succeeded, so exit 0 even though teardown failed
    assert result.exit_code == 0, (
        f"expected exit 0 (exchange succeeded), got {result.exit_code}\n"
        f"output: {result.output}"
    )
    # Output must warn about the leaked peer and hint at rm
    assert "teardown-victim" in result.output
    assert "rm" in result.output
    # Row must still be present (teardown didn't clean it)
    entries = load_registry()
    assert any(e.name == "teardown-victim" for e in entries), (
        "Row must remain visible after failed teardown (AC2-FR)"
    )


# ---------------------------------------------------------------------------
# claude plain spawn: compact JSON receipt, registry row present
# ---------------------------------------------------------------------------


def test_spawn_claude_plain(workdir_claude) -> None:
    """claude bg-substrate spawn: compact JSON receipt on stdout (4a-G2: the
    plain/pane default is mux-hosted; the bg thread lane keeps this shape)."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "myagent-c", "-H", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, (
        f"expected exit 0, got {result.exit_code}\noutput: {result.output}"
    )
    # output is stdout+stderr combined in Typer CliRunner; the JSON receipt
    # is the FIRST line (on stdout), before any stderr teardown notes.
    first_line = result.output.split("\n")[0].strip()
    receipt = json.loads(first_line)
    assert receipt["name"] == "myagent-c"
    assert receipt["harness"] == "claude"
    # AC5: no -P on this spawn -> provider (vendor) and model are absent, not
    # defaulted to the harness. A provider key holding "claude" is the defect.
    assert "provider" not in receipt
    assert "model" not in receipt
    assert receipt["status"] == "live"
    assert "short_id" in receipt
    # jq .short_id must work (i.e. it's a plain string value)
    assert isinstance(receipt["short_id"], str)
    assert len(receipt["short_id"]) == 8

    # Verify the exact format: hand-rolled, keys in order name/short_id/harness/status.
    # No -P -> provider/model absent (AC5); harness carries the harness literal.
    assert first_line == (
        f'{{"name": "{receipt["name"]}", "short_id": "{receipt["short_id"]}", '
        f'"harness": "claude", "status": "live"}}'
    )

    # Registry row must be present
    entries = load_registry()
    entry = next((e for e in entries if e.name == "myagent-c"), None)
    assert entry is not None, "registry row must exist after claude spawn"
    assert entry.harness == "claude"
    assert entry.short_id == receipt["short_id"]


def test_spawn_claude_command_receipt_names_effective_message(workdir_claude) -> None:
    """A healthy receipt must reveal whether a skill payload stayed dispatched."""
    from fno.agents.cli import agents_app

    result = _make_runner().invoke(
        agents_app,
        [
            "spawn", "--name", "command-c", "-H", "claude",
            "/fno:pr check 7", "--substrate", "bg",
        ],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    receipt = json.loads(_receipt_line(result.output))
    assert receipt["effective_message"] == "/fno:pr check 7"


def test_spawn_claude_receipt_surfaces_moved_cwd(workdir_claude, monkeypatch) -> None:
    """x-85fe: when the default moves the worker off the caller (canonical !=
    caller), the bg receipt appends the effective cwd LAST, and the stderr
    redirect note fires (AC1-HP / AC1-UI). The unmoved receipt stays byte-
    identical (proven by test_spawn_claude_plain)."""
    from fno.agents.cli import agents_app

    canon = workdir_claude / "canon"
    canon.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(canon))

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "moved-c", "-H", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    first_line = result.output.split("\n")[0].strip()
    receipt = json.loads(first_line)
    assert receipt["cwd"] == str(canon.resolve())
    # cwd is the LAST key (byte-parity contract with Rust claude_ask).
    assert first_line.rstrip("}").rstrip().endswith(f'"cwd": "{canon.resolve()}"')
    assert "dispatching from canonical main" in result.output


def test_spawn_claude_receipt_cwd_json_encoded(workdir_claude, monkeypatch) -> None:
    """x-85fe (codex #4): a canonical path with a backslash must stay valid JSON
    in the receipt. A bare `"`-escape would emit `\\n`-style sequences that
    json.loads mis-decodes or rejects; json.dumps keeps it parseable and matches
    the Rust json_string_ascii twin."""
    from fno.agents.cli import agents_app

    canon = workdir_claude / "ca\\non"  # a dir literally named ca\non
    canon.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(canon))

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "bs-c", "-H", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(_receipt_line(result.output))
    assert receipt["cwd"] == str(canon.resolve())


# ---------------------------------------------------------------------------
# claude --once: ephemeral headless spawn
# ---------------------------------------------------------------------------


def test_spawn_claude_once_uses_headless(workdir_claude) -> None:
    """claude --once is the legacy spelling for an ephemeral headless spawn."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "cagent", "-H", "claude", "--once", "hello"],
    )

    assert result.exit_code == 0, result.output
    receipt = json.loads(_receipt_line(result.output))
    assert receipt["substrate"] == "headless"
    assert receipt["lifecycle"] == "ephemeral"
    assert all(entry.name != "cagent" for entry in load_registry())


# ---------------------------------------------------------------------------
# codex plain spawn (no --once): Rust runtime is required
# ---------------------------------------------------------------------------


def test_spawn_codex_plain_no_once_requires_runtime(workdir, monkeypatch) -> None:
    """Codex thread spawn reports the missing Rust runtime distinctly."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "ptagent", "-H", "codex", "hello", "--substrate", "bg"],
    )

    assert result.exit_code == 13, (
        f"expected exit 13 for plain codex spawn in Python fallback, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    assert "fno-agents runtime" in result.output


def test_spawn_unknown_provider_exits_2(workdir, monkeypatch, tmp_path) -> None:
    """Unknown --harness on the default pane substrate -> clean exit 2 (not a
    ValueError traceback).

    x-f579: the pane lane is no longer gated on an allowlist. 'foo' is a
    well-shaped undeclared name, so the refusal is the PATH one - it names the
    harness and PATH, before any pane exists. The CliRunner's environment is
    host-dependent, so PATH is pinned to an empty dir.
    """
    from fno.agents.cli import agents_app

    empty_bin = tmp_path / "empty-bin"
    empty_bin.mkdir()
    monkeypatch.setenv("PATH", str(empty_bin))
    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "fooagent", "--harness", "foo", "hello"],
    )

    assert result.exit_code == 2, (
        f"expected exit 2 for unknown provider, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    assert "foo" in result.output
    assert "PATH" in result.output


def test_spawn_thread_refusal_names_axis_and_actual_accept_set(workdir) -> None:
    from fno.agents.cli import agents_app

    result = _make_runner().invoke(
        agents_app,
        [
            "spawn", "--name", "fooagent", "--harness", "zai",
            "--substrate", "thread", "hello",
        ],
    )

    from fno.harness_names import SPAWN_HARNESSES

    assert result.exit_code == 2
    # 'zai' is a VENDOR name with no capability row, so the undeclared arm
    # answers first: it names the harness, says pane is the only substrate, and
    # keeps the vendor-axis hint (the reason this exact input is interesting).
    # It must NOT name gemini's retirement or agy as a successor - a harness
    # the operator never mentioned (x-f579 AC5-ERR).
    assert "zai" in result.output
    assert "no capability row" in result.output
    assert "--substrate pane" in result.output
    assert "If you meant a model VENDOR, that is -P/--provider." in result.output
    assert "gemini" not in result.output
    assert "agy" not in result.output

    # A DECLARED pane-only harness keeps the accept-set message (x-8f7f).
    # agy joined SPAWN_HARNESSES in x-d145, so the declared pane-only name
    # asserted here is gemini: it carries a capability row (so it passes the
    # undeclared arm above) and stays out of the tuple.
    declared = _make_runner().invoke(
        agents_app,
        [
            "spawn", "--name", "fooagent2", "--harness", "gemini",
            "--substrate", "thread", "hello",
        ],
    )
    assert declared.exit_code == 2
    assert (
        "unknown harness 'gemini' on the thread substrate (--harness names the "
        f"CLI BINARY); accepted here: {', '.join(SPAWN_HARNESSES)}."
    ) in declared.output
    # The pane sentence derives from the tuple now, so it names the refused
    # harness and can never contradict the accept list beside it.
    assert "gemini launches on --substrate pane only." in declared.output


def test_spawn_thread_refusal_renders_from_accept_set(monkeypatch) -> None:
    from fno.agents import dispatch

    from fno import harness_names

    dispatch._check_spawn_harness("opencode")
    # Patch the ONE tuple the message builder reads. dispatch's own import is a
    # second binding of the same names, and patching it would prove nothing
    # about where the rendered text comes from.
    monkeypatch.setattr(
        harness_names, "SPAWN_HARNESSES", (*harness_names.SPAWN_HARNESSES, "future")
    )

    with pytest.raises(dispatch.DispatchAskError) as caught:
        # A DECLARED harness outside SPAWN_HARNESSES renders the accept set;
        # an undeclared name takes the undeclared arm instead, so the set is
        # asserted through a declared pane-only harness (gemini; pi joined the
        # set in x-43bd and agy in x-d145).
        dispatch._check_spawn_harness("gemini")

    expected = ", ".join(harness_names.SPAWN_HARNESSES)
    assert f"accepted here: {expected}" in str(caught.value)
    assert expected.endswith("future"), "the monkeypatched name must be rendered"
    # The pane sentence derives too: it names the refused harness, never a
    # hardcoded roster that the tuple can outgrow.
    assert "gemini launches on --substrate pane only." in str(caught.value)


def test_thread_refusal_is_one_message_across_both_seams(monkeypatch) -> None:
    """The spawn seam and the dispatch-lanes seam raise the SAME text.

    Two files used to render the accept set from the tuple and then hardcode
    the same pane-only sentence beside it, so one could name a harness the
    other had since admitted - and did.
    """
    from typer.testing import CliRunner

    from fno.agents import dispatch
    from fno.backlog import advance
    from fno.graph import cli as graph_cli

    with pytest.raises(dispatch.DispatchAskError) as caught:
        dispatch._check_spawn_harness("gemini")

    monkeypatch.setattr(advance, "dispatch_lanes", lambda *a, **k: [])
    lanes = CliRunner().invoke(
        graph_cli.cli, ["dispatch-lanes", "--harness", "gemini", "--max", "1"]
    )

    assert lanes.exit_code == 2
    for line in str(caught.value).splitlines():
        assert line in lanes.output


def test_spawn_pi_thread_passes_the_seam_and_headless_refuses_unmeasured() -> None:
    """x-43bd arm 1: the seam is substrate-aware. pi passes on thread (its
    keeper lane is journey-proven) and refuses on headless, naming the
    `unmeasured` stance - not the old `unknown harness` contradiction."""
    from fno.agents import dispatch

    # Positive marker: the accepted name RETURNS rather than raising.
    dispatch._check_spawn_harness("pi", headless=False)
    dispatch._check_spawn_harness("pi")

    with pytest.raises(dispatch.DispatchAskError) as caught:
        dispatch._check_spawn_harness("pi", headless=True)
    message = str(caught.value)
    assert "unmeasured" in message
    assert "headless" in message
    assert "unattended journey" in message
    assert "unknown harness" not in message


def test_spawn_pi_thread_branch_drives_the_keeper_lane(workdir, monkeypatch) -> None:
    """`dispatch_spawn -H pi --substrate thread` reaches `_lane_b_thread_spawn`
    and returns its session id - the refusal text `unknown harness 'pi' on the
    thread substrate` never renders (x-43bd AC). The seed rides the keeper
    paste, keyed to the MINTED id and pi's own composer-ready marker."""
    from fno.agents import dispatch

    calls: list[dict] = []
    seeds: list[dict] = []

    def _fake_lane_b(*, name, harness, cwd, lock_timeout):
        calls.append({"name": name, "harness": harness, "cwd": cwd})
        return {
            "name": name,
            "harness": harness,
            "session_id": "minted-pi-thread-id",
            "keeper_socket": "/tmp/does-not-matter.sock",
            "keeper_pid": 1,
            "child_pid": 2,
            "argv": ["pi"],
        }

    def _fake_seed(*, name, session_id, sock, message, ready_marker):
        seeds.append(
            {
                "name": name,
                "session_id": session_id,
                "sock": str(sock),
                "message": message,
                "ready_marker": ready_marker,
            }
        )

    monkeypatch.setattr(dispatch, "_lane_b_thread_spawn", _fake_lane_b)
    monkeypatch.setattr(dispatch, "_keeper_seed_submit", _fake_seed)

    result = dispatch.dispatch_spawn(
        name="wkpi",
        message="hello",
        harness="pi",
        cwd=workdir,
    )

    assert result.kind == "created"
    assert result.provider == "pi"
    assert result.short_id == "minted-pi-thread-id"
    assert calls == [
        {"name": "wkpi", "harness": "pi", "cwd": workdir}
    ], "the pi branch must drive the keeper lane exactly once"
    # One seed, keyed to the minted id, pasted against pi's own ready marker.
    # The message arrives with the ambient relay-compression envelope appended,
    # so it is asserted by its start, never by equality.
    assert len(seeds) == 1, f"exactly one seed paste, got {seeds!r}"
    seed = seeds[0]
    assert seed["name"] == "wkpi"
    assert seed["session_id"] == "minted-pi-thread-id"
    assert seed["sock"] == "/tmp/does-not-matter.sock"
    assert seed["message"].startswith("hello")
    assert seed["ready_marker"] == b"(sub)"


def test_spawn_agy_thread_passes_the_seam_and_headless_refuses_unmeasured() -> None:
    """agy passes on thread (its keeper lane is journey-proven) and refuses on
    headless. The refusal cannot come from the seam here: agy's
    state_root_grant records the write-access MECHANISM per lane
    (--add-dir on all three), never whether a lane has been run, so the
    unmeasured-stance check passes for headless too. dispatch_spawn states it
    instead, and the message must name the lane rather than call agy
    pane-only."""
    from fno.agents import dispatch

    # Positive marker: the accepted name RETURNS rather than raising.
    dispatch._check_spawn_harness("agy", headless=False)
    dispatch._check_spawn_harness("agy")


def test_spawn_agy_headless_refuses_by_name(workdir, monkeypatch) -> None:
    from fno.agents import dispatch

    monkeypatch.setattr(
        dispatch,
        "_lane_b_thread_spawn",
        lambda **kw: pytest.fail("headless must never reach the keeper lane"),
    )
    with pytest.raises(dispatch.DispatchAskError) as caught:
        dispatch.dispatch_spawn(
            name="wkagyh", message="hello", harness="agy", cwd=workdir, headless=True
        )
    message = str(caught.value)
    assert "unmeasured" in message
    assert "--substrate thread" in message
    assert "pane only" not in message, (
        "the merged measurement disproved that sentence for agy"
    )


def test_spawn_agy_thread_branch_drives_the_keeper_lane(workdir, monkeypatch) -> None:
    """`dispatch_spawn -H agy --substrate thread` reaches
    `_lane_b_thread_spawn` and returns its minted conversation id - the
    refusal text `unknown harness 'agy' on the thread substrate` never
    renders. The seed rides the keeper paste, keyed to that id and agy's own
    composer-ready marker."""
    from fno.agents import dispatch

    calls: list[dict] = []
    seeds: list[dict] = []

    def _fake_lane_b(**kwargs):
        calls.append(kwargs)
        return {
            "name": kwargs["name"],
            "harness": kwargs["harness"],
            "session_id": "c5661b28-bcba-4690-8b2e-4a4a88541e8c",
            "keeper_socket": "/tmp/does-not-matter.sock",
            "keeper_pid": 1,
            "child_pid": 2,
            "argv": ["agy"],
        }

    def _fake_seed(*, name, session_id, sock, message, ready_marker, clear_modal):
        seeds.append(
            {
                "session_id": session_id,
                "message": message,
                "marker": ready_marker,
                "modal": clear_modal,
            }
        )

    monkeypatch.setattr(dispatch, "_lane_b_thread_spawn", _fake_lane_b)
    monkeypatch.setattr(dispatch, "_keeper_seed_submit", _fake_seed)

    result = dispatch.dispatch_spawn(
        name="wkagy", message="hello", harness="agy", cwd=workdir, model="gemini-3-pro"
    )

    assert result.kind == "created"
    assert result.provider == "agy"
    assert result.short_id == "c5661b28-bcba-4690-8b2e-4a4a88541e8c"
    assert len(calls) == 1, "the agy branch must drive the keeper lane exactly once"
    assert calls[0]["harness"] == "agy"
    assert calls[0]["model"] == "gemini-3-pro", "the model axis rides the lane"
    assert len(seeds) == 1, f"exactly one seed paste, got {seeds!r}"
    assert seeds[0]["session_id"] == "c5661b28-bcba-4690-8b2e-4a4a88541e8c"
    assert seeds[0]["message"].startswith("hello")
    assert seeds[0]["marker"] == b"? for shortcuts"
    # A keeper has nobody to answer agy's folder-trust modal, and a TUI behind
    # one runs nothing while holding a live row, so the seed carries the answer.
    assert seeds[0]["modal"] is not None
    pattern, keys = seeds[0]["modal"]
    assert re.search(pattern, "Do you trust the contents of this project?", re.I)
    assert keys == b"\r"


def test_spawn_seam_refuses_an_absent_stance(monkeypatch) -> None:
    """A future SPAWN_HARNESSES member whose row records no stance for the
    requested substrate is refused beside the \"unmeasured\" one: silence
    would let it inherit a pass from the lanes that did run."""
    import fno.agents.harness_map as harness_map
    from fno.agents import dispatch

    monkeypatch.setattr(
        dispatch,
        "SPAWN_HARNESSES",
        (*dispatch.SPAWN_HARNESSES, "future"),
    )
    real_caps = harness_map.capabilities_or_undeclared

    def caps_without_stance(harness):
        caps = dict(real_caps("claude"))
        caps["state_root_grant"] = {"pane": "unsandboxed", "thread": "unsandboxed"}
        return caps

    monkeypatch.setattr(
        harness_map, "capabilities_or_undeclared", caps_without_stance
    )
    dispatch._check_spawn_harness("future", headless=False)
    with pytest.raises(dispatch.DispatchAskError) as caught:
        dispatch._check_spawn_harness("future", headless=True)
    message = str(caught.value)
    assert "absent or unmeasured" in message
    assert "headless" in message


def test_route_on_an_undeclared_harness_refuses_cleanly(
    workdir, monkeypatch, tmp_path
) -> None:
    """Round-1 review finding 1: `--route` on the pane substrate read
    capabilities() at the route_on_pane guard, which RAISED an uncaught
    DispatchResolveError (exit 1 traceback) for an undeclared harness. The
    posture answers route_on_pane=False, so the refusal is the clean exit-2
    one the guard was written to print."""
    from fno.agents.cli import agents_app

    empty_bin = tmp_path / "empty-bin"
    empty_bin.mkdir()
    monkeypatch.setenv("PATH", str(empty_bin))
    result = _make_runner().invoke(
        agents_app,
        ["spawn", "--name", "rt", "-H", "nanoclaw", "--route", "zai,glm-5.3", "hi"],
    )
    assert result.exit_code == 2, (
        f"expected the clean route_on_pane refusal, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    assert "route_on_pane" in result.output
    assert "DispatchResolveError" not in result.output


def test_undeclared_harness_thread_and_headless_refuse_pane_only() -> None:
    """AC5-ERR: thread AND headless both refuse an undeclared harness, naming
    it, saying it declares no capability row, and naming pane as its only
    substrate - never gemini's retirement, never agy the successor."""
    from fno.agents import dispatch

    for kwargs in ({}, {"once": True}):
        with pytest.raises(dispatch.DispatchAskError) as caught:
            dispatch.dispatch_spawn(
                name="peer",
                message="hi",
                harness="nanoclaw",
                cwd="/tmp",
                **kwargs,
            )
        msg = str(caught.value)
        assert caught.value.exit_code == 2
        assert "nanoclaw" in msg
        assert "no capability row" in msg
        assert "pane" in msg
        assert "only substrate" in msg
        assert "gemini" not in msg
        assert "agy" not in msg


# ---------------------------------------------------------------------------
# CLI wiring: spawn verb is registered
# ---------------------------------------------------------------------------


def test_spawn_verb_registered() -> None:
    """fno agents spawn is a registered command (Python-implemented)."""
    from fno.agents.cli import agents_app

    registered = {cmd.name for cmd in agents_app.registered_commands}
    assert "spawn" in registered, (
        f"'spawn' must be registered as a Python command. Got: {sorted(registered)}"
    )


# ---------------------------------------------------------------------------
# RUST_ONLY_VERB_HELP no longer lists spawn
# ---------------------------------------------------------------------------


def test_spawn_not_in_rust_only_verb_help() -> None:
    """spawn is Python-registered, so it must NOT appear in RUST_ONLY_VERB_HELP."""
    from fno.agents import rust_runtime as rr

    assert "spawn" not in rr.RUST_ONLY_VERB_HELP, (
        "spawn has a Python implementation; it must not be in RUST_ONLY_VERB_HELP"
    )


# ---------------------------------------------------------------------------
# opencode bg: delegation to the Rust serve lane
# ---------------------------------------------------------------------------


def test_spawn_opencode_bg_delegates_to_serve_lane(workdir, monkeypatch) -> None:
    """A node-bearing opencode bg spawn reaches the serve lane.

    --node forces spawn onto the Python parser (x-84a8: the Rust client cannot
    parse provenance flags), and before this arm the provider fell through to
    the retired-gemini refusal. The helper is stubbed; the real lane is proven
    live by the Rust journey test."""
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.cli import agents_app

    calls: list[dict] = []

    def _fake_serve(**kwargs):
        calls.append(kwargs)
        return "ses_deleg1"

    monkeypatch.setattr(dispatch_mod, "_opencode_serve_spawn", _fake_serve)

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        [
            "spawn",
            "--name",
            "wkoc",
            "-H",
            "opencode",
            "--substrate",
            "bg",
            "--node",
            "x-abcd",
            "--cwd",
            str(workdir),
            "run the node",
        ],
    )

    assert result.exit_code == 0, (
        f"expected exit 0, got {result.exit_code}\noutput: {result.output}"
    )
    assert "retired" not in result.output
    assert calls, "serve delegation never ran"
    assert calls[0]["name"] == "wkoc"
    assert "run the node" in calls[0]["message"]
    receipt = json.loads(_receipt_line(result.output))
    assert receipt["short_id"] == "ses_deleg1"
    assert receipt["harness"] == "opencode"


def test_spawn_opencode_bg_once_refused(workdir) -> None:
    """The serve lane is the bg substrate; a one-shot is refused, not fallen
    through to the retired-gemini text."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        [
            "spawn",
            "--name",
            "wkoc",
            "-H",
            "opencode",
            "--substrate",
            "bg",
            "--once",
            "--cwd",
            str(workdir),
            "hello",
        ],
    )

    assert result.exit_code == 2, (
        f"expected exit 2, got {result.exit_code}\noutput: {result.output}"
    )
    assert "headless" in result.output


def test_spawn_opencode_bg_role_refused(workdir) -> None:
    """--role has no carrier on the serve row, so it is refused loudly rather
    than silently dropped."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        [
            "spawn",
            "--name",
            "wkoc",
            "-H",
            "opencode",
            "--substrate",
            "bg",
            "--role",
            "archer",
            "--cwd",
            str(workdir),
            "hello",
        ],
    )

    assert result.exit_code == 2, (
        f"expected exit 2, got {result.exit_code}\noutput: {result.output}"
    )
    assert "--role" in result.output


def test_spawn_opencode_bg_resume_refused(workdir) -> None:
    """Resume over the serve API is a filed follow-up; the spawn refuses
    instead of pretending to resume."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        [
            "spawn",
            "--name",
            "wkoc",
            "-H",
            "opencode",
            "--substrate",
            "bg",
            "--resume",
            "ses_old123",
            "--cwd",
            str(workdir),
            "hello",
        ],
    )

    assert result.exit_code == 2, (
        f"expected exit 2, got {result.exit_code}\noutput: {result.output}"
    )
    assert "--resume" in result.output


def test_opencode_serve_spawn_without_binary_exits_13(workdir, monkeypatch) -> None:
    """No fno-agents runtime on the box -> the same exit-13 shape as codex
    plain spawn, naming the pane escape."""
    from fno import rust_binary
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskError, _opencode_serve_spawn

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: None)
    with pytest.raises(DispatchAskError) as exc_info:
        _opencode_serve_spawn(
            name="wkoc",
            message="hello",
            cwd=workdir,
            from_name="",
            model=None,
        )
    assert exc_info.value.exit_code == 13
    assert "--substrate pane" in str(exc_info.value)
    assert dispatch_mod._opencode_serve_spawn is _opencode_serve_spawn
