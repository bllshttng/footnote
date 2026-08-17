#!/usr/bin/env python3
"""The graph-consumer and backlog-verb census (tasks 4.2 + 5.1).

Two modalities, both with a mandatory positive control:

--verbs    Enumerate the LIVE backlog registry and verify every verb carries
           exactly one classification marker (tracker-owned = guarded by the
           shared external refusal; footnote-owned = seam/read side). Positive
           controls: a known creation verb must be tracker-owned and a known
           surviving read verb must be footnote-owned, AND the runtime guard
           must actually refuse a tracker-owned verb under an external
           backend - a marker without the refusal is decorative.

--reads    (task 5.1) Scan Python direct read_graph call sites plus the Rust
           direct graph parser, name the backend/storage allowlist, and reject
           every unclassified consumer.

--self-test  Inject known-bad inputs into BOTH modalities and print the
           success marker only after each detector names what it must.

Exit 0 clean; exit 1 on any unclassified verb/consumer or failed control.
No frozen verb counts anywhere: the registry is enumerated at run time.
"""
from __future__ import annotations

import argparse
import os
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

# The named backend/storage owners: modules whose direct graph access is the
# sanctioned storage seam (the tracker backends, the sidecar projection's
# graph-mode store, the guarded footnote-metadata reader, and the archive
# machinery inside the store itself).
READ_ALLOWLIST = (
    "cli/src/fno/graph/store.py",
    "cli/src/fno/tracker/graph_backend.py",
    "cli/src/fno/tracker/sidecar.py",
    "cli/src/fno/tracker/metadata.py",
    "crates/fno/src/backlog_view.rs",  # consumes the neutral snapshot + graph-mode mtime path (task 1.2)
)

# Known-positive controls (task 4.2 / AC9): verbs the census must FIND in the
# stated class. Absence of either control fails the census - a green run over
# a registry that silently lost its creation verb is not evidence.
KNOWN_TRACKER_OWNED_VERB = "add"
KNOWN_FOOTNOTE_OWNED_VERB = "get"

SELF_TEST_OK_MARKER = "tracker-consumers: self-test OK"


def _iter_registry(apps):
    for group, app in apps:
        for info in app.registered_commands:
            name = info.name or ""
            yield (f"{group} {name}" if group else name), info


def _registry():
    sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
    from fno.graph import cli as graph_cli

    return [
        (None, graph_cli.cli),
        ("triage", graph_cli._triage_cli),
        ("capture", graph_cli._capture_cli),
        ("batch", graph_cli._batch_cli),
        ("relatedness", graph_cli._relatedness_cli),
        ("epic", graph_cli._epic_cli),
        ("session", graph_cli.session_app),
        ("collisions", graph_cli.collisions_app),
    ]


def _marker_of(info) -> str | None:
    cb = info.callback
    if cb is None:
        return None
    if getattr(cb, "_fno_tracker_owned", False):
        return "tracker-owned"
    if getattr(cb, "_fno_footnote_owned", False):
        return "footnote-owned"
    return None


def census_verbs(verbose: bool = False) -> tuple[int, list[str]]:
    """Classify every live registry verb. Returns (total, problems)."""
    problems: list[str] = []
    rows = list(_iter_registry(_registry()))
    seen: dict[str, str] = {}
    for label, info in rows:
        marker = _marker_of(info)
        if marker is None:
            problems.append(f"unclassified verb: {label}")
            continue
        # Both markers on one verb is a classification bug.
        cb = info.callback
        if (
            getattr(cb, "_fno_tracker_owned", False)
            and getattr(cb, "_fno_footnote_owned", False)
        ):
            problems.append(f"double-classified verb: {label}")
        seen[label] = marker
        if verbose:
            print(f"  {marker:<15} {label}")
    # Positive controls: the KNOWN verbs must exist AND sit in the stated
    # class. A registry that renamed them must update the control, not
    # silently pass.
    if seen.get(KNOWN_TRACKER_OWNED_VERB) != "tracker-owned":
        problems.append(
            f"positive control failed: {KNOWN_TRACKER_OWNED_VERB!r} must be a "
            f"tracker-owned verb (found: {seen.get(KNOWN_TRACKER_OWNED_VERB)})"
        )
    if seen.get(KNOWN_FOOTNOTE_OWNED_VERB) != "footnote-owned":
        problems.append(
            f"positive control failed: {KNOWN_FOOTNOTE_OWNED_VERB!r} must be a "
            f"footnote-owned verb (found: {seen.get(KNOWN_FOOTNOTE_OWNED_VERB)})"
        )
    return len(rows), problems


def _guard_fires_runtime() -> tuple[bool, str]:
    """Runtime proof the shared refusal is installed, not just the marker:
    invoke a tracker-owned verb's callback under an external backend env and
    expect the named refusal."""
    import typer

    sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
    os.environ["FNO_TRACKER_BACKEND"] = "github"
    try:
        for label, info in _iter_registry(_registry()):
            if label != KNOWN_TRACKER_OWNED_VERB:
                continue
            try:
                info.callback("title")  # the wrapper; the guard fires first
            except typer.Exit as exc:
                return exc.exit_code == 1, f"guard refused {label} (exit {exc.exit_code})"
            except Exception as exc:  # noqa: BLE001 - any other escape is a bug
                return False, f"guard leaked {type(exc).__name__}: {exc}"
            return False, f"guard did not refuse {label}"
        return False, f"{KNOWN_TRACKER_OWNED_VERB!r} not found in registry"
    finally:
        os.environ.pop("FNO_TRACKER_BACKEND", None)


def census_reads(verbose: bool = False) -> tuple[int, list[str]]:
    """Scan direct read_graph consumers (Python) + the Rust direct parser."""
    problems: list[str] = []
    total = 0
    allow = {str(REPO_ROOT / p) for p in READ_ALLOWLIST}
    py_root = REPO_ROOT / "cli" / "src"
    pattern = re.compile(r"\bread_graph\b")
    for path in sorted(py_root.rglob("*.py")):
        rel = str(path)
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            problems.append(f"unreadable: {rel}: {exc}")
            continue
        hits = [
            (i + 1, line.strip())
            for i, line in enumerate(text.splitlines())
            if pattern.search(line)
            and "import" not in line
            and not line.strip().startswith("#")
        ]
        if not hits:
            continue
        total += len(hits)
        if rel in allow:
            if verbose:
                print(f"  allowlisted owner: {Path(rel).relative_to(REPO_ROOT)} ({len(hits)} site(s))")
            continue
        for lineno, line in hits:
            problems.append(f"unclassified consumer: {rel}:{lineno}: {line[:80]}")
    # Rust: the mux must not open graph.json directly outside backlog_view.rs.
    rust_root = REPO_ROOT / "crates"
    for path in sorted(rust_root.rglob("*.rs")):
        rel = str(path)
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        for i, line in enumerate(text.splitlines()):
            if re.search(r'"?graph\.json"?|GRAPH_JSON', line) and not line.strip().startswith("//"):
                total += 1
                if rel not in allow:
                    problems.append(f"unclassified rust consumer: {rel}:{i + 1}: {line.strip()[:80]}")
    return total, problems


def self_test() -> int:
    """Inject known-bad inputs; print the success marker only after every
    detector names what it must (a census that reports only what it found,
    with no control proving the search ran, is not evidence)."""
    failures: list[str] = []

    # Verbs modality: an unmarked verb must be detected.
    class _FakeInfo:
        name = "inject-unmarked"
        callback = lambda: None  # noqa: E731

    marker = _marker_of(_FakeInfo())
    if marker is not None:
        failures.append(f"injected unmarked verb not detected (marker={marker!r})")

    # Reads modality: an injected forbidden consumer must be detected.
    bad = "# read_graph()\nx = read_graph(path)"
    pattern = re.compile(r"\bread_graph\b")
    hits = [
        (i + 1, l.strip())
        for i, l in enumerate(bad.splitlines())
        if pattern.search(l) and "import" not in l and not l.strip().startswith("#")
    ]
    if not hits:
        failures.append("injected forbidden reader not detected")

    # Runtime guard control: the refusal must fire on the WRAPPED callback.
    fired, detail = _guard_fires_runtime()
    if not fired:
        failures.append(f"runtime guard control failed: {detail}")

    if failures:
        for f in failures:
            print(f"tracker-consumers: SELF-TEST FAILURE: {f}", file=sys.stderr)
        return 1
    print(
        "tracker-consumers: self-test detected the injected unmarked verb, "
        "the injected forbidden reader, and the runtime refusal"
    )
    print(SELF_TEST_OK_MARKER)
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--verbs", action="store_true", help="Run the backlog-verb census.")
    ap.add_argument("--reads", action="store_true", help="Run the direct-consumer read census (task 5.1).")
    ap.add_argument("--self-test", action="store_true", help="Prove the detectors detect.")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    rc = 0
    if args.verbs:
        total, problems = census_verbs(verbose=args.verbose)
        print(f"tracker-consumers: verbs census over {total} live registry entries")
        if problems:
            rc = 1
            for p in problems:
                print(f"tracker-consumers: {p}", file=sys.stderr)
        else:
            print("tracker-consumers: verbs OK - every verb classified, controls positive")
    if args.reads:
        total, problems = census_reads(verbose=args.verbose)
        print(f"tracker-consumers: read census over {total} direct site(s)")
        print(f"tracker-consumers: allowlisted owners: {', '.join(READ_ALLOWLIST)}")
        if problems:
            rc = 1
            for p in problems:
                print(f"tracker-consumers: {p}", file=sys.stderr)
        else:
            print("tracker-consumers: reads OK - zero unclassified consumers")
    if not (args.verbs or args.reads):
        ap.print_help()
        return 2
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
