"""fno.plan.schema - one authority for single-doc plan frontmatter.

Replays the proven `fno.config` pattern: a Pydantic model as the single source
of truth for what a plan's YAML frontmatter may contain, plus a drift lint
(`cli/tests/test_plan_schema_drift.py`) that fails CI the moment the model and
the real writers/readers diverge.

Validate-only. This model validates the dict `_doc.load_plan` already parsed
with PyYAML; it never re-serializes a plan. `_stamp.py`'s hand-rolled writer
stays byte-preserving (Locked Decision 1) - a YAML round-trip would reorder
keys and reformat opaque blocks like `kill_criteria`.

`PlanStatus` is derived FROM `status.STATUS_PROGRESSION` (never re-listed) so
the two definitions cannot drift; `done`/`superseded` join as off-axis sibling
terminals, matching `status`'s own split (Locked Decision 2).
"""
from __future__ import annotations

import enum
from datetime import date, datetime
from typing import Any, Literal, Self

from pydantic import BaseModel, Field, field_validator, model_validator

from fno.company.contracts import CompanyWorkRefs, validate_company_work_for_node
from fno.plan._status import STATUS_ALIASES, STATUS_PROGRESSION, TERMINAL_STATUSES

# Str-enum built directly from the status axis + terminals. Functional API so
# the members are *derived*, never hand-listed here - the drift lint asserts
# this stays set-equal to status's own vocabulary.
PlanStatus = enum.Enum(  # type: ignore[misc]
    "PlanStatus",
    {name: name for name in (*STATUS_PROGRESSION, *TERMINAL_STATUSES)},
    type=str,
)

#: The date the difficulty requirement shipped (x-baef). 1190 of the 1196
#: plans that existed on 2026-08-26 carry no ``difficulty``, and backfilling
#: would fabricate 1190 estimates nobody made; plans created on or before
#: this date pass without one. The boundary is strictly-after, not
#: on-or-after: a plan created ON the gate date predates the gate reaching
#: its author (the same boundary ``check_consolidation_file`` uses in
#: skills/blueprint/scripts/validate-plan.sh).
DIFFICULTY_REQUIRED_AFTER = date(2026, 8, 26)


def difficulty_gate_error(frontmatter: dict) -> str | None:
    """The date-keyed difficulty refusal for raw frontmatter, or None.

    ONE implementation shared by the model validator and the execution-scope
    early return in ``fno do plan validate``: validate-plan.sh runs
    ``--execution``, which never reaches ``PlanFrontmatter``, so without
    this shared helper the gate a plan hits at CREATION would drift from the
    one the model enforces later. An undatable ``created`` returns None -
    validate-plan.sh's own date fallback refuses that plan separately.
    """
    created = frontmatter.get("created")
    if isinstance(created, datetime):
        created = created.date()
    elif isinstance(created, date):
        pass  # a YAML-parsed date object is already the answer
    elif isinstance(created, str):
        try:
            created = date.fromisoformat(created.strip()[:10])
        except ValueError:
            created = None
    else:
        # int/list/None-ish scalars: an unquoted YAML int (20260827) would
        # otherwise fall through the isinstance(date) test and bypass the
        # gate everywhere (pydantic even coerces it to an epoch date in the
        # model scope, reading post-gate as pre-gate).
        created = None
    if "created" in frontmatter and created is None:
        # PRESENT but unreadable: returning None here would bypass the gate
        # in the --execution scope, which never reaches the model's own date
        # validation. An ABSENT created still returns None below -
        # validate-plan.sh's filename-date fallback owns that case.
        return (
            f"cannot read created: {frontmatter.get('created')!r}; the difficulty gate needs "
            "a date. Set created: <YYYY-MM-DD> and, for post-gate plans, "
            "difficulty: low, medium, high."
        )
    if not isinstance(created, date) or created <= DIFFICULTY_REQUIRED_AFTER:
        return None
    band = frontmatter.get("difficulty")
    if isinstance(band, str) and band.strip().lower() in {"low", "medium", "high"}:
        return None
    return (
        "difficulty is required for plans created after "
        f"{DIFFICULTY_REQUIRED_AFTER.isoformat()}: set it to one of "
        "low, medium, high (an intrinsic property of the work - the "
        "expected time, edge cases, unknowns, and what a senior tech "
        "lead makes of the problem; not a model or capacity hint)"
    )


class ConsolidationEntry(BaseModel):
    """One candidate id judged by blueprint's step 2d gate, with its reason."""

    id: str = Field(min_length=1)
    reason: str = Field(min_length=1)

    @field_validator("id")
    @classmethod
    def _id_is_a_node_id(cls, v: str) -> str:
        # Same predicate the graph mints against, imported rather than
        # re-spelled: a reason pointing at a typo'd id is a decision no later
        # reader can check, which is the one thing this block exists to give.
        # Imported HERE, not at module scope: `fno.graph` costs 126ms of this
        # module's 219ms import, and every plan read would pay it for a key
        # most plans carry once.
        from fno.graph._constants import is_wellformed_node_id

        if not is_wellformed_node_id(v):
            raise ValueError("is not a node id (expected <prefix>-<hex>, e.g. x-3bd3)")
        return v


class DecisionAcknowledgment(BaseModel):
    """One of the node's live decision-index rulings, judged non-closing."""

    decision_id: str = Field(min_length=1)
    reason: str = Field(min_length=1)

    @field_validator("decision_id")
    @classmethod
    def _decision_id_is_wellformed(cls, v: str) -> str:
        from fno.decide import looks_like_decision_id

        if not looks_like_decision_id(v):
            raise ValueError("is not a decision id (expected d-<hex>, e.g. d-a4b6e1c8)")
        # Normalize to casefold: the regex accepts d-ABCD1234 (case-insensitive,
        # matching looks_like_decision_id itself), but a real minted id
        # (mint_decision_id) is always lowercase hex. Without this, the
        # validator's own comparison against the (always-lowercase) live index
        # would refuse a plan that named the right ruling in the wrong case.
        return v.strip().casefold()


class ConsolidationBlock(BaseModel):
    """The step 2d Consolidation Gate's recorded outcome.

    Sole shape authority. `validate-plan.sh` loads the frontmatter with
    PyYAML and validates the block THROUGH this model rather than walking it
    in awk, so there is no second implementation to diverge from. The bash
    gate keeps only what is not shape: whether a block is present at all
    (grandfathering pre-gate plans is a policy date), the contradiction
    between an `append` outcome and a plan file existing to carry it, and
    whether every one of the node's LIVE decisions is named in
    `decisions_acknowledged` - that check needs the live decision index, which
    this model has no access to and does not attempt.
    """

    outcome: Literal["absorb", "append", "proceed_alone"]
    absorbed: list[ConsolidationEntry] = Field(default_factory=list)
    appended_to: list[ConsolidationEntry] = Field(default_factory=list)
    proceed_alone_against: list[ConsolidationEntry] = Field(default_factory=list)
    decisions_acknowledged: list[DecisionAcknowledgment] = Field(default_factory=list)
    # Scalar OR list: one reversing command, or the several an absorb of
    # several nodes needs. The bash gate accepts both, and a model that
    # took only a scalar produced the 'plan is unreadable or invalid'
    # divergence downstream rather than a validation error here.
    reversal: str | list[str] | None = None

    @model_validator(mode="after")
    def _outcome_has_its_decision(self) -> "ConsolidationBlock":
        # An outcome that records no decision is an empty block wearing a
        # label - the same rule the bash gate enforces on its side.
        if self.outcome == "absorb" and not self.absorbed:
            raise ValueError("outcome absorb requires at least one absorbed entry")
        if self.outcome == "append" and not self.appended_to:
            raise ValueError("outcome append requires at least one appended_to entry")
        return self


class DispatchHoldBlock(BaseModel):
    """One attributable, manually released plan-level dispatch hold."""

    reason: str = Field(min_length=1)
    release_when: str = Field(min_length=1)
    review_on: date
    set_by: str = Field(min_length=1)

    @field_validator("reason", "release_when", "set_by")
    @classmethod
    def _non_blank(cls, value: str) -> str:
        if not value.strip():
            raise ValueError("must not be blank")
        return value


class PlanFrontmatter(BaseModel):
    """The canonical shape of a single-doc plan's YAML frontmatter.

    Required core is the plan==PR==node identity: `node`, `status`, `created`.
    Everything else is optional - a bare plan is valid. `title` is optional on
    purpose: design docs carry the title as the H1, not in frontmatter (every
    plan sampled in internal/fno/{plans,design}/ confirms this).

    Unknown keys are ignored (Pydantic's default) - real plans carry 200+
    distinct historical keys, and this model deliberately does not police them.
    """

    # Canonical keys (x-f34f US7): `node`, `created`, `blocked_by`, `type` are
    # the single authority per axis. Their legacy synonyms (`graph_node_id`,
    # `created_at`, `depends_on`, `kind`) are collapsed by `fno do plan
    # migrate-keys`; readers keep a one-release fallback (e.g. reconcile's
    # _plan_link_id reads node -> claims -> graph_node_id). `deliverable_type`
    # stays distinct from `type` (different axes, both read). `claims` is an
    # observed duplicate of `node`, dropped by the migration where identical.
    node: str
    status: PlanStatus
    # datetime BEFORE date so a full timestamp keeps its time (specific-first;
    # Pydantic v2 smart-union already prefers datetime, but the order is explicit).
    created: datetime | date

    claims: str | None = None  # observed identical to `node` in every sampled plan; not asserted (Open Q1)
    title: str | None = None
    size: Literal["S", "M", "L"] | None = None
    type: str | None = None
    # Mirror fields: a PROJECTION of the graph node, written only by fno verbs
    # (intake + backlog update), never hand-edited. They give the Obsidian Bases
    # the navigation columns the graph already has (order "Next up" by priority).
    priority: str | None = None
    blocks_everything: bool = False
    difficulty: str | None = None
    blocked_by: list[str] = []
    project: str | None = None
    executor: str | None = None
    kind: str | None = None
    parent_epic: str | None = None
    source_doc: str | None = None
    # Scalar OR list-of-mappings - the frontmatter form is predominantly a list
    # (237 list vs 17 scalar in the corpus). The dead `## Kill Criteria`
    # markdown-heading form stays out of scope (Locked Decision 3).
    kill_criteria: str | list[Any] | None = None
    # Step 2d gate decision; shape-validated above. Absent on plans created
    # before the gate shipped (the bash validator warns, not errors, there).
    consolidation: ConsolidationBlock | None = None
    updated: datetime | None = None
    # compiled-v1 marks a plan whose Acceptance Criteria Blueprint compiled and
    # whose task acceptance references all resolve (x-f905). Absent on historical
    # plans, which keep permissive legacy reads.
    acceptance_contract: Literal["compiled-v1"] | None = None
    completion: Literal["delivery"] | None = None
    shipped_at: datetime | None = None  # PR creation (implementation complete)
    done_at: datetime | None = None  # PR merged (first-write-only; x-f34f)
    urls: list[str] = []
    session_ids: list[str] = []
    # >= 1 when present: graduate gates on `len(urls) >= expected`, so 0/negative
    # would graduate a plan with no URLs; the stamp/set-expected writers already
    # reject < 1, and this makes validate catch the same corrupt frontmatter.
    expected_url_count: int | None = Field(default=None, ge=1)
    # Node-closure outcome probes (x-5d34), read by the three close verbs via
    # resolve_promise_evidence. Sibling to loop-check's `done_probes` (which
    # gates session termination); this gates node closure. Scalar or list, the
    # same permissive shape as `kill_criteria`, since the Rust runner parses the
    # raw frontmatter itself.
    close_probes: str | list[Any] | None = None
    dispatch_hold: DispatchHoldBlock | None = None
    company_work: CompanyWorkRefs | None = None

    @field_validator("created", mode="before")
    @classmethod
    def _created_must_be_dateable(cls, v: Any) -> Any:
        # An unquoted YAML int (created: 20260827) coerces to an epoch date
        # under the datetime|date union, reading a post-gate plan as
        # pre-gate; refuse the type instead of guessing an epoch.
        if isinstance(v, bool) or isinstance(v, (int, float)):
            raise ValueError(
                "created must be a date (YYYY-MM-DD), not a bare number"
            )
        return v

    @field_validator("status", mode="before")
    @classmethod
    def _coerce_status_bool(cls, v: Any) -> Any:
        """Normalize a raw frontmatter status before enum validation.

        Two jobs. Unquoted `status: true` in YAML parses to Python `True`;
        without the bool coercion the enum would reject it with a confusing type
        error instead of naming the (coerced) invalid value - the same bug
        `status.py`'s coerce_status_from_yaml exists to catch, reproduced here
        rather than reused because that helper rejects `done`/`superseded`, which
        are valid for this model's superset enum. Then a retired spelling
        resolves to its survivor, so a doc stamped under the old vocabulary
        validates without the enum having to carry dead members. The alias is
        matched exactly, never case-folded: canonical values are case-sensitive
        here, so normalizing only the alias would let `Shipped` through where
        `Design` is rejected.
        """
        if isinstance(v, bool):
            return str(v).lower()
        return STATUS_ALIASES.get(v, v) if isinstance(v, str) else v

    @field_validator("difficulty", mode="before")
    @classmethod
    def _validate_difficulty(cls, v: Any) -> Any:
        if v is None:
            return v
        band = str(v).strip().lower()
        if band not in {"low", "medium", "high"}:
            raise ValueError("difficulty must be one of: low, medium, high")
        return band

    @model_validator(mode="after")
    def _require_difficulty_after_gate(self) -> Self:
        # Pydantic-optional but gate-required: the model must keep parsing
        # the 1190 pre-gate plans that carry no difficulty, so the refusal
        # lives here keyed on the plan's own created date, not on the field.
        gate_error = difficulty_gate_error(self.model_dump())
        if gate_error:
            raise ValueError(gate_error)
        return self

    @model_validator(mode="after")
    def _company_work_matches_plan_node(self) -> Self:
        validate_company_work_for_node(self.company_work, self.node, owner="plan node")
        return self
