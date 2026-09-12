//! Native task-context execution binding (x-59b0).
//!
//! One binding per executing attempt: who is executing (node, attempt, full
//! harness/session identity, worktree), what it must have read (plan reference
//! + digest, selected ContextBundle reference/digest, explicitly required
//! constraints and sources with captured revisions), and how far observation
//! has actually gotten (Prepared/Submitted/Observed, or Unavailable when the
//! capability is unknown - never a claimed read).
//!
//! Every enforced decision lives here: structural validation, the canonical
//! digest, stage monotonicity, and live-source revalidation. Python
//! (`fno do resume receipt ...`) is a thin transport that shells these verbs
//! through the binary, exactly like the evidence gate; it owns no verdict.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

pub const BINDING_VERSION: u32 = 1;

pub const STAGE_PREPARED: &str = "prepared";
pub const STAGE_SUBMITTED: &str = "submitted";
pub const STAGE_OBSERVED: &str = "observed";
pub const STAGE_UNAVAILABLE: &str = "unavailable";

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct SourceRef {
    /// Worktree-relative path of the required source.
    pub path: String,
    /// The revision the bytes were captured at (git rev or snapshot id). Lineage
    /// only: revalidation compares content digests, so an unrelated code-HEAD
    /// move cannot stale an unchanged source.
    pub content_revision: String,
    /// sha256 hex of the source bytes at capture time.
    pub content_digest: String,
    pub byte_size: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Prepared,
    Submitted,
    Observed,
    /// Observation capability unknown or unsupported. Explicit honesty label:
    /// it never means Read and never advances toward Observed.
    Unavailable,
}

impl Stage {
    fn rank(&self) -> u8 {
        match self {
            Stage::Prepared => 0,
            Stage::Submitted => 1,
            // Unavailable is an honesty ceiling, not progress: it can follow a
            // prepare or a submit, but never counts as an observation.
            Stage::Observed | Stage::Unavailable => 2,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Prepared => STAGE_PREPARED,
            Stage::Submitted => STAGE_SUBMITTED,
            Stage::Observed => STAGE_OBSERVED,
            Stage::Unavailable => STAGE_UNAVAILABLE,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TaskContextBinding {
    pub version: u32,
    pub node: String,
    /// The executing attempt (the target session's minted run id). One binding
    /// per attempt; a different attempt is a different binding, never a match.
    pub attempt: String,
    pub harness: String,
    pub session: String,
    pub worktree: String,
    pub plan_path: String,
    pub plan_digest: String,
    /// The selected ContextBundle, by its own reference + digest. Selection
    /// stays owned by the existing resolver; this only names what was bound.
    #[serde(default)]
    pub bundle_reference: Option<String>,
    #[serde(default)]
    pub bundle_digest: Option<String>,
    /// Constraints the operator/task declared as required, carried verbatim.
    #[serde(default)]
    pub required_constraints: Vec<String>,
    /// Sources that MUST revalidate before execution or a committed handoff.
    #[serde(default)]
    pub required_sources: Vec<SourceRef>,
    /// Byte measure of the required sources (their bytes, not what was sent).
    #[serde(default)]
    pub source_bytes: u64,
    /// Byte measure of the prepared payload (what submission actually carries).
    #[serde(default)]
    pub payload_bytes: u64,
    pub stage: Stage,
}

/// A named refusal. A corrupt DECLARED binding refuses with a reason, never an
/// empty successful bundle; the code is the contract the callers print.
pub type BindResult<T> = Result<T, String>;

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl TaskContextBinding {
    /// Structural validation. Every failure names its reason.
    pub fn validate(&self) -> BindResult<()> {
        if self.version != BINDING_VERSION {
            return Err(format!(
                "unsupported_version:{} != {}",
                self.version, BINDING_VERSION
            ));
        }
        for (field, value) in [
            ("node", &self.node),
            ("attempt", &self.attempt),
            ("harness", &self.harness),
            ("session", &self.session),
            ("worktree", &self.worktree),
            ("plan_path", &self.plan_path),
        ] {
            if value.trim().is_empty() {
                return Err(format!("malformed_binding: empty {field}"));
            }
        }
        if !is_sha256_hex(&self.plan_digest) {
            return Err("malformed_binding: plan_digest is not sha256 hex".to_string());
        }
        if let Some(d) = &self.bundle_digest {
            if !is_sha256_hex(d) {
                return Err("malformed_binding: bundle_digest is not sha256 hex".to_string());
            }
        }
        if self.bundle_reference.is_some() != self.bundle_digest.is_some() {
            return Err(
                "malformed_binding: bundle_reference and bundle_digest must be set together"
                    .to_string(),
            );
        }
        let mut total: u64 = 0;
        for source in &self.required_sources {
            if source.path.trim().is_empty() || source.content_revision.trim().is_empty() {
                return Err(format!(
                    "malformed_binding: required source needs path and revision ({})",
                    source.path
                ));
            }
            if !is_sha256_hex(&source.content_digest) {
                return Err(format!(
                    "malformed_binding: source {} digest is not sha256 hex",
                    source.path
                ));
            }
            total = total.saturating_add(source.byte_size);
        }
        if total != self.source_bytes {
            return Err(format!(
                "source_bytes_mismatch: {total} != declared {}",
                self.source_bytes
            ));
        }
        Ok(())
    }

    /// Canonical serialization: field-declaration order, the only writer is
    /// this module, so declaration order is the canonical form.
    pub fn canonical(&self) -> BindResult<String> {
        serde_json::to_string(self).map_err(|e| format!("serialize_failed: {e}"))
    }

    pub fn digest(&self) -> BindResult<String> {
        let canonical = self.canonical()?;
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Everything that must be identical for a stage advance to be the SAME
    /// binding moving forward rather than a new binding.
    fn core_matches(&self, other: &TaskContextBinding) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        a.stage = Stage::Prepared;
        b.stage = Stage::Prepared;
        a == b
    }

    /// Advance the observation stage. Forward-only (Observed is final);
    /// Unavailable may follow Prepared/Submitted but is not an observation.
    pub fn advance_stage(&self, to: &Stage) -> BindResult<TaskContextBinding> {
        self.validate()?;
        let mut next = self.clone();
        next.stage = to.clone();
        if !self.core_matches(&next) {
            return Err("malformed_binding: stage advance changed binding core".to_string());
        }
        if next.stage == Stage::Observed && self.stage == Stage::Unavailable {
            return Err("stage_regression: unavailable cannot become observed".to_string());
        }
        if to.rank() < self.stage.rank() {
            return Err(format!(
                "stage_regression: {} -> {}",
                self.stage.as_str(),
                to.as_str()
            ));
        }
        Ok(next)
    }
}

/// A stored binding: the binding plus its self-describing digest.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoundBinding {
    #[serde(flatten)]
    pub binding: TaskContextBinding,
    pub binding_digest: String,
}

impl BoundBinding {
    /// Parse + verify. Digest mismatch means the stored bytes were tampered or
    /// written by something else: a named refusal, never a best-effort read.
    pub fn load(value: &Value) -> BindResult<BoundBinding> {
        let bound: BoundBinding =
            serde_json::from_value(value.clone()).map_err(|e| format!("malformed_binding: {e}"))?;
        let recomputed = bound.binding.digest()?;
        if recomputed != bound.binding_digest {
            return Err("binding_digest_mismatch".to_string());
        }
        bound.binding.validate()?;
        Ok(bound)
    }
}

fn read_stdin_json() -> BindResult<Value> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("could not read stdin: {e}"))?;
    serde_json::from_str(&input).map_err(|e| format!("bad request JSON: {e}"))
}

fn print_refusal(reason: &str) {
    println!("{}", serde_json::json!({ "ok": false, "reason": reason }));
}

/// Shared transport shape: request rides stdin, answer prints on stdout, a
/// semantic refusal is data (exit 0), only an unreadable request is exit 2.
fn run_stdin_verb(args: &[String], usage: &str, f: impl FnOnce(Value)) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: fno-agents {usage}");
        return 0;
    }
    if !args.is_empty() {
        eprintln!("fno-agents: unexpected arguments; the request rides stdin");
        return 2;
    }
    match read_stdin_json() {
        Ok(req) => {
            f(req);
            0
        }
        Err(reason) => {
            eprintln!("fno-agents: {reason}");
            2
        }
    }
}

/// `task-context-prepare`: validate the binding the caller assembled and stamp
/// its digest. Pure: the caller owns where the bound file lands (the existing
/// plan/session artifact root).
pub fn run_prepare(args: &[String]) -> i32 {
    run_stdin_verb(
        args,
        "task-context-prepare  (one JSON request on stdin: binding)",
        |req| {
            let parsed: BindResult<TaskContextBinding> = req
                .get("binding")
                .ok_or_else(|| "malformed_binding: missing binding".to_string())
                .and_then(|b| {
                    serde_json::from_value(b.clone()).map_err(|e| format!("malformed_binding: {e}"))
                });
            match parsed.and_then(|b| {
                b.validate()?;
                let digest = b.digest()?;
                Ok((b, digest))
            }) {
                Ok((binding, digest)) => {
                    let value = serde_json::to_value(&binding).unwrap_or(Value::Null);
                    println!(
                        "{}",
                        serde_json::json!({"ok": true, "binding": value, "binding_digest": digest, "stage": binding.stage.as_str()})
                    );
                }
                Err(reason) => print_refusal(&reason),
            }
        },
    )
}

/// `task-context-stage`: advance the observation stage on an existing bound
/// binding, forward-only, re-stamping the digest for the new stage.
pub fn run_stage(args: &[String]) -> i32 {
    run_stdin_verb(
        args,
        "task-context-stage  (one JSON request on stdin: binding, to)",
        |req| {
            let to = req
                .get("to")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let stage = match to.as_str() {
                STAGE_SUBMITTED => Some(Stage::Submitted),
                STAGE_OBSERVED => Some(Stage::Observed),
                STAGE_UNAVAILABLE => Some(Stage::Unavailable),
                _ => None,
            };
            let out = match (stage, req.get("binding")) {
                (None, _) => Err(format!(
                    "malformed_binding: unknown stage {to:?} (expected submitted|observed|unavailable)"
                )),
                (Some(_), None) | (Some(_), Some(Value::Null)) => {
                    Err("malformed_binding: missing binding".to_string())
                }
                (Some(stage), Some(b)) => BoundBinding::load(b).and_then(|bound| {
                    let next = bound.binding.advance_stage(&stage)?;
                    let digest = next.digest()?;
                    let value = serde_json::to_value(&next).unwrap_or(Value::Null);
                    Ok(serde_json::json!({"ok": true, "binding": value, "binding_digest": digest, "stage": next.stage.as_str()}))
                }),
            };
            match out {
                Ok(v) => println!("{v}"),
                Err(reason) => print_refusal(&reason),
            }
        },
    )
}

/// `task-context-show`: load + verify a declared binding (the read side of
/// show). Legacy/unbound never reaches here: the receipt transport reports
/// `bound: false` itself.
pub fn run_show(args: &[String]) -> i32 {
    run_stdin_verb(
        args,
        "task-context-show  (one JSON request on stdin: binding)",
        |req| match req
            .get("binding")
            .ok_or_else(|| "malformed_binding: missing binding".to_string())
            .and_then(BoundBinding::load)
        {
            Ok(bound) => {
                let value = serde_json::to_value(&bound).unwrap_or(Value::Null);
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "bound": true, "binding": value, "stage": bound.binding.stage.as_str()})
                );
            }
            Err(reason) => print_refusal(&reason),
        },
    )
}

/// `task-context-revalidate`: the gate. Verifies the stored digest, the
/// expected executing identity, and every required source's live bytes under
/// `root`. Content digests decide staleness; a moved code HEAD alone never
/// fails an unchanged source.
pub fn run_revalidate(args: &[String]) -> i32 {
    run_stdin_verb(
        args,
        "task-context-revalidate  (one JSON request on stdin: binding, expect{node,attempt,session}, root)",
        |req| {
            println!("{}", revalidate_request(&req));
        },
    )
}

/// The revalidation verdict table, shared by the binary verb and the unit
/// tests. Returns the answer object (ok or a named refusal).
pub fn revalidate_request(req: &Value) -> Value {
    let root = req
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expect = req.get("expect").cloned().unwrap_or(Value::Null);
    let verdict: BindResult<Value> = (|| {
        if root.trim().is_empty() {
            return Err("malformed_binding: missing root".to_string());
        }
        let bound = BoundBinding::load(
            req.get("binding")
                .ok_or_else(|| "malformed_binding: missing binding".to_string())?,
        )?;
        let b = &bound.binding;
        for (field, want) in [
            ("node", b.node.as_str()),
            ("attempt", b.attempt.as_str()),
            ("session", b.session.as_str()),
        ] {
            // A door checks the identities it KNOWS: an absent expectation key
            // is unchecked at that door, never a silent pass (the receipt door
            // passes attempt+session; the init gate knows node + root). A key
            // that IS present must match exactly.
            let expected = match expect.get(field) {
                Some(Value::String(s)) => s.as_str(),
                _ => continue,
            };
            if expected != want {
                let reason = match field {
                    "attempt" => "wrong_attempt",
                    "session" => "foreign_session",
                    _ => "wrong_node",
                };
                return Err(format!("{reason}: expected {want}, got {expected}"));
            }
        }
        for source in &b.required_sources {
            let path = Path::new(&root).join(&source.path);
            let bytes =
                std::fs::read(&path).map_err(|_| format!("missing_source: {}", source.path))?;
            if bytes.len() as u64 != source.byte_size {
                return Err(format!("stale_source: {}", source.path));
            }
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let digest = format!("{:x}", hasher.finalize());
            if digest != source.content_digest {
                return Err(format!("stale_source: {}", source.path));
            }
        }
        Ok(serde_json::json!({
            "ok": true,
            "node": b.node,
            "attempt": b.attempt,
            "stage": b.stage.as_str(),
            "checked_sources": b.required_sources.len(),
        }))
    })();
    match verdict {
        Ok(v) => v,
        Err(reason) => serde_json::json!({"ok": false, "reason": reason}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source(path: &str, body: &str) -> SourceRef {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        SourceRef {
            path: path.to_string(),
            content_revision: "b48ba4b8c".to_string(),
            content_digest: format!("{:x}", hasher.finalize()),
            byte_size: body.len() as u64,
        }
    }

    fn binding(sources: Vec<SourceRef>) -> TaskContextBinding {
        TaskContextBinding {
            version: BINDING_VERSION,
            node: "x-59b0".to_string(),
            attempt: "20260912T052218Z-cl63988-4133dc".to_string(),
            harness: "claude".to_string(),
            session: "850a419a".to_string(),
            worktree: "/wt/x-59b0".to_string(),
            plan_path: "plans/20260907-task-context.md".to_string(),
            plan_digest: "a".repeat(64),
            bundle_reference: Some("roles/fno-archer.md".to_string()),
            bundle_digest: Some("b".repeat(64)),
            required_constraints: vec!["Do not widen scope beyond the plan".to_string()],
            source_bytes: sources.iter().map(|s| s.byte_size).sum(),
            required_sources: sources,
            payload_bytes: 512,
            stage: Stage::Prepared,
        }
    }

    #[test]
    fn prepare_validates_and_stamps_a_digest() {
        let b = binding(vec![source("PLAN.md", "plan bytes\n")]);
        b.validate().expect("valid");
        let digest = b.digest().expect("digest");
        assert_eq!(digest.len(), 64);
        // Deterministic: same binding, same digest.
        assert_eq!(digest, b.digest().expect("digest"));
    }

    #[test]
    fn byte_measures_stay_separate() {
        let body = "plan bytes\n";
        let b = binding(vec![source("PLAN.md", body)]);
        assert_eq!(b.source_bytes, body.len() as u64);
        assert_eq!(b.payload_bytes, 512);
        assert_ne!(b.source_bytes, b.payload_bytes);
    }

    #[test]
    fn source_bytes_must_match_references() {
        let mut b = binding(vec![source("PLAN.md", "plan bytes\n")]);
        b.source_bytes = 999;
        assert_eq!(
            b.validate().unwrap_err(),
            "source_bytes_mismatch: 11 != declared 999"
        );
    }

    #[test]
    fn corrupt_declared_binding_refuses_by_name() {
        let b = binding(vec![source("PLAN.md", "plan bytes\n")]);
        let mut value = serde_json::to_value(&b).expect("serialize");
        value["binding_digest"] = json!(format!("{:0>64}", "0"));
        let err = BoundBinding::load(&value).unwrap_err();
        assert_eq!(err, "binding_digest_mismatch");
    }

    #[test]
    fn unsupported_version_refuses_by_name() {
        let mut b = binding(vec![]);
        b.version = 99;
        assert!(b.validate().unwrap_err().starts_with("unsupported_version"));
    }

    #[test]
    fn stage_moves_forward_only_and_restamps() {
        let b = binding(vec![]);
        let submitted = b.advance_stage(&Stage::Submitted).expect("advance");
        assert_eq!(submitted.stage, Stage::Submitted);
        assert_ne!(submitted.digest().expect("d"), b.digest().expect("d"));
        assert!(submitted
            .advance_stage(&Stage::Prepared)
            .unwrap_err()
            .starts_with("stage_regression"));
        let observed = submitted.advance_stage(&Stage::Observed).expect("advance");
        assert!(observed
            .advance_stage(&Stage::Submitted)
            .unwrap_err()
            .starts_with("stage_regression"));
        let unavailable = submitted
            .advance_stage(&Stage::Unavailable)
            .expect("ceiling");
        assert!(unavailable
            .advance_stage(&Stage::Observed)
            .unwrap_err()
            .starts_with("stage_regression"));
    }

    #[test]
    fn stage_advance_cannot_silent_change_the_core() {
        let mut b = binding(vec![]);
        b.payload_bytes = 1;
        let mut tampered = b.clone();
        tampered.payload_bytes = 2;
        tampered.stage = Stage::Submitted;
        // advance_stage from the untampered binding never yields the tampered core.
        let next = b.advance_stage(&Stage::Submitted).expect("advance");
        assert_ne!(next, tampered);
    }

    fn write_source(root: &Path, rel: &str, body: &str) {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, body).expect("write");
    }

    /// The stored shape: the binding plus its stamped digest. Revalidation
    /// always sees a PREPARED binding, never a bare one.
    fn bound_value(b: &TaskContextBinding) -> Value {
        let mut v = serde_json::to_value(b).expect("s");
        v["binding_digest"] = json!(b.digest().expect("d"));
        v
    }

    #[test]
    fn revalidate_ok_when_sources_unchanged_regardless_of_head() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().to_string_lossy().to_string();
        write_source(dir.path(), "docs/PLAN.md", "plan bytes\n");
        let mut b = binding(vec![source("docs/PLAN.md", "plan bytes\n")]);
        b.worktree = root.clone();
        // The caller's live HEAD is NOT consulted: only content decides.
        let verdict = revalidate_request(&json!({
            "binding": bound_value(&b),
            "expect": {"node": "x-59b0", "attempt": b.attempt, "session": b.session},
            "root": root,
        }));
        assert_eq!(verdict["ok"], json!(true), "{verdict}");
        assert_eq!(verdict["checked_sources"], json!(1));
    }

    #[test]
    fn revalidate_names_missing_and_stale_sources() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().to_string_lossy().to_string();
        write_source(dir.path(), "docs/PLAN.md", "plan bytes\n");
        let mut b = binding(vec![
            source("docs/PLAN.md", "plan bytes\n"),
            source("docs/GONE.md", "gone\n"),
        ]);
        b.worktree = root.clone();
        let expect = json!({"node": "x-59b0", "attempt": b.attempt, "session": b.session});
        let value = bound_value(&b);
        assert_eq!(
            revalidate_request(&json!({"binding": value, "expect": expect, "root": root}))
                ["reason"],
            json!("missing_source: docs/GONE.md")
        );

        write_source(dir.path(), "docs/PLAN.md", "CHANGED bytes\n");
        let value2 = bound_value(&b);
        assert_eq!(
            revalidate_request(&json!({"binding": value2, "expect": expect, "root": root}))
                ["reason"],
            json!("stale_source: docs/PLAN.md")
        );
    }

    #[test]
    fn revalidate_names_wrong_attempt_and_foreign_session() {
        let dir = tempfile::tempdir().expect("tmp");
        let b = binding(vec![]);
        let wrong_attempt = json!({
            "binding": bound_value(&b),
            "expect": {"node": "x-59b0", "attempt": "OTHER", "session": b.session},
            "root": dir.path().to_string_lossy(),
        });
        assert!(revalidate_request(&wrong_attempt)["reason"]
            .as_str()
            .unwrap()
            .starts_with("wrong_attempt"));
        let foreign = json!({
            "binding": bound_value(&b),
            "expect": {"node": "x-59b0", "attempt": b.attempt, "session": "someone-else"},
            "root": dir.path().to_string_lossy(),
        });
        assert!(revalidate_request(&foreign)["reason"]
            .as_str()
            .unwrap()
            .starts_with("foreign_session"));
    }
}
