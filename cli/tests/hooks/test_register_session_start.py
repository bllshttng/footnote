"""Shell-level checks for the SessionStart registration hook."""
from __future__ import annotations

import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / "hooks" / "register-session-start.sh"


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

