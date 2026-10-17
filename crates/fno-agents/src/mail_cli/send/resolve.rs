// Ported from the single-file draft of send.rs; split by question.
use super::*;

/// Python `is_full_session_id` (store_fallback): 32-hex or dashed-uuid shape.
pub(super) fn is_full_session_id(token: &str) -> bool {
    let stripped: String = token.chars().filter(|c| *c != '-').collect();
    if stripped.len() != 32 {
        return false;
    }
    stripped.chars().all(|c| c.is_ascii_hexdigit())
}

/// Python `is_session_shaped`: a full id or an 8-hex short handle.
pub(super) fn is_session_shaped(token: &str) -> bool {
    is_full_session_id(token) || is_unsafe_short_address(token)
}

// ---------------------------------------------------------------------------
// Address resolution: the five durable stores
// ---------------------------------------------------------------------------

/// Python `ReachableSession`: one stored session a token can name.
#[derive(Clone, Debug)]
pub(super) struct ReachableSession {
    pub(super) session_id: String,
    pub(super) source: &'static str,
    pub(super) agent: String,
    pub(super) cwd: Option<String>,
}

/// The Python `resolve_reachable` result: `Ok((hit, ambiguous))` on clean
/// reads, `Err((degraded, resolved))` when a store could not be read. A
/// degraded read with exactly one hit still carries the hit: the caller
/// refuses before any delivery side effect (unproven uniqueness).
pub(super) struct ReachResult {
    pub(super) hit: Option<ReachableSession>,
    pub(super) ambiguous: Vec<String>,
    pub(super) degraded: Vec<&'static str>,
}

/// Python `_token_matches`: full id or 8-hex short, case-folded hex.
pub(super) fn token_matches(token: &str, session_id: &str) -> bool {
    if token.is_empty() || session_id.is_empty() {
        return false;
    }
    crate::identity::session_handle_tier(token, session_id).is_some()
}

/// Python `_decode_project_dir`: best-effort cwd from a claude projects dir
/// name. The encoding is lossy, so this stays a guess (verbatim=False).
pub(super) fn decode_project_dir(name: &str) -> Option<String> {
    if !name.starts_with('-') {
        return None;
    }
    let candidate = format!("/{}", name[1..].replace('-', "/"));
    if Path::new(&candidate).is_dir() {
        Some(candidate)
    } else {
        None
    }
}

pub(super) fn projects_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("projects");
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
        .join(".claude")
        .join("projects")
}

pub(super) fn roster_path() -> PathBuf {
    let base = std::env::var("FNO_CLAUDE_DAEMON_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
                .join(".claude")
                .join("daemon")
        });
    base.join("roster.json")
}

/// Python `_alias_to_session_ids`: alias map inversion plus the registry rows'
/// alias lists. `Err(())` reads degraded (a torn store is not a clean miss).
pub(super) fn alias_to_session_ids(token: &str) -> Result<Vec<String>, ()> {
    let path = AgentsHome::from_env().root().join("name-map.json");
    let mut hits: Vec<String> = Vec::new();
    if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|_| ())?;
        let doc: Value = serde_json::from_str(&raw).map_err(|_| ())?;
        let Some(obj) = doc.as_object() else {
            return Err(());
        };
        for (sid, alias) in obj {
            if alias.as_str() == Some(token) {
                hits.push(sid.clone());
            }
        }
    }
    let registry = AgentsHome::shared_registry_json();
    if let Ok(rows) = crate::client_verbs::load_registry_entries(&registry) {
        for row in &rows {
            let aliases = row.get("aliases").and_then(Value::as_array);
            if let Some(aliases) = aliases {
                if aliases.iter().any(|a| a.as_str() == Some(token)) {
                    if let Some(sid) = opt_str_field(row, "harness_session_id")
                        .or_else(|| opt_str_field(row, "short_id"))
                    {
                        if !hits.contains(&sid) {
                            hits.push(sid);
                        }
                    }
                }
            }
        }
    }
    Ok(hits)
}

/// Python `_reachable_from_registry`.
pub(super) fn reachable_from_registry(token: &str) -> (Vec<ReachableSession>, bool) {
    let registry = AgentsHome::shared_registry_json();
    let rows = match crate::client_verbs::load_registry_entries(&registry) {
        Ok(rows) => rows,
        Err(_) => return (Vec::new(), false),
    };
    let mut hits: Vec<ReachableSession> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    for e in &rows {
        let Some(sid) = opt_str_field(e, "harness_session_id") else {
            continue;
        };
        let harness = opt_str_field(e, "harness").unwrap_or_else(|| "claude".to_string());
        let key = (
            harness.clone(),
            crate::spawn_context::session_identity_key(&sid),
        );
        if token_matches(token, &sid) && !seen.contains(&key) {
            seen.insert(key);
            hits.push(ReachableSession {
                session_id: sid,
                source: "registry",
                agent: harness,
                cwd: opt_str_field(e, "cwd"),
            });
        }
    }
    (hits, true)
}

/// Python `_reachable_from_transcripts`: `<uuid>.jsonl` files inside the
/// claude projects subdirs. An absent root is a definitive empty; an
/// unreadable root is a failure.
pub(super) fn reachable_from_transcripts(token: &str) -> (Vec<ReachableSession>, bool) {
    let root = projects_dir();
    if !root.exists() {
        return (Vec::new(), true);
    }
    let read = match std::fs::read_dir(&root) {
        Ok(read) => read,
        Err(_) => return (Vec::new(), false),
    };
    let mut hits: Vec<ReachableSession> = Vec::new();
    for entry in read.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let files = match std::fs::read_dir(&dir) {
            Ok(files) => files,
            Err(_) => return (Vec::new(), false),
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if token_matches(token, stem) {
                hits.push(ReachableSession {
                    session_id: stem.to_string(),
                    source: "transcript",
                    agent: "claude".to_string(),
                    cwd: dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(decode_project_dir),
                });
            }
        }
    }
    (hits, true)
}

/// Python `_reachable_from_roster`: the claude daemon roster rows, exited
/// ones included. Absent file: real empty. Type-drifted: failure.
pub(super) fn reachable_from_roster(token: &str) -> (Vec<ReachableSession>, bool) {
    let path = roster_path();
    if !path.exists() {
        return (Vec::new(), true);
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return (Vec::new(), false),
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return (Vec::new(), false);
    };
    let Some(workers) = doc.get("workers").and_then(Value::as_object) else {
        return (Vec::new(), false);
    };
    let mut hits: Vec<ReachableSession> = Vec::new();
    for (_key, row) in workers {
        let Some(sid) = row.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        if token_matches(token, sid) {
            hits.push(ReachableSession {
                session_id: sid.to_string(),
                source: "roster",
                agent: "claude".to_string(),
                cwd: opt_str_field(row, "cwd"),
            });
        }
    }
    (hits, true)
}

/// Python `_reachable_from_graph`: session stamps on backlog nodes. The
/// malformed-tolerant read Python used has no Rust twin, so an unreadable
/// store reads degraded, never empty.
pub(super) fn reachable_from_graph(token: &str) -> (Vec<ReachableSession>, bool) {
    let graph_path = crate::org_board::scope::graph_json_path(Path::new("."));
    let Ok(rows) = crate::backlog::read_entries(&graph_path) else {
        return (Vec::new(), false);
    };
    let mut hits: Vec<ReachableSession> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    for node in &rows {
        let Some(sessions) = node.get("sessions").and_then(Value::as_array) else {
            continue;
        };
        for entry in sessions {
            let Some(sid) = entry.get("session_id").and_then(Value::as_str) else {
                continue;
            };
            let harness = entry
                .get("harness")
                .and_then(Value::as_str)
                .filter(|h| !h.is_empty())
                .unwrap_or("claude");
            let key = (
                harness.to_string(),
                crate::spawn_context::session_identity_key(sid),
            );
            if token_matches(token, sid) && !seen.contains(&key) {
                seen.insert(key);
                hits.push(ReachableSession {
                    session_id: sid.to_string(),
                    source: "graph",
                    agent: harness.to_string(),
                    cwd: node.get("cwd").and_then(Value::as_str).map(str::to_string),
                });
            }
        }
    }
    (hits, true)
}

/// Python `_reachable_from_harness_stores` over
/// `store_fallback.complete_store_hits`. The per-harness store probes
/// (claude/codex/opencode JSONL scans) have no Rust twin yet, so this leg
/// reads clean and empty rather than degrading on stores it never opened;
/// the token ladder's other four legs carry resolution.
pub(super) fn reachable_from_harness_stores(_token: &str) -> (Vec<ReachableSession>, bool) {
    (Vec::new(), true)
}

/// Python `resolve_reachable`: the union with the same merge, ambiguity and
/// degradation rules.
pub(super) fn resolve_reachable(token: &str) -> ReachResult {
    if token.trim().is_empty() {
        return ReachResult {
            hit: None,
            ambiguous: Vec::new(),
            degraded: Vec::new(),
        };
    }
    let mut tokens: Vec<String> = vec![token.to_string()];
    let mut degraded: Vec<&'static str> = Vec::new();
    match alias_to_session_ids(token) {
        Ok(more) => tokens.extend(more),
        Err(()) => degraded.push("registry"),
    }
    type Source = fn(&str) -> (Vec<ReachableSession>, bool);
    let sources: [(&'static str, Source); 5] = [
        ("transcript", reachable_from_transcripts),
        ("harness-store", reachable_from_harness_stores),
        ("registry", reachable_from_registry),
        ("roster", reachable_from_roster),
        ("graph", reachable_from_graph),
    ];
    let mut found: std::collections::HashMap<(String, String), ReachableSession> =
        Default::default();
    let mut cwd_verbatim: std::collections::HashMap<(String, String), bool> = Default::default();
    for (source, lookup) in sources {
        for tok in &tokens {
            let (hits, read_ok) = lookup(tok);
            if !read_ok && !degraded.contains(&source) {
                degraded.push(source);
            }
            if !read_ok {
                continue;
            }
            for hit in hits {
                let key = (
                    hit.agent.clone(),
                    crate::spawn_context::session_identity_key(&hit.session_id),
                );
                let prior = found.get(&key);
                let take_cwd = match prior {
                    None => true,
                    Some(prior) => {
                        (prior.cwd.is_none() && hit.cwd.is_some())
                            || (hit.cwd.is_some()
                                && source_matches_verbatim(source)
                                && !cwd_verbatim.get(&key).copied().unwrap_or(false))
                    }
                };
                if let std::collections::hash_map::Entry::Vacant(slot) = found.entry(key.clone()) {
                    slot.insert(hit);
                    cwd_verbatim.insert(key.clone(), source_is_verbatim(source));
                } else if take_cwd {
                    let prior = found.get_mut(&key).expect("checked above");
                    prior.cwd = hit.cwd;
                    cwd_verbatim.insert(key.clone(), source_is_verbatim(source));
                }
            }
        }
    }
    if found.len() > 1 {
        let mut ids: Vec<String> = found.values().map(|f| f.session_id.clone()).collect();
        ids.sort();
        named_ambiguity(token, &found);
        return ReachResult {
            hit: None,
            ambiguous: ids,
            degraded: Vec::new(),
        };
    }
    if found.len() == 1 {
        let hit = found.into_values().next().expect("len == 1");
        if !degraded.is_empty() {
            return ReachResult {
                hit: Some(hit),
                ambiguous: Vec::new(),
                degraded,
            };
        }
        return ReachResult {
            hit: Some(hit),
            ambiguous: Vec::new(),
            degraded: Vec::new(),
        };
    }
    ReachResult {
        hit: None,
        ambiguous: Vec::new(),
        degraded,
    }
}

pub(super) fn source_is_verbatim(source: &str) -> bool {
    matches!(source, "registry" | "roster" | "graph" | "harness-store")
}

pub(super) fn source_matches_verbatim(source: &str) -> bool {
    source_is_verbatim(source)
}

/// Python `_report_ambiguity`: names each candidate and its first source.
pub(super) fn named_ambiguity(
    token: &str,
    found: &std::collections::HashMap<(String, String), ReachableSession>,
) {
    let mut named: Vec<String> = found
        .values()
        .map(|f| format!("{} (source={}, agent={})", f.session_id, f.source, f.agent))
        .collect();
    named.sort();
    eprintln!(
        "resolve_reachable: token {} is ambiguous: {}",
        py_repr(token),
        named.join(", ")
    );
}

// ---------------------------------------------------------------------------
// Token exits (cli.py)
// ---------------------------------------------------------------------------

pub(super) fn ambiguous_token_exit(name: &str, candidates: &[String]) -> i32 {
    eprintln!(
        "ambiguous session token {}: matches {}. Send to a full session id.",
        py_repr(name),
        candidates.join(", ")
    );
    2
}

pub(super) fn unreachable_token_exit(name: &str, candidates: &[String], code: i32) -> i32 {
    let hint = if candidates.is_empty() {
        String::new()
    } else {
        format!(" Closest live sessions: {}.", candidates.join(", "))
    };
    eprintln!(
        "unknown agent or live-session handle: {}.{hint}",
        py_repr(name)
    );
    code
}

pub(super) fn unavailable_token_exit(name: &str, failed: &[String], candidates: &[String]) -> i32 {
    let visible = if candidates.is_empty() {
        String::new()
    } else {
        format!(" Visible candidates: {}.", candidates.join(", "))
    };
    let line = format!(
        "cannot resolve short session token {}: unreadable stores: {}.{} Send to a full session id.",
        py_repr(name),
        failed.join(", "),
        visible
    );
    eprintln!("{line}");
    2
}

// ---------------------------------------------------------------------------
// The name lane and the dispatch engine
// ---------------------------------------------------------------------------

/// Python `_self_recipient`: canonical own address after resolution.
pub(super) fn self_recipient(
    token: &str,
    resolved_session_id: Option<&str>,
    full_only: bool,
) -> Option<String> {
    let own = resolve_self_session().unwrap_or_default();
    if own.is_empty() {
        return None;
    }
    let candidate = resolved_session_id.unwrap_or(token);
    let tier = crate::identity::session_handle_tier(candidate, &own);
    let tier = tier?;
    if (resolved_session_id.is_some() && tier != 0) || (full_only && tier != 0) {
        return None;
    }
    Some(crate::identity::canonical_handle(&own))
}

/// Python `_reply_session_for`: the collision-safe sender session id, or None
/// when --from-name named a third address.
pub(super) fn reply_session_for(from_name: Option<&str>) -> Option<String> {
    let session = resolve_self_session()?;
    let named = from_name?.trim().to_string();
    if named.is_empty() {
        return Some(session);
    }
    let key = crate::spawn_context::session_identity_key(&session);
    let exact = [
        key.chars().take(8).collect::<String>(),
        key,
        session.clone(),
    ];
    if exact.contains(&named) {
        return Some(session);
    }
    let hex_fold = named.chars().all(|c| c.is_ascii_hexdigit() || c == '-') && !named.is_empty();
    if hex_fold {
        let lowered = named.to_lowercase();
        if exact.iter().any(|c| c.to_lowercase() == lowered) {
            return Some(session);
        }
    }
    None
}

/// Python `_resolve_token`: reachability BEFORE the envelope is addressed.
pub(super) fn resolve_token(
    token: &str,
) -> Result<(Option<ReachableSession>, Option<String>), TokenFailure> {
    let reach = resolve_reachable(token);
    // A StoreReadError outranks ambiguity exactly as the Python exception
    // ordering does: an unreadable store with a lone hit is the unproven-
    // uniqueness case, not a clean multi-match.
    if !reach.degraded.is_empty() {
        if !is_full_session_id(token) {
            let candidates: Vec<String> = reach
                .hit
                .as_ref()
                .map(|h| vec![h.session_id.clone()])
                .unwrap_or_default();
            return Err(TokenFailure::Unavailable {
                failed: reach.degraded.iter().map(|s| s.to_string()).collect(),
                candidates,
            });
        }
        return Ok((
            reach.hit,
            Some(format!(
                "wake=stores-unreadable({})",
                reach.degraded.join(",")
            )),
        ));
    }
    if !reach.ambiguous.is_empty() {
        return Err(TokenFailure::Ambiguous {
            candidates: reach.ambiguous.clone(),
        });
    }
    Ok((reach.hit, None))
}

pub(super) enum TokenFailure {
    Ambiguous {
        candidates: Vec<String>,
    },
    Unavailable {
        failed: Vec<String>,
        candidates: Vec<String>,
    },
}

/// Python `_is_job_name` / `is_job_token`.
pub(super) fn is_job_token(token: &str) -> bool {
    token.starts_with("node:") || token.starts_with("pr:")
}
