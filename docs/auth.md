# Authentication

footnote sits on top of Claude Code's authentication. When you drive the loop with `/target`, the underlying agent runtime authenticates using the same mechanisms it would for an interactive Claude Code session.

## Credential resolution order

1. **Claude Code OAuth.** If you have signed into Claude Code (`claude login` or via the desktop app), footnote uses that session. Recommended for solo founders and interactive use.
2. **`ANTHROPIC_API_KEY` environment variable.** If set, this overrides the OAuth path. Use for CI, scripts, or when you explicitly want to bill an API account separately from your interactive Claude.ai account.
3. **Other agent runtimes.** If you have configured megawalk's drivers to use alternative LLM hosts, those runtimes have their own authentication. See driver-specific docs.

## Multi-account warning

If you use multiple Claude.ai accounts (personal vs work, or solo-founder vs company), you may have multiple OAuth sessions on the same machine. footnote uses whichever session Claude Code is currently authenticated as. Verify before running long autonomous loops:

```bash
claude /status
```

A run against the wrong account bills the wrong subscription.

## What credentials get used during a target run

A single `/target` run (think through ship, end to end) uses:

- **Anthropic credentials** (one of the above) for the LLM invocations.
- **Local git credentials** for repo operations.
- **`gh` CLI authentication** for PR creation. Run `gh auth status` to verify.

If any of these are misconfigured, the run will fail with a clear error in the relevant phase.

## What credentials are never used

- No footnote-internal credentials. There is no footnote cloud service.
- No credentials sent to third-party services other than the agent runtime you have configured.
- No telemetry beyond what Anthropic and GitHub already collect for their products.

See `docs/security-posture.md` for the broader trust model.
