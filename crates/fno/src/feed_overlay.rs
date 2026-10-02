//! The activity-feed shell-out leg : a bounded, fail-open shell-out to
//! `fno agents feed --json`, in the same shape as [`crate::court_overlay`]'s
//! fold (: the 800ms cap it first copied was half the projection's
//! measured runtime, so the feed could never render).
//!
//! The projection itself lives in `fno-agents` (`feed.rs`): it joins
//! questions.jsonl and graph.json into ordered rows, so no second
//! implementation of the join can drift from the verb's. This module only
//! carries the row shape the client renders and the one bounded call that
//! fetches it - off the UI loop, and a typed [`FeedError`] instead of a bare
//! `None` so the client can say WHICH failure fired.

use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// Ten seconds, court_overlay's budget for a comparable multi-store read.
/// The feed joins three stores and serialises 200 rows; its measured runtime
/// is 1.6s against the 800ms this file inherited from needs_overlay's cheap
/// reads, so under the old cap it could not succeed at all. A long budget
/// costs nothing: the fold runs off the UI loop, one at a time, and
/// `kill_on_drop` reaps the child on the timeout.
const SHELLOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// One feed row, as emitted by `fno agents feed --json`. `session_id` is what
/// the deep link resolves through the sideline's own attach path.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FeedItem {
    pub ts: String,
    pub kind: String,
    #[serde(default)]
    pub node: Option<String>,
    /// The graph node's project directory for node-created launch prefill.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
    /// The registry row's worker name, on a removal: the handle the resume
    /// action and the copied command address.
    #[serde(default)]
    pub name: Option<String>,
    /// Who acted, when that is a mechanism rather than a session. Provenance,
    /// never an attach target.
    #[serde(default)]
    pub actor: Option<String>,
    /// The model observed for that session AT EVENT TIME.
    #[serde(default)]
    pub model: Option<String>,
    /// The effort recorded for that session at event time.
    #[serde(default)]
    pub effort: Option<String>,
    /// The pipeline phase the session was in.
    #[serde(default)]
    pub phase: Option<String>,
    /// A rendered recovery line the panel hands over verbatim, on a
    /// `session_reaped` row.
    #[serde(default)]
    pub detail: Option<String>,
    /// Why the row happened, when the source records one: a removal's
    /// recorded cause, verbatim.
    #[serde(default)]
    pub reason: Option<String>,
    /// `L{level} {scope}` for the crown kinds and a crowned removal.
    #[serde(default)]
    pub crown: Option<String>,
    /// The crowned worker's name on the crown kinds; the feed search
    /// answers `l:` through it.
    #[serde(default)]
    pub holder: Option<String>,
    /// The king or epic the row rolls up to; the panel groups on it.
    #[serde(default)]
    pub owner: Option<String>,
    /// The session that spawned this row's session, from the birth event.
    #[serde(default)]
    pub parent: Option<String>,
    /// The PR URL, on a ship row. The provenance view's PR action opens it.
    #[serde(default)]
    pub url: Option<String>,
}

/// Why a feed fold failed. Each variant is a different user action - retune a
/// budget, free an admission slot, read the projection's stderr - which is
/// why they stopped collapsing into one `None`.
#[derive(Debug, Clone)]
pub enum FeedError {
    /// The projection outlived [`SHELLOUT_TIMEOUT`].
    Timeout,
    /// The projection never ran: spawn failure or a process-admission
    /// refusal. Carries the io error's own words.
    Spawn(String),
    /// The projection ran and exited non-zero. Carries its stderr.
    Exit(String),
    /// stdout was not the row array. Carries the serde error and stderr.
    Malformed(String),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedError::Timeout => {
                write!(f, "feed timed out after {}s", SHELLOUT_TIMEOUT.as_secs())
            }
            FeedError::Spawn(e) => write!(f, "feed could not start: {e}"),
            FeedError::Exit(stderr) => write!(f, "feed exited non-zero: {stderr}"),
            FeedError::Malformed(e) => write!(f, "feed returned malformed JSON: {e}"),
        }
    }
}

/// The projection's stderr, first line, capped for the overlay footer. The
/// ONE shaping rule for stderr in this file: both failure renders go through
/// it, so an Exit line and a Malformed line cannot drift apart.
fn stderr_first_line(stderr: &[u8], none_label: &str) -> String {
    std::str::from_utf8(stderr)
        .ok()
        .and_then(|text| text.lines().next())
        .filter(|line| !line.is_empty())
        .unwrap_or(none_label)
        .chars()
        .take(160)
        .collect()
}

/// The Exit variant's stderr note; a stderr that says nothing (empty, or not
/// text at all) degrades to the exit's own spelling so the line never dangles.
fn stderr_note(stderr: &[u8], status: &std::process::ExitStatus) -> String {
    if std::str::from_utf8(stderr).is_ok() {
        stderr_first_line(stderr, "no stderr")
    } else {
        format!("{status}")
    }
}

/// One fold result, as it travels the client's single-flight channel.
pub type FoldResult = Result<Vec<FeedItem>, FeedError>;

/// Run the feed projection, `Err` carrying the typed reason on any failure.
/// An empty feed is `Ok(vec![])` - a real answer, not a failure.
pub async fn feed_now(since_epoch: &str) -> FoldResult {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args([
            "agents",
            "feed",
            "--json",
            "--since-epoch",
            since_epoch,
            "--limit",
            "200",
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .map_err(|_| FeedError::Timeout)?
        .map_err(|e| FeedError::Spawn(e.to_string()))?;
    if !output.status.success() {
        return Err(FeedError::Exit(stderr_note(&output.stderr, &output.status)));
    }
    parse_feed(&output.stdout, &output.stderr)
}

fn parse_feed(stdout: &[u8], stderr: &[u8]) -> Result<Vec<FeedItem>, FeedError> {
    serde_json::from_slice::<Vec<FeedItem>>(stdout).map_err(|e| {
        FeedError::Malformed(format!(
            "{e}; stderr: {}",
            stderr_first_line(stderr, "none")
        ))
    })
}

/// The context one event row's field map reads: the graph rows' project,
/// parent and open state per node, and the registry's session identities.
pub struct EventCtx {
    /// node id -> (project, parent, open)
    pub nodes: std::collections::HashMap<String, (Option<String>, Option<String>, bool)>,
    pub sessions: crate::search_query::SessionDirectory,
}

impl EventCtx {
    pub fn from_rows(rows: &[Value], registry: &[crate::agents_view::RegistryAgent]) -> Self {
        let mut nodes = std::collections::HashMap::new();
        for r in rows {
            let Some(id) = r.get("id").and_then(Value::as_str) else {
                continue;
            };
            let project = r.get("project").and_then(Value::as_str).map(str::to_string);
            let parent = r.get("parent").and_then(Value::as_str).map(str::to_string);
            let open = r
                .get("completed_at")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty);
            nodes.insert(id.to_string(), (project, parent, open));
        }
        EventCtx {
            nodes,
            sessions: crate::search_query::SessionDirectory::from_registry(registry),
        }
    }
}

/// The event row's search field map, the `Surface::Event` leg. The same
/// canonical keys the node leg answers; `s:` refuses here because kind is
/// the event's work state.
pub fn event_fields(item: &FeedItem, ctx: &EventCtx) -> crate::search_query::Fields {
    use crate::search_query::Fields;
    let mut f = Fields::new();
    let push = |f: &mut Fields, key: &str, val: Option<String>| {
        if let Some(v) = val.filter(|v| !v.is_empty()) {
            let v = v.to_lowercase();
            let slot = f.entry(key.to_string()).or_default();
            if !slot.contains(&v) {
                slot.push(v);
            }
        }
    };
    push(&mut f, "id", item.node.clone());
    push(&mut f, "kind", Some(item.kind.clone()));
    if let Some(sid) = &item.session_id {
        push(&mut f, "session", Some(sid.clone()));
        if let Some(entry) = ctx.sessions.get(sid) {
            for id in &entry.ids {
                push(&mut f, "session", Some(id.clone()));
            }
            push(&mut f, "agent", Some(entry.name.clone()));
            push(&mut f, "harness", entry.harness.clone());
            push(&mut f, "model", entry.model.clone());
        }
    }
    push(&mut f, "spawner", item.parent.clone());
    push(&mut f, "agent", item.name.clone());
    push(&mut f, "actor", item.actor.clone());
    push(&mut f, "harness", item.harness.clone());
    push(&mut f, "model", item.model.clone());
    push(&mut f, "effort", item.effort.clone());
    push(&mut f, "phase", item.phase.clone());
    if let Some(url) = &item.url {
        if let Some(rest) = url.split("/pull/").nth(1) {
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            push(&mut f, "pr", (!num.is_empty()).then_some(num));
        }
    }
    let node_ctx = item.node.as_deref().and_then(|n| ctx.nodes.get(n));
    let project = item
        .cwd
        .as_deref()
        .and_then(|cwd| {
            cwd.rsplit('/')
                .find(|seg| !seg.is_empty())
                .map(str::to_string)
        })
        .or_else(|| node_ctx.and_then(|(p, _, _)| p.clone()));
    push(&mut f, "project", project);
    let epic = item
        .owner
        .as_deref()
        .and_then(|o| o.strip_prefix("epic "))
        .and_then(|rest| rest.split_whitespace().next().map(str::to_string));
    push(&mut f, "epic", epic);
    push(&mut f, "epic", node_ctx.and_then(|(_, p, _)| p.clone()));
    // `in`: the node and its ancestors through the context.
    let mut chain: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(n) = &item.node {
        chain.push(n.clone());
        seen.insert(n.clone());
        let mut cur = node_ctx.and_then(|(_, p, _)| p.clone());
        while let Some(p) = cur {
            if !seen.insert(p.clone()) {
                break;
            }
            chain.push(p.clone());
            cur = ctx.nodes.get(&p).and_then(|(_, parent, _)| parent.clone());
        }
    }
    for id in chain {
        push(&mut f, "in", Some(id));
    }
    let holder = item.holder.clone().or_else(|| {
        let o = item.owner.as_deref().unwrap_or("");
        if let Some(rest) = o.strip_prefix("king ") {
            rest.split_once(" L").map(|(h, _)| h.to_string())
        } else {
            o.rsplit_once('(')
                .and_then(|(_, r)| r.strip_suffix(')').map(str::to_string))
        }
    });
    push(&mut f, "lead", holder.clone());
    push(&mut f, "agent", holder.clone());
    push(&mut f, "ts", Some(item.ts.clone()));
    let area = crate::search_query::areas_for_kind(&item.kind);
    for a in area {
        push(&mut f, "area", Some(a.to_string()));
    }
    if node_ctx.is_some_and(|(_, _, open)| *open) {
        push(&mut f, "is", Some("open".to_string()));
    }
    if item
        .session_id
        .as_deref()
        .and_then(|sid| ctx.sessions.get(sid))
        .is_some_and(|entry| !entry.exited)
    {
        push(&mut f, "is", Some("live".to_string()));
    }
    if item.node.is_some() {
        push(&mut f, "has", Some("node".to_string()));
    }
    if item.session_id.is_some() {
        push(&mut f, "has", Some("session".to_string()));
    }
    if item.url.as_deref().is_some_and(|u| u.contains("/pull/")) {
        push(&mut f, "has", Some("pr".to_string()));
    }
    if item.reason.is_some() {
        push(&mut f, "has", Some("reason".to_string()));
    }
    push(&mut f, "title", Some(item.title.clone()));
    push(&mut f, "details", item.reason.clone());
    push(&mut f, "details", item.detail.clone());
    push(&mut f, "text", Some(item.title.clone()));
    push(&mut f, "text", item.node.clone());
    push(&mut f, "text", item.name.clone());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_deserializes() {
        let body = br#"[{"ts":"2026-09-02T17:12:52Z","kind":"node_started","node":"x-aaaa","session_id":"s-do","harness":"claude","title":"port the claim classifier","ref":null},{"ts":"2026-09-02T18:27:06Z","kind":"pr_created","node":"x-aaaa","session_id":"s-ship","title":"PR 1395","ref":"1395"}]"#;
        let items = parse_feed(body, b"").expect("a well-formed body parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "node_started");
        assert_eq!(items[0].session_id.as_deref(), Some("s-do"));
        assert_eq!(items[1].r#ref.as_deref(), Some("1395"));
    }

    #[test]
    fn empty_body_is_an_empty_vec_not_an_error() {
        let items = parse_feed(b"[]", b"").expect("an empty feed is a real answer");
        assert!(items.is_empty());
    }

    #[test]
    fn new_fields_deserialize_when_the_projection_sends_them() {
        let body = br#"[{"ts":"2026-09-28T16:48:49Z","kind":"session_reaped","title":"heir removed","reason":"why","crown":"L2 x-eeee","owner":"epic x-2222 the epic","parent":"s-lead","holder":"heir"},{"ts":"2026-09-29T08:00:00Z","kind":"node_created","node":"x-aaaa","cwd":"/workspace/node-project","title":"created node"}]"#;
        let items = parse_feed(body, b"").expect("a body carrying the new fields parses");
        assert_eq!(items[0].reason.as_deref(), Some("why"));
        assert_eq!(items[0].crown.as_deref(), Some("L2 x-eeee"));
        assert_eq!(items[0].owner.as_deref(), Some("epic x-2222 the epic"));
        assert_eq!(items[0].parent.as_deref(), Some("s-lead"));
        assert_eq!(items[0].holder.as_deref(), Some("heir"));
        assert_eq!(items[1].cwd.as_deref(), Some("/workspace/node-project"));
        // AC7-HP: the event row interface answers the grammar's keys, and
        // `s:` refuses on the event surface.
        let item = FeedItem {
            ts: "2026-10-01T08:00:00Z".into(),
            kind: "question_asked".into(),
            node: Some("x-eeee".into()),
            session_id: Some("s-king-1234".into()),
            title: "proceed with the merge?".into(),
            owner: Some("king rowan L2".into()),
            url: Some("https://github.com/o/r/pull/2890".into()),
            ..Default::default()
        };
        let mut ctx = EventCtx::from_rows(&[], &[]);
        ctx.nodes.insert("x-eeee".into(), (None, None, true));
        let f = event_fields(&item, &ctx);
        let now = 1791854400;
        let keeps = |q: &str| -> bool {
            match crate::search_query::parse(q, crate::search_query::Surface::Event, now) {
                Ok(p) => p.keeps(&f),
                Err(e) => panic!("{q} refused: {e}"),
            }
        };
        assert!(keeps("k:question"), "k:question keeps");
        assert!(keeps("l:rowan"), "l:rowan keeps through the owner holder");
        assert!(keeps("pr:2890"), "pr:2890 keeps through the url");
        assert!(keeps("ar:mail"), "ar:mail keeps");
        assert!(keeps("sid:s-king-1234"), "sid: keeps");
        assert!(keeps("is:open"), "the open node keeps is:open");
        assert!(
            crate::search_query::parse("s:ready", crate::search_query::Surface::Event, now)
                .is_err()
        );
        // A crown row keeps under l: through the new holder field.
        let crown = FeedItem {
            ts: "2026-09-30T10:00:00Z".into(),
            kind: "crown_granted".into(),
            title: "heir crowned L2 x-eeee".into(),
            holder: Some("heir".into()),
            ..Default::default()
        };
        let fc = event_fields(&crown, &ctx);
        let p = crate::search_query::parse("l:heir", crate::search_query::Surface::Event, now)
            .expect("l:heir parses");
        assert!(p.keeps(&fc), "the crown row keeps under l:heir");
    }

    #[test]
    fn garbage_is_malformed_carrying_stderr() {
        let err = parse_feed(b"not json", b"skipped 3 malformed question lines\n")
            .expect_err("garbage is a typed error, not a silent None");
        assert!(matches!(err, FeedError::Malformed(_)));
        let rendered = err.to_string();
        assert!(rendered.contains("malformed JSON"));
        assert!(rendered.contains("skipped 3 malformed question lines"));
    }

    // regression: the timeout names itself and its duration, so the
    // rendered line is a budget statement, never the old generic failure.
    #[test]
    fn timeout_renders_the_budget_it_outran() {
        let line = FeedError::Timeout.to_string();
        assert!(line.contains("timed out after 10s"), "got: {line}");
        assert!(!line.contains("failed"));
    }

    #[test]
    fn exit_carries_stderr_first_line() {
        let status = std::process::Command::new("true")
            .status()
            .expect("true runs");
        let note = stderr_note(b"unreadable store: graph.json\nsecond line", &status);
        assert_eq!(note, "unreadable store: graph.json");
        let rendered = FeedError::Exit(note).to_string();
        assert!(rendered.contains("unreadable store"), "got: {rendered}");
    }
}
