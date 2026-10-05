"""The merge blocks: config.auto_merge and config.merge.

Extracted from config/__init__.py by the file-budget gate; the question
they answer is one word (merge), so they live in one module.
"""

from pydantic import BaseModel, ConfigDict, Field, field_validator


def _coerce_affirmative(v: object, default: bool) -> bool:
    """Map a settings value to a bool with the bash get_config truth table.

    The bash auto_merge helpers read each flag with ``get_config <key> <dflt>``
    and then test ``[[ "$value" == "true" ]]``. So an ABSENT key takes the
    field default, but a PRESENT non-affirmative value behaves as false. This
    helper is the ``before`` coercer half: it only runs when the key is present,
    so it returns True solely on a clear affirmative and False otherwise,
    matching ``== "true"`` for every present value. The field's own default
    (passed as ``default`` here only for documentation symmetry) covers the
    absent case via pydantic, where the validator never fires.
    """
    if isinstance(v, bool):
        return v
    if isinstance(v, int):  # bool already handled above
        return v == 1
    if isinstance(v, str):
        return v.strip().lower() in {"1", "true", "yes", "on"}
    return False


class AutoMergeBlock(BaseModel):
    """Auto-merge settings (nested under 'config.auto_merge').

    The typed reader for what the bash ``scripts/lib/config.sh`` auto_merge
    helpers used to parse (``is_auto_merge_allowed_for`` / ``get_auto_merge_*``).
    The ``fno do pr`` port reads these via :func:`load_settings`
    instead of re-parsing settings.yaml in a subprocess, so the 4-tier
    precedence + caching live in one place.

    Validation mirrors the bash exactly: an invalid ``merge_strategy`` falls
    back to ``merge``, an invalid ``conflict_resolution`` to ``opus``, an
    invalid ``remediation`` to ``attempt`` (the bash printed a warning and used
    the same fallback). A malformed block degrades to defaults (auto-merge OFF)
    rather than failing the whole settings load - false-enabled is the dangerous
    direction for a merge opt-in.

    Scope-ordered to read like the AND chain ``fno do pr merge`` enforces:
    ``enabled`` is PROJECT scope (the standing arm, re-read live at merge and
    arm time so an operator disarm mid-flight still withholds);
    ``grant`` is ACTOR scope (who may merge once ``enabled`` passes); the
    policy keys below only matter once both do.

    RUN scope is deliberately not a key here. ``fno do target init`` folds
    ``enabled`` plus ``grant`` plus ``--allow-merge`` / ``--no-merge`` plus the
    ``/target bg`` injected default into ``auto_merge_approved`` in
    ``.fno/target-state.md``, with ``auto_merge_source`` naming the decider.
    The fold withholds and it grants: a per-run refusal (``false``) outranks
    every grant at the merge gate (``pr/_merge.py``), and a
    ``TARGET_AUTO_MERGE=1`` grant (``env-target-auto-merge``) satisfies the
    standing arm on its own. Scrubbed on runs carrying a mesh
    identity (``FNO_AGENT_SELF``) or an unattended marker; an interactive
    session the operator launched carries neither and is the documented
    carrier of the grant, inside the operator's trust boundary, with the
    source stamp keeping every grant auditable.
    The review rung of the same chain is ``config.review.required_bots`` /
    ``config.review.reviewers``, enforced at the coverage guard in
    ``_merge.py``.
    """

    model_config = ConfigDict(extra="ignore")

    # MIRROR NOTE (posture readers): the Rust reader at
    # crates/fno-agents/src/agents_config.rs `auto_merge_enabled` accepts the
    # same affirmative set this coercer does. It was stricter while it read
    # only for the native arm. `fno do pr merge` now resolves its standing
    # switch through that reader too, and a split would have refused
    # `enabled = "true"` at the very verb that honored it a release ago. Any
    # change to either spelling set must move both readers and the
    # git-protection hook, or the gates split on exactly that spelling.
    enabled: bool = False
    # ACTOR scope: who may merge once `enabled` passes. Replaces
    # `dispatch.auto_merge`, which spelled the same decision in another table.
    #   none     - humans only, via `fno do pr merge`
    #   dispatch - autonomously dispatched /target workers may merge too
    grant: str = "none"
    merge_strategy: str = "merge"
    delete_branch_on_merge: bool = True
    require_checks_pass: bool = True
    require_fresh_ci: bool = True
    conflict_resolution: str = "opus"
    remediation: str = "attempt"

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        return _coerce_affirmative(v, default=False)

    @field_validator("grant", mode="before")
    @classmethod
    def _coerce_grant(cls, v: object) -> str:
        """Only the two literals are honored; anything else degrades to "none".
        Same stance as `DispatchBlock._coerce_auto_merge`: a config error can
        never grant merge rights, only withhold them. A str is trimmed first so
        a padded literal still grants - the Rust reader compares
        ``s.trim() == "dispatch"`` and the two runtimes must not disagree on
        who may merge."""
        if isinstance(v, str):
            v = v.strip()
        return v if v in ("none", "dispatch") else "none"

    @field_validator(
        "delete_branch_on_merge", "require_checks_pass", "require_fresh_ci", mode="before"
    )
    @classmethod
    def _coerce_flag(cls, v: object) -> bool:
        return _coerce_affirmative(v, default=True)

    @field_validator("merge_strategy", mode="before")
    @classmethod
    def _coerce_strategy(cls, v: object) -> str:
        if isinstance(v, str) and v.strip() in {"merge", "squash", "rebase"}:
            return v.strip()
        return "merge"

    @field_validator("conflict_resolution", mode="before")
    @classmethod
    def _coerce_conflict_resolution(cls, v: object) -> str:
        if isinstance(v, str) and v.strip() in {"opus", "fail"}:
            return v.strip()
        return "opus"

    @field_validator("remediation", mode="before")
    @classmethod
    def _coerce_remediation(cls, v: object) -> str:
        if isinstance(v, str) and v.strip() in {"attempt", "verify_only"}:
            return v.strip()
        return "attempt"


class MergeBlock(BaseModel):
    """Per-project merge-gate keys (nested under 'config.merge').

    `visual_paint_paths` lists the render-surface paths whose PRs hold for the
    user's visual approval until an answered question page names the PR. The
    Rust merge gates read it (crates/fno-agents/src/merge_gates.rs); Python
    validates and carries it. Empty (the default) disarms the gate.
    """

    model_config = ConfigDict(extra="ignore")

    visual_paint_paths: list[str] = Field(default_factory=list)
