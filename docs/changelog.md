# Changelog

> Auto-generated from git log (conventional commits). Last updated: 2026-03-31.

---

## Features

- **feat(bbb): wire buddy_react API for free Sonnet reactions** (`085a760`)
  Add native buddy_react API integration for free Sonnet-quality companion reactions with graceful Haiku fallback. Includes OAuth credential reader (macOS Keychain + file), shared config reader, three Claude Code hooks (Stop, SessionStart, UserPromptSubmit), and ambient status line renderer. Reactions are instant (local template) with async API upgrade.

- **feat: add gemini project-agent orchestration parity** (`2be8758`)
  Add orchestration support for Gemini CLI project-agent mode, bringing feature parity with Claude Code and Codex for workspaces that opt into experimental agents.

- **feat(skills): add autoresearch-inspired iteration skills** (`69e756b`)
  Introduce iteration skills based on autoresearch patterns - pre-flight checks, verify+guard, git-as-memory, and atomic operations.

- **feat(codex): add target parity runtime** (`3ba3adb`)
  Bring Codex CLI to full target parity with Claude Code, including state management, agent recovery, and hook-based loop enforcement.

- **feat(roadmap): add discovery relay and context injection** (`2e9a2a6`)
  Add discovery relay mechanism and context injection for expedition, enabling multi-session campaign awareness.

- **feat(roadmap): add expedition skill, campaign state** (`d5f26de`)
  Introduce the expedition skill with persistent campaign state for multi-session task orchestration following Citadel patterns.

- **feat(roadmap): add task lifecycle management** (`1106ddc`)
  Add roadmap-tasks.py for task lifecycle management - status tracking, dependency resolution, and roadmap mode for tasks.json.

- **feat: implement domain-agnostic target pipeline** (`29a36b8`)
  Make the target pipeline domain-agnostic with domain profiles, generic gates, and phase resolution chain for non-code workflows.

- **feat: multi-CLI hooks support for Gemini and Codex** (`6dd45b2`)
  Ship multi-CLI hooks: stop hook, session-start, and pre-compact for Claude Code, Gemini CLI, and Codex CLI.

- **feat: auto-generate HANDOFF.md** (`e5aed52`)
  Automatically generate HANDOFF.md context documents for session continuity across compactions.

- **feat: add agent-browser dependency check** (`a36b38f`)
  Add dependency check for agent-browser tool availability during setup.

---

## Bug Fixes

- **fix: support gemini in setup script** (`2aac3d3`)
  Update setup.sh to recognize and configure Gemini CLI as a valid provider.

- **fix: preserve gemini workspace upgrade detection** (`4b54510`)
  Fix workspace upgrade detection logic so Gemini sessions correctly identify prepared workspaces.

- **fix(validate-plan): detect stub-only critical path lines** (`3c45a8b`)
  Plan validation now catches critical path lines that contain only stubs without real implementation steps.

- **fix(codex): harden state and agent recovery** (`fe6cf53`)
  Harden Codex state management and agent recovery for edge cases during interrupted sessions.

- **fix: harden codex skills setup and doctor** (`1f7b9d6`)
  Fix Codex-specific setup and doctor script failures on missing directories and permissions.

- **fix(target): update remaining tasks.jsonl refs** (`3d50b6a`)
  Update stale references from tasks.jsonl to the current tasks.json format.

- **fix(target): add missing goal_verification gate** (`de37441`)
  Add the missing goal_verification quality gate that was skipped in certain pipeline paths.

- **fix(target): renumber Step 2b** (`2dd94a7`)
  Fix step numbering in target pipeline documentation.

- **fix(target): use domain-resolved phases** (`325ce59`)
  Update target to use domain-resolved phase definitions instead of hardcoded phases.

- **fix(target): update stale tasks.jsonl refs** (`605685f`)
  Additional cleanup of stale tasks.jsonl references across the codebase.

- **fix(roadmap): address 6 bugs from debug** (`8175a6c`)
  Fix six bugs identified during roadmap debugging session.

- **fix(roadmap): address Gemini review** (`437a2e8`)
  Address code review findings from Gemini review of the roadmap feature.

- **fix(roadmap): address code review findings** (`7719180`)
  Fix issues identified in code review of the roadmap pull request.

- **fix: use gate_satisfied() for artifact_shipped** (`b23a4ba`)
  Use the proper gate_satisfied() check for the artifact_shipped quality gate.

- **fix: fail-safe domain functions** (`a64a900`)
  Add fail-safe defaults to domain resolution functions for robustness.

- **fix: address Gemini review** (`2348f5a`)
  Address code review findings from Gemini on the domain-agnostic pipeline.

- **fix: address code review findings in multi-CLI stop hook** (`0904b79`)
  Fix issues identified in code review of the multi-CLI hooks implementation.

- **fix: session cost auto-registration** (`12afe80`)
  Fix automatic session cost registration so costs are tracked without manual intervention.

- **fix(target): clarify TDD Step 4** (`b3f0bcd`)
  Clarify Step 4 of TDD enforcement in target agent - verify database state, not just UI.

- **fix: archive artifacts to plan folder** (`812809a`)
  Route archived artifacts to the plan folder instead of the project root.

- **fix: use template placeholder for github_org** (`9666f19`)
  Replace hardcoded github_org with template placeholder for portability.

- **fix: improve Linear CLI install instructions** (`db945e0`)
  Improve installation instructions for the Linear CLI dependency.

- **fix: address code review for open-source readiness** (`25178e3`)
  Address code review findings to prepare the repository for open-source release.

- **fix: code review fixes for planning cost tracking** (`6bd329f`)
  Fix issues in planning cost tracking identified during code review.

---

## Refactoring

- **refactor: rename orchestration commands and executor agent** (`577f00d`)
  Rename orchestration commands and the executor agent for clarity and consistency across the codebase.

- **refactor(hooks): share gemini state detection helpers** (`00309af`)
  Extract shared helper functions for Gemini state detection to reduce duplication across hook scripts.

- **refactor(scripts): share codex skills root helpers** (`6955a3b`)
  Extract shared Codex skills root path helpers into a common module.

- **refactor: consolidate 5 doing-* agents into single target** (`caaf921`)
  Consolidate five specialized doing-* agents (frontend, backend, devops, data, general) into a single target agent with domain routing.

- **refactor: DRY up session cost tracking** (`1a7c9a9`, `dfef5e7`, `7479b64`)
  Three commits removing duplicated session cost tracking logic into shared utilities.

---

## Documentation

- **docs: document autoresearch skill integration** (`0cd0cec`)
  Document how autoresearch-inspired iteration skills integrate with the footnote pipeline.

- **docs: sync multi-cli adapter context** (`0648ea0`)
  Synchronize documentation for multi-CLI adapter configuration across providers.

- **docs: add expedition architecture doc** (`919b83b`)
  Add architecture documentation for the expedition feature, covering campaign state and discovery relay.

- **docs: architecture doc for domain-agnostic pipeline** (`63aefc7`)
  Document the architecture of the domain-agnostic target pipeline including domain profiles and phase resolution.

- **docs: domain-agnostic target design spec** (`cba2a9b`)
  Publish the accepted design specification for domain-agnostic target.

---

## Chores

- **chore: initialize target state in gemini and codex hooks** (`f3721ab`)
  Add target state initialization to Gemini and Codex hook configurations.

- **chore: prepare repo for open-source release** (`9d4619c`)
  Clean up repository for open-source release - remove internal references, add license, update paths.
