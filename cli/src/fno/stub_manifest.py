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
    reconciled: bool = False,
) -> Path:
    """Validate and write the manifest, returning its path. Creates `.fno/` if
    needed."""
    data = {
        "node": node_id,
        "contract_version": contract_version,
        "contract_ref": contract_ref,
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
        )
    except (StubManifestError, json.JSONDecodeError, OSError) as exc:
        typer.echo(f"stub-manifest: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(str(path))


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
    pr: int = typer.Option(..., "--pr", help="PR number"),
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
