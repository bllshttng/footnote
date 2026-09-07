//! The hidden `fno-agents claim` debug front over the native claims module.
//!
//! Extracted from client_verbs.rs, which it had outgrown: the cross-impl
//! compatibility matrix (`cli/tests/integration/test_claims_cross_impl.py`)
//! drives the Rust side of the lockfile protocol through these verbs, and the
//! claim-sweep JSON assembly (the serializer the Python claim classifier golden
//! is byte-checked against) lives here too. Dispatch stays in `bin/client.rs`
//! via the `mail-inject` `matches!` pattern, so the verb remains hidden from
//! `CLIENT_VERB_USAGE` / `RUST_CLIENT_VERBS`; `fno agents claim` is still the
//! only operator CLI for claims.

use serde_json::Value;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

/// `fno-agents claim <acquire|release|status|list> <key> [flags]` — a thin front
/// over [`crate::claims`], the native lockfile-protocol implementation.
///
/// Purpose: (a) the cross-impl compatibility matrix
/// (`cli/tests/integration/test_claims_cross_impl.py`) drives the Rust side
/// of the protocol through it, and (b) an ops escape hatch when the Python
/// CLI is unavailable. It is deliberately HIDDEN — dispatched via `matches!`
/// in `bin/client.rs` (the `mail-inject` pattern) so it stays out of
/// `CLIENT_VERB_USAGE` / `RUST_CLIENT_VERBS`; `fno agents claim` remains the only
/// operator CLI for claims.
///
/// Output is one JSON object on stdout. Exit codes: 0 success, 1 held by
/// another live writer, 2 usage/validation/io error.
pub fn run_claim(args: &[String]) -> i32 {
    let Some(op) = args.first().map(String::as_str) else {
        eprintln!("fno-agents: claim requires an operation: acquire|release|status|list|sweep");
        return 2;
    };
    if op == "sweep" {
        return run_claim_sweep(&args[1..]);
    }
    if op == "list" {
        return run_claim_list(&args[1..]);
    }
    let Some(key) = args.get(1).filter(|k| !k.starts_with("--")).cloned() else {
        eprintln!("fno-agents: claim {op} requires a key argument");
        return 2;
    };

    let mut holder: Option<String> = None;
    let mut opts = crate::claims::AcquireOpts::default();
    let mut it = args[2..].iter();
    while let Some(a) = it.next() {
        let mut take = |name: &str| -> Option<String> {
            let v = it.next().cloned();
            if v.is_none() {
                eprintln!("fno-agents: claim: {name} requires a value");
            }
            v
        };
        match a.as_str() {
            "--holder" => holder = take("--holder"),
            "--pid" => match take("--pid").and_then(|v| v.parse::<u32>().ok()) {
                Some(p) => opts.pid = Some(p),
                None => return 2,
            },
            "--pid-unavailable" => opts.pid_unavailable = true,
            "--ttl-ms" => match take("--ttl-ms").and_then(|v| v.parse::<i64>().ok()) {
                Some(t) => opts.ttl_ms = Some(t),
                None => return 2,
            },
            "--reason" => match take("--reason") {
                Some(r) => opts.reason = Some(r),
                None => return 2,
            },
            "--metadata" => {
                let Some(raw) = take("--metadata") else {
                    return 2;
                };
                match serde_json::from_str::<Value>(&raw) {
                    Ok(Value::Object(m)) => opts.metadata = Some(m),
                    _ => {
                        eprintln!("fno-agents: claim: --metadata must be a JSON object");
                        return 2;
                    }
                }
            }
            "--root" => match take("--root") {
                Some(r) => opts.root = Some(PathBuf::from(r)),
                None => return 2,
            },
            "--json" | "-J" => {} // output is always JSON; accepted for symmetry
            other => {
                eprintln!("fno-agents: claim: unknown flag {other}");
                return 2;
            }
        }
    }

    match op {
        "acquire" => {
            let Some(holder) = holder else {
                eprintln!("fno-agents: claim acquire requires --holder");
                return 2;
            };
            match crate::claims::acquire(&key, &holder, opts) {
                crate::claims::AcquireOutcome::Acquired(rec) => {
                    let mut out = serde_json::to_value(&rec)
                        .unwrap_or_else(|_| Value::Object(Default::default()));
                    if let Value::Object(m) = &mut out {
                        m.insert("outcome".into(), Value::String("acquired".into()));
                    }
                    println!("{out}");
                    0
                }
                crate::claims::AcquireOutcome::HeldByOther { holder, pid, host } => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "outcome": "held_by_other",
                            "holder": holder, "pid": pid, "host": host,
                        })
                    );
                    1
                }
                crate::claims::AcquireOutcome::Error(e) => {
                    eprintln!("fno-agents: claim acquire failed: {e}");
                    2
                }
            }
        }
        "release" => {
            let Some(holder) = holder else {
                eprintln!("fno-agents: claim release requires --holder");
                return 2;
            };
            match crate::claims::release(
                &key,
                &holder,
                opts.root.as_deref(),
                opts.events_dir.as_deref(),
            ) {
                Ok(()) => {
                    println!("{}", serde_json::json!({"outcome": "released", "key": key}));
                    0
                }
                Err(e) => {
                    eprintln!("fno-agents: claim release failed: {e}");
                    2
                }
            }
        }
        "status" => {
            let (state, rec) = crate::claims::status(&key, opts.root.as_deref());
            // Mirror the `fno agents claim status -J` dict shape so the compat
            // matrix can diff the two implementations field-by-field.
            let output = rec
                .map(|record| claim_status_value(&record))
                .unwrap_or_else(|| serde_json::json!({"key": key, "state": state.as_str()}));
            println!("{output}");
            0
        }
        other => {
            eprintln!(
                "fno-agents: unknown claim operation: {other} (use acquire|release|status|list|sweep)"
            );
            2
        }
    }
}

/// `fno-agents claim list [--prefix <prefix>] [--include-stale] [--root <dir>]`
/// — return the same status-shaped rows as the Python list verb, after one
/// native read of both global and repository claim roots.
fn run_claim_list(args: &[String]) -> i32 {
    let mut prefix: Option<String> = None;
    let mut root: Option<PathBuf> = None;
    let mut include_stale = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--prefix" => match it.next() {
                Some(value) => prefix = Some(value.clone()),
                None => {
                    eprintln!("fno-agents: claim list: --prefix requires a value");
                    return 2;
                }
            },
            "--root" => match it.next() {
                Some(value) => root = Some(PathBuf::from(value)),
                None => {
                    eprintln!("fno-agents: claim list: --root requires a value");
                    return 2;
                }
            },
            "--include-stale" => include_stale = true,
            "--json" | "-J" => {}
            other => {
                eprintln!("fno-agents: claim list: unknown flag {other}");
                return 2;
            }
        }
    }
    let local_root = root.or_else(|| std::env::current_dir().ok());
    let rows = crate::claims::list(prefix.as_deref(), local_root.as_deref(), include_stale);
    let (witness, witness_answer) = default_session_witness();
    let witness: crate::claims::SessionWitness = &witness;
    let rows: Vec<Value> = rows
        .iter()
        .map(|rec| claim_status_value_with_witness(rec, Some(witness), &witness_answer))
        .collect();
    println!("{}", Value::Array(rows));
    0
}

fn claim_status_value(rec: &crate::claims::ClaimRecord) -> Value {
    let (witness, witness_answer) = default_session_witness();
    let witness: crate::claims::SessionWitness = &witness;
    claim_status_value_with_witness(rec, Some(witness), &witness_answer)
}

fn claim_status_value_with_witness(
    rec: &crate::claims::ClaimRecord,
    witness: Option<crate::claims::SessionWitness<'_>>,
    witness_answer: &std::cell::RefCell<Option<&'static str>>,
) -> Value {
    let (state, basis) = match witness {
        Some(witness) => crate::claims::classify_with_basis_and_exclusivity(
            rec,
            None,
            &|pid| crate::claims::probe_pid(pid),
            None,
            Some(witness),
        ),
        None => crate::claims::classify_with_basis(rec, None, &|pid| crate::claims::probe_pid(pid)),
    };
    let mut out = serde_json::Map::new();
    out.insert("key".into(), Value::String(rec.key.clone()));
    out.insert("state".into(), Value::String(state.as_str().into()));
    out.insert("basis".into(), Value::String(basis.into()));
    out.insert("holder".into(), Value::String(rec.holder.clone()));
    out.insert(
        "schema_version".into(),
        Value::Number(rec.schema_version.into()),
    );
    out.insert(
        "pid".into(),
        rec.pid.map(Value::from).unwrap_or(Value::Null),
    );
    out.insert("pid_unavailable".into(), Value::Bool(rec.pid_unavailable));
    out.insert("host".into(), Value::String(rec.host.clone()));
    out.insert(
        "machine_id".into(),
        rec.machine_id
            .clone()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    out.insert("acquired_at".into(), Value::Number(rec.acquired_at.into()));
    out.insert(
        "expires_at".into(),
        rec.expires_at.map(Value::from).unwrap_or(Value::Null),
    );
    if let Some(reason) = &rec.reason {
        out.insert("reason".into(), Value::String(reason.clone()));
    }
    if let Some(harness) = &rec.harness {
        out.insert("harness".into(), Value::String(harness.clone()));
    }
    if let Some(session) = &rec.session_id {
        out.insert("session_id".into(), Value::String(session.clone()));
    }
    if let Some(answered) = witness_answer.borrow_mut().take() {
        out.insert("session_basis".into(), Value::String(answered.into()));
    }
    if !rec.metadata.is_empty() {
        out.insert("metadata".into(), Value::Object(rec.metadata.clone()));
    }
    Value::Object(out)
}

/// `fno-agents claim sweep [--json] [--root <dir>]` — read matching claim
/// lockfiles, classify each with the canonical [`crate::claims`] decision, and
/// print ONE JSON object. The bare form keeps its historical `node:` /
/// `dispatch:` filter; `--prefix`, repeated `--key`, and `--all` widen it.
///
/// The mux shells this (bounded, fail-open) to overlay in-flight state onto
/// work-queue cards — the verdict shape above is a pinned contract (additive
/// fields allowed, renames are not; `state` uses `ClaimState::as_str`
/// vocabulary and consumers treat only `"live"` as in-flight).
///
/// A missing/unreadable claims dir is an EMPTY sweep (exit 0), not an error:
/// no claims means no overlay. Unparseable/newer-schema lockfiles are
/// excluded from the payload and logged to stderr (never fatal).
fn run_claim_sweep(args: &[String]) -> i32 {
    let mut root: Option<PathBuf> = None;
    let mut claims_dir: Option<PathBuf> = None;
    let mut prefix: Option<String> = None;
    let mut keys: Vec<String> = Vec::new();
    let mut all = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => match it.next() {
                Some(r) => root = Some(PathBuf::from(r)),
                None => {
                    eprintln!("fno-agents: claim sweep: --root requires a value");
                    return 2;
                }
            },
            "--claims-dir" => match it.next() {
                Some(d) => claims_dir = Some(PathBuf::from(d)),
                None => {
                    eprintln!("fno-agents: claim sweep: --claims-dir requires a value");
                    return 2;
                }
            },
            "--prefix" => match it.next() {
                Some(value) => prefix = Some(value.clone()),
                None => {
                    eprintln!("fno-agents: claim sweep: --prefix requires a value");
                    return 2;
                }
            },
            "--key" => match it.next() {
                Some(value) => keys.push(value.clone()),
                None => {
                    eprintln!("fno-agents: claim sweep: --key requires a value");
                    return 2;
                }
            },
            "--all" => all = true,
            "--json" | "-J" => {} // output is always JSON; accepted for symmetry
            other => {
                eprintln!("fno-agents: claim sweep: unknown flag {other}");
                return 2;
            }
        }
    }
    if claims_dir.is_some() && root.is_some() {
        eprintln!("fno-agents: claim sweep: --claims-dir and --root are mutually exclusive");
        return 2;
    }
    let records = if let Some(dir) = claims_dir {
        crate::claims::list_in(std::slice::from_ref(&dir), None, true)
    } else {
        let local_root = root.clone().or_else(|| std::env::current_dir().ok());
        crate::claims::list(None, local_root.as_deref(), true)
    };
    println!(
        "{}",
        claim_sweep_payload_from_records(&records, prefix.as_deref(), &keys, all)
    );
    0
}

/// Read every parseable lockfile in one claims directory. The caller applies
/// the historical prefix filter after parsing the record, so a filename cannot
/// widen or narrow the decision by lying about its encoded key.
#[cfg(test)]
fn claim_records_from_dir(dir: &Path) -> Vec<crate::claims::ClaimRecord> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                && entry.file_name().to_string_lossy().ends_with(".lock")
        })
        .filter_map(
            |entry| match crate::claims::read_claim_file(&entry.path()) {
                Ok(rec) => Some(rec),
                Err(crate::claims::ReadError::GoneAway) => None,
                Err(crate::claims::ReadError::Corrupted(error)) => {
                    eprintln!(
                        "fno-agents: claim sweep: skipping {}: {error}",
                        entry.file_name().to_string_lossy()
                    );
                    None
                }
            },
        )
        .collect()
}

/// The dispatcher-minted handover holder (mirrors `HANDOVER_HOLDER_PREFIX` in
/// `fno.claims.cli`): the suffix is the launched WORKER's name, and the
/// record's own `session_id` is the dispatcher's, not the worker's.
const HANDOVER_HOLDER_PREFIX: &str = "spawn-handover:";

/// The production session witness (x-a613). Resolution order for the record's
/// resolved subject session (the worker a handover holder names, else the
/// record's own session id): (a) the fleet registry row keyed by `harness_session_id`, whose
/// pid + start time is probed - the row's session binding is the identity
/// proof, so no create-time arithmetic is applied; (b) the transcript
/// reachability probe, the witness that answered every dated specimen (it
/// reads the newest timestamped entry, never the mtime, which overstates by
/// up to 939 minutes); (c) Unresolved. The registry loads ONCE per invocation
/// and is shared across every record the sweep classifies, the same shape as
/// the pid-exclusivity index.
///
/// The returned cell carries the LAST answer the witness gave (None while it
/// was never consulted, e.g. a pid-decided verdict or a record with no
/// session id). The caller reads and drains it after each classification to
/// fill the payload's `session_basis`.
pub(crate) fn default_session_witness() -> (
    impl Fn(&crate::claims::ClaimRecord) -> crate::claims::SessionLiveness,
    std::rc::Rc<std::cell::RefCell<Option<&'static str>>>,
) {
    let index: std::cell::RefCell<Option<SessionRegistryIndex>> = std::cell::RefCell::new(None);
    // One answer per RESOLVED subject session per invocation: a sweep consults
    // the witness for the same record twice (classify, then classify_for_sweep)
    // and several records can share one session, so the memo bounds the witness
    // traffic to one resolution per session per invocation.
    let memo: std::cell::RefCell<
        std::collections::HashMap<String, crate::claims::SessionLiveness>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    let last_answer: std::rc::Rc<std::cell::RefCell<Option<&'static str>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let cell = last_answer.clone();
    let witness = move |rec: &crate::claims::ClaimRecord| -> crate::claims::SessionLiveness {
        let answer = session_liveness_answer(rec, &index, &memo);
        *last_answer.borrow_mut() = Some(match &answer {
            crate::claims::SessionLiveness::Live(basis) => *basis,
            crate::claims::SessionLiveness::Unresolved => "unresolved",
        });
        answer
    };
    (witness, cell)
}

/// The witness's answer for one record: registry row first, then transcript.
/// Memoized per resolved subject session for the invoking process's lifetime.
fn session_liveness_answer(
    rec: &crate::claims::ClaimRecord,
    index: &std::cell::RefCell<Option<SessionRegistryIndex>>,
    memo: &std::cell::RefCell<std::collections::HashMap<String, crate::claims::SessionLiveness>>,
) -> crate::claims::SessionLiveness {
    // The subject is the holder the record NAMES, not the session that wrote
    // it. A dispatcher-minted `spawn-handover:<worker>` record carries the
    // MINTER's session_id, so answering from that field asks the dispatcher
    // whether the worker is alive - a long-lived king then keeps every claim
    // it ever launched reading live after the worker died (x-41f7). Join the
    // worker name to its registry row's session; no row means Unresolved
    // (bounded grace), never a fallback to the minter's session.
    let subject: Option<String> = match rec.holder.strip_prefix(HANDOVER_HOLDER_PREFIX) {
        Some(worker) => {
            load_session_registry_index(index);
            index
                .borrow()
                .as_ref()
                .and_then(|i| i.by_name.get(worker).cloned())
        }
        None => rec.session_id.clone().filter(|s| !s.is_empty()),
    };
    let Some(session) = subject else {
        return crate::claims::SessionLiveness::Unresolved;
    };
    if let Some(answer) = memo.borrow().get(&session) {
        return answer.clone();
    }
    let answer = session_liveness_answer_uncached(&session, index);
    memo.borrow_mut().insert(session, answer.clone());
    answer
}

/// The registry-backed resolution inputs, built once per invocation and shared
/// across every record the sweep classifies. `by_session` carries (pid, start)
/// for the registry-row liveness proof, so it requires session id, pid and
/// start time. `by_name` is the spawn-handover join (x-41f7) and requires only
/// name + session id: a thread worker has no pid (39 of 39 rows measured
/// 2026-09-07), so a pid requirement here would make every handover claim
/// unresolvable and hand its verdict back to the dispatcher.
struct SessionRegistryIndex {
    by_session: std::collections::HashMap<String, (u32, u64)>,
    by_name: std::collections::HashMap<String, String>,
}

fn load_session_registry_index(index: &std::cell::RefCell<Option<SessionRegistryIndex>>) {
    let mut cache = index.borrow_mut();
    if cache.is_some() {
        return;
    }
    let mut by_session = std::collections::HashMap::new();
    let mut by_name = std::collections::HashMap::new();
    let path = crate::paths::AgentsHome::from_env().registry_json();
    if let Ok(registry) = crate::state::load_registry(&path) {
        for e in &registry.entries {
            let Some(sid) = e.harness_session_id.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            if let (Some(pid), Some(start)) = (e.pid, e.pid_start_time) {
                by_session.insert(sid.to_string(), (pid, start));
            }
            if !e.name.is_empty() {
                by_name.insert(e.name.clone(), sid.to_string());
            }
        }
    }
    *cache = Some(SessionRegistryIndex {
        by_session,
        by_name,
    });
}

/// The uncached resolution: registry row first, then transcript.
fn session_liveness_answer_uncached(
    session: &str,
    index: &std::cell::RefCell<Option<SessionRegistryIndex>>,
) -> crate::claims::SessionLiveness {
    load_session_registry_index(index);
    if let Some(&(pid, start)) = index
        .borrow()
        .as_ref()
        .and_then(|i| i.by_session.get(session))
    {
        if crate::daemon::pid_is_ours(pid, Some(start)) {
            return crate::claims::SessionLiveness::Live(
                crate::claims::basis::REGISTRY_SESSION_LIVE,
            );
        }
    }
    // The row is missing or its pid is stale (a resume leaves rows behind) -
    // the transcript still answers. Reachability is the liveness reading;
    // "waiting" or "stalled" names a wedged session, never a dead one.
    if let Some(probe) = crate::truth_probe::family1_truth_probe(session) {
        if probe.reachability.as_deref() == Some("reachable") {
            return crate::claims::SessionLiveness::Live(crate::claims::basis::TRANSCRIPT_LIVE);
        }
    }
    crate::claims::SessionLiveness::Unresolved
}

/// Pure(ish) core of `claim sweep`: build the pinned verdict object from a
/// complete record set. `claim_sweep_payload` keeps the old single-directory
/// test seam; the command path supplies the both-root set from `claims::list`.
fn claim_sweep_payload_from_records(
    records: &[crate::claims::ClaimRecord],
    prefix: Option<&str>,
    keys: &[String],
    all: bool,
) -> Value {
    let key_set: std::collections::BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let full_scan = key_set.is_empty();
    let exclusivity = full_scan
        .then(|| crate::claims::pid_exclusivity(records))
        .unwrap_or_default();
    let now = crate::claims::now_ms();
    let (witness, witness_answer) = default_session_witness();
    let mut claims: Vec<Value> = Vec::new();
    for rec in records {
        let selected = if !key_set.is_empty() {
            key_set.contains(rec.key.as_str())
        } else if let Some(prefix) = prefix {
            rec.key.starts_with(prefix)
        } else if all {
            true
        } else {
            rec.key.starts_with("node:") || rec.key.starts_with("dispatch:")
        };
        if !selected {
            continue;
        }
        let identity = rec.machine_id.clone().unwrap_or_else(|| rec.host.clone());
        let pid_exclusive = full_scan
            .then(|| {
                rec.pid
                    .and_then(|pid| exclusivity.get(&(identity, pid)).copied())
            })
            .flatten();
        let probe = &|pid| crate::claims::probe_pid(pid);
        let (state, basis) = crate::claims::classify_with_basis_and_exclusivity(
            rec,
            Some(now),
            probe,
            pid_exclusive,
            Some(&witness),
        );
        let (provably_dead, bucket) =
            crate::claims::classify_for_sweep(rec, Some(now), probe, pid_exclusive, Some(&witness));
        let expired = rec.expires_at.is_some_and(|expires_at| now >= expires_at);
        // Which witness answered for THIS record, or None when the pid
        // evidence decided and the witness was never consulted.
        let session_basis = witness_answer.borrow_mut().take();
        let mut row = serde_json::json!({
            "key": rec.key,
            "state": state.as_str(),
            "holder": rec.holder,
            "schema_version": rec.schema_version,
            "host": rec.host,
            "pid": rec.pid,
            "basis": basis,
            "expired": expired,
            "provably_dead": provably_dead,
            "bucket": bucket,
            "machine_id": rec.machine_id,
            "pid_unavailable": rec.pid_unavailable,
            "pid_provenance": rec.pid_provenance,
            "acquired_at": rec.acquired_at,
            "expires_at": rec.expires_at,
        });
        if let Value::Object(fields) = &mut row {
            if let Some(reason) = &rec.reason {
                fields.insert("reason".into(), Value::String(reason.clone()));
            }
            if let Some(harness) = &rec.harness {
                fields.insert("harness".into(), Value::String(harness.clone()));
            }
            if let Some(session) = &rec.session_id {
                fields.insert("session_id".into(), Value::String(session.clone()));
            }
            if let Some(answered) = session_basis {
                fields.insert("session_basis".into(), Value::String(answered.into()));
            }
            if !rec.metadata.is_empty() {
                fields.insert("metadata".into(), Value::Object(rec.metadata.clone()));
            }
        }
        claims.push(row);
    }
    claims.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    serde_json::json!({ "claims": claims })
}

#[cfg(test)]
fn claim_sweep_payload(dir: &Path) -> Value {
    let records = claim_records_from_dir(dir);
    claim_sweep_payload_from_records(&records, None, &[], false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- claim sweep (x-54fa) --------------------------------------------

    fn sweep_acquire(root: &std::path::Path, key: &str) {
        let opts = crate::claims::AcquireOpts {
            root: Some(root.to_path_buf()),
            events_dir: Some(root.to_path_buf()),
            ..Default::default()
        };
        match crate::claims::acquire(key, "test-holder", opts) {
            crate::claims::AcquireOutcome::Acquired(_) => {}
            other => panic!("acquire {key} failed: {other:?}"),
        }
    }

    fn sweep_dir(root: &std::path::Path) -> PathBuf {
        crate::claims::claims_dir_for(Some(root)).unwrap()
    }

    #[test]
    fn claim_sweep_empty_or_missing_dir_is_empty_payload() {
        let td = tempfile::TempDir::new().unwrap();
        // Dir does not exist yet: empty payload, not an error (Boundaries:
        // "must handle an empty claims directory").
        let payload = claim_sweep_payload(&sweep_dir(td.path()));
        assert_eq!(payload, serde_json::json!({"claims": []}));
    }

    #[test]
    fn claim_sweep_reports_live_node_and_dispatch_claims() {
        let td = tempfile::TempDir::new().unwrap();
        sweep_acquire(td.path(), "node:x-ef41");
        sweep_acquire(td.path(), "dispatch:x-ef41");
        sweep_acquire(td.path(), "session:not-swept"); // out-of-scope prefix
        let payload = claim_sweep_payload(&sweep_dir(td.path()));
        let claims = payload["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 2, "session: claim must be excluded");
        // Sorted by key: dispatch: before node:.
        assert_eq!(claims[0]["key"], "dispatch:x-ef41");
        assert_eq!(claims[1]["key"], "node:x-ef41");
        for c in claims {
            // Acquired by THIS live process => live.
            assert_eq!(c["state"], "live");
            assert_eq!(c["holder"], "test-holder");
            assert_eq!(c["pid"], std::process::id());
            assert!(c["host"].as_str().is_some_and(|h| !h.is_empty()));
        }
    }

    #[test]
    fn claim_sweep_reports_classifier_basis_and_claim_facts() {
        let td = tempfile::TempDir::new().unwrap();
        sweep_acquire(td.path(), "node:x-facts");
        let claims = claim_sweep_payload(&sweep_dir(td.path()))["claims"]
            .as_array()
            .unwrap()
            .to_vec();
        let row = claims
            .iter()
            .find(|claim| claim["key"] == "node:x-facts")
            .expect("the acquired claim is present");
        assert_eq!(row["state"], "live");
        assert_eq!(row["basis"], "live");
        assert_eq!(row["expired"], false);
        assert_eq!(row["provably_dead"], false);
        assert_eq!(row["bucket"], "live");
        assert_eq!(row["pid_unavailable"], false);
        assert!(row["acquired_at"].as_i64().is_some());
        assert!(row.get("machine_id").is_some());
        assert!(row.get("pid_provenance").is_some());
        assert!(row.get("expires_at").is_some());
    }

    #[test]
    fn claim_sweep_filters_by_prefix_key_and_all() {
        let td = tempfile::TempDir::new().unwrap();
        sweep_acquire(td.path(), "node:x-filter");
        sweep_acquire(td.path(), "dispatch:x-filter");
        sweep_acquire(td.path(), "session:x-filter");
        let records = claim_records_from_dir(&sweep_dir(td.path()));

        let prefix = claim_sweep_payload_from_records(&records, Some("session:"), &[], false);
        assert_eq!(prefix["claims"].as_array().unwrap().len(), 1);
        assert_eq!(prefix["claims"][0]["key"], "session:x-filter");

        let keys = vec!["node:x-filter".to_string(), "session:x-filter".to_string()];
        let selected = claim_sweep_payload_from_records(&records, None, &keys, false);
        let selected_keys: Vec<_> = selected["claims"]
            .as_array()
            .unwrap()
            .iter()
            .map(|claim| claim["key"].as_str().unwrap())
            .collect();
        assert_eq!(selected_keys, vec!["node:x-filter", "session:x-filter"]);

        let all = claim_sweep_payload_from_records(&records, None, &[], true);
        assert_eq!(all["claims"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn claim_sweep_excludes_corrupted_and_newer_schema_lockfiles() {
        let td = tempfile::TempDir::new().unwrap();
        sweep_acquire(td.path(), "node:x-good");
        let dir = sweep_dir(td.path());
        // Corrupted YAML under a sweep-prefixed name.
        fs::write(dir.join("node%3Ax-bad.lock"), "{not yaml: [").unwrap();
        // Newer schema writer: parse refuses, sweep excludes (does not crash).
        fs::write(
            dir.join("node%3Ax-newer.lock"),
            "schema_version: 999\nkey: node:x-newer\nholder: h\nacquired_at: 1\npid: 1\nhost: x\n",
        )
        .unwrap();
        // Non-lock and dot files are skipped.
        fs::write(dir.join("node%3Ax-tmp.partial"), "x").unwrap();
        let payload = claim_sweep_payload(&dir);
        let claims = payload["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0]["key"], "node:x-good");
    }

    // ---- the handover witness subject (x-41f7) ---------------------------

    fn own_pid_start() -> u64 {
        // Registry units: daemon::process_start_time's native value, the same
        // pair pid_is_ours compares. The claims epoch-ms twin would never
        // compare equal here.
        crate::daemon::process_start_time(std::process::id())
            .expect("this test process has a start time")
    }

    /// Pin FNO_AGENTS_HOME to a temp registry for `f`. test_env_lock
    /// serializes the process-global env against every other env-touching
    /// test in the crate (the paths.rs from_env_honors_override idiom).
    fn with_registry(entries: serde_json::Value, f: impl FnOnce()) {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let td = tempfile::TempDir::new().unwrap();
        std::fs::write(
            td.path().join("registry.json"),
            serde_json::json!({"schema_version": 1, "entries": entries}).to_string(),
        )
        .unwrap();
        std::env::set_var("FNO_AGENTS_HOME", td.path());
        f();
        std::env::remove_var("FNO_AGENTS_HOME");
    }

    fn witness_rec(holder: &str, session: &str) -> crate::claims::ClaimRecord {
        crate::claims::ClaimRecord {
            schema_version: 1,
            key: "node:x-t".into(),
            holder: holder.into(),
            acquired_at: crate::claims::now_ms(),
            pid: None,
            host: "test-host".into(),
            pid_unavailable: false,
            expires_at: None,
            reason: None,
            harness: Some("claude".into()),
            session_id: Some(session.into()),
            pid_provenance: Some("ambient".into()),
            machine_id: None,
            metadata: serde_json::Map::new(),
        }
    }

    #[test]
    fn handover_witness_never_answers_from_the_minter_session() {
        // The minter (a long-lived king) is PROVABLY live: its session's
        // registry row names this very test process. The handover names a
        // worker with no row. The witness must answer Unresolved anyway -
        // answering from the minter kept every claim a dead worker left
        // behind reading live for the rest of the king's reign (x-41f7).
        let me = std::process::id();
        with_registry(
            serde_json::json!([{
                "name": "king-row",
                "status": "live",
                "cwd": "/w",
                "created_at": "2026-09-07T00:00:00Z",
                "harness_session_id": "s-king",
                "pid": me,
                "pid_start_time": own_pid_start(),
            }]),
            || {
                let (witness, _drain) = default_session_witness();
                let handover = witness_rec("spawn-handover:ghost", "s-king");
                assert!(matches!(
                    witness(&handover),
                    crate::claims::SessionLiveness::Unresolved
                ));
                // Control: the SAME session answers Live for a non-handover
                // record, so the Unresolved above is the subject switch, not
                // a dead fixture.
                let plain = witness_rec("plain-holder", "s-king");
                assert!(matches!(
                    witness(&plain),
                    crate::claims::SessionLiveness::Live(_)
                ));
            },
        );
    }

    #[test]
    fn handover_witness_resolves_the_named_worker_row_without_a_pid() {
        // The worker row is a THREAD worker: no pid (39 of 39 measured
        // 2026-09-07). The name join must still resolve - a pid requirement
        // on by_name would hand every thread-worker handover back to the
        // minter - and the Live answer can only come from the worker's
        // session: rec.session_id names nothing resolvable.
        let me = std::process::id();
        with_registry(
            serde_json::json!([
                {
                    "name": "w-thread",
                    "status": "live",
                    "cwd": "/w",
                    "created_at": "2026-09-07T00:00:00Z",
                "cwd": "/w",
                "created_at": "2026-09-07T00:00:00Z",
                    "harness_session_id": "s-worker",
                },
                {
                    "name": "w-proof",
                    "status": "live",
                    "cwd": "/w",
                    "created_at": "2026-09-07T00:00:00Z",
                "cwd": "/w",
                "created_at": "2026-09-07T00:00:00Z",
                    "harness_session_id": "s-worker",
                    "pid": me,
                    "pid_start_time": own_pid_start(),
                },
            ]),
            || {
                let (witness, _drain) = default_session_witness();
                let rec = witness_rec("spawn-handover:w-thread", "s-king-elsewhere");
                assert!(matches!(
                    witness(&rec),
                    crate::claims::SessionLiveness::Live(_)
                ));
            },
        );
    }

    #[test]
    fn expired_handover_claim_reads_stale_while_minter_is_live() {
        // AC1 end to end: the dispatching session stays live the whole time,
        // the worker is gone, the claim is past TTL and past the unresolved
        // grace - so the bucket is non-live. The control record, same shape
        // but self-held, keeps reading live off the same session.
        let me = std::process::id();
        with_registry(
            serde_json::json!([{
                "name": "king-row",
                "status": "live",
                "cwd": "/w",
                "created_at": "2026-09-07T00:00:00Z",
                "harness_session_id": "s-king",
                "pid": me,
                "pid_start_time": own_pid_start(),
            }]),
            || {
                let (witness, _drain) = default_session_witness();
                let now = crate::claims::now_ms();
                let past = now - (crate::claims::UNRESOLVED_GRACE_MS + 60_000);
                let verdict = |holder: &str| {
                    let mut rec = witness_rec(holder, "s-king");
                    rec.acquired_at = past;
                    rec.expires_at = Some(past);
                    // A dead pid: nothing but the witness could hold it live.
                    rec.pid = Some(999_999);
                    crate::claims::classify_with_basis_and_exclusivity(
                        &rec,
                        Some(now),
                        &crate::claims::probe_pid,
                        None,
                        Some(&witness),
                    )
                };
                assert_eq!(
                    verdict("spawn-handover:ghost").0,
                    crate::claims::ClaimState::Stale
                );
                assert_eq!(verdict("plain-holder").0, crate::claims::ClaimState::Live);
            },
        );
    }
}
