//! The session-origin record, vendored from
//! `crates/fno/src/session_origin.rs`.
//!
//! Both crates publish separately, and a real fno dependency stays blocked
//! on the publish gate, so the writer side lives here as a mirror. The
//! contract parity test below pins every observable to the fno crate's
//! module, and its deletion is the trigger to retire this file.

use crate::claims;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One session's origin: the machine it began on, named without exposing the
/// hardware id, plus everything a later reader needs to find the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOrigin {
    pub machine: String,
    pub host: String,
    pub harness: String,
    pub session_id: String,
    pub transcript_path: String,
    pub recorded_at: String,
}

/// 16 hex chars of sha256 over the machine id. The raw hardware id never
/// lands in a synced file; the prefix still answers "same machine?".
pub fn this_machine() -> String {
    let id = claims::machine_id();
    if id.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(id.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

impl SessionOrigin {
    pub fn for_this_machine(harness: &str, session_id: &str, transcript: &Path) -> Self {
        SessionOrigin {
            machine: this_machine(),
            host: claims::hostname(),
            harness: harness.to_string(),
            session_id: session_id.to_string(),
            transcript_path: transcript.to_string_lossy().into_owned(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// `origin:` YAML for a handoff doc frontmatter. Each value rides as a
    /// JSON string, which is valid YAML and survives spaces and colons.
    pub fn frontmatter(&self) -> String {
        let v = |s: &str| serde_json::to_string(s).unwrap_or_default();
        format!(
            "origin:\n  machine: {}\n  host: {}\n  harness: {}\n  session_id: {}\n  transcript_path: {}\n  recorded_at: {}",
            v(&self.machine),
            v(&self.host),
            v(&self.harness),
            v(&self.session_id),
            v(&self.transcript_path),
            v(&self.recorded_at)
        )
    }

    /// One line for the history card: where, then what.
    pub fn origin_text(&self, mine: &str) -> String {
        let place = if mine.is_empty() {
            format!("{} (this machine's id is unreadable)", self.host)
        } else if mine == self.machine {
            format!("{} (this machine)", self.host)
        } else if !self.machine.is_empty() {
            format!("{} (another machine, id {})", self.host, self.machine)
        } else {
            format!("{} (machine id unrecorded)", self.host)
        };
        format!(
            "{}, {} session {}, transcript {}",
            place, self.harness, self.session_id, self.transcript_path
        )
    }
}

/// A uuid safe to use as a path component; the same rule as
/// `fno::transcript_tail::transcript_uuid_shaped`, mirrored for the same
/// publish-gate reason as the rest of this file.
fn transcript_uuid_shaped(uuid: &str) -> bool {
    !uuid.is_empty() && uuid.len() <= 64 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// The record's path beside the transcript, or None for an id that is not
/// safe as a path component: registry and payload content is untrusted and
/// lands in a path join. The `.fno.json` suffix stays out of every store
/// walker: codex walkers keep `rollout-` names, claude walkers keep `.jsonl`.
fn record_path(transcript: &Path, session_id: &str) -> Option<PathBuf> {
    if !transcript_uuid_shaped(session_id) {
        return None;
    }
    Some(transcript.parent()?.join(format!("{session_id}.fno.json")))
}

/// Write the record if none exists, then prune this machine's stale records
/// in the same folder. Returns whether a new record was written.
pub fn write_if_absent(origin: &SessionOrigin) -> std::io::Result<bool> {
    let Some(path) = record_path(Path::new(&origin.transcript_path), &origin.session_id) else {
        return Ok(false);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let written = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            let bytes = serde_json::to_vec_pretty(origin)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            file.write_all(&bytes)?;
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e),
    };
    if let Some(dir) = path.parent() {
        prune_beside(dir, &origin.session_id);
    }
    Ok(written)
}

// ponytail: prune runs only on write in the same folder; a folder with no new
// sessions keeps its stale records.
fn prune_beside(dir: &Path, keep: &str) {
    let mine = this_machine();
    if mine.is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut transcripts: Vec<String> = Vec::new();
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".jsonl") {
            transcripts.push(stem.to_string());
            continue;
        }
        let Some(id) = name.strip_suffix(".fno.json") else {
            continue;
        };
        if id == keep || !transcript_uuid_shaped(id) {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 3600));
        if stale {
            candidates.push((entry.path(), id.to_string()));
        }
    }
    for (path, id) in candidates {
        // A machine prunes only records it wrote: sync keeps a file's mtime,
        // so another machine's just-arrived record must survive a prune here,
        // or its deletion would sync back and erase it at home.
        let record: SessionOrigin = match std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(r) => r,
            None => continue,
        };
        if record.machine != mine {
            continue;
        }
        let live = transcripts
            .iter()
            .any(|stem| stem == &id || stem.ends_with(&format!("-{id}")));
        if !live {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Write a record that arrived from elsewhere beside a local transcript.
/// No prune: pruning stays the writing machine's job, on its own writes.
pub fn write_record_beside(transcript: &Path, origin: &SessionOrigin) -> std::io::Result<bool> {
    let Some(path) = record_path(transcript, &origin.session_id) else {
        return Ok(false);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            let bytes = serde_json::to_vec_pretty(origin)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            file.write_all(&bytes)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Read the record beside a transcript. Any error is None: a missing,
/// unreadable or foreign-shaped record means "not recorded", never a failure.
pub fn read_beside(transcript: &Path, session_id: &str) -> Option<SessionOrigin> {
    let path = record_path(transcript, session_id)?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    fn sample() -> SessionOrigin {
        SessionOrigin {
            machine: "aaaaaaaaaaaaaaaa".into(),
            host: "mac-a".into(),
            harness: "claude".into(),
            session_id: "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d".into(),
            transcript_path: "/t/w.jsonl".into(),
            recorded_at: "2026-10-07T00:00:00+00:00".into(),
        }
    }

    #[test]
    fn mirror_matches_the_fno_module_on_every_observable() {
        let mine = sample();
        let theirs: fno::session_origin::SessionOrigin =
            serde_json::from_value(serde_json::to_value(&mine).unwrap()).unwrap();
        assert_eq!(mine.frontmatter(), theirs.frontmatter());
        assert_eq!(
            mine.origin_text("aaaaaaaaaaaaaaaa"),
            theirs.origin_text("aaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            mine.origin_text("bbbbbbbbbbbbbbbb"),
            theirs.origin_text("bbbbbbbbbbbbbbbb")
        );
        assert_eq!(mine.origin_text(""), theirs.origin_text(""));
        // The machine hash is defined by the fno module; the mirror answers
        // the same 16 hex chars for the same machine.
        assert_eq!(this_machine(), fno::session_origin::this_machine());
        assert_eq!(this_machine().len(), 16);
        // The path rule refuses the same ids on both sides.
        for bad in ["", "../x", "a/b", &"x".repeat(65)] {
            let built = SessionOrigin {
                session_id: bad.into(),
                ..sample()
            };
            assert!(write_if_absent(&built).unwrap() == false, "{bad}");
        }
        // A record arriving from elsewhere places identically on both
        // sides, byte for byte, and the second placement is a no-op.
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let transcript_a = dir_a.path().join("w.jsonl");
        let transcript_b = dir_b.path().join("w.jsonl");
        std::fs::write(
            &transcript_a,
            "{}
",
        )
        .unwrap();
        std::fs::write(
            &transcript_b,
            "{}
",
        )
        .unwrap();
        let origin = SessionOrigin {
            machine: "aaaaaaaaaaaaaaaa".into(),
            host: "mac-a".into(),
            harness: "codex".into(),
            session_id: "0197bbbb-1234-7abc-9def-0123456789ab".into(),
            transcript_path: "/gone/w.jsonl".into(),
            recorded_at: "2026-10-07T00:00:00+00:00".into(),
        };
        let theirs: fno::session_origin::SessionOrigin =
            serde_json::from_value(serde_json::to_value(&origin).unwrap()).unwrap();
        let mine_written = write_record_beside(&transcript_a, &origin).unwrap();
        let theirs_written =
            fno::session_origin::write_record_beside(&transcript_b, &theirs).unwrap();
        assert_eq!(mine_written, theirs_written);
        assert_eq!(mine_written, true);
        let record_name = "0197bbbb-1234-7abc-9def-0123456789ab.fno.json";
        assert_eq!(
            std::fs::read(dir_a.path().join(record_name)).unwrap(),
            std::fs::read(dir_b.path().join(record_name)).unwrap()
        );
        // A second placement is a no-op on both sides.
        assert_eq!(write_record_beside(&transcript_a, &origin).unwrap(), false);
        assert_eq!(
            fno::session_origin::write_record_beside(&transcript_b, &theirs).unwrap(),
            false
        );
    }
}
