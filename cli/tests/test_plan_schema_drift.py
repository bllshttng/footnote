"""Anti-drift guards for the plan-frontmatter schema.

Mirrors ``test_config_schema_drift.py``: ``fno.plan.schema.PlanFrontmatter`` is
the single source of truth for plan frontmatter, and these tests fail CI the
moment the model drifts from its three upstream sources - ``status.py`` (the
status vocabulary the enum is derived from), ``_stamp.py`` (the ship-time
writer whose keys the model must cover), and ``skills/blueprint/references/
single-doc-spec.md`` (the design-doc template blueprint mutates every plan
from).

This is the one git-CI piece of the plan-schema feature (Locked Decision 4):
plans themselves live in the untracked vault, so the model - which lives in the
repo - is what CI can protect.
"""
from __future__ import annotations

import re
from pathlib import Path

import fno.plan._stamp as _stamp_mod
from fno.plan._status import STATUS_PROGRESSION, TERMINAL_STATUSES
from fno.plan.schema import PlanFrontmatter, PlanStatus


def test_plan_status_axis_matches_status_module() -> None:
    """PlanStatus members == STATUS_PROGRESSION plus exactly {done, superseded} (AC2-HP).

    Fails the build the moment someone edits the axis in ``status.py`` (or the
    enum here) without the other - the exact drift-killer the config schema
    already uses.
    """
    members = {m.value for m in PlanStatus}
    axis = set(STATUS_PROGRESSION)
    terminals = set(TERMINAL_STATUSES)

    assert terminals == {"done", "superseded"}, (
        f"off-axis terminals drifted: {sorted(terminals)} != ['done', 'superseded']"
    )
    assert members == axis | terminals, (
        "PlanStatus drifted from status.py; the enum must be derived from "
        "STATUS_PROGRESSION + TERMINAL_STATUSES (never hand-listed)"
    )
    # done/superseded are off the monotonic axis, never inserted into it.
    assert members - axis == terminals


def test_stamp_written_fields_are_modeled() -> None:
    """Every frontmatter key ``_stamp.py`` writes has a PlanFrontmatter field.

    Catches the drift class where the ship-time writer starts emitting a key
    the schema doesn't know about, so ``fno do plan validate`` would silently pass
    a plan carrying an unmodeled ship field.
    """
    src = Path(_stamp_mod.__file__).read_text(encoding="utf-8")
    written = set(re.findall(r'fields\["(\w+)"\]\s*=', src))

    # Guard against the regex going inert (a refactor renaming `fields`): the
    # load-bearing ship keys must always be found.
    documented = {"status", "shipped_at", "urls", "session_ids", "expected_url_count"}
    assert documented <= written, (
        f"stamp-writer regex missed documented keys: {sorted(documented - written)}"
    )

    modeled = set(PlanFrontmatter.model_fields)
    missing = written - modeled
    assert not missing, (
        f"_stamp.py writes frontmatter keys with no PlanFrontmatter field: {sorted(missing)}"
    )


def test_retired_status_spellings_still_validate() -> None:
    """AC2-FR (x-3ad5): the vault carries docs stamped under the retired
    vocabulary. They must validate at their surviving rung, or the rename
    invalidates every one of them - the gap that hid in this file, since the
    enum is derived from the axis the rename moved.
    """
    assert PlanFrontmatter(
        node="x-1", status="shipped", created="2026-07-20"
    ).status.value == "in_review"
    assert PlanFrontmatter(
        node="x-1", status="archived", created="2026-07-20"
    ).status.value == "superseded"


# -- single-doc-spec.md, the third upstream source (x-b9d7 US4) --

DESIGN_SPEC = (
    Path(__file__).resolve().parents[2]
    / "skills/blueprint/references/single-doc-spec.md"
)

QUICK_TEMPLATE = (
    Path(__file__).resolve().parents[2]
    / "skills/blueprint/references/quick-template.md"
)


def _design_template() -> str:
    """The ```markdown design-doc block from single-doc-spec.md.

    The spec carries the design-doc shape blueprint mutates; the template block
    is the one holding the frontmatter, identified by its `status: design`.
    """
    blocks = re.findall(r"```markdown\n(.*?)```", DESIGN_SPEC.read_text(encoding="utf-8"), re.S)
    contract = [b for b in blocks if "status: design" in b]
    assert len(contract) == 1, (
        f"expected exactly one design-doc block in {DESIGN_SPEC.name}, "
        f"found {len(contract)} - the extractor has gone inert or the doc grew a twin"
    )
    return contract[0]


def test_design_template_carries_every_required_plan_field() -> None:
    """AC8: a design written to blueprint's template validates with zero violations.

    Fails the build the moment PlanFrontmatter gains a required field the
    template does not show - the drift that made every authored design report
    `Field required` from `fno do plan validate`.
    """
    frontmatter = _design_template().split("---")[1]
    shown = set(re.findall(r"^([A-Za-z_][\w-]*):", frontmatter, re.M))
    required = {
        name for name, f in PlanFrontmatter.model_fields.items() if f.is_required()
    }

    # Guard against the introspection going inert: the identity triple is
    # required by construction (plan == PR == node), so an empty set means the
    # check stopped checking rather than the model getting looser.
    assert {"node", "status", "created"} <= required, (
        f"PlanFrontmatter lost a required identity field: {sorted(required)}"
    )
    assert required <= shown, (
        f"{DESIGN_SPEC.name}'s frontmatter template omits required PlanFrontmatter "
        f"field(s): {sorted(required - shown)}"
    )


def test_design_template_actually_validates() -> None:
    """AC8, the real check: the template's own VALUES must satisfy the model.

    The name-subset test above cannot see a type error - a template writing
    `claims` as a list, `status` as a retired spelling, or `created` as
    something that is not a date shows the right keys and passes it. That is
    exactly the drift that shipped (`claims: [x-9999, x-8888]` -> `Input should
    be a valid string`), so this instantiates the model from the template with
    its `<...>` placeholders filled in.
    """
    import yaml

    frontmatter = _design_template().split("---")[1]
    # Fill the angle-bracket placeholders the way the doc tells an author to.
    filled = (
        frontmatter.replace("<title>", "Probe")
        .replace("<node id; a scalar, never a list>", "x-9999")
        .replace("<node id>", "x-9999")
        .replace("<YYYY-MM-DD>", "2026-07-28")
        .replace("<low | medium | high>", "medium")
        .replace("light|standard|deep", "standard")
        .replace("[<artifacts actually read>]", "[probe.py]")
        .replace("<absorb | append | proceed_alone>", "proceed_alone")
    )
    assert "<" not in filled, (
        "the frontmatter template grew a placeholder this test does not fill, "
        f"so it is no longer validating the real shape: {filled!r}"
    )
    PlanFrontmatter(**yaml.safe_load(filled))  # raises ValidationError on drift


def test_design_template_shows_difficulty() -> None:
    """x-baef: the gate now demands ``difficulty`` on every post-gate plan, and
    the field stays Pydantic-optional so the required-subset test above cannot
    see it. Name it directly or the template can silently drop the one key the
    gate refuses to default."""
    frontmatter = _design_template().split("---")[1]
    shown = set(re.findall(r"^([A-Za-z_][\w-]*):", frontmatter, re.M))
    assert "difficulty" in shown, (
        f"{DESIGN_SPEC.name}'s frontmatter template omits `difficulty`; a plan "
        "written to it after 2026-08-26 fails validation on a key the author "
        "was never shown"
    )


def test_quick_template_shows_difficulty() -> None:
    """The same guard for quick-template.md, the /blueprint quick surface; a
    template the gate's own PR taught can lose the key just as silently."""
    blocks = re.findall(r"```markdown\n(.*?)```", QUICK_TEMPLATE.read_text(encoding="utf-8"), re.S)
    assert blocks, f"{QUICK_TEMPLATE.name} lost its markdown template block"
    frontmatter = blocks[0].split("---")[1]
    shown = set(re.findall(r"^([A-Za-z_][\w-]*):", frontmatter, re.M))
    assert "difficulty" in shown, (
        f"{QUICK_TEMPLATE.name}'s frontmatter template omits `difficulty`; a "
        "quick plan written to it after 2026-08-26 fails validation on a key "
        "the author was never shown"
    )


def test_design_template_carries_the_section_blueprint_builds_tasks_from() -> None:
    """AC9: `/blueprint` compiles `## User Stories` into the task skeleton.

    A design-doc template that omits it hands mutate_doc.py nothing to build
    tasks from.
    """
    assert "## User Stories" in _design_template(), (
        f"{DESIGN_SPEC.name}'s design-doc template omits `## User Stories`, the "
        "section mutate_doc.py builds its wave and task skeleton from"
    )
