"""Tests for the managed credential store (US1 register, US2 switch).

The slot backend (Keychain / credential file) is patched to a fake dict so the
orchestration - capture-before-overwrite, live-pin gate, verification+rollback,
atomic store writes - is exercised without touching the real Keychain/network.

Run: cd cli && uv run pytest src/fno/adapters/providers/test_managed.py -v
"""
from __future__ import annotations

import json
import stat
import subprocess

import pytest
from typer.testing import CliRunner

from fno.adapters.providers import managed
from fno.adapters.providers.cli import cli as providers_app
from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

runner = CliRunner()


def _blob(token: str) -> str:
    return json.dumps({"claudeAiOauth": {"accessToken": token}})


def _codex_blob(token: str) -> str:
    return json.dumps(
        {
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": token,
                "refresh_token": f"refresh-{token}",
                "id_token": "header.payload.signature",
            },
        }
    )


def _rec(id_: str, cli: str = "claude") -> ProviderRecord:
    return ProviderRecord(id=id_, name=id_, cli=cli, auth="managed")


@pytest.fixture()
def fake_slot(monkeypatch):
    """A fake credential slot: {cli: blob}. Patches the read/write seam and
    forces the live-pin gate clear by default."""
    slot: dict[str, str | None] = {}
    monkeypatch.setattr(managed, "_read_slot_blob", lambda cli, config_dir=None: slot.get(cli))
    monkeypatch.setattr(
        managed, "_write_slot_blob", lambda cli, blob, config_dir=None: slot.__setitem__(cli, blob)
    )
    monkeypatch.setattr(managed, "pinning_sessions", lambda config_dir=None: [])
    return slot


# ---------------------------------------------------------------------------
# US1: register / snapshot
# ---------------------------------------------------------------------------


class TestRegister:
    def test_snapshot_creates_store_with_private_modes(self, fake_slot, tmp_path):
        """AC1-HP: register captures the current login into a 700 dir / 600 blob."""
        fake_slot["claude"] = _blob("A0")
        adir = managed.snapshot_current(_rec("work-a"), root=tmp_path)
        assert adir == tmp_path / "work-a"
        assert stat.S_IMODE(adir.stat().st_mode) == 0o700
        blob_path = tmp_path / "work-a" / "blob"
        assert blob_path.read_text() == _blob("A0")
        assert stat.S_IMODE(blob_path.stat().st_mode) == 0o600
        meta = managed.read_meta("work-a", root=tmp_path)
        assert meta["cli"] == "claude" and meta["account_id"] == "work-a"

    def test_snapshot_refuses_when_no_login(self, fake_slot, tmp_path):
        """US1 boundary: never store an empty blob when there is no current login."""
        fake_slot.pop("claude", None)
        with pytest.raises(managed.ManagedStoreError):
            managed.snapshot_current(_rec("work-a"), root=tmp_path)

    def test_reregister_refreshes_snapshot(self, fake_slot, tmp_path):
        """US1: registering again refreshes the stored blob (idempotent)."""
        fake_slot["claude"] = _blob("A0")
        managed.snapshot_current(_rec("work-a"), root=tmp_path)
        fake_slot["claude"] = _blob("A1")
        managed.snapshot_current(_rec("work-a"), root=tmp_path)
        assert (tmp_path / "work-a" / "blob").read_text() == _blob("A1")


# ---------------------------------------------------------------------------
# US2: switch (materialize)
# ---------------------------------------------------------------------------


def _register_two(fake_slot, tmp_path):
    """work-a stored from A0, work-b stored from B0, slot left holding B (active)."""
    a, b = _rec("work-a"), _rec("work-b")
    fake_slot["claude"] = _blob("A0")
    managed.snapshot_current(a, root=tmp_path)
    managed._atomic_write_private(managed._active_stamp_path("claude", tmp_path), "work-a")
    fake_slot["claude"] = _blob("B0")
    managed.snapshot_current(b, root=tmp_path)
    managed._atomic_write_private(managed._active_stamp_path("claude", tmp_path), "work-b")
    return {"work-a": a, "work-b": b}


class TestSwitch:
    def test_materializes_and_verifies(self, fake_slot, tmp_path):
        """AC2-HP: use work-a materializes A's blob into the slot and verifies."""
        by_id = _register_two(fake_slot, tmp_path)
        result = managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert result.active == "work-a"
        assert fake_slot["claude"] == _blob("A0")
        assert managed.active_slot_id("claude", tmp_path) == "work-a"

    def test_capture_before_overwrite_saves_outgoing_rotated_token(self, fake_slot, tmp_path):
        """AC2-HP: switching away re-snapshots the outgoing account's CURRENT
        (rotated) slot token before the slot is overwritten."""
        by_id = _register_two(fake_slot, tmp_path)
        # B's token rotated in the slot since register (B0 -> B1).
        fake_slot["claude"] = _blob("B1")
        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        # work-b's store now holds B1, not the stale B0.
        assert (tmp_path / "work-b" / "blob").read_text() == _blob("B1")

    def test_round_trip_capture(self, fake_slot, tmp_path):
        """AC2-HP round-trip: use A then use B captures A's switch-away token."""
        by_id = _register_two(fake_slot, tmp_path)
        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)  # slot -> A0
        fake_slot["claude"] = _blob("A1")  # A rotates while active
        managed.switch(by_id["work-b"], by_id=by_id, root=tmp_path)
        assert (tmp_path / "work-a" / "blob").read_text() == _blob("A1")
        assert fake_slot["claude"] == _blob("B0")

    def test_already_active_is_noop(self, fake_slot, tmp_path):
        by_id = _register_two(fake_slot, tmp_path)  # active = work-b
        result = managed.switch(by_id["work-b"], by_id=by_id, root=tmp_path)
        assert result.active == "work-b"

    def test_stale_stamp_rematerializes_not_silent_noop(self, fake_slot, tmp_path):
        """codex P2: stamp names the target but the slot holds different creds
        (out-of-band /login) - re-materialize instead of a false no-op."""
        by_id = _register_two(fake_slot, tmp_path)  # stamp=work-b, slot=B0
        fake_slot["claude"] = _blob("SOMEONE_ELSE")  # slot changed out-of-band
        managed.switch(by_id["work-b"], by_id=by_id, root=tmp_path)
        assert fake_slot["claude"] == _blob("B0")  # re-materialized work-b's stored blob

    def test_emits_account_switched(self, fake_slot, tmp_path, monkeypatch):
        by_id = _register_two(fake_slot, tmp_path)
        events: list[tuple] = []
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: pytest.fail("Claude switch must not probe Codex"),
        )
        managed.switch(
            by_id["work-a"], by_id=by_id, root=tmp_path,
            emit_fn=lambda kind, **d: events.append((kind, d)),
        )
        assert events == [
            (
                "account_switched",
                {"provider": "work-a", "account_id": "work-a", "outgoing": "work-b"},
            )
        ]


class TestSwitchGuards:
    def test_live_pin_refuses_and_leaves_slot_untouched(self, fake_slot, tmp_path, monkeypatch):
        """AC1-ERR: a pinned slot defers, names the session, and mutates nothing."""
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(4242, "claude")],
        )
        before = fake_slot["claude"]
        stored_b = (tmp_path / "work-b" / "blob").read_text()
        with pytest.raises(managed.SwitchDeferred) as exc:
            managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert "4242" in str(exc.value)
        assert fake_slot["claude"] == before  # slot untouched
        assert (tmp_path / "work-b" / "blob").read_text() == stored_b  # store untouched

    def test_missing_snapshot_refuses(self, fake_slot, tmp_path):
        """Boundary: never materialize an account with no stored snapshot."""
        by_id = {"work-a": _rec("work-a")}
        managed._atomic_write_private(managed._active_stamp_path("claude", tmp_path), "work-b")
        with pytest.raises(managed.ManagedStoreError):
            managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)

    def test_failed_verify_rolls_back(self, fake_slot, tmp_path, monkeypatch):
        """AC3-ERR shape: a stale/revoked stored token fails verification and the
        slot rolls back to the captured outgoing blob."""
        by_id = _register_two(fake_slot, tmp_path)
        outgoing_blob = fake_slot["claude"]  # B0
        monkeypatch.setattr(managed, "verify_slot", lambda record, expected_blob: False)
        with pytest.raises(managed.ManagedStoreError):
            managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert fake_slot["claude"] == outgoing_blob  # rolled back to B
        assert managed.active_slot_id("claude", tmp_path) == "work-b"  # stamp not advanced

    def test_capture_keychain_error_aborts_without_overwrite(self, fake_slot, tmp_path, monkeypatch):
        """A Keychain read failure during capture-before-overwrite must ABORT the
        switch (not be swallowed), so the outgoing account's token is never lost."""
        by_id = _register_two(fake_slot, tmp_path)
        before = fake_slot["claude"]  # B0, still in the slot

        def _boom(record, root=None):
            raise managed.KeychainError("security find-generic-password timed out")

        monkeypatch.setattr(managed, "snapshot_current", _boom)
        with pytest.raises(managed.KeychainError):
            managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert fake_slot["claude"] == before  # slot never overwritten

    def test_rollback_failure_reported_truthfully(self, fake_slot, tmp_path, monkeypatch):
        """When verify fails AND the rollback write also fails, the receipt says
        the slot is indeterminate - it never lies 'rolled back'."""
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(managed, "verify_slot", lambda record, expected_blob: False)
        calls = {"n": 0}

        def _write(cli, blob, config_dir=None):
            calls["n"] += 1
            if calls["n"] >= 2:  # the rollback write
                raise managed.KeychainError("rollback write denied")
            fake_slot[cli] = blob

        monkeypatch.setattr(managed, "_write_slot_blob", _write)
        with pytest.raises(managed.ManagedStoreError) as exc:
            managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert "indeterminate" in str(exc.value)


# ---------------------------------------------------------------------------
# Model: managed auth strategy takes neither credentials_source nor env
# ---------------------------------------------------------------------------


class TestManagedRecordValidation:
    def test_managed_rejects_credentials_source(self):
        from pathlib import Path

        with pytest.raises(ValueError, match="auth=managed"):
            ProviderRecord(
                id="bad", name="bad", cli="claude", auth="managed",
                credentials_source=Path("/tmp/x"),
            )

    def test_managed_rejects_env(self):
        with pytest.raises(ValueError, match="auth=managed"):
            ProviderRecord(
                id="bad", name="bad", cli="claude", auth="managed",
                env={"ANTHROPIC_API_KEY": "x"},
            )

    def test_managed_bare_record_ok(self):
        rec = ProviderRecord(id="ok", name="ok", cli="claude", auth="managed")
        assert rec.auth == "managed" and rec.account_id == "ok"


# ---------------------------------------------------------------------------
# AC2-ERR: Keychain denial / timeout surfaces a receipt, never a hang
# ---------------------------------------------------------------------------


class TestKeychainErrors:
    def test_security_timeout_raises_receipt(self, monkeypatch):
        def _boom(*a, **k):
            raise subprocess.TimeoutExpired(cmd="security", timeout=5)

        monkeypatch.setattr(managed.subprocess, "run", _boom)
        with pytest.raises(managed.KeychainError):
            managed._run_security(["find-generic-password"])

    def test_security_oserror_raises_receipt(self, monkeypatch):
        monkeypatch.setattr(
            managed.subprocess, "run", lambda *a, **k: (_ for _ in ()).throw(OSError("nope"))
        )
        with pytest.raises(managed.KeychainError):
            managed._run_security(["add-generic-password"])


# ---------------------------------------------------------------------------
# AC1-FR: atomic store write leaves no partial on failure
# ---------------------------------------------------------------------------


class TestAtomicWrite:
    def test_no_partial_on_write_error(self, tmp_path, monkeypatch):
        target = tmp_path / "blob"
        target.write_text("original")

        def _boom(*a, **k):
            raise RuntimeError("disk full mid-write")

        monkeypatch.setattr(managed.os, "replace", _boom)
        with pytest.raises(RuntimeError):
            managed._atomic_write_private(target, "new-secret")
        assert target.read_text() == "original"  # untouched
        # No leftover temp files.
        assert list(tmp_path.glob(".blob.*.tmp")) == []

    def test_fchmod_failure_cleans_up_temp(self, tmp_path, monkeypatch):
        """A fchmod failure before fdopen takes the fd must still clean the temp
        (and not leak the fd - covered by closing it in the except)."""
        monkeypatch.setattr(
            managed.os, "fchmod", lambda *a, **k: (_ for _ in ()).throw(OSError("denied"))
        )
        with pytest.raises(OSError):
            managed._atomic_write_private(tmp_path / "blob", "secret")
        assert list(tmp_path.glob(".blob.*.tmp")) == []


class TestLooksLikeClaude:
    def test_whitespace_only_cmdline_part_no_indexerror(self):
        # A whitespace-only arg used to crash on part.split()[0]; now safe.
        assert managed._looks_like_claude(None, ["   ", ""]) is False

    def test_matches_claude_binary(self):
        assert managed._looks_like_claude("claude", []) is True
        assert managed._looks_like_claude(None, ["/opt/homebrew/bin/claude --resume"]) is True


# ---------------------------------------------------------------------------
# Real file-backed slot (codex auth.json) - exercises the un-mocked backend
# ---------------------------------------------------------------------------


def _register_codex_pair(tmp_path, monkeypatch):
    auth = tmp_path / ".codex" / "auth.json"
    monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
    monkeypatch.setattr(managed, "codex_pinning_sessions", lambda auth_path=None: [])
    a, b = _rec("cx-a", cli="codex"), _rec("cx-b", cli="codex")
    managed._write_slot_blob("codex", _codex_blob("A0"))
    managed.snapshot_current(a, root=tmp_path)
    managed._atomic_write_private(managed._active_stamp_path("codex", tmp_path), "cx-a")
    managed._write_slot_blob("codex", _codex_blob("B0"))
    managed.snapshot_current(b, root=tmp_path)
    managed._atomic_write_private(managed._active_stamp_path("codex", tmp_path), "cx-b")
    return a, b


class TestCodexFileBackend:
    @pytest.mark.parametrize(
        "blob",
        [
            json.dumps({"OPENAI_API_KEY": "sk-test"}),
            _codex_blob("token"),
            json.dumps({"personal_access_token": "pat"}),
            json.dumps({"auth_mode": "agentIdentity", "agent_identity": "header.payload.sig"}),
            json.dumps(
                {
                    "auth_mode": "agentIdentity",
                    "agent_identity": {
                        "agent_runtime_id": "runtime",
                        "agent_private_key": "private",
                        "account_id": "account",
                        "chatgpt_user_id": "user",
                        "plan_type": "pro",
                        "chatgpt_account_is_fedramp": False,
                    }
                }
            ),
            json.dumps({"bedrock_api_key": {"api_key": "key", "region": "us-east-1"}}),
            json.dumps(
                {
                    "auth_mode": "chatgptAuthTokens",
                    "tokens": {
                        "access_token": "access",
                        "refresh_token": "",
                        "id_token": "header.payload.signature",
                    },
                }
            ),
            json.dumps(
                {
                    "OPENAI_API_KEY": None,
                    "tokens": json.loads(_codex_blob("legacy"))["tokens"],
                }
            ),
            json.dumps(
                {
                    "personal_access_token": None,
                    "bedrock_api_key": {"api_key": "key", "region": "us-east-1"},
                }
            ),
            json.dumps(
                {
                    "personal_access_token": None,
                    "bedrock_api_key": None,
                    "OPENAI_API_KEY": "sk-fallback",
                }
            ),
        ],
    )
    def test_codex_auth_requires_supported_credential_material(self, blob):
        assert managed._codex_auth_present(blob) is True

    @pytest.mark.parametrize(
        "blob",
        [
            "not-json",
            "[]",
            "{}",
            json.dumps({"foo": "bar"}),
            json.dumps({"OPENAI_API_KEY": " "}),
            json.dumps({"tokens": {"access_token": "only"}}),
            json.dumps({"agent_identity": "header.payload.signature"}),
            json.dumps({"agent_identity": {"agent_runtime_id": "only"}}),
            json.dumps({"bedrock_api_key": {"api_key": "only"}}),
            json.dumps(
                {
                    "auth_mode": "chatgpt",
                    "OPENAI_API_KEY": "sk-inactive",
                    "tokens": None,
                }
            ),
            json.dumps({"personal_access_token": "", "OPENAI_API_KEY": "sk-inactive"}),
            json.dumps({"auth_mode": "headers", "tokens": json.loads(_codex_blob("token"))["tokens"]}),
            json.dumps({"auth_mode": "unknown", "OPENAI_API_KEY": "sk-inactive"}),
        ],
    )
    def test_codex_auth_rejects_malformed_or_tokenless_blobs(self, blob):
        assert managed._codex_auth_present(blob) is False

    def test_codex_login_status_uses_slot_home_and_exit_code(self, tmp_path, monkeypatch):
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        for name in managed._CODEX_AUTH_ENV_VARS:
            monkeypatch.setenv(name, "ambient-credential")
        calls = []

        def _run(argv, **kwargs):
            calls.append((argv, kwargs))
            return subprocess.CompletedProcess(argv, 0, stdout="Logged in", stderr="")

        monkeypatch.setattr(managed.subprocess, "run", _run)
        result = managed._codex_login_ok()

        assert result.ok is True and result.reason is None
        assert calls[0][0] == ["codex", "login", "status"]
        assert calls[0][1]["env"]["CODEX_HOME"] == str(auth.parent)
        assert all(name not in calls[0][1]["env"] for name in managed._CODEX_AUTH_ENV_VARS)
        assert calls[0][1]["timeout"] == 5

        monkeypatch.setattr(
            managed.subprocess,
            "run",
            lambda *args, **kwargs: subprocess.CompletedProcess(args[0], 1),
        )
        assert managed._codex_login_ok().ok is False

    @pytest.mark.parametrize(
        ("error", "reason"),
        [
            (FileNotFoundError("codex"), "codex-login-status-missing"),
            (
                subprocess.TimeoutExpired(cmd=["codex", "login", "status"], timeout=5),
                "codex-login-status-timeout",
            ),
        ],
    )
    def test_codex_login_status_unavailable_degrades(self, monkeypatch, error, reason):
        def _raise(*args, **kwargs):
            raise error

        monkeypatch.setattr(managed.subprocess, "run", _raise)
        result = managed._codex_login_ok()
        assert result.ok is None and result.reason == reason

    def test_file_slot_round_trip(self, tmp_path, monkeypatch):
        """The codex file backend reads/writes auth.json (0600) via the real
        _read_slot_blob/_write_slot_blob path (not the fake-slot seam)."""
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        assert managed._read_slot_blob("codex") is None  # no login yet
        managed._write_slot_blob("codex", _codex_blob("cx"))
        assert managed._read_slot_blob("codex") == _codex_blob("cx")
        assert stat.S_IMODE(auth.stat().st_mode) == 0o600

    def test_codex_switch_captures_and_materializes(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        # switch to A: captures B's current slot, materializes A0, verifies.
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=True),
        )
        result = managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert managed._read_slot_blob("codex") == _codex_blob("A0")
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")
        assert result.slot_changed is True
        assert result.verification == "codex-recognized"
        assert result.reason is None

    def test_codex_switch_discloses_structural_fallback(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(
                ok=None,
                reason="codex-login-status-missing",
            ),
        )

        events = []
        result = managed.switch(
            a,
            by_id={"cx-a": a, "cx-b": b},
            root=tmp_path,
            emit_fn=lambda kind, **data: events.append((kind, data)),
        )

        assert result.slot_changed is True
        assert result.verification == "structural"
        assert result.reason == "codex-login-status-missing"
        assert events[0][1]["reason"] == "codex-login-status-missing"

    def test_codex_login_rejection_rolls_back_without_event(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=False),
        )
        events = []

        with pytest.raises(managed.ManagedStoreError, match="not recognized"):
            managed.switch(
                a,
                by_id={"cx-a": a, "cx-b": b},
                root=tmp_path,
                emit_fn=lambda kind, **data: events.append((kind, data)),
            )

        assert managed._read_slot_blob("codex") == _codex_blob("B0")
        assert managed.active_slot_id("codex", tmp_path) == "cx-b"
        assert events == []

    def test_tokenless_codex_snapshot_rolls_back_before_native_probe(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        (tmp_path / "cx-a" / "blob").write_text(json.dumps({"foo": "bar"}))
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: pytest.fail("tokenless auth must fail structural verification"),
        )

        with pytest.raises(managed.ManagedStoreError, match="failed verification"):
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)

        assert managed._read_slot_blob("codex") == _codex_blob("B0")
        assert managed.active_slot_id("codex", tmp_path) == "cx-b"

    def test_codex_login_rejection_without_rollback_blob_reports_slot_state(
        self, tmp_path, monkeypatch
    ):
        auth = tmp_path / ".codex" / "auth.json"
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        auth.unlink()
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=False),
        )
        events = []

        with pytest.raises(managed.ManagedStoreError, match="nothing to roll back"):
            managed.switch(
                a,
                by_id={"cx-a": a, "cx-b": b},
                root=tmp_path,
                emit_fn=lambda kind, **data: events.append((kind, data)),
            )

        assert managed._read_slot_blob("codex") == _codex_blob("A0")
        assert managed.active_slot_id("codex", tmp_path) is None
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")
        assert events == []

        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=True),
        )
        managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert managed.active_slot_id("codex", tmp_path) == "cx-a"
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

    def test_codex_structural_failure_with_failed_rollback_clears_stamp(
        self, tmp_path, monkeypatch
    ):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        original_verify = managed.verify_slot
        original_write = managed._write_slot_blob
        calls = {"count": 0}

        monkeypatch.setattr(managed, "verify_slot", lambda record, expected_blob: False)

        def _write(cli, blob, config_dir=None):
            calls["count"] += 1
            if calls["count"] == 2:
                raise managed.ManagedStoreError("rollback denied")
            return original_write(cli, blob, config_dir)

        monkeypatch.setattr(managed, "_write_slot_blob", _write)
        with pytest.raises(managed.ManagedStoreError, match="indeterminate"):
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)

        assert managed.active_slot_id("codex", tmp_path) is None
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

        monkeypatch.setattr(managed, "verify_slot", original_verify)
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=True),
        )
        managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert managed.active_slot_id("codex", tmp_path) == "cx-a"
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

    def test_codex_hard_probe_error_rolls_back(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)

        def _raise():
            raise managed.ManagedStoreError("permission denied")

        monkeypatch.setattr(managed, "_codex_login_ok", _raise)
        with pytest.raises(managed.ManagedStoreError, match="permission denied"):
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert managed._read_slot_blob("codex") == _codex_blob("B0")
        assert managed.active_slot_id("codex", tmp_path) == "cx-b"

    def test_codex_probe_interrupt_rolls_back_then_reraises(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)

        def _interrupt():
            raise KeyboardInterrupt

        monkeypatch.setattr(managed, "_codex_login_ok", _interrupt)
        with pytest.raises(KeyboardInterrupt) as caught:
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert caught.value.__notes__ == [
            "codex login verification interrupted; slot rolled back to the previous account"
        ]
        assert managed._read_slot_blob("codex") == _codex_blob("B0")
        assert managed.active_slot_id("codex", tmp_path) == "cx-b"

    def test_codex_probe_interrupt_reraises_even_when_rollback_fails(
        self, tmp_path, monkeypatch
    ):
        # A KeyboardInterrupt must propagate as itself even if best-effort
        # rollback fails - never downgrade a BaseException to a caught Exception.
        a, b = _register_codex_pair(tmp_path, monkeypatch)

        def _interrupt():
            raise KeyboardInterrupt

        monkeypatch.setattr(managed, "_codex_login_ok", _interrupt)
        original_write = managed._write_slot_blob
        calls = {"count": 0}

        def _write(cli, blob, config_dir=None):
            calls["count"] += 1
            if calls["count"] == 2:
                raise managed.ManagedStoreError("rollback denied")
            return original_write(cli, blob, config_dir)

        monkeypatch.setattr(managed, "_write_slot_blob", _write)
        with pytest.raises(KeyboardInterrupt) as caught:
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert "rollback ALSO failed (rollback denied)" in caught.value.__notes__[0]
        assert "active stamp cleared" in caught.value.__notes__[0]
        assert managed.active_slot_id("codex", tmp_path) is None
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

    def test_codex_rejection_reports_rollback_failure(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=False),
        )
        original_write = managed._write_slot_blob
        calls = {"count": 0}

        def _write(cli, blob, config_dir=None):
            calls["count"] += 1
            if calls["count"] == 2:
                raise managed.ManagedStoreError("rollback denied")
            return original_write(cli, blob, config_dir)

        monkeypatch.setattr(managed, "_write_slot_blob", _write)
        with pytest.raises(managed.ManagedStoreError, match="indeterminate"):
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert managed.active_slot_id("codex", tmp_path) is None
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=True),
        )
        managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert managed.active_slot_id("codex", tmp_path) == "cx-a"
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

    def test_codex_switch_event_records_verification_strength(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=True),
        )
        events = []

        managed.switch(
            a,
            by_id={"cx-a": a, "cx-b": b},
            root=tmp_path,
            emit_fn=lambda kind, **data: events.append((kind, data)),
        )

        assert events == [
            (
                "account_switched",
                {
                    "provider": "cx-a",
                    "account_id": "cx-a",
                    "outgoing": "cx-b",
                    "slot_changed": True,
                    "verification": "codex-recognized",
                },
            )
        ]

    def test_codex_already_active_is_probe_free(self, tmp_path, monkeypatch):
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        target = _rec("cx-a", cli="codex")
        managed._write_slot_blob("codex", _codex_blob("A0"))
        managed.snapshot_current(target, root=tmp_path)
        managed._atomic_write_private(managed._active_stamp_path("codex", tmp_path), "cx-a")
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: pytest.fail("already-active switch must not probe codex"),
        )

        result = managed.switch(target, by_id={"cx-a": target}, root=tmp_path)

        assert result.slot_changed is False
        assert result.verification == "structural"
        assert result.reason == "slot-already-active"

    def test_codex_switch_pin_defers(self, tmp_path, monkeypatch):
        """US6 Invariant: a live codex session pinning the slot defers the switch,
        names the session, and mutates nothing - claude parity for codex."""
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        a, b = _rec("cx-a", cli="codex"), _rec("cx-b", cli="codex")
        managed._write_slot_blob("codex", _codex_blob("A0"))
        managed.snapshot_current(a, root=tmp_path)
        managed._atomic_write_private(managed._active_stamp_path("codex", tmp_path), "cx-a")
        managed._write_slot_blob("codex", _codex_blob("B0"))
        managed.snapshot_current(b, root=tmp_path)
        managed._atomic_write_private(managed._active_stamp_path("codex", tmp_path), "cx-b")
        monkeypatch.setattr(
            managed, "codex_pinning_sessions",
            lambda auth_path=None: [managed.PinningSession(555, "codex exec")],
        )
        before = managed._read_slot_blob("codex")
        with pytest.raises(managed.SwitchDeferred) as exc:
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert "555" in str(exc.value) and "codex" in str(exc.value)
        assert managed._read_slot_blob("codex") == before  # slot untouched
        assert (tmp_path / "cx-a" / "blob").read_text() == _codex_blob("A0")  # store untouched

    def test_codex_session_launched_mid_switch_rolls_back(self, tmp_path, monkeypatch):
        """TOCTOU narrowing (cv-f578cbe7): the pre-write pin check is clear, but a
        codex session appears during the write. The post-write re-scan catches it,
        rolls the slot back to the outgoing creds, and defers - never leaving
        auth.json rewritten under the session that started mid-switch."""
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        a, b = _rec("cx-a", cli="codex"), _rec("cx-b", cli="codex")
        managed._write_slot_blob("codex", _codex_blob("A0"))
        managed.snapshot_current(a, root=tmp_path)
        managed._atomic_write_private(managed._active_stamp_path("codex", tmp_path), "cx-a")
        managed._write_slot_blob("codex", _codex_blob("B0"))  # slot holds outgoing B
        managed.snapshot_current(b, root=tmp_path)
        managed._atomic_write_private(managed._active_stamp_path("codex", tmp_path), "cx-b")
        # First scan (pre-write) clear; second scan (immediately post-write) finds
        # a session before the native probe can widen the rollback race.
        calls = {"n": 0}

        def _scan(auth_path=None):
            calls["n"] += 1
            return [] if calls["n"] == 1 else [managed.PinningSession(777, "codex")]

        monkeypatch.setattr(managed, "codex_pinning_sessions", _scan)
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: pytest.fail("late pin must defer before probing codex"),
        )
        with pytest.raises(managed.SwitchDeferred) as exc:
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)
        assert "777" in str(exc.value) and "during the switch" in str(exc.value)
        assert managed._read_slot_blob("codex") == _codex_blob("B0")  # rolled back to outgoing
        assert managed.active_slot_id("codex", tmp_path) == "cx-b"  # stamp not advanced
        assert calls["n"] == 2  # both scans ran

    def test_codex_late_pin_without_rollback_blob_clears_stamp(self, tmp_path, monkeypatch):
        auth = tmp_path / ".codex" / "auth.json"
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        auth.unlink()
        calls = {"count": 0}

        def _scan(auth_path=None):
            calls["count"] += 1
            return [] if calls["count"] == 1 else [managed.PinningSession(891, "codex")]

        monkeypatch.setattr(managed, "codex_pinning_sessions", _scan)
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: pytest.fail("late pin must defer before probing codex"),
        )

        with pytest.raises(managed.SwitchDeferred, match="active stamp cleared"):
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)

        assert managed._read_slot_blob("codex") == _codex_blob("A0")
        assert managed.active_slot_id("codex", tmp_path) is None
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

    def test_codex_session_started_during_successful_probe_keeps_target(self, tmp_path, monkeypatch):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        pin_active = {"value": False}
        scans = {"count": 0}

        def _scan(auth_path=None):
            scans["count"] += 1
            if pin_active["value"]:
                return [managed.PinningSession(888, "codex")]
            return []

        def _probe():
            pin_active["value"] = True
            return managed._CodexLoginResult(ok=True)

        monkeypatch.setattr(managed, "codex_pinning_sessions", _scan)
        monkeypatch.setattr(managed, "_codex_login_ok", _probe)

        result = managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)

        assert result.verification == "codex-recognized"
        assert managed._read_slot_blob("codex") == _codex_blob("A0")
        assert managed.active_slot_id("codex", tmp_path) == "cx-a"
        assert scans["count"] == 2

    def test_codex_rejection_with_pin_during_probe_withholds_rollback(
        self, tmp_path, monkeypatch
    ):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        pin_active = {"value": False}

        def _scan(auth_path=None):
            if pin_active["value"]:
                return [managed.PinningSession(889, "codex")]
            return []

        def _probe():
            pin_active["value"] = True
            return managed._CodexLoginResult(ok=False)

        monkeypatch.setattr(managed, "codex_pinning_sessions", _scan)
        monkeypatch.setattr(managed, "_codex_login_ok", _probe)

        with pytest.raises(managed.ManagedStoreError, match="rollback withheld.*pid 889"):
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)

        assert managed._read_slot_blob("codex") == _codex_blob("A0")
        assert managed.active_slot_id("codex", tmp_path) is None
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

        pin_active["value"] = False
        monkeypatch.setattr(
            managed,
            "_codex_login_ok",
            lambda: managed._CodexLoginResult(ok=True),
        )
        result = managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)

        assert result.verification == "codex-recognized"
        assert managed._read_slot_blob("codex") == _codex_blob("A0")
        assert managed.active_slot_id("codex", tmp_path) == "cx-a"
        assert (tmp_path / "cx-b" / "blob").read_text() == _codex_blob("B0")

    def test_codex_interrupt_with_pin_during_probe_withholds_rollback(
        self, tmp_path, monkeypatch
    ):
        a, b = _register_codex_pair(tmp_path, monkeypatch)
        pin_active = {"value": False}

        def _scan(auth_path=None):
            if pin_active["value"]:
                return [managed.PinningSession(890, "codex")]
            return []

        def _probe():
            pin_active["value"] = True
            raise KeyboardInterrupt

        monkeypatch.setattr(managed, "codex_pinning_sessions", _scan)
        monkeypatch.setattr(managed, "_codex_login_ok", _probe)

        with pytest.raises(KeyboardInterrupt) as caught:
            managed.switch(a, by_id={"cx-a": a, "cx-b": b}, root=tmp_path)

        assert "rollback withheld" in caught.value.__notes__[0]
        assert "pid 890" in caught.value.__notes__[0]
        assert managed._read_slot_blob("codex") == _codex_blob("A0")
        assert managed.active_slot_id("codex", tmp_path) is None

    def test_claude_switch_has_single_pin_check(self, fake_slot, tmp_path, monkeypatch):
        """The post-write re-scan is codex-only: claude keeps G1's single pre-write
        check (byte-for-byte), so a clean claude switch scans exactly once."""
        by_id = _register_two(fake_slot, tmp_path)
        calls = {"n": 0}

        def _scan(config_dir=None):
            calls["n"] += 1
            return []

        monkeypatch.setattr(managed, "pinning_sessions", _scan)
        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert calls["n"] == 1  # claude scanned once, not twice


class _FakeProc:
    """A psutil-proc stand-in for the pin matcher's process scan."""

    def __init__(self, pid, name, cmdline, environ=None, environ_raises=False):
        self.info = {"pid": pid, "name": name, "cmdline": cmdline}
        self._environ = environ or {}
        self._environ_raises = environ_raises

    def environ(self):
        if self._environ_raises:
            raise PermissionError("denied")
        return self._environ


class TestLooksLikeCodex:
    def test_matches_codex_binary(self):
        assert managed._looks_like_codex("codex", []) is True
        assert managed._looks_like_codex(None, ["/opt/homebrew/bin/codex exec"]) is True

    def test_non_codex_is_false(self):
        assert managed._looks_like_codex("claude", []) is False
        assert managed._looks_like_codex(None, ["   ", ""]) is False

    def test_codex_as_later_arg_does_not_match(self):
        # 'codex' in a non-argv[0] position (grep target, commit message) must
        # NOT match - else a random command spuriously defers a switch.
        assert managed._looks_like_codex(None, ["grep", "codex"]) is False
        assert managed._looks_like_codex(None, ["git", "commit", "-m", "codex fix"]) is False
        assert managed._looks_like_codex(None, ["nano", "codex.json"]) is False

    def test_matches_argv0_joined_or_split(self):
        assert managed._looks_like_codex(None, ["/opt/homebrew/bin/codex", "exec"]) is True
        assert managed._looks_like_codex(None, ["/opt/homebrew/bin/codex exec"]) is True


class TestCodexPinningSessions:
    def _iter(self, monkeypatch, proc):
        monkeypatch.setattr(managed.psutil, "process_iter", lambda attrs=None: iter([proc]))

    def test_codex_home_at_slot_pins(self, tmp_path, monkeypatch):
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        self._iter(monkeypatch, _FakeProc(
            4242, "codex", ["codex", "exec"], environ={"CODEX_HOME": str(tmp_path / ".codex")}
        ))
        assert [p.pid for p in managed.codex_pinning_sessions()] == [4242]

    def test_codex_home_elsewhere_does_not_pin(self, tmp_path, monkeypatch):
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        self._iter(monkeypatch, _FakeProc(
            1, "codex", ["codex"], environ={"CODEX_HOME": str(tmp_path / "other")}
        ))
        assert managed.codex_pinning_sessions() == []

    def test_unreadable_env_is_conservative_pin(self, tmp_path, monkeypatch):
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        self._iter(monkeypatch, _FakeProc(7, "codex", ["codex"], environ_raises=True))
        assert [p.pid for p in managed.codex_pinning_sessions()] == [7]

    def test_non_codex_process_ignored(self, tmp_path, monkeypatch):
        auth = tmp_path / ".codex" / "auth.json"
        monkeypatch.setattr(managed, "_codex_slot_auth_path", lambda: auth)
        self._iter(monkeypatch, _FakeProc(
            9, "claude", ["claude"], environ={"CODEX_HOME": str(tmp_path / ".codex")}
        ))
        assert managed.codex_pinning_sessions() == []


class TestPinningSessionsFor:
    def test_dispatches_claude_and_codex(self, monkeypatch):
        monkeypatch.setattr(managed, "pinning_sessions", lambda config_dir=None: ["C"])
        monkeypatch.setattr(managed, "codex_pinning_sessions", lambda auth_path=None: ["X"])
        assert managed.pinning_sessions_for("claude") == ["C"]
        assert managed.pinning_sessions_for("codex") == ["X"]

    def test_unsupported_cli_refuses_before_mutation(self):
        # A cli with no managed matcher must fail loud with a receipt, not fall
        # back to the claude scan (which would let the switch corrupt the claude
        # slot via the downstream slot ops).
        with pytest.raises(managed.ManagedStoreError, match="not supported for cli 'gemini'"):
            managed.pinning_sessions_for("gemini")


# ---------------------------------------------------------------------------
# CLI surface (register / use / list)
# ---------------------------------------------------------------------------


def _cli_slot(monkeypatch):
    slot: dict[str, str | None] = {}
    monkeypatch.setattr(managed, "_read_slot_blob", lambda cli, config_dir=None: slot.get(cli))
    monkeypatch.setattr(
        managed, "_write_slot_blob", lambda cli, blob, config_dir=None: slot.__setitem__(cli, blob)
    )
    monkeypatch.setattr(managed, "pinning_sessions", lambda config_dir=None: [])
    return slot


class TestCliSurface:
    def _invoke_codex_use(self, monkeypatch, result):
        config = ProvidersConfig(records=[_rec("cx-a", cli="codex")], active="cx-a")
        monkeypatch.setattr(
            "fno.adapters.providers.cli.load_providers",
            lambda repo_root=None: config,
        )
        monkeypatch.setattr(
            "fno.adapters.providers.cli.save_providers",
            lambda *args, **kwargs: None,
        )
        monkeypatch.setattr(managed, "switch", lambda *args, **kwargs: result)
        return runner.invoke(providers_app, ["use", "cx-a"], catch_exceptions=False)

    def test_register_then_list_marks_active(self, tmp_path, monkeypatch):
        slot = _cli_slot(monkeypatch)
        env = {"HOME": str(tmp_path), "PWD": str(tmp_path)}
        slot["claude"] = _blob("A0")
        r1 = runner.invoke(providers_app, ["register", "work-a"], env=env, catch_exceptions=False)
        assert r1.exit_code == 0, r1.output
        slot["claude"] = _blob("B0")
        r2 = runner.invoke(providers_app, ["register", "work-b"], env=env, catch_exceptions=False)
        assert r2.exit_code == 0, r2.output
        rl = runner.invoke(providers_app, ["list"], env=env, catch_exceptions=False)
        assert rl.exit_code == 0
        active = [ln for ln in rl.output.splitlines() if "work-b" in ln]
        assert active and active[0].lstrip().startswith("*")
        assert "snapshot=" in active[0]

    def test_register_no_login_errors(self, tmp_path, monkeypatch):
        _cli_slot(monkeypatch)  # slot empty
        env = {"HOME": str(tmp_path), "PWD": str(tmp_path)}
        r = runner.invoke(providers_app, ["register", "work-a"], env=env, catch_exceptions=False)
        assert r.exit_code == 1
        assert "no current" in r.output

    def test_use_managed_materializes(self, tmp_path, monkeypatch):
        slot = _cli_slot(monkeypatch)
        env = {"HOME": str(tmp_path), "PWD": str(tmp_path)}
        slot["claude"] = _blob("A0")
        runner.invoke(providers_app, ["register", "work-a"], env=env, catch_exceptions=False)
        slot["claude"] = _blob("B0")
        runner.invoke(providers_app, ["register", "work-b"], env=env, catch_exceptions=False)
        r = runner.invoke(providers_app, ["use", "work-a"], env=env, catch_exceptions=False)
        assert r.exit_code == 0, r.output
        assert slot["claude"] == _blob("A0")
        assert "Materialized managed account 'work-a' into the slot (verified)." in r.output

    def test_use_codex_reports_native_verification(self, monkeypatch):
        result = managed.SwitchResult(
            active="cx-a",
            slot_changed=True,
            verification="codex-recognized",
        )
        response = self._invoke_codex_use(monkeypatch, result)
        assert response.exit_code == 0
        assert "Codex recognized" in response.output

    def test_use_codex_reports_structural_fallback(self, monkeypatch):
        result = managed.SwitchResult(
            active="cx-a",
            slot_changed=True,
            verification="structural",
            reason="codex-login-status-timeout",
        )
        response = self._invoke_codex_use(monkeypatch, result)
        assert response.exit_code == 0
        assert "structural fallback" in response.output
        assert "codex-login-status-timeout" in response.output

    def test_use_codex_reports_already_active_noop(self, monkeypatch):
        result = managed.SwitchResult(
            active="cx-a",
            slot_changed=False,
            verification="structural",
            reason="slot-already-active",
        )
        response = self._invoke_codex_use(monkeypatch, result)
        assert response.exit_code == 0
        assert "already materialized" in response.output
        assert "slot-already-active" in response.output

    def test_use_codex_interrupt_surfaces_rollback_receipt(self, monkeypatch):
        config = ProvidersConfig(records=[_rec("cx-a", cli="codex")], active="cx-a")
        monkeypatch.setattr(
            "fno.adapters.providers.cli.load_providers",
            lambda repo_root=None: config,
        )

        def _interrupt(*args, **kwargs):
            exc = KeyboardInterrupt()
            exc.add_note(
                "codex login verification interrupted; rollback ALSO failed "
                "(rollback denied); slot is in an indeterminate state"
            )
            raise exc

        monkeypatch.setattr(managed, "switch", _interrupt)
        response = runner.invoke(providers_app, ["use", "cx-a"])

        assert response.exit_code == 130
        assert "switch interrupted: codex login verification interrupted" in response.output
        assert "slot is in an indeterminate state" in response.output

    def test_use_managed_live_pin_defers(self, tmp_path, monkeypatch):
        slot = _cli_slot(monkeypatch)
        env = {"HOME": str(tmp_path), "PWD": str(tmp_path)}
        slot["claude"] = _blob("A0")
        runner.invoke(providers_app, ["register", "work-a"], env=env, catch_exceptions=False)
        slot["claude"] = _blob("B0")
        runner.invoke(providers_app, ["register", "work-b"], env=env, catch_exceptions=False)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(99, "claude")],
        )
        r = runner.invoke(providers_app, ["use", "work-a"], env=env, catch_exceptions=False)
        assert r.exit_code == 2
        assert "deferred" in r.output and "99" in r.output
