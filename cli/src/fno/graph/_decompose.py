"""Bounded epic decomposition into group child nodes (ab-e9c81ed3, C1).

A `group child node` bundles 1+ execution waves into a single shippable PR.
This module holds the pure validation + planning logic; the IO (reading the
graph, the locked mutation, stdout) lives in the `decompose` CLI command.

Identity of a group child is (parent == epic, plan_path == base#group-<slug>),
so a re-decomposition with the same slugs upserts in place rather than
duplicating. See internal/fno/plans/2026-05-24-epic-scoped-execution.md.
"""
from __future__ import annotations

import re
from pathlib import Path
from typing import Optional, TypedDict

# Slug must be filesystem/URL-safe so `#group-<slug>` is a stable plan fragment.
GROUP_SLUG_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]*$")

# Interface-contract pin markers (G1, PR #35): a `## Interface Contract` section
# carries `**contract_version: N**` (single) or `### Contract vN` subheadings
# (multi-version). These are what a `contract`-tier dependency stubs against.
_CONTRACT_HEADING_RE = re.compile(r"^##[ \t]+Interface Contract[ \t]*$", re.M)
_CONTRACT_VERSION_RE = re.compile(r"\*\*contract_version:\s*(\d+)\s*\*\*")
_CONTRACT_SUBHEADING_RE = re.compile(r"^###[ \t]+Contract v(\d+)\b", re.M | re.I)


class NormalizedGroup(TypedDict):
    """A validated group spec. All keys are guaranteed present.

    `project`/`cwd` are None when the group inherits the epic's repo, or carry
    the per-group routing for a child that belongs to a different repo (the
    cross-project decomposition path). cwd resolution from `project` happens in
    the CLI, outside the graph lock; validation here only checks shape.

    `dep` is the dependency tier (`hard` default, `contract` opt-in). `contract`
    means the dependent stubs against a pinned interface contract instead of
    deferring until its blocker lands; the pin check (and possible fall-back to
    `hard`) happens in the CLI, which can read the epic doc. `stub_against` is an
    optional explicit contract-ref override; absent, the CLI derives it.

    `needs_think` (x-edf7 US3, default False) flags a group whose child gets a
    dispatched `/think` + `/blueprint` design pass rather than inline-fill - set
    it for a group that owns a feasibility spike, carries unresolved epic Open
    Questions, or introduces a novel subsystem. The decompose invocation is the
    operator consent for that spawn (Locked Decision 3); the RunState cap + daily
    ceiling still bound it.
    """

    slug: str
    title: str
    waves: str
    blocked_by_groups: list[str]
    project: Optional[str]
    cwd: Optional[str]
    dep: str
    stub_against: Optional[str]
    needs_think: bool


def extract_contract_versions(doc_text: str) -> set[int]:
    """Interface-contract versions pinned in a design doc, or an empty set.

    A `contract`-tier dependency is eligible only when the epic doc pins a
    `## Interface Contract` (G1). An empty set means no pin, so the dep falls
    back to `hard` (AC2-HP). Both the single-version (`**contract_version: N**`)
    and the multi-version (`### Contract vN`) layouts are read.
    """
    match = _CONTRACT_HEADING_RE.search(doc_text or "")
    if not match:
        return set()
    # Scope the version search to the section BODY (until the next level-1/2
    # heading). A stray `**contract_version: N**` or `### Contract vN` in a later
    # section (Locked Decisions, Open Questions, prose) must NOT satisfy the pin
    # gate. A level-3 `### Contract vN` does not close the section, so the
    # multi-version layout still parses.
    body = doc_text[match.end():]
    nxt = re.search(r"^##?[ \t]+", body, re.M)
    if nxt:
        body = body[: nxt.start()]
    versions = {int(m.group(1)) for m in _CONTRACT_VERSION_RE.finditer(body)}
    versions |= {int(m.group(1)) for m in _CONTRACT_SUBHEADING_RE.finditer(body)}
    return versions


def classify_group_dep(
    grp: NormalizedGroup, pinned_versions: set[int], base: str
) -> tuple[str, Optional[str], Optional[int], Optional[str]]:
    """Resolve a group's dependency tier to its persisted form.

    Returns ``(dep, stub_against, contract_version, downgrade_reason)``:
      - `hard` (the default): ``("hard", None, None, None)``.
      - `contract` against a pinned contract: ``("contract", <ref>, max(pinned),
        None)``; the dependent stubs against the newest pinned version.
      - `contract` with no pin: downgraded to ``("hard", None, None, <reason>)``
        (AC2-HP). The pin is the gate; an unpinned interface is not eligible.
    """
    if grp.get("dep") != "contract":
        return ("hard", None, None, None)
    if not pinned_versions:
        return (
            "hard",
            None,
            None,
            f"group {grp['slug']!r} requested dep=contract but the epic doc pins "
            "no ## Interface Contract; falling back to hard serialization",
        )
    version = max(pinned_versions)
    stub_against = grp.get("stub_against") or f"{base}#interface-contract"
    return ("contract", stub_against, version, None)


class DecomposeError(ValueError):
    """Raised for any invalid decomposition request.

    Carries an `exit_code` so the CLI can map error classes to the graph
    CLI's documented exit codes (1 user error, 2 bad state/cycle, 3 not found).
    """

    def __init__(self, message: str, exit_code: int = 1) -> None:
        super().__init__(message)
        self.exit_code = exit_code


def plan_base(plan_path: Optional[str]) -> str:
    """Strip any `#fragment` from an epic plan_path to get the base doc path."""
    if not plan_path:
        raise DecomposeError(
            "epic has no plan_path; cannot address group fragments", exit_code=1
        )
    return plan_path.split("#", 1)[0]


def child_plan_path(base: str, slug: str) -> str:
    return f"{base}#group-{slug}"


def separate_plan_path(base: str, slug: str) -> str:
    """Self-contained per-child plan path: `<dir>/<stem>.group-<slug>.md`.

    The `separate` packaging (the only packaging; the legacy `fragment`
    `<base>#group-<slug>` is no longer authored) points a child at its own
    quick-plan file - one plan == one PR == one node. Pure string derivation so
    the same slug maps to the same path on every run (idempotency), whether
    applied to the verbatim base (the stored plan_path) or the resolved-on-disk
    base (the file to scaffold).
    """
    p = Path(base)
    stem = p.stem if p.name.endswith(".md") else p.name
    return str(p.with_name(f"{stem}.group-{slug}.md"))


def canonical_child_plan_path(
    slug: str, child_id: str, child_root: str, created_at: Optional[str]
) -> str:
    """The canonical `fno plan path` for a group child's scaffold stub.

    Returns the `plan_doc_path` shape (`<child-project plans dir>/YYYYMMDD-<slug>-<id>.md`)
    routed into `child_root`'s plans dir. Pure recompute from durable node
    fields, so the same child maps to the same path on every run - `created_at`
    (not today) sources the date, which is what makes a re-decompose on a later
    day idempotent. An unparseable/absent `created_at` degrades to today's date
    with a stderr warning rather than crashing decompose (defensive: mint always
    writes an ISO+00:00 string, so this is a hand-corruption path only).
    """
    import sys
    from datetime import datetime

    from fno.paths import plan_doc_path

    now: Optional[datetime] = None
    if created_at:
        try:
            now = datetime.fromisoformat(created_at)
        except (TypeError, ValueError):
            now = None
    if now is None:
        print(
            f"warning: child {child_id} has unparseable created_at "
            f"{created_at!r}; using today's date for its plan filename",
            file=sys.stderr,
        )
    return str(plan_doc_path(slug, child_id, project_root=Path(child_root), now=now))


# Stub markers the validator refuses to link/ready (x-edf7 US1). An unfilled
# scaffold carries these; inline-fill (or the fan-out design pass) must replace
# every one before the child is linked. Kept next to the scaffold that emits
# them so the writer and the checker share one list.
STUB_MARKERS: tuple[str, ...] = (
    "<!-- Seeded from epic waves",
    "<!-- From the epic's File Ownership Map",
    "<!-- The checks that prove",
    "<!-- Why (from epic):",  # the empty-why sentinel (US4)
)

# The seed the scaffold leaves in `## Why (from epic)` when the epic doc yields
# no usable intent to transcribe - itself a stub marker, so the validator refuses
# a child whose why never got filled (US4 fallback).
_WHY_STUB = (
    "<!-- Why (from epic): transcribe the epic's intent line + the Locked "
    "Decisions that bind this child (not a pointer). -->"
)

# `\b.*$` (not `[ \t]*$`) so `## Overview: Goal` / `## Overview - Intent` match,
# while `## Overviews` (no word boundary) does not.
_OVERVIEW_RE = re.compile(r"^##[ \t]+Overview\b.*$", re.M)
_LOCKED_RE = re.compile(r"^##[ \t]+Locked Decisions\b.*$", re.M)


def _section_body(doc_text: str, heading_re: re.Pattern) -> str:
    """The body under a `## Heading`, bounded by the next `## `/`# ` heading."""
    m = heading_re.search(doc_text or "")
    if not m:
        return ""
    body = doc_text[m.end():]
    nxt = re.search(r"^##?[ \t]+", body, re.M)
    return (body[: nxt.start()] if nxt else body).strip()


def extract_why_digest(doc_text: str) -> tuple[str, Optional[str]]:
    """Transcribe the epic's why for a child scaffold (US4, Layer 3).

    Returns ``(digest, warning)``. The digest is the epic's intent (the first
    paragraph of ``## Overview``, else the first prose paragraph) plus its
    ``## Locked Decisions`` block verbatim - a transcription the builder narrows,
    NOT a pointer. When the doc has no ``## Locked Decisions`` the digest degrades
    to the intent line alone and ``warning`` names the gap (Boundaries: never
    fail the decompose on a why-less epic). An unreadable/empty doc yields
    ``("", None)`` so the caller seeds the ``_WHY_STUB`` sentinel instead.
    """
    # Normalize CRLF so paragraph splitting + regex anchors behave on in-memory
    # strings that never went through universal-newline translation.
    text = (doc_text or "").replace("\r\n", "\n")
    intent = _section_body(text, _OVERVIEW_RE)
    if intent:
        intent = intent.split("\n\n", 1)[0].strip()
    else:
        # No Overview heading: first non-heading, non-frontmatter prose paragraph.
        body = re.sub(r"(?s)^---\n.*?\n---\n", "", text, count=1)
        for para in re.split(r"\n\s*\n", body):
            para = para.strip()
            if para and not para.startswith("#") and not para.startswith("---"):
                intent = para
                break
    if not intent:
        return ("", None)

    locked = _section_body(text, _LOCKED_RE)
    if locked:
        return (f"{intent}\n\n**Locked Decisions (binding this child):**\n\n{locked}", None)
    return (
        intent,
        "epic doc has no ## Locked Decisions section; why-digest degraded to the "
        "intent line only - narrow it by hand during inline-fill",
    )


def scaffold_separate_plan(
    group: NormalizedGroup,
    epic_id: str,
    source_doc: str,
    why_digest: str = "",
) -> str:
    """A self-contained quick-plan stub for one group child.

    Seeded from the group's wave range, a transcribed ``## Why (from epic)``
    (US4), and stub markers for the concrete change/file/verify detail the
    builder fills inline. Born ``status: stub`` (NOT ``ready``, x-edf7 US1): the
    validator refuses to link a child still carrying any :data:`STUB_MARKERS`, so
    a fresh-context worker never dispatches against an unfilled plan. The epic doc
    remains the design authority; this child carries its own execution plan.
    """
    # Escape so a title containing a double quote can't emit invalid YAML.
    yaml_title = group["title"].replace("\\", "\\\\").replace('"', '\\"')
    why_block = why_digest.strip() or _WHY_STUB
    return (
        f'---\n'
        f'title: "{yaml_title}"\n'
        f'status: stub\n'
        f'kind: quick-plan\n'
        f'parent_epic: {epic_id}\n'
        f'source_doc: {source_doc}\n'
        f'---\n\n'
        f'# {group["title"]}\n\n'
        f'## Why (from epic)\n\n'
        f'{why_block}\n\n'
        f'## Context\n\n'
        f'Group child of epic `{epic_id}` (see `{source_doc}`). Covers wave(s) '
        f'{group["waves"] or "(unset)"} of the epic\'s Execution Strategy. This is a '
        f'self-contained quick-plan for a fresh-context builder; pull scope detail '
        f'from the named waves and the epic\'s `## File Ownership Map`.\n\n'
        f'## Changes\n\n'
        f'<!-- Seeded from epic waves {group["waves"] or "(unset)"}. Fill in the '
        f'concrete changes this group ships. -->\n\n'
        f'## Files to Modify\n\n'
        f'<!-- From the epic\'s File Ownership Map: the files this group owns. -->\n\n'
        f'## Verification\n\n'
        f'<!-- The checks that prove this group\'s slice works. -->\n'
    )


def is_shipped(node: dict) -> bool:
    """True when a group child node already has a PR / merge / completion signal.

    Re-decomposing past such a node would orphan shipped work, so the caller
    refuses unless explicitly forced (plan Errors invariant, line 84).
    """
    return bool(
        node.get("pr_number")
        or node.get("merge_status")
        or node.get("completed_at")
        or node.get("additional_prs")
    )


def group_child_slug(node: dict, base: str) -> Optional[str]:
    """The group slug of a child node, or None if it is not a group child.

    Identity is the durable ``group_slug`` field (x-edf7 US2) - present on every
    child born unlinked, so a child with no ``plan_path`` yet is still
    identifiable. Falls back to deriving the slug from a legacy ``plan_path`` (the
    ``fragment`` form ``<base>#group-<slug>`` or the ``separate`` form
    ``<dir>/<stem>.group-<slug>.md``) for children created before the field
    existed, so re-decompose stays idempotent across the migration.
    """
    gslug = node.get("group_slug")
    if isinstance(gslug, str) and gslug:
        return gslug
    pp = node.get("plan_path") or ""
    p = Path(base)
    stem = p.stem if p.name.endswith(".md") else p.name
    # Match on FILENAMES, not full dir paths, so an abs/rel mismatch between base
    # and a legacy plan_path never hides a group child (which would duplicate it
    # on re-decompose). The `<stem>.group-<slug>.md` shape is guaranteed.
    if "#group-" in pp:
        base_part, slug = pp.split("#group-", 1)
        if Path(base_part).name == p.name:
            return slug
    name = Path(pp).name
    sep_pre = f"{stem}.group-"
    if name.startswith(sep_pre) and name.endswith(".md"):
        return name[len(sep_pre):-3]
    return None


def find_orphans(
    entries: list[dict], epic_id: str, base: str, keep_slugs: set[str]
) -> list[dict]:
    """Existing group children of the epic whose slug is absent from the new spec.

    A child is a group node when it is parented to the epic and carries a
    resolvable group slug (the ``group_slug`` field, else a legacy plan_path -
    see :func:`group_child_slug`). Returns those whose slug is not in keep_slugs,
    in graph order, so a re-decomposition can surface or refuse the orphans
    regardless of packaging mode or whether the child was ever linked.
    """
    orphans: list[dict] = []
    for e in entries:
        if e.get("parent") != epic_id:
            continue
        slug = group_child_slug(e, base)
        if slug and slug not in keep_slugs:
            orphans.append(e)
    return orphans


def validate_groups(groups: object, max_prs: Optional[int]) -> list[NormalizedGroup]:
    """Validate the group spec; return the normalized list or raise DecomposeError.

    Checks (all before any graph write, so the caller stays atomic):
      - groups is a non-empty list of objects with `slug` and `title`
      - max_prs, if given, is >= 1 and >= len(groups) (the ceiling, AC1-ERR)
      - slugs are well-formed and unique
      - blocked_by_groups reference declared slugs
      - the inter-group dependency graph is acyclic
    """
    if max_prs is not None and max_prs < 1:
        raise DecomposeError(
            f"--max-prs must be >= 1 (got {max_prs}); the ceiling cannot be zero",
            exit_code=1,
        )
    if not isinstance(groups, list) or not groups:
        raise DecomposeError(
            "groups must be a non-empty JSON array of {slug, title, ...} objects",
            exit_code=1,
        )
    if max_prs is not None and len(groups) > max_prs:
        raise DecomposeError(
            f"{len(groups)} groups exceed the ceiling --max-prs {max_prs}; "
            "regroup into fewer delivery groups (N is a ceiling, not a quota)",
            exit_code=1,
        )

    normalized: list[NormalizedGroup] = []
    seen_slugs: set[str] = set()
    for i, grp in enumerate(groups):
        if not isinstance(grp, dict):
            raise DecomposeError(f"group #{i + 1} is not an object", exit_code=1)
        slug = grp.get("slug")
        title = grp.get("title")
        if not isinstance(slug, str) or not GROUP_SLUG_RE.match(slug):
            raise DecomposeError(
                f"group #{i + 1} has invalid slug {slug!r}; "
                "use [A-Za-z0-9-] starting alphanumeric",
                exit_code=1,
            )
        if slug in seen_slugs:
            raise DecomposeError(f"duplicate group slug {slug!r}", exit_code=1)
        seen_slugs.add(slug)
        if not isinstance(title, str) or not title.strip():
            raise DecomposeError(
                f"group {slug!r} is missing a non-empty title", exit_code=1
            )
        deps = grp.get("blocked_by_groups") or []
        if not isinstance(deps, list) or not all(isinstance(d, str) for d in deps):
            raise DecomposeError(
                f"group {slug!r} blocked_by_groups must be a list of slugs",
                exit_code=1,
            )
        # Optional per-group repo routing. When present each must be a non-empty
        # string; absent -> None (inherit the epic's project/cwd). cwd is NOT
        # resolved here (that needs a settings read, done in the CLI outside the
        # graph lock); validation only checks shape so a bad spec fails before
        # any graph write (atomicity).
        project = grp.get("project")
        if project is not None and (not isinstance(project, str) or not project.strip()):
            raise DecomposeError(
                f"group {slug!r} project must be a non-empty string when set",
                exit_code=1,
            )
        cwd = grp.get("cwd")
        if cwd is not None and (not isinstance(cwd, str) or not cwd.strip()):
            raise DecomposeError(
                f"group {slug!r} cwd must be a non-empty string when set",
                exit_code=1,
            )
        # Optional dependency tier. Default `hard` (defer until the blocker
        # lands, then dispatch fresh on merge, the existing behavior); `contract`
        # opts the dependent into stubbing against a pinned interface contract.
        # Whether a `contract` request actually sticks (vs falling back to
        # `hard`) depends on the epic doc's pin, checked in the CLI; here we only
        # validate shape so a typo'd tier fails before any graph write.
        dep_tier = grp.get("dep", "hard")
        if dep_tier not in ("hard", "contract"):
            raise DecomposeError(
                f"group {slug!r} dep must be 'hard' or 'contract' (got {dep_tier!r})",
                exit_code=1,
            )
        stub_against = grp.get("stub_against")
        if stub_against is not None and (
            not isinstance(stub_against, str) or not stub_against.strip()
        ):
            raise DecomposeError(
                f"group {slug!r} stub_against must be a non-empty string when set",
                exit_code=1,
            )
        # Optional design-pass flag (x-edf7 US3). Default False (inline-fill).
        needs_think = grp.get("needs_think", False)
        if not isinstance(needs_think, bool):
            raise DecomposeError(
                f"group {slug!r} needs_think must be a boolean (got {needs_think!r})",
                exit_code=1,
            )
        normalized.append(
            {
                "slug": slug,
                "title": title.strip(),
                "waves": str(grp.get("waves", "")).strip(),
                "blocked_by_groups": list(deps),
                "project": project.strip() if isinstance(project, str) else None,
                "cwd": cwd.strip() if isinstance(cwd, str) else None,
                "dep": dep_tier,
                "stub_against": (
                    stub_against.strip() if isinstance(stub_against, str) else None
                ),
                "needs_think": needs_think,
            }
        )

    # All referenced slugs must be declared in this decomposition.
    for grp in normalized:
        for dep in grp["blocked_by_groups"]:
            if dep not in seen_slugs:
                raise DecomposeError(
                    f"group {grp['slug']!r} depends on undeclared slug {dep!r}",
                    exit_code=1,
                )

    # A `contract` dependent must name the blocker it stubs against; stubbing
    # against an interface with no blocker to reconcile back to is meaningless.
    for grp in normalized:
        if grp["dep"] == "contract" and not grp["blocked_by_groups"]:
            raise DecomposeError(
                f"group {grp['slug']!r} is dep=contract but has no "
                "blocked_by_groups; a contract dependency must name its blocker",
                exit_code=1,
            )

    cycle = _first_cycle(normalized)
    if cycle:
        raise DecomposeError(
            "inter-group dependency cycle: " + " -> ".join(cycle),
            exit_code=2,
        )

    return normalized


def _first_cycle(groups: list[NormalizedGroup]) -> Optional[list[str]]:
    """Return a cycle path among groups (by slug) if one exists, else None.

    Precondition: every slug in `blocked_by_groups` is declared in `groups`
    (enforced by the reference-integrity check in `validate_groups`); calling
    this standalone with an undeclared dependency raises KeyError.
    """
    adj = {g["slug"]: list(g["blocked_by_groups"]) for g in groups}
    WHITE, GRAY, BLACK = 0, 1, 2
    color = {s: WHITE for s in adj}
    stack: list[str] = []

    def visit(node: str) -> Optional[list[str]]:
        color[node] = GRAY
        stack.append(node)
        for nxt in adj[node]:
            if color[nxt] == GRAY:
                idx = stack.index(nxt)
                return stack[idx:] + [nxt]
            if color[nxt] == WHITE:
                found = visit(nxt)
                if found:
                    return found
        stack.pop()
        color[node] = BLACK
        return None

    for s in adj:
        if color[s] == WHITE:
            found = visit(s)
            if found:
                return found
    return None
