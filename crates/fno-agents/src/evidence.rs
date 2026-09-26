//! The read that produced a ruling's code fact, carried on the ruling.
//!
//! Rust port of the checker + bounded read runner that first shipped as
//! Python `cli/src/fno/decide/evidence.py`, moved off the file-budget-gated
//! Python tree. Contract: docs/architecture/decision-record.md.
//! Transport is the hidden binary-direct `evidence-gate` verb: one JSON
//! request on stdin (lane, text, reads, root, timeout), one JSON answer on
//! stdout. A refusal is DATA (`ok: false`), not a process error.

use chrono::Utc;
use regex::Regex;
use serde::Deserialize;
#[cfg(test)]
use serde_json::Map;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The claim vocabulary, ONE constant so a reviewer audits it in one read. A
/// false positive on ordinary prose is the failure that gets this gate
/// disabled: it fires on two shapes and nothing else - a `path:line` citation
/// over a source extension, and a count bound to a fixed noun list (or its
/// negative; "zero callers" is a code fact, and the expensive kind).
pub const SOURCE_EXTS: &[&str] = &[
    "py", "rs", "ts", "tsx", "js", "sh", "md", "toml", "yaml", "json",
];
pub const MAX_READS: usize = 5;
pub const OUT_HEAD_LINES: usize = 5;
pub const OUT_HEAD_CHARS: usize = 400;

fn nouns() -> &'static str {
    "(?:lines?|call sites?|callers?|consumers?|usages?|occurrences?|matches|files?)"
}

fn citation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"\b[\w./\\-]+\.(?:{}):\d+(?:-\d+)?",
            SOURCE_EXTS.join("|")
        ))
        .expect("citation regex")
    })
}

fn counted_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"\b\+?\d+\s+{}", nouns())).expect("counted regex"))
}

fn negative_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"\b(?:no|zero)\s+{}", nouns())).expect("negative regex"))
}

/// The cited-ruling shape, case-sensitive like
/// `scripts/ci/check-skill-decision-ids.sh`: the all-capitals form is the
/// escape for an illustrative id.
fn decision_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bd-[0-9a-f]{8}\b").expect("decision id regex"))
}

/// A code fact stated with no read attached, or a read that could not run
/// (`kind: "unmeasured"`); a citation the repo contradicts (`kind:
/// "citation"`). The refusal strings are the durable teaching text.
#[derive(Debug)]
pub struct GateRefusal {
    pub kind: &'static str,
    pub message: String,
}

fn refusal(kind: &'static str, message: String) -> GateRefusal {
    GateRefusal { kind, message }
}

/// Claim spans in a ruling body, empty when it asserts none. Citations first,
/// then negatives, then counted; deduplicated preserving first occurrence.
pub fn find_code_claims(text: &str) -> Vec<String> {
    let mut claims: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for re in [citation_re(), negative_re(), counted_re()] {
        for m in re.find_iter(text) {
            if seen.insert(m.as_str()) {
                claims.push(m.as_str().to_string());
            }
        }
    }
    claims
}

fn tracked_files(root: &Path) -> Vec<String> {
    if let Ok(out) = Command::new("git")
        .arg("ls-files")
        .current_dir(root)
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    // Not a repo (a hermetic test tmp dir): every file counts.
    let mut files: Vec<String> = Vec::new();
    let _ = walk_files(root, root, &mut files);
    files.sort();
    files
}

fn walk_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            if p.file_name().and_then(|n| n.to_str()) != Some(".git") {
                walk_files(root, &p, out)?;
            }
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// One failure line per citation the repo contradicts, empty when clean.
/// The tracked-file read happens only when a citation exists: a claim-free
/// body answers without touching git.
pub fn check_citations(text: &str, root: &Path) -> Vec<String> {
    let claims = find_code_claims(text);
    if !claims.iter().any(|c| citation_re().is_match(c)) {
        return Vec::new();
    }
    let tracked = tracked_files(root);
    let tracked_norm: Vec<String> = tracked.iter().map(|p| p.replace('\\', "/")).collect();
    let mut failures: Vec<String> = Vec::new();
    for claim in claims {
        if !citation_re().is_match(&claim) {
            continue;
        }
        let split_at = claim.rfind(':').expect("citation carries a colon");
        let path_text = &claim[..split_at];
        let line_text = &claim[split_at + 1..];
        let line: usize = line_text
            .split('-')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        let exact: Vec<&String> = tracked_norm.iter().filter(|p| *p == path_text).collect();
        let resolved: Option<&String> = if !exact.is_empty() {
            Some(exact[0])
        } else {
            let named: Vec<&String> = tracked_norm
                .iter()
                .filter(|p| p.rsplit('/').next() == Some(path_text))
                .collect();
            match named.len() {
                1 => Some(named[0]),
                0 => {
                    failures.push(format!("{claim}: names no tracked file"));
                    None
                }
                n => {
                    failures.push(format!(
                        "{claim}: '{path_text}' resolves to {n} tracked files; \
                         write the repo-relative path"
                    ));
                    None
                }
            }
        };
        let Some(rel) = resolved else { continue };
        let full = root.join(rel);
        match std::fs::read(&full) {
            Err(_) => failures.push(format!("{claim}: the file exists but is unreadable")),
            Ok(bytes) => {
                let length = String::from_utf8_lossy(&bytes).lines().count();
                if line > length {
                    failures.push(format!("{claim}: the file has {length} lines"));
                }
            }
        }
    }
    failures
}

/// The cited ids whose lowercase form names no known ruling, first-seen
/// order, deduped. Pure: the store read stays in
/// [`check_decision_citations`].
pub fn unknown_decision_ids(text: &str, known: &HashSet<String>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for m in decision_id_re().find_iter(text) {
        if seen.insert(m.as_str().to_string()) && !known.contains(&m.as_str().to_lowercase()) {
            ids.push(m.as_str().to_string());
        }
    }
    ids
}

/// Whether the text cites the decision-id shape at all. A caller scanning
/// many texts (the push's commit loop) uses it to load the known set once,
/// on the first match, instead of paying a store read per text.
pub fn cites_decision_id(text: &str) -> bool {
    decision_id_re().is_match(text)
}

/// The known decision ids: every `decision_id` on an operator_decision row
/// of the store, retired rows included (supersession history honestly cites
/// retired ids), lowercase.
pub fn known_decision_ids() -> Result<HashSet<String>, String> {
    let (rows, _) = crate::decision_index::read_store_rows(
        &crate::graph_get::default_graph_path(),
        &crate::decision_index::default_state_path("decisions.jsonl"),
    )?;
    Ok(rows
        .iter()
        .filter(|row| {
            row.get("_event_type")
                .and_then(Value::as_str)
                .map_or(true, |t| t == "operator_decision")
        })
        .filter_map(|row| row.get("decision_id").and_then(Value::as_str))
        .map(|id| id.to_lowercase())
        .collect())
}

/// The refusal line for one unknown id: the durable teaching text.
fn unknown_id_failure(id: &str) -> String {
    format!(
        "{id}: no ruling on this machine carries this id. Check it with \
         fno backlog decisions {id}, cite the live ruling, or write the id \
         as {} (capitals) when it is an example",
        id.to_uppercase()
    )
}

/// The decision-id arm: one failure per cited id no ruling on this machine
/// carries. Text with no id reads nothing. A store read that fails is one
/// failure naming the error: a citation that cannot be checked is not a
/// pass.
pub fn check_decision_citations(text: &str) -> Vec<String> {
    if !cites_decision_id(text) {
        return Vec::new();
    }
    match known_decision_ids() {
        Ok(known) => check_decision_citations_against(text, &known),
        Err(reason) => vec![format!(
            "the decision store could not be read ({reason}); the citation cannot be \
             checked, and an unchecked citation is not a pass"
        )],
    }
}

/// The arm against a caller-supplied known set, so one store read serves a
/// whole scan.
pub fn check_decision_citations_against(text: &str, known: &HashSet<String>) -> Vec<String> {
    unknown_decision_ids(text, known)
        .into_iter()
        .map(|id| unknown_id_failure(&id))
        .collect()
}

#[derive(Clone)]
pub struct RunOutcome {
    pub exit: i32,
    pub stdout: String,
}

pub type Runner<'a> = dyn FnMut(&str, &Path) -> Result<RunOutcome, String> + 'a;

/// The default read runner: POSIX `sh -c` (pipes are the point), cwd pinned
/// to the repo root, output captured, bounded by `timeout` via poll+kill -
/// a hung read refuses the write instead of hanging the gate.
pub fn shell_run(cmd: &str, root: &Path, timeout: u64) -> Result<RunOutcome, String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("os error {e}"))?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_thread = stdout_pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
    });
    let stderr_thread = stderr_pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= Duration::from_secs(timeout) {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(h) = stdout_thread {
                        let _ = h.join();
                    }
                    if let Some(h) = stderr_thread {
                        let _ = h.join();
                    }
                    return Err("timeout".to_string());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = stdout_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let _ = stderr_thread.map(|h| h.join());
    Ok(RunOutcome {
        exit: status.code().unwrap_or(-1),
        stdout,
    })
}

fn head_sha(root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Run each read, bounded, and return one evidence row per command.
///
/// A read that cannot run stores no row. An all-zero answer set refuses
/// under the standing pitfall - assert a positive marker, never an absence.
/// A non-zero exit WITH output is a measurement: `grep -c` exiting 1 on a
/// real zero is the read working.
pub fn run_reads(
    commands: &[String],
    root: &Path,
    runner: &mut Runner,
) -> Result<Vec<Value>, GateRefusal> {
    if commands.len() > MAX_READS {
        return Err(refusal(
            "unmeasured",
            format!(
                "cap is {MAX_READS} reads per ruling, got {}",
                commands.len()
            ),
        ));
    }
    let mut rows: Vec<Value> = Vec::new();
    let mut zero_flags: Vec<bool> = Vec::new();
    let sha = head_sha(root);
    for cmd in commands {
        let outcome = runner(cmd, root).map_err(|why| {
            refusal(
                "unmeasured",
                format!(
                    "read '{cmd}' did not run ({why}) and stored no row. A ruling \
                     whose own read does not run is not evidence."
                ),
            )
        })?;
        if outcome.exit == 126 || outcome.exit == 127 {
            // The shell started but the command did not (not found, not
            // executable). That is a broken read, not a measurement.
            return Err(refusal(
                "unmeasured",
                format!(
                    "read '{cmd}' did not run (exit {}) and stored no row. A ruling \
                     whose own read does not run is not evidence.",
                    outcome.exit
                ),
            ));
        }
        // The zero shapes read the FULL stdout, never the truncated head: a
        // read whose first five lines are blank but whose content follows is
        // a measurement, not a zero.
        let stripped = outcome.stdout.trim();
        zero_flags.push(stripped.is_empty() || stripped == "0");
        let head = outcome
            .stdout
            .lines()
            .take(OUT_HEAD_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        rows.push(json!({
            "cmd": cmd,
            "exit": outcome.exit,
            "out_head": truncate_chars(&head, OUT_HEAD_CHARS),
            "ts": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "head_sha": sha,
        }));
    }
    if !rows.is_empty() && zero_flags.iter().all(|z| *z) {
        return Err(refusal(
            "unmeasured",
            format!(
                "read '{}' produced a zero. A zero needs a control: a second \
                 --read of the same shape aimed at something known to be present. \
                 Check the axes that could move the reading - symbol, age, \
                 directory - before trusting the zero.",
                rows[0]["cmd"].as_str().unwrap_or_default()
            ),
        ));
    }
    Ok(rows)
}

/// Ruling-lane gate: the rows to store, or None when the body has no claim.
///
/// Order matters: a citation the repo contradicts is refused whatever is
/// attached to it, then a cited id no ruling carries, then a claim with no
/// read. No claim, no change from today's behavior.
pub fn check_ruling_evidence(
    text: &str,
    reads: &[String],
    root: &Path,
    runner: &mut Runner,
) -> Result<Option<Vec<Value>>, GateRefusal> {
    let mut failures = check_citations(text, root);
    failures.extend(check_decision_citations(text));
    if !failures.is_empty() {
        return Err(refusal(
            "citation",
            format!(
                "{}. Fix or drop the citation; an attached read does not save it.",
                failures.join("; ")
            ),
        ));
    }
    let claims = find_code_claims(text);
    if claims.is_empty() {
        return Ok(None);
    }
    if reads.is_empty() {
        return Err(refusal(
            "unmeasured",
            format!(
                "the ruling asserts a code fact ('{}') and carries no read. Attach --read <command>: \
                 it runs at record time and its output is stored on the row. fno inbox decide and \
                 fno inbox law set take --read. fno inbox outstanding clear does not: record the answer \
                 with fno inbox decide <subject> \"<answer>\" --question-id <q> --read \"<command>\", \
                 then run fno inbox outstanding clear <q> with no --answer. Example: --read \
                 \"rg -n '<term>' <file>\". Pair a zero with a control.",
                claims[0]
            ),
        ));
    }
    Ok(Some(run_reads(reads, root, runner)?))
}

/// Note-lane gate: (rows, claims), each None when not applicable.
///
/// A contradicted citation RAISES (a note is a fact on the node even when
/// --quiet); a claim with no read only reports - the note verb advises,
/// never refuses a body.
pub fn note_evidence(
    text: &str,
    reads: &[String],
    root: &Path,
    runner: &mut Runner,
) -> Result<(Option<Vec<Value>>, Option<Vec<String>>), GateRefusal> {
    let mut failures = check_citations(text, root);
    failures.extend(check_decision_citations(text));
    if !failures.is_empty() {
        return Err(refusal("citation", failures.join("; ")));
    }
    let claims = find_code_claims(text);
    if claims.is_empty() {
        return Ok((None, None));
    }
    if reads.is_empty() {
        return Ok((None, Some(claims)));
    }
    Ok((Some(run_reads(reads, root, runner)?), None))
}

#[derive(Deserialize)]
struct GateRequest {
    lane: String,
    text: String,
    #[serde(default)]
    reads: Option<Vec<String>>,
    root: String,
    #[serde(default)]
    timeout: Option<u64>,
}

fn gate_answer(answer: Result<Value, GateRefusal>) -> Value {
    match answer {
        Ok(value) => value,
        Err(r) => json!({"ok": false, "kind": r.kind, "message": r.message}),
    }
}

/// `fno-agents evidence-gate`: the hidden binary-direct transport behind the
/// Python ruling/note evidence gates (same `matches!` treatment as
/// `component-verdict`, so the routable-verb parity guard does not see it).
/// Reads one JSON request on stdin, prints one JSON answer on stdout, and
/// exits 0 whenever an answer was computed - a refusal is data, not an
/// error; exit 2 on malformed args or an unreadable request.
pub fn run_evidence_gate(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: fno-agents evidence-gate  (one JSON request on stdin: lane, text, reads, root, timeout)");
        return 0;
    }
    if !args.is_empty() {
        eprintln!("fno-agents evidence-gate: unexpected arguments; the request rides stdin");
        return 2;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("fno-agents evidence-gate: could not read stdin");
        return 2;
    }
    let req: GateRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents evidence-gate: bad request: {e}");
            return 2;
        }
    };
    let root = PathBuf::from(&req.root);
    let reads = req.reads.unwrap_or_default();
    let timeout = req.timeout.unwrap_or(20);
    let mut runner = |cmd: &str, r: &Path| shell_run(cmd, r, timeout);
    let answer = match req.lane.as_str() {
        "ruling" => gate_answer(
            check_ruling_evidence(&req.text, &reads, &root, &mut runner)
                .map(|rows| json!({"ok": true, "rows": rows})),
        ),
        "note" => gate_answer(
            note_evidence(&req.text, &reads, &root, &mut runner)
                .map(|(rows, claims)| json!({"ok": true, "rows": rows, "claims": claims})),
        ),
        other => {
            eprintln!("fno-agents evidence-gate: unknown lane '{other}'");
            return 2;
        }
    };
    println!("{answer}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(root: &Path, rel: &str, lines: usize) -> PathBuf {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &full,
            (1..=lines)
                .map(|i| format!("line {i}\n"))
                .collect::<String>(),
        )
        .expect("write");
        full
    }

    fn ok_run(stdout: &str, exit: i32) -> Result<RunOutcome, String> {
        Ok(RunOutcome {
            exit,
            stdout: stdout.to_string(),
        })
    }

    // AC1-HP: a clean citation resolves against the repo and fails nothing.
    #[test]
    fn clean_citation_returns_no_failures() {
        let dir = tempfile::tempdir().expect("tmp");
        write_file(dir.path(), "cli/src/fno/law.py", 57);
        assert!(check_citations("see cli/src/fno/law.py:57", dir.path()).is_empty());
    }

    // AC2-ERR: a line past the end names the real length.
    #[test]
    fn line_past_end_names_real_length() {
        let dir = tempfile::tempdir().expect("tmp");
        write_file(dir.path(), "cli/src/fno/law.py", 57);
        assert_eq!(
            check_citations("see cli/src/fno/law.py:99999", dir.path()),
            vec!["cli/src/fno/law.py:99999: the file has 57 lines"]
        );
    }

    // AC3-EDGE: an ambiguous bare basename names the count and the remedy.
    #[test]
    fn ambiguous_basename_names_count() {
        let dir = tempfile::tempdir().expect("tmp");
        write_file(dir.path(), "a/cli.py", 50);
        write_file(dir.path(), "b/cli.py", 50);
        let failures = check_citations("see cli.py:41", dir.path());
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("resolves to 2 tracked files"));
        assert!(failures[0].contains("write the repo-relative path"));
    }

    // AC4-ERR: an untracked path refuses.
    #[test]
    fn untracked_path_refuses() {
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(
            check_citations("see nosuchfile.py:1", dir.path()),
            vec!["nosuchfile.py:1: names no tracked file"]
        );
    }

    // AC5-HP: a counted noun is one claim span.
    #[test]
    fn counted_noun_is_a_claim() {
        assert_eq!(
            find_code_claims("the drain loop is 167 lines"),
            vec!["167 lines"]
        );
    }

    // AC6-EDGE: a negative claim over a listed noun is a code fact.
    #[test]
    fn negative_claim_is_a_claim() {
        assert_eq!(
            find_code_claims("it has no callers, zero files"),
            vec!["no callers", "zero files"]
        );
    }

    // AC7-EDGE: ordinary prose with a bare basename is never a claim.
    #[test]
    fn bare_basename_in_prose_is_not_a_claim() {
        let dir = tempfile::tempdir().expect("tmp");
        write_file(dir.path(), "sample.py", 200);
        assert!(find_code_claims("port sample.py out of the tree").is_empty());
        assert!(check_citations("port sample.py out of the tree", dir.path()).is_empty());
    }

    #[test]
    fn claims_dedupe_preserving_first_order() {
        let claims = find_code_claims("167 lines here; 167 lines there; no callers");
        assert_eq!(claims, vec!["no callers", "167 lines"]);
    }

    // AC8-HP: the real shell runner executes and captures the head.
    #[test]
    fn real_runner_executes_a_read() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner = |cmd: &str, root: &Path| shell_run(cmd, root, 20);
        let rows = run_reads(&["echo hi".to_string()], dir.path(), &mut runner).expect("rows");
        assert_eq!(rows[0]["exit"], 0);
        assert_eq!(rows[0]["out_head"], "hi");
        assert!(rows[0]["ts"].as_str().expect("ts").ends_with('Z'));
    }

    // AC9-EDGE: an all-zero answer set refuses under the zero-control rule;
    // one read with output alongside the zero lets both rows through.
    #[test]
    fn all_zero_answer_set_refuses() {
        let dir = tempfile::tempdir().expect("tmp");
        let outcomes: Vec<Result<RunOutcome, String>> = vec![ok_run("", 1), ok_run("", 0)];
        let mut calls = 0usize;
        let mut runner = |_cmd: &str, _root: &Path| -> Result<RunOutcome, String> {
            let r = outcomes[calls].clone();
            calls += 1;
            r
        };
        let err = run_reads(
            &["grep -c x f".to_string(), "true".to_string()],
            dir.path(),
            &mut runner,
        )
        .expect_err("zero refuses");
        assert_eq!(err.kind, "unmeasured");
        assert!(err.message.contains("zero needs a control"));
        assert!(err.message.contains("symbol, age, directory"));
    }

    #[test]
    fn zero_alongside_a_measurement_passes_both_rows() {
        let dir = tempfile::tempdir().expect("tmp");
        let outcomes: Vec<Result<RunOutcome, String>> = vec![ok_run("", 1), ok_run("3\n", 0)];
        let mut calls = 0usize;
        let mut runner = |_cmd: &str, _root: &Path| -> Result<RunOutcome, String> {
            let r = outcomes[calls].clone();
            calls += 1;
            r
        };
        let rows = run_reads(
            &["grep -c x f".to_string(), "rg -c y g".to_string()],
            dir.path(),
            &mut runner,
        )
        .expect("rows");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn zero_full_stdout_counts_even_when_head_is_blank() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner = |_cmd: &str, _root: &Path| ok_run("\n\n\n\n\n\n0\n", 0);
        let err =
            run_reads(&["weird".to_string()], dir.path(), &mut runner).expect_err("zero refuses");
        assert!(err.message.contains("zero needs a control"));
    }

    // AC10-ERR: a read that cannot run refuses, naming the command.
    #[test]
    fn failed_read_refuses_naming_the_command() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner =
            |_cmd: &str, _root: &Path| -> Result<RunOutcome, String> { Err("timeout".to_string()) };
        let err =
            run_reads(&["sleep 999".to_string()], dir.path(), &mut runner).expect_err("refuses");
        assert!(err
            .message
            .contains("read 'sleep 999' did not run (timeout)"));
        assert!(err.message.contains("is not evidence"));
    }

    #[test]
    fn shell_not_found_exit_refuses() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner = |_cmd: &str, _root: &Path| ok_run("", 127);
        let err = run_reads(&["nosuchcmd arg".to_string()], dir.path(), &mut runner)
            .expect_err("refuses");
        assert!(err.message.contains("did not run (exit 127)"));
    }

    #[test]
    fn read_cap_is_five() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner = |_cmd: &str, _root: &Path| ok_run("1\n", 0);
        let cmds: Vec<String> = (0..6).map(|i| format!("echo {i}")).collect();
        let err = run_reads(&cmds, dir.path(), &mut runner).expect_err("refuses");
        assert!(err.message.contains("cap is 5 reads per ruling, got 6"));
    }

    #[test]
    fn out_head_is_five_lines_and_four_hundred_chars() {
        let dir = tempfile::tempdir().expect("tmp");
        let short = (0..8).map(|i| format!("line {i}\n")).collect::<String>();
        let mut runner = move |_cmd: &str, _root: &Path| ok_run(&short, 0);
        let rows = run_reads(&["cat log".to_string()], dir.path(), &mut runner).expect("rows");
        let head = rows[0]["out_head"].as_str().expect("head");
        assert_eq!(head.lines().count(), 5);
        let long_line: String = "x".repeat(500);
        let long = (0..3).map(|_| format!("{long_line}\n")).collect::<String>();
        let mut runner = move |_cmd: &str, _root: &Path| ok_run(&long, 0);
        let rows = run_reads(&["cat big".to_string()], dir.path(), &mut runner).expect("rows");
        let head = rows[0]["out_head"].as_str().expect("head");
        assert_eq!(head.chars().count(), 400);
    }

    #[test]
    fn grep_count_one_with_real_output_is_a_measurement() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner = |_cmd: &str, _root: &Path| ok_run("3\n", 1);
        let rows = run_reads(
            &["grep -c territory d".to_string()],
            dir.path(),
            &mut runner,
        )
        .expect("rows");
        assert_eq!(rows[0]["exit"], 1);
        assert_eq!(rows[0]["out_head"], "3");
    }

    fn tmp_repo_with_sample(root: &Path) {
        write_file(root, "ruling-scope/sample.py", 300);
    }

    #[test]
    fn ruling_lane_citation_failure_outranks_missing_read() {
        let dir = tempfile::tempdir().expect("tmp");
        tmp_repo_with_sample(dir.path());
        let mut runner = |_cmd: &str, _root: &Path| ok_run("1\n", 0);
        let err = check_ruling_evidence(
            "sample.py:99999 is the territory resolver",
            &[],
            dir.path(),
            &mut runner,
        )
        .expect_err("citation refuses");
        assert_eq!(err.kind, "citation");
        assert!(err.message.contains("Fix or drop the citation"));
    }

    #[test]
    fn ruling_lane_unmeasured_claim_refuses_with_example() {
        let dir = tempfile::tempdir().expect("tmp");
        tmp_repo_with_sample(dir.path());
        let mut runner = |_cmd: &str, _root: &Path| ok_run("1\n", 0);
        let err = check_ruling_evidence(
            "the territory resolver is 167 lines",
            &[],
            dir.path(),
            &mut runner,
        )
        .expect_err("unmeasured refuses");
        assert_eq!(err.kind, "unmeasured");
        assert!(err.message.contains("167 lines"));
        assert!(err.message.contains("Pair a zero with a control"));
        assert!(err.message.contains("fno inbox decide"));
        assert!(err.message.contains("--question-id"));
        assert!(err.message.contains("fno inbox outstanding clear"));
        assert!(err.message.contains("with no --answer"));
    }

    #[test]
    fn ruling_lane_no_claim_is_no_rows_and_no_error() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner = |_cmd: &str, _root: &Path| ok_run("1\n", 0);
        let rows = check_ruling_evidence("hold at the current head", &[], dir.path(), &mut runner)
            .expect("no claim");
        assert!(rows.is_none());
    }

    #[test]
    fn ruling_lane_reads_run_and_store_rows() {
        let dir = tempfile::tempdir().expect("tmp");
        tmp_repo_with_sample(dir.path());
        let mut runner = |_cmd: &str, _root: &Path| ok_run("17\n", 0);
        let rows = check_ruling_evidence(
            "the territory resolver is 167 lines",
            &["git diff main HEAD -- sample.py | grep -c territory".to_string()],
            dir.path(),
            &mut runner,
        )
        .expect("rows")
        .expect("some rows");
        assert_eq!(rows[0]["exit"], 0);
        assert_eq!(rows[0]["out_head"], "17");
    }

    #[test]
    fn note_lane_unread_claim_reports_without_refusing() {
        let dir = tempfile::tempdir().expect("tmp");
        tmp_repo_with_sample(dir.path());
        let mut runner = |_cmd: &str, _root: &Path| ok_run("1\n", 0);
        let (rows, claims) =
            note_evidence("the resolver is 159 lines", &[], dir.path(), &mut runner)
                .expect("warn disposition");
        assert!(rows.is_none());
        assert_eq!(claims, Some(vec!["159 lines".to_string()]));
    }

    #[test]
    fn note_lane_citation_refuses_even_quiet() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut runner = |_cmd: &str, _root: &Path| ok_run("1\n", 0);
        let err = note_evidence("see gone.py:12", &[], dir.path(), &mut runner)
            .expect_err("citation refuses");
        assert_eq!(err.kind, "citation");
    }

    #[test]
    fn note_lane_reads_store_rows() {
        let dir = tempfile::tempdir().expect("tmp");
        tmp_repo_with_sample(dir.path());
        let mut runner = |_cmd: &str, _root: &Path| ok_run("head line\n", 0);
        let (rows, claims) = note_evidence(
            "the resolver is 159 lines",
            &["head -5 sample.py".to_string()],
            dir.path(),
            &mut runner,
        )
        .expect("rows");
        assert!(claims.is_none());
        let rows = rows.expect("some rows");
        assert_eq!(rows[0]["cmd"], "head -5 sample.py");
        assert_eq!(rows[0]["out_head"], "head line");
    }

    #[test]
    fn citation_range_takes_first_line_number() {
        let dir = tempfile::tempdir().expect("tmp");
        write_file(dir.path(), "a.py", 30);
        assert!(check_citations("see a.py:29-30", dir.path()).is_empty());
        assert_eq!(
            check_citations("see a.py:31-45", dir.path()),
            vec!["a.py:31-45: the file has 30 lines"]
        );
    }

    #[test]
    fn tracked_paths_normalize_windows_separators() {
        let dir = tempfile::tempdir().expect("tmp");
        write_file(dir.path(), "src/a.py", 5);
        let tracked = tracked_files(dir.path());
        assert_eq!(tracked, vec!["src/a.py"]);
    }

    #[test]
    fn gate_request_round_trips_through_json() {
        let payload =
            r#"{"lane": "ruling", "text": "t", "reads": null, "root": "/tmp", "timeout": 20}"#;
        let req: GateRequest = serde_json::from_str(payload).expect("parses");
        assert_eq!(req.lane, "ruling");
        assert_eq!(req.reads, None);
        assert_eq!(req.timeout, Some(20));
    }

    #[test]
    fn gate_answer_maps_refusal_to_data() {
        let answer = gate_answer(Err(refusal(
            "citation",
            "a.py:1: names no tracked file".to_string(),
        )));
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["kind"], "citation");
        assert!(answer["message"]
            .as_str()
            .expect("msg")
            .contains("no tracked file"));
        let empty: Map<String, Value> = Map::new();
        assert!(empty.is_empty());
    }

    // Pure half: first-seen order, dedupe, lowercase known-set match, the
    // capitals escape never cited.
    #[test]
    fn unknown_ids_keep_first_seen_order_and_match_case_insensitively() {
        let known: HashSet<String> = ["d-aaaa0001", "d-bbbb0002"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ids = unknown_decision_ids(
            "per d-bbbb0002 then d-deadbeef, again d-bbbb0002 and D-DEADBEEF",
            &known,
        );
        assert_eq!(ids, vec!["d-deadbeef"]);
    }

    fn decision_envelope(id: &str, ts: &str) -> String {
        format!(
            "{{\"type\":\"operator_decision\",\"ts\":\"{ts}\",\"data\":{{\"decision_id\":\"{id}\",\
             \"subject\":\"s\",\"decision\":\"R.\",\"text\":\"R.\",\
             \"authority_source\":\"operator\"}}}}"
        )
    }

    // AC2-HP, AC2-ERR, AC2-EDGE, the read-error refusal and AC3-ERR, against
    // seeded FNO_HOME stores so nothing here touches the machine store. One
    // test body: set_var is process-global and cargo runs tests in parallel.
    #[test]
    fn decision_id_arm_refuses_unknown_ids_and_a_dead_store() {
        let old_home = std::env::var("FNO_HOME").ok();
        let seeded = tempfile::tempdir().expect("tmp");
        std::env::set_var("FNO_HOME", seeded.path());
        std::fs::write(
            seeded.path().join("decisions.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                decision_envelope("d-aaaa0001", "2026-09-12T00:00:00Z"),
                decision_envelope("d-eeee0005", "2026-09-05T00:00:00Z"),
                "{\"type\":\"decision_retracted\",\"ts\":\"2026-09-13T00:00:00Z\",\
                 \"data\":{\"retraction_id\":\"d-rrrr0009\",\
                 \"target_decision_id\":\"d-eeee0005\",\"reason\":\"r\"}}"
            ),
        )
        .expect("write index");
        // AC2-HP: a live id on a JSONL-only store passes.
        assert!(check_decision_citations("per d-aaaa0001").is_empty());
        // AC2-ERR: the unknown id is refused, named, with the check verb.
        let failures = check_decision_citations("per d-deadbeef");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("d-deadbeef"), "{}", failures[0]);
        assert!(
            failures[0].contains("fno backlog decisions d-deadbeef"),
            "{}",
            failures[0]
        );
        // AC2-EDGE: a retired id is known, and the capitals form escapes.
        assert!(check_decision_citations("per d-eeee0005 and D-DEADBEEF").is_empty());

        // AC3-ERR: against the seeded store the ruling lane refuses, names
        // the id, and no read runs past the refusal.
        let mut ran = false;
        let mut runner = |_cmd: &str, _root: &Path| {
            ran = true;
            ok_run("1\n", 0)
        };
        let err = check_ruling_evidence("per d-deadbeef we ruled", &[], seeded.path(), &mut runner)
            .expect_err("unknown id refuses");
        assert_eq!(err.kind, "citation");
        assert!(err.message.contains("d-deadbeef"), "{}", err.message);
        assert!(!ran, "no read ran past the refusal");

        // An empty FNO_HOME: no id never reads the store, an id names the
        // read error instead of passing.
        let empty = tempfile::tempdir().expect("tmp");
        std::env::set_var("FNO_HOME", empty.path());
        assert!(check_decision_citations("no citation in this note").is_empty());
        let failures = check_decision_citations("per d-deadbeef");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("could not be read"), "{}", failures[0]);

        match old_home {
            Some(v) => std::env::set_var("FNO_HOME", v),
            None => std::env::remove_var("FNO_HOME"),
        }
    }
}
