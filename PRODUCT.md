<!-- style-exception: both sections are verbatim copies of the PDR text the plan requires; never rewrite the operator's shipped wording -->
# PRODUCT

## Product Purpose

AI coding assistants today are copilots. They suggest code, answer questions, and generate snippets - but they do not ship features. The gap between "here is some code" and "here is a working, tested, reviewed PR" remains a manual process that requires a developer to:

1. Decompose the feature into tasks
2. Write tests before or alongside implementation
3. Execute tasks in the right order (respecting dependencies)
4. Run quality checks and code review
5. Create a pull request with proper description
6. Address review feedback
7. Actually merge and ship

For solo founders and small teams, this gap is the bottleneck. They do not need another autocomplete tool - they need an autonomous software engineer that takes a feature from idea to shipped PR while they focus on product decisions.

Source: docs/project-overview-pdr.md:13-21

## Users

### Primary: Solo Founders

Solo founders building products cannot afford to context-switch between product decisions and implementation details. footnote lets them describe what they want built, review the plan, and let the autonomous pipeline handle execution through shipping.

### Secondary: Small Teams (2-5 developers)

Small teams where every developer is stretched across multiple concerns. The plugin handles the mechanical work - test writing, code review, PR creation - while humans focus on architecture decisions and product direction.

## Alternatives

A solo founder without footnote drives a coding agent by hand: every session is re-briefed, no backlog joins one session's work to the next, and nothing carries a task to a green, reviewed PR unattended. A tracker plus a coding agent keeps the plan somewhere, but nothing joins the ticket to the session that executes it or the gate that ships it. A hosted autonomous agent runs unattended, but it owns neither the plan document nor the merge gate, so the review that decides what ships happens outside the operator's repo.

### Anti-Targets

- Large enterprise teams with established CI/CD and dedicated QA - they have humans for this
- Developers who want fine-grained control over every line - target makes autonomous decisions
- Teams that do not use git-based workflows - the plugin assumes git + PR-based delivery

Source: docs/project-overview-pdr.md:145-159
