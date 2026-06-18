# Pre-Execution Plan Validation

Before spending tokens on execution, do a quick structural validation (~2K tokens):

```bash
# Run the plan validator if it exists
if [[ -x "${CLAUDE_PLUGIN_ROOT}/scripts/validate-plan.sh" ]]; then
  bash "${CLAUDE_PLUGIN_ROOT}/scripts/validate-plan.sh" "$PLAN_DIR"
fi
```

If the validator isn't available (or the variable isn't set), do a manual structural check:

1. **Acceptance criteria coverage** - Does every AC in 00-INDEX.md have at least one task referencing it across the phase files?
2. **Task file references** - Does every task reference at least one file to modify? Tasks with no file targets are too vague to execute.
3. **Orphan tasks** - Are there tasks with no acceptance criteria at all? Flag them.
4. **Dependency sanity** - Does any task reference a file that's only created in a later phase? That's a dependency error.

**On ERROR:** STOP. Report structural issues. Do NOT proceed - the plan needs fixes first.
**On WARN:** Log warnings, proceed with caution.
**On PASS:** Proceed to execution.

This is a grep/parse check, not LLM reasoning. The goal is to catch obvious gaps before burning 50K+ tokens on execution.
