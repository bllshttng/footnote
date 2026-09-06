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
            "--pid-provenance" => match take("--pid-provenance") {
                Some(p) => opts.pid_provenance = Some(p),
                None => return 2,
            },
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
    let rows: Vec<Value> = rows.iter().map(claim_status_value).collect();
    println!("{}", Value::Array(rows));
    0
}

fn claim_status_value(rec: &crate::claims::ClaimRecord) -> Value {
    let (state, basis) =
        crate::claims::classify_with_basis(rec, None, &|pid| crate::claims::probe_pid(pid));
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
        );
        let (provably_dead, bucket) =
            crate::claims::classify_for_sweep(rec, Some(now), probe, pid_exclusive);
        let expired = rec.expires_at.is_some_and(|expires_at| now >= expires_at);
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
}
