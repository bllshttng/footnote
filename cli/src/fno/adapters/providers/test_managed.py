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
from fno.adapters.providers.model import _HARNESS_LITERAL, ProviderRecord, ProvidersConfig

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


def _rec(id_: str, harness: _HARNESS_LITERAL = "claude") -> ProviderRecord:
    return ProviderRecord(id=id_, name=id_, harness=harness, auth="managed")


@pytest.fixture(autouse=True)
def no_profile_network(monkeypatch):
    """Principal resolution reaches ``/api/oauth/profile``; unit tests must not.

    Default every test to an unreachable endpoint, which ``slot_principal``
    classifies as ``profile-unavailable``. A test that cares about the profile
    overrides this - either by patching ``urlopen`` itself or by patching
    ``managed.slot_principal`` - so no test silently depends on a live account.
    """
    import urllib.error

    def _no_network(*_args, **_kwargs):
        raise urllib.error.URLError("network disabled in tests")

    monkeypatch.setattr(managed.urllib.request, "urlopen", _no_network)


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
    monkeypatch.setattr(
        managed, "canonical_slot_blobs",
        lambda cli: [slot[cli]] if slot.get(cli) else [],
    )
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
        assert meta["harness"] == "claude" and meta["account_id"] == "work-a"

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


class TestDuplicateCredential:
    """The store must never describe one account as two (Evidence 2)."""

    def test_finds_the_other_holder_of_the_same_credential(self, fake_slot, tmp_path):
        fake_slot["claude"] = _blob("A0")
        managed.snapshot_current(_rec("work-a"), root=tmp_path)
        assert (
            managed.duplicate_credential_holder(
                _blob("A0"), exclude_id="work-b", root=tmp_path
            )
            == "work-a"
        )

    def test_a_distinct_credential_is_not_a_duplicate(self, fake_slot, tmp_path):
        fake_slot["claude"] = _blob("A0")
        managed.snapshot_current(_rec("work-a"), root=tmp_path)
        assert (
            managed.duplicate_credential_holder(
                _blob("B0"), exclude_id="work-b", root=tmp_path
            )
            is None
        )

    def test_reregistering_the_same_id_is_not_a_duplicate(self, fake_slot, tmp_path):
        # Refreshing one account's own snapshot must stay idempotent.
        fake_slot["claude"] = _blob("A0")
        managed.snapshot_current(_rec("work-a"), root=tmp_path)
        assert (
            managed.duplicate_credential_holder(
                _blob("A0"), exclude_id="work-a", root=tmp_path
            )
            is None
        )

    def test_digest_is_not_the_secret(self):
        # The comparison must never carry token material anywhere it could be
        # logged, printed, or persisted.
        digest = managed.credential_digest(_blob("super-secret-token"))
        assert digest is not None
        assert "super-secret-token" not in digest
        assert len(digest) == 64

    def test_codex_blobs_compare_by_whole_payload(self, tmp_path):
        # A codex auth.json carries no claudeAiOauth.accessToken; identical
        # payloads are still the same credential.
        a = managed.credential_digest(_codex_blob("T0"))
        assert a == managed.credential_digest(_codex_blob("T0"))
        assert a != managed.credential_digest(_codex_blob("T1"))

    def test_empty_blob_has_nothing_to_compare(self, tmp_path):
        assert managed.credential_digest(None) is None
        assert managed.credential_digest("   ") is None
        assert (
            managed.duplicate_credential_holder(None, exclude_id="x", root=tmp_path)
            is None
        )


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
    def test_live_pin_defer_policy_refuses_and_leaves_slot_untouched(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """pin_policy="defer" (the recovery path): a pinned slot defers, names the
        session, and mutates nothing."""
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(4242, "claude")],
        )
        before = fake_slot["claude"]
        stored_b = (tmp_path / "work-b" / "blob").read_text()
        with pytest.raises(managed.SwitchDeferred) as exc:
            managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path, pin_policy="defer")
        assert "4242" in str(exc.value)
        assert fake_slot["claude"] == before  # slot untouched
        assert (tmp_path / "work-b" / "blob").read_text() == stored_b  # store untouched

    def test_live_pin_default_warns_and_proceeds(self, fake_slot, tmp_path, monkeypatch):
        """Default policy: a pinned claude slot switches anyway (the same rewrite a
        manual /login performs) and reports the pinning pids on the result."""
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(4242, "claude")],
        )
        result = managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert result.active == "work-a"
        assert result.pinned_by == (4242,)
        assert fake_slot["claude"] == (tmp_path / "work-a" / "blob").read_text()
        assert managed.active_slot_id("claude", tmp_path) == "work-a"

    def test_unpinned_switch_reports_no_pins(self, fake_slot, tmp_path):
        by_id = _register_two(fake_slot, tmp_path)
        result = managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert result.pinned_by == ()
        assert not managed.slot_tainted("claude", tmp_path)

    def test_invalid_pin_policy_rejected(self, fake_slot, tmp_path):
        by_id = _register_two(fake_slot, tmp_path)
        with pytest.raises(ValueError, match="invalid pin_policy"):
            managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path, pin_policy="deferr")

    def test_pinned_switch_taints_stamp_and_skips_next_capture(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """A warn-under-pin switch leaves an untrustworthy stamp: a pinned session
        may overwrite the slot afterward, so the NEXT switch must not capture the
        slot blob into the stamped account's snapshot (poisoning it)."""
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(4242, "claude")],
        )
        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert managed.slot_tainted("claude", tmp_path)

        # A pinned work-b session refreshes and overwrites the slot out-of-band.
        fake_slot["claude"] = _blob("B-rotated")
        snapshot_a = (tmp_path / "work-a" / "blob").read_text()

        # Pin-free switch back to work-b: capture must be skipped (work-a's
        # snapshot untouched by the foreign blob) and the taint cleared.
        monkeypatch.setattr(managed, "pinning_sessions", lambda config_dir=None: [])
        managed.switch(by_id["work-b"], by_id=by_id, root=tmp_path)
        assert (tmp_path / "work-a" / "blob").read_text() == snapshot_a
        assert not managed.slot_tainted("claude", tmp_path)

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

        def _boom(cli):
            raise managed.KeychainError("security find-generic-password timed out")

        monkeypatch.setattr(managed, "canonical_slot_blobs", _boom)
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
                id="bad", name="bad", harness="claude", auth="managed",
                credentials_source=Path("/tmp/x"),
            )

    def test_managed_rejects_env(self):
        with pytest.raises(ValueError, match="auth=managed"):
            ProviderRecord(
                id="bad", name="bad", harness="claude", auth="managed",
                env={"ANTHROPIC_API_KEY": "x"},
            )

    def test_managed_bare_record_ok(self):
        rec = ProviderRecord(id="ok", name="ok", harness="claude", auth="managed")
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
    a, b = _rec("cx-a", harness="codex"), _rec("cx-b", harness="codex")
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
        target = _rec("cx-a", harness="codex")
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
        a, b = _rec("cx-a", harness="codex"), _rec("cx-b", harness="codex")
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
        a, b = _rec("cx-a", harness="codex"), _rec("cx-b", harness="codex")
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
    monkeypatch.setattr(
        managed, "canonical_slot_blobs",
        lambda cli: [slot[cli]] if slot.get(cli) else [],
    )
    return slot


class TestCliSurface:
    def _invoke_codex_use(self, monkeypatch, result):
        config = ProvidersConfig(records=[_rec("cx-a", harness="codex")], active="cx-a")
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

    def test_register_refuses_a_duplicate_credential(self, tmp_path, monkeypatch):
        """AC3-ERR: capturing the SAME login under a second id is refused.

        This is the guard that would have surfaced the live store defect on the
        day it was created rather than ten days later.
        """
        slot = _cli_slot(monkeypatch)
        env = {"HOME": str(tmp_path), "PWD": str(tmp_path)}
        slot["claude"] = _blob("shared-secret-token")
        r1 = runner.invoke(providers_app, ["register", "work-a"], env=env, catch_exceptions=False)
        assert r1.exit_code == 0, r1.output

        # The operator never signed into the second account: the slot still holds
        # work-a's credential.
        r2 = runner.invoke(providers_app, ["register", "work-b"], env=env, catch_exceptions=False)
        assert r2.exit_code != 0
        assert "work-a" in r2.output and "work-b" in r2.output
        assert "--config-dir" in r2.output
        # No blob was written under the refused id.
        assert not (tmp_path / ".fno" / "providers" / "work-b" / "blob").exists()
        # No token or fragment reaches the receipt.
        assert "shared-secret-token" not in r2.output

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
        config = ProvidersConfig(records=[_rec("cx-a", harness="codex")], active="cx-a")
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

    def test_use_managed_live_pin_proceeds_with_warning(self, tmp_path, monkeypatch):
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
        assert r.exit_code == 0
        assert "Materialized managed account 'work-a'" in r.output
        assert "warning: swapped under 1 live claude session(s)" in r.output


# ---------------------------------------------------------------------------
# x-4b8d: principal reconciliation (prove the live slot's identity)
# ---------------------------------------------------------------------------


def _profile(uuid: str, email: str = "a@example.com", org: str = "org-1") -> dict:
    """The non-secret shape /api/oauth/profile returns (verified live 2026-08-03)."""
    return {
        "account": {"uuid": uuid, "email": email, "full_name": "JN"},
        "organization": {"uuid": org, "name": f"{email}'s Organization"},
        "application": {"slug": "claude-code"},
    }


def _store_state(root) -> dict[str, bytes]:
    """Every byte of the store that a refusal must leave untouched.

    The switch mutex is excluded: taking a lock is not a store mutation, and
    whether the lockfile survives release is a filelock implementation detail.
    """
    return {
        str(p.relative_to(root)): p.read_bytes()
        for p in sorted(root.rglob("*"))
        if p.is_file() and p.name != ".switch.lock"
    }


def _bind(record_id: str, uuid: str, root, email: str = "a@example.com") -> None:
    fingerprint = managed.principal_fingerprint(_profile(uuid, email))
    assert fingerprint is not None
    managed.write_record_principal(record_id, fingerprint, root)


class TestPrincipalFingerprint:
    def test_account_uuid_is_the_discriminator(self):
        """The smallest stable non-secret field set: account.uuid decides a match."""
        fp = managed.principal_fingerprint(_profile("acct-1", "jn@example.com", "org-9"))
        assert fp["account_uuid"] == "acct-1"
        assert fp["organization_uuid"] == "org-9"
        assert fp["email"] == "jn@example.com"
        assert not any("token" in k.lower() for k in fp)

    @pytest.mark.parametrize(
        "payload",
        [None, {}, {"account": {}}, {"account": {"uuid": ""}}, {"account": "nope"}, "text"],
    )
    def test_no_stable_uuid_is_not_a_fingerprint(self, payload):
        assert managed.principal_fingerprint(payload) is None

    def test_slot_principal_classifies_failures(self, monkeypatch):
        """A refusal must say WHICH way identity went unproven (US3), and a dead
        credential is not the same as an unanswered question."""
        import urllib.error

        def _raise(exc):
            def _urlopen(*a, **k):
                raise exc
            return _urlopen

        def _http(code):
            return _raise(
                urllib.error.HTTPError(managed._PROFILE_URL, code, "no", {}, None)
            )

        # The endpoint ANSWERED: this credential is dead.
        monkeypatch.setattr(managed.urllib.request, "urlopen", _http(401))
        assert managed.slot_principal(_blob("T")) == (None, "credential-rejected")
        monkeypatch.setattr(managed.urllib.request, "urlopen", _http(403))
        assert managed.slot_principal(_blob("T")) == (None, "credential-rejected")

        # The question went unanswered - a different thing entirely.
        monkeypatch.setattr(managed.urllib.request, "urlopen", _http(429))
        assert managed.slot_principal(_blob("T")) == (None, "profile-unavailable")
        monkeypatch.setattr(managed.urllib.request, "urlopen", _http(503))
        assert managed.slot_principal(_blob("T")) == (None, "profile-unavailable")
        monkeypatch.setattr(managed.urllib.request, "urlopen", _raise(TimeoutError()))
        assert managed.slot_principal(_blob("T")) == (None, "profile-unavailable")

        # A blob with no usable bearer never reaches the network, and is dead.
        assert managed.slot_principal("{}") == (None, "credential-rejected")

    def test_an_unanswered_candidate_blocks_the_whole_slot(self, monkeypatch):
        """A live account whose profile call merely timed out must not be skipped
        while the other candidate is stamped - claude may be reading the one we
        failed to ask about."""
        def _principal(blob):
            if blob == _blob("SILENT"):
                return None, "profile-unavailable"
            return managed.principal_fingerprint(_profile("acct-b")), None

        monkeypatch.setattr(managed, "slot_principal", _principal)
        assert managed.principal_of_blobs([_blob("SILENT"), _blob("B")]) == (
            None, None, "profile-unavailable"
        )


class TestReconcileSlot:
    def test_clears_false_taint_with_matching_principal(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """AC1-HP: the stamped record IS the live principal - refresh, stamp, clear."""
        by_id = _register_two(fake_slot, tmp_path)  # stamp = work-b
        _bind("work-b", "acct-b", tmp_path)
        fake_slot["claude"] = _blob("B_ROTATED")
        managed._set_slot_taint("claude", tmp_path, True)
        monkeypatch.setattr(
            managed, "slot_principal", lambda blob: (managed.principal_fingerprint(_profile("acct-b")), None)
        )

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "matched" and result.record_id == "work-b"
        assert not managed.slot_tainted("claude", tmp_path)
        assert managed.active_slot_id("claude", tmp_path) == "work-b"
        assert (tmp_path / "work-b" / "blob").read_text() == _blob("B_ROTATED")

    def test_adopts_out_of_band_login_without_poisoning_the_other_store(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """AC2-HP: stamp says work-b but `claude /login` put work-a in the slot."""
        by_id = _register_two(fake_slot, tmp_path)  # stamp = work-b
        _bind("work-a", "acct-a", tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        fake_slot["claude"] = _blob("A_FRESH")  # out-of-band /login as work-a
        managed._set_slot_taint("claude", tmp_path, True)
        monkeypatch.setattr(
            managed, "slot_principal", lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None)
        )

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "matched" and result.record_id == "work-a"
        assert managed.active_slot_id("claude", tmp_path) == "work-a"
        assert (tmp_path / "work-a" / "blob").read_text() == _blob("A_FRESH")
        # work-b's credential is NOT overwritten with work-a's token.
        assert (tmp_path / "work-b" / "blob").read_text() == _blob("B0")

    @pytest.mark.parametrize(
        "principal_fn,outcome",
        [
            (lambda blob: (None, "profile-unavailable"), "profile-unavailable"),
            (lambda blob: (None, "malformed-profile"), "malformed-profile"),
            (
                lambda blob: (managed.principal_fingerprint(_profile("acct-stranger")), None),
                "zero-match",
            ),
        ],
    )
    def test_unproven_identity_leaves_the_store_byte_identical(
        self, fake_slot, tmp_path, monkeypatch, principal_fn, outcome
    ):
        """AC3-ERR: no match, no mutation - stamp, snapshots and taint unchanged."""
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-a", "acct-a", tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        managed._set_slot_taint("claude", tmp_path, True)
        fake_slot["claude"] = _blob("MYSTERY")
        monkeypatch.setattr(managed, "slot_principal", principal_fn)
        before = _store_state(tmp_path)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == outcome and result.record_id is None
        assert _store_state(tmp_path) == before
        assert managed.slot_tainted("claude", tmp_path)

    def test_ambiguous_match_refuses(self, fake_slot, tmp_path, monkeypatch):
        """AC3-ERR: two records fingerprinted to one principal is not proof."""
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-a", "acct-dup", tmp_path)
        _bind("work-b", "acct-dup", tmp_path)
        managed._set_slot_taint("claude", tmp_path, True)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-dup")), None),
        )
        before = _store_state(tmp_path)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "ambiguous-match"
        assert "work-a" in result.detail and "work-b" in result.detail
        assert _store_state(tmp_path) == before

    def test_config_dir_records_never_enter_shared_slot_reconciliation(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """Boundaries: a record with its own dir is attributable without the slot."""
        by_id = _register_two(fake_slot, tmp_path)
        own = ProviderRecord(
            id="own-dir", name="own-dir", harness="claude", auth="managed",
            config_dir=tmp_path / "alt-home",
        )
        by_id["own-dir"] = own
        _bind("own-dir", "acct-x", tmp_path)
        managed._set_slot_taint("claude", tmp_path, True)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-x")), None),
        )

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "zero-match"
        assert managed.slot_tainted("claude", tmp_path)

    def test_empty_slot_refuses(self, fake_slot, tmp_path):
        by_id = _register_two(fake_slot, tmp_path)
        fake_slot["claude"] = None
        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)
        assert result.outcome == "no-slot-credential"

    def test_codex_is_unsupported(self, fake_slot, tmp_path):
        """Only claude has a profile endpoint; guessing for codex is not proof."""
        result = managed.reconcile_slot("codex", by_id={}, root=tmp_path)
        assert result.outcome == "unsupported-harness"

    def test_capture_before_overwrite_preserves_a_stored_principal(
        self, fake_slot, tmp_path
    ):
        """A re-snapshot must not wipe the identity that makes reconcile work."""
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        fake_slot["claude"] = _blob("B1")
        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert managed.record_principal("work-b", tmp_path)["account_uuid"] == "acct-b"


class TestReconcileConcurrency:
    def test_held_switch_lock_yields_a_typed_refusal(self, fake_slot, tmp_path):
        """AC5-CON: reconciliation waits on the SAME mutex a switch takes."""
        import filelock

        by_id = _register_two(fake_slot, tmp_path)
        held = filelock.FileLock(str(managed._switch_lock_path(tmp_path)), timeout=1)
        held.acquire()
        try:
            result = managed.reconcile_slot(
                "claude", by_id=by_id, root=tmp_path, lock_timeout=0.2
            )
        finally:
            held.release()
        assert result.outcome == "lock-timeout"

    def test_racing_a_switch_never_mixes_stamp_and_snapshot(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """AC5-CON: one complete identity transition commits before the other runs."""
        import threading

        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-a", "acct-a", tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        # The live principal always describes whatever blob is in the slot NOW.
        token_to_uuid = {_blob("A0"): "acct-a", _blob("B0"): "acct-b"}
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (
                managed.principal_fingerprint(_profile(token_to_uuid.get(blob, "acct-?"))),
                None,
            ),
        )
        results: list[object] = []
        barrier = threading.Barrier(2)

        def _switch():
            barrier.wait()
            results.append(managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path))

        def _reconcile():
            barrier.wait()
            results.append(managed.reconcile_slot("claude", by_id=by_id, root=tmp_path))

        threads = [threading.Thread(target=_switch), threading.Thread(target=_reconcile)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=20)

        stamped = managed.active_slot_id("claude", tmp_path)
        assert stamped in ("work-a", "work-b")
        # The stamp and the slot describe the SAME account: never A's snapshot
        # under B's stamp.
        assert managed.read_blob(stamped, tmp_path) == fake_slot["claude"]


class TestReconcileRespectsLiveTaintWriters:
    """Proving the principal proves it NOW. A session that was pinning when the
    taint was written holds the PREVIOUS account's token and can flush a refresh
    of it into the slot afterwards, so clearing on its watch would trust a stamp
    the next refresh invalidates."""

    @staticmethod
    def _arm(fake_slot, tmp_path, monkeypatch, pids):
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        fake_slot["claude"] = _blob("B_ROTATED")
        managed._set_slot_taint("claude", tmp_path, True, pids)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-b")), None),
        )
        return by_id

    def test_a_live_taint_writer_blocks_the_repair(
        self, fake_slot, tmp_path, monkeypatch
    ):
        import os

        by_id = self._arm(fake_slot, tmp_path, monkeypatch, [os.getpid()])
        before = _store_state(tmp_path)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "slot-pinned"
        assert str(os.getpid()) in result.detail
        assert _store_state(tmp_path) == before
        assert managed.slot_tainted("claude", tmp_path)

    def test_a_dead_taint_writer_no_longer_blocks(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """The common repair: the rotated-out session already exited."""
        by_id = self._arm(fake_slot, tmp_path, monkeypatch, [999_999])
        monkeypatch.setattr(managed.psutil, "pid_exists", lambda pid: False)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "matched" and result.record_id == "work-b"
        assert not managed.slot_tainted("claude", tmp_path)

    def test_an_unrelated_live_session_does_not_block(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """A session started AFTER the switch read the new credential, so it is
        not a risk - and on the shared slot it is usually the account being
        proven, including the very session running the repair."""
        by_id = self._arm(fake_slot, tmp_path, monkeypatch, [999_999])
        monkeypatch.setattr(managed.psutil, "pid_exists", lambda pid: False)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(4242, "claude")],
        )
        assert managed.reconcile_slot("claude", by_id=by_id, root=tmp_path).ok

    def test_a_legacy_marker_falls_back_to_a_live_scan(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """A marker written before pids were recorded cannot say who was live,
        so the conservative scan is the only honest answer."""
        by_id = self._arm(fake_slot, tmp_path, monkeypatch, [])
        managed._atomic_write_private(managed._slot_taint_path("claude", tmp_path), "1")
        assert managed.tainting_pids("claude", tmp_path) is None
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(4242, "claude")],
        )

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "slot-pinned" and "4242" in result.detail

    def test_switch_records_the_pins_it_proceeded_under(
        self, fake_slot, tmp_path, monkeypatch
    ):
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(77, "claude")],
        )
        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert managed.tainting_pids("claude", tmp_path) == (77,)


class TestCanonicalSlotRead:
    def test_an_ambient_config_dir_never_redirects_the_identity_read(
        self, tmp_path, monkeypatch
    ):
        """A worker pinned to another account exports CLAUDE_CONFIG_DIR. Reading
        that dir would prove the PINNED account's identity and stamp it onto the
        canonical slot, which is the misattribution this whole node exists to
        kill - arriving through the repair meant to prevent it."""
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "claude-alt"))
        monkeypatch.setattr(managed.sys, "platform", "linux")
        asked: list[tuple] = []

        def _record(cfg, *, shared):
            asked.append((cfg, shared))
            return _blob("CANONICAL")

        monkeypatch.setattr(managed, "_read_claude_blob", _record)

        assert managed.read_canonical_slot_blob("claude") == _blob("CANONICAL")
        assert asked == [(tmp_path / ".claude", True)]

    def test_both_keychain_items_are_candidates(self, tmp_path, monkeypatch):
        """darwin keeps a scoped and an unscoped item for the canonical dir, and
        they can hold different accounts."""
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr(managed.sys, "platform", "darwin")
        items = {
            managed._claude_scoped_service(tmp_path / ".claude"): _blob("SCOPED"),
            managed._CLAUDE_KEYCHAIN_SERVICE: _blob("UNSCOPED"),
        }
        monkeypatch.setattr(
            managed, "_read_claude_keychain_item", lambda service: items.get(service)
        )
        assert managed.canonical_slot_blobs("claude") == [
            _blob("SCOPED"), _blob("UNSCOPED")
        ]

    def test_identical_items_collapse_to_one_candidate(self, tmp_path, monkeypatch):
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr(managed.sys, "platform", "darwin")
        monkeypatch.setattr(
            managed, "_read_claude_keychain_item", lambda service: _blob("SAME")
        )
        assert managed.canonical_slot_blobs("claude") == [_blob("SAME")]

    def test_two_accounts_in_one_slot_refuses_rather_than_picking(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """Not a tie to break: whichever was stamped, some reader would get the
        other one."""
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-a", "acct-a", tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs",
            lambda cli: [_blob("SCOPED"), _blob("UNSCOPED")],
        )
        by_blob = {_blob("SCOPED"): "acct-a", _blob("UNSCOPED"): "acct-b"}
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile(by_blob[blob])), None),
        )
        before = _store_state(tmp_path)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "ambiguous-slot"
        assert _store_state(tmp_path) == before

    def test_agreeing_items_reconcile_normally(
        self, fake_slot, tmp_path, monkeypatch
    ):
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        managed._set_slot_taint("claude", tmp_path, True, [])
        monkeypatch.setattr(
            managed, "canonical_slot_blobs",
            lambda cli: [_blob("SCOPED"), _blob("UNSCOPED")],
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-b")), None),
        )

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "matched" and result.record_id == "work-b"


class TestReconcileCommitsOnlyWhatItProved:
    """Peer-review findings on PR #712: identity was proven about bytes that
    could already be gone, and a refusal could still touch disk."""

    def test_a_writer_that_rewrites_and_exits_mid_profile_blocks_the_commit(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """The window the pid check alone cannot close: the recorded writer
        replaces the slot DURING the profile call and then exits, so a liveness
        check afterwards finds nothing and we would stamp a credential we never
        looked at."""
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        fake_slot["claude"] = _blob("B_AT_READ")
        managed._set_slot_taint("claude", tmp_path, True, [999_999])
        monkeypatch.setattr(managed.psutil, "pid_exists", lambda pid: False)

        def _profile_then_rewrite(blob):
            fake_slot["claude"] = _blob("SOMEONE_ELSE")  # the writer's last act
            return managed.principal_fingerprint(_profile("acct-b")), None

        monkeypatch.setattr(managed, "slot_principal", _profile_then_rewrite)
        before = _store_state(tmp_path)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "slot-changed"
        assert _store_state(tmp_path) == before
        assert managed.slot_tainted("claude", tmp_path)

    def test_a_live_writer_is_refused_before_the_endpoint_is_touched(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """Proving identity first would spend a network round trip to answer a
        question the pin gate already settles."""
        import os

        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        managed._set_slot_taint("claude", tmp_path, True, [os.getpid()])
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: pytest.fail("resolved a principal for a pinned slot"),
        )

        assert managed.reconcile_slot(
            "claude", by_id=by_id, root=tmp_path
        ).outcome == "slot-pinned"

    def test_a_refusal_never_creates_the_store(self, fake_slot, tmp_path):
        """`matched` is the only outcome allowed to touch disk, and that has to
        include the store directory itself."""
        missing = tmp_path / "no-store-here"

        result = managed.reconcile_slot("claude", by_id={}, root=missing)

        assert result.outcome == "no-managed-store"
        assert not missing.exists()


class TestForcedRebindNeverLeavesAStalePrincipal:
    def test_a_failed_reprove_drops_the_previous_binding(self, fake_slot, tmp_path):
        """Re-registering an id points it at whoever is signed in NOW, while
        write_snapshot preserves the old principal for capture-before-overwrite.
        If the reprove fails, keeping that binding would claim the new
        credential belongs to the old account."""
        record = _rec("work-a")
        fake_slot["claude"] = _blob("A0")
        managed.snapshot_current(record, root=tmp_path)
        _bind("work-a", "acct-old", tmp_path)

        # A different account is signed in now, and the profile call fails
        # (the autouse fixture disables the network).
        fake_slot["claude"] = _blob("SOMEONE_ELSE")
        managed.snapshot_current(record, root=tmp_path)
        assert managed.record_principal("work-a", tmp_path) is not None  # preserved
        managed.capture_record_principal(record, root=tmp_path, force=True)

        assert managed.record_principal("work-a", tmp_path) is None
        # The rest of the metadata survives.
        assert managed.read_meta("work-a", tmp_path)["harness"] == "claude"

    def test_an_unforced_capture_leaves_a_bound_record_alone(self, fake_slot, tmp_path):
        """A switch of an already-bound record must not spend a call, nor clear
        a binding that is still correct."""
        record = _rec("work-a")
        fake_slot["claude"] = _blob("A0")
        managed.snapshot_current(record, root=tmp_path)
        _bind("work-a", "acct-a", tmp_path)

        managed.capture_record_principal(record, root=tmp_path)

        assert managed.record_principal("work-a", tmp_path)["account_uuid"] == "acct-a"


class TestTaintWriterIdentity:
    """A pid is not an identity. Pids get reused, and a recycled one wearing the
    number of a long-gone session would hold the repair open forever - a
    permanent refusal, which is worse than the transient one it imitates."""

    @staticmethod
    def _arm(fake_slot, tmp_path, monkeypatch, pids):
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        fake_slot["claude"] = _blob("B_ROTATED")
        managed._set_slot_taint("claude", tmp_path, True, pids)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-b")), None),
        )
        return by_id

    def test_the_start_time_is_recorded_alongside_the_pid(
        self, fake_slot, tmp_path, monkeypatch
    ):
        import os

        self._arm(fake_slot, tmp_path, monkeypatch, [os.getpid()])
        writers = managed.tainting_writers("claude", tmp_path)
        assert writers is not None and writers[0][0] == os.getpid()
        assert writers[0][1] == pytest.approx(
            managed._process_started_at(os.getpid())
        )

    def test_a_recycled_pid_does_not_hold_the_repair(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """Same number, different process: it never touched this slot."""
        import os

        by_id = self._arm(fake_slot, tmp_path, monkeypatch, [os.getpid()])
        # Rewrite the marker so the recorded start time predates this process.
        managed._atomic_write_private(
            managed._slot_taint_path("claude", tmp_path),
            json.dumps({"writers": [{"pid": os.getpid(), "started": 1.0}]}),
        )

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "matched" and result.record_id == "work-b"

    def test_the_original_process_still_holds_it(
        self, fake_slot, tmp_path, monkeypatch
    ):
        import os

        by_id = self._arm(fake_slot, tmp_path, monkeypatch, [os.getpid()])
        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)
        assert result.outcome == "slot-pinned" and str(os.getpid()) in result.detail

    def test_a_pid_only_marker_still_blocks_conservatively(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """A marker written before start times existed cannot distinguish, so it
        refuses; the next switch replaces it with a full record."""
        import os

        by_id = self._arm(fake_slot, tmp_path, monkeypatch, [])
        managed._atomic_write_private(
            managed._slot_taint_path("claude", tmp_path),
            json.dumps({"pids": [os.getpid()]}),
        )
        assert managed.reconcile_slot(
            "claude", by_id=by_id, root=tmp_path
        ).outcome == "slot-pinned"


class TestSwitchNeverBindsAnIdentity:
    def test_a_switch_does_not_bind_an_unbound_record(self, fake_slot, tmp_path):
        """A switch materializes the record's STORED snapshot, whose provenance
        is the store, not the operator. An earlier out-of-band login plus
        capture-before-overwrite can leave one account's credential filed under
        another's id, so binding from it would manufacture confident attribution
        to the wrong account."""
        by_id = _register_two(fake_slot, tmp_path)

        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)

        assert managed.record_principal("work-a", tmp_path) is None

    def test_a_switch_invalidates_cached_principal_evidence(
        self, fake_slot, tmp_path
    ):
        """The slot now holds a different credential, so evidence about the
        previous occupant must not survive it."""
        by_id = _register_two(fake_slot, tmp_path)
        managed.note_slot_principal("claude", tmp_path, "acct-b", "B0", now=1000.0)

        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)

        assert managed.cached_slot_principal(
            "claude", tmp_path, "B0", now=1000.0 + 1
        ) is None

    def test_the_pin_scan_carries_each_start_time_into_the_taint(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """Sampling the start time after the scan would read the REPLACEMENT of
        a pid that exited in between - fingerprinting the process the start time
        exists to exclude."""
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "pinning_sessions",
            lambda config_dir=None: [managed.PinningSession(77, "claude", 4242.0)],
        )
        monkeypatch.setattr(
            managed, "_process_started_at",
            lambda pid: pytest.fail("re-sampled a start time after the scan"),
        )

        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)

        assert managed.tainting_writers("claude", tmp_path) == [(77, 4242.0)]


class TestOrganizationScopedIdentity:
    """Claude Code usage is organization-scoped, and one human can belong to two
    organizations. Comparing the account uuid alone would let an org-B bearer
    pass as the org-A record and file its usage there."""

    def test_identity_needs_both_halves(self):
        assert managed.identity_key(
            {"account_uuid": "a", "organization_uuid": "o"}
        ) == "a/o"
        assert managed.identity_key({"account_uuid": "a"}) is None
        assert managed.identity_key({"organization_uuid": "o"}) is None
        assert managed.identity_key(None) is None

    def test_same_account_different_org_is_a_mismatch(self, tmp_path, monkeypatch):
        managed.write_record_principal(
            "org-a", {"account_uuid": "human-1", "organization_uuid": "org-a"}, tmp_path
        )
        monkeypatch.setattr(
            managed, "principal_of_bearer",
            lambda bearer: (
                {"account_uuid": "human-1", "organization_uuid": "org-b"}, None
            ),
        )
        assert managed.bearer_principal_verdict(
            "claude", "org-a", tmp_path, "tok"
        ) == "mismatch"

    def test_an_incomplete_binding_cannot_vouch_for_anything(self, tmp_path):
        managed.write_record_principal("half", {"account_uuid": "human-1"}, tmp_path)
        assert managed.bearer_principal_verdict(
            "claude", "half", tmp_path, "tok"
        ) == "unprovable"

    def test_two_orgs_in_one_slot_is_ambiguous_not_a_match(
        self, fake_slot, tmp_path, monkeypatch
    ):
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("A"), _blob("B")]
        )
        orgs = {_blob("A"): "org-a", _blob("B"): "org-b"}
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (
                {"account_uuid": "human-1", "organization_uuid": orgs[blob]}, None
            ),
        )
        assert managed.canonical_slot_principal("claude") == (None, "ambiguous-slot")
        assert managed.reconcile_slot(
            "claude", by_id=by_id, root=tmp_path
        ).outcome == "ambiguous-slot"


class TestRegisterCapturesOneCredential:
    """Proving one read and snapshotting another is how account A's identity
    ends up bound to account B's credential."""

    def test_the_proved_bytes_are_the_stored_bytes(self, tmp_path, monkeypatch):
        """An ambient CLAUDE_CONFIG_DIR redirects `_read_slot_blob` but must not
        reach the capture, which reads the canonical slot once."""
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("CANONICAL")]
        )
        monkeypatch.setattr(
            managed, "_read_slot_blob",
            lambda cli, config_dir=None: pytest.fail("re-read the slot after proving"),
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure is None and principal["account_uuid"] == "acct-a"
        assert (adir / "blob").read_text() == _blob("CANONICAL")
        assert managed.record_principal("work-a", tmp_path)["account_uuid"] == "acct-a"

    def test_an_ambiguous_slot_writes_nothing(self, tmp_path, monkeypatch):
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("A"), _blob("B")]
        )
        who = {_blob("A"): "acct-a", _blob("B"): "acct-b"}
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile(who[blob])), None),
        )

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure == "ambiguous-slot" and principal is None
        assert not (tmp_path / "work-a" / "blob").exists()

    def test_an_unprovable_identity_still_registers_but_unbound(
        self, tmp_path, monkeypatch
    ):
        """Registration must work offline; the record is then simply unbound and
        `doctor` says so."""
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("OFFLINE")]
        )
        adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )
        assert principal is None and failure == "profile-unavailable"
        assert (adir / "blob").read_text() == _blob("OFFLINE")
        assert managed.record_principal("work-a", tmp_path) is None

    def test_it_serializes_against_a_switch(self, tmp_path, monkeypatch):
        import filelock

        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("X")]
        )
        held = filelock.FileLock(str(managed._switch_lock_path(tmp_path)), timeout=1)
        held.acquire()
        try:
            with pytest.raises(managed.SwitchDeferred):
                managed.register_slot_snapshot(_rec("work-a"), tmp_path, lock_timeout=0.2)
        finally:
            held.release()


class TestReconcileProvesItsOwnCapture:
    def test_only_the_captured_bytes_are_ever_proven(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """Re-reading the slot to prove it would let an A -> B -> A flip prove B,
        survive the later comparison against the captured A, and cache A's
        bearer under B's identity. The proof must see only the capture."""
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        managed._set_slot_taint("claude", tmp_path, True, [])
        reads = {"n": 0}

        def _blobs(cli):
            reads["n"] += 1
            # Anything after the capture sees a different credential.
            return [_blob("B0")] if reads["n"] == 1 else [_blob("IMPOSTOR")]

        proven: list[str] = []

        def _principal(blob):
            proven.append(blob)
            return (
                managed.principal_fingerprint(
                    _profile("acct-b" if blob == _blob("B0") else "acct-impostor")
                ),
                None,
            )

        monkeypatch.setattr(managed, "canonical_slot_blobs", _blobs)
        monkeypatch.setattr(managed, "slot_principal", _principal)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        # The impostor was never proven, and the genuine change was caught.
        assert proven == [_blob("B0")]
        assert result.outcome == "slot-changed"
        assert managed.slot_tainted("claude", tmp_path)

    def test_a_stable_slot_proves_and_commits(
        self, fake_slot, tmp_path, monkeypatch
    ):
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-b", "acct-b", tmp_path)
        managed._set_slot_taint("claude", tmp_path, True, [])
        proven: list[str] = []
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("B0")]
        )

        def _principal(blob):
            proven.append(blob)
            return managed.principal_fingerprint(_profile("acct-b")), None

        monkeypatch.setattr(managed, "slot_principal", _principal)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "matched" and result.record_id == "work-b"
        assert proven == [_blob("B0")]  # proved once, over the capture


class TestRegisterRejectsADuplicatePrincipal:
    """`duplicate_credential_holder` compares TOKENS, which rotate. The same
    account registered again after a rotation slips past it, creating two
    records for one quota pool that reconciliation can then never tell apart."""

    @staticmethod
    def _arm(monkeypatch, uuid):
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("ROTATED")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile(uuid)), None),
        )

    def test_a_rotated_token_for_a_known_principal_is_refused(
        self, tmp_path, monkeypatch
    ):
        _bind("work-a", "acct-a", tmp_path)
        self._arm(monkeypatch, "acct-a")

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a-again"), tmp_path
        )

        assert failure == "duplicate-principal:work-a" and principal is None
        assert not (tmp_path / "work-a-again" / "blob").exists()

    def test_a_new_principal_registers(self, tmp_path, monkeypatch):
        _bind("work-a", "acct-a", tmp_path)
        self._arm(monkeypatch, "acct-new")

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-b"), tmp_path
        )

        assert failure is None and principal["account_uuid"] == "acct-new"

    def test_re_registering_the_same_id_is_not_a_duplicate(
        self, tmp_path, monkeypatch
    ):
        _bind("work-a", "acct-a", tmp_path)
        self._arm(monkeypatch, "acct-a")

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure is None and principal["account_uuid"] == "acct-a"

    def test_the_active_stamp_is_written_inside_the_lock(
        self, tmp_path, monkeypatch
    ):
        """Releasing first would let a concurrent switch install and stamp
        another account before this stamp overwrote it."""
        self._arm(monkeypatch, "acct-a")
        stamped: list[str] = []
        real_stamp = managed.stamp_active_slot

        def _stamp(cli, record_id, root=None):
            lock = managed._switch_lock_path(root)
            assert lock.exists(), "stamped outside the switch lock"
            stamped.append(record_id)
            real_stamp(cli, record_id, root)

        monkeypatch.setattr(managed, "stamp_active_slot", _stamp)
        managed.register_slot_snapshot(_rec("work-a"), tmp_path)
        assert stamped == ["work-a"]
        assert managed.active_slot_id("claude", tmp_path) == "work-a"


class TestRegisterCommitOrder:
    @staticmethod
    def _arm(monkeypatch, uuid="acct-a", blob_token="LIVE"):
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob(blob_token)]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile(uuid)), None),
        )

    def test_a_failed_config_save_leaves_no_stamp(self, tmp_path, monkeypatch):
        """Stamping an unconfigured orphan would make every configured shared
        account unattributable behind it."""
        self._arm(monkeypatch)

        def _boom() -> None:
            raise OSError("disk full")

        with pytest.raises(OSError):
            managed.register_slot_snapshot(_rec("work-a"), tmp_path, persist=_boom)

        assert managed.active_slot_id("claude", tmp_path) is None

    def test_persist_runs_inside_the_lock(self, tmp_path, monkeypatch):
        """Releasing before the save would let a switch land between them."""
        self._arm(monkeypatch)
        seen: list[bool] = []

        def _persist() -> None:
            seen.append(managed._switch_lock_path(tmp_path).exists())

        managed.register_slot_snapshot(_rec("work-a"), tmp_path, persist=_persist)
        assert seen == [True]
        assert managed.active_slot_id("claude", tmp_path) == "work-a"

    def test_the_token_duplicate_check_runs_against_the_captured_blob(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """A check made outside the lock can be invalidated by a concurrent
        switch, and with the profile endpoint down the principal check cannot
        cover for it."""
        fake_slot["claude"] = _blob("SHARED")
        managed.snapshot_current(_rec("work-a"), root=tmp_path)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("SHARED")]
        )
        # Endpoint unavailable, so only the digest can catch this.
        monkeypatch.setattr(
            managed, "slot_principal", lambda blob: (None, "profile-unavailable")
        )

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-b"), tmp_path
        )

        assert failure == "duplicate-credential:work-a" and principal is None
        assert not (tmp_path / "work-b" / "blob").exists()
        assert managed.active_slot_id("claude", tmp_path) is None


class TestRegisterCommitSafety:
    def test_a_login_during_the_profile_request_refuses(self, tmp_path, monkeypatch):
        """The profile request is a network round trip; an out-of-band login
        during it would otherwise stamp the account we proved while the slot
        holds the one that replaced it."""
        reads = {"n": 0}

        def _blobs(cli):
            reads["n"] += 1
            return [_blob("A")] if reads["n"] == 1 else [_blob("B")]

        monkeypatch.setattr(managed, "canonical_slot_blobs", _blobs)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure == "slot-changed" and principal is None
        assert not (tmp_path / "work-a" / "blob").exists()
        assert managed.active_slot_id("claude", tmp_path) is None

    def test_a_failed_save_leaves_no_store_residue(self, tmp_path, monkeypatch):
        """Store residue from a failed registration is what a later attempt
        reads as a duplicate credential and refuses."""
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("LIVE")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        def _boom() -> None:
            raise OSError("disk full")

        with pytest.raises(OSError):
            managed.register_slot_snapshot(_rec("work-a"), tmp_path, persist=_boom)

        assert not (tmp_path / "work-a" / "blob").exists()
        assert managed.record_principal("work-a", tmp_path) is None
        assert managed.active_slot_id("claude", tmp_path) is None


class TestUnreadableSlotIsARefusal:
    def test_a_keychain_timeout_is_typed_not_raised(self, tmp_path, monkeypatch):
        """`security` can time out or be denied; an operator verb must report
        that, not surface a traceback."""
        def _boom(cli):
            raise managed.KeychainError("`security find-generic-password` timed out")

        monkeypatch.setattr(managed, "canonical_slot_blobs", _boom)

        result = managed.reconcile_slot("claude", by_id={}, root=tmp_path)

        assert result.outcome == "slot-unreadable" and "timed out" in result.detail

    def test_drift_reports_nothing_rather_than_raising(self, tmp_path, monkeypatch):
        managed.stamp_active_slot("claude", "work-a", tmp_path)
        managed.write_record_principal(
            "work-a", {"account_uuid": "a", "organization_uuid": "o"}, tmp_path
        )

        def _boom(cli):
            raise managed.KeychainError("denied")

        monkeypatch.setattr(managed, "canonical_slot_blobs", _boom)
        assert managed.slot_identity_drift("claude", tmp_path) is None


class TestRegisterPostWriteSlotMove:
    def test_a_login_during_the_writes_taints_the_stamp(self, tmp_path, monkeypatch):
        """The writes take time. An out-of-band login during them would leave an
        UNTAINTED stamp naming this record while the slot holds someone else,
        and the next switch would capture that credential into this snapshot."""
        reads = {"n": 0}

        def _blobs(cli):
            reads["n"] += 1
            # Reads 1 and 2 are the capture and its pre-commit re-check; the
            # third is the post-write look, by which time someone has logged in.
            return [_blob("A")] if reads["n"] <= 2 else [_blob("B")]

        monkeypatch.setattr(managed, "canonical_slot_blobs", _blobs)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        # Registered, but the stamp is explicitly not trusted.
        assert failure == "slot-moved-after-write"
        assert principal["account_uuid"] == "acct-a"
        assert (adir / "blob").read_text() == _blob("A")
        assert managed.active_slot_id("claude", tmp_path) == "work-a"
        assert managed.slot_tainted("claude", tmp_path)

    def test_a_stable_slot_leaves_the_stamp_trusted(self, tmp_path, monkeypatch):
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("A")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        _adir, _principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure is None
        assert not managed.slot_tainted("claude", tmp_path)


class TestProvisionalTaintSpansTheCommit:
    """A stamp exists but is unverified for the length of the commit. The taint
    has to cover that whole window, or a crash - or a Keychain read that fails
    at the end - leaves an unverified stamp fully trusted."""

    def test_registration_taints_before_writing_and_clears_after(
        self, tmp_path, monkeypatch
    ):
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("A")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )
        tainted_when_stamped: list[bool] = []
        real_stamp = managed.stamp_active_slot

        def _stamp(cli, record_id, root=None):
            tainted_when_stamped.append(managed.slot_tainted(cli, root))
            real_stamp(cli, record_id, root)

        monkeypatch.setattr(managed, "stamp_active_slot", _stamp)

        _adir, _principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure is None
        assert tainted_when_stamped == [True]  # covered while unverified
        assert not managed.slot_tainted("claude", tmp_path)  # cleared once settled

    def test_a_failed_final_read_leaves_the_stamp_untrusted(
        self, tmp_path, monkeypatch
    ):
        """A look we could not take counts as unsettled, not as settled."""
        reads = {"n": 0}

        def _blobs(cli):
            reads["n"] += 1
            if reads["n"] >= 3:
                raise managed.KeychainError("`security` timed out")
            return [_blob("A")]

        monkeypatch.setattr(managed, "canonical_slot_blobs", _blobs)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        _adir, _principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure == "slot-moved-after-write"
        assert managed.active_slot_id("claude", tmp_path) == "work-a"
        assert managed.slot_tainted("claude", tmp_path)

    def test_an_untainted_drift_repair_is_tainted_while_it_commits(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """The drift path starts with no marker at all, so without this there
        would be no window cover during its writes."""
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-a", "acct-a", tmp_path)
        assert not managed.slot_tainted("claude", tmp_path)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("A_FRESH")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )
        tainted_when_stamped: list[bool] = []
        real_stamp = managed.stamp_active_slot

        def _stamp(cli, record_id, root=None):
            tainted_when_stamped.append(managed.slot_tainted(cli, root))
            real_stamp(cli, record_id, root)

        monkeypatch.setattr(managed, "stamp_active_slot", _stamp)

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "matched" and result.record_id == "work-a"
        assert tainted_when_stamped == [True]
        assert not managed.slot_tainted("claude", tmp_path)

    def test_a_failed_final_read_during_reconcile_keeps_the_taint(
        self, fake_slot, tmp_path, monkeypatch
    ):
        by_id = _register_two(fake_slot, tmp_path)
        _bind("work-a", "acct-a", tmp_path)
        reads = {"n": 0}

        def _blobs(cli):
            reads["n"] += 1
            if reads["n"] >= 3:
                raise managed.KeychainError("denied")
            return [_blob("A_FRESH")]

        monkeypatch.setattr(managed, "canonical_slot_blobs", _blobs)
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        result = managed.reconcile_slot("claude", by_id=by_id, root=tmp_path)

        assert result.outcome == "slot-changed"
        assert managed.slot_tainted("claude", tmp_path)


class TestEveryCandidateMustProve:
    """No candidate is set aside, whatever the reason it did not prove. A 401
    rejects an ACCESS token while its refresh token may still be live, so claude
    can refresh that account straight back into the slot it reads first."""

    def test_an_unprovable_candidate_blocks_the_slot(self, tmp_path, monkeypatch):
        monkeypatch.setattr(
            managed, "canonical_slot_blobs",
            lambda cli: [_blob("REJECTED"), _blob("LIVE")],
        )

        def _principal(blob):
            if blob == _blob("REJECTED"):
                return None, "credential-rejected"
            return managed.principal_fingerprint(_profile("acct-live")), None

        monkeypatch.setattr(managed, "slot_principal", _principal)

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure == "credential-rejected" and principal is None
        # Registered unbound rather than bound to a slot we cannot vouch for.
        assert (tmp_path / "work-a" / "blob").exists()

    def test_one_candidate_that_proves_is_stored_and_bound(
        self, tmp_path, monkeypatch
    ):
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("LIVE")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-live")), None),
        )

        adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure is None and principal["account_uuid"] == "acct-live"
        assert (adir / "blob").read_text() == _blob("LIVE")

    def test_a_codex_slot_registers_without_a_principal(self, tmp_path, monkeypatch):
        """Only claude has a principal endpoint. Running a codex auth blob
        through the claude parser reported it as a dead credential and refused a
        registration that had already been written."""
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_codex_blob("cx")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: pytest.fail("resolved a claude principal for a codex slot"),
        )

        adir, principal, failure = managed.register_slot_snapshot(
            _rec("cx-a", "codex"), tmp_path
        )

        assert failure is None and principal is None
        assert adir is not None and (adir / "blob").read_text() == _codex_blob("cx")

    def test_a_duplicate_is_caught_on_the_blob_that_will_be_stored(
        self, fake_slot, tmp_path, monkeypatch
    ):
        fake_slot["claude"] = _blob("LIVE")
        managed.snapshot_current(_rec("work-a"), root=tmp_path)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("LIVE")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-live")), None),
        )

        adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-b"), tmp_path
        )

        assert failure == "duplicate-credential:work-a" and principal is None
        assert adir is None  # nothing written, reported structurally
        assert not (tmp_path / "work-b" / "blob").exists()


class TestRegisterRespectsALiveTaintWriter:
    def test_a_live_writer_blocks_registration(self, tmp_path, monkeypatch):
        """The provisional taint would otherwise discard the recorded writer,
        and it can still overwrite the slot after the final read."""
        import os

        managed._set_slot_taint("claude", tmp_path, True, [os.getpid()])
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("LIVE")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure.startswith("slot-pinned:") and str(os.getpid()) in failure
        assert principal is None
        assert not (tmp_path / "work-a" / "blob").exists()
        assert managed.tainting_pids("claude", tmp_path) == (os.getpid(),)

    def test_a_dead_writer_does_not_block(self, tmp_path, monkeypatch):
        managed._set_slot_taint("claude", tmp_path, True, [999_999])
        monkeypatch.setattr(managed.psutil, "pid_exists", lambda pid: False)
        monkeypatch.setattr(managed, "_process_started_at", lambda pid: None)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("LIVE")]
        )
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (managed.principal_fingerprint(_profile("acct-a")), None),
        )

        _adir, principal, failure = managed.register_slot_snapshot(
            _rec("work-a"), tmp_path
        )

        assert failure is None and principal["account_uuid"] == "acct-a"
        assert not managed.slot_tainted("claude", tmp_path)


class TestCaptureBeforeOverwriteAgreesWithIdentity:
    """Reconciliation may store the PROVEN (unscoped) credential while a
    scoped-first capture would write the other one straight back over it - the
    two reads must not disagree about which credential belongs to a record."""

    def test_capture_reads_the_same_candidates_identity_does(
        self, fake_slot, tmp_path, monkeypatch
    ):
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs", lambda cli: [_blob("B_ROTATED")]
        )

        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)

        assert (tmp_path / "work-b" / "blob").read_text() == _blob("B_ROTATED")

    def test_two_credentials_in_the_slot_capture_nothing(
        self, fake_slot, tmp_path, monkeypatch
    ):
        """Guessing would file another account's credential under this record -
        silent, where a lost rotated token is recoverable with a login."""
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(
            managed, "canonical_slot_blobs",
            lambda cli: [_blob("SCOPED_OTHER"), _blob("UNSCOPED_B")],
        )

        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)

        # work-b keeps its earlier snapshot rather than gaining a stranger's.
        assert (tmp_path / "work-b" / "blob").read_text() == _blob("B0")

    def test_an_empty_slot_captures_nothing(self, fake_slot, tmp_path, monkeypatch):
        by_id = _register_two(fake_slot, tmp_path)
        monkeypatch.setattr(managed, "canonical_slot_blobs", lambda cli: [])
        managed.switch(by_id["work-a"], by_id=by_id, root=tmp_path)
        assert (tmp_path / "work-b" / "blob").read_text() == _blob("B0")


class TestCredentialFileIsACandidate:
    """The usage probe reads `~/.claude/.credentials.json` FIRST, even on darwin
    where claude reads the Keychain. A stale file bearer could otherwise prove
    out and have its quota reported while the Keychain account occupies the
    slot."""

    def test_the_file_joins_the_candidate_set_on_darwin(self, tmp_path, monkeypatch):
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr(managed.sys, "platform", "darwin")
        slot = tmp_path / ".claude"
        slot.mkdir()
        (slot / ".credentials.json").write_text(_blob("FILE"))
        monkeypatch.setattr(
            managed, "_read_claude_keychain_item",
            lambda service: _blob("KEYCHAIN")
            if service == managed._CLAUDE_KEYCHAIN_SERVICE
            else None,
        )

        assert managed.canonical_slot_blobs("claude") == [
            _blob("KEYCHAIN"), _blob("FILE")
        ]

    def test_a_file_matching_the_keychain_adds_no_ambiguity(
        self, tmp_path, monkeypatch
    ):
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr(managed.sys, "platform", "darwin")
        slot = tmp_path / ".claude"
        slot.mkdir()
        (slot / ".credentials.json").write_text(_blob("SAME"))
        monkeypatch.setattr(
            managed, "_read_claude_keychain_item", lambda service: _blob("SAME")
        )

        assert managed.canonical_slot_blobs("claude") == [_blob("SAME")]

    def test_a_logged_out_file_residue_is_not_a_candidate(
        self, tmp_path, monkeypatch
    ):
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr(managed.sys, "platform", "darwin")
        slot = tmp_path / ".claude"
        slot.mkdir()
        (slot / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "", "refreshToken": ""}})
        )
        monkeypatch.setattr(
            managed, "_read_claude_keychain_item",
            lambda service: _blob("LIVE")
            if service == managed._CLAUDE_KEYCHAIN_SERVICE
            else None,
        )

        assert managed.canonical_slot_blobs("claude") == [_blob("LIVE")]
