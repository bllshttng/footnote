//! Per-pane worker identity: the `fno_id` join, the shared member-evidence
//! fold the daemon sweeps consume, and the orphan verdict (v71, x-688b).

use super::*;

impl Core {
    /// The `fno_id` (durable session id) of the registry row hosting `pid` in
    /// this session, if any. The forward half of the identity join (Locked
    /// Decision 6); `PaneWhere` is the reverse.
    pub(super) fn fno_id_for_pane_with_agents(
        &self,
        pid: u64,
        agents: &[RegistryAgent],
    ) -> Option<String> {
        let mut ids = std::collections::BTreeSet::new();
        for a in agents {
            if let Some((sess, pane)) = &a.mux {
                if sess == &self.session_name && *pane == pid {
                    if let Some(identity) = a.effective_identity() {
                        ids.insert(identity.to_string());
                    }
                }
            }
        }
        // (x-b029) The resume birthright. A pane the daemon itself re-homed
        // (workspace restore, `pane run --worker`) is recorded in
        // `worker_session_pane` with its (harness, session id) at spawn - the
        // same id the resume argv carries. The registry FILE's row can still
        // point at the pre-restart pane, so the join above misses and the
        // pane read `-` while doing real work. This map is fno's own record,
        // not an argv heuristic: the id was stamped at birth by the code that
        // built the resume command.
        for ((_, session_id), pane) in &self.worker_session_pane {
            if *pane == pid {
                ids.insert(session_id.clone());
            }
        }
        if ids.is_empty() {
            ids.extend(crate::thread_viewer::identity_for_pane(
                &self.portals,
                pid,
                agents,
            ));
        }
        (ids.len() == 1).then(|| ids.into_iter().next()).flatten()
    }

    /// Fold the cached registry rows and the spawn journal into the same
    /// evidence the standalone workspace-prune verb computes - one shared
    /// fold (`fold_registry_rows`), so the daemon sweep and the CLI apply
    /// can never drift on what a row or a reaped name proves. Unknown rows
    /// contribute no verdict; only a positive `Alive` or `Dead` reading
    /// enters a set. The journal read is injected so tests never touch the
    /// operator's real events.jsonl.
    pub(super) fn member_evidence(&self) -> crate::squad_store::MemberEvidence {
        self.member_evidence_with_journal(&self.journal)
    }

    /// The path-injected core of [`Self::member_evidence`].
    pub(super) fn member_evidence_with_journal(
        &self,
        journal: &crate::spawn_journal::SpawnJournal,
    ) -> crate::squad_store::MemberEvidence {
        let mut evidence =
            crate::squad_store::MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        let held = crate::spawn_journal::held_worker_names(&journal.receipts);
        evidence.fold_registry_rows(
            &self.agents,
            journal.spawned_names.clone(),
            held,
            self.agents_read_ok,
        );
        for name in journal.never_bound.keys() {
            evidence.add_dead_name(name.clone());
        }
        evidence
    }

    /// The fail-closed send gate: `Some(refusal)` when the pane must not
    /// be typed into. An unreconciled pane (adopted at a fresh id
    /// because its birth id was taken) refuses outright; an unaddressed
    /// send to a labelled pane whose session id resolves to nothing
    /// refuses too. A pane with no label is an operator shell and is
    /// untouched.
    pub(super) fn pane_send_identity_gate(
        &self,
        pane: u64,
        label: Option<&str>,
        unreconciled: bool,
        expected_identity: Option<&str>,
        agents: Result<&[RegistryAgent], &'static str>,
    ) -> Option<ServerMsg> {
        // Fail closed: a pane whose identity did not reconcile is never typed
        // into blind - the number may name a different occupant than its label
        // claims. A mis-delivered send is worse than a refused one. A pane with
        // no label at all is an operator shell and is untouched.
        if unreconciled {
            let host = label.unwrap_or("<no label>");
            return Some(ServerMsg::Err {
                code: err_code::TARGET_IDENTITY_MISMATCH,
                msg: format!(
                    "pane {pane} carries label {host}; its birth pane id could not be reused, so it was adopted at a fresh id and its identity never reconciled; re-address by session id through `fno mux where`"
                ),
            });
        }
        if expected_identity.is_none() {
            if let (Some(host), Ok(rows)) = (label, agents) {
                if self.fno_id_for_pane_with_agents(pane, rows).is_none() {
                    return Some(ServerMsg::Err {
                        code: err_code::TARGET_IDENTITY_MISMATCH,
                        msg: format!(
                            "pane {pane} carries label {host} but no session id resolves for it; re-address by session id through `fno mux where`"
                        ),
                    });
                }
            }
        }
        None
    }

    pub(super) fn dead_sweep_count(&self) -> usize {
        let mut evidence = self.member_evidence();
        for entry in self.panes.values() {
            if let Some(worker) = &entry.refused_worker {
                evidence.add_dead(worker.clone());
            }
        }
        self.squad_members
            .values()
            .flatten()
            .filter(|member| {
                matches!(
                    evidence.verdict(member),
                    crate::squad_store::MemberLiveness::Dead
                )
            })
            .count()
    }

    /// (v71) The per-pane orphan verdict: `orphaned` is true when a stored
    /// member bound to `pid` (`member_pane`) is judged Dead by the evidence
    /// built from `agents` and the reap journal, and no registry row is live
    /// on this pane. A refused restore placeholder reads `true` too: the same
    /// category with an earlier marker. (x-688b) A spawned-name pane - the
    /// entry carries the worker name the spawn captured, `FNO_AGENT_SELF`,
    /// but the registry join never resolved an id - reads `true` when that
    /// name is positively dead: the pane is fno's worker pane, not an
    /// operator shell, and must not fall through to the used-shells opt-in
    /// bucket. The default prune closes such a tab; pristine stays the test
    /// for tabs that never hosted a worker.
    ///
    /// (x-1b90) The release tier, LAST: a pane whose name's newest journal
    /// fact is a resumable reap carries the marker that proves the work done
    /// (`release`). The receipt stays the resume path, so the tier holds
    /// unless the session wrote nothing after the reap - a transcript
    /// written at or after the reap `ts` is someone resuming it in place.
    pub(super) fn orphaned_worker_for_pane(
        &self,
        pid: u64,
        agents: &[RegistryAgent],
        evidence: &crate::squad_store::MemberEvidence,
    ) -> PaneOrphanVerdict {
        self.orphaned_worker_for_pane_in(
            pid,
            agents,
            evidence,
            default_reap_receipts_dir().as_deref(),
        )
    }

    /// The receipts-dir-injected core of [`Self::orphaned_worker_for_pane`],
    /// so a test stages its own reap receipts and never reads the ambient
    /// home.
    pub(super) fn orphaned_worker_for_pane_in(
        &self,
        pid: u64,
        agents: &[RegistryAgent],
        evidence: &crate::squad_store::MemberEvidence,
        receipts_dir: Option<&std::path::Path>,
    ) -> PaneOrphanVerdict {
        let live_row_on_pane = agents.iter().any(|a| {
            a.mux
                .as_ref()
                .is_some_and(|(sess, pane)| sess == &self.session_name && *pane == pid)
                && a.liveness == agents_view::Liveness::Alive
        });
        if live_row_on_pane {
            return PaneOrphanVerdict::kept();
        }
        let entry = self.panes.get(&pid);
        if entry
            .and_then(|entry| entry.refused_worker.as_ref())
            .is_some()
        {
            return PaneOrphanVerdict {
                orphaned: true,
                release: None,
            };
        }
        if orphaned_by_spawned_name(entry.and_then(|entry| entry.name.as_deref()), evidence) {
            return PaneOrphanVerdict {
                orphaned: true,
                release: None,
            };
        }
        if self.squad_members.values().flatten().any(|member| {
            self.member_pane(member) == Some(pid)
                && matches!(
                    evidence.verdict(member),
                    crate::squad_store::MemberLiveness::Dead
                )
        }) {
            return PaneOrphanVerdict {
                orphaned: true,
                release: None,
            };
        }
        // (x-1b90) The reaped tier: name joined to a resumable reap marker
        // in the session's own journal, no live registry row carrying that
        // name, and no transcript write after the reap. The markers live on
        // the journal, never on the evidence: the receipt stays the resume
        // path, and only the pane release reads the newer fact.
        let reaped_name = entry
            .and_then(|entry| entry.name.as_deref())
            .filter(|name| !name.is_empty())
            .filter(|name| {
                !agents
                    .iter()
                    .any(|a| a.name == *name && a.liveness == agents_view::Liveness::Alive)
            })
            .filter(|name| self.journal.reaped.contains_key(*name));
        if let Some(marker) = reaped_name.and_then(|name| self.journal.reaped.get(name)) {
            if let Some(release) = reaped_release_for(marker, receipts_dir) {
                return PaneOrphanVerdict {
                    orphaned: true,
                    release: Some(release),
                };
            }
        }
        PaneOrphanVerdict::kept()
    }
}

/// (x-688b) The name tier, pure so it is unit-testable without a live pty:
/// a pane entry carrying a spawned worker name that the shared fold judged
/// dead (reaped row, exited row, or never-bound marker - each reuse-guarded
/// upstream) is an orphaned worker pane. `None`/empty is a shell pane: no
/// name, no verdict.
pub(super) fn orphaned_by_spawned_name(
    name: Option<&str>,
    evidence: &crate::squad_store::MemberEvidence,
) -> bool {
    name.is_some_and(|n| !n.is_empty() && evidence.is_dead_name(n))
}

/// (x-1b90) The per-pane orphan verdict plus its release evidence. `release`
/// is `Some` only when the release tier fired: the pane's name joined to a
/// resumable reap marker AND the session wrote nothing after the reap. A tab
/// that closes on the tier prints the release in the prune receipt, so the
/// reason names the harness, the session id, the reap time and the basis.
pub(super) struct PaneOrphanVerdict {
    pub(super) orphaned: bool,
    pub(super) release: Option<String>,
}

impl PaneOrphanVerdict {
    fn kept() -> Self {
        Self {
            orphaned: false,
            release: None,
        }
    }
}

/// (x-1b90) The release proof for one reap marker: the reap receipt
/// `reap-receipts/<harness>-<session id>.json` is read back, and every
/// EXISTING transcript it located must carry an mtime older than the reap
/// `ts`. A write at or after the reap is someone resuming the session in
/// place, and the pane stays. No readable receipt, no existing transcript,
/// or an unparseable stamp also keeps the pane: absence is not quiet. The
/// receipts dir is a parameter so a test stages its own.
pub(super) fn reaped_release_for(
    marker: &crate::spawn_journal::ReapedMarker,
    receipts_dir: Option<&std::path::Path>,
) -> Option<String> {
    let harness = marker.harness.as_str();
    let session_id = marker.harness_session_id.as_str();
    if harness.is_empty() || session_id.is_empty() {
        return None;
    }
    let reap_ts = crate::agents_view::ts_to_secs(&marker.ts)?;
    let dir = receipts_dir?;
    let receipt_path = dir.join(format!("{harness}-{session_id}.json"));
    let raw = std::fs::read_to_string(receipt_path).ok()?;
    let receipt: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let transcripts = receipt
        .get("native_locator")?
        .get("transcripts")?
        .as_array()?;
    let mut existing = 0;
    for transcript in transcripts.iter().filter_map(|t| t.as_str()) {
        let Ok(meta) = std::fs::metadata(transcript) else {
            continue;
        };
        let mtime = meta.modified().ok()?;
        let mtime_secs = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        existing += 1;
        if mtime_secs >= reap_ts {
            // Wrote at or after the reap: resumed in place. The pane stays.
            return None;
        }
    }
    if existing == 0 {
        // Absence is not quiet: no transcript to compare is no proof.
        return None;
    }
    Some(format!(
        "reaped {harness} {session_id} at {}: {}",
        marker.ts, marker.basis
    ))
}

/// The agents home's reap-receipt directory, resolved off the same registry
/// path the spawn journal reads. `None` when the home cannot be resolved.
pub(super) fn default_reap_receipts_dir() -> Option<std::path::PathBuf> {
    crate::agents_view::registry_path()
        .parent()
        .map(|home| home.join("reap-receipts"))
}
