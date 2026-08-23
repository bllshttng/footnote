//! King driver: `KingQueue` + the `loop run --driver king` walk arm.
//!
//! Module name starts with "loop" so its lines count toward the control-plane
//! LOC-ratchet glob `crates/fno-agents/src/loop*`, the same reason
//! `loop_runtime.rs` and `loop_target.rs` are named that way.
//!
//! ## Why this arm was cut, and what unblocked the rebuild
//!
//! A cross-session `KingQueue` first shipped reading the single-file
//! `.fno/king-state.md` and was cut before release. Its unit carried the
//! manifest `fno_id` as `session_key` while BOTH king arms write a termination
//! under exactly that id, so `run_loop`'s resume guard closed every walk after
//! the first on sight. On the rare dispatch path, `CONTINUE_PROMPT` was
//! hardcoded to `/target --resume` and `Unit.extra_env` was read by nothing,
//! so the spawned session was a target resume that did not know it was a king.
//! The lifecycle those defects sat on is now real: manifests are per-scope at
//! `.fno/kings/<scope>.md`, coronation arms them, `fno agents king done`
//! expires them, and a leftover file is inert without a live registry crown.
//!
//! The rebuild fixes the identity split at the source: the walk keys its unit
//! per invocation (`{fno_id}-w{nanos}`), so no prior king terminal can close
//! it, and bounds respawns with an explicit manifest counter rather than by
//! key collision. At the ceiling the walk terminates on Budget before
//! dispatching.
//!
//! ## What this arm is for
//!
//! The in-session arm (`loop-check --driver king`) holds an awake king working
//! until its board is clean. It cannot help a king that is already gone: a
//! stop hook does not fire in a session that exited or is rate-limited. This
//! arm is the outside half. It respawns a king while the board is non-empty
//! and terminates `NoWork` when it is not.
//!
//! It does NOT cover the other edge, a king that correctly exited on an empty
//! board and now needs waking because the board refilled. Nothing in this crate
//! observes that; the fleet watchdog owns it.
//!
//! ## Why `close()` is inert
//!
//! Same reason `TargetQueue::close()` is. The king manifest is immutable after
//! arming (the respawn counter is the one field the walk rewrites, under the
//! manifest lock). There is no plan to stamp and no node to graduate, so there
//! is nothing left for a close to do.

use crate::loop_runtime::{CloseOutcome, Evidence, LoopError, Queue, Unit};
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Dispatches allowed per king unit before the walk parks it. The target arm
/// passes `None` (its single unit re-dispatches until it terminates); a king
/// unit is re-derived from the board each pass, so an unbounded re-dispatch
/// against a board that will not shrink is the shape that would burn a night.
pub const KING_MAX_DISPATCHES: u64 = 3;

pub struct KingQueue {
    fno_bin: String,
    cwd: PathBuf,
    manifest_path: PathBuf,
    fno_id: String,
    scope: String,
    /// Per-invocation correlation key minted at construction. The king's own
    /// stop hook writes terminations under the manifest `fno_id`, which is
    /// stable across reigns; keying the unit on it made the resume guard
    /// close every walk after the first on the previous reign's terminal.
    /// The mint never collides, so the guard never fires for a prior reign.
    walk_key: String,
    respawn_count: u64,
    respawn_ceiling: u64,
    /// One walk invocation bills exactly one respawn, even though `next()`
    /// re-derives the unit while the board holds actionable rows.
    billed: bool,
}

impl KingQueue {
    /// Read `.fno/kings/<scope>.md` from `repo_root` and construct the queue.
    ///
    /// A missing manifest is an error, not an empty queue. An empty queue
    /// would terminate `NoWork` and report success, which is the
    /// absence-as-evidence trap: "no work" and "nobody told me what I am
    /// watching" would produce the same clean exit.
    pub fn from_manifest(
        repo_root: &Path,
        scope: &str,
        fno_bin: String,
    ) -> Result<Self, LoopError> {
        let scope = scope.trim();
        if scope.is_empty()
            || scope.contains("..")
            || scope.contains('/')
            || scope.contains('\\')
            || scope.contains('\0')
        {
            return Err(LoopError::Queue(format!(
                "unsafe king scope for the walk: {scope:?}"
            )));
        }
        let manifest_path = repo_root
            .join(".fno")
            .join("kings")
            .join(format!("{scope}.md"));
        let content = fs::read_to_string(&manifest_path).map_err(|_| {
            LoopError::Queue(format!(
                "no king manifest at {} - crown the scope first (`fno agents spawn --crown \
                 <scope>` or `fno agents crown` arms it); the walk respawns a king, it \
                 cannot mint one",
                manifest_path.display()
            ))
        })?;
        let manifest = crate::loopcheck::parse_king_manifest(&content).ok_or_else(|| {
            LoopError::Queue(format!(
                "king manifest at {} has no fno_id - it is not parseable as a crown manifest",
                manifest_path.display()
            ))
        })?;
        let scope = if manifest.scope.is_empty() {
            scope.to_string()
        } else {
            manifest.scope
        };
        // Registry authority, not file authority. The manifest deliberately
        // outlives a crashed king (it is inert without a live crown), and the
        // walk exists to RECOVER that orphaned scope - so an absent or terminal
        // holder is the green light, not a refusal. But a LIVE row still
        // holding the scope means a king is already reigning: respawning a
        // second one is the double-rule the one-live-crown guard exists to
        // stop, and a stale or copied manifest must not outvote it.
        if let Some(live_holder) = live_crown_holder(&scope) {
            return Err(LoopError::Queue(format!(
                "a live king ({live_holder}) already reigns over {scope:?}: the walk \
                 respawns an orphaned scope, it never doubles a live one. Wake or \
                 reconcile the reigning king instead (`fno agents top`, \
                 `fno agents watchdog`)"
            )));
        }
        Ok(Self {
            walk_key: mint_walk_key(&manifest.fno_id),
            fno_id: manifest.fno_id,
            respawn_count: manifest.respawn_count,
            respawn_ceiling: manifest.respawn_ceiling,
            scope,
            fno_bin,
            cwd: repo_root.to_path_buf(),
            manifest_path,
            billed: false,
        })
    }

    /// The walk refuses to respawn another king past the manifest ceiling.
    /// `run_loop_verb_inner` is the ceiling authority: it terminates the walk
    /// on Budget before dispatching. The queue re-checks so a ceiling crossed
    /// by a concurrent walk between preflight and dequeue also stops here
    /// rather than spawning past it.
    pub fn at_respawn_ceiling(&self) -> bool {
        self.respawn_ceiling > 0 && self.respawn_count >= self.respawn_ceiling
    }

    pub fn walk_key(&self) -> &str {
        &self.walk_key
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn respawn_count(&self) -> u64 {
        self.respawn_count
    }

    pub fn respawn_ceiling(&self) -> u64 {
        self.respawn_ceiling
    }

    fn board_actionable(&self) -> Result<i64, LoopError> {
        let board =
            crate::loopcheck::read_king_board(&self.fno_bin, &self.cwd, &self.manifest_path)
                .map_err(LoopError::Queue)?;
        Ok(board.actionable)
    }
}

/// `{fno_id}-w{nanos}`: unique per invocation by the nanosecond clock, and
/// names the crown it belongs to so a journal read by a human says which
/// reign spawned the unit.
pub(crate) fn mint_walk_key(fno_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{fno_id}-w{nanos}")
}

/// The env var carrying [`KingQueue::walk_key`] into the dispatched session.
/// A king session gated by the stop hook emits its termination under the
/// manifest `fno_id`, which is stable across reigns; the walk keys its unit
/// per invocation so the resume guard cannot close on a prior reign. The two
/// only meet when the child inherits this var and `king_decide` tags the
/// terminal with it, letting the walk close the unit on the pass's own
/// verdict instead of parking it at the dispatch cap.
pub(crate) const WALK_SESSION_KEY_ENV: &str = "FNO_KING_WALK_SESSION_KEY";

/// The name of any live registry row holding a crown over `scope`, if the
/// registry is readable and such a row exists. An unreadable registry answers
/// `None` (fail-open to the recovery path): the walk's whole job is reviving
/// scopes whose registry state is suspect, and refusing on a read error would
/// strand exactly those, while the live-holder refusal above catches the
/// double-rule case whenever the registry CAN be read.
fn live_crown_holder(scope: &str) -> Option<String> {
    let home = crate::paths::AgentsHome::from_env();
    live_crown_holder_in(&home.registry_json(), scope)
}

fn live_crown_holder_in(registry_path: &Path, scope: &str) -> Option<String> {
    let registry = crate::state::load_registry(registry_path).ok()?;
    let is_terminal = |row: &crate::state::RegistryEntry| {
        matches!(
            row.status,
            crate::AgentStatus::Orphaned
                | crate::AgentStatus::Failed
                | crate::AgentStatus::Exited
                | crate::AgentStatus::PermanentDead
        )
    };
    registry
        .entries
        .iter()
        .filter(|row| !is_terminal(row))
        .find(|row| row.crown_scope.as_deref() == Some(scope))
        .map(|row| row.name.clone())
}

/// Tell the operator the king stopped with work still pending.
///
/// Called from every `NoProgress` terminal in `king_decide`, via that
/// function's shared `terminate` closure, so a terminal added later is covered
/// without anyone remembering to wire it. Returns the one-line outcome to
/// record, never an error: a failed escalation is named and moves on, since
/// blocking the terminal on it leaves the king stopped with nobody told either
/// way.
pub(crate) fn escalate_stalled(fno_bin: &str, cwd: &Path, ids: &[String], reason: &str) -> String {
    let output = Command::new(fno_bin)
        .args([
            "agents",
            "king",
            "escalate",
            "--stalled",
            &ids.join(","),
            "--reason",
            reason,
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let qid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            format!("escalated to the operator as {qid}")
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim().chars().take(300).collect::<String>();
            eprintln!("king: escalation failed: {detail}");
            format!("escalation FAILED: {detail}")
        }
        Err(e) => {
            eprintln!("king: cannot run {fno_bin} agents king escalate: {e}");
            format!("escalation FAILED: cannot run {fno_bin} agents king escalate: {e}")
        }
    }
}

fn frontmatter_field(line: &str, key: &str) -> Option<u64> {
    let (k, raw) = line.split_once(':')?;
    if k.trim() != key {
        return None;
    }
    raw.trim().trim_matches('"').parse::<u64>().ok()
}

/// +1 the manifest's `respawn_count`, returning the new count.
///
/// Takes the same `<scope>.md.lock` file the Python arming path flocks, so a
/// concurrent re-crown (which rewrites the whole manifest) and a bump cannot
/// interleave. All other lines pass through untouched.
pub(crate) fn bump_respawn_count(path: &Path) -> Result<u64, String> {
    let lock_path = path.with_extension("md.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let result = (|| {
        let content =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let count = content
            .lines()
            .find_map(|l| frontmatter_field(l, "respawn_count"))
            .unwrap_or(0)
            + 1;
        let mut out = String::with_capacity(content.len() + 8);
        let mut replaced = false;
        for line in content.lines() {
            if !replaced && frontmatter_field(line, "respawn_count").is_some() {
                out.push_str(&format!("respawn_count: {count}"));
                replaced = true;
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        if !replaced {
            // A manifest armed before the counter existed carries no field.
            // Insert after the identity line so the frontmatter stays grouped.
            out.clear();
            let mut inserted = false;
            for line in content.lines() {
                out.push_str(line);
                out.push('\n');
                if !inserted && line.trim_start().starts_with("fno_id:") {
                    out.push_str(&format!("respawn_count: {count}\n"));
                    inserted = true;
                }
            }
            if !inserted {
                return Err(format!(
                    "cannot bill a respawn on {}: no fno_id line to anchor the counter",
                    path.display()
                ));
            }
        }
        let tmp = path.with_extension("md.tmp");
        {
            let mut handle = fs::File::create(&tmp)
                .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            handle
                .write_all(out.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        }
        fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
        Ok(count)
    })();
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    result
}

impl KingQueue {
    /// Bill this walk's single respawn. `Ok(false)` means the LOCKED increment
    /// landed past the ceiling - a concurrent walk won the last slot between
    /// this walk's construction and its first dispatch - so the caller yields
    /// no unit. The over-billed count stays on the manifest as the race's
    /// scar; every later walk refuses at the preflight ceiling.
    fn bill_one_respawn(&mut self) -> Result<bool, LoopError> {
        if self.billed {
            return Ok(true);
        }
        let billed = bump_respawn_count(&self.manifest_path).map_err(LoopError::Queue)?;
        self.respawn_count = billed;
        self.billed = true;
        Ok(self.respawn_ceiling == 0 || billed <= self.respawn_ceiling)
    }
}

impl Queue for KingQueue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        if self.at_respawn_ceiling() {
            return Ok(None);
        }
        if self.board_actionable()? == 0 {
            return Ok(None);
        }
        if !self.bill_one_respawn()? {
            return Ok(None);
        }
        Ok(Some(Unit {
            id: self.fno_id.clone(),
            title: if self.scope.is_empty() {
                "king board drain".to_string()
            } else {
                format!("king reign over {}", self.scope)
            },
            session_key: self.walk_key.clone(),
            plan_path: None,
        }))
    }

    /// The respawn ceiling gates here too, so a walk re-probing after a
    /// concurrent bump answers "nothing affordable" and the outer budget
    /// check reports Budget rather than queueing a past-ceiling respawn.
    fn has_pending(&mut self) -> Result<bool, LoopError> {
        Ok(!self.at_respawn_ceiling() && self.board_actionable()? > 0)
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
    fn mints_a_key_that_names_the_crown_and_never_repeats() {
        let a = mint_walk_key("k-1");
        let b = mint_walk_key("k-1");
        assert!(a.starts_with("k-1-w"), "the key names its crown: {a}");
        assert_ne!(a, b, "two invocations must never share a key");
    }

    #[test]
    fn a_prior_terminal_under_the_bare_fno_id_cannot_close_a_walk_unit() {
        // The original defect, pinned at the seam it lived at: the unit key is
        // not the manifest fno_id, so a termination written under fno_id by
        // either king arm matches no walk unit's session_key.
        let key = mint_walk_key("k-1");
        assert_ne!(key, "k-1");
        assert!(!key.contains('\n'));
    }

    #[test]
    fn bumps_the_counter_and_preserves_every_other_line() {
        let dir = std::env::temp_dir().join(format!("kingbump-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.md");
        fs::write(
            &path,
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_count: 2\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(bump_respawn_count(&path).unwrap(), 3);
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            after,
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_count: 3\n---\nbody\n"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bills_a_manifest_that_predates_the_counter() {
        let dir = std::env::temp_dir().join(format!("kingbump-old-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.md");
        fs::write(&path, "---\nfno_id: k-1\nscope: epic-x\n---\n").unwrap();
        assert_eq!(bump_respawn_count(&path).unwrap(), 1);
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("fno_id: k-1\nrespawn_count: 1\n"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_ceiling_of_zero_means_unbounded_not_always_at_ceiling() {
        // An explicit ceiling of 0 is the unbounded spelling (the budget is
        // the only bound); reading it as "at ceiling" would refuse every
        // respawn for a scope that deliberately disabled the counter.
        let dir = std::env::temp_dir().join(format!("kingq-{}", std::process::id()));
        let kings = dir.join(".fno").join("kings");
        fs::create_dir_all(&kings).unwrap();
        let path = kings.join("k.md");
        fs::write(
            &path,
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_ceiling: 0\n---\n",
        )
        .unwrap();
        let q = KingQueue::from_manifest(&dir, "k", "fno".to_string()).unwrap();
        assert_eq!(q.respawn_ceiling(), 0);
        assert!(!q.at_respawn_ceiling());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_an_unsafe_scope_and_names_the_manifest_it_tried() {
        let err = KingQueue::from_manifest(Path::new("."), "../escape", "fno".to_string())
            .err()
            .expect("escape scope must refuse");
        assert!(err.to_string().contains("unsafe king scope"));
    }

    fn write_registry(dir: &Path, status: &str, scope: Option<&str>) -> PathBuf {
        let row = serde_json::json!({
            "name": "reigning-king",
            "cwd": "/tmp",
            "status": status,
            "created_at": "2026-08-23T00:00:00Z",
            "crown_level": scope.map(|_| 2),
            "crown_scope": scope,
            "crown_grantor": scope.map(|_| "human"),
        });
        let path = dir.join("registry.json");
        fs::write(
            &path,
            serde_json::json!({"schema_version": 11, "agents": [row]}).to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn a_live_crown_holder_read_from_the_registry_refuses_the_walk() {
        let dir = std::env::temp_dir().join(format!("kinglive-{}", std::process::id()));
        let kings = dir.join(".fno").join("kings");
        fs::create_dir_all(&kings).unwrap();
        fs::write(kings.join("k.md"), "---\nfno_id: k-1\nscope: epic-x\n---\n").unwrap();
        let registry = write_registry(&dir, "busy", Some("epic-x"));

        assert_eq!(
            live_crown_holder_in(&registry, "epic-x"),
            Some("reigning-king".to_string())
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_terminal_or_absent_holder_leaves_the_scope_recoverable() {
        let dir = std::env::temp_dir().join(format!("kingdead-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let exited = write_registry(&dir, "exited", Some("epic-x"));
        assert_eq!(live_crown_holder_in(&exited, "epic-x"), None);
        let uncrowned = write_registry(&dir, "busy", None);
        assert_eq!(live_crown_holder_in(&uncrowned, "epic-x"), None);
        assert_eq!(
            live_crown_holder_in(&dir.join("no-such-registry.json"), "epic-x"),
            None,
            "an unreadable registry fails open to the recovery path"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_concurrent_over_bill_refuses_instead_of_dispatching() {
        // Two walks raced past the stale ceiling check; the loser sees the
        // locked increment return a count PAST the ceiling and must yield no
        // unit. Simulated by bumping the file between construction and next().
        let dir = std::env::temp_dir().join(format!("kingrace-{}", std::process::id()));
        let kings = dir.join(".fno").join("kings");
        fs::create_dir_all(&kings).unwrap();
        let path = kings.join("k.md");
        fs::write(
            &path,
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_count: 3\nrespawn_ceiling: 4\n---\n",
        )
        .unwrap();
        let mut q = KingQueue::from_manifest(&dir, "k", "fno".to_string()).unwrap();
        assert!(!q.at_respawn_ceiling(), "3 of 4 is under the ceiling");
        // The concurrent winner bills the ceiling first...
        assert_eq!(bump_respawn_count(&path).unwrap(), 4);
        // ...so this walk's own bump returns 5, past the ceiling.
        assert!(
            !q.bill_one_respawn().unwrap(),
            "the race loser must not dispatch"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
