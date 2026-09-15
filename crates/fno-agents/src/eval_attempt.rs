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
const USAGE: &str = "usage: fno-agents evals-attempt (--row-json '<json>' | --rows <jsonl> | --cohorts '<json>' [--known-ids '<json>'] [--task-ids '<json>']) [--expected-rev <sha>]";

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
    let rev_match =
        expected_rev.map(|want| row.get("bank_rev").and_then(Value::as_str) == Some(want));
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

pub fn run_evals_attempt(args: &[String]) -> i32 {
    let mut row_json: Option<String> = None;
    let mut rows_path: Option<String> = None;
    let mut cohorts_json: Option<String> = None;
    let mut known_ids: Option<String> = None;
    let mut task_ids: Option<String> = None;
    let mut expected_rev: Option<String> = None;
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
    let has_row = row_json.is_some();
    let has_rows = rows_path.is_some();
    let has_cohorts = cohorts_json.is_some();
    if has_row as u8 + has_rows as u8 + has_cohorts as u8 != 1 {
        eprintln!("evals-attempt: exactly one of --row-json, --rows or --cohorts is required");
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
    if let Some(raw) = cohorts_json {
        let Ok(decl) = serde_json::from_str::<Value>(&raw) else {
            eprintln!("evals-attempt: --cohorts is not valid JSON");
            return EXIT_USAGE;
        };
        let parse = |raw: &Option<String>, what: &str| -> Option<Value> {
            let raw = raw.as_deref()?;
            match serde_json::from_str::<Value>(raw) {
                Ok(v) => Some(v),
                Err(_) => {
                    eprintln!("evals-attempt: {what} is not valid JSON");
                    None
                }
            }
        };
        let known = parse(&known_ids, "--known-ids");
        let interest = parse(&task_ids, "--task-ids");
        if (known_ids.is_some() && known.is_none()) || (task_ids.is_some() && interest.is_none()) {
            return EXIT_USAGE;
        }
        let (ok, errors, roles) = validate_cohorts(&decl, known.as_ref(), interest.as_ref());
        println!(
            "{}",
            serde_json::to_string(&json!({
                "ok": ok,
                "errors": errors,
                "roles": Value::Object(roles),
            }))
            .unwrap_or_default()
        );
        return 0;
    }
    let path = rows_path.unwrap_or_default();
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("evals-attempt: cannot read {path}");
        return EXIT_USAGE;
    };
    println!(
        "{}",
        serde_json::to_string(&classify_rows(&text, expected_rev.as_deref())).unwrap_or_default()
    );
    0
}

#[cfg(test)]
mod tests;
