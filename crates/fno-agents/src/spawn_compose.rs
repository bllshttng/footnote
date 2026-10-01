//! The spawn-defaults composition, ported from Python
//! `fno.agents.spawn_defaults.inject_spawn_defaults` (766 lines). Rust owns
//! the rung order, the argv assembly and the refusal texts; the Python seam
//! keeps the call site and becomes a transport
//! (`compose_spawn_argv`). Split in two: [`gather`] reads the world into
//! [`Inputs`]; [`compose`] is pure, so the characterization goldens
//! (`tests/fixtures/spawn_compose/`) feed it parsed TOML and never touch the
//! environment. Reached as payload kind `compose` on the spawn-overlay verb.

use serde_json::{json, Map, Value};
use std::path::Path;

const CROWN_VERBS: [&str; 3] = ["lead", "reign", "fno-me"];
/// Keep the old spelling on the canonical profile key for one release.
const VERB_ALIASES: [(&str, &str); 1] = [("do", "execute")];
/// A lane is a COMPLETE coordinate: route/model stop at the lane.
const LANE_EXCLUSIVE: [&str; 2] = ["route", "model"];
const SUBSTRATES: [&str; 4] = ["pane", "thread", "headless", "bg"];
/// The one built-in answer for an unattended worker's permission mode.
const SPAWN_PERMISSION_BUILTIN: &str = "bypassPermissions";

/// The pure compose's world: everything `gather` read, so the goldens can
/// hand it over pre-parsed.
pub struct Inputs {
    pub argv: Vec<String>,
    /// `agents.defaults` (empty table when unset).
    pub defaults: toml::Value,
    /// `agents.profiles` (empty table when unset).
    pub profiles: toml::Value,
    /// `dispatch.verbs` registry (empty table when unset).
    pub dispatch_verbs: toml::Value,
    /// The shipped footnote verb roster.
    pub roster: Vec<String>,
    pub node_verb: Option<String>,
    pub env_node: Option<String>,
    /// The resolved ambient harness (Python's `resolve_dispatch_harness`
    /// builtin default is claude).
    pub ambient_harness: String,
    pub apply_permission_builtin: bool,
    pub scan: Value,
    pub facts: Value,
    /// The resolved node id (flag/env/seed) and its graph row.
    pub node: Option<String>,
    pub node_row: Option<Value>,
}

/// Production read side: config subtrees, the node and its row, the ambient
/// harness. The parity tests never call this.
pub fn gather(ask: &Value, cwd: &Path) -> Inputs {
    let cfg = |keys: &[&str]| {
        crate::agents_config::config_value_deep(cwd, keys)
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()))
    };
    let scan = ask.get("scan").cloned().unwrap_or_else(|| json!({}));
    let env_node = ask
        .get("env_node")
        .and_then(Value::as_str)
        .map(str::to_string);
    let flag_node = scan
        .get("flag_node")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|v| !v.is_empty());
    let argv: Vec<String> = ask
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    let node = resolve_node_id(&argv, &scan, flag_node, env_node.clone());
    let node_row = node
        .as_ref()
        .and_then(|id| graph_row_for(id))
        .filter(|row| row.is_object());
    Inputs {
        ambient_harness: crate::claims::resolve_harness_from(|key| {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "claude".to_string()),
        apply_permission_builtin: ask
            .get("permission_builtin")
            .map(|v| !v.is_null())
            .unwrap_or(true),
        argv,
        defaults: cfg(&["agents", "defaults"]),
        dispatch_verbs: cfg(&["dispatch", "verbs"]),
        env_node,
        facts: ask.get("facts").cloned().unwrap_or_else(|| json!({})),
        node,
        node_row,
        node_verb: ask
            .get("node_verb")
            .and_then(Value::as_str)
            .map(str::to_string),
        profiles: cfg(&["agents", "profiles"]),
        roster: crate::provider::footnote_verbs().into_iter().collect(),
        scan,
    }
}

/// The node id the old seam resolved: `--node` wins, then the env carrier,
/// then the seed slot's payload-named node (the `spawn-axes` `spawn_node`
/// ask, now in-process).
fn resolve_node_id(
    argv: &[String],
    scan: &Value,
    flag_node: Option<String>,
    env_node: Option<String>,
) -> Option<String> {
    if let Some(flag) = flag_node {
        return Some(flag);
    }
    // No flag: the ask resolves seed-first, then the env carrier.
    let ask = json!({
        "argv": argv,
        "seed_index": scan.get("seed_index"),
        "seed_form": scan.get("seed_form"),
        "flag_node": Value::Null,
        "env_node": env_node,
    });
    let answer = crate::node_seed::resolve_node(&ask);
    answer
        .get("node")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|v| !v.is_empty())
}

/// The grid row Python's `_grid_node` read: the node's entry in the graph
/// store, advisory only.
fn graph_row_for(node_id: &str) -> Option<Value> {
    let rows = crate::graph_store::read_rows(&crate::graph_get::default_graph_path()).ok()?;
    rows.into_iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(node_id))
}

/// The first verb-shaped token anywhere in the seed, sigil and namespace
/// stripped (Python `_verb_token`).
fn verb_token(seed: Option<&str>) -> Option<String> {
    let seed = seed?;
    for tok in seed.split_whitespace() {
        if let Some((verb, _)) = crate::provider::parse_verb_token(tok) {
            return Some(verb.to_string());
        }
    }
    None
}

/// Whether the seed's verb token carried the `fno:` namespace - the one
/// marker that PROVES the seed names a footnote stage (Python
/// `_carries_fno_namespace`).
fn carries_namespace(seed: Option<&str>, tok: &str) -> bool {
    let Some(seed) = seed else {
        return false;
    };
    seed.split_whitespace()
        .any(|t| matches!(crate::provider::parse_verb_token(t), Some((v, true)) if v == tok))
}

fn known_verb_keys(inputs: &Inputs) -> (Vec<String>, bool) {
    let mut known: Vec<String> = inputs
        .profiles
        .as_table()
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();
    let roster_ok = !inputs.roster.is_empty();
    known.extend(inputs.roster.iter().cloned());
    if let Some(registry) = inputs.dispatch_verbs.as_table() {
        for key in registry.keys() {
            let canon = crate::backlog::fields::canonical_verb_key(key);
            let canon = canon.trim_start_matches('/').to_string();
            if !canon.is_empty() && !known.iter().any(|k| k == &canon) {
                known.push(canon);
            }
        }
    }
    (known, roster_ok)
}

/// The seed's profile key (Python `_profile_key`): a resolvable verb token
/// names its canonical profile; a king verb or no token names `crown`; an
/// unknown `fno:`-namespaced token refuses (None).
fn profile_key(seed: Option<&str>, known: Option<&[String]>) -> Option<String> {
    let Some(tok) = verb_token(seed) else {
        return Some("crown".to_string());
    };
    let key = VERB_ALIASES
        .iter()
        .find(|(old, _)| *old == tok)
        .map(|(_, new)| new.to_string())
        .unwrap_or_else(|| tok.clone());
    if CROWN_VERBS.contains(&key.as_str()) {
        return Some("crown".to_string());
    }
    match known {
        None => Some(key),
        Some(known) => {
            if known.iter().any(|k| k == &tok) || known.iter().any(|k| k == &key) {
                return Some(key);
            }
            if carries_namespace(seed, &tok) {
                return None;
            }
            Some("crown".to_string())
        }
    }
}

/// The scan projection Python computed once, before the slot resolver: a
/// lane named on the command line, an occupied model axis, a `--yolo`
/// spelling of the permission knob.
pub(crate) struct Axes {
    pub has_harness: bool,
    pub explicit_harness: Option<String>,
    pub has_model: bool,
    pub has_effort: bool,
    pub explicit_vendor: Option<String>,
    pub explicit_vendor_present: bool,
    pub explicit_route: bool,
    pub route_value: Option<String>,
    pub model_value: Option<String>,
    pub role: Option<String>,
    pub explicit_substrate: Option<String>,
    pub permission_value: Option<String>,
    pub has_permission: bool,
    pub flag_node: Option<String>,
    pub seed: Option<String>,
    pub account_flag_present: bool,
    pub tab_flag_present: bool,
    pub name: Option<String>,
    pub positional_present: bool,
}

fn scan_of(scan: &Value) -> Axes {
    let s = |key: &str| -> Option<String> {
        scan.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|v| !v.is_empty())
    };
    let b = |key: &str| scan.get(key).and_then(Value::as_bool).unwrap_or(false);
    Axes {
        has_harness: b("has_harness"),
        explicit_harness: s("explicit_harness"),
        has_model: b("has_model"),
        has_effort: b("has_effort"),
        explicit_vendor_present: scan
            .get("explicit_vendor")
            .and_then(Value::as_str)
            .is_some(),
        explicit_vendor: s("explicit_vendor"),
        explicit_route: b("explicit_route"),
        route_value: s("route_value"),
        model_value: s("model_value"),
        role: s("role"),
        explicit_substrate: s("explicit_substrate"),
        permission_value: s("permission_value"),
        has_permission: b("has_permission"),
        flag_node: s("flag_node"),
        seed: s("seed"),
        account_flag_present: b("account_flag_present"),
        tab_flag_present: b("tab_flag_present"),
        name: s("name"),
        positional_present: b("positional_present"),
    }
}

fn profile_table<'a>(inputs: &'a Inputs, verb: &str) -> Option<(&'a toml::Table, String)> {
    let profiles = inputs.profiles.as_table()?;
    let direct = profiles.get(verb);
    if let Some(t) = direct.and_then(toml::Value::as_table) {
        return Some((t, verb.to_string()));
    }
    // The legacy alias: the canonical verb keeps the old spelling's row.
    let legacy = VERB_ALIASES
        .iter()
        .find(|(_, new)| *new == verb)
        .map(|(old, _)| old.to_string());
    let old = legacy?;
    let table = profiles.get(&old)?.as_table()?;
    Some((table, old))
}

fn cfg_str(table: Option<&toml::Table>, key: &str) -> String {
    table
        .and_then(|t| t.get(key))
        .and_then(toml::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// One field's effective value + source rung: lane > profile > defaults,
/// harness-blind (the two harness rungs live in the overlay answer).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Field(pub String, pub Option<String>);

fn field_read(
    lane: Option<&Value>,
    lane_index: Option<usize>,
    profile: Option<&toml::Table>,
    profile_verb: &str,
    defaults: &toml::Table,
    name: &str,
) -> Field {
    if let (Some(lane), Some(index)) = (lane, lane_index) {
        let value = lane_value(lane, name);
        if !value.is_empty() {
            return Field(
                value,
                Some(format!("agents.profiles.{profile_verb}.lanes[{index}]")),
            );
        }
        if LANE_EXCLUSIVE.contains(&name) {
            return Field(String::new(), None);
        }
    }
    let pv = cfg_str(profile, name);
    if !pv.is_empty() {
        return Field(pv, Some(format!("agents.profiles.{profile_verb}")));
    }
    let dv = cfg_str(Some(defaults), name);
    if !dv.is_empty() {
        return Field(dv, Some("agents.defaults".to_string()));
    }
    Field(String::new(), None)
}

fn lane_value(lane: &Value, name: &str) -> String {
    lane.get(name)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The compose's answer, exactly what the transport applies.
#[derive(Debug, Clone)]
pub struct Answer {
    pub argv: Vec<String>,
    pub stderr: Vec<String>,
    pub exit: i32,
    pub stdout: Option<Value>,
    pub events: Vec<Value>,
    pub injected: bool,
    /// The billing gate needs `facts.role_resolves` and the ask carried no
    /// answer: nothing applied, nothing journaled; the transport resolves
    /// the role once and asks again.
    pub role_gate_needed: bool,
}

impl Answer {
    pub fn to_value(&self) -> Value {
        json!({
            "argv": self.argv,
            "stderr": self.stderr,
            "exit": self.exit,
            "stdout": self.stdout.clone().unwrap_or(Value::Null),
            "events": self.events,
            "injected": self.injected,
            "role_gate_needed": self.role_gate_needed,
        })
    }
}

/// Output collector shared by the stages, in the seam's own order.
#[derive(Default)]
struct Seam {
    stderr: Vec<String>,
    applied: Vec<Value>,
    suppressed: Vec<Value>,
    inject: Vec<String>,
    bundle: Vec<String>,
    events: Vec<Value>,
    exit: Option<i32>,
    stdout: Option<Value>,
    argv: Option<Vec<String>>,
    injected: bool,
    role_gate_needed: bool,
}

impl Seam {
    /// A refusal. The trailing "refusing; no worker launched" line is the
    /// caller's vocabulary (only some Python refusals printed it), never a
    /// fixture of this function.
    fn refuse(&mut self, line: String) {
        self.stderr.push(line);
        self.exit = Some(2);
    }

    fn note(&mut self, line: String) {
        self.stderr.push(line);
    }
}

/// The stage context: Python's locals a stage reads, one struct so the
/// stages stay pure over the same world the goldens pinned.
struct Stage<'a> {
    inputs: &'a Inputs,
    scan: Axes,
    verb: String,
    profile_verb: String,
    profile: Option<&'a toml::Table>,
    defaults: toml::Table,
    lane: Option<Value>,
    lane_index: Option<usize>,
    slot_candidate: Option<Value>,
    slot_chain: Vec<String>,
    fingerprint: String,
    grid_account_injected: bool,
    node_id_present: bool,
    harness_done: bool,
    harness: Option<String>,
    overlay_answer: Option<Value>,
    injected_substrate: Option<String>,
    out_tail_empty: bool,
    fields: Option<Fields>,
    has_harness: bool,
    has_model: bool,
    has_effort: bool,
}

impl<'a> Stage<'a> {
    fn argv_tail(&self) -> Vec<String> {
        self.inputs.argv.iter().skip(1).cloned().collect()
    }

    /// The tail the spawn-token scanners read: pre-fence tokens only.
    fn spawn_token_tail(&self) -> Vec<String> {
        let tail = self.argv_tail();
        match tail.iter().position(|t| t == "--argv" || t == "--") {
            Some(cut) => tail[..cut].to_vec(),
            None => tail,
        }
    }

    fn fields(&self) -> &Fields {
        self.fields
            .as_ref()
            .expect("fields were read before this stage")
    }
}

/// The pure composition: the ported body of the old Python seam, in the
/// order Python ran it.
pub fn compose(inputs: &Inputs) -> Answer {
    let scan = scan_of(&inputs.scan);
    let mut seam = Seam {
        argv: Some(inputs.argv.clone()),
        ..Default::default()
    };
    compose_body(inputs, scan, &mut seam);
    // Python printed one stderr line per visual line; a refusal composed
    // with embedded newlines lands as separate entries.
    let stderr = seam
        .stderr
        .iter()
        .flat_map(|line| line.split('\n').map(str::to_string))
        .collect();
    Answer {
        // A refused spawn never hands the caller an argv: Python's seam
        // raised SystemExit inside, so the transport kept the argv it was
        // given. The answer carries the input argv unchanged on any
        // nonzero exit, injected or not.
        argv: if seam.exit.unwrap_or(0) != 0 {
            inputs.argv.clone()
        } else {
            seam.argv.take().unwrap_or_default()
        },
        stderr,
        exit: seam.exit.unwrap_or(0),
        stdout: seam.stdout.take(),
        events: seam.events,
        injected: seam.injected,
        role_gate_needed: seam.role_gate_needed,
    }
}

fn compose_body(inputs: &Inputs, scan: Axes, seam: &mut Seam) {
    // Python's passthrough gates: a non-spawn head, or -h/--help before the
    // --argv boundary, returns unchanged and journals nothing.
    let help_rides = inputs
        .argv
        .iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--argv")
        .any(|a| a == "-h" || a == "--help");
    if inputs.argv.first().map(String::as_str) != Some("spawn") || help_rides {
        return;
    }
    let (verb, profile_verb, profile) = match resolve_profile(inputs, &scan) {
        Ok(found) => found,
        Err(lines) => {
            for line in lines {
                seam.refuse(line);
            }
            return;
        }
    };
    let defaults = inputs.defaults.as_table().cloned().unwrap_or_default();
    let node_id_present =
        scan.flag_node.is_some() || inputs.env_node.is_some() || inputs.node.is_some();
    let has_harness = scan.has_harness;
    let has_model = scan.has_model;
    let has_effort = scan.has_effort;
    let mut stage = Stage {
        inputs,
        scan,
        verb,
        profile_verb,
        profile: profile.as_ref(),
        defaults,
        lane: None,
        lane_index: None,
        slot_candidate: None,
        slot_chain: Vec::new(),
        fingerprint: String::new(),
        grid_account_injected: false,
        node_id_present,
        harness_done: false,
        harness: None,
        overlay_answer: None,
        injected_substrate: None,
        out_tail_empty: false,
        fields: None,
        has_harness,
        has_model,
        has_effort,
    };
    walk_stage(&mut stage, seam);
    if seam.exit.is_some() {
        return;
    }
    let mut stage = {
        let fields = read_fields(&stage);
        stage.fields = Some(fields);
        stage
    };
    {
        let fields = stage.fields();
        if early_return(&stage, fields, seam) {
            return;
        }
    }
    grid_inject(&mut stage, seam);
    harness_rung(&mut stage, seam);
    if seam.exit.is_some() {
        return;
    }
    // Python's seam resolved the role ONLY when the billing model branch
    // needed it (config model, axis free, a role named): a bare role spawn
    // resolved zero times at the seam, and cmd_spawn's own single resolve
    // is the one the role-wiring contract pins. When the gate fires and
    // the ask carries no answer, stop before any side effect: the
    // transport resolves and asks again.
    {
        let fields = stage.fields.as_ref().expect("fields were read");
        let needs_role =
            !fields.model.0.is_empty() && !stage.has_model && stage.scan.role.is_some();
        let answered = stage
            .inputs
            .facts
            .get("role_resolves")
            .and_then(Value::as_bool)
            .is_some();
        if needs_role && !answered {
            seam.role_gate_needed = true;
            return;
        }
    }
    billing_axes(&mut stage, seam);
    overlay_stage(&mut stage, seam);
    if seam.exit.is_some() {
        return;
    }
    mechanical_axes(&mut stage, seam);
    pane_bundle_stage(&mut stage, seam);
    assemble(&mut stage, seam);
}

/// Profile-key resolution: the seed's verb names the profile; an unknown
/// `fno:`-namespaced token refuses with the two lines Python printed.
fn resolve_profile(
    inputs: &Inputs,
    scan: &Axes,
) -> Result<(String, String, Option<toml::Table>), Vec<String>> {
    let (known, roster_ok) = known_verb_keys(inputs);
    let mut profile_seed = scan.seed.clone();
    if profile_seed.is_none() {
        if let Some(node_verb) = &inputs.node_verb {
            profile_seed = Some(format!("/{node_verb}"));
        }
    }
    let known_ref = if roster_ok {
        Some(known.as_slice())
    } else {
        None
    };
    let verb = profile_key(profile_seed.as_deref(), known_ref).ok_or_else(|| {
        let tok = verb_token(scan.seed.as_deref()).unwrap_or_default();
        vec![
            format!(
                "fno agents spawn: seed {} names verb-shaped token {} but no \
                 shipped footnote verb or configured profile answers it",
                crate::spawn_axes::repr(scan.seed.as_deref().unwrap_or("")),
                crate::spawn_axes::repr(&tok)
            ),
            "fno agents spawn: refusing; no worker launched".to_string(),
        ]
    })?;
    let (profile, profile_verb) = match profile_table(inputs, &verb) {
        Some((table, name)) => (Some(table.clone()), name),
        None => (None, verb.clone()),
    };
    Ok((verb, profile_verb, profile))
}

/// Where a scalar field's value would come from with no lane in play.
fn scalar_rung(
    profile: Option<&toml::Table>,
    profile_verb: &str,
    defaults: &toml::Table,
    name: &str,
) -> Option<String> {
    if !cfg_str(profile, name).is_empty() {
        return Some(format!("agents.profiles.{profile_verb}"));
    }
    if !cfg_str(Some(defaults), name).is_empty() {
        return Some("agents.defaults".to_string());
    }
    None
}

fn above_defaults(rung: &Option<String>) -> bool {
    rung.as_deref()
        .map(|r| r != "agents.defaults")
        .unwrap_or(false)
}

/// The slot walk, in process: fill the walk's payload, run the resolver,
/// print the chain notes, and apply the terminal rules (refusal exit 2,
/// exhausted queue exit 78 with the payload on stdout, receipts).
fn walk_stage(stage: &mut Stage, seam: &mut Seam) {
    let scan = &stage.scan;
    let lanes_present = stage.profile.is_some_and(|p| {
        p.get("lanes").and_then(toml::Value::as_array).is_some()
            || p.get("by_difficulty")
                .and_then(toml::Value::as_table)
                .is_some_and(|t| !t.is_empty())
    });
    let model_rung = scalar_rung(stage.profile, &stage.profile_verb, &stage.defaults, "model");
    let route_rung = scalar_rung(stage.profile, &stage.profile_verb, &stage.defaults, "route");
    let model_occupied = scan.has_model
        || scan.explicit_vendor_present
        || scan.explicit_route
        || above_defaults(&model_rung)
        || above_defaults(&route_rung);
    // Python's arm: the walk consults when lanes are configured, the model
    // axis is free, or strict routing is on. An occupied axis over a
    // lane-less profile stands down loudly: one grid receipt, no walk.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let enforced = crate::route_gather::policy_for(&cwd)["enforce_inventory"]
        .as_bool()
        .unwrap_or(false);
    if !(lanes_present || !model_occupied || enforced) {
        if stage.node_id_present {
            seam.applied
                .push(json!(["grid", "grid=model-axis-occupied", "routing"]));
        }
        return;
    }
    let mut grid_node_entry: Option<Value> = None;
    if !model_occupied || enforced {
        grid_node_entry = stage.inputs.node_row.clone();
    }
    // No lanes, no node to grid on, nothing strict: the walk has no
    // question to answer.
    if !(lanes_present || grid_node_entry.is_some() || enforced) {
        return;
    }
    let grid_role = if stage.verb == "blueprint" || stage.verb == "think" {
        Some("planning".to_string())
    } else {
        None
    };
    let constrain = scan
        .explicit_harness
        .clone()
        .or_else(|| Some(cfg_str(stage.profile, "provider")))
        .filter(|v| !v.trim().is_empty());
    let ask = json!({
        "rung_base": format!("agents.profiles.{}", stage.profile_verb),
        "lanes_raw": lanes_payload(stage.profile),
        "profile": crate::route_gather::profile_fields_for(stage.profile),
        "node": grid_node_entry,
        "substrate": scan.explicit_substrate,
        "permission_mode": scan.permission_value,
        "constrain_harness": constrain,
        "explicit_lane": scan.has_harness || scan.explicit_route || scan.explicit_vendor_present,
        "gate_bypassed": false,
        "role": grid_role,
        "protected_role": stage.inputs.facts.get("role_protected"),
        "model_occupied": model_occupied,
        "work_verb": stage.verb,
        "explicit_model_value": if scan.has_model { scan.model_value.clone() } else { None },
        "explicit_route_value": if scan.explicit_route { scan.route_value.clone() } else { None },
        "explicit_vendor_value": if scan.explicit_vendor_present {
            scan.explicit_vendor.clone()
        } else {
            None
        },
        "capacity_refresh": true,
    });
    let filled = crate::route_gather::fill(&ask, &cwd);
    let out = crate::route_slot::resolve_slot_payload(&filled);
    crate::route_slot::journal_routing_refusal(&filled, &out);
    apply_walk_answer(stage, seam, &out);
}

/// The walk answer's receipts and terminals: chain notes print and bill, the
/// refusal terminal rules, the exhausted queue rides stdout with exit 78,
/// and a lane pick feeds `field()` from here on.
fn apply_walk_answer(stage: &mut Stage, seam: &mut Seam, out: &Value) {
    let chain: Vec<String> = out
        .get("chain")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    for line in &chain {
        if line.starts_with("slot skip")
            || line.starts_with("slot note")
            || line.starts_with("slot demote")
        {
            seam.note(format!("fno agents spawn: {line}"));
            seam.suppressed.push(json!(["slot", "", "", line]));
        }
    }
    if let Some(refusal) = out.get("refusal_terminal").filter(|r| !r.is_null()) {
        let text = refusal.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            seam.refuse(format!(
                "fno agents spawn: {text}\nfno agents spawn: refusing; no \
                 worker launched"
            ));
            return;
        }
    }
    if let Some(exhausted) = out.get("exhausted_payload").filter(|e| !e.is_null()) {
        seam.stdout = Some(exhausted.clone());
        seam.exit = Some(78);
        return;
    }
    stage.fingerprint = out
        .get("fingerprint")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let candidate = out.get("candidate").filter(|c| !c.is_null());
    let terminal = chain.last().cloned().unwrap_or_default();
    if let Some(candidate) = candidate {
        if let Some(rung) = candidate.get("lane_rung").and_then(Value::as_str) {
            let lane_name = candidate.get("lane").and_then(Value::as_str).unwrap_or("");
            stage.lane = candidate.get("lane_fields").cloned();
            stage.lane_index = candidate
                .get("lane_index")
                .and_then(Value::as_u64)
                .map(|i| i as usize);
            seam.applied
                .push(json!(["slot", format!("{rung} {lane_name}"), "routing",]));
            stage.slot_candidate = Some(candidate.clone());
            return;
        }
    }
    // A slot=-terminal receipts under the slot arm (the pin override); any
    // other non-empty terminal receipts under the grid arm - the no-lanes
    // grid vocabulary (grid=no-inventory-declared, ...) needs no candidate
    // behind it.
    if terminal.starts_with("slot=") {
        seam.applied.push(json!([
            "slot",
            terminal.trim_start_matches("slot="),
            "routing"
        ]));
    } else if !terminal.is_empty() {
        seam.applied.push(json!(["grid", terminal, "routing"]));
    }
    stage.slot_candidate = candidate.cloned();
}

/// The profile's lanes as the payload carries them (padded vocabulary dump).
fn lanes_payload(profile: Option<&toml::Table>) -> Value {
    let Some(lanes) = profile
        .and_then(|p| p.get("lanes"))
        .and_then(toml::Value::as_array)
    else {
        return Value::Null;
    };
    Value::Array(
        lanes
            .iter()
            .map(crate::route_gather::lane_entry_for)
            .collect(),
    )
}

/// The field() reads: lane > profile > defaults, one per config field.
pub(crate) struct Fields {
    pub harness: Field,
    pub model: Field,
    pub effort: Field,
    pub substrate: Field,
    pub permission: Field,
    pub route: Field,
    pub account: Field,
    pub pane_group: Field,
}

fn read_fields(stage: &Stage) -> Fields {
    let view = (stage.lane.as_ref(), stage.lane_index, stage.profile);
    let f = |name: &str| {
        field_read(
            view.0,
            view.1,
            view.2,
            &stage.profile_verb,
            &stage.defaults,
            name,
        )
    };
    let mut permission = f("permission_mode");
    if permission.0.is_empty()
        && stage.inputs.apply_permission_builtin
        && is_seed_verb(&stage.scan.seed)
    {
        permission = Field(
            SPAWN_PERMISSION_BUILTIN.to_string(),
            Some("builtin.autonomous".to_string()),
        );
    }
    Fields {
        harness: f("provider"),
        model: f("model"),
        effort: f("effort"),
        substrate: f("substrate"),
        permission,
        route: f("route"),
        account: f("account"),
        pane_group: f("pane_group"),
    }
}

/// Whether the seed's FIRST token is verb-shaped (Python `is_verb_seed`).
fn is_seed_verb(seed: &Option<String>) -> bool {
    seed.as_deref()
        .and_then(|s| s.split_whitespace().next())
        .is_some_and(|tok| crate::provider::parse_verb_token(tok).is_some())
}

/// An overlay table (or lane args) can carry the ONLY value this spawn
/// injects, so an empty harness-blind read must not end composition early.
fn overlays_present(stage: &Stage) -> bool {
    let table_has = |t: &toml::Table| {
        t.get("harness")
            .and_then(toml::Value::as_table)
            .is_some_and(|h| !h.is_empty())
    };
    if table_has(&stage.defaults) {
        return true;
    }
    if stage.profile.is_some_and(table_has) {
        return true;
    }
    stage
        .lane
        .as_ref()
        .and_then(|lane| lane.get("args"))
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// The early return: no config field resolved, no grid candidate, no
/// receipts, no overlays - any `--model` here was typed, so only the vendor
/// check and the journal run.
fn early_return(stage: &Stage, fields: &Fields, seam: &mut Seam) -> bool {
    let any_field = !fields.harness.0.is_empty()
        || !fields.model.0.is_empty()
        || !fields.effort.0.is_empty()
        || !fields.substrate.0.is_empty()
        || !fields.permission.0.is_empty()
        || !fields.route.0.is_empty()
        || !fields.account.0.is_empty()
        || !fields.pane_group.0.is_empty();
    let grid_candidate = grid_candidate(stage).is_some();
    if any_field || grid_candidate || !seam.applied.is_empty() || overlays_present(stage) {
        return false;
    }
    vendor_check(stage, seam, None);
    journal(stage, seam, "");
    true
}

/// A grid-branch candidate (no lane_rung) is an atomic harness/model/effort
/// TRIPLE; a lane candidate feeds field() instead.
fn grid_candidate(stage: &Stage) -> Option<Value> {
    let candidate = stage.slot_candidate.as_ref()?;
    if candidate.get("lane_rung").is_some() {
        return None;
    }
    Some(candidate.clone())
}

/// The resolved-target harness, lazily: explicit -H, then the merged config
/// `provider`, then the ambient inference (builtin claude).
fn resolved_harness(stage: &mut Stage) -> Option<String> {
    if !stage.harness_done {
        stage.harness_done = true;
        let cfg = stage.fields().harness.0.clone();
        stage.harness = if let Some(explicit) = stage
            .scan
            .explicit_harness
            .clone()
            .filter(|h| !h.trim().is_empty())
        {
            Some(explicit)
        } else if !cfg.is_empty() {
            Some(cfg)
        } else {
            Some(stage.inputs.ambient_harness.clone())
        };
    }
    stage.harness.clone()
}

fn fields_of<'a>(stage: &'a Stage<'a>) -> &'a Fields {
    stage.fields()
}

/// Lane first (a lane is a complete coordinate), then the harness-keyed
/// answer, then the harness-blind scalars: field()'s order with the two
/// harness rungs spliced in above it.
fn seamed_read(stage: &Stage, name: &str, blind: Field) -> Field {
    if let (Some(lane), Some(index)) = (stage.lane.as_ref(), stage.lane_index) {
        let value = lane_value(lane, name);
        if !value.is_empty() {
            return Field(
                value,
                Some(format!(
                    "agents.profiles.{}.lanes[{index}]",
                    stage.profile_verb
                )),
            );
        }
    }
    if let Some(answer) = &stage.overlay_answer {
        if let Some(entry) = answer
            .get("effective")
            .and_then(|e| e.get(name))
            .filter(|e| !e.is_null())
        {
            let value = entry.get("value").and_then(Value::as_str).unwrap_or("");
            let rung = entry.get("rung").and_then(Value::as_str).unwrap_or("");
            if !value.is_empty() {
                return Field(value.to_string(), Some(rung.to_string()));
            }
        }
    }
    blind
}

/// The grid triple's injections: the pin's model is already on the argv, so
/// only the harness injects; effort joins when its axis was free; the route
/// rides beside the model; the account is claude-only at the CLI.
fn grid_inject(stage: &mut Stage, seam: &mut Seam) {
    let Some(candidate) = grid_candidate(stage) else {
        return;
    };
    let s = |key: &str| {
        candidate
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let pin_row = s("pin_row");
    let src = if pin_row.is_empty() {
        "difficulty-grid".to_string()
    } else {
        format!("routing.models.{pin_row}")
    };
    let harness = s("harness");
    let model = s("model");
    let mut inject = vec!["--harness".to_string(), harness.clone()];
    if !stage.scan.has_model {
        inject.extend(["--model".to_string(), model.clone()]);
    }
    if !pin_row.is_empty() {
        seam.applied.push(json!(["harness", harness, src]));
    } else {
        seam.applied
            .push(json!(["grid", format!("{harness}/{model}"), src]));
    }
    stage.has_harness = true;
    stage.has_model = true;
    let effort = s("effort");
    let effort_occupied = stage.scan.has_effort
        || above_defaults(&scalar_rung(
            stage.profile,
            &stage.profile_verb,
            &stage.defaults,
            "effort",
        ))
        || stage.lane.is_some();
    if !effort.is_empty() && !effort_occupied {
        inject.extend(["--effort".to_string(), effort.clone()]);
        seam.applied.push(json!(["effort", effort, src]));
        stage.has_effort = true;
    }
    let route = s("route");
    if !route.is_empty() && !stage.scan.explicit_route && !stage.scan.explicit_vendor_present {
        inject.extend(["--route".to_string(), route.clone()]);
        seam.applied.push(json!(["route", route, src]));
    }
    let account = s("account");
    if !account.is_empty() && !stage.scan.account_flag_present {
        if harness == "claude" {
            inject.extend(["--account".to_string(), account.clone()]);
            seam.applied.push(json!(["account", account, src]));
            stage.grid_account_injected = true;
        } else {
            seam.note(format!(
                "fno agents spawn: account skipped (claude-only, grid harness {}); {} ignored",
                crate::spawn_axes::repr(&harness),
                crate::spawn_axes::repr(&account),
            ));
        }
    }
    seam.inject.extend(inject);
    // Python pre-seeds the resolved-harness cache with the candidate's
    // harness, so the effort/substrate/permission checks read the grid's
    // coordinate, never the ambient default.
    stage.harness = Some(harness);
    stage.harness_done = true;
}

/// The harness rung: the config `provider` fills the HARNESS axis only when
/// nothing on the argv set it; an unknown harness refuses, and a route only
/// claude can carry collides with a non-claude profile harness.
fn harness_rung(stage: &mut Stage, seam: &mut Seam) {
    let cfg_harness = stage.fields().harness.0.clone();
    let provider_rung = stage.fields().harness.1.clone().unwrap_or_default();
    if cfg_harness.is_empty() || stage.has_harness {
        return;
    }
    if !crate::provider::KNOWN_PROVIDERS.contains(&cfg_harness.as_str()) {
        let valid = crate::provider::KNOWN_PROVIDERS.join(", ");
        seam.refuse(format!(
            "fno agents spawn: config.{provider_rung}.provider = {} is not a \
             known harness; valid: {valid}",
            crate::spawn_axes::repr(&cfg_harness),
        ));
        return;
    }
    let route_shaped =
        stage.scan.explicit_route || (stage.scan.explicit_vendor_present && stage.scan.has_model);
    if route_shaped && cfg_harness != "claude" {
        let caller_spelling = if stage.scan.explicit_route {
            format!(
                "--route {}",
                stage.scan.route_value.clone().unwrap_or_default()
            )
        } else {
            format!(
                "-P {} --model {}",
                stage.scan.explicit_vendor.clone().unwrap_or_default(),
                stage.scan.model_value.clone().unwrap_or_default(),
            )
        };
        seam.refuse(format!(
            "fno agents spawn: config.{provider_rung}.provider = {} sets the \
             HARNESS axis,\nand a route only the claude harness can carry is \
             already on your command line\n({caller_spelling}).\nNothing you \
             passed set the harness: -P names the model vendor, a different \
             axis,\nso the profile filled it.\nPass -H claude to keep your \
             route, or clear {provider_rung}.provider.",
            crate::spawn_axes::repr(&cfg_harness),
        ));
        return;
    }
    seam.inject
        .extend(["--harness".to_string(), cfg_harness.clone()]);
    seam.applied.push(json!([
        "harness",
        cfg_harness,
        format!("{provider_rung}.provider")
    ]));
    stage.has_harness = true;
}

/// The vendor-mismatch judgment over the FINAL argv: a typed mismatch warns
/// and proceeds, an injected one refuses; an explicit route suppresses both.
fn vendor_check(stage: &Stage, seam: &mut Seam, model_source: Option<&str>) {
    // Python's fast path: no --model on the final argv (typed or injected)
    // leaves nothing to judge. The lane's harness is the final argv's first
    // --harness: an injected one is the only kind the head can add, because
    // injection is suppressed when the operator typed one.
    let model_on_argv = stage.scan.has_model || seam.inject.iter().any(|t| t == "--model");
    if !model_on_argv {
        return;
    }
    let harness_typed = stage
        .scan
        .explicit_harness
        .as_deref()
        .map(|h| !h.trim().is_empty())
        .unwrap_or(false);
    let harness = seam
        .inject
        .iter()
        .position(|t| t == "--harness")
        .and_then(|i| seam.inject.get(i + 1))
        .cloned()
        .or_else(|| stage.scan.explicit_harness.clone());
    let argv_tail: Vec<String> = seam
        .argv
        .as_ref()
        .map(|a| a.iter().skip(1).cloned().collect())
        .unwrap_or_default();
    let ask = json!({
        "kind": "model-vendor",
        "argv_tail": argv_tail,
        "argv_head": "spawn",
        "harness": harness,
        "env_harness": stage.inputs.ambient_harness,
        "model_source": model_source,
        "harness_typed": harness_typed,
    });
    let answer = match crate::spawn_overlay::resolve(ask) {
        Ok(answer) => answer,
        Err(exc) => {
            seam.note(format!("fno agents spawn: vendor check skipped ({exc})"));
            return;
        }
    };
    if let Some(event) = answer
        .get("event")
        .filter(|e| e.is_object() && !e.is_null())
    {
        seam.events.push(event.clone());
    }
    match answer.get("verdict").and_then(Value::as_str) {
        Some("refuse") => {
            let message = answer.get("message").and_then(Value::as_str).unwrap_or("");
            seam.refuse(message.to_string());
        }
        Some("warn") => {
            let message = answer.get("message").and_then(Value::as_str).unwrap_or("");
            seam.note(message.to_string());
        }
        _ => {}
    }
}

/// The billing axes (route/account/model, ruling 4) through the one Rust
/// owner, in process; the model-target pre-read runs here first.
fn billing_axes(stage: &mut Stage, seam: &mut Seam) {
    // Python's prov here is resolved_harness(): explicit -H, the grid's
    // seeded pick, the config field rung, then ambient - the cached chain,
    // never the ambient default directly. Resolved before `fields` borrows
    // the stage immutably.
    let prov = resolved_harness(stage).unwrap_or_default();
    let fields = stage.fields();
    if fields.route.0.is_empty() && fields.account.0.is_empty() && fields.model.0.is_empty() {
        return;
    }
    let role = stage.scan.role.clone();
    let mut role_resolves = false;
    if !fields.model.0.is_empty() && !stage.has_model && role.is_some() {
        role_resolves = stage
            .inputs
            .facts
            .get("role_resolves")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    }
    let mut harness_target: Option<String> = None;
    // Python set this only when its dispatch-harness probe threw; the
    // in-process read cannot fail, so the ask always carries false.
    let harness_target_failed = false;
    if !fields.model.0.is_empty()
        && !stage.has_model
        && !stage.scan.explicit_route
        && !stage.scan.explicit_vendor_present
        && !role_resolves
    {
        harness_target = if let Some(explicit) = stage
            .scan
            .explicit_harness
            .clone()
            .filter(|h| !h.trim().is_empty())
        {
            Some(explicit)
        } else if !fields.harness.0.is_empty() {
            Some(fields.harness.0.clone())
        } else {
            Some(stage.inputs.ambient_harness.clone())
        };
    }
    let ask = json!({
        "route": {"value": fields.route.0.clone(), "rung": fields.route.1.clone().unwrap_or_default()},
        "account": {"value": fields.account.0.clone(), "rung": fields.account.1.clone().unwrap_or_default()},
        "model": {"value": fields.model.0.clone(), "rung": fields.model.1.clone().unwrap_or_default()},
        "explicit_route": stage.scan.explicit_route,
        "explicit_vendor_present": stage.scan.explicit_vendor_present,
        "explicit_vendor": stage.scan.explicit_vendor.clone().unwrap_or_default(),
        "explicit_model_present": stage.has_model,
        "has_model": stage.has_model,
        "grid_candidate_present": grid_candidate(stage).is_some(),
        "slot_chain": stage.slot_chain,
        "grid_account_injected": stage.grid_account_injected,
        "account_flag_present": stage.scan.account_flag_present,
        "prov": if !stage.fields().account.0.is_empty() {
            json!(prov)
        } else {
            json!("")
        },
        "role": role,
        "role_resolves": role_resolves,
        "cfg_harness": fields.harness.0.clone(),
        "harness_target": harness_target,
        "harness_target_failed": harness_target_failed,
    });
    let answer = crate::spawn_axes::decide(&ask);
    for line in answer
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        seam.note(line.as_str().unwrap_or_default().to_string());
    }
    for pair in answer
        .get("inject")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(pair) = pair.as_array() {
            for token in pair {
                seam.inject
                    .push(token.as_str().unwrap_or_default().to_string());
            }
        }
    }
    if let Some(applied) = answer.get("applied").and_then(Value::as_array) {
        seam.applied.extend(applied.iter().cloned());
    }
    if let Some(suppressed) = answer.get("suppressed").and_then(Value::as_array) {
        seam.suppressed.extend(suppressed.iter().cloned());
    }
}

/// The mechanical axes: effort/substrate/permission re-read through the
/// harness rungs, pre-checked in process, decided by the one owner.
fn mechanical_axes(stage: &mut Stage, seam: &mut Seam) {
    let mut effort = fields_of(stage).effort.clone();
    if !stage.scan.has_effort {
        effort = seamed_read(stage, "effort", effort);
    }
    let mut effort_reason = String::new();
    if !effort.0.is_empty() {
        let prov = resolved_harness(stage).unwrap_or_default();
        if let Err(exc) = crate::effort_surface::effort_tokens(&prov, &effort.0) {
            effort_reason = exc;
        }
    }
    let explicit_substrate = stage.scan.explicit_substrate.clone();
    let mut substrate = fields_of(stage).substrate.clone();
    if explicit_substrate.is_none() {
        substrate = seamed_read(stage, "substrate", substrate);
    } else {
        substrate = Field(String::new(), None);
    }
    let has_permission = stage.scan.has_permission;
    let mut permission = fields_of(stage).permission.clone();
    if !has_permission {
        let re_read = seamed_read(stage, "permission_mode", permission.clone());
        if !re_read.0.is_empty() {
            permission = re_read;
        }
    }
    // Python's prov: explicit -H, then the config field read, then ambient
    // inference - the cached resolved_harness, never the grid pick.
    let prov = resolved_harness(stage).unwrap_or_default();
    let substrate_unknown = !substrate.0.is_empty() && !SUBSTRATES.contains(&substrate.0.as_str());
    let substrate_ok = !prov.is_empty()
        && !substrate.0.is_empty()
        && crate::effort_surface::substrate_compatible(&substrate.0, &prov);
    let mut pane_tokens_ok = false;
    let mut thread_tokens_ok = false;
    if !permission.0.is_empty() && !prov.is_empty() && !has_permission {
        pane_tokens_ok = crate::codex_posture::permission_mappable(&prov, &permission.0, "pane")
            .unwrap_or(false);
        thread_tokens_ok =
            crate::codex_posture::permission_mappable(&prov, &permission.0, "thread")
                .unwrap_or(false);
    }
    if effort.0.is_empty() && substrate.0.is_empty() && permission.0.is_empty() {
        set_fields(stage, effort, substrate, permission);
        return;
    }
    let ask = json!({
        "effort": {"value": effort.0, "rung": effort.1.clone().unwrap_or_default()},
        "has_effort": stage.has_effort,
        "effort_reason": effort_reason,
        "substrate": {"value": substrate.0, "rung": substrate.1.clone().unwrap_or_default()},
        "explicit_substrate": explicit_substrate.clone().unwrap_or_default(),
        "substrate_unknown": substrate_unknown,
        "substrate_ok": substrate_ok,
        "substrate_valid_list": "pane, thread, headless, bg",
        "permission_mode": {"value": permission.0, "rung": permission.1.clone().unwrap_or_default()},
        "has_permission": has_permission,
        "pane_tokens_ok": pane_tokens_ok,
        "thread_tokens_ok": thread_tokens_ok,
        "prov": prov,
    });
    let answer = crate::spawn_axes::decide(&ask);
    apply_axes_answer(seam, &answer);
    stage.injected_substrate = answer
        .get("injected_substrate")
        .and_then(Value::as_str)
        .map(str::to_string);
    set_fields(stage, effort, substrate, permission);
}

/// The pane group and the harness bundle: the verb's placement judgment,
/// then the axes answer that lands the bundle behind the `--` fence at the
/// argv TAIL (a boundary the caller already typed displaces it).
fn pane_bundle_stage(stage: &mut Stage, seam: &mut Seam) {
    let pane_group = fields_of(stage).pane_group.clone();
    let bundle = stage
        .overlay_answer
        .as_ref()
        .and_then(|a| a.get("bundle"))
        .cloned()
        .filter(|b| b.is_object());
    if pane_group.0.is_empty() && bundle.is_none() {
        return;
    }
    let pg_rung = format!(
        "{}.pane_group",
        fields_of(stage).pane_group.1.clone().unwrap_or_default()
    );
    let mut pg_answer: Option<Value> = None;
    let mut pg_unavailable = String::new();
    if !pane_group.0.is_empty() && !stage.scan.tab_flag_present {
        let effective = stage
            .scan
            .explicit_substrate
            .clone()
            .or_else(|| stage.injected_substrate.clone())
            .unwrap_or_else(|| "pane".to_string());
        let ask = json!({
            "kind": "pane-group",
            "group": pane_group.0,
            "rung": pg_rung,
            "eff_substrate": effective,
            "argv_tail": stage.spawn_token_tail(),
        });
        match crate::spawn_overlay::resolve(ask) {
            Ok(answer) => pg_answer = Some(answer),
            Err(exc) => pg_unavailable = exc,
        }
    }
    let ask = json!({
        "pane_group": {"value": pane_group.0, "rung": fields_of(stage).pane_group.1.clone().unwrap_or_default()},
        "tab_flag_present": stage.scan.tab_flag_present,
        "pane_group_pg_rung": pg_rung,
        "pane_group_unavailable": pg_unavailable,
        "pane_group_answer": pg_answer,
        "bundle": bundle,
        "positional_present": stage.scan.positional_present,
    });
    let answer = crate::spawn_axes::decide(&ask);
    apply_axes_answer(seam, &answer);
    for pair in answer
        .get("bundle_inject")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(pair) = pair.as_array() {
            for token in pair {
                seam.bundle
                    .push(token.as_str().unwrap_or_default().to_string());
            }
        }
    }
    if answer
        .get("out_tail_append_empty")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        stage.out_tail_empty = true;
    }
}

/// The axes answer's mechanical application, shared by all three calls.
fn apply_axes_answer(seam: &mut Seam, answer: &Value) {
    for line in answer
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        seam.note(line.as_str().unwrap_or_default().to_string());
    }
    for pair in answer
        .get("inject")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(pair) = pair.as_array() {
            for token in pair {
                seam.inject
                    .push(token.as_str().unwrap_or_default().to_string());
            }
        }
    }
    if let Some(applied) = answer.get("applied").and_then(Value::as_array) {
        seam.applied.extend(applied.iter().cloned());
    }
    if let Some(suppressed) = answer.get("suppressed").and_then(Value::as_array) {
        seam.suppressed.extend(suppressed.iter().cloned());
    }
}

/// Assembly: the applied line, the head inject, the bundle tail, the vendor
/// check over the FINAL argv, and the journal row.
fn assemble(stage: &mut Stage, seam: &mut Seam) {
    if !seam.applied.is_empty() {
        let parts: Vec<String> = seam
            .applied
            .iter()
            .filter_map(|entry| {
                let entry = entry.as_array()?;
                let axis = entry.first()?.as_str()?;
                let value = entry.get(1)?.as_str()?;
                let source = entry.get(2)?.as_str()?;
                Some(format!("{axis}={value} ({source})"))
            })
            .collect();
        seam.note(format!("fno agents spawn: applied {}", parts.join(", ")));
    }
    let mut argv = seam.argv.clone().unwrap_or_default();
    if !seam.inject.is_empty() {
        let mut next = vec![argv.first().cloned().unwrap_or_else(|| "spawn".to_string())];
        next.extend(seam.inject.iter().cloned());
        next.extend(argv.into_iter().skip(1));
        argv = next;
        seam.injected = true;
    }
    if !seam.bundle.is_empty() {
        argv.extend(seam.bundle.iter().cloned());
        seam.injected = true;
    }
    seam.argv = Some(argv);
    let model_source = seam
        .applied
        .iter()
        .filter_map(|entry| {
            let entry = entry.as_array()?;
            if entry.first()?.as_str()? == "model" {
                entry.get(2)?.as_str().map(str::to_string)
            } else {
                None
            }
        })
        .next();
    vendor_check(stage, seam, model_source.as_deref());
    if seam.exit.is_some() {
        return;
    }
    journal(stage, seam, &stage.fingerprint);
}

/// The spawn_defaults_applied receipt, through the journal op (the WRITE
/// belongs to the verb; the caller passes the resolved path). Never fails
/// the spawn.
fn journal(stage: &Stage, seam: &Seam, fingerprint: &str) {
    let path = match non_empty_env("FNO_EVENTS_PATH") {
        Some(path) => std::path::PathBuf::from(path),
        None => match crate::agents_config::state_dir(std::path::Path::new(".")) {
            Some(dir) => dir.join("events.jsonl"),
            None => return,
        },
    };
    let axes = [
        ("provider", stage.fields().harness.clone()),
        ("model", stage.fields().model.clone()),
        ("effort", stage.fields().effort.clone()),
        ("substrate", stage.fields().substrate.clone()),
        ("permission_mode", stage.fields().permission.clone()),
        ("route", stage.fields().route.clone()),
        ("account", stage.fields().account.clone()),
        ("pane_group", stage.fields().pane_group.clone()),
    ];
    let mut resolved = Map::new();
    for (axis, field) in axes {
        resolved.insert(axis.to_string(), json!({"value": field.0, "rung": field.1}));
    }
    let ask = json!({
        "op": "journal",
        "path": path,
        "event": {
            "name": stage.scan.name,
            "verb": stage.profile_verb,
            "seed": stage.scan.seed,
            "fingerprint": fingerprint,
            "resolved": resolved,
            "applied": seam.applied
                .iter()
                .map(|e| e.as_array().cloned().unwrap_or_default())
                .collect::<Vec<_>>(),
            "suppressed": seam.suppressed
                .iter()
                .map(|e| e.as_array().cloned().unwrap_or_default())
                .collect::<Vec<_>>(),
        },
    });
    let _ = crate::route_slot::run_route_slot_journal(&ask);
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Store the re-read fields back on the stage for the journal.
fn set_fields(stage: &mut Stage, effort: Field, substrate: Field, permission: Field) {
    if let Some(fields) = stage.fields.as_mut() {
        fields.effort = effort;
        fields.substrate = substrate;
        fields.permission = permission;
    }
}

/// One spawn-defaults block as the overlay verb's JSON view (Python
/// `_overlay_payload`): the typed dump carries every scalar plus the harness
/// table.
fn overlay_payload(table: &toml::Table) -> Value {
    let mut out = Map::new();
    for key in [
        "provider",
        "model",
        "effort",
        "substrate",
        "permission_mode",
        "route",
        "account",
        "pane_group",
    ] {
        out.insert(key.to_string(), json!(cfg_str(Some(table), key)));
    }
    if let Some(harness) = table.get("harness").and_then(toml::Value::as_table) {
        let mut blocks = Map::new();
        for (name, block) in harness {
            if let Some(b) = block.as_table() {
                let mut view = Map::new();
                // Python sent harness blocks verbatim (`dict(block)`), so
                // only PRESENT keys ride: an empty placeholder here would
                // read as a declared lane field and the overlay guard
                // refuses its own synthetic key.
                for key in [
                    "provider",
                    "model",
                    "effort",
                    "substrate",
                    "permission_mode",
                    "route",
                    "account",
                    "pane_group",
                ] {
                    let value = cfg_str(Some(b), key);
                    if !value.is_empty() {
                        view.insert(key.to_string(), json!(value));
                    }
                }
                if let Some(args) = b.get("args").and_then(toml::Value::as_array) {
                    view.insert(
                        "args".to_string(),
                        Value::Array(
                            args.iter()
                                .map(|a| json!(a.as_str().unwrap_or_default()))
                                .collect(),
                        ),
                    );
                }
                blocks.insert(name.clone(), Value::Object(view));
            }
        }
        out.insert("harness".to_string(), Value::Object(blocks));
    }
    Value::Object(out)
}

/// The harness-keyed rungs: one round trip answers effort/substrate/
/// permission plus the ONE bundle and refuses a bad overlay. Gated on an
/// overlay table (or lane args) being present, so an overlay-free spawn
/// never runs it and the harness-blind field() reads answer exactly.
fn overlay_stage(stage: &mut Stage, seam: &mut Seam) {
    if !overlays_present(stage) {
        return;
    }
    let lane_args = stage
        .lane
        .as_ref()
        .and_then(|lane| lane.get("args"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|a: &Vec<String>| !a.is_empty());
    let harness = resolved_harness(stage).unwrap_or_default();
    let ask = json!({
        "kind": "overlay",
        "verb": stage.profile_verb,
        "harness": harness,
        "defaults": overlay_payload(&stage.defaults),
        "profile": stage.profile.map(overlay_payload),
        "lane": lane_args.as_ref().map(|args| json!({ "args": args })),
        "lane_index": stage.lane_index,
        "argv_tail": stage.argv_tail(),
    });
    match crate::spawn_overlay::resolve(ask) {
        Ok(answer) => {
            if let Some(refusal) = answer.get("refusal").and_then(Value::as_str) {
                if !refusal.is_empty() {
                    seam.refuse(refusal.to_string());
                    return;
                }
            }
            stage.overlay_answer = Some(answer);
        }
        Err(exc) => {
            seam.note(format!(
                "fno agents spawn: harness-keyed defaults skipped ({exc})"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Saved = Vec<(&'static str, Option<std::ffi::OsString>)>;

    /// The per-process fake world the CLAIMS root pins to, for the whole
    /// run. The claims pin is SET-FOREVER and the dir is never deleted:
    /// restoring it reopens live-$HOME claims reads (28 CI failures), and
    /// a deleted dir starves later readers of a readable-empty world (6).
    /// The STATE pins are NOT set-forever: a persisting empty state world
    /// shadows the tests that pin their own (5 CI failures), so those
    /// snapshot-restore like the config path.
    fn fake_root() -> &'static std::path::Path {
        static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let root = ROOT.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("fno-fake-world-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            dir
        });
        root.as_path()
    }

    fn hermetic() -> (
        std::sync::MutexGuard<'static, ()>,
        &'static std::path::Path,
        Saved,
    ) {
        let guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = fake_root();
        // Snapshot the config and state pins for restore; only the claims
        // root is set-forever (see fake_root). FNO_TEST_HERMETIC is never
        // touched, so CI's declared ambient ("0") survives the process.
        let saved: Saved = vec![
            ("FNO_CONFIG", std::env::var_os("FNO_CONFIG")),
            ("FNO_STATE_DIR", std::env::var_os("FNO_STATE_DIR")),
            ("FNO_AGENTS_HOME", std::env::var_os("FNO_AGENTS_HOME")),
        ];
        // The consult arm reads the declared rows, the policy and the lanes
        // from DISK (the gather's own read); pin an empty config or the
        // test process's ambient config answers the walk.
        std::fs::write(root.join("config.toml"), "").unwrap();
        std::env::set_var("FNO_CONFIG", root.join("config.toml"));
        std::env::set_var("FNO_CLAIMS_ROOT", root);
        std::env::set_var("FNO_STATE_DIR", root.join("state"));
        std::env::set_var("FNO_AGENTS_HOME", root.join("agents-home"));
        (guard, root, saved)
    }

    fn clear_hermetic(_root: &std::path::Path, saved: Saved) {
        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    /// The role gate: a config model, a free axis and a role with no
    /// `facts.role_resolves` answer stops before any side effect and asks
    /// the transport to resolve. Python's seam resolved the role only
    /// under this same gate, so a bare role spawn's single cmd_spawn
    /// resolve is the one the role-wiring contract pins.
    #[test]
    fn a_missing_role_answer_gates_the_composition() {
        let (guard, root, saved) = hermetic();
        let inputs = Inputs {
            argv: vec![
                "spawn".to_string(),
                "--name".to_string(),
                "w".to_string(),
                "--role".to_string(),
                "tidy".to_string(),
                "work".to_string(),
            ],
            defaults: toml::Value::Table(toml::map::Map::new()),
            profiles: "[target]\nmodel = \"cfg-m\"\n".parse().unwrap(),
            dispatch_verbs: toml::Value::Table(toml::map::Map::new()),
            roster: vec!["target".to_string()],
            node_verb: None,
            env_node: None,
            ambient_harness: "claude".to_string(),
            apply_permission_builtin: true,
            scan: serde_json::json!({
                "has_harness": false, "explicit_harness": null,
                "has_model": false, "has_effort": false,
                "explicit_route": false, "role": "tidy",
                "has_permission": false, "seed": "/fno:target work",
                "name": "w", "positional_present": true
            }),
            facts: serde_json::json!({"role_resolves": null}),
            node: None,
            node_row: None,
        };
        let answer = compose(&inputs);
        drop(guard);
        clear_hermetic(&root, saved);
        assert!(answer.role_gate_needed, "the gate declares the need");
        assert_eq!(answer.exit, 0, "no refusal");
        assert!(answer.stderr.is_empty(), "no lines printed");
        assert!(!answer.injected, "nothing applied");
        assert_eq!(answer.argv, inputs.argv, "argv unchanged");
    }

    /// A carried answer composes straight through: false composes with the
    /// config model injecting (the billing ask receives it), true skips
    /// the model injection. Either way no gate fires.
    #[test]
    fn a_carried_role_answer_composes_through() {
        let (guard, root, saved) = hermetic();
        let inputs = Inputs {
            argv: vec![
                "spawn".to_string(),
                "--name".to_string(),
                "w".to_string(),
                "--role".to_string(),
                "tidy".to_string(),
                "work".to_string(),
            ],
            defaults: toml::Value::Table(toml::map::Map::new()),
            profiles: "[target]\nmodel = \"cfg-m\"\n".parse().unwrap(),
            dispatch_verbs: toml::Value::Table(toml::map::Map::new()),
            roster: vec!["target".to_string()],
            node_verb: None,
            env_node: None,
            ambient_harness: "claude".to_string(),
            apply_permission_builtin: true,
            scan: serde_json::json!({
                "has_harness": false, "explicit_harness": null,
                "has_model": false, "has_effort": false,
                "explicit_route": false, "role": "tidy",
                "has_permission": false, "seed": "/fno:target work",
                "name": "w", "positional_present": true
            }),
            facts: serde_json::json!({"role_resolves": false}),
            node: None,
            node_row: None,
        };
        let answer = compose(&inputs);
        drop(guard);
        clear_hermetic(&root, saved);
        assert!(!answer.role_gate_needed, "the carried answer composes");
        assert!(
            answer.argv.iter().any(|t| t == "--model"),
            "an unresolved role lets the config model inject"
        );
    }
}
