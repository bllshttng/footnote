"""enumerated_scope: the one-way ratchet that withdraws the cheap exit (x-cbab AC3).

Detection of multi-deliverable scope from prose is impossible - proven on three
real specimens, pinned here as controls. enumerated_scope fires ONLY on
unambiguous enumerations; a non-fire asserts nothing (it is never read as
"singular"), because x-0707's real ask is a coordinated noun phrase that no prose
predicate catches, and the structural denominator_absent predicate protects it.
"""
from __future__ import annotations

from fno.target.denominator import (
    _cardinal_governs_plural,
    _ordinal_run,
    denominator_absent,
    enumerated_scope,
    is_code_payload,
)

# Positive control: the node that DID get a plan, whose shortfall stayed
# expressible. Carries BOTH an ordinal run AND a cardinal governing a plural.
POS_TITLE = "cohort benchmarking across four bands"
POS_DETAILS = (
    "THE ASK, four controls: (1) county cohort; (2) statewide CA SNF cohort; "
    "(3) bed-size-matched peers, similar licensed bed count and comparable case "
    "mix; (4) the operator's own portfolio"
)

# Negative control: measurement digits that look enumerative but are values.
# A predicate that fires here is a false-positive machine and kills exit 2 for
# every organic node.
NEG_DETAILS = (
    "Band counts inside section 21 versus the earlier cost-math sections: "
    "County 1 vs 15, Statewide CA 9 vs 14, National 1 vs 14, "
    "Same-size peers 0 vs 14, Operator portfolio 0 vs 15."
)

# Pinned miss: x-0707's ACTUAL ask - the coordinated noun phrase. Four
# deliverables, zero numerals. No prose predicate catches it; this is the proof
# that count is a declaration, never a detection. denominator_absent protects it.
MISS_DETAILS = (
    "We need to compare across other facilities in CA. Other facilities that "
    "have similar ranges of beds, etc. maybe against the county, the state, AND "
    "similarly sized facilities, and even comparing the entire portfolio."
)


# --- positive control: fires, on each clause independently -------------------


def test_positive_control_fires():
    assert enumerated_scope(POS_TITLE, POS_DETAILS) is True


def test_positive_control_ordinal_clause_fires():
    """The ordinal run (1)..(4) fires on its own."""
    assert _ordinal_run(f"{POS_TITLE}\n{POS_DETAILS}") is True


def test_positive_control_cardinal_clause_fires():
    """The cardinal-governs-plural ('four controls') fires on its own."""
    assert _cardinal_governs_plural(f"{POS_TITLE}\n{POS_DETAILS}") is True


# --- negative control: measurement digits do NOT fire ------------------------


def test_negative_control_does_not_fire():
    """The measurement line must not read as an enumeration."""
    assert enumerated_scope("band counts", NEG_DETAILS) is False


def test_negative_control_no_ordinal_run():
    assert _ordinal_run(NEG_DETAILS) is False


def test_negative_control_no_cardinal_governs_plural():
    # Digits are 1/0/9/14/15 - none in 2-10, and 'vs'/'versus' are not plurals.
    assert _cardinal_governs_plural(NEG_DETAILS) is False


# --- pinned miss: the real ask, a known non-fire -----------------------------


def test_pinned_miss_does_not_fire():
    """The coordinated-noun-phrase ask does not fire, and that is correct. The
    structural gate (denominator_absent) is what protects this node, not this
    ratchet: pinning the miss stops a later contributor tightening the regex
    until the negative control breaks."""
    assert enumerated_scope("compare across facilities", MISS_DETAILS) is False


# --- structural predicate: denominator_absent + is_code_payload ---------------


def test_code_payload_with_no_plan_and_no_deliverables_is_absent():
    assert denominator_absent(
        plan_path="", deliverables=None, payload_is_code=True
    ) is True


def test_plan_present_is_not_absent():
    assert denominator_absent(
        plan_path="/x/plan.md", deliverables=None, payload_is_code=True
    ) is False


def test_deliverables_declared_is_not_absent():
    # The cheap N=1 exit creates the denominator; it is never a hole.
    assert denominator_absent(
        plan_path="", deliverables=1, payload_is_code=True
    ) is False


def test_non_code_payload_is_not_absent():
    # A plan-only/think run needs no deliverable denominator.
    assert denominator_absent(
        plan_path="", deliverables=None, payload_is_code=False
    ) is False


def test_is_code_payload_plan_only_is_false():
    assert is_code_payload(["think", "plan"]) is False


def test_is_code_payload_with_do_is_true():
    assert is_code_payload(["think", "plan", "do"]) is True


def test_is_code_payload_unresolved_is_true():
    # Fail closed: the common case is a build, and the cost of a false positive
    # is "name a denominator", not "ship untracked scope".
    assert is_code_payload(None) is True
    assert is_code_payload([]) is True
