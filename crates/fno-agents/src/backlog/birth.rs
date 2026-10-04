//! The born-with-why birth hook, ported from
//! `cli/src/fno/provenance/spawn_think.py` for the birth branch: gate-first,
//! strictly non-fatal, and an unforced birth NEVER auto-spawns - the worst
//! outcome is one durable `think_offered` event plus its stderr offer line.
//! The auto-spawn tail (day cap, dedup token, bg dispatch) is unreachable for
//! an unforced birth and stays unported.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const EVENT_OFFERED: &str = "think_offered";
pub const EVENT_SKIPPED: &str = "think_skipped";
const EVENT_SOURCE: &str = "backlog";
const ENV_OVERRIDE: &str = "FNO_THINK_SPAWN";
const ENV_PRESENCE: &str = "FNO_THINK_SPAWN_PRESENCE";
const REASON_BIRTH: &str = "birth";

/// One birth-hook evaluation: `noop` (gate off, nothing anywhere), `skipped`
/// (one skip event), or `offered` (stderr line + one offer event).
#[derive(Debug, Clone)]
pub struct ThinkSpawnResult {
    pub kind: &'static str,
    pub event: Option<&'static str>,
    pub reason: Option<String>,
    pub node_id: Option<String>,
    pub presence: Option<String>,
    pub resolved: Option<bool>,
    pub offer_line: Option<String>,
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// The config candidates walk for one nested key, first hit wins. Best-effort:
/// a missing or unparseable file contributes nothing.
fn config_flag(keys: &[&str]) -> Option<Value> {
    for doc in super::settings::config_candidates() {
        let mut cur = &doc;
        let mut found = true;
        for key in keys {
            match cur.get(key) {
                Some(next) => cur = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            return Some(cur.clone());
        }
    }
    None
}

/// The gate: autonomy master switch, then the FNO_THINK_SPAWN env override,
/// then config.think_spawn.enabled, then default off. Any settings-read
/// failure degrades to disabled. Returns (armed, granting rank).
fn think_spawn_resolve(get: &impl Fn(&str) -> Option<String>) -> (bool, &'static str) {
    think_spawn_resolve_with(get, &config_flag)
}

fn think_spawn_resolve_with(
    get: &impl Fn(&str) -> Option<String>,
    config: &impl Fn(&[&str]) -> Option<Value>,
) -> (bool, &'static str) {
    // Autonomy panic switch: config.autonomy.enabled, default on.
    match config(&["autonomy", "enabled"]) {
        Some(Value::Bool(false)) => return (false, "autonomy"),
        _ => {}
    }
    if let Some(override_value) = get(ENV_OVERRIDE) {
        return (truthy(&override_value), "env");
    }
    match config(&["think_spawn", "enabled"]) {
        Some(Value::Bool(b)) => (b, "config"),
        _ => (false, "default"),
    }
}

/// The blast-radius cap from config, fail-safe to 5.
fn max_per_run() -> usize {
    match config_flag(&["think_spawn", "max_per_run"]) {
        Some(Value::Number(n)) => n.as_u64().map(|v| v as usize).unwrap_or(5),
        _ => 5,
    }
}

/// The configured user-facing name: config user.name, else the git identity
/// at the repo root, else "you".
fn display_name(root: &Path) -> String {
    if let Some(name) = config_flag(&["user", "name"]).and_then(|v| v.as_str().map(str::to_string))
    {
        if !name.trim().is_empty() {
            return name;
        }
    }
    let out = std::process::Command::new("git")
        .args(["config", "user.name"])
        .current_dir(root)
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "you".to_string()
}

/// The first `<key>: <value>` value in a target-state.md manifest.
pub(crate) fn scan_md_field(text: &str, key: &str) -> Option<String> {
    let pattern = format!("(?m)^\\s*{}:\\s*(.+)", key.replace(':', "\\:"));
    let re = regex::Regex::new(&pattern).ok()?;
    let value = re.captures(text)?.get(1)?.as_str().trim().to_string();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return Some(value[1..value.len() - 1].to_string());
        }
    }
    Some(value)
}

/// The owned manifest's attended flag, when this process provably owns it.
fn owned_manifest_attended(root: &Path, session: &str) -> Option<bool> {
    let text = std::fs::read_to_string(root.join(".fno").join("target-state.md")).ok()?;
    let manifest_sid = scan_md_field(&text, "claude_session_id")
        .or_else(|| scan_md_field(&text, "claude_transcript_id"))?;
    if manifest_sid != session {
        return None;
    }
    scan_md_field(&text, "attended").map(|v| truthy(&v))
}

/// Whether the ambient env alone says no operator is at this session.
fn env_marks_unattended(get: &impl Fn(&str) -> Option<String>) -> bool {
    if get("TARGET_UNATTENDED").is_some_and(|v| v.trim() == "1") {
        return true;
    }
    let spawned = get("FNO_AGENT_SELF").is_some_and(|v| !v.trim().is_empty())
        || get("FNO_BG").is_some_and(|v| truthy(&v));
    if !spawned {
        return false;
    }
    // An UNKNOWN substrate on a spawned worker reads unattended; only a
    // pane (an operator can answer a prompt there) reads attended.
    let substrate = get("FNO_AGENT_SUBSTRATE").unwrap_or_default();
    substrate.trim() != "pane"
}

/// Classify the originating session as attended or away. The override wins,
/// then the ambient unattended marks, then the owned manifest, then an
/// interactive claude/codex session identity, then away.
fn classify_presence(root: &Path, get: &impl Fn(&str) -> Option<String>) -> String {
    if let Some(override_value) = get(ENV_PRESENCE) {
        let v = override_value.trim().to_lowercase();
        if v == "attended" || v == "away" {
            return v;
        }
    }
    if env_marks_unattended(get) {
        return "away".to_string();
    }
    let get_env = |name: &str| std::env::var(name).ok();
    let ident = crate::spawn_context::resolve_self_identity(
        &get_env,
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    let session = ident.session_id.clone().unwrap_or_default();
    let harness = ident.harness.clone().unwrap_or_default();
    if !session.is_empty() {
        if let Some(attended) = owned_manifest_attended(root, &session) {
            return if attended { "attended" } else { "away" }.to_string();
        }
    }
    if !session.is_empty() && (harness == "claude" || harness == "codex") {
        return "attended".to_string();
    }
    "away".to_string()
}

/// The claude transcript for a session id, searched across every project dir
/// (EnterWorktree re-keys transcripts between cwd slugs; a stub in the other
/// dir is common). A dotted stem is a sibling artifact, never a transcript.
fn resolve_claude_transcript(session_id: &str) -> Option<PathBuf> {
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".claude").join("projects"))?;
    let mut matches: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return None;
    };
    for dir in entries.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.starts_with(session_id)
                && name.ends_with(".jsonl")
                && !name[..name.len() - 6].contains('.')
            {
                matches.push(file.path());
            }
        }
    }
    matches.sort();
    match matches.len() {
        0 => None,
        1 => Some(matches.remove(0)),
        _ => {
            let stems: std::collections::BTreeSet<String> = matches
                .iter()
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            if stems.len() > 1 {
                return Some(matches.remove(0)); // ambiguous: first-sorted, contract kept
            }
            // Same session copied across dirs: prefer copies that actually
            // carry conversation, newest write among those.
            let with_convo: Vec<PathBuf> = matches
                .iter()
                .filter(|p| claude_has_conversation(p))
                .cloned()
                .collect();
            newest_mtime(if with_convo.is_empty() {
                &matches
            } else {
                &with_convo
            })
        }
    }
}

fn claude_has_conversation(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if matches!(
            rec.get("type").and_then(Value::as_str),
            Some("user") | Some("assistant")
        ) {
            return true;
        }
    }
    false
}

fn newest_mtime(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for p in paths {
        let Ok(mt) = p.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| mt > *b) {
            best = Some((mt, p.clone()));
        }
    }
    best.map(|(_, p)| p)
}

/// Resolve a provenance pointer to its on-disk transcript path. Never fails;
/// an unresolved pointer degrades the seed to the stored triple.
fn resolve_transcript(harness: &str, session_id: &str, cwd: &str) -> (Option<PathBuf>, bool) {
    if session_id.is_empty() {
        return (None, false);
    }
    let path = match harness {
        "claude" if !cwd.is_empty() => resolve_claude_transcript(session_id),
        "codex" => crate::codex_store::codex_rollout_path(None, session_id),
        // opencode keys its SQLite store on the session id; the probe stays
        // out of the birth path's scope here and degrades unresolved.
        _ => None,
    };
    let resolved = path.is_some();
    (path, resolved)
}

/// The seed's one copy-pasteable line: with a resolved transcript it carries
/// the pointer, else the bare /think line.
fn offer_line_for(node_id: &str, resolved: bool, transcript: Option<&Path>) -> String {
    if resolved {
        if let Some(path) = transcript {
            return format!("/think {node_id}  # origin transcript: {}", path.display());
        }
    }
    format!("/think {node_id}")
}

/// The journal the offer/skip events land in: FNO_EVENTS_PATH when the env
/// pins it, else the repo root's .fno/events.jsonl.
fn events_path(project_root: Option<&Path>) -> PathBuf {
    if let Some(pinned) = std::env::var_os("FNO_EVENTS_PATH").filter(|v| !v.is_empty()) {
        return PathBuf::from(pinned);
    }
    let root = project_root.unwrap_or_else(|| Path::new("."));
    root.join(".fno").join("events.jsonl")
}

/// Best-effort event emit through the native store. Never raises.
fn emit(kind: &str, data: Map<String, Value>, events_path: &Path) {
    let emitter = crate::events::EventEmitter::new(events_path, EVENT_SOURCE);
    if let Err(e) = emitter.emit_fields(kind, data) {
        eprintln!("spawn_think: WARNING: event emit failed ({kind}): {e}");
    }
}

/// The birth-hook decision ladder for an unforced birth: gate, id, bulk
/// intake, origin, design-warranting, blast cap, presence + seed, then the
/// OFFER. Steps past the offer (day cap, dedup token, bg dispatch) are
/// unreachable for an unforced birth and stay unported.
pub fn maybe_spawn_think(
    node: &Value,
    project_root: Option<&Path>,
    events_path_override: Option<&Path>,
    get: &impl Fn(&str) -> Option<String>,
) -> ThinkSpawnResult {
    let ev_path: PathBuf = match events_path_override {
        Some(p) => p.to_path_buf(),
        None => events_path(Some(project_root.unwrap_or(Path::new(".")))),
    };
    let node_id = node.get("id").and_then(Value::as_str).unwrap_or_default();
    let (armed, rank) = think_spawn_resolve(get);
    if !armed {
        return ThinkSpawnResult {
            kind: "noop",
            event: None,
            reason: Some("disabled".into()),
            node_id: None,
            presence: None,
            resolved: None,
            offer_line: None,
        };
    }
    let root = project_root.unwrap_or_else(|| Path::new("."));
    let skip =
        |reason: &str, detail: Option<String>, presence: Option<String>| -> ThinkSpawnResult {
            let mut data = Map::new();
            data.insert("reason".into(), Value::String(reason.into()));
            data.insert("trigger".into(), Value::String(REASON_BIRTH.into()));
            data.insert("rank".into(), Value::String(rank.into()));
            if !node_id.is_empty() {
                data.insert("node_id".into(), Value::String(node_id.into()));
            }
            if let Some(d) = &detail {
                data.insert("detail".into(), Value::String(d.clone()));
            }
            emit(EVENT_SKIPPED, data, &ev_path);
            ThinkSpawnResult {
                kind: "skipped",
                event: Some(EVENT_SKIPPED),
                reason: Some(reason.into()),
                node_id: Some(node_id.into()),
                presence,
                resolved: None,
                offer_line: None,
            }
        };
    // 1. Eligibility: a node must have a usable id.
    if node_id.is_empty() {
        return skip("no-node-id", None, None);
    }
    // 2. Bulk roadmap/vision intake is excluded. Null/empty are absent,
    // matching Python's falsy .get().
    let has_roadmap = node
        .get("roadmap_id")
        .and_then(Value::as_str)
        .is_some_and(|v| !v.trim().is_empty());
    let has_vision = node
        .get("vision_path")
        .and_then(Value::as_str)
        .is_some_and(|v| !v.trim().is_empty());
    if has_roadmap || has_vision {
        return skip("bulk-intake", None, None);
    }
    // 3. A node with no captured origin cannot carry a why.
    let origin = node
        .get("source_session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if origin.is_empty() {
        return skip("no-origin", None, None);
    }
    // 3b. Automatic fan-out is for design-warranting work, not a small bug.
    let node_type = node
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let node_size = node
        .get("size")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_uppercase();
    if node_type == "bug" || node_size == "S" {
        // The `or '?'` spelling: an absent word reads as ?, never empty.
        let type_word = if node_type.is_empty() {
            "?"
        } else {
            node_type.as_str()
        };
        let size_word = if node_size.is_empty() {
            "?"
        } else {
            node_size.as_str()
        };
        return skip(
            "not-design-warranting",
            Some(format!("type={type_word} size={size_word}")),
            None,
        );
    }
    // 4. Blast-radius cap over the run. One node per run here, so the cap
    // binds only at a cap of 0.
    if max_per_run() == 0 {
        return skip("cap-exceeded", Some("max_per_run=0".into()), None);
    }
    // 5. Presence + seed.
    let presence = classify_presence(root, get);
    let harness = node
        .get("source_harness")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let origin_cwd = node
        .get("source_cwd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (transcript, resolved) = resolve_transcript(harness, &origin, &origin_cwd);
    let offer_line = offer_line_for(node_id, resolved, transcript.as_deref());
    // 6. An UNFORCED birth must never silently auto-spawn: offer, once.
    eprintln!(
        "spawn_think: OFFER PENDING (nothing spawned). Ask {} whether to run `{offer_line}` now, or skip.",
        display_name(root)
    );
    let mut data = Map::new();
    data.insert("node_id".into(), Value::String(node_id.into()));
    data.insert("trigger".into(), Value::String(REASON_BIRTH.into()));
    data.insert("presence".into(), Value::String(presence.clone()));
    data.insert("resolved".into(), Value::Bool(resolved));
    data.insert("offer_line".into(), Value::String(offer_line.clone()));
    data.insert("rank".into(), Value::String(rank.into()));
    emit(EVENT_OFFERED, data, &ev_path);
    ThinkSpawnResult {
        kind: "offered",
        event: Some(EVENT_OFFERED),
        reason: None,
        node_id: Some(node_id.into()),
        presence: Some(presence),
        resolved: Some(resolved),
        offer_line: Some(offer_line),
    }
}

/// Single post-persist birth hook. Gate-first (a default-OFF install pays
/// nothing), strictly non-fatal, and the born node is re-read by id after
/// the write so the seed reads the durable (possibly re-slugged) row.
/// `events_override` pins the journal (the door passes the caller's resolved
/// project journal so the events never land cwd-relative).
pub fn on_node_born(
    graph: &Path,
    node: &Value,
    events_override: Option<&Path>,
) -> Option<ThinkSpawnResult> {
    let node_id = node.get("id").and_then(Value::as_str).unwrap_or_default();
    let get = |name: &str| std::env::var(name).ok();
    let (armed, _rank) = think_spawn_resolve(&get);
    if node_id.is_empty() || !armed {
        return None;
    }
    // Durable re-read: the store may have re-slugged the row inside the
    // write; the seed must read the durable node, not the builder's dict.
    let durable = crate::graph_store::read_rows(graph)
        .ok()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(node_id))
                .cloned()
        })
        .unwrap_or_else(|| node.clone());
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        maybe_spawn_think(&durable, None, events_override, &get)
    }))
    .ok()
}

/// The door the Python birth forwarder execs: `fno-agents backlog birth-hook
/// --graph <path> --node-id <id> [--events-path <journal>]`. Transport only -
/// the ladder stays in [`maybe_spawn_think`]/[`on_node_born`], whose offer
/// line and events land inside this process. Prints the result JSON on
/// stdout (`null` when the gate sent nothing anywhere) and exits 0; a
/// malformed argv exits 2. `--events-path` pins the journal so a forwarded
/// birth never writes a cwd-relative `.fno/events.jsonl`.
pub fn run_birth_hook(tail: &[String]) -> i32 {
    const USAGE: &str = "usage: birth-hook --graph <path> --node-id <id> [--events-path <journal>]";
    let mut graph: Option<PathBuf> = None;
    let mut events: Option<PathBuf> = None;
    let mut node_id = String::new();
    let mut it = tail.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--graph" => graph = it.next().map(PathBuf::from),
            "--node-id" => node_id = it.next().cloned().unwrap_or_default(),
            "--events-path" => events = it.next().map(PathBuf::from),
            other => {
                eprintln!("birth-hook: unknown argument {other}\n{USAGE}");
                return 2;
            }
        }
    }
    let Some(graph) = graph else {
        eprintln!("birth-hook: --graph <path> is required\n{USAGE}");
        return 2;
    };
    if node_id.is_empty() {
        eprintln!("birth-hook: --node-id <id> is required\n{USAGE}");
        return 2;
    }
    let payload = match on_node_born(
        &graph,
        &serde_json::json!({ "id": node_id }),
        events.as_deref(),
    ) {
        None => Value::Null,
        Some(r) => serde_json::json!({
            "kind": r.kind,
            "event": r.event,
            "reason": r.reason,
            "node_id": r.node_id,
            "presence": r.presence,
            "resolved": r.resolved,
            "offer_line": r.offer_line,
        }),
    };
    println!("{payload}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_override_ranks_above_config() {
        // No config at all: the ranks resolve without ambient files.
        let no_config = |_keys: &[&str]| -> Option<Value> { None };
        let get = |name: &str| -> Option<String> {
            if name == ENV_OVERRIDE {
                Some("1".into())
            } else {
                None
            }
        };
        let (armed, rank) = think_spawn_resolve_with(&get, &no_config);
        assert!(armed);
        assert_eq!(rank, "env");
        let get = |name: &str| -> Option<String> {
            if name == ENV_OVERRIDE {
                Some("0".into())
            } else {
                None
            }
        };
        let (armed, rank) = think_spawn_resolve_with(&get, &no_config);
        assert!(!armed);
        assert_eq!(rank, "env");
    }

    #[test]
    fn the_config_rank_and_the_default_rank_resolve() {
        let no_config = |_keys: &[&str]| -> Option<Value> { None };
        let no_env = |_name: &str| -> Option<String> { None };
        let (armed, rank) = think_spawn_resolve_with(&no_env, &no_config);
        assert!(!armed);
        assert_eq!(rank, "default");
        let enabled = |keys: &[&str]| -> Option<Value> {
            if keys == ["think_spawn", "enabled"] {
                Some(Value::Bool(true))
            } else {
                None
            }
        };
        let (armed, rank) = think_spawn_resolve_with(&no_env, &enabled);
        assert!(armed);
        assert_eq!(rank, "config");
        // The autonomy panic switch outranks the env override.
        let autonomy_off = |keys: &[&str]| -> Option<Value> {
            if keys == ["autonomy", "enabled"] {
                Some(Value::Bool(false))
            } else {
                None
            }
        };
        let get = |name: &str| -> Option<String> {
            if name == ENV_OVERRIDE {
                Some("1".into())
            } else {
                None
            }
        };
        let (armed, rank) = think_spawn_resolve_with(&get, &autonomy_off);
        assert!(!armed);
        assert_eq!(rank, "autonomy");
    }

    #[test]
    fn the_offer_line_degrades_without_a_transcript() {
        assert_eq!(
            offer_line_for("ab-1234abcd", false, None),
            "/think ab-1234abcd"
        );
    }

    #[test]
    fn scan_md_field_strips_quotes() {
        let text = "claude_session_id: \"abc-123\"\nattended: true\n";
        assert_eq!(
            scan_md_field(text, "claude_session_id").as_deref(),
            Some("abc-123")
        );
        assert_eq!(scan_md_field(text, "attended").as_deref(), Some("true"));
        assert_eq!(scan_md_field(text, "graph_node_id"), None);
    }

    #[test]
    fn the_birth_hook_door_refuses_a_malformed_argv() {
        let argv =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() };
        assert_eq!(run_birth_hook(&argv(&["--node-id", "ab-1"])), 2);
        assert_eq!(run_birth_hook(&argv(&["--graph", "/tmp/g.db"])), 2);
        assert_eq!(
            run_birth_hook(&argv(&["--graph", "/tmp/g.db", "--bogus"])),
            2
        );
        assert_eq!(
            run_birth_hook(&argv(&["--graph", "/tmp/g.db", "--node-id", "ab-1"])),
            0
        );
        assert_eq!(
            run_birth_hook(&argv(&[
                "--graph",
                "/tmp/g.db",
                "--node-id",
                "ab-1",
                "--events-path",
                "/tmp/ev.jsonl",
            ])),
            0
        );
    }
}
