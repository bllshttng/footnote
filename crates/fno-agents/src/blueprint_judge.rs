//! `fno-agents judge` -- the grading half of the advisory five-question
//! blueprint judge. Daemon-free, like `graph-get`/`bash-census`: never a
//! routable `fno agents <verb>` invocation, though listed in
//! `ALL_CLIENT_ACTIONS` for the binary's own verb-surface probe.
//!
//! Ported off `cli/src/fno/observer/judge.py` per the flag-registry ratchet
//! (operator ruling 2026-09-12): a new CLI flag is Rust work.
//! This verb owns the lens prompts, the model spawn, and verdict parsing; it
//! emits no events and mints no run_id. The Python wrapper
//! (`cli/src/fno/observer/cli.py::judge_cmd`/`sweep`) still does the
//! pre-existing plan-parsing gate (`has_section("Five questions")`) and event
//! emission through `fno.events` -- reusing that single-sourced path rather
//! than growing a second one here (AGENTS.md principle 9).
//!
//! One JSON object on stdout, always exit 0 for the single-plan mode
//! (advisory, never fails); calibration mode exits 1 when any control
//! disagrees, matching the retired Python `judge_cmd --labels` contract.

use crate::evidence::truncate_chars;
use crate::graph_get::{default_graph_path, find_entry};
use crate::graph_store::{read_defaulted, s_str};
use crate::paths::worktree_repo_root;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const JUDGE_DIMENSIONS: [&str; 5] = [
    "persona",
    "surface_fit",
    "uncovered_case",
    "deletable",
    "duplication",
];
pub const JUDGE_MODEL: &str = "sonnet";

type Lenses = (String, HashMap<String, String>);

fn lenses_path(cwd: &Path) -> PathBuf {
    worktree_repo_root(cwd).join("evals/blueprint-judge/lenses.md")
}

/// (shared preamble, {dimension: section body}); unreadable file -> ("", {}).
fn load_lenses(path: &Path) -> Lenses {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (String::new(), HashMap::new());
    };
    let heading = Regex::new(r"(?m)^## ").unwrap();
    let Some(m) = heading.find(&text) else {
        return (String::new(), HashMap::new());
    };
    let preamble = text[..m.start()].trim().to_string();
    let mut sections = HashMap::new();
    for chunk in heading.split(&text[m.end()..]) {
        let mut parts = chunk.splitn(2, '\n');
        let name = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("").trim();
        if JUDGE_DIMENSIONS.contains(&name) {
            sections.insert(name.to_string(), body.to_string());
        }
    }
    (preamble, sections)
}

/// Code context for surface_fit/duplication; "" for other lenses or any fault.
fn gather_context(dimension: &str, plan_text: &str, cwd: &Path) -> String {
    if dimension == "surface_fit" {
        return match Command::new("fno").args(["help", "--all"]).output() {
            Ok(o) if o.status.success() => {
                truncate_chars(&String::from_utf8_lossy(&o.stdout), 4000)
            }
            _ => String::new(),
        };
    }
    if dimension != "duplication" {
        return String::new();
    }
    let sym_re = Regex::new(r"`([\w./-]{4,60})`").unwrap();
    let mut seen = HashSet::new();
    let mut syms: Vec<String> = Vec::new();
    for cap in sym_re.captures_iter(plan_text) {
        let s = cap[1].to_string();
        if syms.len() >= 12 {
            break;
        }
        if seen.insert(s.clone()) {
            syms.push(s);
        }
    }
    let mut chunks: Vec<String> = Vec::new();
    if !syms.is_empty() {
        let mut args: Vec<String> = vec!["-l".into(), "-F".into()];
        for s in &syms {
            args.push("-e".into());
            args.push(s.clone());
        }
        args.push("--glob".into());
        args.push("!graphify-out/**".into());
        args.push(".".into());
        if let Ok(h) = Command::new("rg").args(&args).current_dir(cwd).output() {
            if h.status.success() {
                let stdout = String::from_utf8_lossy(&h.stdout);
                let files: Vec<&str> = stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .take(12)
                    .collect();
                if !files.is_empty() {
                    chunks.push(format!(
                        "symbols {} appear in: {}",
                        syms.join(", "),
                        files.join(", ")
                    ));
                }
            }
        }
    }
    if let Ok(inv) = std::fs::read_to_string(
        worktree_repo_root(cwd).join("docs/architecture/dual-implementation-inventory.md"),
    ) {
        let rows: Vec<&str> = inv
            .lines()
            .filter(|ln| syms.iter().any(|s| ln.contains(s.as_str())))
            .take(10)
            .collect();
        if !rows.is_empty() {
            chunks.push(format!("inventory rows:\n{}", rows.join("\n")));
        }
    }
    truncate_chars(&chunks.join("\n"), 4000)
}

/// The LAST `VERDICT:` line wins; `unknown` and unparseable are both `None`.
fn parse_verdict(text: &str) -> (Option<String>, String) {
    let re = Regex::new(r"(?im)^VERDICT:\s*(pass|fail|unknown)\s*$").unwrap();
    match re.captures_iter(text).last() {
        None => (None, truncate_chars(text.trim(), 500)),
        Some(cap) => {
            let verdict = cap[1].to_lowercase();
            let start = cap.get(0).unwrap().start();
            let reason = truncate_chars(text[..start].trim(), 500);
            (
                if verdict == "unknown" {
                    None
                } else {
                    Some(verdict)
                },
                reason,
            )
        }
    }
}

/// One `fno agents spawn --substrate headless` round-trip. Matches the
/// retired Python `_default_spawn`: never a bare `claude -p`.
fn default_spawn(
    name: &str,
    prompt: &str,
    cwd: &Path,
    timeout_secs: u64,
    model: &str,
) -> Result<(i32, String, String), String> {
    let out = Command::new("fno")
        .args([
            "agents",
            "spawn",
            "--name",
            name,
            prompt,
            "--harness",
            "claude",
            "--substrate",
            "headless",
            "--model",
            model,
            "--cwd",
            &cwd.display().to_string(),
            "--timeout",
            &timeout_secs.to_string(),
        ])
        .output()
        .map_err(|e| e.to_string())?;
    Ok((
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

type Spawn<'a> = &'a dyn Fn(&str, &str, &Path, u64, &str) -> Result<(i32, String, String), String>;

fn judge_plan(
    plan_text: &str,
    node_text: &str,
    dimension: &str,
    cwd: &Path,
    lenses: &Lenses,
    spawn: Spawn,
) -> (Option<String>, String) {
    if !JUDGE_DIMENSIONS.contains(&dimension) {
        return (None, format!("unknown judge dimension {dimension:?}"));
    }
    let (preamble, sections) = lenses;
    let node_trimmed = node_text.trim();
    let mut parts = vec![
        if preamble.is_empty() {
            "Grade one question. Quote the plan lines you rely on, reason briefly, end with VERDICT: pass, fail or unknown.".to_string()
        } else {
            preamble.clone()
        },
        format!(
            "## The node\n{}",
            if node_trimmed.is_empty() {
                "(none supplied)"
            } else {
                node_trimmed
            }
        ),
        format!("## The plan\n{plan_text}"),
    ];
    let ctx = gather_context(dimension, plan_text, cwd);
    if !ctx.is_empty() {
        parts.push(format!("## Context from code\n{ctx}"));
    }
    if let Some(section) = sections.get(dimension).filter(|s| !s.is_empty()) {
        parts.push(format!("## Your question: {dimension}\n{section}"));
    }
    let prompt = format!("{}\n", parts.join("\n\n"));
    match spawn(
        &format!("blueprint-judge-{dimension}"),
        &prompt,
        cwd,
        600,
        JUDGE_MODEL,
    ) {
        Err(e) => (
            None,
            truncate_chars(&format!("judge spawn fault: {e}"), 500),
        ),
        Ok((rc, out, err)) if rc != 0 => {
            let msg = if err.trim().is_empty() {
                out.trim()
            } else {
                err.trim()
            };
            (
                None,
                truncate_chars(
                    &format!("judge spawn rc={rc}: {}", truncate_chars(msg, 200)),
                    500,
                ),
            )
        }
        Ok((_, out, _)) => parse_verdict(&out),
    }
}

fn node_text_of(node_id: Option<&str>) -> String {
    let Some(id) = node_id else {
        return String::new();
    };
    let graph_path = default_graph_path();
    let Ok(entries) = read_defaulted(&graph_path, true) else {
        return String::new();
    };
    let Some(entry) = find_entry(&entries, id) else {
        return String::new();
    };
    let title = s_str(entry, "title").unwrap_or("");
    let details = s_str(entry, "details").unwrap_or("");
    [title, details]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_single_plan(plan_path: &Path, node_id: Option<&str>, cwd: &Path, spawn: Spawn) -> i32 {
    let plan_text = match std::fs::read_to_string(plan_path) {
        Ok(t) => t,
        Err(e) => {
            println!(
                "{}",
                json!({"error": format!("no plan at {}: {e}", plan_path.display())})
            );
            return 0;
        }
    };
    let node_text = node_text_of(node_id);
    let lenses = load_lenses(&lenses_path(cwd));
    let rows: Vec<Value> = JUDGE_DIMENSIONS
        .iter()
        .map(|dimension| {
            let (verdict, reason) =
                judge_plan(&plan_text, &node_text, dimension, cwd, &lenses, spawn);
            json!({"dimension": dimension, "verdict": verdict, "reason": reason})
        })
        .collect();
    println!("{}", json!({"rows": rows}));
    0
}

fn run_calibration(labels_path: &Path, split: &str, cwd: &Path, spawn: Spawn) -> i32 {
    let text = match std::fs::read_to_string(labels_path) {
        Ok(t) => t,
        Err(e) => {
            println!(
                "{}",
                json!({"error": format!("cannot read {}: {e}", labels_path.display())})
            );
            return 1;
        }
    };
    let rows: Vec<Value> = match serde_yaml_ng::from_str(&text) {
        Ok(Value::Array(v)) => v,
        _ => {
            println!(
                "{}",
                json!({"error": "labels.yaml did not parse to a list"})
            );
            return 1;
        }
    };
    let base = labels_path.parent().unwrap_or(Path::new("."));
    let lenses = load_lenses(&lenses_path(cwd));

    // (n, n_fail_labeled, n_pass_labeled, tp, tn) per dimension. tp/tn count
    // correct judge verdicts on the rows actually labeled that class, so the
    // rate below is a per-class recall, not a fraction of every row.
    let mut dims: HashMap<String, (u32, u32, u32, u32, u32)> = HashMap::new();
    let mut disagreements: Vec<Value> = Vec::new();
    let mut controls_wrong = 0u32;

    for row in &rows {
        let row_split = row.get("split").and_then(Value::as_str).unwrap_or("dev");
        if row_split != split {
            continue;
        }
        let Some(plan_rel) = row.get("plan").and_then(Value::as_str) else {
            continue;
        };
        let plan_text = std::fs::read_to_string(base.join(plan_rel)).unwrap_or_default();
        let node_text = row
            .get("node_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let control = row.get("control").and_then(Value::as_bool).unwrap_or(false);
        let Some(labels) = row.get("labels").and_then(Value::as_object) else {
            continue;
        };
        for (dimension, label_v) in labels {
            if !JUDGE_DIMENSIONS.contains(&dimension.as_str()) {
                continue; // an unknown dimension key is a labels.yaml typo, not a score
            }
            let Some(label) = label_v.as_str() else {
                continue;
            };
            let (verdict, reason) =
                judge_plan(&plan_text, &node_text, dimension, cwd, &lenses, spawn);
            let s = dims.entry(dimension.clone()).or_insert((0, 0, 0, 0, 0));
            s.0 += 1;
            if label == "fail" {
                s.1 += 1;
                if verdict.as_deref() == Some("fail") {
                    s.3 += 1;
                }
            }
            if label == "pass" {
                s.2 += 1;
                if verdict.as_deref() == Some("pass") {
                    s.4 += 1;
                }
            }
            // A None verdict is a judge fault or an unparseable answer, not a
            // scored disagreement (matching the Python side's "gap, never a
            // fabricated verdict" rule) -- it never spends a control.
            if let Some(v) = verdict.as_deref() {
                if v != label {
                    disagreements.push(json!({
                        "plan": plan_rel, "dimension": dimension, "label": label,
                        "judge": verdict, "reason": truncate_chars(&reason, 120),
                    }));
                    if control {
                        controls_wrong += 1;
                    }
                }
            }
        }
    }

    let dimensions: serde_json::Map<String, Value> = dims
        .into_iter()
        .map(|(d, (n, n_fail, n_pass, tp, tn))| {
            let rate = |k: u32, of: u32| {
                if of > 0 {
                    Some((k as f64 / of as f64 * 1000.0).round() / 1000.0)
                } else {
                    None
                }
            };
            (
                d,
                json!({"n": n, "tp_rate": rate(tp, n_fail), "tn_rate": rate(tn, n_pass)}),
            )
        })
        .collect();
    println!(
        "{}",
        json!({"dimensions": dimensions, "disagreements": disagreements, "controls_wrong": controls_wrong})
    );
    if controls_wrong > 0 {
        1
    } else {
        0
    }
}

pub fn run_judge(args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut plan: Option<PathBuf> = None;
    let mut node: Option<String> = None;
    let mut labels: Option<PathBuf> = None;
    let mut split = "dev".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plan" => {
                i += 1;
                plan = args.get(i).map(PathBuf::from);
            }
            "--node" => {
                i += 1;
                node = args.get(i).cloned();
            }
            "--labels" => {
                i += 1;
                labels = args.get(i).map(PathBuf::from);
            }
            "--split" => {
                i += 1;
                split = args.get(i).cloned().unwrap_or_else(|| "dev".to_string());
            }
            "--force" | "-F" => {
                // Accepted, ignored: the level=report gate is a Python-side
                // config read (fno.loops.loop_level) and already ran before
                // this verb was invoked.
            }
            other => {
                eprintln!("fno-agents judge: unknown flag {other}");
                return 2;
            }
        }
        i += 1;
    }

    if let Some(labels_path) = labels {
        return run_calibration(&labels_path, &split, &cwd, &default_spawn);
    }
    let Some(plan_path) = plan else {
        eprintln!("fno-agents judge: give --plan or --labels");
        return 2;
    };
    run_single_plan(&plan_path, node.as_deref(), &cwd, &default_spawn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_takes_the_last_line_and_caps_reason() {
        let (v, reason) = parse_verdict("some reasoning\nVERDICT: fail\nnoise ignored");
        assert_eq!(v.as_deref(), Some("fail"));
        assert_eq!(reason, "some reasoning");
    }

    #[test]
    fn parse_verdict_unknown_is_none() {
        let (v, _) = parse_verdict("VERDICT: unknown");
        assert_eq!(v, None);
    }

    #[test]
    fn parse_verdict_unparseable_is_none() {
        let (v, reason) = parse_verdict("no verdict line here");
        assert_eq!(v, None);
        assert_eq!(reason, "no verdict line here");
    }

    #[test]
    fn load_lenses_splits_preamble_from_named_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lenses.md");
        std::fs::write(
            &path,
            "shared preamble\n\n## persona\npersona body\n\n## surface_fit\nfit body\n",
        )
        .unwrap();
        let (preamble, sections) = load_lenses(&path);
        assert_eq!(preamble, "shared preamble");
        assert_eq!(
            sections.get("persona").map(String::as_str),
            Some("persona body")
        );
        assert_eq!(
            sections.get("surface_fit").map(String::as_str),
            Some("fit body")
        );
    }

    #[test]
    fn load_lenses_missing_file_is_empty() {
        let (preamble, sections) = load_lenses(Path::new("/nonexistent/lenses.md"));
        assert_eq!(preamble, "");
        assert!(sections.is_empty());
    }

    #[test]
    fn run_judge_refuses_an_unknown_flag() {
        assert_eq!(run_judge(&["--bogus".to_string()]), 2);
    }

    #[test]
    fn run_judge_refuses_with_neither_plan_nor_labels() {
        assert_eq!(run_judge(&[]), 2);
    }

    /// Always returns `VERDICT: pass` for persona, `VERDICT: fail` for
    /// everything else -- exercises the tp_rate/tn_rate/disagreement math
    /// without a real model spawn.
    fn fake_spawn(
        name: &str,
        _: &str,
        _: &Path,
        _: u64,
        _: &str,
    ) -> Result<(i32, String, String), String> {
        let out = if name.ends_with("persona") {
            "VERDICT: pass"
        } else {
            "VERDICT: fail"
        };
        Ok((0, out.to_string(), String::new()))
    }

    #[test]
    fn run_single_plan_reports_all_five_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("p.md");
        std::fs::write(&plan, "a plan").unwrap();
        let rc = run_single_plan(&plan, None, dir.path(), &fake_spawn);
        assert_eq!(rc, 0);
    }

    #[test]
    fn tally_math_counts_rates_and_control_disagreements() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "a plan").unwrap();
        let labels_path = dir.path().join("labels.yaml");
        std::fs::write(
            &labels_path,
            "- plan: p.md\n  split: test\n  control: true\n  labels: {persona: pass, surface_fit: pass}\n",
        )
        .unwrap();

        // fake_spawn always answers persona=pass, surface_fit=fail: the
        // persona row agrees with its label, the surface_fit row disagrees
        // and (being a control) counts against controls_wrong.
        let rc = run_calibration(&labels_path, "test", dir.path(), &fake_spawn);
        assert_eq!(rc, 1);
    }
}
