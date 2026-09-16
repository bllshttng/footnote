//! The owned-caller proof, native.
//!
//! The complete port of Python's `resolve_self_identity`
//! (`cli/src/fno/claims/self_identity.py`) and its `harness_identity`
//! resolver: marker tables, the fno stamp, canonical/vendor consistency, the
//! proven/ambiguous elimination ladder, the process-tree attester walk, the
//! cwd-keyed spawn record, and the collision checks. The spawn door consumes
//! THIS proof; the claims-side resolver only folds marker consistency and is
//! not equivalent.
//!
//! Every function takes injectable lookups (`get`) or hooks (`prove`,
//! `collide`, `witness`) so the resolution contract runs without mutating
//! process-global env or the real registry.

use crate::census::{self, ProcRow};
use crate::claims::{
    canonical_identity_from, same_session_id, CanonicalDisposition, HARNESS_SESSION_MARKERS,
    LEGACY_HARNESS_SESSION_MARKERS,
};
use crate::paths::AgentsHome;
use crate::state::load_registry;
use serde_json::Value;
use std::collections::BTreeMap;

/// Normalize one session id for identity comparison across stores: UUID-family
/// ids are case-insensitive, OpenCode `ses_` ids are not. Mirror of Python
/// `session_identity_key`.
pub fn session_identity_key(session_id: &str) -> String {
    if session_id.starts_with("ses_") {
        session_id.to_string()
    } else {
        session_id.to_lowercase()
    }
}

/// Every nonblank ambient harness marker, in shared precedence order:
/// `(marker, harness, value)`. Mirror of Python `present_harness_markers`.
pub fn present_markers(get: &impl Fn(&str) -> Option<String>) -> Vec<(String, String, String)> {
    HARNESS_SESSION_MARKERS
        .iter()
        .chain(LEGACY_HARNESS_SESSION_MARKERS.iter())
        .filter_map(|(marker, harness)| {
            get(marker)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(|v| (marker.to_string(), harness.to_string(), v))
        })
        .collect()
}

/// The fno-owned spawn stamp, parsed WITHOUT treating a partial stamp as
/// evidence. Mirror of Python `parse_canonical_identity`.
pub fn parse_canonical_stamp(
    get: &impl Fn(&str) -> Option<String>,
) -> (Option<String>, Option<String>, CanonicalDisposition) {
    canonical_identity_from(get)
}

/// Canonical-vs-vendor resolution with its disposition class. Mirror of
/// Python `_resolve_canonical_identity`, which names invalid/contradiction
/// where the claims-side resolver only returns empty pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalResolution {
    pub session_id: Option<String>,
    pub harness: Option<String>,
    pub class: CanonicalClass,
    pub markers: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalClass {
    Absent,
    Invalid,
    Contradiction,
    Canonical,
}

/// Port of Python `_resolve_canonical_identity`: the canonical stamp leads;
/// a malformed stamp is `Invalid`; markers from two harness families, or a
/// vendor family disagreeing with the stamp's, are `Contradiction`.
pub fn resolve_canonical_resolution(get: &impl Fn(&str) -> Option<String>) -> CanonicalResolution {
    let (session, harness, disposition) = parse_canonical_stamp(get);
    let markers = present_markers(get);
    // The vendor fold, inlined (Python `_vendor_identity`): first family
    // wins; two ids of one family that disagree conflict; multiple families
    // resolve to nothing.
    let mut family: Option<String> = None;
    let mut vendor_session: Option<String> = None;
    let mut conflicted = false;
    for (_marker, mharness, value) in &markers {
        match &family {
            None => {
                family = Some(mharness.clone());
                vendor_session = Some(value.clone());
            }
            Some(prev) if prev != mharness => {
                family = None;
                vendor_session = None;
                conflicted = true;
            }
            Some(_) => {
                if !same_session_id(vendor_session.as_deref().unwrap_or_default(), value) {
                    conflicted = true;
                }
            }
        }
    }
    if conflicted {
        family = None;
        vendor_session = None;
    }
    let class = match disposition {
        CanonicalDisposition::Absent => CanonicalClass::Absent,
        CanonicalDisposition::Invalid => CanonicalClass::Invalid,
        CanonicalDisposition::NameOnly | CanonicalDisposition::Complete => {
            let families: std::collections::BTreeSet<&str> =
                markers.iter().map(|(_, h, _)| h.as_str()).collect();
            if families.len() > 1 {
                CanonicalClass::Contradiction
            } else if let (Some(vh), Some(ch)) = (&family, &harness) {
                if vh != ch {
                    CanonicalClass::Contradiction
                } else if let (Some(cs), Some(vs)) = (&session, &vendor_session) {
                    if !same_session_id(cs, vs) {
                        CanonicalClass::Contradiction
                    } else {
                        CanonicalClass::Canonical
                    }
                } else {
                    CanonicalClass::Canonical
                }
            } else {
                CanonicalClass::Canonical
            }
        }
    };
    CanonicalResolution {
        session_id: if class == CanonicalClass::Canonical {
            if session.is_some() {
                session
            } else {
                vendor_session
            }
        } else {
            None
        },
        harness: if class == CanonicalClass::Canonical {
            harness
        } else {
            None
        },
        class,
        markers,
    }
}

/// The identity this process can PROVE it owns.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedIdentity {
    pub session_id: Option<String>,
    pub harness: Option<String>,
    pub disposition: OwnedDisposition,
    pub markers_present: Vec<(String, String, String)>,
    /// Ids a live row already owns, for the event record.
    pub rejected: Vec<RejectedId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedDisposition {
    /// The resolved family was the only one present.
    Single,
    /// Multiple families present, one proven ours.
    Proven,
    /// A COMPLETE stamp the prover confirmed.
    AsCanonical,
    /// Cannot resolve to a specific id without guessing.
    Ambiguous,
    /// No marker present.
    Empty,
    /// The fno stamp is malformed.
    Invalid,
    /// Markers from two families (one foreign and inherited).
    Contradiction,
    /// Session id from the cwd-keyed spawn record (a codex thread worker).
    SpawnRecord,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RejectedId {
    pub harness: String,
    pub session_id: String,
    pub reason: String,
    pub owner: Option<String>,
}

/// Proof hooks. `prove(harness, id) -> Option<bool>` (None = cannot tell);
/// `collide(harness, id) -> Option<String>` (Some = a live row owns it);
/// `witness(harness) -> Vec<String>` (ids a live rollout fd witnesses).
pub type ProveHook<'a> = &'a dyn Fn(&str, &str) -> Option<bool>;
pub type CollideHook<'a> = &'a dyn Fn(&str, &str) -> Option<String>;
pub type WitnessHook<'a> = &'a dyn Fn(&str) -> Vec<String>;

fn ambiguous(present: Vec<(String, String, String)>, rejected: Vec<RejectedId>) -> OwnedIdentity {
    OwnedIdentity {
        session_id: None,
        harness: None,
        disposition: OwnedDisposition::Ambiguous,
        markers_present: present,
        rejected,
    }
}

/// Port of Python `resolve_owned_identity`: proof beats collision; a marker
/// the prover actively contradicts is excluded; among the rest the sole
/// surviving family wins or the result degrades to ambiguous. Never guesses
/// by precedence across families.
pub fn resolve_owned_identity_from(
    get: &impl Fn(&str) -> Option<String>,
    prove: Option<ProveHook>,
    collide: Option<CollideHook>,
) -> OwnedIdentity {
    let _ = &get;
    let markers = present_markers(get);
    let present = markers.clone();
    let canonical_res = resolve_canonical_resolution(get);
    if canonical_res.class == CanonicalClass::Invalid {
        return OwnedIdentity {
            session_id: None,
            harness: None,
            disposition: OwnedDisposition::Invalid,
            markers_present: present,
            rejected: Vec::new(),
        };
    }
    if canonical_res.class == CanonicalClass::Contradiction {
        return ambiguous(present, Vec::new());
    }
    if canonical_res.class == CanonicalClass::Canonical {
        match (&canonical_res.session_id, &canonical_res.harness) {
            (Some(sid), Some(h)) => {
                let verdict = prove.and_then(|p| p(h, sid));
                if verdict == Some(true) {
                    return OwnedIdentity {
                        session_id: Some(sid.clone()),
                        harness: Some(h.clone()),
                        disposition: OwnedDisposition::AsCanonical,
                        markers_present: present,
                        rejected: Vec::new(),
                    };
                }
                if !(verdict.is_none() && !present.is_empty()) {
                    // A silent prover with markers present falls to the
                    // ladder below; every other verdict (absent prover,
                    // Some(false)) still gets the collide check first.
                    let owner = collide.and_then(|c| c(h, sid));
                    if owner.is_some() {
                        return OwnedIdentity {
                            session_id: None,
                            harness: None,
                            disposition: OwnedDisposition::Ambiguous,
                            markers_present: present,
                            rejected: vec![RejectedId {
                                harness: h.clone(),
                                session_id: sid.clone(),
                                reason: "owned_by_live_row".into(),
                                owner,
                            }],
                        };
                    }
                    return ambiguous(present, Vec::new());
                }
                // A silent prover with markers present: fall to the ladder.
            }
            (None, Some(h)) => {
                if prove.is_none() || present.is_empty() {
                    return ambiguous(present, Vec::new());
                }
                // Proven family, no id: the ladder settles it.
                let _ = h;
            }
            _ => return ambiguous(present, Vec::new()),
        }
    }
    ladder(get, prove, collide, present)
}

/// The marker loop Python runs after the canonical branch: prove/collide/
/// contradict each marker, then the single-family elimination.
fn ladder(
    _get: &impl Fn(&str) -> Option<String>,
    prove: Option<ProveHook>,
    collide: Option<CollideHook>,
    present: Vec<(String, String, String)>,
) -> OwnedIdentity {
    if present.is_empty() {
        return OwnedIdentity {
            session_id: None,
            harness: None,
            disposition: OwnedDisposition::Empty,
            markers_present: Vec::new(),
            rejected: Vec::new(),
        };
    }
    let mut rejected: Vec<RejectedId> = Vec::new();
    let mut proven: Vec<(String, String, String)> = Vec::new();
    let mut contradicted = false;
    let mut unresolved: Vec<(String, String, String)> = Vec::new();
    let mut collide_memo: std::collections::HashMap<(String, String), Option<String>> =
        std::collections::HashMap::new();
    for (marker, harness, value) in &present {
        let verdict = prove.and_then(|p| p(harness, value));
        if verdict == Some(true) {
            proven.push((marker.clone(), harness.clone(), value.clone()));
            continue;
        }
        let key = (harness.clone(), session_identity_key(value));
        let owner = collide_memo
            .entry(key)
            .or_insert_with(|| collide.and_then(|c| c(harness, value)));
        if let Some(owner) = owner {
            rejected.push(RejectedId {
                harness: harness.clone(),
                session_id: value.clone(),
                reason: "owned_by_live_row".into(),
                owner: Some(owner.clone()),
            });
            continue;
        }
        if verdict == Some(false) {
            contradicted = true;
            continue;
        }
        unresolved.push((marker.clone(), harness.clone(), value.clone()));
    }
    ladder_settle(present, rejected, proven, contradicted, unresolved)
}

/// The settle rules of the elimination ladder.
fn ladder_settle(
    present: Vec<(String, String, String)>,
    rejected: Vec<RejectedId>,
    proven: Vec<(String, String, String)>,
    contradicted: bool,
    unresolved: Vec<(String, String, String)>,
) -> OwnedIdentity {
    use std::collections::BTreeMap;
    let distinct: std::collections::BTreeSet<&str> =
        present.iter().map(|(_, h, _)| h.as_str()).collect();
    let mut proven_by_family: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (_, harness, value) in &proven {
        proven_by_family
            .entry(harness.as_str())
            .or_default()
            .push(value);
    }
    if proven_by_family.len() == 1 {
        let (family, ids) = proven_by_family.iter().next().expect("len checked");
        if ids.len() == 1 {
            let value = ids[0].to_string();
            let harness = family.to_string();
            return OwnedIdentity {
                session_id: Some(value),
                harness: Some(harness),
                disposition: if distinct.len() == 1 {
                    OwnedDisposition::Single
                } else {
                    OwnedDisposition::Proven
                },
                markers_present: present,
                rejected,
            };
        }
        // Proven family but multiple DISTINCT ids: proof is harness-level,
        // not id-level. One measured exception: codex sets CODEX_SESSION_ID
        // to the ROOT session and CODEX_THREAD_ID per thread, so the thread
        // id names this process.
        if *family == "codex" {
            let thread_rows: Vec<String> = proven
                .iter()
                .filter(|(m, _, _)| m.as_str() == "CODEX_THREAD_ID")
                .map(|(_, _, v)| v.clone())
                .collect();
            if !thread_rows.is_empty() {
                return OwnedIdentity {
                    session_id: Some(thread_rows[0].to_string()),
                    harness: Some(family.to_string()),
                    disposition: if distinct.len() == 1 {
                        OwnedDisposition::Single
                    } else {
                        OwnedDisposition::Proven
                    },
                    markers_present: present,
                    rejected,
                };
            }
        }
        return ambiguous(present, rejected);
    }
    if proven_by_family.len() > 1 {
        return ambiguous(present, rejected);
    }
    // No marker proven.
    if distinct.len() == 1 && !contradicted && rejected.is_empty() && !unresolved.is_empty() {
        let family_value_keys: std::collections::BTreeSet<String> = unresolved
            .iter()
            .map(|(_, _, v)| session_identity_key(v))
            .collect();
        let single_family: String = distinct.iter().next().unwrap().to_string();
        if family_value_keys.len() > 1 {
            // The family's markers disagree on the id: unresolved,
            // never table-first.
            return ambiguous(present, rejected);
        }
        let (_m, _h, value) = &unresolved[0];
        return OwnedIdentity {
            session_id: Some(value.clone()),
            harness: Some(single_family),
            disposition: OwnedDisposition::Single,
            markers_present: present,
            rejected,
        };
    }
    ambiguous(present, rejected)
}

/// The harness a process runs as, from its command line (census argv): the
/// prove-rule port. `claude` matches by substring on the EXEC path only
/// (argv[0]; its versioned binary hides the name in the path), never on later
/// argv entries; the rest match by basename or stem (gemini.js -> gemini).
/// Returns the harness name, so a caller can prove which session id this
/// process owns.
pub fn harness_name_of_command(command: &str) -> Option<&'static str> {
    const SEGMENT_TOKENS: [&str; 5] = ["codex", "gemini", "opencode", "agy", "cursor-agent"];
    let argv: Vec<&str> = command.split_whitespace().collect();
    if argv.is_empty() {
        return None;
    }
    // argv[0] plays the name/exe role (census argv[0] is the exec path);
    // claude matches by substring there only.
    let exe = argv[0].to_lowercase();
    if exe.contains("claude") {
        return Some("claude");
    }
    for cand in argv.iter().take(2) {
        let low = cand.to_lowercase();
        let basename = low.rsplit('/').next().unwrap_or("");
        if SEGMENT_TOKENS.contains(&basename) {
            return Some(basename_into_harness(basename));
        }
        let stem = basename.rsplit('.').next().unwrap_or("");
        if SEGMENT_TOKENS.contains(&stem) {
            return Some(basename_into_harness(stem));
        }
    }
    None
}

/// Map a matched token to its harness name. Only cursor-agent needs the
/// mapping; the rest already carry their name.
fn basename_into_harness(token: &str) -> &'static str {
    if token == "cursor-agent" {
        "cursor-agent"
    } else {
        match token {
            "codex" => "codex",
            "gemini" => "gemini",
            "opencode" => "opencode",
            "agy" => "agy",
            _ => "cursor-agent",
        }
    }
}

/// How far up the parent chain to walk before giving up (Python parity:
/// `_MAX_DEPTH`/`_MAX_ANCESTRY_DEPTH`).
const MAX_ANCESTRY_DEPTH: usize = 25;

/// The nearest harness ancestor's name walking UP from `start_pid` over
/// `table` (pid -> row). Python parity of `session_pid.resolve_session_harness`
/// minus the env stamps, which the ambient wrapper reads first. First match
/// wins: a claude worker spawned by codex anchors to claude, not codex.
pub fn resolve_session_harness_from_table(
    table: &BTreeMap<u32, ProcRow>,
    start_pid: u32,
) -> Option<&'static str> {
    let mut pid = start_pid;
    let mut depth = 0;
    while depth < MAX_ANCESTRY_DEPTH {
        let row = table.get(&pid)?;
        let command = row.command.as_str();
        if let Some(harness) = harness_name_of_command(command) {
            return Some(harness);
        }
        let ppid = row.ppid;
        if ppid == 0 || ppid == pid {
            return None;
        }
        pid = ppid;
        depth += 1;
    }
    None
}

/// The census table as a pid -> row map, for the walks.
pub fn ancestry_table() -> BTreeMap<u32, ProcRow> {
    let (rows, _unreadable) = census::process_table();
    rows.into_iter().map(|r| (r.pid, r)).collect()
}

/// Ambient wrapper: the launcher-stamped proof pair
/// (`FNO_SESSION_PID` + `FNO_SESSION_HARNESS`) leads; otherwise the walk from
/// this process's ppid over the live census table. Degrades to `None` when no
/// harness ancestor is found (a plain shell), so "unproven" and "no ancestor"
/// are the same answer.
pub fn resolve_session_harness_ambient() -> Option<&'static str> {
    let stamp = std::env::var("FNO_SESSION_HARNESS").unwrap_or_default();
    let stamp = stamp.trim().to_lowercase();
    if [
        "claude",
        "codex",
        "gemini",
        "opencode",
        "agy",
        "cursor-agent",
    ]
    .contains(&stamp.as_str())
    {
        let pid_raw = std::env::var("FNO_SESSION_PID").unwrap_or_default();
        if let Ok(pid) = pid_raw.trim().parse::<i64>() {
            if pid > 0 && stamp_pid_is_live() {
                return harness_static(&stamp);
            }
        }
    }
    let table = ancestry_table();
    let ppid: u32 = unsafe { libc::getppid() } as u32;
    resolve_session_harness_from_table(&table, ppid)
}

/// The stamp pair's pid half: `FNO_SESSION_PID` must parse to a positive,
/// live pid, or the stamp is ignored and the walk decides.
fn stamp_pid_is_live() -> bool {
    let pid_raw = std::env::var("FNO_SESSION_PID").unwrap_or_default();
    match pid_raw.trim().parse::<u32>() {
        Ok(0) | Err(_) => false,
        Ok(pid) => unsafe { libc::kill(pid as libc::pid_t, 0) == 0 },
    }
}

/// Static helper so the stamp arm returns a `&'static str` tied to the table
/// of known harnesses, not the env string.
fn harness_static(stamp: &str) -> Option<&'static str> {
    match stamp {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "gemini" => Some("gemini"),
        "opencode" => Some("opencode"),
        "agy" => Some("agy"),
        "cursor-agent" => Some("cursor-agent"),
        _ => None,
    }
}

/// The value of `marker` in `pid`'s environment, or `None` when the env is
/// unreadable or the marker absent. macOS reads the process args buffer
/// natively (`KERN_PROCARGS2`, the same source `census::argv_of` uses, whose
/// tail after argv is the environment); Linux reads `/proc/<pid>/environ`.
/// Readability is PARTIAL by measurement (Python's `ps eww` carrier had the
/// same property); `None` covers both "not carried" and "unreadable" because
/// neither changes a caller decision.
#[cfg(target_os = "macos")]
fn ancestor_env_marker(pid: u32, marker: &str) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < 4
    {
        return None;
    }
    let mut buf = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let buf = &buf[..size];
    let argc = i32::from_ne_bytes(buf[0..4].try_into().ok()?) as usize;
    if argc == 0 {
        return None;
    }
    let mut i = 4;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    for _ in 0..argc {
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        i += 1;
    }
    let prefix = format!("{marker}=");
    while i < buf.len() {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if let Ok(text) = std::str::from_utf8(&buf[start..i]) {
            if let Some(v) = text.strip_prefix(prefix.as_str()) {
                return Some(v.to_string());
            }
        }
        i += 1;
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn ancestor_env_marker(pid: u32, marker: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{marker}=");
    for entry in raw.split(|b| *b == 0) {
        if let Ok(text) = std::str::from_utf8(entry) {
            if let Some(v) = text.strip_prefix(prefix.as_str()) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// One ancestor in the attester walk: its argv0 basename (lowercased, the
/// family-carrier test) and the marker value its env carries, if readable.
pub struct AncestorFacts {
    pub exe_basename: String,
    pub marker_value: Option<String>,
}

/// The ancestry chain for MARKER, nearest parent first, stopping AFTER the
/// nearest process whose argv0 basename contains the marker's harness family
/// token: the nearest family carrier decides, READABLE OR NOT (Python parity;
/// walking past it would let a farther stale family carrier veto). Unreadable
/// ppid ends the chain; depth bounded at 25.
pub fn attester_chain(table: &BTreeMap<u32, ProcRow>, marker: &str) -> Vec<AncestorFacts> {
    let family_token = HARNESS_SESSION_MARKERS
        .iter()
        .find(|(m, _)| *m == marker)
        .map(|(_, h)| *h)
        .unwrap_or("");
    let mut chain = Vec::new();
    let mut pid: u32 = unsafe { libc::getppid() } as u32;
    let mut depth = 0;
    while depth < MAX_ANCESTRY_DEPTH {
        let row = match table.get(&pid) {
            Some(r) => r,
            None => break,
        };
        let exe_basename = row
            .command
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let carrier = !family_token.is_empty() && exe_basename.contains(family_token);
        chain.push(AncestorFacts {
            exe_basename,
            marker_value: ancestor_env_marker(pid, marker),
        });
        if carrier {
            break;
        }
        pid = row.ppid;
        if pid == 0 {
            break;
        }
        depth += 1;
    }
    chain
}

/// The witness for a marker value from its ancestry. The NEAREST family
/// carrier decides: agree -> `Process`; disagree -> `Conflict`; no family
/// carrier at all -> `EnvOnly`. Mirror of Python `_attester_witness`.
pub enum AttesterWitness {
    Process,
    EnvOnly,
    Conflict(String, String, String),
}

pub fn witness_from_chain(
    marker: &str,
    session_id: &str,
    chain: &[AncestorFacts],
) -> AttesterWitness {
    for ancestor in chain {
        let Some(value) = &ancestor.marker_value else {
            continue;
        };
        if !ancestor.exe_basename.contains(family_token_of(marker)) {
            continue;
        }
        if value != session_id {
            return AttesterWitness::Conflict(
                marker.to_string(),
                value.clone(),
                session_id.to_string(),
            );
        }
        return AttesterWitness::Process;
    }
    AttesterWitness::EnvOnly
}

fn family_token_of(marker: &str) -> &str {
    HARNESS_SESSION_MARKERS
        .iter()
        .find(|(m, _)| *m == marker)
        .map(|(_, h)| *h)
        .unwrap_or("")
}

/// Port of Python `resolve_attester_identity`: the winning marker value from
/// the shared precedence, corroborated against the process ancestry. A
/// mixed-family env resolves empty. Returns `(session_id, witness)`.
pub fn resolve_attester_identity(
    get: &impl Fn(&str) -> Option<String>,
) -> (Option<String>, AttesterWitness) {
    let markers = present_markers(get);
    let mut families: Vec<String> = Vec::new();
    let mut family_conflicted = false;
    let mut winner: Option<(String, String)> = None;
    for (marker, harness, value) in &markers {
        if !families.contains(harness) {
            families.push(harness.clone());
        }
        // Track a same-family disagreement.
        let _ = &marker;
        match &winner {
            None => winner = Some((marker.clone(), value.clone())),
            Some((_, seen)) => {
                if harness == &families[0] && !same_session_id(seen, value) {
                    family_conflicted = true;
                }
            }
        }
    }
    if families.len() > 1 || winner.is_none() {
        return (None, AttesterWitness::EnvOnly);
    }
    let (marker, session_id) = winner.expect("checked");
    let table = ancestry_table();
    let chain = attester_chain(&table, &marker);
    let witness = witness_from_chain(&marker, &session_id, &chain);
    if !matches!(witness, AttesterWitness::Process) && family_conflicted {
        // Unreadable ancestry cannot resolve a family disagreement either:
        // unresolved, never the table-first value.
        return (None, AttesterWitness::EnvOnly);
    }
    (Some(session_id), witness)
}

/// The row statuses under which a session still owns its identity. Mirror of
/// Python `OWNERSHIP_LIVE_STATUSES`.
const OWNERSHIP_LIVE_STATUSES: [&str; 6] =
    ["spawning", "ready", "idle", "busy", "live", "restarting"];

/// The `(harness, session_id)` of the ONE live codex row holding `cwd`,
/// read from the registry. Exactly one ownership-live `thread`/`pane` row
/// with a non-empty harness and session id answers; zero and two-plus
/// matches return `None`. Mirror of Python `live_thread_row_for_cwd`.
pub fn live_thread_row_for_cwd(cwd: &str, home: &AgentsHome) -> Option<(String, String)> {
    if cwd.is_empty() {
        return None;
    }
    let registry = load_registry(&home.registry_json()).ok()?;
    let wanted = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.into());
    let mut matches: Vec<(String, String)> = Vec::new();
    for row in &registry.entries {
        let status_word = match row.status {
            crate::AgentStatus::Spawning => "spawning",
            crate::AgentStatus::Ready => "ready",
            crate::AgentStatus::Idle => "idle",
            crate::AgentStatus::Busy => "busy",
            crate::AgentStatus::Live => "live",
            crate::AgentStatus::Restarting => "restarting",
            _ => "",
        };
        if !OWNERSHIP_LIVE_STATUSES.contains(&status_word) {
            continue;
        }
        let substrate = row.substrate.as_deref().unwrap_or_default();
        if substrate != "thread" && substrate != "pane" {
            continue;
        }
        let (Some(harness), Some(session_id)) = (&row.harness, &row.harness_session_id) else {
            continue;
        };
        let row_cwd = std::path::Path::new(row.cwd.as_str());
        let row_real = std::fs::canonicalize(row_cwd).unwrap_or_else(|_| row_cwd.to_path_buf());
        if row_real != wanted {
            continue;
        }
        matches.push((harness.clone(), session_id.clone()));
    }
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

/// Fill a session id the ancestry walk could not supply from the cwd-keyed
/// spawn record. Guards, in order: a resolved session id
/// short-circuits before the read, a fail-closed disposition is never
/// overwritten, and a process carrying ANY other family's marker never
/// adopts (cwd is shared by bystanders).
fn fill_spawn_record(owned: OwnedIdentity, cwd: &str, home: &AgentsHome) -> OwnedIdentity {
    if owned.session_id.is_some()
        || matches!(
            owned.disposition,
            OwnedDisposition::Invalid | OwnedDisposition::Contradiction
        )
    {
        return owned;
    }
    let Some((harness, session_id)) = live_thread_row_for_cwd(cwd, home) else {
        return owned;
    };
    if owned.harness.as_deref().is_some_and(|h| h != harness) {
        return owned;
    }
    let marker_families: std::collections::BTreeSet<&str> = owned
        .markers_present
        .iter()
        .map(|(_, h, _)| h.as_str())
        .collect();
    if marker_families.iter().any(|h| h != &harness.as_str()) {
        return owned;
    }
    OwnedIdentity {
        session_id: Some(session_id),
        harness: Some(harness),
        disposition: OwnedDisposition::SpawnRecord,
        ..owned
    }
}

/// The composition Python calls `resolve_self_identity`: the process-tree
/// prover leads, the attester corroborates a COMPLETE stamp, the elimination
/// ladder resolves the rest, and the cwd-keyed spawn record fills a session
/// id ancestry cannot supply.
pub fn resolve_self_identity(
    get: &impl Fn(&str) -> Option<String>,
    collide: Option<CollideHook>,
    witness: Option<WitnessHook>,
    home: &AgentsHome,
) -> OwnedIdentity {
    let true_harness = resolve_session_harness_ambient();
    let (stamp_session, stamp_harness, stamp_disposition) = parse_canonical_stamp(get);

    if !matches!(
        stamp_disposition,
        CanonicalDisposition::Complete | CanonicalDisposition::NameOnly
    ) {
        // No usable stamp: resolve by the ladder with the walk as the only
        // prover, no collide (a session with no stamp at all resolves by the
        // uncontended single-family elimination, exactly as SessionStart
        // registration expects).
        let prove: ProveHook = &|h, _| true_harness.map(|t| h == t);
        let owned = resolve_owned_identity_from(get, Some(prove), None);
        let cwd = ambient_cwd();
        return fill_spawn_record(owned, &cwd, home);
    }

    let (attested_session_id, attester_witness) = resolve_attester_identity(get);
    let mut canonical_session_id = stamp_session.clone().or(attested_session_id.clone());
    let mut canonical_proven = true_harness.is_some()
        && stamp_harness.as_deref() == true_harness
        && matches!(attester_witness, AttesterWitness::Process)
        && canonical_session_id.is_some()
        && attested_session_id.is_some()
        && same_session_id(
            canonical_session_id.as_deref().unwrap_or_default(),
            attested_session_id.as_deref().unwrap_or_default(),
        );

    // a name_only codex stamp carries no id; a rollout witness value
    // seen in a live fd IS this process's id (the thread id wins).
    let mut witnessed_value: Option<String> = None;
    if let (Some(witness_hook), Some(th)) = (witness, true_harness) {
        if !canonical_proven
            && stamp_disposition == CanonicalDisposition::NameOnly
            && stamp_harness.as_deref() == Some(th)
        {
            let environ_thread = get("CODEX_THREAD_ID")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
            let seen: std::collections::BTreeSet<String> = witness_hook(th)
                .iter()
                .map(|s| session_identity_key(s))
                .collect();
            if let Some(thread) = environ_thread {
                if seen.contains(&session_identity_key(&thread)) {
                    witnessed_value = Some(thread);
                }
            }
            if witnessed_value.is_none() {
                let candidates: Vec<String> = present_markers(get)
                    .iter()
                    .filter(|(_, h, v)| h == th && seen.contains(&session_identity_key(v)))
                    .map(|(_, _, v)| v.clone())
                    .collect();
                if candidates.len() == 1 {
                    witnessed_value = candidates.into_iter().next();
                }
            }
            if witnessed_value.is_some() {
                canonical_session_id = witnessed_value.clone();
                canonical_proven = true;
            }
        }
    }

    // The stamp-declared pair: a COMPLETE stamp names the id independently of
    // the markers under test; a name_only stamp completes NO pair.
    let own_pair: Option<(String, String)> = match (&stamp_harness, &canonical_session_id) {
        (Some(h), Some(s)) if stamp_disposition == CanonicalDisposition::Complete => {
            Some((h.to_lowercase(), session_identity_key(s)))
        }
        _ => witnessed_value
            .as_ref()
            .zip(true_harness)
            .map(|(s, h)| (h.to_string(), session_identity_key(s))),
    };

    let prove: ProveHook = &|h, sid| {
        let t = true_harness?;
        if h != t {
            return Some(false);
        }
        if !canonical_proven {
            return None;
        }
        Some(same_session_id(
            sid,
            canonical_session_id.as_deref().unwrap_or_default(),
        ))
    };
    let collide_impl = move |h: &str, s: &str| -> Option<String> {
        // A row agreeing with the own pair on both halves is the caller's
        // OWN row, never contention.
        if let Some((oh, os)) = &own_pair {
            if oh == h && os == &session_identity_key(s) {
                return None;
            }
        }
        collide.and_then(|c| c(h, s))
    };
    let collide_wrapped: Option<CollideHook> = if canonical_proven {
        None
    } else {
        Some(&collide_impl)
    };
    let owned = resolve_owned_identity_from(get, Some(prove), collide_wrapped);
    let cwd = ambient_cwd();
    fill_spawn_record(owned, &cwd, home)
}

fn ambient_cwd() -> String {
    std::env::var("PWD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .unwrap_or_default()
}

/// Stamp the caller's proven initiating context onto a daemon-bound spawn
/// request: the client is the earliest boundary that can prove the
/// caller, so the proof is captured ONCE here and serialized across the RPC
/// boundary; the daemon consumes the request values and never recaptures
/// from its own scrubbed environment. When the owned proof resolves to a
/// session (or a cwd-keyed spawn record names one), the request carries a
/// `Session` origin with the proven parent and a `Session` owner; an
/// unproven caller sends no stamp and the daemon keeps today's ambient
/// capture until the door enforces (schema v33 rollout).
pub fn stamp_spawn_lineage(params: &mut serde_json::Map<String, Value>) -> Result<(), String> {
    // Explicit dispatch context outranks ambient capture: a daemon
    // producer (mission drain, blueprinter) exports FNO_SPAWN_ORIGIN +
    // FNO_SPAWN_OWNER naming its arm and the responsible mission/crown, and
    // those ride the request verbatim. A malformed carrier is a producer bug
    // and refuses by name instead of falling back silently.
    let carried = match (
        std::env::var("FNO_SPAWN_ORIGIN").ok(),
        std::env::var("FNO_SPAWN_OWNER").ok(),
    ) {
        (Some(o), Some(w)) => Some((o, w)),
        (Some(_), None) | (None, Some(_)) => {
            return Err("FNO_SPAWN_ORIGIN and FNO_SPAWN_OWNER must be exported together".into())
        }
        (None, None) => None,
    };
    if let Some((origin_raw, owner_raw)) = carried {
        let origin: crate::spawn_contract::SpawnOrigin =
            serde_json::from_str(&origin_raw).map_err(|e| {
                format!("FNO_SPAWN_ORIGIN is malformed ({e}); the producer carrier must speak the door's vocabulary")
            })?;
        let owner: crate::spawn_contract::SpawnOwner =
            serde_json::from_str(&owner_raw).map_err(|e| {
                format!("FNO_SPAWN_OWNER is malformed ({e}); the producer carrier must speak the door's vocabulary")
            })?;
        let request = crate::spawn_contract::SpawnRequest::new(
            origin,
            owner,
            crate::spawn_contract::SpawnHow::new("claude", "headless"),
            crate::spawn_contract::SpawnWork::new("carrier", "", "."),
        );
        crate::spawn_contract::validate(&request)
            .map_err(|e| format!("FNO_SPAWN_ORIGIN/FNO_SPAWN_OWNER refused by the door: {e}"))?;
        let origin_value: Value = serde_json::from_str(&origin_raw).unwrap_or(Value::Null);
        let owner_value: Value = serde_json::from_str(&owner_raw).unwrap_or(Value::Null);
        params.insert("origin".into(), origin_value);
        params.insert("owner".into(), owner_value);
        return Ok(());
    }
    let get = |k: &str| std::env::var(k).ok();
    let home = crate::paths::AgentsHome::from_env();
    let owned = resolve_self_identity(&get, None, None, &home);
    let (Some(session_id), Some(harness)) = (owned.session_id, owned.harness) else {
        return Ok(());
    };
    let cwd = std::env::var("PWD")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
    if cwd.is_empty() {
        return Ok(());
    }
    let parent = crate::spawn_contract::SessionRef {
        harness,
        session_id,
        cwd,
    };
    params.insert(
        "origin".into(),
        serde_json::to_value(&crate::spawn_contract::SpawnOrigin::Session {
            parent: parent.clone(),
            invocation: None,
        })
        .unwrap_or(Value::Null),
    );
    params.insert(
        "owner".into(),
        serde_json::to_value(&crate::spawn_contract::SpawnOwner::Session(parent))
            .unwrap_or(Value::Null),
    );
    Ok(())
}

/// The tier-remap refusal, native: a claude
/// spawn naming a tier alias the ambient env redefines to a FOREIGN vendor's
/// model id refuses before launch. Endpoint and credential resolve separately
/// from the alias, so the worker would report live and die on its first turn.
/// The Python seam (`rust_runtime.inherited_tier_remap`) enforces the same
/// rule for Python-routed spawns; this closes the direct-native entry, which
/// previously had no guard.
pub fn refuse_inherited_tier_remap(params: &serde_json::Map<String, Value>) -> Result<(), String> {
    refuse_inherited_tier_remap_with(params, |k| std::env::var(k).ok())
}

pub fn refuse_inherited_tier_remap_with(
    params: &serde_json::Map<String, Value>,
    env_get: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    const TIER_ALIASES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];
    let harness = params
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("claude")
        .trim()
        .to_lowercase();
    if harness != "claude" {
        return Ok(());
    }
    // A composed route (route named, or an account pinned) chooses endpoint,
    // auth and model as one unit and is exempt, same as the Python seam.
    if params.contains_key("route") || params.contains_key("account") {
        return Ok(());
    }
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !TIER_ALIASES.contains(&model.as_str()) {
        return Ok(());
    }
    let var = format!("ANTHROPIC_DEFAULT_{}_MODEL", model.to_uppercase());
    let remapped = env_get(&var).unwrap_or_default().trim().to_string();
    if remapped.is_empty() {
        return Ok(());
    }
    // Pinning the tier to a specific Anthropic model is a supported
    // customization, not a conflict.
    let name = remapped.to_lowercase();
    let anthropic = name.starts_with("claude-") || TIER_ALIASES.contains(&name.as_str());
    if anthropic {
        return Ok(());
    }
    Err(format!(
        "--model {model} is ambiguous here. This session exports {var}={remapped}, so '{model}' \
         resolves to that vendor's model id, while the worker's endpoint and credential are \
         resolved separately and would not match it. The spawn would report \"live\" and then \
         fail on its first turn. No worker launched.\n\
         Name the route instead, so endpoint, auth, and model are chosen as one unit:\n\
           --account <id> --model {model}      # Anthropic's {model} (ids from `fno config accounts list`)\n\
           -P <vendor> --model {remapped}   # stay on the routed vendor (vendors from `fno config route ls`)\n\
         Or unset {var} for this command."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn single_family_resolves() {
        let get = env_of(&[("CODEX_THREAD_ID", "abc-123")]);
        let owned = resolve_owned_identity_from(&get, None, None);
        assert_eq!(owned.session_id.as_deref(), Some("abc-123"));
        assert_eq!(owned.harness.as_deref(), Some("codex"));
        assert_eq!(owned.disposition, OwnedDisposition::Single);
    }

    #[test]
    fn two_families_never_launder() {
        let get = env_of(&[
            ("CODEX_THREAD_ID", "abc-123"),
            ("CLAUDE_CODE_SESSION_ID", "uuid-claude"),
        ]);
        let owned = resolve_owned_identity_from(&get, None, None);
        assert_eq!(owned.session_id, None);
        assert_eq!(owned.harness, None);
        assert_eq!(owned.disposition, OwnedDisposition::Ambiguous);
    }

    #[test]
    fn same_family_disagreeing_ids_refuse() {
        let get = env_of(&[
            ("CODEX_THREAD_ID", "abc-123"),
            ("CODEX_SESSION_ID", "zzz-999"),
        ]);
        let owned = resolve_owned_identity_from(&get, None, None);
        assert_eq!(owned.session_id, None);
        assert_eq!(owned.disposition, OwnedDisposition::Ambiguous);
    }

    #[test]
    fn complete_stamp_proves_and_beats_collision() {
        let get = env_of(&[
            ("FNO_HARNESS_NAME", "claude"),
            ("FNO_HARNESS_SESSION_ID", "uuid-1"),
            ("CLAUDE_CODE_SESSION_ID", "uuid-1"),
        ]);
        let prove: ProveHook = &|_, _| Some(true);
        let collide: CollideHook = &|_, _| Some("someone-else".to_string());
        let owned = resolve_owned_identity_from(&get, Some(prove), Some(collide));
        assert_eq!(owned.session_id.as_deref(), Some("uuid-1"));
        // Proof is self: a live row holding this id is the session's own row.
        assert!(owned.rejected.is_empty(), "{:?}", owned.rejected);
    }

    #[test]
    fn contested_stamp_id_rejects_named() {
        let get = env_of(&[
            ("FNO_HARNESS_NAME", "claude"),
            ("FNO_HARNESS_SESSION_ID", "uuid-2"),
        ]);
        let collide: CollideHook = &|_, _| Some("owner-x".to_string());
        let owned = resolve_owned_identity_from(&get, None, Some(collide));
        assert_eq!(owned.disposition, OwnedDisposition::Ambiguous);
        assert_eq!(owned.rejected.len(), 1);
        assert_eq!(owned.rejected[0].owner.as_deref(), Some("owner-x"));
    }

    #[test]
    fn invalid_stamp_refuses() {
        let get = env_of(&[("FNO_HARNESS_NAME", "")]);
        let owned = resolve_owned_identity_from(&get, None, None);
        assert_eq!(owned.disposition, OwnedDisposition::Invalid);
    }

    #[test]
    fn empty_env_reads_empty() {
        let get = env_of(&[]);
        let owned = resolve_owned_identity_from(&get, None, None);
        assert_eq!(owned.disposition, OwnedDisposition::Empty);
    }

    #[test]
    fn proven_beats_foreign_family() {
        let get = env_of(&[
            ("CODEX_THREAD_ID", "foreign-thread"),
            ("CLAUDE_CODE_SESSION_ID", "mine"),
        ]);
        let prove: ProveHook = &|h, _| if h == "claude" { Some(true) } else { None };
        let owned = resolve_owned_identity_from(&get, Some(prove), None);
        assert_eq!(owned.session_id.as_deref(), Some("mine"));
        assert_eq!(owned.harness.as_deref(), Some("claude"));
        assert_eq!(owned.disposition, OwnedDisposition::Proven);
    }

    #[test]
    fn session_key_case_rules() {
        assert_eq!(session_identity_key("ses_ABC"), "ses_ABC");
        assert_eq!(
            session_identity_key("ABC-DEF"),
            session_identity_key("abc-def")
        );
    }

    #[test]
    fn shape_harness_port_lives_in_contract() {
        // The id-shape helper is exercised in spawn_contract tests; here we
        // pin the cross-module contract: the door validates a proven session
        // parent and refuses a shape contradiction.
        use crate::spawn_contract::{
            validate, SessionRef, SpawnHow, SpawnOrigin, SpawnOwner, SpawnRequest, SpawnWork,
        };
        let req = SpawnRequest::new(
            SpawnOrigin::Session {
                parent: SessionRef {
                    harness: "claude".into(),
                    session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
                    cwd: "/repo".into(),
                },
                invocation: None,
            },
            SpawnOwner::Operator {
                tty: "/dev/ttys001".into(),
            },
            SpawnHow::new("claude", "bg"),
            SpawnWork::new("w", "seed", "/repo"),
        );
        assert!(validate(&req).is_ok());
    }
}
