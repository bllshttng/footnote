//! `evals-attempt`: the native attempt-eligibility decision for eval history
//! rows. One history row describes one planned attempt; this module decides,
//! from STRUCTURED observations only, whether that attempt produced a valid
//! task grade, an infrastructure/unavailable failure, or nothing classifiable
//! (legacy). Python collects the observations and persists the verdict; the
//! decision itself never runs on reason-text matching.
//!
//! Transport-only action behind the same door as `evals-trend`:
//! `evals-attempt --row-json '<row>'` prints one verdict JSON object,
//! `evals-attempt --rows <jsonl>` prints a JSON array (line-numbered verdicts).
//!
//! Status vocabulary (shared with the Python runner and report):
//! `graded` (valid task grade, `graded` field true/false, not retryable),
//! `infrastructure` (fixture never prepared; retryable),
//! `unavailable` (worker could not run or was lost: spawn refused, timeout,
//! cancellation, gate-blocked; retryable),
//! `ungraded` (worker ran, no grader result; retryable),
//! `legacy` (no structured evidence; never guessed, never retryable).
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

const EXIT_USAGE: i32 = 2;
const EXIT_REFUSED: i32 = 3;
const USAGE: &str = "usage: fno-agents evals-attempt (--row-json '<json>' | --rows <jsonl> | --cohorts '<json>' | --cohorts-yaml <path> | --aggregate | --export-train --out <file>) [--known-ids '<json>'] [--task-ids '<json>'] [--expected-rev <sha>] [--repo <dir>]";

/// Validate a declared cohort split and (optionally) resolve membership.
///
/// *decl* is `{"train": [id], "validation": [id], "qualification": [id]}`.
/// Rules: every role list holds unique string ids and the three lists are
/// pairwise disjoint; when *known* is supplied every declared id must exist
/// in it. *interest* (optional) maps each named task id to its role or null.
/// Returns `(ok, errors, roles)`; `ok` is false whenever any rule is
/// violated, so a run or an export can refuse BEFORE spending a worker call.
pub fn validate_cohorts(
    decl: &Value,
    known: Option<&Value>,
    interest: Option<&Value>,
) -> (bool, Vec<String>, Map<String, Value>) {
    let mut errors: Vec<String> = Vec::new();
    let mut roles: Map<String, Value> = Map::new();
    let decl = match decl.as_object() {
        Some(o) => o,
        None => {
            return (
                false,
                vec!["cohort declaration must be a JSON object".into()],
                roles,
            )
        }
    };
    const ROLES: [&str; 3] = ["train", "validation", "qualification"];
    let mut seen: Vec<(String, String)> = Vec::new(); // (id, role)
    for role in ROLES {
        let list = match decl.get(role) {
            Some(Value::Array(items)) => items,
            Some(_) => {
                errors.push(format!("'{role}' must be a list of task ids"));
                continue;
            }
            None => {
                errors.push(format!("missing '{role}' list"));
                continue;
            }
        };
        for item in list {
            let Some(id) = item.as_str() else {
                errors.push(format!("'{role}' holds a non-string task id"));
                continue;
            };
            if seen.iter().any(|(i, r)| i == id && r == role) {
                errors.push(format!("task id '{id}' declared twice in '{role}'"));
                continue;
            }
            seen.push((id.to_string(), role.to_string()));
        }
    }
    for (id, role) in &seen {
        for (other_id, other_role) in &seen {
            if id == other_id && role != other_role && role < other_role {
                errors.push(format!(
                    "task id '{id}' is in both '{role}' and '{other_role}'"
                ));
            }
        }
    }
    if let Some(known) = known.and_then(Value::as_array) {
        let known: Vec<&str> = known.iter().filter_map(Value::as_str).collect();
        for (id, role) in &seen {
            if !known.contains(&id.as_str()) {
                errors.push(format!("unknown task id '{id}' in '{role}'"));
            }
        }
    }
    if let Some(interest) = interest.and_then(Value::as_array) {
        for id in interest.iter().filter_map(Value::as_str) {
            let role = seen
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, r)| Value::String(r.clone()))
                .unwrap_or(Value::Null);
            roles.insert(id.to_string(), role);
        }
    }
    (errors.is_empty(), errors, roles)
}

pub struct Verdict {
    pub status: &'static str,
    /// Some(true/false) only for a `graded` attempt.
    pub graded: Option<bool>,
    pub retryable: bool,
    /// Some only when an expected revision was supplied: whether the row's
    /// `bank_rev` matches it (a row with no bank_rev never matches).
    pub rev_match: Option<bool>,
}

impl Verdict {
    fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "graded": self.graded,
            "retryable": self.retryable,
            "rev_match": self.rev_match,
        })
    }
}

fn bool_field(obs: &Map<String, Value>, key: &str) -> Option<bool> {
    obs.get(key).and_then(Value::as_bool)
}

/// Classify one history row from its structured `obs` evidence.
pub fn classify(row: &Value, expected_rev: Option<&str>) -> Verdict {
    let obs = match row.get("obs").and_then(Value::as_object) {
        Some(o) => o,
        // No structured evidence: a pre-attempt-era row. Its `pass` boolean is
        // never retrospectively reinterpreted.
        None => {
            return Verdict {
                status: "legacy",
                graded: None,
                retryable: false,
                rev_match: None,
            }
        }
    };
    // A row with NO bank_rev has unknown provenance, not a proven mismatch:
    // only an explicit differing rev excludes it as wrong-rev.
    let row_rev = row.get("bank_rev").and_then(Value::as_str);
    let rev_match = match (expected_rev, row_rev) {
        (Some(want), Some(rev)) => Some(rev == want),
        _ => None,
    };
    let verdict = classify_obs(obs);
    Verdict {
        rev_match,
        ..verdict
    }
}

fn classify_obs(obs: &Map<String, Value>) -> Verdict {
    let graded = |passed: bool| Verdict {
        status: "graded",
        graded: Some(passed),
        retryable: false,
        rev_match: None,
    };
    let retry = |status: &'static str| Verdict {
        status,
        graded: None,
        retryable: true,
        rev_match: None,
    };
    // Fixture first: without a prepared tree nothing downstream is evidence.
    if bool_field(obs, "fixture_prepared") == Some(false) {
        return retry("infrastructure");
    }
    // The gate (or any runner-side refusal) stopped the worker before launch.
    if bool_field(obs, "gate_blocked") == Some(true) {
        return retry("unavailable");
    }
    if bool_field(obs, "worker_timed_out") == Some(true)
        || bool_field(obs, "cancelled") == Some(true)
    {
        return retry("unavailable");
    }
    if bool_field(obs, "worker_required") == Some(true) {
        match bool_field(obs, "worker_started") {
            Some(false) | None => return retry("unavailable"),
            Some(true) => {}
        }
    }
    match (
        bool_field(obs, "grader_ran"),
        bool_field(obs, "grader_passed"),
    ) {
        (Some(true), Some(passed)) => graded(passed),
        // Worker ran (or grade-only) but no grader result: an ungraded attempt.
        (Some(true), None) => retry("ungraded"),
        (Some(false), _) => retry("ungraded"),
        // Evidence present but insufficient to classify anything: legacy.
        (None, _) => Verdict {
            status: "legacy",
            graded: None,
            retryable: false,
            rev_match: None,
        },
    }
}

/// Classify every JSONL line in *path*; each verdict carries its 1-based line
/// number so the Python read side can join back to the row it came from.
pub fn classify_rows(text: &str, expected_rev: Option<&str>) -> Value {
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut v = json!({ "line": lineno + 1 });
        let verdict = match serde_json::from_str::<Value>(line) {
            Ok(row) => classify(&row, expected_rev),
            // A corrupt line is not a legacy attempt; it is unparseable and
            // reads as legacy so it can never count as a valid grade.
            Err(_) => Verdict {
                status: "legacy",
                graded: None,
                retryable: false,
                rev_match: None,
            },
        };
        if let Value::Object(map) = &mut v {
            if let Value::Object(verdict) = verdict.to_json() {
                for (k, val) in verdict {
                    map.insert(k, val);
                }
            }
        }
        out.push(v);
    }
    Value::Array(out)
}

/// Load a `cohorts.yaml` declaration: YAML -> JSON, missing role keys default
/// to empty lists, `bank_rev` must be a non-empty string. The semantic rules
/// stay in `validate_cohorts`; this is shape only.
fn load_cohorts_yaml(path: &str) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let raw: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&text).map_err(|e| format!("malformed YAML in {path}: {e}"))?;
    let mut decl: Value = serde_json::to_value(raw)
        .map_err(|e| format!("unconvertible cohort declaration in {path}: {e}"))?;
    if !decl.is_object() {
        return Err(format!("{path}: top level must be a mapping"));
    }
    match decl.get("bank_rev").and_then(Value::as_str) {
        Some(rev) if !rev.trim().is_empty() => {}
        _ => return Err(format!("{path}: 'bank_rev' must be a non-empty string")),
    }
    for role in ["train", "validation", "qualification"] {
        if decl.get(role).is_none() {
            decl[role] = json!([]);
        }
    }
    Ok(decl)
}

/// True when the bank files are unchanged from *rev* in the repo at *dir*.
/// The split pins the bank, not the whole repo; an unreadable rev voids it.
fn bank_unchanged(dir: &str, rev: &str) -> bool {
    let verify = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .current_dir(dir)
        .output();
    match verify {
        Ok(o) if o.status.success() => {}
        _ => return false,
    }
    let diff = std::process::Command::new("git")
        .args(["diff", "--quiet", rev, "HEAD", "--", "evals/bank"])
        .current_dir(dir)
        .output();
    matches!(diff, Ok(o) if o.status.success())
}

/// The qualify fold: validate the declared split (existence vs *known*,
/// uniqueness, disjointness, bank-rev currency), then fold the history rows
/// of the qualification cohort into the allowed aggregate projection. Every
/// refusal reports `{"qualified": false, "reason": ...}`; a valid fold never
/// carries a held-out prompt, trace, or per-attempt row.
fn qualify_aggregate(
    rows_text: &str,
    decl: &Value,
    known: Option<&Value>,
    repo: Option<&str>,
    expected_rev: Option<&str>,
) -> Value {
    let (ok, errors, _roles) = validate_cohorts(decl, known, None);
    if !ok {
        return json!({"qualified": false, "reason": errors.join("; ")});
    }
    let pinned_rev = decl.get("bank_rev").and_then(Value::as_str).unwrap_or("");
    if let Some(dir) = repo {
        if !bank_unchanged(dir, pinned_rev) {
            return json!({
                "qualified": false,
                "reason": format!("bank changed since the pinned rev {}", &pinned_rev[..12.min(pinned_rev.len())]),
            });
        }
    }
    let qual_ids: Vec<&str> = decl
        .get("qualification")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if qual_ids.is_empty() {
        return json!({"qualified": false, "reason": "no declared qualification cohort"});
    }
    let want_rev = expected_rev
        .map(String::from)
        .unwrap_or_else(|| pinned_rev.to_string());
    let mut by_status: Map<String, Value> = Map::new();
    let mut valid = 0usize;
    let mut passes = 0usize;
    let mut covered: Vec<String> = Vec::new();
    let mut wrong_rev = 0usize;
    let mut legacy = 0usize;
    for line in rows_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            legacy += 1;
            continue;
        };
        let Some(task_id) = row.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        if !qual_ids.contains(&task_id) {
            continue;
        }
        let verdict = classify(&row, Some(&want_rev));
        if verdict.status == "legacy" {
            legacy += 1;
            continue;
        }
        if verdict.rev_match == Some(false) {
            wrong_rev += 1;
            continue;
        }
        let entry = by_status
            .entry(verdict.status.to_string())
            .or_insert(json!(0));
        *entry = json!(entry.as_u64().unwrap_or(0) + 1);
        if verdict.status == "graded" {
            valid += 1;
            if verdict.graded == Some(true) {
                passes += 1;
            }
            if !covered.contains(&task_id.to_string()) {
                covered.push(task_id.to_string());
            }
        }
    }
    let declared = qual_ids.len();
    json!({
        "qualified": true,
        "bank_rev": pinned_rev,
        "declared_tasks": declared,
        "tasks_with_valid_grades": covered.len(),
        "missing_tasks": declared - covered.len(),
        "valid_grades": valid,
        "passes": passes,
        "attempts": Value::Object(by_status),
        "excluded": {"wrong_rev": wrong_rev, "legacy": legacy},
    })
}

/// The tuning export: validate the declared split, then write ONLY train
/// rows (each tagged `role: train`) to *out_path*. Held-out trajectories
/// never enter the file. Err text is a refusal reason for the caller.
fn export_train(
    rows_text: &str,
    decl: &Value,
    known: Option<&Value>,
    repo: Option<&str>,
    out_path: &str,
) -> Result<Value, String> {
    let (ok, errors, _roles) = validate_cohorts(decl, known, None);
    if !ok {
        return Err(errors.join("; "));
    }
    if let Some(dir) = repo {
        let pinned_rev = decl.get("bank_rev").and_then(Value::as_str).unwrap_or("");
        if !bank_unchanged(dir, pinned_rev) {
            return Err(format!(
                "cohort split is pinned to bank rev {} but the bank changed since; redeclare cohorts.yaml against the current bank",
                &pinned_rev[..12.min(pinned_rev.len())]
            ));
        }
    }
    let train: Vec<&str> = decl
        .get("train")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = String::new();
    let mut count = 0usize;
    for line in rows_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(mut row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(task_id) = row.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        if !train.contains(&task_id) {
            continue;
        }
        if let Value::Object(map) = &mut row {
            map.insert("role".into(), json!("train"));
        }
        out.push_str(&serde_json::to_string(&row).unwrap_or_default());
        out.push('\n');
        count += 1;
    }
    std::fs::write(out_path, out).map_err(|e| format!("cannot write {out_path}: {e}"))?;
    Ok(json!({"exported": count, "train_tasks": train.len(), "out": out_path}))
}

/// --- the observe door: native identity + usage readback --------------------
///
/// One attempt's lane evidence read back from the harness's own transcript
/// store, replacing the Python registry read (`runner.py::_observe_worker` /
/// `_lane_evidence`). The store is only read. A transcript that cannot be
/// found or read leaves `lane_status` at `unverified` with a reason and
/// `usage` at null, never zero (AC10-EDGE).
use crate::claude_drive::claude_projects_dir;
use std::path::{Path, PathBuf};

struct Observed {
    harness: &'static str,
    model: Option<String>,
    session_id: String,
    usage: Option<Value>,
    source: &'static str,
}

type UsageSum = (u64, u64, u64, u64); // input, output, cache_read, cache_write

fn add_usage(acc: Option<UsageSum>, next: UsageSum) -> UsageSum {
    match acc {
        None => next,
        Some((i, o, r, w)) => (i + next.0, o + next.1, r + next.2, w + next.3),
    }
}

fn usage_json(u: UsageSum) -> Value {
    json!({"input": u.0, "output": u.1, "cache_read": u.2, "cache_write": u.3})
}

/// Window on the attempt: a transcript's last write lands between the spawn
/// and the observe call, so anything older than the start (minus skew) or in
/// the future (plus skew, a pinned-clock test artifact) cannot be it. The
/// workdir match below is the real identity; this only bounds the scan.
fn in_window(mtime_s: f64, started: f64, now: f64) -> bool {
    mtime_s + 300.0 >= started && mtime_s <= now + 300.0
}

/// One claude transcript folded for identity: whether any record carries
/// `cwd` == *workdir*, the last non-synthetic assistant model, and the usage
/// summed over every assistant record (the attempt's whole transcript).
fn fold_claude_transcript(path: &Path, workdir: &str) -> (bool, Option<String>, Option<UsageSum>) {
    let Ok(f) = std::fs::File::open(path) else {
        return (false, None, None);
    };
    let mut cwd_match = false;
    let mut model: Option<String> = None;
    let mut usage: Option<UsageSum> = None;
    for line in std::io::BufRead::lines(std::io::BufReader::new(f)) {
        let Ok(line) = line else { continue };
        let Ok(rec) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if !cwd_match && rec.get("cwd").and_then(Value::as_str) == Some(workdir) {
            cwd_match = true;
        }
        if rec.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Usage reads route through the one owned parser
        // (context_window::parse_usage_record); a second reader of the
        // cache fields is the reachable-paths class the baseline holds.
        let Some(parsed) = crate::context_window::parse_usage_record(&rec) else {
            continue;
        };
        if parsed.model != "<synthetic>" && !parsed.model.is_empty() {
            model = Some(parsed.model.clone());
        }
        // ContextUsage does not carry output tokens; the pricing fold needs
        // them, so this one unguarded field is read here.
        let Some(output) = rec
            .get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
        else {
            continue;
        };
        usage = Some(add_usage(
            usage,
            (
                parsed.input_tokens,
                output,
                parsed.cache_read_input_tokens,
                parsed.cache_creation_input_tokens,
            ),
        ));
    }
    (cwd_match, model, usage)
}

fn observe_claude(root: &Path, workdir: &str, started: f64, now: f64) -> Option<Observed> {
    let mut best: Option<(
        std::time::SystemTime,
        PathBuf,
        Option<String>,
        Option<UsageSum>,
    )> = None;
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for file in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = file.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            // A dotted stem is a sibling artifact, never a transcript.
            if stem.contains('.') {
                continue;
            }
            let path = file.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Ok(mtime) = modified.duration_since(std::time::UNIX_EPOCH) else {
                continue;
            };
            if !in_window(mtime.as_secs_f64(), started, now) {
                continue;
            }
            let (cwd_match, model, usage) = fold_claude_transcript(&path, workdir);
            if !cwd_match {
                continue;
            }
            let newer = best.as_ref().map_or(true, |(b, _, _, _)| modified > *b);
            if newer {
                best = Some((modified, path, model, usage));
            }
        }
    }
    let (_, path, model, usage) = best?;
    let session_id = path.file_stem()?.to_string_lossy().into_owned();
    Some(Observed {
        harness: "claude",
        model,
        session_id,
        usage: usage.map(usage_json),
        source: "claude-transcript",
    })
}

fn observe_opencode(dbs: &[PathBuf], workdir: &str, started: f64, now: f64) -> Option<Observed> {
    const SESSION_SQL: &str = "SELECT id, time_updated FROM session \
         WHERE directory = ?1 AND parent_id IS NULL";
    const MESSAGE_SQL: &str =
        "SELECT data FROM message WHERE session_id = ?1 ORDER BY time_created";
    let mut best: Option<(i64, String)> = None;
    for db in dbs {
        let Ok(conn) =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(SESSION_SQL) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([workdir], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        }) else {
            continue;
        };
        for (id, updated_ms) in rows.flatten() {
            let mtime_s = updated_ms.max(0) as f64 / 1000.0;
            if !in_window(mtime_s, started, now) {
                continue;
            }
            if best.as_ref().map_or(true, |(b, _)| updated_ms > *b) {
                best = Some((updated_ms, id));
            }
        }
    }
    let (_, session_id) = best?;
    let mut model: Option<String> = None;
    let mut usage: Option<UsageSum> = None;
    for db in dbs {
        let Ok(conn) =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(MESSAGE_SQL) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([&session_id], |r| r.get::<_, String>(0)) else {
            continue;
        };
        for data in rows.flatten() {
            let Ok(msg) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if let Some(m) = msg.get("modelID").and_then(Value::as_str) {
                model = Some(m.to_string());
            }
            let Some(tokens) = msg.get("tokens") else {
                continue;
            };
            let num = |k: &str| tokens.get(k).and_then(Value::as_u64);
            let (Some(input), Some(output)) = (num("input"), num("output")) else {
                continue;
            };
            let cache = tokens.get("cache");
            let cache_read = cache
                .and_then(|c| c.get("read"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_write = cache
                .and_then(|c| c.get("write"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            usage = Some(add_usage(usage, (input, output, cache_read, cache_write)));
        }
    }
    Some(Observed {
        harness: "opencode",
        model,
        session_id,
        usage: usage.map(usage_json),
        source: "opencode-store",
    })
}

/// A footnote session whose header names `workdir` and whose transcript
/// changed inside the window. A transcript with an unknown non-ignorable
/// record type is unreadable, so it is skipped rather than counted as zero.
fn observe_footnote(root: &Path, workdir: &str, started: f64, now: f64) -> Option<Observed> {
    let mut best: Option<(f64, PathBuf)> = None;
    let projects = std::fs::read_dir(root).ok()?.flatten();
    for f in projects.flat_map(|p| std::fs::read_dir(p.path()).into_iter().flatten().flatten()) {
        // The record is the top-level `<fno_id>.jsonl` file; sidecar dirs
        // carry no `.jsonl` extension and child transcripts live one level
        // down, inside them.
        let path = f.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(mtime) = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
        else {
            continue;
        };
        if !in_window(mtime, started, now) || best.as_ref().is_some_and(|(b, _)| mtime <= *b) {
            continue;
        }
        // Match on the header's cwd before ranking, so a newer session in
        // another workdir cannot hide this attempt's own.
        let header_cwd = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.lines().next().map(str::to_string))
            .and_then(|l| serde_json::from_str::<Value>(&l).ok())
            .and_then(|r| r["data"]["cwd"].as_str().map(str::to_string));
        if header_cwd.as_deref() == Some(workdir) {
            best = Some((mtime, path));
        }
    }
    let (_, path) = best?;
    let records = crate::footnote_harness::transcript::read_records(&path).ok()?;
    let header = &records.first()?["data"];
    let mut model = None;
    let mut usage: Option<UsageSum> = None;
    for r in &records {
        let d = &r["data"];
        match r["type"].as_str() {
            Some("model_response") => {
                model = d["reported_model"].as_str().map(str::to_string).or(model)
            }
            Some("usage") => {
                let n = |k: &str| d[k].as_u64().unwrap_or(0);
                usage = Some(add_usage(
                    usage,
                    (
                        n("input_tokens"),
                        n("output_tokens"),
                        n("cache_read_tokens"),
                        n("cache_write_tokens"),
                    ),
                ));
            }
            _ => {}
        }
    }
    Some(Observed {
        harness: "footnote",
        model,
        session_id: header["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        usage: usage.map(usage_json),
        source: "footnote-transcript",
    })
}

/// The `{"op": "observe"}` payload's lane evidence. Mirrors the Python
/// `_lane_evidence` field-for-field and adds `usage`/`usage_source`. The
/// requested-vs-observed substitution rule is unchanged: a harness or model
/// mismatch reads `substituted`, never silently ok.
pub fn observe(payload: &Value) -> Value {
    let Some(lane) = payload.get("lane").and_then(Value::as_object) else {
        return json!({});
    };
    let get = |k: &str| {
        lane.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let (requested_lane, requested_harness, requested_model, requested_effort) =
        (get("name"), get("harness"), get("model"), get("effort"));
    let mut fields = json!({
        "requested_lane": requested_lane,
        "requested_harness": requested_harness,
        "requested_model": requested_model,
        "requested_effort": requested_effort,
        "observed_harness": Value::Null,
        "observed_model": Value::Null,
        "observed_model_basis": Value::Null,
        "observed_effort": Value::Null,
        "observed_session_id": Value::Null,
        "substituted": false,
        "lane_status": "unverified",
        "lane_reason": Value::Null,
        "usage": Value::Null,
        "usage_source": Value::Null,
    });
    let attempted = payload
        .get("attempted")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let spawned = payload
        .get("spawned")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !attempted {
        fields["lane_status"] = json!("not-applicable");
        return fields;
    }
    if !spawned {
        fields["lane_status"] = json!("unavailable");
        return fields;
    }
    let now = payload
        .get("now_epoch")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        });
    let started = payload
        .get("started_epoch")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let workdir = payload
        .get("workdir")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let observed = match requested_harness.as_str() {
        "claude" => {
            let root = payload
                .get("projects_root")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(claude_projects_dir);
            observe_claude(&root, workdir, started, now)
        }
        "opencode" => {
            let dbs: Vec<PathBuf> = payload
                .get("opencode_dbs")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_else(crate::opencode_transcript::opencode_stores);
            observe_opencode(&dbs, workdir, started, now)
        }
        "footnote" => {
            let root = payload
                .get("footnote_sessions_root")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(crate::footnote_harness::transcript::sessions_root);
            observe_footnote(&root, workdir, started, now)
        }
        _ => None,
    };
    match observed {
        None => {
            let reason = match requested_harness.as_str() {
                "claude" => "no claude transcript for this workdir".to_string(),
                "opencode" => "no opencode session for this workdir".to_string(),
                "footnote" => "no readable footnote transcript for this workdir".to_string(),
                other => format!("no transcript reader for harness '{other}'"),
            };
            fields["lane_reason"] = json!(reason);
        }
        Some(o) => {
            fields["observed_harness"] = json!(o.harness);
            fields["observed_model"] = o.model.clone().map(Value::String).unwrap_or(Value::Null);
            fields["observed_session_id"] = json!(o.session_id);
            let substituted = (!requested_harness.is_empty() && o.harness != requested_harness)
                || (!requested_model.is_empty()
                    && o.model.as_deref() != Some(requested_model.as_str()));
            fields["substituted"] = json!(substituted);
            fields["lane_status"] = json!(if substituted { "substituted" } else { "ok" });
            if let Some(u) = o.usage {
                fields["usage"] = u;
                fields["usage_source"] = json!(o.source);
            }
        }
    }
    fields
}

pub fn run_evals_attempt(args: &[String]) -> i32 {
    let mut row_json: Option<String> = None;
    let mut rows_path: Option<String> = None;
    let mut cohorts_json: Option<String> = None;
    let mut cohorts_yaml: Option<String> = None;
    let mut known_ids: Option<String> = None;
    let mut task_ids: Option<String> = None;
    let mut expected_rev: Option<String> = None;
    let mut repo: Option<String> = None;
    let mut aggregate = false;
    let mut export_train_flag = false;
    let mut out_path: Option<String> = None;
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].clone();
        i += 1;
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut value = |what: &str| -> Result<String, ()> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => match args.get(i).cloned() {
                    Some(v) => {
                        i += 1;
                        Ok(v)
                    }
                    None => {
                        eprintln!("evals-attempt: {what} needs a value");
                        Err(())
                    }
                },
            }
        };
        match name.as_str() {
            "--row-json" => match value("--row-json") {
                Ok(v) => row_json = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--rows" => match value("--rows") {
                Ok(v) => rows_path = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--cohorts" => match value("--cohorts") {
                Ok(v) => cohorts_json = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--cohorts-yaml" => match value("--cohorts-yaml") {
                Ok(v) => cohorts_yaml = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--repo" => match value("--repo") {
                Ok(v) => repo = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--out" => match value("--out") {
                Ok(v) => out_path = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--aggregate" => aggregate = true,
            "--export-train" => export_train_flag = true,
            "--known-ids" => match value("--known-ids") {
                Ok(v) => known_ids = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--task-ids" => match value("--task-ids") {
                Ok(v) => task_ids = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--expected-rev" => match value("--expected-rev") {
                Ok(v) => expected_rev = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            _ => {
                eprintln!("evals-attempt: unknown flag {name}");
                eprintln!("{USAGE}");
                return EXIT_USAGE;
            }
        }
    }
    if args.is_empty() {
        // stdin classify: a JSON payload on stdin (the verb_call shape),
        // `{"op": "classify", "row": {...}, "expected_rev": "..."}`.
        use std::io::Read;

        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_ok() {
            if let Ok(v) = serde_json::from_str::<Value>(&buf) {
                if v.get("op").and_then(Value::as_str) == Some("observe") {
                    println!(
                        "{}",
                        serde_json::to_string(&observe(&v)).unwrap_or_default()
                    );
                    return 0;
                }
                if v.get("op").and_then(Value::as_str) == Some("classify") {
                    if let Some(row) = v.get("row") {
                        let rev = v.get("expected_rev").and_then(Value::as_str);
                        println!(
                            "{}",
                            serde_json::to_string(&classify(row, rev).to_json())
                                .unwrap_or_default()
                        );
                        return 0;
                    }
                }
            }
        }
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    }
    let mut mode_count = 0usize;
    if row_json.is_some() {
        mode_count += 1;
    }
    if rows_path.is_some() && !aggregate && !export_train_flag {
        mode_count += 1;
    }
    if (cohorts_json.is_some() || cohorts_yaml.is_some()) && !aggregate && !export_train_flag {
        mode_count += 1;
    }
    if aggregate {
        mode_count += 1;
    }
    if export_train_flag {
        mode_count += 1;
    }
    if mode_count != 1 {
        eprintln!("evals-attempt: exactly one mode is required");
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    }
    if let Some(raw) = row_json {
        let Ok(row) = serde_json::from_str::<Value>(&raw) else {
            eprintln!("evals-attempt: --row-json is not valid JSON");
            return EXIT_USAGE;
        };
        println!(
            "{}",
            serde_json::to_string(&classify(&row, expected_rev.as_deref()).to_json())
                .unwrap_or_default()
        );
        return 0;
    }
    let parse_json_flag = |raw: &Option<String>, what: &str| -> Option<Value> {
        let raw = raw.as_deref()?;
        match serde_json::from_str::<Value>(raw) {
            Ok(v) => Some(v),
            Err(_) => {
                eprintln!("evals-attempt: {what} is not valid JSON");
                None
            }
        }
    };
    let known = parse_json_flag(&known_ids, "--known-ids");
    let interest = parse_json_flag(&task_ids, "--task-ids");
    if (known_ids.is_some() && known.is_none()) || (task_ids.is_some() && interest.is_none()) {
        return EXIT_USAGE;
    }
    let decl: Option<Value> = if let Some(ref raw) = cohorts_json {
        serde_json::from_str::<Value>(&raw).ok()
    } else if let Some(path) = cohorts_yaml.as_deref() {
        match load_cohorts_yaml(path) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("evals-attempt: {e}");
                return EXIT_USAGE;
            }
        }
    } else {
        None
    };
    if (cohorts_json.is_some() || cohorts_yaml.is_some()) && decl.is_none() {
        // An unparseable declaration is a usage error, never a silent pass.
        if cohorts_json.is_some() {
            eprintln!("evals-attempt: --cohorts is not valid JSON");
        }
        return EXIT_USAGE;
    }
    if aggregate || export_train_flag {
        let Some(decl) = decl.clone() else {
            eprintln!("evals-attempt: aggregate/export-train need a --cohorts-yaml declaration");
            eprintln!("{USAGE}");
            return EXIT_USAGE;
        };
        let Some(path) = rows_path.clone() else {
            eprintln!("evals-attempt: aggregate/export-train need --rows");
            eprintln!("{USAGE}");
            return EXIT_USAGE;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("evals-attempt: cannot read {path}");
            return EXIT_USAGE;
        };
        if export_train_flag {
            let Some(out) = out_path.clone() else {
                eprintln!("evals-attempt: --export-train needs --out");
                return EXIT_USAGE;
            };
            match export_train(&text, &decl, known.as_ref(), repo.as_deref(), &out) {
                Ok(v) => {
                    println!("{}", serde_json::to_string(&v).unwrap_or_default());
                    0
                }
                Err(e) => {
                    // A refusal text, echoed for the caller to map to its exit.
                    println!(
                        "{}",
                        serde_json::to_string(&json!({"error": e})).unwrap_or_default()
                    );
                    EXIT_REFUSED
                }
            }
        } else {
            println!(
                "{}",
                serde_json::to_string(&qualify_aggregate(
                    &text,
                    &decl,
                    known.as_ref(),
                    repo.as_deref(),
                    expected_rev.as_deref(),
                ))
                .unwrap_or_default()
            );
            0
        }
    } else if let Some(decl) = decl {
        let (ok, errors, roles) = validate_cohorts(&decl, known.as_ref(), interest.as_ref());
        let mut bank_unchanged_field: Value = json!(null);
        if ok {
            if let Some(dir) = repo.as_deref() {
                let pinned = decl.get("bank_rev").and_then(Value::as_str).unwrap_or("");
                bank_unchanged_field = json!(bank_unchanged(dir, pinned));
            }
        }
        println!(
            "{}",
            serde_json::to_string(&json!({
                "ok": ok,
                "errors": errors,
                "roles": Value::Object(roles),
                "bank_unchanged": bank_unchanged_field,
            }))
            .unwrap_or_default()
        );
        0
    } else {
        let path = rows_path.unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("evals-attempt: cannot read {path}");
            return EXIT_USAGE;
        };
        println!(
            "{}",
            serde_json::to_string(&classify_rows(&text, expected_rev.as_deref()))
                .unwrap_or_default()
        );
        0
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_observe;
