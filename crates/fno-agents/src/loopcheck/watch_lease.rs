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

/// What a `<watching>` tag says the session waits on: the blocker class the
/// idle event records, and the PR number only when the tag names a real one.
/// `pr="0"`, `pr="pending"` and an absent pr all read as no PR. The tag's own
/// `reason=`/`pr=` attributes are the only source here (the idle verifies
/// nothing), so the blocker is only as trustworthy as the agent's tag.
pub(super) fn watch_target(reason: &str, pr: Option<&str>) -> (&'static str, Option<i64>) {
    let blocker = match reason {
        "ci" => "ci",
        "review" => "review",
        "merge_slot" => "merge_slot",
        "local" => "local",
        _ => "unknown",
    };
    (
        blocker,
        pr.and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|n| *n > 0),
    )
}

/// The only runtime text a session with no PR sees teaching the `<watching>`
/// tag: the Step-5 continue message passed to the inbox nudge.
pub(super) const CONTINUE_WORKING: &str = "continue working; no completion signal. If you are \
only waiting with nothing to do, arm a harness-tracked watcher with a hard timeout and end your \
turn with the tag. A PR wait: background Bash `fno do pr wait <N> --until settled \
--timeout=30m` (REST through the coalescing cache, 60s interval, never `gh pr checks --watch`, \
which spends the shared GraphQL quota; a review wait is `--until review`), then `<watching \
reason=\"ci|review\" pr=\"<N>\" timeout=\"30m\">`. A local run (a test suite, a build, a review \
fork) is its own background task, then `<watching reason=\"local\" timeout=\"30m\">` with no \
pr: pr is a real PR number or left out, never 0. The session idles until the watcher exits \
instead of re-waking every tick.";

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

/// The lead of the arm-and-tag hint. One literal, shared by the writer (the
/// Step-5 continue message) and the idle-refusal tests, so the cut can never
/// drift off the sentence it cuts.
pub(super) const ARM_HINT_LEAD: &str = " Arm a harness-tracked watcher";

/// The arm-and-tag hint's lead, shared with the writer: cutting a hint means
/// cutting from this exact sentence on.
pub(super) fn without_arm_hint(reason: &str) -> String {
    match reason.find(ARM_HINT_LEAD) {
        Some(i) => reason[..i].trim_end().to_string(),
        None => reason.to_string(),
    }
}

/// Whether a refusal text is permanent for the session: re-arming a watcher
/// cannot change it, so the block drops the arm hint instead of contradicting
/// itself inside one message.
pub(super) fn refusal_is_permanent(reason: &str) -> bool {
    reason == NO_CLAIM_REFUSAL || reason.starts_with(PERMANENT_REFUSAL_LEAD)
}

/// Attach the refusal's cause class to a loop_check row.
pub(super) fn attach_watch_refusal(event: &mut serde_json::Value, kind: Option<&'static str>) {
    if let Some(kind) = kind {
        event["watch_refusal"] = serde_json::Value::String(kind.to_string());
    }
}

/// Why a watch-lease renewal declined. The first three are permanent
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
    /// The cause string. schema.yaml keeps the `watch_refusal` enum for old
    /// journals; nothing emits the event since the inline refusal landed.
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
/// ("watch lease could not be renewed", harness, not-async) never carry it.
const PERMANENT_REFUSAL_LEAD: &str = "watching ignored: this session's watch lease is dead:";

/// The refusal for a cause, or `None` for a transient one: the caller keeps
/// the generic text, because a retry can succeed.
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

/// Why `renew` did not answer Ok(true) for this session's own claim pair
///. Reads the claim once and applies the same status verdict
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

/// The refusal a watching fire falls through to when it cannot idle, with
/// the `watch_refusal` row field naming the cause class.
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
    }

    #[test]
    fn gone_and_stale_refusals_are_permanent_and_name_the_remedy() {
        let td = tempfile::TempDir::new().unwrap();
        // gone: no lockfile at all.
        let cause = renew_cause("node:x-b445t", "target-session:me", None, Some(td.path()));
        assert!(matches!(cause, RenewCause::Gone));
        assert!(renewal_refusal(&cause).is_some());
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
    }

    #[test]
    fn write_failed_cause_from_renew_error() {
        let cause = renew_cause("node:x-b445t", "target-session:me", Some("boom"), None);
        assert!(matches!(cause, RenewCause::WriteFailed));
    }

    /// A pid the OS does not report, so the claim reads as a corpse.
    fn dead_pid() -> u32 {
        let mut candidate = 999_999u32;
        while std::path::Path::new(&format!("/proc/{candidate}")).exists()
            || unsafe { libc::kill(candidate as i32, 0) } == 0
        {
            candidate += 1;
        }
        candidate
    }

    #[test]
    fn renew_extends_an_expired_bg_job_claim_whose_witness_says_live() {
        //: a claude BACKGROUND-JOB session holds a node claim (ee2edef3
        // on, PR 2010), arms the sanctioned watcher, and idles past the
        // claim TTL. The job's supervisor pid is gone by the next stop, but the
        // session itself is alive: the witness answers from the registry row
        // keyed by the bg-job session id. Pre- renew refused every
        // expired claim, and the stop hook rejected the watching tag 32 times
        // with the transient "could not be renewed" text. This pins the
        // bg-job shape: expired + a LIVE verdict through a session id means
        // renew extends and re-anchors to the live row pid.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let td = tempfile::TempDir::new().unwrap();
        let home = td.path().join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        let saved_home = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", &home);

        let live = std::process::id();
        let entry = crate::state::RegistryEntry {
            name: "t-3227-bgjob".into(),
            harness: Some("claude".into()),
            harness_session_id: Some("t-3227-bgjob-session".into()),
            pid: Some(live),
            pid_start_time: crate::daemon::process_start_time(live),
            created_at: "2026-09-15T00:00:00Z".into(),
            ..Default::default()
        };
        let registry = crate::state::Registry {
            entries: vec![entry],
            ..Default::default()
        };
        let registry_path = crate::paths::AgentsHome::at(&home).registry_json();
        std::fs::write(&registry_path, serde_json::to_string(&registry).unwrap()).unwrap();

        let opts = crate::claims::AcquireOpts {
            root: Some(td.path().to_path_buf()),
            ttl_ms: Some(60_000),
            pid: Some(dead_pid()),
            ..Default::default()
        };
        let _ = crate::claims::acquire("node:-bglease", "target-session:me", opts);
        let path = crate::claims::claim_path("node:-bglease", Some(td.path())).unwrap();
        let mut rec = crate::claims::read_claim_file(&path).unwrap();
        rec.session_id = Some("t-3227-bgjob-session".into());
        rec.expires_at = Some(crate::claims::now_ms() - 1);
        crate::claims::atomic_replace(&path, &crate::claims::serialize_claim(&rec).unwrap())
            .unwrap();

        let live_rec = crate::claims::read_claim_file(&path).unwrap();
        let (state, basis) = crate::claim_verbs::status_verdict(&live_rec);
        assert_eq!(
            state,
            crate::claims::ClaimState::Live,
            "fixture must read LIVE through the bg-job witness or AC proves nothing"
        );
        assert_eq!(
            basis,
            crate::claims::basis::REGISTRY_SESSION_LIVE,
            "{basis}"
        );

        let result = crate::claims::renew(
            "node:-bglease",
            "target-session:me",
            120_000,
            Some(td.path()),
        );
        match saved_home {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        assert_eq!(result, Ok(true));

        let after = crate::claims::read_claim_file(&path).unwrap();
        assert_eq!(
            after.pid,
            Some(live as i32),
            "the anchor must MOVE to the live bg-job row pid"
        );
        assert!(after.expires_at.unwrap() > crate::claims::now_ms());
    }

    #[test]
    fn watch_target_maps_known_reasons_and_local() {
        assert_eq!(watch_target("ci", Some("404")), ("ci", Some(404)));
        assert_eq!(watch_target("review", Some("9")), ("review", Some(9)));
        assert_eq!(
            watch_target("merge_slot", Some("1")),
            ("merge_slot", Some(1))
        );
        assert_eq!(watch_target("local", None), ("local", None));
        assert_eq!(watch_target("cargo-test", None), ("unknown", None));
    }

    #[test]
    fn watch_target_never_reads_pr_zero_or_non_numbers() {
        // AC1-ERR: pr="0" is no PR, never PR zero.
        assert_eq!(watch_target("ci", Some("0")), ("ci", None));
        // AC1-EDGE: non-numbers and non-positives all read absent.
        assert_eq!(watch_target("review", Some("pending")), ("review", None));
        assert_eq!(watch_target("ci", Some("")), ("ci", None));
        assert_eq!(watch_target("ci", Some("-3")), ("ci", None));
        assert_eq!(watch_target("local", Some("0")), ("local", None));
    }

    #[test]
    fn watch_target_keeps_a_real_pr_number_even_padded() {
        assert_eq!(watch_target("ci", Some(" 2206 ")), ("ci", Some(2206)));
    }

    #[test]
    fn continue_working_teaches_local_and_never_pr_zero() {
        // AC2-HP: the tag the message teaches for a local run parses back
        // through the same reader the stop hook uses.
        let start = CONTINUE_WORKING
            .find("<watching reason=\"local\"")
            .expect("the local tag form is taught");
        let end = start + CONTINUE_WORKING[start..].find('>').unwrap() + 1;
        let tag = &CONTINUE_WORKING[start..end];
        assert_eq!(
            super::super::detect_intent_from_text(tag),
            super::super::Intent::Watching {
                reason: "local".into(),
                pr: None,
                timeout: Some("30m".into()),
            }
        );
        // AC2-ERR: the PR form stays taught, PR zero never is.
        assert!(CONTINUE_WORKING.contains("reason=\"ci|review\" pr=\"<N>\""));
        assert!(!CONTINUE_WORKING.contains("pr=\"0\""));
        assert!(CONTINUE_WORKING.contains("pr is a real PR number or left out, never 0"));
    }
}
