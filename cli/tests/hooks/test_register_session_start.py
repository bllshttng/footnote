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


def test_codex_thread_id_wins_with_mocked_uv(tmp_path: Path) -> None:
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

    argv = capture.read_text(encoding="utf-8").splitlines()
    assert argv[argv.index("--provider") + 1] == "codex"
    assert argv[argv.index("--session-id") + 1] == "thread-wins"


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
    assert argv.count("--provider") == 1
    assert argv[argv.index("--provider") + 1] == "codex"
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

    assert not capture.exists()
