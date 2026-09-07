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


def test_register_session_start_shell_syntax() -> None:
    subprocess.run(["bash", "-n", str(HOOK)], check=True)


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
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> \"$UV_CAPTURE\"\n",
        encoding="utf-8",
    )
    uv.chmod(0o755)
    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
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
    assert any(item.endswith("context_observation.py") for item in argv)
    assert "agents" not in argv
    assert "register" not in argv


def test_spawned_worker_restamps_without_consulting_the_optin_knob(tmp_path: Path) -> None:
    """x-1e34: a footnote-spawned worker (FNO_AGENT_SELF) takes the restamp path.

    Two things are asserted because both are load-bearing. `--agent-self` must
    reach the entry point (registration keys on the re-mintable session id and
    would append a second row instead of correcting the first), and the
    auto_register_sessions knob must NOT be consulted -- it governs whether a
    hand-started terminal JOINS the roster, while a spawned worker is already on
    it and its row going stale is a defect at any knob setting.
    """
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "uv-argv"
    knob_read = tmp_path / "knob-read"
    uv = bin_dir / "uv"
    uv.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$UV_CAPTURE\"\n", encoding="utf-8"
    )
    uv.chmod(0o755)
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
        "CLAUDE_PROJECT_DIR": str(tmp_path),
        "CLAUDE_PLUGIN_ROOT": str(ROOT),
        "CLAUDE_CODE_SESSION_ID": "08054b1d-a907-47ab-a3d2-4a1e7a87eb4e",
        "FNO_AGENT_SELF": "target-x-f0c2",
        "UV_CAPTURE": str(capture),
        "KNOB_READ": str(knob_read),
    }
    subprocess.run(["bash", str(HOOK)], check=True, env=env)

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

    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin",
        "HOME": str(tmp_path),
        "CLAUDE_PROJECT_DIR": str(tmp_path),
        "CLAUDE_PLUGIN_ROOT": str(ROOT),
        "CLAUDE_CODE_SESSION_ID": "0718619e-2527-4bba-9cc0-5e493313240c",
        "UV_CAPTURE": str(capture),
    }
    subprocess.run(["bash", str(HOOK)], check=True, env=env)
    assert not capture.exists(), "knob off must still suppress hand-started auto-join"

    _mock_fno_auto_register(bin_dir)
    subprocess.run(["bash", str(HOOK)], check=True, env=env)
    argv = capture.read_text(encoding="utf-8").splitlines()
    assert "--agent-self" not in argv
