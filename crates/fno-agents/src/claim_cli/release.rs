//! The `claim release` leaf, ported from `cli/src/fno/claims/cli.py::release`
//! plus the lane and force helpers it folded (`_release_lane`,
//! `_force_release`) and the do-row close/rollback it drives
//! (`core._stamp_do_on_release`, `core._rollback_do_on_release`).
//!
//! One command, three modes that were three verbs: the default releases a
//! claim we hold, `--lane <id>` releases a lane slot, `--force` drops a claim
//! regardless of owner. Output contract: human lines by default, `--json`
//! for the operator receipt `{"key", "released"}`. The engine leg
//! (`core.release_claim`) passes `--with-claim` and gets the receipt with
//! the removed record embedded, the shape `_native_claim_model` reads.
//! Exit codes: 0 released or no-op; 1 force-release with nothing archived;
//! 2 validation; 3 contended/transient; 4 holder mismatch under --strict.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::{node_aware_root, rollback_do_on_release, stamp_do_on_release};
use crate::claims::ClaimRecord;

struct LeafArgs {
    key: Option<String>,
    holder: String,
    lane: Option<String>,
    force: bool,
    reason: String,
    strict: bool,
    stamp_do: bool,
    rollback_do: bool,
    json_output: bool,
    with_claim: bool,
    root: Option<PathBuf>,
}

pub fn run(args: &[String]) -> i32 {
    let mut a = LeafArgs {
        key: None,
        holder: String::new(),
        lane: None,
        force: false,
        reason: String::new(),
        strict: false,
        stamp_do: false,
        rollback_do: false,
        json_output: false,
        with_claim: false,
        root: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--holder" => match it.next() {
                Some(v) => a.holder = v.clone(),
                None => return usage_flag("--holder"),
            },
            "--lane" => match it.next() {
                Some(v) => a.lane = Some(v.clone()),
                None => return usage_flag("--lane"),
            },
            "--reason" | "-R" => match it.next() {
                Some(v) => a.reason = v.clone(),
                None => return usage_flag("--reason"),
            },
            "--force" | "-F" => a.force = true,
            "--strict" => a.strict = true,
            "--stamp-do" => a.stamp_do = true,
            "--rollback-do" => a.rollback_do = true,
            "--json" | "-J" => a.json_output = true,
            // Hidden engine flag: `core.release_claim` asks for the removed
            // record so its parse keeps working (the operator --json receipt
            // stays the frozen two-field surface).
            "--with-claim" => a.with_claim = true,
            // Hidden engine flags the native callers forward.
            "--root" => match it.next() {
                Some(v) => a.root = Some(PathBuf::from(v)),
                None => return usage_flag("--root"),
            },
            "--holding-recovery-lock" => {}
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
    run_parsed(a)
}

fn usage_flag(name: &str) -> i32 {
    eprintln!("fno-agents: claim: {name} requires a value");
    2
}

fn run_parsed(a: LeafArgs) -> i32 {
    // A flag that belongs to another mode is REFUSED, never ignored
    // (cli.py:313-327): silently dropping --stamp-do on the force path loses
    // exactly the provenance the flag was passed to record.
    if let Some(lane) = a.lane.clone() {
        if a.key.is_some()
            || a.force
            || !a.holder.is_empty()
            || a.strict
            || a.stamp_do
            || a.rollback_do
            || !a.reason.is_empty()
        {
            eprintln!(
                "validation error: --lane takes only the lane id (no KEY, and \
                 none of --force/--holder/--strict/--reason/--stamp-do/\
                 --rollback-do): a lane slot has no owner and no do window"
            );
            return 2;
        }
        return run_lane(&a, &lane);
    }
    let Some(key) = a.key.clone() else {
        eprintln!("validation error: KEY is required (or use --lane <id>)");
        return 2;
    };
    if !a.reason.is_empty() && !a.force {
        eprintln!(
            "validation error: --reason records the --force override and has no \
             effect on an ordinary release"
        );
        return 2;
    }
    if a.force {
        if a.strict || a.stamp_do || a.rollback_do {
            eprintln!(
                "validation error: --force is the administrative drop and takes \
                 none of --strict/--stamp-do/--rollback-do (there is no owner to \
                 check and no do window to stamp)"
            );
            return 2;
        }
        if !a.holder.is_empty() {
            eprintln!(
                "validation error: --force drops the claim regardless of owner, \
                 so --holder is meaningless with it"
            );
            return 2;
        }
        if a.reason.is_empty() {
            eprintln!(
                "validation error: --force requires --reason (the override is \
                 recorded in the audit trail)"
            );
            return 2;
        }
        return run_force(&a, &key);
    }
    if a.holder.is_empty() {
        eprintln!("validation error: --holder is required (or use --force)");
        return 2;
    }
    if a.stamp_do && a.rollback_do {
        eprintln!(
            "validation error: --stamp-do and --rollback-do are mutually \
             exclusive (one records a finished do window, the other removes a \
             row for work that never ran)"
        );
        return 2;
    }
    run_keyed(&a, &key)
}

fn run_lane(a: &LeafArgs, lane: &str) -> i32 {
    let mut args: Vec<String> = vec!["--lane".into(), lane.to_string()];
    if a.json_output {
        args.push("--json".into());
    }
    crate::claim_lanes_cli::run_lane_release(&args)
}

fn run_keyed(a: &LeafArgs, key: &str) -> i32 {
    let root = a.root.clone().or_else(|| node_aware_root(key));
    // The strict prior check `core.release_claim` runs before the native
    // release (core.py:2133-2138): a foreign holder is a named exit 4, every
    // other state falls through to the release itself. A corrupt file under
    // --strict is a named transient exit 3 - strict mode exists so a caller
    // can TELL a corruption from an already-released claim.
    if a.strict {
        let (state, prior) = crate::claims::status(key, root.as_deref());
        if state == crate::claims::ClaimState::Corrupted {
            eprintln!(
                "transient error: {key}: claim corrupted; cannot verify \
                 ownership (use `fno agents claim release --force`)"
            );
            return 3;
        }
        if state != crate::claims::ClaimState::Free {
            if let Some(prior) = prior {
                if !prior.holder.is_empty() && prior.holder != a.holder {
                    eprintln!(
                        "holder mismatch: claim '{key}': holder mismatch \
                         (expected '{}', got '{}')",
                        a.holder, prior.holder
                    );
                    return 4;
                }
            }
        }
    }
    match crate::claims::release_with_receipt(key, &a.holder, root.as_deref(), None) {
        Ok(Some(claim)) => {
            if key.starts_with("node:") {
                if a.stamp_do {
                    stamp_do_on_release(key, &claim, &a.holder);
                } else if a.rollback_do {
                    rollback_do_on_release(key, &claim, &a.holder);
                }
            }
            if a.json_output {
                println!("{}", receipt(key, Some(&claim), a.with_claim));
            } else {
                println!("released: {key}");
            }
            0
        }
        Ok(None) => {
            // Nothing was unlinked - one of four causes the receipt cannot
            // tell apart - so the do row this call would have touched stays
            // as it was, named rather than silent (cli.py:404-423).
            if key.starts_with("node:") && (a.stamp_do || a.rollback_do) {
                let (what, why) = if a.stamp_do {
                    ("do stamp skipped", "no do row was closed")
                } else {
                    ("do rollback skipped", "no do row was dropped")
                };
                eprintln!("{what} for {key}: release was a no-op (nothing was unlinked, so {why})");
            }
            if a.json_output {
                println!("{}", receipt(key, None, a.with_claim));
            } else {
                println!("no-op: {key} was not released (nothing was unlinked)");
            }
            0
        }
        Err(e) => {
            if e.contains("recovery mutex") {
                // A losing racer in a two-process release: the recovery-dir
                // mutex timed out. A clean named exit, never a traceback -
                // and never the exit 0 that would read as a release that did
                // not happen.
                eprintln!("claim contended: {e}");
                3
            } else {
                eprintln!("validation error: {e}");
                2
            }
        }
    }
}

/// The release receipt. Operator surface (`--json`): the frozen two-field
/// `{"key", "released"}`. Engine surface (`--with-claim`): the removed
/// record embedded, the shape `_native_claim_model` reconstructs a Claim
/// from.
fn receipt(key: &str, claim: Option<&ClaimRecord>, with_claim: bool) -> String {
    if with_claim {
        return match claim {
            Some(c) => {
                json!({"outcome": "released", "released": true, "key": key, "claim": c}).to_string()
            }
            None => json!({"outcome": "not_released", "released": false, "key": key}).to_string(),
        };
    }
    json!({"key": key, "released": claim.is_some()}).to_string()
}

fn run_force(a: &LeafArgs, key: &str) -> i32 {
    let root = a.root.clone().or_else(|| node_aware_root(key));
    // core.force_release_claim only surfaces the empty-key/empty-reason
    // refusals; the leaf pre-validated both, so any engine error here is
    // still a validation-class refusal.
    let payload = match crate::claim_store::force_release(key, &a.reason, root.as_deref(), false) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("validation error: {e}");
            return 2;
        }
    };
    let path = payload["path"].as_str().unwrap_or_default().to_string();
    if payload["archived"].as_bool() == Some(true) {
        if a.json_output {
            println!(
                "{}",
                json!({
                    "key": key, "reason": a.reason, "path": path,
                    "archived": true, "force_released": true,
                })
            );
        } else {
            println!("force-released: {key} (archived {path})");
        }
        return 0;
    }
    // Nothing at the resolved path REFUSES (exit 1), naming the path read
    // and any other default root that still holds the file - the specimen
    // released nothing while printing success (cli.py:1811-1859).
    let others = other_roots(key, Path::new(&path));
    if a.json_output {
        let rows: Vec<Value> = others
            .iter()
            .map(|(raw, p)| {
                json!({
                    "path": p.display().to_string(),
                    "root": raw.as_ref().map(|r| r.display().to_string())
                        .unwrap_or_else(|| "default".into()),
                })
            })
            .collect();
        println!(
            "{}",
            json!({
                "key": key, "reason": a.reason, "path": path,
                "archived": false, "force_released": false,
                "other_roots": rows,
            })
        );
    } else {
        println!("nothing released: no claim file at {path}");
        for (raw, p) in &others {
            let reach = raw
                .as_ref()
                .map(|r| format!("--root {}", r.display()))
                .unwrap_or_else(|| "the default root (omit --root)".into());
            println!(
                "a claim file for this key exists at {} ({reach})",
                p.display()
            );
        }
    }
    1
}

/// The other default-root claim files for this key (`_force_release`'s
/// `others`): the global root and the default root, deduped, minus the path
/// just read, kept only when the file exists.
fn other_roots(key: &str, taken: &Path) -> Vec<(Option<PathBuf>, PathBuf)> {
    let encoded = crate::claims::encode_key(key);
    let mut out = Vec::new();
    let mut seen: Vec<PathBuf> = vec![taken.to_path_buf()];
    let mut roots: Vec<(Option<PathBuf>, PathBuf)> = Vec::new();
    if let Some(global) = crate::claims_root::global_claims_root() {
        let dir = global.join(crate::claims::CLAIMS_DIRNAME);
        roots.push((Some(global), dir));
    }
    if let Some(dir) = default_claims_dir() {
        roots.push((None, dir));
    }
    for (raw, dir) in roots {
        let file = dir.join(format!("{encoded}.lock"));
        if seen.iter().any(|p| p == &file) {
            continue;
        }
        // A table root holds the row behind the locator; a legacy root still
        // holds the file itself. Never open a store that does not exist yet.
        let in_table = crate::claim_store::database_path_from_directory(&dir)
            .is_ok_and(|db| db.exists())
            && matches!(crate::claim_store::read_at_path(&file), Ok(Some(_)));
        if in_table || file.is_file() {
            seen.push(file.clone());
            out.push((raw, file));
        }
    }
    out
}

/// The python `claims_dir(None)` for the others scan: the override, else the
/// repo space. KEY-AGNOSTIC on purpose - `claim_path`'s prefix routing would
/// fold a global key's "other" root into the one just read, and the scan is
/// exactly about naming the root the resolved path did not read.
fn default_claims_dir() -> Option<PathBuf> {
    let override_root = std::env::var_os("FNO_CLAIMS_ROOT").filter(|v| !v.is_empty());
    if let Some(root) = override_root {
        return Some(PathBuf::from(root).join(crate::claims::CLAIMS_DIRNAME));
    }
    let cwd = std::env::current_dir().ok()?;
    Some(crate::paths::space_dir(&cwd).join("claims"))
}
