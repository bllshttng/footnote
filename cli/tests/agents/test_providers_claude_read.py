"""Tests for the US3 read-path additions to providers/claude.py.

Covers:
- ``claude_agents_json()`` — best-effort shellout, all-failure-modes return ({}, warnings).
- ``logs(short_id, ...)`` — shells out to ``claude logs <short_id>``, raw passthrough.

The actual ``claude`` binary is not invoked; ``subprocess.run`` is
monkeypatched via the module-level ``_subprocess_run`` indirection that
the existing surface already exposes (Locked Decision 6 from US1).
"""
from __future__ import annotations

import io
import json
import subprocess
from types import SimpleNamespace

import pytest

from fno.agents.providers import claude as claude_mod


def _fake_completed(stdout: str = "", stderr: str = "", returncode: int = 0):
    return SimpleNamespace(stdout=stdout, stderr=stderr, returncode=returncode)


# --- claude_agents_json() ---------------------------------------------------


def test_claude_agents_json_accepts_the_current_claude_row_shape(monkeypatch):
    """Regression: the parser read ``short_id``/``status``, but claude emits
    ``id``/``state``. Every row was dropped, so the live_status enrichment
    returned {} on EVERY call while this file's other tests stayed green on
    fabricated old-shape fixtures - a test suite pinning a shape production
    never produces.

    This payload is copied verbatim from real ``claude agents --json`` output
    (keys and lowercase state values as observed), so it fails if the parser
    stops handling what the binary actually emits."""
    payload = {
        "agents": [
            {
                "id": "907fc8c5",
                "sessionId": "907fc8c5-137e-44ff-95a0-f02b038ffefe",
                "name": "king-crown-agents",
                "state": "working",
                "kind": "background",
                "cwd": "/repo",
                "startedAt": 1786023667738,
            },
            {
                "id": "baf9409a",
                "sessionId": "baf9409a-2af5-444a-846c-059e8fa2f758",
                "name": "tgt-let-s-think-about-wh",
                "state": "blocked",
                "kind": "background",
            },
        ]
    }

    def _fake(argv, **kwargs):
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {
        "907fc8c5": {"live_status": "Working"},
        "baf9409a": {"live_status": "Needs input"},
    }
    # No row is dropped and no vocabulary-drift warning fires: "working" and
    # "blocked" are real values the current binary emits, not drift. They are
    # normalized onto the output vocabulary rather than passed through raw, so
    # a consumer never has to know which binary produced the row.
    assert warnings == [], warnings


def test_claude_agents_json_prefers_state_over_status_on_live_rows(monkeypatch):
    """Regression: `state` outranks `status` when a row carries both.

    Payload captured verbatim from ``claude agents --json`` (2.1.220), which
    emits BOTH keys on the rows that have a live pid, in two different
    vocabularies whose values disagree: ``state: working`` next to
    ``status: idle``. Preferring ``status`` - the older spelling, present on a
    handful of rows where ``state`` is on ~94% of them - resolved an actively
    working session to Idle, which read.py then replaced with a stale
    truth-status reading. That is the render-Idle-while-working bug, arrived at
    after the id/state aliases had already landed.
    """
    payload = [
        {
            "pid": 66325,
            "id": "7f7d7627",
            "cwd": "/repo/.claude/worktrees/x-af36",
            "kind": "background",
            "startedAt": 1786107208148,
            "sessionId": "7f7d7627-e39a-4f31-a4a5-836e6f174d50",
            "name": "target-x-af36-agents-list",
            "status": "busy",
            "state": "working",
        },
        {
            "pid": 66350,
            "id": "6500bad9",
            "cwd": "/repo",
            "kind": "background",
            "startedAt": 1786107224171,
            "sessionId": "6500bad9-7fe4-4303-acfe-16270c12333c",
            "name": "king-x-b917-wave16",
            "status": "idle",
            "state": "working",
        },
        {
            "pid": 69787,
            "cwd": "/repo",
            "kind": "interactive",
            "startedAt": 1786107225801,
            "sessionId": "00e91b4c-04c0-41a4-8ab8-6183461a5994",
            "name": "regready-ccld-pipeline-50",
        },
    ]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {
        "7f7d7627": {"live_status": "Working"},
        "6500bad9": {"live_status": "Working"},
    }
    # busy/working mean the same thing, so normalizing before the comparison
    # keeps that row silent; idle/working genuinely disagree and still report.
    assert not any("7f7d7627" in w or "row 0" in w for w in warnings), warnings
    assert any("conflicting values across aliases" in w for w in warnings), warnings
    # The interactive row is not an agent: skipped silently, not warned about
    # and not crashed on.
    assert not any("short id" in w for w in warnings), warnings


def test_claude_agents_json_maps_every_observed_spelling(monkeypatch):
    """Both wire vocabularies land on the output set, and neither warns.

    `state` carries working/blocked/done; `status` carries busy/idle. All five
    were observed from a live binary in one snapshot. A value the map does not
    know still warns - that separation is what keeps the next rename loud.
    """
    payload = [
        {"id": "aaaaaaa1", "state": "working"},
        {"id": "aaaaaaa2", "state": "blocked"},
        {"id": "aaaaaaa3", "state": "done"},
        {"id": "aaaaaaa4", "status": "busy"},
        {"id": "aaaaaaa5", "status": "idle"},
    ]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert [r["live_status"] for r in result.values()] == [
        "Working",
        "Needs input",
        "Done",
        "Working",
        "Idle",
    ]
    assert warnings == [], warnings
    assert all(
        r["live_status"] in claude_mod.KNOWN_LIVE_STATUSES for r in result.values()
    )


def test_claude_agents_json_total_row_drop_warns_once_about_drift(monkeypatch):
    """A schema change dropping EVERY row shipped unnoticed once: 42 quiet
    per-row warnings and an empty map that reads exactly like "no agents
    running". A total drop is a different claim from a partial one and says so.
    """
    payload = [
        {"agentId": "907fc8c5", "kind": "background", "state": "working"}
        for _ in range(3)
    ]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {}
    assert any("0 of 3 agent rows parsed" in w for w in warnings), warnings


def test_claude_agents_json_all_interactive_rows_is_not_drift(monkeypatch):
    """Peer-review finding: an operator with no agents running is not drift.

    claude lists the operator's own interactive sessions in the same array and
    those carry no id by design, so counting them toward the total-drop claim
    cries schema drift at a perfectly healthy machine - the false alarm that
    teaches people to ignore the warning this exists to make loud.
    """
    payload = [
        {"pid": 69787, "kind": "interactive", "name": "repo-50"},
        {"pid": 75516, "kind": "interactive", "name": "repo-74"},
    ]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {}
    # Silent, not merely un-warned about drift: a per-row "no usable short id"
    # for each interactive session is the same false alarm one level down.
    assert warnings == [], warnings


def test_claude_agents_json_malformed_alias_does_not_mask_a_valid_one(monkeypatch):
    """Each alias is validated independently. A row carrying a junk `short_id`
    alongside a valid `id` must resolve to the `id` and survive; taking the
    first merely-PRESENT key would resolve to the junk and drop the row - the
    same silent-drop the aliasing exists to prevent."""
    payload = {
        "agents": [
            {"short_id": 12345, "id": "907fc8c5", "state": "working"},
            {"short_id": "", "id": "baf9409a", "status": None, "state": "blocked"},
        ]
    }

    def _fake(argv, **kwargs):
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {
        "907fc8c5": {"live_status": "Working"},
        "baf9409a": {"live_status": "Needs input"},
    }
    assert warnings == [], warnings


def test_claude_agents_json_conflicting_aliases_warn_rather_than_pick_silently(monkeypatch):
    """Two aliases both valid but DIFFERENT: there is no way to tell which the
    producer meant, so the pick is reported. Silently choosing one is the
    receipt-cannot-express-the-truth shape; the row still resolves so a
    conflict never costs the enrichment."""
    payload = {"agents": [{"short_id": "aaaaaaaa", "id": "bbbbbbbb", "state": "working"}]}

    def _fake(argv, **kwargs):
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {"aaaaaaaa": {"live_status": "Working"}}
    assert len(warnings) == 1, warnings
    assert "conflicting values across aliases" in warnings[0]
    assert "aaaaaaaa" in warnings[0] and "bbbbbbbb" in warnings[0]


def test_claude_agents_json_unhashable_status_does_not_raise(monkeypatch):
    """A status alias is accepted as any non-None JSON value, so a dict or list
    one reaches the conflict check and the vocabulary check. Comparing aliases
    through a set, or hashing one for the sentinel lookup, would raise TypeError
    out of a best-effort probe that read.py deliberately does not wrap, crashing
    `fno agents ls` on drifted output.

    Row 1 carries the dict alongside a valid ``state``, so the good value still
    wins; row 2 carries only the dict, so it is what reaches the output."""
    payload = {
        "agents": [
            {"id": "907fc8c5", "status": {"phase": "run"}, "state": "working"},
            {"id": "baf9409a", "status": {"phase": "run"}},
        ]
    }

    def _fake(argv, **kwargs):
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {
        "907fc8c5": {"live_status": "Working"},
        "baf9409a": {"live_status": {"phase": "run"}},
    }
    assert any("conflicting values across aliases" in w for w in warnings), warnings
    assert any("unrecognized status=" in w for w in warnings), warnings


def test_claude_agents_json_success_returns_short_id_map(monkeypatch):
    payload = {
        "agents": [
            {"short_id": "abc12345", "status": "Working"},
            {"short_id": "def67890", "status": "Idle"},
        ]
    }

    def _fake(argv, **kwargs):
        assert argv[:2] == ["claude", "agents"]
        assert "--json" in argv
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {
        "abc12345": {"live_status": "Working"},
        "def67890": {"live_status": "Idle"},
    }
    assert warnings == []


def test_claude_agents_json_timeout_returns_empty_with_warning(monkeypatch):
    def _fake(argv, **kwargs):  # noqa: ARG001
        raise subprocess.TimeoutExpired(cmd=argv, timeout=kwargs.get("timeout", 3.0))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json(timeout=3.0)

    assert result == {}
    assert any("timed out" in w.lower() for w in warnings)


def test_claude_agents_json_non_zero_exit_returns_empty_with_warning(monkeypatch):
    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(
            stdout="", stderr="claude: command not found", returncode=127
        )

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {}
    assert any("127" in w or "non-zero" in w.lower() for w in warnings)


def test_claude_agents_json_invalid_json_returns_empty_with_warning(monkeypatch):
    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout="not json at all", returncode=0)

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {}
    assert any("parse" in w.lower() or "json" in w.lower() for w in warnings)


def test_claude_agents_json_missing_binary_returns_empty_with_warning(monkeypatch):
    def _fake(argv, **kwargs):  # noqa: ARG001
        raise FileNotFoundError("claude")

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {}
    assert any("claude" in w.lower() for w in warnings)


def test_claude_agents_json_partial_record_omits_missing_short_id(monkeypatch):
    """AC3-FR — a record missing short_id is dropped with a WARN; others survive."""
    payload = {
        "agents": [
            {"short_id": "abc12345", "status": "Working"},
            {"status": "Idle"},  # no short_id
        ]
    }

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert "abc12345" in result
    assert len(result) == 1
    assert any("short_id" in w for w in warnings)


# --- logs(short_id, ...) ----------------------------------------------------


def test_logs_passes_through_stdout_for_claude(monkeypatch):
    def _fake(argv, **kwargs):
        assert argv[:2] == ["claude", "logs"]
        assert "abc12345" in argv
        return _fake_completed(stdout="line1\nline2\nline3\n")

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=False, stdout=stdout, stderr=stderr
    )

    assert exit_code == 0
    assert stdout.getvalue() == "line1\nline2\nline3\n"


def test_logs_tail_slices_last_n_lines(monkeypatch):
    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout="\n".join(f"line{i}" for i in range(1, 11)) + "\n")

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=3, follow=False, stdout=stdout, stderr=stderr
    )

    assert exit_code == 0
    lines = stdout.getvalue().splitlines()
    assert lines == ["line8", "line9", "line10"]


def test_logs_non_zero_exit_propagates(monkeypatch):
    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(
            stdout="", stderr="claude: id not found\n", returncode=17
        )

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="ghost", tail=None, follow=False, stdout=stdout, stderr=stderr
    )

    assert exit_code == 17
    assert "id not found" in stderr.getvalue()


def test_logs_missing_binary_returns_non_zero(monkeypatch):
    def _fake(argv, **kwargs):  # noqa: ARG001
        raise FileNotFoundError("claude")

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=False, stdout=stdout, stderr=stderr
    )

    assert exit_code != 0
    assert "claude" in stderr.getvalue().lower()


def test_logs_non_zero_with_empty_stderr_emits_fallback_diagnostic(monkeypatch):
    """Silent-failure-hunter finding — non-zero exit with empty stderr now surfaces a note."""
    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout="", stderr="", returncode=2)

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=False, stdout=stdout, stderr=stderr
    )

    assert exit_code == 2
    assert "exited 2" in stderr.getvalue().lower()


# --- claude_agents_json live_status sentinel validation --------------------


def test_claude_agents_json_unrecognized_live_status_warns_passes_through(monkeypatch):
    """Sentinel drift warning — unknown status surfaces a WARN, value passes through."""
    payload = {
        "agents": [
            {"short_id": "abc12345", "status": "Reflecting"},
        ]
    }

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {"abc12345": {"live_status": "Reflecting"}}
    assert any("Reflecting" in w for w in warnings)
    assert any("unrecognized" in w.lower() or "expected" in w.lower() for w in warnings)


def test_claude_agents_json_accepts_bare_array_shape(monkeypatch):
    """Codex P1 — claude can emit a bare JSON array; the dict-wrapper is optional."""
    payload = [
        {"short_id": "abc12345", "status": "Working"},
        {"short_id": "def67890", "status": "Idle"},
    ]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {
        "abc12345": {"live_status": "Working"},
        "def67890": {"live_status": "Idle"},
    }
    assert warnings == []


def test_claude_agents_json_unexpected_top_level_shape_warns_and_falls_back(monkeypatch):
    """A JSON scalar (string / int) is neither dict nor list → warned fallback."""
    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout='"unexpected"')

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {}
    assert any("unexpected shape" in w.lower() for w in warnings)


def test_claude_agents_json_null_live_status_does_not_warn(monkeypatch):
    """A null/absent status field is normal — no drift warning needed."""
    payload = {
        "agents": [
            {"short_id": "abc12345"},  # no status
        ]
    }

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    result, warnings = claude_mod.claude_agents_json()

    assert result == {"abc12345": {"live_status": None}}
    # No "unrecognized" warning for absent status.
    assert not any("unrecognized" in w.lower() for w in warnings)


# --- logs() streaming follow-mode tests (AC2-FR for claude path) -----------


class _FakePopenStreaming:
    """Minimal Popen-look-alike for streaming-mode tests.

    Constructor takes the lines the child should emit on stdout. The
    test scaffolding closes stdout once the lines are exhausted (returns
    "" from readline so the iter sentinel terminates) and surfaces the
    requested returncode from wait(). When the production code uses
    stderr=subprocess.STDOUT, the real Popen sets proc.stderr=None and
    interleaves child stderr into stdout; this fake mirrors that by
    default (stderr_text gets merged into the stdout stream).
    """

    def __init__(self, lines, returncode=0, stderr_text="", sigint_returncode=130):
        self._lines = list(lines)
        self._stderr_text = stderr_text
        self._returncode = returncode
        self._sigint_returncode = sigint_returncode
        # Merge stderr into stdout to mirror real subprocess.STDOUT behavior.
        merged = "".join(self._lines) + (self._stderr_text or "")
        self.stdout = io.StringIO(merged)
        self.stderr = None  # subprocess.STDOUT contract: stderr attr is None
        self.signals_received = []
        self.killed = False
        self._wait_calls = 0

    def wait(self, timeout=None):  # noqa: ARG002
        self._wait_calls += 1
        return self._returncode

    def send_signal(self, sig):
        self.signals_received.append(sig)
        # Mimic SIGINT making the child exit immediately.
        self._returncode = self._sigint_returncode

    def kill(self):
        self.killed = True
        self._returncode = -9


def test_logs_follow_streams_lines_in_order(monkeypatch):
    """AC2-HP variant — follow mode forwards each line as the child emits it."""
    fake = _FakePopenStreaming(lines=["one\n", "two\n", "three\n"])

    def _fake_popen(argv, **kwargs):
        assert argv[:2] == ["claude", "logs"]
        assert "--follow" in argv
        return fake

    monkeypatch.setattr(claude_mod, "_subprocess_popen", _fake_popen)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=True, stdout=stdout, stderr=stderr
    )

    assert exit_code == 0
    assert stdout.getvalue() == "one\ntwo\nthree\n"


def test_logs_follow_handles_keyboard_interrupt(monkeypatch):
    """AC2-FR — KeyboardInterrupt during follow → SIGINT forwarded, exit 0, no traceback."""
    class _SigintRaisingPopen(_FakePopenStreaming):
        def __init__(self):
            super().__init__(lines=[])
            self._readline_calls = 0

        @property
        def stdout(self):
            return self

        @stdout.setter
        def stdout(self, _):
            pass

        def readline(self):
            self._readline_calls += 1
            if self._readline_calls == 1:
                raise KeyboardInterrupt

        def close(self):
            pass

    fake = _SigintRaisingPopen()
    fake.stderr = io.StringIO()

    def _fake_popen(argv, **kwargs):
        return fake

    monkeypatch.setattr(claude_mod, "_subprocess_popen", _fake_popen)

    import signal as _signal

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=True, stdout=stdout, stderr=stderr
    )

    assert exit_code == 0
    assert _signal.SIGINT in fake.signals_received
    assert "traceback" not in stderr.getvalue().lower()


def test_logs_follow_missing_binary_returns_127(monkeypatch):
    def _fake_popen(argv, **kwargs):
        raise FileNotFoundError("claude")

    monkeypatch.setattr(claude_mod, "_subprocess_popen", _fake_popen)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=True, stdout=stdout, stderr=stderr
    )

    assert exit_code == 127
    assert "claude" in stderr.getvalue().lower()


def test_logs_follow_merges_stderr_into_stdout_stream(monkeypatch):
    """The Popen call uses stderr=subprocess.STDOUT to avoid pipe-buffer deadlock.

    Verify the kwarg is set and that stderr-flavored content in the
    merged stream lands on the operator's stdout (not stderr).
    """
    seen_kwargs = {}

    def _fake_popen(argv, **kwargs):
        seen_kwargs.update(kwargs)
        return _FakePopenStreaming(
            lines=["stdout-line\n"],
            stderr_text="warn: throttled\n",
        )

    monkeypatch.setattr(claude_mod, "_subprocess_popen", _fake_popen)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=True, stdout=stdout, stderr=stderr
    )

    assert exit_code == 0
    assert seen_kwargs.get("stderr") is subprocess.STDOUT
    # The merged stream is what claude's `--follow` emits in real life;
    # both ordinary lines and stderr-flavored text land on our stdout.
    assert "stdout-line" in stdout.getvalue()
    assert "warn: throttled" in stdout.getvalue()


def test_logs_follow_kills_child_that_ignores_sigint(monkeypatch):
    """SIGINT-then-TimeoutExpired → SIGKILL fallback, still exit 0."""

    class _StubbornChild(_FakePopenStreaming):
        def __init__(self):
            super().__init__(lines=[])

            class _BlockingStdout:
                def readline(_self):
                    raise KeyboardInterrupt

                def close(_self):
                    pass

            self.stdout = _BlockingStdout()

        def wait(self, timeout=None):
            self._wait_calls += 1
            if self._wait_calls == 1:
                # First wait() (after send_signal) → child ignores SIGINT.
                raise subprocess.TimeoutExpired(cmd="claude logs", timeout=timeout)
            # Second wait() (after kill) → child finally exits.
            return -9

    fake = _StubbornChild()

    def _fake_popen(argv, **kwargs):
        return fake

    monkeypatch.setattr(claude_mod, "_subprocess_popen", _fake_popen)

    import signal as _signal

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=True, stdout=stdout, stderr=stderr
    )

    assert exit_code == 0
    assert _signal.SIGINT in fake.signals_received
    assert fake.killed is True
    assert "traceback" not in stderr.getvalue().lower()


def test_logs_follow_process_lookup_during_cleanup_is_swallowed(monkeypatch):
    """If the child exits between KeyboardInterrupt and send_signal, no traceback."""

    class _AlreadyGone(_FakePopenStreaming):
        def __init__(self):
            super().__init__(lines=[])

            class _RaisingStdout:
                def readline(_self):
                    raise KeyboardInterrupt

                def close(_self):
                    pass

            self.stdout = _RaisingStdout()

        def send_signal(self, sig):
            self.signals_received.append(sig)
            raise ProcessLookupError("child already exited")

    fake = _AlreadyGone()

    def _fake_popen(argv, **kwargs):
        return fake

    monkeypatch.setattr(claude_mod, "_subprocess_popen", _fake_popen)

    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = claude_mod.logs(
        short_id="abc12345", tail=None, follow=True, stdout=stdout, stderr=stderr
    )

    assert exit_code == 0
    assert "traceback" not in stderr.getvalue().lower()
