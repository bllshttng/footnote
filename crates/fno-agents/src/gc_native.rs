//! Native retirement effects (x-70e1 task 3): the harness-native half of a
//! retirement, applied through the transports each harness already owns and
//! reported as typed per-effect outcomes.
//!
//! The sweep owns the sequence (stop, active-surface removal, registry drop,
//! tree prune); this module owns the ACTIVE-SURFACE removal - claude's agent
//! list, codex's session index, cursor-agent's detached worker servers,
//! opencode's active session listing - through the same cascade the `rm` verb
//! walks. A history deletion never happens here: the codex index drop keeps
//! the rollout files, the claude list removal keeps the transcript, and an
//! archive op (codex `thread/archive`, opencode `time.archived`) is
//! history-preserving by construction.
//!
//! opencode is the one arm that does NOT run through the shared cascade. The
//! cascade is also the `rm` verb's, and the deleted Python rm twin left an
//! opencode record alone on purpose. Archiving inside the cascade would move
//! one of those two legs and not the other, so the arm lives here, in the
//! retirement lane, which has no twin.
//!
//! Absence is only accepted after a complete enumeration of the exact
//! identity; a failed read is `Unverified` or `Failed`, never absence.

use crate::daemon::{cascade_harness_session_result_with, CascadeOutcome};
use crate::opencode_serve::ArchiveOutcome;
use crate::receipt::EffectRecord;
use crate::state::RegistryEntry;
use std::time::Duration;

impl CascadeOutcome {
    /// The effect-record vocabulary: `confirmed-removed`,
    /// `confirmed-already-absent`, `kept`, `failed`, `not-applicable`. One
    /// string per outcome, named in the receipt, so a partial retirement is
    /// never readable as a full one.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            CascadeOutcome::Removed => "confirmed-removed",
            CascadeOutcome::AlreadyAbsent(_) => "confirmed-already-absent",
            CascadeOutcome::Unverified(_) => "kept",
            CascadeOutcome::Failed(_) => "failed",
            CascadeOutcome::NotApplicable => "not-applicable",
        }
    }

    pub(crate) fn detail(&self) -> Option<String> {
        match self {
            CascadeOutcome::AlreadyAbsent(r)
            | CascadeOutcome::Unverified(r)
            | CascadeOutcome::Failed(r) => Some(r.clone()),
            CascadeOutcome::Removed | CascadeOutcome::NotApplicable => None,
        }
    }

    /// Whether this outcome satisfies the applied gate: only a positive
    /// confirmation (or a measured not-applicable) does. `kept` and `failed`
    /// hold the row for retry.
    pub(crate) fn satisfies_applied(&self) -> bool {
        matches!(
            self,
            CascadeOutcome::Removed
                | CascadeOutcome::AlreadyAbsent(_)
                | CascadeOutcome::NotApplicable
        )
    }

    /// The receipt's EffectRecord for this outcome under a named op.
    pub(crate) fn effect_record(&self, op: &str) -> EffectRecord {
        EffectRecord {
            op: op.to_string(),
            outcome: self.as_str().to_string(),
            detail: self.detail(),
            at: crate::daemon::now_rfc3339_like(),
        }
    }
}

/// The typed effect record for the confirmed stop (x-5aef task 1.1). The
/// stop seam answers a bare bool, so the vocabulary is two-valued: a
/// confirmed stop reads `confirmed-removed`, anything else `failed` - and a
/// `failed` stop holds the row for retry, never retires it. The `detail`
/// (x-1b90) makes the record a measurement: the pane arm fills it with what
/// actually ran; every other caller passes `None` and keeps the two-valued
/// vocabulary.
pub(crate) fn stop_outcome_effect(confirmed: bool, detail: Option<String>) -> EffectRecord {
    EffectRecord {
        op: "native-stop".into(),
        outcome: if confirmed {
            "confirmed-removed".into()
        } else {
            "failed".into()
        },
        detail,
        at: crate::daemon::now_rfc3339_like(),
    }
}

/// The one exception to law d-81c6da7e (remove needs no prior stop): a claude
/// background thread. Every other live row's process is ended by the removal
/// itself; only here does a stop run first - and since x-a33f, `rm` runs that
/// stop itself rather than asking a caller to compose one. A row with no
/// substrate stamp counts: adopted claude rows carry none, and the claude
/// roster lists background sessions only.
pub(crate) fn stop_precedes_removal(e: &RegistryEntry) -> bool {
    e.harness_name() == "claude"
        && e.mux.is_none()
        && !matches!(e.substrate.as_deref(), Some("pane") | Some("headless"))
}

/// Apply the ACTIVE-SURFACE removal for one row through the production
/// seams (the daemon roster read and `claude rm`), returning the typed
/// outcome the sweep records on the receipt. The roster read unions every
/// account root, and the removal is routed to the root where that read found
/// the row - absence from a single ambient read is a WRONG-ROOT absence and
/// has never been removal evidence.
pub(crate) fn apply_active_surface_removal(e: &RegistryEntry) -> CascadeOutcome {
    if e.harness_name() == "opencode" {
        return apply_opencode_archive(e);
    }
    // The snapshot is computed ONCE here and handed to the cascade, matching
    // the rm handler: the pre-check, the removal and the post-read must see
    // the same listing generation, and a claude arm without a snapshot is a
    // panic the caller cannot recover from.
    let snapshot = crate::claude_roster::read_all_agents_union();
    cascade_harness_session_result_with(
        e,
        Some(&snapshot),
        &crate::claude_roster::read_all_agents_union,
        &|short_id| {
            let dir = crate::claude_roster::removal_config_dir(
                &snapshot,
                short_id,
                e.launch_account.as_deref(),
            )?;
            crate::daemon::run_claude_rm_in(dir.as_deref(), short_id)
        },
    )
}

/// The mux squad store's default host session, shared with the daemon's
/// other mux calls (`crates/fno/src/proto.rs` `DEFAULT_SESSION`).
pub(crate) const MUX_DEFAULT_SERVER: &str = "main";

/// Apply the MUX-MEMBER retirement for one row: retire the row's
/// squad membership from the shared mux store through `fno mux
/// retire-session`. The live-membership measurement is the squad store,
/// never the registry `mux` ref: thread members carry no mux ref, so a ref
/// alone answers neither yes nor no.
pub(crate) fn apply_mux_member_retirement(e: &RegistryEntry) -> CascadeOutcome {
    mux_member_outcome_for_row(e, crate::gc_inventory::read_mux_members(), &|server| {
        let harness = e.harness_name();
        let sid = e.harness_session_id.clone().unwrap_or_default();
        run_mux_retire_session(server, &harness, &sid)
    })
}

/// The decision with the squad-store read result as an input, so every
/// branch is testable without touching `HOME`.
pub(crate) fn mux_member_outcome_for_row(
    e: &RegistryEntry,
    members: Result<Vec<(String, String)>, String>,
    run: &dyn Fn(&str) -> Result<u64, String>,
) -> CascadeOutcome {
    let live = match members {
        Ok(members) => {
            let sid = e.harness_session_id.as_deref().unwrap_or("");
            members
                .iter()
                .any(|(h, s)| h == e.harness_name() && s == sid)
        }
        Err(reason) => return CascadeOutcome::Failed(reason),
    };
    mux_member_outcome(
        &e.harness_name(),
        &e.harness_session_id.clone().unwrap_or_default(),
        live,
        e.mux.as_ref(),
        run,
    )
}

/// The pure decision behind [`apply_mux_member_retirement`]: a row with no
/// live squad member and no mux ref is not-applicable; no live member WITH a
/// ref is a measured absence; a live member retires through `run`, which
/// answers the member count the store reported.
pub(crate) fn mux_member_outcome(
    harness: &str,
    session_id: &str,
    live_member: bool,
    mux: Option<&crate::state::MuxRef>,
    run: &dyn Fn(&str) -> Result<u64, String>,
) -> CascadeOutcome {
    if !live_member {
        return match mux {
            None => CascadeOutcome::NotApplicable,
            Some(_) => CascadeOutcome::AlreadyAbsent(format!(
                "no live squad member for {harness}:{session_id}"
            )),
        };
    }
    let server = mux
        .map(|m| m.session.as_str())
        .unwrap_or(MUX_DEFAULT_SERVER);
    match run(server) {
        Ok(n) if n >= 1 => CascadeOutcome::Removed,
        Ok(_) => {
            CascadeOutcome::AlreadyAbsent("the mux store retired 0 members for this session".into())
        }
        Err(detail) => CascadeOutcome::Failed(detail),
    }
}

/// One bounded `fno mux retire-session` round: `Ok(n)` is the member count
/// the store reported retired; `Err` names the exit and the first stderr
/// line. Exit 20 (sent, unanswered) is a failure like any other - an
/// outcome unknown is not an absence.
fn run_mux_retire_session(server: &str, harness: &str, session_id: &str) -> Result<u64, String> {
    let mut child = std::process::Command::new(crate::scrape::fno_bin())
        .args([
            "mux",
            "retire-session",
            server,
            "--harness",
            harness,
            "--session-id",
            session_id,
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("mux retire-session failed to start: {error}"))?;
    let deadline = std::time::Instant::now() + crate::daemon::CASCADE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("mux retire-session read failed: {error}"))?;
                let reply: serde_json::Value = serde_json::from_slice(&output.stdout)
                    .map_err(|error| format!("mux retire-session reply unparsable: {error}"))?;
                let retired = reply
                    .get("retired")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        "mux retire-session reply carried no retired count".to_string()
                    })?;
                return Ok(retired);
            }
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                let output = child.wait_with_output().ok();
                let stderr = output
                    .as_ref()
                    .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                    .unwrap_or_default();
                let first = stderr.lines().next().unwrap_or_default().trim();
                return Err(format!("mux retire-session exited {code}: {first}"));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("mux retire-session timed out".into());
            }
            Err(error) => return Err(format!("mux retire-session wait failed: {error}")),
        }
    }
}

/// opencode's active-surface removal, wired to the production seams: the
/// recorded serve and the archive PATCH.
fn apply_opencode_archive(e: &RegistryEntry) -> CascadeOutcome {
    let serve = crate::paths::AgentsHome::from_env_opt()
        .and_then(|home| crate::opencode_serve::archive_capable_serve(&home))
        .map(|handle| (handle.base_url, handle.token));
    opencode_archive_outcome(
        e.harness_session_id.as_deref(),
        serve,
        &crate::opencode_serve::archive_session,
    )
}

/// Map one archive attempt onto the effect vocabulary.
///
/// Every skip answers `NotApplicable`, which is what an opencode row measured
/// before this op existed. That is deliberate: a missing serve or a serve too
/// old for the op is not evidence about the session, and answering `kept`
/// there would hold every opencode row on a machine that runs no serve.
///
/// A transport error is `Unverified` (the row comes back next sweep). A write
/// the server accepted and did not store is `Failed` - the same shape as a
/// claude row surviving a successful `claude rm`.
///
/// The id must be shape-valid before any request carries it, the same gate the
/// reachability probe applies before it reaches SQL. An id of another harness's
/// shape would 404 and read as `confirmed-already-absent`: a receipt claiming a
/// measured absence for a session this code never addressed.
pub(crate) fn opencode_archive_outcome(
    session_id: Option<&str>,
    serve: Option<(String, String)>,
    archive: &dyn Fn(&str, &str, &str) -> Result<ArchiveOutcome, String>,
) -> CascadeOutcome {
    let Some(sid) = session_id.filter(|s| crate::provider::is_opencode_session_id(s)) else {
        return CascadeOutcome::NotApplicable;
    };
    let Some((base_url, token)) = serve else {
        return CascadeOutcome::NotApplicable;
    };
    match archive(&base_url, &token, sid) {
        Ok(ArchiveOutcome::Archived) => CascadeOutcome::Removed,
        Ok(ArchiveOutcome::AlreadyArchived) => {
            CascadeOutcome::AlreadyAbsent(format!("opencode session {sid} was already archived"))
        }
        Ok(ArchiveOutcome::Gone) => CascadeOutcome::AlreadyAbsent(format!(
            "opencode session {sid} is absent from the store"
        )),
        Ok(ArchiveOutcome::Survived) => CascadeOutcome::Failed(format!(
            "opencode session {sid} is still unarchived after an accepted archive write"
        )),
        Err(reason) => CascadeOutcome::Unverified(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        harness: &str,
        substrate: Option<&str>,
        mux: Option<crate::state::MuxRef>,
    ) -> RegistryEntry {
        RegistryEntry {
            name: "w".into(),
            harness: Some(harness.into()),
            substrate: substrate.map(str::to_string),
            mux,
            ..Default::default()
        }
    }

    #[test]
    fn stop_precedes_removal_names_only_the_claude_background_thread() {
        // AC1-HP: the claude background thread is the one yes.
        assert!(stop_precedes_removal(&row("claude", Some("thread"), None)));
        assert!(!stop_precedes_removal(&row("codex", Some("thread"), None)));
        assert!(!stop_precedes_removal(&row(
            "codex",
            Some("pane"),
            Some(crate::state::MuxRef {
                session: "main".into(),
                pane_id: 1,
            })
        )));
        assert!(!stop_precedes_removal(&row("claude", Some("pane"), None)));
        assert!(!stop_precedes_removal(&row(
            "claude",
            Some("headless"),
            None
        )));
    }

    #[test]
    fn an_unstamped_claude_row_counts_and_a_mux_ref_disqualifies() {
        // AC1-EDGE: adopted claude rows carry no substrate stamp.
        assert!(stop_precedes_removal(&row("claude", None, None)));
        assert!(!stop_precedes_removal(&row(
            "claude",
            None,
            Some(crate::state::MuxRef {
                session: "main".into(),
                pane_id: 2,
            })
        )));
    }

    #[test]
    fn mux_member_without_a_live_member_and_without_a_ref_is_not_applicable() {
        // AC1-EDGE: neither branch runs the retire-session child.
        let called = std::cell::Cell::new(false);
        let outcome = mux_member_outcome("claude", "sess-a", false, None, &|_server| {
            called.set(true);
            Ok(1)
        });
        assert!(matches!(outcome, CascadeOutcome::NotApplicable));
        assert!(!called.get(), "no live member and no ref: nothing to run");
    }

    #[test]
    fn mux_member_without_a_live_member_but_with_a_ref_is_already_absent() {
        // AC1-EDGE: the ref alone is not membership evidence; the store is.
        let called = std::cell::Cell::new(false);
        let mux = crate::state::MuxRef {
            session: "main".into(),
            pane_id: 1,
        };
        let outcome = mux_member_outcome("claude", "sess-a", false, Some(&mux), &|_server| {
            called.set(true);
            Ok(1)
        });
        match &outcome {
            CascadeOutcome::AlreadyAbsent(detail) => {
                assert!(detail.contains("claude:sess-a"), "{detail}");
            }
            other => panic!("expected AlreadyAbsent, got {other:?}"),
        }
        assert!(!called.get(), "no live member: nothing to run");
    }

    #[test]
    fn mux_member_reads_the_server_from_the_mux_ref_and_the_default_otherwise() {
        let mux = crate::state::MuxRef {
            session: "aux".into(),
            pane_id: 1,
        };
        let servers: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let live = |server: &str| -> Result<u64, String> {
            servers.borrow_mut().push(server.to_string());
            Ok(1)
        };
        let with_ref = mux_member_outcome("claude", "s", true, Some(&mux), &live);
        assert!(matches!(with_ref, CascadeOutcome::Removed));
        let without_ref = mux_member_outcome("claude", "s", true, None, &live);
        assert!(matches!(without_ref, CascadeOutcome::Removed));
        assert_eq!(
            servers.into_inner(),
            vec!["aux".to_string(), MUX_DEFAULT_SERVER.to_string()]
        );
    }

    #[test]
    fn mux_member_maps_the_run_answers_onto_the_effect_vocabulary() {
        // AC1-EDGE: retired 0 is a measured absence; an error is a failure.
        let run_zero = |_: &str| -> Result<u64, String> { Ok(0) };
        let outcome = mux_member_outcome("claude", "s", true, None, &run_zero);
        assert!(matches!(outcome, CascadeOutcome::AlreadyAbsent(_)));
        let run_err = |_: &str| -> Result<u64, String> {
            Err("mux retire-session exited 20: timed out waiting for reply".into())
        };
        let outcome = mux_member_outcome("claude", "s", true, None, &run_err);
        assert!(matches!(outcome, CascadeOutcome::Failed(_)));
    }

    #[test]
    fn mux_member_unreadable_store_is_failed_and_a_missing_one_is_not_a_failure() {
        // AC1-EDGE: an unread store is not a measured absence. The missing
        // store half is `read_mux_members`'s own contract; here the read
        // fails and the row holds.
        let e = row("claude", None, None);
        let outcome = mux_member_outcome_for_row(
            &e,
            Err("squads.json unreadable: permission denied".into()),
            &|_| Ok(1),
        );
        assert!(matches!(outcome, CascadeOutcome::Failed(_)));
    }
}
