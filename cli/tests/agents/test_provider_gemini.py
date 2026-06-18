"""Unit tests for providers.gemini (US4-gemini Wave 2.1).

Covers the small-surface invariants without exercising a real gemini
subprocess: parser shape, error class taxonomy, inject_from_name +
sandbox_flag, and the reachability probe's tri-state contract.

Real-subprocess testing lives in Wave 2.3
(test_gemini_integration_smoke.py + test_gemini_signal_handling.py).
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from unittest import mock

import pytest

from fno.agents.providers import gemini as gemini_mod
from fno.agents.providers.base import ReachabilityProbeError


# ---------------------------------------------------------------------------
# inject_from_name + sandbox_flag — Locked Decision 8 / OQ5 ports
# ---------------------------------------------------------------------------


def test_inject_from_name_matches_codex_contract() -> None:
    """Identical string contract to codex (Locked Decision 8)."""
    result = gemini_mod.inject_from_name("draft the migration", "orchestrator")
    assert result == "[from: orchestrator]\n\ndraft the migration"


def test_inject_from_name_handles_multiline_prompt() -> None:
    """Multiline prompts retain their internal newlines."""
    result = gemini_mod.inject_from_name("line1\nline2", "agent-A")
    assert result == "[from: agent-A]\n\nline1\nline2"


def test_sandbox_flag_yolo_returns_yolo_token() -> None:
    """Explicit full yolo passes bare --yolo (unsandboxed full-auto)."""
    assert gemini_mod.sandbox_flag(True) == ["--yolo"]


def test_sandbox_flag_bounded_with_sandbox_provider() -> None:
    """AC2-HP: bounded + a sandbox provider -> --approval-mode yolo --sandbox
    (never-prompt AND sandboxed); never `default`/`auto_edit`."""
    assert gemini_mod.sandbox_flag(False, sandbox_available=True) == [
        "--approval-mode", "yolo", "--sandbox",
    ]


def test_sandbox_flag_bounded_fallback_no_provider() -> None:
    """AC2-EDGE: bounded with NO sandbox provider falls back to
    --approval-mode yolo (never-prompt, unsandboxed), never a prompting mode."""
    assert gemini_mod.sandbox_flag(False, sandbox_available=False) == [
        "--approval-mode", "yolo",
    ]
    # the fallback NEVER emits a prompting mode
    assert "default" not in gemini_mod.sandbox_flag(False, sandbox_available=False)
    assert "auto_edit" not in gemini_mod.sandbox_flag(False, sandbox_available=False)


def test_sandbox_provider_detection_never_raises() -> None:
    """AC2-FR: the detection helper is best-effort and returns a bool, never raises."""
    assert isinstance(gemini_mod._gemini_sandbox_available(), bool)


# ---------------------------------------------------------------------------
# Bounded-posture amendment (US2/US3). The headless autonomous exec lane
# (create/resume) defaults to BOUNDED (--approval-mode yolo --sandbox), so a
# headless gemini cannot hang AND keeps the sandbox. Full yolo (bare --yolo,
# unsandboxed) is reachable only via the explicit yolo opt-in.
# ---------------------------------------------------------------------------

_BOUNDED = ["--approval-mode", "yolo", "--sandbox"]
_FULL_YOLO = ["--yolo"]


def _hermetic_config(tmp_path, monkeypatch, content: str = "schema_version: 1\n") -> None:
    from fno import config as config_mod

    f = tmp_path / "settings.yaml"
    f.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(f))
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def test_headless_default_is_bounded() -> None:
    """AC2-HP: a headless gemini exec (not yolo) is BOUNDED, not full --yolo."""
    eff = gemini_mod._effective_yolo(yolo=False, headless_yolo=False)
    assert gemini_mod.sandbox_flag(eff, sandbox_available=True) == _BOUNDED


def test_config_full_yolo_opt_in_yields_bare_yolo() -> None:
    """headless_yolo=true opts into bare --yolo (unsandboxed full-auto)."""
    eff = gemini_mod._effective_yolo(yolo=False, headless_yolo=True)
    assert gemini_mod.sandbox_flag(eff) == _FULL_YOLO


def test_explicit_yolo_forces_bare_yolo() -> None:
    """AC3-HP: the explicit yolo bareword forces bare --yolo."""
    eff = gemini_mod._effective_yolo(yolo=True, headless_yolo=False)
    assert gemini_mod.sandbox_flag(eff) == _FULL_YOLO


def test_headless_default_resolves_from_config_to_bounded(tmp_path, monkeypatch) -> None:
    """headless_yolo=None reads config; default config (no agents block) -> BOUNDED."""
    _hermetic_config(tmp_path, monkeypatch)
    eff = gemini_mod._effective_yolo(yolo=False, headless_yolo=None)
    assert gemini_mod.sandbox_flag(eff, sandbox_available=True) == _BOUNDED


def test_headless_config_full_yolo_resolves_to_bare_yolo(tmp_path, monkeypatch) -> None:
    """A config full-yolo opt-in resolves through headless_yolo=None to bare --yolo."""
    _hermetic_config(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    gemini:\n      headless_yolo: true\n",
    )
    eff = gemini_mod._effective_yolo(yolo=False, headless_yolo=None)
    assert gemini_mod.sandbox_flag(eff) == _FULL_YOLO


# ---------------------------------------------------------------------------
# _parse_response — single-blob JSON parser shape (the cleavage from codex)
# ---------------------------------------------------------------------------


def test_parse_response_happy_path() -> None:
    """Pinned shape from Wave 2.0 fixture."""
    payload = json.dumps({
        "session_id": "cedb6b44-d140-4fa4-86f1-3b3e7aed339d",
        "response": "The magic word is TURNIP.",
        "stats": {"models": {}},
    })
    session_id, reply = gemini_mod._parse_response(payload)
    assert session_id == "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"
    assert reply == "The magic word is TURNIP."


def test_parse_response_uses_pinned_fixture(tmp_path: Path) -> None:
    """The committed fixture deserializes through _parse_response without
    raising — Wave 2.3 will re-pin against a fresh capture; this test
    pins against the snapshot we shipped."""
    fixture = (
        Path(__file__).parent / "fixtures" / "gemini-json-sample.json"
    )
    text = fixture.read_text(encoding="utf-8")
    session_id, reply = gemini_mod._parse_response(text)
    assert session_id is not None and len(session_id) == 36
    assert reply  # non-empty in the captured run ("PONG")


def test_parse_response_empty_reply_returns_empty_string() -> None:
    """Locked Decision (silent-failure-hunter row 2): empty response is
    NOT null — gemini returns "" when the model declined to emit text."""
    payload = json.dumps({
        "session_id": "11111111-1111-1111-1111-111111111111",
        "response": "",
        "stats": {},
    })
    session_id, reply = gemini_mod._parse_response(payload)
    assert reply == ""


def test_parse_response_null_reply_treated_as_empty() -> None:
    """The model-errored case (gemini emits response: null) surfaces as
    "" so the caller relies on the non-zero exit code + stderr tee for
    error context."""
    payload = json.dumps({
        "session_id": "11111111-1111-1111-1111-111111111111",
        "response": None,
        "stats": {},
    })
    session_id, reply = gemini_mod._parse_response(payload)
    assert reply == ""


def test_parse_response_malformed_json_raises() -> None:
    """AC4-EDGE: malformed JSON raises GeminiParseError with raw head."""
    raw = "{not valid json"
    with pytest.raises(gemini_mod.GeminiParseError) as exc_info:
        gemini_mod._parse_response(raw)
    assert exc_info.value.raw_head == raw


def test_parse_response_empty_stdout_raises() -> None:
    """Empty / whitespace-only stdout raises (degenerate case)."""
    with pytest.raises(gemini_mod.GeminiParseError):
        gemini_mod._parse_response("")
    with pytest.raises(gemini_mod.GeminiParseError):
        gemini_mod._parse_response("   \n\t")


def test_parse_response_non_object_raises() -> None:
    """A JSON array at the top level is schema drift — not the object shape."""
    with pytest.raises(gemini_mod.GeminiParseError):
        gemini_mod._parse_response('["a", "b"]')


def test_parse_response_non_string_session_id_raises() -> None:
    """Schema drift: a future gemini release that returns int session_id
    must fail loudly rather than break downstream registry writes."""
    payload = json.dumps({
        "session_id": 12345,
        "response": "ok",
        "stats": {},
    })
    with pytest.raises(gemini_mod.GeminiParseError):
        gemini_mod._parse_response(payload)


def test_parse_response_missing_session_id_returns_none() -> None:
    """The session_id field is parsed when present; absence is allowed
    here (the caller decides whether expect_session forces a raise)."""
    payload = json.dumps({"response": "hi", "stats": {}})
    session_id, reply = gemini_mod._parse_response(payload)
    assert session_id is None
    assert reply == "hi"


def test_parse_response_missing_response_field_raises() -> None:
    """Codex P2 fix (PR #317): a missing ``response`` key is schema drift,
    not a silent empty reply. Pre-fix, ``{session_id: "..."}`` parsed as
    success with reply="" and updated the registry — contract regressions
    became invisible. Post-fix: GeminiParseError fails loudly."""
    payload = json.dumps({
        "session_id": "11111111-1111-1111-1111-111111111111",
        "stats": {},
    })
    with pytest.raises(gemini_mod.GeminiParseError):
        gemini_mod._parse_response(payload)


def test_parse_response_missing_stats_field_raises() -> None:
    """Codex P2 fix (PR #317): a missing ``stats`` key is schema drift."""
    payload = json.dumps({
        "session_id": "11111111-1111-1111-1111-111111111111",
        "response": "hi",
    })
    with pytest.raises(gemini_mod.GeminiParseError):
        gemini_mod._parse_response(payload)


# ---------------------------------------------------------------------------
# Error class taxonomy — exit code surface contract
# ---------------------------------------------------------------------------


def test_invocation_error_carries_exit_code() -> None:
    err = gemini_mod.GeminiInvocationError(11)
    assert err.exit_code == 11
    assert "11" in str(err)


def test_parse_error_carries_raw_head() -> None:
    err = gemini_mod.GeminiParseError("garbage")
    assert err.raw_head == "garbage"


def test_timeout_error_carries_timeout() -> None:
    err = gemini_mod.GeminiTimeoutError(30.0)
    assert err.timeout_sec == 30.0
    assert "30.0" in str(err)


# ---------------------------------------------------------------------------
# gemini_session_reachable — tri-state probe contract
# ---------------------------------------------------------------------------


def test_reachable_true_when_session_file_matches(
    tmp_path: Path, monkeypatch
) -> None:
    """Happy path: a session file matching the short prefix + full UUID
    in its first-line metadata returns True."""
    cwd = tmp_path / "myproject"
    cwd.mkdir()
    # Stub Path.home() so we control the gemini chats dir layout.
    fake_home = tmp_path / "home"
    chats_dir = fake_home / ".gemini" / "tmp" / "myproject" / "chats"
    chats_dir.mkdir(parents=True)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    session_id = "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"
    session_file = chats_dir / "session-2026-05-21T22-13-cedb6b44.jsonl"
    session_file.write_text(
        json.dumps({"sessionId": session_id, "startTime": "2026-05-21"}) + "\n"
    )

    assert gemini_mod.gemini_session_reachable(session_id, cwd) is True


def test_reachable_false_when_no_short_prefix_match(
    tmp_path: Path, monkeypatch
) -> None:
    """Chats dir exists but no file matches the session's short prefix
    -> definitive miss, returns False (reconcile flips to orphaned)."""
    cwd = tmp_path / "myproject"
    cwd.mkdir()
    fake_home = tmp_path / "home"
    chats_dir = fake_home / ".gemini" / "tmp" / "myproject" / "chats"
    chats_dir.mkdir(parents=True)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    # Seed an unrelated session
    (chats_dir / "session-2026-05-21T22-13-deadbeef.jsonl").write_text("{}\n")

    session_id = "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"
    assert gemini_mod.gemini_session_reachable(session_id, cwd) is False


def test_reachable_raises_when_chats_dir_missing(
    tmp_path: Path, monkeypatch
) -> None:
    """No chats dir for this cwd -> tri-state inconclusive (raise)."""
    cwd = tmp_path / "myproject"
    cwd.mkdir()
    fake_home = tmp_path / "home"
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    session_id = "11111111-1111-1111-1111-111111111111"
    with pytest.raises(ReachabilityProbeError) as exc_info:
        gemini_mod.gemini_session_reachable(session_id, cwd)
    assert exc_info.value.provider == "gemini"
    assert "chats dir does not exist" in exc_info.value.reason


def test_reachable_raises_on_permission_error(
    tmp_path: Path, monkeypatch
) -> None:
    """AC8-FR: chmod 000 on the chats dir surfaces as inconclusive."""
    cwd = tmp_path / "myproject"
    cwd.mkdir()
    fake_home = tmp_path / "home"
    chats_dir = fake_home / ".gemini" / "tmp" / "myproject" / "chats"
    chats_dir.mkdir(parents=True)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    # Force PermissionError via monkeypatched glob.
    real_glob = chats_dir.glob
    def raise_perm(*args, **kwargs):
        raise PermissionError("EACCES on chats dir")
    monkeypatch.setattr(Path, "glob", lambda self, pat: (_ for _ in ()).throw(PermissionError("EACCES")))

    session_id = "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"
    with pytest.raises(ReachabilityProbeError) as exc_info:
        gemini_mod.gemini_session_reachable(session_id, cwd)
    assert exc_info.value.provider == "gemini"
    assert "EACCES" in exc_info.value.reason or "permission" in exc_info.value.reason.lower()


def test_reachable_raises_on_empty_session_id(tmp_path: Path) -> None:
    """Empty UUID is a registry-corruption signal -> inconclusive."""
    with pytest.raises(ReachabilityProbeError) as exc_info:
        gemini_mod.gemini_session_reachable("", tmp_path)
    assert "empty" in exc_info.value.reason.lower()


def test_reachable_raises_on_too_short_session_id(tmp_path: Path) -> None:
    """A truncated UUID (less than 8 hex chars) cannot match the
    short-prefix layout — inconclusive."""
    with pytest.raises(ReachabilityProbeError):
        gemini_mod.gemini_session_reachable("abc", tmp_path)


def test_reachable_short_prefix_collision_returns_false(
    tmp_path: Path, monkeypatch
) -> None:
    """A short-prefix match whose first-line UUID differs from ours
    means the file is a DIFFERENT session (vanishingly rare but
    handled defensively) -> definitively False."""
    cwd = tmp_path / "myproject"
    cwd.mkdir()
    fake_home = tmp_path / "home"
    chats_dir = fake_home / ".gemini" / "tmp" / "myproject" / "chats"
    chats_dir.mkdir(parents=True)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    our_session = "abc12345-1111-1111-1111-111111111111"
    other_session = "abc12345-2222-2222-2222-222222222222"  # same prefix!
    session_file = chats_dir / "session-2026-05-21T22-13-abc12345.jsonl"
    session_file.write_text(json.dumps({"sessionId": other_session}) + "\n")

    assert gemini_mod.gemini_session_reachable(our_session, cwd) is False


# ---------------------------------------------------------------------------
# Tee + stderr drain (basic shape; full subprocess tests in Wave 2.3)
# ---------------------------------------------------------------------------


def test_open_tee_creates_parent_directories(tmp_path: Path) -> None:
    """The tee opener mkdirs parent on first write."""
    output_path = tmp_path / "nested" / "deeper" / "output.jsonl"
    fh = gemini_mod._open_tee(output_path)
    try:
        fh.write("hello\n")
    finally:
        fh.close()
    assert output_path.parent.is_dir()
    assert output_path.read_text(encoding="utf-8") == "hello\n"


def test_drain_pipe_into_list_appends_to_tee_and_returns_text(tmp_path: Path) -> None:
    """_drain_pipe_into_list collects bytes AND tees them via the shared lock."""
    import subprocess
    import threading

    output_path = tmp_path / "out.jsonl"
    tee_fh = gemini_mod._open_tee(output_path)

    # Spawn a tiny subprocess that emits two stderr lines and exits.
    proc = subprocess.Popen(
        ["sh", "-c", "echo a >&2; echo b >&2"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    proc.wait()

    captured: list[str] = []
    tee_lock = threading.Lock()
    gemini_mod._drain_pipe_into_list(proc.stderr, captured, 1024, tee_fh, tee_lock)
    tee_fh.close()
    assert "a\n" in captured
    assert "b\n" in captured
    assert "a\n" in output_path.read_text()
    assert "b\n" in output_path.read_text()


# ---------------------------------------------------------------------------
# Schema-drift smoke test against the pinned fixture
# ---------------------------------------------------------------------------


def test_pinned_keys_match_fixture(tmp_path: Path) -> None:
    """The _GEMINI_KEYS constants block matches the captured fixture's
    actual top-level keys. A future gemini release that drifts will
    fail the Wave 2.3 smoke test; this static check catches naming
    drift in the constants block itself (e.g. a typo in _GEMINI_KEYS)."""
    fixture = (
        Path(__file__).parent / "fixtures" / "gemini-json-sample.json"
    )
    data = json.loads(fixture.read_text(encoding="utf-8"))
    assert gemini_mod._GEMINI_KEYS["session"] in data
    assert gemini_mod._GEMINI_KEYS["reply"] in data
    assert gemini_mod._GEMINI_KEYS["stats"] in data
