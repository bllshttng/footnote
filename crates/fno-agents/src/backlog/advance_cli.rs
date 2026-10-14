//! Native command and caller door for backlog dispatch and mission drains.
use super::advance::{self, AdvanceInput, AdvanceResult};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Default, Debug)]
pub(super) struct Args {
    pub(super) closed: Option<String>,
    pub(super) epic: Option<String>,
    pub(super) project: Option<String>,
    pub(super) model: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) source: Option<String>,
    pub(super) stop: bool,
    pub(super) loose: bool,
    pub(super) continuation: bool,
    pub(super) explain: bool,
    pub(super) json: bool,
    pub(super) verbose: bool,
    pub(super) max: Option<i64>,
    pub(super) explain_node: Option<String>,
    pub(super) top: usize,
}
fn parse(args: &[String]) -> Result<Args, String> {
    let mut o = Args {
        top: 5,
        ..Default::default()
    };
    let mut i = 0;
    while i < args.len() {
        let (arg, inline) = args[i]
            .split_once('=')
            .map(|(a, v)| (a, Some(v.to_string())))
            .unwrap_or((&args[i], None));
        let mut value = || -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i)
                .filter(|s| !s.starts_with("--"))
                .cloned()
                .ok_or_else(|| format!("{arg} requires a value"))
        };
        match arg {
            "--closed" => o.closed = Some(value()?),
            "--epic" => o.epic = Some(value()?),
            "--project" | "-p" => o.project = Some(value()?),
            "--model" | "-m" => o.model = Some(value()?),
            "--provider" => o.provider = Some(value()?),
            "--source" => o.source = Some(value()?),
            "--max" => o.max = Some(value()?.parse().map_err(|_| "--max requires an integer")?),
            "--explain-node" => o.explain_node = Some(value()?),
            "--explain-top" => {
                o.top = value()?
                    .parse()
                    .map_err(|_| "--explain-top requires an integer")?
            }
            "--stop" => o.stop = true,
            "--loose" => o.loose = true,
            "--continuation" => o.continuation = true,
            "--explain" => o.explain = true,
            "--json" | "-J" => o.json = true,
            "--verbose" => o.verbose = true,
            _ => return Err(format!("unexpected argument {arg}")),
        }
        i += 1;
    }
    if let Some(source) = o.source.as_deref() {
        if !crate::naming::dispatch_sources().contains(source) {
            return Err(format!(
                "unknown --source {source:?}; known: {}",
                crate::naming::dispatch_sources()
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if !o.explain {
        if o.model.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err("--model must not be empty".into());
        }
        if let Some(provider) = o.provider.as_deref() {
            if provider.trim().is_empty() {
                return Err("--provider must not be empty".into());
            }
            if super::advance_dispatch::launch_harness_axis(provider, None).is_none() {
                return Err(format!(
                    "--provider {provider:?} names no known harness or account record"
                ));
            }
        }
        if o.explain_node.is_some() || o.top != 5 {
            return Err("--explain-node / --explain-top require --explain".into());
        }
        if o.epic.is_some() && (o.closed.is_some() || o.loose) {
            return Err("--epic is mutually exclusive with --closed/--loose".into());
        }
        if o.loose && (o.closed.is_some() || o.project.is_none()) {
            return Err("--loose requires --project and excludes --closed".into());
        }
        if o.epic.is_none() && !o.loose && (o.stop || o.continuation || o.max.is_some()) {
            return Err("--stop / --max / --continuation require --epic".into());
        }
    }
    Ok(o)
}

pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: fno backlog advance [--closed ID | --epic ID | --loose --project P]\n  --stop --continuation --max N --project P|-p P --model M|-m M --provider H\n  --source ab|ac --json|-J --verbose --explain --explain-node ID --explain-top N\nMerge continuation is opt-in and nonfatal. --epic activates a mission. --stop deactivates it. --continuation never reactivates.");
        return 0;
    }
    if args == ["--stdin"] {
        return run_request();
    }
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("advance: {e}");
            return 2;
        }
    };
    if o.explain {
        return match super::advance_explain::build(&o) {
            Ok(v) => {
                if o.json {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                } else {
                    println!("{}", super::advance_explain::render(&v));
                }
                0
            }
            Err(e) => {
                eprintln!("advance --explain: {e}");
                1
            }
        };
    }
    if crate::loops_pause::is_paused() {
        if o.json {
            println!("{}", json!({"decision":"skipped","reason":"paused"}));
        } else {
            println!("advance: skipped reason=paused");
        }
        return 0;
    }
    let canonical = o.epic.as_ref().and_then(|epic| {
        rows().ok().and_then(|rows| {
            crate::backlog_ready::find_node_index(&rows, epic)
                .0
                .map(|i| s(&rows[i], "id").to_string())
        })
    });
    let canonical = canonical.or_else(|| o.epic.clone());
    let key = canonical
        .as_ref()
        .map(|id| format!("flight:backlog-advance:epic:{id}"))
        .unwrap_or_else(|| "flight:backlog-advance".into());
    let holder = format!("advance:{}", std::process::id());
    let mut acquired = false;
    if !o.stop {
        match crate::claims::acquire(
            &key,
            &holder,
            crate::claims::AcquireOpts {
                ttl_ms: Some(30 * 60 * 1000),
                reason: Some("single-flight".into()),
                ..Default::default()
            },
        ) {
            crate::claims::AcquireOutcome::Acquired(_) => acquired = true,
            crate::claims::AcquireOutcome::HeldByOther { holder, .. } => {
                if o.json {
                    println!("{}", json!({"decision":"held","held":true,"holder":holder}));
                } else {
                    println!("backlog advance: held, a run for this scope is already in flight (holder={holder})");
                }
                return 0;
            }
            other => eprintln!("advance: single-flight gate unavailable: {other:?}"),
        }
    }
    let code = if o.epic.is_some() || o.loose {
        match drain(&o) {
            Ok(v) => render_drain(&v, &o),
            Err(e) => {
                eprintln!("advance: unexpected error (non-fatal): {e}");
                println!("advance: failed reason=unexpected-error");
                0
            }
        }
    } else {
        let result = advance::advance(&AdvanceInput {
            closed_node_id: o.closed.as_deref(),
            project: o.project.as_deref(),
            verbose: o.verbose,
            model: o.model.as_deref(),
            provider: o.provider.as_deref(),
            source: o.source.as_deref(),
            ..Default::default()
        });
        if let Some(closed) = o.closed.as_deref() {
            let project = rows().ok().and_then(|rows| {
                crate::backlog_ready::find_node_index(&rows, closed)
                    .0
                    .map(|i| s(&rows[i], "project").to_string())
            });
            advance::advance_dependents(
                closed,
                project.as_deref(),
                None,
                None,
                o.verbose,
                o.model.as_deref(),
                o.provider.as_deref(),
                o.source.as_deref(),
            );
        }
        if let Some(closed) = o.closed.as_deref() {
            super::advance_contract::for_blocker(closed);
        }
        if o.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&result.json_receipt()).unwrap()
            );
        } else {
            for line in result.render() {
                println!("{line}");
            }
        }
        0
    };
    if acquired {
        let _ = crate::claims::release(&key, &holder, None, None);
    }
    code
}
pub(super) fn rows() -> Result<Vec<Value>, String> {
    crate::graph_store::read_rows_strict(&super::settings::graph_path()).map_err(|e| e.to_string())
}
pub(super) fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}
pub fn mission_active(id: &str, active: bool) -> Result<bool, String> {
    let changed = std::cell::Cell::new(false);
    crate::graph_store::mutate_rows(
        &super::settings::graph_path(),
        std::time::Duration::from_secs(30),
        None,
        None,
        |rows| {
            changed.set(false);
            let Some(row) = rows.iter_mut().find(|r| s(r, "id") == id) else {
                return Ok(false);
            };
            let Some(obj) = row.as_object_mut() else {
                return Ok(false);
            };
            if active {
                if obj.get("mission_active") != Some(&Value::Bool(true)) {
                    obj.insert("mission_active".into(), json!(true));
                    changed.set(true);
                }
            } else if obj.remove("mission_active").is_some() {
                changed.set(true);
            }
            Ok(changed.get())
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(changed.get())
}
pub(super) fn ready(
    entries: &[Value],
    project: Option<&str>,
    epic: Option<&str>,
) -> Result<Vec<Value>, String> {
    let occupied = advance::live_worked_overlay()?.into_keys().collect();
    let opts = crate::backlog_ready::ReadyOpts {
        project: project.map(str::to_string),
        all: project.is_none(),
        parent: epic.map(str::to_string),
        repo_root: Some(
            super::collision::find_repo_root()
                .to_string_lossy()
                .into_owned(),
        ),
        claimed: occupied,
        held: advance::held_questions(),
        now_ms: crate::claims::now_ms(),
        staleness_days: Some(advance::guard_staleness_days(None)),
        ..Default::default()
    };
    crate::backlog_ready::select(entries, &opts)
        .map(|r| r.rows)
        .map_err(|e| e.0)
}

#[derive(Debug)]
pub(super) struct Budget {
    pub(super) fleet: i64,
    pub(super) remaining: BTreeMap<String, i64>,
    pub(super) binding: Option<String>,
    pub(super) used: BTreeMap<String, i64>,
}
pub(super) fn budget() -> Budget {
    let probe = crate::spawn_gate_verb::probe_answer(
        &json!({"mode":"probe","caller_session":crate::claims::resolve_identity().1}),
    );
    let fleet = if matches!(s(&probe, "verdict"), "unknown" | "refused") {
        0
    } else {
        probe.get("max_live").and_then(Value::as_i64).unwrap_or(0)
            - probe
                .get("live_workers")
                .and_then(Value::as_i64)
                .unwrap_or(0)
    };
    let remaining = probe["lanes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.as_str() != "fleet")
        .filter_map(|(k, v)| Some((k.clone(), v["cap"].as_i64()? - v["live"].as_i64()?)))
        .collect::<BTreeMap<_, _>>();
    let binding = remaining
        .iter()
        .min_by_key(|(_, n)| **n)
        .map(|(k, _)| k.clone());
    Budget {
        fleet,
        remaining,
        binding,
        used: BTreeMap::new(),
    }
}
fn priced_lane(child: &Value, model: Option<&str>, provider: Option<&str>) -> Option<String> {
    let args = super::advance_dispatch::resolve_node_spawn(
        s(child, "id"),
        child["cwd"].as_str(),
        child["slug"].as_str(),
        child,
        model,
        provider,
        None,
        None,
        None,
        None,
    )
    .ok()?;
    let mut argv_tail = Vec::new();
    for (flag, value) in [
        ("--route", args.route.or(args.resolved_route)),
        ("--provider", args.vendor),
        ("--model", args.model),
    ] {
        if let Some(value) = value {
            argv_tail.extend([flag.to_string(), value]);
        }
    }
    crate::spawn_overlay::resolve(
        json!({"kind":"lane-vendor","argv_tail":argv_tail,"harness":args.resolved_harness,"env_harness":null,"argv_head":"fno"}),
    ).ok().and_then(|answer|answer["vendor"].as_str().map(str::to_string))
}
pub(super) fn cap_verdict(
    child: &Value,
    budget: &Budget,
    total: i64,
    model: Option<&str>,
    provider: Option<&str>,
) -> (bool, Option<String>, Option<i64>) {
    if total >= budget.fleet {
        return (true, None, Some(budget.fleet - total));
    }
    let lane = priced_lane(child, model, provider);
    if let Some(lane) = &lane {
        if let Some(n) = budget.remaining.get(lane) {
            let left = n - budget.used.get(lane).copied().unwrap_or(0);
            return (left <= 0, Some(lane.clone()), Some(left));
        }
        return (false, Some(lane.clone()), None);
    }
    let left = budget
        .binding
        .as_ref()
        .and_then(|l| budget.remaining.get(l))
        .map(|n| n - total);
    (left.is_some_and(|n| n <= 0), None, left)
}
fn drain(o: &Args) -> Result<Value, String> {
    let entries = rows()?;
    let mut label = o
        .epic
        .as_deref()
        .or(o.project.as_deref())
        .unwrap_or("")
        .to_string();
    if let Some(epic) = o.epic.as_deref() {
        let Some(index) = crate::backlog_ready::find_node_index(&entries, epic).0 else {
            return Ok(
                json!({"epic_id":label,"error":"no-such-node","activated":false,"deactivated":false,"all_done":false,"dispatched":[],"children":[]}),
            );
        };
        let row = &entries[index];
        label = s(row, "id").into();
        let descendants = crate::backlog_ready::descendants_of(&entries, &label);
        if descendants.is_empty() {
            return Ok(
                json!({"epic_id":label,"error":"not-a-container","activated":false,"deactivated":false,"all_done":false,"dispatched":[],"children":[]}),
            );
        }
        if o.stop {
            mission_active(&label, false)?;
            advance::advance_emit(
                "mission_deactivated",
                json!({"epic_id":label,"reason":"stop"}),
                None,
            );
            return Ok(
                json!({"epic_id":label,"error":null,"activated":false,"deactivated":true,"all_done":false,"dispatched":[],"children":[]}),
            );
        }
        let (armed, rank) = advance::auto_continue_resolve(None);
        if !armed {
            advance::advance_emit(
                advance::EVENT_SKIPPED,
                json!({"reason":"disabled","mission":label,"rank":rank}),
                None,
            );
            return Ok(
                json!({"epic_id":label,"error":"disabled","activated":false,"deactivated":false,"all_done":false,"dispatched":[],"children":[]}),
            );
        }
        let kids = entries
            .iter()
            .filter(|child| descendants.contains(s(child, "id")))
            .collect::<Vec<_>>();
        let all_done = crate::backlog_ready::truthy(row.get("completed_at"))
            || super::workflows::children_all_closed(row, &kids);
        if all_done {
            mission_active(&label, false)?;
            advance::advance_emit(
                "mission_deactivated",
                json!({"epic_id":label,"reason":"complete","epic_closed":crate::backlog_ready::truthy(row.get("completed_at"))}),
                None,
            );
            return Ok(
                json!({"epic_id":label,"error":null,"activated":false,"deactivated":true,"all_done":true,"dispatched":[],"children":[]}),
            );
        }
        if o.continuation {
            if !crate::backlog_ready::truthy(row.get("mission_active")) {
                return Ok(
                    json!({"epic_id":label,"error":null,"activated":false,"deactivated":true,"all_done":false,"dispatched":[],"children":[]}),
                );
            }
        } else if mission_active(&label, true)? {
            advance::advance_emit("mission_activated", json!({"epic_id":label}), None);
        }
    } else if !advance::auto_continue_enabled(None) {
        return Ok(
            json!({"epic_id":label,"error":"disabled","activated":false,"deactivated":false,"all_done":false,"dispatched":[],"children":[]}),
        );
    }
    let children = ready(
        &entries,
        if o.loose { o.project.as_deref() } else { None },
        if o.loose { None } else { Some(&label) },
    )?
    .into_iter()
    .filter(|r| !o.loose || s(r, "parent").is_empty())
    .collect::<Vec<_>>();
    let mut budget = budget();
    if o.loose {
        if let Some(n) = budget
            .binding
            .as_ref()
            .and_then(|lane| budget.remaining.get(lane))
        {
            budget.fleet = budget.fleet.min(*n);
        }
    }
    let mut results = Vec::new();
    let mut dispatched = Vec::new();
    for (index, child) in children.iter().enumerate() {
        let total = dispatched.len() as i64;
        if o.max.is_some_and(|n| total >= n) {
            break;
        }
        let project = s(child, "project");
        let root = super::triage::project_root_from_settings(Some(project));
        let (capped, lane, headroom) = cap_verdict(
            child,
            &budget,
            total,
            o.model.as_deref(),
            o.provider.as_deref(),
        );
        let reason = if project.is_empty() {
            Some("no-project")
        } else if root.is_none() {
            Some("unmapped-project")
        } else if capped {
            Some("lane-cap")
        } else {
            None
        };
        let result = if let Some(reason) = reason {
            let detail = if reason == "lane-cap" {
                format!(
                    "{project}: lane={} headroom={} (spawn gate)",
                    lane.as_deref().unwrap_or("fleet"),
                    headroom.unwrap_or(0)
                )
            } else {
                project.to_string()
            };
            advance::advance_emit(
                advance::EVENT_SKIPPED,
                json!({"reason":reason,"node_id":child["id"],"mission":label,"detail":detail}),
                None,
            );
            let mut r = AdvanceResult::new(
                "skipped",
                advance::EVENT_SKIPPED,
                Some(reason.into()),
                Some(s(child, "id").into()),
            );
            r.detail = Some(detail);
            r
        } else {
            advance::converge_one(
                child,
                root.as_deref().unwrap(),
                None,
                o.verbose,
                !o.loose,
                Some(&label),
                None,
                o.model.as_deref(),
                o.provider.as_deref(),
                None,
                o.source.as_deref(),
            )
        };
        if result.decision == "dispatched" {
            dispatched.push(
                result
                    .node_id
                    .clone()
                    .unwrap_or_else(|| s(child, "id").into()),
            );
            if let Some(lane) = lane {
                *budget.used.entry(lane).or_default() += 1;
            }
        }
        let global = result.decision == "skipped"
            && result
                .exit_code
                .is_some_and(|code| code != crate::spawn_gate::EXIT_PROVIDER_CAP);
        results.push(child_receipt(&result));
        if global {
            for child in &children[index + 1..] {
                advance::advance_emit(
                    advance::EVENT_SKIPPED,
                    json!({"reason":result.reason,"node_id":child["id"],"mission":label,"attempted":false}),
                    None,
                );
                results.push(json!({"node_id":child["id"],"decision":"skipped","reason":result.reason,"detail":null,"short_id":null,"substrate":null,"notes":[]}));
            }
            break;
        }
    }
    Ok(
        json!({"epic_id":label,"error":null,"activated":!o.loose,"deactivated":false,"all_done":false,"dispatched":dispatched,"children":results}),
    )
}
fn child_receipt(r: &AdvanceResult) -> Value {
    json!({"node_id":r.node_id,"decision":r.decision,"reason":r.reason,"detail":r.detail,"short_id":r.short_id,"substrate":r.substrate,"notes":r.notes})
}
fn render_drain(v: &Value, o: &Args) -> i32 {
    let kind = if o.loose { "loose" } else { "epic" };
    let label = s(v, "epic_id");
    let error = s(v, "error");
    if o.json {
        println!("{}", serde_json::to_string_pretty(v).unwrap())
    } else if !error.is_empty() {
        if matches!(error, "no-such-node" | "not-a-container") {
            eprintln!("{kind} {label}: {error}")
        } else {
            println!("{kind} {label}: {error}")
        }
    } else if v["deactivated"] == true {
        println!(
            "{kind} {label}: mission deactivated ({})",
            if v["all_done"] == true {
                "complete"
            } else {
                "stopped"
            }
        )
    } else {
        println!(
            "{kind} {label}: dispatched {}",
            v["dispatched"].as_array().map(Vec::len).unwrap_or(0)
        );
        for child in v["children"].as_array().into_iter().flatten() {
            for note in child["notes"].as_array().into_iter().flatten() {
                println!(
                    "{kind} {label}: {}: {}",
                    s(child, "node_id"),
                    note.as_str().unwrap_or("")
                );
            }
        }
    }
    if matches!(error, "no-such-node" | "not-a-container") {
        1
    } else {
        0
    }
}

pub(super) fn spawn_error(e: super::advance_dispatch::SpawnOutcome) -> String {
    match e {
        super::advance_dispatch::SpawnOutcome::AlreadyRunning(s) => s,
        super::advance_dispatch::SpawnOutcome::Failed(e) => {
            if e.detail.is_empty() {
                e.message
            } else {
                e.detail
            }
        }
    }
}
fn run_request() -> i32 {
    use std::io::Read;
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        return 2;
    }
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("advance request: {e}");
            return 2;
        }
    };
    let result: Result<Value, String> = match s(&v, "op") {
        "selection-guards" => {
            let by_id: BTreeMap<String, Value> = v["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["id"].as_str().map(|id| (id.to_string(), row.clone())))
                .collect();
            let now = v["now_ms"].as_i64().unwrap_or_else(crate::claims::now_ms);
            let stale = v["staleness_days"]
                .as_i64()
                .unwrap_or_else(|| advance::guard_staleness_days(None));
            let held = advance::held_questions();
            let mut guards = serde_json::Map::new();
            for (id, row) in &by_id {
                let mut dead = None;
                let mut parent = row["parent"].as_str();
                let mut seen = BTreeSet::new();
                while let Some(pid) = parent {
                    if seen.len() >= 64 || !seen.insert(pid) {
                        break;
                    }
                    let Some(ancestor) = by_id.get(pid) else {
                        break;
                    };
                    let terminal = crate::backlog_ready::truthy(ancestor.get("completed_at"))
                        || crate::backlog_ready::truthy(ancestor.get("deferred_at"))
                        || (crate::backlog_ready::truthy(ancestor.get("superseded_by"))
                            && (!ancestor["supersession"].is_object()
                                || crate::backlog_ready::truthy(
                                    ancestor["supersession"].get("verified_at"),
                                )));
                    if terminal {
                        dead = Some(pid.to_string());
                        break;
                    }
                    parent = ancestor["parent"].as_str();
                }
                guards.insert(id.clone(),json!({"reason":crate::backlog_ready::selection_guards(row,&by_id,now,stale,&held),"dead_ancestor":dead}));
            }
            Ok(json!({"guards":guards,"staleness_days":stale}))
        }
        "staleness" => Ok(
            json!({"days":advance::guard_staleness_days(v["project_root"].as_str().map(Path::new))}),
        ),
        "armed" => {
            let (a, r) = advance::auto_continue_resolve(v["project_root"].as_str().map(Path::new));
            Ok(json!({"armed":a,"rank":r}))
        }
        "fire-pending" => {
            super::advance_contract::queue_pending(s(&v, "node_id"), Path::new(s(&v, "root")))
                .map(|v| v.unwrap_or(Value::Null))
        }
        "fire-pending-now" => Ok(super::advance_contract::fire_pending(
            s(&v, "node_id"),
            Path::new(s(&v, "root")),
        )
        .unwrap_or(Value::Null)),
        "width" => super::advance_join::plan_graph(Path::new(s(&v, "plan_path")))
            .and_then(|g| super::advance_join::width(&g))
            .map(|n| json!({"width":n})),
        "mission" => mission_active(s(&v, "node_id"), v["active"] == true)
            .map(|changed| json!({"changed":changed})),
        "gate-detail" => Ok(json!({"detail":advance::gate_refusal_detail(s(&v,"stderr"))})),
        "budget" => {
            let b = budget();
            Ok(
                json!({"fleet":b.fleet,"vendor_remaining":b.remaining,"binding":b.binding,"binding_remaining":b.binding.as_ref().and_then(|l|b.remaining.get(l)),"dispatched_by_vendor":b.used}),
            )
        }
        "claim" => {
            let o = advance::observe_node_claim(
                s(&v, "node_id"),
                v["cwd"].as_str(),
                v["enforce_failure_limit"].as_bool().unwrap_or(true),
                v["emit_events"].as_bool().unwrap_or(true),
                None,
                None,
                None,
                None,
            );
            let warning = if v["emit_events"].as_bool().unwrap_or(true) {
                o.contested_warning(s(&v, "node_id"))
            } else {
                None
            };
            Ok(
                json!({"verdict":o.verdict,"claim_state":o.claim_state,"holder":o.holder,"truth_status":o.truth_status,"action":o.action,"worker":o.worker,"block_reason":o.block_reason,"worked_error":o.worked_error,"blocks_dispatch":o.blocks_dispatch(),"refusal_reason":o.refusal_reason(),"warning":warning}),
            )
        }
        "guard" => Ok(
            json!({"reason":advance::node_dispatch_block_reason(s(&v,"node_id"),v["cwd"].as_str(),None,None,None)}),
        ),
        "spawn" => {
            let node = &v["node"];
            let id = v["node_id"].as_str().unwrap_or_else(|| s(node, "id"));
            let reservation = v["dispatch_reservation"]
                .as_array()
                .and_then(|r| Some((r.first()?.as_str()?, r.get(1)?.as_str()?)));
            super::advance_dispatch::resolve_node_spawn(
                id,
                v["cwd"].as_str(),
                v["slug"].as_str(),
                node,
                v["model"].as_str(),
                v["provider"].as_str(),
                v["harness"].as_str(),
                v["verb"].as_str(),
                v["brief"].as_str(),
                v["source"].as_str(),
            )
            .map_err(|e| e.message)
            .and_then(|mut args| {
                args.dispatch_account = v["dispatch_account"].as_str().map(str::to_string);
                args.vendor = v["vendor"].as_str().map(str::to_string);
                if let Some(env) = v["extra_env"].as_object() {
                    for (k, v) in env {
                        args.env.insert(k.clone(), scalar(v));
                    }
                }
                super::advance_dispatch::launch_resolved(
                    id,
                    args,
                    reservation,
                    v["caller"].as_str().unwrap_or("unknown"),
                    v["source"].as_str(),
                    None,
                )
                .map(|(id, row)| json!({"short_id":id,"receipt":row}))
                .map_err(spawn_error)
            })
        }
        other => Err(format!("unknown advance operation {other}")),
    };
    match result {
        Ok(v) => {
            println!("{v}");
            0
        }
        Err(e) => {
            eprintln!("advance request: {e}");
            1
        }
    }
}
fn scalar(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}
