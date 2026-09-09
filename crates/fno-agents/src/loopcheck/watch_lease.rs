//! The watch lease: what a `<watching>` tag is allowed to idle on, and for
//! how long. One question, so one module - the four answers below were four
//! loose helpers in the middle of the stop gate.

/// Slack added beyond the declared watch window so the claim lease outlives the
/// agent's watcher: a watcher that fires right at its timeout must not
/// race claim expiry.
pub(super) const WATCH_SLACK_MS: i64 = 12 * 60_000;

/// Lease window for an idle watch: the declared timeout clamped to [5m, 2h]
/// (never trust the tag for an unbounded hold) plus slack. Defaults to 30m when
/// the tag omits or mangles `timeout`, giving the ~40m default lease.
pub(super) fn watch_window_ms(timeout: Option<&str>) -> i64 {
    let declared = timeout
        .and_then(crate::claims::parse_ttl_ms)
        .unwrap_or(30 * 60_000);
    declared.clamp(5 * 60_000, 2 * 3_600_000) + WATCH_SLACK_MS
}

/// Whether a session's harness + substrate can park-and-wake on a `<watching>`
/// idle. Only a Claude session's harness-tracked background/Monitor
/// tasks re-invoke the model when they exit, so only Claude may idle. A
/// `fno-agents loop run` child exits on allow (FNO_DRIVER_LIB set), and
/// codex/gemini have no self-wake on background-task exit - their waker is the
/// fno-agents daemon consuming the watch event, shipped as a separate
/// live-verified follow-up - so all of those keep today's block behavior rather
/// than idling with nothing to wake them (a dead watch). This is the design's
/// "unroutable harness -> status quo, never a dead watch" degradation.
pub(super) fn harness_can_idle(author_harness: Option<&str>, is_loop_run_child: bool) -> bool {
    author_harness == Some("claude") && !is_loop_run_child
}

pub(super) fn watching_harness_refusal(
    author_harness: Option<&str>,
    is_loop_run_child: bool,
) -> String {
    if is_loop_run_child {
        "watching ignored: loop-run child cannot idle".to_string()
    } else {
        format!(
            "watching ignored: harness {} cannot idle",
            author_harness.unwrap_or("unknown")
        )
    }
}

/// The claim pair this session recorded at init, if it recorded one.
///
/// `fno do target init` writes neither field when the node was already held by
/// another session, and the manifest is write-once. So an absent pair is
/// PERMANENT for the life of the session: no lease here can ever renew.
pub(super) fn claim_pair(manifest: &str) -> Option<(String, String)> {
    super::scan_manifest_field(manifest, "target_claim_key")
        .zip(super::scan_manifest_field(manifest, "target_claim_holder"))
}

/// Said instead of the generic renewal refusal when there is no claim at all.
/// The generic one reads as transient, so a reader believes another watcher
/// will help, arms one on every stop, and never idles once.
pub(super) const NO_CLAIM_REFUSAL: &str =
    "watching ignored: this session recorded no node claim at init, so no watch lease can \
ever renew, and arming another watcher will not change that. Spend each wake on real work, \
or hand the PR to a session that holds the claim.";
