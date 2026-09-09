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
/// missing, AC3-HP), then presence, then revision match.
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
    };
    if let Some(err) = &probe.instrument_error {
        v.detail = Some(err.clone());
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
        }
        None => {
            // Present but silent: fail toward repair, never toward fresh.
            v.status = if attempted {
                Status::Failed
            } else {
                Status::Stale
            };
            v.detail = Some("executable present but does not self-report a revision".to_string());
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

/// `fno-agents component-verdict`: read one JSON request on stdin, print the
/// verdict JSON on stdout. Exit 0 whenever a verdict was computed - a
/// not-converged fleet is data, not an error; exit 2 on malformed input.
pub fn run_component_verdict(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: fno-agents component-verdict < request.json");
        return 0;
    }
    let mut input = String::new();
    use std::io::Read;
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("fno-agents component-verdict: cannot read stdin: {e}");
        return 2;
    }
    let req: VerdictRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents component-verdict: malformed request JSON: {e}");
            return 2;
        }
    };
    match serde_json::to_string(&verdict(&req)) {
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
}
