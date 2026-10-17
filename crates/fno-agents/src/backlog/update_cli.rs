//! `fno backlog update` - the legacy field-flag surface, ported from
//! `cmd_update` wave by wave and cut over fully native: every flag parses
//! here, and any other shape refuses with the retired-forward message.
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
use super::update_ops;
use crate::backlog::settings;
use crate::graph_store::{
    self, entry_id, recompute_statuses_with_plan_rungs, MutateInput, StoreError,
    DEFAULT_LOCK_TIMEOUT,
};

/// The lock deadline every update write rides.
const LOCK_TIMEOUT: Duration = DEFAULT_LOCK_TIMEOUT;

/// The parse outcome: usable args, or a usage refusal this verb owns.
enum ParsedUpdate {
    Args(UpdateArgs),
    Refusal { message: String, exit: i32 },
}

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
    /// `completion_note=text` (a replace, not an append); written before the
    /// append slot so a same-call `+=` joins the replaced text.
    completion_note_replace: Option<String>,
    acknowledge_collisions: Option<String>,
    /// Raw token; parse validates the int shape with typer's message.
    fixes_pr: Option<String>,
    reverted: Option<bool>,
    difficulty: Option<String>,
    tag: Vec<String>,
    untag: Vec<String>,
    dispatch_verb: Option<String>,
    dispatch_brief: Option<String>,
    locked_by: Option<String>,
    locked_by_harness: Option<String>,
    locked_by_harness_session: Option<String>,
    plan_path: Option<String>,
    force: bool,
    parent: Option<String>,
    caused_by: Option<String>,
    source_node: Option<String>,
    /// `None` = flag absent; `Some(list)` = the replace spelling.
    blocked_by: Option<Vec<String>>,
    add_blocker: Vec<String>,
    remove_blocker: Vec<String>,
    related: Vec<String>,
    /// `--pr-number` accepts 'null'; `--add-pr`/`--remove-pr` carry
    /// typer's int gate as a raw token the PR block validates.
    pr_number: Option<String>,
    pr_url: Option<String>,
    repo: Option<String>,
    add_pr: Option<String>,
    add_pr_url: Option<String>,
    add_pr_note: Option<String>,
    remove_pr: Option<String>,
    door: Vec<String>,
    /// Lowered operator tokens (`field=value`, `field+=value`,
    /// `field-=value`), in argument order; the relay refusal quotes them as
    /// typed.
    ops: Vec<update_ops::FieldOp>,
    /// `details+=` values, folded through `update_ops::append_text` inside
    /// the write attempt.
    details_append: Vec<String>,
    /// `dispatch_brief+=` values, merged into the brief before the override
    /// write so the 8 KB warning measures the merged text.
    brief_append: Vec<String>,
    /// `tags=v` list tokens; the writer normalizes and refuses malformed
    /// tags before any write.
    tags_replace: Option<Vec<String>>,
    /// `related+=` / `related-=` list tokens, resolved against the store
    /// inside the write attempt.
    related_add: Vec<String>,
    related_remove: Vec<String>,
    /// The replace slot was filled by `blocked_by=`, not `--blocked-by`: the
    /// mutual-exclusion refusal names the form that wrote the slot.
    blocked_by_replace_from_op: bool,
}

impl UpdateArgs {
    fn parse(tail: &[String]) -> ParsedUpdate {
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
            completion_note_replace: None,
            acknowledge_collisions: None,
            fixes_pr: None,
            reverted: None,
            difficulty: None,
            tag: Vec::new(),
            untag: Vec::new(),
            dispatch_verb: None,
            dispatch_brief: None,
            locked_by: None,
            locked_by_harness: None,
            locked_by_harness_session: None,
            plan_path: None,
            force: false,
            parent: None,
            caused_by: None,
            source_node: None,
            blocked_by: None,
            add_blocker: Vec::new(),
            remove_blocker: Vec::new(),
            related: Vec::new(),
            pr_number: None,
            pr_url: None,
            repo: None,
            add_pr: None,
            add_pr_url: None,
            add_pr_note: None,
            remove_pr: None,
            door: Vec::new(),
            ops: Vec::new(),
            details_append: Vec::new(),
            brief_append: Vec::new(),
            tags_replace: None,
            related_add: Vec::new(),
            related_remove: Vec::new(),
            blocked_by_replace_from_op: false,
        };
        let mut i = 0;
        let mut id: Option<String> = None;
        while i < tail.len() {
            let raw = tail[i].as_str();
            let (name, inline) = match raw.split_once('=') {
                Some((n, v)) if n.starts_with('-') => (n.to_string(), Some(v.to_string())),
                _ => (raw.to_string(), None),
            };
            macro_rules! take_bool_value {
                ($name:expr, $inline:expr, $i:expr, $bare:expr) => {{
                    $i += 1;
                    match $inline {
                        Some(ref v) => match parse_click_bool(v) {
                            Some(b) => b,
                            None => {
                                return ParsedUpdate::Refusal {
                                    message: format!(
                                        "Error: Invalid value for '{}': '{v}' \
                                     is not a valid boolean.",
                                        $name
                                    ),
                                    exit: 2,
                                }
                            }
                        },
                        None => $bare,
                    }
                }};
            }
            macro_rules! take_value {
                ($slot:expr) => {{
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
                            None => {
                                return ParsedUpdate::Refusal {
                                    message: format!(
                                        "Error: Option '{name}' requires an argument."
                                    ),
                                    exit: 2,
                                }
                            }
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
                                return ParsedUpdate::Refusal {
                                    message: format!(
                                        "Error: Option '{name}' requires an argument."
                                    ),
                                    exit: 2,
                                };
                            }
                            a.door.push(tail[i].clone());
                            a.door.push(tail[i + 1].clone());
                        }
                    }
                    i += if inline.is_some() { 1 } else { 2 };
                    continue;
                }
                "--title" | "-t" => take_value!(a.title),
                "--details" | "--description" | "-d" => take_value!(a.details),
                "--details-file" => take_value!(a.details_file),
                "--domain" => take_value!(a.domain),
                "--size" => take_value!(a.size),
                "--model" => take_value!(a.model),
                "--model-tier" => take_value!(a.model_tier),
                "--type" => take_value!(a.type_),
                "--public" | "--no-public" => {
                    a.public = Some(take_bool_value!(name, inline, i, name == "--public"));
                }
                "--batch" => take_value!(a.batch),
                "--orphan-ok" => take_value!(a.orphan_ok),
                "--has-brief" => take_value!(a.has_brief),
                "--priority" | "-p" => take_value!(a.priority),
                "--blocks-everything" => {
                    a.blocks_everything = take_bool_value!(name, inline, i, true);
                }
                "--project" => take_value!(a.project),
                "--cwd" | "-c" => take_value!(a.cwd),
                "--completion-note" => take_value!(a.completion_note),
                "--acknowledge-collisions" => take_value!(a.acknowledge_collisions),
                "--fixes-pr" => {
                    take_value!(a.fixes_pr);
                    if let Some(v) = &a.fixes_pr {
                        if let Some(message) = check_fixes_pr_int("--fixes-pr", v) {
                            return ParsedUpdate::Refusal { message, exit: 2 };
                        }
                    }
                }
                "--reverted" | "--no-reverted" => {
                    a.reverted = Some(take_bool_value!(name, inline, i, name == "--reverted"));
                }
                "--difficulty" => take_value!(a.difficulty),
                "--tag" | "--untag" => {
                    match inline.clone().or_else(|| {
                        if i + 1 < tail.len() && !tail[i + 1].starts_with('-') {
                            i += 1;
                            Some(tail[i].clone())
                        } else {
                            None
                        }
                    }) {
                        Some(v) => {
                            if name == "--tag" {
                                a.tag.push(v);
                            } else {
                                a.untag.push(v);
                            }
                            i += 1;
                        }
                        None => {
                            return ParsedUpdate::Refusal {
                                message: format!("Error: Option '{name}' requires an argument."),
                                exit: 2,
                            }
                        }
                    }
                }
                "--dispatch-verb" => take_value!(a.dispatch_verb),
                "--dispatch-brief" => take_value!(a.dispatch_brief),
                "--locked-by" => take_value!(a.locked_by),
                "--locked-by-harness" => take_value!(a.locked_by_harness),
                "--locked-by-harness-session" => take_value!(a.locked_by_harness_session),
                "--plan-path" => take_value!(a.plan_path),
                "--force" | "-F" => {
                    a.force = take_bool_value!(name, inline, i, true);
                }
                "--parent" => take_value!(a.parent),
                "--caused-by" => take_value!(a.caused_by),
                "--source-node" => take_value!(a.source_node),
                "--pr-number" => take_value!(a.pr_number),
                "--pr-url" => take_value!(a.pr_url),
                "--repo" => take_value!(a.repo),
                "--add-pr" => take_value!(a.add_pr),
                "--add-pr-url" => take_value!(a.add_pr_url),
                "--add-pr-note" => take_value!(a.add_pr_note),
                "--remove-pr" => take_value!(a.remove_pr),
                "--related" | "--blocked-by" | "--add-blocker" | "--remove-blocker" => {
                    match inline.clone().or_else(|| {
                        if i + 1 < tail.len() && !tail[i + 1].starts_with('-') {
                            i += 1;
                            Some(tail[i].clone())
                        } else {
                            None
                        }
                    }) {
                        Some(v) => {
                            if name == "--blocked-by" {
                                a.blocked_by.get_or_insert_with(Vec::new).push(v);
                            } else if name == "--related" {
                                a.related.push(v);
                            } else if name == "--add-blocker" {
                                a.add_blocker.push(v);
                            } else {
                                a.remove_blocker.push(v);
                            }
                            i += 1;
                        }
                        None => {
                            return ParsedUpdate::Refusal {
                                message: format!("Error: Option '{name}' requires an argument."),
                                exit: 2,
                            }
                        }
                    }
                }
                other => {
                    if let Some(op) = update_ops::parse_op_token(raw) {
                        match update_ops::check(&op) {
                            Ok(kind) => match lower_op(&mut a, op, kind) {
                                Ok(()) => i += 1,
                                Err(message) => {
                                    return ParsedUpdate::Refusal { message, exit: 2 };
                                }
                            },
                            Err(message) => {
                                return ParsedUpdate::Refusal { message, exit: 2 };
                            }
                        }
                    } else if !other.starts_with('-') && id.is_none() {
                        id = Some(other.to_string());
                        i += 1;
                    } else if other.starts_with('-') {
                        return ParsedUpdate::Refusal {
                            message: format!(
                                "Error: no such option: {other}. \
                                 `fno backlog update` carries one door per call: \
                                 --status, --leave, and repeatable --set field=value \
                                 (legacy one-flag-per-field spellings are retired)."
                            ),
                            exit: 2,
                        };
                    } else {
                        return ParsedUpdate::Refusal {
                            message: format!("Error: Got unexpected extra argument ({other})."),
                            exit: 2,
                        };
                    }
                }
            }
        }
        if a.help {
            return ParsedUpdate::Args(a);
        }
        match id {
            Some(task_id) => {
                a.task_id = task_id;
                ParsedUpdate::Args(a)
            }
            None => ParsedUpdate::Refusal {
                message: "Error: Missing argument 'TASK_ID'.".to_string(),
                exit: 2,
            },
        }
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

/// The boolean spellings click's BOOL type accepts on a `--flag=value`.
fn parse_click_bool(v: &str) -> Option<bool> {
    match v.to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Lower one validated operator onto its flag slot, in argument order, so
/// the writers and validators behind the legacy flags run unchanged:
/// `blocked_by+=x` lands in the same vec `--add-blocker` fills, `tags+=x`
/// in the same vec `--tag` fills. List replace tokens keep their commas; the
/// writer splits them with `parse_list`. A slot conflict (a second
/// `additional_prs` token, a bad boolean) refuses naming the token shape.
fn lower_op(
    a: &mut UpdateArgs,
    op: update_ops::FieldOp,
    _kind: update_ops::FieldKind,
) -> Result<(), String> {
    a.ops.push(op.clone());
    let value = op.value;
    match (op.field.as_str(), op.op) {
        ("details", update_ops::Op::Set) => a.details = Some(value),
        ("details", update_ops::Op::Add) => a.details_append.push(value),
        ("dispatch_brief", update_ops::Op::Set) => a.dispatch_brief = Some(value),
        ("dispatch_brief", update_ops::Op::Add) => a.brief_append.push(value),
        ("completion_note", update_ops::Op::Add) => a.completion_note = Some(value),
        ("completion_note", update_ops::Op::Set) => {
            if value.to_lowercase() == "null" {
                a.completion_note = Some(value);
            } else {
                a.completion_note_replace = Some(value);
            }
        }
        ("tags", update_ops::Op::Set) => {
            a.tags_replace = Some(parse_list(std::slice::from_ref(&value)))
        }
        ("tags", update_ops::Op::Add) => a.tag.push(value),
        ("tags", update_ops::Op::Remove) => a.untag.push(value),
        ("blocked_by", update_ops::Op::Set) => {
            a.blocked_by_replace_from_op = true;
            a.blocked_by.get_or_insert_with(Vec::new).push(value)
        }
        ("blocked_by", update_ops::Op::Add) => a.add_blocker.push(value),
        ("blocked_by", update_ops::Op::Remove) => a.remove_blocker.push(value),
        ("related", update_ops::Op::Set) => a.related.push(value),
        ("related", update_ops::Op::Add) => a.related_add.push(value),
        ("related", update_ops::Op::Remove) => a.related_remove.push(value),
        ("additional_prs", update_ops::Op::Add) => {
            if a.add_pr.is_some() {
                return Err("Error: additional_prs takes one += per call.".to_string());
            }
            if let Some(message) = check_pr_int("additional_prs+=", &value) {
                return Err(message);
            }
            a.add_pr = Some(value);
        }
        ("additional_prs", update_ops::Op::Remove) => {
            if a.remove_pr.is_some() {
                return Err("Error: additional_prs takes one -= per call.".to_string());
            }
            if let Some(message) = check_pr_int("additional_prs-=", &value) {
                return Err(message);
            }
            a.remove_pr = Some(value);
        }
        ("title", update_ops::Op::Set) => a.title = Some(value),
        ("domain", update_ops::Op::Set) => a.domain = Some(value),
        ("size", update_ops::Op::Set) => a.size = Some(value),
        ("model", update_ops::Op::Set) => a.model = Some(value),
        ("type", update_ops::Op::Set) => a.type_ = Some(value),
        ("batch", update_ops::Op::Set) => a.batch = Some(value),
        ("orphan_ok", update_ops::Op::Set) => a.orphan_ok = Some(value),
        ("priority", update_ops::Op::Set) => a.priority = Some(value),
        ("project", update_ops::Op::Set) => a.project = Some(value),
        ("cwd", update_ops::Op::Set) => a.cwd = Some(value),
        ("difficulty", update_ops::Op::Set) => a.difficulty = Some(value),
        ("dispatch_verb", update_ops::Op::Set) => a.dispatch_verb = Some(value),
        ("locked_by", update_ops::Op::Set) => a.locked_by = Some(value),
        ("plan_path", update_ops::Op::Set) => a.plan_path = Some(value),
        ("parent", update_ops::Op::Set) => a.parent = Some(value),
        ("caused_by", update_ops::Op::Set) => a.caused_by = Some(value),
        ("pr_number", update_ops::Op::Set) => a.pr_number = Some(value),
        ("pr_url", update_ops::Op::Set) => a.pr_url = Some(value),
        ("collisions_acknowledged", update_ops::Op::Set) => a.acknowledge_collisions = Some(value),
        ("source_node_id", update_ops::Op::Set) => a.source_node = Some(value),
        ("fixes_pr", update_ops::Op::Set) => {
            if let Some(message) = check_fixes_pr_int("fixes_pr=", &value) {
                return Err(message);
            }
            a.fixes_pr = Some(value);
        }
        ("public", update_ops::Op::Set) => match parse_click_bool(&value) {
            Some(b) => a.public = Some(b),
            None => {
                return Err(format!(
                    "Error: Invalid value for 'public=': '{value}' is not a valid boolean."
                ))
            }
        },
        ("reverted", update_ops::Op::Set) => match parse_click_bool(&value) {
            Some(b) => a.reverted = Some(b),
            None => {
                return Err(format!(
                    "Error: Invalid value for 'reverted=': '{value}' is not a valid boolean."
                ))
            }
        },
        ("has_brief", update_ops::Op::Set) => a.has_brief = Some(value),
        _ => {} // check() refused every field outside OP_FIELDS at parse
    }
    Ok(())
}

/// The integer gate `--fixes-pr` and `fixes_pr=` share; the label is the
/// spelling the refusal quotes.
fn check_fixes_pr_int(label: &str, v: &str) -> Option<String> {
    check_pr_int(label, v)
}

/// The integer gate the PR-number tokens share: the legacy flags carried
/// typer's INTEGER type, so an operator token refuses the same shape.
fn check_pr_int(label: &str, v: &str) -> Option<String> {
    if v.parse::<i64>().is_err() {
        Some(format!(
            "Error: Invalid value for '{label}': '{v}' is not a valid integer."
        ))
    } else {
        None
    }
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
    let args = match UpdateArgs::parse(tail) {
        ParsedUpdate::Args(a) => a,
        ParsedUpdate::Refusal { message, exit } => {
            eprintln!("{message}");
            return exit;
        }
    };
    if args.help {
        print!("{}", UPDATE_HELP);
        return 0;
    }
    // The tracker-owned refusal the python leg carried at its callback:
    // update owns graph state, so under any external tracker backend it
    // refuses before any read or write.
    let backend = crate::tracker::backend_name(None);
    if backend != "graph" {
        eprintln!(
            "fno backlog update: this verb owns graph state; under the \
             {backend} tracker backend it is refused. Track the item in the \
             tracker by its id."
        );
        return 1;
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
    if !args.ops.is_empty() {
        let tokens: Vec<String> = args.ops.iter().map(|op| op.token.clone()).collect();
        eprintln!(
            "Error: --status/--leave/--set cannot be combined with field operators \
             ({}) in one call. Run two calls.",
            tokens.join(", ")
        );
        return 2;
    }
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
    note("--locked-by", args.locked_by.is_some(), &mut legacy);
    note(
        "--locked-by-harness",
        args.locked_by_harness.is_some(),
        &mut legacy,
    );
    note(
        "--locked-by-harness-session",
        args.locked_by_harness_session.is_some(),
        &mut legacy,
    );
    note("--plan-path", args.plan_path.is_some(), &mut legacy);
    note("--force", args.force, &mut legacy);
    note("--parent", args.parent.is_some(), &mut legacy);
    note("--caused-by", args.caused_by.is_some(), &mut legacy);
    note("--source-node", args.source_node.is_some(), &mut legacy);
    note("--related", !args.related.is_empty(), &mut legacy);
    note("--blocked-by", args.blocked_by.is_some(), &mut legacy);
    note("--add-blocker", !args.add_blocker.is_empty(), &mut legacy);
    note(
        "--remove-blocker",
        !args.remove_blocker.is_empty(),
        &mut legacy,
    );
    note("--pr-number", args.pr_number.is_some(), &mut legacy);
    note("--pr-url", args.pr_url.is_some(), &mut legacy);
    note("--repo", args.repo.is_some(), &mut legacy);
    note("--add-pr", args.add_pr.is_some(), &mut legacy);
    note("--add-pr-url", args.add_pr_url.is_some(), &mut legacy);
    note("--add-pr-note", args.add_pr_note.is_some(), &mut legacy);
    note("--remove-pr", args.remove_pr.is_some(), &mut legacy);
    note("--difficulty", args.difficulty.is_some(), &mut legacy);
    note("--dispatch-verb", args.dispatch_verb.is_some(), &mut legacy);
    note(
        "--dispatch-brief",
        args.dispatch_brief.is_some(),
        &mut legacy,
    );
    note("--tag", !args.tag.is_empty(), &mut legacy);
    note("--untag", !args.untag.is_empty(), &mut legacy);
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
            "--model-tier was retired: the work-difficulty axis is difficulty=<band>, \
             an intrinsic property of the work \
             (`fno backlog update <id> difficulty=<band>`).",
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

    if args.blocked_by.is_some()
        && (!parse_list(&args.add_blocker).is_empty()
            || !parse_list(&args.remove_blocker).is_empty())
    {
        if args.blocked_by_replace_from_op {
            return Err(refused(
                "Error: blocked_by= replaces the list. It cannot be combined with \
                 blocked_by+= or blocked_by-= in one call.",
                2,
            ));
        }
        return Err(refused(
            "Error: --blocked-by is mutually exclusive with --add-blocker/--remove-blocker",
            2,
        ));
    }

    // U8: PR attribution resolves outside the graph lock; a wrong or
    // unattributable link refuses before any write.
    let graph = settings::graph_path();
    let pr = derive_pr_links(args, derived_cwd.as_deref(), &graph)?;
    let linked_size = linked_plan_size(args);
    write_update(
        &graph,
        args,
        details.as_deref(),
        details_from_file,
        derived_cwd.as_deref(),
        linked_size.as_deref(),
        &pr,
    )?;
    Ok(())
}

/// The PR links a wave-5 call resolves before the lock: the derived values
/// the mutator stores plus the clearing flag the receipts need. All empty
/// for a call without PR flags.
struct DerivedPr {
    pr_number: Option<i64>,
    pr_url: Option<String>,
    add_pr_url: Option<String>,
    clearing_number: bool,
}

fn is_digit_token(v: &str) -> bool {
    let t = v.trim();
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())
}

fn derive_pr_links(
    args: &UpdateArgs,
    derived_cwd: Option<&str>,
    graph: &Path,
) -> Result<DerivedPr, Refusal> {
    let pr_flags = args.pr_number.is_some()
        || args.pr_url.is_some()
        || args.repo.is_some()
        || args.add_pr.is_some()
        || args.add_pr_url.is_some()
        || args.add_pr_note.is_some()
        || args.remove_pr.is_some();
    if !pr_flags {
        return Ok(DerivedPr {
            pr_number: None,
            pr_url: None,
            add_pr_url: None,
            clearing_number: false,
        });
    }
    for (flag, raw) in [("--add-pr", &args.add_pr), ("--remove-pr", &args.remove_pr)] {
        if let Some(v) = raw {
            if v.parse::<i64>().is_err() {
                return Err(refused(
                    format!("Error: Invalid value for '{flag}': '{v}' is not a valid integer."),
                    2,
                ));
            }
        }
    }
    // The node as it stands now, for cwd and current-PR reads (the Python
    // `_node_before_update` cache, read eagerly here).
    let pre_node: Option<Value> = {
        let rows = graph_store::read_rows(graph)
            .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
        node_ref::find_node(&rows, &args.task_id).cloned()
    };
    let pre_field =
        |name: &str| -> Option<Value> { pre_node.as_ref().and_then(|n| n.get(name)).cloned() };
    let clearing_number = args
        .pr_number
        .as_deref()
        .is_some_and(|v| v.to_lowercase() == "null");
    let clearing_url = args
        .pr_url
        .as_deref()
        .is_some_and(|v| v.to_lowercase() == "null");

    if args.add_pr.is_none() && (args.add_pr_url.is_some() || args.add_pr_note.is_some()) {
        return Err(refused(
            "Error: --add-pr-url and --add-pr-note require --add-pr",
            2,
        ));
    }
    if args.repo.is_some() {
        if args.pr_url.is_some() && !clearing_url {
            return Err(refused(
                "Error: --repo and --pr-url both name a repo; pass one, not both",
                2,
            ));
        }
        if args.add_pr_url.is_some() {
            return Err(refused(
                "Error: --repo and --add-pr-url both name a repo; pass one, not both",
                2,
            ));
        }
    }
    if args.pr_url.is_some() && !clearing_url {
        let value = args.pr_url.as_deref().unwrap_or_default();
        let expect: Option<i64> = match args.pr_number.as_deref() {
            Some(pn) if is_digit_token(pn) => pn.trim().parse().ok(),
            Some(_) => None,
            None => pre_field("pr_number").as_ref().and_then(Value::as_i64),
        };
        check_url_shape(value, "--pr-url", expect)?;
    }
    if let Some(value) = args.add_pr_url.as_deref() {
        let expect = args.add_pr.as_deref().and_then(|v| v.parse().ok());
        check_url_shape(value, "--add-pr-url", expect)?;
    }

    // The cwd a derivation may fall back on: the project workmap expansion,
    // then the flag (abs-expanded), then the node's recorded cwd.
    let slug_cwd: Option<String> = derived_cwd
        .map(str::to_string)
        .or_else(|| args.cwd.as_deref().map(abs_expand))
        .or_else(|| {
            pre_field("cwd")
                .as_ref()
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let pre_pr_url = pre_field("pr_url")
        .as_ref()
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut derived_pr_url: Option<String> = None;
    if args.pr_number.is_some() && !clearing_number {
        let pn = args.pr_number.as_deref().unwrap_or_default();
        if !is_digit_token(pn) {
            return Err(refused(
                format!("Error: --pr-number '{pn}' is not a number (or 'null')"),
                2,
            ));
        }
        if clearing_url {
            return Err(refused(
                "Error: --pr-url null cannot accompany --pr-number: that writes a \
                 url-less pr_number. Clear both, or supply a url.",
                2,
            ));
        }
        if args.pr_url.is_none() {
            derived_pr_url = Some(resolve_or_refuse(
                pn.trim().parse().unwrap_or_default(),
                "--pr-url",
                true,
                args.repo.as_deref(),
                slug_cwd.as_deref(),
                pre_pr_url.as_deref(),
            )?);
        }
    } else if clearing_url && !clearing_number {
        if let Some(n) = pre_field("pr_number").as_ref().and_then(Value::as_i64) {
            return Err(refused(
                format!(
                    "Error: --pr-url null would leave this node's pr_number ({n}) \
                     unattributable. Pass --pr-number null too, or supply a replacement url."
                ),
                2,
            ));
        }
    }

    // A url-only update derives pr_number from the url: without it, reconcile
    // never sees the PR (node_pr_refs gates on the number) and the node stays
    // invisible to merge detection.
    let derived_pr_number: Option<i64> =
        if args.pr_url.is_some() && !clearing_url && args.pr_number.is_none() {
            super::pr_link::pr_number_from_url(args.pr_url.as_deref())
        } else {
            None
        };

    // additional_prs entries are read by the same repo-scoped matcher as the
    // primary field, so a bare --add-pr is unattributable for the same reason.
    let derived_add_pr_url: Option<String> = if args.add_pr.is_some() && args.add_pr_url.is_none() {
        Some(resolve_or_refuse(
            args.add_pr
                .as_deref()
                .and_then(|v| v.parse().ok())
                .unwrap_or_default(),
            "--add-pr-url",
            false,
            args.repo.as_deref(),
            slug_cwd.as_deref(),
            pre_pr_url.as_deref(),
        )?)
    } else {
        None
    };

    Ok(DerivedPr {
        pr_number: derived_pr_number,
        pr_url: derived_pr_url,
        add_pr_url: derived_add_pr_url,
        clearing_number,
    })
}

fn check_url_shape(value: &str, label: &str, expect: Option<i64>) -> Result<(), Refusal> {
    if super::pr_link::repo_slug_from_url(Some(value)).is_none() {
        return Err(refused(
            format!(
                "Error: {label} '{value}' is not a GitHub PR url \
                 (expected https://github.com/<owner>/<repo>/pull/<n>)"
            ),
            2,
        ));
    }
    let named = super::pr_link::pr_number_from_url(Some(value));
    if let Some(expect) = expect {
        if named != Some(expect) {
            let shown = named
                .map(|n| n.to_string())
                .unwrap_or_else(|| "None".into());
            return Err(refused(
                format!(
                    "Error: {label} names PR #{shown}, not #{expect} - a row pointing at \
                     two different PRs matches neither."
                ),
                2,
            ));
        }
    }
    Ok(())
}

fn resolve_or_refuse(
    number: i64,
    label: &str,
    primary: bool,
    repo: Option<&str>,
    slug_cwd: Option<&str>,
    pre_pr_url: Option<&str>,
) -> Result<String, Refusal> {
    // A named --repo is an assertion: build the url from it and say so. It
    // is the one legal way to override a recorded pr_url that names another
    // repo, so it applies before any derivation.
    if let Some(repo) = repo {
        if !super::pr_link::is_repo_slug(repo) {
            return Err(refused(
                format!(
                    "Error: --repo '{repo}' is not an owner/name slug \
                     (expected <owner>/<repo>, e.g. bllshttng/footnote)"
                ),
                2,
            ));
        }
        let url = super::pr_link::pr_url_from_slug(repo.trim(), number);
        eprintln!("note: stamped --repo {} {url}", repo.trim());
        return Ok(url);
    }
    let Some(url) = super::pr_link::pr_url_for_repo(number, slug_cwd) else {
        return Err(refused(
            format!(
                "Error: cannot resolve the repo for PR #{number} - refusing to stamp an \
                 unattributable pr_number. Fix with either `gh auth login` or \
                 `{label} https://github.com/<owner>/<repo>/pull/{number}`."
            ),
            2,
        ));
    };
    if primary {
        // A cwd-derived url is a guess. On the primary PR ref it must not
        // overwrite recorded truth that names a different repo: PR numbers
        // collide across repos. --add-pr is exempt: additional_prs are
        // cross-repo by design.
        let recorded = super::pr_link::repo_slug_from_url(pre_pr_url).map(|s| s.to_lowercase());
        let derived = super::pr_link::repo_slug_from_url(Some(&url)).map(|s| s.to_lowercase());
        if let (Some(recorded), Some(derived)) = (recorded, derived) {
            if recorded != derived {
                return Err(refused(
                    format!(
                        "Error: derived {label} {url} names repo {derived}, but this node's \
                         recorded pr_url names {recorded} - refusing to re-stamp a \
                         cross-repo move on a cwd derivation. Assert it with \
                         --repo {derived} (or a full {label} url), or clear the link \
                         with --pr-number null first."
                    ),
                    2,
                ));
            }
        }
    }
    eprintln!("note: derived {label} {url}");
    Ok(url)
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
    linked_size: Option<&str>,
    pr: &DerivedPr,
) -> Result<(), Refusal> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        // The version anchors BEFORE the row read: a commit that lands between
        // the two makes this attempt's base_version stale, so the store
        // refuses and the retry converges. The reverse order would publish
        // the stale snapshot over the concurrent commit as a no-conflict
        // write - a lost update.
        let base_version = graph_store::base_version(graph)
            .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
        let rows = graph_store::read_rows(graph)
            .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
        let planned = plan_mutation(
            graph,
            rows,
            args,
            details,
            details_from_file,
            derived_cwd,
            linked_size,
            pr,
        )?;
        match planned {
            MutationPlan::Applied {
                working,
                rungs,
                node_id,
                warnings,
                ship_stamp,
                repaint_old_parent,
            } => {
                match graph_store::locked_mutate(
                    graph,
                    MutateInput {
                        entries: working,
                        canonical_path: None,
                        base_version,
                        plan_rungs: Some(rungs),
                    },
                    LOCK_TIMEOUT,
                ) {
                    Ok(_) => {
                        // The dispatch-brief warning echoes once, after the
                        // write lands and before the receipt - the shape the
                        // Python caller ran.
                        for w in &warnings {
                            eprintln!("{w}");
                        }
                        let stored = confirm_readback(graph, args, &node_id)?;
                        if let Some(raw) = &args.locked_by {
                            verify_lock_stamp(graph, &node_id, raw)?;
                        }
                        // U8 receipts, in cmd_update's order.
                        if args.add_pr.is_some()
                            && stored.get("status").and_then(Value::as_str) == Some("ready")
                        {
                            eprintln!(
                                "warning: {} is still offered by ready; bind ownership and \
                                 the primary PR with --locked-by <worker> --pr-number {}",
                                stored.get("id").and_then(Value::as_str).unwrap_or(&node_id),
                                args.add_pr.as_deref().unwrap_or_default()
                            );
                        }
                        if args.pr_number.is_some() && !pr.clearing_number {
                            let stored_id =
                                stored.get("id").and_then(Value::as_str).unwrap_or(&node_id);
                            let stored_owner = stored
                                .get("locked_by")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .unwrap_or("unknown");
                            let stored_pr = match stored.get("pr_number").and_then(Value::as_i64) {
                                Some(n) => n.to_string(),
                                None => "None".to_string(),
                            };
                            let stored_status = stored
                                .get("status")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .unwrap_or("unknown");
                            let ready_effect = if stored_status == "ready" {
                                "still offered by ready"
                            } else {
                                "not offered by ready"
                            };
                            println!(
                                "ownership: node={stored_id} owner={stored_owner} \
                                 pr={stored_pr} status={stored_status}; {ready_effect}"
                            );
                        }
                        println!("Updated {node_id}");
                        // The link just committed (lock released), so the ship
                        // stamp runs here: the session row takes its own lock,
                        // and stamping inside the mutator would deadlock.
                        if let Some(target) = ship_stamp {
                            super::pr_link::stamp_ship_on_link(graph, &target);
                        }
                        repaint(graph, args, &node_id, repaint_old_parent.as_deref());
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
    Applied {
        working: Vec<Value>,
        rungs: BTreeMap<String, String>,
        node_id: String,
        warnings: Vec<String>,
        ship_stamp: Option<String>,
        repaint_old_parent: Option<String>,
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
    linked_size: Option<&str>,
    pr: &DerivedPr,
) -> Result<MutationPlan, Refusal> {
    let _ = graph;
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
    let (warnings, ship_stamp, repaint_old_parent) = apply_mutators(
        &mut rows,
        idx,
        args,
        details,
        details_from_file,
        derived_cwd,
        linked_size,
        pr,
    )?;

    // The status recompute runs over the POST-write rows: a plan this call
    // just bound must carry its rung, and the rungs map is rebuilt over the
    // mutated rows so the repaint sees the write's own plan binding.
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

    Ok(MutationPlan::Applied {
        working: rows,
        rungs,
        node_id,
        warnings,
        ship_stamp,
        repaint_old_parent,
    })
}

/// Whole-word check for a node id in a hold's free text, the way
/// `named_question_ids` matches `q-` ids: word boundaries, case-insensitive.
fn hold_text_names_id(haystack: &str, id: &str) -> bool {
    match regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(id))) {
        Ok(re) => re.is_match(haystack),
        Err(_) => false,
    }
}

/// Removing a blocker a held hold still names strands the hold: the worker
/// it parks keeps parking after the blocker row is gone, and the hold's
/// release condition names a blocker no longer on the row. Refuse and name
/// the release verb; lift the hold, then rerun the removal. An INVALID hold
/// (unreadable, not active) never refuses: there is nothing to strand.
fn hold_names_blockers(
    node: &Value,
    node_id: &str,
    removing: &[String],
    rows: &[Value],
) -> Result<(), Refusal> {
    if removing.is_empty() {
        return Ok(());
    }
    let by_id: BTreeMap<String, Value> = rows
        .iter()
        .filter_map(|e| entry_id(e).map(|id| (id.to_string(), e.clone())))
        .collect();
    let Some(receipt) = crate::backlog_ready::hold_verdict_receipt(node, &by_id) else {
        return Ok(());
    };
    if !receipt.held {
        return Ok(());
    }
    let text = format!("{}\n{}", receipt.reason, receipt.release_when);
    let named: Vec<String> = removing
        .iter()
        .filter(|b| hold_text_names_id(&text, b))
        .cloned()
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    Err(refused(
        format!(
            "Error: cannot remove blocker(s) {named} from {node_id}: an active \
             dispatch hold at {owner} names it: reason={reason} \
             release_when={release_when}; release the hold first with \
             `fno do pr hold release {owner} --evidence <proof>`, then rerun \
             the removal.",
            named = named.join(", "),
            node_id = node_id,
            owner = receipt.owner,
            reason = receipt.reason,
            release_when = receipt.release_when,
        ),
        3,
    ))
}

/// The wave-2/3/4 mutator arms, in cmd_update's field order. The whole rows
/// vector comes in because related's symmetry writes peer rows, caused-by
/// and parent resolve against the graph, and parent's contained-release
/// mutates the owner's subtree.
fn apply_mutators(
    rows: &mut Vec<Value>,
    idx: usize,
    args: &UpdateArgs,
    details: Option<&str>,
    details_from_file: bool,
    derived_cwd: Option<&str>,
    linked_size: Option<&str>,
    pr: &DerivedPr,
) -> Result<(Vec<String>, Option<String>, Option<String>), Refusal> {
    let node_id = rows[idx]
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let node = rows[idx].clone();
    // -- U6: topology arms, in cmd_update's order: related first (its
    // symmetric writes touch peer rows too), then source-node, then the
    // blocker ladder, then the lock stamps.
    let replace_tokens = parse_list(&args.related);
    // Keyed on the raw slot, not the parsed tokens: `--related ' '` parsed
    // to an empty list and cleared the field before the operators landed,
    // and that legacy shape stays byte-identical here. The stored-list arm
    // keys on the same raw slot, so an empty parse from a PRESENT replace
    // token still clears; only a genuinely absent replace reads the row.
    let has_related_edit =
        !args.related.is_empty() || !args.related_add.is_empty() || !args.related_remove.is_empty();
    if has_related_edit {
        let mut desired: Vec<String> = if replace_tokens.len() == 1 && replace_tokens[0] == "null" {
            Vec::new()
        } else if args.related.is_empty() {
            rows[idx]
                .get("related")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            replace_tokens
                .iter()
                .map(|t| {
                    node_ref::resolve_asserted_id(t, rows, "--related", Some(&node_id))
                        .map_err(|(m, e)| refused(m, e))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        for t in parse_list(&args.related_add) {
            let resolved = node_ref::resolve_asserted_id(&t, rows, "related", Some(&node_id))
                .map_err(|(m, e)| refused(m, e))?;
            if !desired.contains(&resolved) {
                desired.push(resolved);
            }
        }
        let removes: Vec<String> = parse_list(&args.related_remove)
            .iter()
            .map(|t| {
                node_ref::resolve_asserted_id(t, rows, "related", Some(&node_id))
                    .map_err(|(m, e)| refused(m, e))
            })
            .collect::<Result<Vec<_>, _>>()?;
        desired.retain(|r| !removes.contains(r));
        crate::graph_keeper::set_related(rows, &node_id, &desired)
            .map_err(|e| refused(format!("{e}"), 1))?;
    }
    if let Some(v) = &args.source_node {
        let resolved = if v == "null" {
            None
        } else {
            Some(
                node_ref::resolve_asserted_id(v, rows, "--source-node", Some(&node_id))
                    .map_err(|(m, e)| refused(m, e))?,
            )
        };
        rows[idx].as_object_mut().expect("row is an object").insert(
            "source_node_id".into(),
            resolved.map(|s| json!(s)).unwrap_or(Value::Null),
        );
    }
    let has_blocker_edit = args.blocked_by.is_some()
        || !args.add_blocker.is_empty()
        || !args.remove_blocker.is_empty();
    if has_blocker_edit {
        if let Some(replace) = &args.blocked_by {
            let desired = dedupe(parse_list(replace));
            node_ref::validate_blockers(&desired, rows, &args.task_id)
                .map_err(|(m, e)| refused(m, e))?;
            // A replace that drops blockers removes them: the same stranding
            // hazard --remove-blocker refuses, so the same guard runs here.
            let current: Vec<String> = rows[idx]
                .get("blocked_by")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let removed: Vec<String> = current
                .iter()
                .filter(|b| !desired.contains(b))
                .cloned()
                .collect();
            hold_names_blockers(&rows[idx], &node_id, &removed, rows)?;
            rows[idx]
                .as_object_mut()
                .expect("row is an object")
                .insert("blocked_by".into(), json!(desired));
        } else {
            let add = parse_list(&args.add_blocker);
            let remove = parse_list(&args.remove_blocker);
            hold_names_blockers(&rows[idx], &node_id, &remove, rows)?;
            node_ref::validate_blockers(&add, rows, &args.task_id)
                .map_err(|(m, e)| refused(m, e))?;
            let mut current: Vec<String> = rows[idx]
                .get("blocked_by")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            for b in add {
                if !current.contains(&b) {
                    current.push(b);
                }
            }
            current.retain(|b| !remove.contains(b));
            rows[idx]
                .as_object_mut()
                .expect("row is an object")
                .insert("blocked_by".into(), json!(current));
        }
    }
    if let Some(v) = &args.locked_by {
        let session = null_if(v);
        match session {
            Some(holder) => {
                // The claim store is the lock's single writer: `--locked-by`
                // acquires `node:<id>` for the holder and the projection
                // reads it back; the row carries no mirror copy. A foreign
                // live claim refuses naming the claim verb.
                let key = format!("node:{node_id}");
                let pid = crate::claims::durable_session_pid().map(|p| p as u32);
                let (pid_unavailable, ttl_ms) = match pid {
                    Some(_) => (false, None),
                    // No durable session to anchor: a 2h lease keeps the
                    // stamp readable, the same shape the pid-less task
                    // claim rides.
                    None => (true, Some(7_200_000)),
                };
                match crate::claims::acquire(
                    &key,
                    &holder,
                    crate::claims::AcquireOpts {
                        pid,
                        pid_unavailable,
                        ttl_ms,
                        reason: Some("locked-by stamp".into()),
                        metadata: None,
                        pid_provenance: None,
                        host: None,
                        root: None,
                        events_dir: None,
                        identity: None,
                    },
                ) {
                    crate::claims::AcquireOutcome::Acquired(_) => {}
                    crate::claims::AcquireOutcome::HeldByOther { holder: h, .. } => {
                        return Err(refused(
                            format!(
                                "error: {node_id} is held by '{h}'. To hold it: \
                                 fno agents claim acquire node:{node_id} --holder {holder} \
                                 (after the other holder releases), or clear that claim."
                            ),
                            3,
                        ));
                    }
                    crate::claims::AcquireOutcome::Error(e) => {
                        return Err(refused(e, 2));
                    }
                }
            }
            None => {
                // Clearing drops the row mirror and releases the invoking
                // holder's own claim. A foreign live holder stays put and the
                // refusal names the claim verb (the unclaim/requeue
                // contract); an override there is `claim release --force`.
                let key = format!("node:{node_id}");
                let (state, record) = crate::claims::status(&key, None);
                if let Some(record) = record {
                    let held = matches!(
                        state,
                        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
                    );
                    let self_holder = crate::claims::resolve_identity().0.unwrap_or_default();
                    if held && record.holder != self_holder {
                        return Err(refused(
                            format!(
                                "error: {node_id} is held by live claim holder '{}'. Release it \
                                 first: fno agents claim release node:{node_id} --holder {}",
                                record.holder, record.holder
                            ),
                            3,
                        ));
                    }
                    crate::claims::release(&key, &record.holder, None, None)
                        .map_err(|e| refused(format!("error: claim release failed: {e}"), 2))?;
                }
                let obj = rows[idx].as_object_mut().expect("row is an object");
                obj.insert("locked_by".into(), Value::Null);
                obj.insert("locked_at".into(), Value::Null);
                obj.insert("locked_by_harness".into(), Value::Null);
                obj.insert("locked_by_harness_session".into(), Value::Null);
            }
        }
    }
    // The claim store owns the harness stamp; a direct mirror write is the
    // retired second answerer (the merge that resurrected this write is the
    // same shape the session-add deletion fixed). Refuse, never drop.
    if args.locked_by_harness.is_some() || args.locked_by_harness_session.is_some() {
        return Err(refused(
            "error: --locked-by-harness/--locked-by-harness-session are retired: the claim \
             store owns the harness stamp. Re-acquire with \
             `fno agents claim acquire node:<id> --holder <id>`, which captures it.",
            3,
        ));
    }
    if let Some(v) = &args.has_brief {
        let obj = rows[idx].as_object_mut().expect("row is an object");
        obj.insert("has_brief".into(), Value::Bool(v.to_lowercase() == "true"));
    }
    // -- U7: the plan binding, directly after has_brief in cmd_update's
    // order. 'null' clears; the one-plan-one-node conflict refuses naming
    // the owner, and --force binds anyway with the named note.
    let mut warnings: Vec<String> = Vec::new();
    if let Some(v) = &args.plan_path {
        if v.to_lowercase() == "null" {
            rows[idx]
                .as_object_mut()
                .expect("row is an object")
                .insert("plan_path".into(), Value::Null);
        } else {
            let owner = crate::graph_store::plan_path_owner_conflict(rows, Some(&node_id), Some(v));
            if let Some(owner) = owner {
                if !args.force {
                    return Err(refused(
                        format!(
                            "error: plan {v} is already the delivery unit of {owner}\n\
                             \x20 a plan is one PR is one node; binding it to {node_id} would arm both\n\
                             \x20 to record that {node_id} ships inside that PR: \
                             fno backlog decompose ... \"adopt\": [\"{node_id}\"]\n\
                             \x20 to repoint deliberately: --force"
                        ),
                        2,
                    ));
                }
                // Rides the warnings vec, not a print: plan_mutation re-runs
                // per write attempt, and a print here would repeat on every
                // contention retry - the shape the Python caller avoided.
                warnings.push(format!(
                    "note: plan {v} is also held by {owner}; binding {node_id} anyway \
                     (--force). Both will dispatch and cost independently."
                ));
            }
            {
                let obj = rows[idx].as_object_mut().expect("row is an object");
                obj.insert("plan_path".into(), json!(v));
                if linked_size.is_some()
                    && !obj
                        .get("size")
                        .is_some_and(|s| s.is_string() && !s.as_str().unwrap().is_empty())
                {
                    obj.insert("size".into(), json!(linked_size.unwrap()));
                }
            }
        }
    }
    // -- U8: the PR links, directly after the plan binding in cmd_update's
    // order. The unset->set transition on pr_number is the ship choke point;
    // the stamp itself runs after the lock releases (write_update), since the
    // session row takes its own lock.
    let pr_number_before = node.get("pr_number").and_then(Value::as_i64);
    {
        let obj = rows[idx].as_object_mut().expect("row is an object");
        if let Some(v) = &args.pr_number {
            let value = if v.to_lowercase() == "null" {
                Value::Null
            } else {
                json!(v.trim().parse::<i64>().unwrap_or_default())
            };
            obj.insert("pr_number".into(), value);
        } else if let Some(n) = pr.pr_number {
            obj.insert("pr_number".into(), json!(n));
        }
    }
    let pr_number_now = rows[idx].get("pr_number").and_then(Value::as_i64);
    let ship_stamp =
        (pr_number_now.is_some() && pr_number_before.is_none()).then(|| node_id.clone());
    {
        let obj = rows[idx].as_object_mut().expect("row is an object");
        if let Some(v) = &args.pr_url {
            let value = if v.to_lowercase() == "null" {
                Value::Null
            } else {
                json!(v)
            };
            obj.insert("pr_url".into(), value);
        } else if let Some(u) = &pr.pr_url {
            obj.insert("pr_url".into(), json!(u));
        }
    }
    let warnings: Vec<String> = {
        let obj = rows[idx].as_object_mut().expect("row is an object");
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
        // `dispatch_brief+=`: merge the appends into the replace value (if
        // any) or the stored brief, so the override write below stores one
        // merged text and its 8 KB warning measures the merged bytes.
        let mut brief = args.dispatch_brief.clone();
        for add in &args.brief_append {
            let base = brief.take().or_else(|| {
                obj.get("dispatch_brief")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            brief = update_ops::append_text(base.as_deref(), add).or(base);
        }
        let dispatch_warnings: Vec<String> = super::fields::apply_dispatch_overrides(
            obj,
            args.dispatch_verb.as_deref(),
            brief.as_deref(),
        )
        .map_err(|refusal| refused(refusal, 2))?;
        warnings.extend(dispatch_warnings);
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
        for add in &args.details_append {
            // Reads the CURRENT row, so a replace in the same call joins the
            // replaced text and plan_mutation re-runs per attempt on retry.
            let merged = update_ops::append_text(obj.get("details").and_then(Value::as_str), add);
            if let Some(text) = merged {
                obj.insert("details".into(), json!(text));
            }
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
        if let Some(raw) = &args.difficulty {
            let band = if raw.to_lowercase() == "null" {
                None
            } else {
                match super::fields::normalize_difficulty(raw) {
                    Ok(band) => Some(band),
                    Err(exc) => {
                        return Err(refused(format!("fno backlog update: {exc}"), 2));
                    }
                }
            };
            super::fields::write_canonical_difficulty(
                obj,
                band.as_deref(),
                "update",
                &crate::graph_store::now_isoformat(),
                "change",
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
        if let Some(text) = &args.completion_note_replace {
            // Whitespace-only replace is a no-op, the same no-op the append
            // spelling keeps (help: "Whitespace-only is a no-op").
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                obj.insert("completion_note".into(), json!(trimmed));
            }
        }
        if let Some(note) = &args.completion_note {
            if note.to_lowercase() == "null" {
                obj.insert("completion_note".into(), Value::Null);
            } else {
                let trimmed = note.trim();
                if !trimmed.is_empty() {
                    // Reads the CURRENT row, so a same-call replace joins the
                    // replaced text (the details append reads obj for the
                    // same reason).
                    let existing = obj
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
        if let Some(v) = &args.add_pr {
            let num: i64 = v.trim().parse().unwrap_or_default();
            let mut list: Vec<Value> = obj
                .get("additional_prs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("number".into(), json!(num));
            entry.insert(
                "url".into(),
                args.add_pr_url
                    .as_deref()
                    .or(pr.add_pr_url.as_deref())
                    .map(|s| json!(s))
                    .unwrap_or(Value::Null),
            );
            if let Some(note) = &args.add_pr_note {
                entry.insert("note".into(), json!(note));
            }
            let mut replaced = false;
            for item in list.iter_mut() {
                let Some(map) = item.as_object_mut() else {
                    continue;
                };
                if map.get("number").and_then(Value::as_i64) == Some(num) {
                    for (k, val) in entry.clone() {
                        map.insert(k, val);
                    }
                    replaced = true;
                    break;
                }
            }
            if !replaced {
                list.push(Value::Object(entry));
            }
            obj.insert("additional_prs".into(), Value::Array(list));
        }
        if let Some(v) = &args.remove_pr {
            let num: i64 = v.trim().parse().unwrap_or_default();
            let list: Vec<Value> = obj
                .get("additional_prs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|item| item.get("number").and_then(Value::as_i64) != Some(num))
                .collect();
            obj.insert("additional_prs".into(), Value::Array(list));
        }
        warnings
    };
    // -- caused-by: the resolved id is stored, never the raw token.
    if let Some(v) = &args.caused_by {
        if v.to_lowercase() == "null" {
            rows[idx]
                .as_object_mut()
                .expect("row is an object")
                .insert("caused_by".into(), Value::Null);
        } else {
            let origin = node_ref::find_node(rows, v)
                .ok_or_else(|| refused(format!("Error: --caused-by node {v} not found"), 1))?;
            let origin_id = origin
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if origin_id == node_id {
                return Err(refused(
                    "Error: --caused-by cannot reference the node itself",
                    1,
                ));
            }
            rows[idx]
                .as_object_mut()
                .expect("row is an object")
                .insert("caused_by".into(), json!(origin_id));
        }
    }
    {
        let obj = rows[idx].as_object_mut().expect("row is an object");
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
        if !args.tag.is_empty() || !args.untag.is_empty() || args.tags_replace.is_some() {
            // Idempotent set semantics, order-preserving: adds skip dupes,
            // removes are no-ops if absent. Normalization refuses before the
            // write lands, so a malformed tag never mutates. `tags=` starts
            // `current` from the normalized replace list in place of the
            // stored list; add and remove then apply as today.
            let mut normalized_tag = Vec::new();
            for t in &args.tag {
                normalized_tag.push(
                    super::fields::normalize_tag(t)
                        .map_err(|e| refused(format!("Error: {e}"), 1))?,
                );
            }
            let mut normalized_untag = Vec::new();
            for t in &args.untag {
                normalized_untag.push(
                    super::fields::normalize_tag(t)
                        .map_err(|e| refused(format!("Error: {e}"), 1))?,
                );
            }
            let mut current: Vec<String> = match &args.tags_replace {
                // The help's "('null' clears)" holds on = for every field it
                // can store: a lone null replace token empties the list, the
                // same shape --related null clears.
                Some(replace) if replace.len() == 1 && replace[0] == "null" => Vec::new(),
                Some(replace) => {
                    let mut replaced = Vec::new();
                    for t in replace {
                        replaced.push(
                            super::fields::normalize_tag(t)
                                .map_err(|e| refused(format!("Error: {e}"), 1))?,
                        );
                    }
                    replaced
                }
                None => obj
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            for t in normalized_tag {
                if !current.contains(&t) {
                    current.push(t);
                }
            }
            current.retain(|t| !normalized_untag.contains(t));
            obj.insert("tags".into(), json!(current));
        }
    }
    // -- parent: last in cmd_update's order. A move that leaves the
    // containing subtree releases the containment (dropping the owner's
    // inherited PR refs); cycle and the epic-depth cap refuse.
    let mut new_parent_id: Option<String> = None;
    if let Some(v) = &args.parent {
        let new_parent = null_if(v);
        let owner = node
            .get("contained_in")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(owner) = owner {
            let mut cursor: Option<String> = match &new_parent {
                Some(p) => node_ref::find_node(rows, p)
                    .and_then(|e| e.get("id").and_then(Value::as_str))
                    .map(str::to_string),
                None => None,
            };
            let mut seen: std::collections::HashSet<String> = Default::default();
            let mut still_contained = false;
            while let Some(cur) = cursor {
                if cur == owner || seen.contains(&cur) || seen.len() >= 64 {
                    if cur == owner {
                        still_contained = true;
                    }
                    break;
                }
                seen.insert(cur.clone());
                cursor = node_ref::find_node(rows, &cur)
                    .and_then(|e| e.get("parent").and_then(Value::as_str))
                    .map(str::to_string);
            }
            if !still_contained {
                node_ref::release_contained(rows, &node_id)
                    .map_err(|e| refused(format!("{e}"), 1))?;
            }
        }
        match new_parent {
            None => {
                rows[idx]
                    .as_object_mut()
                    .expect("row is an object")
                    .insert("parent".into(), Value::Null);
            }
            Some(p) => {
                let target = node_ref::find_node(rows, &p)
                    .ok_or_else(|| refused(format!("Error: parent node {p} not found"), 1))?;
                let target_id = target
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                new_parent_id = Some(target_id.clone());
                if node_ref::would_create_cycle(rows, &node_id, &target_id) {
                    return Err(refused(
                        format!(
                            "Error: setting parent of {node_id} to {target_id} \
                             would create a cycle"
                        ),
                        1,
                    ));
                }
                let node_row = rows[idx].clone();
                if node_ref::would_exceed_epic_depth(rows, &node_row, target) {
                    return Err(refused(
                        format!(
                            "Error: parenting epic {node_id} under {target_id} \
                             would exceed the 2-level cap \
                             (mission -> epic -> leaf); an epic may nest only \
                             under a top-level mission"
                        ),
                        1,
                    ));
                }
                rows[idx]
                    .as_object_mut()
                    .expect("row is an object")
                    .insert("parent".into(), json!(target_id));
            }
        }
    }
    // The depth cap must also hold when a --type change alone promotes a
    // node to epic under an already-nested epic (the --parent guard above
    // never fires without --parent). Checked against the FINAL parent edge.
    if args.type_.as_deref() == Some("epic") {
        let node_row = rows[idx].clone();
        let parent_id = node_row
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(parent_id) = parent_id {
            if let Some(parent_row) = node_ref::find_node(rows, &parent_id) {
                let parent_row = parent_row.clone();
                if node_ref::would_exceed_epic_depth(rows, &node_row, &parent_row) {
                    return Err(refused(
                        format!(
                            "Error: making {node_id} an epic under {parent_id} \
                             would exceed the 2-level cap \
                             (mission -> epic -> leaf); an epic may nest only \
                             under a top-level mission"
                        ),
                        1,
                    ));
                }
            }
        }
    }
    // A reparent leaves the OLD parent's stale rollup on its doc: the
    // python verb repainted it alongside the node, so name it for the
    // projector's target list.
    let old_parent = node
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_string);
    let repaint_old_parent =
        old_parent.filter(|old| Some(old.as_str()) != new_parent_id.as_deref());
    Ok((warnings, ship_stamp, repaint_old_parent))
}

/// The `_parse_blocker_list` twin: comma-split, trim, skip empties.
fn parse_list(values: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for v in values {
        for token in v.split(',') {
            let t = token.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// `dict.fromkeys` order-preserving dedupe.
fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in values {
        if seen.insert(v.clone()) {
            out.push(v);
        }
    }
    out
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
fn confirm_readback(graph: &Path, args: &UpdateArgs, node_id: &str) -> Result<Value, Refusal> {
    let _ = args;
    let rows = crate::graph_store::read_rows_where(
        graph,
        &crate::backlog::RowQuery {
            filter: crate::backlog::api::NodeFilter {
                id_in: Some(vec![node_id.to_string()]),
                ..Default::default()
            },
            with_blockers: true,
            ..Default::default()
        },
    )
    .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
    // LIVE rows only: the archived case lands its write and then fails
    // exactly here, the captured contract.
    let live: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("archived_at").is_none())
        .collect();
    match live.iter().find(|r| entry_id(r) == Some(node_id)) {
        Some(row) => Ok((*row).clone()),
        None => Err(refused(
            format!(
                "Error: the update of {node_id} reported success but the row does not \
                 read back; the write did not land"
            ),
            1,
        )),
    }
}

/// The plan-frontmatter size read that flows doc->graph on a (re)link, for a
/// node with no size yet. Best-effort: an unreadable plan contributes nothing.
fn linked_plan_size(args: &UpdateArgs) -> Option<String> {
    let raw = args.plan_path.as_deref()?;
    if raw.to_lowercase() == "null" {
        return None;
    }
    let mut pp = PathBuf::from(raw);
    if !pp.is_absolute() {
        let root = std::env::current_dir().ok()?;
        pp = root.join(pp);
    }
    let (_, fields, _) = crate::plan_doc::codec::read_plan_file(&pp).ok()?;
    let size = match fields.get("size")? {
        crate::plan_doc::codec::Value::Scalar(s) => s.trim().to_uppercase(),
        _ => return None,
    };
    if ["S", "M", "L"].contains(&size.as_str()) {
        Some(size)
    } else {
        None
    }
}

/// The post-commit read-back for `--locked-by`: the Updated receipt answers
/// "was the command accepted", never "is the value there". The claim store
/// is the truth the receipt checks: a stamp is accepted when a claim for the
/// node names that holder and reads live or suspect; a release refuses when
/// it leaves the node wedged in_progress.
fn verify_lock_stamp(graph: &Path, node_id: &str, locked_by: &str) -> Result<(), Refusal> {
    let rows = crate::graph_store::read_rows_where(
        graph,
        &crate::backlog::RowQuery {
            filter: crate::backlog::api::NodeFilter {
                id_in: Some(vec![node_id.to_string()]),
                ..Default::default()
            },
            with_blockers: true,
            ..Default::default()
        },
    )
    .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
    let stored: Option<&Value> = rows
        .iter()
        .filter(|r| r.get("archived_at").is_none())
        .find(|r| entry_id(r) == Some(node_id));
    let expected = null_if(locked_by);
    if let Some(holder) = expected {
        let (state, record) = crate::claims::status(&format!("node:{node_id}"), None);
        let holder_matches = record.as_ref().is_some_and(|r| r.holder == holder)
            && matches!(
                state,
                crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
            );
        if !holder_matches {
            return Err(refused(
                format!(
                    "error: {node_id} read back no live claim for '{holder}': the stamp \
                     did not hold. Hold it with: fno agents claim acquire \
                     node:{node_id} --holder {holder}"
                ),
                1,
            ));
        }
        return Ok(());
    }
    // A release: the earned-success rule. A lock clear that left the node
    // in_progress on its own open do rows did not return it to the queue.
    let row = stored.unwrap_or(&Value::Null);
    let stored_status = row
        .get("persisted_status")
        .and_then(Value::as_str)
        .or_else(|| row.get("status").and_then(Value::as_str));
    if stored_status == Some("in_progress") {
        let open_do = row
            .get("sessions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter(|r| graph_store::is_open_do_row(r)).count())
            .unwrap_or(0);
        let plural = if open_do != 1 { "s" } else { "" };
        return Err(refused(
            format!(
                "update: {node_id} still reads in_progress after clearing the \
                 claim ({open_do} open do row{plural}). The claim was not what \
                 held it. Use: fno backlog requeue {node_id}"
            ),
            3,
        ));
    }
    Ok(())
}

/// The plan repaint: graph-authoritative fields flow onto the linked plan
/// when a mirrored or status-affecting field changed - ownership, blockers,
/// plan binding, parentage and tags included, the trigger set the python
/// verb ran. Best-effort.
fn repaint(graph: &Path, args: &UpdateArgs, node_id: &str, old_parent: Option<&str>) {
    let difficulty_clear = args
        .difficulty
        .as_deref()
        .is_some_and(|d| d.to_lowercase() == "null");
    let has_blocker_edit = args.blocked_by.is_some()
        || !args.add_blocker.is_empty()
        || !args.remove_blocker.is_empty();
    let has_tag_edit =
        !args.tag.is_empty() || !args.untag.is_empty() || args.tags_replace.is_some();
    let mirror_triggers = args.locked_by.is_some()
        || args.priority.is_some()
        || args.project.is_some()
        || args.type_.is_some()
        || args.difficulty.is_some()
        || has_blocker_edit
        || args.plan_path.is_some()
        || args.size.is_some()
        || args.parent.is_some()
        || has_tag_edit;
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
    if args.difficulty.is_some() {
        mirror_keys.push("difficulty".into());
    }
    // A reparent repaints the old parent's rollup alongside the node: the
    // converger walks each id's ancestors in the post-mutation graph, so the
    // old chain is only reachable through this explicit target.
    let mut targets: Vec<String> = Vec::new();
    targets.push(node_id.to_string());
    if let Some(old) = old_parent {
        targets.push(old.to_string());
    }
    crate::plan_doc::project::project_graph_nodes(
        &rows,
        &targets,
        None,
        Some((node_id.to_string(), mirror_keys)),
        None,
        // An explicit `--difficulty null` is the ONE clear the projector
        // honors for that key.
        difficulty_clear.then(|| (node_id.to_string(), vec!["difficulty".to_string()])),
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
        let a = match UpdateArgs::parse(&tail) {
            ParsedUpdate::Args(a) => a,
            ParsedUpdate::Refusal { .. } => panic!("parses"),
        };
        assert_eq!(a.task_id, "x-aaaa1111");
        assert_eq!(a.title.as_deref(), Some("T"));
        assert_eq!(a.public, Some(true));
        assert_eq!(a.door, vec!["--set=size=L".to_string()]);
    }

    #[test]
    fn an_unknown_flag_refuses_with_the_retired_message() {
        let retired: Vec<String> = ["x-aaaa1111", "--completed", "yes"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let message = match UpdateArgs::parse(&retired) {
            ParsedUpdate::Args(_) => panic!("retired flag must refuse"),
            ParsedUpdate::Refusal { message, exit } => {
                assert_eq!(exit, 2);
                message
            }
        };
        assert_eq!(
            message,
            "Error: no such option: --completed. \
             `fno backlog update` carries one door per call: --status, --leave, \
             and repeatable --set field=value (legacy one-flag-per-field \
             spellings are retired)."
        );
    }

    #[test]
    fn usage_refusals_keep_the_typer_shapes() {
        let missing: Vec<String> = ["--title", "T"].iter().map(|s| s.to_string()).collect();
        match UpdateArgs::parse(&missing) {
            ParsedUpdate::Args(_) => panic!("missing task id must refuse"),
            ParsedUpdate::Refusal { message, exit } => {
                assert_eq!(message, "Error: Missing argument 'TASK_ID'.");
                assert_eq!(exit, 2);
            }
        }
        let no_value: Vec<String> = ["x-aaaa1111", "--title"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match UpdateArgs::parse(&no_value) {
            ParsedUpdate::Args(_) => panic!("valueless flag must refuse"),
            ParsedUpdate::Refusal { message, exit } => {
                assert_eq!(message, "Error: Option '--title' requires an argument.");
                assert_eq!(exit, 2);
            }
        }
        let bad_int: Vec<String> = ["x-aaaa1111", "--fixes-pr", "soon"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match UpdateArgs::parse(&bad_int) {
            ParsedUpdate::Args(_) => panic!("non-int fixes-pr must refuse"),
            ParsedUpdate::Refusal { message, exit } => {
                assert_eq!(
                    message,
                    "Error: Invalid value for '--fixes-pr': 'soon' is not a valid integer."
                );
                assert_eq!(exit, 2);
            }
        }
        let unknown_field: Vec<String> = ["x-aaaa1111", "nope=1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match UpdateArgs::parse(&unknown_field) {
            ParsedUpdate::Args(_) => panic!("unknown operator field must refuse"),
            ParsedUpdate::Refusal { message, exit } => {
                assert_eq!(exit, 2);
                assert!(
                    message.starts_with("Error: unknown field 'nope'."),
                    "{message}"
                );
                assert!(
                    message.contains("details, dispatch_brief, tags, blocked_by, related"),
                    "{message}"
                );
            }
        }
        let text_remove: Vec<String> = ["x-aaaa1111", "details-=x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match UpdateArgs::parse(&text_remove) {
            ParsedUpdate::Args(_) => panic!("a remove on text must refuse"),
            ParsedUpdate::Refusal { message, exit } => {
                assert_eq!(exit, 2);
                assert_eq!(message, "Error: details is text. It takes = and += only.");
            }
        }
    }

    #[test]
    fn operator_tokens_parse_and_lower_onto_slots() {
        let argv: Vec<String> = [
            "x-aaaa1111",
            "details+=first",
            "tags=Big-Idea,other",
            "blocked_by+=x-bbbb2222",
            "related-=beta",
            "details=a=b",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        match UpdateArgs::parse(&argv) {
            ParsedUpdate::Refusal { message, .. } => panic!("operators must parse: {message}"),
            ParsedUpdate::Args(a) => {
                assert_eq!(a.task_id, "x-aaaa1111");
                assert_eq!(a.details_append, ["first"]);
                assert_eq!(a.details, Some("a=b".to_string()));
                assert_eq!(
                    a.tags_replace,
                    Some(vec!["Big-Idea".to_string(), "other".to_string()])
                );
                assert_eq!(a.add_blocker, ["x-bbbb2222"]);
                assert_eq!(a.related_remove, ["beta"]);
                assert_eq!(a.ops.len(), 5);
            }
        }
        // An operator before the id parses the same way: the id is never
        // read as an operator because a node id never contains `=`.
        let id_last: Vec<String> = ["tags+=t", "x-aaaa1111"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match UpdateArgs::parse(&id_last) {
            ParsedUpdate::Refusal { message, .. } => panic!("id after operator parses: {message}"),
            ParsedUpdate::Args(a) => {
                assert_eq!(a.task_id, "x-aaaa1111");
                assert_eq!(a.tag, ["t"]);
            }
        }
    }

    #[test]
    fn append_text_joins_skips_the_tail_noop_and_starts_empty() {
        assert_eq!(
            update_ops::append_text(Some("A"), "B"),
            Some("A\n\nB".to_string())
        );
        assert_eq!(update_ops::append_text(Some("A\n\nB"), "B"), None);
        assert_eq!(
            update_ops::append_text(Some("A  "), "B"),
            Some("A\n\nB".to_string())
        );
        assert_eq!(update_ops::append_text(None, "B"), Some("B".to_string()));
        assert_eq!(
            update_ops::append_text(Some(""), "B"),
            Some("B".to_string())
        );
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
    fn removing_a_blocker_a_held_hold_names_refuses() {
        // The held plan names the blocker in its reason; the removal refuses and
        // names the release verb. An INVALID hold never
        // refuses, and a removal the hold does not name passes.
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(
            &plan,
            "---\nstatus: ready\ndispatch_hold:\n  reason: waiting on t-fcb4\n  release_when: t-fcb4 clears\n  review_on: 2099-08-20\n  set_by: lead\n---\n",
        )
        .unwrap();
        let held = serde_json::json!({
            "id": "t-bf51", "slug": "bf51", "title": "bf51",
            "blocked_by": ["t-fcb4"],
            "plan_path": plan.display().to_string(),
            "cwd": dir.path().display().to_string(),
        });
        let blocker = serde_json::json!({
            "id": "t-fcb4", "slug": "fcb4", "title": "fcb4",
            "cwd": dir.path().display().to_string(),
        });
        let rows = vec![held, blocker];

        let err = hold_names_blockers(&rows[0], "t-bf51", &["t-fcb4".to_string()], &rows)
            .expect_err("a held hold naming the blocker refuses");
        match err {
            Refusal { message, exit } => {
                assert_eq!(exit, 3);
                assert!(message.contains("t-fcb4"), "{message}");
                assert!(message.contains("hold release"), "{message}");
            }
        }

        // A removal the hold does not name passes.
        assert!(
            hold_names_blockers(&rows[0], "t-bf51", &["t-other".to_string()], &rows).is_ok(),
            "an unnamed removal passes"
        );

        // An INVALID hold (a malformed block, not an active one) never
        // refuses: there is nothing to strand.
        let broken = dir.path().join("broken.md");
        std::fs::write(
            &broken,
            "---\nstatus: ready\ndispatch_hold:\n  reason: waiting on t-fcb4\n---\n",
        )
        .unwrap();
        let broken_row = serde_json::json!({
            "id": "t-bf51", "slug": "bf51", "title": "bf51",
            "blocked_by": ["t-fcb4"],
            "plan_path": broken.display().to_string(),
            "cwd": dir.path().display().to_string(),
        });
        let broken_rows = vec![broken_row, rows[1].clone()];
        assert!(
            hold_names_blockers(
                &broken_rows[0],
                "t-bf51",
                &["t-fcb4".to_string()],
                &broken_rows,
            )
            .is_ok(),
            "an INVALID hold never refuses"
        );
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
