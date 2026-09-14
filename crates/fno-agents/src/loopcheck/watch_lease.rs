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
ever renew, and arming another watcher will not change that. Get the claim back with \
`fno do target start <node>` from inside this worktree, then resume; or hand the PR to \
a session that holds the claim.";

/// The lead of the arm-and-tag hint. One literal, shared by the writer and by
/// [`without_arm_hint`], so the cut can never drift off the sentence it cuts.
pub(super) const ARM_HINT_LEAD: &str = " Arm a harness-tracked watcher";

/// `reason` with the arm-and-tag hint cut off.
///
/// Said when [`NO_CLAIM_REFUSAL`] already told the reader that no watcher can
/// help. One message must not prescribe the ritual it just refused: a session
/// obeyed exactly that contradiction three times before this cut existed.
pub(super) fn without_arm_hint(reason: &str) -> String {
    match reason.find(ARM_HINT_LEAD) {
        Some(i) => reason[..i].trim_end().to_string(),
        None => reason.to_string(),
    }
}

/// Why a watch-lease renewal declined (x-b445). The first three are permanent
/// for this session: arming another watcher cannot change them. `contended`
/// (a peer held the recovery mutex, or the record answered nothing) and
/// `write_failed` can succeed on the next stop.
#[derive(Debug)]
pub(super) enum RenewCause {
    /// The claim lockfile is missing (released or reaped).
    Gone,
    /// The lockfile's holder differs from the manifest's recorded holder.
    HeldByOther(String),
    /// The holder matches but the status verdict reads stale.
    Stale,
    /// The holder matches, the verdict reads live/suspect, and renewal still
    /// declined: a peer held the recovery mutex.
    Contended,
    /// `renew` returned Err.
    WriteFailed,
}

impl RenewCause {
    /// The `watch_refusal` event value for this cause (schema.yaml enum).
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            RenewCause::Gone => "gone",
            RenewCause::HeldByOther(_) => "held_by_other",
            RenewCause::Stale => "stale",
            RenewCause::Contended => "contended",
            RenewCause::WriteFailed => "write_failed",
        }
    }

    pub(super) fn is_permanent(&self) -> bool {
        matches!(
            self,
            RenewCause::Gone | RenewCause::HeldByOther(_) | RenewCause::Stale
        )
    }
}

/// The lead shared by every permanent-cause refusal. The transient refusals
/// ("watch lease could not be renewed", harness, not-async) never carry it,
/// which is what [`refusal_is_permanent`] matches on.
const PERMANENT_REFUSAL_LEAD: &str = "watching ignored: this session's watch lease is dead:";

/// The refusal for a cause, or `None` for a transient one: the caller keeps
/// today's generic text and its arm hint, because a retry can succeed.
pub(super) fn renewal_refusal(cause: &RenewCause) -> Option<String> {
    let remedy = "Get the claim back with `fno do target start <node>` from inside this \
worktree, then resume; or hand the PR to a session that holds the claim.";
    match cause {
        RenewCause::Gone => Some(format!(
            "{PERMANENT_REFUSAL_LEAD} the node claim lockfile is gone (released or \
reaped). Arming another watcher will not change that. {remedy}"
        )),
        RenewCause::HeldByOther(holder) => Some(format!(
            "{PERMANENT_REFUSAL_LEAD} the node claim is held by {holder}, not this \
session. Arming another watcher will not change that. {remedy}"
        )),
        RenewCause::Stale => Some(format!(
            "{PERMANENT_REFUSAL_LEAD} the claim's holder reads dead (stale). Arming \
another watcher will not change that. {remedy}"
        )),
        RenewCause::Contended | RenewCause::WriteFailed => None,
    }
}

/// True for the no-claim refusal and every permanent-cause refusal: arming
/// another watcher cannot help, so the block reason must not prescribe the
/// ritual it just refused (x-b445 generalizes the [`NO_CLAIM_REFUSAL`] cut).
pub(super) fn refusal_is_permanent(reason: &str) -> bool {
    reason == NO_CLAIM_REFUSAL || reason.starts_with(PERMANENT_REFUSAL_LEAD)
}

/// Why `renew` did not answer Ok(true) for this session's own claim pair
/// (x-b445). Reads the claim once and applies the same status verdict
/// `fno agents claim status` prints, so a refusal names the answer the
/// operator would see - never a second liveness opinion. `renew_error` is
/// renew's Err payload when it errored; `root` mirrors renew's own root
/// argument (production passes None).
pub(super) fn renew_cause(
    key: &str,
    holder: &str,
    renew_error: Option<&str>,
    root: Option<&std::path::Path>,
) -> RenewCause {
    if renew_error.is_some() {
        return RenewCause::WriteFailed;
    }
    let path = match crate::claims::claim_path(key, root) {
        Ok(path) => path,
        Err(_) => return RenewCause::WriteFailed,
    };
    match crate::claims::read_claim_file(&path) {
        Err(crate::claims::ReadError::GoneAway) => RenewCause::Gone,
        Err(crate::claims::ReadError::Corrupted(_)) => RenewCause::Contended,
        Ok(rec) if rec.holder != holder => RenewCause::HeldByOther(rec.holder),
        Ok(rec) => {
            if crate::claim_verbs::status_verdict(&rec).0 == crate::claims::ClaimState::Stale {
                RenewCause::Stale
            } else {
                RenewCause::Contended
            }
        }
    }
}

/// Attach the watching refusal cause to a block `loop_check` event, but ONLY
/// when the fire carried one: a non-watching block carries no `watch_refusal`
/// key at all, so consumers read its ABSENCE, never a null (x-b445).
pub(super) fn attach_watch_refusal(event: &mut serde_json::Value, kind: Option<&'static str>) {
    if let Some(kind) = kind {
        event["watch_refusal"] = serde_json::Value::String(kind.to_string());
    }
}
