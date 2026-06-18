# Dynamic Expertise Injection

If expertise was specified (e.g., `/do waves frontend`), inject the corresponding skill into each archer prompt:

```bash
# Determine expertise from $ARGUMENTS (first word if it matches known types)
expertise=$(echo "$ARGUMENTS" | awk '{print $1}')
case "$expertise" in
  frontend|backend|architect|fullstack|devops|qa|ml-engineer|data-engineer)
    # Load senior-{expertise} content
    expertise_content=$(cat "plugins/engineering/commands/senior-${expertise}.md" 2>/dev/null)
    ;;
  *)
    expertise=""  # Not an expertise, treat as plan path
    ;;
esac
```

When spawning archer, include in the prompt:
```markdown
## Expertise Context
{expertise_content}

## Task
{task_details_from_plan}
```

This "swizzles" the appropriate engineering expertise into each archer at runtime.
