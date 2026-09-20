//! The attention-item model: a read-time projection over the three stores
//! that already exist (question journals, escalation notes, the user lane).
//! No new store; the mux overlay, the sinks and the future answer endpoint
//! all read this one projection. Every user-facing name here is `question`,
//! `pin` or `mine`; "attention item" stays inside code and the plan.

use serde::Serialize;
use serde_json::Value;

/// Who asked, and how to reach them. `live` is `None` when unmeasured, never
/// `false` by inference. `reach` is the exact command that opens the asking
/// session (from the agents registry), `None` when no live row joins.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Asker {
    pub handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemOption {
    pub n: u32,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub pros: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recommendation {
    pub option: u32,
    pub why: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downside: Option<String>,
}

/// One item. Field names are the OpenAPI contract (`docs/architecture/
/// attention-items.md`); they are the wire shape the sinks and the endpoint
/// share, so they do not follow this repo's rust naming beyond serde defaults.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttentionItem {
    pub id: String,
    /// `question` (someone waits) | `pin` (an action left for the user) | `mine`.
    pub kind: String,
    /// One line, no ids needed to read it.
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Routing key, from the asker's cwd.
    pub project: String,
    /// `high` when the item blocks nodes or carries a class, else `normal`.
    pub priority: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_silence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub blocks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asker: Option<Asker>,
    /// The asking session's node, or the literal `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_because: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options_rationale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<Recommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub options: Vec<ItemOption>,
    /// What the asker has not thought through yet; `none found` is not
    /// accepted, the asker names what it did not check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknowns: Option<String>,
    /// `yes | costly | no`; says whether a king may answer instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reversible: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_if_wrong: Option<String>,
    /// What the asker does while it waits: stops, or proceeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meanwhile: Option<String>,
    /// False when any required field for the kind is absent; a sink never
    /// delivers a not-ready item.
    pub ready: bool,
    /// Names of the absent required fields.
    pub missing: Vec<String>,
    /// `open | answered | withdrawn`.
    pub state: String,
}

/// Every defect, so the asker fixes the row instead of a sink dropping it
/// silently (mirrors `escalation::problems`).
pub fn problems(item: &AttentionItem) -> Vec<String> {
    let mut out = Vec::new();
    if matches!(item.kind.as_str(), "question" | "pin") {
        match &item.asker {
            None => out.push("missing asker".to_string()),
            Some(a) if a.handle.trim().is_empty() => out.push("missing asker".to_string()),
            Some(_) => {}
        }
        match &item.node {
            None => out.push("missing node".to_string()),
            Some(n) if n.trim().is_empty() => out.push("missing node".to_string()),
            Some(_) => {}
        }
        if item
            .blocked_because
            .as_ref()
            .is_none_or(|b| b.trim().is_empty())
        {
            out.push("missing blocked_because".to_string());
        }
        if item.unknowns.as_ref().is_none_or(|u| u.trim().is_empty()) {
            out.push("missing unknowns".to_string());
        }
    }
    if item.kind == "question" {
        if item.options.len() < 2 {
            out.push("fewer than two options".to_string());
        }
        for (i, option) in item.options.iter().enumerate() {
            if option.text.trim().is_empty() {
                out.push(format!("option {} has no text", i + 1));
            }
            if option.next.as_ref().is_none_or(|n| n.trim().is_empty()) {
                out.push(format!("option {} has no next", i + 1));
            }
        }
        if item
            .options_rationale
            .as_ref()
            .is_none_or(|r| r.trim().is_empty())
        {
            out.push("missing options_rationale".to_string());
        }
        match &item.recommendation {
            None => out.push("missing recommendation".to_string()),
            Some(r) => {
                if r.option == 0 || r.option as usize > item.options.len() {
                    out.push("recommendation outside the options".to_string());
                }
                if r.why.trim().is_empty() {
                    out.push("recommendation has no why".to_string());
                }
            }
        }
        match item.reversible.as_deref() {
            None | Some("") => out.push("missing reversible".to_string()),
            Some("costly")
                if item
                    .cost_if_wrong
                    .as_ref()
                    .is_none_or(|c| c.trim().is_empty()) =>
            {
                out.push("missing cost_if_wrong".to_string());
            }
            Some(v) if !matches!(v, "yes" | "costly" | "no") => {
                out.push(format!("unknown reversible: {v}"));
            }
            Some(_) => {}
        }
        if item.meanwhile.as_ref().is_none_or(|m| m.trim().is_empty()) {
            out.push("missing meanwhile".to_string());
        }
    }
    out
}

impl AttentionItem {
    /// `ready` and `missing` derived from [`problems`], so the gate and the
    /// message the asker receives can never disagree.
    pub fn seal_readiness(&mut self) {
        self.missing = problems(self);
        self.ready = self.missing.is_empty();
    }
}

/// Envelope-agnostic scalar read: nested under `data` (unified rows) or
/// top-level (retired flat rows). Same shape `needs.rs` keeps locally.
fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
        .and_then(|f| f.as_str())
}

fn arr_field<'a>(v: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
        .and_then(|f| f.as_array())
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build one item from an `operator_question` row (`data` fields read
/// envelope-agnostically). `data.context`, when present, carries the
/// structured context fields the Rust intake writes (wave 3); bare
/// `options` strings arrive without `next`/`pros`/`cons` and read not-ready.
fn item_from_question_row(row: &Value, ts: &str) -> Option<AttentionItem> {
    let qid = str_field(row, "question_id")?;
    let question = str_field(row, "question").unwrap_or("");
    let title = question.lines().next().unwrap_or("").to_string();
    let ask = str_field(row, "ask").unwrap_or("");
    let options: Vec<ItemOption> = arr_field(row, "options")
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, o)| match o {
                    Value::String(text) => ItemOption {
                        n: i as u32 + 1,
                        text: text.clone(),
                        next: None,
                        pros: vec![],
                        cons: vec![],
                    },
                    Value::Object(_) => ItemOption {
                        n: o.get("n").and_then(Value::as_u64).unwrap_or(i as u64 + 1) as u32,
                        text: o
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        next: o.get("next").and_then(Value::as_str).map(str::to_string),
                        pros: o
                            .get("pros")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        cons: o
                            .get("cons")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    },
                    _ => ItemOption {
                        n: i as u32 + 1,
                        text: String::new(),
                        next: None,
                        pros: vec![],
                        cons: vec![],
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    let kind = if options.is_empty() && !ask.is_empty() {
        "pin"
    } else {
        "question"
    };
    let ctx = row
        .get("data")
        .and_then(|d| d.get("context"))
        .or_else(|| row.get("context"));
    let ctx_str = |key: &str| -> Option<String> {
        ctx.and_then(|c| c.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let blocks: Vec<String> = arr_field(row, "blocks")
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let asker = str_field(row, "asker").map(|handle| Asker {
        handle: handle.to_string(),
        session_id: str_field(row, "session_id").map(str::to_string),
        harness: ctx_str("harness"),
        rank: ctx_str("rank"),
        live: None,
        reach: ctx
            .and_then(|c| c.get("reach"))
            .and_then(Value::as_str)
            .map(str::to_string),
    });
    let recommendation = ctx.and_then(|c| c.get("recommendation")).and_then(|r| {
        Some(Recommendation {
            option: r.get("option").and_then(Value::as_u64)? as u32,
            why: r
                .get("why")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            downside: r
                .get("downside")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    });
    let mut item = AttentionItem {
        id: qid.to_string(),
        kind: kind.to_string(),
        title,
        body: Some(question.to_string()).filter(|b| !b.is_empty()),
        project: str_field(row, "cwd")
            .map(basename)
            .unwrap_or("")
            .to_string(),
        priority: String::new(),
        created_at: ts.to_string(),
        deadline: None,
        on_silence: None,
        class: None,
        blocks,
        subject: str_field(row, "subject").map(str::to_string),
        asker,
        node: str_field(row, "node").map(str::to_string),
        blocked_because: ctx_str("blocked_because"),
        options_rationale: ctx_str("options_rationale"),
        recommendation,
        options,
        unknowns: ctx_str("unknowns"),
        reversible: ctx_str("reversible"),
        cost_if_wrong: ctx_str("cost_if_wrong"),
        meanwhile: ctx_str("meanwhile"),
        ready: false,
        missing: vec![],
        state: "open".to_string(),
    };
    item.priority = if item.blocks.is_empty() {
        "normal"
    } else {
        "high"
    }
    .to_string();
    item.seal_readiness();
    Some(item)
}

/// One escalation note becomes a `question` item with `class` and `deadline`.
fn item_from_note(slug: &str, text: &str) -> Option<AttentionItem> {
    let esc = crate::escalation::parse(text);
    if esc.status != "open" {
        return None;
    }
    let options: Vec<ItemOption> = esc
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| ItemOption {
            n: i as u32 + 1,
            text: o.text.clone(),
            next: (!o.next.is_empty()).then(|| o.next.clone()),
            pros: vec![],
            cons: vec![],
        })
        .collect();
    let mut item = AttentionItem {
        id: format!("note-{slug}"),
        kind: "question".to_string(),
        title: esc.title.clone(),
        body: (!esc.deciding.is_empty()).then(|| esc.deciding.clone()),
        project: String::new(),
        priority: "high".to_string(),
        created_at: esc.raised_at.clone(),
        deadline: (!esc.deadline.is_empty()).then(|| esc.deadline.clone()),
        on_silence: (!esc.on_silence.is_empty()).then(|| esc.on_silence.clone()),
        class: (!esc.class.is_empty()).then(|| esc.class.clone()),
        blocks: esc.node.clone().into_iter().collect(),
        subject: None,
        asker: (!esc.raised_by.is_empty()).then(|| Asker {
            handle: esc.raised_by.clone(),
            session_id: None,
            harness: None,
            rank: None,
            live: None,
            reach: None,
        }),
        node: esc.node.clone(),
        blocked_because: (!esc.why_now.is_empty()).then(|| esc.why_now.clone()),
        options_rationale: None,
        recommendation: esc.recommend.map(|n| Recommendation {
            option: n as u32,
            why: esc.recommendation.clone(),
            downside: None,
        }),
        options,
        unknowns: None,
        reversible: None,
        cost_if_wrong: None,
        meanwhile: None,
        ready: false,
        missing: vec![],
        state: "open".to_string(),
    };
    item.seal_readiness();
    Some(item)
}

fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Lane lines the user wrote: `- [ ] text` with no `-> node` or
/// `-> parked:` suffix (those are linked or parked, not open).
fn mine_items(lane_text: &str) -> Vec<AttentionItem> {
    let mut out = Vec::new();
    for line in lane_text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("- [ ] ") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() || rest.contains("-> ") {
            continue;
        }
        out.push(AttentionItem {
            id: format!("mine-{:016x}", fnv1a(rest)),
            kind: "mine".to_string(),
            title: rest.to_string(),
            body: None,
            project: "user".to_string(),
            priority: "normal".to_string(),
            created_at: String::new(),
            deadline: None,
            on_silence: None,
            class: None,
            blocks: vec![],
            subject: None,
            asker: None,
            node: None,
            blocked_because: None,
            options_rationale: None,
            recommendation: None,
            options: vec![],
            unknowns: None,
            reversible: None,
            cost_if_wrong: None,
            meanwhile: None,
            ready: true,
            missing: vec![],
            state: "open".to_string(),
        });
    }
    out
}

/// The projection: every open item across the three stores, board order
/// (high priority first, then newest, then id for determinism). `journals_raw`
/// is the newline-joined question journals, `notes` the (slug, text) pairs of
/// the escalation notes, `lane_text` the user lane file. All arguments are
/// pre-read text, so tests need no disk.
pub fn project(
    journals_raw: &str,
    notes: &[(String, String)],
    lane_text: &str,
    _now: u64,
) -> Vec<AttentionItem> {
    let mut items: Vec<AttentionItem> = Vec::new();
    // Latest ask of a qid wins; a close row drops the item.
    let mut asked: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut closed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in journals_raw.lines() {
        if line.trim().is_empty() || !line.contains("operator_question") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // torn/malformed tail line: skip, never abort
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "operator_question" => {
                if let Some(qid) = str_field(&v, "question_id") {
                    asked.insert(qid.to_string(), v);
                }
            }
            "operator_question_closed" => {
                if let Some(qid) = str_field(&v, "question_id") {
                    closed.insert(qid.to_string());
                }
            }
            _ => {}
        }
    }
    for (qid, row) in &asked {
        if closed.contains(qid) {
            continue;
        }
        let ts = row.get("ts").and_then(Value::as_str).unwrap_or("");
        if let Some(item) = item_from_question_row(row, ts) {
            items.push(item);
        }
    }
    for (slug, text) in notes {
        if let Some(item) = item_from_note(slug, text) {
            items.push(item);
        }
    }
    items.extend(mine_items(lane_text));
    items.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(qid: &str, question: &str, extra: Value) -> String {
        let mut v = json!({
            "ts": "2026-09-18T12:00:00Z",
            "type": "operator_question",
            "source": "test",
            "data": {
                "question_id": qid,
                "question": question,
                "session_id": "s1",
                "cwd": "/repo/fno"
            }
        });
        if let (Some(base), Some(extra_obj)) = (
            v.get_mut("data").and_then(Value::as_object_mut),
            extra.as_object(),
        ) {
            for (k, val) in extra_obj {
                base.insert(k.clone(), val.clone());
            }
        }
        v.to_string()
    }

    #[test]
    fn ac1_hp_open_question_projects_with_missing_fields_named() {
        let journals = format!(
            "{}\n{}\n",
            row("q-closed", "already answered", json!({"ask": "do x"})),
            row(
                "q-open",
                "Rule on the law",
                json!({"ask": "pick one", "options": ["yes", "no"], "blocks": ["x-aaaa"]})
            ),
        );
        let closed = json!({
            "ts": "2026-09-18T13:00:00Z",
            "type": "operator_question_closed",
            "source": "test",
            "data": {"question_id": "q-closed"}
        })
        .to_string();
        let raw = format!("{journals}{closed}\n");
        let items = project(&raw, &[], "", 0);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id, "q-open");
        assert_eq!(item.kind, "question");
        assert_eq!(item.priority, "high");
        assert_eq!(item.state, "open");
        assert!(!item.ready);
        let missing = item.missing.join(",");
        assert!(missing.contains("asker"), "missing: {missing}");
        assert!(missing.contains("blocked_because"), "missing: {missing}");
        assert!(missing.contains("unknowns"), "missing: {missing}");
        assert!(missing.contains("options_rationale"), "missing: {missing}");
        assert!(missing.contains("recommendation"), "missing: {missing}");
        assert!(missing.contains("reversible"), "missing: {missing}");
        assert!(missing.contains("meanwhile"), "missing: {missing}");
    }

    #[test]
    fn ac1_edge_pin_note_and_lane_read_their_kinds() {
        // Pin: no options, ask set.
        let pin = row(
            "q-pin",
            "Publish the crate",
            json!({"ask": "publish fno-event-store"}),
        );
        // Escalation note with status: open.
        let note = "# Decide the outage\n\nclass: irreversible\nstatus: open\nnode: x-aaaa\nraised_by: king-fno-g6\nraised_at: 2026-09-18T09:00:00Z\ndeadline: 2026-09-19T09:00:00Z\nrecommend: 1\non_silence: wait\n\n## What is being decided\nThe rollback.\n\n## Options\n- Roll back now\n    What happens next: the fleet restarts\n- Wait an hour\n    What happens next: risk grows\n\n## Recommendation\nOption 1, the narrowest stop.\n\n## If no answer by the deadline\nWe wait.\n";
        let items = project(
            &pin,
            &[("outage".to_string(), note.to_string())],
            "- [ ] ship tonight\n- [ ] linked -> x-bbbb\n- [x] done thing\n",
            0,
        );
        let kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, vec!["question", "pin", "mine"]);
        let note_item = &items[0];
        assert_eq!(note_item.class.as_deref(), Some("irreversible"));
        assert_eq!(note_item.deadline.as_deref(), Some("2026-09-19T09:00:00Z"));
        assert_eq!(note_item.recommendation.as_ref().unwrap().option, 1);
        assert_eq!(items[2].title, "ship tonight");
        assert_eq!(items[2].missing.len(), 0, "a mine item needs nothing");
    }

    #[test]
    fn context_block_supplies_the_fields() {
        let ctx = json!({
            "question_id": "q-ctx",
            "question": "Which reading?",
            "ask": "pick one",
            "session_id": "s9",
            "cwd": "/repo/fno",
            "options": [
                {"n": 1, "text": "Narrow", "next": "unblocks today", "pros": ["fast"], "cons": ["strict"]},
                {"n": 2, "text": "Wide", "next": "waits", "pros": ["safe"], "cons": ["slow"]}
            ],
            "context": {
                "blocked_because": "two repairs are both Python edits",
                "options_rationale": "the three readings kings acted on",
                "recommendation": {"option": 1, "why": "narrowest", "downside": "a repair hides a feature"},
                "unknowns": "whether a net-zero move counts",
                "reversible": "costly",
                "cost_if_wrong": "the allowance drops",
                "meanwhile": "stops"
            }
        });
        let raw = format!(
            r#"{{"ts":"2026-09-18T12:00:00Z","type":"operator_question","source":"test","data":{}}}"#,
            ctx
        );
        let items = project(&raw, &[], "", 0);
        assert_eq!(items.len(), 1);
        assert!(items[0].ready, "missing: {:?}", items[0].missing);
    }
}
