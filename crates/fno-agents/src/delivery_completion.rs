//! Strict process boundary for explicitly activated generic delivery.

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const EVALUATOR_COMMAND_ID: &str = "fno delivery evaluate --json";

#[derive(Debug)]
pub enum DeliveryCompletion {
    Inactive,
    Passed {
        fact_revision: String,
        verdict: Value,
    },
    Nonpassing(String),
}

#[derive(Debug)]
pub struct DeliveryReceipt {
    pub node: String,
    pub attempt: String,
    pub fact_revision: String,
    pub uri: String,
}

/// Exact current-read conjunction that authorizes the legacy PR terminal.
pub fn pr_passes(open: bool, ci: bool, reviewed: bool, head: bool, probes: bool) -> bool {
    open && ci && reviewed && head && probes
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: String,
    status: String,
    fact_revision: Value,
    verdict: Value,
    diagnostics: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Verdict {
    evaluator_version: String,
    #[serde(default, rename = "session_id")]
    _session_id: Option<String>,
    work_order_node_id: String,
    attempt_id: String,
    aggregate: String,
    fact_revision: Value,
    required_requirements: Vec<RequirementBinding>,
    requirements: Vec<Requirement>,
    diagnostics: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequirementBinding {
    deliverable_id: String,
    evidence_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Requirement {
    deliverable_id: String,
    evidence_id: String,
    subject_kind: String,
    subject_id: String,
    result: String,
    producers: Vec<String>,
    source_revisions: Vec<String>,
    diagnostics: Vec<String>,
}

pub fn evaluate(fno_bin: &str, cwd: &Path, plan_path: &Path, events: &Path) -> DeliveryCompletion {
    match activation(plan_path) {
        Activation::Inactive => return DeliveryCompletion::Inactive,
        Activation::Invalid(reason) => return DeliveryCompletion::Nonpassing(reason),
        Activation::Active => {}
    }
    let output = match Command::new(fno_bin)
        .args(["delivery", "evaluate", "--json", "--plan-path"])
        .arg(plan_path)
        .arg("--events")
        .arg(events)
        .current_dir(cwd)
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return DeliveryCompletion::Nonpassing(format!(
                "delivery evaluator exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
        Err(error) => {
            return DeliveryCompletion::Nonpassing(format!(
                "delivery evaluator could not start: {error}"
            ))
        }
    };
    parse_response(&output.stdout)
}

pub fn evaluate_manifest(cwd: &Path, plan_path: Option<&str>, events: &Path) -> DeliveryCompletion {
    let Some(plan_path) = plan_path else {
        return DeliveryCompletion::Inactive;
    };
    let plan_path = PathBuf::from(plan_path);
    let plan_path = if plan_path.is_absolute() {
        plan_path
    } else {
        cwd.join(plan_path)
    };
    let fno_bin = std::env::var("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|_| "fno".into());
    evaluate(&fno_bin, cwd, &plan_path, events)
}

#[allow(clippy::too_many_arguments)]
pub fn gate_output(
    completion: &DeliveryCompletion,
    promise: bool,
    project_events: &Path,
    global_events: &Path,
    session_id: &str,
    intent_source: &str,
    fingerprint: &str,
    fires: u64,
) -> Option<String> {
    let DeliveryCompletion::Passed {
        fact_revision,
        verdict,
    } = completion
    else {
        return match completion {
            DeliveryCompletion::Inactive => None,
            DeliveryCompletion::Nonpassing(reason) => Some(crate::completion_output::allow_output(
                "block",
                None,
                &format!("generic delivery undeterminable: {reason}"),
                fires,
                Some(fingerprint.into()),
            )),
            DeliveryCompletion::Passed { .. } => unreachable!(),
        };
    };
    if !promise {
        return Some(crate::completion_output::allow_output(
            "block",
            None,
            "generic delivery requires a promise",
            fires,
            Some(fingerprint.into()),
        ));
    }
    if !emit_verdict(project_events, global_events, session_id, verdict) {
        return Some(crate::completion_output::allow_output(
            "block",
            None,
            "generic delivery verdict could not be durably recorded",
            fires,
            Some(fingerprint.into()),
        ));
    }
    let terminal_message =
        format!("generic delivery passed via {EVALUATOR_COMMAND_ID} at {fact_revision}");
    crate::loopcheck::emit_to_both(
        project_events,
        global_events,
        "termination",
        serde_json::json!({
            "session_id": session_id, "reason": "DoneDelivery",
            "message": terminal_message
        }),
    );
    crate::loopcheck::emit_to_both(
        project_events,
        global_events,
        "loop_check",
        serde_json::json!({
            "session_id": session_id, "decision": "allow", "intent": "promise",
            "intent_source": intent_source, "fingerprint": fingerprint,
            "fires": fires, "fact_revision": fact_revision
        }),
    );
    Some(crate::completion_output::allow_output(
        "allow",
        Some(crate::loopcheck::TerminationReason::DoneDelivery),
        &terminal_message,
        fires,
        Some(fingerprint.into()),
    ))
}

pub fn emit_verdict(
    project_events: &Path,
    global_events: &Path,
    session_id: &str,
    verdict: &Value,
) -> bool {
    let mut data = verdict.clone();
    let Some(mapping) = data.as_object_mut() else {
        return false;
    };
    mapping.insert("session_id".into(), Value::String(session_id.into()));
    let envelope = serde_json::json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "type": "delivery_verdict_evaluated",
        "source": "target",
        "data": data,
    });
    let Ok(mut line) = serde_json::to_vec(&envelope) else {
        return false;
    };
    line.push(b'\n');
    let durable = append(project_events, &line);
    if project_events != global_events {
        let _ = append(global_events, &line);
    }
    durable
}

fn append(path: &Path, line: &[u8]) -> bool {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut file) => match file.write_all(line) {
            Ok(()) => true,
            Err(error) => {
                eprintln!(
                    "delivery completion: append {} failed: {error}",
                    path.display()
                );
                false
            }
        },
        Err(error) => {
            eprintln!(
                "delivery completion: open {} failed: {error}",
                path.display()
            );
            false
        }
    }
}

pub fn selected_receipt(
    events: &Path,
    expected_node: Option<&str>,
    session_id: &str,
) -> Option<DeliveryReceipt> {
    let content = std::fs::read_to_string(events).ok()?;
    content.lines().rev().find_map(|line| {
        let event: Value = serde_json::from_str(line).ok()?;
        if event.get("type")?.as_str()? != "delivery_verdict_evaluated"
            || event.get("source")?.as_str()? != "target"
        {
            return None;
        }
        let data = event.get("data")?;
        if data.get("session_id")?.as_str()? != session_id {
            return None;
        }
        let revision = data.get("fact_revision")?.as_str()?;
        let verdict = strict_passed_verdict(data, revision)?;
        let node = verdict.work_order_node_id.as_str();
        if expected_node.is_some_and(|expected| expected != node) {
            return None;
        }
        let attempt = verdict.attempt_id.as_str();
        Some(DeliveryReceipt {
            node: node.into(),
            attempt: attempt.into(),
            fact_revision: revision.into(),
            uri: format!("fno-delivery://{node}/{attempt}/{revision}"),
        })
    })
}

pub fn write_receipt_handoff(
    dir: &Path,
    session_id: &str,
    receipt: &DeliveryReceipt,
) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let prefix: String = session_id.chars().take(16).collect();
    let path = dir.join(format!(
        "{}-{prefix}-delivery.md",
        chrono::Utc::now().format("%Y-%m-%d")
    ));
    let body = format!(
        "# generic delivery receipt\n\n- session: `{session_id}`\n- node: `{}`\n- attempt: `{}`\n- fact_revision: `{}`\n- receipt: {}\n",
        receipt.node, receipt.attempt, receipt.fact_revision, receipt.uri
    );
    std::fs::write(&path, body).map_err(|error| error.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

enum Activation {
    Inactive,
    Active,
    Invalid(String),
}

fn activation(plan_path: &Path) -> Activation {
    let text = match std::fs::read_to_string(plan_path) {
        Ok(text) => text,
        Err(error) => {
            return Activation::Invalid(format!("delivery plan could not be read: {error}"))
        }
    };
    let Some(rest) = text.strip_prefix("---") else {
        return Activation::Inactive;
    };
    let Some(end) = rest.find("\n---") else {
        return Activation::Invalid("delivery plan frontmatter is unterminated".into());
    };
    let frontmatter = match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&rest[..end]) {
        Ok(frontmatter) => frontmatter,
        Err(error) => {
            return Activation::Invalid(format!("delivery plan frontmatter is malformed: {error}"))
        }
    };
    let Some(mapping) = frontmatter.as_mapping() else {
        return Activation::Invalid("delivery plan frontmatter is not a mapping".into());
    };
    if mapping
        .get(serde_yaml_ng::Value::from("completion"))
        .and_then(serde_yaml_ng::Value::as_str)
        != Some("delivery")
    {
        return Activation::Inactive;
    }
    let company_key = serde_yaml_ng::Value::from("company_work");
    let Some(company_value) = mapping.get(&company_key) else {
        return Activation::Inactive;
    };
    let Some(company) = company_value.as_mapping() else {
        return Activation::Invalid("delivery plan company_work is not a mapping".into());
    };
    let work_order_key = serde_yaml_ng::Value::from("work_order");
    let work_order_valid = match company.get(&work_order_key) {
        None => false,
        Some(value) => {
            let Some(work_order) = value.as_mapping() else {
                return Activation::Invalid(
                    "delivery plan company_work.work_order is not a mapping".into(),
                );
            };
            let node = work_order.get(serde_yaml_ng::Value::from("node_id"));
            let attempt = work_order.get(serde_yaml_ng::Value::from("attempt_id"));
            match (node, attempt) {
                (None, _) | (_, None) => {
                    return Activation::Invalid(
                        "delivery plan work_order is missing node_id or attempt_id".into(),
                    );
                }
                (Some(node), Some(attempt)) => {
                    let Some(node) = node.as_str() else {
                        return Activation::Invalid(
                            "delivery plan work_order.node_id is not a string".into(),
                        );
                    };
                    let Some(attempt) = attempt.as_str() else {
                        return Activation::Invalid(
                            "delivery plan work_order.attempt_id is not a string".into(),
                        );
                    };
                    if node.trim().is_empty() || attempt.trim().is_empty() {
                        return Activation::Invalid(
                            "delivery plan work_order node_id or attempt_id is empty".into(),
                        );
                    }
                    true
                }
            }
        }
    };
    let deliverables_key = serde_yaml_ng::Value::from("deliverables");
    let Some(deliverables_value) = company.get(&deliverables_key) else {
        return Activation::Inactive;
    };
    let Some(deliverables) = deliverables_value.as_sequence() else {
        return Activation::Invalid("delivery plan company_work.deliverables is not a list".into());
    };
    let mut has_required = false;
    for deliverable in deliverables {
        let Some(deliverable) = deliverable.as_mapping() else {
            return Activation::Invalid("delivery plan deliverable is not a mapping".into());
        };
        let required_key = serde_yaml_ng::Value::from("required_evidence_ids");
        let Some(required_value) = deliverable.get(&required_key) else {
            continue;
        };
        let Some(required_ids) = required_value.as_sequence() else {
            return Activation::Invalid("delivery plan required_evidence_ids is not a list".into());
        };
        for evidence_id in required_ids {
            let Some(evidence_id) = evidence_id.as_str() else {
                return Activation::Invalid(
                    "delivery plan required evidence id is not a string".into(),
                );
            };
            if evidence_id.trim().is_empty() {
                return Activation::Invalid("delivery plan required evidence id is empty".into());
            }
            has_required = true;
        }
    }
    if work_order_valid && !deliverables.is_empty() && has_required {
        Activation::Active
    } else {
        Activation::Inactive
    }
}

fn parse_response(raw: &[u8]) -> DeliveryCompletion {
    let response: Response = match serde_json::from_slice(raw) {
        Ok(response) => response,
        Err(error) => {
            return DeliveryCompletion::Nonpassing(format!(
                "malformed delivery evaluator response: {error}"
            ))
        }
    };
    if response.version != "delivery-evaluate-response.v1" {
        return DeliveryCompletion::Nonpassing(
            "unknown delivery evaluator response version".into(),
        );
    }
    match response.status.as_str() {
        "inactive" if response.fact_revision.is_null() && response.verdict.is_null() => {
            DeliveryCompletion::Inactive
        }
        "undeterminable" if response.fact_revision.is_null() && response.verdict.is_null() => {
            DeliveryCompletion::Nonpassing(response.diagnostics.join("; "))
        }
        "evaluated" => parse_evaluated(response),
        _ => DeliveryCompletion::Nonpassing("invalid delivery evaluator response state".into()),
    }
}

fn parse_evaluated(response: Response) -> DeliveryCompletion {
    let Some(fact_revision) = response.fact_revision.as_str() else {
        return DeliveryCompletion::Nonpassing("evaluated response has no fact revision".into());
    };
    if strict_passed_verdict(&response.verdict, fact_revision).is_none() {
        return DeliveryCompletion::Nonpassing(
            "delivery verdict is nonpassing or incomplete".into(),
        );
    }
    DeliveryCompletion::Passed {
        fact_revision: fact_revision.to_string(),
        verdict: response.verdict,
    }
}

fn strict_passed_verdict(value: &Value, fact_revision: &str) -> Option<Verdict> {
    let verdict: Verdict = serde_json::from_value(value.clone()).ok()?;
    let required: Vec<_> = verdict
        .required_requirements
        .iter()
        .map(|item| (item.deliverable_id.as_str(), item.evidence_id.as_str()))
        .collect();
    let rows: Vec<_> = verdict
        .requirements
        .iter()
        .map(|item| (item.deliverable_id.as_str(), item.evidence_id.as_str()))
        .collect();
    let unique: HashSet<_> = required.iter().copied().collect();
    let complete = verdict.evaluator_version == "delivery-evaluator.v1"
        && !verdict.work_order_node_id.is_empty()
        && !verdict.attempt_id.is_empty()
        && verdict.aggregate == "passed"
        && verdict.fact_revision.as_str() == Some(fact_revision)
        && !required.is_empty()
        && required == rows
        && unique.len() == required.len()
        && verdict.requirements.iter().all(valid_passed_requirement)
        && verdict.diagnostics.is_empty();
    complete.then_some(verdict)
}

fn valid_passed_requirement(requirement: &Requirement) -> bool {
    const SUBJECTS: &[&str] = &[
        "artifact",
        "review",
        "approval",
        "probe",
        "acknowledgment",
        "deliverable",
        "effect",
    ];
    !requirement.deliverable_id.is_empty()
        && !requirement.evidence_id.is_empty()
        && SUBJECTS.contains(&requirement.subject_kind.as_str())
        && !requirement.subject_id.is_empty()
        && requirement.result == "passed"
        && !requirement.producers.is_empty()
        && !requirement.source_revisions.is_empty()
        && requirement.diagnostics.is_empty()
}
