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

/// Classify a declined renewal of this session's recorded claim pair.
/// `outcome` is what `renew` answered; `None` on either side means the
/// manifest recorded no pair or renew never ran, and the cause stays None.
pub(super) fn declined_cause(
    claim: Option<&(String, String)>,
    outcome: Option<&Result<bool, String>>,
) -> Option<RenewCause> {
    claim.zip(outcome).map(|((key, holder), res)| {
        renew_cause(key, holder, res.as_ref().err().map(String::as_str), None)
    })
}

/// The lease sentence appended to a stand-down block reason: the permanent
/// refusal for a dead lease, or nothing (a transient one keeps its retry).
pub(super) fn permanent_lease_note(
    claim: Option<&(String, String)>,
    outcome: Option<&Result<bool, String>>,
) -> String {
    declined_cause(claim, outcome)
        .filter(|c| c.is_permanent())
        .map(|c| format!(" {}", renewal_refusal(&c).unwrap_or_default()))
        .unwrap_or_default()
}

/// The refusal a watching fire falls through to when it cannot idle, with the
/// `watch_refusal` event kind for the same answer. One construction for both
/// the block reason and the event field, so they cannot disagree.
pub(super) struct WatchRefusal {
    pub(super) reason: String,
    pub(super) kind: &'static str,
}

pub(super) fn idle_refusal(
    can_idle: bool,
    author_harness: Option<&str>,
    is_loop_run_child: bool,
    blocker_none: bool,
    unaddressed_findings: usize,
    claim_present: bool,
    lease_cause: Option<&RenewCause>,
) -> WatchRefusal {
    let reason = if !can_idle {
        watching_harness_refusal(author_harness, is_loop_run_child)
    } else if blocker_none && unaddressed_findings > 0 {
        format!(
            "watching ignored: {unaddressed_findings} unaddressed findings, this is not \
an async wait"
        )
    } else if blocker_none {
        "watching ignored: PR is not in an async wait class".to_string()
    } else if !claim_present {
        NO_CLAIM_REFUSAL.to_string()
    } else if let Some(cause) = lease_cause.filter(|c| c.is_permanent()) {
        renewal_refusal(cause)
            .unwrap_or_else(|| "watching ignored: watch lease could not be renewed".to_string())
    } else {
        "watching ignored: watch lease could not be renewed".to_string()
    };
    let kind = if !can_idle {
        "harness"
    } else if blocker_none {
        "not_async"
    } else if !claim_present {
        "no_claim"
    } else {
        lease_cause.map(|c| c.as_str()).unwrap_or("write_failed")
    };
    WatchRefusal { reason, kind }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest fields `claim_pair` scans, as `fno do target init` writes
    /// them: AFTER the closing `---`, one `field: "value"` line each.
    fn watch_manifest(holder: &str) -> String {
        format!("target_claim_key: \"node:x-b445t\"\ntarget_claim_holder: \"{holder}\"\n")
    }

    /// A holder-side fixture acquired under an explicit claims root.
    fn live_claim_opts(root: &std::path::Path) -> crate::claims::AcquireOpts {
        crate::claims::AcquireOpts {
            root: Some(root.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn claim_pair_reads_the_manifest_pair() {
        assert_eq!(
            claim_pair(&watch_manifest("target-session:a")),
            Some(("node:x-b445t".to_string(), "target-session:a".to_string()))
        );
        assert_eq!(claim_pair("target_claim_key: \"node:x\""), None);
    }

    #[test]
    fn held_by_other_refusal_names_the_holder_and_the_remedy_without_the_arm_hint() {
        // AC3-HP: manifest names holder `a`, lockfile held by `b`.
        let td = tempfile::TempDir::new().unwrap();
        let _ = crate::claims::acquire(
            "node:x-b445t",
            "target-session:b",
            live_claim_opts(td.path()),
        );
        let cause = renew_cause("node:x-b445t", "target-session:a", None, Some(td.path()));
        let refusal = renewal_refusal(&cause).expect("held-by-other is permanent");
        assert!(refusal.contains("target-session:b"), "{refusal}");
        assert!(refusal.contains("fno do target start <node>"), "{refusal}");
        assert!(
            !refusal.contains(ARM_HINT_LEAD),
            "permanent refusal must not carry the arm hint: {refusal}"
        );
        assert!(refusal_is_permanent(&refusal));
        // The cut composes refusal + hint-cut blocker the way the fire path does.
        let blocker = format!("some blocker.{ARM_HINT_LEAD} continue watching");
        let composed = format!("{refusal}; {}", without_arm_hint(&blocker));
        assert!(
            !composed.contains(ARM_HINT_LEAD),
            "composed block reason must not re-arm: {composed}"
        );
    }

    #[test]
    fn contended_lease_keeps_the_arm_hint() {
        // AC3-EDGE: holder matches and the verdict reads live, but a peer held
        // the recovery mutex when renew ran, so renewal declined: keep the
        // generic text (the next stop can succeed).
        let td = tempfile::TempDir::new().unwrap();
        let _ = crate::claims::acquire(
            "node:x-b445t",
            "target-session:me",
            live_claim_opts(td.path()),
        );
        // A peer "holds" the recovery mutex: a fresh (non-stale) lock dir.
        let path = crate::claims::claim_path("node:x-b445t", Some(td.path())).unwrap();
        let recovery = path.with_file_name(format!(
            "{}.recovery.d",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir(&recovery).unwrap();
        let renewed = crate::claims::renew(
            "node:x-b445t",
            "target-session:me",
            120_000,
            Some(td.path()),
        );
        assert_eq!(renewed, Ok(false));
        let cause = renew_cause("node:x-b445t", "target-session:me", None, Some(td.path()));
        assert!(matches!(cause, RenewCause::Contended), "{cause:?}");
        assert!(renewal_refusal(&cause).is_none());
        assert!(!refusal_is_permanent(
            "watching ignored: watch lease could not be renewed"
        ));
    }

    #[test]
    fn gone_and_stale_refusals_are_permanent_and_name_the_remedy() {
        let td = tempfile::TempDir::new().unwrap();
        // gone: no lockfile at all.
        let cause = renew_cause("node:x-b445t", "target-session:me", None, Some(td.path()));
        assert!(matches!(cause, RenewCause::Gone));
        assert!(refusal_is_permanent(&renewal_refusal(&cause).unwrap()));
        // stale: dead-pid claim past its TTL with no session witness to heal it.
        let mut o = live_claim_opts(td.path());
        o.pid = Some(999_999_999);
        let _ = crate::claims::acquire("node:x-b445t", "target-session:me", o);
        let claim_path = crate::claims::claim_path("node:x-b445t", Some(td.path())).unwrap();
        let mut rec = crate::claims::read_claim_file(&claim_path).unwrap();
        rec.session_id = None;
        rec.expires_at = Some(crate::claims::now_ms() - 1);
        crate::claims::atomic_replace(&claim_path, &crate::claims::serialize_claim(&rec).unwrap())
            .unwrap();
        let cause = renew_cause("node:x-b445t", "target-session:me", None, Some(td.path()));
        assert!(matches!(cause, RenewCause::Stale), "{cause:?}");
        let refusal = renewal_refusal(&cause).unwrap();
        assert!(refusal.contains("stale"), "{refusal}");
        assert!(refusal_is_permanent(&refusal));
    }

    #[test]
    fn write_failed_cause_from_renew_error() {
        let cause = renew_cause("node:x-b445t", "target-session:me", Some("boom"), None);
        assert!(matches!(cause, RenewCause::WriteFailed));
        assert!(!refusal_is_permanent(
            "watching ignored: PR is not in an async wait class"
        ));
    }

    #[test]
    fn attach_watch_refusal_sets_the_key_only_when_some() {
        // AC4-HP/EDGE: a watching fire's refusal rides the event; a
        // non-watching block carries no `watch_refusal` key at all.
        let mut event = serde_json::json!({"decision": "block"});
        attach_watch_refusal(&mut event, Some("held_by_other"));
        assert_eq!(event["watch_refusal"], serde_json::json!("held_by_other"));
        let mut bare = serde_json::json!({"decision": "block"});
        attach_watch_refusal(&mut bare, None);
        assert!(bare.get("watch_refusal").is_none());
    }
}
