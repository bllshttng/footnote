use chrono::DateTime;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Experiment {
    schema_version: u32,
    kind: Kind,
    cohort_id: String,
    declared_at: String,
    intervention: String,
    baseline_revision: String,
    candidate_revision: String,
    runtime: Runtime,
    repetitions: u32,
    tasks: Vec<Task>,
    runs: Vec<Run>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Calibration,
    Experiment,
}

#[derive(Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Runtime {
    harness: String,
    model: String,
    effort: String,
    budget_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
    input_sha256: String,
    expected: Value,
}

#[derive(Clone, Copy, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Arm {
    Baseline,
    Candidate,
}

impl Arm {
    fn index(self) -> usize {
        match self {
            Self::Baseline => 0,
            Self::Candidate => 1,
        }
    }
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Termination {
    Completed,
    TaskFailed,
    Abandoned,
    InfrastructureError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    arm: Arm,
    task_id: String,
    repeat: u32,
    attempt_id: String,
    revision: String,
    runtime: Runtime,
    started_at: String,
    termination: Termination,
    exit_code: i32,
    input: Artifact,
    result: Option<Artifact>,
    capture_complete: bool,
    scratch_dir: String,
    scratch: Vec<Artifact>,
}

#[derive(Default)]
struct Metrics {
    attempts: usize,
    completed: usize,
    wrappers: usize,
    attempts_with_wrappers: usize,
}

impl Metrics {
    fn record(&mut self, complete: bool, wrappers: usize) {
        self.attempts += 1;
        self.completed += usize::from(complete);
        self.wrappers += wrappers;
        self.attempts_with_wrappers += usize::from(wrappers > 0);
    }

    fn json(&self) -> Value {
        json!({
            "attempts": self.attempts, "completed": self.completed,
            "completion_rate": self.completed as f64 / self.attempts as f64,
            "wrappers": self.wrappers,
            "wrappers_per_attempt": self.wrappers as f64 / self.attempts as f64,
            "attempts_with_wrappers": self.attempts_with_wrappers,
            "wrapper_attempt_rate": self.attempts_with_wrappers as f64 / self.attempts as f64
        })
    }
}

fn require(condition: bool, reason: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(reason.to_string())
    }
}

fn valid_hash(s: &str, size: usize) -> bool {
    s.len() == size
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn checked_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    require(!relative.is_empty(), "empty evidence path")?;
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(format!("evidence path must stay relative: {relative}"));
        };
        path.push(name);
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| format!("unreadable evidence path {relative}: {e}"))?;
        require(
            !meta.file_type().is_symlink(),
            "symlink evidence path refused",
        )?;
    }
    Ok(path)
}

fn read_artifact(root: &Path, artifact: &Artifact) -> Result<Vec<u8>, String> {
    require(valid_hash(&artifact.sha256, 64), "invalid artifact hash")?;
    let path = checked_path(root, &artifact.path)?;
    require(path.is_file(), "artifact path is not a regular file")?;
    let bytes = fs::read(&path).map_err(|e| format!("artifact read failed: {e}"))?;
    require(
        format!("{:x}", Sha256::digest(&bytes)) == artifact.sha256,
        &format!("artifact hash mismatch: {}", artifact.path),
    )?;
    Ok(bytes)
}

fn inventory(dir: &Path, root: &Path, found: &mut BTreeSet<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("scratch inventory unreadable: {e}"))? {
        let entry = entry.map_err(|e| format!("scratch entry unreadable: {e}"))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("scratch type unreadable: {e}"))?;
        require(!kind.is_symlink(), "symlink in scratch inventory")?;
        if kind.is_dir() {
            inventory(&path, root, found)?;
        } else {
            require(kind.is_file(), "non-regular scratch inventory entry")?;
            let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
            let name = relative.to_str().ok_or("non-UTF8 scratch path")?;
            found.insert(name.replace(std::path::MAIN_SEPARATOR, "/"));
        }
    }
    Ok(())
}

fn evaluate(root: &Path, manifest: &Value) -> Result<Value, String> {
    let e: Experiment = serde_json::from_value(manifest.clone())
        .map_err(|err| format!("invalid experiment JSON: {err}"))?;
    require(e.schema_version == 1, "unsupported schema version")?;
    require(!e.cohort_id.trim().is_empty(), "missing cohort identity")?;
    require(!e.intervention.trim().is_empty(), "missing intervention")?;
    require(
        e.repetitions >= 3,
        "at least three repetitions are required",
    )?;
    require(!e.tasks.is_empty(), "no declared tasks")?;
    require(
        valid_hash(&e.baseline_revision, 40)
            && valid_hash(&e.candidate_revision, 40)
            && e.baseline_revision != e.candidate_revision,
        "baseline and candidate need different full revision SHAs",
    )?;
    require(
        !e.runtime.harness.trim().is_empty()
            && !e.runtime.model.trim().is_empty()
            && !e.runtime.effort.trim().is_empty()
            && e.runtime.budget_seconds > 0,
        "incomplete runtime declaration",
    )?;
    let declared = DateTime::parse_from_rfc3339(&e.declared_at)
        .map_err(|_| "invalid declaration timestamp")?;
    require(
        fno_agents::scratch::classify("json.loads(raw)") == Some("fno_wrap_json"),
        "scratch classifier positive control failed",
    )?;
    let mut task_map = BTreeMap::new();
    for task in &e.tasks {
        require(!task.id.trim().is_empty(), "empty task identity")?;
        require(
            valid_hash(&task.input_sha256, 64),
            "invalid task input hash",
        )?;
        require(
            task_map.insert(task.id.as_str(), task).is_none(),
            "duplicate task identity",
        )?;
    }
    let mut keys = HashSet::new();
    let mut attempts = HashSet::new();
    for run in &e.runs {
        require(
            task_map.contains_key(run.task_id.as_str()),
            "undeclared task in capture",
        )?;
        require(
            run.repeat > 0 && run.repeat <= e.repetitions,
            "undeclared repetition",
        )?;
        require(
            keys.insert((run.arm, run.task_id.as_str(), run.repeat)),
            "duplicate paired attempt",
        )?;
        require(
            !run.attempt_id.trim().is_empty(),
            "missing attempt identity",
        )?;
        require(
            attempts.insert(run.attempt_id.as_str()),
            "duplicate attempt identity",
        )?;
    }
    let expected = e
        .tasks
        .len()
        .checked_mul(e.repetitions as usize)
        .and_then(|n| n.checked_mul(2))
        .ok_or("attempt count overflow")?;
    require(
        e.runs.len() == expected,
        &format!(
            "missing attempt: expected {expected}, captured {}",
            e.runs.len()
        ),
    )?;
    let mut roots: Vec<PathBuf> = vec![];
    let mut results = HashSet::new();
    let mut total: [Metrics; 2] = Default::default();
    let mut per_task: BTreeMap<&str, [Metrics; 2]> = BTreeMap::new();
    for run in &e.runs {
        require(
            run.runtime == e.runtime,
            "observed runtime differs from declaration",
        )?;
        let revision = if run.arm == Arm::Baseline {
            &e.baseline_revision
        } else {
            &e.candidate_revision
        };
        require(
            &run.revision == revision,
            "observed revision differs from declared arm",
        )?;
        let started =
            DateTime::parse_from_rfc3339(&run.started_at).map_err(|_| "invalid run timestamp")?;
        require(started > declared, "run must start after declaration")?;
        require(run.capture_complete, "incomplete scratch capture")?;
        require(
            run.termination != Termination::InfrastructureError,
            "infrastructure error makes comparison insufficient",
        )?;
        require(
            run.termination != Termination::Completed || run.exit_code == 0,
            "completed termination has nonzero exit code",
        )?;
        let task = task_map[run.task_id.as_str()];
        let input = read_artifact(root, &run.input)?;
        require(
            format!("{:x}", Sha256::digest(&input)) == task.input_sha256,
            "captured input hash differs from task",
        )?;
        let scratch_root = checked_path(root, &run.scratch_dir)?;
        require(
            scratch_root.is_dir(),
            "scratch inventory root is not a directory",
        )?;
        require(
            !roots
                .iter()
                .any(|r| r.starts_with(&scratch_root) || scratch_root.starts_with(r)),
            "reused or overlapping scratch evidence",
        )?;
        roots.push(scratch_root.clone());
        let mut actual = BTreeSet::new();
        inventory(&scratch_root, &scratch_root, &mut actual)?;
        let mut declared_files = BTreeSet::new();
        let mut wrappers = 0;
        for file in &run.scratch {
            require(
                declared_files.insert(file.path.replace(std::path::MAIN_SEPARATOR, "/")),
                "duplicate scratch inventory path",
            )?;
            let bytes = read_artifact(&scratch_root, file)?;
            if matches!(
                Path::new(&file.path).extension().and_then(|v| v.to_str()),
                Some("py" | "sh")
            ) {
                let text = std::str::from_utf8(&bytes).map_err(|_| "non-UTF8 scratch source")?;
                wrappers +=
                    usize::from(fno_agents::scratch::classify(text) == Some("fno_wrap_json"));
            }
        }
        require(
            actual == declared_files,
            "scratch inventory differs from captured files",
        )?;
        let result = if let Some(a) = &run.result {
            let bytes = read_artifact(root, a)?;
            let path = checked_path(root, &a.path)?;
            require(results.insert(path), "reused result artifact")?;
            Some(
                serde_json::from_slice::<Value>(&bytes)
                    .map_err(|e| format!("result is not valid JSON: {e}"))?,
            )
        } else {
            None
        };
        require(
            run.termination != Termination::Completed || result.is_some(),
            "completed attempt is missing result evidence",
        )?;
        let completed =
            run.termination == Termination::Completed && result.as_ref() == Some(&task.expected);
        total[run.arm.index()].record(completed, wrappers);
        per_task.entry(run.task_id.as_str()).or_default()[run.arm.index()]
            .record(completed, wrappers);
    }
    let baseline = &total[0];
    let candidate = &total[1];
    let (comparison, reason) = if per_task
        .values()
        .any(|m| m[1].completed < m[0].completed || m[1].wrappers > m[0].wrappers)
    {
        (
            "regressed",
            "a task lost successful completions or gained wrappers",
        )
    } else if baseline.completed == 0 {
        ("insufficient", "no successful baseline attempt")
    } else if candidate.completed != candidate.attempts {
        ("insufficient", "candidate completion floor is 100 percent")
    } else if baseline.wrappers == 0 {
        ("insufficient", "no baseline wrapper signal")
    } else if candidate.wrappers < baseline.wrappers {
        (
            "improved",
            "fewer wrappers with all candidate outcomes mechanically verified",
        )
    } else {
        ("unchanged", "wrapper count did not fall")
    };
    let calibration = e.kind == Kind::Calibration;
    let verdict = if calibration {
        "calibration_only"
    } else if comparison == "improved" {
        "observed_improvement"
    } else {
        comparison
    };
    Ok(json!({
        "schema_version":1,"cohort_id":e.cohort_id,
        "kind":if calibration {"calibration"} else {"experiment"},
        "verdict":verdict,"comparison":comparison,"reason":reason,
        "live_benefit_proven":false,
        "scope":"supplied frozen artifact captures; no causal or business-value claim",
        "baseline_revision":e.baseline_revision,"candidate_revision":e.candidate_revision,
        "classifier_sha256":format!("{:x}", Sha256::digest(include_bytes!("../src/scratch.rs"))),
        "evaluator_sha256":format!("{:x}", Sha256::digest(include_bytes!("wrapper_outcome_eval.rs"))),
        "baseline":baseline.json(),"candidate":candidate.json(),
        "tasks":per_task.iter().map(|(id,m)| json!({"task_id":id,"baseline":m[0].json(),"candidate":m[1].json()})).collect::<Vec<_>>()
    }))
}

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let outcome = (|| {
        require(
            args.len() == 1,
            "usage: wrapper_outcome_eval <experiment.json>",
        )?;
        let path = PathBuf::from(&args[0]);
        let root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(|e| format!("experiment directory unavailable: {e}"))?;
        let bytes = fs::read(&path).map_err(|e| format!("experiment read failed: {e}"))?;
        let manifest =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid experiment JSON: {e}"))?;
        evaluate(&root, &manifest)
    })();
    let (report, code) = match outcome {
        Ok(report) => {
            let code = if report["comparison"] == "insufficient" {
                2
            } else if report["verdict"] == "calibration_only" || report["comparison"] == "improved"
            {
                0
            } else {
                1
            };
            (report, code)
        }
        Err(reason) => (
            json!({"schema_version":1,"verdict":"insufficient","reason":reason,"live_benefit_proven":false}),
            2,
        ),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("report contains only JSON values")
    );
    std::process::exit(code);
}

#[cfg(test)]
#[path = "wrapper_outcome_eval/tests.rs"]
mod tests;
