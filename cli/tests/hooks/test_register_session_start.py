"""Shell-level checks for the SessionStart registration hook."""
from __future__ import annotations

import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / "hooks" / "register-session-start.sh"
SHARED_HOOK = ROOT / "hooks" / "session-start.sh"


def _mock_fno_auto_register(bin_dir: Path) -> None:
    """A mock `fno` on PATH that answers the hook's one config read
    (`config get agents.auto_register_sessions`) with `true`, so the opt-in
    auto-register gate proceeds to the registration these tests exercise."""
    fno = bin_dir / "fno"
    fno.write_text(
        '#!/usr/bin/env bash\n[[ "$1" == "config" && "$2" == "get" ]] && echo true\nexit 0\n',
        encoding="utf-8",
    )
    fno.chmod(0o755)


def test_codex_disagreeing_ids_register_no_row(tmp_path: Path) -> None:
    """The resolvers degrade a same-family id disagreement to unresolved; a row
    registered under the table-first id is one this session can never resolve
    against, so the hook registers nothing instead."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "uv-argv"
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$UV_CAPTURE\"\n",
        encoding="utf-8",
    )
    uv.chmod(0o755)
    _mock_fno_auto_register(bin_dir)

    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
        # A hand-built env inherits no conftest pin, so it declares nothing.
        # HOME is already a sandbox here; say so, or the SessionStart chain
        # writes its stranded cache into the real checkout .fno.
        "FNO_TEST_HERMETIC": "1",
        "CLAUDE_PROJECT_DIR": str(tmp_path),
        "CODEX_PLUGIN_ROOT": str(ROOT),
        "CODEX_THREAD_ID": "thread-wins",
        "CODEX_SESSION_ID": "legacy-loses",
        "UV_CAPTURE": str(capture),
    }
    subprocess.run(["bash", str(HOOK)], check=True, env=env)

    assert not capture.exists(), "a disagreed id family must not register a row"


def test_codex_same_value_dup_registers_once(tmp_path: Path) -> None:
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "uv-argv"
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$UV_CAPTURE\"\n",
        encoding="utf-8",
    )
    uv.chmod(0o755)
    _mock_fno_auto_register(bin_dir)

    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
        # A hand-built env inherits no conftest pin, so it declares nothing.
        # HOME is already a sandbox here; say so, or the SessionStart chain
        # writes its stranded cache into the real checkout .fno.
        "FNO_TEST_HERMETIC": "1",
        "CLAUDE_PROJECT_DIR": str(tmp_path),
        "CODEX_PLUGIN_ROOT": str(ROOT),
        "CODEX_THREAD_ID": "same-id",
        "CODEX_SESSION_ID": "same-id",
        "UV_CAPTURE": str(capture),
    }
    subprocess.run(["bash", str(HOOK)], check=True, env=env)

    argv = capture.read_text(encoding="utf-8").splitlines()
    assert argv[argv.index("--harness") + 1] == "codex"
    assert argv[argv.index("--session-id") + 1] == "same-id"


def test_shared_codex_session_start_registers_thread_once(tmp_path: Path) -> None:
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "uv-argv"
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> \"$UV_CAPTURE\"\n",
        encoding="utf-8",
    )
    uv.chmod(0o755)
    _mock_fno_auto_register(bin_dir)

    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
        # A hand-built env inherits no conftest pin, so it declares nothing.
        # HOME is already a sandbox here; say so, or the SessionStart chain
        # writes its stranded cache into the real checkout .fno.
        "FNO_TEST_HERMETIC": "1",
        "FNO_PLATFORM": "codex",
        "CODEX_THREAD_ID": "shared-thread",
        "UV_CAPTURE": str(capture),
    }
    subprocess.run(
        ["bash", str(SHARED_HOOK)],
        check=True,
        cwd=tmp_path,
        env=env,
        input="{}",
        text=True,
    )

    argv = capture.read_text(encoding="utf-8").splitlines()
    assert argv.count("--harness") == 1
    assert argv[argv.index("--harness") + 1] == "codex"
    assert argv[argv.index("--session-id") + 1] == "shared-thread"


def test_shared_session_start_does_not_duplicate_claude_registration(
    tmp_path: Path,
) -> None:
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "uv-argv"
    # The observer's uv call is gone, so the capture may stay empty; it is
    # pre-touched so the absence asserts below read a file, not a missing one.
    capture.touch()
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> \"$UV_CAPTURE\"\n",
        encoding="utf-8",
    )
    uv.chmod(0o755)
    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
        # A hand-built env inherits no conftest pin, so it declares nothing.
        # HOME is already a sandbox here; say so, or the SessionStart chain
        # writes its stranded cache into the real checkout .fno.
        "FNO_TEST_HERMETIC": "1",
        "FNO_PLATFORM": "claude",
        "CLAUDE_PLUGIN_ROOT": str(ROOT),
        "CLAUDE_SESSION_ID": "claude-direct-hook-owns-registration",
        "UV_CAPTURE": str(capture),
    }

    subprocess.run(
        ["bash", str(SHARED_HOOK)],
        check=True,
        cwd=tmp_path,
        env=env,
        input="{}",
        text=True,
        stdout=subprocess.DEVNULL,
    )

    argv = capture.read_text(encoding="utf-8").splitlines()
    assert not any(item.endswith("context_observation.py") for item in argv)
    assert "agents" not in argv
    assert "register" not in argv


def test_spawned_worker_restamps_without_consulting_the_optin_knob(tmp_path: Path) -> None:
    """x-1e34: a footnote-spawned worker (FNO_AGENT_SELF) takes the restamp path.

    The daemon's session-report ingest holds the reported id on every lane;
    the bounded Python restamp supplements it on the pane lane only
    (FNO_AGENT_ROW_PENDING), the path that heals the row's mux ref and opens
    the parked pending graph row. Two things stay load-bearing. `--agent-self`
    must reach both the thin report and the entry point (registration keys on
    the re-mintable session id and would append a second row instead of
    correcting the first), and the auto_register_sessions knob must NOT be
    consulted -- it governs whether a hand-started terminal JOINS the roster,
    while a spawned worker is already on it and its row going stale is a
    defect at any knob setting.
    """
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "uv-argv"
    agents_capture = tmp_path / "agents-argv"
    knob_read = tmp_path / "knob-read"
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$UV_CAPTURE\"\n", encoding="utf-8"
    )
    uv.chmod(0o755)
    # The thin session-report verb rides fno-agents, resolved through
    # FNO_AGENTS_BIN; the mock records its argv so the report lane is visible.
    agents_bin = bin_dir / "fno-agents"
    agents_bin.write_text(
        '#!/usr/bin/env bash\nprintf \'%s\\n\' "$@" > "$AGENTS_CAPTURE"\n',
        encoding="utf-8",
    )
    agents_bin.chmod(0o755)
    # A mock `fno` that records any call and answers the knob with `false`: if
    # the restamp were gated on it, the hook would exit before reaching uv.
    fno = bin_dir / "fno"
    fno.write_text(
        '#!/usr/bin/env bash\ntouch "$KNOB_READ"\necho false\nexit 0\n', encoding="utf-8"
    )
    fno.chmod(0o755)

    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
        # A hand-built env inherits no conftest pin, so it declares nothing.
        # HOME is already a sandbox here; say so, or the SessionStart chain
        # writes its stranded cache into the real checkout .fno.
        "FNO_TEST_HERMETIC": "1",
        "CLAUDE_PROJECT_DIR": str(tmp_path),
        "CLAUDE_PLUGIN_ROOT": str(ROOT),
        "CLAUDE_CODE_SESSION_ID": "08054b1d-a907-47ab-a3d2-4a1e7a87eb4e",
        "FNO_AGENT_SELF": "target-x-f0c2",
        # Pane substrate: the only worker lane that still runs the Python
        # restamp beside the thin report.
        "FNO_AGENT_ROW_PENDING": "target-x-f0c2",
        "FNO_AGENTS_BIN": str(agents_bin),
        "AGENTS_CAPTURE": str(agents_capture),
        "UV_CAPTURE": str(capture),
        "KNOB_READ": str(knob_read),
    }
    subprocess.run(["bash", str(HOOK)], check=True, env=env)

    report = agents_capture.read_text(encoding="utf-8").splitlines()
    assert report[report.index("--agent-self") + 1] == "target-x-f0c2"
    argv = capture.read_text(encoding="utf-8").splitlines()
    assert argv[argv.index("--agent-self") + 1] == "target-x-f0c2"
    assert argv[argv.index("--harness") + 1] == "claude"
    assert (
        argv[argv.index("--session-id") + 1] == "08054b1d-a907-47ab-a3d2-4a1e7a87eb4e"
    )
    assert not knob_read.exists(), "spawned-worker restamp must not read the opt-in knob"


def test_hand_started_session_still_gated_on_the_optin_knob(tmp_path: Path) -> None:
    """The other half: without FNO_AGENT_SELF the knob still governs, and the
    call carries no --agent-self (there is no spawned row to correct)."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "uv-argv"
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$UV_CAPTURE\"\n", encoding="utf-8"
    )
    uv.chmod(0o755)
    fno = bin_dir / "fno"
    fno.write_text('#!/usr/bin/env bash\necho false\nexit 0\n', encoding="utf-8")
    fno.chmod(0o755)
    # The origin-only call rides fno-agents the same way the worker report
    # does; the mock records its argv so that lane is visible here too.
    agents_bin = bin_dir / "fno-agents"
    agents_bin.write_text(
        '#!/usr/bin/env bash\nprintf \'%s\\n\' "$@" > "$AGENTS_CAPTURE"\n',
        encoding="utf-8",
    )
    agents_bin.chmod(0o755)

    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
        # A hand-built env inherits no conftest pin, so it declares nothing.
        # HOME is already a sandbox here; say so, or the SessionStart chain
        # writes its stranded cache into the real checkout .fno.
        "FNO_TEST_HERMETIC": "1",
        "CLAUDE_PROJECT_DIR": str(tmp_path),
        "CLAUDE_PLUGIN_ROOT": str(ROOT),
        "CLAUDE_CODE_SESSION_ID": "0718619e-2527-4bba-9cc0-5e493313240c",
        "FNO_AGENTS_BIN": str(agents_bin),
        "AGENTS_CAPTURE": str(tmp_path / "agents-argv"),
        "UV_CAPTURE": str(capture),
    }
    subprocess.run(["bash", str(HOOK)], check=True, env=env)
    assert not capture.exists(), "knob off must still suppress hand-started auto-join"
    # The origin record lane runs regardless of the knob: the session-start
    # hook asks for the record write and nothing else (AC2-HOOK).
    origin_argv = env["AGENTS_CAPTURE"] and (tmp_path / "agents-argv").read_text(
        encoding="utf-8"
    ).splitlines()
    assert origin_argv[:2] == ["report", "--kind"]
    assert origin_argv[2] == "session" and origin_argv[3] == "--origin-only"
    assert origin_argv[origin_argv.index("--harness") + 1] == "claude"
    assert (
        origin_argv[origin_argv.index("--session-id") + 1]
        == "0718619e-2527-4bba-9cc0-5e493313240c"
    )

    _mock_fno_auto_register(bin_dir)
    subprocess.run(["bash", str(HOOK)], check=True, env=env)
    argv = capture.read_text(encoding="utf-8").splitlines()
    assert "--agent-self" not in argv
