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
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

import filelock
import psutil

from fno.adapters.providers.model import ProviderRecord

# macOS Keychain item claude reads (mirrors usage.py._CLAUDE_KEYCHAIN_SERVICE).
_CLAUDE_KEYCHAIN_SERVICE = "Claude Code-credentials"
_SECURITY_TIMEOUT_S = 5  # ponytail: same 5s ceiling usage.py uses for `security`
_CODEX_LOGIN_TIMEOUT_S = 5


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
    pid: int
    cmdline: str


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
    cfg = config_dir or _claude_slot_config_dir()
    if sys.platform == "darwin":
        acct = _claude_keychain_account()
        for service in (_claude_scoped_service(cfg), _CLAUDE_KEYCHAIN_SERVICE):
            out = _run_security(["find-generic-password", "-s", service, "-a", acct, "-w"])
            if out.returncode == 0 and out.stdout.strip():
                return out.stdout.strip()
        return None
    try:
        return (cfg / ".credentials.json").read_text(encoding="utf-8")
    except OSError:
        return None


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
    got = _read_slot_blob(record.cli)
    if got is None or got.strip() != expected_blob.strip():
        return False
    return _token_present(got)


@dataclass(frozen=True)
class _CodexLoginResult:
    ok: Optional[bool]
    reason: Optional[str] = None


def _codex_login_ok() -> _CodexLoginResult:
    """Ask Codex to recognize the materialized auth schema in its exact home."""
    env = os.environ.copy()
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
    """A materialized blob must decode to something with an access token.

    Mirrors usage.py's tolerance: the token can sit at a couple of known paths;
    a non-JSON codex file counts as present (its shape is opaque here)."""
    try:
        data = json.loads(blob)
    except (ValueError, TypeError):
        return bool(blob.strip())
    if not isinstance(data, dict):
        return bool(blob.strip())
    oauth = data.get("claudeAiOauth")
    if isinstance(oauth, dict) and oauth.get("accessToken"):
        return True
    return bool(data.get("accessToken") or data.get("access_token") or data)


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
    for proc in psutil.process_iter(["pid", "name", "cmdline"]):
        try:
            if proc.info["pid"] == me:
                continue
            cmdline = proc.info.get("cmdline") or []
            if not looks_like(proc.info.get("name"), cmdline):
                continue
            try:
                env = proc.environ()
            except Exception:  # noqa: BLE001 - unreadable env: assume it pins the default slot
                found.append(PinningSession(proc.info["pid"], " ".join(cmdline)))
                continue
            override = env.get(env_var)
            proc_dir = _safe_resolve(Path(override)) if override else default_resolved
            # Resolve both sides so a symlinked/relative path still matches; an
            # unresolvable proc dir (proc_dir is None) is treated as pinning
            # (conservative: under-detecting a live session is the unsafe way).
            if proc_dir is None or proc_dir == slot:
                found.append(PinningSession(proc.info["pid"], " ".join(cmdline)))
        except Exception:  # noqa: BLE001 - a vanished/denied process is not our switch's problem
            continue
    return found


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
    blob = _read_slot_blob(record.cli)
    if blob is None or not blob.strip():
        raise ManagedStoreError(
            f"no current {record.cli} login to snapshot for '{record.id}' "
            "(sign in first, then register)"
        )
    try:
        adir = account_dir(record.id, root)
        adir.mkdir(parents=True, exist_ok=True)
        os.chmod(adir, 0o700)
        _atomic_write_private(_blob_path(record.id, root), blob)
        meta = {
            "cli": record.cli,
            "account_id": record.account_id or record.id,
            "captured_at": _utc_now_iso(),
            "kind": "keychain" if (record.cli == "claude" and sys.platform == "darwin") else "file",
        }
        _atomic_write_private(_meta_path(record.id, root), json.dumps(meta, indent=2))
    except OSError as exc:
        raise ManagedStoreError(f"failed to write snapshot for '{record.id}': {exc}") from exc
    return adir


def read_meta(record_id: str, root: Path | None = None) -> Optional[dict]:
    try:
        return json.loads(_meta_path(record_id, root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def snapshot_age_label(record_id: str, root: Path | None = None) -> str:
    """Human 'snapshot 3d' style age for `list`; 'none' when unregistered."""
    meta = read_meta(record_id, root)
    if not meta or "captured_at" not in meta:
        return "none"
    try:
        captured = datetime.strptime(meta["captured_at"], "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=timezone.utc
        )
    except (ValueError, TypeError):
        return "unknown"
    delta = datetime.now(timezone.utc) - captured
    days = delta.days
    if days >= 1:
        return f"{days}d"
    hours = delta.seconds // 3600
    if hours >= 1:
        return f"{hours}h"
    return f"{delta.seconds // 60}m"


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


# ---------------------------------------------------------------------------
# Switch (materialize with both guards)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SwitchResult:
    active: str  # a returned result is always verified; failure raises instead
    slot_changed: bool = True
    verification: str = "structural"
    reason: Optional[str] = None


def _rollback_materialized_slot(cli: str, rollback_blob: Optional[str]) -> tuple[str, bool]:
    if not rollback_blob:
        return "nothing to roll back to; slot may hold the unverified target", False
    try:
        _write_slot_blob(cli, rollback_blob)
    except ManagedStoreError as exc:
        return f"rollback ALSO failed ({exc}); slot is in an indeterminate state", False
    return "slot rolled back to the previous account", True


def switch(
    target: ProviderRecord,
    *,
    by_id: dict[str, ProviderRecord],
    root: Path | None = None,
    emit_fn: Optional[Callable[..., None]] = None,
) -> SwitchResult:
    """Materialize ``target`` into the slot with capture-before-overwrite + the
    live-pin gate, serialized by a cross-process mutex. Rolls the slot back and
    raises on a failed post-materialize verification.

    Raises ``SwitchDeferred`` when the slot is pinned or the mutex is held,
    ``KeychainError``/``ManagedStoreError`` on a failed write with a receipt.
    """
    root = root or store_root()
    root.mkdir(parents=True, exist_ok=True)
    lock = filelock.FileLock(str(_switch_lock_path(root)), timeout=10)
    try:
        lock.acquire()
    except filelock.Timeout as exc:
        raise SwitchDeferred("another switch is in progress (mutex held); try again") from exc
    try:
        return _switch_locked(target, by_id=by_id, root=root, emit_fn=emit_fn)
    finally:
        lock.release()


def _switch_locked(
    target: ProviderRecord,
    *,
    by_id: dict[str, ProviderRecord],
    root: Path,
    emit_fn: Optional[Callable[..., None]],
) -> SwitchResult:
    stored = _blob_path(target.id, root)
    try:
        target_blob = stored.read_text(encoding="utf-8")
    except OSError as exc:
        raise ManagedStoreError(
            f"no credential snapshot for '{target.id}' at {stored} - run "
            f"`fno providers register {target.id}` first"
        ) from exc
    if not target_blob.strip():
        raise ManagedStoreError(f"credential snapshot for '{target.id}' is empty; refusing to materialize")

    outgoing_id = active_slot_id(target.cli, root)  # this CLI's slot occupant
    if outgoing_id == target.id and verify_slot(target, target_blob):
        # Stamp says target AND the slot actually reads back target's blob:
        # a true no-op. If the slot was changed out-of-band (manual /login,
        # stale stamp after a partial failure), verify_slot is False and we
        # fall through to re-materialize rather than falsely report success.
        return SwitchResult(
            active=target.id,
            slot_changed=False,
            reason="slot-already-active" if target.cli == "codex" else None,
        )

    # Live-pin gate INSIDE the critical section (a session starting between
    # check and write is caught: the mutex is held across both). Per-CLI: claude
    # keys off CLAUDE_CONFIG_DIR, codex off CODEX_HOME (both never rewrite the
    # slot under a live session on it).
    pins = pinning_sessions_for(target.cli)
    if pins:
        names = ", ".join(f"pid {p.pid}" for p in pins)
        raise SwitchDeferred(
            f"slot is pinned by a live {target.cli} session ({names}); stop it or retry",
            sessions=pins,
        )

    # Capture-before-overwrite: the slot currently holds the outgoing account's
    # (possibly rotated) creds. Re-snapshot them before we overwrite the slot.
    rollback_blob: Optional[str] = _read_slot_blob(target.cli)
    if outgoing_id and outgoing_id in by_id:
        try:
            snapshot_current(by_id[outgoing_id], root)
        except KeychainError:
            # A real read failure over a live credential must NOT be swallowed:
            # proceeding would overwrite the slot and lose the outgoing account's
            # rotated refresh token. Abort with the receipt; slot still untouched.
            raise
        except ManagedStoreError:
            # No readable outgoing login (fresh slot / already cleared): nothing
            # to capture, so nothing to lose. Proceed.
            pass

    _write_slot_blob(target.cli, target_blob)

    if not verify_slot(target, target_blob):
        # Verification failed: roll the slot back to the captured outgoing blob.
        # Tell the truth about the resulting slot state - operators act on it.
        tail, _ = _rollback_materialized_slot(target.cli, rollback_blob)
        raise ManagedStoreError(
            f"switch to '{target.id}' failed verification (stored token may be "
            f"stale/revoked); {tail}"
        )

    verification = "structural"
    verification_reason: Optional[str] = None
    if target.cli == "codex":
        try:
            login = _codex_login_ok()
        except ManagedStoreError as exc:
            tail, _ = _rollback_materialized_slot(target.cli, rollback_blob)
            raise ManagedStoreError(f"codex login verification failed ({exc}); {tail}") from exc
        except KeyboardInterrupt as exc:
            tail, rolled_back = _rollback_materialized_slot(target.cli, rollback_blob)
            if rolled_back:
                raise
            raise ManagedStoreError(
                f"codex login verification was interrupted; {tail}"
            ) from exc
        if login.ok is False:
            tail, _ = _rollback_materialized_slot(target.cli, rollback_blob)
            raise ManagedStoreError(
                f"switch to '{target.id}' was not recognized by `codex login status`; {tail}"
            )
        if login.ok is True:
            verification = "codex-recognized"
        else:
            verification_reason = login.reason

    # Codex TOCTOU narrowing (cv-f578cbe7): the pin gate above runs BEFORE the
    # write, so a codex launched in the snapshot+write window - having read the
    # OUTGOING creds at startup - is not caught by it. Re-scan once here; if one
    # appeared, roll the slot back to the outgoing creds and defer, so we never
    # LEAVE auth.json rewritten under a session that started mid-switch. This is
    # best-effort, not a full fix: a launch in the tiny write->recheck gap is
    # irreducible without a lease the external codex binary honors. claude keeps
    # G1's single pre-write check (this arm only, by request).
    if target.cli == "codex":
        late_pins = pinning_sessions_for(target.cli)
        if late_pins:
            names = ", ".join(f"pid {p.pid}" for p in late_pins)
            if not rollback_blob:
                raise SwitchDeferred(
                    f"a live {target.cli} session ({names}) started during the switch "
                    "(no prior creds to restore); retry once it exits",
                    sessions=late_pins,
                )
            try:
                _write_slot_blob(target.cli, rollback_blob)
            except ManagedStoreError as rb:
                raise ManagedStoreError(
                    f"a live {target.cli} session started mid-switch and the rollback "
                    f"ALSO failed ({rb}); slot may hold '{target.id}' under a live session"
                ) from rb
            raise SwitchDeferred(
                f"a live {target.cli} session ({names}) started during the switch; "
                "slot rolled back to the previous account, retry once it exits",
                sessions=late_pins,
            )

    # Crash window: a kill between the slot write above and this stamp leaves the
    # stamp naming the previous account while the slot holds target. Rare and
    # self-correcting on the next successful switch; journaling is not worth it
    # for a manual v1 (US3's daemon path can revisit if a postmortem shows it).
    stamp_active_slot(target.cli, target.id, root)
    if emit_fn is not None:
        event = {
            "provider": target.id,
            "account_id": target.account_id or target.id,
            "outgoing": outgoing_id or "",
        }
        if target.cli == "codex":
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
    )
