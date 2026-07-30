"""A logged-out credential residue must not read as a valid login (x-fd8f).

claude/darwin keeps a non-empty Keychain blob after logout: claudeAiOauth
retains scopes/subscriptionType/rateLimitTier while accessToken/refreshToken
clear to ''. _read_slot_blob returned the first non-empty blob, so register
snapshotted the residue and `accounts use ... (verified)` lied. The gate is a
real credential (access OR refresh token), falling through to the next service.

Run: cd cli && uv run pytest tests/unit/test_managed_slot_blob.py -v
"""
from __future__ import annotations

import json
import subprocess

from fno.adapters.providers import managed


def _residue() -> str:
    """The observed logged-out shape: tokens empty, metadata non-empty."""
    return json.dumps(
        {
            "claudeAiOauth": {
                "accessToken": "",
                "refreshToken": "",
                "scopes": ["org:create_api_key"],
                "subscriptionType": "max",
                "rateLimitTier": "tier_4",
                "expiresAt": 0,
            }
        }
    )


def _live(token: str = "sk-live") -> str:
    return json.dumps({"claudeAiOauth": {"accessToken": token, "refreshToken": "rt"}})


# ---------------------------------------------------------------------------
# _token_present: the root-cause gate (platform-independent)
# ---------------------------------------------------------------------------


class TestTokenPresent:
    def test_logged_out_residue_is_not_a_login(self) -> None:
        assert managed._token_present(_residue()) is False

    def test_non_empty_dict_without_a_token_is_not_a_login(self) -> None:
        # The old `or data` fallback returned True for any non-empty dict.
        assert managed._token_present(json.dumps({"claudeAiOauth": {"scopes": ["x"]}})) is False
        assert managed._token_present(json.dumps({"subscriptionType": "max"})) is False

    def test_access_token_is_a_login(self) -> None:
        assert managed._token_present(_live()) is True

    def test_refresh_token_alone_is_a_login(self) -> None:
        # A refresh token mints access tokens, so it is a usable credential.
        assert (
            managed._token_present(json.dumps({"claudeAiOauth": {"refreshToken": "rt-only"}}))
            is True
        )

    def test_top_level_access_token_is_a_login(self) -> None:
        assert managed._token_present(json.dumps({"accessToken": "top-level"})) is True

    def test_opaque_blob_is_a_login(self) -> None:
        # Tolerance for an opaque keychain blob (a raw key with no JSON shape).
        assert managed._token_present("sk-opaque-key") is True

    def test_empty_is_not_a_login(self) -> None:
        assert managed._token_present("") is False
        assert managed._token_present("   ") is False


# ---------------------------------------------------------------------------
# _read_slot_blob: fall through a scoped residue to the live token (darwin)
# ---------------------------------------------------------------------------


def _darwin_slot(monkeypatch, services: dict[str, str]) -> None:
    """Pin the claude/darwin read path to a {service: stdout} fake and force the
    darwin branch regardless of host platform."""
    monkeypatch.setattr(managed.sys, "platform", "darwin")
    monkeypatch.setattr(managed, "_claude_keychain_account", lambda: "acct")
    monkeypatch.setattr(managed, "_claude_scoped_service", lambda cfg: "scoped")

    def _run(argv: list[str]) -> subprocess.CompletedProcess:
        svc = argv[argv.index("-s") + 1]
        out = services.get(svc, "")
        return subprocess.CompletedProcess(argv, 0 if out else 1, stdout=out, stderr="")

    monkeypatch.setattr(managed, "_run_security", _run)


class TestReadSlotBlobResidue:
    def test_falls_through_scoped_residue_to_unscoped_live(self, tmp_path, monkeypatch) -> None:
        # The observed state: scoped item holds the residue, unscoped holds the
        # live token. The reader must NOT stop at the scoped residue.
        _darwin_slot(
            monkeypatch,
            {"scoped": _residue(), managed._CLAUDE_KEYCHAIN_SERVICE: _live()},
        )
        assert managed._read_slot_blob("claude", config_dir=tmp_path) == _live()

    def test_residue_only_returns_none(self, tmp_path, monkeypatch) -> None:
        # No live token anywhere: register must refuse, not store the residue.
        _darwin_slot(monkeypatch, {"scoped": _residue()})
        assert managed._read_slot_blob("claude", config_dir=tmp_path) is None

    def test_live_in_scoped_is_returned(self, tmp_path, monkeypatch) -> None:
        _darwin_slot(monkeypatch, {"scoped": _live()})
        assert managed._read_slot_blob("claude", config_dir=tmp_path) == _live()

    def test_empty_slot_returns_none(self, tmp_path, monkeypatch) -> None:
        _darwin_slot(monkeypatch, {})
        assert managed._read_slot_blob("claude", config_dir=tmp_path) is None
