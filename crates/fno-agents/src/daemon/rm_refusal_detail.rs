//! The rm live-row refusal text, one builder for every roster verdict.
//!
//! Named by the question it answers: why `fno agents rm` refuses a stored-live
//! row. The file-budget gate holds daemon.rs at shrink-only, so message
//! construction lives here instead of growing the handler.

/// Build the Busy-refusal detail for a stored-live row. `row` is the harness
/// row id (or the `(no harness row id)` placeholder); `row_present` only
/// means something when `roster_known` holds; `warnings` is the snapshot's
/// own warning text (partial-list marker, or the read's failure reason).
/// `pane` is the row's mux ref as `(session, pane_id)` when it could name a
/// pane; a ref with no session is not a pane address and reads as `None`.
pub(crate) fn live_row_refusal(
    name: &str,
    row: &str,
    harness_name: &str,
    harness_row_id_none: bool,
    roster_known: bool,
    row_present: bool,
    warnings: &str,
    pane: Option<(&str, u64)>,
) -> String {
    let pane = pane.filter(|(session, _)| !session.is_empty());
    if harness_name != "claude" {
        if let Some((session, pane_id)) = pane {
            // stop cannot serve a pane worker (ruling d-658e6834), so name
            // the pane-kill path rm itself re-checks: once the pane is gone
            // the probe proves absence and rm proceeds with no --force.
            return format!(
                "agent {name} is still live. It is a pane worker; stop cannot serve it. \
                 Kill the pane: `fno mux pane kill {session}:{pane_id}`. The registry \
                 row survives that; re-run `fno agents rm {name}` and it proceeds on \
                 its own once the pane is provably gone."
            );
        }
        format!(
            "agent {name} is still live. Stop it with `fno agents stop {name}`; rm \
             proceeds on its own once the row is gone. Forcing it through orphans a \
             live process and spends the row's resume handle. If stop answers no_op \
             (no addressable session behind the row), the row cannot prove liveness \
             either way; the override for that case is documented in `fno agents rm \
             --help`, not here."
        )
    } else if harness_row_id_none {
        // claude_row_provably_absent short-circuits to `false` (not
        // provably gone) whenever the row id is None, independent of the
        // roster -- so presence was never actually checked here, and the
        // roster_known branch's "is present in" claim plus its runnable
        // `claude stop <row>` commands would both be false (self-review
        // finding).
        format!(
            "agent {name} is still live, but it has no resolvable harness row id, so \
             its presence in `claude agents --json --all` cannot be checked. Stop it \
             with `fno agents stop {name}`; rm proceeds on its own once the row is \
             gone. Forcing it through spends the resume handle the row still holds. \
             If stop refuses or no-ops because the row has no addressable session, \
             the row cannot prove liveness either way; the override for that case is \
             documented in `fno agents rm --help`, not here."
        )
    } else if roster_known && row_present {
        // A Known list can still be partial (warnings). The row IS in
        // what parsed, but the operator should know the list was not
        // clean.
        let mut message = format!(
            "agent {name} is still live. Its harness row {row} is present in \
             `claude agents --json --all`. Stop it with `fno agents stop {name}`; rm \
             proceeds on its own once that row is gone. Do not tear the row down by \
             hand: that spends the resume handle for nothing, and `fno agents rm` \
             makes the same call itself."
        );
        if !warnings.is_empty() {
            message.push_str(&format!(
                " (the roster read carried warnings, so the list is partial: {warnings})"
            ));
        }
        message
    } else if roster_known {
        format!(
            "agent {name} is still live, and its harness row {row} was not in the \
             parsed rows, but the roster read carried warnings, so the list is \
             partial and absence is not proof: {warnings}. Retry once the roster \
             reads clean: rm re-reads it and proceeds on its own when the row is \
             provably gone. Forcing it through spends the resume handle on \
             unverified evidence."
        )
    } else {
        // Name the read's own reason: "the roster read failed" without it
        // sends callers to retry a timeout, which reproduces forever.
        let reason = if warnings.is_empty() {
            "no reason recorded"
        } else {
            warnings
        };
        format!(
            "agent {name} is still live, and its harness row {row}'s presence in \
             `claude agents --json --all` could not be confirmed (the roster read \
             failed: {reason}). Retry once that read succeeds: rm re-reads it and \
             proceeds on its own when the row is provably gone. Forcing it through \
             spends the resume handle on unverified evidence."
        )
    }
}
