# Wiring existing verifiers into corrections.log

A pre-commit verifier becomes a corrections.log writer by adding one bash line to wrap an existing block. The wrapper validates and appends; the verifier still decides what to check and what severity to assign.

## The calling convention

```bash
bash ~/code/me/abilities/scripts/corrections-verifier-log.sh \
  --source <verifier-id> \
  --location "<file:line | session-id | repo>" \
  --severity <S0|S1|S2> \
  --details "<short description, <200 chars>"
```

Required flags: `--source`, `--severity`. The wrapper validates severity and refuses unknown values. Pipes in `--details` are escaped automatically.

## Pattern: pre-commit verifier that blocks on a match

Most pre-commit verifiers follow the shape "grep for a bad pattern, exit non-zero if found." Add the wrapper call before the non-zero exit:

```bash
#!/usr/bin/env bash
# Example: emdash-grep verifier
set -euo pipefail

for file in $(git diff --cached --name-only --diff-filter=ACM | grep -E '\.(md|txt)$'); do
  if grep -nP '[\x{2014}]' "$file" >/dev/null 2>&1; then
    LINE=$(grep -nP '[\x{2014}]' "$file" | head -1 | cut -d: -f1)
    echo "emdash detected in $file:$LINE" >&2
    bash ~/code/me/abilities/scripts/corrections-verifier-log.sh \
      --source emdash-grep \
      --location "$file:$LINE" \
      --severity S1 \
      --details "unicode emdash detected"
    exit 1
  fi
done
```

The verifier still owns its detection logic and exit behavior. The wrapper call records the event before the verifier blocks the commit.

## Severity guidance

The verifier chooses severity; the wrapper validates the value. Use this matrix:

| Verifier category | Severity | Examples |
|---|---|---|
| Secret / credential leak | S0 | secret-scanner, aws-key-detector, ssh-key-grep |
| Destructive command | S0 | rm-rf-detector, shell-eval-warner |
| Style / formatting / convention | S1 | emdash-grep, trailing-whitespace, missing-newline-at-eof |
| Lint / type / build break | S1 | shellcheck, mypy, eslint |
| Documentation / drift | S2 | skill-bundle-fresh, todo-without-jc-marker |

S0 verifiers should be additionally wired to fire an immediate review (see Phase 02 Task 2.4 for the inline S0 trigger). Most verifiers stay at S1 or S2.

## Known verifiers worth wiring

The user's active verifier population at time of authoring:

### Example data pipeline (example-pipeline)

- **shellcheck** (S1) - `scripts/lint/shellcheck.sh`
- **emdash-grep** (S1) - covered by the user's global formatting rule

### footnote

- **shellcheck** (S1) - `.github/workflows/ci.yml` step
- **skill-bundle-freshness** (S2) - `scripts/lint/check-skill-bundles-fresh.sh`
- **no-repo-root-scripts-in-skills** (S2) - `scripts/lint/no-repo-root-scripts-in-skills.sh`
- **dunder-import-shortcut** (S1) - flagged repeatedly by Gemini reviews

### Cross-repo (potential future)

- **secret-scanner** (S0) - any tool detecting AWS keys, GitHub tokens, etc.
- **rm-rf-detector** (S0) - any pre-commit hook flagging `rm -rf` against `$HOME` or root paths

## Adding a new verifier

1. Decide the SOURCE identifier. Lowercase, hyphens not underscores, name after what fires (e.g. `emdash-grep` not `formatting-checker`).
2. Decide the severity tier. Use the matrix above; when in doubt, S1.
3. Add the wrapper call before the non-zero exit. If the verifier may block on multiple lines per run, decide whether to call the wrapper once per match (preferred, more data) or once per file (less noisy).
4. Optionally extend the matrix in this doc so the next person doesn't have to re-derive the severity.

## What the wrapper does NOT do

- It does not call out to the LLM. The wrapper is pure bash + a `flock`-equivalent acquire. No API cost.
- It does not aggregate or deduplicate. Every call writes a line. The packet builder in Phase 02 handles deduplication during review.
- It does not bootstrap `corrections.log`. If the log doesn't exist, the wrapper exits non-zero with stderr "run corrections-log-init.sh first" rather than silently creating the file.

The asymmetry is intentional: bootstrap is one-time install action that the user explicitly runs; capture is a per-event operation that should fail fast when the loop isn't installed.
