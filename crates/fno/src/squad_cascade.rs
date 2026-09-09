//! What the provenance cascade and the reap receipts say about squad
//! members. The prune's own evidence (live sets, journal markers) cannot
//! see a name-only or session-keyed member whose doors never wrote back;
//! this module asks the surviving sources and folds POSITIVE answers in.

use crate::squad_store::{MemberEvidence, MemberLiveness, StoredSquad};

/// Ask the provenance cascade about every name-only member still reading
/// Unknown, and arm the Unmeasured expiry. Only POSITIVE cascade answers
/// enter the evidence: a name resolved to a done, PR-confirmed node
/// becomes retire-eligible, and every failure (binary missing, unreadable
/// graph, timeout) changes nothing - fail-open to the historical Unknown.
/// A live name never asks: the caller folds live identities first.
pub fn fold_cascade_verdicts(mut evidence: MemberEvidence) -> MemberEvidence {
    let loaded = crate::squad_store::load();
    // The Unmeasured expiry rides the same fold: 24h of unmeasured silence
    // after the last recorded activity is the bound a stale-sideline row
    // gets; a fresher or never-active row stays fail-safe Unknown.
    evidence.set_unmeasured_expiry(
        crate::squad_store::now_epoch_secs().unwrap_or(0) as u64,
        24 * 3600,
    );
    let unknown_names = cascade_names(&evidence, &loaded.squads);
    let unknown_pairs = cascade_pairs(&evidence, &loaded.squads);
    if unknown_names.is_empty() && unknown_pairs.is_empty() {
        return evidence;
    }
    let mut command = std::process::Command::new(crate::digest_overlay::fno_agents_bin());
    command
        .args(["node-route", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if !unknown_names.is_empty() {
        command.args(["--names", &unknown_names.join(",")]);
    }
    if !unknown_pairs.is_empty() {
        let keys: Vec<String> = unknown_pairs
            .iter()
            .map(|(h, sid, _)| format!("{h}:{sid}"))
            .collect();
        command.args(["--pairs", &keys.join(",")]);
    }
    let Ok(mut child) = command.spawn() else {
        return evidence;
    };
    // Bounded wait: the cascade read is one graph read per call, so two
    // seconds is generous; a slower binary fails open to Unknown.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let stdout = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return evidence;
                }
                let mut text = String::new();
                use std::io::Read;
                let Some(mut pipe) = child.stdout.take() else {
                    return evidence;
                };
                if pipe.read_to_string(&mut text).is_err() {
                    return evidence;
                }
                break text;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                return evidence;
            }
        }
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&stdout)
    else {
        return evidence;
    };
    for (name, verdict) in &parsed {
        if verdict["state"] == "retire-eligible" {
            evidence.add_retire_eligible_name(name.clone());
        }
    }
    // A pair the transcript store reads fresh is LIVE, full stop: the
    // expiry's registry-age bound must never outvote the session's own
    // activity (the converse the cross-door property pins). The live pair
    // also lifts the member's worker NAME, so a name-only sibling sharing
    // it is kept by the same evidence.
    for (harness, sid, worker) in &unknown_pairs {
        let key = format!("{harness}:{sid}");
        if parsed.get(&key).and_then(|v| v.get("state")) == Some(&"live".into()) {
            evidence.add_live_pair(harness.clone(), sid.clone());
            if !worker.is_empty() {
                evidence.add_live(worker.clone());
            }
        }
    }
    evidence
}

/// The session-keyed members still reading Unknown, for the transcript
/// store to judge. Their registry row may be stale (the expiry would fold
/// the name), but freshness is a property of the SESSION, so the question
/// rides the pair, never the row.
fn cascade_pairs(
    evidence: &MemberEvidence,
    squads: &[StoredSquad],
) -> Vec<(String, String, String)> {
    let mut pairs: Vec<(String, String, String)> = Vec::new();
    for squad in squads {
        for member in &squad.members {
            if member.tombstone {
                continue;
            }
            let (Some(harness), Some(sid)) = (
                member.harness.as_deref(),
                member.harness_session_id.as_deref(),
            ) else {
                continue;
            };
            if harness.is_empty() || sid.is_empty() {
                continue;
            }
            if evidence.verdict(member) != MemberLiveness::Unknown {
                continue;
            }
            let worker = member.worker.clone().unwrap_or_default();
            if pairs.iter().any(|(h, s, _)| h == harness && s == sid) {
                continue;
            }
            pairs.push((harness.to_string(), sid.to_string(), worker));
        }
    }
    pairs
}

/// The name-only members still reading Unknown, for the cascade to judge.
/// A tombstoned or session-keyed member never asks.
fn cascade_names(evidence: &MemberEvidence, squads: &[StoredSquad]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for squad in squads {
        for member in &squad.members {
            if member.tombstone || member.harness_session_id.is_some() {
                continue;
            }
            let Some(worker) = member.worker.as_deref().filter(|w| !w.is_empty()) else {
                continue;
            };
            if evidence.verdict(member) != MemberLiveness::Unknown {
                continue;
            }
            if names.contains(&worker.to_string()) {
                continue;
            }
            names.push(worker.to_string());
        }
    }
    names
}

/// The receipts leg: a staged reap receipt is POSITIVE evidence the row
/// was removed, whatever door removed it. Its session id joins the dead
/// pairs, so a squad member keyed by a removed row's session id reads Dead
/// instead of Unknown forever - the join the roster and registry doors
/// never wrote back into the squad store.
pub fn fold_receipt_leg(evidence: &mut MemberEvidence, registry_path: &std::path::Path) {
    let receipts_dir = registry_path
        .parent()
        .map(|p| p.join("reap-receipts"))
        .unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(&receipts_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(raw) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&raw))
        else {
            continue;
        };
        let harness = value["harness"].as_str().unwrap_or_default().to_string();
        let sid = value["harness_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if harness.is_empty() || sid.is_empty() {
            continue;
        }
        evidence.add_dead_pair(&harness, &sid);
        if let Some(name) = value["row_name"].as_str() {
            evidence.add_dead(name);
        }
    }
}
