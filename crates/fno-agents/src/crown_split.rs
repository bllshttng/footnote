//! Two rows on one territory, and which kind of two.
//!
//! `double_ruled` is an authority finding: more than one NON-TERMINAL row
//! carries the same territory key. For a single-member scope that is the
//! same condition `crown_settle::resolve` refuses a grant on. Settle
//! compares the raw scope string, so a comma re-spelling or an alias name
//! can pass the grant door yet read as double-ruled here. That gap is
//! named, not fixed.
//!
//! `stale` is a board finding: a TERMINAL row still carrying crown fields.
//! Nothing clears a crown when a row goes terminal, so these accumulate and
//! are what a reader mistakes for two kings. Separate word, separate remedy.

use crate::state::RegistryEntry;
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
pub struct ScopeSplit {
    pub scope: String,
    pub holders: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct StaleCrown {
    pub row: String,
    pub scope: String,
    pub stored_status: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CrownSplits {
    pub double_ruled: Vec<ScopeSplit>,
    pub stale: Vec<StaleCrown>,
}

/// Group crowned rows by normalized territory key over ALL rows, live and
/// terminal. The court's own conflict scan filters to stored-live rows, so
/// a terminal crowned row is invisible there by construction; this reader
/// reads the registry directly for that reason. Same territory key is
/// deliberately narrower than the court's overlap rule: a live level-1
/// crown over a project and a live level-2 crown over one node never pair,
/// which is the legitimate portfolio-and-court arrangement.
pub(crate) fn read_crown_splits(rows: &[RegistryEntry]) -> CrownSplits {
    let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stale: Vec<StaleCrown> = Vec::new();
    for row in rows {
        let Some(scope) = row
            .crown_scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let key = crate::loop_reign::territory_key(scope);
        if key.is_empty() {
            continue;
        }
        if crate::loop_reign::is_terminal(row) {
            // The stored word, exactly as the registry serializes it, so the
            // anomaly line and a court JSON read compare without a
            // translation table.
            let stored_status = serde_json::to_value(row.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            stale.push(StaleCrown {
                row: row.name.clone(),
                scope: key,
                stored_status,
            });
        } else {
            live.entry(key).or_default().push(row.name.clone());
        }
    }
    // BTreeMap iteration is key-sorted, so the output order is a function of
    // the registry alone and two beats over an unchanged registry render
    // identically.
    let double_ruled = live
        .into_iter()
        .filter(|(_, holders)| holders.len() > 1)
        .map(|(scope, mut holders)| {
            holders.sort();
            ScopeSplit { scope, holders }
        })
        .collect();
    stale.sort_by(|a, b| (&a.scope, &a.row).cmp(&(&b.scope, &b.row)));
    CrownSplits {
        double_ruled,
        stale,
    }
}

/// Why one stale crowned row is down, read from its transcript tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadCallReading {
    /// The newest tool call has its result, or the tail holds none.
    Clear,
    /// The newest tool call has no result. `boot` is set only when the call
    /// predates the host boot.
    Open {
        session_id: String,
        tool: String,
        at: String,
        boot: Option<String>,
    },
    /// No reading; the reason is printed.
    Unread(String),
}

/// The tail window for the dead-call reading: far past the few records a
/// tool-call turn spans, and a bound on the read of a huge transcript.
pub(crate) const DEAD_CALL_TAIL_BYTES: u64 = 1 << 20;

pub(crate) fn dead_call(row: &RegistryEntry, boot_ms: Option<i64>) -> DeadCallReading {
    dead_call_in(&crate::claude_drive::claude_projects_dir(), row, boot_ms)
}

fn dead_call_in(
    projects: &std::path::Path,
    row: &RegistryEntry,
    boot_ms: Option<i64>,
) -> DeadCallReading {
    let harness = row.harness_name();
    if harness != "claude" {
        return DeadCallReading::Unread(format!("no tool-call reader for harness {harness}"));
    }
    let Some(sid) = row.harness_session_id.as_deref().filter(|s| !s.is_empty()) else {
        return DeadCallReading::Unread("row carries no session id".to_string());
    };
    let Some(path) = crate::claude_drive::find_transcript_in(projects, sid) else {
        return DeadCallReading::Unread(format!("no transcript for session {sid}"));
    };
    let tail = crate::tail_text(&path, DEAD_CALL_TAIL_BYTES);
    if tail.is_empty() {
        return DeadCallReading::Unread("transcript tail unreadable".to_string());
    }
    match crate::interrupt_classify::trailing_open_call(&tail) {
        None => DeadCallReading::Clear,
        Some(call) => {
            let at = call
                .at
                .clone()
                .unwrap_or_else(|| "an unrecorded time".to_string());
            let boot = boot_ms.and_then(|ms| {
                call.at
                    .as_deref()
                    .and_then(|a| chrono::DateTime::parse_from_rfc3339(a).ok())
                    .filter(|t| t.timestamp_millis() < ms)
                    .and_then(|_| chrono::DateTime::from_timestamp_millis(ms))
                    .map(|b| b.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            });
            DeadCallReading::Open {
                session_id: sid.to_string(),
                tool: call.name,
                at,
                boot,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentStatus;

    fn row(name: &str, scope: Option<&str>, status: AgentStatus) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            crown_scope: scope.map(str::to_string),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn two_live_rows_on_one_scope_are_one_double_rule() {
        let rows = [
            row("king-b", Some("shared"), AgentStatus::Busy),
            row("king-a", Some("shared"), AgentStatus::Live),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.stale.is_empty());
        assert_eq!(out.double_ruled.len(), 1);
        assert_eq!(out.double_ruled[0].scope, "shared");
        assert_eq!(out.double_ruled[0].holders, vec!["king-a", "king-b"]);
    }

    #[test]
    fn a_terminal_row_is_stale_never_double_ruled() {
        let rows = [
            row("king-live", Some("shared"), AgentStatus::Live),
            row("king-dead", Some("shared"), AgentStatus::Orphaned),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.double_ruled.is_empty());
        assert_eq!(out.stale.len(), 1);
        assert_eq!(out.stale[0].row, "king-dead");
        assert_eq!(out.stale[0].scope, "shared");
        assert_eq!(out.stale[0].stored_status, "orphaned");
    }

    #[test]
    fn comma_members_normalize_to_one_territory() {
        let rows = [
            row("one", Some("alpha,beta"), AgentStatus::Busy),
            row("two", Some("beta, alpha"), AgentStatus::Busy),
        ];
        let out = read_crown_splits(&rows);
        assert_eq!(out.double_ruled.len(), 1);
        assert_eq!(out.double_ruled[0].scope, "alpha,beta");
        assert_eq!(out.double_ruled[0].holders, vec!["one", "two"]);
    }

    #[test]
    fn no_crowned_row_reads_empty_on_both_sides() {
        let rows = [
            row("plain", None, AgentStatus::Busy),
            row("blank", Some("  "), AgentStatus::Live),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.double_ruled.is_empty());
        assert!(out.stale.is_empty());
    }

    #[test]
    fn two_terminal_rows_still_are_not_a_double_rule() {
        let rows = [
            row("dead-1", Some("shared"), AgentStatus::Exited),
            row("dead-2", Some("shared"), AgentStatus::PermanentDead),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.double_ruled.is_empty());
        assert_eq!(out.stale.len(), 2);
    }

    fn claude_row(name: &str, sid: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            crown_scope: Some("fno".to_string()),
            status: AgentStatus::Exited,
            harness: Some("claude".to_string()),
            harness_session_id: Some(sid.to_string()),
            ..Default::default()
        }
    }

    fn plant_open_tail(dir: &std::path::Path, sid: &str, answered: bool) {
        let project = dir.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let call = r#"{"type":"assistant","timestamp":"2026-09-21T08:21:13.913Z","message":{"content":[{"type":"tool_use","id":"toolu_01DRy8JKGCBLeubeGxap9SwP","name":"Bash","input":{}}]}}"#;
        let queue = r#"{"type":"user","timestamp":"2026-09-21T08:25:22.043Z","message":{"content":[{"type":"text","content":"queue-operation enqueue"}]}}"#;
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01DRy8JKGCBLeubeGxap9SwP","content":"ok"}]}}"#;
        let body = if answered {
            format!("{call}\n{result}\n{queue}\n")
        } else {
            format!("{call}\n{queue}\n")
        };
        std::fs::write(project.join(format!("{sid}.jsonl")), body).unwrap();
    }

    // AC4-HP shape: an open newest call reads Open and stamps the boot when
    // the call predates it.
    #[test]
    fn dead_call_reads_an_open_call_and_stamps_the_boot() {
        let dir = std::env::temp_dir().join(format!("cs-dc-a-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "278c9a89-11ed-49af-a6fb-371bb36e410d";
        plant_open_tail(&dir, sid, false);
        let reading = dead_call_in(&dir, &claude_row("king-fno-g6", sid), Some(1789997638000));
        match reading {
            DeadCallReading::Open {
                session_id,
                tool,
                at,
                boot,
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(tool, "Bash");
                assert_eq!(at, "2026-09-21T08:21:13.913Z");
                assert_eq!(boot.as_deref(), Some("2026-09-21T13:33:58Z"));
            }
            other => panic!("expected Open, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // Unread paths: a non-claude harness, and a claude row with no transcript.
    #[test]
    fn dead_call_is_unread_off_the_claude_path() {
        let row = RegistryEntry {
            name: "codex-king".to_string(),
            crown_scope: Some("fno".to_string()),
            status: AgentStatus::Exited,
            harness: Some("codex".to_string()),
            harness_session_id: Some("deadbeef-dead-4ead-8ead-deadbeefdead".to_string()),
            ..Default::default()
        };
        let reading = dead_call_in(&std::env::temp_dir(), &row, None);
        assert_eq!(
            reading,
            DeadCallReading::Unread("no tool-call reader for harness codex".to_string())
        );
    }

    #[test]
    fn dead_call_is_unread_without_a_transcript_or_id() {
        let dir = std::env::temp_dir().join(format!("cs-dc-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let row = claude_row("king-c", "11111111-2222-4333-8444-555555555555");
        let no_id = claude_row("king-nosid", "");
        let matches = |r: &DeadCallReading, want: &str| {
            assert!(
                matches!(r, DeadCallReading::Unread(m) if m == want),
                "got {r:?}"
            );
        };
        matches(
            &dead_call_in(&dir, &row, None),
            "no transcript for session 11111111-2222-4333-8444-555555555555",
        );
        matches(
            &dead_call_in(&dir, &no_id, None),
            "row carries no session id",
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // AC4-EDGE shape: an answered newest call reads Clear.
    #[test]
    fn dead_call_is_clear_when_the_newest_call_is_answered() {
        let dir = std::env::temp_dir().join(format!("cs-dc-d-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        plant_open_tail(&dir, sid, true);
        let reading = dead_call_in(&dir, &claude_row("king-clear", sid), None);
        assert_eq!(reading, DeadCallReading::Clear);
        std::fs::remove_dir_all(&dir).ok();
    }
}
