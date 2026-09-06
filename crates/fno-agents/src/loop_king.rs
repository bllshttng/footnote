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
//! `<space>/kings/<scope>.md`, coronation arms them, `fno agents king done`
//! expires them, and a leftover file is inert without a live registry crown.
//!
//! The rebuild fixes the identity split at the source: the walk keys its unit
//! per invocation (`{fno_id}-w{nanos}`), so no prior king terminal can close
//! it, and bounds dispatch-bearing walk invocations with an explicit manifest
//! counter rather than by key collision. What `bill_one_respawn` charges is a
//! walk that found an undelivered scope and dispatched a king - at most once
//! per invocation, billed only after the NoWork return - so the counter is a
//! dispatch budget, never a failure-retry count. At the ceiling the walk
//! terminates on Budget before dispatching.
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
//! board and now needs waking because the board refilled. Nothing in this
//! crate CAN observe it: a loop that terminated `NoWork` is not running, so
//! there is no "inside" left to observe from. The fleet watchdog does NOT own
//! it either - it wakes on `classify_tail == "stalled"`, a session gone silent
//! while still owing its next move, and a cleanly-exited king is neither. The
//! owner is the `wake` phase of `fno pr-watch tick`, which enters this crate
//! through `loop run --driver king --scope <scope> --wake`.
//!
//! ## Why `close()` is inert
//!
//! Same reason `TargetQueue::close()` is. The king manifest is immutable after
//! arming except for two walk-rewritten fields, each under the manifest lock:
//! `respawn_count` (the walk arm, after a dispatch-bearing invocation) and
//! `wake_times` (the pr-watch wake phase's rolling wake ledger). There is no
//! plan to stamp and no node to graduate, so there is nothing left for a
//! close to do.

use crate::loop_runtime::{CloseOutcome, Evidence, LoopError, Queue, Unit};
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
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
    /// re-derives the unit while the scope holds undelivered nodes.
    billed: bool,
    /// Wake mode (`--wake`): the walk is executing a wake the caller already
    /// gated, so it neither spends nor is refused by the failure budget above.
    /// The wake ledger on the manifest is the bound in this mode, enforced by
    /// the CALLER before invoking the walk; an operator running `--wake` by
    /// hand is deliberately bypassing a rate limit, not a safety limit.
    wake: bool,
    /// Successor mode (`--wake-successor`): the wake fired because the holder
    /// is GONE, so the dispatch this walk performs is a new king generation -
    /// exactly what the respawn budget exists to bound. Unlike an ordinary
    /// wake (a parked holder resuming, normal operation billed to the wake
    /// ledger alone), a successor bills `respawn_count` and is refused by the
    /// respawn ceiling like any walk respawn. There is no second respawn
    /// budget: same counter, same ceiling.
    successor: bool,
}

impl KingQueue {
    /// Read `<space>/kings/<scope>.md` from `repo_root` and construct the queue.
    ///
    /// A missing manifest is an error, not an empty queue. An empty queue
    /// would terminate `NoWork` and report success, which is the
    /// absence-as-evidence trap: "no work" and "nobody told me what I am
    /// watching" would produce the same clean exit.
    ///
    /// The successor modifier is a parameter, not a second constructor: only
    /// the wake phase passes `true` (a dead holder's replacement); every
    /// other caller passes `false` and never mints successors.
    pub fn from_manifest_full(
        repo_root: &Path,
        scope: &str,
        fno_bin: String,
        wake: bool,
        wake_holder: Option<&str>,
        successor: bool,
    ) -> Result<Self, LoopError> {
        let home = crate::paths::AgentsHome::from_env();
        Self::from_manifest_with_registry(
            repo_root,
            scope,
            fno_bin,
            wake,
            wake_holder,
            successor,
            &home.registry_json(),
        )
    }

    /// The construction path with the registry injected, so the live-holder
    /// decision is unit-testable without mutating process env (a set_var race
    /// against parallel tests reading the same env would test the scheduler,
    /// not the guard).
    #[allow(clippy::too_many_arguments)]
    pub fn from_manifest_with_registry(
        repo_root: &Path,
        scope: &str,
        fno_bin: String,
        wake: bool,
        wake_holder: Option<&str>,
        successor: bool,
        registry_path: &Path,
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
        let manifest_path = crate::paths::space_dir(repo_root)
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
        //
        // Wake mode skips that refusal ONLY for the row the wake caller named:
        // the caller reached this walk after transcript truth resolved that
        // holder gone, and a cleanly-exited session's registry row stays
        // non-terminal (the status word is not liveness), so wake_holder names
        // the one row the guard must not outvote. Any OTHER live holder - or
        // a hand-run --wake that names nobody - still refuses, so the flag
        // can never double a reigning king.
        if let Some(live_holder) = live_crown_holder_in(registry_path, &scope) {
            let caller_named_this_row = wake && wake_holder == Some(live_holder.as_str());
            if !caller_named_this_row {
                return Err(LoopError::Queue(format!(
                    "a live king ({live_holder}) already reigns over {scope:?}: the walk \
                     respawns an orphaned scope, it never doubles a live one. Wake or \
                     reconcile the reigning king instead (`fno agents top`, \
                     `fno agents watchdog`)"
                )));
            }
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
            wake,
            successor,
        })
    }

    /// Whether this walk's dispatches count against the respawn budget. An
    /// ordinary wake does not (the caller's wake ledger is its bound); a
    /// successor does, because each successor IS a king generation.
    fn respawn_accounted(&self) -> bool {
        !self.wake || self.successor
    }

    /// The walk refuses to respawn another king past the manifest ceiling.
    /// `run_loop_verb_inner` is the ceiling authority: it terminates the walk
    /// on Budget before dispatching. The queue re-checks so a ceiling crossed
    /// by a concurrent walk between preflight and dequeue also stops here
    /// rather than spawning past it. What the ceiling bounds is
    /// dispatch-bearing walk invocations per crown (what `bill_one_respawn`
    /// charges), not failures; in `--wake` mode neither this check nor the
    /// bill runs, because a woken respawn is normal operation whose bound is
    /// the caller's wake ledger.
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

    /// The reign's work test for this crown: see `scope_undelivered_count`.
    fn scope_undelivered(&self) -> Result<i64, LoopError> {
        scope_undelivered_count(&self.fno_bin, &self.cwd, &self.scope)
    }
}

/// The reign's work test as a free read: crown nodes not done and not
/// superseded, answered by the one Python seam that compiles a scope. The
/// goal text keys completion on that count, so every termination decision
/// reads it and no queue: a row with a driver leaves the actionable board
/// while its work is unshipped. An unreadable answer is an error, never
/// zero - a caller that cannot see the scope must not certify it drained.
pub(crate) fn scope_undelivered_count(
    fno_bin: &str,
    cwd: &Path,
    scope: &str,
) -> Result<i64, LoopError> {
    scope_undelivered_count_with_timeout(
        fno_bin,
        cwd,
        scope,
        crate::loopcheck::stopgate_read_timeout(),
    )
}

fn scope_undelivered_count_with_timeout(
    fno_bin: &str,
    cwd: &Path,
    scope: &str,
    timeout: std::time::Duration,
) -> Result<i64, LoopError> {
    let out = crate::loopcheck::bounded_read(
        std::ffi::OsStr::new(fno_bin),
        &["agents", "king", "drain", scope],
        cwd,
        "king drain",
        timeout,
    )
    .map_err(|error| {
        LoopError::Queue(format!("king drain for {scope} failed: {}", error.render()))
    })?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr_tail);
        return Err(LoopError::Queue(format!(
            "king drain for {scope} failed ({}): {}",
            out.status,
            detail.trim().chars().take(200).collect::<String>()
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    let payload: serde_json::Value = serde_json::from_str(trimmed).map_err(|_| {
        LoopError::Queue(format!(
            "king drain for {scope} returned no JSON (exit {}): {}",
            out.status,
            trimmed.chars().take(200).collect::<String>()
        ))
    })?;
    payload
        .get("undelivered")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| {
            LoopError::Queue(format!(
                "king drain payload for {scope} carries no undelivered count"
            ))
        })
}

/// `{fno_id}-w{nanos}`: unique per invocation by the nanosecond clock, and
/// names the crown it belongs to so a journal read by a human says which
/// reign spawned the unit.
pub(crate) fn mint_walk_key(fno_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // The clock alone is not a unique key. macOS reports `SystemTime` at
    // microsecond granularity, so two walks minted in the same tick -- in one
    // process or in two started together -- got the SAME key, and the resume
    // guard would then close a unit on a prior reign's verdict. The random
    // suffix is what makes "never repeats" true; the timestamp stays because it
    // makes the key sortable and readable.
    let mut entropy = [0u8; 4];
    let suffix = if getrandom::fill(&mut entropy).is_ok() {
        u32::from_le_bytes(entropy)
    } else {
        // A pid is constant for the life of the process, so a pid-only fallback
        // rebuilds the very collision above for two mints in one tick. The
        // counter is what keeps the fallback unique in-process.
        static SEQ: AtomicU32 = AtomicU32::new(0);
        std::process::id() ^ SEQ.fetch_add(1, Ordering::Relaxed)
    };
    format!("{fno_id}-w{nanos}-{suffix:08x}")
}

/// The env var carrying [`KingQueue::walk_key`] into the dispatched session.
/// A king session gated by the stop hook emits its termination under the
/// manifest `fno_id`, which is stable across reigns; the walk keys its unit
/// per invocation so the resume guard cannot close on a prior reign. The two
/// only meet when the child inherits this var and `king_decide` tags the
/// terminal with it, letting the walk close the unit on the pass's own
/// verdict instead of parking it at the dispatch cap.
pub(crate) const WALK_SESSION_KEY_ENV: &str = "FNO_KING_WALK_SESSION_KEY";

/// Do two stored crown scopes share a member? A rung-2 crown is stored as the
/// canonical comma-joined set, and a set-holder already reigns over each
/// member, so the one-live-crown guard must answer set membership - never
/// string equality, which would let a second king over one member double-rule
/// the set-holder.
fn scopes_overlap(held: &str, requested: &str) -> bool {
    let members: Vec<&str> = held
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .collect();
    if members.is_empty() {
        return false;
    }
    requested
        .split(',')
        .map(str::trim)
        .any(|r| !r.is_empty() && members.contains(&r))
}

/// The name of any live registry row holding a crown over `scope`, if the
/// registry is readable and such a row exists. An unreadable registry answers
/// `None` (fail-open to the recovery path): the walk's whole job is reviving
/// scopes whose registry state is suspect, and refusing on a read error would
/// strand exactly those, while the live-holder refusal above catches the
/// double-rule case whenever the registry CAN be read.
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
        .find(|row| {
            row.crown_scope
                .as_deref()
                .is_some_and(|held| scopes_overlap(held, scope))
        })
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
        if self.respawn_accounted() && self.at_respawn_ceiling() {
            return Ok(None);
        }
        // Stays in wake mode too: a spurious trigger over a drained scope must
        // still terminate NoWork, or a missed mail flag spawns a king with
        // nothing to do.
        if self.scope_undelivered()? == 0 {
            return Ok(None);
        }
        if self.respawn_accounted() && !self.bill_one_respawn()? {
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
    /// Wake mode drops the ceiling term for the same reason `next()` does.
    fn has_pending(&mut self) -> Result<bool, LoopError> {
        Ok((!self.respawn_accounted() || !self.at_respawn_ceiling())
            && self.scope_undelivered()? > 0)
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

        // Two mints only catch a timestamp-only key when the clock happens to
        // tick between them, which is how this test passed for a build that
        // could collide. A tight batch cannot get that luck.
        let batch: std::collections::BTreeSet<String> =
            (0..1000).map(|_| mint_walk_key("k-1")).collect();
        assert_eq!(batch.len(), 1000, "1000 mints must produce 1000 keys");
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
        let kings = crate::paths::space_dir(&dir).join("kings");
        fs::create_dir_all(&kings).unwrap();
        let path = kings.join("k.md");
        fs::write(
            &path,
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_ceiling: 0\n---\n",
        )
        .unwrap();
        let q = KingQueue::from_manifest_full(&dir, "k", "fno".to_string(), false, None, false)
            .unwrap();
        assert_eq!(q.respawn_ceiling(), 0);
        assert!(!q.at_respawn_ceiling());
        fs::remove_dir_all(crate::paths::space_dir(&dir)).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_an_unsafe_scope_and_names_the_manifest_it_tried() {
        let err = KingQueue::from_manifest_full(
            Path::new("."),
            "../escape",
            "fno".to_string(),
            false,
            None,
            false,
        )
        .err()
        .expect("escape scope must refuse");
        assert!(err.to_string().contains("unsafe king scope"));
    }

    #[test]
    fn termination_reads_the_scope_drain_not_the_actionable_board() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("kingdrain-{}", std::process::id()));
        let kings = dir.join(".fno").join("kings");
        fs::create_dir_all(&kings).unwrap();
        fs::write(
            kings.join("k.md"),
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_ceiling: 0\n---\n",
        )
        .unwrap();
        let registry = dir.join("no-registry.json");
        let stub = |body: &str, name: &str| -> String {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\necho '{body}'\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().to_string()
        };

        // The 2026-09-06 incident state: every row driven, nothing shipped. An
        // inbox-board read answers zero here; the drain read must not.
        let undelivered = stub(r#"{"scope":"epic-x","undelivered":4}"#, "fno-drain-some");
        let mut q = KingQueue::from_manifest_with_registry(
            &dir,
            "k",
            undelivered,
            false,
            None,
            false,
            &registry,
        )
        .unwrap();
        assert!(
            q.next().unwrap().is_some(),
            "an undelivered scope re-derives the unit"
        );

        let drained = stub(r#"{"scope":"epic-x","undelivered":0}"#, "fno-drain-none");
        let mut q0 = KingQueue::from_manifest_with_registry(
            &dir, "k", drained, false, None, false, &registry,
        )
        .unwrap();
        assert!(
            q0.next().unwrap().is_none(),
            "a drained scope terminates NoWork"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drain_rejects_json_from_a_failed_command() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("kingdrain-failed-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fno-drain-failed");
        fs::write(
            &path,
            "#!/bin/sh\necho '{\"scope\":\"epic-x\",\"undelivered\":0}'\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        let result = scope_undelivered_count(path.to_str().unwrap(), &dir, "epic-x");

        assert!(
            result.is_err(),
            "a failed drain must not certify a clean scope"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drain_kills_a_hung_command_inside_its_read_bound() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fno-drain-hung");
        fs::write(&path, "#!/bin/sh\nsleep 1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let result = scope_undelivered_count_with_timeout(
            path.to_str().unwrap(),
            dir.path(),
            "epic-x",
            std::time::Duration::from_millis(100),
        );

        let error = result.expect_err("a hung drain must not certify a scope");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "drain exceeded its wall-clock bound: {:?}",
            started.elapsed()
        );
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
        fs::create_dir_all(&dir).unwrap();
        let kings = crate::paths::space_dir(&dir).join("kings");
        fs::create_dir_all(&kings).unwrap();
        fs::write(kings.join("k.md"), "---\nfno_id: k-1\nscope: epic-x\n---\n").unwrap();
        let registry = write_registry(&dir, "busy", Some("epic-x"));

        assert_eq!(
            live_crown_holder_in(&registry, "epic-x"),
            Some("reigning-king".to_string())
        );
        // The same registry through the walk: an ordinary walk refuses, and a
        // wake refuses too unless it names the very row transcript truth
        // resolved gone. A cleanly-exited session's row stays non-terminal
        // (the status word is not liveness), so the named wake must pass -
        // and a wake that names nobody (a hand-run one) can never double a
        // live king.
        let plain = KingQueue::from_manifest_with_registry(
            &dir,
            "k",
            "fno".to_string(),
            false,
            None,
            false,
            &registry,
        );
        assert!(plain.is_err(), "an ordinary walk never doubles a live row");
        let unnamed = KingQueue::from_manifest_with_registry(
            &dir,
            "k",
            "fno".to_string(),
            true,
            None,
            false,
            &registry,
        );
        assert!(
            unnamed.is_err(),
            "a wake that names nobody never doubles a live row"
        );
        let named = KingQueue::from_manifest_with_registry(
            &dir,
            "k",
            "fno".to_string(),
            true,
            Some("reigning-king"),
            false,
            &registry,
        );
        assert!(
            named.is_ok(),
            "wake mode outranks the status word of the row it named"
        );
        let wrong_row = KingQueue::from_manifest_with_registry(
            &dir,
            "k",
            "fno".to_string(),
            true,
            Some("someone-else"),
            false,
            &registry,
        );
        assert!(
            wrong_row.is_err(),
            "naming a row other than the live holder never doubles a live one"
        );
        fs::remove_dir_all(crate::paths::space_dir(&dir)).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_crown_holder_in_set() {
        // A rung-2 crown is stored as the canonical comma-joined set. The
        // guard must answer set membership: a live king over {epic-a,epic-b}
        // already reigns over epic-a alone, so a walk recovering either
        // member - or the joined name itself - finds the holder.
        let dir = std::env::temp_dir().join(format!("kingset-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let registry = write_registry(&dir, "busy", Some("epic-a,epic-b"));

        assert_eq!(
            live_crown_holder_in(&registry, "epic-a"),
            Some("reigning-king".to_string())
        );
        assert_eq!(
            live_crown_holder_in(&registry, "epic-b"),
            Some("reigning-king".to_string())
        );
        assert_eq!(
            live_crown_holder_in(&registry, "epic-a,epic-b"),
            Some("reigning-king".to_string())
        );
        assert_eq!(live_crown_holder_in(&registry, "epic-c"), None);
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
    fn a_successor_wake_is_refused_by_the_respawn_ceiling_like_any_walk() {
        // Wake mode normally drops the ceiling term (the caller's wake ledger
        // is the bound there); a successor is a king generation, so the
        // respawn budget binds it. The gate fires before any board read, so
        // this needs no live fno binary to prove the refusal.
        let dir = std::env::temp_dir().join(format!("kingsucc-{}", std::process::id()));
        let kings = crate::paths::space_dir(&dir).join("kings");
        fs::create_dir_all(&kings).unwrap();
        fs::write(
            &kings.join("k.md"),
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_count: 4\nrespawn_ceiling: 4\n---\n",
        )
        .unwrap();
        let mut q = KingQueue::from_manifest_full(
            &dir,
            "k",
            "fno".to_string(),
            true,
            Some("reigning-king"),
            true,
        )
        .unwrap();
        assert!(q.at_respawn_ceiling());
        assert!(
            q.next().is_ok_and(|unit| unit.is_none()),
            "an at-ceiling successor yields no unit, before any board read"
        );
        fs::remove_dir_all(crate::paths::space_dir(&dir)).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_concurrent_over_bill_refuses_instead_of_dispatching() {
        // Two walks raced past the stale ceiling check; the loser sees the
        // locked increment return a count PAST the ceiling and must yield no
        // unit. Simulated by bumping the file between construction and next().
        let dir = std::env::temp_dir().join(format!("kingrace-{}", std::process::id()));
        let kings = crate::paths::space_dir(&dir).join("kings");
        fs::create_dir_all(&kings).unwrap();
        let path = kings.join("k.md");
        fs::write(
            &path,
            "---\nfno_id: k-1\nscope: epic-x\nrespawn_count: 3\nrespawn_ceiling: 4\n---\n",
        )
        .unwrap();
        let mut q = KingQueue::from_manifest_full(&dir, "k", "fno".to_string(), false, None, false)
            .unwrap();
        assert!(!q.at_respawn_ceiling(), "3 of 4 is under the ceiling");
        // The concurrent winner bills the ceiling first...
        assert_eq!(bump_respawn_count(&path).unwrap(), 4);
        // ...so this walk's own bump returns 5, past the ceiling.
        assert!(
            !q.bill_one_respawn().unwrap(),
            "the race loser must not dispatch"
        );
        fs::remove_dir_all(crate::paths::space_dir(&dir)).ok();
        fs::remove_dir_all(&dir).ok();
    }
}
