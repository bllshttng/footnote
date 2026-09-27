//! `fno backlog update` - the legacy field-flag surface, ported from
//! `cmd_update` wave by wave. The dispatcher routes here when every flag in
//! the tail is one this port already owns ([`NATIVE_UPDATE_FLAGS`]); any
//! other shape still rides the compat forward, so the not-yet-ported flags
//! keep their Python answers until their wave lands. The cut-over wave
//! deletes that constant and the forward together.
//!
//! Door flags (`--status`/`--leave`/`--set`) relay to the patch door
//! in-process, refusing the mixed legacy+door call exactly as the Python
//! relay did; the receipts are the door's own bytes.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::node_ref;
use super::patch;
use crate::backlog::settings;
use crate::graph_store::{
    self, entry_id, recompute_statuses_with_plan_rungs, MutateInput, StoreError,
    DEFAULT_LOCK_TIMEOUT,
};

/// Flags the native engine owns so far. Widened per port wave; the cut-over
/// wave deletes it together with the forward arm.
const NATIVE_UPDATE_FLAGS: &[&str] = &[
    "--title",
    "-t",
    "--details",
    "--description",
    "-d",
    "--details-file",
    "--domain",
    "--size",
    "--model",
    "--model-tier",
    "--public",
    "--no-public",
    "--batch",
    "--orphan-ok",
    "--has-brief",
    "--priority",
    "-p",
    "--blocks-everything",
    "--project",
    "--cwd",
    "-c",
    "--completion-note",
    "--acknowledge-collisions",
    "--fixes-pr",
    "--reverted",
    "--no-reverted",
    "--help",
    "-h",
    "--status",
    "--leave",
    "--set",
];

const DOOR_FLAGS: &[&str] = &["--status", "--leave", "--set"];

/// `--flag=value` spellings the mechanical kebab rule cannot produce.
const MODEL_TIER_FLAG: &str = "--model-tier";

/// The lock deadline every update write rides.
const LOCK_TIMEOUT: Duration = DEFAULT_LOCK_TIMEOUT;

/// One parsed invocation. A `None` from [`UpdateArgs::parse`] is the forward
/// shape: any token outside [`NATIVE_UPDATE_FLAGS`].
struct UpdateArgs {
    task_id: String,
    help: bool,
    title: Option<String>,
    details: Option<String>,
    details_file: Option<String>,
    domain: Option<String>,
    size: Option<String>,
    model: Option<String>,
    model_tier: Option<String>,
    type_: Option<String>,
    public: Option<bool>,
    batch: Option<String>,
    orphan_ok: Option<String>,
    has_brief: Option<String>,
    priority: Option<String>,
    blocks_everything: bool,
    project: Option<String>,
    cwd: Option<String>,
    completion_note: Option<String>,
    acknowledge_collisions: Option<String>,
    /// Raw token: a non-int value rides the forward, which carries typer's
    /// usage error until the cut-over wave owns it.
    fixes_pr: Option<String>,
    reverted: Option<bool>,
    door: Vec<String>,
}

impl UpdateArgs {
    fn parse(tail: &[String]) -> Option<UpdateArgs> {
        let mut a = UpdateArgs {
            task_id: String::new(),
            help: false,
            title: None,
            details: None,
            details_file: None,
            domain: None,
            size: None,
            model: None,
            model_tier: None,
            type_: None,
            public: None,
            batch: None,
            orphan_ok: None,
            has_brief: None,
            priority: None,
            blocks_everything: false,
            project: None,
            cwd: None,
            completion_note: None,
            acknowledge_collisions: None,
            fixes_pr: None,
            reverted: None,
            door: Vec::new(),
        };
        let mut i = 0;
        let mut id: Option<String> = None;
        while i < tail.len() {
            let raw = tail[i].as_str();
            let (name, inline) = match raw.split_once('=') {
                Some((n, v)) if n.starts_with('-') => (n.to_string(), Some(v.to_string())),
                _ => (raw.to_string(), None),
            };
            macro_rules! take_value {
                ($slot:expr) => {{
                    if !NATIVE_UPDATE_FLAGS.contains(&name.as_str()) {
                        return None;
                    }
                    match inline {
                        Some(ref v) => {
                            $slot = Some(v.clone());
                            i += 1;
                        }
                        None => match flag_value(&name, tail, &mut i) {
                            Some(v) => {
                                $slot = Some(v);
                                i += 1;
                            }
                            None => return None,
                        },
                    }
                }};
            }
            match name.as_str() {
                "--help" | "-h" => {
                    a.help = true;
                    i += 1;
                }
                "--status" | "--leave" | "--set" => {
                    match inline {
                        Some(_) => a.door.push(tail[i].clone()),
                        None => {
                            if i + 1 >= tail.len() {
                                return None;
                            }
                            a.door.push(tail[i].clone());
                            a.door.push(tail[i + 1].clone());
                        }
                    }
                    i += if inline.is_some() { 1 } else { 2 };
                    continue;
                }
                "--title" => take_value!(a.title),
                "--details" | "--description" => take_value!(a.details),
                "--details-file" => take_value!(a.details_file),
                "--domain" => take_value!(a.domain),
                "--size" => take_value!(a.size),
                "--model" => take_value!(a.model),
                "--model-tier" => take_value!(a.model_tier),
                "--type" => take_value!(a.type_),
                "--public" => {
                    a.public = Some(true);
                    i += 1;
                }
                "--no-public" => {
                    a.public = Some(false);
                    i += 1;
                }
                "--batch" => take_value!(a.batch),
                "--orphan-ok" => take_value!(a.orphan_ok),
                "--has-brief" => take_value!(a.has_brief),
                "--priority" | "-p" => take_value!(a.priority),
                "--blocks-everything" => {
                    a.blocks_everything = true;
                    i += 1;
                }
                "--project" => take_value!(a.project),
                "--cwd" | "-c" => take_value!(a.cwd),
                "--completion-note" => take_value!(a.completion_note),
                "--acknowledge-collisions" => take_value!(a.acknowledge_collisions),
                "--fixes-pr" => take_value!(a.fixes_pr),
                "--reverted" => {
                    a.reverted = Some(true);
                    i += 1;
                }
                "--no-reverted" => {
                    a.reverted = Some(false);
                    i += 1;
                }
                other => {
                    if !other.starts_with('-') && id.is_none() {
                        id = Some(other.to_string());
                        i += 1;
                    } else {
                        return None;
                    }
                }
            }
        }
        a.task_id = id?;
        Some(a)
    }
}

/// The value of a value-taking flag at `*i` (which names the flag itself):
/// the next token, when present and not flag-shaped. Flag-shaped values ride
/// the `=` spelling only, the same contract click parses.
fn flag_value(_name: &str, tail: &[String], i: &mut usize) -> Option<String> {
    let next = tail.get(*i + 1)?;
    if next.starts_with('-') && next.len() > 1 {
        return None;
    }
    *i += 1;
    Some(next.clone())
}

/// A validation refusal: one stderr line, then the exit.
struct Refusal {
    message: String,
    exit: i32,
}

fn refused(message: impl Into<String>, exit: i32) -> Refusal {
    Refusal {
        message: message.into(),
        exit,
    }
}

/// The run entry: returns the process exit code.
pub fn run(tail: &[String]) -> i32 {
    let Some(args) = UpdateArgs::parse(tail) else {
        return super::cli::forward_to_python("update", tail);
    };
    if args.help {
        print!("{}", UPDATE_HELP);
        return 0;
    }
    if !args.door.is_empty() {
        return run_door_relay(&args);
    }
    match run_native(&args) {
        Ok(()) => 0,
        Err(r) => {
            eprintln!("{}", r.message);
            r.exit
        }
    }
}

/// The door relay: refuse the mixed legacy+door call, then hand the door
/// flags to the patch door in-process, receipts and exit verbatim.
fn run_door_relay(args: &UpdateArgs) -> i32 {
    let mut legacy: Vec<&str> = Vec::new();
    let note = |flag: &'static str, set: bool, legacy: &mut Vec<&str>| {
        if set {
            legacy.push(flag);
        }
    };
    // Identity, not truthiness: `--fixes-pr 0` means clear, and 0 == false
    // would drop it from the refusal, the exact bug the zero-valued golden
    // pins.
    note(
        "--acknowledge-collisions",
        args.acknowledge_collisions.is_some(),
        &mut legacy,
    );
    note("--batch", args.batch.is_some(), &mut legacy);
    note("--blocks-everything", args.blocks_everything, &mut legacy);
    note(
        "--completion-note",
        args.completion_note.is_some(),
        &mut legacy,
    );
    note("--cwd", args.cwd.is_some(), &mut legacy);
    note("--details", args.details.is_some(), &mut legacy);
    note("--details-file", args.details_file.is_some(), &mut legacy);
    note("--domain", args.domain.is_some(), &mut legacy);
    note("--fixes-pr", args.fixes_pr.is_some(), &mut legacy);
    note("--has-brief", args.has_brief.is_some(), &mut legacy);
    note("--model", args.model.is_some(), &mut legacy);
    note("--model-tier", args.model_tier.is_some(), &mut legacy);
    note("--no-public", args.public == Some(false), &mut legacy);
    note("--orphan-ok", args.orphan_ok.is_some(), &mut legacy);
    note("--priority", args.priority.is_some(), &mut legacy);
    note("--project", args.project.is_some(), &mut legacy);
    note("--public", args.public == Some(true), &mut legacy);
    note("--reverted", args.reverted == Some(true), &mut legacy);
    note("--no-reverted", args.reverted == Some(false), &mut legacy);
    note("--size", args.size.is_some(), &mut legacy);
    note("--title", args.title.is_some(), &mut legacy);
    note("--type", args.type_.is_some(), &mut legacy);
    if !legacy.is_empty() {
        legacy.sort_unstable();
        eprintln!(
            "Error: --status/--leave/--set cannot be mixed with the legacy field flags \
             ({}). Run two calls: one through the door, one with the legacy flags.",
            legacy.join(", ")
        );
        return 2;
    }
    let mut door_args: Vec<String> = vec![
        "--node".into(),
        args.task_id.clone(),
        "--graph".into(),
        settings::graph_path().to_string_lossy().to_string(),
    ];
    door_args.extend(args.door.iter().cloned());
    patch::run_update(&door_args)
}

/// The native scalar path: validate outside the write, mutate, publish, read
/// back, receipt, repaint.
fn run_native(args: &UpdateArgs) -> Result<(), Refusal> {
    // Validation order follows cmd_update's, so multi-flag calls refuse on
    // the same first offense.
    if let Some(v) = &args.model_tier {
        let _ = v;
        return Err(refused(
            "--model-tier was retired: the work-difficulty axis is --difficulty \
             low|medium|high, an intrinsic property of the work \
             (`fno backlog update <id> --difficulty <band>`).",
            2,
        ));
    }
    let (details, details_from_file) = read_details(args)?;
    if !node_ref::has_node_id_prefix(&args.task_id) {
        return Err(refused(
            format!(
                "Error: task_id must be a <prefix>-<4..8 hex> node id, got '{}'",
                args.task_id
            ),
            1,
        ));
    }
    if let Some(priority) = &args.priority {
        if !["p0", "p1", "p2", "p3"].contains(&priority.as_str()) {
            return Err(refused(
                format!("Error: invalid priority '{priority}'. Must be: p0, p1, p2, p3"),
                1,
            ));
        }
        if priority == "p0" && !args.blocks_everything {
            return Err(refused(
                "Error: p0 blocks everything else, usually a bug. Use --blocks-everything \
                 only when a broken service or fleet-wide resource leak blocks all \
                 downstream work. If it does, say so with --blocks-everything; otherwise \
                 file it p1.",
                2,
            ));
        }
    }
    if let Some(project) = &args.project {
        if project.trim().is_empty() {
            return Err(refused("Error: --project must be a non-empty string", 1));
        }
    }
    if let Some(size) = &args.size {
        if size.to_lowercase() != "null" && !["S", "M", "L"].contains(&size.to_uppercase().as_str())
        {
            return Err(refused(
                format!("Error: invalid size '{size}'. Must be one of: S, M, L"),
                1,
            ));
        }
    }
    if let Some(model) = &args.model {
        if model.to_lowercase() != "null" && !model_token_ok(model) {
            return Err(refused(
                "Error: --model must be a single token of [A-Za-z0-9._:/-], at most 64 \
                 chars (e.g. fable|opus|sonnet or a full provider-model id); no whitespace \
                 or shell/glob metacharacters",
                1,
            ));
        }
    }
    if let Some(type_) = &args.type_ {
        if !["bug", "epic", "feature", "roadmap"].contains(&type_.as_str()) {
            return Err(refused(
                format!(
                    "Error: invalid type '{type_}'. Must be one of: bug, epic, feature, roadmap"
                ),
                1,
            ));
        }
    }
    // The work-map cwd derive happens outside the write, the same as the
    // Python pre-lock pass: settings reads never ride the graph lock.
    let derived_cwd: Option<String> = if args.project.is_some() && args.cwd.is_none() {
        let project = args.project.as_deref().unwrap_or_default();
        match settings::project_root(project) {
            Some(root) => Some(root),
            None => {
                eprintln!(
                    "warning: project '{project}' not in any settings.yaml work-map; \
                     cwd left unchanged"
                );
                None
            }
        }
    } else {
        None
    };
    if let Some(cwd) = &args.cwd {
        if cwd.trim().is_empty() {
            return Err(refused("Error: --cwd must be a non-empty string", 1));
        }
    }

    let graph = settings::graph_path();
    write_update(
        &graph,
        args,
        details.as_deref(),
        details_from_file,
        derived_cwd.as_deref(),
    )?;
    Ok(())
}

/// `[A-Za-z0-9._:/-]{1,64}` fullmatch.
fn model_token_ok(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 64
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

/// `read_text_arg`: refuse both forms at once, read `-` as stdin, refuse an
/// unreadable file. A file's content is data - the `null` clear-sentinel is
/// CLI-only, tracked by the second return value.
fn read_details(args: &UpdateArgs) -> Result<(Option<String>, bool), Refusal> {
    match (&args.details, &args.details_file) {
        (Some(_), Some(_)) => Err(refused(
            "error: provide the details once - inline or as a file, not both",
            1,
        )),
        (inline, None) => Ok((inline.clone(), false)),
        (None, Some(path)) => {
            if path == "-" {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .map_err(|e| refused(format!("error: cannot read -: {e}"), 1))?;
                Ok((Some(buf), true))
            } else {
                match std::fs::read_to_string(path) {
                    Ok(body) => Ok((Some(body), true)),
                    Err(e) => Err(refused(format!("error: cannot read {path}: {e}"), 1)),
                }
            }
        }
    }
}

/// The write cycle: read, recompute, mutate, publish with retry. Returns the
/// read-back-confirmed node id.
fn write_update(
    graph: &Path,
    args: &UpdateArgs,
    details: Option<&str>,
    details_from_file: bool,
    derived_cwd: Option<&str>,
) -> Result<(), Refusal> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        let rows = graph_store::read_rows(graph)
            .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
        let planned = plan_mutation(graph, rows, args, details, details_from_file, derived_cwd)?;
        match planned {
            MutationPlan::Refused(r) => return Err(r),
            MutationPlan::Applied {
                working,
                rungs,
                node_id,
            } => {
                match graph_store::locked_mutate(
                    graph,
                    MutateInput {
                        entries: working,
                        canonical_path: None,
                        base_version: graph_store::base_version(graph)
                            .map_err(|e| refused(format!("graph read failed: {e}"), 1))?,
                        plan_rungs: Some(rungs),
                    },
                    LOCK_TIMEOUT,
                ) {
                    Ok(_) => {
                        confirm_readback(graph, args, &node_id)?;
                        println!("Updated {node_id}");
                        repaint(graph, args, &node_id);
                        return Ok(());
                    }
                    Err(StoreError::Conflict | StoreError::LockTimeout(..))
                        if attempt + 1 < ATTEMPTS =>
                    {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(StoreError::Conflict) => {
                        return Err(refused(
                            format!("refused: {node_id} graph changed under the write after {ATTEMPTS} attempts"),
                            2,
                        ));
                    }
                    Err(StoreError::LockTimeout(..)) => {
                        return Err(refused(
                            format!("refused: {node_id} the graph lock stayed busy across {ATTEMPTS} attempts"),
                            2,
                        ));
                    }
                    Err(e) => return Err(refused(format!("graph write failed: {e}"), 1)),
                }
            }
        }
    }
    unreachable!("every loop arm returns")
}

enum MutationPlan {
    Refused(Refusal),
    Applied {
        working: Vec<Value>,
        rungs: BTreeMap<String, String>,
        node_id: String,
    },
}

#[allow(clippy::too_many_arguments)]
fn plan_mutation(
    graph: &Path,
    mut rows: Vec<Value>,
    args: &UpdateArgs,
    details: Option<&str>,
    details_from_file: bool,
    derived_cwd: Option<&str>,
) -> Result<MutationPlan, Refusal> {
    let _ = graph;
    let rungs: BTreeMap<String, String> = rows
        .iter()
        .filter_map(|e| {
            entry_id(e).map(|id| {
                (
                    id.to_string(),
                    crate::backlog_ready::plan_rung(e).to_string(),
                )
            })
        })
        .collect();
    recompute_statuses_with_plan_rungs(&mut rows, Some(&rungs));

    // The lookup runs over the RAW snapshot, archived residents included -
    // the captured Python behavior: an archived node takes the write, the
    // write lands, and the LIVE read-back below refuses it (never a silent
    // success, never the absent-node error).
    let node = match node_ref::find_node(&rows, &args.task_id) {
        Some(node) => node.clone(),
        None => {
            return Err(refused(
                format!("Error: graph node {} not found", args.task_id),
                1,
            ));
        }
    };
    let node_id = node
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let idx = rows
        .iter()
        .position(|r| entry_id(r) == Some(node_id.as_str()))
        .ok_or_else(|| refused(format!("Error: graph node {} not found", args.task_id), 1))?;
    {
        let obj = rows[idx].as_object_mut().expect("row is an object");
        apply_mutators(
            obj,
            node.clone(),
            args,
            details,
            details_from_file,
            derived_cwd,
        )?;
    }

    Ok(MutationPlan::Applied {
        working: rows,
        rungs,
        node_id,
    })
}

/// The wave-2 mutator arms, in cmd_update's field order. Topology, lock,
/// PR-attribution and dispatch arms land with their waves.
fn apply_mutators(
    obj: &mut Map<String, Value>,
    node: Value,
    args: &UpdateArgs,
    details: Option<&str>,
    details_from_file: bool,
    derived_cwd: Option<&str>,
) -> Result<(), Refusal> {
    if let Some(v) = &args.has_brief {
        obj.insert("has_brief".into(), Value::Bool(v.to_lowercase() == "true"));
    }
    if let Some(v) = &args.batch {
        obj.insert(
            "batch".into(),
            null_if(v).map(|s| json!(s)).unwrap_or(Value::Null),
        );
    }
    if let Some(v) = &args.orphan_ok {
        if v.to_lowercase() == "null" {
            obj.insert("orphan_ok".into(), Value::Null);
        } else if v.trim().is_empty() {
            return Err(refused(
                "Error: --orphan-ok needs a reason (or 'null' to clear)",
                2,
            ));
        } else {
            obj.insert("orphan_ok".into(), json!(v));
        }
    }
    if let Some(priority) = &args.priority {
        obj.insert("priority".into(), json!(priority));
    }
    if args.blocks_everything {
        let effective = args.priority.clone().or_else(|| {
            node.get("priority")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if effective.as_deref() == Some("p0") {
            obj.insert("blocks_everything".into(), Value::Bool(true));
        } else {
            let shown = node
                .get("priority")
                .map(|p| match p {
                    Value::String(s) => format!("'{s}'"),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "None".into());
            return Err(refused(
                format!(
                    "Error: --blocks-everything acknowledges p0; pass --priority p0 too \
                     (node priority is {shown})"
                ),
                2,
            ));
        }
    }
    if let Some(project) = &args.project {
        obj.insert("project".into(), json!(project));
    }
    if let Some(cwd) = &args.cwd {
        obj.insert("cwd".into(), json!(abs_expand(cwd)));
    } else if let Some(derived) = derived_cwd {
        obj.insert("cwd".into(), json!(derived));
    }
    if let Some(title) = &args.title {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Err(refused(
                "Error: --title cannot be empty or whitespace-only",
                1,
            ));
        }
        obj.insert("title".into(), json!(trimmed));
    }
    if let Some(details) = details {
        let cleared = !details_from_file && details.to_lowercase() == "null";
        obj.insert(
            "details".into(),
            if cleared { Value::Null } else { json!(details) },
        );
    }
    if let Some(domain) = &args.domain {
        obj.insert("domain".into(), json!(domain));
    }
    if let Some(size) = &args.size {
        obj.insert(
            "size".into(),
            if size.to_lowercase() == "null" {
                Value::Null
            } else {
                json!(size.to_uppercase())
            },
        );
    }
    if let Some(model) = &args.model {
        obj.insert(
            "model".into(),
            null_if(model).map(|s| json!(s)).unwrap_or(Value::Null),
        );
    }
    if let Some(type_) = &args.type_ {
        obj.insert("type".into(), json!(type_));
    }
    if let Some(public) = args.public {
        obj.insert("public".into(), Value::Bool(public));
    }
    if let Some(v) = &args.acknowledge_collisions {
        let ids: Vec<&str> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        obj.insert("collisions_acknowledged".into(), json!(ids));
    }
    if let Some(note) = &args.completion_note {
        if note.to_lowercase() == "null" {
            obj.insert("completion_note".into(), Value::Null);
        } else {
            let trimmed = note.trim();
            if !trimmed.is_empty() {
                let existing = node
                    .get("completion_note")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let merged = match existing {
                    Some(prev) => format!("{prev} + {trimmed}"),
                    None => trimmed.to_string(),
                };
                obj.insert("completion_note".into(), json!(merged));
            }
        }
    }
    if let Some(raw) = &args.fixes_pr {
        match raw.parse::<i64>() {
            Ok(0) => {
                obj.insert("fixes_pr".into(), Value::Null);
            }
            Ok(n) => {
                obj.insert("fixes_pr".into(), json!(n));
            }
            Err(_) => {
                return Err(refused(
                    format!("Error: --fixes-pr {raw:?} is not a number"),
                    2,
                ))
            }
        }
    }
    if let Some(reverted) = args.reverted {
        obj.insert("reverted".into(), Value::Bool(reverted));
    }
    Ok(())
}

/// The `null` clear-sentinel: "null" (any case) clears, anything else passes.
fn null_if(v: &str) -> Option<String> {
    if v.to_lowercase() == "null" {
        None
    } else {
        Some(v.to_string())
    }
}

/// `os.path.abspath(os.path.expanduser(value))`'s twin: `~` expands against
/// HOME, relative paths join the process cwd, then lexically normalizes.
fn abs_expand(value: &str) -> String {
    let expanded = match value.strip_prefix("~") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}{rest}"),
            Err(_) => value.to_string(),
        },
        None => value.to_string(),
    };
    let path = PathBuf::from(&expanded);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized.to_string_lossy().into_owned()
}

/// The read-back receipt guard: an `Updated` line the store cannot confirm is
/// an error, never a success.
fn confirm_readback(graph: &Path, args: &UpdateArgs, node_id: &str) -> Result<(), Refusal> {
    let _ = args;
    let rows =
        graph_store::read_rows(graph).map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
    // LIVE rows only: the archived case lands its write and then fails
    // exactly here, the captured contract.
    let live: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("archived_at").is_none())
        .collect();
    if !live.iter().any(|r| entry_id(r) == Some(node_id)) {
        return Err(refused(
            format!(
                "Error: the update of {node_id} reported success but the row does not \
                 read back; the write did not land"
            ),
            1,
        ));
    }
    Ok(())
}

/// The plan repaint: graph-authoritative fields flow onto the linked plan
/// when a mirrored or status-affecting field changed. Best-effort.
fn repaint(graph: &Path, args: &UpdateArgs, node_id: &str) {
    let mirror_triggers = args.priority.is_some()
        || args.project.is_some()
        || args.type_.is_some()
        || args.size.is_some();
    if !mirror_triggers {
        return;
    }
    let Ok(rows) = graph_store::read_rows(graph) else {
        return;
    };
    let mut mirror_keys: Vec<String> = Vec::new();
    if args.type_.is_some() {
        mirror_keys.push("type".into());
    }
    crate::plan_doc::project::project_graph_nodes(
        &rows,
        std::slice::from_ref(&node_id.to_string()),
        None,
        Some((node_id.to_string(), mirror_keys)),
        None,
        None,
    );
}

/// The captured `fno backlog update --help` (typer's rendering, byte for
/// byte); the cut-over wave prints this where the forward used to.
const UPDATE_HELP: &str = include_str!("update_help.txt");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_scalars_and_door_flags() {
        let tail: Vec<String> = ["x-aaaa1111", "--title", "T", "--set=size=L", "--public"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = UpdateArgs::parse(&tail).expect("parses");
        assert_eq!(a.task_id, "x-aaaa1111");
        assert_eq!(a.title.as_deref(), Some("T"));
        assert_eq!(a.public, Some(true));
        assert_eq!(a.door, vec!["--set=size=L".to_string()]);
    }

    #[test]
    fn an_unknown_or_not_yet_native_flag_rides_the_forward() {
        let retired: Vec<String> = ["x-aaaa1111", "--completed", "yes"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(UpdateArgs::parse(&retired).is_none());
        let later_wave: Vec<String> = ["x-aaaa1111", "--parent", "x-eeee5555"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(UpdateArgs::parse(&later_wave).is_none());
    }

    #[test]
    fn model_token_gate_matches_the_python_regex() {
        assert!(model_token_ok("glm-5.3-flash"));
        assert!(model_token_ok("a"));
        assert!(!model_token_ok("two words"));
        assert!(!model_token_ok("bad*glob"));
        assert!(!model_token_ok(&"x".repeat(65)));
    }

    #[test]
    fn abs_expand_normalizes_like_os_path_abspath() {
        // A relative join collapses; an absolute path passes through.
        let abs = abs_expand("/tmp/x");
        assert_eq!(abs, "/tmp/x");
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert!(abs_expand("~/x").starts_with(&home));
        }
    }
}
