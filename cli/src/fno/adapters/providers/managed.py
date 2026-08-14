"""Managed credential store for the single-slot multi-account substrate.

Group ``managed-store`` of the multi-account epic (US1 register, US2 switch).

A ``managed`` ProviderRecord shares ONE config slot (``~/.claude`` on darwin,
the codex ``auth.json`` for codex) across accounts. Each registered account has
its login snapshotted into ``~/.fno/providers/<id>/`` (dir 700, blob 600) and
materialized back into the slot on switch. Two guards make a switch safe:

  1. capture-before-overwrite: re-snapshot the OUTGOING account's current
     (rotated) slot blob into its store BEFORE overwriting the slot, so its
     fresh OAuth refresh token is never lost.
  2. live-pin gate: never rotate credentials out from under a live CLI process
     using that slot (orca's live-pty-gate lesson). A pinned slot defers.

The slot read/write and the verification are behind small module-level
functions (``_read_slot_blob`` / ``_write_slot_blob`` / ``verify_slot``) so
tests exercise the orchestration without touching the real Keychain or network.

Auto-switch (US3) and session revival (US4/US5) build on this store in later
groups. codex parity (US6) is complete: the slot backend, snapshot, switch, and
the live-pin gate all dispatch per-CLI (claude Keychain/credential-file, codex
``auth.json`` file copy; live-pin keyed off ``CODEX_HOME`` for codex).
"""
from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional, Sequence

import filelock
import psutil

from fno.adapters.providers.model import ProviderRecord

# macOS Keychain item claude reads (mirrors usage.py._CLAUDE_KEYCHAIN_SERVICE).
_CLAUDE_KEYCHAIN_SERVICE = "Claude Code-credentials"
_SECURITY_TIMEOUT_S = 5  # ponytail: same 5s ceiling usage.py uses for `security`
_CODEX_LOGIN_TIMEOUT_S = 5
_CODEX_AUTH_ENV_VARS = ("CODEX_ACCESS_TOKEN", "CODEX_API_KEY", "OPENAI_API_KEY")

# The claude OAuth profile endpoint. Verified live 2026-08-03: it answers the
# one question the store cannot answer about itself - WHO the credential now in
# the slot belongs to - as non-secret identity (`account.uuid`, `organization`,
# `email`), with no token echoed back. It is what makes an out-of-band
# `claude /login` observable at all: that path writes the Keychain directly and
# tells footnote nothing, so the stamp is the state known to drift.
_PROFILE_URL = "https://api.anthropic.com/api/oauth/profile"
_PROFILE_TIMEOUT_S = 10  # same budget usage.py gives the sibling usage endpoint
_PROFILE_USER_AGENT = "claude-code/2.1.0"  # a custom UA risks being rejected
# How long a REFUSED auto-reconcile is backed off. Only failures are cached, and
# only as backoff, never as proof: a success clears both the taint and this file.
_RECONCILE_BACKOFF_S = 300
# How long PROVEN slot-principal evidence stays good. Bounds the cost of
# checking a stamp against the live credential on every fresh probe.
_PRINCIPAL_TTL_S = 900


class ManagedStoreError(RuntimeError):
    """A managed-store operation failed with a receipt (never a silent partial)."""


class KeychainError(ManagedStoreError):
    """The macOS ``security`` tool denied, errored, or timed out.

    Raised instead of hanging (AC2-ERR): the caller surfaces the receipt. The
    two-item darwin write orders the CLI-preferred (scoped) item last, so a
    mid-write failure leaves the CLI reading a consistent pre-switch token
    rather than a half-applied target.
    """


class SwitchDeferred(ManagedStoreError):
    """The switch could not run now (live-pin gate or mutex contention).

    Carries the pinning sessions so the caller names them. No credential in the
    slot or any store was modified (AC1-ERR).
    """

    def __init__(self, message: str, sessions: Optional[list["PinningSession"]] = None):
        super().__init__(message)
        self.sessions = sessions or []


@dataclass(frozen=True)
class PinningSession:
    """A live process pinning a shared slot.

    ``started`` is sampled during the scan, not later: between a scan and the
    taint write a process can exit and its pid be recycled, and a start time
    read afterwards would describe the replacement - fingerprinting exactly the
    process the start time was added to exclude.
    """

    pid: int
    cmdline: str
    started: Optional[float] = None


# ---------------------------------------------------------------------------
# Store layout
# ---------------------------------------------------------------------------


def store_root() -> Path:
    """Root of the managed store: ``<state_dir>/providers`` (default ~/.fno/providers).

    Routed through ``fno.paths`` (no bare ``~/.fno`` fallback): this is only
    reached from the register/use/switch CLI commands, well after config load,
    so ``state_dir()`` is always resolvable here."""
    from fno import paths as _paths

    return _paths.state_dir() / "providers"


def account_dir(record_id: str, root: Path | None = None) -> Path:
    return (root or store_root()) / record_id


def _blob_path(record_id: str, root: Path | None = None) -> Path:
    return account_dir(record_id, root) / "blob"


def _meta_path(record_id: str, root: Path | None = None) -> Path:
    return account_dir(record_id, root) / "meta.json"


def _active_stamp_path(cli: str, root: Path | None = None) -> Path:
    """The id currently materialized in a CLI's slot (capture-before-overwrite target).

    Per-CLI (``.active-claude`` / ``.active-codex``): each CLI has its own slot,
    so a single global stamp would let a codex switch make a later claude switch
    capture the wrong (codex) slot and lose the claude account's token."""
    return (root or store_root()) / f".active-{cli}"


def _switch_lock_path(root: Path | None = None) -> Path:
    return (root or store_root()) / ".switch.lock"


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _atomic_write_private(path: Path, content: str, mode: int = 0o600) -> None:
    """temp+rename write with a private mode, so a crash never leaves a partial
    blob and the secret never lands world-readable (AC1-FR atomicity)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp")
    tmp = Path(tmp_name)
    fd_open = False
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fd_open = True  # fdopen now owns fd; its context manager closes it
            fh.write(content)
        os.replace(tmp, path)
    except BaseException:
        if not fd_open:  # os.fchmod raised before fdopen took ownership - close it ourselves
            try:
                os.close(fd)
            except OSError:
                pass
        tmp.unlink(missing_ok=True)
        raise


# ---------------------------------------------------------------------------
# Slot backends (the testable seam). Read/write the credential the CLI reads.
# ---------------------------------------------------------------------------


def _claude_keychain_account() -> str:
    return os.environ.get("USER") or os.environ.get("USERNAME") or "user"


def _claude_scoped_service(config_dir: Path) -> str:
    suffix = hashlib.sha256(str(config_dir).encode()).hexdigest()[:8]
    return f"{_CLAUDE_KEYCHAIN_SERVICE}-{suffix}"


def _run_security(args: list[str]) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(
            ["security", *args], capture_output=True, text=True, timeout=_SECURITY_TIMEOUT_S
        )
    except subprocess.TimeoutExpired as exc:
        raise KeychainError(f"`security {args[0]}` timed out after {_SECURITY_TIMEOUT_S}s") from exc
    except OSError as exc:
        raise KeychainError(f"`security {args[0]}` failed to run: {exc}") from exc


def _claude_slot_config_dir() -> Path:
    """The shared slot config dir a managed claude account materializes into."""
    override = os.environ.get("CLAUDE_CONFIG_DIR")
    return Path(override) if override else Path.home() / ".claude"


def _codex_slot_auth_path() -> Path:
    home = os.environ.get("CODEX_HOME")
    return (Path(home) if home else Path.home() / ".codex") / "auth.json"


def _read_slot_blob(cli: str, config_dir: Path | None = None) -> Optional[str]:
    """Read the credential blob the CLI currently reads from its slot.

    claude/darwin: the Keychain item (scoped-for-dir first, unscoped fallback).
    claude/linux + codex: the on-disk credential file. Returns None when no
    login exists (register/capture then refuse rather than store an empty blob).
    """
    if cli == "codex":
        try:
            return _codex_slot_auth_path().read_text(encoding="utf-8")
        except OSError:
            return None

    # claude
    default_cfg = _claude_slot_config_dir()
    cfg = config_dir or default_cfg
    return _read_claude_blob(cfg, shared=cfg == default_cfg)


def read_canonical_slot_blob(cli: str) -> Optional[str]:
    """The credential a reader of the SHARED slot gets, ignoring ambient overrides.

    ``_read_slot_blob`` honors ``CLAUDE_CONFIG_DIR``, which is right for an
    operator verb writing the slot and wrong for an identity read: a worker
    pinned to another account exports it, and reconciliation would then prove
    the pinned account and stamp it onto the canonical slot.
    """
    blobs = canonical_slot_blobs(cli)
    return blobs[0] if blobs else None


# `security find-generic-password` exits 44 for errSecItemNotFound (verified on
# darwin 25.3). Any OTHER nonzero status is a read that FAILED - denied, locked
# keychain, a broken tool - which is a different thing entirely.
_SECURITY_ITEM_NOT_FOUND = 44


def _read_claude_keychain_item(service: str) -> Optional[str]:
    """One Keychain item's blob, or None when absent or a logged-out residue.

    A read that FAILS raises rather than reporting absence. Collapsing the two
    would silently drop a candidate: if the scoped item holds account B but the
    read is denied while the unscoped item holds A, the slot would look
    unambiguous, A would be proven and stamped and the taint cleared - with
    claude still able to read B. An unreadable source has to stop attribution,
    not shrink the set it is computed over.
    """
    out = _run_security(
        ["find-generic-password", "-s", service, "-a", _claude_keychain_account(), "-w"]
    )
    if out.returncode == _SECURITY_ITEM_NOT_FOUND:
        return None
    if out.returncode != 0:
        raise KeychainError(
            f"`security find-generic-password -s {service}` exited "
            f"{out.returncode}: {out.stderr.strip()}"
        )
    blob = out.stdout.strip()
    if not blob:
        return None
    return blob if _token_present(blob) else None


def canonical_slot_blobs(cli: str) -> list[str]:
    """Every distinct credential the SHARED slot can present, ignoring overrides.

    darwin keeps TWO Keychain items for the canonical dir, scoped and unscoped,
    and they can hold different accounts - a stale scoped item beside a live
    unscoped one is the observed reality, and the reason the usage probe tries
    several bearers. The on-disk ``.credentials.json`` is a third source, read
    first by that probe. All of them are candidates: proving one and stamping it
    would trust one account while a reader gets another.
    """
    if cli != "claude":
        blob = _read_slot_blob(cli)
        return [blob] if blob and blob.strip() else []
    canonical = Path.home() / ".claude"
    if sys.platform != "darwin":
        blob = _read_claude_blob(canonical, shared=True)
        return [blob] if blob and blob.strip() else []
    out: list[str] = []
    for service in (_claude_scoped_service(canonical), _CLAUDE_KEYCHAIN_SERVICE):
        blob = _read_claude_keychain_item(service)
        if blob and blob not in out:
            out.append(blob)
    # The on-disk credential file counts too, even on darwin where claude reads
    # the Keychain: the usage probe reads it FIRST, so a stale file bearer could
    # prove out and have its quota reported while the Keychain account is the
    # one actually occupying the slot. The candidate set has to be every source
    # anything reads, or "is this slot unambiguous" answers a narrower question
    # than the one that matters.
    try:
        blob = (canonical / ".credentials.json").read_text(encoding="utf-8")
    except OSError:
        blob = ""
    if blob.strip() and _token_present(blob) and blob not in out:
        out.append(blob)
    return out


def canonical_slot_principal(cli: str) -> tuple[Optional[dict], Optional[str]]:
    """The one principal the shared slot presents, or a typed failure."""
    principal, _blob, failure = canonical_slot_identity(cli)
    return principal, failure


def canonical_slot_identity(
    cli: str,
) -> tuple[Optional[dict], Optional[str], Optional[str]]:
    """``(principal, the blob it was proven from, failure)`` for the shared slot."""
    return principal_of_blobs(canonical_slot_blobs(cli))


def principal_of_blobs(
    blobs: list[str],
) -> tuple[Optional[dict], Optional[str], Optional[str]]:
    """``(principal, proven_blob, failure)`` for a captured set of credentials.

    Takes the bytes rather than re-reading the slot so a caller can prove, store
    and bind THE SAME credential: proving one read and snapshotting another is
    how account A's identity ends up bound to account B's blob.

    EVERY distinct candidate must prove, and they must all name one account.
    Anything less is not attributable: whichever credential we stamped, some
    reader could get the other one - and on macOS claude reads the scoped item
    first while the usage probe reads the unscoped one, so "some reader" is not
    hypothetical. ``ambiguous-slot`` (two accounts) is therefore not a tie to
    break, and a candidate that failed to prove is not one to set aside.
    """
    if not blobs:
        return None, None, "no-slot-credential"
    resolved: list[tuple[dict, str]] = []
    for blob in blobs:
        principal, failure = slot_principal(blob)
        if principal is None:
            # No candidate is set aside, whatever the reason it did not prove.
            # A 401 rejects an ACCESS token while its refresh token may still be
            # live, so claude can refresh that account straight back into the
            # slot it reads first; and an unanswered call says nothing at all.
            # Either way we cannot show the slot holds one account, which is the
            # only thing that makes it attributable.
            return None, None, failure
        resolved.append((principal, blob))
    keys = {identity_key(principal) for principal, _blob in resolved}
    if None in keys:
        return None, None, "malformed-profile"
    if len(keys) > 1:
        return None, None, "ambiguous-slot"
    principal, blob = resolved[0]
    return principal, blob, None


def _read_claude_blob(cfg: Path, *, shared: bool) -> Optional[str]:
    """Read claude's credential for ``cfg``; ``shared`` allows the unscoped item.

    ``shared`` is a parameter rather than a ``cfg == default`` test inside so a
    canonical read stays canonical under an ambient override: the unscoped
    Keychain item belongs to the shared slot, and whether we may read it is a
    property of which slot we asked for, not of the environment.
    """
    if sys.platform == "darwin":
        acct = _claude_keychain_account()
        # The unscoped Keychain item belongs to the default ~/.claude account
        # (account_env._login_present reads the scoped item ONLY, never the
        # unscoped fallback, for exactly this reason). Fall back to it only when
        # reading that account; for an alternate config_dir the credential lives
        # only in its scoped item, so falling through would return the default
        # account's credential under the alternate dir (a misattribution).
        services = [_claude_scoped_service(cfg)]
        if shared:
            services.append(_CLAUDE_KEYCHAIN_SERVICE)
        for service in services:
            out = _run_security(["find-generic-password", "-s", service, "-a", acct, "-w"])
            if out.returncode == 0 and out.stdout.strip():
                blob = out.stdout.strip()
                # A logged-out residue is non-empty JSON (scopes/subscriptionType/
                # rateLimitTier survive while accessToken/refreshToken clear to ''),
                # so a presence check alone returns it as a live login. Require a
                # real credential and fall through to the next service - the scoped
                # item may hold the residue while the unscoped fallback holds the
                # live token - else None, so register refuses rather than stores it.
                if _token_present(blob):
                    return blob
        return None
    try:
        blob = (cfg / ".credentials.json").read_text(encoding="utf-8")
    except OSError:
        return None
    return blob if _token_present(blob) else None


def _write_slot_blob(cli: str, blob: str, config_dir: Path | None = None) -> None:
    """Materialize ``blob`` into the slot the CLI reads.

    claude/darwin: write BOTH the config-dir-scoped item and the unscoped
    fallback (a stale scoped item + a live unscoped is the observed reality;
    writing both guarantees claude reads a consistent token, pitfall 2).
    claude/linux + codex: overwrite the credential file (0600)."""
    if cli == "codex":
        try:
            _atomic_write_private(_codex_slot_auth_path(), blob)
        except OSError as exc:
            raise ManagedStoreError(f"failed to write codex credential to slot: {exc}") from exc
        return

    cfg = config_dir or _claude_slot_config_dir()
    if sys.platform == "darwin":
        acct = _claude_keychain_account()
        # Write the unscoped fallback FIRST and the config-dir-scoped item LAST.
        # claude (and _read_slot_blob) read scoped-first for a dir, so if the
        # second write fails the scoped item still holds the PRE-switch token:
        # both the CLI and the next capture-before-overwrite read the correct
        # outgoing creds, never a half-applied target (no corruption on partial).
        for service in (_CLAUDE_KEYCHAIN_SERVICE, _claude_scoped_service(cfg)):
            # -U updates in place if the item exists. Blob rides argv: a known
            # ponytail ceiling (ps exposure on a single-user box); `security`
            # has no stdin password path for add-generic-password.
            out = _run_security(
                ["add-generic-password", "-U", "-s", service, "-a", acct, "-w", blob]
            )
            if out.returncode != 0:
                raise KeychainError(
                    f"`security add-generic-password -s {service}` exited "
                    f"{out.returncode}: {out.stderr.strip()}"
                )
        return
    try:
        cfg.mkdir(parents=True, exist_ok=True)
        _atomic_write_private(cfg / ".credentials.json", blob)
    except OSError as exc:
        raise ManagedStoreError(f"failed to write credential to slot: {exc}") from exc


def verify_slot(record: ProviderRecord, expected_blob: str) -> bool:
    """Post-materialize verification: the slot now reads back the blob we wrote
    and it carries a parseable token. Catches a silently half-applied write
    (scoped/unscoped mismatch). US3 auto-switch strengthens this to a live
    network probe; a manual `use` re-reads what the CLI would read."""
    got = _read_slot_blob(record.harness)
    if got is None or got.strip() != expected_blob.strip():
        return False
    if record.harness == "codex":
        return _codex_auth_present(got)
    return _token_present(got)


@dataclass(frozen=True)
class _CodexLoginResult:
    ok: Optional[bool]
    reason: Optional[str] = None


def _codex_login_ok() -> _CodexLoginResult:
    """Ask Codex to recognize the materialized auth schema in its exact home."""
    env = os.environ.copy()
    for name in _CODEX_AUTH_ENV_VARS:
        env.pop(name, None)
    env["CODEX_HOME"] = str(_codex_slot_auth_path().parent)
    try:
        result = subprocess.run(
            ["codex", "login", "status"],
            capture_output=True,
            text=True,
            timeout=_CODEX_LOGIN_TIMEOUT_S,
            env=env,
        )
    except FileNotFoundError:
        return _CodexLoginResult(ok=None, reason="codex-login-status-missing")
    except subprocess.TimeoutExpired:
        return _CodexLoginResult(ok=None, reason="codex-login-status-timeout")
    except OSError as exc:
        raise ManagedStoreError(f"`codex login status` failed to run: {exc}") from exc
    return _CodexLoginResult(ok=result.returncode == 0)


def _token_present(blob: str) -> bool:
    """A materialized blob must carry a usable credential (access OR refresh token).

    Mirrors usage.py's tolerance for Claude credentials: the token can sit at
    a couple of known paths, and an opaque non-JSON keychain blob counts. A
    logged-out residue (non-empty JSON whose accessToken/refreshToken cleared to
    '') does NOT count: returning it as a login is the register-logged-out-blob
    bug, so the dict fallback is a real token field, not 'any non-empty dict'."""
    try:
        data = json.loads(blob)
    except (ValueError, TypeError):
        return bool(blob.strip())
    if not isinstance(data, dict):
        return bool(blob.strip())
    oauth = data.get("claudeAiOauth")
    if isinstance(oauth, dict) and (oauth.get("accessToken") or oauth.get("refreshToken")):
        return True
    return bool(data.get("accessToken") or data.get("access_token"))


def _codex_auth_present(blob: str) -> bool:
    """Require credential material for Codex's effective AuthDotJson mode."""
    try:
        data = json.loads(blob)
    except (ValueError, TypeError):
        return False
    if not isinstance(data, dict):
        return False

    def nonempty(value: object) -> bool:
        return isinstance(value, str) and bool(value.strip())

    def tokens_present(*, refresh_required: bool) -> bool:
        tokens = data.get("tokens")
        if not isinstance(tokens, dict):
            return False
        if not all(nonempty(tokens.get(field)) for field in ("access_token", "id_token")):
            return False
        refresh = tokens.get("refresh_token")
        return nonempty(refresh) if refresh_required else isinstance(refresh, str)

    def identity_present() -> bool:
        identity = data.get("agent_identity")
        if nonempty(identity):
            return True
        if not isinstance(identity, dict):
            return False
        return (
            all(
                nonempty(identity.get(field))
                for field in (
                    "agent_runtime_id",
                    "agent_private_key",
                    "account_id",
                    "chatgpt_user_id",
                )
            )
            and nonempty(identity.get("plan_type"))
            and isinstance(identity.get("chatgpt_account_is_fedramp"), bool)
        )

    def bedrock_present() -> bool:
        bedrock = data.get("bedrock_api_key")
        return isinstance(bedrock, dict) and all(
            nonempty(bedrock.get(field)) for field in ("api_key", "region")
        )

    mode = data.get("auth_mode")
    if mode is None:
        if data.get("personal_access_token") is not None:
            mode = "personalAccessToken"
        elif data.get("bedrock_api_key") is not None:
            mode = "bedrockApiKey"
        elif data.get("OPENAI_API_KEY") is not None:
            mode = "apikey"
        else:
            mode = "chatgpt"

    if mode == "apikey":
        return nonempty(data.get("OPENAI_API_KEY"))
    if mode == "chatgpt":
        return tokens_present(refresh_required=True)
    if mode == "chatgptAuthTokens":
        return tokens_present(refresh_required=False)
    if mode == "agentIdentity":
        return identity_present()
    if mode == "personalAccessToken":
        return nonempty(data.get("personal_access_token"))
    if mode == "bedrockApiKey":
        return bedrock_present()
    return False


# ---------------------------------------------------------------------------
# Live-pin gate
# ---------------------------------------------------------------------------


def _pinning_sessions(
    *,
    looks_like: Callable[[Optional[str], list[str]], bool],
    env_var: str,
    slot_dir: Path,
    default_dir: Path,
) -> list[PinningSession]:
    """Live processes pinning a shared slot dir, generic over the CLI.

    A process pins when its effective ``env_var`` resolves to ``slot_dir`` (a
    process on its own dir does NOT pin the shared slot). Conservative on
    ambiguity: a matching process whose environ is unreadable, or whose slot
    override cannot be resolved, is treated as pinning - deferring a switch is
    safe, rotating credentials under a live session corrupts it.
    """
    slot = _safe_resolve(slot_dir) or slot_dir
    default_resolved = _safe_resolve(default_dir) or default_dir
    me = os.getpid()
    found: list[PinningSession] = []
    for proc in psutil.process_iter(["pid", "name", "cmdline", "create_time"]):
        try:
            if proc.info["pid"] == me:
                continue
            cmdline = proc.info.get("cmdline") or []
            if not looks_like(proc.info.get("name"), cmdline):
                continue
            try:
                env = proc.environ()
            except Exception:  # noqa: BLE001 - unreadable env: assume it pins the default slot
                found.append(_pinning_session(proc, cmdline))
                continue
            override = env.get(env_var)
            proc_dir = _safe_resolve(Path(override)) if override else default_resolved
            # Resolve both sides so a symlinked/relative path still matches; an
            # unresolvable proc dir (proc_dir is None) is treated as pinning
            # (conservative: under-detecting a live session is the unsafe way).
            if proc_dir is None or proc_dir == slot:
                found.append(_pinning_session(proc, cmdline))
        except Exception:  # noqa: BLE001 - a vanished/denied process is not our switch's problem
            continue
    return found


def _pinning_session(proc, cmdline: list[str]) -> PinningSession:
    started = proc.info.get("create_time")
    return PinningSession(
        proc.info["pid"],
        " ".join(cmdline),
        float(started) if isinstance(started, (int, float)) else None,
    )


def pinning_sessions(config_dir: Path | None = None) -> list[PinningSession]:
    """Live claude processes pinning the slot ``config_dir`` (default ~/.claude)
    via their effective ``CLAUDE_CONFIG_DIR``."""
    return _pinning_sessions(
        looks_like=_looks_like_claude,
        env_var="CLAUDE_CONFIG_DIR",
        slot_dir=config_dir or _claude_slot_config_dir(),
        default_dir=Path.home() / ".claude",
    )


def codex_pinning_sessions(auth_path: Path | None = None) -> list[PinningSession]:
    """Live codex processes pinning the slot via their effective ``CODEX_HOME``.

    The codex slot is a file (``auth.json``); the pin is on its parent dir
    (``CODEX_HOME``), so a process whose ``CODEX_HOME`` resolves to that dir
    pins the slot. Same conservative-on-ambiguity posture as claude."""
    slot_dir = (auth_path or _codex_slot_auth_path()).parent
    return _pinning_sessions(
        looks_like=_looks_like_codex,
        env_var="CODEX_HOME",
        slot_dir=slot_dir,
        default_dir=Path.home() / ".codex",
    )


def pinning_sessions_for(cli: str) -> list[PinningSession]:
    """Dispatch the live-pin scan to the matcher for ``cli``'s slot.

    Only claude and codex have a managed slot + a matcher. Any other cli is
    refused HERE (this runs first in _switch_locked, before any slot is read or
    written): without a matcher we cannot prove the slot is unpinned, and the
    downstream slot ops would otherwise mis-route it to the claude slot and
    corrupt it. Fail loud with a receipt rather than a silent claude fallback."""
    if cli == "codex":
        return codex_pinning_sessions()
    if cli == "claude":
        return pinning_sessions()
    raise ManagedStoreError(
        f"managed account switching is not supported for cli '{cli}' "
        "(only claude and codex have a managed credential slot)"
    )


def _safe_resolve(p: Path) -> Optional[Path]:
    """Resolve symlinks/relative segments; None if the path can't be resolved."""
    try:
        return p.resolve()
    except OSError:
        return None


def _looks_like_claude(name: Optional[str], cmdline: list[str]) -> bool:
    # ponytail: matches the standalone `claude` binary (today's distribution).
    # A node-launched `.../cli.js` would slip past; upgrade to matching the
    # claude entrypoint path if that distribution reappears (US3 daemon needs it).
    if name and Path(name).name == "claude":
        return True
    for part in cmdline:
        toks = part.split() if part else []
        if toks and Path(toks[0]).name == "claude":
            return True
    return False


def _looks_like_codex(name: Optional[str], cmdline: list[str]) -> bool:
    # ponytail: matches the standalone `codex` binary by name or argv[0] only -
    # NOT any arg, or `grep codex` / `git commit -m "codex fix"` would false-match
    # (and spuriously defer a switch when CODEX_HOME is exported). A node-launched
    # wrapper slips past; upgrade to the entrypoint path if that distribution appears.
    if name and Path(name).name == "codex":
        return True
    if cmdline:
        toks = cmdline[0].split() if cmdline[0] else []
        if toks and Path(toks[0]).name == "codex":
            return True
    return False


# ---------------------------------------------------------------------------
# Snapshot (register + capture)
# ---------------------------------------------------------------------------


def snapshot_current(record: ProviderRecord, root: Path | None = None) -> Path:
    """Snapshot the CURRENT slot login into the record's store (dir 700, blob 600).

    Used by register (first snapshot) and by capture-before-overwrite (re-snapshot
    the outgoing account before a switch). Raises when no login exists to capture
    (US1 boundary: never store an empty blob)."""
    blob = _read_slot_blob(record.harness)
    if blob is None or not blob.strip():
        raise ManagedStoreError(
            f"no current {record.harness} login to snapshot for '{record.id}' "
            "(sign in first, then register)"
        )
    return write_snapshot(record, blob, root)


def write_snapshot(record: ProviderRecord, blob: str, root: Path | None = None) -> Path:
    """Store ``blob`` as ``record``'s snapshot (dir 700, blob 600).

    Split out of :func:`snapshot_current` so reconciliation can store the exact
    blob whose principal it just proved, rather than re-reading the slot and
    trusting that nothing moved in between.

    Any previously proven ``principal`` survives the rewrite: it identifies the
    ACCOUNT, not the credential, so a capture-before-overwrite that dropped it
    would silently disarm the reconciliation that depends on it.
    """
    try:
        adir = account_dir(record.id, root)
        adir.mkdir(parents=True, exist_ok=True)
        os.chmod(adir, 0o700)
        _atomic_write_private(_blob_path(record.id, root), blob)
        meta = {
            "harness": record.harness,
            "account_id": record.account_id or record.id,
            "captured_at": _utc_now_iso(),
            "kind": "keychain" if (record.harness == "claude" and sys.platform == "darwin") else "file",
        }
        prior = read_meta(record.id, root) or {}
        for key in ("principal", "principal_at"):
            if key in prior:
                meta[key] = prior[key]
        _atomic_write_private(_meta_path(record.id, root), json.dumps(meta, indent=2))
    except OSError as exc:
        raise ManagedStoreError(f"failed to write snapshot for '{record.id}': {exc}") from exc
    return adir


def credential_digest(blob: Optional[str]) -> Optional[str]:
    """Stable digest identifying the credential inside ``blob``, or None.

    A DIGEST, never the secret: this is what makes "do two records hold the same
    credential?" answerable without a token reaching a receipt, a log, or disk.
    Prefers ``claudeAiOauth.accessToken`` (the claude shape) and falls back to
    the whole normalized blob, which is the right identity for codex's
    ``auth.json`` and for an opaque Keychain payload.
    """
    if not blob or not blob.strip():
        return None
    material = blob.strip()
    try:
        data = json.loads(material)
    except (ValueError, TypeError):
        data = None
    if isinstance(data, dict):
        oauth = data.get("claudeAiOauth")
        if isinstance(oauth, dict):
            token = oauth.get("accessToken")
            if isinstance(token, str) and token:
                material = token
    return hashlib.sha256(material.encode("utf-8")).hexdigest()


def credential_expiry(blob: Optional[str]) -> Optional[float]:
    """``claudeAiOauth.expiresAt`` as epoch SECONDS, or None when absent.

    Claude Code stores it in milliseconds; a value in that range is scaled here
    so no caller has to guess the unit. A stored blob whose expiry has passed is
    a dead credential: `fno config accounts use` would materialize it and the next
    session would prompt for a login.
    """
    if not blob:
        return None
    try:
        data = json.loads(blob)
    except (ValueError, TypeError):
        return None
    if not isinstance(data, dict):
        return None
    oauth = data.get("claudeAiOauth")
    if not isinstance(oauth, dict):
        return None
    raw = oauth.get("expiresAt")
    if not isinstance(raw, (int, float)):
        return None
    # Milliseconds since epoch is the shape Claude Code writes; anything past
    # the year 33658 in seconds is really milliseconds.
    return float(raw) / 1000.0 if raw > 1e12 else float(raw)


def read_blob(record_id: str, root: Path | None = None) -> Optional[str]:
    """The stored credential blob for ``record_id``, or None when unregistered."""
    try:
        return _blob_path(record_id, root).read_text(encoding="utf-8")
    except OSError:
        return None


def duplicate_credential_holder(
    blob: Optional[str], *, exclude_id: str, root: Path | None = None
) -> Optional[str]:
    """The id of an existing account whose stored blob holds the SAME credential.

    ``fno config accounts register`` snapshots whatever the shared slot holds at
    capture time. Two captures taken while the same account was signed in store
    one credential under two ids, and every later per-account decision - usage
    attribution, headroom, `fno config accounts use` - is then arithmetic on a
    duplicate. Comparing digests at register time is what turns that into a
    refusal instead of a silent records defect discovered days later.

    Returns None when ``blob`` carries no identifiable credential (nothing to
    compare) or when no other account matches. Also consumed by
    ``fno config accounts doctor`` to report stores that predate this guard.
    """
    target = credential_digest(blob)
    if target is None:
        return None
    base = root or store_root()
    try:
        entries = sorted(p for p in base.iterdir() if p.is_dir())
    except OSError:
        return None
    for entry in entries:
        if entry.name == exclude_id:
            continue
        if credential_digest(read_blob(entry.name, root)) == target:
            return entry.name
    return None


def read_meta(record_id: str, root: Path | None = None) -> Optional[dict]:
    try:
        return json.loads(_meta_path(record_id, root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def age_label_from_seconds(age_seconds: float) -> str:
    """The ``d``/``h``/``m`` age ladder, shared by every age display in this
    module. A negative age (clock skew) floors to 0m rather than printing a
    negative number."""
    age_seconds = max(0.0, age_seconds)
    days = int(age_seconds // 86400)
    if days >= 1:
        return f"{days}d"
    hours = int(age_seconds // 3600)
    if hours >= 1:
        return f"{hours}h"
    return f"{int(age_seconds // 60)}m"


def snapshot_age_label(record_id: str, root: Path | None = None) -> str:
    """Human 'snapshot 3d' style age for `list`; 'none' when unregistered.

    This is the CREDENTIAL BLOB age (when this record was last registered/
    re-registered), not the usage-probe age - the two are unrelated and the
    TTL governs neither of them the same way. See `_usage_age_col` in cli.py
    for the usage-probe reading.
    """
    meta = read_meta(record_id, root)
    if not meta or "captured_at" not in meta:
        return "none"
    try:
        captured = datetime.strptime(meta["captured_at"], "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=timezone.utc
        )
    except (ValueError, TypeError):
        return "unknown"
    delta_seconds = (datetime.now(timezone.utc) - captured).total_seconds()
    return age_label_from_seconds(delta_seconds)


def active_slot_id(cli: str, root: Path | None = None) -> Optional[str]:
    """The account id materialized in ``cli``'s slot, or None if never stamped.

    Only a missing stamp is None; a present-but-unreadable stamp raises (a
    corrupt store must abort a switch, never silently skip capture-before-overwrite)."""
    try:
        return _active_stamp_path(cli, root).read_text(encoding="utf-8").strip() or None
    except FileNotFoundError:
        return None


def stamp_active_slot(cli: str, record_id: str, root: Path | None = None) -> None:
    """Record which account is materialized in ``cli``'s slot (public entry so
    callers don't reach into the private stamp path)."""
    _atomic_write_private(_active_stamp_path(cli, root), record_id)


def _slot_taint_path(cli: str, root: Path) -> Path:
    return _active_stamp_path(cli, root).with_suffix(".tainted")


def slot_tainted(cli: str, root: Path) -> bool:
    """True when the stamp was written under live pins: a pinned session may
    have overwritten the slot since, so the slot content is not trustworthy
    as the stamped account's (skip capture-before-overwrite)."""
    return _slot_taint_path(cli, root).exists()


def _set_slot_taint(
    cli: str,
    root: Path,
    tainted: bool,
    pids: "Sequence[int | tuple[int, Optional[float]]]" = (),
) -> None:
    """Write or clear the taint marker, recording WHICH sessions caused it.

    The pids are what make the taint clearable later. Those processes were
    launched under the OUTGOING account, so each is a live candidate to
    overwrite the slot with that account's refreshed credential; a session
    started afterwards already read the new credential and is harmless. Without
    the list, a repair could only ask "is anything pinned right now?", which on
    the shared slot is almost always yes - including the very session doing the
    repair.
    """
    path = _slot_taint_path(cli, root)
    if tainted:
        # A pid alone is not an identity: pids are reused, and a recycled one
        # would make an unrelated process look like the tainting session and
        # block the repair FOREVER. The start time pins which process it was.
        # A caller that already scanned passes (pid, started) so the sample is
        # not taken after the process may have exited and been replaced.
        writers = [
            {"pid": entry[0], "started": entry[1]}
            if isinstance(entry, tuple)
            else {"pid": entry, "started": _process_started_at(entry)}
            for entry in pids
        ]
        _atomic_write_private(
            path, json.dumps({"pids": [w["pid"] for w in writers], "writers": writers})
        )
    else:
        path.unlink(missing_ok=True)


def _process_started_at(pid: int) -> Optional[float]:
    """A process's creation time, or None when it cannot be read."""
    try:
        return float(psutil.Process(pid).create_time())
    except Exception:  # noqa: BLE001 - gone or denied; the caller decides
        return None


def tainting_writers(
    cli: str, root: Path
) -> Optional[list[tuple[int, Optional[float]]]]:
    """``(pid, started_at)`` for each recorded taint writer, or None if unrecorded.

    None means "this marker cannot say" - a legacy marker written before writers
    were recorded - and the caller falls back to a conservative live scan. An
    empty list is a real answer: nothing was pinning.
    """
    try:
        raw = _slot_taint_path(cli, root).read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        data = json.loads(raw)
    except (ValueError, TypeError):
        return None
    if not isinstance(data, dict):
        return None
    writers = data.get("writers")
    if isinstance(writers, list):
        out: list[tuple[int, Optional[float]]] = []
        for entry in writers:
            if isinstance(entry, dict) and isinstance(entry.get("pid"), int):
                started = entry.get("started")
                out.append(
                    (entry["pid"], float(started) if isinstance(started, (int, float)) else None)
                )
        return out
    pids = data.get("pids")
    if not isinstance(pids, list):
        return None
    # A marker from before start times were recorded: pid-only, so a recycled
    # pid still over-blocks there. Refusing is the safe direction, and the entry
    # is replaced by the next switch.
    return [(pid, None) for pid in pids if isinstance(pid, int)]


def tainting_pids(cli: str, root: Path) -> Optional[tuple[int, ...]]:
    """Just the pids from :func:`tainting_writers`, or None when unrecorded."""
    writers = tainting_writers(cli, root)
    return None if writers is None else tuple(pid for pid, _started in writers)


def taint_writers_still_live(cli: str, root: Path) -> list[str]:
    """Sessions that could still rewrite the slot with a DIFFERENT credential.

    Proving the live principal proves it NOW. A session that was pinning when
    the taint was written holds the previous account's token and can flush a
    refresh of it into the slot at any moment, so clearing the taint while one
    is alive restores exactly the condition the taint records - and the next
    capture-before-overwrite would then file that credential under the record we
    just stamped.
    """
    if not slot_tainted(cli, root):
        # No taint means nothing recorded a writer, which is different from a
        # marker that cannot say. Falling through to a live scan here would let
        # any pinned session block the out-of-band-/login repair - the case the
        # verb exists for, and the one where a live pin is routinely the very
        # session running it.
        return []
    recorded = tainting_writers(cli, root)
    if recorded is None:
        return [f"pid {session.pid}" for session in pinning_sessions_for(cli)]
    alive: list[str] = []
    for pid, started in recorded:
        current = _process_started_at(pid)
        if current is None:
            try:
                if not psutil.pid_exists(pid):
                    continue  # gone: it can no longer write anything
            except Exception:  # noqa: BLE001 - unreadable, so assume it is there
                pass
            alive.append(f"pid {pid}")
            continue
        # A pid that exists but started at a different time is a DIFFERENT
        # process wearing a recycled number, and must not hold the repair.
        if started is None or abs(current - started) < 1.0:
            alive.append(f"pid {pid}")
    return alive


# ---------------------------------------------------------------------------
# Principal reconciliation: the live credential is the truth, the store is cache
#
# Everything above assumes footnote performs every slot transition. `claude
# /login` breaks that assumption - it writes the Keychain directly and tells us
# nothing - and the taint marker written under live pins has no clearer, so a
# false taint made every quota read UNKNOWN with no way back. Both are the same
# gap: the store has no way to ask who is actually in the slot. Asking the
# credential itself is the answer, and it must be an ANSWER, never a guess:
# taint clears only when the live principal binds to exactly one record.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class ReconcileResult:
    """Why reconciliation did or did not repair the slot.

    ``outcome`` is the typed reason a caller branches on and a receipt prints;
    every value other than ``matched`` means NOTHING was written.
    """

    outcome: str
    record_id: Optional[str] = None
    detail: str = ""

    @property
    def ok(self) -> bool:
        return self.outcome == "matched"


def principal_fingerprint(profile: object) -> Optional[dict]:
    """The smallest stable non-secret identity in a ``/api/oauth/profile`` body.

    ``account.uuid`` is the whole discriminator: it names the human, so it
    survives every token refresh, and it differs across configured accounts.
    ``organization_uuid`` and ``email`` ride along only to make a refusal
    readable. A MATCH is decided by :func:`identity_key`, which requires both
    the account and the organization: Claude Code usage is organization-scoped,
    so the account uuid alone would let a bearer for another organization pass
    as this record. Returns None when the payload carries no stable account
    uuid, which the caller reports as ``malformed-profile`` rather than
    treating an unknown shape as a match.
    """
    if not isinstance(profile, dict):
        return None
    account = profile.get("account")
    if not isinstance(account, dict):
        return None
    uuid = account.get("uuid")
    if not isinstance(uuid, str) or not uuid:
        return None
    out: dict = {"account_uuid": uuid}
    org = profile.get("organization")
    if isinstance(org, dict) and isinstance(org.get("uuid"), str) and org["uuid"]:
        out["organization_uuid"] = org["uuid"]
    email = account.get("email")
    if isinstance(email, str) and email:
        out["email"] = email
    return out


def slot_principal(blob: Optional[str]) -> tuple[Optional[dict], Optional[str]]:
    """``(principal, failure)`` for the credential inside ``blob``.

    Exactly one side is ever set. ``failure`` is ``profile-unavailable`` (no
    bearer, network error, timeout, 401) or ``malformed-profile`` (a 200 whose
    body carries no stable principal). An unavailable endpoint proves NOTHING -
    not that the slot changed and not that it did not - so it can never clear a
    taint. The bearer is used and dropped: never returned, logged, or stored.
    """
    from fno.adapters.providers.usage import _token_from_blob

    bearer = _token_from_blob(blob)
    if not bearer:
        # No usable bearer at all: nothing can present this as a live identity,
        # so it is a dead candidate rather than an unanswered question.
        return None, "credential-rejected"
    return principal_of_bearer(bearer)


def principal_of_bearer(bearer: str) -> tuple[Optional[dict], Optional[str]]:
    """``(principal, failure)`` for one exact OAuth bearer.

    Taking the bearer rather than a slot location is what lets a caller prove
    the identity of the CREDENTIAL IT WILL ACTUALLY USE. Proving one credential
    and then measuring another is the same misattribution by a longer route:
    the scoped and unscoped Keychain items can hold different accounts, and a
    stale scoped item is exactly why the usage probe tries several bearers.
    """
    req = urllib.request.Request(
        _PROFILE_URL,
        headers={
            "Authorization": f"Bearer {bearer}",
            "anthropic-beta": "oauth-2025-04-20",
            "User-Agent": _PROFILE_USER_AGENT,
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=_PROFILE_TIMEOUT_S) as resp:
            body = resp.read()
    except urllib.error.HTTPError as exc:
        # 401/403 is the endpoint ANSWERING: this credential is not usable, so
        # it cannot be what anyone is billing. Every other status - 429, 5xx -
        # is the question going unanswered, which is a different thing and must
        # not be mistaken for a dead credential.
        if exc.code in (401, 403):
            return None, "credential-rejected"
        return None, "profile-unavailable"
    except (urllib.error.URLError, OSError, TimeoutError, ValueError):
        return None, "profile-unavailable"
    try:
        payload = json.loads(body)
    except (ValueError, TypeError):
        return None, "malformed-profile"
    fingerprint = principal_fingerprint(payload)
    if fingerprint is None:
        return None, "malformed-profile"
    return fingerprint, None


def identity_key(principal: Optional[dict]) -> Optional[str]:
    """The comparable identity in a principal, or None when it is incomplete.

    Both the account AND the organization, because Claude Code usage is
    organization-scoped: one human can belong to two organizations, and
    comparing the account uuid alone would let an org-B bearer pass as the
    org-A record and file its usage there. An identity missing either half is
    not comparable at all - fail closed rather than match on the half we have.
    """
    if not isinstance(principal, dict):
        return None
    account = principal.get("account_uuid")
    org = principal.get("organization_uuid")
    if not isinstance(account, str) or not account:
        return None
    if not isinstance(org, str) or not org:
        return None
    return f"{account}/{org}"


def record_principal(record_id: str, root: Path | None = None) -> Optional[dict]:
    """``record_id``'s proven principal, or None when it has never been bound."""
    meta = read_meta(record_id, root) or {}
    principal = meta.get("principal")
    if isinstance(principal, dict) and principal.get("account_uuid"):
        return principal
    return None


def write_record_principal(record_id: str, principal: dict, root: Path | None = None) -> None:
    """Bind ``principal`` to ``record_id`` in its private metadata (600).

    Private store metadata, never a receipt and never the graph: it is identity
    for matching, and the less of it that travels the better.
    """
    meta = read_meta(record_id, root) or {}
    meta["principal"] = principal
    meta["principal_at"] = _utc_now_iso()
    _atomic_write_private(_meta_path(record_id, root), json.dumps(meta, indent=2))


def capture_record_principal(
    record: ProviderRecord,
    blob: Optional[str] = None,
    root: Path | None = None,
    *,
    force: bool = False,
) -> Optional[dict]:
    """Best-effort: prove and store ``record``'s principal from its credential.

    Called where footnote KNOWS which account a blob belongs to (register, and
    the tail of a verified switch), so the binding is established while the
    answer is certain. Never raises and never blocks its caller: a record with
    no bound principal is simply unmatchable later, and reconciliation refuses
    loudly instead of guessing.

    ``force`` re-binds an already-bound record. Register sets it (re-registering
    an id is how an operator rebinds it to a different account); switch does
    not, so a routine switch of an already-bound record costs no network call.
    """
    if record.harness != "claude":
        return None
    if not force and record_principal(record.id, root) is not None:
        return None
    material = blob if blob is not None else read_blob(record.id, root)
    principal, _failure = slot_principal(material)
    if principal is None:
        if force:
            # Re-registering an id points it at whatever is signed in NOW, while
            # `write_snapshot` deliberately preserves the previous principal for
            # capture-before-overwrite. Leaving that binding here would claim the
            # new credential belongs to the old account - a confident lie is
            # worse than an unmatchable record, so drop it.
            _clear_record_principal(record.id, root)
        return None
    try:
        write_record_principal(record.id, principal, root)
    except OSError:
        return None
    return principal


def slot_identity_drift(cli: str, root: Path | None = None) -> Optional[dict]:
    """``{stamped, live}`` when the stamp and the live slot disagree, else None.

    The taint marker only watches the door footnote controls. An out-of-band
    `claude /login` walks through the other one, leaving a stamp that is wrong
    and UNTAINTED - so attribution proceeds confidently and files the new
    account's usage under the old account's name. This is the read that makes
    that loud.

    Read-only, and free until it can answer: with no bound principal there is
    nothing to compare, so an unbound store never pays for a profile call.
    """
    if cli != "claude":
        return None
    try:
        stamped = active_slot_id(cli, root)
    except OSError:
        return None
    if not stamped:
        return None
    bound = record_principal(stamped, root)
    if bound is None:
        return None
    try:
        principal, failure = canonical_slot_principal(cli)
    except ManagedStoreError:
        return None  # an unreadable slot cannot demonstrate drift
    if failure == "ambiguous-slot":
        # Reporting healthy here would hide two accounts sharing one slot.
        return {"stamped": stamped, "live": None, "ambiguous": True}
    if principal is None or identity_key(principal) == identity_key(bound):
        return None
    return {
        "stamped": stamped,
        "live": principal.get("email") or principal.get("account_uuid"),
        "ambiguous": False,
    }


def _clear_record_principal(record_id: str, root: Path | None = None) -> None:
    """Drop a record's principal binding, leaving the rest of its metadata."""
    meta = read_meta(record_id, root) or {}
    if not meta.pop("principal", None) and "principal_at" not in meta:
        return
    meta.pop("principal_at", None)
    try:
        _atomic_write_private(_meta_path(record_id, root), json.dumps(meta, indent=2))
    except OSError:
        pass


def _principal_cache_path(cli: str, root: Path) -> Path:
    return _active_stamp_path(cli, root).with_suffix(".principal")


def cached_slot_principal(
    cli: str,
    root: Path,
    credential: Optional[str],
    *,
    now: float | None = None,
    ttl: float = _PRINCIPAL_TTL_S,
) -> Optional[str]:
    """The account uuid last PROVEN for exactly ``blob``, while still fresh.

    Keyed on a digest of the credential, not just the harness. Time alone is the
    wrong key: an out-of-band `/login` inside the TTL would otherwise reuse
    evidence about the credential it replaced, and the check built to catch that
    login would be the thing that hides it.

    Successful evidence only. A failure is never cached here - it would become
    an excuse to skip the next check, the opposite of what an unproven identity
    should cause.
    """
    digest = credential_digest(credential)
    if digest is None:
        return None
    try:
        data = json.loads(_principal_cache_path(cli, root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict) or data.get("credential") != digest:
        return None
    uuid, at = data.get("account_uuid"), data.get("at")
    if not isinstance(uuid, str) or not isinstance(at, (int, float)):
        return None
    return uuid if (now if now is not None else time.time()) - at < ttl else None


def note_slot_principal(
    cli: str,
    root: Path,
    identity: str,
    credential: Optional[str],
    *,
    now: float | None = None,
) -> None:
    digest = credential_digest(credential)
    if digest is None:
        return
    try:
        _atomic_write_private(
            _principal_cache_path(cli, root),
            json.dumps({
                "account_uuid": identity,
                "credential": digest,
                "at": now if now is not None else time.time(),
            }),
        )
    except OSError:
        pass


def clear_slot_principal_cache(cli: str, root: Path) -> None:
    _principal_cache_path(cli, root).unlink(missing_ok=True)


def bearer_principal_verdict(
    cli: str,
    record_id: str,
    root: Path,
    bearer: str,
    *,
    now: float | None = None,
    ttl: float = _PRINCIPAL_TTL_S,
) -> str:
    """Does ``bearer`` belong to ``record_id``? ``match``/``mismatch``/``unprovable``.

    The taint marker only watches the door footnote controls, so an out-of-band
    `claude /login` leaves a stamp that is wrong AND untainted and attribution
    proceeds confidently. This is the check that catches it - and it takes the
    exact bearer so the credential proven is the credential measured.

    ``unprovable`` (no bound principal, or the endpoint could not answer) is NOT
    a pass. Shared-slot attribution without fresh proof is exactly the confident
    wrong number this module exists to stop, and refusing costs little: the
    usage endpoint that would consume the attribution shares a host with the
    profile endpoint, so an outage hiding identity has already taken the
    measurement with it. `doctor` reports an unbound principal so the resulting
    unknown always carries a reason and a fix.
    """
    bound = record_principal(record_id, root)
    if bound is None:
        return "unprovable"
    want = identity_key(bound)
    if want is None:
        return "unprovable"  # an incomplete binding cannot vouch for anything
    cached = cached_slot_principal(cli, root, bearer, now=now, ttl=ttl)
    if cached is not None:
        return "match" if cached == want else "mismatch"
    principal, _failure = principal_of_bearer(bearer)
    got = identity_key(principal)
    if got is None:
        return "unprovable"
    note_slot_principal(cli, root, got, bearer, now=now)
    return "match" if got == want else "mismatch"


def principal_holder(
    identity: Optional[str], *, exclude_id: str, root: Path | None = None
) -> Optional[str]:
    """Another shared-slot record already bound to ``identity``, or None.

    ``duplicate_credential_holder`` compares TOKENS, which rotate, so the same
    account registered again after a rotation slips past it and creates two
    records for one quota pool - and reconciliation then matches both and
    refuses forever as ``ambiguous-match``. Principals do not rotate, so this
    catches what the digest cannot.
    """
    if identity is None:
        return None
    base = root or store_root()
    try:
        entries = sorted(entry for entry in base.iterdir() if entry.is_dir())
    except OSError:
        return None
    for entry in entries:
        if entry.name == exclude_id:
            continue
        if identity_key(record_principal(entry.name, root)) == identity:
            return entry.name
    return None


def register_slot_snapshot(
    record: ProviderRecord,
    root: Path | None = None,
    *,
    lock_timeout: float = 10,
    persist: Optional[Callable[[], None]] = None,
) -> tuple[Optional[Path], Optional[dict], Optional[str]]:
    """Capture the shared slot for ``record`` and bind the identity of THOSE bytes.

    ``(account_dir, principal, failure)``, where a ``None`` account_dir means
    NOTHING was written - the caller reports a refusal on exactly that, rather
    than on a list of failure names a new value could slip past. One read serves the proof, the
    snapshot, and the binding, under the same mutex a switch takes - otherwise
    the identity proved and the credential stored can be two different accounts,
    either because an ambient ``CLAUDE_CONFIG_DIR`` redirects the second read or
    because a concurrent switch replaces the slot between them.

    ``ambiguous-slot``, ``duplicate-principal:<id>``,
    ``duplicate-credential:<id>`` and ``slot-changed`` write nothing;
    ``slot-moved-after-write`` DID register, but the slot moved during the
    writes, so the stamp is tainted rather than trusted: with two
    accounts in the slot there is no way to know which one the operator meant,
    and a principal another record already holds would create two records for
    one quota pool. Any other failure still snapshots (registration must work
    offline) and leaves the record unbound, which `doctor` reports.

    ``persist`` (the caller's config save) runs inside the lock and BEFORE any
    store write: a failed save must not leave a snapshot behind, because that
    residue is what a later registration reads as a duplicate credential and
    refuses. The stamp comes last, and is written here rather than by the
    caller, because the captured credential IS what the slot holds at that
    moment - and stamping an unconfigured orphan would make every configured
    shared account unattributable behind it.
    """
    root = root or store_root()
    root.mkdir(parents=True, exist_ok=True)
    lock = filelock.FileLock(str(_switch_lock_path(root)), timeout=lock_timeout)
    try:
        lock.acquire()
    except filelock.Timeout as exc:
        raise SwitchDeferred(
            "another slot transition is in progress; try again"
        ) from exc
    try:
        blobs = canonical_slot_blobs(record.harness)
        if not blobs:
            raise ManagedStoreError(
                f"no current {record.harness} login to snapshot for '{record.id}' "
                "(sign in first, then register)"
            )
        if record.harness == "claude":
            principal, proven_blob, failure = principal_of_blobs(blobs)
        else:
            # Only claude has a principal endpoint. Running a codex auth blob
            # through the claude parser would report it as a dead credential
            # and refuse a registration that had already been written.
            principal, proven_blob, failure = None, blobs[0], None
        if failure == "ambiguous-slot":
            return None, None, failure
        # A session recorded as tainting the slot can still overwrite it after
        # the final read, so the provisional taint below must not simply
        # discard it. Registration establishes a new truth; it cannot do that
        # while something else can still change the slot underneath it.
        blockers = taint_writers_still_live(record.harness, root)
        if blockers:
            return None, None, f"slot-pinned:{', '.join(blockers)}"
        holder = principal_holder(
            identity_key(principal), exclude_id=record.id, root=root
        )
        if holder is not None:
            return None, None, f"duplicate-principal:{holder}"
        # The token check belongs in here too, and must hash THE BLOB WE WILL
        # STORE: an expired scoped candidate in front of a live unscoped one
        # shifts what gets stored, and hashing the first candidate would then
        # miss a duplicate and file it under a second id. A concurrent switch
        # can also replace the slot after any check made outside this lock, and
        # with the profile endpoint unavailable the principal check above cannot
        # catch what the digest would.
        stored_blob = proven_blob or blobs[0]
        token_holder = duplicate_credential_holder(
            stored_blob, exclude_id=record.id, root=root
        )
        if token_holder is not None:
            return None, None, f"duplicate-credential:{token_holder}"
        # The profile request above is a network round trip. Re-verify the
        # capture before committing anything, so an out-of-band login during it
        # cannot leave a registration stamped for the account we proved while
        # the slot holds the one that replaced it.
        if canonical_slot_blobs(record.harness) != blobs:
            return None, None, "slot-changed"
        # Persist the record FIRST. Everything after this writes to the store,
        # and store residue from a failed registration is what later reads as a
        # duplicate credential and refuses a legitimate one; a config entry with
        # no snapshot just tells the next switch to run register.
        if persist is not None:
            persist()
        # Taint BEFORE the trusted writes, not after them. The writes take
        # time; a crash, or a Keychain read that fails at the end, would
        # otherwise leave an untainted stamp naming this record while the slot
        # holds whoever logged in during the window - and the next switch would
        # capture that credential into this record's snapshot. Provisional taint
        # means "this stamp is not verified yet", which is what the marker is
        # for, and makes every abrupt exit fail safe.
        _set_slot_taint(record.harness, root, True, [])
        adir = write_snapshot(record, stored_blob, root)
        if principal is not None:
            write_record_principal(record.id, principal, root)
        else:
            _clear_record_principal(record.id, root)
        # Stamp INSIDE the lock: the captured credential is what the slot holds
        # right now, and releasing first would let a concurrent switch install
        # and stamp another account before this stamp overwrote it - leaving the
        # stamp naming this record while the slot holds the other one.
        stamp_active_slot(record.harness, record.id, root)
        try:
            settled = canonical_slot_blobs(record.harness) == blobs
        except ManagedStoreError:
            settled = False  # could not confirm, so do not clear the taint
        if not settled:
            # The account IS registered; only the stamp's trustworthiness is in
            # question, so this reports success with a warning, not a refusal.
            return adir, principal, "slot-moved-after-write"
        _set_slot_taint(record.harness, root, False)
        return adir, principal, failure
    finally:
        lock.release()


def _reconcile_backoff_path(cli: str, root: Path) -> Path:
    return _active_stamp_path(cli, root).with_suffix(".reconcile-attempt")


def reconcile_backoff_active(
    cli: str, root: Path, *, now: float | None = None, window: float = _RECONCILE_BACKOFF_S
) -> bool:
    """True when a refused auto-reconcile is still inside its backoff window.

    Bounds the cost of a slot whose principal matches nothing: without it every
    usage probe would re-hit the profile endpoint. Only the EXPLICIT
    ``reconcile-slot`` verb ignores this, so an operator repairing the store is
    never told to wait.
    """
    try:
        stamp = float(_reconcile_backoff_path(cli, root).read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return False
    return (now if now is not None else time.time()) - stamp < window


def note_reconcile_attempt(cli: str, root: Path, *, now: float | None = None) -> None:
    try:
        _atomic_write_private(
            _reconcile_backoff_path(cli, root), str(now if now is not None else time.time())
        )
    except OSError:
        pass


def _clear_reconcile_backoff(cli: str, root: Path) -> None:
    _reconcile_backoff_path(cli, root).unlink(missing_ok=True)


def reconcile_slot(
    cli: str,
    *,
    by_id: dict[str, ProviderRecord],
    root: Path | None = None,
    lock_timeout: float = 10,
) -> ReconcileResult:
    """Prove who is in ``cli``'s shared slot and repair the store to match.

    Takes the SAME mutex as :func:`switch`, and reads the slot INSIDE it, so a
    concurrent footnote switch can never make this observation stale between
    the read and the write it justifies.

    On a unique match: refresh that record's snapshot from the live blob, stamp
    it active, clear the taint. On anything else - endpoint down, malformed
    body, no matching record, two matching records - write nothing at all and
    return the typed reason. The one exception is a change detected AFTER the
    commit began: the taint set at its start simply stays, which is a write in
    the safe direction (nothing trusts the stamp) rather than a repair. That asymmetry is the point: a wrong clear files
    one account's usage under another's name, which is worse than staying
    UNKNOWN.
    """
    if cli != "claude":
        return ReconcileResult(
            "unsupported-harness",
            detail=(
                f"'{cli}' has no principal endpoint to prove slot identity with; "
                "only claude slots can be reconciled"
            ),
        )
    root = root or store_root()
    if not root.is_dir():
        # Creating the store here would be a write on a path that cannot
        # possibly match anything, breaking the guarantee that only `matched`
        # touches disk. Nothing is registered, so the answer is already known.
        return ReconcileResult(
            "no-managed-store",
            detail=(
                f"no managed store at {root}; register an account while it is "
                "signed in before there is anything to reconcile against"
            ),
        )
    lock = filelock.FileLock(str(_switch_lock_path(root)), timeout=lock_timeout)
    try:
        lock.acquire()
    except filelock.Timeout:
        return ReconcileResult(
            "lock-timeout", detail="another slot transition is in progress; try again"
        )
    try:
        return _reconcile_locked(cli, by_id=by_id, root=root)
    except ManagedStoreError as exc:
        # A `security` timeout or denial is a slot we could not read, which is
        # a refusal like any other - not a traceback out of an operator verb.
        return ReconcileResult("slot-unreadable", detail=str(exc))
    finally:
        lock.release()


def _slot_pinned_detail(blockers: list[str]) -> str:
    return (
        f"{', '.join(blockers)} was pinning the slot when it was tainted and can "
        "still write the previous account's refreshed credential, so proving the "
        "identity now would not keep it true; stop it and retry"
    )


def _reconcile_locked(
    cli: str, *, by_id: dict[str, ProviderRecord], root: Path
) -> ReconcileResult:
    # The pin gate runs FIRST. Resolving the principal is a network round trip,
    # and a recorded writer that rewrites the slot and exits during that call
    # would pass a liveness check made afterwards - leaving us to stamp a
    # credential we proved before it was replaced.
    blockers = taint_writers_still_live(cli, root)
    if blockers:
        return ReconcileResult("slot-pinned", detail=_slot_pinned_detail(blockers))

    blobs = canonical_slot_blobs(cli)
    if not blobs:
        return ReconcileResult(
            "no-slot-credential",
            detail=f"no live {cli} login in the shared slot; sign in, then reconcile",
        )
    # Prove THE CAPTURE, not a second read: an A -> B -> A flip between the two
    # would prove B, survive the later comparison against the captured A, and
    # cache A's bearer under B's identity.
    principal, proven_blob, failure = principal_of_blobs(blobs)
    blob = proven_blob or blobs[0]
    if principal is None:
        if failure == "ambiguous-slot":
            return ReconcileResult(
                "ambiguous-slot",
                detail=(
                    f"the {cli} slot presents credentials belonging to different "
                    "accounts (a stale scoped Keychain item beside a live unscoped "
                    "one); whichever was stamped, some reader would get the other - "
                    "sign out and back in to settle it"
                ),
            )
        return ReconcileResult(
            failure or "profile-unavailable",
            detail=(
                "could not prove who the live slot credential belongs to; "
                "taint, stamp and snapshots are unchanged"
            ),
        )

    # A record with its OWN config_dir is attributable without the shared slot
    # (usage.py accepts its dir before taint is ever consulted), so it is not a
    # candidate for the slot's identity and must not be able to claim it.
    matches = [
        record_id
        for record_id, record in sorted(by_id.items())
        if record.harness == cli
        and record.auth == "managed"
        and record.config_dir is None
        and identity_key(record_principal(record_id, root)) == identity_key(principal)
    ]
    who = principal.get("email") or principal.get("account_uuid") or "an unknown account"
    if not matches:
        return ReconcileResult(
            "zero-match",
            detail=(
                f"the live slot holds {who}, which matches no registered account's "
                "proven identity; sign that account in and run "
                "`fno config accounts register <id>` to bind it"
            ),
        )
    if len(matches) > 1:
        return ReconcileResult(
            "ambiguous-match",
            detail=(
                f"the live slot holds {who}, which matches {len(matches)} records "
                f"({', '.join(matches)}); one of them was registered while the other "
                "was signed in"
            ),
        )

    matched = matches[0]
    # Identity was proven about the bytes read above, so commit only if those
    # bytes are still what the slot holds. Re-reading is what actually closes
    # the profile-call window: a writer that rewrote the slot and exited during
    # it leaves nothing for a liveness check to find.
    if canonical_slot_blobs(cli) != blobs:
        return ReconcileResult(
            "slot-changed",
            detail=(
                "the slot credential changed while its identity was being "
                "proven; nothing was written, retry once it settles"
            ),
        )
    blockers = taint_writers_still_live(cli, root)
    if blockers:
        return ReconcileResult("slot-pinned", detail=_slot_pinned_detail(blockers))

    # Snapshot first, stamp second, clear taint last: a crash at any point
    # leaves the taint set, which is the safe direction to fail.
    # Taint FIRST, so the marker covers the whole window in which the stamp
    # exists but is not yet verified. This path is also reached from an
    # UNTAINTED drift repair, where there would otherwise be no marker at all
    # while the writes ran - and a crash between them would leave a freshly
    # written, unverified stamp fully trusted. The recorded pids are dropped
    # deliberately: the live-writer gate above already passed, so they no longer
    # gate anything, and an empty list lets a retry proceed once the slot settles.
    _set_slot_taint(cli, root, True, [])
    write_snapshot(by_id[matched], blob, root)
    write_record_principal(matched, principal, root)
    stamp_active_slot(cli, matched, root)
    # Clearing the taint is what makes the stamp TRUSTED, so it gets the last
    # look - and a look we could not take counts as unsettled.
    try:
        settled = canonical_slot_blobs(cli) == blobs
    except ManagedStoreError:
        settled = False
    if not settled:
        clear_slot_principal_cache(cli, root)
        return ReconcileResult(
            "slot-changed",
            detail=(
                f"'{matched}' was proven and its snapshot refreshed, but the slot "
                "changed again before the stamp could be trusted; the slot is "
                "marked tainted so nothing reads it - retry once it settles"
            ),
        )
    _set_slot_taint(cli, root, False)
    _clear_reconcile_backoff(cli, root)
    # Key the evidence on the BEARER inside the blob, not the blob, so the usage
    # probe's per-bearer lookup finds this entry instead of re-proving it.
    from fno.adapters.providers.usage import _token_from_blob

    note_slot_principal(
        cli, root, identity_key(principal) or "", _token_from_blob(blob)
    )
    return ReconcileResult(
        "matched",
        record_id=matched,
        detail=(
            f"the live {cli} slot belongs to '{matched}' (proven by live profile); "
            "snapshot refreshed, stamped active, taint cleared"
        ),
    )


# ---------------------------------------------------------------------------
# Switch (materialize with both guards)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SwitchResult:
    active: str  # a returned result is always verified; failure raises instead
    slot_changed: bool = True
    verification: str = "structural"
    reason: Optional[str] = None
    # pids of live sessions the switch proceeded under (pin_policy="warn")
    pinned_by: tuple[int, ...] = ()


def _rollback_materialized_slot(cli: str, rollback_blob: Optional[str]) -> tuple[str, bool]:
    if not rollback_blob:
        return "nothing to roll back to; slot may hold the unverified target", False
    try:
        _write_slot_blob(cli, rollback_blob)
    except ManagedStoreError as exc:
        return f"rollback ALSO failed ({exc}); slot is in an indeterminate state", False
    return "slot rolled back to the previous account", True


def _rollback_after_codex_probe(
    target_id: str, rollback_blob: Optional[str], root: Path
) -> tuple[str, bool]:
    pins = pinning_sessions_for("codex")
    if pins:
        names = ", ".join(f"pid {pin.pid}" for pin in pins)
        stamp_receipt = _clear_unverified_codex_stamp(root)
        return (
            f"rollback withheld because the slot is pinned by a live codex session "
            f"({names}); slot may hold '{target_id}'; {stamp_receipt}",
            False,
        )
    return _rollback_codex_slot(rollback_blob, root)


def _rollback_codex_slot(rollback_blob: Optional[str], root: Path) -> tuple[str, bool]:
    tail, rolled_back = _rollback_materialized_slot("codex", rollback_blob)
    if rolled_back:
        return tail, True
    return f"{tail}; {_clear_unverified_codex_stamp(root)}", False


def _clear_unverified_codex_stamp(root: Path) -> str:
    try:
        stamp_active_slot("codex", "", root)
    except OSError as exc:
        return f"active stamp could not be cleared ({exc})"
    return "active stamp cleared because the slot occupant is unverified"


def _capture_outgoing(outgoing: ProviderRecord, root: Path) -> bool:
    """Re-snapshot the outgoing account's current slot credential. True if done.

    Reads the SAME canonical candidates the identity path resolves, because
    those two must not disagree about which credential belongs to a record:
    reconciliation may have stored the proven (unscoped) blob while a
    scoped-first read here would capture the other one straight back over it.

    More than one distinct credential in the slot means we cannot say which is
    this record's, so it captures nothing and the older snapshot stands. That
    loses a rotated refresh token at worst - recoverable with a login - where
    guessing would file another account's credential under this record, which
    is silent and is not. It is the same "skip capture rather than poison it"
    stance the taint check above already takes.

    A read failure still propagates: overwriting the slot without capturing a
    live credential we could not read would lose the outgoing token for real.
    """
    blobs = canonical_slot_blobs(outgoing.harness)  # KeychainError propagates
    if len(blobs) != 1:
        return False
    write_snapshot(outgoing, blobs[0], root)
    return True


def switch(
    target: ProviderRecord,
    *,
    by_id: dict[str, ProviderRecord],
    root: Path | None = None,
    emit_fn: Optional[Callable[..., None]] = None,
    pin_policy: str = "warn",
) -> SwitchResult:
    """Materialize ``target`` into the slot (capture-before-overwrite), serialized
    by a cross-process mutex. Rolls back and raises on failed verification.

    pin_policy: "warn" (default) proceeds under live claude pins, reporting them
    on the result; "defer" raises SwitchDeferred. Codex always defers.
    """
    if pin_policy not in ("warn", "defer"):
        raise ValueError(f"invalid pin_policy: {pin_policy!r} (expected 'warn' or 'defer')")
    root = root or store_root()
    root.mkdir(parents=True, exist_ok=True)
    lock = filelock.FileLock(str(_switch_lock_path(root)), timeout=10)
    try:
        lock.acquire()
    except filelock.Timeout as exc:
        raise SwitchDeferred("another switch is in progress (mutex held); try again") from exc
    try:
        return _switch_locked(
            target, by_id=by_id, root=root, emit_fn=emit_fn, pin_policy=pin_policy
        )
    finally:
        lock.release()


def _switch_locked(
    target: ProviderRecord,
    *,
    by_id: dict[str, ProviderRecord],
    root: Path,
    emit_fn: Optional[Callable[..., None]],
    pin_policy: str = "warn",
) -> SwitchResult:
    stored = _blob_path(target.id, root)
    try:
        target_blob = stored.read_text(encoding="utf-8")
    except OSError as exc:
        raise ManagedStoreError(
            f"no credential snapshot for '{target.id}' at {stored} - run "
            f"`fno config accounts register {target.id}` first"
        ) from exc
    if not target_blob.strip():
        raise ManagedStoreError(f"credential snapshot for '{target.id}' is empty; refusing to materialize")

    outgoing_id = active_slot_id(target.harness, root)  # this CLI's slot occupant
    if outgoing_id == target.id and verify_slot(target, target_blob):
        # Stamp says target AND the slot actually reads back target's blob:
        # a true no-op. If the slot was changed out-of-band (manual /login,
        # stale stamp after a partial failure), verify_slot is False and we
        # fall through to re-materialize rather than falsely report success.
        return SwitchResult(
            active=target.id,
            slot_changed=False,
            reason="slot-already-active" if target.harness == "codex" else None,
        )

    # Pin gate inside the critical section (mutex held across check + write).
    # claude "warn" proceeds — the same rewrite a manual /login performs;
    # codex always defers (its TOCTOU re-scan assumes an unpinned slot).
    pins = pinning_sessions_for(target.harness)
    pinned_by: tuple[int, ...] = ()
    if pins:
        if pin_policy == "defer" or target.harness == "codex":
            names = ", ".join(f"pid {p.pid}" for p in pins)
            raise SwitchDeferred(
                f"slot is pinned by a live {target.harness} session ({names}); stop it or retry",
                sessions=pins,
            )
        pinned_by = tuple(p.pid for p in pins)

    # Capture-before-overwrite: the slot currently holds the outgoing account's
    # (possibly rotated) creds. Re-snapshot them before we overwrite the slot.
    # A tainted stamp (written under live pins) means the slot may hold a
    # DIFFERENT account's creds by now: skip capture rather than poison the
    # stamped account's snapshot with them.
    rollback_blob: Optional[str] = _read_slot_blob(target.harness)
    if outgoing_id and outgoing_id in by_id and not slot_tainted(target.harness, root):
        _capture_outgoing(by_id[outgoing_id], root)

    _write_slot_blob(target.harness, target_blob)

    if not verify_slot(target, target_blob):
        # Verification failed: roll the slot back to the captured outgoing blob.
        # Tell the truth about the resulting slot state - operators act on it.
        if target.harness == "codex":
            tail, _ = _rollback_codex_slot(rollback_blob, root)
        else:
            tail, _ = _rollback_materialized_slot(target.harness, rollback_blob)
        raise ManagedStoreError(
            f"switch to '{target.id}' failed verification (stored token may be "
            f"stale/revoked); {tail}"
        )

    # Codex TOCTOU narrowing (cv-f578cbe7): the pin gate above runs BEFORE the
    # write, so a codex launched in the snapshot+write window - having read the
    # OUTGOING creds at startup - is not caught by it. Re-scan immediately after
    # structural verification; if one appeared, roll the slot back and defer so
    # the native status probe never widens the accepted write->recheck race. This
    # is best-effort, not a full fix: a launch in the tiny write->recheck gap is
    # irreducible without a lease the external codex binary honors. claude keeps
    # G1's single pre-write check (this arm only, by request).
    if target.harness == "codex":
        late_pins = pinning_sessions_for(target.harness)
        if late_pins:
            names = ", ".join(f"pid {p.pid}" for p in late_pins)
            tail, rolled_back = _rollback_codex_slot(rollback_blob, root)
            if not rolled_back:
                if rollback_blob:
                    raise ManagedStoreError(
                        f"a live {target.harness} session started mid-switch; {tail}; "
                        f"slot may hold '{target.id}' under a live session"
                    )
                raise SwitchDeferred(
                    f"a live {target.harness} session ({names}) started during the switch "
                    f"({tail}); retry once it exits",
                    sessions=late_pins,
                )
            raise SwitchDeferred(
                f"a live {target.harness} session ({names}) started during the switch; "
                "slot rolled back to the previous account, retry once it exits",
                sessions=late_pins,
            )

    verification = "structural"
    verification_reason: Optional[str] = None
    if target.harness == "codex":
        try:
            login = _codex_login_ok()
        except ManagedStoreError as exc:
            tail, _ = _rollback_after_codex_probe(target.id, rollback_blob, root)
            raise ManagedStoreError(f"codex login verification failed ({exc}); {tail}") from exc
        except KeyboardInterrupt as exc:
            # Preserve the interrupt while carrying a truthful rollback receipt
            # through Click's BaseException handling.
            tail, _ = _rollback_after_codex_probe(target.id, rollback_blob, root)
            exc.add_note(f"codex login verification interrupted; {tail}")
            raise
        if login.ok is False:
            tail, _ = _rollback_after_codex_probe(target.id, rollback_blob, root)
            raise ManagedStoreError(
                f"switch to '{target.id}' was not recognized by `codex login status`; {tail}"
            )
        if login.ok is True:
            verification = "codex-recognized"
        else:
            verification_reason = login.reason

    # Crash window: a kill between the slot write above and this stamp leaves the
    # stamp naming the previous account while the slot holds target. Rare and
    # self-correcting on the next successful switch; journaling is not worth it
    # for a manual v1 (US3's daemon path can revisit if a postmortem shows it).
    stamp_active_slot(target.harness, target.id, root)
    _set_slot_taint(
        target.harness,
        root,
        bool(pinned_by),
        [(session.pid, session.started) for session in pins],
    )
    # The slot now holds a different credential, so any cached principal
    # evidence describes the previous occupant.
    clear_slot_principal_cache(target.harness, root)
    # Deliberately NOT binding target's principal here. A switch materializes
    # the record's STORED snapshot, and a snapshot's provenance is the store,
    # not the operator: an earlier out-of-band login plus capture-before-
    # overwrite can leave one account's credential filed under another's id
    # (the `duplicate-credential` doctor finding exists for that). Binding from
    # it would manufacture confident attribution to the wrong account, which is
    # the failure this module is about. Only `register` - where the operator
    # asserts that the signed-in account IS this record - is trusted provenance.
    if emit_fn is not None:
        event: dict[str, object] = {
            "provider": target.id,
            "account_id": target.account_id or target.id,
            "outgoing": outgoing_id or "",
        }
        if target.harness == "codex":
            event.update(
                slot_changed=True,
                verification=verification,
            )
            if verification_reason:
                event["reason"] = verification_reason
        emit_fn("account_switched", **event)
    return SwitchResult(
        active=target.id,
        verification=verification,
        reason=verification_reason,
        pinned_by=pinned_by,
    )
