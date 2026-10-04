//! The main-CI repair-closure proof: `fno-agents probe-run main-ci
//! --workflow <name> --node <id>`.
//!
//! A node that repairs a red main used to close on its merge while main
//! stayed red (PR 2086's repair closed while main stayed red). The close
//! verbs already refuse a close
//! while a declared `close_probes` command fails; this module is the
//! trustworthy probe they were missing. It proves that the newest completed
//! verdict of ONE named workflow on main is `success` on a head that
//! CONTAINS the node's merge commit, and fails closed with one named word
//! (`red`, `pending`, `absent`, `stale`, `ambiguous`, `unreadable`).
//! Nothing is inferred from prose: a plan that declares no probe closes
//! exactly as it did before this module existed.
//!
//! It is an argument on the existing `probe-run` action, not a new action
//! (law d-fe66560a; `bin/client.rs` sits at the file budget).

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How many latest completed runs of the named workflow to scan. Same sizing
/// argument as `awaiting_merge::MAIN_RUN_LOOKBACK`: the `--workflow` filter
/// keeps a slow workflow's runs inside the window on a main that merges every
/// few minutes, where an unfiltered list can push them out and read a false
/// `absent` or `stale`.
const MAIN_CI_LOOKBACK: usize = 40;

/// The wall-clock ceiling for each `gh` read. The probe itself runs under the
/// close_probes runner's budget; these bounds keep any single read from
/// spending it all on one hang.
const GH_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The proof's verdict. `Green` is the only passing word; every other state
/// keeps the node open (fail closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proof {
    /// The newest verdict run for the workflow on a head containing the merge
    /// commit is `success`.
    Green { run_id: i64, head: String },
    /// The newest verdict run on a head containing the merge commit is
    /// `failure`.
    Red { run_id: i64 },
    /// No verdict run yet contains the merge commit, but an in-progress run
    /// does: main may still turn green.
    Pending { run_id: i64 },
    /// No run of the named workflow exists, or the node has no merged PR.
    Absent,
    /// Verdict runs exist but every one predates the merge commit.
    Stale,
    /// Two workflow ids share the requested workflow name.
    Ambiguous,
    /// A read could not answer: malformed payload or a failed ancestry read.
    Unreadable(String),
}

impl Proof {
    /// The one named word a close_probes consumer reads.
    pub fn word(&self) -> &'static str {
        match self {
            Proof::Green { .. } => "green",
            Proof::Red { .. } => "red",
            Proof::Pending { .. } => "pending",
            Proof::Absent => "absent",
            Proof::Stale => "stale",
            Proof::Ambiguous => "ambiguous",
            Proof::Unreadable(_) => "unreadable",
        }
    }

    /// The run id the verdict came from, when one exists.
    pub fn run_id(&self) -> Option<i64> {
        match self {
            Proof::Green { run_id, .. } | Proof::Red { run_id } | Proof::Pending { run_id } => {
                Some(*run_id)
            }
            _ => None,
        }
    }
}

/// `fno-agents probe-run main-ci --workflow <name> --node <id>`.
/// Exit 0 only on `green`; exit 1 on every other verdict; exit 2 on a usage
/// error. stdout is exactly the word (with the run id when one exists), so a
/// reconcile reason can quote it; the diagnostic detail goes to stderr.
pub fn run(args: &[String]) -> i32 {
    let mut workflow: Option<String> = None;
    let mut node_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workflow" => {
                i += 1;
                if i < args.len() {
                    workflow = Some(args[i].clone());
                }
            }
            "--node" => {
                i += 1;
                if i < args.len() {
                    node_id = Some(args[i].clone());
                }
            }
            _ => {}
        }
        i += 1;
    }
    let (Some(workflow), Some(node_id)) = (workflow, node_id) else {
        eprintln!("usage: fno-agents probe-run main-ci --workflow <workflowName> --node <id>");
        return 2;
    };

    // The node carries the PR number and its own cwd (the repo the PR lives
    // in), so a probe can run from any checkout.
    let Some(node) = read_node(&node_id, Path::new(".")) else {
        return print_verdict(&Proof::Unreadable(format!("node {node_id} unreadable")));
    };
    let Some(pr) = node
        .get("pr_number")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .map(|n| n.to_string())
    else {
        // A node whose PR is not merged reads absent; no PR at all is the
        // weaker case of the same fact.
        return print_verdict(&Proof::Absent);
    };
    let Some(node_cwd) = node_repo_dir(&node) else {
        // No cwd names no repo, and the caller's directory is evidence about
        // the caller, never about this node. Answer unreadable, not absent.
        return print_verdict(&Proof::Unreadable("node carries no cwd".to_string()));
    };

    // The merge commit the workflow runs must contain.
    let gh = std::ffi::OsStr::new("gh");
    let view = crate::loopcheck::bounded_read(
        gh,
        &["pr", "view", pr.as_str(), "--json", "state,mergeCommit"],
        &node_cwd,
        "main_ci_pr_view",
        GH_READ_TIMEOUT,
    );
    let merge_commit = match view {
        Ok(out) if out.status.success() => serde_json::from_slice::<Value>(&out.stdout)
            .ok()
            .and_then(|v: Value| {
                let state = v.get("state").and_then(Value::as_str)?;
                if state != "MERGED" {
                    return None;
                }
                v.get("mergeCommit")
                    .and_then(|m| m.get("oid"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        _ => None,
    };
    let Some(merge_commit) = merge_commit else {
        // Not MERGED (or the read failed) - the merge itself is not a fact
        // this proof can stand on.
        return print_verdict(&Proof::Absent);
    };

    let Some(slug) = crate::finalize::slug_from_git_remote(&node_cwd) else {
        return print_verdict(&Proof::Unreadable(
            "repo slug unreadable from the git remote".to_string(),
        ));
    };

    let limit = MAIN_CI_LOOKBACK.to_string();
    let fields = "databaseId,status,conclusion,headSha,workflowName,workflowDatabaseId";
    let list = crate::loopcheck::bounded_read(
        gh,
        &[
            "run",
            "list",
            "--branch",
            "main",
            "--workflow",
            workflow.as_str(),
            "--limit",
            limit.as_str(),
            "--json",
            fields,
        ],
        &node_cwd,
        "main_ci_run_list",
        GH_READ_TIMEOUT,
    );
    let runs: Value = match list {
        Ok(out) if out.status.success() => match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            Err(e) => return print_verdict(&Proof::Unreadable(format!("run list malformed: {e}"))),
        },
        _ => return print_verdict(&Proof::Unreadable("run list read failed".to_string())),
    };

    // Ancestry oracle: does `head` contain the merge commit? `ahead` or
    // `identical` means the merge commit is an ancestor of the head. A
    // compare read that cannot answer returns None, which classifies as
    // Unreadable rather than guessing.
    let slug_for_contains = slug.clone();
    let merge_for_contains = merge_commit.clone();
    let contains = move |head: &str| -> Option<bool> {
        let target = format!("repos/{slug_for_contains}/compare/{merge_for_contains}...{head}");
        let out = crate::loopcheck::bounded_read(
            gh,
            &["api", target.as_str(), "--jq", ".status"],
            &node_cwd,
            "main_ci_compare",
            GH_READ_TIMEOUT,
        )
        .ok()?;
        if !out.status.success() {
            return None;
        }
        let status = String::from_utf8_lossy(&out.stdout);
        match status.trim() {
            "ahead" | "identical" => Some(true),
            "behind" | "diverged" => Some(false),
            _ => None,
        }
    };

    let proof = classify(&runs, &workflow, &contains);
    print_verdict(&proof)
}

/// Print the verdict line and translate it to the exit code. stdout is the
/// word plus the run id when one exists; the evidence line on green names the
/// workflow, run id, head sha and merge commit the proof stood on.
fn print_verdict(proof: &Proof) -> i32 {
    match proof {
        Proof::Green { run_id, head } => {
            println!("green run={run_id} head={head}");
            0
        }
        other => {
            let word = other.word();
            match other.run_id() {
                Some(id) => println!("{word} run={id}"),
                None => println!("{word}"),
            }
            if let Proof::Unreadable(detail) = other {
                eprintln!("main-ci proof: {detail}");
            }
            1
        }
    }
}

/// The pure decision over an already-read `gh run list` payload. Newest-first
/// (the order gh returns); only exact `workflowName` matches participate, and
/// distinct `workflowDatabaseId` values under one name read Ambiguous.
/// `cancelled`/`skipped`/`neutral` runs produced no verdict and are skipped
/// without deciding - the same rule `awaiting_merge::parse_failing_run_ids`
/// applies. The first `success`/`failure` run whose head contains the merge
/// commit decides; verdicts that predate the merge commit are walked past.
pub fn classify(runs: &Value, workflow: &str, contains: &dyn Fn(&str) -> Option<bool>) -> Proof {
    let Some(arr) = runs.as_array() else {
        return Proof::Unreadable("run list is not a JSON array".to_string());
    };
    let mut workflow_ids: BTreeSet<i64> = BTreeSet::new();
    let mut named: Vec<&Value> = Vec::new();
    for run in arr {
        if run.get("workflowName").and_then(Value::as_str) != Some(workflow) {
            continue;
        }
        if let Some(id) = run.get("workflowDatabaseId").and_then(Value::as_i64) {
            workflow_ids.insert(id);
        }
        named.push(run);
    }
    if workflow_ids.len() > 1 {
        return Proof::Ambiguous;
    }
    if named.is_empty() {
        return Proof::Absent;
    }
    let mut pending_run: Option<i64> = None;
    for run in named {
        let status = run.get("status").and_then(Value::as_str).unwrap_or("");
        let conclusion = run.get("conclusion").and_then(Value::as_str).unwrap_or("");
        let run_id = run.get("databaseId").and_then(Value::as_i64);
        let head = run.get("headSha").and_then(Value::as_str).unwrap_or("");
        if status != "completed" {
            // An in-progress run whose head contains the merge commit keeps
            // main's answer open; a non-contained one proves nothing either
            // way.
            if !head.is_empty() && contains(head) == Some(true) {
                pending_run = pending_run.or(run_id);
            }
            continue;
        }
        match conclusion {
            "cancelled" | "skipped" | "neutral" => continue,
            "success" | "failure" => {
                if head.is_empty() {
                    return Proof::Unreadable("a verdict run carries no head sha".to_string());
                }
                match contains(head) {
                    Some(true) => {
                        return if conclusion == "success" {
                            Proof::Green {
                                run_id: run_id.unwrap_or(0),
                                head: head.to_string(),
                            }
                        } else {
                            Proof::Red {
                                run_id: run_id.unwrap_or(0),
                            }
                        };
                    }
                    Some(false) => continue,
                    None => {
                        return Proof::Unreadable("ancestry read failed".to_string());
                    }
                }
            }
            other => {
                return Proof::Unreadable(format!("unknown conclusion {other:?}"));
            }
        }
    }
    if let Some(run_id) = pending_run {
        return Proof::Pending { run_id };
    }
    Proof::Stale
}

/// The node row for `node_id`, read from the graph at `cwd`'s project scope.
fn read_node(node_id: &str, cwd: &Path) -> Option<Value> {
    let graph_path = crate::org_board::scope::graph_json_path(cwd);
    let store = crate::backlog::api::Store::new(&graph_path);
    let rows = crate::backlog::api::rows(&store).ok()?;
    crate::graph_get::find_entry(&rows, node_id).cloned()
}

/// The directory every gh read for this node runs in: the node's own cwd, the
/// one repo the proof can stand on. A node without one names no repo, so the
/// caller's directory is no substitute - it is evidence about the caller, and
/// reading gh there would answer a different repo's question.
fn node_repo_dir(node: &Value) -> Option<PathBuf> {
    node.get("cwd").and_then(Value::as_str).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run_row(id: i64, status: &str, conclusion: &str, head: &str, wf_id: i64) -> Value {
        json!({
            "databaseId": id,
            "status": status,
            "conclusion": conclusion,
            "headSha": head,
            "workflowName": "cli-ci",
            "workflowDatabaseId": wf_id,
        })
    }

    /// Oracles: "abc2" and later contain the merge commit "abc1"; "abc0"
    /// predates it; "old" ancestry cannot be read at all.
    fn oracle(head: &str) -> Option<bool> {
        match head {
            "abc2" | "abc3" => Some(true),
            "abc0" => Some(false),
            "old" => None,
            _ => Some(false),
        }
    }

    #[test]
    fn a_newest_success_run_on_a_containing_head_is_green() {
        // AC5-HP.
        let runs = json!([
            run_row(11, "completed", "success", "abc2", 7),
            run_row(9, "completed", "failure", "abc0", 7),
        ]);
        let proof = classify(&runs, "cli-ci", &oracle);
        assert_eq!(
            proof,
            Proof::Green {
                run_id: 11,
                head: "abc2".to_string()
            }
        );
    }

    #[test]
    fn the_newest_verdict_on_a_containing_head_decides_even_when_red() {
        let runs = json!([
            run_row(12, "completed", "failure", "abc2", 7),
            run_row(10, "completed", "success", "abc3", 7),
        ]);
        // Newest-first: run 12 contains the merge commit and failed - the
        // older green on a descendant is superseded. This is exactly the
        // closed-while-main-stayed-red repair the proof exists to catch.
        let proof = classify(&runs, "cli-ci", &oracle);
        assert_eq!(proof, Proof::Red { run_id: 12 });
    }

    #[test]
    fn an_in_progress_run_on_a_containing_head_reads_pending() {
        let runs = json!([run_row(13, "in_progress", "", "abc2", 7)]);
        let proof = classify(&runs, "cli-ci", &oracle);
        assert_eq!(proof, Proof::Pending { run_id: 13 });
    }

    #[test]
    fn no_run_of_the_workflow_reads_absent() {
        let runs = json!([run_row(1, "completed", "success", "abc2", 7)]);
        let proof = classify(&runs, "rust-ci", &oracle);
        assert_eq!(proof, Proof::Absent);
    }

    #[test]
    fn verdicts_that_all_predate_the_merge_commit_read_stale() {
        let runs = json!([run_row(9, "completed", "success", "abc0", 7)]);
        let proof = classify(&runs, "cli-ci", &oracle);
        assert_eq!(proof, Proof::Stale);
    }

    #[test]
    fn two_workflow_ids_under_one_name_read_ambiguous() {
        let runs = json!([
            run_row(1, "completed", "success", "abc2", 7),
            run_row(2, "completed", "success", "abc2", 8),
        ]);
        let proof = classify(&runs, "cli-ci", &oracle);
        assert_eq!(proof, Proof::Ambiguous);
    }

    #[test]
    fn a_failed_ancestry_read_is_unreadable_never_a_guess() {
        let runs = json!([run_row(14, "completed", "success", "old", 7)]);
        let proof = classify(&runs, "cli-ci", &oracle);
        assert_eq!(proof, Proof::Unreadable("ancestry read failed".to_string()));
    }

    #[test]
    fn a_non_array_payload_is_unreadable() {
        let proof = classify(&json!({"error": "boom"}), "cli-ci", &oracle);
        assert!(matches!(proof, Proof::Unreadable(_)));
    }

    #[test]
    fn a_node_without_a_cwd_names_no_repo_dir() {
        // The caller's directory is evidence about the caller, never about
        // the node: without a cwd the proof has no repo to read and answers
        // unreadable rather than asking gh a wrong-repo question.
        assert_eq!(node_repo_dir(&json!({"pr_number": 7})), None);
        assert_eq!(
            node_repo_dir(&json!({"cwd": "/repo/b"})),
            Some(PathBuf::from("/repo/b"))
        );
    }
}
