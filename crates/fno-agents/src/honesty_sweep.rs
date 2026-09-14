//! `fno-agents honesty-sweep` -- find fields that were declared rather than
//! measured, across any declared population.
//!
//! The sweep has produced two defect classes, and they do not share a shape,
//! which is why one pass is not enough:
//!
//! 1. UNIFORM. `stop_hook` read "native" on every row, and
//!    `send_keys_enter_delay_ms` read 0 on four of five, before that. A field
//!    holding one value across every harness has not been measured for any of
//!    them. It was declared once and inherited since.
//!
//! 2. FALSE NEGATIVE. codex declared `interactive_attach.kind = "unsupported"`
//!    while a working codex attach shipped hardcoded in Rust. That value is not
//!    uniform, it parses perfectly, and it is wrong. A uniformity sweep cannot
//!    see it. What it looks like instead is a NEGATIVE claim in the table
//!    beside a harness-NAMED implementation in the source, which is also the
//!    shape the capability-mirror law forbids.
//!
//! A finding is a candidate, not a verdict: pair a negative claim with the
//! identifier that contradicts it, then go read both.
//!
//! Pass 1 splits in two. `uniform` keeps today's rule (an absent field counts
//! as `<absent>`), so the harness-table guard keeps its meaning.
//! `uniform_among_declarers` is the rule that surfaces the rest: older
//! manifests predate a field, so absence would hide an unbilled counter
//! (`respawn_count` read 0 on every manifest that declared it while nothing
//! swept the rows that lacked the field). A field whose one value is correct
//! by construction (`harness: claude` on a one-harness machine) prints as a
//! candidate; the sweep never calls it a defect.
//!
//! Client-side and daemon-free, like [`crate::prove_it_verdicts`]: a sweep
//! reads declared populations from disk; it is not an agent-lifecycle
//! operation. Ported from the deleted Python diagnostic of the same name
//! (law d-b6cc1a2a: existing Python is ported, never extended).

use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::evals_macro::canonical_json as canon;

/// A value asserting that a capability is ABSENT. Each one is a claim that can
/// be contradicted by an implementation somewhere else in the tree.
const NEGATIVE: [&str; 8] = [
    "\"unsupported\"",
    "\"refused\"",
    "\"none\"",
    "\"\"",
    "[]",
    "0",
    "false",
    "{}",
];
const SOURCE_DIRS: [&str; 2] = ["cli/src", "crates"];
const CANON: &str = "crates/fno-agents/src/harness_capabilities.toml";

/// Segments that name the SHAPE of a field rather than the capability it is
/// about. Pairing on these would match everything and find nothing.
const STOPWORDS: [&str; 35] = [
    "kind",
    "tokens",
    "supported",
    "keys",
    "rule",
    "ids",
    "rule_ids",
    "strategy",
    "forms",
    "ms",
    "required",
    "timeout",
    "timeout_ms",
    "status",
    "labels",
    "response",
    "binding",
    "marker",
    "grant",
    "root",
    "state",
    "on",
    "prefix",
    "command",
    "pattern",
    "effort",
    "effort_labels",
    "allow",
    "deny",
    "once",
    "always",
    "send",
    "enter",
    "delay",
    "keys_enter_delay_ms",
];

/// A field allowed to hold one value on every row, with the reason it is a
/// measured result rather than an inherited default. Empty today, and adding
/// to it is the moment to prove the uniformity rather than assume it.
#[cfg(test)]
const UNIFORM_BY_MEASUREMENT: &[(&str, &str)] = &[];

/// One row of a population: an id plus flattened `field path -> canonical
/// JSON text` (leaf values compact, object keys sorted).
struct Row {
    id: String,
    fields: BTreeMap<String, String>,
}

enum PopSpec {
    HarnessCapabilities,
    KingManifests,
    RowsJson {
        path: String,
        key: Option<String>,
        name: String,
    },
}

const USAGE: &str = "usage: fno-agents honesty-sweep [--json] \
[--population harness-capabilities|king-manifests] \
[--rows-json <path|->] [--rows-key <key>] [--name <label>]
populations repeat; each named population is swept in turn. \
Exit 0 measured (a finding is a candidate, never a verdict), 2 unmeasured.";

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, sub) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, sub, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), canon(other));
        }
    }
}

fn git_root() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

/// The capability table's rows, parsed from TOML text. Shared with the
/// compiled-in-table guard so it needs no repo on disk.
fn rows_from_toml(text: &str) -> Result<Vec<Row>, String> {
    let table: toml::Value =
        toml::from_str(text).map_err(|e| format!("cannot parse {CANON}: {e}"))?;
    table_to_rows(&table)
}

/// The capability table's rows, read from disk the way the ported script did.
fn read_harness_rows(root: &Path) -> Result<Vec<Row>, String> {
    let path = root.join(CANON);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    rows_from_toml(&text)
}

fn table_to_rows(table: &toml::Value) -> Result<Vec<Row>, String> {
    let rows = table
        .get("harness")
        .and_then(|h| h.as_table())
        .ok_or_else(|| format!("{CANON} has no [harness] table"))?;
    let mut out = Vec::new();
    for (name, row) in rows {
        let value =
            serde_json::to_value(row).map_err(|e| format!("cannot convert harness {name}: {e}"))?;
        let mut fields = BTreeMap::new();
        flatten("", &value, &mut fields);
        out.push(Row {
            id: name.clone(),
            fields,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// One scalar `key: value` line of a king manifest's frontmatter. Values that
/// parse as JSON keep that type (`respawn_count: 0` becomes `0`); anything
/// else becomes a JSON string, and an empty value becomes `""`.
fn manifest_field(line: &str) -> Option<(String, String)> {
    let (key, raw) = line.split_once(':')?;
    let key = key.trim();
    if key.is_empty() || key.contains(' ') {
        return None;
    }
    let raw = raw.trim();
    let value = if raw.is_empty() {
        "\"\"".to_string()
    } else {
        match serde_json::from_str::<Value>(raw) {
            Ok(v) => canon(&v),
            Err(_) => Value::String(raw.to_string()).to_string(),
        }
    };
    Some((key.to_string(), value))
}

fn read_king_rows() -> Result<Vec<Row>, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let kings = crate::paths::space_dir(&cwd).join("kings");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&kings)
        .map_err(|e| format!("cannot read kings dir {}: {e}", kings.display()))?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut fields = BTreeMap::new();
        let mut inside = false;
        let mut fences = 0usize;
        for line in text.lines() {
            if line.trim() == "---" {
                fences += 1;
                if fences == 2 {
                    break;
                }
                inside = true;
                continue;
            }
            if inside {
                if let Some((key, value)) = manifest_field(line) {
                    fields.insert(key, value);
                }
            }
        }
        if fences < 2 {
            return Err(format!("{} has no --- frontmatter fences", path.display()));
        }
        out.push(Row { id, fields });
    }
    Ok(out)
}

fn read_rows_json(spec: &PopSpec) -> Result<Vec<Row>, String> {
    let PopSpec::RowsJson { path, key, .. } = spec else {
        return Ok(Vec::new());
    };
    let text = if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?
    };
    let parsed: Value =
        serde_json::from_str(&text).map_err(|e| format!("invalid JSON from {path}: {e}"))?;
    let array = match &parsed {
        Value::Array(items) => items.clone(),
        Value::Object(_) => {
            let key = key
                .as_deref()
                .ok_or_else(|| "--rows-key is required when the input is an object".to_string())?;
            parsed
                .get(key)
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("no array under key {key:?} in {path}"))?
                .clone()
        }
        _ => {
            return Err(format!(
                "input from {path} is neither an array nor an object"
            ))
        }
    };
    let mut out = Vec::new();
    for (index, item) in array.iter().enumerate() {
        let obj: Map<String, Value> = match item {
            Value::Object(map) => map.clone(),
            _ => return Err(format!("row {index} in {path} is not an object")),
        };
        let mut fields = BTreeMap::new();
        let value = Value::Object(obj.clone());
        flatten("", &value, &mut fields);
        let id = obj
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| index.to_string());
        out.push(Row { id, fields });
    }
    Ok(out)
}

/// `cli/src` and `crates` text files, sorted. Skips any path containing
/// `/tests/`, any file named `test_*`, and ONLY `crates/<crate>/target`:
/// never a bare `target` name, because `cli/src/fno/target` is source.
fn load_sources(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in SOURCE_DIRS {
        let base = root.join(dir);
        let mut stack = vec![base.clone()];
        while let Some(current) = stack.pop() {
            let entries = match std::fs::read_dir(&current) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                let name = entry.file_name().to_string_lossy().to_string();
                if meta.is_dir() {
                    if name == "target" && dir == "crates" {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                let extension = path.extension().and_then(|e| e.to_str());
                if extension != Some("rs") && extension != Some("py") {
                    continue;
                }
                if name.starts_with("test_") {
                    continue;
                }
                let relative = match path.strip_prefix(root) {
                    Ok(rel) => rel.to_string_lossy().to_string(),
                    Err(_) => continue,
                };
                if relative.contains("/tests/") {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push((relative, text));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The capability words in a field path, minus the shape words.
fn keywords(field: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for segment in field.split('.') {
        for word in segment.split('_') {
            if word.len() > 3 && !STOPWORDS.contains(&word) && !words.iter().any(|w| w == word) {
                words.push(word.to_string());
            }
        }
    }
    words
}

/// Definitions named after BOTH the harness and one of its capability words.
/// Only EXPORTED definitions. A private helper is an implementation detail and
/// a `#[cfg(test)]` function is a fixture; neither is the capability shipping
/// behind the table's back, and matching them buries the ones that are.
fn named_hits(sources: &[(String, String)], harness: &str, words: &[String]) -> Vec<String> {
    let alternation = words
        .iter()
        .map(|w| regex::escape(w))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = Regex::new(&format!(
        r"(?i)\b(?:pub(?:\([a-z]+\))?\s+(?:fn|const|static)|def)\s+\w*{}\w*(?:{})\w*",
        regex::escape(harness),
        alternation
    ))
    .expect("named-hits regex");
    let mut hits = Vec::new();
    for (relative, text) in sources {
        for (lineno, line) in text.lines().enumerate() {
            if pattern.is_match(line) {
                hits.push(format!(
                    "{}:{}: {}",
                    relative,
                    lineno + 1,
                    line.trim().chars().take(100).collect::<String>()
                ));
            }
        }
    }
    hits
}

fn name_lists(sources: &[(String, String)], harnesses: &[String]) -> Vec<String> {
    let names = harnesses
        .iter()
        .map(|h| regex::escape(h))
        .collect::<Vec<_>>()
        .join("|");
    let literal = Regex::new(&format!(
        r#"[\[(]\s*\"(?:{})\"\s*,\s*\"(?:{})\"[^\]\)]*[\])]"#,
        names, names
    ))
    .expect("name-list regex");
    let mut out = Vec::new();
    for (relative, text) in sources {
        for (lineno, line) in text.lines().enumerate() {
            if literal.is_match(line) {
                out.push(format!(
                    "{}:{}: {}",
                    relative,
                    lineno + 1,
                    line.trim().chars().take(110).collect::<String>()
                ));
            }
        }
    }
    out
}

/// The four passes over one population. `sources` is `None` for populations
/// whose row ids are not harness names (passes 3 and 4 print as skipped).
struct Report {
    fields: usize,
    uniform: Vec<(String, String)>,
    declarers: Vec<(String, String, usize)>,
    negatives: BTreeMap<String, usize>,
    named_pairs: Vec<Value>,
    name_lists: Vec<String>,
    pass34_skipped: bool,
}

fn sweep(rows: &[Row], sources: Option<&[(String, String)]>) -> Report {
    let mut fields: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        for field in row.fields.keys() {
            fields.insert(field.clone());
        }
    }
    // 1a. One value across every row, an absent field counting as <absent>.
    let uniform: Vec<(String, String)> = fields
        .iter()
        .filter(|field| {
            let mut values = BTreeSet::new();
            for row in rows {
                values.insert(
                    row.fields
                        .get(*field)
                        .map(|s| s.as_str())
                        .unwrap_or("<absent>"),
                );
            }
            values.len() == 1
        })
        .map(|field| {
            let value = rows
                .iter()
                .find_map(|row| row.fields.get(field))
                .cloned()
                .unwrap_or_else(|| "<absent>".to_string());
            (field.clone(), value)
        })
        .collect();
    // 1b. Present on 2+ rows and absent on 1+, one distinct value among the
    // rows that have it. Older rows predate a field, so absence would hide an
    // unbilled counter.
    let declarers: Vec<(String, String, usize)> = fields
        .iter()
        .filter_map(|field| {
            let present: Vec<&Row> = rows
                .iter()
                .filter(|row| row.fields.contains_key(field))
                .collect();
            if present.len() < 2 || present.len() == rows.len() {
                return None;
            }
            let mut values = BTreeSet::new();
            for row in &present {
                values.insert(row.fields[field].as_str());
            }
            (values.len() == 1).then(|| {
                (
                    field.clone(),
                    present[0].fields[field].clone(),
                    present.len(),
                )
            })
        })
        .collect();
    let negatives: BTreeMap<String, usize> = rows
        .iter()
        .map(|row| {
            let count = row
                .fields
                .values()
                .filter(|v| NEGATIVE.contains(&v.as_str()))
                .count();
            (row.id.clone(), count)
        })
        .collect();
    let mut named_pairs = Vec::new();
    let mut name_list_hits = Vec::new();
    let mut pass34_skipped = false;
    match sources {
        Some(sources) => {
            for row in rows {
                for (field, value) in &row.fields {
                    if !NEGATIVE.contains(&value.as_str()) {
                        continue;
                    }
                    let words = keywords(field);
                    if words.is_empty() {
                        continue;
                    }
                    let hits = named_hits(sources, &row.id, &words);
                    if hits.is_empty() {
                        continue;
                    }
                    named_pairs.push(json!({
                        "row": row.id,
                        "field": field,
                        "value": value,
                        "hits": hits.into_iter().take(6).collect::<Vec<_>>(),
                    }));
                }
            }
            let names: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
            name_list_hits = name_lists(sources, &names);
        }
        None => pass34_skipped = true,
    }
    Report {
        fields: fields.len(),
        uniform,
        declarers,
        negatives,
        named_pairs,
        name_lists: name_list_hits,
        pass34_skipped,
    }
}

fn population_json(name: &str, rows: &[Row], report: &Report) -> Value {
    let mut object = json!({
        "name": name,
        "status": "measured",
        "rows": rows.len(),
        "fields": report.fields,
        "uniform": report
            .uniform
            .iter()
            .map(|(field, value)| json!({"field": field, "value": value}))
            .collect::<Vec<_>>(),
        "uniform_among_declarers": report
            .declarers
            .iter()
            .map(|(field, value, declared)| {
                json!({"field": field, "value": value, "declared": declared, "rows": rows.len()})
            })
            .collect::<Vec<_>>(),
        "negative_counts": report.negatives,
    });
    let object_mut = object.as_object_mut().expect("population object");
    if report.pass34_skipped {
        object_mut.insert(
            "pass34".to_string(),
            Value::String("skipped: row ids are not harness names".to_string()),
        );
    }
    object_mut.insert(
        "named_pairs".to_string(),
        Value::Array(report.named_pairs.clone()),
    );
    object_mut.insert(
        "name_lists".to_string(),
        Value::Array(
            report
                .name_lists
                .iter()
                .map(|h| Value::String(h.clone()))
                .collect(),
        ),
    );
    object
}

fn print_text_population(name: &str, rows: &[Row], report: &Report) {
    println!(
        "population: {name}  rows={} fields={}",
        rows.len(),
        report.fields
    );
    println!();
    println!("=== 1. uniform fields (one value across every row) ===");
    if report.uniform.is_empty() {
        println!("  none. Every field carries at least two distinct values.");
    } else {
        for (field, value) in &report.uniform {
            println!("  UNIFORM  {field} = {value}");
        }
        println!();
        println!("  One value on every row is a declaration inherited, not a");
        println!("  measurement taken. Measure each row or prove the uniformity.");
    }
    println!();
    println!("=== 1b. uniform among declarers (present on 2+, absent on 1+) ===");
    if report.declarers.is_empty() {
        println!("  none.");
    } else {
        for (field, value, declared) in &report.declarers {
            println!(
                "  UNIFORM-DECLARERS  {field} = {value} (declared {declared} of {} rows)",
                rows.len()
            );
        }
    }
    println!();
    println!("=== 2. negative claims (a capability declared ABSENT) ===");
    for (row, count) in &report.negatives {
        println!("  {row}: {count}");
    }
    println!();
    println!("=== 3. a negative claim beside a harness-NAMED implementation ===");
    if report.pass34_skipped {
        println!("  skipped: row ids are not harness names");
    } else if report.named_pairs.is_empty() {
        println!("  none.");
    } else {
        for pair in &report.named_pairs {
            println!(
                "\n  {}.{} = {}  (declared absent)",
                pair["row"].as_str().unwrap_or(""),
                pair["field"].as_str().unwrap_or(""),
                pair["value"].as_str().unwrap_or("")
            );
            if let Some(hits) = pair["hits"].as_array() {
                for hit in hits {
                    println!("      {}", hit.as_str().unwrap_or(""));
                }
            }
        }
    }
    println!();
    println!("=== 4. hardcoded harness lists (a name standing in for a capability) ===");
    if report.pass34_skipped {
        println!("  skipped: row ids are not harness names");
    } else if report.name_lists.is_empty() {
        println!("  none.");
    } else {
        for hit in &report.name_lists {
            println!("  {hit}");
        }
    }
    println!();
}

pub fn run_honesty_sweep(args: &[String]) -> i32 {
    let mut json_output = false;
    let mut specs: Vec<PopSpec> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json_output = true,
            "--population" => {
                i += 1;
                match args.get(i).map(|s| s.as_str()) {
                    Some("harness-capabilities") => specs.push(PopSpec::HarnessCapabilities),
                    Some("king-manifests") => specs.push(PopSpec::KingManifests),
                    Some(other) => {
                        eprintln!("fno-agents honesty-sweep: unknown population {other:?}");
                        eprintln!("{USAGE}");
                        return 2;
                    }
                    None => {
                        eprintln!("fno-agents honesty-sweep: --population needs a name");
                        eprintln!("{USAGE}");
                        return 2;
                    }
                }
            }
            "--rows-json" => {
                i += 1;
                match args.get(i) {
                    Some(path) => specs.push(PopSpec::RowsJson {
                        path: path.clone(),
                        key: None,
                        name: "rows-json".to_string(),
                    }),
                    None => {
                        eprintln!("fno-agents honesty-sweep: --rows-json needs a path or -");
                        eprintln!("{USAGE}");
                        return 2;
                    }
                }
            }
            "--rows-key" | "--name" => {
                let flag = args[i].as_str();
                i += 1;
                let value = match args.get(i) {
                    Some(value) => value.clone(),
                    None => {
                        eprintln!("fno-agents honesty-sweep: {flag} needs a value");
                        eprintln!("{USAGE}");
                        return 2;
                    }
                };
                let Some(PopSpec::RowsJson { key, name, .. }) = specs.last_mut() else {
                    eprintln!("fno-agents honesty-sweep: {flag} must follow a --rows-json");
                    eprintln!("{USAGE}");
                    return 2;
                };
                if flag == "--rows-key" {
                    *key = Some(value);
                } else {
                    *name = value;
                }
            }
            other => {
                eprintln!("fno-agents honesty-sweep: unknown flag {other}");
                eprintln!("{USAGE}");
                return 2;
            }
        }
        i += 1;
    }
    if specs.is_empty() {
        specs.push(PopSpec::HarnessCapabilities);
    }

    let root = git_root();
    let sources = root.as_ref().map(|root| load_sources(root));
    let mut populations = Vec::new();
    let mut all_measured = true;
    for spec in &specs {
        let (name, read) = match spec {
            PopSpec::HarnessCapabilities => (
                "harness-capabilities",
                root.as_ref()
                    .ok_or_else(|| "no git root".to_string())
                    .and_then(|root| read_harness_rows(root)),
            ),
            PopSpec::KingManifests => ("king-manifests", read_king_rows()),
            PopSpec::RowsJson { name, .. } => (name.as_str(), read_rows_json(spec)),
        };
        let population = match read {
            Err(detail) => {
                all_measured = false;
                json!({"name": name, "status": "unmeasured", "detail": detail})
            }
            Ok(rows) if rows.len() < 2 => {
                all_measured = false;
                json!({
                    "name": name,
                    "status": "unmeasured",
                    "detail": format!("{} row(s); a sweep needs at least 2", rows.len()),
                })
            }
            Ok(rows) => {
                let sources_for = match spec {
                    PopSpec::HarnessCapabilities => sources.as_deref(),
                    _ => None,
                };
                let report = sweep(&rows, sources_for);
                if json_output {
                    population_json(name, &rows, &report)
                } else {
                    print_text_population(name, &rows, &report);
                    json!({"name": name, "status": "measured"})
                }
            }
        };
        populations.push(population);
    }
    if json_output {
        let status = if all_measured {
            "measured"
        } else {
            "unmeasured"
        };
        println!("{}", json!({"status": status, "populations": populations}));
    } else if !all_measured {
        for population in &populations {
            if population["status"] == "unmeasured" {
                eprintln!(
                    "unmeasured: {} ({})",
                    population["name"].as_str().unwrap_or(""),
                    population["detail"].as_str().unwrap_or("")
                );
            }
        }
    }
    if all_measured {
        0
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, fields: &[(&str, &str)]) -> Row {
        Row {
            id: id.to_string(),
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn table_guard_no_capability_field_is_uniform() {
        let rows =
            rows_from_toml(crate::harness_capabilities::CAPABILITY_TOML).expect("canon table");
        let report = sweep(&rows, None);
        assert_eq!(rows.len(), 8);
        assert!(report.fields > 30, "fields={}", report.fields);
        let uniform: Vec<_> = report
            .uniform
            .iter()
            .filter(|(field, _)| !UNIFORM_BY_MEASUREMENT.iter().any(|(f, _)| f == field))
            .collect();
        assert!(uniform.is_empty(), "uniform: {uniform:?}");
    }

    #[test]
    fn uniform_among_declarers_surfaces_the_absent_row() {
        let rows = vec![
            row("a", &[("respawn_count", "0"), ("kind", "\"x\"")]),
            row("b", &[("respawn_count", "0"), ("kind", "\"y\"")]),
            row("c", &[("kind", "\"z\"")]),
        ];
        let report = sweep(&rows, None);
        assert!(report.uniform.is_empty(), "uniform: {:?}", report.uniform);
        let hit = report
            .declarers
            .iter()
            .find(|(field, _, _)| field == "respawn_count")
            .expect("respawn_count in declarers");
        assert_eq!(hit.1, "0");
        assert_eq!(hit.2, 2);
    }

    #[test]
    fn declarers_rule_ignores_fields_every_row_declares() {
        let rows = vec![
            row("a", &[("budget_max_iterations", "40")]),
            row("b", &[("budget_max_iterations", "40")]),
        ];
        let report = sweep(&rows, None);
        assert!(
            report.declarers.is_empty(),
            "a field on every row is uniform, never a declarers hit"
        );
        assert_eq!(report.uniform.len(), 1);
    }

    #[test]
    fn named_pairs_match_exported_definitions_only() {
        let sources = vec![(
            "crates/x/src/a.rs".to_string(),
            "pub fn codex_attach_argv() {}\nfn codex_attach_argv() {}\n".to_string(),
        )];
        let rows = vec![
            row(
                "codex",
                &[
                    ("interactive_attach.kind", "\"unsupported\""),
                    ("resume", "\"flag\""),
                ],
            ),
            row(
                "claude",
                &[
                    ("interactive_attach.kind", "\"keystrokes\""),
                    ("resume", "\"flag\""),
                ],
            ),
        ];
        let report = sweep(&rows, Some(&sources));
        assert_eq!(
            report.named_pairs.len(),
            1,
            "pairs: {:?}",
            report.named_pairs
        );
        let pair = &report.named_pairs[0];
        assert_eq!(pair["row"], "codex");
        assert_eq!(pair["hits"].as_array().unwrap().len(), 1);
        let hits = serde_json::to_string(&pair["hits"]).unwrap();
        assert!(hits.contains("pub fn codex_attach_argv"), "hits: {hits}");
        assert!(
            !hits.contains(":2: fn codex_attach_argv"),
            "private fn must not match: {hits}"
        );
    }

    #[test]
    fn named_pairs_ignore_private_definitions() {
        let sources = vec![(
            "crates/x/src/a.rs".to_string(),
            "fn codex_attach_argv() {}\n".to_string(),
        )];
        let rows = vec![
            row("codex", &[("interactive_attach.kind", "\"unsupported\"")]),
            row("claude", &[("interactive_attach.kind", "\"keystrokes\"")]),
        ];
        let report = sweep(&rows, Some(&sources));
        assert!(
            report.named_pairs.is_empty(),
            "a private fn is an implementation detail, not a finding"
        );
    }

    #[test]
    fn canon_sorts_object_keys() {
        let value: Value =
            serde_json::from_str(r#"{"b": 1, "a": {"z": true, "y": null}}"#).unwrap();
        assert_eq!(canon(&value), r#"{"a":{"y":null,"z":true},"b":1}"#);
    }

    #[test]
    fn manifest_field_types_survive() {
        assert_eq!(
            manifest_field("respawn_count: 0"),
            Some(("respawn_count".into(), "0".into()))
        );
        assert_eq!(
            manifest_field("name: x-abc"),
            Some(("name".into(), "\"x-abc\"".into()))
        );
        assert_eq!(
            manifest_field("empty:"),
            Some(("empty".into(), "\"\"".into()))
        );
        assert_eq!(manifest_field("not a field"), None);
    }
}
