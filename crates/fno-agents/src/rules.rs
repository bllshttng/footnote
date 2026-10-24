//! The rule schema: one `[[rule]]` row format for every rule fno checks
//! (`docs/architecture/event-rules.md`, "The rule schema"). A rule declares
//! its own trigger (`[rule.when]`) and its action, so it reaches an agent at
//! the decision point it governs and nowhere else. The Stop table
//! (`event_rules.toml`) parses through here; so does the law corpus file the
//! `rules-check` mode reads.
//!
//! The tier is derived, never written: a rule with only structural fields is
//! tier 0 (an index lookup, cheap enough for every tool call), `retrieval`
//! makes it tier 1 (ranked text), `classifier` makes it tier 2 (a model
//! call). Only tier 0 may block at a tool call, because only tier 0 answers
//! inside the hook budget every time.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const ACTIONS: &[&str] = &["inform", "warn", "require-ack", "block"];
/// The four hook classes: `block` is sync and may deny, `wait` is sync and
/// adds context, `next-turn` is async and lands at the next turn boundary,
/// `rewake` is async and wakes or steers the session.
const CLASSES: &[&str] = &["block", "wait", "next-turn", "rewake"];
/// How an `inform` reaches its reader: hook context, a notify to the user,
/// or a journal emit. The Stop table's `notify` and `emit` rows are
/// `inform` rows with a channel, not two more severities.
const DELIVERS: &[&str] = &["context", "notify", "emit"];
const FAMILIES: &[&str] = &["hold", "freeze", "naming", "other"];
const EVENTS: &[&str] = &[
    "prompt",
    "skill",
    "pre-tool",
    "post-tool",
    "plan-start",
    "turn-end",
    "stop",
];
const LANES: &[&str] = &["law-door", "decide-door", "rule-file"];
const DISPOSITIONS: &[&str] = &["grant", "retire", "superseded-pair"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct When {
    pub events: Vec<String>,
    pub paths: Vec<String>,
    pub tools: Vec<String>,
    pub commands: Vec<String>,
    pub skills: Vec<String>,
    pub nodes: Vec<String>,
    pub projects: Vec<String>,
    pub phases: Vec<String>,
    pub harnesses: Vec<String>,
    pub models: Vec<String>,
    pub machines: Vec<String>,
    /// Door subjects such as `merge-freeze:*` or `pr-hold:*`.
    pub subjects: Vec<String>,
    /// Named predicates the Stop engine evaluates (`event_rules.rs`).
    pub predicates: Vec<String>,
    pub retrieval: Option<String>,
    pub classifier: Option<String>,
}

impl When {
    fn structural(&self) -> bool {
        [
            &self.events,
            &self.paths,
            &self.tools,
            &self.commands,
            &self.skills,
            &self.nodes,
            &self.projects,
            &self.phases,
            &self.harnesses,
            &self.models,
            &self.machines,
            &self.subjects,
            &self.predicates,
        ]
        .iter()
        .any(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    pub by: String,
    pub at: String,
    pub lane: String,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// The decision id this rule carries, when it came from a law.
    #[serde(default)]
    pub law: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub what: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub instead: String,
    pub action: String,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub deliver: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub authority: Option<Authority>,
    #[serde(default = "yes")]
    pub default: bool,
    #[serde(default)]
    pub when: When,
}

impl Rule {
    pub fn tier(&self) -> u8 {
        if self.when.classifier.is_some() {
            2
        } else if self.when.retrieval.is_some() {
            1
        } else {
            0
        }
    }

    /// The declared class, else `block` for a denying action and `wait`
    /// for the rest.
    pub fn class(&self) -> &str {
        match (&self.class, self.action.as_str()) {
            (Some(c), _) => c,
            (None, "block" | "require-ack") => "block",
            (None, _) => "wait",
        }
    }

    pub fn deliver(&self) -> &str {
        self.deliver.as_deref().unwrap_or("context")
    }

    /// Every reason this row is not a legal rule; empty means valid.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut bad = |m: String| out.push(format!("{}: {m}", self.id));
        if self.id.trim().is_empty() {
            bad("a rule has no id".into());
        }
        if !ACTIONS.contains(&self.action.as_str()) {
            bad(format!("unknown action {:?}", self.action));
        }
        let class = self.class();
        if !CLASSES.contains(&class) {
            bad(format!("unknown class {class:?}"));
        }
        if !DELIVERS.contains(&self.deliver()) {
            bad(format!("unknown deliver {:?}", self.deliver()));
        }
        if self.deliver() != "context" && self.action != "inform" {
            bad(format!("deliver {} needs action inform", self.deliver()));
        }
        let denies = matches!(self.action.as_str(), "block" | "require-ack");
        if denies != (class == "block") {
            bad(format!(
                "action {} does not fit class {class}: only block and require-ack run in class block",
                self.action
            ));
        }
        if matches!(class, "next-turn" | "rewake")
            && !matches!(self.action.as_str(), "inform" | "warn")
        {
            bad(format!("class {class} carries only inform or warn"));
        }
        if !self.when.structural() && self.tier() == 0 {
            bad("no trigger: [rule.when] sets no field".into());
        }
        let door_only =
            !self.when.events.is_empty() && self.when.events.iter().all(|e| e.starts_with("door:"));
        if denies && self.tier() != 0 && !(self.tier() == 2 && door_only) {
            bad(format!(
                "a {} rule needs a tier-0 trigger (tier {} may deny only at a door:* event)",
                self.action,
                self.tier()
            ));
        }
        for e in &self.when.events {
            if !event_known(e) {
                bad(format!("unknown event {e:?}"));
            }
        }
        if let Some(scope) = &self.scope {
            if !scope_known(scope) {
                bad(format!("unknown scope {scope:?}"));
            }
        }
        if let Some(expires) = &self.expires {
            if !expires_known(expires) {
                bad(format!(
                    "expires {expires:?} is not never, YYYY-MM-DD or \"until <node> closes\""
                ));
            }
        }
        if let Some(family) = &self.family {
            if !FAMILIES.contains(&family.as_str()) {
                bad(format!("unknown family {family:?}"));
            }
        }
        if let Some(a) = &self.authority {
            if !LANES.contains(&a.lane.as_str()) {
                bad(format!("unknown authority lane {:?}", a.lane));
            }
        }
        out
    }
}

/// A hook event, a `door:<name>`, a `decision_span:<kind>`, or a
/// `journal:<type>` the Stop engine reads from the events journal.
fn event_known(e: &str) -> bool {
    let named = |prefix: &str| e.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty());
    EVENTS.contains(&e) || named("door:") || named("decision_span:") || named("journal:")
}

fn scope_known(s: &str) -> bool {
    s == "fleet"
        || ["team:", "project:", "node:", "session:"]
            .iter()
            .any(|p| s.strip_prefix(p).is_some_and(|rest| !rest.is_empty()))
}

fn expires_known(s: &str) -> bool {
    let date = s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    let until = s
        .strip_prefix("until ")
        .and_then(|rest| rest.strip_suffix(" closes"))
        .is_some_and(|node| !node.trim().is_empty());
    s == "never" || date || until
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disposition {
    pub law: String,
    pub kind: String,
    pub reason: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleFile {
    pub rule: Vec<Rule>,
    pub disposition: Vec<Disposition>,
}

pub fn parse_file(text: &str) -> Result<RuleFile, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Parse and validate a rule table; the first problem refuses the table.
pub fn parse_table(text: &str) -> Result<Vec<Rule>, String> {
    let file = parse_file(text)?;
    if file.rule.is_empty() {
        return Err("no [[rule]] rows".into());
    }
    let mut seen = BTreeSet::new();
    for rule in &file.rule {
        if let Some(p) = rule.problems().into_iter().next() {
            return Err(p);
        }
        if !seen.insert(rule.id.as_str()) {
            return Err(format!("{}: duplicate id", rule.id));
        }
    }
    Ok(file.rule)
}

/// One decision point as the tier-0 match sees it. The caller fills it from
/// the hook payload and its own environment.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Event {
    pub kind: String,
    pub tool: Option<String>,
    pub paths: Vec<String>,
    pub command: Option<String>,
    pub skill: Option<String>,
    pub node: Option<String>,
    pub project: Option<String>,
    pub phase: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub machine: Option<String>,
    pub subjects: Vec<String>,
}

/// The tier-0 match: every structural field the rule sets must hold for the
/// event, each as a glob where `*` crosses `/`. Predicates are the Stop
/// engine's to evaluate and are not read here.
pub fn matches(rule: &Rule, ev: &Event) -> bool {
    let w = &rule.when;
    let one = |pats: &[String], val: &Option<String>| {
        pats.is_empty()
            || val
                .as_deref()
                .is_some_and(|v| pats.iter().any(|p| crate::sync_canonical::fnmatch(v, p)))
    };
    let many = |pats: &[String], vals: &[String]| {
        pats.is_empty()
            || vals
                .iter()
                .any(|v| pats.iter().any(|p| crate::sync_canonical::fnmatch(v, p)))
    };
    (w.events.is_empty() || w.events.iter().any(|e| *e == ev.kind))
        && many(&w.paths, &ev.paths)
        && many(&w.subjects, &ev.subjects)
        && one(&w.tools, &ev.tool)
        && one(&w.commands, &ev.command)
        && one(&w.skills, &ev.skill)
        && one(&w.nodes, &ev.node)
        && one(&w.projects, &ev.project)
        && one(&w.phases, &ev.phase)
        && one(&w.harnesses, &ev.harness)
        && one(&w.models, &ev.model)
        && one(&w.machines, &ev.machine)
}

fn terms(text: &str) -> Vec<String> {
    text.split_whitespace()
        .flat_map(crate::law_match::tokens)
        .collect()
}

/// Okapi BM25 (k1 = 1.2, b = 0.75) over `(id, text)` documents. Returns the
/// top `k` ids with a positive score, best first, ties by id.
pub fn bm25_rank(docs: &[(String, String)], query: &str, k: usize) -> Vec<(String, f64)> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let bags: Vec<HashMap<String, usize>> = docs
        .iter()
        .map(|(_, text)| {
            let mut bag = HashMap::new();
            for t in terms(text) {
                *bag.entry(t).or_insert(0) += 1;
            }
            bag
        })
        .collect();
    let lens: Vec<f64> = bags
        .iter()
        .map(|b| b.values().sum::<usize>() as f64)
        .collect();
    let n = docs.len() as f64;
    let avg = (lens.iter().sum::<f64>() / n.max(1.0)).max(1.0);
    let idf: Vec<(String, f64)> = crate::law_match::tokens(query)
        .into_iter()
        .map(|q| {
            let df = bags.iter().filter(|b| b.contains_key(&q)).count() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            (q, idf)
        })
        .collect();
    let mut scored: Vec<(String, f64)> = docs
        .iter()
        .zip(bags.iter().zip(&lens))
        .map(|((id, _), (bag, len))| {
            let score = idf
                .iter()
                .filter_map(|(q, idf)| {
                    let tf = *bag.get(q)? as f64;
                    Some(idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg)))
                })
                .sum::<f64>();
            (id.clone(), score)
        })
        .filter(|(_, s)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(k);
    scored
}

fn count(map: &mut BTreeMap<String, usize>, key: &str) {
    *map.entry(key.to_owned()).or_insert(0) += 1;
}

fn law_id(row: &Value) -> Option<&str> {
    row.get("decision_id").and_then(Value::as_str)
}

/// The `rules-check` answer: the file's rules validated and counted, and
/// every live law either carried by a rule (`law = "<id>"`) or disposed of.
/// `ok` holds only when nothing is invalid and no live law is missing.
pub fn check_answer(text: &str, laws: &[Value]) -> Value {
    let file = match parse_file(text) {
        Ok(f) => f,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let live: BTreeSet<&str> = laws.iter().filter_map(law_id).collect();
    let mut invalid = Vec::new();
    let mut seen = BTreeSet::new();
    let mut covered = BTreeSet::new();
    let (mut by_tier, mut by_action, mut by_family, mut tier0_by_family, mut by_kind) = (
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    for rule in &file.rule {
        for p in rule.problems() {
            invalid.push(json!({"id": rule.id, "reason": p}));
        }
        if !seen.insert(rule.id.clone()) {
            invalid.push(json!({"id": rule.id, "reason": format!("{}: duplicate id", rule.id)}));
        }
        if let Some(law) = &rule.law {
            if live.contains(law.as_str()) {
                covered.insert(law.clone());
            } else {
                invalid.push(
                    json!({"id": rule.id, "reason": format!("{}: law {law} is not live", rule.id)}),
                );
            }
        }
        count(&mut by_tier, &format!("tier{}", rule.tier()));
        count(&mut by_action, &rule.action);
        let family = rule.family.as_deref().unwrap_or("other");
        count(&mut by_family, family);
        if rule.tier() == 0 {
            count(&mut tier0_by_family, family);
        }
    }
    let ruled: BTreeSet<&str> = file.rule.iter().filter_map(|r| r.law.as_deref()).collect();
    for d in &file.disposition {
        if !DISPOSITIONS.contains(&d.kind.as_str()) {
            invalid.push(json!({"id": d.law, "reason": format!("{}: unknown disposition {:?}", d.law, d.kind)}));
        }
        if ruled.contains(d.law.as_str()) {
            invalid.push(json!({"id": d.law, "reason": format!("{}: a law carries a rule or a disposition, not both", d.law)}));
        } else if live.contains(d.law.as_str()) {
            covered.insert(d.law.clone());
        } else {
            invalid.push(
                json!({"id": d.law, "reason": format!("{}: disposed law is not live", d.law)}),
            );
        }
        count(&mut by_kind, &d.kind);
    }
    let missing: Vec<&str> = live
        .iter()
        .copied()
        .filter(|id| !covered.contains(*id))
        .collect();
    json!({
        "ok": invalid.is_empty() && missing.is_empty(),
        "rules": file.rule.len(),
        "by_tier": by_tier,
        "by_action": by_action,
        "by_family": by_family,
        "tier0_by_family": tier0_by_family,
        "dispositions": by_kind,
        "laws_live": live.len(),
        "laws_annotated": covered.len(),
        "laws_missing": missing,
        "invalid": invalid,
    })
}

#[derive(Deserialize)]
struct Case {
    name: String,
    stage: String,
    query: String,
    expected: Vec<String>,
}

#[derive(Deserialize)]
struct Cases {
    cases: Vec<Case>,
}

/// The `rules-measure` answer: for each labeled case, BM25 over the live
/// laws' `{subject} {decision}` against the stage keyword match the stage
/// read uses today. The keyword baseline skips the stage read's project and
/// node-subject filters, so it reads the keyword table alone.
pub fn measure_answer(
    cases: &str,
    laws: &[Value],
    keywords: impl Fn(&str) -> Vec<&'static str>,
) -> Value {
    let cases: Cases = match serde_json::from_str(cases) {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "error": format!("cases: {e}")}),
    };
    let docs: Vec<(String, String)> = laws
        .iter()
        .filter_map(|row| {
            let field = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("");
            Some((
                law_id(row)?.to_owned(),
                format!("{} {}", field("subject"), field("decision")),
            ))
        })
        .collect();
    let ratio = |hit: usize, of: usize| if of == 0 { 0.0 } else { hit as f64 / of as f64 };
    let mut rows = Vec::new();
    let mut sums = [0.0f64; 5];
    for case in &cases.cases {
        let expected: BTreeSet<&str> = case.expected.iter().map(String::as_str).collect();
        let ranked = bm25_rank(&docs, &case.query, 10);
        let hits = |n: usize| {
            ranked
                .iter()
                .take(n)
                .filter(|(id, _)| expected.contains(id.as_str()))
                .count()
        };
        let words = keywords(&case.stage);
        let picked: Vec<&str> = docs
            .iter()
            .filter(|(_, text)| {
                let text = text.to_lowercase();
                words.iter().any(|w| text.contains(w))
            })
            .map(|(id, _)| id.as_str())
            .collect();
        let picked_hits = picked.iter().filter(|id| expected.contains(*id)).count();
        let row = [
            ratio(hits(5), expected.len()),
            ratio(hits(10), expected.len()),
            ratio(hits(10), 10),
            ratio(picked_hits, expected.len()),
            ratio(picked_hits, picked.len()),
        ];
        for (sum, v) in sums.iter_mut().zip(row) {
            *sum += v;
        }
        rows.push(json!({
            "name": case.name,
            "stage": case.stage,
            "expected": expected.len(),
            "bm25_top10": ranked.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            "bm25_recall_at_5": row[0],
            "bm25_recall_at_10": row[1],
            "bm25_precision_at_10": row[2],
            "keywords_picked": picked.len(),
            "keywords_recall": row[3],
            "keywords_precision": row[4],
        }));
    }
    let n = cases.cases.len().max(1) as f64;
    json!({
        "ok": true,
        "cases": cases.cases.len(),
        "laws": docs.len(),
        "mean": {
            "bm25_recall_at_5": sums[0] / n,
            "bm25_recall_at_10": sums[1] / n,
            "bm25_precision_at_10": sums[2] / n,
            "keywords_recall": sums[3] / n,
            "keywords_precision": sums[4] / n,
        },
        "per_case": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(toml_row: &str) -> Rule {
        parse_file(&format!("[[rule]]\n{toml_row}"))
            .unwrap()
            .rule
            .remove(0)
    }

    #[test]
    fn a_denying_rule_needs_a_tier0_trigger_and_a_block_class() {
        let r =
            rule("id = \"r1\"\naction = \"block\"\n[rule.when]\nretrieval = \"python edits\"\n");
        let problems = r.problems();
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("r1: ") && p.contains("tier-0 trigger")),
            "{problems:?}"
        );
        let r = rule("id = \"r2\"\naction = \"block\"\nclass = \"next-turn\"\n[rule.when]\ntools = [\"Edit\"]\n");
        assert!(r
            .problems()
            .iter()
            .any(|p| p.contains("does not fit class")));
        // A tier-2 rule may deny at a door, where the call is off the hot path.
        let r = rule("id = \"r3\"\naction = \"block\"\n[rule.when]\nevents = [\"door:merge\"]\nclassifier = \"does the PR break a freeze?\"\n");
        assert!(r.problems().is_empty(), "{:?}", r.problems());
        let r = rule("id = \"r4\"\naction = \"warn\"\nexpires = \"until x closes\"\nscope = \"project:fno\"\n[rule.when]\npaths = [\"a\"]\n");
        assert!(r.problems().is_empty(), "{:?}", r.problems());
        let r = rule("id = \"r5\"\naction = \"warn\"\nexpires = \"soon\"\n");
        assert_eq!(
            r.problems().len(),
            2,
            "bad expires and no trigger: {:?}",
            r.problems()
        );
    }

    #[test]
    fn tier0_match_reads_every_set_field_as_a_glob() {
        let r = rule("id = \"py\"\naction = \"block\"\n[rule.when]\nevents = [\"pre-tool\"]\ntools = [\"Edit\", \"Write\"]\npaths = [\"cli/src/fno/*.py\"]\n");
        let mut ev = Event {
            kind: "pre-tool".into(),
            tool: Some("Edit".into()),
            paths: vec!["cli/src/fno/backlog/law.py".into()],
            ..Event::default()
        };
        assert!(matches(&r, &ev));
        ev.paths = vec!["crates/fno-agents/src/rules.rs".into()];
        assert!(!matches(&r, &ev), "a Rust path is outside the glob");
        ev.paths = vec!["cli/src/fno/law.py".into()];
        ev.tool = Some("Read".into());
        assert!(!matches(&r, &ev), "a Read is not an Edit");
        ev.tool = None;
        assert!(!matches(&r, &ev), "a set field needs a value");
    }

    #[test]
    fn bm25_ranks_the_rare_term_first() {
        let docs = vec![
            (
                "a".to_owned(),
                "merge the pull request when review passes".to_owned(),
            ),
            (
                "b".to_owned(),
                "merge freeze holds every release merge".to_owned(),
            ),
            (
                "c".to_owned(),
                "review the pull request before merge".to_owned(),
            ),
        ];
        let ranked = bm25_rank(&docs, "may I merge during the freeze", 3);
        assert_eq!(ranked[0].0, "b", "{ranked:?}");
        assert!(bm25_rank(&docs, "unrelated words", 3).is_empty());
    }

    #[test]
    fn rules_check_names_every_missing_law_and_dispositions_cover() {
        let laws = vec![
            json!({"decision_id": "d-11111111", "subject": "new-code-language"}),
            json!({"decision_id": "d-22222222", "subject": "merge-grant:x"}),
            json!({"decision_id": "d-33333333", "subject": "mail-raw"}),
        ];
        let text = "[[rule]]\nid = \"rust-only\"\nlaw = \"d-11111111\"\naction = \"block\"\nfamily = \"naming\"\n[rule.when]\npaths = [\"cli/src/fno/*.py\"]\n\n[[disposition]]\nlaw = \"d-22222222\"\nkind = \"grant\"\nreason = \"a head-pinned grant\"\n";
        let answer = check_answer(text, &laws);
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["laws_missing"], json!(["d-33333333"]));
        assert_eq!(answer["laws_annotated"], 2);
        assert_eq!(answer["tier0_by_family"]["naming"], 1);
        let both = format!(
            "{text}\n[[disposition]]\nlaw = \"d-11111111\"\nkind = \"grant\"\nreason = \"x\"\n"
        );
        assert!(check_answer(&both, &laws)["invalid"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not both"));
        let with_gone = format!(
            "{text}\n[[disposition]]\nlaw = \"d-99999999\"\nkind = \"retire\"\nreason = \"x\"\n"
        );
        let answer = check_answer(&with_gone, &laws);
        assert!(answer["invalid"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not live"));
    }

    #[test]
    fn rules_measure_scores_bm25_and_the_keyword_baseline() {
        let laws = vec![
            json!({"decision_id": "d-1", "subject": "merge-approval", "decision": "merge the pull request when review passes"}),
            json!({"decision_id": "d-2", "subject": "merge-freeze", "decision": "a freeze holds every release merge"}),
            json!({"decision_id": "d-3", "subject": "review-lane", "decision": "review inside the session"}),
        ];
        let cases = r#"{"cases": [{"name": "freeze", "stage": "review", "query": "may I merge during the freeze", "expected": ["d-2"]}]}"#;
        let keywords = |stage: &str| {
            if stage == "review" {
                vec!["review"]
            } else {
                vec![]
            }
        };
        let answer = measure_answer(cases, &laws, keywords);
        assert_eq!(answer["cases"], 1);
        assert_eq!(answer["per_case"][0]["bm25_top10"][0], "d-2");
        assert_eq!(answer["mean"]["bm25_recall_at_5"], 1.0);
        assert_eq!(answer["mean"]["bm25_precision_at_10"], 0.1);
        // The keyword baseline picks the two review laws and misses the freeze.
        assert_eq!(answer["per_case"][0]["keywords_picked"], 2);
        assert_eq!(answer["mean"]["keywords_recall"], 0.0);
        assert_eq!(measure_answer("not json", &laws, keywords)["ok"], false);
    }
}
