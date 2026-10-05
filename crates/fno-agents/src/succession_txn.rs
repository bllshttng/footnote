//! Succession as one transaction. Every step writes one event row sharing
//! the pending record's `succession_id` (`<ts>:<scope>`), so the journal
//! answers "did this handoff complete?" for any harness without a human
//! re-deriving the steps. The registry commit in `team_settle` stays the
//! authority: a failed announce or retro never undoes the transfer.

use crate::team_names::{PendingSuccession, RevertedSuccession};
use serde_json::json;
use std::path::Path;
use std::process::Stdio;

/// One succession's journal key: the pending record's ts plus the scope.
fn succession_id(pending: &PendingSuccession, scope: &str) -> String {
    format!("{}:{scope}", pending.ts)
}

fn emitter() -> Option<crate::events::EventEmitter> {
    crate::paths::AgentsHome::from_env_opt()
        .map(|home| crate::events::EventEmitter::new(home.events_jsonl(), "agents"))
}

/// The announce step: one fleet announcement from the heir's name, then one
/// `team_succession_announced` (or `_failed`) row. A failed announce prints
/// one stderr line; it never undoes the transfer.
pub(crate) fn announce(scope: &str, pending: &PendingSuccession) {
    let heir_session = pending
        .heir_session
        .as_deref()
        .unwrap_or("session pending its first beat");
    let body = format!(
        "Succession over {scope}: {} hands the crown to {} ({heir_session}). The transfer is committed; the heir's first beat verifies it.",
        pending.predecessor_name, pending.heir_name
    );
    let sent =
        crate::announce::announce_all(&pending.heir_name, &format!("succession: {scope}"), &body);
    let payload = json!({
        "succession_id": succession_id(pending, scope),
        "scope": scope,
        "heir_name": pending.heir_name,
        "predecessor_name": pending.predecessor_name,
    });
    match sent {
        Ok(_) => {
            if let Some(events) = emitter() {
                let _ = events.emit("team_succession_announced", &payload);
            }
        }
        Err(e) => {
            eprintln!("succession: announce failed: {e}");
            if let Some(events) = emitter() {
                let _ = events.emit("team_succession_announce_failed", &payload);
            }
        }
    }
}

/// The transfer step's receipt. The commit itself stays in `team_settle`'s
/// `carry_succession`; this row is the journal answer to "was it handed".
pub(crate) fn transferred(scope: &str, pending: &PendingSuccession) {
    if let Some(events) = emitter() {
        let _ = events.emit(
            "team_succession_transferred",
            &json!({
                "succession_id": succession_id(pending, scope),
                "scope": scope,
                "heir_name": pending.heir_name,
                "predecessor_name": pending.predecessor_name,
            }),
        );
    }
}

/// Verify, release, retro, from the heir's first beat: `cleared` is the
/// pending record `bind_and_refresh` just took. The release readback reads
/// the live registry rows: a predecessor still seated over the scope names
/// itself in `team_succession_release_unproven`. The retro launches the
/// intel writer detached for the predecessor's session and names its eval
/// dir; a predecessor with no session id, or no resolvable eval dir, reads
/// `team_succession_retro_unmeasured`.
pub(crate) fn verified(scope: &str, cwd: &Path, cleared: &PendingSuccession) {
    let id = succession_id(cleared, scope);
    if let Some(events) = emitter() {
        let _ = events.emit(
            "team_succession_verified",
            &json!({
                "succession_id": id,
                "scope": scope,
                "heir_name": cleared.heir_name,
                "predecessor_name": cleared.predecessor_name,
            }),
        );
    }
    // Release readback: crown.py vacates the predecessor at spawn; this is
    // the readback proving it happened. No live row over the scope, or a
    // seated session that is not the predecessor's, is the release proof.
    let canon = crate::territory::canonical_scope(scope);
    let registry_path = crate::paths::AgentsHome::from_env().registry_json();
    match crate::team_names::live_index(&registry_path) {
        Err(e) => release_unproven(scope, &id, &format!("registry unreadable: {e}")),
        Ok(live) => match live.get(&canon).and_then(|t| t.holder_session.clone()) {
            Some(holder) if Some(&holder) == cleared.predecessor_session.as_ref() => {
                release_unproven(scope, &id, &format!("{scope} still held by {holder}"))
            }
            _ => {
                if let Some(events) = emitter() {
                    let _ = events.emit(
                        "team_succession_released",
                        &json!({
                            "succession_id": id,
                            "scope": scope,
                            "heir_name": cleared.heir_name,
                        }),
                    );
                }
            }
        },
    }
    retro(scope, cwd, cleared, &id);
}

fn release_unproven(scope: &str, id: &str, reason: &str) {
    if let Some(events) = emitter() {
        let _ = events.emit(
            "team_succession_release_unproven",
            &json!({
                "succession_id": id,
                "scope": scope,
                "reason": reason,
            }),
        );
    }
}

/// The retro step: the intel writer for the predecessor's session, detached,
/// stdout and stderr into the agents home, writing the default eval dir the
/// lead used to file by hand.
fn retro(scope: &str, cwd: &Path, cleared: &PendingSuccession, id: &str) {
    let Some(session) = cleared
        .predecessor_session
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
    else {
        retro_unmeasured(scope, id, "no predecessor session id");
        return;
    };
    let Some(plans) = crate::plans_path::plans_content_dir(cwd) else {
        retro_unmeasured(scope, id, "no resolvable plans dir");
        return;
    };
    let short = session.get(..8).unwrap_or(&session);
    // The same default dir shape lead_eval's --write resolves: the scope's
    // first tag under the evals' kings dir, so the hand-filed and the
    // auto-filed retro land in one place.
    let tag = scope
        .split(',')
        .next()
        .unwrap_or(scope)
        .trim()
        .rsplit('-')
        .next()
        .unwrap_or(scope);
    let dir = plans
        .join("..")
        .join("evals")
        .join("kings")
        .join(format!("lead-{tag}-{short}"));
    let spawned = std::env::current_exe().ok().and_then(|exe| {
        let home = crate::paths::AgentsHome::from_env_opt()?;
        let log_path = home.root().join(format!("succession-retro-{short}.log"));
        let log = std::fs::File::create(&log_path).ok()?;
        let stderr = log.try_clone().ok()?;
        std::process::Command::new(exe)
            .args(["intel", "--windows", "--session", &session, "--write"])
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(stderr)
            .spawn()
            .ok()
    });
    if spawned.is_none() {
        eprintln!("succession: retro writer did not launch for session {session}");
    }
    if let Some(events) = emitter() {
        let _ = events.emit(
            "team_succession_retro_filed",
            &json!({
                "succession_id": id,
                "scope": scope,
                "session": session,
                "dir": dir.display().to_string(),
            }),
        );
    }
}

fn retro_unmeasured(scope: &str, id: &str, reason: &str) {
    if let Some(events) = emitter() {
        let _ = events.emit(
            "team_succession_retro_unmeasured",
            &json!({
                "succession_id": id,
                "scope": scope,
                "reason": reason,
            }),
        );
    }
}

/// The rollback step's announcement, from the predecessor's name: the heir
/// never bound, the reap sweep restored the predecessor, and the fleet
/// reads one line saying the crown moved back.
pub(crate) fn rolled_back(reverted: &RevertedSuccession) {
    let body = format!(
        "Succession over {} rolled back: {} never bound within the window; {} resumes the crown.",
        reverted.scope, reverted.heir_name, reverted.predecessor_name,
    );
    let from = if reverted.predecessor_name.is_empty() {
        &reverted.heir_name
    } else {
        &reverted.predecessor_name
    };
    let sent = crate::announce::announce_all(
        from,
        &format!("succession reverted: {}", reverted.scope),
        &body,
    );
    let payload = json!({
        "scope": reverted.scope,
        "heir_name": reverted.heir_name,
        "predecessor_name": reverted.predecessor_name,
        "evidence": reverted.evidence,
    });
    match sent {
        Ok(_) => {
            if let Some(events) = emitter() {
                let _ = events.emit("team_succession_announced", &payload);
            }
        }
        Err(e) => {
            eprintln!("succession: rollback announce failed: {e}");
            if let Some(events) = emitter() {
                let _ = events.emit("team_succession_announce_failed", &payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingSuccession {
        PendingSuccession {
            heir_name: "lead-heir".into(),
            heir_session: Some("sess-new".into()),
            predecessor_name: "lead-old".into(),
            predecessor_session: Some("sess-old".into()),
            ts: "2026-10-04T00:00:00Z".into(),
        }
    }

    /// Pin a private agents home so the transaction's event rows land where
    /// the test reads them. Holds the env lock for the test's whole scope.
    fn pin_txn_home() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        let lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("home")).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", home.path().join("home"));
        (lock, home)
    }

    fn registry_seeded(dir: &std::path::Path, session: &str) {
        std::fs::write(
            dir.join("home/registry.json"),
            serde_json::to_string(&json!({
                "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                "agents": [{
                    "name": session, "status": "live", "crown_scope": "x-aaaa",
                    "crown_level": 2, "cwd": "/repo", "harness": "claude",
                    "harness_session_id": session,
                    "created_at": "2026-10-04T00:00:00Z",
                }],
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn read_events_raw(home: &std::path::Path) -> String {
        let journal = home.join("home/events.jsonl");
        if let Ok(rows) = crate::event_store::query_events(&journal, &Default::default()) {
            return rows
                .iter()
                .map(|r| r.line.clone())
                .collect::<Vec<_>>()
                .join("\n");
        }
        std::fs::read_to_string(&journal).unwrap_or_default()
    }

    /// AC3-HP: the heir's first beat verifies, releases and files the retro
    /// once each under one succession id; the release readback passes when
    /// the heir (not the predecessor) holds the scope.
    #[test]
    fn the_heirs_beat_verifies_releases_and_reports_the_retro_once() {
        let (_guard, home) = pin_txn_home();
        registry_seeded(home.path(), "sess-new");
        // The plans chain falls back to the space dir, so the filed branch
        // is the deterministic one: the retro names its eval dir.
        verified("x-aaaa", home.path(), &pending());
        let raw = read_events_raw(home.path());
        assert_eq!(raw.matches("team_succession_verified").count(), 1, "{raw}");
        assert_eq!(raw.matches("team_succession_released").count(), 1, "{raw}");
        assert_eq!(
            raw.matches("team_succession_retro_filed").count(),
            1,
            "{raw}"
        );
        assert!(raw.contains("2026-10-04T00:00:00Z:x-aaaa"), "{raw}");
        // A predecessor with no session id reads unmeasured, never a launch.
        let mut nopred = pending();
        nopred.predecessor_session = None;
        verified("x-aaaa", home.path(), &nopred);
        let raw = read_events_raw(home.path());
        assert_eq!(
            raw.matches("team_succession_retro_unmeasured").count(),
            1,
            "{raw}"
        );
    }

    /// AC3-EDGE: a predecessor row still live over the scope names itself in
    /// the unproven row.
    #[test]
    fn a_predecessor_still_seated_reads_release_unproven() {
        let (_guard, home) = pin_txn_home();
        registry_seeded(home.path(), "sess-old");
        verified("x-aaaa", home.path(), &pending());
        let raw = read_events_raw(home.path());
        assert_eq!(
            raw.matches("team_succession_release_unproven").count(),
            1,
            "{raw}"
        );
        assert!(raw.contains("sess-old"), "{raw}");
    }
}
