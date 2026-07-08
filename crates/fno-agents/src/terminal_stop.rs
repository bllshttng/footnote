//! Terminal-stop markers for fire-and-forget `claude --bg` workers (x-fcbf).
//!
//! A fire-and-forget bg `/target`/`/think` worker reaches a terminal loop
//! decision, and the stop hook returns exit-0 = allow: that ends the agent's
//! *turn*, not the *process*. An interactive `claude --bg` session then parks
//! at its idle prompt, alive, holding a slot against `config.agents.max_live`
//! forever. They pile up (observed leaking to 34) until the spawn gate wedges.
//!
//! `finalize` cannot self-exit the worker: it runs as a *child* of that very
//! `claude --bg` process (stop hook -> loop-check -> finalize), so a control.sock
//! self-quit would be a process exiting its own parent mid-turn. The fix is a
//! two-party handoff over the filesystem:
//!
//! 1. `finalize` (in the worker) drops a marker keyed by the claude session
//!    uuid, gated to footnote-SPAWNED (`FNO_AGENT_SELF` set), non-loop-driven
//!    (`FNO_DRIVER_LIB` unset) sessions. An operator's own terminal `/target`
//!    (no `FNO_AGENT_SELF`) and a `fno-agents loop run` child (the driver owns
//!    its lifecycle) are never marked, so they stay parked.
//! 2. The daemon sweep (external to every worker, so it *can* stop them) reads
//!    the markers each tick and runs the shipped `claude stop <short>`. A clean
//!    settle is never Claude-daemon-respawned; roster-presence itself excludes
//!    owned-PTY panes and operator foreground terminals, which are never
//!    `claude --bg` daemon jobs.
//!
//! Everything I/O-touching lives in thin helpers; the two decisions are pure and
//! unit-tested here (mirrors `gc.rs`: one decision, two triggers).

use crate::paths::AgentsHome;
use std::path::{Path, PathBuf};

/// One parsed terminal-stop marker: the claude session uuid to stop and the
/// terminal reason that produced it (recorded for the sweep's event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub uuid: String,
    pub reason: String,
}

/// What the sweep should do with one marker this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopAction {
    /// The session is still on the roster: `claude stop` the carried short id,
    /// then remove the marker on success.
    Stop(String),
    /// The session is already gone from the roster (exited on its own, or a
    /// prior tick stopped it): drop the stale marker, no stop needed.
    RemoveStale,
}

/// Decide whether `finalize` should drop a terminal-stop marker. Pure.
///
/// Returns the uuid to mark when ALL hold: the session is a footnote-spawned
/// worker (`agent_self`), it is NOT loop-run-driven (`!driver_lib`), and the
/// manifest carries a syntactically-valid claude session uuid. A `None`/empty/
/// separator-bearing uuid is rejected so a malformed manifest can never steer
/// the marker write outside the marker dir.
pub fn should_mark(agent_self: bool, driver_lib: bool, uuid: Option<&str>) -> Option<&str> {
    if !agent_self || driver_lib {
        return None;
    }
    let uuid = uuid?;
    if is_valid_uuid(uuid) {
        Some(uuid)
    } else {
        None
    }
}

/// Decide the sweep action for a marker given the roster lookup result: the
/// resolved short id if the session is still on the roster, else `None`. Pure.
/// Carrying the short id in `Stop` lets the caller skip re-deriving it (no
/// "should never happen" unwrap).
pub fn stop_decision(short: Option<String>) -> StopAction {
    match short {
        Some(s) => StopAction::Stop(s),
        None => StopAction::RemoveStale,
    }
}

/// A claude session id / short-id is hex and dashes only. Rejecting anything
/// else keeps the marker filename inside the marker dir (no `/`, `..`, NUL).
fn is_valid_uuid(uuid: &str) -> bool {
    !uuid.is_empty() && uuid.len() <= 64 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Write a marker file named by `uuid`, content = `reason`. Best-effort: the
/// caller logs and moves on. Creates the marker dir if absent. Revalidates the
/// uuid at this effect boundary so the write is self-defending, not reliant on
/// the caller having run `should_mark` first.
pub fn write_marker(home: &AgentsHome, uuid: &str, reason: &str) -> std::io::Result<PathBuf> {
    if !is_valid_uuid(uuid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to write terminal-stop marker for invalid uuid: {uuid:?}"),
        ));
    }
    let dir = home.terminal_stop_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(uuid);
    std::fs::write(&path, reason.as_bytes())?;
    Ok(path)
}

/// Read every marker in the dir. A file whose name is not a valid uuid is
/// skipped (never let a stray file steer a `claude stop`). Missing dir -> empty.
pub fn read_markers(home: &AgentsHome) -> Vec<Marker> {
    read_markers_in(&home.terminal_stop_dir())
}

fn read_markers_in(dir: &Path) -> Vec<Marker> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let uuid = match name.to_str() {
            Some(s) if is_valid_uuid(s) => s.to_string(),
            _ => continue,
        };
        let reason = std::fs::read_to_string(entry.path())
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(Marker { uuid, reason });
    }
    // Deterministic order so the sweep + tests are stable.
    out.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    out
}

/// Remove a consumed / stale marker. Best-effort; a missing file is not an error.
/// An unexpected failure (e.g. permissions) is logged, not silent: the marker
/// would otherwise persist and the sweep would re-visit it every tick with no
/// hint why.
pub fn remove_marker(home: &AgentsHome, uuid: &str) {
    if !is_valid_uuid(uuid) {
        return;
    }
    if let Err(e) = std::fs::remove_file(home.terminal_stop_dir().join(uuid)) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("terminal_stop: failed to remove marker {uuid}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "c79bce5b-1682-4a8c-97f4-d6254cf18112";

    #[test]
    fn marks_spawned_non_driven_session() {
        assert_eq!(should_mark(true, false, Some(UUID)), Some(UUID));
    }

    #[test]
    fn operator_terminal_not_marked() {
        // No FNO_AGENT_SELF: an operator's own /target must stay parked (AC3-UI).
        assert_eq!(should_mark(false, false, Some(UUID)), None);
    }

    #[test]
    fn loop_run_driven_not_marked() {
        // FNO_DRIVER_LIB set: the loop-run driver owns lifecycle (AC6-FR).
        assert_eq!(should_mark(true, true, Some(UUID)), None);
    }

    #[test]
    fn missing_uuid_not_marked() {
        assert_eq!(should_mark(true, false, None), None);
        assert_eq!(should_mark(true, false, Some("")), None);
    }

    #[test]
    fn separator_bearing_uuid_rejected() {
        // A malformed manifest must never write outside the marker dir.
        assert_eq!(should_mark(true, false, Some("../../etc/passwd")), None);
        assert_eq!(should_mark(true, false, Some("a/b")), None);
    }

    #[test]
    fn roster_present_stops_absent_removes() {
        assert_eq!(
            stop_decision(Some("c79bce5b".to_string())),
            StopAction::Stop("c79bce5b".to_string())
        );
        assert_eq!(stop_decision(None), StopAction::RemoveStale);
    }

    #[test]
    fn write_marker_rejects_invalid_uuid() {
        // The effect boundary is self-defending, not reliant on the caller.
        let home = AgentsHome::at(std::env::temp_dir().join("fno-ts-invalid-xyz"));
        assert!(write_marker(&home, "../../etc/passwd", "DonePRGreen").is_err());
    }

    #[test]
    fn write_read_remove_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("fno-ts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let home = AgentsHome::at(&tmp);

        let p = write_marker(&home, UUID, "DonePRGreen").unwrap();
        assert!(p.exists());

        let markers = read_markers(&home);
        assert_eq!(
            markers,
            vec![Marker {
                uuid: UUID.to_string(),
                reason: "DonePRGreen".to_string(),
            }]
        );

        remove_marker(&home, UUID);
        assert!(read_markers(&home).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_skips_non_uuid_files() {
        let tmp = std::env::temp_dir().join(format!("fno-ts-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("terminal-stop");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.txt"), b"not a uuid").unwrap();
        std::fs::write(dir.join(UUID), b"NoWork").unwrap();

        let markers = read_markers(&AgentsHome::at(&tmp));
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].uuid, UUID);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_missing_dir_is_empty() {
        let home = AgentsHome::at(std::env::temp_dir().join("fno-ts-nonexistent-xyz"));
        assert!(read_markers(&home).is_empty());
    }
}
