//! The per-pane orphan verdict (v71): a pane whose stored member the evidence
//! judges Dead, with no live registry row on it, is orphaned and its tab
//! closes under the default prune. Mounted as a child of server.rs's
//! `mod tests` (use super::*).

use super::*;

fn dead_evidence(attach: &str) -> crate::squad_store::MemberEvidence {
    crate::squad_store::MemberEvidence::from_sets(
        std::collections::HashSet::new(),
        [attach.to_string()].into_iter().collect(),
    )
}

#[test]
fn dead_member_bound_to_a_pane_reads_orphaned() {
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 1, "deadbee1");
    let evidence = dead_evidence("deadbee1");
    assert!(
        core.orphaned_worker_for_pane(1, &[], &evidence).orphaned,
        "a Dead member's pane is orphaned"
    );
}

#[test]
fn a_live_registry_row_on_the_pane_beats_the_dead_evidence() {
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 1, "deadbee1");
    let evidence = dead_evidence("deadbee1");
    let mut row = exited_claude_row("harden-worker", None);
    row.mux = Some(("test".into(), 1));
    row.liveness = agents_view::Liveness::Alive;
    assert!(
        !core.orphaned_worker_for_pane(1, &[row], &evidence).orphaned,
        "a live row on the pane means the worker is not orphaned"
    );
}

#[test]
fn a_pane_with_no_member_binding_is_not_orphaned() {
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 1, "deadbee1");
    // Pane 2 hosts nothing: no member binds to it.
    let evidence = dead_evidence("deadbee1");
    assert!(
        !core.orphaned_worker_for_pane(2, &[], &evidence).orphaned,
        "no binding, no verdict"
    );
}

/// (x-688b) The name tier: a pane whose spawn captured a worker name the
/// registry join never resolved is fno's worker pane, and it is orphaned the
/// moment the shared fold judges that name dead. A held receipt (resumable
/// worker) and a nameless shell pane both stay kept.
#[test]
fn a_spawned_name_pane_is_orphaned_only_when_the_name_is_dead() {
    use crate::server::pane_identity::orphaned_by_spawned_name;
    let fold = |held: &[&str]| {
        let mut evidence = crate::squad_store::MemberEvidence::from_sets(
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        );
        evidence.fold_registry_rows(
            &[],
            ["t-688b-muxtabs".to_string()].into_iter().collect(),
            held.iter().map(|s| s.to_string()).collect(),
            true,
        );
        evidence
    };
    assert!(
        orphaned_by_spawned_name(Some("t-688b-muxtabs"), &fold(&[])),
        "a reaped spawned worker's pane closes as orphaned under default flags"
    );
    assert!(
        !orphaned_by_spawned_name(Some("t-688b-muxtabs"), &fold(&["t-688b-muxtabs"])),
        "a held receipt keeps the worker resumable and the pane kept"
    );
    assert!(
        !orphaned_by_spawned_name(None, &fold(&[])),
        "a shell pane with no spawned name never reaches the tier"
    );
}

#[test]
fn member_pane_reads_the_recorded_pane_id_first_and_falls_through_when_it_is_gone() {
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let recorded = core.spawn_pane(24, 80, "/a").unwrap();
    let joined = core.spawn_pane(24, 80, "/a").unwrap();
    core.worker_session_pane
        .insert(("codex".into(), "session-one".into()), joined);
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("t-worker".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("session-one".into()),
        pane_id: Some(recorded),
    };
    assert_eq!(
        core.member_pane(&member),
        Some(recorded),
        "a recorded live pane id wins over the derived join"
    );
    let gone = crate::squad_store::StoredMember {
        pane_id: Some(joined + 100),
        ..member.clone()
    };
    assert_eq!(
        core.member_pane(&gone),
        Some(joined),
        "a dead recorded id falls through to the member's own session join"
    );
    let stranger = crate::squad_store::StoredMember {
        harness_session_id: Some("session-two".into()),
        ..gone.clone()
    };
    assert_eq!(
        core.member_pane(&stranger),
        None,
        "a dead recorded id never lands on another worker's pane"
    );
}

/// (x-1b90) A throwaway receipts dir, same per-test isolation rule as
/// JournalScratch: never the operator's real reap receipts.
struct ReceiptsScratch(std::path::PathBuf);

impl ReceiptsScratch {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("fno-1b90-receipts-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn stage(&self, harness: &str, session_id: &str, transcript: &std::path::Path) {
        std::fs::write(
            self.0.join(format!("{harness}-{session_id}.json")),
            format!(
                r#"{{"native_locator":{{"transcripts":["{}"]}}}}"#,
                transcript.display()
            ),
        )
        .unwrap();
    }
}

impl Drop for ReceiptsScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// (x-1b90) AC2-HP: the release fires when the pane's name joined a resumable
/// reap marker, no live row carries the name, and the receipt's transcript
/// was last written BEFORE the reap. The release string names the harness,
/// the session id, the reap time and the basis. AC2-ERR in the same shape:
/// a transcript written AFTER the reap keeps the pane under not-pristine.
#[test]
fn reaped_pane_releases_only_when_nothing_wrote_after_the_reap() {
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(24, 80, "/a").unwrap();
    if let Some(entry) = core.panes.get_mut(&pid) {
        entry.name = Some("x-1b90-worker".into());
    }
    let evidence = crate::squad_store::MemberEvidence::from_sets(
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    );
    let marker = |ts: String| crate::spawn_journal::ReapedMarker {
        harness: "claude".into(),
        harness_session_id: "sess-1b90".into(),
        ts,
        basis: "every named node done: x-1b90".into(),
    };
    let receipts = ReceiptsScratch::new("release");
    let transcript = receipts.0.join("session.jsonl");
    std::fs::write(&transcript, "{}").unwrap();
    receipts.stage("claude", "sess-1b90", &transcript);
    // AC2-HP: the reap happened AFTER the last write. The transcript's mtime
    // is now; a 2027 stamp is a reap from the future relative to it.
    core.journal.reaped.insert(
        "x-1b90-worker".to_string(),
        marker("2027-01-01T00:00:00.000Z".into()),
    );
    let verdict = core.orphaned_worker_for_pane_in(pid, &[], &evidence, Some(&receipts.0));
    assert!(verdict.orphaned, "the reaped tier releases the pane");
    let release = verdict.release.expect("the release names the reap");
    assert!(
        release.contains("reaped claude sess-1b90 at ")
            && release.contains("every named node done: x-1b90"),
        "{release}"
    );

    // AC2-ERR: the transcript was written AFTER the reap (2020 stamp is
    // before the just-created transcript's mtime): the pane stays, and no
    // release is printed.
    core.journal.reaped.insert(
        "x-1b90-worker".to_string(),
        marker("2020-01-01T00:00:00.000Z".into()),
    );
    let verdict = core.orphaned_worker_for_pane_in(pid, &[], &evidence, Some(&receipts.0));
    assert!(
        !verdict.orphaned,
        "a write after the reap keeps the pane off the reaped tier"
    );
    assert!(verdict.release.is_none());
}

/// (x-1b90) AC2-EDGE: the name was spawned again after its reap, so the
/// recency guard dropped the marker and the reaped tier never fires. The
/// marker-only journal (spawn, reap) keeps the marker.
#[test]
fn a_respawned_name_never_reads_as_reaped() {
    let reap_is_last = crate::spawn_journal::parse_journal_events(&concat!(
        r#"{"ts":"2026-09-10T05:00:00.000Z","type":"agent_spawned","data":{"name":"w","provider":"codex","harness_session_id":"s1","substrate":"pane"}}"#,
        "\n",
        r#"{"ts":"2026-09-10T06:00:00.000Z","type":"agent_row_reaped","data":{"name":"w","harness":"codex","harness_session_id":"s1","basis":"every named node done: x-6208","resumable":true}}"#,
        "\n",
    ));
    assert!(
        reap_is_last.reaped.contains_key("w"),
        "the reap is the newest fact about w"
    );
    let events = crate::spawn_journal::parse_journal_events(&concat!(
        r#"{"ts":"2026-09-10T05:00:00.000Z","type":"agent_spawned","data":{"name":"w","provider":"codex","harness_session_id":"s1","substrate":"pane"}}"#,
        "\n",
        r#"{"ts":"2026-09-10T06:00:00.000Z","type":"agent_row_reaped","data":{"name":"w","harness":"codex","harness_session_id":"s1","basis":"every named node done: x-6208","resumable":true}}"#,
        "\n",
        r#"{"ts":"2026-09-10T07:00:00.000Z","type":"agent_spawned","data":{"name":"w","provider":"codex","harness_session_id":"s2","substrate":"pane"}}"#,
        "\n",
    ));
    // marker dropped by the recency guard
    assert!(!events.reaped.contains_key("w"));
}
