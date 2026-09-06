//! `reign_state` + the shape rewrite: who is reigning, in what shape, is it live.
//!
//! Module name starts with "loop" for the same LOC-ratchet reason as
//! `loop_king.rs`: its lines must count toward the `crates/fno-agents/src/loop*`
//! control-plane glob.
//!
//! Ported from the Python reader (`cli/src/fno/king/state.py`, x-7b36): the
//! Python tree is shrink-only as a whole, so the compute landed here and the
//! Python side kept a thin JSON client plus the CLI shell. The port is
//! behavior-identical, refusal strings included, because the escalate closing
//! sentence and the tests key on those substrings.
//!
//! The contract the port preserves: every unknownable field answers `None`
//! with `unknown_reason` naming the unreadable side, never a clean `false`.
//! The four consumers (escalate's closing sentence, the Stop nudge's court
//! branch, court's split count, the crown-liveness monitor) each act
//! differently on "absent" versus "cannot read", and flattening the two is the
//! shared root this reader exists to end.
//!
//! CLI surface (direct dispatch in `bin/client.rs`, not routable `fno agents`
//! verbs, same reasoning as `kill-check`):
//!   `fno-agents reign-state [--scope S | --session ID] [--root PATH]`
//!     one JSON ReignState on stdout, exit 0; unknowns are encoded in the JSON.
//!   `fno-agents reign-shape --scope S --shape pass|court [--session ID]`
//!     rewrites `shape` on the scope's manifest under the manifest lock;
//!     exit 0 prints the shape now on the file, exit 1 carries the refusal.

use crate::loop_king::scopes_overlap;
use crate::state::{load_registry, RegistryEntry};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The four terminal row statuses, Python's `TERMINAL_STATUSES` exactly.
fn is_terminal(row: &RegistryEntry) -> bool {
    matches!(
        row.status,
        crate::AgentStatus::Exited
            | crate::AgentStatus::Orphaned
            | crate::AgentStatus::Failed
            | crate::AgentStatus::PermanentDead
    )
}

/// One read of who is reigning, over what, in what shape, and is it live.
/// Field names mirror the Python dataclass the JSON client deserializes into.
#[derive(Debug, Default, Serialize)]
pub struct ReignState {
    pub crowned: Option<bool>,
    pub scope: Option<String>,
    /// `pass` | `court` from the manifest; `None` when the manifest side is
    /// unreadable. A readable manifest written before the field existed reads
    /// as `pass`, the value its writer would have recorded.
    pub shape: Option<String>,
    pub manifest_session: Option<String>,
    pub registry_session: Option<String>,
    /// The crown holder's row is live (non-terminal). `None` = unreadable.
    pub live: Option<bool>,
    /// `Some(true)` only when BOTH sides were read and name different
    /// sessions. `None` when either side is unknown - a vacated crown and an
    /// unreadable manifest are not disagreements, and must never render as one.
    pub split: Option<bool>,
    /// The manifest file's path, when it exists (x-f0d2): the manifest is the
    /// durable crown record, and court names where the record lives.
    pub manifest_path: Option<String>,
    /// The manifest carries `crown_scope` equal to the scope: a durable crown
    /// copy exists, not just identity (a pre-fields manifest holds the holder
    /// id but no crown to restore). `None` when the manifest side is unreadable.
    pub crown_on_manifest: Option<bool>,
    pub unknown_reason: Option<String>,
}

/// Same unsafe-scope refusal as `king_manifest_path`: scope becomes a filename
/// here, so two spellings of one scope must never select two files and no
/// scope may escape the state root.
fn manifest_path(root: &Path, scope: &str) -> Result<PathBuf, String> {
    let scope = scope.trim();
    if scope.is_empty()
        || scope.contains("..")
        || scope.contains('/')
        || scope.contains('\\')
        || scope.contains('\0')
    {
        return Err(format!("unsafe king scope for manifest path: {scope:?}"));
    }
    // `root` is the state root itself (the repo's space), matching Python's
    // king_manifest_path: kings sit at <root>/kings, NOT <root>/.fno/kings.
    Ok(root.join("kings").join(format!("{scope}.md")))
}

/// The row's canonical session id, tolerating legacy rows without one
/// (Python's `_row_session`).
fn row_session(row: &RegistryEntry) -> Option<String> {
    row.harness_session_id
        .clone()
        .or_else(|| row.cc_session_id.clone())
}

/// Python `_find_by_session`, both of its forms. A known harness scopes the
/// match to rows of that harness and to exact ids only; `None` (the explicit
/// session form, and any harness Python could not resolve) keeps the original
/// claude-shaped scan: exact id match first, then the claude 8-hex `short_id`
/// prefix (a 32-bit jobId is a prefix of a claude session uuid). Without the
/// scoping, a non-claude caller's uuid could fall through to an unrelated
/// claude row's short_id prefix.
fn find_by_session<'a>(
    rows: &'a [RegistryEntry],
    sid: &str,
    harness: Option<&str>,
) -> Option<&'a RegistryEntry> {
    let exact = |r: &RegistryEntry| {
        r.harness_session_id.as_deref() == Some(sid) || r.cc_session_id.as_deref() == Some(sid)
    };
    match harness {
        Some(h) if h != "claude" => rows
            .iter()
            .find(|r| r.harness.as_deref() == Some(h) && exact(r)),
        _ => rows.iter().find(|r| exact(r)).or_else(|| {
            rows.iter().find(|r| {
                r.harness.as_deref() == Some("claude")
                    && !r.short_id.is_empty()
                    && sid.len() >= 8
                    && sid.starts_with(&r.short_id)
            })
        }),
    }
}

/// `(manifest_session, shape, crown_on_manifest)` from one content read;
/// `(None, None, None)` when the file cannot read. A manifest from before
/// `shape` existed reads as its writer's default, not as a third unknown
/// shape. `crown_on_manifest` answers durability, not identity: a manifest
/// written before crown fields existed holds no crown copy to restore.
fn read_manifest_identity(path: &Path) -> (Option<String>, Option<String>, Option<bool>) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None, None),
    };
    let Some(m) = crate::loopcheck::parse_king_manifest(&content) else {
        return (None, None, None);
    };
    let session = m.harness_session_id.filter(|s| !s.is_empty());
    // An empty shape is a manifest from before the field existed: its writer's
    // default, not a third unknown shape.
    let shape = if m.shape.is_empty() {
        "pass".to_string()
    } else {
        m.shape
    };
    let crown = Some(crate::claude_adopt::manifest_field(&content, "crown_scope").is_some());
    (session, Some(shape), crown)
}

/// Fill the manifest limb of an otherwise-answered state, in place
/// (Python's `_with_manifest`).
fn with_manifest(mut state: ReignState, scope: &str, root: &Path) -> ReignState {
    let path = match manifest_path(root, scope) {
        Ok(p) => p,
        Err(_) => {
            if state.unknown_reason.is_none() {
                state.unknown_reason = Some(format!("unsafe scope for manifest: {scope:?}"));
            }
            return state;
        }
    };
    if !path.is_file() {
        if state.unknown_reason.is_none() {
            state.unknown_reason = Some(format!("no manifest at {}", path.display()));
        }
        return state;
    }
    state.manifest_path = Some(path.display().to_string());
    let (session, shape, crown) = read_manifest_identity(&path);
    state.manifest_session = session;
    state.shape = shape;
    state.crown_on_manifest = crown;
    if state.manifest_session.is_none() {
        // Unreadable, or readable with no session to compare; shape may still
        // have read, and each gap names itself.
        if state.unknown_reason.is_none() {
            let reason = if state.shape.is_none() {
                format!("manifest unreadable: {}", path.display())
            } else {
                format!("manifest names no session: {}", path.display())
            };
            state.unknown_reason = Some(reason);
        }
        return state;
    }
    if let Some(registry_session) = state.registry_session.as_deref() {
        state.split = Some(state.manifest_session.as_deref() != Some(registry_session));
    }
    state
}

/// Load the registry with Python's missing-file semantics: absent reads as
/// empty (nothing crowned), damage reads as an error the caller names.
fn load_rows(path: &Path) -> Result<Vec<RegistryEntry>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    load_registry(path)
        .map(|r| r.entries)
        .map_err(|e| format!("registry unreadable: {e}"))
}

/// The reader. With `scope`, the registry crown rows over that territory are
/// the authority and the manifest is corroborated against them. With `session`
/// instead, the caller's own row resolves the scope exactly as the Python
/// reader's caller form does.
pub fn reign_state(
    root: &Path,
    scope: Option<&str>,
    session: Option<&str>,
    harness: Option<&str>,
    registry_path: &Path,
) -> ReignState {
    reign_state_with_projects(
        root,
        scope,
        session,
        harness,
        registry_path,
        &crate::king_board::project_map(root),
    )
}

fn reign_state_with_projects(
    root: &Path,
    scope: Option<&str>,
    session: Option<&str>,
    harness: Option<&str>,
    registry_path: &Path,
    projects_result: &Result<HashMap<String, String>, String>,
) -> ReignState {
    let rows = match load_rows(registry_path) {
        Ok(rows) => rows,
        Err(e) => {
            // The registry is the crown authority, so crowned/live/split are
            // unanswerable. The manifest (scope permitting) still reads: shape
            // is a file fact, and starving the caller of it because a
            // different instrument broke is the absence-lie this reader
            // refuses. An unsafe scope must degrade the manifest side to
            // unknown, never add a second error to the named reason.
            let mut reason = e.clone();
            let mut manifest_session = None;
            let mut shape = None;
            let mut manifest_path_field = None;
            let mut crown_on_manifest = None;
            if let Some(s) = scope {
                match manifest_path(root, s) {
                    Ok(path) => {
                        if path.is_file() {
                            manifest_path_field = Some(path.display().to_string());
                        }
                        let (m, sh, cr) = read_manifest_identity(&path);
                        manifest_session = m;
                        shape = sh;
                        crown_on_manifest = cr;
                    }
                    Err(_) => reason = format!("{e}; unsafe scope {s:?}"),
                }
            }
            return ReignState {
                crowned: None,
                scope: scope.map(str::to_string),
                shape,
                manifest_session,
                manifest_path: manifest_path_field,
                crown_on_manifest,
                registry_session: None,
                live: None,
                split: None,
                unknown_reason: Some(reason),
            };
        }
    };

    let scope = match scope {
        Some(s) => s.to_string(),
        None => {
            // Caller form: the caller's own row resolves the scope.
            let Some(sid) = session.filter(|s| !s.is_empty()) else {
                return ReignState {
                    crowned: Some(false),
                    live: Some(false),
                    unknown_reason: Some(
                        "no session identity: not resolvable to a crown".to_string(),
                    ),
                    ..Default::default()
                };
            };
            let Some(row) = find_by_session(&rows, sid, harness) else {
                return ReignState {
                    crowned: Some(false),
                    live: Some(false),
                    unknown_reason: Some(format!("no registry row matches session {sid}")),
                    ..Default::default()
                };
            };
            if is_terminal(row) {
                return ReignState {
                    crowned: Some(false),
                    live: Some(false),
                    unknown_reason: Some(format!("row status is terminal ({:?})", row.status)),
                    ..Default::default()
                };
            }
            let registry_session = row_session(row);
            let Some(own) = row.crown_scope.clone().filter(|s| !s.trim().is_empty()) else {
                return ReignState {
                    crowned: Some(false),
                    registry_session,
                    live: Some(false),
                    unknown_reason: Some("row holds no crown".to_string()),
                    ..Default::default()
                };
            };
            return with_manifest(
                ReignState {
                    crowned: Some(true),
                    scope: Some(own.clone()),
                    registry_session,
                    live: Some(true),
                    ..Default::default()
                },
                &own,
                root,
            );
        }
    };

    // Scope form: live holders over the named territory. A rung-2 crown is
    // stored as the joined set and a set-holder reigns over each member, so
    // the scan answers shared membership through the same alias-aware helper
    // the walk guard uses - never string equality, which reads a live king
    // over e-1,e-2 as "no crown over e-1".
    let projects = projects_result.clone().unwrap_or_default();
    let holders: Vec<&RegistryEntry> = rows
        .iter()
        .filter(|r| {
            !is_terminal(r)
                && r.crown_scope
                    .as_deref()
                    .is_some_and(|held| scopes_overlap(held, &scope, &projects))
        })
        .collect();
    if holders.is_empty() {
        // An unreadable project map degrades alias matching to raw spellings,
        // so an empty answer can be a MISS. But only when live crowned rows
        // exist for a miss to hide: an empty (or all-terminal) registry is
        // decisive on its own, whatever the map read.
        let live_crowned = rows
            .iter()
            .filter(|r| {
                !is_terminal(r)
                    && r.crown_scope
                        .as_deref()
                        .is_some_and(|s| !s.trim().is_empty())
            })
            .count();
        if live_crowned > 0 && projects_result.is_err() {
            let reason = format!(
                "project map unreadable ({}), so alias members may hide a live \
                 holder; no raw-spelled holder over {scope}",
                projects_result.as_ref().err().cloned().unwrap_or_default()
            );
            return ReignState {
                crowned: None,
                scope: Some(scope),
                live: None,
                unknown_reason: Some(reason),
                ..Default::default()
            };
        }
        let reason = format!("no live crowned row over {scope}");
        return ReignState {
            crowned: Some(false),
            scope: Some(scope),
            live: Some(false),
            unknown_reason: Some(reason),
            ..Default::default()
        };
    }
    let registry_session = row_session(holders[0]);
    let mut state = ReignState {
        crowned: Some(true),
        scope: Some(scope.clone()),
        registry_session,
        live: Some(true),
        ..Default::default()
    };
    if holders.len() > 1 {
        state.unknown_reason = Some(format!(
            "multiple live rows hold {scope}; fno agents court shows every crown"
        ));
    }
    // The manifest keys on the HOLDER's own scope: a holder found through one
    // member of its set arms the set's manifest, and a manifest named for the
    // member alone does not exist.
    let holder_scope = holders[0]
        .crown_scope
        .clone()
        .unwrap_or_else(|| scope.clone());
    with_manifest(state, &holder_scope, root)
}

/// Rewrite `shape` in place on one scope's existing manifest, under the same
/// `<scope>.md.lock` the arming and respawn paths flock. The only legal
/// post-init write to a king manifest; every refusal is a `String` the CLI
/// shell relays, matching the Python ValueError texts.
pub fn set_manifest_shape(
    root: &Path,
    scope: &str,
    shape: &str,
    expect_session: Option<&str>,
) -> Result<String, String> {
    if shape != "pass" && shape != "court" {
        return Err(format!("shape must be pass or court, got {shape:?}"));
    }
    let path = manifest_path(root, scope)?;
    if !path.is_file() {
        return Err(format!(
            "no manifest at {}; declare a shape only on a crown you have armed with \
             `fno agents king init --scope`.",
            path.display()
        ));
    }
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
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let parsed = crate::loopcheck::parse_king_manifest(&content)
            .ok_or_else(|| format!("manifest unreadable: {}", path.display()))?;
        let manifest_session = parsed.harness_session_id.filter(|s| !s.is_empty());
        if let (Some(expect), Some(named)) = (expect_session, manifest_session.as_deref()) {
            if named != expect {
                return Err(format!(
                    "refusing to reshape {scope:?}: the manifest names session {named}, not \
                     {expect}. Re-read with `fno agents court` before touching anything."
                ));
            }
        }
        let mut out = String::with_capacity(content.len() + 16);
        let mut replaced = false;
        for line in content.lines() {
            if !replaced && line.split(':').next().is_some_and(|k| k.trim() == "shape") {
                out.push_str(&format!("shape: {shape}"));
                replaced = true;
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        if !replaced {
            // Insert just after the opening fence, where a reader scanning the
            // frontmatter expects the crown's own fields. Prepending to the
            // file would land the line ABOVE the `---`, invisible to every
            // fenced parser, and every manifest written before the field
            // existed hits this branch. Rebuild fresh: the replace loop above
            // already emitted every line into `out`.
            let mut rebuilt = String::with_capacity(out.len() + 16);
            let mut inserted = false;
            for line in content.lines() {
                rebuilt.push_str(line);
                rebuilt.push('\n');
                if !inserted && line.trim() == "---" {
                    rebuilt.push_str(&format!("shape: {shape}\n"));
                    inserted = true;
                }
            }
            if !inserted {
                rebuilt = format!("shape: {shape}\n{rebuilt}");
            }
            out = rebuilt;
        }
        let tmp = path.with_extension("md.tmp");
        {
            let mut handle = fs::File::create(&tmp)
                .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            handle
                .write_all(out.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
        Ok(shape.to_string())
    })();
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    result
}

fn usage(verb: &str) -> String {
    format!("fno-agents {verb}: [--scope S | --session ID] [--root PATH] [--registry PATH]")
}

/// One orphan crown for the court sweep: a manifest that carries a crown no
/// live crowned row holds. Field names are the Python renderer's row keys.
#[derive(Debug, Default, Serialize)]
pub struct OrphanCrown {
    pub scope: String,
    pub level: Option<u32>,
    pub grantor: Option<String>,
    /// The holder session the manifest names; the file stem when it names none.
    pub manifest_session: Option<String>,
    pub manifest_path: String,
}

/// Normalized territory key for held matching: comma-split members, trimmed,
/// sorted - "a, b" and "b,a" are one territory, mirroring crown's
/// `_territory_key` so the sweep and the grant-time checks cannot disagree.
fn territory_key(scope: &str) -> String {
    let mut members: Vec<String> = scope
        .split(',')
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect();
    members.sort();
    members.join(",")
}

/// Crowns whose registry row is gone but whose manifest still holds them
/// (x-f0d2): scan every project space's `kings/` under `root`, keep manifests
/// carrying `crown_scope` not among `held`. The sweep walks the spaces ROOT,
/// not the rows' own projects - a vanished row names no cwd, and the
/// vanished-row scope is exactly the case this read exists to surface.
pub fn court_orphans(root: &Path, held: &[String]) -> Vec<OrphanCrown> {
    let held_keys: std::collections::BTreeSet<String> =
        held.iter().map(|h| territory_key(h)).collect();
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let mut out = Vec::new();
    // Sorted walk, like the Python sweep this replaces: read_dir order is
    // arbitrary, and which spelling of a duplicated territory wins the dedup
    // must not vary run to run.
    let mut spaces: Vec<_> = match fs::read_dir(root) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return out,
    };
    spaces.sort_by_key(|e| e.path());
    for space in spaces {
        let kings = space.path().join("kings");
        let mut files: Vec<_> = match fs::read_dir(&kings) {
            Ok(rd) => rd.flatten().collect(),
            Err(_) => continue,
        };
        files.sort_by_key(|e| e.path());
        for file in files {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(scope) = crate::claude_adopt::manifest_field(&content, "crown_scope") else {
                continue;
            };
            let key = territory_key(&scope);
            if held_keys.contains(&key) || !seen.insert(key) {
                continue;
            }
            let level = crate::claude_adopt::manifest_field(&content, "crown_level")
                .and_then(|v| v.parse().ok());
            out.push(OrphanCrown {
                scope,
                level,
                grantor: crate::claude_adopt::manifest_field(&content, "crown_grantor"),
                manifest_session: crate::claude_adopt::manifest_field(
                    &content,
                    "harness_session_id",
                )
                .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned())),
                manifest_path: path.display().to_string(),
            });
        }
    }
    out
}

/// `fno-agents court-orphans`: print the orphan crowns as JSON, exit 0.
pub fn run_court_orphans(args: &[String]) -> i32 {
    let mut root = None;
    let mut held: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" if i + 1 < args.len() => {
                root = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--held" if i + 1 < args.len() => {
                // One flag per scope: a multi-project scope itself contains
                // commas, so a CSV value could not carry it.
                held.push(args[i + 1].clone());
                i += 2;
            }
            other => {
                eprintln!("fno-agents court-orphans: unknown flag {other}");
                eprintln!("fno-agents court-orphans: --root PATH [--held SCOPE]...");
                return 2;
            }
        }
    }
    let Some(root) = root else {
        eprintln!("fno-agents court-orphans: --root is required");
        return 2;
    };
    match serde_json::to_string(&court_orphans(&root, &held)) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(e) => {
            eprintln!("fno-agents court-orphans: cannot serialize: {e}");
            1
        }
    }
}

/// `fno-agents reign-state`: print one ReignState as JSON, exit 0.
pub fn run_reign_state(args: &[String]) -> i32 {
    let mut scope: Option<String> = None;
    let mut session: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut registry: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" if i + 1 < args.len() => {
                scope = Some(args[i + 1].clone());
                i += 2;
            }
            "--session" if i + 1 < args.len() => {
                session = Some(args[i + 1].clone());
                i += 2;
            }
            "--harness" if i + 1 < args.len() => {
                harness = Some(args[i + 1].clone());
                i += 2;
            }
            "--root" if i + 1 < args.len() => {
                root = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--registry" if i + 1 < args.len() => {
                registry = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => {
                eprintln!("fno-agents reign-state: unknown flag {other}");
                eprintln!("{}", usage("reign-state"));
                return 2;
            }
        }
    }
    if scope.is_none() && session.is_none() {
        eprintln!("fno-agents reign-state: one of --scope or --session is required");
        eprintln!("{}", usage("reign-state"));
        return 2;
    }
    let registry_path =
        registry.unwrap_or_else(|| crate::paths::AgentsHome::from_env().registry_json());
    let state = reign_state(
        &root,
        scope.as_deref(),
        session.as_deref(),
        harness.as_deref(),
        &registry_path,
    );
    match serde_json::to_string(&state) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(e) => {
            eprintln!("fno-agents reign-state: cannot serialize the reign state: {e}");
            1
        }
    }
}

/// `fno-agents reign-shape`: rewrite the manifest's shape, exit 0/1/2.
pub fn run_reign_shape(args: &[String]) -> i32 {
    let mut scope: Option<String> = None;
    let mut shape: Option<String> = None;
    let mut session: Option<String> = None;
    let mut root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" if i + 1 < args.len() => {
                scope = Some(args[i + 1].clone());
                i += 2;
            }
            "--shape" if i + 1 < args.len() => {
                shape = Some(args[i + 1].clone());
                i += 2;
            }
            "--session" if i + 1 < args.len() => {
                session = Some(args[i + 1].clone());
                i += 2;
            }
            "--root" if i + 1 < args.len() => {
                root = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            other => {
                eprintln!("fno-agents reign-shape: unknown flag {other}");
                eprintln!("fno-agents reign-shape: --scope S --shape pass|court [--session ID] [--root PATH]");
                return 2;
            }
        }
    }
    let (Some(scope), Some(shape)) = (scope, shape) else {
        eprintln!("fno-agents reign-shape: --scope and --shape are required");
        return 2;
    };
    match set_manifest_shape(&root, &scope, &shape, session.as_deref()) {
        Ok(new_value) => {
            println!("{new_value}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentStatus;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loop-reign-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(root: &Path, scope: &str, session: &str, shape: &str) -> PathBuf {
        let path = manifest_path(root, scope).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = format!(
            "---\nfno_id: 20260904T000000Z-kg1-abcdef\nscope: {scope}\nshape: {shape}\n\
             harness: claude\nharness_session_id: {session}\nowner_pid: 1\n\
             budget_max_iterations: 40\nrespawn_count: 0\nrespawn_ceiling: 4\n---\n"
        );
        fs::write(&path, body).unwrap();
        path
    }

    fn row(
        name: &str,
        session: &str,
        scope: Option<&str>,
        status: AgentStatus,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "cwd": "/tmp",
            "status": status,
            "created_at": "2026-09-04T00:00:00Z",
            "harness": "claude",
            "harness_session_id": session,
            "crown_level": scope.map(|_| 2),
            "crown_scope": scope,
            "crown_grantor": scope.map(|_| "human"),
        })
    }

    fn registry_file(dir: &Path, rows: &[serde_json::Value]) -> PathBuf {
        let path = dir.join("registry.json");
        fs::write(
            &path,
            serde_json::json!({"schema_version": 11, "agents": rows}).to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn court_orphans_matches_territory_and_dedups() {
        let root = tmp("orphans");
        let kings = root.join("space-a").join("kings");
        fs::create_dir_all(&kings).unwrap();
        let crown = |scope: &str, level: &str, file: &str| {
            let body = format!(
                "---\nscope: x\nshape: pass\nharness: claude\n\
                 harness_session_id: sess-{file}\ncrown_scope: {scope}\n\
                 crown_level: {level}\ncrown_grantor: operator\n---\n"
            );
            fs::write(kings.join(file), body).unwrap();
        };
        crown("alpha", "2", "alpha.md");
        crown("beta,gamma", "1", "multi.md");
        crown("gamma, beta", "1", "dup.md");

        // Held as a differently-spelled member list: no orphan, and the two
        // spellings of one territory do not double-report.
        let held = vec!["alpha".to_string(), "beta, gamma".to_string()];
        assert!(court_orphans(&root, &held).is_empty());

        // Not held: the first spelling in sorted order surfaces, parsed, with
        // its session; dup.md sorts before multi.md.
        let out = court_orphans(&root, &["alpha".to_string()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].scope, "gamma, beta");
        assert_eq!(out[0].level, Some(1));
        assert_eq!(out[0].grantor.as_deref(), Some("operator"));
        assert_eq!(out[0].manifest_session.as_deref(), Some("sess-dup.md"));

        // A manifest naming no session falls back to the file stem.
        fs::write(
            kings.join("stem.md"),
            "---\ncrown_scope: delta\ncrown_level: 0\n---\n",
        )
        .unwrap();
        let out = court_orphans(&root, &[]);
        let delta = out.iter().find(|o| o.scope == "delta").unwrap();
        assert_eq!(delta.manifest_session.as_deref(), Some("stem"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn caller_form_answers_scope_shape_live() {
        let root = tmp("caller");
        let sid = "aaaa1111-0000-4000-8000-000000000001";
        let reg = registry_file(&root, &[row("king", sid, Some("alpha"), AgentStatus::Busy)]);
        write_manifest(&root, "alpha", sid, "pass");
        let state = reign_state(&root, None, Some(sid), None, &reg);
        assert_eq!(state.crowned, Some(true));
        assert_eq!(state.scope.as_deref(), Some("alpha"));
        assert_eq!(state.live, Some(true));
        assert_eq!(state.shape.as_deref(), Some("pass"));
        assert_eq!(state.split, Some(false));
    }

    #[test]
    fn manifest_and_registry_sessions_differ_is_split() {
        let root = tmp("split");
        let reg = registry_file(
            &root,
            &[row(
                "heir",
                "bbbb2222-0000-4000-8000-000000000002",
                Some("alpha"),
                AgentStatus::Idle,
            )],
        );
        write_manifest(
            &root,
            "alpha",
            "aaaa1111-0000-4000-8000-000000000001",
            "court",
        );
        let state = reign_state(&root, Some("alpha"), None, None, &reg);
        assert_eq!(state.split, Some(true));
        assert_eq!(
            state.manifest_session.as_deref(),
            Some("aaaa1111-0000-4000-8000-000000000001")
        );
        assert_eq!(
            state.registry_session.as_deref(),
            Some("bbbb2222-0000-4000-8000-000000000002")
        );
    }

    #[test]
    fn scope_form_finds_a_set_holder_through_any_member() {
        // A rung-2 crown is stored as the joined set; asking after one member
        // must still find the king that holds it, and the manifest read keys
        // on the HOLDER's own scope, not the member the caller named.
        let root = tmp("setholder");
        let sid = "aaaa1111-0000-4000-8000-00000000000a";
        let reg = registry_file(
            &root,
            &[row(
                "set-king",
                sid,
                Some("epic-a,epic-b"),
                AgentStatus::Busy,
            )],
        );
        write_manifest(&root, "epic-a,epic-b", sid, "pass");

        let state = reign_state(&root, Some("epic-a"), None, None, &reg);
        assert_eq!(state.crowned, Some(true));
        assert_eq!(state.live, Some(true));
        assert_eq!(state.shape.as_deref(), Some("pass"));
        assert_eq!(
            state.manifest_session.as_deref(),
            Some(sid),
            "the manifest read keys on the holder's own scope"
        );
    }

    #[test]
    fn unreadable_project_map_answers_unknown_not_no_crown() {
        // The scan degrades to raw spellings when the map errs, so an empty
        // answer can be an alias miss. That must read as unknown, never as a
        // clean "no live crowned row".
        let root = tmp("nomap");
        let reg = registry_file(
            &root,
            &[row(
                "other-king",
                "cccc3333-0000-4000-8000-000000000003",
                Some("gamma"),
                AgentStatus::Busy,
            )],
        );
        let projects: Result<std::collections::HashMap<String, String>, String> =
            Err("no work.workspaces in any candidate config.toml".to_string());
        let state = reign_state_with_projects(&root, Some("alpha"), None, None, &reg, &projects);
        assert_eq!(state.crowned, None);
        assert_eq!(state.live, None);
        let reason = state.unknown_reason.unwrap();
        assert!(
            reason.contains("project map unreadable"),
            "reason was {reason}"
        );
    }

    #[test]
    fn unreadable_registry_answers_unknown_never_clean_false() {
        let root = tmp("unreadable");
        let bad = root.join("not-a-registry.json");
        fs::write(&bad, "{ this is not json").unwrap();
        let state = reign_state(&root, Some("alpha"), None, None, &bad);
        assert_eq!(state.live, None);
        assert_eq!(state.crowned, None);
        assert_eq!(state.split, None);
        let reason = state.unknown_reason.unwrap();
        assert!(reason.contains("registry"), "reason was {reason}");
    }

    #[test]
    fn unsafe_scope_with_unreadable_registry_degrades_not_raises() {
        let root = tmp("unsafe");
        let bad = root.join("not-a-registry.json");
        fs::write(&bad, "{ this is not json").unwrap();
        let state = reign_state(&root, Some("a/b"), None, None, &bad);
        assert_eq!(state.live, None);
        assert_eq!(state.split, None);
        let reason = state.unknown_reason.unwrap();
        assert!(reason.contains("unsafe scope"), "reason was {reason}");
    }

    #[test]
    fn missing_registry_file_reads_as_no_crown_not_unknown() {
        // Python load_registry: a missing file is empty, not an error.
        let root = tmp("missing");
        let state = reign_state(&root, Some("alpha"), None, None, &root.join("absent.json"));
        assert_eq!(state.crowned, Some(false));
        assert_eq!(state.live, Some(false));
        assert!(state
            .unknown_reason
            .unwrap()
            .contains("no live crowned row"));
    }

    #[test]
    fn scope_with_no_manifest_keeps_split_none_and_names_it() {
        let root = tmp("nomanifest");
        let reg = registry_file(
            &root,
            &[row(
                "king",
                "aaaa1111-0000-4000-8000-000000000001",
                Some("alpha"),
                AgentStatus::Busy,
            )],
        );
        let state = reign_state(&root, Some("alpha"), None, None, &reg);
        assert_eq!(state.crowned, Some(true));
        assert_eq!(state.split, None);
        assert_eq!(state.shape, None);
        assert!(state.unknown_reason.unwrap().contains("no manifest"));
    }

    #[test]
    fn terminal_crown_row_is_not_a_live_reign() {
        let root = tmp("terminal");
        let reg = registry_file(
            &root,
            &[row(
                "king",
                "aaaa1111-0000-4000-8000-000000000001",
                Some("alpha"),
                AgentStatus::Exited,
            )],
        );
        let state = reign_state(&root, Some("alpha"), None, None, &reg);
        assert_eq!(state.crowned, Some(false));
        assert_eq!(state.live, Some(false));
        assert_eq!(state.split, None);
    }

    #[test]
    fn caller_form_without_session_identity_is_not_a_crown() {
        let root = tmp("nosession");
        let reg = registry_file(&root, &[]);
        let state = reign_state(&root, None, None, None, &reg);
        assert_eq!(state.crowned, Some(false));
        assert_eq!(state.live, Some(false));
        // The empty-registry caller form finds no row for the id.
        let state = reign_state(
            &root,
            None,
            Some("cccc3333-0000-4000-8000-000000000003"),
            None,
            &reg,
        );
        assert_eq!(state.crowned, Some(false));
        assert_eq!(state.live, Some(false));
        assert!(state
            .unknown_reason
            .unwrap()
            .contains("no registry row matches"));
    }

    #[test]
    fn caller_row_without_crown_names_it() {
        let root = tmp("nocrown");
        let sid = "aaaa1111-0000-4000-8000-000000000001";
        let reg = registry_file(&root, &[row("worker", sid, None, AgentStatus::Busy)]);
        let state = reign_state(&root, None, Some(sid), None, &reg);
        assert_eq!(state.crowned, Some(false));
        assert_eq!(state.live, Some(false));
        assert!(state.unknown_reason.unwrap().contains("row holds no crown"));
    }

    #[test]
    fn multiple_live_holders_still_corroborate_the_manifest() {
        let root = tmp("multi");
        let reg = registry_file(
            &root,
            &[
                row(
                    "king",
                    "aaaa1111-0000-4000-8000-000000000001",
                    Some("alpha"),
                    AgentStatus::Busy,
                ),
                row(
                    "heir",
                    "bbbb2222-0000-4000-8000-000000000002",
                    Some("alpha"),
                    AgentStatus::Idle,
                ),
            ],
        );
        write_manifest(
            &root,
            "alpha",
            "aaaa1111-0000-4000-8000-000000000001",
            "court",
        );
        let state = reign_state(&root, Some("alpha"), None, None, &reg);
        assert_eq!(state.crowned, Some(true));
        assert!(state.unknown_reason.unwrap().contains("multiple live rows"));
        assert_eq!(state.shape.as_deref(), Some("court"));
    }

    #[test]
    fn legacy_manifest_without_shape_reads_as_pass() {
        let root = tmp("legacy");
        let reg = registry_file(
            &root,
            &[row(
                "king",
                "dddd1111-0000-4000-8000-00000000000d",
                Some("legacy"),
                AgentStatus::Busy,
            )],
        );
        let path = manifest_path(&root, "legacy").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "---\nscope: legacy\nfno_id: 20260904T000000Z-kg1-abcdef\n\
             harness_session_id: dddd1111-0000-4000-8000-00000000000d\n---\n",
        )
        .unwrap();
        let state = reign_state(&root, Some("legacy"), None, None, &reg);
        assert_eq!(state.shape.as_deref(), Some("pass"));
    }

    #[test]
    fn a_known_non_claude_harness_never_falls_through_to_a_claude_prefix() {
        let root = tmp("scoped");
        // A claude row whose 8-hex short_id prefixes the codex caller's uuid.
        let codex_sid = "abcd1234-0000-4000-8000-00000000000c";
        let claude_row = serde_json::json!({
            "name": "claude-worker",
            "cwd": "/tmp",
            "status": "busy",
            "created_at": "2026-09-04T00:00:00Z",
            "harness": "claude",
            "short_id": "abcd1234",
            "harness_session_id": "ffff0000-0000-4000-8000-00000000000f",
        });
        let reg = registry_file(&root, &[claude_row]);
        // Harness scoped: no codex row carries the id, so no row matches even
        // though the claude row's short_id prefixes it.
        let state = reign_state(&root, None, Some(codex_sid), Some("codex"), &reg);
        assert_eq!(state.crowned, Some(false));
        assert!(state
            .unknown_reason
            .unwrap()
            .contains("no registry row matches"));
        // The claude-shaped scan (harness unknown) still finds it by prefix,
        // which is the original reader's explicit-session behavior.
        let state = reign_state(&root, None, Some(codex_sid), None, &reg);
        assert_eq!(state.crowned, Some(false));
        assert!(state.unknown_reason.unwrap().contains("row holds no crown"));
    }

    #[test]
    fn shape_rewrite_is_idempotent_and_refuses_a_foreign_manifest() {
        let root = tmp("shape");
        let sid = "aaaa1111-0000-4000-8000-000000000001";
        write_manifest(&root, "alpha", sid, "pass");

        assert_eq!(
            set_manifest_shape(&root, "alpha", "court", None).unwrap(),
            "court"
        );
        let path = manifest_path(&root, "alpha").unwrap();
        let binding = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = binding.lines().collect();
        assert_eq!(lines[0], "---");
        assert!(
            lines.contains(&"shape: court"),
            "field replaced in place, was {lines:?}"
        );
        assert_eq!(lines.iter().filter(|l| **l == "shape: court").count(), 1);

        // Same value again is a no-op rewrite, not a refusal.
        assert_eq!(
            set_manifest_shape(&root, "alpha", "court", Some(sid)).unwrap(),
            "court"
        );
        // A different expect-session refuses.
        let err = set_manifest_shape(
            &root,
            "alpha",
            "pass",
            Some("bbbb2222-0000-4000-8000-000000000002"),
        )
        .unwrap_err();
        assert!(err.contains("names session"), "err was {err}");
    }

    #[test]
    fn shape_insert_on_a_legacy_manifest_lands_inside_the_fence() {
        let root = tmp("insert");
        let path = manifest_path(&root, "legacy").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "---\nscope: legacy\nfno_id: 20260904T000000Z-kg1-abcdef\n---\n",
        )
        .unwrap();
        set_manifest_shape(&root, "legacy", "court", None).unwrap();
        let binding = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = binding.lines().collect();
        assert_eq!(lines[0], "---");
        assert_eq!(lines[1], "shape: court");
    }

    #[test]
    fn shape_refusals_name_the_remedy() {
        let root = tmp("refuse");
        assert!(set_manifest_shape(&root, "alpha", "siege", None)
            .unwrap_err()
            .contains("pass or court"));
        let err = set_manifest_shape(&root, "alpha", "court", None).unwrap_err();
        assert!(err.contains("no manifest"), "err was {err}");
        assert!(set_manifest_shape(&root, "a/b", "court", None)
            .unwrap_err()
            .contains("unsafe king scope"));
    }
}
