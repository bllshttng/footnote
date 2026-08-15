"""Tests for fno.agents.harnesses.claude — TDD Red phase for Task 1.2.

ACs (US1):
- AC1-HP: bg_create returns ProviderResult with parsed short_id from stdout
- Locked Decision 6: regex ``^backgrounded · ([0-9a-f]{8}) · `` extracts id
- AC1-FR parse failure: ProviderParseError carries first 200 chars of stdout
- AC1-FR subprocess non-zero: ProviderSubprocessError preserves verbatim stderr
- AC1-EDGE argv overflow: ``len_argv > 200KB`` routes via subprocess stdin
"""
from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest

from tests.agents._fake_claude import configure_fake, install_fake_claude


# ---------------------------------------------------------------------------
# Symbol surface
# ---------------------------------------------------------------------------


def test_provider_module_exports() -> None:
    """claude.py exports bg_create + parse_short_id + error types."""
    from fno.agents.harnesses import claude as claude_mod

    assert hasattr(claude_mod, "bg_create")
    assert hasattr(claude_mod, "parse_short_id")
    assert hasattr(claude_mod, "ProviderParseError")
    assert hasattr(claude_mod, "ProviderSubprocessError")


# ---------------------------------------------------------------------------
# parse_short_id (unit-level regex contract)
# ---------------------------------------------------------------------------


def test_parse_short_id_extracts_8hex() -> None:
    """parse_short_id extracts the 8-hex id from the documented stdout shape."""
    from fno.agents.harnesses.claude import parse_short_id

    stdout = "backgrounded · 7c5dcf5d · frontend-worker\n"
    assert parse_short_id(stdout) == "7c5dcf5d"


def test_parse_short_id_only_first_line() -> None:
    """parse_short_id only consults stdout's first line (AC contract anchor)."""
    from fno.agents.harnesses.claude import parse_short_id

    stdout = "backgrounded · abcdef01 · worker\nsome trailing noise\n"
    assert parse_short_id(stdout) == "abcdef01"


def test_parse_short_id_rejects_uppercase_hex() -> None:
    """Locked Decision 6: short-id regex requires LOWERCASE 8-hex."""
    from fno.agents.harnesses.claude import ProviderParseError, parse_short_id

    stdout = "backgrounded · ABCDEF01 · worker\n"
    with pytest.raises(ProviderParseError):
        parse_short_id(stdout)


def test_parse_short_id_rejects_wrong_length() -> None:
    from fno.agents.harnesses.claude import ProviderParseError, parse_short_id

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
    from fno.agents.harnesses.claude import ProviderParseError, parse_short_id

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
    from fno.agents.harnesses.base import ProviderResult
    from fno.agents.harnesses.claude import bg_create

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
    from fno.agents.harnesses.claude import (
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
    from fno.agents.harnesses.claude import ProviderParseError, bg_create

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
    from fno.agents.harnesses.claude import bg_create

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
    from fno.agents.harnesses.claude import bg_create

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
    from fno.agents.harnesses import claude as claude_mod

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


def test_claude_stop_hook_block_cap_default_and_override(monkeypatch) -> None:
    """The cap defaults to 50 and honors an operator-set env value (x-1680)."""
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.delenv("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", raising=False)
    assert claude_mod.claude_stop_hook_block_cap() == "50"
    monkeypatch.setenv("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", "99")
    assert claude_mod.claude_stop_hook_block_cap() == "99"


def test_bg_create_sets_stop_hook_block_cap(tmp_path: Path, monkeypatch) -> None:
    """bg spawn_env carries the raised Stop-hook block cap, honoring override (x-1680)."""
    from fno.agents.harnesses import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["env"] = kwargs.get("env")
        result = MagicMock()
        result.returncode = 0
        result.stdout = "backgrounded · 7c5dcf5d · demo\n"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    monkeypatch.delenv("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", raising=False)

    cwd = tmp_path / "workdir"
    cwd.mkdir()
    claude_mod.bg_create(name="demo", message="hi", cwd=cwd, timeout=5)
    assert captured["env"]["CLAUDE_CODE_STOP_HOOK_BLOCK_CAP"] == "50"

    # An explicit operator value wins over the fno default.
    monkeypatch.setenv("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", "99")
    claude_mod.bg_create(name="demo2", message="hi", cwd=cwd, timeout=5)
    assert captured["env"]["CLAUDE_CODE_STOP_HOOK_BLOCK_CAP"] == "99"


def test_bg_create_argv_shape_overflow_message(
    tmp_path: Path, monkeypatch
) -> None:
    """Over-threshold bg_create omits the message from argv and pipes via stdin."""
    from fno.agents.harnesses import claude as claude_mod

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
    from fno.agents.harnesses.claude import _build_argv

    assert _build_argv("a", "hi", False, "fable") == [
        "claude", "--bg", "--name", "a", "--model", "fable", "--", "hi",
    ]
    assert _build_argv("a", "hi", True, "fable") == [
        "claude", "--bg", "--name", "a", "--model", "fable",
    ]
    # Empty/None pin == unset: byte-identical, no flag (AC1-EDGE).
    assert _build_argv("a", "hi", False, "") == _build_argv("a", "hi", False, None)
    assert _build_argv("a", "hi", False, None) == [
        "claude", "--bg", "--name", "a", "--", "hi",
    ]


def test_build_argv_resume_session() -> None:
    """US4 bg-thread revival: a resume_session_id inserts ``--resume <uuid>`` so a
    replacement bg supervisor continues the dead session's transcript under the
    new account's env. Unset is byte-identical to today. This flag is spawn-only
    (the Rust ask-hop build_argv never resumes), so cross-runtime parity is scoped
    to the model/permission/effort flags, not this one."""
    from fno.agents.harnesses.claude import _build_argv

    assert _build_argv("a", "hi", False, resume_session_id="U-123") == [
        "claude", "--bg", "--name", "a", "--resume", "U-123", "--", "hi",
    ]
    # stdin path (large message): message omitted, resume still present.
    assert _build_argv("a", "hi", True, resume_session_id="U-123") == [
        "claude", "--bg", "--name", "a", "--resume", "U-123",
    ]
    # Unset/empty == today (byte-identical, no flag).
    assert _build_argv("a", "hi", False, resume_session_id=None) == [
        "claude", "--bg", "--name", "a", "--", "hi",
    ]
    assert _build_argv("a", "hi", False, resume_session_id="") == _build_argv(
        "a", "hi", False, resume_session_id=None
    )
    # Composes with a model pin (resume after model, before message).
    assert _build_argv("a", "hi", False, "fable", resume_session_id="U-9") == [
        "claude", "--bg", "--name", "a", "--model", "fable", "--resume", "U-9", "--", "hi",
    ]


def test_build_argv_tier3_parity() -> None:
    """x-b6e2: the Tier-3 passthrough bundle maps to claude's own spellings in a
    fixed order (--add-dir/--agent/--allowedTools/--disallowedTools), riding
    after --effort and before --model. Must byte-match the Rust
    HarnessFlags::push_onto order (AC2-EDGE cross-runtime parity)."""
    from fno.agents.harnesses.claude import _build_argv

    assert _build_argv(
        "a", "hi", False, None, None, None,
        add_dir="/work", agent="reviewer", tools="Read,Edit", deny_tools="Bash",
    ) == [
        "claude", "--bg", "--name", "a",
        "--add-dir", "/work",
        "--agent", "reviewer",
        "--allowedTools", "Read,Edit",
        "--disallowedTools", "Bash",
        "--",
        "hi",
    ]
    # Empty fields are unset: byte-identical to the bare argv.
    assert _build_argv("a", "hi", False, add_dir="", agent=None) == [
        "claude", "--bg", "--name", "a", "--", "hi",
    ]


def test_headless_create_applies_account_env(tmp_path: Path, monkeypatch) -> None:
    """x-d012: --account headless must thread CLAUDE_CONFIG_DIR into the -p env
    (a one-shot claude -p inherits the parent env otherwise -> mis-bill)."""
    from fno.agents.harnesses import claude as claude_mod

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
    from fno.agents.harnesses import claude as claude_mod

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


def test_headless_create_forwards_json_output_format(tmp_path: Path, monkeypatch) -> None:
    """Internal canonical callers can preserve Claude's result envelope."""
    from fno.agents.harnesses import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["argv"] = argv
        result = MagicMock()
        result.returncode = 0
        result.stdout = '{"is_error":false}'
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(message="hi", cwd=cwd, output_format="json")

    argv = captured["argv"]
    assert argv[-4:] == ["--output-format", "json", "--", "hi"]


def test_headless_create_scrubs_inherited_auth(tmp_path: Path, monkeypatch) -> None:
    """x-d012: an --account spawn scrubs inherited ANTHROPIC_API_KEY /
    CLAUDE_CODE_OAUTH_TOKEN so an ambient token can't override the account."""
    from fno.agents.harnesses import claude as claude_mod

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


def test_headless_create_routed_scrubs_ambient_creds(tmp_path: Path, monkeypatch) -> None:
    """x-6de8 codex P1: a routed headless spawn must scrub ambient
    ANTHROPIC_API_KEY / CLAUDE_CODE_OAUTH_TOKEN (claude prefers an env credential
    over the --settings file), else it authenticates with the primary account
    instead of the routed provider. Mirrors bg_create's scrub."""
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-parent")
    monkeypatch.setenv("CLAUDE_CODE_OAUTH_TOKEN", "oauth-parent")
    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["env"] = kwargs.get("env")
        captured["argv"] = argv
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(
        message="hi",
        cwd=cwd,
        route_env={
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "zk-routed",
        },
    )
    env = captured["env"]
    assert env is not None, "route_env must build a scrubbed spawn env"
    assert "ANTHROPIC_API_KEY" not in env
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in env
    assert env["ANTHROPIC_AUTH_TOKEN"] == "zk-routed"
    assert env["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert "--settings" in captured["argv"]


def test_headless_receipt_emitted_before_blocking_subprocess(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """The headless spawn receipt is flushed to stderr BEFORE the synchronous
    blocking claude -p runs, and names the passed worktree cwd + transcript
    locator. Without an up-front receipt a long one-shot looks dead to a
    watcher (the twin-spawn failure mode this closes)."""
    import json

    from fno.agents.harnesses import claude as claude_mod
    from fno.provenance.resolver import _slug

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        # Snapshot stderr AT the moment the blocking subprocess is invoked:
        # the receipt must already be flushed here, not written after.
        captured["stderr_at_call"] = capsys.readouterr().err
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(message="hi", cwd=cwd, name="wk-zzz")

    snap = captured.get("stderr_at_call")
    assert snap, "no receipt was flushed before the blocking subprocess call"
    rec = json.loads(snap.strip().splitlines()[-1])
    assert rec["substrate"] == "headless"
    assert rec["name"] == "wk-zzz"
    assert rec["cwd"] == str(cwd)
    assert rec["lifecycle"] == "ephemeral"
    assert rec["roster"] == "unbound"
    # transcript locator slug derived from the effective cwd (canonical encoding)
    assert _slug(str(cwd)) in rec["transcript_dir"]
    # promises nothing that does not exist yet at this synchronous boundary
    assert "session_id" not in rec
    assert "pid" not in rec


def test_headless_receipt_goes_to_stderr_not_stdout(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """The receipt goes to stderr so stdout stays the clean reply stream. A
    structured consumer (pr_watch under --output-format json) does json.loads on
    the full headless stdout; a receipt line there would corrupt the envelope."""
    import json

    from fno.agents.harnesses import claude as claude_mod

    envelope = '{"is_error":false,"result":"ok"}'

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        result = MagicMock()
        result.returncode = 0
        result.stdout = envelope
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    res = claude_mod.headless_create(
        message="hi", cwd=cwd, name="wk", output_format="json"
    )
    captured = capsys.readouterr()
    # the reply returned to the caller is the clean envelope, unmodified
    assert json.loads(res.stdout) == json.loads(envelope)
    # the receipt landed on stderr, never on stdout
    rec = json.loads(captured.err.strip().splitlines()[-1])
    assert rec["substrate"] == "headless"
    assert "headless" not in captured.out


def test_headless_receipt_resolves_remapped_model(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """The receipt prints the RESOLVED request model when a DEFAULT_*_MODEL env
    var remaps the alias, so an operator is not misled into thinking the
    spawn serves 'opus' when the routed secondary model actually wins."""
    import json

    from fno.agents.harnesses import claude as claude_mod

    # Clear ambient overrides so the alias-remap path is exercised
    # deterministically (this test process may itself run routed, with
    # ANTHROPIC_MODEL set as a global override that would otherwise win).
    monkeypatch.delenv("ANTHROPIC_MODEL", raising=False)
    monkeypatch.setenv("ANTHROPIC_DEFAULT_OPUS_MODEL", "glm-5.2")
    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["stderr_at_call"] = capsys.readouterr().err
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(message="hi", cwd=cwd, name="wk", model="opus")

    rec = json.loads(captured["stderr_at_call"].strip().splitlines()[-1])
    assert rec["model"] == "glm-5.2"


def test_headless_receipt_reports_routed_model_without_argv_pin(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """cli.py's --provider/--model path clears the argv model token and routes
    via ANTHROPIC_MODEL env instead; the receipt must still report that routed
    model, not null."""
    import json

    from fno.agents.harnesses import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["stderr_at_call"] = capsys.readouterr().err
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(
        message="hi",
        cwd=cwd,
        name="wk",
        route_env={
            "ANTHROPIC_BASE_URL": "https://z",
            "ANTHROPIC_AUTH_TOKEN": "t",
            "ANTHROPIC_MODEL": "glm-5.2",
        },
    )

    rec = json.loads(captured["stderr_at_call"].strip().splitlines()[-1])
    assert rec["model"] == "glm-5.2"


def test_headless_receipt_transcript_dir_follows_account_config_dir(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """An --account spawn relocates claude's config tree via CLAUDE_CONFIG_DIR;
    the transcript locator must point under that root, not ~/.claude/projects
    where the transcript would never appear (which would recreate the
    false-dead failure this receipt exists to prevent)."""
    import json

    from fno.agents.harnesses import claude as claude_mod

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["stderr_at_call"] = capsys.readouterr().err
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "wd"
    cwd.mkdir()
    claude_mod.headless_create(
        message="hi",
        cwd=cwd,
        name="wk",
        account_env={"CLAUDE_CONFIG_DIR": "/x/.claude-alt"},
    )

    rec = json.loads(captured["stderr_at_call"].strip().splitlines()[-1])
    assert rec["transcript_dir"].startswith("/x/.claude-alt/projects/")


def test_headless_receipt_slug_preserves_underscore(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """The transcript slug uses claude's canonical encoding (provenance
    resolver): underscores are preserved, not folded to dashes, so the locator
    points at the real projects dir for an underscore-bearing cwd."""
    import json

    from fno.agents.harnesses import claude as claude_mod
    from fno.provenance.resolver import _slug

    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        captured["stderr_at_call"] = capsys.readouterr().err
        result = MagicMock()
        result.returncode = 0
        result.stdout = "ok"
        result.stderr = ""
        return result

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    cwd = tmp_path / "my_repo"
    cwd.mkdir()
    claude_mod.headless_create(message="hi", cwd=cwd, name="wk")

    rec = json.loads(captured["stderr_at_call"].strip().splitlines()[-1])
    assert _slug(str(cwd)) in rec["transcript_dir"]
    assert "my_repo" in rec["transcript_dir"]
    assert "my-repo" not in rec["transcript_dir"]
