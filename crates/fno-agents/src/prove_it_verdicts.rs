//! `fno-agents prove-it-verdicts` -- one reader for terminal prove-it records.
//!
//! Client-side and daemon-free, like [`crate::graph_get`]: a verdict read walks
//! the graph and plan artifact files; it is a filesystem read, not an
//! agent-lifecycle operation.
//!
//! The census this verb answers : `/fno:review prove-it` ends every
//! report with the machine line `fno-prove-it: {"verdict":"...","claim":"..."}`,
//! and nothing read it after it was written. The PR 1599 coverage audit sat in
//! a plan artifacts directory with verdict FAIL while three sessions worked the
//! same problem it had already answered. A FAIL is strictly more actionable
//! than a PASS and is the one the machine dropped: a PASS needs no routing, a
//! FAIL means a node's claimed outcome did not hold.
//!
//! Contract: for every node with a `plan_path`, the read walks
//! `<plan>.artifacts/` and takes each file's LAST non-empty line (the same
//! rule `skills/review/scripts/validate-prove-it.sh` applies -- a mid-file
//! mention is not a record). SKIP and BLOCKED carry no verdict, so they never
//! retire a FAIL and never headline. A FAIL row stays `open` until it is
//! retired: a newer PASS whose claim STATES the FAIL claim (the full FAIL
//! claim text inside the PASS claim; the record may scope this with a
//! `retires` report path, which never replaces the claim match), or a ruling
//! whose `text` names the report (read from the machine-wide decision
//! index). Every unretired FAIL surfaces: a narrow re-run PASS cannot mute a
//! broader claim it did not answer. The newest PASS/FAIL record is
//! also emitted as the node's headline verdict. The verb never changes a
//! node's status: an unverified auditor must not move doneness, a king
//! rules. `--route` writes the one progress note that surfaces an open,
//! unrouted FAIL on the node.

use crate::decision_index;
use crate::graph_get::{default_graph_path, external_backend_selected};
use crate::graph_store::{self, entry_id, s_str};
use serde_json::{json, Value};
use std::collections::HashMap;
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
            "--json" | "-J" => {}
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

    // Journal record bodies by node: the third place a routing note lives.
    // A missing journal file reads as no records.
    let journal_bodies: HashMap<String, Vec<String>> =
        match crate::backlog::note_history::read(&graph_path, None, 0, usize::MAX) {
            Ok((records, _)) => {
                let mut map: HashMap<String, Vec<String>> = HashMap::new();
                for rec in records {
                    let Some(node) = rec.get("node_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let body = rec
                        .get("original")
                        .map(crate::backlog::note_history::record_body)
                        .unwrap_or("")
                        .to_string();
                    map.entry(node.to_string()).or_default().push(body);
                }
                map
            }
            Err(_) => HashMap::new(),
        };

    let mut unreadable: Vec<Value> = Vec::new();
    let rulings = load_rulings();
    let rows = build_rows(&entries, &mut unreadable, &rulings, &journal_bodies);

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
    /// Optional `retires` report path: the FAIL report this PASS declares it
    /// answers. Scopes retirement to that report; the claim match is still
    /// required.
    retires: Option<String>,
    mtime: SystemTime,
}

fn build_rows(
    entries: &[Value],
    unreadable: &mut Vec<Value>,
    rulings: &[Value],
    journal_bodies: &HashMap<String, Vec<String>>,
) -> Vec<Value> {
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
                    Ok(Some((verdict, claim, retires))) => {
                        let mtime = std::fs::metadata(&f)
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        reports.push(Report {
                            path,
                            verdict,
                            claim,
                            retires,
                            mtime,
                        });
                    }
                },
            }
        }
        // Verdict-bearing records only, oldest first; ties break on path so
        // the order is stable.
        let mut scored: Vec<&Report> = reports
            .iter()
            .filter(|r| r.verdict == "PASS" || r.verdict == "FAIL")
            .collect();
        if scored.is_empty() {
            continue;
        }
        scored.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));
        // A FAIL retires only through a newer PASS whose claim STATES the
        // FAIL claim (the reader used to take the newest record by mtime and
        // never compared claims, so a narrow re-run PASS muted a broader
        // FAIL). Every unretired FAIL surfaces.
        for (i, fail) in scored
            .iter()
            .enumerate()
            .filter(|(_, r)| r.verdict == "FAIL")
        {
            let retired = scored[i + 1..].iter().any(|pass| pass_retires(pass, fail));
            if !retired {
                rows.push(row_for(node, entry, fail, rulings, journal_bodies));
            }
        }
        // The newest PASS/FAIL record is still the node's headline verdict.
        let newest = scored[scored.len() - 1];
        if newest.verdict == "PASS" {
            rows.push(row_for(node, entry, newest, rulings, journal_bodies));
        }
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
/// record -- the negative control is the plan, which quotes the marker
/// mid-file). Err: a terminal line EXISTS but is not a legal record, which
/// must surface in `unreadable` and never silently read as zero records.
fn terminal_record(text: &str) -> Result<Option<(String, String, Option<String>)>, String> {
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
    let retires = parsed
        .get("retires")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some((verdict.to_string(), claim.to_string(), retires)))
}

/// A PASS retires a FAIL only when its claim STATES the FAIL's claim: the
/// full FAIL claim text appears inside the PASS claim -- the across-reports
/// twin of the validator's within-report rule that a narrower row cannot
/// prove a broader claim. An empty FAIL claim matches nothing, so it never
/// auto-retires. A present `retires` name scopes the retirement to that
/// report; it never replaces the claim match.
fn pass_retires(pass: &Report, fail: &Report) -> bool {
    if fail.claim.trim().is_empty() || !pass.claim.contains(&fail.claim) {
        return false;
    }
    match &pass.retires {
        Some(named) => named == &fail.path,
        None => true,
    }
}

fn row_for(
    node: &str,
    entry: &Value,
    win: &Report,
    rulings: &[Value],
    journal_bodies: &HashMap<String, Vec<String>>,
) -> Value {
    let status = s_str(entry, "status").map(str::to_string);
    // `routed` reads the graph, the ledger this run already has: the report
    // path named in any progress note (the legacy place), the current_state
    // body, or a journal record body means the note (the delivery leg)
    // already ran.
    let in_notes = entry
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
    let in_state = entry
        .get(crate::backlog::node_state::STATE_KEY)
        .and_then(|s| s.get("body"))
        .and_then(Value::as_str)
        .map(|t| t.contains(&win.path))
        .unwrap_or(false);
    let in_journal = journal_bodies
        .get(node)
        .map(|bodies| bodies.iter().any(|b| b.contains(&win.path)))
        .unwrap_or(false);
    let routed = in_notes || in_state || in_journal;
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

/// The machine-wide decision index, read through the shared module
/// (`decision_index`) so every Rust reader flattens and retires the same way.
/// The retirement key is the report PATH in a ruling's text, which needs no
/// subject resolution. A missing or damaged index reads as no rulings: a
/// maybe-ruled FAIL re-surfaces, a live one is never hidden.
fn load_rulings() -> Vec<Value> {
    let path = decision_index::default_state_path("decisions.jsonl");
    match decision_index::read_live(&path) {
        Ok(index) => index
            .rows
            .into_iter()
            // Only a row whose `text` can name a report path retires a FAIL.
            // Law rows carry `decision` and no `text`, and never reach here.
            .filter(|row| row.get("text").and_then(Value::as_str).is_some())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The LIVE rulings: decisions whose `text` can still retire a FAIL. The
/// flatten, the retirement derivation and the damaged-line count moved to
/// [`crate::decision_index::derive_live`]; this wrapper stays as the seam the
/// module's tests already call.
#[cfg(test)]
fn derive_live_rulings(text: &str) -> Vec<Value> {
    decision_index::derive_live(text)
        .rows
        .into_iter()
        .filter(|row| row.get("text").and_then(Value::as_str).is_some())
        .collect()
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
Retire with a newer PASS whose claim states this claim, or fno inbox decide {} naming this report.",
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

    fn record_line_named(verdict: &str, claim: &str, retires: &str) -> String {
        format!(
            "fno-prove-it: {}",
            serde_json::json!({"verdict": verdict, "claim": claim, "retires": retires})
        )
    }

    #[test]
    fn a_terminal_record_parses_and_a_midfile_mention_is_not_a_record() {
        let fail = format!("body\n\n{}", record_line("FAIL", "the claim"));
        assert_eq!(
            terminal_record(&fail).expect("parse"),
            Some(("FAIL".to_string(), "the claim".to_string(), None))
        );
        let named = record_line_named("PASS", "recheck", "/p/REPORT.md");
        assert_eq!(
            terminal_record(&named).expect("parse"),
            Some((
                "PASS".to_string(),
                "recheck".to_string(),
                Some("/p/REPORT.md".to_string())
            ))
        );
        // Negative control: the marker quoted mid-file (the plan shape).
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
    fn a_claim_stating_pass_retires_a_fail_and_a_newer_skip_does_not() {
        // AC1-EDGE, on the full row assembly with a temp graph. The PASS
        // retires only because its claim STATES the FAIL claim.
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
            &record_line("PASS", "recheck: old fail"),
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
        let rows = build_rows(&entries, &mut unreadable, &[], &HashMap::new());

        assert_eq!(rows.len(), 2, "no PASS/FAIL record means no row: {rows:?}");
        let a = rows.iter().find(|r| r["node"] == "x-aaa").expect("row a");
        assert_eq!(
            a["verdict"], "PASS",
            "a claim-stating PASS retires the FAIL"
        );
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
    fn a_narrow_pass_after_a_broad_fail_leaves_the_fail_open() {
        // The specimen: the re-run proved only the gate half and said
        // so; the broad FAIL must stay open beside the narrow PASS.
        let dir = tempfile::tempdir().expect("tempdir");
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).expect("mkdir");
        let broad = "the retirement done probe rejects incomplete evidence";
        let fail = report(
            &dir.path(),
            "plans/a.md.artifacts/REPORT.md",
            &record_line("FAIL", broad),
            ago(200),
        );
        let narrow = report(
            &dir.path(),
            "plans/a.md.artifacts/recheck/REPORT.md",
            &record_line(
                "PASS",
                "an isolated receipt with only active-surface exits 1",
            ),
            SystemTime::now(),
        );
        let entries = vec![
            json!({"id": "x-aaa", "status": "in_progress", "plan_path": plans.join("a.md").display().to_string(), "cwd": dir.path().display().to_string()}),
        ];
        let mut unreadable = Vec::new();
        let rows = build_rows(&entries, &mut unreadable, &[], &HashMap::new());

        assert_eq!(rows.len(), 2, "both rows surface: {rows:?}");
        let f = rows
            .iter()
            .find(|r| r["verdict"] == "FAIL")
            .expect("fail row");
        assert_eq!(f["report"], fail, "the row names the failing report");
        assert_eq!(f["claim"], broad);
        assert_eq!(f["open"], true, "the broad FAIL stays open");
        let p = rows
            .iter()
            .find(|r| r["verdict"] == "PASS")
            .expect("pass row");
        assert_eq!(p["report"], narrow);
        assert_eq!(p["open"], false);
    }

    #[test]
    fn a_named_retirement_still_needs_the_claim_match() {
        // `retires` names the report a PASS answers; the claim match is still
        // required, a name pointing elsewhere retires nothing, and an empty
        // FAIL claim never auto-retires.
        let dir = tempfile::tempdir().expect("tempdir");
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).expect("mkdir");
        let mk = |name: &str| plans.join(name).display().to_string();

        let named_fail = report(
            &dir.path(),
            "plans/d.md.artifacts/REPORT.md",
            &record_line("FAIL", "broad claim"),
            ago(200),
        );
        let _other_fail = report(
            &dir.path(),
            "plans/e.md.artifacts/REPORT.md",
            &record_line("FAIL", "broad claim"),
            ago(200),
        );
        let _empty_fail = report(
            &dir.path(),
            "plans/f.md.artifacts/REPORT.md",
            // Whitespace-only: matches nothing, the same as empty.
            &record_line("FAIL", " "),
            ago(200),
        );
        // Name without match retires nothing.
        let _mismatch = report(
            &dir.path(),
            "plans/d.md.artifacts/recheck/REPORT.md",
            &record_line_named("PASS", "narrow half only", &named_fail),
            ago(100),
        );
        // Claim stated but the name points at another report: scoped away.
        let _scoped_away = report(
            &dir.path(),
            "plans/e.md.artifacts/recheck/REPORT.md",
            &record_line_named("PASS", "recheck: broad claim", "/nowhere/REPORT.md"),
            ago(100),
        );
        // An empty FAIL claim matches nothing.
        let _vacuous = report(
            &dir.path(),
            "plans/f.md.artifacts/recheck/REPORT.md",
            &record_line("PASS", "recheck"),
            ago(100),
        );
        // Claim stated, no name: retires the FAIL whose claim it states.
        let _broad_pass = report(
            &dir.path(),
            "plans/g.md.artifacts/REPORT.md",
            &record_line("FAIL", "broad claim"),
            ago(300),
        );
        let _g_pass = report(
            &dir.path(),
            "plans/g.md.artifacts/recheck/REPORT.md",
            &record_line("PASS", "recheck: broad claim"),
            ago(50),
        );

        let entries: Vec<Value> = ["d", "e", "f", "g"]
            .iter()
            .map(|n| {
                json!({"id": format!("x-{n}"), "status": "done", "plan_path": mk(&format!("{n}.md")), "cwd": dir.path().display().to_string()})
            })
            .collect();
        let mut unreadable = Vec::new();
        let rows = build_rows(&entries, &mut unreadable, &[], &HashMap::new());

        assert_eq!(
            rows.len(),
            7,
            "d/e/f each surface FAIL+PASS, g one PASS: {rows:?}"
        );
        let d = rows
            .iter()
            .find(|r| r["node"] == "x-d" && r["verdict"] == "PASS")
            .expect("row d");
        assert_eq!(d["verdict"], "PASS", "the headline PASS still surfaces");
        let d_fail = rows
            .iter()
            .filter(|r| r["node"] == "x-d")
            .find(|r| r["verdict"] == "FAIL");
        assert_eq!(
            d_fail.expect("fail row stays")["open"],
            true,
            "a named-but-mismatched PASS retires nothing"
        );
        let e = rows.iter().find(|r| r["node"] == "x-e").expect("row e");
        assert_eq!(
            e["open"], true,
            "a claim-stating PASS scoped to another report retires nothing"
        );
        let f = rows.iter().find(|r| r["node"] == "x-f").expect("row f");
        assert_eq!(f["open"], true, "an empty FAIL claim never auto-retires");
        let g: Vec<_> = rows.iter().filter(|r| r["node"] == "x-g").collect();
        assert_eq!(g.len(), 1, "the claim-stating unnamed PASS retired: {g:?}");
        assert_eq!(g[0]["verdict"], "PASS");
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
        let rows = build_rows(&entries, &mut unreadable, &[], &HashMap::new());
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
            retires: None,
            mtime: SystemTime::now(),
        };
        let rulings: Vec<Value> = vec![json!({"decision_id": "d-2", "text": "unrelated"})];
        let row = row_for("x-aaa", &entry, &win, &rulings, &HashMap::new());
        assert_eq!(
            row["routed"], true,
            "a note naming the report is the routed marker"
        );
        assert_eq!(row["open"], true, "routed-but-unruled stays open");

        let rulings = vec![
            json!({"decision_id": "d-1", "text": "ruled on /plans/a.md.artifacts/coverage/REPORT.md: wont_fix"}),
            json!({"decision_id": "d-2", "text": "unrelated"}),
        ];
        let row = row_for("x-aaa", &entry, &win, &rulings, &HashMap::new());
        assert_eq!(row["ruled_by"], "d-1");
        assert_eq!(
            row["open"], false,
            "a ruling naming the report retires the FAIL"
        );

        assert_eq!(ruling_for(&rulings, "/nowhere/else.md"), None);
        assert_eq!(ruling_for(&[], report_path), None);
    }

    #[test]
    fn a_current_state_body_naming_the_report_reads_routed() {
        let report_path = "/plans/b.md.artifacts/REPORT.md";
        let entry = json!({
            "id": "x-bbb",
            "status": "in_progress",
            "current_state": {"revision": 6, "body": format!("routed the FAIL at {report_path}")},
        });
        let win = Report {
            path: report_path.to_string(),
            verdict: "FAIL".to_string(),
            claim: "c".to_string(),
            retires: None,
            mtime: SystemTime::now(),
        };
        let row = row_for("x-bbb", &entry, &win, &[], &HashMap::new());
        assert_eq!(row["routed"], true, "current_state.body is a routed marker");
        // A state body naming something else does not route.
        let other = json!({
            "id": "x-bbb",
            "status": "in_progress",
            "current_state": {"revision": 6, "body": "unrelated prose"},
        });
        let row = row_for("x-bbb", &other, &win, &[], &HashMap::new());
        assert_eq!(row["routed"], false);
    }

    #[test]
    fn a_journal_body_naming_the_report_reads_routed() {
        let report_path = "/plans/c.md.artifacts/REPORT.md";
        let entry = json!({"id": "x-ccc", "status": "in_progress"});
        let win = Report {
            path: report_path.to_string(),
            verdict: "FAIL".to_string(),
            claim: "c".to_string(),
            retires: None,
            mtime: SystemTime::now(),
        };
        let mut journal: HashMap<String, Vec<String>> = HashMap::new();
        journal.insert(
            "x-ccc".to_string(),
            vec![format!("replaced state after routing {report_path}")],
        );
        let row = row_for("x-ccc", &entry, &win, &[], &journal);
        assert_eq!(
            row["routed"], true,
            "journal record body is a routed marker"
        );
        // A journal record with a legacy `text` original reads the same way.
        let mut journal = HashMap::new();
        journal.insert(
            "x-ccc".to_string(),
            vec![format!("legacy migration note {report_path}")],
        );
        let row = row_for("x-ccc", &entry, &win, &[], &journal);
        assert_eq!(row["routed"], true);
        let empty: HashMap<String, Vec<String>> = HashMap::new();
        let row = row_for("x-ccc", &entry, &win, &[], &empty);
        assert_eq!(row["routed"], false);
    }

    #[test]
    fn a_legacy_progress_note_still_reads_routed() {
        let report_path = "/plans/d.md.artifacts/REPORT.md";
        let entry = json!({
            "id": "x-ddd",
            "status": "in_progress",
            "progress_notes": [{"text": format!("delivered {report_path}")}],
        });
        let win = Report {
            path: report_path.to_string(),
            verdict: "FAIL".to_string(),
            claim: "c".to_string(),
            retires: None,
            mtime: SystemTime::now(),
        };
        let row = row_for("x-ddd", &entry, &win, &[], &HashMap::new());
        assert_eq!(row["routed"], true, "the legacy read is unchanged");
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
    fn the_short_json_spelling_parses_like_the_long_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_graph(dir.path(), &[json!({"id": "x-aaa"})]);
        let args: Vec<String> = vec![
            "-J".to_string(),
            "--graph".to_string(),
            path.display().to_string(),
        ];
        // -J is accepted (exit 0 on an empty report), not an unknown flag (2).
        assert_eq!(run_prove_it_verdicts(&args), 0);
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
