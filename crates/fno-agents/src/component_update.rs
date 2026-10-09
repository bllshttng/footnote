//! Deployed-component convergence verdict.
//!
//! Pure decision over probe data: classify each deployed component (the
//! Python tool and the four cargo binaries) as Fresh, Updated, Stale,
//! Missing, Failed or Unknown, and name the executable repair command for
//! each. The probes (`<bin> version --json`, the installed-rev marker) are
//! instruments the Python transport runs; the classification lives here so
//! `fno doctor update` and `fno doctor` cannot fork it. A no-op success or an
//! attempted build is never convergence: only a post-effect probe matching
//! the expected revision proves a component.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PYTHON_TOOL: &str = "python-tool";
pub const MUX_FRONT_DOOR: &str = "fno";
pub const AGENTS_CLIENT: &str = "fno-agents";
pub const AGENTS_DAEMON: &str = "fno-agents-daemon";
pub const AGENTS_WORKER: &str = "fno-agents-worker";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Fresh,
    Updated,
    Stale,
    Missing,
    Failed,
    Unknown,
}

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Status::Fresh => "fresh",
            Status::Updated => "updated",
            Status::Stale => "stale",
            Status::Missing => "missing",
            Status::Failed => "failed",
            Status::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ComponentProbe {
    pub component: String,
    #[serde(default)]
    pub expected_rev: Option<String>,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub pre_rev: Option<String>,
    #[serde(default)]
    pub post_rev: Option<String>,
    /// Positive evidence the reported rev is a NEWER git descendant of the
    /// expected rev (`git merge-base --is-ancestor`). A deploy that lands a
    /// build newer than the rev this pass expected is not behind; only a
    /// proven descendant passes on a mismatched rev. Absent means not proven,
    /// so a mismatched rev without it stays Stale/Failed.
    #[serde(default)]
    pub observed_is_descendant: Option<bool>,
    /// Why the post-effect probe could not answer (named instrument, AC3-HP).
    #[serde(default)]
    pub instrument_error: Option<String>,
    /// Positive evidence that contradicts a matching revision (the installed
    /// bytes differ while a marker reads fresh). Stale wins over the rev.
    #[serde(default)]
    pub contradicting_evidence: Option<String>,
    #[serde(default)]
    pub effect_attempted: bool,
    #[serde(default)]
    pub effect_ok: Option<bool>,
    /// Positive evidence that a probe answered only after the inode repair:
    /// names the classification that triggered it. Rendered even on a fresh
    /// row so a repair is never invisible in the update output.
    #[serde(default)]
    pub repair_evidence: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VerdictRequest {
    /// Default expected revision: the crates/ subtree rev for cargo bins.
    /// A probe may override (the Python tool expects the source rev).
    pub expected_rev: String,
    #[serde(default)]
    pub crates_agents_dir: Option<String>,
    #[serde(default)]
    pub crates_mux_dir: Option<String>,
    pub components: Vec<ComponentProbe>,
}

#[derive(Debug, Serialize)]
pub struct ComponentVerdict {
    pub component: String,
    pub status: Status,
    pub expected_rev: Option<String>,
    pub observed_rev: Option<String>,
    pub executable: Option<String>,
    pub repair: Option<String>,
    pub detail: Option<String>,
    /// The operator-facing one-liner (no log prefix). Built beside the verdict
    /// so every surface renders the evidence identically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

/// The one-liner for a non-fresh row: status, both revs, the named instrument
/// or failure detail, and the executable repair command.
fn render_line(v: &ComponentVerdict) -> String {
    let rev_text = match &v.observed_rev {
        Some(r) => format!("rev {}", &r[..r.len().min(12)]),
        None => "no revision reported".to_string(),
    };
    let exp_text = match &v.expected_rev {
        Some(e) => format!("expected {}", &e[..e.len().min(12)]),
        None => "unknown expected rev".to_string(),
    };
    let mut line = format!(
        "component {}: {} ({}, {})",
        v.component,
        v.status.as_str(),
        rev_text,
        exp_text
    );
    if let Some(d) = &v.detail {
        line.push_str(&format!("; {d}"));
    }
    if let Some(r) = &v.repair {
        line.push_str(&format!("; repair: {r}"));
    }
    line
}

#[derive(Debug, Serialize)]
pub struct VerdictReport {
    pub converged: bool,
    pub components: Vec<ComponentVerdict>,
}

/// Shell-quote a path for display inside a repair command, so a source
/// checkout with spaces or metacharacters renders an executable command.
fn shell_quote(path: &str) -> String {
    let safe = !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c));
    if safe {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

fn repair_command(component: &str, req: &VerdictRequest) -> Option<String> {
    match component {
        PYTHON_TOOL => Some("fno doctor update".to_string()),
        MUX_FRONT_DOOR => Some(match &req.crates_mux_dir {
            Some(dir) => format!("cargo install --path {} --bins", shell_quote(dir)),
            None => "fno doctor update --rust".to_string(),
        }),
        AGENTS_CLIENT | AGENTS_DAEMON | AGENTS_WORKER => Some(match &req.crates_agents_dir {
            Some(dir) => format!("cargo install --path {} --bins", shell_quote(dir)),
            None => "fno doctor update --rust".to_string(),
        }),
        _ => None,
    }
}

/// The per-component decision. Order matters: an instrument that could not
/// answer gates everything (a probe failure must never collapse into fresh or
/// missing), then presence, then revision match. Fresh and Updated rows need
/// no one-liner; every other row renders beside its verdict.
pub fn classify(probe: &ComponentProbe, req: &VerdictRequest) -> ComponentVerdict {
    let expected = probe
        .expected_rev
        .clone()
        .unwrap_or_else(|| req.expected_rev.clone());
    let observed = probe.post_rev.clone().or_else(|| probe.pre_rev.clone());
    let mut v = ComponentVerdict {
        component: probe.component.clone(),
        status: Status::Unknown,
        expected_rev: Some(expected.clone()),
        observed_rev: observed,
        executable: probe.executable.clone(),
        repair: None,
        detail: None,
        line: None,
    };
    if let Some(err) = &probe.instrument_error {
        v.detail = Some(err.clone());
        v.line = Some(render_line(&v));
        return v;
    }
    if let Some(ev) = &probe.contradicting_evidence {
        v.status = Status::Stale;
        v.detail = Some(ev.clone());
        v.line = Some(render_line(&v));
        return v;
    }
    v.repair = repair_command(&probe.component, req);
    let attempted = probe.effect_attempted;
    let deployed = match &probe.executable {
        None => {
            // Nothing on disk: an attempted deploy that left nothing is a
            // failed deploy, not a missing component.
            v.status = if attempted {
                Status::Failed
            } else {
                Status::Missing
            };
            if attempted {
                v.detail = Some("deploy attempted but no executable landed".to_string());
            }
            v.line = Some(render_line(&v));
            return v;
        }
        Some(_) => probe.post_rev.clone(),
    };
    match deployed {
        Some(rev) if rev == expected => {
            v.status = if attempted {
                Status::Updated
            } else {
                Status::Fresh
            };
        }
        Some(rev) if probe.observed_is_descendant == Some(true) && rev != expected => {
            // A deployed build NEWER than the rev this pass expected is not
            // behind (a second deploy landed mid-flight, or the source moved
            // between the expected-rev read and the post-deploy probe). The
            // descendant proof is named in the detail so a converged verdict
            // with differing revs is never a silent pass.
            v.status = if attempted {
                Status::Updated
            } else {
                Status::Fresh
            };
            v.detail = Some(format!(
                "deployed rev {rev} is a newer descendant of the expected rev; accepted as newer"
            ));
        }
        Some(_) => {
            v.status = if attempted {
                Status::Failed
            } else {
                Status::Stale
            };
            if attempted {
                v.detail = Some(
                    "deployed revision still differs from source after the refresh".to_string(),
                );
            }
            v.line = Some(render_line(&v));
        }
        None => {
            // Present but silent: fail toward repair, never toward fresh.
            v.status = if attempted {
                Status::Failed
            } else {
                Status::Stale
            };
            v.detail = Some("executable present but does not self-report a revision".to_string());
            v.line = Some(render_line(&v));
        }
    }
    // AC1-RECEIPT: a probe that answered only after the inode repair renders
    // its evidence even when the re-probe made the row fresh.
    if let Some(ev) = &probe.repair_evidence {
        let note = format!("path repaired after {ev}");
        v.detail = Some(match v.detail.take() {
            Some(d) => format!("path repaired after {ev}; {d}"),
            None => note,
        });
        v.line = Some(render_line(&v));
    }
    v
}

/// Converged only when every component proves Fresh or Updated from its own
/// post-effect observation (AC2-EDGE).
pub fn verdict(req: &VerdictRequest) -> VerdictReport {
    let components: Vec<ComponentVerdict> =
        req.components.iter().map(|p| classify(p, req)).collect();
    let converged = !components.is_empty()
        && components
            .iter()
            .all(|c| matches!(c.status, Status::Fresh | Status::Updated));
    VerdictReport {
        converged,
        components,
    }
}

/// One probe of a deployed executable through the shared exec-prover: it
/// classifies the exec, repairs a poisoned path at most once (the kill-shaped
/// classifications), and re-probes. Returns (rev, python_script,
/// instrument_error, repair_evidence): the error names the instrument so an
/// unanswerable probe lands at the classifier as Unknown (never collapsed into
/// fresh or missing), and the evidence names the classification that triggered
/// a repair so a healed path is visible in the verdict.
fn probe_binary(
    path: &std::path::Path,
    timeout: Duration,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let outcome =
        crate::install_verify::verify_and_repair(path, timeout, crate::install_verify::probe_exec);
    (
        outcome.rev,
        outcome.script,
        outcome.instrument_error,
        outcome.repaired_after,
    )
}

/// True when `observed` is a git descendant of `expected` (expected is an
/// ancestor of observed): the deployed build is NEWER than the rev this pass
/// expected, which is convergence, not staleness. `None` when git cannot
/// answer (no repo, unknown or ambiguous rev) - the caller then fails toward
/// the exact-match rule, never toward a false fresh. `repo_dir` anchors the
/// git call at the source checkout the revs belong to; prefixes are accepted
/// when unambiguous.
fn rev_is_descendant(expected: &str, observed: &str, repo_dir: &Path) -> Option<bool> {
    if expected == observed || expected.is_empty() || observed.is_empty() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args([
            "-C",
            repo_dir.to_str()?,
            "merge-base",
            "--is-ancestor",
            expected,
            observed,
        ])
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

struct ProbeArgs {
    bindir: PathBuf,
    expected: String,
    attempted: bool,
    include_mux: bool,
    agents_dir: Option<String>,
    mux_dir: Option<String>,
    python_expected: Option<String>,
    python_rev: Option<String>,
    python_error: Option<String>,
    python_evidence: Option<String>,
}

fn parse_probe_args(args: &[String]) -> Result<ProbeArgs, String> {
    let mut p = ProbeArgs {
        bindir: PathBuf::new(),
        expected: String::new(),
        attempted: false,
        include_mux: false,
        agents_dir: None,
        mux_dir: None,
        python_expected: None,
        python_rev: None,
        python_error: None,
        python_evidence: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next()
                .ok_or_else(|| format!("{name} needs a value"))
                .cloned()
        };
        match a.as_str() {
            "--attempted" => p.attempted = true,
            "--include-mux" => p.include_mux = true,
            "--bindir" => p.bindir = PathBuf::from(value("--bindir")?),
            "--expected" => p.expected = value("--expected")?,
            "--agents-dir" => p.agents_dir = Some(value("--agents-dir")?),
            "--mux-dir" => p.mux_dir = Some(value("--mux-dir")?),
            "--python-expected" => p.python_expected = Some(value("--python-expected")?),
            "--python-rev" => {
                let v = value("--python-rev")?;
                p.python_rev = Some(v).filter(|v| v != "-");
            }
            "--python-error" => p.python_error = Some(value("--python-error")?),
            "--python-evidence" => p.python_evidence = Some(value("--python-evidence")?),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if p.bindir.as_os_str().is_empty() {
        return Err("--bindir is required".to_string());
    }
    if p.expected.is_empty() {
        return Err("--expected is required".to_string());
    }
    Ok(p)
}

/// Probe the cargo components in `bindir` (plus the mux front door and the
/// python-tool row when requested) and classify. Converged only when every
/// row proves Fresh or Updated from its own probe.
fn verdict_from_probe(p: &ProbeArgs) -> VerdictReport {
    let probe_timeout = Duration::from_secs(20);
    let exe = if cfg!(windows) { ".exe" } else { "" };
    let mut components: Vec<ComponentProbe> = Vec::new();
    let mut python_exec: Option<String> = None;
    for (name, stem) in [
        (format!("fno-agents{exe}"), AGENTS_CLIENT),
        (format!("fno-agents-daemon{exe}"), AGENTS_DAEMON),
        (format!("fno-agents-worker{exe}"), AGENTS_WORKER),
    ] {
        let path = p.bindir.join(&name);
        let present = path.is_file();
        let (rev, _script, err, evidence) = if present {
            probe_binary(&path, probe_timeout)
        } else {
            (None, None, None, None)
        };
        components.push(ComponentProbe {
            component: stem.to_string(),
            expected_rev: None,
            executable: if present {
                Some(path.to_string_lossy().into_owned())
            } else {
                None
            },
            pre_rev: None,
            post_rev: rev,
            instrument_error: err,
            contradicting_evidence: None,
            effect_attempted: p.attempted,
            effect_ok: None,
            observed_is_descendant: None,
            repair_evidence: evidence,
        });
    }
    if p.include_mux {
        let path = p.bindir.join(format!("fno{exe}"));
        let present = path.is_file();
        let (rev, script, err, evidence) = if present {
            probe_binary(&path, probe_timeout)
        } else {
            (None, None, None, None)
        };
        python_exec = script;
        components.push(ComponentProbe {
            component: MUX_FRONT_DOOR.to_string(),
            expected_rev: None,
            executable: if present {
                Some(path.to_string_lossy().into_owned())
            } else {
                None
            },
            pre_rev: None,
            post_rev: rev,
            instrument_error: err,
            contradicting_evidence: None,
            effect_attempted: p.attempted,
            effect_ok: None,
            observed_is_descendant: None,
            repair_evidence: evidence,
        });
    }
    if p.python_rev.is_some() || p.python_error.is_some() {
        // A requested python-tool row is emitted even when the marker could
        // not be read: that state is Unknown with the named instrument, never
        // a silent omission from the summary.
        components.push(ComponentProbe {
            component: PYTHON_TOOL.to_string(),
            expected_rev: p.python_expected.clone(),
            executable: python_exec.or(Some("<unresolved python>".to_string())),
            pre_rev: None,
            post_rev: p.python_rev.clone(),
            instrument_error: p.python_error.clone(),
            contradicting_evidence: p.python_evidence.clone(),
            effect_attempted: p.attempted,
            effect_ok: None,
            observed_is_descendant: None,
            repair_evidence: None,
        });
    }
    stamp_newer_descendants(&mut components, &p.expected, p.agents_dir.as_deref());
    let req = VerdictRequest {
        expected_rev: p.expected.clone(),
        crates_agents_dir: p.agents_dir.clone(),
        crates_mux_dir: p.mux_dir.clone(),
        components,
    };
    verdict(&req)
}

/// Stamp `observed_is_descendant` on every cargo row whose reported rev
/// differs from the expected rev. The triad bins share one build, so the
/// ancestry answer is computed once per DISTINCT observed rev and reused;
/// a missing `--agents-dir` (no repo to anchor the git call) leaves the
/// field unset and the mismatch stale, exactly as before.
fn stamp_newer_descendants(
    components: &mut [ComponentProbe],
    expected_rev: &str,
    agents_dir: Option<&str>,
) {
    let Some(repo_dir) = agents_dir.map(std::path::Path::new) else {
        return;
    };
    if !repo_dir.is_dir() {
        return;
    }
    let mut cache: std::collections::HashMap<(String, String), Option<bool>> =
        std::collections::HashMap::new();
    for probe in components.iter_mut() {
        if probe.component == PYTHON_TOOL {
            continue;
        }
        let Some(rev) = probe.post_rev.clone() else {
            continue;
        };
        let expected = probe
            .expected_rev
            .clone()
            .unwrap_or_else(|| expected_rev.to_string());
        if rev == expected {
            continue;
        }
        let answer = *cache
            .entry((expected.clone(), rev.clone()))
            .or_insert_with(|| rev_is_descendant(&expected, &rev, repo_dir));
        if answer == Some(true) {
            probe.observed_is_descendant = Some(true);
        }
    }
}

/// `fno-agents component-verdict`: probe + classify + print. Exit 0 whenever a
/// verdict was computed - a not-converged fleet is data, not an error; exit 2
/// on malformed args.
pub fn run_component_verdict(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usage: fno-agents component-verdict --bindir <dir> --expected <rev>\n\
             [--attempted] [--include-mux] [--agents-dir <dir>] [--mux-dir <dir>]\n\
             [--python-expected <rev>] [--python-rev <rev|->] [--python-evidence <text>]"
        );
        return 0;
    }
    let p = match parse_probe_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno-agents component-verdict: {e}");
            return 2;
        }
    };
    match serde_json::to_string(&verdict_from_probe(&p)) {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => {
            eprintln!("fno-agents component-verdict: serialization error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(components: Vec<ComponentProbe>) -> VerdictRequest {
        VerdictRequest {
            expected_rev: "abc123".to_string(),
            crates_agents_dir: Some("/src/crates/fno-agents".to_string()),
            crates_mux_dir: Some("/src/crates/fno".to_string()),
            components,
        }
    }

    fn probe(component: &str, post_rev: Option<&str>) -> ComponentProbe {
        ComponentProbe {
            component: component.to_string(),
            expected_rev: None,
            executable: Some(format!("/bin/{component}")),
            pre_rev: None,
            post_rev: post_rev.map(|s| s.to_string()),
            instrument_error: None,
            contradicting_evidence: None,
            effect_attempted: false,
            effect_ok: None,
            observed_is_descendant: None,
            repair_evidence: None,
        }
    }

    #[test]
    fn repair_paths_with_metacharacters_are_shell_quoted() {
        let mut r = req(vec![probe(AGENTS_CLIENT, Some("old"))]);
        r.crates_agents_dir = Some("/src/my crates/fno-agents".to_string());
        let v = verdict(&r).components.remove(0);
        assert_eq!(
            v.repair.as_deref(),
            Some("cargo install --path '/src/my crates/fno-agents' --bins")
        );
    }

    #[test]
    fn ac1_hp_fresh_triad_failed_mux_never_converges() {
        // Fresh triad, stale mux front door whose repair attempt failed:
        // the mux verdict is Failed with an executable repair command, and
        // the fleet is not converged.
        let mut mux = probe(MUX_FRONT_DOOR, None);
        mux.executable = None;
        mux.effect_attempted = true;
        let r = verdict(&req(vec![
            probe(AGENTS_CLIENT, Some("abc123")),
            probe(AGENTS_DAEMON, Some("abc123")),
            probe(AGENTS_WORKER, Some("abc123")),
            mux,
        ]));
        let mux_v = r
            .components
            .iter()
            .find(|c| c.component == MUX_FRONT_DOOR)
            .unwrap();
        assert_eq!(mux_v.status, Status::Failed);
        assert!(mux_v
            .repair
            .as_deref()
            .unwrap()
            .contains("cargo install --path /src/crates/fno"));
        assert!(!r.converged);
    }

    #[test]
    fn ac1_edge_dry_run_names_stale_without_claiming_update() {
        // No effect ran: a mismatching component is Stale, never Updated,
        // and the repair command is named.
        let r = verdict(&req(vec![probe(MUX_FRONT_DOOR, Some("old999"))]));
        let v = &r.components[0];
        assert_eq!(v.status, Status::Stale);
        assert_eq!(v.observed_rev.as_deref(), Some("old999"));
        assert_eq!(v.expected_rev.as_deref(), Some("abc123"));
        assert!(v.repair.is_some());
        assert!(!r.converged);
    }

    #[test]
    fn updated_requires_post_match_and_attempted_effect() {
        let mut p = probe(AGENTS_CLIENT, Some("abc123"));
        p.effect_attempted = true;
        assert!(verdict(&req(vec![p])).converged);

        let mut q = probe(AGENTS_CLIENT, Some("abc123"));
        q.effect_attempted = false;
        assert_eq!(verdict(&req(vec![q])).components[0].status, Status::Fresh);

        let mut r = probe(AGENTS_CLIENT, Some("stale"));
        r.effect_attempted = true;
        assert_eq!(verdict(&req(vec![r])).components[0].status, Status::Failed);
    }

    #[test]
    fn ac3_hp_instrument_failure_is_unknown_not_fresh_or_missing() {
        let mut p = probe(AGENTS_WORKER, Some("abc123"));
        p.instrument_error = Some("fno-agents-worker hung on version --json (>20s)".to_string());
        let v = verdict(&req(vec![p])).components.remove(0);
        assert_eq!(v.status, Status::Unknown);
        assert_eq!(v.repair, None);
        assert!(v.detail.as_deref().unwrap().contains("hung"));
    }

    #[test]
    fn present_but_silent_binary_fails_toward_stale() {
        let mut p = probe(AGENTS_DAEMON, None);
        p.executable = Some("/bin/fno-agents-daemon".to_string());
        let v = verdict(&req(vec![p])).components.remove(0);
        assert_eq!(v.status, Status::Stale);
        assert!(v.detail.as_deref().unwrap().contains("self-report"));
    }

    #[test]
    fn repaired_row_renders_evidence_even_when_fresh() {
        let mut p = probe(AGENTS_CLIENT, Some("abc123"));
        p.repair_evidence = Some("signal(9)".to_string());
        let r = verdict(&req(vec![p]));
        assert!(r.converged);
        let v = &r.components[0];
        assert_eq!(v.status, Status::Fresh);
        assert_eq!(v.detail.as_deref(), Some("path repaired after signal(9)"));
        let line = v.line.as_deref().unwrap();
        assert!(line.contains("repaired after signal(9)"));
        assert!(line.contains("fresh"));
    }

    #[test]
    fn missing_component_names_install_repair() {
        let mut p = probe(AGENTS_WORKER, None);
        p.executable = None;
        let v = verdict(&req(vec![p])).components.remove(0);
        assert_eq!(v.status, Status::Missing);
        assert_eq!(
            v.repair.as_deref(),
            Some("cargo install --path /src/crates/fno-agents --bins")
        );
    }

    #[test]
    fn python_tool_expects_source_rev_and_update_repair() {
        let mut p = probe(PYTHON_TOOL, Some("def456"));
        p.expected_rev = Some("def456".to_string());
        let v = verdict(&req(vec![p])).components.remove(0);
        assert_eq!(v.status, Status::Fresh);
        assert_eq!(v.repair.as_deref(), Some("fno doctor update"));
        assert_eq!(v.expected_rev.as_deref(), Some("def456"));
    }

    #[test]
    fn empty_component_list_is_never_converged() {
        assert!(!verdict(&req(vec![])).converged);
    }

    #[test]
    fn contradicting_evidence_beats_a_matching_revision() {
        // The marker reads fresh but the installed bytes differ: Stale wins
        // over the rev match, and the evidence is carried as the detail.
        let mut p = probe(PYTHON_TOOL, Some("abc123"));
        p.expected_rev = Some("abc123".to_string());
        p.contradicting_evidence = Some("3 .py file(s) on disk differ from source".to_string());
        let v = verdict(&req(vec![p])).components.remove(0);
        assert_eq!(v.status, Status::Stale);
        assert!(v.detail.as_deref().unwrap().contains("differ"));
    }

    #[test]
    fn request_json_round_trips_through_the_verb_contract() {
        let body = serde_json::json!({
            "expected_rev": "abc123",
            "crates_agents_dir": "/src/crates/fno-agents",
            "components": [
                {"component": "fno-agents", "executable": "/bin/fno-agents",
                 "post_rev": "abc123"},
                {"component": "fno", "instrument_error": "exited 1"}
            ]
        });
        let parsed: VerdictRequest = serde_json::from_value(body).unwrap();
        let report = verdict(&parsed);
        assert_eq!(report.components.len(), 2);
        assert!(!report.converged);
        let out = serde_json::to_string(&report).unwrap();
        assert!(out.contains("\"status\":\"fresh\""));
        assert!(out.contains("\"status\":\"unknown\""));
    }

    // ---- probe mode (POSIX: the fixtures are sh scripts) ----

    use crate::write_exec_stub as write_script;

    fn version_script(rev: &str, extra: &str) -> String {
        format!("#!/bin/sh\necho '{{\"crates_rev\": \"{rev}\", \"dirty\": false{extra}}}'\n")
    }

    #[test]
    #[cfg(unix)]
    fn probe_mode_classifies_a_converged_bindir() {
        let dir = std::env::temp_dir().join(format!("fno-cu-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "fno-agents",
            "fno-agents-daemon",
            "fno-agents-worker",
            "fno",
        ] {
            write_script(&dir, n, &version_script("aabbcc", ""));
        }
        let p = parse_probe_args(&[
            "--bindir".to_string(),
            dir.to_string_lossy().into_owned(),
            "--expected".to_string(),
            "aabbcc".to_string(),
            "--include-mux".to_string(),
            "--python-rev".to_string(),
            "-".to_string(),
        ])
        .unwrap();
        let r = verdict_from_probe(&p);
        assert_eq!(r.components.len(), 4);
        assert!(r.converged, "{:?}", r.components);
        let mux = r
            .components
            .iter()
            .find(|c| c.component == MUX_FRONT_DOOR)
            .unwrap();
        assert_eq!(mux.status, Status::Fresh);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn probe_mode_stale_worker_and_garbage_sibling_never_converge() {
        let dir = std::env::temp_dir().join(format!("fno-cu-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_script(&dir, "fno-agents", &version_script("aabbcc", ""));
        write_script(&dir, "fno-agents-daemon", &version_script("aabbcc", ""));
        write_script(&dir, "fno-agents-worker", &version_script("000000", ""));
        // The worker rev is stale; the garbage bin cannot answer at all.
        let p = parse_probe_args(&[
            "--bindir".to_string(),
            dir.to_string_lossy().into_owned(),
            "--expected".to_string(),
            "aabbcc".to_string(),
            "--attempted".to_string(),
            "--agents-dir".to_string(),
            "/src/crates/fno-agents".to_string(),
        ])
        .unwrap();
        let r = verdict_from_probe(&p);
        assert!(!r.converged);
        let worker = &r.components[2];
        assert_eq!(worker.status, Status::Failed);
        assert!(worker
            .repair
            .as_deref()
            .unwrap()
            .starts_with("cargo install --path /src/crates/fno-agents"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn probe_mode_unanswerable_binary_is_unknown_with_named_instrument() {
        let dir = std::env::temp_dir().join(format!("fno-cu-junk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_script(&dir, "fno-agents", "#!/bin/sh\necho not-json-at-all\n");
        write_script(&dir, "fno-agents-daemon", "#!/bin/sh\nexit 3\n");
        write_script(&dir, "fno-agents-worker", &version_script("aabbcc", ""));
        let p = parse_probe_args(&[
            "--bindir".to_string(),
            dir.to_string_lossy().into_owned(),
            "--expected".to_string(),
            "aabbcc".to_string(),
        ])
        .unwrap();
        let r = verdict_from_probe(&p);
        assert!(!r.converged);
        let client = &r.components[0];
        assert_eq!(client.status, Status::Unknown);
        assert!(client
            .detail
            .as_deref()
            .unwrap()
            .contains("unparseable `version --json`"));
        let daemon = &r.components[1];
        assert_eq!(daemon.status, Status::Unknown);
        assert!(daemon.detail.as_deref().unwrap().contains("exited 3"));
        assert_eq!(r.components[2].status, Status::Fresh);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_mode_requires_bindir_and_expected() {
        assert!(parse_probe_args(&[]).is_err());
        assert!(parse_probe_args(&["--bindir".to_string(), "/tmp".to_string()]).is_err());
        assert!(parse_probe_args(&["--wat".to_string()]).is_err());
        assert!(parse_probe_args(&[
            "--bindir".to_string(),
            "/tmp".to_string(),
            "--expected".to_string(),
            "r".to_string()
        ])
        .is_ok());
    }

    #[test]
    fn classify_accepts_a_newer_descendant_rev() {
        let mut p = probe(AGENTS_CLIENT, Some("def456"));
        p.observed_is_descendant = Some(true);
        let v = classify(&p, &req(vec![]));
        assert_eq!(v.status, Status::Fresh);
        assert!(
            v.detail.as_deref().unwrap().contains("newer descendant"),
            "the pass names why the differing revs converged: {:?}",
            v.detail
        );
    }

    #[test]
    fn classify_attempted_descendant_is_updated_not_failed() {
        let mut p = probe(AGENTS_CLIENT, Some("def456"));
        p.observed_is_descendant = Some(true);
        p.effect_attempted = true;
        let v = classify(&p, &req(vec![]));
        assert_eq!(v.status, Status::Updated);
    }

    #[test]
    fn mismatched_rev_without_descendant_proof_stays_stale() {
        let mut p = probe(AGENTS_CLIENT, Some("def456"));
        p.observed_is_descendant = Some(false);
        let v = classify(&p, &req(vec![]));
        assert_eq!(v.status, Status::Stale);
    }

    #[test]
    fn descendant_proof_without_mismatch_changes_nothing() {
        let mut p = probe(AGENTS_CLIENT, Some("abc123"));
        p.observed_is_descendant = Some(true);
        let v = classify(&p, &req(vec![]));
        assert_eq!(v.status, Status::Fresh);
        assert!(
            v.detail.is_none(),
            "an exact match never carries the descendant note"
        );
    }

    /// Run git in `dir`, asserting success; stdout trimmed. Shared by the
    /// ancestry tests.
    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A real temp git repo answers the ancestry question both ways:
    /// child descends from base (Some(true)), base does not descend from
    /// child (Some(false)).
    #[test]
    #[cfg(unix)]
    fn rev_is_descendant_reads_git_ancestry() {
        let dir = std::env::temp_dir().join(format!("fno-cu-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        let base = git(&dir, &["rev-parse", "HEAD"]);
        git(&dir, &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "child",
        ]);
        let child = git(&dir, &["rev-parse", "HEAD"]);
        assert_eq!(rev_is_descendant(&base, &child, &dir), Some(true));
        assert_eq!(rev_is_descendant(&child, &base, &dir), Some(false));
        assert_eq!(rev_is_descendant("nonexistent", &child, &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe pass stamps a descendant rev through the real git path and
    /// the fleet converges with differing revs; an OLDER observed rev stays
    /// Failed under --attempted.
    #[test]
    #[cfg(unix)]
    fn probe_pass_stamps_a_descendant_and_converges() {
        let dir = std::env::temp_dir().join(format!("fno-cu-git2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        let base = git(&dir, &["rev-parse", "HEAD"]);
        git(&dir, &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "child",
        ]);
        let child = git(&dir, &["rev-parse", "HEAD"]);
        write_script(&dir, "fno-agents", &version_script(&child, ""));
        write_script(&dir, "fno-agents-daemon", &version_script(&child, ""));
        write_script(&dir, "fno-agents-worker", &version_script(&child, ""));
        let newer = parse_probe_args(&[
            "--bindir".to_string(),
            dir.to_string_lossy().into_owned(),
            "--expected".to_string(),
            base.clone(),
            "--agents-dir".to_string(),
            dir.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let r = verdict_from_probe(&newer);
        assert!(r.converged, "{:?}", r.components);
        assert!(
            r.components[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("newer descendant"),
            "{:?}",
            r.components[0].detail
        );
        // An OLDER observed rev (base deployed, child expected) never passes:
        // the ancestry read goes the other way.
        write_script(&dir, "fno-agents", &version_script(&base, ""));
        write_script(&dir, "fno-agents-daemon", &version_script(&base, ""));
        write_script(&dir, "fno-agents-worker", &version_script(&base, ""));
        let older = parse_probe_args(&[
            "--bindir".to_string(),
            dir.to_string_lossy().into_owned(),
            "--expected".to_string(),
            child.clone(),
            "--agents-dir".to_string(),
            dir.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let r = verdict_from_probe(&older);
        assert!(!r.converged, "{:?}", r.components);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
