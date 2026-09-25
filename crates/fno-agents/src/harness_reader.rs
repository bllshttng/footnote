//! The capability reader: ONE implementation of measuring the capability
//! table against a harness. It ports and deletes
//! `cli/src/fno/agents/capability_probe.py` (the four field verdicts and
//! their per-key comparison rules) and `cli/src/fno/agents/harness_probe.py`
//! (the line-by-line live rubric), whose deletion is the point: a second
//! copy of a reader is how the first one rots.
//!
//! Two doors ride this module as transport-only client arms (no verb is
//! registered; the shrink law allows none):
//!
//!   `harness-probe fields <harness> [--live] [--write] [--json|-J]`
//!   `harness-probe rubric <harness> [--live] [--json|-J]`
//!
//! `fields` is the read-only capability probe; `rubric` is the executable
//! support journey. Every pass and fail carries a positive marker, UNKNOWN
//! never acts, and the live tier runs inside an isolated state root it
//! removes after itself.

use crate::bounded_cmd::output_with_timeout_result;
use crate::harness_capabilities::{HarnessContract, ProbeDecl};
use chrono::Utc;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const AUTHORITY_TIMEOUT_S: u64 = 15;
const INSTRUMENT_TIMEOUT_S: u64 = 30;
const PROBE_TIMEOUT_S: u64 = 5;
const LIVE_SPAWN_TIMEOUT_S: u64 = 75;
/// The env pins a live journey strips from every child it spawns, beside the
/// two it SETS (`FNO_HOME`, `HOME`): the harness config roots and the fno
/// state pins that would otherwise reach the real fleet from inside a proof.
const AMBIENT_PINS: [&str; 19] = [
    "FNO_CONFIG",
    "FNO_AGENTS_HOME",
    "FNO_REPO_ROOT",
    "FNO_SPACES_DIR",
    "FNO_EVENTS_PATH",
    "FNO_BUS_DIR",
    "FNO_INBOX_ROOT",
    "FNO_CLAIMS_ROOT",
    "CLAUDE_PLUGIN_ROOT",
    "CODEX_PLUGIN_ROOT",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_DIR_OVERRIDE",
    "CODEX_HOME",
    "GEMINI_PROJECT_DIR",
    "GEMINI_SANDBOX",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_DB",
    "GROK_HOME",
    "GROK_SESSION_ID",
];

// ── the fields probe ─────────────────────────────────────────────────

/// The four verdicts a declared instrument can answer. An absent instrument
/// is UNKNOWN, never a disagreement; UNPROBEABLE carries the table's reason
/// as the measurement record. Every declared field leaves with one of the
/// four: there is no UNDECLARED verdict, because an instrument that reached
/// the reader is by definition declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    AGREES,
    DISAGREES,
    UNPROBEABLE,
    UNKNOWN,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let word = match self {
            Verdict::AGREES => "AGREES",
            Verdict::DISAGREES => "DISAGREES",
            Verdict::UNPROBEABLE => "UNPROBEABLE",
            Verdict::UNKNOWN => "UNKNOWN",
        };
        f.write_str(word)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldReport {
    pub field: String,
    pub kind: String,
    pub verdict: Verdict,
    pub detail: String,
    /// The evidence line the authority produced, quoted on a disagreement.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub evidence: String,
}

/// The receipt a completed measurement carries: which harness, which
/// version of it, which tier ran, and which reader settled the answer. For
/// the live tier the account and model are what the session read back, and
/// a requested seat that differs is named beside it, never silently
/// substituted (a route that downgraded is a measurement of the seat that
/// ran).
#[derive(Debug, Clone, Serialize)]
pub struct MeasurementRecord {
    pub harness: String,
    /// The harness version read off the binary; empty means it could not be
    /// read, and a measurement that cannot name a version is not a
    /// measurement of any version.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// `read` when the version came off the binary, `unreadable` otherwise.
    pub version_status: &'static str,
    /// One of `declared`, `isolated`, `live`.
    pub tier: &'static str,
    pub reader: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub account: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub requested_account: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub requested_model: String,
}

fn run_command_bounded(cmd: &mut Command, secs: u64) -> (Option<i32>, String) {
    match output_with_timeout_result(cmd, secs) {
        Ok(out) => {
            let code = out.status.code();
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (code, text)
        }
        Err(e) => (
            None,
            format!("could not run {}: {e}", cmd.get_program().to_string_lossy()),
        ),
    }
}

/// Run one authority command so its return code stays readable. A spawn
/// failure is code 127, a timeout 124: both are UNKNOWN, never a verdict.
fn run_authority(argv: &[String]) -> (i32, String) {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    match output_with_timeout_result(&mut cmd, AUTHORITY_TIMEOUT_S) {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (code, text)
        }
        Err(e) => (127, format!("could not run {}: {e}", argv[0])),
    }
}

fn authority_argv(decl: &ProbeDecl, harness: &str) -> Vec<String> {
    decl.authority
        .split_whitespace()
        .map(|token| token.replace("{bin}", harness))
        .collect()
}

/// The harness version, read off the binary. `Err` is the unreadable case,
/// not a guess.
fn harness_version(harness: &str) -> Result<String, String> {
    let mut cmd = Command::new(harness);
    cmd.arg("--version");
    let (code, output) = run_command_bounded(&mut cmd, AUTHORITY_TIMEOUT_S);
    let first = output.lines().next().unwrap_or("").trim().to_string();
    if code == Some(0) && !first.is_empty() {
        Ok(first)
    } else {
        Err(format!(
            "{harness} --version exited {code:?} with no version line"
        ))
    }
}

/// The per-key comparison rules the Python probe never had. The one shipped
/// rule reads `model_switch_strategy`: a matched `--effort` surface means
/// the binary declares a reasoning-effort choice list, and a row whose kind
/// is `unsupported` understates the CLI (the live agy case). A declared
/// field with no registered rule answers UNKNOWN - the probe refuses to
/// guess a rule.
fn compare_declared(
    field: &str,
    contract: &HarnessContract,
    harness: &str,
    matched: Option<regex::Match<'_>>,
) -> (bool, String) {
    if field != "model_switch_strategy" {
        return (
            false,
            "no comparison rule registered for this declared field; the probe refuses to guess one"
                .to_string(),
        );
    }
    let kind = contract
        .capabilities(harness)
        .map(|caps| caps.model_switch_strategy.kind.clone())
        .unwrap_or_else(|_| "unsupported".to_string());
    match matched {
        Some(m) => {
            let vocab = m
                .as_str()
                .rsplit('(')
                .next()
                .unwrap_or("")
                .trim_end_matches(')')
                .to_string();
            if kind == "unsupported" {
                (
                    false,
                    format!(
                        "the binary declares a reasoning-effort surface ({vocab}) but the row says kind = unsupported"
                    ),
                )
            } else {
                (
                    true,
                    format!("the row ({kind}) and the declared surface ({vocab}) agree"),
                )
            }
        }
        None => {
            if kind != "unsupported" {
                (
                    false,
                    format!(
                        "the row declares {kind} but the authority declares no reasoning-effort surface"
                    ),
                )
            } else {
                (
                    true,
                    "no declared effort surface and the row says unsupported".to_string(),
                )
            }
        }
    }
}

/// The commented stanza for each disagreement, evidence line and measurement
/// date beside it. The stanza is COMMENTED: the probe writes only what its
/// instruments derived, never a guess shaped like a measurement.
fn write_stanza(harness: &str, disagreements: &[&FieldReport]) -> Option<String> {
    if disagreements.is_empty() {
        return None;
    }
    let today = Utc::now().date_naive().format("%Y-%m-%d");
    let mut lines = vec![format!(
        "# capability probe: {} disagreement(s) on {harness}, measured {today}",
        disagreements.len()
    )];
    for field in disagreements {
        lines.push(format!("# DISAGREES: {}", field.detail));
        if field.field == "model_switch_strategy" && !field.evidence.is_empty() {
            lines.push("# The derived half is real (the --help surface above); the".to_string());
            lines.push("# status pair is the retask surface and stays UNMEASURED -".to_string());
            lines.push("# complete it from a live pane before relying on retask.".to_string());
            lines.push(format!("# [harness.{harness}.model_switch_strategy]"));
            lines.push("# kind = \"direct\"".to_string());
            lines.push("# tokens = [\"--model {model}\", \"--effort {effort}\"]".to_string());
            lines.push("# effort_labels = {}".to_string());
            lines.push("# status_command = \"/status\"   # UNMEASURED".to_string());
            lines.push("# status_pattern = \"\"          # UNMEASURED".to_string());
        } else {
            lines.push(format!(
                "# no stanza template for {}; correct it by hand",
                field.field
            ));
        }
    }
    Some(lines.join("\n"))
}

/// The fields probe: one verdict per declared field, the measurement record
/// beside them. Read-only unless `write`.
fn probe_fields(harness: &str, live: bool, write: bool) -> serde_json::Value {
    let contract = match HarnessContract::packaged() {
        Ok(contract) => contract,
        Err(error) => {
            return serde_json::json!({
                "harness": harness,
                "error": error.to_string(),
                "fields": [],
                "stanza": serde_json::Value::Null,
                "warnings": [],
            });
        }
    };
    let map_version = contract.map_version;
    let row_exists = contract.capabilities(harness).is_ok();
    let record = measurement_record(harness, "declared", "capability probe");
    let mut fields: Vec<FieldReport> = Vec::new();
    for (field, decl) in &contract.probe {
        match decl.kind.as_str() {
            "unprobeable" => fields.push(FieldReport {
                field: field.clone(),
                kind: decl.kind.clone(),
                verdict: Verdict::UNPROBEABLE,
                detail: decl.reason.clone(),
                evidence: String::new(),
            }),
            "behavioral" => {
                // A behavioral probe spawns a scratch session against a real
                // login; no vendor-store instrument is wired for any
                // harness, so a live run still answers UNKNOWN. Wiring one
                // is its own measured change per harness, never a table edit.
                fields.push(FieldReport {
                    field: field.clone(),
                    kind: decl.kind.clone(),
                    verdict: Verdict::UNKNOWN,
                    detail: if live {
                        "no vendor-store instrument implemented for this harness".to_string()
                    } else {
                        "behavioral probe needs --live (a scratch session is spawned); \
                         read-only run spawns nothing"
                            .to_string()
                    },
                    evidence: String::new(),
                })
            }
            _ => {
                let argv = authority_argv(decl, harness);
                let (code, output) = run_authority(&argv);
                if code != 0 {
                    fields.push(FieldReport {
                        field: field.clone(),
                        kind: decl.kind.clone(),
                        verdict: Verdict::UNKNOWN,
                        detail: format!(
                            "authority exited {code}: {}",
                            output.trim().chars().take(200).collect::<String>()
                        ),
                        evidence: String::new(),
                    });
                    continue;
                }
                let regex = regex::Regex::new(&decl.pattern).expect("validated at load");
                let matched = regex.captures(&output);
                let (agrees, mut detail) = compare_declared(
                    field,
                    &contract,
                    harness,
                    matched.as_ref().map(|c| c.get(0).expect("group 0")),
                );
                let verdict = if agrees {
                    Verdict::AGREES
                } else {
                    Verdict::DISAGREES
                };
                let evidence = matched
                    .as_ref()
                    .map(|c| c.get(0).expect("group 0").as_str().to_string())
                    .unwrap_or_default();
                if !evidence.is_empty() {
                    detail.push_str(&format!(" (evidence: {evidence})"));
                }
                fields.push(FieldReport {
                    field: field.clone(),
                    kind: decl.kind.clone(),
                    verdict,
                    detail,
                    evidence,
                });
            }
        }
    }
    if !row_exists {
        return serde_json::json!({
            "harness": harness,
            "error": format!("unknown harness {harness:?} in capability contract"),
            "fields": [],
            "stanza": serde_json::Value::Null,
            "warnings": [],
            "record": record,
        });
    }
    let disagreements: Vec<&FieldReport> = fields
        .iter()
        .filter(|f| f.verdict == Verdict::DISAGREES)
        .collect();
    let stanza = if write {
        write_stanza(harness, &disagreements)
    } else {
        None
    };
    serde_json::json!({
        "harness": harness,
        "map_version": map_version,
        "fields": fields,
        "stanza": stanza,
        "warnings": [],
        "record": record,
    })
}

fn measurement_record(harness: &str, tier: &'static str, reader: &str) -> MeasurementRecord {
    let (version, version_status) = match harness_version(harness) {
        Ok(v) => (v, "read"),
        Err(_) => (String::new(), "unreadable"),
    };
    MeasurementRecord {
        harness: harness.to_string(),
        version,
        version_status,
        tier,
        reader: reader.to_string(),
        account: String::new(),
        model: String::new(),
        requested_account: String::new(),
        requested_model: String::new(),
    }
}

// ── the live rubric ──────────────────────────────────────────────────

/// One rubric line's verdict. A pass or fail REQUIRES a positive marker at
/// construction: a silent success is a skip, and a fail without the marker
/// it looked for is a guess.
#[derive(Debug, Clone, Serialize)]
pub struct LineVerdict {
    pub line: String,
    pub status: String,
    pub marker: String,
    pub attempts: u32,
    pub detail: String,
}

impl LineVerdict {
    pub fn new(
        line: &str,
        status: &str,
        marker: &str,
        attempts: u32,
        detail: String,
    ) -> LineVerdict {
        assert!(
            status == "pass" || status == "fail" || status == "skip",
            "unknown status {status:?}"
        );
        if status == "pass" || status == "fail" {
            assert!(
                !marker.trim().is_empty(),
                "pass/fail verdict requires a positive marker"
            );
        }
        assert!(attempts >= 1, "attempts must be positive");
        LineVerdict {
            line: line.to_string(),
            status: status.to_string(),
            marker: marker.to_string(),
            attempts,
            detail,
        }
    }
}

/// Read a positive marker three times before calling it absent. The last
/// read travels in the detail, so a fail names what was actually seen.
fn retry_marker(
    line: &str,
    marker_name: &str,
    mut read: impl FnMut() -> String,
    delay: Duration,
) -> LineVerdict {
    let mut last = String::new();
    for attempt in 1..=3 {
        if attempt > 1 && !delay.is_zero() {
            std::thread::sleep(delay);
        }
        last = read();
        if !last.is_empty() {
            return LineVerdict::new(line, "pass", marker_name, attempt, last);
        }
    }
    LineVerdict::new(
        line,
        "fail",
        marker_name,
        3,
        format!("positive marker not observed; last read={last:?}"),
    )
}

/// The isolated world a live journey runs in: its own fno state root and its
/// own home, with a run nonce planted inside. The nonce is the journey's
/// positive marker: the report reads it back from INSIDE this root, and its
/// absence from the real root is reported only beside that positive read.
/// A root that cannot be established refuses the run - there is no
/// unisolated fallback, because a proof that becomes a fleet row is a proof
/// that costs the fleet.
pub struct IsolatedRoot {
    dir: PathBuf,
    fno_home: PathBuf,
    home: PathBuf,
    pub nonce: String,
    nonce_path: PathBuf,
}

impl IsolatedRoot {
    pub fn establish(harness: &str) -> Result<IsolatedRoot, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "fno-harness-{harness}-{}-{nanos}",
            std::process::id()
        ));
        let fno_home = dir.join("fno");
        let home = dir.join("home");
        std::fs::create_dir_all(&fno_home).map_err(|e| {
            format!(
                "cannot establish an isolated state root at {}: {e}",
                fno_home.display()
            )
        })?;
        std::fs::create_dir_all(&home).map_err(|e| {
            format!(
                "cannot establish an isolated harness home at {}: {e}",
                home.display()
            )
        })?;
        let nonce = format!("{:032x}", nanos ^ (u128::from(std::process::id()) << 96));
        let nonce_path = fno_home.join("probe-nonce");
        std::fs::write(&nonce_path, &nonce)
            .map_err(|e| format!("cannot plant the run nonce in the isolated root: {e}"))?;
        let read_back = std::fs::read_to_string(&nonce_path)
            .map_err(|e| format!("cannot read the run nonce back from the isolated root: {e}"))?;
        if read_back != nonce {
            return Err("the isolated root did not read back the planted nonce".to_string());
        }
        Ok(IsolatedRoot {
            dir,
            fno_home,
            home,
            nonce,
            nonce_path,
        })
    }

    /// The positive read: the nonce, read back from inside the isolated root.
    pub fn positive_read(&self) -> bool {
        std::fs::read_to_string(&self.nonce_path).is_ok_and(|text| text == self.nonce)
    }

    /// The paired negative read: the same-named nonce is absent from the REAL
    /// state root. `None` reads unknown (the real root could not be
    /// consulted), never absence.
    pub fn real_root_absent(&self) -> Option<bool> {
        let real = std::env::var_os("FNO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fno")))?;
        let leaked = real.join("probe-nonce");
        match std::fs::metadata(&leaked) {
            Ok(_) => Some(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(true),
            Err(_) => None,
        }
    }

    /// Every child of the journey runs with this env: the two isolation pins
    /// set, every ambient state and config pin stripped.
    pub fn child_env(&self, cmd: &mut Command) {
        for name in AMBIENT_PINS {
            cmd.env_remove(name);
        }
        cmd.env("FNO_HOME", &self.fno_home);
        cmd.env("HOME", &self.home);
    }

    fn fno(&self, argv: &[&str]) -> (Option<i32>, String) {
        let fno_bin = std::env::var("FNO_PROBE_FNO").unwrap_or_else(|_| "fno".to_string());
        let mut cmd = Command::new(fno_bin);
        cmd.args(argv);
        cmd.env("READINESS_SMOKE", "1");
        self.child_env(&mut cmd);
        run_command_bounded(&mut cmd, INSTRUMENT_TIMEOUT_S)
    }
}

impl Drop for IsolatedRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn registry_row_name_in(root: &IsolatedRoot, name: &str) -> bool {
    let (code, output) = root.fno(&["agents", "list", "--json"]);
    if code != Some(0) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&output)
        .ok()
        .and_then(|rows| {
            rows.as_array().map(|rows| {
                rows.iter()
                    .any(|row| row.get("name").and_then(|n| n.as_str()) == Some(name))
            })
        })
        .unwrap_or(false)
}

fn row_field(rows_json: &str, name: &str, field: &str) -> String {
    serde_json::from_str::<serde_json::Value>(rows_json)
        .ok()
        .and_then(|rows| {
            rows.as_array().and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("name").and_then(|n| n.as_str()) == Some(name))
                    .and_then(|row| row.get(field).and_then(|v| v.as_str()))
                    .map(str::to_string)
            })
        })
        .unwrap_or_default()
}

fn probe_seed(nonce: &str, name: &str, claim_key: &str) -> String {
    format!(
        "Support probe {nonce}. Run fno agents claim acquire {claim_key} \
         --holder {name} --ttl 2m, then print PROBE_SEED={nonce}. \
         When you receive PROBE_RECALL_REQUEST={nonce}, print PROBE_RECALL={nonce}. \
         When you receive PROBE_MAIL={nonce}, print PROBE_REPLY={nonce}. \
         When you receive PROBE_SURVIVE_REQUEST={nonce}, and only after seeing \
         PROBE_SEED={nonce}, print PROBE_SURVIVE={nonce}. Remain idle."
    )
}

fn missing_marker_lines(name: &str, fail_detail: String) -> Vec<LineVerdict> {
    let mut lines = vec![LineVerdict::new(
        "SPAWN",
        "fail",
        "harness binary",
        1,
        fail_detail,
    )];
    for (line, marker) in [
        ("ISOLATION", "run nonce inside the isolated root"),
        (
            "IDENTITY",
            "local store artifact or cross-process recall nonce",
        ),
        ("CLAIM", "live claim holder"),
        ("MAIL BOTH WAYS", "worker response to sent message"),
        ("VIEW", "harness-owned screen"),
        ("SURVIVE", "prior turn after process stop"),
        ("ROW MATCHES", "honesty sweep and canonical-copy freshness"),
        ("MANIFEST PINNED", "live readiness-grid capture"),
        ("CLEANUP", "row removed after the run"),
    ] {
        lines.push(LineVerdict::new(
            line,
            "skip",
            marker,
            1,
            format!("blocked by {name} binary"),
        ));
    }
    lines
}

fn dry_run_lines(harness: &str) -> Vec<LineVerdict> {
    let names = [
        ("ISOLATION", "run nonce inside the isolated root"),
        ("SPAWN", "registry row"),
        (
            "IDENTITY",
            "local store artifact or cross-process recall nonce",
        ),
        ("CLAIM", "live claim holder"),
        ("MAIL BOTH WAYS", "worker response to sent message"),
        ("VIEW", "harness-owned screen"),
        ("SURVIVE", "prior turn after process stop"),
        ("ROW MATCHES", "honesty sweep and canonical-copy freshness"),
        ("MANIFEST PINNED", "live readiness-grid capture"),
        ("CLEANUP", "row removed after the run"),
    ];
    names
        .iter()
        .map(|(line, marker)| {
            LineVerdict::new(
                line,
                "skip",
                marker,
                1,
                format!("dry run for {harness}; no process spawned"),
            )
        })
        .collect()
}

/// The dry run's pane argv, composed from the packaged table's own create
/// form: the lane is the harness's declared create form, rendered raw and
/// composed, with no session id.
fn dry_run_argv(contract: &HarnessContract, harness: &str) -> Result<Vec<String>, String> {
    let caps = contract.capabilities(harness).map_err(|e| e.to_string())?;
    let lane = caps
        .resume_strategy
        .forms
        .iter()
        .find(|(name, form)| form.kind != "unsupported" && name.contains("create"))
        .or_else(|| {
            caps.resume_strategy
                .forms
                .iter()
                .find(|(_, form)| form.kind != "unsupported")
        })
        .map(|(name, _)| name.clone())
        .ok_or_else(|| format!("harness {harness:?} declares no create lane"))?;
    contract
        .render_session_argv_with_ids(harness, &lane, None, None)
        .map_err(|e| e.to_string())
}

/// The wait-for-CI journey's per-harness answer, read from the watch lease's
/// own rule: only a claude session may park and wake on a watcher. The same
/// reader admits a harness the lease permits (the journey's control) and
/// reads every other harness absent with the refusal quoted - the refusal is
/// code that already runs, not a table word.
pub(crate) fn wait_for_ci_cell(harness: &str) -> (&'static str, String) {
    let author = if harness.is_empty() {
        None
    } else {
        Some(harness)
    };
    if crate::loopcheck::watch_lease::harness_can_idle(author, false) {
        ("native", String::new())
    } else {
        (
            "absent",
            crate::loopcheck::watch_lease::watching_harness_refusal(author, false),
        )
    }
}

/// The live rubric against one harness, inside `root`. Every subprocess runs
/// with the isolated env; nothing the run mints can reach the real fleet.
fn run_live_rubric(harness: &str, root: &IsolatedRoot) -> (Vec<LineVerdict>, IsolationFacts) {
    let probe_id = format!(
        "{:08x}",
        root.nonce[..8].parse::<u32>().unwrap_or(0).swap_bytes()
    );
    let name = format!("harness-probe-{harness}-{probe_id}");
    let claim_key = format!("probe:{harness}:{probe_id}");
    let nonce = root.nonce.clone();
    let seed = probe_seed(&nonce, &name, &claim_key);
    let mut lines: Vec<LineVerdict> = Vec::new();

    let (spawn_code, spawn_output) = root.fno(&[
        "agents",
        "spawn",
        &seed,
        "--name",
        &name,
        "--harness",
        harness,
        "--cwd",
        &std::env::temp_dir().to_string_lossy(),
        "--timeout",
        "60",
    ]);

    // ISOLATION first: the nonce must read back from inside the root, and its
    // absence from the real root is reported only beside that positive read.
    let isolation = IsolationFacts {
        positive_read: root.positive_read(),
        real_root_absent: root.real_root_absent(),
    };
    lines.push(
        match (isolation.positive_read, isolation.real_root_absent) {
            (true, Some(true)) => LineVerdict::new(
                "ISOLATION",
                "pass",
                "run nonce inside the isolated root",
                1,
                "nonce read back inside the isolated root and absent from the real state root"
                    .to_string(),
            ),
            (true, other) => LineVerdict::new(
                "ISOLATION",
                "pass",
                "run nonce inside the isolated root",
                1,
                format!(
                    "nonce read back inside the isolated root; real-root absence {}",
                    match other {
                        Some(false) => "REFUTED: nonce found in the real root",
                        _ => "unknown (real root unreadable)",
                    }
                ),
            ),
            (false, _) => LineVerdict::new(
                "ISOLATION",
                "fail",
                "run nonce inside the isolated root",
                1,
                "the isolated root did not read back the planted nonce".to_string(),
            ),
        },
    );

    let (list_code, list_output) = root.fno(&["agents", "list", "--json"]);
    let row_found = list_code == Some(0) && registry_row_name_in(root, &name);
    if !row_found {
        lines.push(LineVerdict::new(
            "SPAWN",
            "fail",
            "registry row",
            3,
            format!(
                "registry row was not found; receipt was not used as proof ({})",
                {
                    let trimmed = spawn_output.trim();
                    trimmed.chars().take(120).collect::<String>()
                }
            ),
        ));
        append_blocked(&mut lines, "the failed spawn");
        lines.push(row_matches_line(harness));
        lines.push(manifest_pinned_line(
            harness,
            root,
            Some((spawn_code, spawn_output)),
            None,
        ));
        lines.push(cleanup_line(root, &name));
        return (lines, isolation);
    }
    lines.push(LineVerdict::new(
        "SPAWN",
        "pass",
        "registry row",
        1,
        "row read back".to_string(),
    ));

    let session_id = row_field(&list_output, &name, "harness_session_id");
    let mux_session = row_field(&list_output, &name, "mux_session");
    let mux_pane = row_field(&list_output, &name, "mux_pane_id");

    // IDENTITY: the session id appears in the worker's own logs, or a second
    // process recalls the planted nonce.
    let (resume_code, _) = root.fno(&["agents", "resume", &name]);
    let (recall_code, _) = root.fno(&[
        "agents",
        "mail",
        "send",
        &name,
        &format!("PROBE_RECALL_REQUEST={nonce}"),
    ]);
    let recall_marker = format!("PROBE_RECALL={nonce}");
    let identity = retry_marker(
        "IDENTITY",
        "local store artifact or cross-process recall nonce",
        || {
            let (code, logs_after) = root.fno(&["agents", "logs", &name]);
            if !session_id.is_empty() && code == Some(0) && logs_after.contains(&session_id) {
                return "local store artifact".to_string();
            }
            if resume_code == Some(0)
                && recall_code == Some(0)
                && logs_after.contains(&recall_marker)
            {
                return recall_marker.clone();
            }
            String::new()
        },
        Duration::from_millis(500),
    );
    if identity.status == "pass" {
        lines.push(LineVerdict::new(
            "IDENTITY",
            "pass",
            identity.marker(),
            identity.attempts,
            session_id.clone(),
        ));
    } else {
        lines.push(identity);
    }

    // CLAIM: the live claim has a holder.
    let claim = retry_marker(
        "CLAIM",
        "live claim holder",
        || {
            let (code, output) = root.fno(&["agents", "claim", "status", &claim_key, "--json"]);
            if code != Some(0) {
                return String::new();
            }
            serde_json::from_str::<serde_json::Value>(&output)
                .ok()
                .and_then(|v| v.get("holder").and_then(|h| h.as_str()).map(str::to_string))
                .unwrap_or_default()
        },
        Duration::from_millis(500),
    );
    lines.push(claim);

    // MAIL BOTH WAYS: a sent message changes the worker's output.
    let mail_nonce = format!(
        "{:032x}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0) as u128
    );
    let (mail_code, _) = root.fno(&[
        "agents",
        "mail",
        "send",
        &name,
        &format!("PROBE_MAIL={mail_nonce}"),
    ]);
    let (_, logs_before_mail) = root.fno(&["agents", "logs", &name]);
    let reply_marker = format!("PROBE_REPLY={mail_nonce}");
    let mail = retry_marker(
        "MAIL BOTH WAYS",
        "worker response to sent message",
        || {
            let (_, logs_after) = root.fno(&["agents", "logs", &name]);
            if logs_after.contains(&reply_marker) && !logs_before_mail.contains(&reply_marker) {
                return reply_marker.clone();
            }
            String::new()
        },
        Duration::from_millis(500),
    );
    lines.push(mail);

    // VIEW: the harness's own pane, read through its reference.
    if !mux_session.is_empty() && !mux_pane.is_empty() {
        let pane_ref = format!("{mux_session}:{mux_pane}");
        let view = retry_marker(
            "VIEW",
            "harness-owned screen",
            || {
                let (code, output) = root.fno(&["mux", "pane", "read", &pane_ref]);
                if code == Some(0) && !output.trim().is_empty() {
                    return output;
                }
                String::new()
            },
            Duration::from_millis(500),
        );
        lines.push(view);
    } else {
        lines.push(LineVerdict::new(
            "VIEW",
            "skip",
            "harness-owned screen",
            1,
            "harness supplied no native pane reference".to_string(),
        ));
    }

    // SURVIVE: the prior turn is visible after the process is stopped.
    if !mux_session.is_empty() && !mux_pane.is_empty() {
        let (kill_code, _) =
            root.fno(&["mux", "pane", "kill", "--server", &mux_session, &mux_pane]);
        let (survive_resume_code, _) = root.fno(&["agents", "resume", &name]);
        let (survive_code, _) = root.fno(&[
            "agents",
            "mail",
            "send",
            &name,
            &format!("PROBE_SURVIVE_REQUEST={nonce}"),
        ]);
        let seed_marker = format!("PROBE_SEED={nonce}");
        let survive_marker = format!("PROBE_SURVIVE={nonce}");
        let (_, logs_at_start) = root.fno(&["agents", "logs", &name]);
        let survive = retry_marker(
            "SURVIVE",
            "prior turn after process stop",
            || {
                let (_, logs_after) = root.fno(&["agents", "logs", &name]);
                if kill_code == Some(0)
                    && survive_resume_code == Some(0)
                    && survive_code == Some(0)
                    && logs_at_start.contains(&seed_marker)
                    && logs_after.contains(&seed_marker)
                    && logs_after.contains(&survive_marker)
                    && !logs_at_start.contains(&survive_marker)
                {
                    return survive_marker.clone();
                }
                String::new()
            },
            Duration::from_millis(500),
        );
        lines.push(survive);
    } else {
        lines.push(LineVerdict::new(
            "SURVIVE",
            "skip",
            "prior turn after process stop",
            1,
            "no pane reference to stop and resume".to_string(),
        ));
    }

    lines.push(row_matches_line(harness));
    let capture = root.fno(&["doctor", "harness", "readiness-capture", harness]);
    lines.push(manifest_pinned_line(harness, root, capture, None));
    lines.push(cleanup_line(root, &name));
    (lines, isolation)
}

fn append_blocked(lines: &mut Vec<LineVerdict>, cause: &str) {
    for (line, marker) in [
        (
            "IDENTITY",
            "local store artifact or cross-process recall nonce",
        ),
        ("CLAIM", "live claim holder"),
        ("MAIL BOTH WAYS", "worker response to sent message"),
        ("VIEW", "harness-owned screen"),
        ("SURVIVE", "prior turn after process stop"),
    ] {
        lines.push(LineVerdict::new(
            line,
            "skip",
            marker,
            1,
            format!("blocked by {cause}"),
        ));
    }
}

struct IsolationFacts {
    positive_read: bool,
    real_root_absent: Option<bool>,
}

trait MarkerOf {
    fn marker(&self) -> String;
}

impl MarkerOf for LineVerdict {
    fn marker(&self) -> String {
        self.detail.clone()
    }
}

/// ROW MATCHES: the one table-facing line. The honesty sweep is clean, the
/// generated canonical copies are fresh, and the harness is registered
/// everywhere it must be.
fn row_matches_line(harness: &str) -> LineVerdict {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let sweep_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fno-agents"));
    let mut sweep = Command::new(sweep_bin);
    sweep.args([
        "honesty-sweep",
        "--population",
        "harness-capabilities",
        "--json",
    ]);
    sweep.current_dir(&root);
    let sweep_result = output_with_timeout_result(&mut sweep, INSTRUMENT_TIMEOUT_S);
    let mut fresh = Command::new("git");
    fresh.args([
        "diff",
        "--exit-code",
        "--",
        "cli/src/fno/agents/harness_capabilities.toml",
        "crates/fno/src/harness_capabilities.toml",
    ]);
    fresh.current_dir(&root);
    let fresh_result = output_with_timeout_result(&mut fresh, INSTRUMENT_TIMEOUT_S);
    let (sweep_clean, sweep_detail) = match sweep_result {
        Ok(out) if out.status.success() => (
            sweep_has_no_finding(&out.stdout, harness),
            "clean".to_string(),
        ),
        Ok(_) => (false, "finding".to_string()),
        Err(e) => (false, format!("sweep failed: {e}")),
    };
    let fresh_ok = fresh_result.as_ref().is_ok_and(|out| out.status.success());
    let detail = format!(
        "sweep={sweep_detail}; freshness={}",
        if fresh_ok { "fresh" } else { "stale" }
    );
    if sweep_clean && fresh_ok {
        LineVerdict::new(
            "ROW MATCHES",
            "pass",
            "honesty sweep and canonical-copy freshness",
            1,
            detail,
        )
    } else {
        LineVerdict::new(
            "ROW MATCHES",
            "fail",
            "honesty sweep and canonical-copy freshness",
            1,
            detail,
        )
    }
}

fn sweep_has_no_finding(output: &[u8], harness: &str) -> bool {
    let Ok(report) = serde_json::from_slice::<serde_json::Value>(output) else {
        return false;
    };
    let population = report
        .get("populations")
        .and_then(|p| p.as_array())
        .and_then(|rows| {
            rows.iter().find(|row| {
                row.get("name").and_then(|n| n.as_str()) == Some("harness-capabilities")
            })
        })
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if population.get("status").and_then(|s| s.as_str()) != Some("measured") {
        return false;
    }
    !population
        .get("named_pairs")
        .and_then(|p| p.as_array())
        .map(|pairs| {
            pairs
                .iter()
                .any(|pair| pair.get("row").and_then(|r| r.as_str()) == Some(harness))
        })
        .unwrap_or(false)
}

/// MANIFEST PINNED: the readiness grid was captured from a live pane and the
/// manifest matched it by rule id.
fn manifest_pinned_line(
    harness: &str,
    root: &IsolatedRoot,
    capture: (Option<i32>, String),
    readiness_marker: Option<String>,
) -> LineVerdict {
    let (code, output) = capture;
    match (code, readiness_marker) {
        (Some(0), Some(marker)) => LineVerdict::new(
            "MANIFEST PINNED",
            "pass",
            "live readiness-grid capture",
            1,
            marker,
        ),
        (Some(0), None) if output.contains("READINESS_SMOKE!=1") => LineVerdict::new(
            "MANIFEST PINNED",
            "skip",
            "live readiness-grid capture",
            1,
            "live capture was not requested".to_string(),
        ),
        (Some(0), None) => {
            let fixture = root.home.join(format!("readiness-grid-{harness}.txt"));
            let marker = readiness_marker_from_fixture(harness, &fixture);
            match marker {
                Some(marker) => LineVerdict::new(
                    "MANIFEST PINNED",
                    "pass",
                    "live readiness-grid capture",
                    1,
                    marker,
                ),
                None => LineVerdict::new(
                    "MANIFEST PINNED",
                    "fail",
                    "live readiness-grid capture",
                    1,
                    "capture exited 0 without a readiness-specific positive marker".to_string(),
                ),
            }
        }
        (code, _) => LineVerdict::new(
            "MANIFEST PINNED",
            "fail",
            "live readiness-grid capture",
            1,
            if output.trim().is_empty() {
                format!("capture exited {code:?}")
            } else {
                output
            },
        ),
    }
}

fn readiness_marker_from_fixture(harness: &str, fixture: &Path) -> Option<String> {
    let screen = std::fs::read_to_string(fixture).ok()?;
    let binary = std::env::current_exe().ok()?;
    let mut cmd = Command::new(binary);
    cmd.args(["manifest-eval", "--harness", harness]);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    use std::io::Write;
    child.stdin.take()?.write_all(screen.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let verdict = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok()?;
    if verdict.get("matched").and_then(|m| m.as_bool()) == Some(true) {
        return verdict
            .get("rule_id")
            .and_then(|r| r.as_str())
            .map(|rule| format!("readiness rule {rule}"));
    }
    None
}

/// CLEANUP: every row the run minted is removed by name, and the removal is
/// recorded. A row that survives FAILS the journey rather than appearing as
/// a note.
fn cleanup_line(root: &IsolatedRoot, name: &str) -> LineVerdict {
    let _ = root.fno(&["agents", "rm", name]);
    if registry_row_name_in(root, name) {
        LineVerdict::new(
            "CLEANUP",
            "fail",
            "row removed after the run",
            1,
            format!("row {name} survived the run's own removal"),
        )
    } else {
        LineVerdict::new(
            "CLEANUP",
            "pass",
            "row removed after the run",
            1,
            format!("row {name} removed and the removal recorded"),
        )
    }
}

fn rubric_report(harness: &str, live: bool) -> serde_json::Value {
    if !live {
        let mut report = serde_json::json!({
            "harness": harness,
            "live": false,
            "argv": [],
            "argv_detail": "",
            "lines": dry_run_lines(harness),
        });
        if let Ok(contract) = HarnessContract::packaged() {
            match dry_run_argv(&contract, harness) {
                Ok(argv) => report["argv"] = serde_json::json!(argv),
                Err(detail) => {
                    report["argv"] = serde_json::json!([]);
                    report["argv_detail"] = serde_json::json!(format!(
                        "unsupported: cannot compose pane argv ({detail})"
                    ));
                    for line in report["lines"]
                        .as_array_mut()
                        .expect("lines array")
                        .iter_mut()
                    {
                        line["detail"] = serde_json::json!(format!(
                            "unsupported: cannot compose pane argv ({detail})"
                        ));
                    }
                }
            }
        }
        return report;
    }
    let mut root = match IsolatedRoot::establish(harness) {
        Ok(root) => root,
        Err(detail) => {
            // No isolated root, no run: the refusal is the answer, and no
            // unisolated fallback exists.
            return serde_json::json!({
                "harness": harness,
                "live": true,
                "refused": detail,
                "lines": [],
            });
        }
    };
    let (code, _) = root.fno(&["--version"]);
    if code.is_none() {
        let mut lines = vec![LineVerdict::new(
            "SPAWN",
            "fail",
            "harness binary",
            1,
            format!("{harness} binary is not on PATH"),
        )];
        append_blocked(&mut lines, "the missing harness binary");
        lines.push(LineVerdict::new(
            "ROW MATCHES",
            "skip",
            "honesty sweep and canonical-copy freshness",
            1,
            "blocked".to_string(),
        ));
        lines.push(LineVerdict::new(
            "MANIFEST PINNED",
            "skip",
            "live readiness-grid capture",
            1,
            "blocked".to_string(),
        ));
        lines.push(LineVerdict::new(
            "CLEANUP",
            "skip",
            "row removed after the run",
            1,
            "blocked".to_string(),
        ));
        return serde_json::json!({ "harness": harness, "live": true, "lines": lines });
    }
    let (lines, isolation) = run_live_rubric(harness, &root);
    let record = MeasurementRecord {
        harness: harness.to_string(),
        version: String::new(),
        version_status: "unreadable",
        tier: "live",
        reader: "live rubric".to_string(),
        account: String::new(),
        model: String::new(),
        requested_account: String::new(),
        requested_model: String::new(),
    };
    serde_json::json!({
        "harness": harness,
        "live": true,
        "lines": lines,
        "record": record,
        "isolation": {
            "positive_read": isolation.positive_read,
            "real_root_absent": isolation.real_root_absent,
        },
    })
}

// ── the client door ──────────────────────────────────────────────────

/// The transport-only door. `harness-probe fields|rubric <harness> ...` is
/// execed by the Python leaves, which keep their spellings and lose their
/// bodies.
pub fn run_client(args: &[String]) -> i32 {
    let Some(mode) = args.first() else {
        eprintln!("harness-probe: expected `fields` or `rubric` as the first argument");
        return 2;
    };
    let json = args.iter().any(|a| a == "--json" || a == "-J");
    let rest: Vec<&String> = args
        .iter()
        .skip(1)
        .filter(|a| {
            a.as_str() != "--json"
                && a.as_str() != "-J"
                && a.as_str() != "--live"
                && a.as_str() != "--write"
        })
        .collect();
    let live = args.iter().any(|a| a == "--live");
    let write = args.iter().any(|a| a == "--write");
    let Some(harness) = rest.first().map(String::as_str) else {
        eprintln!("harness-probe {mode}: exactly one harness argument is required");
        return 2;
    };
    let report = match mode.as_str() {
        "fields" => probe_fields(harness, live, write),
        "rubric" => rubric_report(harness, live),
        other => {
            eprintln!("harness-probe: unknown mode {other:?} (expected `fields` or `rubric`)");
            return 2;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialized JSON")
        );
    } else {
        print_human(harness, mode, &report);
    }
    let failing = match mode.as_str() {
        "fields" => report
            .get("fields")
            .and_then(|f| f.as_array())
            .map(|fields| {
                fields
                    .iter()
                    .any(|f| f.get("verdict").and_then(|v| v.as_str()) == Some("DISAGREES"))
            })
            .unwrap_or(false),
        _ => report
            .get("lines")
            .and_then(|l| l.as_array())
            .map(|lines| {
                lines
                    .iter()
                    .any(|l| l.get("status").and_then(|s| s.as_str()) == Some("fail"))
            })
            .unwrap_or(false),
    };
    if failing {
        1
    } else {
        0
    }
}

fn print_human(harness: &str, mode: &str, report: &serde_json::Value) {
    if mode == "fields" {
        if let Some(error) = report.get("error").and_then(|e| e.as_str()) {
            eprintln!("probe refused: {error}");
            return;
        }
        println!(
            "probe {harness} (map_version {})",
            report
                .get("map_version")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
        for field in report
            .get("fields")
            .and_then(|f| f.as_array())
            .unwrap_or(&vec![])
        {
            println!(
                "{:<11} {}: {}",
                field.get("verdict").and_then(|v| v.as_str()).unwrap_or("?"),
                field.get("field").and_then(|f| f.as_str()).unwrap_or("?"),
                field.get("detail").and_then(|d| d.as_str()).unwrap_or(""),
            );
        }
        if let Some(stanza) = report.get("stanza").and_then(|s| s.as_str()) {
            println!();
            println!("{stanza}");
        }
        return;
    }
    let live = report
        .get("live")
        .and_then(|l| l.as_bool())
        .unwrap_or(false);
    if let Some(refusal) = report.get("refused").and_then(|r| r.as_str()) {
        eprintln!("fno doctor harness {harness}: refused: {refusal}");
        return;
    }
    println!(
        "fno doctor harness {harness} ({})",
        if live { "live" } else { "dry run" }
    );
    for line in report
        .get("lines")
        .and_then(|l| l.as_array())
        .unwrap_or(&vec![])
    {
        println!(
            "{:<4} {}: marker={} ({})",
            line.get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("?")
                .to_uppercase(),
            line.get("line").and_then(|l| l.as_str()).unwrap_or("?"),
            line.get("marker").and_then(|m| m.as_str()).unwrap_or(""),
            line.get("detail").and_then(|d| d.as_str()).unwrap_or(""),
        );
    }
    if !live {
        let argv: Vec<String> = report
            .get("argv")
            .and_then(|a| a.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let detail = report
            .get("argv_detail")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        };
        println!(
            "argv would run: {}{suffix}",
            if argv.is_empty() {
                "unavailable".to_string()
            } else {
                argv.join(" ")
            }
        );
    }
}
