"""Tests for fno.agents.providers.claude — TDD Red phase for Task 1.2.

ACs (US1):
- AC1-HP: bg_create returns ProviderResult with parsed short_id from stdout
- Locked Decision 6: regex ``^backgrounded · ([0-9a-f]{8}) · `` extracts id
- AC1-FR parse failure: ProviderParseError carries first 200 chars of stdout
- AC1-FR subprocess non-zero: ProviderSubprocessError preserves verbatim stderr
- AC1-EDGE argv overflow: ``len_argv > 200KB`` routes via subprocess stdin
"""
from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from tests.agents._fake_claude import configure_fake, install_fake_claude


# ---------------------------------------------------------------------------
# Symbol surface
# ---------------------------------------------------------------------------


def test_provider_module_exports() -> None:
    """claude.py exports bg_create + parse_short_id + error types."""
    from fno.agents.providers import claude as claude_mod

    assert hasattr(claude_mod, "bg_create")
    assert hasattr(claude_mod, "parse_short_id")
    assert hasattr(claude_mod, "ProviderParseError")
    assert hasattr(claude_mod, "ProviderSubprocessError")


# ---------------------------------------------------------------------------
# parse_short_id (unit-level regex contract)
# ---------------------------------------------------------------------------


def test_parse_short_id_extracts_8hex() -> None:
    """parse_short_id extracts the 8-hex id from the documented stdout shape."""
    from fno.agents.providers.claude import parse_short_id

    stdout = "backgrounded · 7c5dcf5d · frontend-worker\n"
    assert parse_short_id(stdout) == "7c5dcf5d"


def test_parse_short_id_only_first_line() -> None:
    """parse_short_id only consults stdout's first line (AC contract anchor)."""
    from fno.agents.providers.claude import parse_short_id

    stdout = "backgrounded · abcdef01 · worker\nsome trailing noise\n"
    assert parse_short_id(stdout) == "abcdef01"


def test_parse_short_id_rejects_uppercase_hex() -> None:
    """Locked Decision 6: short-id regex requires LOWERCASE 8-hex."""
    from fno.agents.providers.claude import ProviderParseError, parse_short_id

    stdout = "backgrounded · ABCDEF01 · worker\n"
    with pytest.raises(ProviderParseError):
        parse_short_id(stdout)


def test_parse_short_id_rejects_wrong_length() -> None:
    from fno.agents.providers.claude import ProviderParseError, parse_short_id

    for bad in (
        "backgrounded · 7c5dcf · worker\n",  # 6 hex
        "backgrounded · 7c5dcf5d1 · worker\n",  # 9 hex
        "Session created: foo-bar\n",  # no match
        "",  # empty
    ):
        with pytest.raises(ProviderParseError):
            parse_short_id(bad)


def test_provider_parse_error_carries_first_200_chars() -> None:
    """ProviderParseError exposes the raw first 200 chars of stdout."""
    from fno.agents.providers.claude import ProviderParseError, parse_short_id

    big = "garbage" * 100  # 700 chars
    try:
        parse_short_id(big)
    except ProviderParseError as exc:
        assert exc.stdout_head == big[:200]
        assert len(exc.stdout_head) == 200
    else:
        pytest.fail("expected ProviderParseError")


# ---------------------------------------------------------------------------
# bg_create — happy path with fake claude on PATH
# ---------------------------------------------------------------------------


def test_bg_create_happy_path(tmp_path: Path, monkeypatch) -> None:
    """bg_create invokes claude --bg, parses short_id, returns ProviderResult."""
    from fno.agents.providers.base import ProviderResult
    from fno.agents.providers.claude import bg_create

    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    result = bg_create(
        name="frontend-worker",
        message="implement Login.tsx",
        cwd=cwd,
        timeout=10,
    )

    assert isinstance(result, ProviderResult)
    assert result.exit_code == 0
    assert result.session_id_out == "7c5dcf5d"
    assert "backgrounded" in result.stdout
    assert result.duration_ms >= 0


def test_bg_create_subprocess_non_zero(tmp_path: Path, monkeypatch) -> None:
    """Non-zero subprocess exit raises ProviderSubprocessError with verbatim stderr."""
    from fno.agents.providers.claude import (
        ProviderSubprocessError,
        bg_create,
    )

    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(
        monkeypatch,
        exit_code=1,
        stderr="Error: not authenticated. Run claude /login\n",
    )

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    with pytest.raises(ProviderSubprocessError) as exc_info:
        bg_create(name="x", message="hi", cwd=cwd, timeout=10)

    err = exc_info.value
    assert err.exit_code == 1
    assert "not authenticated" in err.stderr


def test_bg_create_parse_failure(tmp_path: Path, monkeypatch) -> None:
    """Subprocess succeeds but unparseable stdout raises ProviderParseError."""
    from fno.agents.providers.claude import ProviderParseError, bg_create

    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch, stdout="Session created: foo-bar\n")

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    with pytest.raises(ProviderParseError) as exc_info:
        bg_create(name="x", message="hi", cwd=cwd, timeout=10)

    assert "Session created: foo-bar" in exc_info.value.stdout_head


# ---------------------------------------------------------------------------
# bg_create — argv overflow (AC1-EDGE 300KB → stdin pipe)
# ---------------------------------------------------------------------------


def test_bg_create_argv_overflow_routes_via_stdin(
    tmp_path: Path, monkeypatch
) -> None:
    """Messages above 200KB are piped via subprocess.run(input=msg)."""
    from fno.agents.providers.claude import bg_create

    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))

    stdin_dump = tmp_path / "stdin.bin"
    configure_fake(monkeypatch, stdin_dump=str(stdin_dump))

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    # 300KB message — clearly above the 200KB safety threshold.
    big_msg = "X" * (300 * 1024)
    result = bg_create(
        name="big",
        message=big_msg,
        cwd=cwd,
        timeout=15,
    )

    assert result.exit_code == 0
    assert result.session_id_out == "7c5dcf5d"
    # The fake captured stdin to disk — verify the full 300KB arrived.
    received = stdin_dump.read_text(encoding="utf-8")
    assert len(received) == len(big_msg)
    assert received == big_msg


def test_bg_create_just_under_threshold_uses_argv(
    tmp_path: Path, monkeypatch
) -> None:
    """Messages at or under the 200KB threshold are passed via argv (no stdin)."""
    from fno.agents.providers.claude import bg_create

    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))

    stdin_dump = tmp_path / "stdin.bin"
    configure_fake(monkeypatch, stdin_dump=str(stdin_dump))

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    small_msg = "Y" * (100 * 1024)  # 100KB — well under threshold
    result = bg_create(
        name="small",
        message=small_msg,
        cwd=cwd,
        timeout=10,
    )

    assert result.exit_code == 0
    assert result.session_id_out == "7c5dcf5d"
    # Argv path: stdin should be empty (the fake only dumps if reads stdin,
    # and the implementation does not pipe stdin for sub-threshold msgs).
    assert not stdin_dump.exists() or stdin_dump.read_text() == ""


# ---------------------------------------------------------------------------
# bg_create — argv shape verification via mock subprocess
# ---------------------------------------------------------------------------


def test_bg_create_argv_shape_small_message(tmp_path: Path, monkeypatch) -> None:
    """Sub-threshold bg_create invokes ``claude --bg --name <n> <msg>``."""
    from fno.agents.providers import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["argv"] = argv
        captured["input"] = kwargs.get("input")
        result = MagicMock()
        result.returncode = 0
        result.stdout = "backgrounded · 7c5dcf5d · demo\n"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)

    cwd = tmp_path / "workdir"
    cwd.mkdir()
    claude_mod.bg_create(name="demo", message="hi", cwd=cwd, timeout=5)

    argv = captured["argv"]
    assert argv[0] == "claude"
    assert "--bg" in argv
    assert "--name" in argv
    assert argv[argv.index("--name") + 1] == "demo"
    assert argv[-1] == "hi"
    assert captured["input"] is None  # argv path, not stdin


def test_bg_create_argv_shape_overflow_message(
    tmp_path: Path, monkeypatch
) -> None:
    """Over-threshold bg_create omits the message from argv and pipes via stdin."""
    from fno.agents.providers import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["argv"] = argv
        captured["input"] = kwargs.get("input")
        result = MagicMock()
        result.returncode = 0
        result.stdout = "backgrounded · 7c5dcf5d · demo\n"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)

    cwd = tmp_path / "workdir"
    cwd.mkdir()
    big = "Z" * (250 * 1024)  # above the 200KB threshold
    claude_mod.bg_create(name="demo", message=big, cwd=cwd, timeout=5)

    argv = captured["argv"]
    assert argv[0] == "claude"
    assert "--bg" in argv
    assert "--name" in argv
    # The literal 250KB message must NOT appear in argv
    for token in argv:
        assert len(token) < 200 * 1024
    # ... but it IS piped via stdin
    assert captured["input"] == big


def test_build_argv_model_pin_parity() -> None:
    """x-571f: a model pin appends ``--model <m>`` between --name and message;
    empty/None is byte-identical to today. Must match the Rust ``build_argv``
    cases in crates/fno-agents (AC2-FR cross-runtime parity, AC1-EDGE unset)."""
    from fno.agents.providers.claude import _build_argv

    assert _build_argv("a", "hi", False, "fable") == [
        "claude", "--bg", "--name", "a", "--model", "fable", "hi",
    ]
    assert _build_argv("a", "hi", True, "fable") == [
        "claude", "--bg", "--name", "a", "--model", "fable",
    ]
    # Empty/None pin == unset: byte-identical, no flag (AC1-EDGE).
    assert _build_argv("a", "hi", False, "") == _build_argv("a", "hi", False, None)
    assert _build_argv("a", "hi", False, None) == [
        "claude", "--bg", "--name", "a", "hi",
    ]


def test_build_argv_resume_session() -> None:
    """US4 bg-thread revival: a resume_session_id inserts ``--resume <uuid>`` so a
    replacement bg supervisor continues the dead session's transcript under the
    new account's env. Unset is byte-identical to today. This flag is spawn-only
    (the Rust ask-hop build_argv never resumes), so cross-runtime parity is scoped
    to the model/permission/effort flags, not this one."""
    from fno.agents.providers.claude import _build_argv

    assert _build_argv("a", "hi", False, resume_session_id="U-123") == [
        "claude", "--bg", "--name", "a", "--resume", "U-123", "hi",
    ]
    # stdin path (large message): message omitted, resume still present.
    assert _build_argv("a", "hi", True, resume_session_id="U-123") == [
        "claude", "--bg", "--name", "a", "--resume", "U-123",
    ]
    # Unset/empty == today (byte-identical, no flag).
    assert _build_argv("a", "hi", False, resume_session_id=None) == [
        "claude", "--bg", "--name", "a", "hi",
    ]
    assert _build_argv("a", "hi", False, resume_session_id="") == _build_argv(
        "a", "hi", False, resume_session_id=None
    )
    # Composes with a model pin (resume after model, before message).
    assert _build_argv("a", "hi", False, "fable", resume_session_id="U-9") == [
        "claude", "--bg", "--name", "a", "--model", "fable", "--resume", "U-9", "hi",
    ]


def test_build_argv_tier3_parity() -> None:
    """x-b6e2: the Tier-3 passthrough bundle maps to claude's own spellings in a
    fixed order (--add-dir/--agent/--allowedTools/--disallowedTools), riding
    after --effort and before --model. Must byte-match the Rust
    HarnessFlags::push_onto order (AC2-EDGE cross-runtime parity)."""
    from fno.agents.providers.claude import _build_argv

    assert _build_argv(
        "a", "hi", False, None, None, None,
        add_dir="/work", agent="reviewer", tools="Read,Edit", deny_tools="Bash",
    ) == [
        "claude", "--bg", "--name", "a",
        "--add-dir", "/work",
        "--agent", "reviewer",
        "--allowedTools", "Read,Edit",
        "--disallowedTools", "Bash",
        "hi",
    ]
    # Empty fields are unset: byte-identical to the bare argv.
    assert _build_argv("a", "hi", False, add_dir="", agent=None) == [
        "claude", "--bg", "--name", "a", "hi",
    ]


def test_headless_create_applies_account_env(tmp_path: Path, monkeypatch) -> None:
    """x-d012: --account headless must thread CLAUDE_CONFIG_DIR into the -p env
    (a one-shot claude -p inherits the parent env otherwise -> mis-bill)."""
    from fno.agents.providers import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["env"] = kwargs.get("env")
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(
        message="hi", cwd=cwd, account_env={"CLAUDE_CONFIG_DIR": "/x/.claude-alt"}
    )
    env = captured["env"]
    assert env is not None and env["CLAUDE_CONFIG_DIR"] == "/x/.claude-alt"


def test_headless_create_no_account_inherits_env(tmp_path: Path, monkeypatch) -> None:
    """No --account -> no explicit env (byte-identical to today: inherits parent)."""
    from fno.agents.providers import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["has_env"] = "env" in kwargs
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(message="hi", cwd=cwd)
    assert captured["has_env"] is False


def test_headless_create_scrubs_inherited_auth(tmp_path: Path, monkeypatch) -> None:
    """x-d012: an --account spawn scrubs inherited ANTHROPIC_API_KEY /
    CLAUDE_CODE_OAUTH_TOKEN so an ambient token can't override the account."""
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-parent")
    monkeypatch.setenv("CLAUDE_CODE_OAUTH_TOKEN", "oauth-parent")
    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["env"] = kwargs.get("env")
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(
        message="hi", cwd=cwd, account_env={"CLAUDE_CONFIG_DIR": "/x/.claude-alt"}
    )
    env = captured["env"]
    assert env["CLAUDE_CONFIG_DIR"] == "/x/.claude-alt"
    assert "ANTHROPIC_API_KEY" not in env
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in env
