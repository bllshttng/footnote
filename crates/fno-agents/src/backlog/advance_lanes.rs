//! Isolated lane launches over the shared native fill and spawn owners.
use super::advance;
use super::advance_cli::{s, spawn_error};
use serde_json::{json, Value};
use std::path::Path;

pub fn run_lanes(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: fno backlog dispatch-lanes [--max N] [--project P|-p P] [--mission ID] [--model M|-m M] [--harness H|-H H] [--provider V] [--source ab|ac]");
        return 0;
    }
    let mut max = None;
    let mut project = None;
    let mut mission = None;
    let mut model = None;
    let mut harness = None;
    let mut vendor = None;
    let mut source = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let Some(value) = args.get(i) else {
            eprintln!("dispatch-lanes: {flag} requires a value");
            return 2;
        };
        match flag {
            "--max" => {
                max = value.parse::<i64>().ok();
                if max.is_none() {
                    return 2;
                }
            }
            "--project" | "-p" => project = Some(value.clone()),
            "--mission" => mission = Some(value.clone()),
            "--model" | "-m" => model = Some(value.clone()),
            "--harness" | "-H" => harness = Some(value.clone()),
            "--provider" => vendor = Some(value.clone()),
            "--source" => source = Some(value.clone()),
            _ => {
                eprintln!("dispatch-lanes: unexpected argument {flag}");
                return 2;
            }
        }
        i += 1;
    }
    if source
        .as_deref()
        .is_some_and(|s| !crate::naming::dispatch_sources().contains(s))
    {
        eprintln!("dispatch-lanes: unknown --source");
        return 2;
    }
    if model.as_deref().is_some_and(|s| s.trim().is_empty())
        || harness
            .as_deref()
            .is_some_and(|s| !crate::provider::KNOWN_PROVIDERS.contains(&s))
        || vendor
            .as_deref()
            .is_some_and(|s| s.trim().is_empty() || crate::provider::KNOWN_PROVIDERS.contains(&s))
    {
        eprintln!("dispatch-lanes: invalid model, harness or provider axis");
        return 2;
    }
    let requested = max
        .unwrap_or_else(|| {
            super::advance_settings::load_merged(None)
                .pointer("/parallel/max_lanes")
                .and_then(Value::as_i64)
                .unwrap_or(1)
        })
        .max(0) as usize;
    match lanes(
        requested,
        project.as_deref(),
        mission.as_deref(),
        model.as_deref(),
        harness.as_deref(),
        vendor.as_deref(),
        source.as_deref(),
    ) {
        Ok(v) => {
            let failed = v["lanes"].as_array().is_some_and(|rows| {
                !rows.is_empty() && !rows.iter().any(|r| r["status"] == "dispatched")
            });
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
            i32::from(failed)
        }
        Err(e) => {
            eprintln!("dispatch-lanes: {e}");
            1
        }
    }
}
fn lanes(
    max: usize,
    project: Option<&str>,
    mission: Option<&str>,
    model: Option<&str>,
    harness: Option<&str>,
    vendor: Option<&str>,
    source: Option<&str>,
) -> Result<Value, String> {
    let canonical = super::collision::find_repo_root();
    let (selected, mut fill) =
        super::advance_fill::select_lane_fill(max, project, mission, true, None, &canonical)?;
    let mut receipts = Vec::new();
    for node in selected {
        let id = s(&node, "id");
        let key = format!("dispatch:{id}");
        let holder = format!("advance:{}", std::process::id());
        let launched = (|| -> Result<Value, String> {
            let root = super::triage::project_root_from_settings(node["project"].as_str())
                .or_else(|| {
                    node["cwd"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .ok_or("lane-root: no project root")?;
            if let Some(reason) =
                advance::node_dispatch_block_reason(id, Some(&root), None, None, None)
            {
                return Err(reason);
            }
            match crate::claims::acquire(
                &key,
                &holder,
                crate::claims::AcquireOpts {
                    ttl_ms: Some(advance::DISPATCH_TTL_MS as i64),
                    reason: Some(format!("parallel lane dispatch for {id}")),
                    ..Default::default()
                },
            ) {
                crate::claims::AcquireOutcome::Acquired(_) => {}
                crate::claims::AcquireOutcome::HeldByOther { .. } => {
                    return Err("already-claimed".into())
                }
                other => return Err(format!("claim-error: {other:?}")),
            }
            let resolved = super::advance_dispatch::resolve_node_spawn(
                id,
                Some(&root),
                node["slug"].as_str(),
                &node,
                model,
                None,
                harness,
                node["dispatch_verb"].as_str(),
                None,
                source,
            )
            .map_err(|e| e.message)?;
            let argv = [
                "agents",
                "workspace",
                "worktree",
                "ensure",
                "--repo",
                &root,
                "--name",
                id,
                "--harness",
                &resolved.resolved_harness,
            ]
            .map(str::to_string);
            let out = advance::bounded_command_env(
                Path::new(&crate::scrape::fno_bin()),
                &argv,
                300,
                &[],
            )?;
            let worktree = out.stdout.trim();
            if out.code != 0 || worktree.is_empty() {
                return Err(format!("worktree ensure failed: {}", out.stderr.trim()));
            }
            let path = Path::new(worktree);
            if path != Path::new(&root) {
                let fno = path.join(".fno");
                if fno.is_symlink() {
                    std::fs::remove_file(&fno).map_err(|e| e.to_string())?
                }
                std::fs::create_dir_all(&fno).map_err(|e| e.to_string())?;
                let mut cmd = std::process::Command::new("bash");
                cmd.arg(Path::new(&root).join("scripts/setup/setup-worktree.sh"))
                    .current_dir(path);
                let out = crate::bounded_cmd::output_with_timeout_result(cmd, 300)
                    .map_err(|e| e.to_string())?;
                if !out.status.success() {
                    return Err(format!(
                        "worktree setup failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    ));
                }
                let base =
                    crate::agents_config::config_lookup(Path::new(&root), &["project", "id"])
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| {
                            Path::new(&root)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        });
                let mut table = toml::Table::new();
                table.insert(
                    "project".into(),
                    toml::Value::Table(toml::Table::from_iter([(
                        "id".into(),
                        toml::Value::String(format!("{base}-{id}")),
                    )])),
                );
                std::fs::write(fno.join("config.local.toml"), table.to_string())
                    .map_err(|e| e.to_string())?;
            }
            let brief = super::autobrief::resolve_dispatch_brief(&node);
            let mut resolved = resolved;
            resolved.node_cwd = Some(worktree.to_string());
            resolved.vendor = vendor.map(str::to_string);
            if let Some(text) = brief.text {
                resolved.env.insert("TARGET_BRIEF".into(), text);
            }
            let (identity, row) = super::advance_dispatch::launch_resolved(
                id,
                resolved,
                Some((&key, &holder)),
                "dispatch_lanes",
                source,
                None,
            )
            .map_err(spawn_error)?;
            advance::advance_emit(
                advance::EVENT_DISPATCHED,
                json!({"node_id":id,"short_id":identity,"agent_name":row["agent_name"],"lane":true,"worktree":worktree,"verb":row["verb"],"verb_source":row["verb_source"],"brief":brief.tag}),
                None,
            );
            Ok(json!({"node_id":id,"status":"dispatched","short_id":identity,"worktree":worktree}))
        })();
        match launched {
            Ok(v) => receipts.push(v),
            Err(e) => {
                advance::safe_release(&key, &holder);
                if crate::lanes::find_lane_slot(id, None).is_some() {
                    let _ = crate::lanes::release_lane_slot(id, None);
                }
                receipts.push(json!({"node_id":id,"status":"skipped","error":e}));
            }
        }
    }
    let dispatched = receipts
        .iter()
        .filter(|r| r["status"] == "dispatched")
        .count();
    let filled = fill.remove("filled").unwrap_or(json!(0));
    fill.insert("selected".into(), filled);
    fill.insert("dispatched".into(), json!(dispatched));
    fill.insert("skipped".into(), json!(receipts.len() - dispatched));
    Ok(json!({"lanes":receipts,"fill":fill}))
}
