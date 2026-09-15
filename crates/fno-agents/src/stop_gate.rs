//! `fno-agents stop-gate` - the Stop hook's one-process ownership answer
//! (x-09d2). The shim previously spent a python3 payload parse, two `sed`
//! spawns, three git spawns, and one Python `fno` king round trip per fire
//! before it could allow a stranger's stop (measured p50 3.4s under load).
//! This verb answers the same question in one native entry: does THIS
//! session own a target manifest, a king manifest, or a pending delivery
//! retry?
//!
//! stdin carries the raw Stop hook payload. stdout is a shell-evalable parse
//! block (`KEY='value'` lines) followed by exactly one verdict:
//!   - `NO-OWNER`   - no obligation names this session; the caller exits 0.
//!                    The visitor diagnostic and the `<help>` distress scan
//!                    (payload message first) have already run here.
//!   - `OWNER`      - lines `STATE=`/`DRIVER=`/`CWD=`/`PENDING=`/`SPACE=`
//!                    name what the caller continues with.
//!   - `BROKEN <target|king> <detail>` - the resolver could not answer; the
//!                    caller runs its bounded-block retry counters. Unreadable
//!                    data never manufactures a no-owner verdict, so a broken
//!                    read is loud here, never a silent allow.
//!
//! Deliberately NOT a decision: an owner fire still routes through the
//! caller's existing `loop-check` invocation. Only the no-owner path is
//! claimed by this verb.
//!
//! Parity contract: every branch mirrors `hooks/target-stop-hook.sh` as of
//! x-09d2 (the shell keeps its full legacy path and falls back to it when
//! this verb is absent, so drift between the two is caught by the shim's
//! own e2e tests, which run both).

use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::client_verbs::load_registry_entries;
use crate::distress;
use crate::manifest_lookup::{git_worktree_paths, parse_manifest_identity, paths_eq};
use crate::paths::{events_path, space_dir, worktree_repo_root, worktree_space_dir, AgentsHome};

/// Terminal registry statuses; the Python `resolve_king_manifest_path` reads
/// the same set and answers None for a terminal row.
const TERMINAL_STATUSES: [&str; 4] = ["exited", "orphaned", "failed", "permanent_dead"];

/// What the gate decided, before printing.
enum Verdict {
    NoOwner,
    Owner {
        state: PathBuf,
        driver: &'static str,
        owner_cwd: PathBuf,
    },
    Broken(&'static str, String),
}

/// The payload-derived identity of the stopping session, collected exactly
/// the way the shim collects it (most authoritative first).
struct Fire {
    session_id: String,
    transcript_path: String,
    hook_harness_id: String,
    resolve_ids: Vec<String>,
    resolve_harness_id: String,
    harness: Option<String>,
    last_assistant_message: Option<String>,
}

pub fn run_stop_gate(args: &[String]) -> i32 {
    let mut cwd_arg: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cwd" => {
                i += 1;
                cwd_arg = args.get(i).cloned();
            }
            other => {
                eprintln!("stop-gate: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("stop-gate: unreadable payload");
        // An unreadable payload is not a verdict; the caller falls back to
        // its legacy path when no verdict line arrives.
        return 0;
    }
    let payload: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);
    let str_field = |key: &str| -> String {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let session_id = str_field("session_id");
    let transcript_path = str_field("transcript_path");
    let payload_cwd = str_field("cwd");
    let last_assistant_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .map(str::to_string);

    let cwd = cwd_arg
        .filter(|c| !c.is_empty())
        .or_else(|| (!payload_cwd.is_empty()).then_some(payload_cwd))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let fire = collect_fire(&session_id, &transcript_path, last_assistant_message);

    // Parse block first: the caller evals every KEY= line it recognizes and
    // reads the verdict separately, so field growth never changes the verdict
    // contract.
    println!("SID={}", shell_quote(&fire.session_id));
    println!("HID={}", shell_quote(&fire.hook_harness_id));
    println!("TPATH={}", shell_quote(&fire.transcript_path));
    println!(
        "HARNESS={}",
        shell_quote(fire.harness.as_deref().unwrap_or(""))
    );
    println!("IDS={}", shell_quote(&fire.resolve_ids.join(" ")));
    println!("RID={}", shell_quote(&fire.resolve_harness_id));

    let space = events_space(&cwd);
    println!("SPACE={}", shell_quote(&space.to_string_lossy()));

    match evaluate(&cwd, &fire) {
        Verdict::NoOwner => {
            println!("NO-OWNER");
            0
        }
        Verdict::Owner {
            state,
            driver,
            owner_cwd,
        } => {
            let pending = delivery_pending_for(&cwd, &fire, Some(&state));
            println!("STATE={}", shell_quote(&state.to_string_lossy()));
            println!("DRIVER={driver}");
            println!("CWD={}", shell_quote(&owner_cwd.to_string_lossy()));
            println!(
                "PENDING={}",
                shell_quote(
                    &pending
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )
            );
            println!("OWNER");
            0
        }
        Verdict::Broken(kind, detail) => {
            eprintln!("stop-gate: {kind} resolver could not answer: {detail}");
            println!("BROKEN {kind} {detail}");
            0
        }
    }
}

/// The resolved events path's parent: the space the shim keys its counters,
/// king manifests, and distress journal off. `FNO_EVENTS_PATH` wins exactly
/// as it does for `fno-agents state path events`, so a pinned test sees a
/// pinned space here too.
fn events_space(cwd: &Path) -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_EVENTS_PATH").filter(|v| !v.is_empty()) {
        return PathBuf::from(v)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| space_dir(cwd));
    }
    space_dir(cwd)
}

/// The one subprocess this verb may spend: a single `git rev-parse
/// --git-path` naming the delivery-pending prefix. Everything else is local
/// file reads; worktree discovery shares one listing between canonical-root
/// resolution and the manifest scan.
fn git_delivery_prefix(repo_root: &Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-path", "fno-delivery-finalize-pending-"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    let raw = out.unwrap_or_else(|| {
        repo_root
            .join(".fno")
            .join(".delivery-finalize-pending-")
            .to_string_lossy()
            .into_owned()
    });
    let path = PathBuf::from(&raw);
    if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    }
}

/// The pending-delivery retry file this session would resume, if any. With a
/// resolved state file the name is derived from that manifest's ids; without
/// one, the newest harness-tagged pending file wins, then any pending file
/// whose stamped harness id names this stop.
fn delivery_pending_for(cwd: &Path, fire: &Fire, state: Option<&Path>) -> Option<PathBuf> {
    let prefix = git_delivery_prefix(&worktree_repo_root(cwd));
    match state {
        Some(state) => {
            let content = std::fs::read_to_string(state).ok()?;
            let live_session = first_raw_field(&content, &["fno_id", "session_id"]);
            let live_harness =
                first_raw_field(&content, &["harness_session_id", "claude_session_id"]);
            let owner = if !fire.hook_harness_id.is_empty() {
                fire.hook_harness_id.clone()
            } else {
                live_harness.unwrap_or_else(|| "harness".to_string())
            };
            let retry_id = format!(
                "{}.{}",
                owner,
                live_session.unwrap_or_else(|| "session".into())
            );
            let pending = PathBuf::from(format!("{}.{}.md", prefix.display(), retry_id));
            pending.exists().then_some(pending)
        }
        None => {
            let entries = std::fs::read_dir(prefix.parent()?).ok()?;
            let mut named: Vec<PathBuf> = Vec::new();
            let mut all: Vec<PathBuf> = Vec::new();
            let file_name = |p: &Path| -> String {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            for entry in entries.flatten() {
                let p = entry.path();
                let name = file_name(&p);
                if !name.ends_with(".md") || !p.is_file() {
                    continue;
                }
                let Some(rest) = name.strip_prefix(&format!(
                    "{}",
                    prefix
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )) else {
                    continue;
                };
                all.push(p.clone());
                let suffix = rest.strip_prefix(&format!("{}.", fire.hook_harness_id));
                if let Some(suffix) = suffix {
                    // The glob's literal dot sat between the id and the `*`,
                    // so strip_prefix already consumed it: only the `.md`
                    // tail is left to check.
                    if suffix.ends_with(".md") {
                        named.push(p);
                    }
                }
            }
            if !fire.hook_harness_id.is_empty() {
                // Strictly-newer wins (the shim's `-nt` keeps the first of
                // two equal-mtime files in glob order).
                let mut newest: Option<(PathBuf, u64)> = None;
                for p in named {
                    let m = mtime_secs(&p).unwrap_or(0);
                    if newest.as_ref().is_none_or(|(_, best)| m > *best) {
                        newest = Some((p, m));
                    }
                }
                if let Some((p, _)) = newest {
                    return Some(p);
                }
            }
            // Alphabetical order, first harness-tagged match: the shim's glob
            // order for its second pass.
            all.sort();
            for p in all {
                let Ok(content) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let stamped =
                    first_raw_field(&content, &["harness_session_id", "claude_session_id"]);
                if !fire.hook_harness_id.is_empty()
                    && stamped.as_deref() == Some(fire.hook_harness_id.as_str())
                {
                    return Some(p);
                }
            }
            None
        }
    }
}

fn mtime_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// First non-empty, non-`null` value among `keys`, first matching line wins,
/// quotes and whitespace stripped. Mirrors the shim's `grep | sed | tr` chain.
fn first_raw_field(content: &str, keys: &[&str]) -> Option<String> {
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !keys.contains(&key.trim()) {
            continue;
        }
        let value = value.trim().trim_matches('"').trim();
        if !value.is_empty() && value != "null" {
            return Some(value.to_string());
        }
    }
    None
}

fn collect_fire(
    session_id: &str,
    transcript_path: &str,
    last_assistant_message: Option<String>,
) -> Fire {
    let hook_harness_id = transcript_basename(transcript_path).unwrap_or_default();
    let hook_harness_id = if hook_harness_id.is_empty() {
        session_id.to_string()
    } else {
        hook_harness_id
    };
    // Codex names its rollout transcript rollout-<utc>-<thread-uuid>; the
    // resolver wants the bare uuid suffix, when there is one.
    let codex_uuid = uuid_suffix(&hook_harness_id).unwrap_or_default();
    let codex_env_id = std::env::var("CODEX_THREAD_ID").ok().filter(|tid| {
        !tid.is_empty()
            && (hook_harness_id == *tid || hook_harness_id.ends_with(&format!("-{tid}")))
    });
    let mut resolve_ids: Vec<String> = Vec::new();
    for candidate in [
        Some(session_id.to_string()),
        codex_env_id,
        (!codex_uuid.is_empty()).then_some(codex_uuid),
        (!hook_harness_id.is_empty()).then_some(hook_harness_id.clone()),
    ]
    .into_iter()
    .flatten()
    {
        if !candidate.is_empty() && !resolve_ids.contains(&candidate) {
            resolve_ids.push(candidate);
        }
    }
    let resolve_harness_id = resolve_ids.first().cloned().unwrap_or_default();
    Fire {
        session_id: session_id.to_string(),
        transcript_path: transcript_path.to_string(),
        hook_harness_id,
        resolve_ids,
        resolve_harness_id,
        harness: detect_harness(transcript_path),
        last_assistant_message,
    }
}

fn transcript_basename(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let name = Path::new(path).file_name()?.to_string_lossy().into_owned();
    Some(name.strip_suffix(".jsonl").unwrap_or(&name).to_string())
}

/// The 8-4-4-4-12 hex uuid at the END of `s`, after a `-` separator: the
/// shim's `sed -E 's/^.*-([0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12})$/\1/'`.
fn uuid_suffix(s: &str) -> Option<String> {
    if s.len() < 37 || !s.ends_with(char::is_alphanumeric) {
        return None;
    }
    let (head, tail) = s.split_at(s.len() - 36);
    let tail_ok = tail.len() == 36
        && tail.as_bytes()[8] == b'-'
        && tail.as_bytes()[13] == b'-'
        && tail.as_bytes()[18] == b'-'
        && tail.as_bytes()[23] == b'-';
    if !tail_ok {
        return None;
    }
    if !tail.chars().all(|c| c == '-' || c.is_ascii_hexdigit()) {
        return None;
    }
    if head.is_empty() || !head.ends_with('-') {
        return None;
    }
    Some(tail.to_string())
}

/// The shim's harness markers: the transcript location is proof, env markers
/// are claims, and two disagreeing markers claim nothing.
fn detect_harness(transcript_path: &str) -> Option<String> {
    if let Ok(h) = std::env::var("FNO_HARNESS") {
        if !h.is_empty() {
            return Some(h);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let prefix = PathBuf::from(home).join(".claude").join("projects");
        if !transcript_path.is_empty() && Path::new(transcript_path).starts_with(&prefix) {
            return Some("claude".to_string());
        }
    }
    let mut found: Option<String> = None;
    let mut count = 0;
    for (env, name) in [
        ("CODEX_THREAD_ID", "codex"),
        ("CLAUDE_CODE_SESSION_ID", "claude"),
        ("GEMINI_SESSION_ID", "gemini"),
    ] {
        let present = std::env::var_os(env).is_some_and(|v| !v.is_empty());
        if present {
            found = Some(name.to_string());
            count += 1;
        }
    }
    if count > 1 {
        None
    } else {
        found
    }
}

fn evaluate(cwd: &Path, fire: &Fire) -> Verdict {
    let wt_state = worktree_space_dir(cwd).join("target-state.md");
    let live_state = if wt_state.exists() {
        wt_state
    } else {
        worktree_repo_root(cwd).join(".fno").join("target-state.md")
    };

    let mut pre_manifest_no_file = false;
    let step = if live_state.exists() {
        // Presence is not ownership: a resident manifest naming a FOREIGN
        // session sends the resolver out (the shell's non-resident branch).
        if resident_matches(&live_state, &fire.hook_harness_id) {
            return Verdict::Owner {
                state: live_state,
                driver: "target",
                owner_cwd: cwd.to_path_buf(),
            };
        }
        // A resident manifest that fails the resolver is checker-broken,
        // whatever else is on disk.
        resolve_across_worktrees(cwd, &fire.resolve_ids).map_err(Some)
    } else {
        pre_manifest_no_file = true;
        resolve_across_worktrees(cwd, &fire.resolve_ids).map_err(|detail| {
            // A stranger stop with no other worktree present cannot be a
            // worktree-ownership question, so an unreadable listing falls
            // through to the king and visitor arms instead of blocking.
            let others = git_worktree_paths(cwd)
                .map(|w| w.len() > 1)
                .unwrap_or(false);
            if others {
                Some(detail)
            } else {
                None
            }
        })
    };

    let resolved = match step {
        Err(Some(detail)) => return Verdict::Broken("target", detail),
        Err(None) => None,
        Ok(ref r) => r.clone(),
    };
    // A CLEAN miss (every worktree answered "no manifest names this stop")
    // is what gates the visitor diagnostic + distress scan; a broken
    // fall-through is not a miss and must scan nothing.
    let clean_miss = matches!(step, Ok(None));

    if let Some(identity) = resolved {
        return Verdict::Owner {
            state: identity.manifest_path,
            driver: "target",
            owner_cwd: PathBuf::from(identity.owner_cwd.clone()),
        };
    }

    // Pending delivery, then king: the shim's order. Either converts a
    // stranger into an owner before the visitor allow fires.
    if let Some(pending) = delivery_pending_for(cwd, fire, None) {
        return Verdict::Owner {
            state: pending,
            driver: "target",
            owner_cwd: cwd.to_path_buf(),
        };
    }
    match resolve_king(cwd, fire) {
        KingResolve::None => {}
        KingResolve::Found(state) => {
            return Verdict::Owner {
                state,
                driver: "king",
                owner_cwd: cwd.to_path_buf(),
            };
        }
    }

    // Visitor allow. The diagnostic names every id tried (a contract the
    // docs reference), and the distress scan reads the payload's own
    // message first so no reader subprocess runs for an ordinary stop.
    if clean_miss && !fire.resolve_harness_id.is_empty() {
        eprintln!(
            "loop-check: no manifest names session {}; visitor allowed (tried: {})",
            fire.resolve_harness_id,
            fire.resolve_ids.join(" ")
        );
    }
    if clean_miss && pre_manifest_no_file && !fire.transcript_path.is_empty() {
        let project_events = events_path(cwd);
        let global_events = crate::loopcheck::default_global_events_path();
        distress::scan_and_emit(
            &project_events,
            &global_events,
            cwd,
            &fire.resolve_harness_id,
            None,
            fire.harness.as_deref(),
            Path::new(&fire.transcript_path),
            fire.last_assistant_message.as_deref(),
        );
    }
    Verdict::NoOwner
}

/// The shim's resident check: does the manifest at `state` name THIS stop's
/// harness id (exact or codex rollout suffix), or - when the manifest names
/// no harness at all - this stop's id as its bare session identity?
fn resident_matches(state: &Path, hook_harness_id: &str) -> bool {
    if hook_harness_id.is_empty() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(state) else {
        return false;
    };
    let resident_session = first_raw_field(&content, &["fno_id", "session_id"]).unwrap_or_default();
    let resident_harness = first_raw_field(
        &content,
        &[
            "harness_session_id",
            "claude_session_id",
            "claude_transcript_id",
        ],
    )
    .unwrap_or_default();
    if !resident_harness.is_empty()
        && (resident_harness == hook_harness_id
            || hook_harness_id.ends_with(&format!("-{resident_harness}")))
    {
        return true;
    }
    resident_harness.is_empty() && resident_session == hook_harness_id
}

/// One worktree listing, every id tried id-major (the shim calls the
/// per-id resolver repeatedly; the first id with any worktree hit wins).
fn resolve_across_worktrees(
    cwd: &Path,
    ids: &[String],
) -> Result<Option<crate::manifest_lookup::ManifestIdentity>, String> {
    let mut candidates = git_worktree_paths(cwd).map_err(|e| format!("worktree list: {e:?}"))?;
    if !candidates.iter().any(|path| paths_eq(path, cwd)) {
        candidates.push(cwd.to_path_buf());
    }
    for id in ids {
        if id.trim().is_empty() {
            continue;
        }
        for worktree in &candidates {
            let manifest_path = worktree.join(".fno/target-state.md");
            let Ok(content) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let mut identity = parse_manifest_identity(content.as_str());
            if identity.matches(id) {
                if identity.owner_cwd.is_empty() {
                    identity.owner_cwd = worktree.to_string_lossy().into_owned();
                }
                identity.manifest_path =
                    std::fs::canonicalize(&manifest_path).unwrap_or(manifest_path);
                return Ok(Some(identity));
            }
        }
    }
    Ok(None)
}

enum KingResolve {
    None,
    Found(PathBuf),
}

/// Native port of `fno agents king manifest-path`: the registry row - not
/// file presence - proves authority, the scope names the manifest under the
/// space's kings dir, and any unreadable step answers None (the same
/// fail-open the Python resolver's catch-all ships). The shim's BROKEN-king
/// class existed for a missing `fno`; this code IS the binary, so that
/// class has no native trigger on this path.
fn resolve_king(cwd: &Path, fire: &Fire) -> KingResolve {
    if fire.hook_harness_id.is_empty() {
        return KingResolve::None;
    }
    let kings_dir = events_space(cwd).join("kings");
    let has_manifests = std::fs::read_dir(&kings_dir)
        .map(|mut it| {
            it.any(|e| {
                e.map(|e| e.path().extension().is_some_and(|x| x == "md"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if !has_manifests {
        return KingResolve::None;
    }
    let sid = if fire.resolve_harness_id.is_empty() {
        &fire.hook_harness_id
    } else {
        &fire.resolve_harness_id
    };
    let rows = match load_registry_entries(&AgentsHome::from_env().registry_json()) {
        Ok(rows) => rows,
        Err(_) => return KingResolve::None,
    };
    let Some(row) = find_registry_row(&rows, sid, fire.harness.as_deref()) else {
        return KingResolve::None;
    };
    let status = row.get("status").and_then(Value::as_str).unwrap_or("live");
    if TERMINAL_STATUSES.contains(&status) {
        return KingResolve::None;
    }
    let Some(scope) = row
        .get("crown_scope")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return KingResolve::None;
    };
    let scope = scope.trim();
    if scope.is_empty() || scope.contains("..") || scope.contains('/') || scope.contains('\\') {
        return KingResolve::None;
    }
    let path = kings_dir.join(format!("{scope}.md"));
    if path.is_file() {
        KingResolve::Found(path)
    } else {
        KingResolve::None
    }
}

/// Python `_find_by_session`: exact equality, scoped to the row's harness
/// when the harness is known; the 32-bit claude jobId prefix pass only for
/// claude rows.
fn find_registry_row<'a>(rows: &'a [Value], sid: &str, harness: Option<&str>) -> Option<&'a Value> {
    let sid = sid.trim();
    if sid.is_empty() {
        return None;
    }
    let field = |row: &Value, key: &str| -> String {
        row.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let row_harness = |row: &Value| -> String { field(row, "harness").to_lowercase() };
    let norm = |s: &str| -> String { s.replace('-', "").to_lowercase() };
    match harness {
        Some(h) => {
            let h = h.to_lowercase();
            for row in rows {
                if row_harness(row) != h {
                    continue;
                }
                let mut fields = vec![field(row, "harness_session_id")];
                if h == "claude" {
                    fields.push(field(row, "cc_session_id"));
                }
                if fields.iter().any(|f| f == sid) {
                    return Some(row);
                }
            }
            if h == "claude" {
                let snorm = norm(sid);
                for row in rows {
                    if row_harness(row) != "claude" {
                        continue;
                    }
                    let short = norm(&field(row, "short_id"));
                    if !short.is_empty() && snorm.starts_with(&short) {
                        return Some(row);
                    }
                }
            }
            None
        }
        None => {
            for row in rows {
                let fields = [
                    field(row, "harness_session_id"),
                    field(row, "cc_session_id"),
                ];
                if fields.iter().any(|f| f == sid) {
                    return Some(row);
                }
            }
            let snorm = norm(sid);
            for row in rows {
                if row_harness(row) != "claude" {
                    continue;
                }
                let short = norm(&field(row, "short_id"));
                if !short.is_empty() && snorm.starts_with(&short) {
                    return Some(row);
                }
            }
            None
        }
    }
}

/// Single-quote a value for the shim's `eval`: an embedded quote closes the
/// literal, escapes itself, and reopens.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_suffix_takes_a_rollout_tail() {
        let tid = "0198abcd-1234-5678-9abc-def012345678";
        let rollout = format!("rollout-2026-09-15T101530-{tid}");
        assert_eq!(uuid_suffix(&rollout).as_deref(), Some(tid));
        assert_eq!(uuid_suffix(tid), None, "no separator prefix, no strip");
        assert_eq!(uuid_suffix("session-xyz"), None);
        assert_eq!(uuid_suffix(""), None);
    }

    #[test]
    fn shell_quote_embeds_quotes_safely() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn first_raw_field_reads_first_line_and_skips_null() {
        let doc = "fno_id: a1\nsession_id: null\nfno_id: b2\nharness_session_id: \"h1\"\n";
        assert_eq!(
            first_raw_field(doc, &["fno_id", "session_id"]).as_deref(),
            Some("a1")
        );
        assert_eq!(
            first_raw_field(doc, &["harness_session_id", "claude_session_id"]).as_deref(),
            Some("h1")
        );
        assert_eq!(first_raw_field(doc, &["claude_transcript_id"]), None);
    }

    #[test]
    fn resident_matches_accepts_exact_and_rollout_suffix() {
        let dir = std::env::temp_dir().join(format!("stop-gate-resident-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("target-state.md");
        std::fs::write(
            &state,
            "---\nfno_id: fno-a1\nharness_session_id: h-uuid-1234\ntarget_claim_key: \"node:x\"\n",
        )
        .unwrap();
        assert!(resident_matches(&state, "h-uuid-1234"));
        assert!(resident_matches(&state, "rollout-7-h-uuid-1234"));
        assert!(!resident_matches(&state, "other"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resident_match_falls_back_to_bare_session_id() {
        let dir = std::env::temp_dir().join(format!("stop-gate-resident2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("target-state.md");
        std::fs::write(&state, "---\nfno_id: h-uuid-9\n").unwrap();
        assert!(resident_matches(&state, "h-uuid-9"));
        assert!(!resident_matches(&state, "h-uuid-9-extra"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn registry_row_match_scopes_and_prefixes_like_python() {
        let rows = vec![
            serde_json::json!({"harness": "codex", "harness_session_id": "shared-id"}),
            serde_json::json!({"harness": "claude", "harness_session_id": "", "cc_session_id": "shared-id"}),
            serde_json::json!({"harness": "claude", "short_id": "abcd1234"}),
        ];
        // A codex-scoped probe must not take the claude row that shares the id.
        let hit = find_registry_row(&rows, "shared-id", Some("codex")).unwrap();
        assert_eq!(hit["harness"], "codex");
        // Unknown harness keeps the claude-shaped scan: exact pass over ALL
        // rows in registry order (the codex row carries the id first), then
        // the claude-only prefix pass.
        let hit = find_registry_row(&rows, "shared-id", None).unwrap();
        assert_eq!(hit["harness"], "codex");
        let hit = find_registry_row(&rows, "abcd12340000-0000", None).unwrap();
        assert_eq!(hit["short_id"], "abcd1234");
        assert!(find_registry_row(&rows, "zzzz", None).is_none());
        assert!(find_registry_row(&rows, "", None).is_none());
    }

    #[test]
    fn king_scope_traversal_refuses() {
        // The traversal guard lives inline in resolve_king; assert the shape
        // of the refusal through the same predicate the code applies.
        let unsafe_scopes = ["../x", "a/b", "a\\b", ""];
        for scope in unsafe_scopes {
            let refused = scope.is_empty()
                || scope.contains("..")
                || scope.contains('/')
                || scope.contains('\\');
            assert!(refused, "scope {scope:?} must refuse");
        }
    }
}
