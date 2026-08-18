//! King driver: `KingQueue` + the `loop run --driver king` walk arm.
//!
//! Module name starts with "loop" so its lines count toward the control-plane
//! LOC-ratchet glob `crates/fno-agents/src/loop*`, the same reason
//! `loop_runtime.rs` and `loop_target.rs` are named that way.
//!
//! ## What this arm is for
//!
//! The in-session arm (`loop-check --driver king`) holds an awake king working
//! until its board is clean. It cannot help a king that is already gone: a
//! stop hook does not fire in a session that exited or is rate-limited. This
//! arm is the outside half. It respawns a king while the board is non-empty and
//! terminates `NoWork` when it is not.
//!
//! It does NOT cover the other edge, a king that correctly exited on an empty
//! board and now needs waking because the board refilled. Nothing in this crate
//! observes that; an external watchdog owns it. Shipping this arm alone trades
//! idle-forever-with-work-pending for exited-and-nothing-restarts-me, which is
//! the same failure with a better exit code, so the two belong together.
//!
//! ## Why `close()` is inert
//!
//! Same reason `TargetQueue::close()` is. The king session's own stop hook
//! already emitted the termination event before `close()` is called, and the
//! king manifest is immutable after init. There is no plan to stamp and no
//! node to graduate, so there is nothing left for a close to do.

use crate::loop_runtime::{CloseOutcome, Evidence, LoopError, Queue, Unit};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Dispatches allowed per king unit before the walk parks it. The target arm
/// passes `None` (its single unit re-dispatches until it terminates); a king
/// unit is re-derived from the board each pass, so an unbounded re-dispatch
/// against a board that will not shrink is the shape that would burn a night.
pub const KING_MAX_DISPATCHES: u64 = 3;

pub struct KingQueue {
    fno_bin: String,
    cwd: PathBuf,
    /// The king session this walk respawns, read from the manifest. Used as the
    /// unit's `session_key` so the journal matches the termination event the
    /// king's OWN stop hook emits, rather than inventing a second identity.
    session_id: String,
    scope: String,
}

impl KingQueue {
    /// Read `.fno/king-state.md` from `repo_root` and construct the queue.
    ///
    /// A missing manifest is an error, not an empty queue. An empty queue would
    /// terminate `NoWork` and report success, which is the absence-as-evidence
    /// trap: "no work" and "nobody told me what I am watching" would produce
    /// the same clean exit.
    pub fn from_manifest(repo_root: &Path, fno_bin: String) -> Result<Self, LoopError> {
        let manifest_path = repo_root.join(".fno").join("king-state.md");
        let content = std::fs::read_to_string(&manifest_path).map_err(|_| {
            LoopError::Queue(format!(
                "no king manifest at {} - run `fno king init --scope \"<what>\"` first",
                manifest_path.display()
            ))
        })?;
        let (session_id, scope) = parse_king_fields(&content).ok_or_else(|| {
            LoopError::Queue(format!(
                "king manifest at {} has no fno_id - run `fno king init` to rewrite it",
                manifest_path.display()
            ))
        })?;
        Ok(Self {
            fno_bin,
            cwd: repo_root.to_path_buf(),
            session_id,
            scope,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Actionable row count from `fno king board --json`.
    ///
    /// The board exits non-zero when a queue is unreadable and still prints a
    /// full payload, so the payload is parsed regardless of exit status. Only
    /// an absent or unparseable one is a read failure, and that is an error
    /// rather than a zero: a walk that read "0" from a broken reader would
    /// report the board clean and stop respawning a king that still has work.
    fn actionable(&self) -> Result<i64, LoopError> {
        let output = Command::new(&self.fno_bin)
            .args(["king", "board", "--json"])
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| LoopError::Queue(format!("cannot run {} king board: {e}", self.fno_bin)))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value: Value = serde_json::from_str(&stdout).map_err(|e| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            LoopError::Queue(format!(
                "king board output unparseable ({e}); stderr: {}",
                stderr.trim().chars().take(300).collect::<String>()
            ))
        })?;
        value
            .get("actionable")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| LoopError::Queue("king board payload has no actionable count".into()))
    }
}

/// `(fno_id, scope)` from a king manifest's frontmatter.
pub(crate) fn parse_king_fields(content: &str) -> Option<(String, String)> {
    let mut fno_id = String::new();
    let mut scope = String::new();
    let mut in_frontmatter = false;
    for line in content.lines() {
        if line.trim() == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let value = raw.trim().trim_matches('"').to_string();
        match key.trim() {
            "fno_id" => fno_id = value,
            "scope" => scope = value,
            _ => {}
        }
    }
    if fno_id.is_empty() {
        None
    } else {
        Some((fno_id, scope))
    }
}

impl Queue for KingQueue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        if self.actionable()? == 0 {
            return Ok(None);
        }
        Ok(Some(Unit {
            id: self.session_id.clone(),
            title: if self.scope.is_empty() {
                "king board drain".to_string()
            } else {
                self.scope.clone()
            },
            session_key: self.session_id.clone(),
            plan_path: None,
            extra_env: vec![("FNO_DRIVER".to_string(), "king".to_string())],
        }))
    }

    /// Inert close: see the module doc for why this does nothing.
    fn close(&mut self, _unit: &Unit, _evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        Ok(CloseOutcome::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_fields_the_walk_reads() {
        let (id, scope) =
            parse_king_fields("---\nfno_id: k-1\nscope: drain the board\n---\n").unwrap();
        assert_eq!(id, "k-1");
        assert_eq!(scope, "drain the board");
    }

    #[test]
    fn a_manifest_without_an_fno_id_is_not_a_manifest() {
        assert!(parse_king_fields("---\nscope: nothing\n---\n").is_none());
    }

    #[test]
    fn a_quoted_scope_survives() {
        let (_, scope) = parse_king_fields("---\nfno_id: k\nscope: \"a b\"\n---\n").unwrap();
        assert_eq!(scope, "a b");
    }

    #[test]
    fn body_text_after_the_frontmatter_is_not_parsed_as_fields() {
        let (id, _) =
            parse_king_fields("---\nfno_id: k-1\n---\nfno_id: not-this\n").unwrap();
        assert_eq!(id, "k-1");
    }
}
