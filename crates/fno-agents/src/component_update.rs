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
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

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

fn repair_command(component: &str, req: &VerdictRequest) -> Option<String> {
    match component {
        PYTHON_TOOL => Some("fno doctor update".to_string()),
        MUX_FRONT_DOOR => Some(match &req.crates_mux_dir {
            Some(dir) => format!("cargo install --path {dir} --bins"),
            None => "fno doctor update --rust".to_string(),
        }),
        AGENTS_CLIENT | AGENTS_DAEMON | AGENTS_WORKER => Some(match &req.crates_agents_dir {
            Some(dir) => format!("cargo install --path {dir} --bins"),
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

/// One bounded `version --json` probe of a deployed executable. Returns
/// (rev, python_script, instrument_error): the error names the instrument so
/// an unanswerable probe lands at the classifier as Unknown (never collapsed
/// into fresh or missing).
fn probe_binary(
    path: &std::path::Path,
    timeout: Duration,
) -> (Option<String>, Option<String>, Option<String>) {
    use std::io::Read;
    use std::process::{Command, Stdio};
    if !path.is_file() {
        return (None, None, None);
    }
    let mut child = match Command::new(path)
        .args(["version", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (None, None, Some(format!("could not be executed ({e})"))),
    };
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    return (
                        None,
                        None,
                        Some(format!("hung on `version --json` (>{:?})", timeout)),
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return (None, None, Some(format!("could not be executed ({e})"))),
        }
    };
    if !status.map_or(false, |s| s.success()) {
        return (
            None,
            None,
            Some(format!(
                "exited {} on `version --json`",
                status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)
            )),
        );
    }
    let mut out = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut out);
    }
    if out.trim().is_empty() {
        return (
            None,
            None,
            Some("emitted no `version --json` output".to_string()),
        );
    }
    let data: serde_json::Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(_) => {
            return (
                None,
                None,
                Some("emitted unparseable `version --json` output".to_string()),
            )
        }
    };
    if !data.is_object() {
        return (
            None,
            None,
            Some("emitted unexpected `version --json` output".to_string()),
        );
    }
    if data.get("dirty").and_then(|d| d.as_bool()) == Some(true) {
        return (
            None,
            None,
            Some("was built from a dirty crates/ tree".to_string()),
        );
    }
    let rev = data
        .get("crates_rev")
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty() && *r != "unknown")
        .map(|r| r.to_string());
    if rev.is_none() {
        return (
            None,
            None,
            Some("carries no rev stamp (built outside a git checkout?)".to_string()),
        );
    }
    let script = data
        .get("python_script")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    (rev, script, None)
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
        let (rev, _script, err) = if present {
            probe_binary(&path, probe_timeout)
        } else {
            (None, None, None)
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
        });
    }
    if p.include_mux {
        let path = p.bindir.join(format!("fno{exe}"));
        let present = path.is_file();
        let (rev, script, err) = if present {
            probe_binary(&path, probe_timeout)
        } else {
            (None, None, None)
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
        });
    }
    if let Some(py_rev) = p.python_rev.clone() {
        components.push(ComponentProbe {
            component: PYTHON_TOOL.to_string(),
            expected_rev: p.python_expected.clone(),
            executable: python_exec.or(Some("<unresolved python>".to_string())),
            pre_rev: None,
            post_rev: Some(py_rev),
            instrument_error: None,
            contradicting_evidence: p.python_evidence.clone(),
            effect_attempted: p.attempted,
            effect_ok: None,
        });
    }
    let req = VerdictRequest {
        expected_rev: p.expected.clone(),
        crates_agents_dir: p.agents_dir.clone(),
        crates_mux_dir: p.mux_dir.clone(),
        components,
    };
    verdict(&req)
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
        }
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

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

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
}
