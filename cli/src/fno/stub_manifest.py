"""Stub-manifest: the durable artifact a `contract`-tier dependent's first pass
emits so a later merge-triggered reconciliation can de-stub from it (G3 of the
stub-and-reconcile design, x-24b7).

A `contract` dependent (see `fno backlog decompose`, G2) builds *now* against a
pinned interface contract, stubbing the parts that need its blocker landed, and
opens its PR as draft. It records each stub here. Two consumers read it:

  - `fno pr merge` refuses to merge while a node carries an UNreconciled manifest
    (the "a stubbed PR never merges with mocks" invariant; Locked Decision 4).
  - the G4 reconciliation pass (not built here) reads the manifest to know which
    stubs to swap for the real implementation, then flips `reconciled: true`.

Schema (`[{stub_id, file, symbol, contract_ref, kind}]` per the design's
Claude's-Discretion #1, wrapped in a small envelope)::

    {
      "node": "x-24b7",
      "contract_version": 1,
      "contract_ref": "design.md#interface-contract",
      "reconciled": false,
      "stubs": [
        {"stub_id": "create-user", "file": "src/api.ts",
         "symbol": "createUser", "contract_ref": "design.md#interface-contract",
         "kind": "function"}
      ]
    }

A manifest with zero stubs is valid (the dependent needed no stubs; reconcile is
a no-op re-validate). `reconciled` defaults false; reconciliation sets it true
and retains the file for audit, so "manifest present and unreconciled" is the
single hold signal.
"""
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path
from typing import Any, Optional

import typer

# Required keys on each stub entry. `symbol`/`contract_ref` are advisory (a stub
# may be a whole file), so only the locators every de-stub pass needs are forced.
_STUB_REQUIRED = ("stub_id", "file", "kind")


class StubManifestError(ValueError):
    """A manifest is malformed. Surfaced rather than silently shipping a
    half-real PR (design Failure Modes: manifest completeness is an AC)."""


def manifest_path(node_id: str, root: Path | str) -> Path:
    """`<root>/.fno/stub-manifest-<node>.json`. The manifest lives in the
    dependent's own project `.fno/`, keyed by node id (so two contract
    dependents on one blocker never collide)."""
    return Path(root) / ".fno" / f"stub-manifest-{node_id}.json"


def validate(data: Any) -> dict:
    """Return the manifest dict or raise StubManifestError. Checks only the
    load-bearing shape: a `node` string and a list of stubs each carrying the
    locators a reconcile pass needs."""
    if not isinstance(data, dict):
        raise StubManifestError("manifest must be a JSON object")
    node = data.get("node")
    if not isinstance(node, str) or not node.strip():
        raise StubManifestError("manifest 'node' must be a non-empty string")
    stubs = data.get("stubs")
    if not isinstance(stubs, list):
        raise StubManifestError("manifest 'stubs' must be a list (empty is valid)")
    for i, stub in enumerate(stubs):
        if not isinstance(stub, dict):
            raise StubManifestError(f"stub #{i + 1} is not an object")
        # An explicit null (`"stub_id": null`) must fail too -- str(None) is the
        # non-empty "None", so check for None before string-coercing (gemini).
        missing = [
            k for k in _STUB_REQUIRED
            if stub.get(k) is None or not str(stub.get(k)).strip()
        ]
        if missing:
            raise StubManifestError(
                f"stub #{i + 1} ({stub.get('stub_id', '?')}) missing: {', '.join(missing)}"
            )
    return data


def load(path: Path) -> dict:
    """Read + validate. Raises FileNotFoundError if absent, StubManifestError if
    malformed OR unreadable (bad JSON / bad encoding / OS error). An unreadable
    manifest must NOT escape as a raw exception: the merge guard's caller would
    swallow it and let a mocked PR merge (gemini high). Surface it as malformed
    so the guard holds instead."""
    try:
        with open(path, "r", encoding="utf-8") as fh:
            data = json.load(fh)
    except FileNotFoundError:
        raise
    except (json.JSONDecodeError, OSError, UnicodeDecodeError) as exc:
        raise StubManifestError(f"{path}: unreadable ({exc})") from exc
    return validate(data)


def write(
    node_id: str,
    stubs: list[dict],
    root: Path | str,
    *,
    contract_version: Optional[int] = None,
    contract_ref: Optional[str] = None,
    contract_test: Optional[str] = None,
    reconciled: bool = False,
) -> Path:
    """Validate and write the manifest, returning its path. Creates `.fno/` if
    needed.

    ``contract_test`` is the shell command the blocker ships so the G4 reconcile
    pass has an EXECUTABLE drift gate (Locked Decision 5). A manifest without it
    has no gate, so reconciliation refuses to auto-de-stub (fail-loud, never
    guess). Preserved verbatim through :func:`mark_reconciled`.
    """
    data = {
        "node": node_id,
        "contract_version": contract_version,
        "contract_ref": contract_ref,
        "contract_test": contract_test,
        "reconciled": reconciled,
        "stubs": stubs,
    }
    validate(data)
    path = manifest_path(node_id, root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    return path


def _node_pr_numbers(node: dict) -> set[int]:
    """Every PR number a node carries: its primary `pr_number` plus any in
    `additional_prs` (ints, or URLs ending `/pull/<n>`; codex P2). A contract
    dependent whose PR is recorded only in `additional_prs` must still be found
    or the merge guard is bypassed."""
    out: set[int] = set()
    candidates = [node.get("pr_number"), *(node.get("additional_prs") or [])]
    for raw in candidates:
        if raw is None:
            continue
        try:
            out.add(int(raw))
            continue
        except (TypeError, ValueError):
            pass
        m = re.search(r"/pull/(\d+)", str(raw))
        if m:
            out.add(int(m.group(1)))
    return out


def _node_for_pr(pr_number: int, graph_path: Optional[Path]) -> Optional[dict]:
    """The graph node carrying this pr_number, or None. Read-only; degrades to
    None on ANY graph trouble (the read AND the iteration are guarded, so a
    non-iterable / mid-iteration error never escapes to the merge path; gemini)."""
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        entries = read_graph(graph_path or graph_json())
        for e in entries:
            if pr_number in _node_pr_numbers(e):
                return e
    except Exception:
        return None
    return None


def unreconciled_manifest_for_pr(
    pr_number: int, root: Path | str, *, graph_path: Optional[Path] = None
) -> Optional[dict]:
    """Return the held manifest (with `_node` added) iff merging this PR would
    ship mocks: its node is `dep=contract` AND carries a manifest file that is
    not yet reconciled. None means "nothing holds this merge" — the default
    `hard` path and every non-contract PR fall through unchanged (AC6-EDGE)."""
    node = _node_for_pr(pr_number, graph_path)
    # ponytail: a hard node (or no node) never holds — keeps the x-a10e path
    # byte-for-byte unchanged. dep is popped to absent on hard nodes (G2).
    if not node or node.get("dep") != "contract":
        return None
    node_id = node.get("id")
    if not node_id:
        return None
    # From here we KNOW this PR is a contract dependent, so fail CLOSED on any
    # trouble (hold the merge) rather than letting mocks slip through (gemini):
    # an existing-but-unreadable manifest, or an OS error reading it, all hold.
    held_on_error = {"_node": node_id, "reconciled": False, "_malformed": True, "stubs": []}
    try:
        path = manifest_path(node_id, root)
        if not path.exists():
            # No manifest carried -> nothing to hold against. Reconciliation
            # retains the file (sets reconciled:true) rather than deleting it, so
            # a missing file is not "reconciled-and-cleaned"; the draft-PR flag
            # is the belt for the first-pass-not-yet-written window.
            return None
        manifest = load(path)
    except (StubManifestError, OSError):
        return held_on_error
    if manifest.get("reconciled") is True:
        return None
    manifest["_node"] = node_id
    return manifest


# --------------------------------------------------------------------------- #
# G4 reconciliation: the drift gate + de-stub finalize
# --------------------------------------------------------------------------- #
#
# The merge-triggered reconcile pass (`/target --reconcile`, dispatched by
# fno.backlog.reconcile_dispatch) calls reconcile_verdict() to decide whether it
# is safe to auto-de-stub. The gate is EXECUTABLE, not a doc diff (Locked
# Decision 5): run the blocker's contract-test suite against the now-landed
# schema. A pass authorizes de-stub; a failure OR a missing suite refuses it and
# flags for human (AC4-ERR). A missing/partial manifest also refuses (AC5-FR) --
# never finalize a half-real PR.

# Verdict outcomes (also the CLI exit-code contract; see cmd_reconcile_validate).
AUTHORIZE = "authorize"            # exit 0: contract-test passed, safe to de-stub
DRIFT = "drift"                    # exit 3: suite failed or absent -> refuse, flag
MANIFEST_MISSING = "manifest-missing"  # exit 4: no/partial manifest -> refuse
ALREADY_RECONCILED = "already-reconciled"  # exit 0: no-op, manifest already done


def _run_contract_test(cmd: str, root: Path | str, *, timeout: int = 600) -> tuple[bool, str]:
    """Run the blocker's contract-test command in the dependent's root.

    Returns ``(passed, detail)``. The command is a trusted artifact written by
    the first-pass worker (same trust boundary as a Makefile target), so it runs
    via the shell.
    # ponytail: shell=True over a manifest-authored command; the manifest is the
    # trust boundary. If untrusted manifests ever land, switch to an allowlist.
    """
    try:
        proc = subprocess.run(
            cmd, shell=True, cwd=str(root),
            capture_output=True, text=True, timeout=timeout,
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        return False, f"contract-test did not complete: {exc}"
    tail = (proc.stderr or proc.stdout or "").strip()[-300:]
    return proc.returncode == 0, f"exit {proc.returncode}: {tail}"


def reconcile_verdict(
    node_id: str, root: Path | str, *, run_suite: bool = True
) -> dict:
    """Decide whether the contract dependent NODE_ID may auto-de-stub.

    Returns ``{"outcome": <AUTHORIZE|DRIFT|MANIFEST_MISSING|ALREADY_RECONCILED>,
    "node": node_id, "detail": str, "stubs": int}``. Pure read except the
    contract-test subprocess; never mutates the manifest (finalize does that).
    """
    path = manifest_path(node_id, root)
    if not path.exists():
        return {"outcome": MANIFEST_MISSING, "node": node_id,
                "detail": f"no manifest at {path}", "stubs": 0}
    try:
        manifest = load(path)
    except StubManifestError as exc:
        # A malformed/partial manifest is a refuse, not a crash (AC5-FR): the
        # reconcile pass must surface the gap, never finalize a half-real PR.
        return {"outcome": MANIFEST_MISSING, "node": node_id,
                "detail": f"manifest unusable: {exc}", "stubs": 0}

    n_stubs = len(manifest.get("stubs", []))
    if manifest.get("reconciled") is True:
        return {"outcome": ALREADY_RECONCILED, "node": node_id,
                "detail": "manifest already reconciled", "stubs": n_stubs}

    contract_test = manifest.get("contract_test")
    if not (isinstance(contract_test, str) and contract_test.strip()):
        # Locked Decision 5: no executable gate => refuse. A missing suite is
        # treated exactly like a failing one -- never guess the schema is fine.
        return {"outcome": DRIFT, "node": node_id,
                "detail": "no contract-test suite in manifest; refusing auto-de-stub",
                "stubs": n_stubs}

    if not run_suite:
        # Caller (tests / a dry-run) wants the verdict without executing.
        return {"outcome": AUTHORIZE, "node": node_id,
                "detail": "suite present (not executed: run_suite=False)", "stubs": n_stubs}

    passed, detail = _run_contract_test(contract_test, root)
    return {"outcome": AUTHORIZE if passed else DRIFT, "node": node_id,
            "detail": f"contract-test {detail}", "stubs": n_stubs}


def mark_reconciled(node_id: str, root: Path | str) -> Path:
    """Flip the manifest's ``reconciled`` flag true, preserving every other field.

    Called by the reconcile pass AFTER de-stubbing + tests pass. The manifest is
    retained (not deleted) so "present and unreconciled" stays the single hold
    signal and the de-stub is auditable. Raises FileNotFoundError if absent,
    StubManifestError if malformed (caller must not finalize a broken manifest).
    """
    path = manifest_path(node_id, root)
    manifest = load(path)  # raises if missing/malformed -- finalize must not guess
    manifest["reconciled"] = True
    validate(manifest)
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return path


# --------------------------------------------------------------------------- #
# CLI surface (`fno stub-manifest ...`)
# --------------------------------------------------------------------------- #

stub_manifest_app = typer.Typer(
    no_args_is_help=True,
    help="Stub-manifest: emit/validate the artifact a contract dependent's "
    "first pass records, and check whether a PR is held by an unreconciled one.",
)


@stub_manifest_app.callback()
def _root() -> None:
    """Group callback. Keeps `fno stub-manifest <verb>` routing intact even at a
    single subcommand (a lone @command would collapse into the group)."""


@stub_manifest_app.command("write")
def cmd_write(
    node: str = typer.Option(..., "--node", help="dependent node id"),
    stubs_json: str = typer.Option(
        "[]", "--stubs-json", help="JSON array of {stub_id,file,symbol,contract_ref,kind}"
    ),
    contract_version: Optional[int] = typer.Option(None, "--contract-version"),
    contract_ref: Optional[str] = typer.Option(None, "--contract-ref"),
    contract_test: Optional[str] = typer.Option(
        None, "--contract-test",
        help="shell command the G4 reconcile pass runs as the executable drift "
        "gate (Locked Decision 5). Absent => reconciliation refuses to de-stub.",
    ),
    root: Path = typer.Option(Path("."), "--root", help="project root (default cwd)"),
) -> None:
    """Write `.fno/stub-manifest-<node>.json` from a stubs JSON array."""
    try:
        stubs = json.loads(stubs_json)
        if not isinstance(stubs, list):
            raise StubManifestError("--stubs-json must be a JSON array")
        path = write(
            node, stubs, root,
            contract_version=contract_version, contract_ref=contract_ref,
            contract_test=contract_test,
        )
    except (StubManifestError, json.JSONDecodeError, OSError) as exc:
        typer.echo(f"stub-manifest: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(str(path))
    # AC8: a contract dependent whose blocker merged BEFORE this first pass wrote
    # the manifest left a pending `reconcile:<node>` sentinel. Now that the
    # manifest exists, fire the reconcile dispatch exactly once. Best-effort and
    # non-fatal: a write must never fail because the (optional) re-fire stumbled.
    try:
        from fno.backlog.reconcile_dispatch import fire_pending_reconcile

        fired = fire_pending_reconcile(node, root)
        if fired and fired.decision == "dispatched":
            typer.echo(f"reconcile dispatched for pending sentinel: {node}", err=True)
    except Exception as exc:  # noqa: BLE001 - the manifest write is the contract
        typer.echo(f"stub-manifest: pending-reconcile re-fire skipped: {exc}", err=True)


@stub_manifest_app.command("validate")
def cmd_validate(
    path: Path = typer.Argument(..., help="path to a stub-manifest JSON file"),
) -> None:
    """Exit 0 if the manifest is well-formed, 1 otherwise."""
    try:
        m = load(path)
    except FileNotFoundError:
        typer.echo(f"stub-manifest: not found: {path}", err=True)
        raise typer.Exit(1)
    except StubManifestError as exc:
        typer.echo(f"stub-manifest: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(f"ok: {len(m['stubs'])} stub(s), reconciled={bool(m.get('reconciled'))}")


@stub_manifest_app.command("check-pr")
def cmd_check_pr(
    pr: int = typer.Option(..., "--pr-number", help="PR number"),
    root: Path = typer.Option(Path("."), "--root", help="project root (default cwd)"),
) -> None:
    """Exit 2 (held) if merging this PR would ship unreconciled stubs, else 0."""
    try:
        held = unreconciled_manifest_for_pr(pr, root)
    except Exception as exc:  # noqa: BLE001 - clean CLI exit over a raw traceback
        typer.echo(f"stub-manifest: check-pr failed: {exc}", err=True)
        raise typer.Exit(1)
    if held:
        typer.echo(
            json.dumps(
                {"pr": pr, "outcome": "held", "node": held.get("_node"),
                 "stubs": len(held.get("stubs", []))},
                separators=(",", ":"),
            )
        )
        raise typer.Exit(2)
    typer.echo(json.dumps({"pr": pr, "outcome": "clear"}, separators=(",", ":")))


# Verdict -> CLI exit code. authorize/already-reconciled are success (0); drift
# and manifest-missing are distinct non-zero codes so the reconcile pass can
# branch (drift -> flag for human; missing -> surface the gap). AC4-ERR/AC5-FR.
_VERDICT_EXIT = {
    AUTHORIZE: 0,
    ALREADY_RECONCILED: 0,
    DRIFT: 3,
    MANIFEST_MISSING: 4,
}


@stub_manifest_app.command("reconcile-validate")
def cmd_reconcile_validate(
    node: str = typer.Option(..., "--node", help="contract-dependent node id"),
    root: Path = typer.Option(Path("."), "--root", help="project root (default cwd)"),
    no_run: bool = typer.Option(
        False, "--no-run",
        help="report the verdict WITHOUT executing the contract-test suite "
        "(presence-only; for dry-runs).",
    ),
) -> None:
    """The G4 drift gate: may this dependent auto-de-stub?

    Exit 0 authorize (suite passed / already reconciled), 3 drift (suite failed
    or absent -> refuse + flag for human), 4 manifest-missing (refuse + surface
    the gap). Emits the verdict as one JSON line.
    """
    try:
        verdict = reconcile_verdict(node, root, run_suite=not no_run)
    except Exception as exc:  # noqa: BLE001 - clean CLI exit, fail closed to drift
        typer.echo(json.dumps(
            {"node": node, "outcome": DRIFT, "detail": f"verdict error: {exc}"},
            separators=(",", ":")))
        raise typer.Exit(3)
    typer.echo(json.dumps(verdict, separators=(",", ":")))
    raise typer.Exit(_VERDICT_EXIT.get(verdict["outcome"], 3))


@stub_manifest_app.command("reconcile-finalize")
def cmd_reconcile_finalize(
    node: str = typer.Option(..., "--node", help="contract-dependent node id"),
    root: Path = typer.Option(Path("."), "--root", help="project root (default cwd)"),
) -> None:
    """Mark the manifest reconciled (call AFTER de-stub + tests pass).

    Flips the single hold signal off so `fno pr merge` stops refusing the
    dependent's PR. Does NOT flip the draft PR ready -- that gh action is the
    reconcile skill's, kept out of this pure state write for testability.
    """
    try:
        path = mark_reconciled(node, root)
    except FileNotFoundError:
        typer.echo(f"stub-manifest: no manifest for {node} under {root}", err=True)
        raise typer.Exit(4)
    except StubManifestError as exc:
        typer.echo(f"stub-manifest: cannot finalize a broken manifest: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(json.dumps({"node": node, "reconciled": True, "path": str(path)},
                          separators=(",", ":")))
