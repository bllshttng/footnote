# Abilities Browser Testing

Manual browser testing for visual verification, UX flow testing, and multi-device testing.

## Startup: Load Testing Context (MANDATORY)

Before any browser testing, check for project-specific testing context:

```bash
# Check for settings.yaml
WORKSPACE_CONFIG=""
if [[ -f ".fno/settings.yaml" ]]; then
  WORKSPACE_CONFIG=".fno/settings.yaml"  # Project override (rare)
elif [[ -f "$HOME/.fno/settings.yaml" ]]; then
  WORKSPACE_CONFIG="$HOME/.fno/settings.yaml"  # Primary location
fi
```

**If settings.yaml exists, extract testing context:**
- `testing.{project}.auth` - Available login methods (dev-login, OTP, etc.)
- `testing.{project}.auth.guardian_otp.test_numbers` - Phone numbers for OTP testing
- `testing.{project}.auth.guardian_otp.otp_retrieval` - How to get OTP codes
- `testing.{project}.gotchas` - Project-specific testing reminders

**Display loaded context:**
```
Testing Context Loaded:
  - Dev login available: localhost:3000/dev-login (owner/admin)
  - Guardian OTP: Use test number +15551234567
  - OTP retrieval: scripts/get-otp.sh {phone}
  - Gotchas:
    - QR code scanning requires camera - mock in headless tests
    - Attendance timestamps are UTC, displayed in facility timezone
```

## When to Use

- Visual verification of UI changes
- Testing user journeys that require observation
- Authenticated features (login, admin panels)
- Multi-device/responsive testing
- Debugging UI state issues

## Options

### Option A: Claude Code + Chrome Extension

**Best for:** Visual testing, authenticated features, real-time observation

**Setup:**
```bash
claude --chrome
```

**How it works:**
- Claude controls your real Chrome browser
- Shares your login state (test authenticated features)
- You see actions in real-time
- Can record GIFs of interactions

**Example prompts:**
```
Open localhost:3000/app/attendance and verify the sign-in flow:
1. Click the Phone tab
2. Enter a test phone number
3. Verify the OTP input appears
4. Check error handling with invalid OTP
```

```
Navigate to the dashboard, resize to mobile viewport (375x667),
and verify all cards render correctly without horizontal scroll.
```

```
Record a GIF showing the complete checkout flow from cart to confirmation.
```

**When to use:**
- Testing features that require your login
- Visual verification you want to observe
- Creating demo recordings
- Debugging complex state issues

---

### Option B: agent-browser CLI

**Best for:** Fast iteration, automated scripts, CI/CD

**Dependency check:**
```bash
if ! command -v agent-browser &>/dev/null; then
  echo "agent-browser not installed."
  echo ""
  echo "Install options:"
  echo "  npm install -g agent-browser"
  echo "  npx skills add https://github.com/anthropics/agent-browser --skill agent-browser"
  echo ""
  echo "Then run: agent-browser install  # Download Chromium"
  # STOP - do not proceed with agent-browser commands until installed
fi
```

**Setup:**
```bash
npm install -g agent-browser
agent-browser install  # Download Chromium
```

**Core workflow:**
```bash
# 1. Navigate
agent-browser open http://localhost:3000/app/feature

# 2. Snapshot - get elements with refs
agent-browser snapshot -i
# Output shows: button "Submit" [ref=e1], textbox "Email" [ref=e2]

# 3. Interact using refs
agent-browser click @e1
agent-browser fill @e2 "test@example.com"

# 4. Re-snapshot after page changes
agent-browser snapshot -i
```

**Common commands:**
```bash
# Navigation
agent-browser open <url>
agent-browser back
agent-browser reload

# Interaction (use @refs from snapshot)
agent-browser click @e1
agent-browser fill @e2 "text"
agent-browser press Enter
agent-browser hover @e1

# Get info
agent-browser get text @e1
agent-browser get url
agent-browser screenshot page.png

# Wait
agent-browser wait @e1           # Wait for element
agent-browser wait --text "Success"
agent-browser wait --load networkidle
```

**Multi-device testing:**
```bash
# Mobile
agent-browser set device "iPhone 14"
agent-browser open http://localhost:3000/app/feature
agent-browser screenshot mobile.png

# Tablet
agent-browser set device "iPad"
agent-browser open http://localhost:3000/app/feature
agent-browser screenshot tablet.png

# Desktop
agent-browser set viewport 1920 1080
agent-browser open http://localhost:3000/app/feature
agent-browser screenshot desktop.png
```

**Debug mode (show browser):**
```bash
agent-browser --headed open http://localhost:3000
```

**When to use:**
- Fast iteration (headless = faster)
- Automated testing scripts
- CI/CD pipelines
- When you don't need to watch

---

### Option C: Sizzy

**Best for:** Side-by-side device comparison

```bash
open -a Sizzy "http://localhost:3000/app/feature"
```

**Features:**
- Multiple devices side-by-side
- Synchronized scrolling/clicking
- Device-specific debugging
- Visual comparison

---

## Comparison

| Feature | Chrome Extension | agent-browser | Sizzy |
|---------|------------------|---------------|-------|
| Speed | Slower (visual) | Fast (headless) | Visual only |
| Auth state | Uses your logins | Separate | Uses your logins |
| Scriptable | Via prompts | CLI commands | Manual |
| Multi-device | Viewport resize | Device emulation | Side-by-side |
| Recording | GIF support | Video support | No |
| CI/CD ready | No | Yes | No |

## Testing Checklist

### Happy Path
- [ ] Complete main user journey
- [ ] Verify success feedback
- [ ] Check data persists after refresh

### Error States
- [ ] Submit with invalid data
- [ ] Verify error messages
- [ ] Check form state preserved

### UI State
- [ ] UI updates after mutations (no refresh needed)
- [ ] Loading states appear
- [ ] Success/error toasts show

### Multi-Device
- [ ] Mobile (375px) - touch targets, no horizontal scroll
- [ ] Tablet (768px) - layout adapts
- [ ] Desktop (1920px) - good density

## Example: Test Sign-In Flow

**Using agent-browser:**
```bash
# Navigate to sign-in
agent-browser open http://localhost:3000/app/attendance/sign-in

# Get interactive elements
agent-browser snapshot -i

# Enter phone number
agent-browser fill @e2 "3101234567"
agent-browser click @e3  # Submit

# Wait for OTP screen
agent-browser wait --text "Enter code"
agent-browser snapshot -i

# Enter OTP
agent-browser fill @e1 "123456"
agent-browser click @e2

# Verify success
agent-browser wait --text "Signed in"
agent-browser screenshot success.png
```

**Using Claude + Chrome:**
```
Open localhost:3000/app/attendance/sign-in:
1. Enter phone number 310-123-4567
2. Click Send Code
3. Verify OTP input appears
4. Enter 123456 as OTP
5. Verify sign-in succeeds
6. Take a screenshot of the success state
```

## Key Principles

- **Test like a user** - Click through flows naturally
- **Verify visual state** - Screenshots catch UI bugs
- **Multi-device required** - Most users are on mobile
- **Error paths matter** - Test invalid inputs
- **State changes** - UI should update without refresh

## Post-completion: Write gate artifact

After all browser tests pass (green), drop a session-scoped artifact so target's stop hook can confirm the browser phase actually happened. If target-state.md or `session_id` isn't present (browser testing invoked outside a target session), skip silently. On any red test, do NOT write the artifact — the gate should fail so target re-runs after the UI is fixed.

```bash
STATE_DIR="$(git rev-parse --show-toplevel 2>/dev/null || pwd)/.fno"
SESSION_ID=$(grep -E "^[[:space:]]*session_id:" "$STATE_DIR/target-state.md" 2>/dev/null \
    | tail -1 | sed -e 's/^[[:space:]]*session_id:[[:space:]]*//' -e 's/[[:space:]]*$//')
ARTIFACT="$STATE_DIR/artifacts/browser-${SESSION_ID}.md"

# Distinguish out-of-session (silent no-op) from malformed state (WARN).
if [[ -f "$STATE_DIR/target-state.md" && -z "$SESSION_ID" ]]; then
    echo "target: WARN: target-state.md present but session_id missing - browser gate artifact not written" >&2
fi

# Wipe stale artifact: a previous iteration's green run must not satisfy
# the gate for the current iteration's (potentially different) UI state.
[[ -n "$SESSION_ID" ]] && rm -f "$ARTIFACT"

if [[ -n "$SESSION_ID" && "${TESTS_FAILED:-0}" -eq 0 && "${TESTS_RUN:-0}" -gt 0 ]]; then
    mkdir -p "$STATE_DIR/artifacts"
    TMP="$(mktemp "${ARTIFACT}.tmp.XXXXXX")" || TMP=""
    if [[ -n "$TMP" ]]; then
        cat > "$TMP" <<EOF
---
phase: browser
session_id: ${SESSION_ID}
skill: fno:tdd (browser-testing)
completed_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
tests_run: ${TESTS_RUN}
tests_passed: ${TESTS_PASSED:-$TESTS_RUN}
tool: ${BROWSER_TOOL:-unknown}
---
# Browser-Testing Artifact
EOF
        mv "$TMP" "$ARTIFACT" || { rm -f "$TMP"; echo "target: WARN: could not finalize $ARTIFACT" >&2; }
    fi

    # Factor-3 provenance emit (see skills/target/references/gate-artifacts.md).
    bash "${CLAUDE_PLUGIN_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}/scripts/lib/emit-gate-transition.sh" \
        browser_testing_passed browser \
        "tests_run=${TESTS_RUN}" \
        "tests_passed=${TESTS_PASSED:-$TESTS_RUN}" \
        "tool=${BROWSER_TOOL:-unknown}" 2>/dev/null || true
fi
```

`BROWSER_TOOL` should identify the actual harness (chrome-devtools-mcp, agent-browser, playwright, etc.) so the artifact remains useful for forensics when multiple tooling options are available. Zero tests run counts as failure — it's not a meaningful attestation.
