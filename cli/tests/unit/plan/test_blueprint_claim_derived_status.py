"""A held node claim must not retire a plan's design rung.

`/target <design-rung node>` is a documented branch: skills/target/SKILL.md routes
a node whose plan reads `status: design` through `/blueprint` first. But the same
skill makes `fno do target init` mandatory, and init acquires the node claim. That
claim is projected onto the plan doc as `in_progress`
(cli/src/fno/plan/_project.py), along the forward-only axis in
cli/src/fno/plan/_status.py, so it re-advances on every run.

`in_progress` was then read as "past blueprint phase" and refused, which
deadlocked the branch: holding the claim needed to work the node is what made the
node unblueprintable, and resetting the doc to `design` did not persist.

`in_progress` is lock-derived, so it is not evidence that blueprint ran. The
`## Execution Strategy` section is. These tests pin that distinction on BOTH
reachable entry points - `mutate()` and `finalize()` - because a guard on one of
two paths would leave the other still deadlocked.
"""
from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parents[4]
_SCRIPT_PATH = _REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"


def _load_mutate_module():
    spec = importlib.util.spec_from_file_location("mutate_doc_claim_status", _SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutate_doc_claim_status"] = module
    spec.loader.exec_module(module)
    return module


_mutate_doc = _load_mutate_module()


def _doc(status: str, *, execution_strategy: bool = False) -> str:
    body = f"""---
title: example spec
status: {status}
---

# Example

## Problem

Body text.

## User Stories

**US1:** As an operator, the thing works.

## Failure Modes

**Errors**
- It can fail.

## Acceptance Criteria

### AC1-HP: The thing works

**Given** a configured operator
**When** the thing runs
**Then** it works.
"""
    if execution_strategy:
        body += """
## Execution Strategy

```yaml
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    name: Implementation
    tasks:
      - '1.1'
tasks:
  - id: '1.1'
    title: Do the thing
    surface:
      - cli/src/fno/plan/_status.py
    verify: pytest cli/tests/unit/plan -q
    acceptance:
      - AC1-HP
```
"""
    return body


def _write(tmp_path: Path, status: str, *, execution_strategy: bool = False) -> Path:
    doc = tmp_path / "plan.md"
    doc.write_text(_doc(status, execution_strategy=execution_strategy), encoding="utf-8")
    return doc


def test_mutate_accepts_claim_derived_in_progress_without_execution_strategy(tmp_path):
    """The deadlock repro: a claimed node's plan is still blueprintable."""
    doc = _write(tmp_path, "in_progress")

    rc, out = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)

    assert rc == 0, f"blueprint refused a claim-derived in_progress doc: {out}"
    assert "## Execution Strategy" in out


def test_finalize_accepts_claim_derived_in_progress(tmp_path):
    """finalize() is the second reachable path and must agree with mutate()."""
    doc = _write(tmp_path, "in_progress", execution_strategy=True)

    rc, out = _mutate_doc.finalize(doc, no_emit=True)

    assert rc == 0, f"finalize refused a claim-derived in_progress doc: {out}"


def test_finalize_refuses_in_progress_that_already_finalized(tmp_path):
    """A stamped plan is real work-in-flight, not a claim-stamped draft.

    Demoting it back to `ready` would make actively owned work dispatchable again.
    """
    doc = tmp_path / "plan.md"
    doc.write_text(
        _doc("in_progress", execution_strategy=True).replace(
            "status: in_progress",
            "status: in_progress\nacceptance_contract: compiled-v1",
        ),
        encoding="utf-8",
    )

    rc, out = _mutate_doc.finalize(doc, no_emit=True)

    assert rc == 1, f"expected refusal for an already-finalized plan, got rc={rc}"
    assert "cannot finalize" in out


def test_in_progress_with_execution_strategy_still_refused(tmp_path):
    """The artifact, not the token, is the evidence - so this one IS past blueprint."""
    doc = _write(tmp_path, "in_progress", execution_strategy=True)

    rc, out = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)

    assert rc == 1
    assert "past blueprint phase" in out


def test_pr_derived_statuses_stay_terminal(tmp_path):
    """Only the lock-derived status is relaxed; in_review/shipped are real progress."""
    for status in ("in_review", "shipped"):
        doc = _write(tmp_path, status)

        rc, out = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)

        assert rc == 1, f"{status} should stay terminal, got rc={rc}"
        assert "past blueprint phase" in out
