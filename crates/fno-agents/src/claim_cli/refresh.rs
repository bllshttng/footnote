//! The `claim refresh` leaf, ported from `cli/src/fno/claims/cli.py::refresh`
//! plus the two shapes `core.refresh_claim` kept behind its routing: a
//! global-id key (session:, node:, ...) ran the legacy in-process body
//! (`core._legacy_refresh_claim`: holder mismatch is a named exit 4, a
//! missing file is `claim missing`, the ttl is range-checked) and a
//! repo-local key ran the native leg (a status pre-read, then the engine
//! `renew`, where every benign refusal - wrong holder, gone, corrupted -
//! collapses into the PID-liveness no-op). Both shapes drive the same engine
//! write here; the routing only picks the error taxonomy and the pre-reads.
//!
//! Output contract: `refreshed: {key} (new expires_at=N)` or the claim JSON
//! under `--json`; the PID-liveness no-op line / `{"key", "refreshed",
//! "reason": "pid_liveness"}` receipt. Exit codes: 0 refreshed or no-op; 1
//! write failure; 2 validation; 3 missing/corrupted; 4 holder mismatch.

use serde_json::json;

use super::{claim_json_string, node_aware_root, parse_ttl_expression, ttl_ms_checked};
use crate::claims::{self, ClaimState, MIN_TTL_MS};

struct LeafArgs {
    key: Option<String>,
    holder: Option<String>,
    ttl_ms: Option<i128>,
    json_output: bool,
}

pub fn run(args: &[String]) -> i32 {
    let mut a = LeafArgs {
        key: None,
        holder: None,
        ttl_ms: None,
        json_output: false,
    };
    let mut ttl_raw = String::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--holder" => match it.next() {
                Some(v) => a.holder = Some(v.clone()),
                None => return usage_error("Error: Option '--holder' requires an argument."),
            },
            "--ttl" => match it.next() {
                Some(v) => ttl_raw = v.clone(),
                None => return usage_error("Error: Option '--ttl' requires an argument."),
            },
            "--json" | "-J" => a.json_output = true,
            other => {
                if other.starts_with('-') && other != "-" {
                    eprintln!("fno-agents: claim: unknown flag {other}");
                    return 2;
                }
                if a.key.is_some() {
                    eprintln!("fno-agents: claim: unexpected extra argument {other}");
                    return 2;
                }
                a.key = Some(other.to_string());
            }
        }
    }
    // typer refused an absent required option with the usage layout (the
    // frozen goldens pin the lines); an empty --holder VALUE reached the
    // engine validation instead.
    let Some(key) = a.key.clone() else {
        return usage_error("Error: Missing argument 'KEY'.");
    };
    let Some(holder) = a.holder.clone() else {
        return usage_error("Error: Missing option '--holder'.");
    };
    if key.is_empty() || holder.is_empty() {
        eprintln!("validation error: key and holder must be non-empty");
        return 2;
    }
    a.ttl_ms = match parse_ttl_expression(&ttl_raw) {
        Ok(v) => v,
        Err(e) => return bad_parameter(&e),
    };
    if node_aware_root(&key).is_some() {
        run_global(&a, &key, &holder)
    } else {
        run_local(&a, &key, &holder)
    }
}

fn usage_error(detail: &str) -> i32 {
    eprintln!("Usage: fno agents claim refresh [OPTIONS] KEY");
    eprintln!("Try 'fno agents claim refresh --help' for help.");
    eprintln!();
    eprintln!("{detail}");
    2
}

fn bad_parameter(msg: &str) -> i32 {
    usage_error(&format!("Error: Invalid value: {msg}"))
}

/// The legacy-shaped leg (`core._legacy_refresh_claim`): the named holder
/// mismatch, the range-checked ttl, and a direct file pre-read before the
/// engine extend.
fn run_global(a: &LeafArgs, key: &str, holder: &str) -> i32 {
    let root = node_aware_root(key);
    let ttl = match a.ttl_ms {
        Some(t) => {
            // The legacy body range-checked BEFORE touching the file, with
            // its own message (the engine only refuses non-positive).
            if t < i128::from(MIN_TTL_MS) || t > i128::from(claims::MAX_TTL_MS) {
                eprintln!(
                    "validation error: ttl_ms={t} out of range [{}, {}]",
                    MIN_TTL_MS,
                    claims::MAX_TTL_MS
                );
                return 2;
            }
            t
        }
        None => i128::from(MIN_TTL_MS),
    };
    let ttl = match ttl_ms_checked(ttl) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("validation error: {e}");
            return 2;
        }
    };
    let path = match claims::claim_path(key, root.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("validation error: {e}");
            return 2;
        }
    };
    if !path.exists() {
        eprintln!("claim missing: {}", path.display());
        return 3;
    }
    let record = match claims::read_claim_file(&path) {
        Ok(r) => r,
        Err(claims::ReadError::GoneAway) => {
            eprintln!("claim missing: {}", path.display());
            return 3;
        }
        Err(claims::ReadError::Corrupted(e)) => {
            eprintln!("corrupted claim: {e}");
            return 3;
        }
    };
    if record.holder != holder {
        eprintln!(
            "holder mismatch: claim '{key}': holder mismatch (expected '{holder}', got '{}')",
            record.holder
        );
        return 4;
    }
    if record.expires_at.is_none() {
        return no_op(key, a.json_output);
    }
    // STALE is the only refused verdict, matching the engine's own renew
    // gate; the named refusal is the legacy body's message.
    if crate::claim_verbs::status_verdict(&record).0 == ClaimState::Stale {
        eprintln!(
            "validation error: claim '{key}' expired and its holder reads dead; \
             refusing to resurrect it"
        );
        return 2;
    }
    finish(a, key, holder, root.as_deref(), ttl, 1)
}

/// The native-shaped leg (`core.refresh_claim`'s native path): with an
/// explicit ttl it goes straight to the engine renew (a stale lease then
/// reads as the benign no-op); with an EMPTY --ttl it status-pre-reads
/// first (free names the gone-away, stale refuses, a PID-liveness claim
/// no-ops) and renews by the MIN window. Benign renew refusals all read as
/// the PID-liveness no-op exactly as the Python core returned None.
fn run_local(a: &LeafArgs, key: &str, holder: &str) -> i32 {
    if let Some(t) = a.ttl_ms {
        if t <= 0 {
            eprintln!("validation error: ttl_ms must be positive");
            return 2;
        }
        let ttl = match ttl_ms_checked(t) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("validation error: {e}");
                return 2;
            }
        };
        return finish(a, key, holder, None, ttl, 3);
    }
    let (state, record) = claims::status(key, None);
    match state {
        ClaimState::Free => {
            let path = claims::claim_path(key, None);
            match path {
                Ok(p) => eprintln!("claim missing: {}", p.display()),
                Err(e) => eprintln!("claim missing: {e}"),
            }
            return 3;
        }
        ClaimState::Corrupted => {
            // The status payload carries no parse detail for a corrupted
            // record, so the core's `error or key` fallback names the key.
            eprintln!("corrupted claim: {key}");
            return 3;
        }
        ClaimState::Stale => {
            eprintln!("validation error: claim '{key}' expired and cannot be refreshed");
            return 2;
        }
        _ => {}
    }
    if record.and_then(|r| r.expires_at).is_none() {
        return no_op(key, a.json_output);
    }
    finish(a, key, holder, None, MIN_TTL_MS, 3)
}

/// Extend through the engine and print the receipt. `err_exit` is the exit
/// code the leg's taxonomy gives an engine write failure (legacy surfaced a
/// raw failure as exit 1; the native leg read a renew refusal as a verdict
/// error, exit 3). A benign `Ok(false)` after the pre-reads passed means the
/// record moved under us or a peer holds the recovery mutex - the no-op
/// surface is the leaf's only None shape, so it takes that.
fn finish(
    a: &LeafArgs,
    key: &str,
    holder: &str,
    root: Option<&std::path::Path>,
    ttl_ms: i64,
    err_exit: i32,
) -> i32 {
    match claims::renew(key, holder, ttl_ms, root) {
        Ok(true) => {
            let (_, record) = claims::status(key, root);
            match record {
                Some(r) => {
                    if a.json_output {
                        println!("{}", claim_json_string(&r));
                    } else {
                        println!(
                            "refreshed: {key} (new expires_at={})",
                            r.expires_at.unwrap_or_default()
                        );
                    }
                    0
                }
                None => no_op(key, a.json_output),
            }
        }
        Ok(false) => no_op(key, a.json_output),
        Err(e) => {
            if err_exit == 3 {
                eprintln!("native verdict unavailable: fno-agents claim renew failed: {e}");
            } else {
                eprintln!("refresh failed: {e}");
            }
            err_exit
        }
    }
}

fn no_op(key: &str, json_output: bool) -> i32 {
    if json_output {
        println!(
            "{}",
            json!({"key": key, "refreshed": false, "reason": "pid_liveness"})
        );
    } else {
        println!("no-op for PID-liveness claim: {key}");
    }
    0
}
