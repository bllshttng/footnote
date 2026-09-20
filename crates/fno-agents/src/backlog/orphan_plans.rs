//! `fno-agents backlog-orphan-plans` -- find plan files that claim a node
//! whose `plan_path` never got bound, and (with `--apply`) bind them.
//!
//! Client-side and daemon-free, like [`crate::graph_get`]: both plan write
//! paths already TRY to bind at write time (`mutate_doc.py::_sync_graph_status`
//! on blueprint, `target_cli.py::_bind_node_plan_path` on target init), and
//! each try is one best-effort subprocess with a 30s timeout that warns and
//! moves on. Nothing ever retries what those drop, and no reader compared a
//! plan file's `claims:` against its node -- so each miss was permanent and
//! the king board read the node as `unplanned`. This is the retry and the
//! reader, in one pass over `<plans-dir>/*.md`.
//!
//! Transport-only, like the other early arms: it registers no client verb
//! (the shrink law allows no new one). Its caller is the SessionStart
//! reconcile sweep, which execs the binary directly.
//!
//! Dry run by default; `--apply` binds every `adoptable` row in ONE
//! [`crate::graph_store::mutate_rows`] write, passing a plan-rung map so the
//! derived status recomputes in the same write.

use crate::claims::{self, ClaimState};
use crate::graph_get::{default_graph_path, external_backend_selected};
use crate::graph_store::{self, plan_rung_from_status};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A plan younger than this may still be being written; never bind it.
const SETTLING_SECS: u64 = 600;

/// One claimed id's verdict. Exactly one per id, checked in this order.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Healthy case, dropped silently on a dry run.
    Bound,
    /// No node carries the id.
    Missing,
    /// done / superseded / deferred: a closed node's history is not rewritten.
    Terminal,
    /// Two or more files claim the id; bind none.
    Ambiguous,
    /// The plan maps to a rung below ready: an unfinalized draft.
    Unfinalized,
    /// Another node's plan_path already resolves to this file.
    OwnedBy(String),
    /// A live `blueprint-session:` claim holds the node; a planner is writing.
    Planning,
    /// The file was modified inside the settling window.
    Settling,
    /// The plan predates the node: the id was reused after an archive split.
    IdReuse,
    /// Nothing refuses it: bind on `--apply`.
    Adoptable,
    /// Post-write readback: the path landed.
    BoundNow,
    /// Post-write readback: the node's plan_path is still empty.
    BindFailed,
}

impl Verdict {
    fn name(&self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Missing => "missing",
            Self::Terminal => "terminal",
            Self::Ambiguous => "ambiguous",
            Self::Unfinalized => "unfinalized",
            Self::OwnedBy(_) => "owned_by",
            Self::Planning => "planning",
            Self::Settling => "settling",
            Self::IdReuse => "id_reuse",
            Self::Adoptable => "adoptable",
            Self::BoundNow => "bound_now",
            Self::BindFailed => "bind_failed",
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::OwnedBy(id) => id.clone(),
            Self::Ambiguous => String::from("two or more files claim this node"),
            _ => String::new(),
        }
    }
}

struct Config {
    plans_dir: PathBuf,
    graph: PathBuf,
    graph_overridden: bool,
    apply: bool,
    as_json: bool,
    /// Test seam: an explicit claims root. The CLI passes None and reads the
    /// env-resolved root, exactly like every other claim reader.
    claims_root: Option<PathBuf>,
}

pub fn run_orphan_plans(args: &[String]) -> i32 {
    let mut cfg = Config {
        plans_dir: PathBuf::new(),
        graph: default_graph_path(),
        graph_overridden: false,
        apply: false,
        as_json: false,
        claims_root: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--apply" => cfg.apply = true,
            "--json" => cfg.as_json = true,
            "--plans-dir" => match args.get(i + 1) {
                Some(p) => {
                    cfg.plans_dir = PathBuf::from(p);
                    i += 1;
                }
                None => {
                    eprintln!("fno-agents backlog-orphan-plans: --plans-dir needs a path");
                    return 2;
                }
            },
            "--graph" => match args.get(i + 1) {
                Some(p) => {
                    cfg.graph = PathBuf::from(p);
                    cfg.graph_overridden = true;
                    i += 1;
                }
                None => {
                    eprintln!("fno-agents backlog-orphan-plans: --graph needs a path");
                    return 2;
                }
            },
            other if other.starts_with('-') => {
                eprintln!("fno-agents backlog-orphan-plans: unknown flag {other}");
                return 2;
            }
            other => {
                eprintln!("fno-agents backlog-orphan-plans: unexpected argument {other}");
                return 2;
            }
        }
        i += 1;
    }
    if cfg.plans_dir.as_os_str().is_empty() {
        eprintln!("fno-agents backlog-orphan-plans: --plans-dir is required");
        return 2;
    }
    // Same shape-blind-write guard as prove-it-verdicts: a --graph read
    // redirect must not combine with a live-store write.
    if cfg.apply && cfg.graph_overridden {
        eprintln!(
            "fno-agents backlog-orphan-plans: --apply writes the live store, \
             so it refuses --graph (the fixture read and the live write would disagree)"
        );
        return 2;
    }
    if !cfg.graph_overridden && external_backend_selected() {
        eprintln!(
            "fno-agents backlog-orphan-plans: this reads the graph store directly; \
             under an external tracker backend that store is not authoritative."
        );
        return 1;
    }
    run(&cfg)
}

fn run(cfg: &Config) -> i32 {
    let rows = match graph_store::read_rows(&cfg.graph) {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!("fno-agents backlog-orphan-plans: {err}");
            return 1;
        }
    };

    // plans_dir/*.md with a `claims` scalar, grouped by claimed id.
    let mut by_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let entries = match std::fs::read_dir(&cfg.plans_dir) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!(
                "fno-agents backlog-orphan-plans: cannot read {}: {err}",
                cfg.plans_dir.display()
            );
            return 1;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "md").unwrap_or(true) {
            continue;
        }
        let Some(fm) = crate::backlog_ready::read_frontmatter(&path) else {
            continue;
        };
        let Some(claimed) = fm.get("claims").and_then(scalar) else {
            continue;
        };
        if claimed.is_empty() {
            continue;
        }
        by_id.entry(claimed).or_default().push(path);
    }

    // plan_path -> owner id, for the one-plan-one-node guard. Both sides key
    // on the canonical form: the store legally carries tilde- and
    // relative-spelled paths (resolve_plan_probe expands them), and a
    // byte-exact compare would let one file bind to a second node.
    let mut path_owner: BTreeMap<PathBuf, String> = BTreeMap::new();
    for row in &rows {
        if let (Some(id), Some(p)) = (
            row.get("id").and_then(Value::as_str),
            row.get("plan_path").and_then(Value::as_str),
        ) {
            if !p.is_empty() {
                path_owner.insert(canon(Path::new(p)), id.to_string());
            }
        }
    }

    let now = std::time::SystemTime::now();
    let mut out_rows: Vec<(String, PathBuf, Verdict)> = Vec::new();
    let mut adoptable: Vec<(String, PathBuf, String)> = Vec::new();
    for (node_id, mut paths) in by_id {
        paths.sort();
        let Some(row) = rows
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(node_id.as_str()))
            .cloned()
        else {
            for path in &paths {
                out_rows.push((node_id.clone(), path.clone(), Verdict::Missing));
            }
            continue;
        };
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        if is_terminal(status)
            || row
                .get("deferred_at")
                .map(|v| !v.is_null())
                .unwrap_or(false)
        {
            for path in &paths {
                out_rows.push((node_id.clone(), path.clone(), Verdict::Terminal));
            }
            continue;
        }
        let bound = row
            .get("plan_path")
            .and_then(Value::as_str)
            .map(|p| !p.is_empty())
            .unwrap_or(false);
        if bound {
            continue; // healthy; dropped silently
        }
        if paths.len() > 1 {
            for path in &paths {
                out_rows.push((node_id.clone(), path.clone(), Verdict::Ambiguous));
            }
            continue;
        }
        let path = paths.remove(0);
        let Some(fm) = crate::backlog_ready::read_frontmatter(&path) else {
            continue; // unreadable plan: it carries no claims scalar either way
        };
        let plan_status = fm.get("status").and_then(scalar).unwrap_or_default();
        let rung = plan_rung_from_status(&plan_status);
        if !matches!(rung, "ready" | "in_progress" | "in_review") {
            out_rows.push((node_id.clone(), path.clone(), Verdict::Unfinalized));
            continue;
        }
        if let Some(owner) = path_owner.get(&canon(&path)) {
            if owner != &node_id {
                out_rows.push((
                    node_id.clone(),
                    path.clone(),
                    Verdict::OwnedBy(owner.clone()),
                ));
                continue;
            }
        }
        if let Some(rec) = claims_planning(&node_id, cfg.claims_root.as_deref()) {
            out_rows.push((node_id.clone(), path, rec));
            continue;
        }
        if file_age_secs(&path, now)
            .map(|age| age < SETTLING_SECS)
            .unwrap_or(false)
        {
            out_rows.push((node_id.clone(), path, Verdict::Settling));
            continue;
        }
        let plan_created = fm.get("created").and_then(scalar).unwrap_or_default();
        let node_created = row
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(10)
            .collect::<String>();
        if plan_created.len() >= 10
            && node_created.len() >= 10
            && plan_created.as_str() < node_created.as_str()
        {
            out_rows.push((node_id.clone(), path, Verdict::IdReuse));
            continue;
        }
        adoptable.push((node_id.clone(), path.clone(), rung.to_string()));
        out_rows.push((node_id, path, Verdict::Adoptable));
    }

    let mut exit = 0;
    if cfg.apply && !adoptable.is_empty() {
        let rungs: BTreeMap<String, String> = adoptable
            .iter()
            .map(|(id, _, rung)| (id.clone(), rung.clone()))
            .collect();
        let paths: BTreeMap<String, PathBuf> = adoptable
            .iter()
            .map(|(id, path, _)| (id.clone(), path.clone()))
            .collect();
        let wrote = graph_store::mutate_rows(
            &cfg.graph,
            Duration::from_secs(10),
            Some(rungs),
            None,
            |rows| {
                for (id, path) in &paths {
                    let Some(row) = rows
                        .iter_mut()
                        .find(|row| row.get("id").and_then(Value::as_str) == Some(id.as_str()))
                    else {
                        continue;
                    };
                    let empty = row
                        .get("plan_path")
                        .map(|v| v.is_null() || v.as_str().map(str::is_empty).unwrap_or(true))
                        .unwrap_or(true);
                    if !empty {
                        continue; // a racing intake won
                    }
                    row.as_object_mut()
                        .ok_or_else(|| {
                            graph_store::StoreError::Invalid(format!(
                                "node {id} is not a JSON object"
                            ))
                        })?
                        .insert(
                            "plan_path".to_string(),
                            Value::String(path.to_string_lossy().to_string()),
                        );
                }
                Ok(true)
            },
        );
        if let Err(err) = wrote {
            eprintln!("fno-agents backlog-orphan-plans: {err}");
            return 1;
        }
        // Readback decides bound_now vs bind_failed, whatever the write said.
        let fresh = match graph_store::read_rows(&cfg.graph) {
            Ok(fresh) => fresh,
            Err(err) => {
                eprintln!("fno-agents backlog-orphan-plans: readback: {err}");
                return 1;
            }
        };
        for (id, path, verdict) in out_rows.iter_mut() {
            if *verdict != Verdict::Adoptable {
                continue;
            }
            let bound_path = fresh
                .iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(id.as_str()))
                .and_then(|row| row.get("plan_path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if bound_path.is_empty() {
                *verdict = Verdict::BindFailed;
                exit = 1;
            } else if Path::new(bound_path) == path.as_path() {
                *verdict = Verdict::BoundNow;
            } else {
                *verdict = Verdict::Bound; // the racer bound the same or its own plan
            }
        }
    }

    let rows_json: Vec<Value> = out_rows
        .iter()
        .map(|(id, path, verdict)| {
            json!({
                "node_id": id,
                "plan_path": path.to_string_lossy(),
                "verdict": verdict.name(),
                "detail": verdict.detail(),
            })
        })
        .collect();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (_, _, verdict) in &out_rows {
        *counts.entry(verdict.name().to_string()).or_default() += 1;
    }
    if cfg.as_json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "read_at": graph_store::now_isoformat(),
                "plans_dir": cfg.plans_dir.to_string_lossy(),
                "rows": rows_json,
                "counts": counts,
            }))
            .unwrap_or_else(|_| "{\"rows\":[],\"counts\":{}}".to_string())
        );
    } else {
        for (id, path, verdict) in &out_rows {
            if *verdict == Verdict::Bound {
                continue;
            }
            let detail = verdict.detail();
            if detail.is_empty() {
                println!("{} {} {}", verdict.name(), id, path.display());
            } else {
                println!("{} {} {} ({detail})", verdict.name(), id, path.display());
            }
        }
    }
    exit
}

/// The node's claim read, answered as a verdict: Live or Suspect under a
/// `blueprint-session:` holder means a planner is still writing the plan.
fn claims_planning(node_id: &str, root: Option<&Path>) -> Option<Verdict> {
    let (state, record) = claims::status(&format!("node:{node_id}"), root);
    let live = matches!(state, ClaimState::Live | ClaimState::Suspect);
    let planner = record
        .as_ref()
        .map(|rec| rec.holder.starts_with("blueprint-session:"))
        .unwrap_or(false);
    (live && planner).then_some(Verdict::Planning)
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "superseded" | "deferred")
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().trim_matches(['\'', '"']).to_string()),
        _ => None,
    }
}

fn file_age_secs(path: &Path, now: std::time::SystemTime) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified).ok().map(|d| d.as_secs())
}

/// The path's canonical form, falling back to the raw spelling for a file
/// that does not resolve (the store may name a plan that was moved or
/// deleted; the compare then stays byte-exact, as before).
fn canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, extra: Value) -> Value {
        let mut base = serde_json::json!({
            "id": id, "slug": format!("slug-{id}"), "title": id,
            "status": "idea", "priority": "p2", "type": "feature",
            "created_at": "2026-09-01T00:00:00+00:00",
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn plan_file(dir: &Path, name: &str, claims: &str, status: &str, created: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("---\nclaims: {claims}\ncreated: {created}\nstatus: {status}\n---\n\n# plan\n"),
        )
        .unwrap();
        path
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        graph: PathBuf,
        plans: PathBuf,
    }

    fn fixture(nodes: &[Value]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            serde_json::to_string(&json!({ "entries": nodes })).unwrap(),
        )
        .unwrap();
        let plans = dir.path().join("plans");
        std::fs::create_dir(&plans).unwrap();
        Fixture {
            _dir: dir,
            graph,
            plans,
        }
    }

    fn age_file(path: &Path, secs: u64) {
        let aged = std::time::SystemTime::now() - Duration::from_secs(secs);
        std::fs::File::options()
            .append(true)
            .open(path)
            .unwrap()
            .set_modified(aged)
            .unwrap();
    }

    #[test]
    fn adoptable_binds_on_apply_and_reads_back_ready() {
        let fx = fixture(&[node("x-aaaa", json!({}))]);
        let plan = plan_file(&fx.plans, "p1.md", "x-aaaa", "ready", "2026-09-02");
        age_file(&plan, 3600);
        let cfg = Config {
            plans_dir: fx.plans.clone(),
            graph: fx.graph.clone(),
            graph_overridden: true,
            apply: true,
            as_json: false,
            claims_root: None,
        };
        assert_eq!(run(&cfg), 0);
        let rows = graph_store::read_rows(&fx.graph).unwrap();
        let bound = rows
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some("x-aaaa"))
            .unwrap();
        assert_eq!(
            bound.get("plan_path").and_then(Value::as_str),
            Some(plan.to_string_lossy().as_ref())
        );
        assert_eq!(bound.get("status").and_then(Value::as_str), Some("ready"));
    }

    #[test]
    fn dry_run_lists_adoptable_and_changes_nothing() {
        let fx = fixture(&[node("x-aaaa", json!({}))]);
        let plan = plan_file(&fx.plans, "p1.md", "x-aaaa", "ready", "2026-09-02");
        age_file(&plan, 3600);
        let before = std::fs::read(&fx.graph).unwrap();
        let cfg = Config {
            plans_dir: fx.plans.clone(),
            graph: fx.graph.clone(),
            graph_overridden: true,
            apply: false,
            as_json: false,
            claims_root: None,
        };
        assert_eq!(run(&cfg), 0);
        assert_eq!(
            std::fs::read(&fx.graph).unwrap(),
            before,
            "dry run wrote nothing"
        );
    }

    #[test]
    fn design_plan_reports_unfinalized_and_stays_unbound() {
        let fx = fixture(&[node("x-des", json!({}))]);
        let plan = plan_file(&fx.plans, "d.md", "x-des", "design", "2026-09-02");
        age_file(&plan, 3600);
        let cfg = Config {
            plans_dir: fx.plans.clone(),
            graph: fx.graph.clone(),
            graph_overridden: true,
            apply: true,
            as_json: false,
            claims_root: None,
        };
        assert_eq!(run(&cfg), 0);
        let rows = graph_store::read_rows(&fx.graph).unwrap();
        assert!(
            rows[0]
                .get("plan_path")
                .map(|v| v.is_null())
                .unwrap_or(true),
            "design plan never binds"
        );
    }

    #[test]
    fn two_files_one_node_is_ambiguous_and_binds_none() {
        let fx = fixture(&[node("x-amb", json!({}))]);
        let a = plan_file(&fx.plans, "a.md", "x-amb", "ready", "2026-09-02");
        let b = plan_file(&fx.plans, "b.md", "x-amb", "ready", "2026-09-02");
        age_file(&a, 3600);
        age_file(&b, 3600);
        let cfg = Config {
            plans_dir: fx.plans.clone(),
            graph: fx.graph.clone(),
            graph_overridden: true,
            apply: true,
            as_json: false,
            claims_root: None,
        };
        assert_eq!(run(&cfg), 0);
        let rows = graph_store::read_rows(&fx.graph).unwrap();
        assert!(
            rows[0]
                .get("plan_path")
                .map(|v| v.is_null())
                .unwrap_or(true),
            "an ambiguous claim never binds"
        );
    }

    #[test]
    fn plan_older_than_node_reports_id_reuse() {
        let fx = fixture(&[node(
            "x-reuse",
            json!({"created_at": "2026-09-17T03:01:00+00:00"}),
        )]);
        let plan = plan_file(&fx.plans, "r.md", "x-reuse", "ready", "2026-09-15");
        age_file(&plan, 3600);
        let cfg = Config {
            plans_dir: fx.plans.clone(),
            graph: fx.graph.clone(),
            graph_overridden: true,
            apply: true,
            as_json: false,
            claims_root: None,
        };
        assert_eq!(run(&cfg), 0);
        let rows = graph_store::read_rows(&fx.graph).unwrap();
        assert!(
            rows[0]
                .get("plan_path")
                .map(|v| v.is_null())
                .unwrap_or(true),
            "the reused id never binds"
        );
    }

    #[test]
    fn live_blueprint_claim_reports_planning_and_young_file_reports_settling() {
        let fx = fixture(&[node("x-plan6", json!({})), node("x-young", json!({}))]);
        let held = plan_file(&fx.plans, "held.md", "x-plan6", "ready", "2026-09-02");
        let young = plan_file(&fx.plans, "young.md", "x-young", "ready", "2026-09-02");
        age_file(&held, 3600);
        age_file(&young, 3600);
        age_file(&young, 30); // rewritten a moment ago: the author may be typing
        let claims_home = tempfile::tempdir().unwrap();
        let outcome = claims::acquire(
            "node:x-plan6",
            "blueprint-session:test",
            claims::AcquireOpts {
                root: Some(claims_home.path().to_path_buf()),
                ..Default::default()
            },
        );
        assert!(matches!(outcome, claims::AcquireOutcome::Acquired(_)));
        let cfg = Config {
            plans_dir: fx.plans.clone(),
            graph: fx.graph.clone(),
            graph_overridden: true,
            apply: true,
            as_json: false,
            claims_root: Some(claims_home.path().to_path_buf()),
        };
        assert_eq!(run(&cfg), 0);
        let rows = graph_store::read_rows(&fx.graph).unwrap();
        for (id, name) in [("x-plan6", "planner"), ("x-young", "young file")] {
            let row = rows
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
                .unwrap();
            assert!(
                row.get("plan_path").map(|v| v.is_null()).unwrap_or(true),
                "{name} stays unbound"
            );
        }
    }

    #[test]
    fn done_node_reports_terminal_and_stays_untouched() {
        let fx = fixture(&[node("x-term", json!({"status": "done"}))]);
        let plan = plan_file(&fx.plans, "t.md", "x-term", "ready", "2026-09-02");
        age_file(&plan, 3600);
        let cfg = Config {
            plans_dir: fx.plans.clone(),
            graph: fx.graph.clone(),
            graph_overridden: true,
            apply: true,
            as_json: false,
            claims_root: None,
        };
        assert_eq!(run(&cfg), 0);
        let rows = graph_store::read_rows(&fx.graph).unwrap();
        assert_eq!(rows[0].get("status").and_then(Value::as_str), Some("done"));
        assert!(
            rows[0]
                .get("plan_path")
                .map(|v| v.is_null())
                .unwrap_or(true),
            "a closed node's history is not rewritten"
        );
    }

    #[test]
    fn apply_with_graph_refuses_by_name() {
        let args: Vec<String> = ["--plans-dir", "/tmp/x", "--graph", "/tmp/g.json", "--apply"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(run_orphan_plans(&args), 2);
    }

    #[test]
    fn missing_plans_dir_refuses_as_usage() {
        let args: Vec<String> = ["--apply"].iter().map(|s| s.to_string()).collect();
        assert_eq!(run_orphan_plans(&args), 2);
    }
}
