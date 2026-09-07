"""The operator's real claude setup, end to end (x-d6be).

makers is signed into canonical ``~/.claude``. readyrule has its own
``~/.claude-alt`` whose transcript folder is a symlink back into canonical, so
both accounts read the same sessions. Switching between them is a manual sign
out and sign in that takes about a minute and costs the operator a re-enable of
remote control in every live session.

That workflow is the thing under test, not a thing to replace. Nothing here logs
in, logs out, writes a credential, or touches a setting: every test asserts the
fixture is byte-identical afterwards, symlinks included. What footnote is
allowed to do is read who is signed in and say so.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest
import yaml

from fno.adapters.providers import binding, managed
from fno.adapters.providers.dispatch import dispatch_env
from fno.adapters.providers.model import ProviderUnavailableError
from fno.agents.account_env import AccountResolutionError, resolve_account_overlay

MAKERS = {"account_uuid": "acc-makers", "organization_uuid": "org-1", "email": "makers@x"}
READYRULE = {
    "account_uuid": "acc-readyrule",
    "organization_uuid": "org-1",
    "email": "readyrule@x",
}
PRINCIPALS = {"tok-makers": MAKERS, "tok-readyrule": READYRULE}


def _cred(token: str) -> str:
    return json.dumps({"claudeAiOauth": {"accessToken": token}})


def _login(config_dir: Path, token: str) -> None:
    """What a manual `claude /login` leaves behind, and nothing else."""
    (config_dir / ".credentials.json").write_text(_cred(token))


class Fixture:
    """A disposable copy of the operator's two credential roots."""

    def __init__(self, home: Path, repo_root: Path, store: Path) -> None:
        self.home = home
        self.repo_root = repo_root
        self.store = store
        self.canonical = home / ".claude"
        self.alt = home / ".claude-alt"

    def state(self) -> dict[str, str]:
        """Every credential, transcript and setting byte, plus link targets.

        A read that quietly rewrote a credential or resolved a symlink into a
        copy would still pass an identity assertion. This is what catches it.
        """
        out: dict[str, str] = {}
        for root in (self.canonical, self.alt):
            for path in sorted(root.rglob("*")):
                key = str(path.relative_to(self.home))
                if path.is_symlink():
                    out[key] = f"link:{path.readlink()}"
                elif path.is_file():
                    out[key] = hashlib.sha256(path.read_bytes()).hexdigest()
                else:
                    out[key] = "dir"
        return out


@pytest.fixture
def fx(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Fixture:
    home = tmp_path / "home"
    canonical = home / ".claude"
    alt = home / ".claude-alt"
    (canonical / "projects").mkdir(parents=True)
    alt.mkdir()
    # The operator's real shape: readyrule reads makers' transcripts. Sharing
    # transcripts is a convenience; it never merges credential identity.
    (alt / "projects").symlink_to(canonical / "projects")
    (canonical / "projects" / "a-session.jsonl").write_text('{"type":"user"}\n')
    (canonical / ".claude.json").write_text(json.dumps({"remoteControl": True}))
    _login(canonical, "tok-makers")
    _login(alt, "tok-readyrule")

    # Under the fixture HOME, so the code under test resolves this store the
    # way it resolves the real one - no root threaded in to make it agree.
    store = home / ".fno" / "providers"
    store.mkdir(parents=True)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("FNO_STATE_DIR", str(home / ".fno"))
    # Disposable roots, so the probe reads the fixture's files and never the
    # machine's Keychain or a real account.
    import fno.adapters.providers.usage as usage_mod

    monkeypatch.setattr(managed, "_read_claude_keychain_item", lambda _s: None)
    monkeypatch.setattr(usage_mod, "_read_claude_keychain_blobs", lambda _d: [])
    monkeypatch.setattr(
        managed,
        "slot_principal",
        lambda blob: (
            (PRINCIPALS.get(json.loads(blob)["claudeAiOauth"]["accessToken"]), None)
            if blob
            else (None, "profile-unavailable")
        ),
    )
    monkeypatch.setattr(
        managed,
        "principal_of_bearer",
        lambda bearer: (
            (PRINCIPALS[bearer], None)
            if bearer in PRINCIPALS
            else (None, "profile-unavailable")
        ),
    )
    managed.write_record_principal("makers", MAKERS, store)
    managed.write_record_principal("readyrule", READYRULE, store)

    repo_root = tmp_path / "repo"
    (repo_root / ".fno").mkdir(parents=True)
    (repo_root / ".fno" / "settings.yaml").write_text(yaml.safe_dump({
        "config": {"providers": {"records": [
            {"id": "makers", "name": "Makers", "harness": "claude", "auth": "managed"},
            {"id": "readyrule", "name": "ReadyRule", "harness": "claude",
             "auth": "managed", "config_dir": str(alt)},
            {"id": "routed", "name": "Routed", "harness": "claude", "auth": "api_key",
             "env": {"ANTHROPIC_API_KEY": "k"}},
        ]}}
    }))
    return Fixture(home, repo_root, store)


def _by_id(fx: Fixture) -> dict:
    from fno.adapters.providers.loader import load_providers

    return load_providers(repo_root=fx.repo_root).by_id


def _unpinned(fx: Fixture):
    return binding.resolve_account_binding(
        None, harness="claude", root=fx.store, by_id=_by_id(fx)
    )


# --- AC3-HP -----------------------------------------------------------------


def test_a_manual_canonical_login_moves_the_binding_and_touches_nothing(
    fx: Fixture,
) -> None:
    """The whole operator path: sign out of makers, sign in as readyrule, and
    the next unpinned launch reads the account that is actually there."""
    assert _unpinned(fx).matched_record == "makers"

    _login(fx.canonical, "tok-readyrule")  # the manual switch, by hand
    before = fx.state()

    got = _unpinned(fx)

    assert got.status == binding.MATCHED
    assert got.matched_record == "readyrule"
    assert got.observed_label == "readyrule@x"
    assert fx.state() == before  # credentials, transcripts, symlink, settings
    assert (fx.alt / "projects").is_symlink()
    assert json.loads((fx.canonical / ".claude.json").read_text())["remoteControl"]


def test_a_stale_stamp_does_not_survive_the_login(fx: Fixture) -> None:
    """The stamp still names makers. It is not evidence, and saying so is the
    whole job: an untainted wrong stamp is what bills the wrong account."""
    managed.stamp_active_slot("claude", "makers", fx.store)
    _login(fx.canonical, "tok-readyrule")
    before = fx.state()

    assert managed.active_slot_id("claude", fx.store) == "makers"
    assert _unpinned(fx).matched_record == "readyrule"
    assert fx.state() == before


# --- AC1-EDGE ---------------------------------------------------------------


def test_shared_transcripts_keep_the_two_roots_distinct(fx: Fixture) -> None:
    canonical_blobs = binding.credential_blobs("claude", None)
    alt_blobs = binding.credential_blobs("claude", fx.alt)

    assert canonical_blobs == [_cred("tok-makers")]
    assert alt_blobs == [_cred("tok-readyrule")]


# --- AC2-ERR / AC2-HP: both launch env paths --------------------------------


def test_a_matching_pin_launches_on_its_own_dir(fx: Fixture) -> None:
    overlay = resolve_account_overlay(
        "readyrule", repo_root=fx.repo_root, providers_root=fx.store
    )
    env = dispatch_env("readyrule", repo_root=fx.repo_root, root=fx.store)

    assert overlay.env == {"CLAUDE_CONFIG_DIR": str(fx.alt)}
    assert env == {"CLAUDE_CONFIG_DIR": str(fx.alt)}


def test_a_mismatching_pin_is_refused_by_both_launch_paths(fx: Fixture) -> None:
    """The operator signed the wrong account into the alt dir. Both paths say so
    in the same words, because both read the same binding."""
    _login(fx.alt, "tok-makers")
    before = fx.state()

    with pytest.raises(AccountResolutionError) as overlay_exc:
        resolve_account_overlay(
            "readyrule", repo_root=fx.repo_root, providers_root=fx.store
        )
    with pytest.raises(ProviderUnavailableError) as dispatch_exc:
        dispatch_env("readyrule", repo_root=fx.repo_root, root=fx.store)

    for exc in (overlay_exc, dispatch_exc):
        assert binding.MISMATCH_RECEIPT in str(exc.value)
        assert "serves makers" in str(exc.value)
    assert fx.state() == before


def test_an_api_routed_record_is_not_an_ambient_subscription_question(
    fx: Fixture,
) -> None:
    """A provider endpoint override selects an API route. It does not make the
    signed-in Anthropic subscription the thing being billed."""
    got = binding.resolve_account_binding(
        _by_id(fx)["routed"], root=fx.store, by_id=_by_id(fx)
    )

    assert got.reason == "api-key-route"
    assert dispatch_env("routed", repo_root=fx.repo_root, root=fx.store) == {
        "ANTHROPIC_API_KEY": "k"
    }


# --- AC3-EDGE ---------------------------------------------------------------


def test_unreadable_principal_evidence_reads_unknown_with_its_cause(
    fx: Fixture, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Not proved is not healthy, and it is not a successful switch either."""
    monkeypatch.setattr(
        managed, "slot_principal", lambda _b: (None, "profile-unavailable")
    )
    before = fx.state()

    got = _unpinned(fx)

    assert got.status == binding.UNKNOWN
    assert got.reason == "profile-unavailable"
    assert got.matched_record is None
    assert binding.UNKNOWN_RECEIPT in got.receipt
    assert "makers" not in got.receipt and "readyrule" not in got.receipt
    assert fx.state() == before


def test_an_unproven_identity_does_not_ground_an_unpinned_launch(
    fx: Fixture, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The permissive posture for manual canonical launches stands. What unknown
    forbids is a receipt naming the account, never the launch."""
    managed.stamp_active_slot("claude", "makers", fx.store)
    monkeypatch.setattr(
        managed, "slot_principal", lambda _b: (None, "profile-unavailable")
    )

    overlay = resolve_account_overlay(
        "makers", repo_root=fx.repo_root, providers_root=fx.store
    )

    assert overlay.lane == "managed-active"


# --- AC2-RACE ---------------------------------------------------------------


def test_a_sign_in_during_the_probe_discards_the_reading(
    fx: Fixture, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The reading is real and it is the wrong account's. identity_changed is
    the marker that says so; an absent snapshot would read as a probe that
    never ran."""
    import fno.adapters.providers.usage as usage_mod

    managed.stamp_active_slot("claude", "makers", fx.store)
    record = _by_id(fx)["makers"]

    class _Resp:
        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

        def read(self):
            # The operator signs in as readyrule while the request is in flight.
            _login(fx.canonical, "tok-readyrule")
            return json.dumps({"five_hour": {"utilization": 30.0}}).encode()

    monkeypatch.setattr(usage_mod.urllib.request, "urlopen", lambda *a, **k: _Resp())
    monkeypatch.setattr(usage_mod, "_load_records", lambda: _by_id(fx))

    snap, reason = usage_mod._probe_claude(record, now=1000.0)

    assert snap is None and reason == "identity_changed"
    assert _unpinned(fx).matched_record == "readyrule"
