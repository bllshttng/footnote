//! The x-84b2 worker-name vocabulary owner, ported from Python `naming.py`
//! (crown ruling on laws d-4b39ad4c / d-52ae01cb: the tree budget ports the
//! touched verb to crates/ rather than raising the allowance).
//!
//! One data file stays the contract: the tables are read from the crate-local
//! `naming-codes.yaml` at compile time (the Python copy is deleted; the
//! Python side reads the tables back through `name-codes`), so Python and
//! Rust cannot drift on the codes - only on behavior, which the ported tests pin.

use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::OnceLock;

/// The daemon's public agent-name contract.
pub const MAX_LEN: usize = 64;
/// Per-component cap for human-readable text.
pub const SLUG_CAP: usize = 30;
/// Dispatch-form slug cap (x-57fe): a worker name must stay readable in a
/// narrow terminal, so the human slug gives up more room to the model tag.
pub const DISPATCH_SLUG_CAP: usize = 12;

const CODES_YAML: &str = include_str!("naming-codes.yaml");

#[derive(Deserialize)]
struct RawCodes {
    sources: Vec<String>,
    verbs: Vec<String>,
    word_codes: std::collections::HashMap<String, String>,
    model_codes: std::collections::HashMap<String, String>,
    provenance: Vec<ProvenanceRow>,
}

#[derive(Deserialize)]
struct ProvenanceRow {
    site: String,
    source: String,
    verb: String,
}

pub struct Codes {
    pub sources: HashSet<String>,
    pub verbs: HashSet<String>,
    pub word_codes: std::collections::HashMap<String, String>,
    pub model_codes: std::collections::HashMap<String, String>,
    pub provenance: Vec<(String, String, String)>,
}

fn codes() -> &'static Codes {
    static CODES: OnceLock<Codes> = OnceLock::new();
    CODES.get_or_init(|| {
        let raw: RawCodes = serde_yaml_ng::from_str(CODES_YAML).expect("naming-codes.yaml parses");
        Codes {
            sources: raw.sources.into_iter().collect(),
            verbs: raw.verbs.into_iter().collect(),
            word_codes: raw.word_codes,
            model_codes: raw.model_codes,
            provenance: raw
                .provenance
                .into_iter()
                .map(|r| (r.site, r.source, r.verb))
                .collect(),
        }
    })
}

pub fn dispatch_sources() -> &'static HashSet<String> {
    &codes().sources
}

pub fn dispatch_verbs() -> &'static HashSet<String> {
    &codes().verbs
}

pub fn provenance_rows() -> &'static [(String, String, String)] {
    &codes().provenance
}

/// The vocabulary error: `exit` is 3 for a naming refusal, 2 for bridge usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError {
    pub message: String,
    pub exit: i32,
}

impl NameError {
    fn refused(message: impl Into<String>) -> Self {
        NameError {
            message: message.into(),
            exit: 3,
        }
    }
    fn usage(message: impl Into<String>) -> Self {
        NameError {
            message: message.into(),
            exit: 2,
        }
    }
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Normalize free text to a name-safe tail, byte-for-byte with the shell.
pub fn slug_component(raw: Option<&str>, cap: usize) -> String {
    let Some(raw) = raw else { return String::new() };
    if raw.is_empty() {
        return String::new();
    }
    let lowered: String = raw
        .chars()
        .map(|c| {
            if c == '_' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let mut s = String::with_capacity(lowered.len());
    let mut prev_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            let dash = c == '-';
            if dash && (s.is_empty() || prev_dash) {
                continue;
            }
            s.push(c);
            prev_dash = dash;
        } else {
            if !s.is_empty() && !prev_dash {
                s.push('-');
                prev_dash = true;
            }
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    s.truncate(cap);
    while s.ends_with('-') {
        s.pop();
    }
    s
}

/// The short per-model name tag (x-57fe): a known key in `model_codes`
/// matched longest-first as a substring of the lowered model, else the first
/// 8 alphanumeric characters. `None` for a blank model.
pub fn model_code_for(model: Option<&str>) -> Option<String> {
    let m = model?.trim();
    if m.is_empty() {
        return None;
    }
    let lowered = m.to_lowercase();
    let mut keys: Vec<(&String, &String)> = codes().model_codes.iter().collect();
    keys.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(b.0)));
    for (k, v) in keys {
        if lowered.contains(k.as_str()) {
            return Some(v.clone());
        }
    }
    let squeezed: String = lowered
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    let mut tag: String = squeezed.chars().take(8).collect();
    if tag.is_empty() {
        tag.push('m');
    }
    Some(tag)
}

/// A node-shaped identity (`x-4129`) emits its bare hex: every row in a
/// dispatch shares the prefix, so the name spends those bytes on the model
/// tag instead (x-57fe).
fn node_hex(identity: &str) -> Option<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9]*-([0-9a-f]+)$").unwrap());
    re.captures(identity)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// Truncate a sanitized slug at the last hyphen at or under `cap`; a
/// hyphenless slug truncates hard.
fn cut_slug(raw: Option<&str>, cap: usize) -> String {
    let full = slug_component(raw, usize::MAX);
    if full.chars().count() <= cap {
        return full;
    }
    let taken: String = full.chars().take(cap).collect();
    match taken.rfind('-') {
        Some(i) if i > 0 => taken[..i].trim_end_matches('-').to_string(),
        _ => taken.trim_end_matches('-').to_string(),
    }
}

/// Build `<prefix>-<node_hex>[-<qualifier>][-<slug>][-<model>][-<discriminator>]`.
/// Required identity never shaves; only the human slug gives way.
pub fn agent_name(
    prefix: &str,
    node_id: &str,
    slug: Option<&str>,
    qualifier: Option<&str>,
    discriminator: Option<&str>,
    model: Option<&str>,
) -> Result<String, NameError> {
    let prefix = prefix.trim();
    let node_id = node_id.trim();
    let qualifier = qualifier.unwrap_or("").trim();
    let model_code = model_code_for(model).unwrap_or_default();
    let disc = slug_component(discriminator, SLUG_CAP);

    let node_disp = node_hex(node_id).unwrap_or(node_id);
    let required_parts: Vec<&str> = [
        prefix,
        node_disp,
        qualifier,
        model_code.as_str(),
        disc.as_str(),
    ]
    .iter()
    .filter(|p| !p.is_empty())
    .copied()
    .collect();
    if required_parts.is_empty() {
        return Err(NameError::refused(
            "agent name needs at least a prefix or a node id",
        ));
    }
    let required = required_parts.join("-");
    if required.len() > MAX_LEN {
        // Single-quote the components, never Debug's double quotes: the shell
        // dispatchers relay this message into a `reason="..."` field whose
        // grammar a double quote would break.
        let mut detail = format!(
            "required agent-name identity is {} chars, over the {}-char runtime limit: prefix='{}' node='{}'",
            required.len(), MAX_LEN, prefix, node_id
        );
        if !qualifier.is_empty() {
            detail.push_str(&format!(" qualifier='{}'", qualifier));
        }
        if !model_code.is_empty() {
            detail.push_str(&format!(" model='{}'", model_code));
        }
        if !disc.is_empty() {
            detail.push_str(&format!(" discriminator='{}'", disc));
        }
        return Err(NameError::refused(detail));
    }

    let mut human = cut_slug(slug, SLUG_CAP);
    if !human.is_empty() {
        let avail = MAX_LEN.saturating_sub(required.len() + 1);
        human = human.chars().take(avail).collect();
        while human.ends_with('-') {
            human.pop();
        }
    }

    let name = [
        prefix,
        node_disp,
        qualifier,
        human.as_str(),
        model_code.as_str(),
        disc.as_str(),
    ]
    .iter()
    .filter(|p| !p.is_empty())
    .copied()
    .collect::<Vec<&str>>()
    .join("-");
    let ok = name.len() <= MAX_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        return Err(NameError::refused(format!(
            "generated agent name '{}' violates the runtime contract [A-Za-z0-9_-]{{1,{}}} (node='{}')",
            name, MAX_LEN, node_id
        )));
    }
    Ok(name)
}

/// The verb code for a work-verb word (`/target`, `$fno:blueprint`, ...).
/// Unknown words raise: nothing defaults to `t`.
pub fn verb_code_for(word: Option<&str>) -> Result<String, NameError> {
    let mut v = word.unwrap_or("").trim();
    if let Some(rest) = v.strip_prefix("/fno:") {
        v = rest;
    } else if let Some(rest) = v.strip_prefix("$fno:") {
        v = rest;
    }
    let v = v.trim_start_matches('/');
    let v = if v.is_empty() { "target" } else { v };
    codes().word_codes.get(v).cloned().ok_or_else(|| {
        NameError::refused(format!("unknown dispatch verb '{}'", word.unwrap_or("")))
    })
}

/// Build `[<source>-]<verb>-<hex>[-<qualifier>][-<slug>][-<model>][-<disc>]`.
/// `source` None is the attended manual form; unknown codes raise rather than
/// fabricating provenance. The dispatch form caps the human slug at
/// [`DISPATCH_SLUG_CAP`] and carries the model tag (x-57fe).
pub fn dispatch_agent_name(
    source: Option<&str>,
    verb: &str,
    identity: &str,
    slug: Option<&str>,
    qualifier: Option<&str>,
    discriminator: Option<&str>,
    model: Option<&str>,
) -> Result<String, NameError> {
    let v = verb.trim();
    if !dispatch_verbs().contains(v) {
        return Err(NameError::refused(format!(
            "unknown dispatch verb '{}'",
            verb
        )));
    }
    if let Some(s) = source {
        let s = s.trim();
        if !dispatch_sources().contains(s) {
            return Err(NameError::refused(format!(
                "unknown dispatch source '{}'",
                source.unwrap_or("")
            )));
        }
    }
    let prefix = match source {
        None => v.to_string(),
        Some(s) => format!("{}-{}", s.trim(), v),
    };
    let slug = cut_slug(slug, DISPATCH_SLUG_CAP);
    agent_name(
        &prefix,
        identity,
        Some(&slug),
        qualifier,
        discriminator,
        model,
    )
}

/// The `fno agents name` assembly: `--verb`/`--source` select the dispatch
/// form; a positional prefix alone is the legacy form. `prefix`/`node_id` are
/// the CLI positionals AFTER the one-positional-binds-the-node rule.
pub fn bridge_name(
    prefix: Option<&str>,
    node_id: &str,
    slug: Option<&str>,
    qualifier: Option<&str>,
    discriminator: Option<&str>,
    source: Option<&str>,
    verb: Option<&str>,
    model: Option<&str>,
) -> Result<String, NameError> {
    if verb.is_some() || source.is_some() {
        let p = prefix.unwrap_or("");
        if !p.is_empty() {
            return Err(NameError::usage(
                "pass the legacy prefix form or --source/--verb, not both",
            ));
        }
        let Some(v) = verb.filter(|v| !v.trim().is_empty()) else {
            return Err(NameError::usage("--source requires --verb"));
        };
        let source = source.filter(|s| !s.trim().is_empty());
        let code = if dispatch_verbs().contains(v.trim()) {
            v.trim().to_string()
        } else {
            verb_code_for(Some(v))?
        };
        return dispatch_agent_name(
            source,
            &code,
            node_id,
            slug,
            qualifier,
            discriminator,
            model,
        );
    }
    match prefix.filter(|p| !p.trim().is_empty()) {
        Some(p) => agent_name(p, node_id, slug, qualifier, discriminator, model),
        None => Err(NameError::usage("a prefix or --verb is required")),
    }
}

/// A parsed canonical name. `source` is None for the manual form; `node` is
/// the graph node id when the identity is node-shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub name: String,
    pub source: Option<String>,
    pub verb: String,
    pub node: Option<String>,
    pub tail: String,
}

fn node_shape() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:([a-z][a-z0-9]*-)?([0-9a-f]+))(?:-(.*))?$").unwrap())
}

/// Parse `[<source>-]<verb>-<identity>`, else None. Positional grammar: the
/// first token is a source only when the second is a verb, so a node prefix
/// colliding with a code cannot misread. Pre-cutover names are not canonical.
pub fn parse_dispatch_agent_name(name: Option<&str>) -> Option<Parsed> {
    let name = name?;
    if name.is_empty() {
        return None;
    }
    let tokens: Vec<&str> = name.split('-').collect();
    let (source, verb, rest): (Option<&str>, &str, &[&str]);
    if tokens.len() >= 2
        && dispatch_sources().contains(tokens[0])
        && dispatch_verbs().contains(tokens[1])
    {
        source = Some(tokens[0]);
        verb = tokens[1];
        rest = &tokens[2..];
    } else if dispatch_verbs().contains(tokens[0]) {
        source = None;
        verb = tokens[0];
        rest = &tokens[1..];
    } else {
        return None;
    }
    if rest.is_empty() {
        return None;
    }
    let joined = rest.join("-");
    if rest[0] == "backlog" || rest[0] == "evals" || rest[0] == "session" {
        return Some(Parsed {
            name: name.to_string(),
            source: source.map(str::to_string),
            verb: verb.to_string(),
            node: None,
            tail: joined,
        });
    }
    match node_shape().captures(&joined) {
        Some(c) => Some(Parsed {
            name: name.to_string(),
            source: source.map(str::to_string),
            verb: verb.to_string(),
            node: Some(match (c.get(1), c.get(2)) {
                // Full form keeps the prefix; bare hex stays bare - the
                // Python bridge re-attaches the id via the graph (x-57fe).
                (Some(p), Some(hex)) => format!("{}{}", p.as_str(), hex.as_str()),
                (_, Some(hex)) => hex.as_str().to_string(),
                _ => joined.clone(),
            }),
            tail: c.get(3).map(|m| m.as_str().to_string()).unwrap_or_default(),
        }),
        None => Some(Parsed {
            name: name.to_string(),
            source: source.map(str::to_string),
            verb: verb.to_string(),
            node: None,
            tail: joined,
        }),
    }
}

/// Verb code for a pre-cutover convention name (`target-*` -> `t`,
/// `think-*` -> `th`), else None: the legacy-read window helper.
pub fn legacy_verb_code(name: Option<&str>) -> Option<String> {
    let name = name?;
    if name.starts_with("target-") {
        Some("t".to_string())
    } else if name.starts_with("think-") {
        Some("th".to_string())
    } else {
        None
    }
}

// ── internal machine verbs (binary-direct, not `fno agents` surface) ─────────
//
// The delegation flip: Python's naming.py keeps its public signatures and
// shells THESE verbs, so the tables, mint, and parse own exactly one
// implementation. Matched with `matches!` in client.rs like `reentry-plan`,
// they stay out of the routable-verb parity sets by design.

fn value_of(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| {
            let prefix = format!("{}=", flag);
            args.iter()
                .find_map(|a| a.strip_prefix(&prefix).map(str::to_string))
        })
}

fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            if a.contains('=') {
                i += 1;
            } else {
                i += 2;
            }
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// `fno-agents name-mint [prefix] <node> [--slug S] [--qualifier Q]
/// [--discriminator D] [--source S] [--verb V]`: the bridge, byte-compatible
/// with `fno agents name`. Prints the name; exit 2 usage, 3 refusal.
pub fn run_name_mint(args: &[String]) -> i32 {
    let pos = positionals(args);
    // One positional binds the node (the CLI contract: Click would bind it to
    // the legacy prefix slot; the bridge reads it as the node instead).
    let (prefix, node) = match pos.len() {
        0 => (None, String::new()),
        1 => (None, pos[0].clone()),
        _ => (Some(pos[0].as_str()), pos[1].clone()),
    };
    if node.is_empty() {
        eprintln!("error: a node id is required: fno-agents name-mint <node-id>");
        return 2;
    }
    match bridge_name(
        prefix,
        &node,
        value_of(args, "--slug").as_deref(),
        value_of(args, "--qualifier").as_deref(),
        value_of(args, "--discriminator").as_deref(),
        value_of(args, "--source").as_deref(),
        value_of(args, "--verb").as_deref(),
        value_of(args, "--model").as_deref(),
    ) {
        Ok(name) => {
            println!("{name}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            e.exit
        }
    }
}

/// `fno-agents name-parse` reads one name per line on stdin and prints one
/// JSON object per line: {name, source, verb, node, tail} or {name, null}.
pub fn run_name_parse() -> i32 {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let parsed = parse_dispatch_agent_name(Some(line.trim_end_matches('\n')));
        let json = match parsed {
            Some(p) => format!(
                "{{\"name\":{},\"source\":{},\"verb\":{},\"node\":{},\"tail\":{}}}",
                serde_json::to_string(&p.name).unwrap_or_else(|_| "null".into()),
                p.source
                    .as_ref()
                    .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".into()))
                    .unwrap_or_else(|| "null".into()),
                serde_json::to_string(&p.verb).unwrap_or_else(|_| "null".into()),
                p.node
                    .as_ref()
                    .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".into()))
                    .unwrap_or_else(|| "null".into()),
                serde_json::to_string(&p.tail).unwrap_or_else(|_| "null".into()),
            ),
            None => format!(
                "{{\"name\":{},\"parsed\":null}}",
                serde_json::to_string(&line).unwrap_or_else(|_| "\"\"".into())
            ),
        };
        let _ = writeln!(out, "{json}");
    }
    0
}

/// `fno-agents name-codes --json`: the vocabulary tables for thin readers.
pub fn run_name_codes() -> i32 {
    let c = codes();
    let json = serde_json::json!({
        "sources": c.sources.iter().collect::<Vec<_>>(),
        "verbs": c.verbs.iter().collect::<Vec<_>>(),
        "word_codes": c.word_codes,
        "model_codes": c.model_codes,
        "provenance": c
            .provenance
            .iter()
            .map(|(s, v, x)| serde_json::json!({"site": s, "source": v, "verb": x}))
            .collect::<Vec<_>>(),
    });
    println!("{json}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_dispatch_form_and_shaving() {
        let n = dispatch_agent_name(
            Some("ab"),
            "bp",
            "x-84b2",
            Some("Ab Names"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(n, "ab-bp-84b2-ab-names");
        let manual = dispatch_agent_name(None, "t", "x-84b2", None, None, None, None).unwrap();
        assert_eq!(manual, "t-84b2");
    }

    #[test]
    fn model_segment_appends_and_refuses_unknown_only_when_blank_squeezes() {
        // Known model: longest-key-first substring match on the lowered model.
        let n = dispatch_agent_name(
            Some("sob"),
            "bp",
            "x-57fe",
            Some("250ms Read Floor"),
            None,
            None,
            Some("glm-5.3-flash[1m]"),
        )
        .unwrap();
        assert_eq!(n, "sob-bp-57fe-250ms-read-glm");
        // Unknown model squeezes to 8 alphanumeric characters.
        let n = dispatch_agent_name(
            Some("ab"),
            "t",
            "x-1",
            None,
            None,
            None,
            Some("Kimi K2 Thinking"),
        )
        .unwrap();
        assert_eq!(n, "ab-t-1-kimik2th");
        // Blank model is absent, never a segment of empty text.
        let n = dispatch_agent_name(Some("ab"), "t", "x-1", None, None, None, Some("  ")).unwrap();
        assert_eq!(n, "ab-t-1");
    }

    #[test]
    fn dispatch_slug_cap_cuts_at_hyphen() {
        // 16-char slug under cap 12 cuts at the last hyphen, never mid-word.
        assert_eq!(
            cut_slug(Some("250ms Read Floor"), DISPATCH_SLUG_CAP),
            "250ms-read"
        );
        // A hyphenless slug truncates hard at the cap.
        assert_eq!(cut_slug(Some("abcdefghij"), 4), "abcd");
        // Under the cap the slug passes through whole.
        assert_eq!(cut_slug(Some("Ab Names"), DISPATCH_SLUG_CAP), "ab-names");
        let n = dispatch_agent_name(
            Some("ab"),
            "t",
            "x-1",
            Some("250ms Read Floor"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(n, "ab-t-1-250ms-read");
    }

    #[test]
    fn mint_refusals() {
        let err = dispatch_agent_name(Some("ab"), "zz", "x-1", None, None, None, None).unwrap_err();
        assert_eq!(err.exit, 3);
        assert!(err.message.contains("unknown dispatch verb"));
    }

    #[test]
    fn unknown_source_and_verb_refuse() {
        assert!(dispatch_agent_name(Some("zz"), "t", "x-1", None, None, None, None).is_err());
        assert!(dispatch_agent_name(None, "zz", "x-1", None, None, None, None).is_err());
        assert!(verb_code_for(Some("impeccable")).is_err());
        assert_eq!(verb_code_for(Some("/fno:target")).unwrap(), "t");
        assert_eq!(verb_code_for(Some("$fno:blueprint")).unwrap(), "bp");
        assert_eq!(verb_code_for(Some("builtin")).unwrap(), "t");
    }

    #[test]
    fn required_identity_never_shaves() {
        let long = "n-".to_string() + &"z".repeat(70);
        let err = dispatch_agent_name(Some("ab"), "t", &long, None, None, None, None).unwrap_err();
        assert!(err.message.contains("64-char"), "{err}");
        assert_eq!(err.exit, 3);
    }

    #[test]
    fn slug_gives_way_to_the_budget() {
        let n = agent_name(
            "t",
            "regready-pipeline-2c4f9a1b3d",
            Some("path consolidation wave 0 delegate handoff"),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(n.len() <= 64, "{n}");
        assert!(n.starts_with("t-regready-pipeline-2c4f9a1b3d-"), "{n}");
    }

    #[test]
    fn parse_positional_grammar() {
        let p = parse_dispatch_agent_name(Some("ab-bp-x-84b2-slug")).unwrap();
        assert_eq!(p.source.as_deref(), Some("ab"));
        assert_eq!(p.verb, "bp");
        assert_eq!(p.node.as_deref(), Some("x-84b2"));
        assert_eq!(p.tail, "slug");
        // Bare-hex node: parsed as the node, hex kept bare.
        let p = parse_dispatch_agent_name(Some("ab-bp-84b2-slug")).unwrap();
        assert_eq!(p.node.as_deref(), Some("84b2"));
        assert_eq!(p.tail, "slug");
        // A node prefix colliding with a code never misreads as a source.
        let p = parse_dispatch_agent_name(Some("t-x-84b2")).unwrap();
        assert!(p.source.is_none());
        assert_eq!(p.verb, "t");
        assert_eq!(p.node.as_deref(), Some("x-84b2"));
        // Typed identities stay opaque.
        let p = parse_dispatch_agent_name(Some("ro-t-session-abcd1234")).unwrap();
        assert!(p.node.is_none());
        assert_eq!(p.tail, "session-abcd1234");
        // Pre-cutover names are not canonical.
        assert!(parse_dispatch_agent_name(Some("target-x-84b2-1")).is_none());
        assert!(parse_dispatch_agent_name(Some("j-x-3218-2")).is_none());
    }

    #[test]
    fn bridge_forms() {
        let n = bridge_name(
            None,
            "x-84b2",
            Some("Ab Names"),
            None,
            None,
            Some("ab"),
            Some("bp"),
            None,
        )
        .unwrap();
        assert_eq!(n, "ab-bp-84b2-ab-names");
        assert_eq!(
            bridge_name(
                Some("legacy"),
                "x-1",
                None,
                None,
                None,
                Some("ab"),
                Some("t"),
                None,
            )
            .unwrap_err()
            .exit,
            2
        );
        assert_eq!(
            bridge_name(None, "x-1", None, None, None, Some("ab"), None, None)
                .unwrap_err()
                .exit,
            2
        );
        assert_eq!(
            bridge_name(None, "x-1", None, None, None, None, None, None)
                .unwrap_err()
                .exit,
            2
        );
        let n = bridge_name(Some("target"), "x-3218", None, None, None, None, None, None).unwrap();
        assert_eq!(n, "target-3218");
        // One positional (empty prefix) with a verb is the node.
        let n = bridge_name(
            None,
            "x-84b2",
            None,
            None,
            None,
            Some("kl"),
            Some("th"),
            None,
        )
        .unwrap();
        assert_eq!(n, "kl-th-84b2");
    }

    #[test]
    fn legacy_window_and_tables() {
        assert_eq!(legacy_verb_code(Some("target-x-1-2")).as_deref(), Some("t"));
        assert_eq!(legacy_verb_code(Some("think-x-1")).as_deref(), Some("th"));
        assert_eq!(legacy_verb_code(Some("jn-t-x-1")), None);
        assert!(dispatch_sources().contains("ab"));
        assert!(provenance_rows().len() >= 18);
        let disc = slug_component(Some("Path Consolidation: Wave 0"), SLUG_CAP);
        assert_eq!(disc, "path-consolidation-wave-0");
    }
}
