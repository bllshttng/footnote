# Size Profiles

Target uses t-shirt sizes to control ceremony level. No flag means Medium.

## Small (S)

Build it, PR it, done. Minimal ceremony.

| Capability | Value | Notes |
|------------|-------|-------|
| Spec mode | quick | Single-file plan for `/do` |
| Executor | do | Lightweight, no wave orchestration |
| Dynamic parallelization | off | do doesn't use it |
| Research phase | off | |
| Code review | on | Always on, non-negotiable |
| Fresh verification | off | |
| Adversarial | off | |
| Clean (de-sloppify) | off | |
| Ship (create PR) | on | Always on |
| External review | off | **Forced on when `auto-merge` is set** (see Resolution Algorithm step 5) |
| Browser testing | off | |
| Ship docs | off | |
| How-to guide | off | |

## Medium (M / default)

Build it well. Standard ceremony.

| Capability | Value | Notes |
|------------|-------|-------|
| Spec mode | full (default) | Folder plan for `/do waves` |
| Executor | operator | Wave orchestration with verification |
| Dynamic parallelization | auto | When file ownership map exists |
| Research phase | off | Operator auto-detects if invoked directly |
| Code review | on | |
| Fresh verification | on | |
| Adversarial | off | |
| Clean (de-sloppify) | off | |
| Ship (create PR) | on | |
| External review | on | |
| Browser testing | off | |
| Ship docs | on | |
| How-to guide | off | |

## Large (L)

Build it bulletproof. Full treatment.

| Capability | Value | Notes |
|------------|-------|-------|
| Spec mode | full (default) | Folder plan for `/do waves` |
| Executor | operator | With all coordinator capabilities |
| Dynamic parallelization | auto | |
| Research phase | on | Forces research even under target |
| Code review | on | |
| Fresh verification | on | |
| Adversarial | on | |
| Clean (de-sloppify) | on | |
| Ship (create PR) | on | |
| External review | on | |
| Browser testing | on | |
| Ship docs | on | |
| How-to guide | on | |

## Resolution Algorithm

1. Parse arguments for -S, -M, -L (mutually exclusive, last wins)
2. If no size flag: read `default_size` from settings.yaml, default to M
3. Load the profile table for the resolved size
4. For each individual flag in arguments:
   - If it contradicts the profile: override that specific toggle
   - Examples:
     - `-M --no-docs` -> M profile but docs = off
     - `-S --docs` -> S profile but docs = on
     - `-L --no-browser` -> L profile but browser = off
5. **Auto-merge override** (final pass): if `auto_merge_approved: true`,
   force `no_external: false` regardless of profile or explicit
   `--no-external`. `auto-merge` semantically means "merge after review";
   merging skipped-review wastes the PR. Logged to stderr so the
   override is visible. The skip-flag drift detector reads the
   post-override value as canonical, so the LLM still cannot flip it
   mid-pipeline. See [auto-merge.md](auto-merge.md).
6. Write resolved flags to target-state.md

## Config Key Mapping

Each toggle maps to existing config keys in target-state.md:

```yaml
executor: do | operator        # determines /do vs /do waves invocation
no_research: true | false      # from operator upgrade
no_verify_fresh: true | false  # from operator upgrade
adversarial: true | false      # from operator upgrade
no_clean: true | false         # existing
no_external: true | false      # existing
no_browser: true | false       # existing
no_docs: true | false          # existing
no_how_to: true | false        # existing
no_ship: true | false          # skip PR creation
```

## Resolved Flag Templates

For quick reference, the resolved flags per size:

### Small
```yaml
executor: do
no_research: true
no_verify_fresh: true
adversarial: false
no_clean: true
no_ship: false
no_external: true
no_browser: true
no_docs: true
no_how_to: true
```

### Medium
```yaml
executor: operator
no_research: true
no_verify_fresh: false
adversarial: false
no_clean: true
no_ship: false
no_external: false
no_browser: true
no_docs: false
no_how_to: true
```

### Large
```yaml
executor: operator
no_research: false
no_verify_fresh: false
adversarial: true
no_clean: false
no_ship: false
no_external: false
no_browser: false
no_docs: false
no_how_to: false
```
