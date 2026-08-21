"""Pre-PR stale-base guard.

A worktree branched from a stale local HEAD ships a PR full of phantom
deletions (changes you never made appear as reverts), costing a manual
rebase+repush cycle. ``fno worktree ensure`` bases new worktrees off
``origin/main``, but the EnterWorktree and manual-worktree paths bypass it, so
guard at PR-creation time where every path converges.

One implementation (:func:`check_stale_base`), two call sites: the ``/pr create``
router shells ``fno do pr base-check``; ``fno worker ship`` imports the function
directly. The bypass and the staleness rule live only here so both sites behave
identically.

Exit-code contract (callers branch on codes, not text):
    0  pass       (fresh base / behind-count 0 / bypass / fail-open)
    3  stale       (merge-base > max_hours of missing main history behind tip)
    4  unrelated    (no merge-base with the base ref)
"""
from __future__ import annotations

import datetime as dt
import json
import logging
import os
import re
import stat
import sys
from pathlib import Path
from typing import Iterator, Mapping, Optional, Tuple

from fno.pr._proc import ToolMissing, run

_LOG = logging.getLogger(__name__)

BASE_DEFAULT = "origin/main"
MAX_HOURS = 24
BYPASS_ENV = "FNO_PR_BASE_OK"
BYPASS_VALUE = "stale-acknowledged"

OK = 0
REFUSED_STALE = 3
UNRELATED = 4

# The legs a full preflight MUST have run. Membership here is the real
# assertion: a receipt missing one of these is not a full run.
_PREFLIGHT_BASE_SCOPE = frozenset(
    {
        "smoke",
        "rustfmt:fno-agents",
        "rustfmt:fno",
        "cargo-test:fno-agents",
        "cargo-test:fno",
    }
)
# Legs preflight adds when the tree calls for them. They are ALLOWED, never
# required, and that distinction is why this is a separate set.
#
# It used to be one set compared with `in {BASE, GATE}`, i.e. exact equality
# against two spellings. Every optional leg added since then silently
# invalidated the receipt it appeared in: `tracker-gates:fno` broke it the day
# it landed, and any future optional leg would break it again. The failure is
# quiet - `_trusted_preflight_producer` returns False, the receipt is discarded
# rather than rejected loudly, and a green preflight can never clear
# `check_verification_evidence` on an install with preflight.required = true.
# Adding a leg to preflight.sh must not be able to do that, so the test below
# is "every required leg present, nothing unknown added" rather than equality.
_PREFLIGHT_OPTIONAL_SCOPE = frozenset(
    {
        "squads-leak-guard:fno",
        "tracker-gates:fno",
    }
)
_PREFLIGHT_GATE_SCOPE = _PREFLIGHT_BASE_SCOPE | _PREFLIGHT_OPTIONAL_SCOPE
def _event_timestamp(value: object) -> Optional[dt.datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.astimezone(dt.timezone.utc)


def hosted_ci_decision(
    *,
    declared_none: bool,
    workflow_state: str,
    candidate_sha: str,
    observed_sha: Optional[str],
    checks: Optional[list[dict]],
) -> dict:
    """Reduce hosted-CI policy and observations without collapsing uncertainty."""
    counts = {"total": 0, "passed": 0, "failed": 0, "pending": 0}
    base = {
        "declared_none": declared_none,
        "workflow_state": workflow_state,
        "counts": counts,
    }
    if re.fullmatch(r"[0-9a-f]{40}", candidate_sha, re.IGNORECASE) is None:
        return {**base, "satisfied": False, "result": "unavailable"}
    if workflow_state not in {"present", "absent", "unavailable"}:
        return {**base, "satisfied": False, "result": "unavailable"}
    if workflow_state == "unavailable" or checks is None:
        return {**base, "satisfied": False, "result": "unavailable"}
    if checks and observed_sha is None:
        return {**base, "satisfied": False, "result": "unavailable"}
    if observed_sha is not None:
        if re.fullmatch(r"[0-9a-f]{40}", observed_sha, re.IGNORECASE) is None:
            return {**base, "satisfied": False, "result": "unavailable"}
        if observed_sha.lower() != candidate_sha.lower():
            return {**base, "satisfied": False, "result": "stale"}
    for check in checks:
        if not isinstance(check, dict):
            return {**base, "satisfied": False, "result": "unavailable"}
        counts["total"] += 1
        bucket = str(check.get("bucket") or "").lower()
        status = str(check.get("status") or "").upper()
        conclusion = str(check.get("conclusion") or check.get("state") or "").upper()
        if bucket in {"fail", "cancel"} or conclusion in {
            "FAILURE",
            "TIMED_OUT",
            "CANCELLED",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
            "STALE",
            "ERROR",
        }:
            counts["failed"] += 1
        elif (
            bucket in {"pass", "skipping"}
            or (status in {"", "COMPLETED"} and conclusion in {"SUCCESS", "NEUTRAL", "SKIPPED"})
        ):
            counts["passed"] += 1
        else:
            counts["pending"] += 1
    if counts["failed"]:
        return {**base, "satisfied": False, "result": "failed"}
    if counts["pending"]:
        return {**base, "satisfied": False, "result": "pending"}
    if counts["total"]:
        return {**base, "satisfied": True, "result": "passed"}
    if declared_none and workflow_state == "absent":
        return {**base, "satisfied": True, "result": "not_configured"}
    return {**base, "satisfied": False, "result": "pending"}


def hosted_workflow_state(cwd: Path) -> str:
    """Return present, absent, or unavailable for hosted workflow discovery."""
    workflows = cwd / ".github" / "workflows"
    try:
        workflows.stat()
    except FileNotFoundError:
        return "absent"
    except OSError:
        return "unavailable"
    try:
        if not workflows.is_dir():
            return "unavailable"
        for child in workflows.iterdir():
            if child.suffix.lower() not in {".yml", ".yaml"}:
                continue
            try:
                if stat.S_ISREG(child.stat().st_mode):
                    return "present"
            except OSError:
                return "unavailable"
        return "absent"
    except FileNotFoundError:
        return "unavailable"
    except OSError:
        return "unavailable"


def _verification_decision_all(candidate_sha: str, event_paths: list[Path]) -> dict:
    """Select the newest valid receipt across deduped, unordered journals."""
    from fno.events import ValidationError, validate

    if re.fullmatch(r"[0-9a-f]{40}", candidate_sha, re.IGNORECASE) is None:
        raise ValueError("candidate_sha must be a full 40-hex commit")
    seen_paths: set[str] = set()
    seen_events: set[str] = set()
    receipts: list[tuple[dt.datetime, str, dict]] = []
    malformed = 0
    unreadable = 0
    for raw_path in event_paths:
        path = Path(raw_path).expanduser()
        try:
            path_key = str(path.resolve())
        except OSError:
            path_key = os.path.abspath(path)
        if path_key in seen_paths:
            continue
        seen_paths.add(path_key)
        if not path.exists():
            continue
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeError):
            unreadable += 1
            continue
        for line in lines:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            if not isinstance(event, dict) or event.get("type") != "verification_receipt":
                continue
            try:
                validate(event)
            except ValidationError:
                malformed += 1
                continue
            parsed_ts = _event_timestamp(event.get("ts"))
            if parsed_ts is None or parsed_ts > dt.datetime.now(dt.timezone.utc):
                malformed += 1
                continue
            signature = json.dumps(event, sort_keys=True, separators=(",", ":"))
            if signature in seen_events:
                continue
            seen_events.add(signature)
            receipts.append((parsed_ts, signature, event))
    coverage = {
        "complete": malformed == 0 and unreadable == 0,
        "malformed_lines": malformed,
        "unreadable_paths": unreadable,
        "deduped_events": len(receipts),
    }
    receipts.sort(key=lambda item: (item[0], item[1]))
    exact = [item for item in receipts if item[2]["data"]["candidate_sha"].lower() == candidate_sha.lower()]
    if exact:
        newest_generation = max(item[2]["data"]["generation"] for item in exact)
        newest = [
            item
            for item in exact
            if item[2]["data"]["generation"] == newest_generation
        ]
        if len(newest) != 1:
            coverage["complete"] = False
            coverage["conflicting_latest"] = len(newest)
            return {
                "satisfied": False,
                "mode": None,
                "result": "unavailable",
                "receipt": None,
                "coverage": coverage,
            }
        event = newest[0][2]
        data = event["data"]
        trusted_producer = _trusted_preflight_producer(event)
        return {
            "satisfied": (
                coverage["complete"]
                and trusted_producer
                and data["mode"] == "full"
                and data["result"] == "passed"
            ),
            "mode": data["mode"],
            "result": data["result"],
            "receipt": event,
            "coverage": coverage,
        }
    if receipts:
        event = receipts[-1][2]
        return {
            "satisfied": False,
            "mode": event["data"]["mode"],
            "result": "stale",
            "receipt": event,
            "coverage": coverage,
        }
    return {
        "satisfied": False,
        "mode": None,
        "result": "unavailable",
        "receipt": None,
        "coverage": coverage,
    }


def _trusted_preflight_producer(event: dict) -> bool:
    """The one producer test every receipt must clear before it counts."""
    data = event["data"]
    command = data["command"]
    environment = data["environment"]
    producer = data["producer"]
    command_path = Path(command[0]).as_posix()
    return (
        event["source"] == "target"
        and producer["kind"] == "preflight"
        and producer["id"].startswith(f"{environment['host']}:")
        and environment["runner"] == "scripts/ci/preflight.sh"
        and (
            command_path == "scripts/ci/preflight.sh"
            or command_path.endswith("/scripts/ci/preflight.sh")
        )
        and frozenset(data["scope"]) >= _PREFLIGHT_BASE_SCOPE
        and frozenset(data["scope"]) <= _PREFLIGHT_GATE_SCOPE
        and len(data["scope"]) == len(frozenset(data["scope"]))
    )


def verification_decision(candidate_sha: str, event_paths: list[Path]) -> dict:
    """Require the first journal to originate exact-candidate authority."""
    if not event_paths:
        return _verification_decision_all(candidate_sha, event_paths)
    canonical = _verification_decision_all(candidate_sha, event_paths[:1])
    if canonical["coverage"].get("conflicting_latest"):
        return canonical
    receipt = canonical.get("receipt")
    receipt_data = receipt.get("data") if isinstance(receipt, dict) else None
    receipt_sha = (
        receipt_data.get("candidate_sha")
        if isinstance(receipt_data, dict)
        else None
    )
    canonical_generation = (
        receipt_data.get("generation")
        if isinstance(receipt_data, dict)
        else None
    )
    if not (
        isinstance(receipt_sha, str)
        and receipt_sha.lower() == candidate_sha.lower()
        and isinstance(canonical_generation, (int, float))
        and not isinstance(canonical_generation, bool)
    ):
        canonical["coverage"]["canonical_required"] = True
        canonical["satisfied"] = False
        return canonical

    readable = [event_paths[0]]
    unavailable_mirrors = 0
    for path in event_paths[1:]:
        try:
            Path(path).expanduser().read_text(encoding="utf-8")
        except FileNotFoundError:
            continue
        except (OSError, UnicodeError):
            unavailable_mirrors += 1
            continue
        readable.append(path)
    combined = _verification_decision_all(candidate_sha, readable)
    combined_receipt = combined.get("receipt")
    combined_generation = (
        combined_receipt.get("data", {}).get("generation")
        if isinstance(combined_receipt, dict)
        else None
    )
    if combined["coverage"].get("conflicting_latest") or (
        isinstance(combined_generation, (int, float))
        and not isinstance(combined_generation, bool)
        and combined_generation > canonical_generation
    ):
        combined["satisfied"] = False
        combined["mode"] = None
        combined["result"] = "unavailable"
        combined["receipt"] = None
        combined["coverage"]["mirror_ahead"] = True
        if unavailable_mirrors:
            combined["coverage"]["unavailable_mirrors"] = unavailable_mirrors
        return combined
    canonical["coverage"] = combined["coverage"]
    if not combined["coverage"]["complete"]:
        canonical["satisfied"] = False
    if unavailable_mirrors:
        canonical["coverage"]["unavailable_mirrors"] = unavailable_mirrors
    return canonical


def verification_event_paths(*, cwd: Optional[str] = None) -> tuple[list[Path], list[str]]:
    """Return event journals plus discovery errors that make coverage uncertain."""
    from fno.paths import global_events_json, ledger_json

    repo = Path(cwd or os.getcwd()).resolve()
    errors: list[str] = []
    try:
        discovered = _git(["rev-parse", "--show-toplevel"], str(repo))
        root = Path(discovered.stdout.strip()).resolve() if discovered.returncode == 0 else repo
    except (OSError, ValueError) as exc:
        root = repo
        errors.append(f"repository root discovery failed: {exc}")
    paths = [global_events_json(), root / ".fno" / "events.jsonl"]
    try:
        raw = json.loads(ledger_json().read_text(encoding="utf-8"))
        if isinstance(raw, list):
            rows = raw
        elif isinstance(raw, dict) and isinstance(raw.get("entries"), list):
            rows = raw["entries"]
        else:
            raise ValueError("ledger must be a list or an object with an entries list")
        if not all(isinstance(row, dict) for row in rows):
            raise ValueError("ledger entries must be objects")
        for row in rows:
            normalized_roots: dict[str, Path] = {}
            for field in ("root_path", "canonical_root_path"):
                value = row.get(field)
                if value is not None and (
                    not isinstance(value, str) or not value.strip()
                ):
                    raise ValueError(f"ledger {field} must be a non-empty string or null")
                if value is not None:
                    try:
                        normalized = Path(value).expanduser()
                        if not normalized.is_absolute():
                            raise ValueError("path must be absolute")
                        normalized_roots[field] = normalized.resolve(strict=False)
                    except (OSError, RuntimeError, ValueError) as exc:
                        raise ValueError(f"ledger {field} is invalid: {exc}") from exc
            delivery_root = normalized_roots.get("root_path")
            if delivery_root is not None:
                paths.append(delivery_root / ".fno" / "events.jsonl")
            canonical = normalized_roots.get("canonical_root_path")
            if canonical is not None:
                salvage = canonical / ".fno" / "salvage"
                try:
                    entries = list(os.scandir(salvage))
                except FileNotFoundError:
                    entries = []
                except OSError as exc:
                    raise ValueError(
                        f"salvage journal discovery failed for {salvage}: {exc}"
                    ) from exc
                for entry in sorted(entries, key=lambda item: item.name):
                    try:
                        if not entry.is_dir():
                            continue
                        journal = Path(entry.path) / "events.jsonl"
                        journal.stat()
                    except FileNotFoundError:
                        continue
                    except OSError as exc:
                        raise ValueError(
                            f"salvage journal discovery failed for {journal}: {exc}"
                        ) from exc
                    paths.append(journal)
    except FileNotFoundError:
        pass
    except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as exc:
        errors.append(f"delivery journal discovery failed: {exc}")
    return paths, errors


def next_verification_generation(*, cwd: str, candidate_sha: str) -> int:
    """Return one above every discovered valid exact-SHA receipt generation."""
    from fno.events import MAX_SAFE_EVENT_INTEGER, ValidationError, validate

    if re.fullmatch(r"[0-9a-f]{40}", candidate_sha, re.IGNORECASE) is None:
        raise ValueError("candidate_sha must be a full 40-hex commit")
    paths, errors = verification_event_paths(cwd=cwd)
    if errors:
        raise ValueError("; ".join(errors))
    def highest_generation(
        scan_paths: list[Path], *, skip_unreadable: bool = False
    ) -> tuple[int, bool]:
        seen_paths: set[str] = set()
        highest = 0
        found = False
        for raw_path in scan_paths:
            path = Path(raw_path).expanduser()
            try:
                key = str(path.resolve())
            except OSError:
                key = os.path.abspath(path)
            if key in seen_paths:
                continue
            seen_paths.add(key)
            try:
                lines = path.read_text(encoding="utf-8").splitlines()
            except FileNotFoundError:
                continue
            except (OSError, UnicodeError) as exc:
                if skip_unreadable:
                    continue
                raise ValueError(f"receipt journal unreadable: {path}: {exc}") from exc
            for line in lines:
                if not line.strip():
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError as exc:
                    raise ValueError(f"receipt journal malformed: {path}: {exc}") from exc
                if not isinstance(event, dict) or event.get("type") != "verification_receipt":
                    continue
                data = event.get("data")
                if not isinstance(data, dict):
                    raise ValueError(f"receipt journal malformed: {path}: data must be an object")
                raw_candidate = data.get("candidate_sha")
                if not isinstance(raw_candidate, str):
                    raise ValueError(f"receipt journal malformed: {path}: candidate SHA missing")
                if raw_candidate.lower() != candidate_sha.lower():
                    continue
                try:
                    validate(event)
                except ValidationError as exc:
                    raise ValueError(f"receipt journal malformed: {path}: {exc}") from exc
                found = True
                highest = max(highest, int(event["data"]["generation"]))
        return highest, found

    highest, found = highest_generation(paths[:1])
    if found:
        discovered_highest, _ = highest_generation(paths, skip_unreadable=True)
        if discovered_highest > highest:
            raise ValueError("mirror generation exceeds canonical authority")
    else:
        highest, _ = highest_generation(paths)
    if highest >= MAX_SAFE_EVENT_INTEGER:
        raise ValueError("receipt generation exhausted")
    return highest + 1


def check_verification_evidence(
    *,
    cwd: Optional[str] = None,
    candidate_sha: Optional[str] = None,
    event_paths: Optional[list[Path]] = None,
    allow_equivalent: bool = False,
) -> dict:
    repo = cwd or os.getcwd()
    if candidate_sha is None:
        result = _git(["rev-parse", "HEAD"], repo)
        candidate_sha = result.stdout.strip() if result.returncode == 0 else ""
    if re.fullmatch(r"[0-9a-f]{40}", candidate_sha, re.IGNORECASE) is None:
        return {
            "satisfied": False,
            "mode": None,
            "result": "unavailable",
            "receipt": None,
            "coverage": {"complete": False, "error": "candidate SHA unavailable"},
        }
    # The journal is append-only, so a read excludes nobody: writers
    # append single lines without holding any lock. Taking the preflight
    # writer lock here made every fleet verification unavailable for the
    # duration of any preflight run.
    discovery_errors: list[str] = []
    if event_paths is None:
        event_paths, discovery_errors = verification_event_paths(cwd=repo)
    decision = verification_decision(candidate_sha, event_paths)
    if discovery_errors:
        decision["coverage"]["complete"] = False
        decision["coverage"]["discovery_errors"] = discovery_errors
        decision["satisfied"] = False
    if (
        allow_equivalent
        and not decision["satisfied"]
        and not discovery_errors
        # A failed or still-running verdict for HEAD itself is
        # authoritative: the same patches can fail against a newer
        # main, so an older green for a patch-equal ancestor must
        # never override a fresh red, and an unfinished attempt
        # (result "pending") deserves the same protection.
        and decision.get("result") not in ("failed", "pending")
        # The strict path refuses on incomplete coverage; equivalence
        # must not relax that (malformed lines, unreadable journals).
        and decision["coverage"].get("complete")
    ):
        equivalent = rebase_equivalent_evidence(
            cwd=repo,
            candidate_sha=candidate_sha,
            event_paths=event_paths,
            decision=decision,
        )
        if equivalent is not None:
            decision = equivalent
    return decision


def _patch_identity(
    sha: str, *, cwd: str, base_ref: str = BASE_DEFAULT
) -> Optional[Tuple[str, ...]]:
    """Sorted patch identity of the commits ``sha`` adds over its merge-base.

    ``None`` on any failure (unresolvable ref, git error, empty id set); None
    never matches anything, so every failure mode refuses. The key is
    ``git patch-id --verbatim`` over ``merge-base..sha``: verbatim ids survive
    a clean rebase (line numbers and base-side context outside the hunks are
    normalized away) while counting whitespace, so a re-indent that changes
    Python semantics hashes differently. The default ``--stable`` strips
    whitespace and would hash a re-indent identically to the original. A git
    older than 2.45 lacks ``--verbatim`` and the call fails into ``None``,
    which refuses rather than falling back to the weaker key.
    """
    try:
        mb = _git(["merge-base", base_ref, sha], cwd)
        if mb.returncode != 0 or not mb.stdout.strip():
            return None
        log = _git(["log", "--no-merges", "-p", f"{mb.stdout.strip()}..{sha}"], cwd)
        if log.returncode != 0:
            return None
        ids = run(["git", "patch-id", "--verbatim"], cwd=cwd, input_text=log.stdout)
        if ids.returncode != 0:
            return None
    except ToolMissing:
        return None
    found: set[str] = set()
    for line in ids.stdout.splitlines():
        parts = line.split()
        if parts:
            found.add(f"patch-id:{parts[0]}")
    return tuple(sorted(found)) or None


def rebase_equivalent_evidence(
    *,
    cwd: str,
    candidate_sha: str,
    event_paths: list[Path],
    decision: dict,
    base_ref: str = BASE_DEFAULT,
) -> Optional[dict]:
    """Satisfy a refused strict decision from a receipt whose patches match.

    Runs only when the strict exact-HEAD decision is unsatisfied and the
    refusal is not itself a failed HEAD verdict (typically a rebase rewrote
    HEAD after a full green run). Walks the already-discovered receipts for a
    full/passed one that clears the trusted-producer test, whose commit still
    resolves, and whose patch identity equals HEAD's. On a match returns the
    strict decision with ``satisfied``, ``result: "equivalent"``, and the
    borrowed commit named in coverage; no match returns ``None`` and the
    refusal stands.
    """
    from fno.events import ValidationError, validate

    head_ids = _patch_identity(candidate_sha, cwd=cwd, base_ref=base_ref)
    if head_ids is None:
        return None

    def _validated_events() -> "Iterator[dict]":
        seen_paths: set[str] = set()
        for raw_path in event_paths:
            path = Path(raw_path).expanduser()
            try:
                path_key = str(path.resolve())
            except OSError:
                path_key = os.path.abspath(path)
            if path_key in seen_paths:
                continue
            seen_paths.add(path_key)
            try:
                lines = path.read_text(encoding="utf-8").splitlines()
            except (OSError, UnicodeError):
                continue
            for line in lines:
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if (
                    not isinstance(event, dict)
                    or event.get("type") != "verification_receipt"
                ):
                    continue
                try:
                    validate(event)
                except ValidationError:
                    continue
                parsed_ts = _event_timestamp(event.get("ts"))
                if parsed_ts is None or parsed_ts > dt.datetime.now(dt.timezone.utc):
                    continue
                yield event

    # First pass, everywhere: the no-rescue rule enforced inside the walk
    # itself. The aggregate decision's result can be masked (canonical-
    # required, mirror-ahead) while a failed or unfinished attempt for this
    # HEAD lives in a non-canonical journal; wherever it lives, it outranks
    # an ancestor's green, so the scan must complete before any match.
    for event in _validated_events():
        data = event.get("data")
        if not isinstance(data, dict):
            continue
        head_sha_event = data.get("candidate_sha")
        if (
            isinstance(head_sha_event, str)
            and head_sha_event.lower() == candidate_sha.lower()
            and data.get("result") in ("failed", "pending")
        ):
            return None
    for event in _validated_events():
        data = event.get("data")
        if not isinstance(data, dict):
            continue
        if data.get("mode") != "full" or data.get("result") != "passed":
            continue
        if not _trusted_preflight_producer(event):
            continue
        sha = data.get("candidate_sha")
        if not isinstance(sha, str) or sha.lower() == candidate_sha.lower():
            continue
        if _git(["cat-file", "-e", f"{sha}^{{commit}}"], cwd).returncode != 0:
            continue
        if _patch_identity(sha, cwd=cwd, base_ref=base_ref) == head_ids:
            return {
                **decision,
                "satisfied": True,
                "result": "equivalent",
                "coverage": {
                    **decision.get("coverage", {}),
                    "matched_sha": sha,
                    "equivalence": "patch-id-verbatim",
                },
            }
    return None


def _preflight_required_by_config(root: Path, env: Mapping[str, str]) -> bool:
    """Read ``config.preflight.required`` for ``root``; fail OPEN, not closed.

    The failure this policy exists to remove is a gate no one can satisfy
    stopping a green commit from reaching CI, so an unreadable or malformed
    config means not-required, never a refusal. CI still re-runs everything on
    the PR head, so nothing unverified reaches main. ``$FNO_CONFIG`` pins an
    explicit file and is read directly because ``load_settings_for_repo``
    does not consult it; it is read from ``env`` (the same injection seam the
    skip check uses), never straight from ``os.environ``.
    """
    try:
        from fno.config_io import read_config_flat

        env_path = env.get("FNO_CONFIG")
        if env_path:
            block = read_config_flat(Path(env_path)).get("preflight")
            return bool(isinstance(block, dict) and block.get("required"))
        from fno.config import load_settings_for_repo

        return bool(
            getattr(getattr(load_settings_for_repo(root), "preflight", None), "required", False)
        )
    except Exception:  # noqa: BLE001 - fail-open is the documented contract
        _LOG.warning(
            "preflight policy: config.preflight.required could not be read for %s; "
            "failing open (not required). An explicit opt-in silenced by an "
            "unrelated config error looks exactly like this.",
            root,
        )
        return False


def local_verification_required(
    *, cwd: str, base_ref: str = "origin/main", env: Optional[Mapping[str, str]] = None
) -> tuple[bool, str]:
    environment = os.environ if env is None else env
    root = Path(cwd).resolve()
    if environment.get("FNO_SKIP_PREFLIGHT") == "1":
        return False, "explicit-skip"
    # CI is the gate. A full local preflight is an opt-in rehearsal of it, so a
    # stock config never blocks a ready PR on a job that takes 22 to 47 minutes
    # and cannot even start under a 256 descriptor limit. Set
    # config.preflight.required = true to restore the mandatory rehearsal.
    if not _preflight_required_by_config(root, environment):
        return False, "policy-opt-in"
    runner = root / "scripts" / "ci" / "preflight.sh"
    base_runner = _git(
        ["ls-tree", base_ref, "--", "scripts/ci/preflight.sh"], str(root)
    )
    if base_runner.returncode != 0:
        return True, "base-policy-unavailable"
    base_configured = bool(base_runner.stdout.strip())
    candidate_configured = runner.is_file() and os.access(runner, os.X_OK)
    if base_configured and not candidate_configured:
        return True, "runner-removed"
    if not candidate_configured:
        return False, "runner-not-configured"
    changed = _git(["diff", "--name-only", f"{base_ref}...HEAD"], str(root))
    if changed.returncode != 0:
        return True, "diff-unavailable"
    non_docs = [
        path
        for path in changed.stdout.splitlines()
        if path and path != "README.md" and not path.startswith(("docs/", "internal/"))
    ]
    return (True, "required") if non_docs else (False, "docs-only")


def run_evidence_check(
    *, cwd: Optional[str] = None, allow_equivalent: bool = False
) -> int:
    decision = check_verification_evidence(cwd=cwd, allow_equivalent=allow_equivalent)
    print(json.dumps(decision, separators=(",", ":")))
    if decision.get("satisfied") and decision.get("result") == "equivalent":
        matched = (decision.get("coverage") or {}).get("matched_sha")
        print(
            f"evidence: satisfied by rebase-equivalent receipt for {matched}",
            file=sys.stderr,
        )
    return 0 if decision["satisfied"] else 1


def _git(args, cwd: str):
    return run(["git", *args], cwd=cwd)


def _committer_epoch(ref: str, cwd: str) -> Optional[int]:
    res = _git(["show", "-s", "--format=%ct", ref], cwd)
    out = res.stdout.strip()
    if res.returncode != 0 or not out:
        return None
    try:
        return int(out)
    except ValueError:
        return None


def _split_base(base: str, cwd: str) -> Tuple[str, str]:
    """``origin/main`` -> ``("origin", "main")``. A leading segment is treated as
    the remote ONLY when it matches a real git remote; otherwise the whole ref is
    fetched from ``origin``, so a slash-containing branch (``releases/v1``) is not
    mis-split into a phantom remote ``releases`` (which would fail the fetch and
    silently skip the guard)."""
    if "/" in base:
        remote, ref = base.split("/", 1)
        res = _git(["remote"], cwd)
        if res.returncode == 0:
            remotes = {ln.strip() for ln in res.stdout.splitlines() if ln.strip()}
            if remote in remotes:
                return remote, ref
    return "origin", base


def _stale_message(span_hours: float, base: str) -> str:
    days = span_hours / 24.0
    return (
        f"stale base: your branch's merge-base with {base} is {days:.1f} days "
        f"behind {base}.\n"
        "PRs from stale bases show phantom deletions (changes you never made "
        "appear as reverts).\n"
        "fix:    fno do pr rebase --base=origin/main   (handles fetch + conflicts "
        "+ repush guidance)\n"
        "bypass: FNO_PR_BASE_OK=stale-acknowledged  (deliberate old-base PR; "
        "emits gate_escape)"
    )


def _unrelated_message(base: str) -> str:
    return (
        f"unrelated histories: your branch has no common ancestor with {base}. "
        "That is a bigger problem than a stale base - check you branched from the "
        "right place before opening a PR."
    )


def _emit_bypass(cwd: Optional[str], env: Mapping[str, str], events_path=None) -> None:
    """Fire-and-forget gate_escape on a deliberate bypass. Never blocks (AC2-FR)."""
    try:
        from fno.events.gate_escape import default_dedup_key, emit_gate_escape

        emit_gate_escape(
            "stale-base",
            detail="pre-pr base-check bypass",
            dedup_key=default_dedup_key("stale-base", env),
            events_path=events_path,
            cwd=cwd,
        )
    except Exception:
        pass  # ponytail: telemetry must never block a deliberate ship (AC2-FR)


def check_stale_base(
    base: str = BASE_DEFAULT,
    max_hours: int = MAX_HOURS,
    *,
    cwd: Optional[str] = None,
    env: Optional[Mapping[str, str]] = None,
    events_path=None,
) -> Tuple[int, Optional[str]]:
    """Return ``(exit_code, message)`` without printing or exiting.

    Message is a human string for stderr (a warning on fail-open, a refusal on
    stale/unrelated) or ``None`` on a clean pass.
    """
    e = os.environ if env is None else env
    repo = cwd or os.getcwd()

    # Bypass honored here so both call sites are identical. Only the literal
    # string bypasses; any other value falls through and refuses (AC1-FR).
    if e.get(BYPASS_ENV, "") == BYPASS_VALUE:
        _emit_bypass(repo, e, events_path)
        return OK, None

    try:
        # _split_base shells `git remote`, so it shares the fail-open ToolMissing
        # guard with the fetch. Compare against the remote-tracking ref we
        # actually fetch: `git fetch` advances refs/remotes/<remote>/<ref>, never
        # a local branch, so a bare `--base main` must still read origin/main -
        # comparing local `main` would read a ref the fetch never touched and
        # pass exactly when it should refuse.
        remote, ref = _split_base(base, repo)
        compare = f"{remote}/{ref}"
        fetch = _git(["fetch", remote, ref, "--quiet"], repo)
    except ToolMissing:
        return OK, "could not run git; stale-base check skipped"
    if fetch.returncode != 0:
        # Fail-open: a network flake must not block a ship, and gh pr create is
        # about to make the real network test anyway.
        return OK, f"could not refresh {compare}; stale-base check skipped"

    mb = _git(["merge-base", "HEAD", compare], repo)
    mb_sha = mb.stdout.strip()
    if mb.returncode != 0 or not mb_sha:
        return UNRELATED, _unrelated_message(compare)

    behind = _git(["rev-list", "--count", f"{mb_sha}..{compare}"], repo)
    if behind.returncode != 0:
        # Fail-open on an unexpected rev-list error rather than misreading empty
        # stdout as behind-count 0 (a false "up to date" pass).
        return OK, "could not determine behind count; stale-base check skipped"
    if behind.stdout.strip() == "0":
        return OK, None  # up to date; merge-base age is irrelevant

    tip_ts = _committer_epoch(compare, repo)
    mb_ts = _committer_epoch(mb_sha, repo)
    if tip_ts is None or mb_ts is None:
        return OK, "could not read commit dates; stale-base check skipped"

    span_hours = (tip_ts - mb_ts) / 3600.0
    if span_hours > max_hours:
        return REFUSED_STALE, _stale_message(span_hours, compare)
    return OK, None


def run_base_check(base: str = BASE_DEFAULT, *, cwd: Optional[str] = None) -> int:
    """CLI entry: run the check, print any message to stderr, return the code."""
    code, msg = check_stale_base(base=base, cwd=cwd)
    if msg:
        sys.stderr.write(msg.rstrip("\n") + "\n")
    return code
