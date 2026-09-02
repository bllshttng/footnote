"""Graph store CLIENT: every read and mutation rides the store keeper.

The store itself is ported: `crates/fno-agents/src/graph_store.rs` owns the
byte-compatible JSON I/O, the defaults/migration pipeline,
``recompute_statuses``, canonicalization, slugs, the bounded lock, and the
atomic publish with its backup + SHA256 sidecar. This module is the RPC
client the 32k lines of Python surface call; the public signatures are the
ones the file-reading store exposed, so no caller changed shape.

Transport: `crates/fno-agents/src/graph_keeper.rs` hosts ONE graph file per
keeper process and serves tagged length-prefix frames (``u8 tag | u32 LE
length | payload``, the pane keeper's shape; the version rides Identify).
The socket is a sibling of the graph file (``<graph>.store.sock``), so a tmp
test graph never touches the operator's state root. When no keeper is
listening, the client spawns one (`fno-agents-worker --store-keeper`); when
one cannot be reached or spawned, every call raises
:class:`StoreUnavailable` naming the keeper-lane state - an unreachable
store is NEVER read as an empty graph (the absence-as-answer trap).

The mutation cycle is a versioned transaction: ``begin`` returns the
defaulted entries plus the file's content digest, the mutator runs
client-side against that snapshot, and ``commit`` refuses to publish over a
changed file, so an interleaved writer turns into a retry instead of a
silent clobber. The claim-release hook, renders, and the active-backlog
nudge run after the commit lands, exactly where the flock version ran them
after the lock dropped.

Read-failure taxonomy (unchanged): :class:`GraphCorruptError` (the soft
read's parse failure, swallowed to [] by read_graph, exit 1 by the mutate
path), :class:`GraphUnreadableError` / :class:`GraphMalformedRootError`
(the strict read). load.py's GraphCorruptionError is the SHA256 sidecar
axis, checked only by load_graph.
"""
from __future__ import annotations

import base64 as _base64
import hashlib
import json
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import sys
import time
from datetime import datetime, timedelta, timezone
from typing import Any
from pathlib import Path

from fno.graph._constants import (  # noqa: F401  GRAPH_MD re-exported: patched via store.GRAPH_MD
    GRAPH_JSON,
    GRAPH_MD,
)

# Transaction retry budget: each begin/commit pair re-reads under the
# keeper's lock, so a conflict means an interleaved writer landed and the
# retry sees fresh data. Five is generous for human-rate contention.
_TX_ATTEMPTS = 5

# Bounded lock deadline handed to the keeper (its own default is 10s when
# the spawn omits the flag).
_LOCK_TIMEOUT_SECS = 10

# Frame tags; mirrored by crates/fno-agents/src/graph_keeper.rs.
_TAG_REQUEST = 1
_TAG_SHUTDOWN = 2
_TAG_IDENTIFY = 3
_TAG_RESPONSE = 4
_TAG_IDENTIFY_REPLY = 5
# Mirrors the keeper's cap: a large canonical graph answers a read with a
# same-order JSON array (11 MB file -> an entries reply of that order).
_MAX_FRAME_BYTES = 64 * 1024 * 1024

# Socket states reported by StoreUnavailable, named after the vocabulary in
# fno.agents.keeper_lane (the UNKNOWN discipline: dead is not unknown).
# Sockets are bound under this byte budget: macOS's sockaddr_un sun_path is
# 104 bytes and connect() fails with AF_UNIX path errors past it.
_SOCK_PATH_LIMIT = 96

STATE_NO_LISTENER = "no_listener"
STATE_ABSENT = "absent"
STATE_SPAWN_FAILED = "spawn_failed"
STATE_UNREACHABLE = "unreachable"
STATE_SILENT = "silent"


class GraphCorruptError(Exception):
    """Raised when graph.json or graph-archive.json cannot be parsed as JSON.

    Distinguishes genuine corruption (unparseable bytes) from a valid-but-empty
    graph ({"entries": []}).
    """


class GraphUnreadableError(Exception):
    """Strict-read failure: the graph exists but could not be read as a graph.

    A resolution caller that catches this KNOWS the graph could not be read,
    instead of concluding the node is absent (the duplicate-filing class).
    ``read_graph`` -- the soft display path -- never raises this; it swallows
    to [].
    """


class GraphMalformedRootError(GraphUnreadableError):
    """Root JSON is an object but has no 'entries' key.

    Distinct from a legitimately empty ``{"entries": []}``. A subclass so a
    caller needing only "unreadable vs absent" catches the base, while one
    wanting the finer distinction catches this.
    """


class StoreUnavailable(RuntimeError):
    """The store keeper is unreachable and could not be started.

    Carries the keeper-lane state that applies (never an empty graph: a
    refused store must read as refused). ``state`` is one of the
    ``STATE_*`` constants above.
    """

    def __init__(self, state: str, detail: str):
        self.state = state
        self.detail = detail
        super().__init__(f"graph store unavailable ({state}): {detail}")


class GraphLockTimeout(TimeoutError):
    """The store's bounded lock stayed busy past its deadline.

    The bounded-lock property of the ported store: acquisition takes a
    deadline and answers inside it, never blocks forever.
    """


# ---------------------------------------------------------------------------
# Keeper transport
# ---------------------------------------------------------------------------

def store_socket_for(path: Path) -> Path:
    """The keeper socket for a graph file: a ``<graph>.store.sock`` sibling.

    When the sibling would overrun the unix-socket address limit (macOS
    binds 104 sun_path bytes; a hermetic-test sandbox home or a deep tmp
    graph overruns it easily, directory included), the socket moves to a
    uid-keyed root under the platform temp dir and is named by the graph
    path's hash: still private (0700 root, per-user), still discoverable by
    the daemon sweep, and stable for the same graph path. The Rust keeper
    mirrors this rule; the client is the path authority (it passes --sock).
    """
    path = Path(path)
    sibling = path.parent / f"{path.name}.store.sock"
    if len(str(sibling).encode()) <= _SOCK_PATH_LIMIT:
        return sibling
    try:
        absolute = str(path.resolve())
    except OSError:
        absolute = str(path)
    digest = hashlib.sha256(absolute.encode()).hexdigest()[:16]
    root = Path(tempfile.gettempdir()) / f"fno-store-{os.getuid()}"
    return root / f"{digest}.sock"


def _worker_binary() -> Path | None:
    """Locate `fno-agents-worker` for an on-demand keeper spawn.

    Dev-checkout artifacts come before PATH on purpose: a stale `cargo
    install`ed worker predating the `--store-keeper` lane exits 0 with a
    usage refusal, and a silent old binary is worse than an honest absence.
    The worker is a sibling of the daemon binary everywhere it ships, and it
    is never deleted by the smoke shard's @requires_rust clear (which
    removes only `fno-agents`), so the store keeps working where the parity
    suites skip.
    """
    env = os.environ.get("FNO_AGENTS_WORKER")
    if env:
        candidate = Path(env)
        if candidate.is_file():
            return candidate
    front = os.environ.get("FNO_AGENTS_FRONT")
    if front:
        candidate = Path(front).parent / "fno-agents-worker"
        if candidate.is_file():
            return candidate
    here = Path(__file__).resolve()
    for ancestor in here.parents:
        if (ancestor / "crates" / "fno-agents").is_dir():
            for base in (ancestor / "crates/fno-agents/target", ancestor / "target"):
                for profile in ("debug", "release"):
                    candidate = base / profile / "fno-agents-worker"
                    if candidate.is_file() and os.access(candidate, os.X_OK):
                        return candidate
            break
    found = shutil.which("fno-agents-worker")
    if found:
        return Path(found)
    try:
        from fno.rust_binary import resolve_binary

        sibling = resolve_binary()
        if sibling is not None:
            candidate = sibling.parent / "fno-agents-worker"
            if candidate.is_file():
                return candidate
    except Exception:  # noqa: BLE001 - discovery degrades to absence
        pass
    return None


def _is_canonical(path: Path) -> bool:
    """True when `path` IS the configured canonical graph. Resolved through
    `paths.graph_json()` at call time (the same seam `_constants.__getattr__`
    serves), so config overrides AND test monkeypatches both land here.
    Reading the RESOLVER, not the facade, also keeps this immune to the
    monkeypatch baking trap: a test that patches the facade and restores it
    leaves a frozen attribute behind."""
    try:
        from fno import paths as _paths

        configured = _paths.graph_json()
    except Exception:  # noqa: BLE001 - a broken config owns no graph
        return False
    try:
        return path.resolve() == configured.resolve()
    except OSError:
        return False


def _spawn_keeper(path: Path) -> subprocess.Popen:
    """Spawn a store keeper for `path`, detached from this process's group."""
    binary = _worker_binary()
    if binary is None:
        raise StoreUnavailable(
            STATE_SPAWN_FAILED,
            "fno-agents-worker not found (set FNO_AGENTS_WORKER or install the runtime)",
        )
    sock = store_socket_for(path)
    argv = [
        str(binary),
        "--store-keeper",
        "--sock",
        str(sock),
        "--graph",
        str(path),
        "--session",
        f"store-{os.getpid()}",
        "--lock-timeout-secs",
        str(_LOCK_TIMEOUT_SECS),
    ]
    if _is_canonical(path):
        argv.append("--canonical")
    return subprocess.Popen(
        argv,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


class _Keeper:
    """One request to one store keeper socket. Frames are one-shot: connect,
    send, read the reply, close. Persistence buys nothing at CLI rates and
    the keeper serves each connection on its own thread."""

    def __init__(self, sock: Path, connect_timeout: float = 5.0, read_timeout: float = 60.0):
        self.sock = sock
        self.connect_timeout = connect_timeout
        self.read_timeout = read_timeout

    def _connect(self):
        stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        stream.settimeout(self.connect_timeout)
        try:
            stream.connect(str(self.sock))
        except FileNotFoundError:
            stream.close()
            raise StoreUnavailable(STATE_ABSENT, f"{self.sock} does not exist") from None
        except ConnectionRefusedError:
            stream.close()
            raise StoreUnavailable(
                STATE_NO_LISTENER, f"nothing is listening on {self.sock}"
            ) from None
        except OSError as exc:
            stream.close()
            raise StoreUnavailable(STATE_UNREACHABLE, str(exc)) from None
        return stream

    def request(self, method: str, params: dict) -> Any:
        try:
            stream = self._connect()
        except FileNotFoundError:
            raise StoreUnavailable(STATE_ABSENT, f"{self.sock} does not exist") from None
        except ConnectionRefusedError:
            raise StoreUnavailable(
                STATE_NO_LISTENER, f"nothing is listening on {self.sock}"
            ) from None
        except OSError as exc:
            raise StoreUnavailable(STATE_UNREACHABLE, str(exc)) from None
        try:
            req_id = 1
            payload = json.dumps({"id": req_id, "method": method, "params": params}).encode()
            frame = bytes([_TAG_REQUEST]) + struct.pack("<I", len(payload)) + payload
            stream.settimeout(self.read_timeout)
            stream.sendall(frame)
            header = b""
            while len(header) < 5:
                chunk = stream.recv(5 - len(header))
                if not chunk:
                    raise StoreUnavailable(STATE_SILENT, "keeper closed the connection mid-frame")
                header += chunk
            if header[0] != _TAG_RESPONSE:
                raise StoreUnavailable(
                    STATE_SILENT, f"unexpected frame tag {header[0]} from keeper"
                )
            (length,) = struct.unpack_from("<I", header, 1)
            if length > _MAX_FRAME_BYTES:
                raise StoreUnavailable(STATE_SILENT, f"oversized reply frame ({length} bytes)")
            data = b""
            while len(data) < length:
                chunk = stream.recv(length - len(data))
                if not chunk:
                    raise StoreUnavailable(STATE_SILENT, "keeper closed the connection mid-reply")
                data += chunk
            reply = json.loads(data.decode("utf-8"))
        except (OSError, ValueError) as exc:
            raise StoreUnavailable(STATE_UNREACHABLE, str(exc)) from None
        finally:
            stream.close()
        if reply.get("ok"):
            return reply.get("result")
        error = reply.get("error") or {}
        _raise_store_error(error.get("kind", "invalid"), str(error.get("message", error)))

    # -- typed helpers -----------------------------------------------------
    # The keeper is single-graph (it binds to whatever --graph named), so
    # read/read_file/begin carry no path; only read_archive does, because the
    # archive is a DIFFERENT file than the one the keeper owns.

    def read(self, path: Path, *, strict: bool = False, keep_malformed: bool = False) -> dict:
        del path  # single-graph keeper: the bound graph IS the target
        try:
            return self.request(
                "read" if not strict else "read_strict",
                {"strict": strict, "keep_malformed": keep_malformed},
            )
        except GraphCorruptError as exc:
            if not strict:
                raise
            # Taxonomy, not wording: the strict read's contract is that EVERY
            # parse failure is a GraphUnreadableError (cli.py catches that
            # class to tell "graph unreadable" from "node absent"), while the
            # soft path's parse failure is the swallower's GraphCorruptError.
            # The keeper carries one kind for both; the client splits it.
            raise GraphUnreadableError(str(exc)) from None

    def read_file(self, path: Path) -> dict:
        del path
        return self.request("read_file", {})


def _client_for(path: Path, *, spawn: bool = True) -> _Keeper:
    """A keeper connection for `path`, spawning the keeper when absent.

    Probes with a real connect: `_Keeper` is lazy, so only a connect can
    tell a live listener from a stale socket file. A positively dead socket
    (absent / refused) gets a spawn and a bounded re-probe loop; anything
    else is raised as the state that applies.
    """
    path = Path(path)
    sock = store_socket_for(path)
    keeper = _Keeper(sock)
    try:
        probe = keeper._connect()
        probe.close()
        return keeper
    except StoreUnavailable as exc:
        if not spawn or exc.state not in (STATE_ABSENT, STATE_NO_LISTENER):
            raise
    proc = _spawn_keeper(path)
    deadline = time.monotonic() + 10.0
    last: StoreUnavailable | None = None
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            # Our spawned worker died before binding (a stale binary without
            # the store lane, or the connect-before-bind refusal because a
            # concurrent client's keeper won the socket). Probe once: a live
            # listener means the seat is taken by a valid keeper and the
            # request can ride it; still dead means our binary is the
            # problem, and that never justifies waiting out the clock.
            try:
                probe = keeper._connect()
                probe.close()
                return keeper
            except StoreUnavailable as exc:
                raise StoreUnavailable(
                    STATE_SPAWN_FAILED,
                    f"keeper exited immediately with code {proc.returncode} "
                    f"({proc.args!r}); is fno-agents-worker current? "
                    "`fno doctor` names lag",
                ) from exc
        try:
            probe = keeper._connect()
            probe.close()
            return keeper
        except StoreUnavailable as exc:
            last = exc
            time.sleep(0.05)
    raise last or StoreUnavailable(STATE_SILENT, "keeper never answered")


def _raise_store_error(kind: str, message: str) -> None:
    if kind == "corrupt":
        raise GraphCorruptError(message)
    if kind == "malformed_root":
        raise GraphMalformedRootError(message)
    if kind == "unreadable":
        raise GraphUnreadableError(message)
    if kind == "lock_timeout":
        raise GraphLockTimeout(message)
    if kind == "empty_field_update":
        raise ValueError(message)
    if kind == "conflict":
        raise _Conflict()
    raise RuntimeError(f"store error ({kind}): {message}")


class _Conflict(Exception):
    """Internal: the commit's snapshot is stale; the tx loop retries."""


# ---------------------------------------------------------------------------
# Pure helpers (ported; served by the keeper's pure methods)
# ---------------------------------------------------------------------------

def normalize_plan_path(path: str | None) -> str | None:
    """Normalize a ``plan_path`` for comparison across graph / ledger and
    across absolute-vs-relative + trailing-slash conventions.

    The one normalizer behind every plan-path guard (the ported Rust
    implementation answers through the keeper; see x-04b9 for why every
    comparison site routes through one function).
    """
    result = _client_for(GRAPH_JSON).request("normalize_plan_path", {"path": path})
    return result["path"] if isinstance(result, dict) else None


def plan_path_owner_conflict(
    entries: list[dict], node_id: str | None, plan_path: str | None
) -> str | None:
    """Return the id of another node already bound to the same ``plan_path``.

    The one-plan-one-node invariant (x-04b9); the check lives in the ported
    store so a write site cannot reimplement it.
    """
    result = _query(entries, "plan_path_owner_conflict", {
        "node_id": node_id,
        "plan_path": plan_path,
    })
    return result.get("owner")


def set_related(entries: list[dict], node_id: str, desired: list[str]) -> None:
    """Declare ``node_id``'s related set and mirror it onto every peer.

    Symmetry is stored on both endpoints, not derived. Mutates ``entries``
    in place (both halves land in the caller's ``locked_mutate_graph`` call,
    so a half-written edge aborts the mutation before anything persists).
    """
    out = _pure(entries, "set_related", {"node_id": node_id, "desired": desired})
    entries[:] = out


def canonicalize_entries(entries: list[dict]) -> list[dict]:
    """Reorder each entry's keys status-forward and refresh the children
    index. Returns a new list of new dicts; the ported implementation is the
    only one."""
    result = _client_for(GRAPH_JSON).request(
        "pure_op",
        {"name": "canonicalize", "params": {}, "entries": entries},
    )
    return [e for e in result["entries"]]


def _pure(entries: list[dict], name: str, params: dict) -> list[dict]:
    result = _client_for(GRAPH_JSON).request(
        "pure_op", {"name": name, "params": params, "entries": entries}
    )
    return result["entries"]


def _query(entries: list[dict], name: str, params: dict) -> dict:
    result = _client_for(GRAPH_JSON).request(
        "pure_op", {"name": name, "params": params, "entries": entries}
    )
    return result.get("op") or {}


# Shims over the ported pipeline, for callers holding entries in memory
# (scoreboard fold, drift checks). One implementation: the keeper's.

def _apply_graph_defaults(entries: list[dict], *, keep_malformed: bool = False) -> list[dict]:
    """The one migration seam, answered by the ported store."""
    result = _client_for(GRAPH_JSON).request(
        "defaults", {"entries": entries, "keep_malformed": keep_malformed}
    )
    return result["entries"]


def recompute_statuses_via_store(entries: list[dict]) -> list[dict]:
    """The write-path status cascade (statuses.recompute_statuses), answered
    by the ported store. Pure over the given rows: no file I/O, no publish."""
    result = _client_for(GRAPH_JSON).request("recompute", {"entries": entries})
    return result["entries"]


def apply_readiness_overlay_via_store(entries: list[dict]) -> list[dict]:
    """The read-time readiness overlay (blocked/blocked_reason), answered by
    the ported store. The locked_mutate_graph render pass runs it post-commit
    so a mutation that newly blocks a sibling renders current graph.md."""
    result = _client_for(GRAPH_JSON).request("overlay", {"entries": entries})
    return result["entries"]


def canonical_field_order() -> "list[str]":
    """The canonical key order the store publishes, from the one source of
    truth (graph_store.rs CANONICAL_FIELD_ORDER)."""
    result = _client_for(GRAPH_JSON).request("canonical_field_order", {})
    return list(result["fields"])


def read_file_bytes(path: Path) -> bytes:
    """The file's raw bytes through the keeper's gated read.

    load.py's hash validation consumes this: the gate held across the read
    means the bytes and the sidecar the keeper last wrote answer one
    consistent instant, so a mismatch is real corruption, never the
    two-write window."""
    result = _client_for(Path(path)).read_file(Path(path))
    return _base64.b64decode(result["bytes_b64"])


def _read_json(path: Path) -> list[dict]:
    """Raw read of a JSON entries file through the keeper's byte read.

    Raises GraphCorruptError on JSON parse failure OR when the root value is
    not a JSON object. A missing file or a valid file with no/empty entries
    key returns [] -- those are NOT corruption.
    """
    path = Path(path)
    if not path.exists():
        return []
    raw = read_file_bytes(path)
    try:
        data = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as exc:
        raise GraphCorruptError(str(path)) from exc
    if not isinstance(data, dict):
        raise GraphCorruptError(str(path))
    entries = data.get("entries", [])
    if not isinstance(entries, list):
        raise GraphCorruptError(str(path))
    return entries


def _write_json(entries: list[dict], path: Path) -> None:
    """Raw atomic write of an entries file. ARCHIVE ONLY: the working graph's
    write path is the keeper's publish pipeline, and hand-rolling one here is
    exactly the two-write window the port retired. The archive store keeps
    its own readers and lifetime (out of the port's scope), and its writers
    keep this primitive."""
    path = Path(path)
    data = {"entries": entries}
    tmp = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    try:
        tmp.write_text(json.dumps(data, indent=2) + "\n")
        os.replace(tmp, path)
    except Exception:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise


def _graph_lock_path(path: Path) -> Path:
    """The keeper's lockfile for a graph file: ``<canonical path>.lock``.

    Mirrors `graph_store.rs graph_lock_path` (a path formula, not store
    logic): resolved so two spellings of one graph share one inode, and a
    resolve failure (a symlink loop, a hostile tree) degrades to the raw
    spelling rather than raising.
    """
    path = Path(path)
    try:
        base = path.resolve()
    except (OSError, RuntimeError):
        base = path
    return base.with_name(base.name + ".lock")


# ---------------------------------------------------------------------------
# Reads
# ---------------------------------------------------------------------------

def read_graph(path: Path = GRAPH_JSON) -> list[dict]:
    """Read graph.json through the keeper, defaults applied. No lock needed.

    Swallows corruption on the read path -- commands like `status` and `ready`
    should not crash a user's terminal when graph.json is wedged. An
    UNREACHABLE store is different: it raises StoreUnavailable, never an
    empty graph.
    """
    try:
        result = _client_for(path).read(path)
    except GraphCorruptError:
        print(f"Warning: {path} is corrupt, backup saved to "
              f"{path.with_suffix('.json.bak')}", file=sys.stderr)
        return []
    return result["entries"]


def read_graph_strict(path: Path = GRAPH_JSON) -> list[dict]:
    """Failure-surfacing counterpart to :func:`read_graph`.

    Returns entries (defaults applied) for a populated OR legitimately empty
    graph, and for an absent file. RAISES instead of returning [] when the
    graph cannot be read cleanly, so a resolution caller can tell "node
    absent" apart from "graph unreadable". Diagnosis is read-only: no .bak
    is written on this path.
    """
    return _client_for(path).read(path, strict=True)["entries"]


def entries_with_archive(entries: list) -> list:
    """``entries`` plus archived nodes, the working graph winning on id.

    Best-effort and read-only: an absent or unreadable archive degrades to
    the working graph. The archive store keeps its own readers and lifetime;
    only its bytes ride the keeper.
    """
    from fno.paths import graph_archive_json

    try:
        archive_path = graph_archive_json()
        if not archive_path.exists():
            return entries
        live = {e.get("id") for e in entries if isinstance(e, dict)}
        archived = _client_for(archive_path).request(
            "read_archive", {"path": str(archive_path)}
        )["entries"]
        return [
            *entries,
            *(a for a in archived if isinstance(a, dict) and a.get("id") not in live),
        ]
    except Exception:  # noqa: BLE001 - archive is advisory; any read failure degrades
        return entries


def read_graph_with_archive(path: Path | None = None) -> list[dict]:
    """Read the working graph through the canonical seam, then overlay archive."""
    if path is None:
        from fno.paths import graph_json

        path = graph_json()
    return entries_with_archive(read_graph_strict(path))


# ---------------------------------------------------------------------------
# The mutation cycle
# ---------------------------------------------------------------------------

def _validate_company_work(entries: list[dict]) -> None:
    """company_work validation: pydantic stays client-side (the mutator ran
    here), and the validated refs round-trip exactly as the file leg wrote
    them."""
    from fno.company.contracts import validate_company_work_for_node

    for entry in entries:
        if not isinstance(entry, dict) or entry.get("company_work") is None:
            continue
        entry_id = entry.get("id")
        if not isinstance(entry_id, str):
            raise ValueError("company_work graph entry requires a string id")
        refs = validate_company_work_for_node(
            entry["company_work"], entry_id, owner="graph entry id"
        )
        assert refs is not None
        entry["company_work"] = refs.model_dump(mode="json", exclude_unset=True)


def _finish_mutation(path: Path, outcome: dict) -> list[dict]:
    """Everything that follows a landed publish, shared by the commit cycle
    and the typed-op cycle: the dropped-row warning, the closure claim
    releases, the readiness overlay, the board renders, and the drain-daemon
    nudge. The file leg ran all of these after its lock dropped; the client
    runs them after the keeper's publish lands, which is the same position
    relative to other writers."""
    from fno.graph.render import render_graph_md
    from fno.paths import vault_root

    path = Path(path)
    dropped = outcome["dropped"]
    backup = outcome["backup"]
    if dropped > 0:
        where = (
            f"prior content is preserved in {Path(backup).name}"
            if backup
            else "NO backup was written, so this removal is not recoverable"
        )
        print(
            f"Warning: dropping {dropped} malformed graph "
            f"{'entry' if dropped == 1 else 'entries'} (not a JSON object) "
            f"from {path}; {where}",
            file=sys.stderr,
        )

    is_canonical = _is_canonical(path)
    # Claim releases run AFTER the publish: root resolution and recovery
    # mutexes never belong inside the store's critical section.
    for release in outcome["closure_releases"]:
        release_node_claim_at_closure(release["id"], rung=release["rung"])

    # Renders. The canonical board moved from under-the-lock to after-the-
    # publish with the port: the keeper serializes publishes, and these
    # projections are operator-chosen paths that must never hold (or wait
    # on) the graph lock. Bytes are never partial (atomic replaces).
    entries = outcome["entries"]
    # recompute (server-side) does not derive `blocked` - it is a read-time
    # overlay - so re-apply it before rendering, or a mutation that newly
    # blocks/unblocks a sibling renders stale in graph.md until the next
    # explicit read.
    try:
        entries = apply_readiness_overlay_via_store(entries)
    except Exception:  # noqa: BLE001 - a render-freshness pass never fails a landed publish
        pass
    from fno.graph import _constants as _gc

    md_target = _gc.GRAPH_MD if is_canonical else path.with_name("graph.md")
    try:
        _obsidian = vault_root() is not None
    except Exception:
        _obsidian = False
    try:
        render_graph_md(entries, md_target, obsidian=_obsidian)
    except OSError as e:
        print(f"Warning: graph.md render failed: {e}", file=sys.stderr)
    _archived = entries_with_archive(entries)
    if is_canonical:
        try:
            from fno.graph.roadmap_public import canonical_target, render_one_target

            _canonical_row = canonical_target()
            if _canonical_row is not None:
                render_one_target(_canonical_row, _archived)
        except Exception as e:
            print(f"Warning: canonical board render failed: {e}", file=sys.stderr)
        try:
            from fno.graph.roadmap_public import render_configured_targets

            render_configured_targets(_archived, skip_canonical=True)
        except Exception as e:
            print(f"Warning: configured render targets failed: {e}", file=sys.stderr)
    else:
        # Test and temporary graphs retain a sibling HTML artifact without
        # ever touching the operator's configured targets.
        try:
            from fno.graph.render_html import render_graph_html

            render_graph_html(_archived, path.with_name("graph.html"))
        except OSError as e:
            print(f"Warning: graph.html render failed: {e}", file=sys.stderr)
    # Wake the active-backlog drain daemon (x-c070): best-effort, never
    # wedges the mutation.
    try:
        from fno.active_backlog import touch_nudge

        touch_nudge()
    except Exception:
        pass
    return entries


def locked_mutate_graph(path: Path, mutator) -> list[dict]:
    """Locked read-modify-write via the store keeper. Recomputes statuses
    after mutation; retries on an interleaved writer; surfaces a wedged
    store as GraphLockTimeout inside the deadline instead of blocking.

    The mutator runs client-side against the begin snapshot; the keeper
    re-derives the write pipeline (slugs, statuses, touched_at, closure
    detection, canonicalization) and publishes under the bounded lock with
    backup + sidecar. Renders, claim releases, and the nudge run after the
    publish lands -- the same post-lock position the file leg used.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    client = _client_for(path)

    for attempt in range(_TX_ATTEMPTS):
        snap = client.request("begin", {})
        entries = mutator(snap["entries"])
        _validate_company_work(entries)
        try:
            outcome = client.request(
                "commit",
                {"version": snap["version"], "entries": entries},
            )
            break
        except _Conflict:
            if attempt == _TX_ATTEMPTS - 1:
                raise RuntimeError(
                    f"graph mutated under us {_TX_ATTEMPTS} times at {path}; retrying stopped"
                ) from None
            continue
    else:  # pragma: no cover - the for/else only fires without break/raise
        raise RuntimeError("unreachable: tx loop exited without a commit")
    return _finish_mutation(path, outcome)


# ---------------------------------------------------------------------------
# Node resolution + the targeted helpers (typed ops over the keeper)
# ---------------------------------------------------------------------------

def _resolve_node_id(client_keeper_path: Path, node_id: str) -> str | None:
    """Resolve a (possibly partial) node id against the begin snapshot.

    The fuzzy resolver is surface: it stays Python (`_intake._find_node`),
    reads the snapshot the mutation is keyed on, and the keeper op re-checks
    the resolved id under the lock.
    """
    from fno.graph._intake import _find_node

    snap = _client_for(client_keeper_path).request("begin", {})
    node = _find_node(snap["entries"], node_id)
    return node.get("id") if node else None


def append_progress_note(
    path: Path, node_id: str, note: dict
) -> "tuple[bool, str | None]":
    """Append a ``{ts, text}`` progress note to a node's ``progress_notes``
    (append-only), returning ``(found, plan_path)``. Shared by ``fno backlog
    note`` and the status-fanout backlog-progress adapter (x-2057)."""
    resolved = _resolve_node_id(Path(path), node_id)
    if resolved is None:
        return False, None
    result = _run_op(Path(path), "append_progress_note", {"node_id": resolved, "note": note})
    found = result["found"]
    return bool(found), result.get("plan_path")


def append_encounter(
    path: Path, node_id: str, record: dict
) -> "tuple[bool, str | None, str | None]":
    """Append one encounter, refusing a second from the same voter.

    Returns ``(appended, error, reason)``. ``reason`` is one of ``"missing"``,
    ``"duplicate"``, or ``"unidentified"`` -- a caller picks an exit code from
    a symbol, never from this function's prose.
    """
    resolved = _resolve_node_id(Path(path), node_id)
    if resolved is None:
        return False, f"no node resolves to '{node_id}'", "missing"
    result = _run_op(Path(path), "append_encounter", {"node_id": resolved, "record": record})
    return (
        bool(result.get("appended")),
        result.get("error"),
        result.get("reason"),
    )


def append_wave_note(path: Path, node_id: str, note: dict) -> tuple[bool, str | None]:
    """Append a structured wave note, refusing missing or terminal targets."""
    resolved = _resolve_node_id(Path(path), node_id)
    if resolved is None:
        return False, f"no node resolves to '{node_id}'"
    result = _run_op(Path(path), "append_wave_note", {"node_id": resolved, "note": note})
    return bool(result.get("found")), result.get("error")


def _run_op(path: Path, name: str, params: dict) -> dict:
    """One typed op through the keeper's full locked cycle, followed by the
    same post-publish duties a mutator-based write ran (renders, releases,
    nudge): the targeted helpers replaced locked_mutate_graph calls, so they
    carry the same visible effects."""
    path = Path(path)
    result = _client_for(path).request("op", {"name": name, "params": params})
    _finish_mutation(path, result["outcome"])
    return result["op"]


# Bounded ceiling for harness / session-id strings (x-b6e4).
_SESSION_STR_MAX = 200

_SESSION_PHASES = ("think", "blueprint", "do", "review", "ship")


def _utc_session_stamp(label: str, value: str) -> str:
    """Normalize a session-row timestamp to the canonical ``...Z`` form."""
    try:
        parsed = datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError as exc:
        raise ValueError(
            f"{label} must be an ISO-8601 timestamp, got {value!r}"
        ) from exc
    if parsed.tzinfo is None or parsed.utcoffset() != timedelta(0):
        raise ValueError(
            f"{label} must be a UTC timestamp (offset +00:00 / Z), got {value!r}"
        )
    return parsed.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _validate_session_identity(phase: str, harness: str, session_id: str) -> "tuple[str, str]":
    """Validate a session row's identity triple, returning the stripped pair.

    The keeper re-validates under the lock; this pass keeps the ValueError
    contract at the call boundary.
    """
    if phase not in _SESSION_PHASES:
        raise ValueError(
            f"invalid phase {phase!r}; expected one of {sorted(_SESSION_PHASES)}"
        )
    harness = (harness or "").strip()
    session_id = (session_id or "").strip()
    for label, value in (("harness", harness), ("session_id", session_id)):
        if not value:
            raise ValueError(f"{label} must be a non-empty string")
        if len(value) > _SESSION_STR_MAX:
            raise ValueError(f"{label} exceeds {_SESSION_STR_MAX} chars")
    return harness, session_id


def _observe_model(harness: str, session_id: str) -> dict:
    """What the row's session ACTUALLY answered as, read from its own
    transcript. Delegates to :func:`fno.provenance.observed.observed_model`
    -- the one truth source -- and returns its variant dict UNCHANGED.
    Never raises; a provenance field must not fail a claim release."""
    try:
        from fno.provenance.observed import observed_model, observed_model_for_session

        not_backed = observed_model(harness, None)
        if not_backed.get("kind") == "not-file-backed":
            return not_backed
        if len(session_id) < 32:
            return {
                "kind": "unreadable",
                "reason": f"session id {session_id!r} is prefix-shaped; "
                          "a glob match cannot be proven to be this session",
            }
        return observed_model_for_session(harness, session_id, os.getcwd())
    except Exception as exc:  # noqa: BLE001 - a reporting field never breaks a stamp
        return {"kind": "unreadable", "reason": f"{type(exc).__name__}: {exc}"}


def append_session_record(
    path: Path,
    node_id: str,
    *,
    phase: str,
    harness: str,
    session_id: str,
    effort: "str | None" = None,
    ended_at: "str | None" = None,
    started_at: "str | None" = None,
    merge_grant: "dict | None" = None,
) -> "tuple[bool, bool]":
    """Append a lifecycle record to a node's append-only ``sessions`` list,
    returning ``(found, added)`` (x-b6e4).

    Idempotent under the store's lock: appends only when ``(phase, harness,
    session_id)`` is absent; a duplicate fills only timestamps it left open
    (and ``merge_grant`` when absent, so the first resolved posture owns the
    row), and ``observed_model`` is the one field the LATEST stamp owns.
    Raises ``ValueError`` on an invalid identity/timestamp/merge_grant;
    ``found=False`` when the node is absent (no mutation).
    """
    harness, session_id = _validate_session_identity(phase, harness, session_id)
    if effort is not None:
        if not isinstance(effort, str) or not effort.strip():
            raise ValueError("effort must be a non-empty string when provided")
        if len(effort) > _SESSION_STR_MAX:
            raise ValueError(f"effort exceeds {_SESSION_STR_MAX} chars")
    if ended_at is not None:
        ended_at = _utc_session_stamp("ended_at", ended_at)
    if started_at is not None:
        started_at = _utc_session_stamp("started_at", started_at)

    # The spawner-resolved merge grant, carried on the do row so it outlives
    # the worker's transient manifest. Shape is validated HERE: a caller that
    # cannot name approved/source/recorded_by/recorded_at has no grant to
    # record, and a malformed one is refused rather than stored to be guessed
    # at later. The keeper re-validates at the store boundary.
    grant_row = None
    if merge_grant is not None:
        if not isinstance(merge_grant, dict):
            raise ValueError("merge_grant must be a mapping when provided")
        unknown = set(merge_grant) - {
            "approved", "source", "recorded_by", "recorded_at",
        }
        if unknown:
            raise ValueError(f"merge_grant carries unknown keys: {sorted(unknown)}")
        if not isinstance(merge_grant.get("approved"), bool):
            raise ValueError("merge_grant.approved must be a boolean")
        for key in ("source", "recorded_by", "recorded_at"):
            value = merge_grant.get(key)
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f"merge_grant.{key} must be a non-empty string")
            if key != "recorded_at" and len(value) > _SESSION_STR_MAX:
                raise ValueError(f"merge_grant.{key} exceeds {_SESSION_STR_MAX} chars")
        grant_row = {
            "approved": merge_grant["approved"],
            "source": merge_grant["source"],
            "recorded_by": merge_grant["recorded_by"],
            "recorded_at": _utc_session_stamp("recorded_at", merge_grant["recorded_at"]),
        }

    # Read the transcript BEFORE the mutation cycle: it is not a cheap read
    # and never belongs inside the store's critical section.
    observed = _observe_model(harness, session_id)
    resolved = _resolve_node_id(Path(path), node_id)
    if resolved is None:
        return False, False
    params = {
        "node_id": resolved,
        "phase": phase,
        "harness": harness,
        "session_id": session_id,
        "effort": effort,
        "started_at": started_at,
        "ended_at": ended_at,
        "observed": observed,
    }
    if grant_row is not None:
        params["merge_grant"] = grant_row
    result = _run_op(Path(path), "session_append", params)
    return bool(result["found"]), bool(result["added"])


def remove_open_session_record(
    path: Path,
    node_id: str,
    *,
    phase: str,
    harness: str,
    session_id: str,
    started_at: str,
) -> "tuple[bool, bool]":
    """Remove the still-OPEN lifecycle row an acquire opened, returning
    ``(found, removed)`` -- the one compensating write against the otherwise
    append-only ``sessions`` list.

    Four preconditions must ALL hold before a row is dropped: the identity
    matches, the row carries no ``ended_at``, and its ``started_at`` equals
    the caller's exactly (so an idempotent re-acquire cannot roll back an
    earlier real row)."""
    harness, session_id = _validate_session_identity(phase, harness, session_id)
    started_at = _utc_session_stamp("started_at", started_at)
    resolved = _resolve_node_id(Path(path), node_id)
    if resolved is None:
        return False, False
    result = _run_op(Path(path), "session_remove_open", {
        "node_id": resolved,
        "phase": phase,
        "harness": harness,
        "session_id": session_id,
        "started_at": started_at,
    })
    return bool(result["found"]), bool(result["removed"])


def reap_open_session_record(
    path: Path,
    node_id: str,
    *,
    phase: str,
    harness: str,
    session_id: str,
    ended_at: "str | None" = None,
) -> dict:
    """Close one exact open observer-owned session row and report settlement.

    ``do`` REMOVES the row (an open do window wedges node status in_progress,
    so after death the honest state is "no do window"); every other phase
    FILLS ``ended_at`` and keeps the row (a reviewer session's provenance did
    happen); ``all`` applies both semantics to every open row carrying the
    identity. The fill value defaults to the reap instant, an UPPER BOUND on
    the true end."""
    if phase != "all" and phase not in _SESSION_PHASES:
        raise ValueError(
            f"invalid phase {phase!r}; expected 'all' or one of {sorted(_SESSION_PHASES)}"
        )
    identity_phase = "do" if phase == "all" else phase
    harness_v, session_v = _validate_session_identity(identity_phase, harness, session_id)
    if ended_at is None:
        ended_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    else:
        ended_at = _utc_session_stamp("ended_at", ended_at)
    resolved = _resolve_node_id(Path(path), node_id)
    result = _run_op(Path(path), "session_reap_open", {
        "node_id": resolved,
        "phase": phase,
        "harness": harness_v,
        "session_id": session_v,
        "ended_at": ended_at,
    })
    # status_after/remaining_open_do: the settlement reader wants the POST
    # state; re-read once, best-effort.
    report = dict(result)
    try:
        entries = read_graph(Path(path))
        node = next((e for e in entries if e.get("id") == resolved), None)
        if node is None:
            report.update({"status_after": None, "remaining_open_do": 0, "settled": report.get("found", False)})
            return report
        rows = node.get("sessions") or []
        report["status_after"] = node.get("status")
        report["remaining_open_do"] = sum(
            1
            for row in rows
            if isinstance(row, dict)
            and row.get("phase") == "do"
            and isinstance(row.get("started_at"), str)
            and row["started_at"].strip()
            and "ended_at" not in row
        )
        report["settled"] = True
    except Exception:  # noqa: BLE001 - the settlement read is advisory
        report["settled"] = report.get("found", False)
    return report


# ---------------------------------------------------------------------------
# PR attribution + claims
# ---------------------------------------------------------------------------

def find_nodes_for_pr(
    path: Path, pr_number: int, *, repo: "str | None" = None
) -> "list[str]":
    """Node ids carrying ``pr_number``, optionally narrowed to one repo slug
    (x-d5f9: pr_number is not unique across repos; the url is the only
    per-node field carrying the repo slug)."""
    entries = read_graph(Path(path))
    result = _query(entries, "find_for_pr", {"pr_number": pr_number, "repo": repo})
    return result.get("ids", [])


def stamp_session_for_pr(
    path: Path,
    pr_number: int,
    *,
    phase: str,
    harness: str,
    session_id: str,
    effort: "str | None" = None,
    ended_at: "str | None" = None,
    started_at: "str | None" = None,
    repo: "str | None" = None,
) -> "tuple[str | None, str]":
    """Resolve the UNIQUE node carrying ``pr_number`` and append a lifecycle
    record, returning ``(node_id, status)`` (x-b6e4). ``status`` is ``added``
    | ``duplicate`` | ``no-node`` | ``ambiguous``; the last two leave the
    graph untouched (0 or >1 matches never fans out)."""
    matches = find_nodes_for_pr(path, pr_number, repo=repo)
    if not matches:
        return None, "no-node"
    if len(matches) > 1:
        return None, "ambiguous"
    node_id = matches[0]
    _found, added = append_session_record(
        path, node_id, phase=phase, harness=harness, session_id=session_id,
        effort=effort, ended_at=ended_at, started_at=started_at,
    )
    return node_id, ("added" if added else "duplicate")


def release_node_claim_at_closure(node_id: str, *, rung: str) -> None:
    """Drop the ``node:<id>`` claim a closure just made moot (x-94f8).

    Holder-agnostic: the closer is usually not the worker that holds the
    claim. Best-effort and loud: a release failure is a named stderr line,
    never a failed graph mutation - closure outranks release, and the
    reaper's node-aware settlement is the backstop.
    """
    from fno.claims.core import claim_path, force_release_claim
    from fno.claims.io import claims_root_for, dedup_claims_roots

    key = f"node:{node_id}"
    try:
        for raw_root, _dir in dedup_claims_roots([claims_root_for(key), None]):
            path = claim_path(key, root=raw_root)
            if not path.exists():
                continue
            force_release_claim(key, reason=f"node closed ({rung})", root=raw_root)
    except Exception as exc:  # noqa: BLE001 - closure must not fail on this
        print(
            f"node closure: claim release failed for {key}: {exc}",
            file=sys.stderr,
        )
