//! `fno-agents prove-it-verdicts` -- one reader for terminal prove-it records.
//!
//! Client-side and daemon-free, like [`crate::graph_get`]: a verdict read walks
//! the graph and plan artifact files; it is a filesystem read, not an
//! agent-lifecycle operation.
//!
//! The census this verb answers (x-6d64): `/fno:review prove-it` ends every
//! report with the machine line `fno-prove-it: {"verdict":"...","claim":"..."}`,
//! and nothing read it after it was written. The PR 1599 coverage audit sat in
//! a plan artifacts directory with verdict FAIL while three sessions worked the
//! same problem it had already answered. A FAIL is strictly more actionable
//! than a PASS and is the one the machine dropped: a PASS needs no routing, a
//! FAIL means a node's claimed outcome did not hold.
//!
//! Contract: for every node with a `plan_path`, walk `<plan>.artifacts/` and
//! take each file's LAST non-empty line (the same rule
//! `skills/review/scripts/validate-prove-it.sh` applies -- a mid-file mention
//! is not a record). Per node the newest record by mtime among PASS and FAIL
//! wins; SKIP and BLOCKED carry no verdict, so they never retire a FAIL. A
//! FAIL row is `open` until a ruling whose `text` names the report retires it
//! (read from the machine-wide decision index). The verb never changes a
//! node's status: an
//! unverified auditor must not move doneness, a king rules. `--route` writes
//! the one progress note that surfaces an open, unrouted FAIL on the node.

use crate::graph_get::{default_graph_path, external_backend_selected};
use crate::graph_store::{self, entry_id, s_str};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const RECORD_PREFIX: &str = "fno-prove-it: ";
/// One record per report file; a node's artifacts tree is bounded so a
/// runaway directory cannot turn a board read into a walk.
const MAX_FILES_PER_NODE: usize = 200;
/// `<plan>.artifacts/` subtree depth: the specimen audit lives two levels down
/// (`coverage-audit-20260908/REPORT.md`); deeper nesting is not a report home.
const MAX_DEPTH: usize = 3;

pub fn run_prove_it_verdicts(args: &[String]) -> i32 {
    let mut route = false;
    let mut graph_path = default_graph_path();
    let mut graph_overridden = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // JSON is the verb's only output shape; the flag is accepted (and
            // spelled in every caller) so the read stays explicit.
            "--json" => {}
            "--route" => route = true,
            "--graph" => {
                i += 1;
                match args.get(i) {
                    Some(p) => {
                        graph_path = PathBuf::from(p);
                        graph_overridden = true;
                    }
                    None => {
                        eprintln!("fno-agents prove-it-verdicts: --graph needs a path");
                        return 2;
                    }
                }
            }
            other if other.starts_with('-') => {
                eprintln!("fno-agents prove-it-verdicts: unknown flag {other}");
                return 2;
            }
            other => {
                eprintln!("fno-agents prove-it-verdicts: unexpected argument {other}");
                return 2;
            }
        }
        i += 1;
    }
    // --route is a WRITE against the live store; a --graph read redirect
    // would make the note's target disagree with the rows it was derived from.
    if route && graph_overridden {
        eprintln!(
            "fno-agents prove-it-verdicts: --route writes notes to the live store, \
             so it refuses --graph (the fixture read and the live write would disagree)"
        );
        return 2;
    }
    // Same blind-read guard as graph-get: under an external tracker backend
    // the graph store is not authoritative (an explicit --graph is trusted).
    if !graph_overridden && external_backend_selected() {
        eprintln!(
            "fno-agents prove-it-verdicts: this reads graph.json directly; \
             under an external tracker backend that store is not authoritative."
        );
        return 1;
    }

    let mut entries = match graph_store::read_defaulted(&graph_path, false) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("fno-agents prove-it-verdicts: {err}");
            return 1;
        }
    };
    graph_store::apply_readiness_overlay(&mut entries);

    let mut unreadable: Vec<Value> = Vec::new();
    let rulings = load_rulings();
    let rows = build_rows(&entries, &mut unreadable, &rulings);

    let mut exit = 0;
    if route {
        exit = route_rows(&rows);
    }
    println!(
        "{}",
        serde_json::to_string(&json!({
            "read_at": iso_now(),
            "rows": rows,
            "unreadable": unreadable,
        }))
        .unwrap_or_else(|_| "{\"rows\":[],\"unreadable\":[]}".to_string())
    );
    exit
}

/// One plan artifacts tree's scanned report records.
struct Report {
    path: String,
    verdict: String,
    claim: String,
    mtime: SystemTime,
}

fn build_rows(entries: &[Value], unreadable: &mut Vec<Value>, rulings: &[Value]) -> Vec<Value> {
    let mut rows = Vec::new();
    for entry in entries.iter().filter(|e| e.is_object()) {
        let Some(node) = entry_id(entry) else {
            continue;
        };
        let Some(plan) = s_str(entry, "plan_path").filter(|p| !p.is_empty()) else {
            continue;
        };
        let plan_clean = plan.split('#').next().unwrap_or(plan);
        let artifacts = match artifacts_dir(plan_clean, entry) {
            Ok(a) => a,
            Err(err) => {
                unreadable.push(json!({"node": node, "path": plan_clean, "error": err}));
                continue;
            }
        };
        if !artifacts.is_dir() {
            continue;
        }
        let mut files: Vec<PathBuf> = Vec::new();
        walk_md(&artifacts, 0, &mut files);
        let mut reports: Vec<Report> = Vec::new();
        for f in files {
            let path = f.display().to_string();
            match std::fs::read_to_string(&f) {
                Err(err) => {
                    unreadable.push(json!({"node": node, "path": path, "error": format!("{err}")}))
                }
                Ok(text) => match terminal_record(&text) {
                    Err(err) => unreadable.push(json!({"node": node, "path": path, "error": err})),
                    Ok(None) => {}
                    Ok(Some((verdict, claim))) => {
                        let mtime = std::fs::metadata(&f)
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        reports.push(Report {
                            path,
                            verdict,
                            claim,
                            mtime,
                        });
                    }
                },
            }
        }
        // Newest PASS/FAIL record wins. A >= on equal mtimes keeps the later
        // walk order (paths are visited sorted), so the winner is stable.
        let winner = reports
            .iter()
            .filter(|r| r.verdict == "PASS" || r.verdict == "FAIL")
            .max_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));
        let Some(win) = winner else { continue };
        rows.push(row_for(node, entry, win, rulings));
    }
    rows
}

/// `<plan>.artifacts/`: the plan file's name with `.artifacts` appended, the
/// sidecar layout `cli/src/fno/retro/cli.py` reads and the target stop hook
/// writes. A `#fragment` plan_path is stripped before resolving; a relative
/// path belongs to the node's own checkout, not the caller's CWD.
fn artifacts_dir(plan_clean: &str, entry: &Value) -> Result<PathBuf, String> {
    let plan_file = Path::new(plan_clean);
    let plan_file = if plan_file.is_absolute() {
        plan_file.to_path_buf()
    } else {
        let cwd = s_str(entry, "cwd")
            .filter(|c| !c.is_empty())
            .ok_or_else(|| "plan path is relative and the node carries no cwd".to_string())?;
        Path::new(cwd).join(plan_file)
    };
    Ok(PathBuf::from(format!("{}.artifacts", plan_file.display())))
}

fn walk_md(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= MAX_FILES_PER_NODE {
            return;
        }
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_file() {
            if p.extension().and_then(|x| x.to_str()) == Some("md") {
                out.push(p);
            }
        } else if ft.is_dir() && depth < MAX_DEPTH {
            walk_md(&p, depth + 1, out);
        }
    }
}

/// The terminal record: the LAST non-empty line, the exact `fno-prove-it: `
/// prefix at the line's start (the validator's own rule), parseable JSON with
/// a legal verdict. Ok(None): no terminal record (a mid-file mention is not a
/// record -- the negative control is the x-5aef plan, which quotes the marker
/// mid-file). Err: a terminal line EXISTS but is not a legal record, which
/// must surface in `unreadable` and never silently read as zero records.
fn terminal_record(text: &str) -> Result<Option<(String, String)>, String> {
    let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return Ok(None);
    };
    let Some(record) = line.strip_prefix(RECORD_PREFIX) else {
        return Ok(None);
    };
    let parsed: Value = match serde_json::from_str(record) {
        Ok(v) => v,
        Err(err) => {
            return Err(format!(
                "terminal fno-prove-it record is not valid JSON ({err})"
            ))
        }
    };
    let verdict = parsed.get("verdict").and_then(Value::as_str).unwrap_or("");
    match verdict {
        "PASS" | "FAIL" | "BLOCKED" | "SKIP" => {}
        other => {
            return Err(format!(
                "terminal fno-prove-it record carries no legal verdict (got {other:?})"
            ))
        }
    }
    let claim = parsed.get("claim").and_then(Value::as_str).unwrap_or("");
    Ok(Some((verdict.to_string(), claim.to_string())))
}

fn row_for(node: &str, entry: &Value, win: &Report, rulings: &[Value]) -> Value {
    let status = s_str(entry, "status").map(str::to_string);
    // `routed` reads the graph, the ledger this run already has: any progress
    // note naming the report means the note (the delivery leg) already ran.
    let routed = entry
        .get("progress_notes")
        .and_then(Value::as_array)
        .map(|notes| {
            notes.iter().any(|n| {
                n.get("text")
                    .and_then(Value::as_str)
                    .map(|t| t.contains(&win.path))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let mut ruled_by = None;
    let mut open = false;
    if win.verdict == "FAIL" {
        ruled_by = ruling_for(rulings, &win.path);
        open = ruled_by.is_none();
    }
    json!({
        "node": node,
        "status": status,
        "report": win.path,
        "verdict": win.verdict,
        "claim": win.claim,
        "mtime": iso(win.mtime),
        "open": open,
        "routed": routed,
        "ruled_by": ruled_by,
    })
}

/// The machine-wide decision index (`paths.decisions_jsonl()`), the same file
/// `fno inbox decisions` reads first. The retirement key is the report PATH in
/// a ruling's text, which needs no subject resolution, so the verb reads the
/// index directly: measured 2026-09-12, shelling the Python verb costs ~25s
/// per FAIL row (it folds graph projections and journal roots), which breaks
/// the SessionStart budget this verb's outstanding leg runs inside. A missing
/// or damaged index reads as no rulings: a maybe-ruled FAIL re-surfaces, a
/// live one is never hidden. Damaged lines are skipped, matching the Python
/// reader's posture.
fn load_rulings() -> Vec<Value> {
    let path = default_state_path("decisions.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    derive_live_rulings(&text)
}

/// The LIVE rulings: decisions whose `text` can still retire a FAIL. Mirrors
/// `fno inbox decisions`' read: the index stores event ENVELOPES
/// (`{type, ts, data}`) that the Python reader flattens (data fields at the
/// top plus `_event_type` and the envelope's `ts`) and rows that are neither
/// a decision nor a retraction envelope are discarded as damaged. It then
/// derives lifecycle: a `decision_retracted` row retires its
/// `target_decision_id` (newest `(ts, reason)` wins) and a decision whose
/// `supersedes` names another retires that one (newest `(ts, decision_id)`
/// wins). Only LIVE rulings retire a FAIL, so an overturned ruling un-hides
/// the FAIL again. ids compare casefolded, the Python reader's own rule.
fn derive_live_rulings(text: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for line in text.lines() {
        let Ok(env) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(data) = env.get("data").and_then(Value::as_object) else {
            continue;
        };
        let Some(etype) = env.get("type").and_then(Value::as_str) else {
            continue;
        };
        if etype != "operator_decision" && etype != "decision_retracted" {
            continue;
        }
        // The Python reader marks an envelope whose required data field is
        // empty as damaged (discarded), not as a live or retiring row.
        let required = if etype == "operator_decision" {
            "decision_id"
        } else {
            "target_decision_id"
        };
        if data
            .get(required)
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true)
        {
            continue;
        }
        let mut flat = Value::Object(data.clone());
        let obj = flat.as_object_mut().expect("just built");
        obj.insert("_event_type".to_string(), json!(etype));
        obj.insert(
            "ts".to_string(),
            env.get("ts").cloned().unwrap_or(Value::Null),
        );
        rows.push(flat);
    }
    let is_decision = |row: &Value| {
        matches!(
            row.get("_event_type").and_then(Value::as_str),
            Some("operator_decision")
        )
    };
    let rank = |row: &Value, tie: &str| {
        (
            row.get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            row.get(tie)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
    };
    let mut retired: std::collections::BTreeMap<String, (String, String)> = Default::default();
    for row in rows
        .iter()
        .filter(|r| r.get("_event_type").and_then(Value::as_str) == Some("decision_retracted"))
    {
        let target = row
            .get("target_decision_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if target.is_empty() {
            continue;
        }
        let r = rank(row, "reason");
        if retired.get(&target).map_or(true, |prev| *prev < r) {
            retired.insert(target, r);
        }
    }
    for row in rows.iter().filter(|r| is_decision(r)) {
        let target = row
            .get("supersedes")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if target.is_empty() {
            continue;
        }
        let r = rank(row, "decision_id");
        if retired.get(&target).map_or(true, |prev| *prev < r) {
            retired.insert(target, r);
        }
    }
    rows.into_iter()
        .filter(is_decision)
        .filter(|row| row.get("text").and_then(Value::as_str).is_some())
        .filter(|row| {
            let id = row
                .get("decision_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            id.is_empty() || !retired.contains_key(&id)
        })
        .collect()
}

/// `$FNO_HOME/<name>`, else `$HOME/.fno/<name>`: the same resolution
/// `graph_get::default_graph_path` applies to the graph store.
fn default_state_path(name: &str) -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(v).join(name);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".fno").join(name)
}

/// A ruling retires the FAIL when its `text` names the report path.
fn ruling_for(rulings: &[Value], report: &str) -> Option<String> {
    rulings.iter().find_map(|d| {
        let text = d.get("text").and_then(Value::as_str)?;
        if text.contains(report) {
            d.get("decision_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            None
        }
    })
}

fn note_exit(node: &str, body: &str, quiet: bool) -> Option<i32> {
    let mut cmd = Command::new("fno");
    cmd.args(["backlog", "note", node, "--body-file", "-"]);
    if quiet {
        cmd.arg("--quiet");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().ok()?;
    child.stdin.take()?.write_all(body.as_bytes()).ok()?;
    child.wait().ok().and_then(|s| s.code())
}

/// One note per open, unrouted FAIL. The note verb refuses BEFORE the append
/// when nobody is bound (exit 3); the `--quiet` retry still writes the note.
/// Exit 4 means the note landed and the mail leg did not confirm -- the note
/// is the delivery this route owes, so that counts as written.
fn route_rows(rows: &[Value]) -> i32 {
    let mut failures = 0;
    for row in rows {
        if row["open"] != Value::Bool(true) || row["routed"] == Value::Bool(true) {
            continue;
        }
        let Some(node) = row["node"].as_str() else {
            continue;
        };
        let body = format!(
            "prove-it FAIL: {}. Report: {}. The node stays {}; a king rules. \
Retire with a newer PASS record or fno inbox decide {} naming this report.",
            row["claim"].as_str().unwrap_or(""),
            row["report"].as_str().unwrap_or(""),
            row["status"].as_str().unwrap_or("unknown"),
            node,
        );
        match note_exit(node, &body, false) {
            Some(0) | Some(4) => {}
            Some(3) => {
                if !matches!(note_exit(node, &body, true), Some(0) | Some(4)) {
                    eprintln!(
                        "fno-agents prove-it-verdicts: --quiet note for {node} was not written"
                    );
                    failures += 1;
                }
            }
            other => {
                eprintln!(
                    "fno-agents prove-it-verdicts: note for {node} not written (fno backlog note exited {other:?})"
                );
                failures += 1;
            }
        }
    }
    i32::from(failures > 0)
}

fn iso(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn iso_now() -> String {
    iso(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn write_graph(dir: &Path, entries: &[Value]) -> PathBuf {
        let path = dir.join("graph.json");
        std::fs::write(&path, serde_json::json!({"entries": entries}).to_string())
            .expect("write graph");
        path
    }

    fn report(dir: &Path, rel: &str, body: &str, mtime: SystemTime) -> String {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&path, body).expect("write report");
        let f = File::options().append(true).open(&path).expect("open");
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .expect("set mtime");
        path.display().to_string()
    }

    fn ago(secs: u64) -> SystemTime {
        SystemTime::now() - std::time::Duration::from_secs(secs)
    }

    fn record_line(verdict: &str, claim: &str) -> String {
        format!(
            "fno-prove-it: {}",
            serde_json::json!({"verdict": verdict, "claim": claim})
        )
    }

    #[test]
    fn a_terminal_record_parses_and_a_midfile_mention_is_not_a_record() {
        let fail = format!("body\n\n{}", record_line("FAIL", "the claim"));
        assert_eq!(
            terminal_record(&fail).expect("parse"),
            Some(("FAIL".to_string(), "the claim".to_string()))
        );
        // Negative control: the marker quoted mid-file (the x-5aef plan shape).
        let mid = format!("fno-prove-it: quoted\ntrailing prose\n");
        assert_eq!(terminal_record(&mid).expect("parse"), None);
        assert_eq!(terminal_record("").expect("parse"), None);
    }

    #[test]
    fn a_malformed_or_illegal_terminal_record_is_an_error() {
        let bad_json = "fno-prove-it: {not json";
        assert!(terminal_record(bad_json).is_err());
        let bad_verdict = format!("fno-prove-it: {}", serde_json::json!({"verdict": "MAYBE"}));
        assert!(terminal_record(&bad_verdict).is_err());
        let no_verdict = format!("fno-prove-it: {}", serde_json::json!({"claim": "x"}));
        assert!(terminal_record(&no_verdict).is_err());
    }

    #[test]
    fn a_newer_pass_retires_a_fail_and_a_newer_skip_does_not() {
        // AC1-EDGE, on the full row assembly with a temp graph.
        let dir = tempfile::tempdir().expect("tempdir");
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).expect("mkdir");
        let mk = |name: &str| plans.join(name).display().to_string();

        let _fail_a = report(
            &dir.path(),
            "plans/a.md.artifacts/REPORT.md",
            &record_line("FAIL", "old fail"),
            ago(200),
        );
        let _pass_a = report(
            &dir.path(),
            "plans/a.md.artifacts/recheck/REPORT.md",
            &record_line("PASS", "recheck"),
            SystemTime::now(),
        );
        let fail_b = report(
            &dir.path(),
            "plans/b.md.artifacts/REPORT.md",
            &record_line("FAIL", "live fail"),
            ago(100),
        );
        let _skip_b = report(
            &dir.path(),
            "plans/b.md.artifacts/late/SKIP.md",
            &record_line("SKIP", "no surface"),
            SystemTime::now(),
        );
        let _mid_c = report(
            &dir.path(),
            "plans/c.md.artifacts/REPORT.md",
            &format!(
                "quotes the marker mid-file:\n{}\ntrailing prose after the marker\n",
                record_line("FAIL", "quoted, not terminal")
            ),
            SystemTime::now(),
        );

        let entries = vec![
            json!({"id": "x-aaa", "status": "done", "plan_path": mk("a.md"), "cwd": dir.path().display().to_string()}),
            json!({"id": "x-bbb", "status": "in_progress", "plan_path": mk("b.md"), "cwd": dir.path().display().to_string()}),
            json!({"id": "x-ccc", "status": "done", "plan_path": mk("c.md"), "cwd": dir.path().display().to_string()}),
        ];
        let mut unreadable = Vec::new();
        let rows = build_rows(&entries, &mut unreadable, &[]);

        assert_eq!(rows.len(), 2, "no PASS/FAIL record means no row: {rows:?}");
        let a = rows.iter().find(|r| r["node"] == "x-aaa").expect("row a");
        assert_eq!(a["verdict"], "PASS", "newest PASS/FAIL wins");
        assert_eq!(a["open"], false);
        let b = rows.iter().find(|r| r["node"] == "x-bbb").expect("row b");
        assert_eq!(b["verdict"], "FAIL");
        assert_eq!(
            b["open"], true,
            "a newer SKIP carries no verdict and retires nothing"
        );
        assert_eq!(b["report"], fail_b, "the row names the failing report");
        assert_eq!(b["claim"], "live fail");
        assert!(
            rows.iter().all(|r| r["node"] != "x-ccc"),
            "a mid-file mention yields no row"
        );
    }

    #[test]
    fn an_unreadable_artifacts_dir_and_a_malformed_record_surface_as_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).expect("mkdir");
        let plan = plans.join("a.md").display().to_string();
        std::fs::create_dir_all(plans.join("a.md.artifacts/locked")).expect("mkdir");
        // A directory named *.md is skipped by the *.md file filter; force an
        // unreadable FILE by writing one with no read permission.
        let locked = plans.join("a.md.artifacts/locked/REPORT.md");
        std::fs::write(&locked, "x").expect("write");
        let mut perm = std::fs::metadata(&locked).expect("meta").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perm.set_mode(0o000);
        }
        std::fs::set_permissions(&locked, perm).expect("chmod");
        report(
            &dir.path().join("plans"),
            "a.md.artifacts/malformed.md",
            "fno-prove-it: not json",
            SystemTime::now(),
        );

        let entries = vec![
            json!({"id": "x-aaa", "status": "done", "plan_path": plan}),
            // A relative plan_path with no cwd cannot resolve: named, not silent.
            json!({"id": "x-bbb", "status": "ready", "plan_path": "plans/rel.md"}),
        ];
        let mut unreadable = Vec::new();
        let rows = build_rows(&entries, &mut unreadable, &[]);
        assert!(rows.is_empty(), "no legal record, no row: {rows:?}");
        assert_eq!(
            unreadable.len(),
            3,
            "all three failures are named: {unreadable:?}"
        );
        assert!(
            unreadable.iter().any(|u| u["node"] == "x-aaa"),
            "the unreadable file is named"
        );
        assert!(
            unreadable.iter().any(|u| u["node"] == "x-bbb"),
            "the unresolvable plan path is named"
        );
    }

    #[test]
    fn the_walk_respects_depth_and_the_file_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        // depth 4 (artifacts/a/b/c/d/deep.md) is past MAX_DEPTH.
        let deep = dir.path().join("a/b/c/d");
        std::fs::create_dir_all(&deep).expect("mkdir");
        std::fs::write(deep.join("deep.md"), "x").expect("write");
        let mut files = Vec::new();
        walk_md(&dir.path(), 0, &mut files);
        assert!(files.is_empty(), "depth 4 is out of the walk: {files:?}");

        let ok = dir.path().join("a/b/c");
        std::fs::write(ok.join("shallow.md"), "x").expect("write");
        let mut files = Vec::new();
        walk_md(&dir.path(), 0, &mut files);
        assert_eq!(files.len(), 1, "depth 3 is in the walk");

        // The cap stops the walk at MAX_FILES_PER_NODE files.
        for i in 0..(MAX_FILES_PER_NODE + 20) {
            std::fs::write(dir.path().join(format!("flat-{i:04}.md")), "x").expect("write");
        }
        let mut files = Vec::new();
        walk_md(&dir.path(), 0, &mut files);
        assert_eq!(files.len(), MAX_FILES_PER_NODE);
    }

    #[test]
    fn a_routed_note_and_a_ruling_both_read_closed() {
        let report_path = "/plans/a.md.artifacts/coverage/REPORT.md";
        let entry = json!({
            "id": "x-aaa",
            "status": "done",
            "progress_notes": [{"text": format!("looked at {report_path} already")}],
        });
        let win = Report {
            path: report_path.to_string(),
            verdict: "FAIL".to_string(),
            claim: "c".to_string(),
            mtime: SystemTime::now(),
        };
        let rulings: Vec<Value> = vec![json!({"decision_id": "d-2", "text": "unrelated"})];
        let row = row_for("x-aaa", &entry, &win, &rulings);
        assert_eq!(
            row["routed"], true,
            "a note naming the report is the routed marker"
        );
        assert_eq!(row["open"], true, "routed-but-unruled stays open");

        let rulings = vec![
            json!({"decision_id": "d-1", "text": "ruled on /plans/a.md.artifacts/coverage/REPORT.md: wont_fix"}),
            json!({"decision_id": "d-2", "text": "unrelated"}),
        ];
        let row = row_for("x-aaa", &entry, &win, &rulings);
        assert_eq!(row["ruled_by"], "d-1");
        assert_eq!(
            row["open"], false,
            "a ruling naming the report retires the FAIL"
        );

        assert_eq!(ruling_for(&rulings, "/nowhere/else.md"), None);
        assert_eq!(ruling_for(&[], report_path), None);
    }

    fn jsonl_of_envelopes(rows: &[(&str, &str, Value)]) -> String {
        rows.iter()
            .map(|(etype, ts, data)| json!({"type": etype, "ts": ts, "data": data}).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_overturned_ruling_no_longer_retires_a_fail() {
        let report = "/plans/a.md.artifacts/REPORT.md";
        // The index stores ENVELOPES ({type, ts, data}); the fixture mirrors
        // the real row shape, not a flattened convenience copy.
        // the real row shape, not a flattened convenience copy.
        let live = json!({"decision_id": "d-1", "text": format!("ruled: {report} stays")});
        // A live ruling retires; an overturned one does not.
        assert_eq!(
            ruling_for(
                &derive_live_rulings(&jsonl_of_envelopes(&[(
                    "operator_decision",
                    "2026-09-10T00:00:00Z",
                    live.clone()
                )])),
                report
            ),
            Some("d-1".to_string())
        );
        let superseding =
            json!({"decision_id": "d-2", "supersedes": "D-1", "text": "changed my mind"});
        let rulings = derive_live_rulings(&jsonl_of_envelopes(&[
            ("operator_decision", "2026-09-10T00:00:00Z", live.clone()),
            ("operator_decision", "2026-09-11T00:00:00Z", superseding),
        ]));
        assert_eq!(
            ruling_for(&rulings, report),
            None,
            "superseded ruling is not live"
        );
        let retraction = json!({"target_decision_id": "d-1", "reason": "wrong"});
        let rulings = derive_live_rulings(&jsonl_of_envelopes(&[
            ("operator_decision", "2026-09-10T00:00:00Z", live.clone()),
            ("decision_retracted", "2026-09-11T00:00:00Z", retraction),
        ]));
        assert_eq!(
            ruling_for(&rulings, report),
            None,
            "retracted ruling is not live"
        );
        // A flat row with no envelope is a damaged line: discarded, and the
        // decision beside it still retires.
        let flat = json!({"decision_id": "d-9", "text": format!("{report} retired")}).to_string();
        let envelopes = jsonl_of_envelopes(&[("operator_decision", "2026-09-10T00:00:00Z", live)]);
        let envelopes = format!("{envelopes}\n{flat}");
        let rulings = derive_live_rulings(&envelopes);
        assert_eq!(
            ruling_for(&rulings, report),
            Some("d-1".to_string()),
            "the flat row is skipped; the envelope row survives"
        );
    }

    #[test]
    fn route_refuses_a_graph_override() {
        let args: Vec<String> = vec![
            "--route".to_string(),
            "--graph".to_string(),
            "/tmp/fake-graph.json".to_string(),
        ];
        assert_eq!(run_prove_it_verdicts(&args), 2);
    }

    #[test]
    fn read_defaulted_reads_the_written_graph_back() {
        // Positive control for the fixture writer: the same read_defaulted the
        // verb uses must see the entries the test wrote.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_graph(
            dir.path(),
            &[json!({"id": "x-aaa", "plan_path": "/p/a.md"})],
        );
        let entries = graph_store::read_defaulted(&path, false).expect("read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entry_id(&entries[0]), Some("x-aaa"));
    }
}
