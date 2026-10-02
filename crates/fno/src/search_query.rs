//! One search grammar for board nodes and feed events: the key table, the
//! parser and the matcher. Ruling d-faf6a93b: one grammar, one meaning per
//! key on every surface. The static page runs a JS twin held to this module
//! by `search_query_cases.json` (the one dual implementation the static-page
//! ruling d-d4a99dde forces).

use crate::agents_view::RegistryAgent;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Which row kind a query runs over. A key the surface cannot answer is
/// refused by name, never silently matched to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Node,
    Event,
}

impl Surface {
    fn name(self) -> &'static str {
        match self {
            Surface::Node => "nodes",
            Surface::Event => "feed events",
        }
    }
}

/// What a key's values mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Prefix when unquoted, exact in quotes (`status:`).
    Value,
    /// Substring when unquoted, phrase in quotes (`title:`, `details:`).
    Text,
    /// `YYYY-MM-DD` or `-7d`, with `> >= < <= =` and `a..b`.
    Date,
    /// `>3`, `3`, `1..5`.
    Number,
    /// Days since the row's `created` (nodes) or `ts` (events).
    Age,
    /// Exact words (`is:`, `has:`); a prefix would make `is:o` mean open OR
    /// operator.
    Flag,
    /// Not a filter: sets the list order.
    Sort,
}

/// One grammar key: `names[0]` is the canonical long name and the field-map
/// key; the rest are accepted spellings, short form first for the docs.
#[derive(Debug, Serialize)]
pub struct KeyDef {
    pub names: &'static [&'static str],
    pub kind: Kind,
    pub node: bool,
    pub event: bool,
}

const fn key(names: &'static [&'static str], kind: Kind, node: bool, event: bool) -> KeyDef {
    KeyDef {
        names,
        kind,
        node,
        event,
    }
}

/// The spec's table, verbatim (plans/20261001-shared-search-grammar.md).
pub static KEYS: &[KeyDef] = &[
    key(&["id", "n"], Kind::Value, true, true),
    key(&["session", "sid"], Kind::Value, true, true),
    key(&["spawner", "by"], Kind::Value, true, true),
    key(&["agent", "a"], Kind::Value, true, true),
    key(&["actor"], Kind::Value, false, true),
    key(&["pr"], Kind::Value, true, true),
    key(&["status", "s"], Kind::Value, true, false),
    key(&["column", "col"], Kind::Value, true, false),
    key(&["priority", "p"], Kind::Value, true, false),
    key(&["size", "z"], Kind::Value, true, false),
    key(&["difficulty", "diff"], Kind::Value, true, false),
    key(&["type", "t"], Kind::Value, true, false),
    key(&["origin"], Kind::Value, true, false),
    key(&["tag"], Kind::Value, true, false),
    key(&["project", "proj"], Kind::Value, true, true),
    key(&["epic", "e"], Kind::Value, true, true),
    key(&["in"], Kind::Value, true, true),
    key(&["lead", "l"], Kind::Value, true, true),
    key(&["domain", "d"], Kind::Value, true, false),
    key(&["area", "ar"], Kind::Value, true, true),
    key(&["harness", "h"], Kind::Value, true, true),
    key(&["model", "m"], Kind::Value, true, true),
    key(&["effort", "ef"], Kind::Value, true, true),
    key(&["phase", "ph"], Kind::Value, true, true),
    key(&["account", "acct"], Kind::Value, true, false),
    key(&["kind", "k"], Kind::Value, false, true),
    key(&["reason"], Kind::Text, false, true),
    key(&["created"], Kind::Date, true, false),
    key(&["updated"], Kind::Date, true, false),
    key(&["done", "completed"], Kind::Date, true, false),
    key(&["ts", "at"], Kind::Date, false, true),
    key(&["is"], Kind::Flag, true, true),
    key(&["has"], Kind::Flag, true, true),
    key(&["votes"], Kind::Number, true, false),
    key(&["children"], Kind::Number, true, false),
    key(&["age"], Kind::Age, true, true),
    key(&["cost"], Kind::Number, true, false),
    key(&["title"], Kind::Text, true, true),
    key(&["details", "body"], Kind::Text, true, true),
    key(&["sort"], Kind::Sort, true, false),
];

/// Grammar sort name to the page's sort key; a `-desc` suffix flips it.
pub static SORT_KEYS: &[(&str, &str)] = &[
    ("created", "created_at"),
    ("updated", "updated_at"),
    ("votes", "encounters"),
    ("priority", "priority"),
    ("id", "id"),
    ("title", "title"),
    ("status", "status"),
    ("size", "size"),
    ("lead", "lead"),
];

/// One derived area: the `ar:` value, answered on both surfaces from this
/// one table. A row matching no rule carries none, and `ar:` never keeps it.
#[derive(Debug)]
pub struct Area {
    pub name: &'static str,
    /// Event kinds that carry the area.
    pub kinds: &'static [&'static str],
    /// Node title whole-word hits.
    pub title_terms: &'static [&'static str],
    /// A substring of a `/`-holding whitespace token in the title or details.
    pub path_terms: &'static [&'static str],
    /// A project whose name maps whole (none today).
    pub projects: &'static [&'static str],
}

/// The spec's Areas table. Event kinds the feed does not emit yet name their
/// area now, so adding the kind adds its area.
pub static AREAS: &[Area] = &[
    Area {
        name: "mux",
        kinds: &[],
        title_terms: &["mux", "sideline", "pane", "portal", "tui"],
        path_terms: &[
            "crates/fno/src/client",
            "crates/fno/src/server",
            "crates/fno/src/tree",
        ],
        projects: &[],
    },
    Area {
        name: "backlog",
        kinds: &[
            "node_created",
            "node_started",
            "node_ended",
            "decision_recorded",
        ],
        title_terms: &["backlog", "board", "kanban", "triage", "groom", "epic"],
        path_terms: &["backlog", "cli/src/fno/graph"],
        projects: &[],
    },
    Area {
        name: "ship",
        kinds: &["node_shipped", "pr_merged"],
        title_terms: &["ship", "pr", "merge", "review"],
        path_terms: &["skills/ship", "skills/review"],
        projects: &[],
    },
    Area {
        name: "ci",
        kinds: &["ci_red", "ci_green"],
        title_terms: &["ci", "workflow", "flaky"],
        path_terms: &[".github/workflows", "scripts/ci"],
        projects: &[],
    },
    Area {
        name: "agents",
        kinds: &[
            "session_spawned",
            "session_reaped",
            "crown_granted",
            "crown_vacated",
            "worker_died",
            "worker_stalled",
            "help_emitted",
            "spawn_refused",
        ],
        title_terms: &[
            "agent", "agents", "spawn", "worker", "session", "reap", "crown", "king", "lead",
            "harness",
        ],
        path_terms: &["agents/", "spawn"],
        projects: &[],
    },
    Area {
        name: "fleet",
        kinds: &["update_started", "update_finished", "daemon_restarted"],
        title_terms: &["fleet", "daemon", "update", "watchdog", "footprint"],
        path_terms: &["daemon", "fleet"],
        projects: &[],
    },
    Area {
        name: "mail",
        kinds: &["question_asked", "question_closed", "mail_to_user"],
        title_terms: &["mail", "inbox", "question", "notify"],
        path_terms: &["mail", "inbox"],
        projects: &[],
    },
];

/// The areas one event kind maps to.
pub fn areas_for_kind(kind: &str) -> Vec<&'static str> {
    AREAS
        .iter()
        .filter(|a| a.kinds.contains(&kind))
        .map(|a| a.name)
        .collect()
}

/// The areas one node maps to: a title term hits on a whole lowercase word
/// of the title, a path term hits when it is a substring of a whitespace
/// token holding `/` in the title or the details, a project hits on
/// equality.
pub fn areas_for_node(title: &str, details: &str, project: Option<&str>) -> Vec<&'static str> {
    let title_lower = title.to_lowercase();
    let details_lower = details.to_lowercase();
    let words: HashSet<&str> = title_lower.split_whitespace().collect();
    let path_tokens: Vec<&str> = title_lower
        .split_whitespace()
        .chain(details_lower.split_whitespace())
        .filter(|w| w.contains('/'))
        .collect();
    AREAS
        .iter()
        .filter(|a| {
            a.title_terms.iter().any(|t| words.contains(t))
                || a.path_terms
                    .iter()
                    .any(|p| path_tokens.iter().any(|tok| tok.contains(p)))
                || project.is_some_and(|p| a.projects.contains(&p))
        })
        .map(|a| a.name)
        .collect()
}

/// One row's flat field map: canonical key to its values, all lowercase.
/// Two reserved keys carry bare words: `text` (short fields, matched whole)
/// and `details` (long fields, fuzzy per whitespace-split word).
pub type Fields = BTreeMap<String, Vec<String>>;

/// `_` and space are the same inside one value, and values are
/// case-insensitive.
fn norm(s: &str) -> String {
    s.to_lowercase().replace('_', " ")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

impl Cmp {
    fn holds(self, a: f64, b: f64) -> bool {
        match self {
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Eq => a == b,
        }
    }

    /// Split a leading comparison operator off a value, longest first.
    fn parse(s: &str) -> (Option<Cmp>, &str) {
        if let Some(rest) = s.strip_prefix(">=") {
            (Some(Cmp::Ge), rest)
        } else if let Some(rest) = s.strip_prefix("<=") {
            (Some(Cmp::Le), rest)
        } else if let Some(rest) = s.strip_prefix('>') {
            (Some(Cmp::Gt), rest)
        } else if let Some(rest) = s.strip_prefix('<') {
            (Some(Cmp::Lt), rest)
        } else if let Some(rest) = s.strip_prefix('=') {
            (Some(Cmp::Eq), rest)
        } else {
            (None, s)
        }
    }
}

/// One leaf predicate: what one `key:value` alternative asserts about a row.
#[derive(Debug, Clone)]
enum Pred {
    /// Prefix when unquoted, exact in quotes.
    Val {
        key: &'static str,
        val: String,
        exact: bool,
    },
    /// Substring (a quoted phrase reads the same way).
    Txt { key: &'static str, val: String },
    /// Exact word.
    Flag { key: &'static str, val: String },
    /// Inclusive epoch-second bounds; `None` side is unbounded.
    Date {
        key: &'static str,
        lo: Option<i64>,
        hi: Option<i64>,
    },
    /// Numeric compare over the key's values; `hi` closes a `a..b` range.
    Num {
        key: &'static str,
        op: Cmp,
        val: f64,
        hi: Option<f64>,
    },
    /// Days between `now` and the row's `created` (nodes) or `ts` (events).
    Age { op: Cmp, days: f64, hi: Option<f64> },
    /// Bare words: fuzzy over `text` (whole) and `details` (per word); a
    /// quoted phrase is a substring over both.
    Bare { val: String, quoted: bool },
    /// A bare UUID or 8-hex token: `sid:<token>` OR the bare word.
    SessionOrText { val: String },
    /// A bare node-id-shaped token: `id:<token>` exact.
    IdExact { val: String },
}

impl Pred {
    fn key(&self) -> Option<&'static str> {
        match self {
            Pred::Val { key, .. }
            | Pred::Txt { key, .. }
            | Pred::Flag { key, .. }
            | Pred::Date { key, .. }
            | Pred::Num { key, .. } => Some(key),
            Pred::IdExact { .. } => Some("id"),
            Pred::SessionOrText { .. } => Some("session"),
            Pred::Age { .. } | Pred::Bare { .. } => None,
        }
    }

    fn holds(&self, f: &Fields, now: i64) -> bool {
        match self {
            Pred::Val { key, val, exact } => f.get(*key).is_some_and(|vals| {
                vals.iter().any(|have| {
                    let have = norm(have);
                    if *exact {
                        have == *val
                    } else {
                        have.starts_with(val.as_str())
                    }
                })
            }),
            Pred::Txt { key, val } => f.get(*key).is_some_and(|vals| {
                vals.iter()
                    .any(|have| have.to_lowercase().contains(val.as_str()))
            }),
            Pred::Flag { key, val } => f
                .get(*key)
                .is_some_and(|vals| vals.iter().any(|have| have == val)),
            Pred::Date { key, lo, hi } => f.get(*key).is_some_and(|vals| {
                vals.iter().any(|have| {
                    stamp_epoch(have).is_some_and(|ts| {
                        lo.is_none_or(|lo| ts >= lo) && hi.is_none_or(|hi| ts <= hi)
                    })
                })
            }),
            Pred::Num { key, op, val, hi } => f.get(*key).is_some_and(|vals| {
                vals.iter()
                    .filter_map(|v| v.parse::<f64>().ok())
                    .any(|n| op.holds(n, *val) && hi.is_none_or(|hi| n <= hi))
            }),
            Pred::Age { op, days, hi } => {
                let stamps = f.get("created").or_else(|| f.get("ts"));
                stamps.is_some_and(|vals| {
                    vals.iter().any(|have| {
                        stamp_epoch(have).is_some_and(|ts| {
                            let age = (now - ts).max(0) as f64 / 86400.0;
                            op.holds(age, *days) && hi.is_none_or(|hi| age <= hi)
                        })
                    })
                })
            }
            Pred::Bare { val, quoted } => bare_hit(f, val, *quoted),
            Pred::SessionOrText { val } => {
                f.get("session").is_some_and(|vals| {
                    vals.iter()
                        .any(|have| have.to_lowercase().starts_with(val.as_str()))
                }) || bare_hit(f, val, false)
            }
            Pred::IdExact { val } => f
                .get("id")
                .is_some_and(|vals| vals.iter().any(|have| norm(have) == *val)),
        }
    }
}

/// The bare-word buckets: fuzzy over `text` whole and over each
/// whitespace-split word of `details`; a quoted needle is a substring over
/// both.
fn bare_hit(f: &Fields, val: &str, quoted: bool) -> bool {
    let text_hit = f.get("text").is_some_and(|vals| {
        vals.iter().any(|have| {
            let have = have.to_lowercase();
            if quoted {
                have.contains(val)
            } else {
                fuzzy_hit(val, &have)
            }
        })
    });
    let details_hit = f.get("details").is_some_and(|vals| {
        vals.iter().any(|have| {
            let have = have.to_lowercase();
            if quoted {
                have.contains(val)
            } else {
                have.split_whitespace().any(|w| fuzzy_hit(val, w))
            }
        })
    });
    text_hit || details_hit
}

/// One token: a word (with its leading `-` already noted) or a lone `|`.
#[derive(Debug)]
enum Tok {
    Word { neg: bool, text: String },
    Pipe,
}

/// Whitespace splits terms except inside `"..."`; a lone `|` is its own
/// token. Quote characters stay in the text; the term parser strips them
/// where the key's kind reads them.
fn tokenize(input: &str) -> Result<Vec<Tok>, String> {
    let mut out: Vec<Tok> = Vec::new();
    let mut in_quote = false;
    let mut cur: Option<(bool, String)> = None;
    for c in input.chars() {
        if c == '"' {
            in_quote = !in_quote;
            cur.get_or_insert_with(|| (false, String::new())).1.push(c);
            continue;
        }
        if !in_quote && (c.is_whitespace() || c == '|') {
            if let Some((neg, text)) = cur.take() {
                out.push(Tok::Word { neg, text });
            }
            if c == '|' {
                out.push(Tok::Pipe);
            }
            continue;
        }
        let entry = cur.get_or_insert_with(|| (false, String::new()));
        if entry.1.is_empty() && c == '-' {
            entry.0 = true;
        } else {
            entry.1.push(c);
        }
    }
    if in_quote {
        return Err("unterminated quote".to_string());
    }
    if let Some((neg, text)) = cur {
        out.push(Tok::Word { neg, text });
    }
    Ok(out)
}

/// One term: its negation flag and its comma-OR alternatives.
#[derive(Debug, Clone)]
struct Term {
    neg: bool,
    alts: Vec<Pred>,
}

impl Term {
    fn holds(&self, f: &Fields, now: i64) -> bool {
        // A term with no alternatives is a no-op (a lone `-` filters
        // nothing), never an always-false predicate.
        if self.alts.is_empty() {
            return true;
        }
        self.alts.iter().any(|p| p.holds(f, now)) != self.neg
    }
}

/// The parsed query: OR groups ANDed together, an optional list sort, and
/// the `now` the relative dates resolved against.
#[derive(Debug, Clone)]
pub struct Parsed {
    groups: Vec<Vec<Term>>,
    /// The resolved page sort key with an optional `-` (descending).
    pub sort: Option<String>,
    now: i64,
}

impl Parsed {
    /// Every group must hold one true term; a negated term is true when its
    /// predicate is false.
    pub fn keeps(&self, f: &Fields) -> bool {
        self.groups
            .iter()
            .all(|g| g.iter().any(|t| t.holds(f, self.now)))
    }

    /// Whether any term uses the canonical key, so the served board reads
    /// `questions.jsonl` only when a query names `has:question`.
    pub fn names(&self, key: &str) -> bool {
        self.groups
            .iter()
            .flatten()
            .any(|t| t.alts.iter().any(|p| p.key() == Some(key)))
    }

    /// Whether the query asks `has:question`, so the served board reads the
    /// 38 MB question journal only then.
    pub fn wants_questions(&self) -> bool {
        self.groups.iter().flatten().any(|t| {
            t.alts
                .iter()
                .any(|p| matches!(p, Pred::Flag { key: "has", val } if val == "question"))
        })
    }
}

/// Parse one query. An empty query parses to a `Parsed` that keeps
/// everything.
pub fn parse(input: &str, surface: Surface, now: i64) -> Result<Parsed, String> {
    let toks = tokenize(input)?;
    let mut groups: Vec<Vec<Term>> = Vec::new();
    let mut sort: Option<String> = None;
    let mut extend = false;
    for tok in toks {
        match tok {
            Tok::Pipe => {
                if groups.is_empty() || extend {
                    return Err("stray '|'".to_string());
                }
                extend = true;
            }
            Tok::Word { neg, text } => {
                if let Some(term) = term_from(&text, neg, surface, now, &mut sort)? {
                    if extend {
                        groups.last_mut().expect("pipe checked").push(term);
                        extend = false;
                    } else {
                        groups.push(vec![term]);
                    }
                }
            }
        }
    }
    if extend {
        return Err("stray '|'".to_string());
    }
    Ok(Parsed { groups, sort, now })
}

/// The first `:` outside quotes splits `name:value`; no split means a bare
/// word.
fn split_colon(text: &str) -> (Option<&str>, &str) {
    let mut in_quote = false;
    for (i, c) in text.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ':' if !in_quote => {
                let (name, rest) = (&text[..i], &text[i + 1..]);
                if name.is_empty() {
                    return (None, text);
                }
                return (Some(name), rest);
            }
            _ => {}
        }
    }
    (None, text)
}

/// Build one term, or `None` for a `sort:` term (which is never a filter).
fn term_from(
    text: &str,
    neg: bool,
    surface: Surface,
    now: i64,
    sort_out: &mut Option<String>,
) -> Result<Option<Term>, String> {
    let (name, value) = split_colon(text);
    let Some(name) = name else {
        return Ok(Some(Term {
            neg,
            alts: bare_alts(text),
        }));
    };
    let def = KEYS
        .iter()
        .find(|k| k.names.contains(&name))
        .ok_or_else(|| unknown_key(name))?;
    let answerable = match surface {
        Surface::Node => def.node,
        Surface::Event => def.event,
    };
    if !answerable {
        return Err(format!(
            "'{name}:' ({}) does not apply to {}",
            def.names[0],
            surface.name()
        ));
    }
    if def.kind == Kind::Sort {
        *sort_out = Some(sort_term(value)?);
        return Ok(None);
    }
    Ok(Some(Term {
        neg,
        alts: value_alts(def, value, now)?,
    }))
}

/// The nearest spelling at the smallest Levenshtein distance, ties keeping
/// table order.
fn unknown_key(name: &str) -> String {
    let mut best: Option<(&str, usize)> = None;
    for k in KEYS {
        for n in k.names {
            let d = strsim::levenshtein(name, n);
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((n, d));
            }
        }
    }
    let best = best.map(|(n, _)| n).unwrap_or("status");
    format!("unknown key '{name}:'; did you mean '{best}:'?")
}

/// Strip one outer quote pair; its absence keeps the text.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// Split alternatives on commas that sit outside quotes.
fn split_alts(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quote = false;
    let mut start = 0usize;
    for (i, c) in value.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ',' if !in_quote => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out.into_iter().filter(|p| !p.is_empty()).collect()
}

/// One key's value text into its predicates.
fn value_alts(def: &KeyDef, value: &str, now: i64) -> Result<Vec<Pred>, String> {
    let key = def.names[0];
    let mut alts = Vec::new();
    for part in split_alts(value) {
        match def.kind {
            Kind::Value => {
                let (raw, exact) = quoted_part(part);
                alts.push(Pred::Val {
                    key,
                    val: norm(&unquote(raw)),
                    exact,
                });
            }
            Kind::Flag => {
                let (raw, _) = quoted_part(part);
                alts.push(Pred::Flag {
                    key,
                    val: unquote(raw).to_lowercase(),
                });
            }
            Kind::Text => {
                let (raw, _) = quoted_part(part);
                alts.push(Pred::Txt {
                    key,
                    val: unquote(raw).to_lowercase(),
                });
            }
            Kind::Date => alts.push(date_pred(key, part, now)?),
            Kind::Number => alts.push(num_pred(key, part)?),
            Kind::Age => alts.push(age_pred(part)?),
            Kind::Sort => {}
        }
    }
    if alts.is_empty() {
        return Err(format!("'{key}:' needs a value"));
    }
    Ok(alts)
}

/// Whether one alternative was quoted, and its text with the quotes left
/// on for [`unquote`].
fn quoted_part(part: &str) -> (&str, bool) {
    let exact = part.len() >= 2 && part.starts_with('"') && part.ends_with('"');
    (part, exact)
}

/// One date alternative: `>=2026-09-20`, `>-7d`, `2026-09-20..2026-09-30`.
/// A relative value with no operator reads `>=`; an absolute one with no
/// operator reads that day.
fn date_pred(key: &'static str, part: &str, now: i64) -> Result<Pred, String> {
    let bad = |v: &str| format!("bad date for '{key}:': '{v}'");
    let (op, rest) = Cmp::parse(part);
    if let Some((a, b)) = rest.split_once("..") {
        if op.is_some() {
            return Err(bad(rest));
        }
        let lo = date_bound(a, now).map_err(|_| bad(a))?;
        let hi = date_bound(b, now).map_err(|_| bad(b))?;
        let hi = if is_literal_date(b) { hi + 86399 } else { hi };
        return Ok(Pred::Date {
            key,
            lo: Some(lo),
            hi: Some(hi),
        });
    }
    let relative = rest.starts_with('-');
    let bound = date_bound(rest, now).map_err(|_| bad(rest))?;
    let (lo, hi) = match op.unwrap_or(if relative { Cmp::Ge } else { Cmp::Eq }) {
        Cmp::Ge => (Some(bound), None),
        Cmp::Gt => (Some(bound + 1), None),
        Cmp::Le => (None, Some(bound)),
        Cmp::Lt => (None, Some(bound - 1)),
        Cmp::Eq if relative => (Some(bound), Some(bound)),
        Cmp::Eq => (Some(bound), Some(bound + 86399)),
    };
    Ok(Pred::Date { key, lo, hi })
}

/// One bound: `-Nd|-Nh|-Nw` from `now`, or an absolute `YYYY-MM-DD` at
/// midnight UTC.
fn date_bound(s: &str, now: i64) -> Result<i64, String> {
    if let Some(rest) = s.strip_prefix('-') {
        let (num, unit) = rest.split_at(rest.len().saturating_sub(1));
        let n: i64 = num
            .parse()
            .map_err(|_| format!("bad relative date '{s}'"))?;
        let secs = match unit {
            "d" => 86400,
            "h" => 3600,
            "w" => 604800,
            _ => return Err(format!("bad relative date '{s}'")),
        };
        return Ok(now - n.max(0) * secs);
    }
    let d =
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("bad date '{s}'"))?;
    Ok(d.and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc()
        .timestamp())
}

fn is_literal_date(s: &str) -> bool {
    !s.starts_with('-')
}

/// One numeric alternative: `>3`, `3`, `1..5`.
fn num_pred(key: &'static str, part: &str) -> Result<Pred, String> {
    let bad = |v: &str| format!("bad number for '{key}:': '{v}'");
    let (op, rest) = Cmp::parse(part);
    if let Some((a, b)) = rest.split_once("..") {
        if op.is_some() {
            return Err(bad(rest));
        }
        let lo: f64 = a.parse().map_err(|_| bad(a))?;
        let hi: f64 = b.parse().map_err(|_| bad(b))?;
        return Ok(Pred::Num {
            key,
            op: Cmp::Ge,
            val: lo,
            hi: Some(hi),
        });
    }
    let val: f64 = rest.parse().map_err(|_| bad(rest))?;
    Ok(Pred::Num {
        key,
        op: op.unwrap_or(Cmp::Eq),
        val,
        hi: None,
    })
}

/// One age alternative, in days.
fn age_pred(part: &str) -> Result<Pred, String> {
    match num_pred("age", part)? {
        Pred::Num { op, val, hi, .. } => Ok(Pred::Age { op, days: val, hi }),
        _ => unreachable!("num_pred answers a Num"),
    }
}

/// The `sort:` term: one [`SORT_KEYS`] name with an optional `-desc`,
/// resolved to the page's sort key.
fn sort_term(value: &str) -> Result<String, String> {
    let v = unquote(value);
    let (name, desc) = match v.strip_suffix("-desc") {
        Some(base) => (base, true),
        None => (v, false),
    };
    let page = SORT_KEYS
        .iter()
        .find(|(gram, _)| *gram == name)
        .map(|(_, page)| *page)
        .ok_or_else(|| {
            let known: Vec<&str> = SORT_KEYS.iter().map(|(g, _)| *g).collect();
            format!("unknown sort '{v}'; known: {}", known.join(", "))
        })?;
    Ok(format!("{}{page}", if desc { "-" } else { "" }))
}

/// Bare words: a node-id-shaped token reads `id:` exact; a UUID or a token
/// of 8 or more hex characters reads `sid:<token>` OR the bare word; a
/// quoted phrase is a substring; anything else is fuzzy.
fn bare_alts(text: &str) -> Vec<Pred> {
    if text.starts_with('"') {
        return vec![Pred::Bare {
            val: unquote(text).to_lowercase(),
            quoted: true,
        }];
    }
    if looks_node_id(text) {
        return vec![Pred::IdExact {
            val: text.to_lowercase(),
        }];
    }
    if text.len() >= 8 && (text.bytes().all(|b| b.is_ascii_hexdigit()) || is_uuid(text)) {
        return vec![Pred::SessionOrText {
            val: text.to_lowercase(),
        }];
    }
    vec![Pred::Bare {
        val: text.to_lowercase(),
        quoted: false,
    }]
}

/// `^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$`, the graph's node-id shape.
fn looks_node_id(s: &str) -> bool {
    let Some((head, tail)) = s.split_once('-') else {
        return false;
    };
    let mut hc = head.chars();
    matches!(hc.next(), Some(c) if c.is_ascii_lowercase())
        && hc.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && head.len() <= 8
        && (4..=8).contains(&tail.len())
        && tail
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An 8-4-4-4-12 hex UUID.
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A stored stamp to epoch seconds: RFC3339 (with `Z` or `+00:00`), a
/// naive `YYYY-MM-DDTHH:MM:SS` read as UTC, or a bare date.
pub(crate) fn stamp_epoch(s: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(
            d.and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc()
                .timestamp(),
        );
    }
    None
}

/// The wall clock, epoch seconds, one `now` per read.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One fzf-style hit: the needle's characters appear in order in the field,
/// gaps allowed. Both sides lowercased by the caller.
pub fn fuzzy_hit(needle: &str, field: &str) -> bool {
    let mut chars = needle.chars();
    let mut want = match chars.next() {
        Some(c) => c,
        None => return true,
    };
    for ch in field.chars() {
        if ch == want {
            match chars.next() {
                Some(c) => want = c,
                None => return true,
            }
        }
    }
    false
}

/// The page's key table as data: the grammar keys and the sort mapping.
pub fn keys_json() -> serde_json::Value {
    serde_json::json!({ "keys": KEYS, "sort": SORT_KEYS })
}

/// One registry row's addressable identity, joined once at read time.
#[derive(Debug, Clone, Default)]
pub struct SessionEntry {
    pub name: String,
    pub account: Option<String>,
    pub model: Option<String>,
    pub harness: Option<String>,
    pub spawned_by_session: Option<String>,
    pub exited: bool,
    /// Every id the row answers to: the harness session id, the fno id and
    /// the short attach id.
    pub ids: Vec<String>,
}

/// Sessions keyed by every id a registry row answers to, so a lookup by a
/// harness uuid, an fno id or a short id yields the same row. Exited rows
/// stay: the names of finished workers still match.
#[derive(Debug, Clone, Default)]
pub struct SessionDirectory {
    by_id: HashMap<String, SessionEntry>,
}

impl SessionDirectory {
    pub fn from_registry(rows: &[RegistryAgent]) -> Self {
        let mut by_id = HashMap::new();
        for r in rows {
            let ids: Vec<String> = [
                r.harness_session_id.as_deref(),
                r.session_id.as_deref(),
                r.attach_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .collect();
            if ids.is_empty() {
                continue;
            }
            let entry = SessionEntry {
                name: r.name.clone(),
                account: r.account.clone(),
                model: r.model.clone(),
                harness: r.harness.clone(),
                spawned_by_session: r.spawned_by_session.clone(),
                exited: r.exited,
                ids: ids.clone(),
            };
            for id in ids {
                by_id.insert(id, entry.clone());
            }
        }
        SessionDirectory { by_id }
    }

    pub fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}
