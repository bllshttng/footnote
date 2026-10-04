//! The plan-promise gate: did the plan's declared work all ship? The
//! resolve_promise_evidence twin, refusal texts intact, so the native close
//! cannot bypass the gates the Python close ran. Fails open (outcome Ok) on
//! an absent, unreadable or unparseable plan so a stale plan_path never
//! wedges a close; the warning names the path.

use serde_json::Value;
use std::path::{Path, PathBuf};

use super::merge_evidence::{node_pr_refs, query_pr_state, repo_slug_from_url, PrReadError};
use crate::acceptance_evidence::decide_probe_run;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromiseOutcome {
    Ok,
    Unmet,
    Unknown,
}

pub(crate) struct PromiseVerdict {
    pub outcome: PromiseOutcome,
    /// The multi-line refusal text on Unmet/Unknown.
    pub reason: Option<String>,
    /// Why a fail-open happened (unreadable plan, unparseable frontmatter),
    /// naming the path. Emitted on stderr by the caller.
    pub warning: Option<String>,
}

impl PromiseVerdict {
    /// True only on positive evidence; every close boundary reads THIS.
    pub fn satisfied(&self) -> bool {
        self.outcome == PromiseOutcome::Ok
    }

    /// 6 promise unmet / 4 unknown: the outage slot, so one number means
    /// "the reader was down, retry" across both gates.
    pub fn exit_code(&self) -> i32 {
        match self.outcome {
            PromiseOutcome::Ok => 0,
            PromiseOutcome::Unmet => 6,
            PromiseOutcome::Unknown => 4,
        }
    }
}

fn ok_verdict(warning: Option<String>) -> PromiseVerdict {
    PromiseVerdict {
        outcome: PromiseOutcome::Ok,
        reason: None,
        warning,
    }
}

/// The canonical root whose `.fno/carveouts.jsonl` owns the node's work:
/// the canonical (main) working tree from `git worktree list`, else the cwd
/// itself. Never raises. The _carveout_ledger_root twin.
fn carveout_ledger_root(cwd: Option<&str>) -> PathBuf {
    let Some(cwd) = cwd else {
        return PathBuf::from(".");
    };
    let out = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output();
    if let Ok(out) = out {
        let text = String::from_utf8_lossy(&out.stdout);
        for block in text.split("\n\n") {
            let mut path: Option<String> = None;
            let mut bare = false;
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path = Some(p.to_string());
                }
                if line.trim() == "bare" {
                    bare = true;
                }
            }
            if let Some(p) = path {
                if !bare && Path::new(&p).join(".git").exists() {
                    return PathBuf::from(p);
                }
            }
        }
    }
    PathBuf::from(cwd)
}

/// Unharvested `deferred` carve-outs on the node's project ledger. A missing
/// or unreadable ledger reads as none: the close never wedges on the gate's
/// own read.
pub(crate) fn unharvested_deferred_carveouts(cwd: Option<&str>) -> Vec<Value> {
    let path = carveout_ledger_root(cwd)
        .join(".fno")
        .join("carveouts.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(|r| r.is_object())
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("deferred"))
        .collect()
}

/// Name each unharvested deferred carve-out by id + need (or description),
/// capped at 5 so a flooded ledger stays readable. The _promise_refusal_d twin.
fn promise_refusal_d(node_id: &str, deferred: &[Value]) -> String {
    let cap = 5usize;
    let mut rows: Vec<String> = Vec::new();
    for rec in deferred.iter().take(cap) {
        let need = rec.get("need").and_then(Value::as_str).unwrap_or("");
        let label = if !need.is_empty() {
            need.to_string()
        } else {
            rec.get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(80)
                .collect()
        };
        let id = rec.get("id").and_then(Value::as_str).unwrap_or("?");
        rows.push(format!("    {id}: {label}"));
    }
    let more = if deferred.len() > cap {
        format!("\n    ...and {} more", deferred.len() - cap)
    } else {
        String::new()
    };
    format!(
        "Refused: {node_id} would close with {len} unharvested deferred \
         carve-out(s) filed by this node's session.\n\
         \x20 A `deferred` carve-out is declared scope that did not ship.\n\
         \x20 carve-outs:\n{rows}{more}\n\n\
         \x20 Two legal exits:\n\
         \x20   harvest them into nodes (`fno backlog retro sweep-carveouts` previews;\n\
         \x20     `--apply` files and consumes each), then close; or\n\
         \x20   close with --force --reason \"deferred carve-out <id> filed as <node>\"",
        len = deferred.len(),
        rows = rows.join("\n"),
    )
}

/// The resolve_promise_evidence twin: decide whether a node's plan promised
/// work that has not all shipped.
pub(crate) fn resolve_promise_evidence(
    node: &Value,
    cwd: Option<&str>,
    extra_refs: &[(i64, Option<String>)],
) -> PromiseVerdict {
    let node_id = node.get("id").and_then(Value::as_str).unwrap_or("");
    // Condition D (checked first; independent of the plan): an unharvested
    // `deferred` carve-out is declared scope that did not ship, and it
    // blocks the close until it becomes a node or is force-overridden. Only
    // the closing node's OWN rows (the `node` field) block.
    let deferred: Vec<Value> = unharvested_deferred_carveouts(cwd)
        .into_iter()
        .filter(|r| r.get("node").and_then(Value::as_str) == Some(node_id))
        .collect();
    if !deferred.is_empty() {
        return PromiseVerdict {
            outcome: PromiseOutcome::Unmet,
            reason: Some(promise_refusal_d(node_id, &deferred)),
            warning: None,
        };
    }

    let plan_path = node.get("plan_path").and_then(Value::as_str).unwrap_or("");
    if plan_path.is_empty() {
        return ok_verdict(None);
    }
    // The readers strip a `#wave-1` fragment; reading the literal name
    // would fail and silently drop the gate.
    let plan_clean = plan_path.split('#').next().unwrap_or(plan_path);
    let mut plan_file = PathBuf::from(plan_clean);
    if !plan_file.is_absolute() {
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            plan_file = Path::new(cwd).join(plan_file);
        }
    }
    let text = match std::fs::read_to_string(&plan_file) {
        Ok(t) => t,
        Err(e) => {
            return ok_verdict(Some(format!(
                "promise gate could not read plan {plan_clean} ({e}); gate skipped for this close"
            )))
        }
    };

    let frontmatter = match crate::plan_doc::codec::parse_frontmatter(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            return ok_verdict(Some(format!(
                "promise gate skipped {plan_clean}; plan frontmatter would not parse ({e})"
            )))
        }
    };

    // The codec's frontmatter scalars are its own Value: a List, a Raw
    // block, or a Scalar string. `expected_url_count` parses from the
    // scalar; `close_probes` is truthy when present and non-empty.
    let close_probes = frontmatter
        .fields
        .get("close_probes")
        .map(|v| match v {
            crate::plan_doc::codec::Value::List(items) => !items.is_empty(),
            crate::plan_doc::codec::Value::BlockList(items) => !items.is_empty(),
            crate::plan_doc::codec::Value::Raw(raw) => !raw.trim().is_empty(),
            crate::plan_doc::codec::Value::Scalar(s) => !s.trim().is_empty(),
        })
        .unwrap_or(false);
    let expected = frontmatter
        .fields
        .get("expected_url_count")
        .and_then(|v| match v {
            crate::plan_doc::codec::Value::Scalar(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        });

    let plan_display = plan_path;

    // Condition E: an open prove-it FAIL on this node's own plan artifacts
    // is claimed work whose outcome did not hold; it needs no declaration.
    // A failed reader is a warning, never a block.
    if !node_id.is_empty() {
        match crate::prove_it_verdicts::prove_it_verdict_rows() {
            Err(e) => {
                return ok_verdict(Some(format!(
                    "prove-it verdict read failed ({e}); gate skipped for this close"
                )))
            }
            Ok(rows) => {
                let fail_row = rows.iter().find(|r| {
                    r.get("node").and_then(Value::as_str) == Some(node_id)
                        && r.get("open")
                            .map(|v| !v.is_null() && v.as_bool() != Some(false))
                            .unwrap_or(false)
                });
                if let Some(row) = fail_row {
                    let report = row.get("report").and_then(Value::as_str).unwrap_or("");
                    let claim = row.get("claim").and_then(Value::as_str).unwrap_or("");
                    return PromiseVerdict {
                        outcome: PromiseOutcome::Unmet,
                        reason: Some(format!(
                            "{node_id}: prove-it FAIL on its own plan artifacts \
                             ({report}): {claim}. Fix and re-run prove-it, rule with \
                             fno inbox decide, or close with --force --reason."
                        )),
                        warning: None,
                    };
                }
            }
        }
    }

    // Condition B: run the declared outcome probes. Opt-in by construction
    // (only fires when the plan declared the field).
    if close_probes {
        let mut args: Vec<String> = vec![
            "--plan".into(),
            plan_clean.to_string(),
            "--key".into(),
            "close_probes".into(),
        ];
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            args.push("--cwd".into());
            args.push(cwd.to_string());
        }
        let (rc, payload) = decide_probe_run(&args);
        if rc != 0 {
            let detail = serde_json::from_str::<Value>(payload.trim())
                .ok()
                .and_then(|v| v.get("reason").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| {
                    format!(
                        "close_probes declared but the probe runner did not \
                         evaluate them (rc={rc}): {}",
                        payload.trim().chars().take(200).collect::<String>()
                    )
                });
            return PromiseVerdict {
                outcome: PromiseOutcome::Unmet,
                reason: Some(promise_refusal_b(node_id, plan_display, &detail)),
                warning: None,
            };
        }
    }

    // Condition C: the promised ship count vs. merged refs. Only the rare
    // multi-ship plan pays for gh I/O here; a 1-PR plan never reaches it.
    if let Some(expected) = expected.filter(|e| *e >= 2) {
        let mut refs = node_pr_refs(node);
        let mut seen: Vec<i64> = refs.iter().map(|(n, _)| *n).collect();
        for (num, url) in extra_refs {
            if !seen.contains(num) {
                refs.push((*num, url.clone()));
                seen.push(*num);
            }
        }
        let (merged, failure) = count_merged_refs(&refs, expected, cwd);
        if merged < expected {
            let retryable = failure.as_ref().map(|f| f.retryable()).unwrap_or(false);
            if retryable {
                let retry = if !extra_refs.is_empty() {
                    "Re-run the same close command once GitHub answers; it \
                     records the explicit --pr ref, which this refusal exited \
                     before writing."
                        .to_string()
                } else {
                    format!(
                        "Retry with `fno backlog reconcile --node {node_id}` \
                         once GitHub answers."
                    )
                };
                let failure_text = failure
                    .as_ref()
                    .map(|f| f.message.clone())
                    .unwrap_or_default();
                return PromiseVerdict {
                    outcome: PromiseOutcome::Unknown,
                    reason: Some(format!(
                        "Unknown: {node_id} could not confirm {expected} ships \
                         ({merged} confirmed MERGED): {failure_text}\n\
                         \x20 The read failed retryably; the node stays open. {retry}"
                    )),
                    warning: None,
                };
            }
            if let Some(f) = &failure {
                return PromiseVerdict {
                    outcome: PromiseOutcome::Unmet,
                    reason: Some(format!(
                        "Refused: {node_id} could not verify promised ships: {}\n  {}",
                        f.message,
                        f.remedy_for(0, None)
                    )),
                    warning: None,
                };
            }
            return PromiseVerdict {
                outcome: PromiseOutcome::Unmet,
                reason: Some(promise_refusal_c(node_id, plan_display, expected, merged)),
                warning: None,
            };
        }
    }

    ok_verdict(None)
}

fn promise_refusal_b(node_id: &str, plan_display: &str, detail: &str) -> String {
    format!(
        "Refused: {node_id} declared close_probes and at least one failed.\n\
         \x20 plan: {plan_display}\n\
         \x20 {detail}\n\
         \n\
         \x20 Two legal exits:\n\
         \x20   make every close_probe pass, then close; or\n\
         \x20   close with --force --reason \"<why the unmet probe is acceptable>\""
    )
}

fn promise_refusal_c(node_id: &str, plan_display: &str, expected: i64, merged: i64) -> String {
    format!(
        "Refused: {node_id} promised {expected} ships; only {merged} merged.\n\
         \x20 plan: {plan_display}\n\
         \x20 expected_url_count: {expected}    merged refs: {merged}\n\
         \n\
         \x20 Two legal exits:\n\
         \x20   ship the rest, then close; or\n\
         \x20   file the remainder (`fno backlog idea`) and close with\n\
         \x20     --force --reason \"remaining ships filed as <id>\""
    )
}

/// Count MERGED refs; return (merged, first typed failure). Stops early at
/// the ceiling: once enough ships are confirmed, the remaining refs cannot
/// change the verdict. An unreachable ref is reported separately from a
/// genuinely unmerged one.
fn count_merged_refs(
    refs: &[(i64, Option<String>)],
    ceiling: i64,
    cwd: Option<&str>,
) -> (i64, Option<PrReadError>) {
    let mut merged = 0i64;
    let mut failure: Option<PrReadError> = None;
    let mut repo: Option<String> = None;
    for (pr_number, pr_url) in refs {
        let pr_repo = repo_slug_from_url(pr_url.as_deref()).or_else(|| repo.clone());
        if repo.is_none() {
            repo = pr_repo.clone();
        }
        let pr_cwd = if pr_repo.is_none() { cwd } else { None };
        match query_pr_state(*pr_number, pr_repo.as_deref(), pr_cwd) {
            Err(exc) => {
                let replaces = failure
                    .as_ref()
                    .map(|f| f.retryable() && !exc.retryable())
                    .unwrap_or(true);
                if replaces {
                    failure = Some(PrReadError::new(
                        format!("PR #{pr_number}: {}", exc.message),
                        &exc.kind,
                    ));
                }
            }
            Ok((state, _url)) => {
                if state == "MERGED" {
                    merged += 1;
                    if merged >= ceiling {
                        return (merged, None);
                    }
                } else if state == "UNKNOWN" {
                    failure = Some(PrReadError::new(
                        "REST reader returned UNKNOWN state",
                        "malformed",
                    ));
                }
            }
        }
    }
    (merged, failure)
}

/// The plan's declared ship count, tri-state: `Absent` (single-ship by
/// default), `Count(n)`, or `Unreadable` (present but not an integer).
/// The reaper's delivery predicate needs the third state - a declared
/// count that cannot be read is unknown, never one ship.
pub(crate) enum DeclaredShips {
    Absent,
    Count(i64),
    Unreadable,
}

pub(crate) fn declared_ships(fields: &crate::plan_doc::codec::Fields) -> DeclaredShips {
    match fields.get("expected_url_count") {
        None => DeclaredShips::Absent,
        Some(crate::plan_doc::codec::Value::Scalar(s)) => match s.trim().parse::<i64>() {
            Ok(n) => DeclaredShips::Count(n),
            Err(_) => DeclaredShips::Unreadable,
        },
        Some(_) => DeclaredShips::Unreadable,
    }
}

/// The deduplicated MERGED ref count one graph row carries: the primary
/// when its merge_status reads merged, plus every numbered additional_prs
/// entry recorded merged. A duplicate recording of the primary, or of an
/// already-counted extra, never counts twice, an unrecorded extra never
/// inflates the count, and a ref with no number is unverifiable evidence
/// that never counts - this is the local confirmed-merged evidence the
/// merged-lag delivery predicate reads, never a promise count.
pub(crate) fn delivery_merged_refs(entry: &Value) -> usize {
    let primary_merged = entry.get("merge_status").and_then(Value::as_str) == Some("merged");
    let primary_number = entry.get("pr_number").and_then(Value::as_i64);
    let mut merged = usize::from(primary_merged);
    let mut seen: Vec<i64> = Vec::new();
    for extra in entry
        .get("additional_prs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if extra.get("merge_status").and_then(Value::as_str) != Some("merged") {
            continue;
        }
        // A ref with no number cannot be told apart from any other ref, so
        // it is unverifiable evidence and never counts.
        let Some(number) = extra.get("number").and_then(Value::as_i64) else {
            continue;
        };
        if primary_merged && Some(number) == primary_number {
            continue;
        }
        if seen.contains(&number) {
            continue;
        }
        seen.push(number);
        merged += 1;
    }
    merged
}

/// The one plan-path resolver: a `#wave-1` fragment is stripped first (the
/// module's reader convention), `~/` expands against `$HOME`, a relative
/// path joins the close or sweep cwd, and an unresolvable path is None -
/// the caller decides what None reads as. One resolver so two readers can
/// never resolve the same plan_path to two different files.
pub(crate) fn resolve_plan_path(plan_path: &str, cwd: Option<&str>) -> Option<std::path::PathBuf> {
    let plan_path = plan_path.split('#').next().unwrap_or(plan_path);
    let path = match plan_path.strip_prefix("~/") {
        Some(rest) => std::path::PathBuf::from(std::env::var("HOME").ok()?).join(rest),
        None => PathBuf::from(plan_path),
    };
    if path.is_relative() {
        Some(Path::new(cwd?).join(path))
    } else {
        Some(path)
    }
}

/// The reaper side of the same delivery rule the close gate runs: a
/// recorded merge_status is the LAST ship, never the whole delivery. The
/// plan completion stamp decides: no plan, or a single-ship promise,
/// settles with the recorded merge; a multi-ship plan keeps its worker
/// until MERGED refs cover the promise; an unreadable or unparseable plan
/// reads as unknown, and unknown keeps the row - unknown is never done.
/// No network: the count arrives deduplicated from the graph's recorded
/// merge evidence, so a promise it cannot cover is unmet by construction.
/// The close gate itself is [`resolve_promise_evidence`].
pub(crate) fn merged_delivery_settled(
    merged_refs: usize,
    plan_path: Option<&str>,
    cwd: Option<&str>,
) -> bool {
    let Some(plan_path) = plan_path.filter(|p| !p.is_empty()) else {
        return true;
    };
    let Some(path) = resolve_plan_path(plan_path, cwd) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(frontmatter) = crate::plan_doc::codec::parse_frontmatter(&text) else {
        return false;
    };
    match declared_ships(&frontmatter.fields) {
        DeclaredShips::Absent => true,
        DeclaredShips::Count(n) if n < 2 => true,
        DeclaredShips::Count(n) => merged_refs >= n as usize,
        DeclaredShips::Unreadable => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_less_node_passes_without_gh_io() {
        let node = serde_json::json!({"id": "ab-1234abcd", "status": "in_progress"});
        let v = resolve_promise_evidence(&node, None, &[]);
        assert!(v.satisfied());
        assert!(v.warning.is_none());
    }

    #[test]
    fn refusal_c_names_the_ship_gap() {
        let text = promise_refusal_c("ab-1234abcd", "plans/p.md", 3, 1);
        assert!(text.contains("promised 3 ships; only 1 merged"), "{text}");
        assert!(text.contains("expected_url_count: 3"), "{text}");
    }

    #[test]
    fn refusal_b_names_the_probes() {
        let text = promise_refusal_b("ab-1234abcd", "plans/p.md", "probe x failed");
        assert!(
            text.contains("declared close_probes and at least one failed"),
            "{text}"
        );
        assert!(text.contains("probe x failed"), "{text}");
    }

    #[test]
    fn refusal_d_caps_the_carveout_list() {
        let deferred: Vec<Value> = (0..7)
            .map(|i| serde_json::json!({"id": format!("c{i}"), "need": format!("n{i}")}))
            .collect();
        let text = promise_refusal_d("ab-1234abcd", &deferred);
        assert!(text.contains("would close with 7 unharvested"), "{text}");
        assert!(text.contains("...and 2 more"), "{text}");
        assert!(!text.contains("c5:"), "{text}");
    }

    #[test]
    fn exit_codes_match_the_python_contract() {
        let mut v = PromiseVerdict {
            outcome: PromiseOutcome::Unmet,
            reason: None,
            warning: None,
        };
        assert_eq!(v.exit_code(), 6);
        v.outcome = PromiseOutcome::Unknown;
        assert_eq!(v.exit_code(), 4);
        v.outcome = PromiseOutcome::Ok;
        assert_eq!(v.exit_code(), 0);
        assert!(v.satisfied());
    }
}
