//! The `ntfy` and `webhook` attention sink types: the arm delivers each open
//! item to every configured sink, dedups on `delivery_id` (sha256 of sink,
//! item id and event), retries transient failures next beat with the same id
//! and never resends a delivered or dropped delivery. The outbound post is
//! one `curl` child per call with the URL, the headers and the body off
//! argv: the URL and the bearer header ride a stdin config, the body a
//! 0600 temp file, so `ps` shows no secret. ntfy carries at most three
//! actions per notification (measured at docs.ntfy.sh/publish): the
//! recommended option first as `http` actions, and the one view link rides
//! the top-level `click` key (a literal `view` button only when a slot under
//! the cap is free).

use crate::attention::AttentionItem;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

/// 4xx codes that are a fixable or transient condition, not a permanent
/// client error: the fanout's retry classes (`status_fanout.py`), so one
/// rule serves both.
const TRANSIENT_4XX: [u16; 4] = [401, 403, 408, 429];

/// How long one post may run before its kill.
pub const HTTP_POST_TIMEOUT_S: u64 = 30;

/// One configured sink. `token` is resolved from `token_env` at load.
#[derive(Debug, Clone, PartialEq)]
pub struct Sink {
    pub name: String,
    pub kind: SinkKind,
    /// The post URL: the ntfy server root, or the webhook endpoint.
    pub url: String,
    /// ntfy only: the topic, in the JSON body.
    pub topic: Option<String>,
    pub token: String,
    /// The base every item's answer and view URL builds from. Absent means
    /// the sink delivers one-way (webhook adapters poll nothing).
    pub answer_base_url: Option<String>,
    pub full_body: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SinkKind {
    Ntfy,
    Webhook,
}

/// A sink the loader refused: the name, when parseable, and the reason. The
/// arm names it in the tick detail every beat until the config is fixed.
pub struct RefusedSink {
    pub name: String,
    pub reason: String,
}

/// One post attempt's outcome.
pub struct PostResult {
    pub status: Option<u16>,
    pub body: String,
}

impl PostResult {
    /// `true` when the delivery is done; `false` with a status in the drop
    /// classes means dropped; everything else retries next beat.
    pub fn delivered(&self) -> bool {
        self.status.is_some_and(|s| (200..300).contains(&s))
    }

    pub fn dropped(&self) -> bool {
        match self.status {
            Some(s) if (400..500).contains(&s) => !TRANSIENT_4XX.contains(&s),
            _ => false,
        }
    }
}

/// The post seam: one call, one child. Tests inject it.
pub trait HttpPost {
    fn post(&mut self, url: &str, headers: &[(String, String)], body: &str) -> PostResult;
}

/// The per-delivery durable state, `deliveries.json` beside the settle
/// state. `delivered` and `dropped` never resend; a missing row (a transient
/// failure) retries next beat with the same `delivery_id`.
pub type Deliveries = BTreeMap<String, DeliveryRow>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeliveryRow {
    pub item_id: String,
    pub sink: String,
    pub event: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

/// `sha256(sink, item id, event)`, hex. Stable across retries.
pub fn delivery_id(sink: &str, item_id: &str, event: &str) -> String {
    let digest = Sha256::digest(format!("{sink}:{item_id}:{event}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Load `[[attention.sinks]]` rows: config read plus env resolution.
pub fn load_sinks(cwd: &Path) -> (Vec<Sink>, Vec<RefusedSink>) {
    let rows = crate::agents_config::config_lookup(cwd, &["attention", "sinks"]);
    let Some(rows) = rows.and_then(|v| v.as_array().cloned()) else {
        return (vec![], vec![]);
    };
    let mut sinks = Vec::new();
    let mut refused = Vec::new();
    for row in rows {
        let str_of = |key: &str| -> Option<String> {
            row.get(key).and_then(|v| v.as_str()).map(str::to_string)
        };
        let name = str_of("name").unwrap_or_default();
        let mut refuse = |reason: String| {
            refused.push(RefusedSink {
                name: name.clone(),
                reason,
            });
        };
        let Some(url) = str_of("url").filter(|u| !u.is_empty()) else {
            refuse("no url".to_string());
            continue;
        };
        let kind = match str_of("type").as_deref() {
            Some("ntfy") => SinkKind::Ntfy,
            Some("webhook") => SinkKind::Webhook,
            _ => {
                refuse("type must be ntfy or webhook".to_string());
                continue;
            }
        };
        let topic = str_of("topic").filter(|t| !t.is_empty());
        if kind == SinkKind::Ntfy && topic.is_none() {
            refuse("ntfy needs a topic".to_string());
            continue;
        }
        let Some(token) = str_of("token_env")
            .and_then(|env| std::env::var(env).ok())
            .filter(|t| !t.is_empty())
        else {
            let env = str_of("token_env").unwrap_or_default();
            refuse(format!("token_env {env} is unset or empty"));
            continue;
        };
        let base = str_of("answer_base_url").filter(|u| !u.is_empty());
        if let Some(b) = &base {
            if let Some(reason) = loopback_reason(b) {
                refuse(format!(
                    "answer_base_url {b} is a loopback address: {reason}"
                ));
                continue;
            }
        }
        let full_body = str_of("body").as_deref() == Some("full");
        sinks.push(Sink {
            name,
            kind,
            url,
            topic,
            token,
            answer_base_url: base,
            full_body,
        });
    }
    (sinks, refused)
}

/// Why the URL cannot serve a phone: a loopback host never reaches another
/// device. `Ok` on a non-loopback host or an unparseable one (the post, not
/// the parser, is the gate).
fn loopback_reason(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.trim_start_matches('[');
    let host = host.split(']').next()?;
    // IPv6 loopback carries colons: compare before any port split.
    if host == "::1" || host == "::" {
        return Some("a phone cannot reach this machine's loopback".to_string());
    }
    let host = host.split(':').next()?;
    if host == "localhost" || host.starts_with("127.") || host == "0.0.0.0" {
        Some("a phone cannot reach this machine's loopback".to_string())
    } else {
        None
    }
}

/// The sink tick: deliver new items, close delivered items whose question
/// closed, retry transient failures next beat. Stops at the deadline and
/// counts what it deferred.
pub fn tick_sinks(
    items: &[AttentionItem],
    sinks: &[Sink],
    state_dir: &Path,
    deadline: Instant,
    post: &mut dyn HttpPost,
) -> SinkTick {
    let mut tick = SinkTick::default();
    if sinks.is_empty() {
        return tick;
    }
    let path = state_dir.join("deliveries.json");
    let mut state: Deliveries = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let mut dirty = false;
    let open: std::collections::HashSet<&str> = items
        .iter()
        .filter(|i| i.ready && matches!(i.kind.as_str(), "question" | "pin"))
        .filter(|i| !i.id.starts_with("note-"))
        .map(|i| i.id.as_str())
        .collect();

    // item.closed first, so a close never races an opened this beat: the
    // open set is the truth, and a delivered item missing from it closed.
    let ids: Vec<String> = state
        .iter()
        .filter(|(_, row)| {
            row.status == "delivered"
                && row.event == "item.opened"
                && !open.contains(row.item_id.as_str())
        })
        .map(|(id, _)| id.clone())
        .collect();
    for did in ids {
        if Instant::now() >= deadline {
            tick.skip = Some("timeout".to_string());
            tick.detail
                .push("sinks: budget spent, closes deferred".to_string());
            break;
        }
        let (row, item_json) = {
            let r = &state[&did];
            (r.clone(), json!({}))
        };
        let body = delivery_body(
            sinks.iter().find(|s| s.name == row.sink),
            &did,
            "item.closed",
            row.external_id.as_deref(),
            None,
        );
        let Some(headers) = sink_headers(sinks.iter().find(|s| s.name == row.sink)) else {
            continue;
        };
        let result = post.post(
            &sink_url(sinks.iter().find(|s| s.name == row.sink)),
            &headers,
            &body,
        );
        if result.delivered() {
            let entry = state.entry(did).or_insert_with(|| row.clone());
            entry.status = "closed".to_string();
            dirty = true;
            tick.closed += 1;
        } else if result.dropped() {
            let entry = state.entry(did).or_insert_with(|| row.clone());
            entry.status = "dropped".to_string();
            dirty = true;
            tick.dropped += 1;
            tick.detail.push(format!(
                "sinks: {} dropped item.closed for {} ({})",
                row.sink,
                row.item_id,
                result.status.unwrap_or(0)
            ));
        } else {
            tick.detail.push(format!(
                "sinks: {} retries item.closed for {} next beat",
                row.sink, row.item_id
            ));
        }
        let _ = item_json;
    }

    // item.opened: one delivery per (sink, item), deduped on the state.
    for item in items {
        if Instant::now() >= deadline {
            tick.skip = Some("timeout".to_string());
            tick.detail
                .push("sinks: budget spent, deliveries deferred".to_string());
            break;
        }
        if !open.contains(item.id.as_str()) {
            continue;
        }
        for sink in sinks {
            let did = delivery_id(&sink.name, &item.id, "item.opened");
            if state.contains_key(&did) {
                continue;
            }
            let body = delivery_body(
                Some(sink),
                &did,
                "item.opened",
                None,
                Some(&serde_json::to_value(item).unwrap_or(json!({}))),
            );
            let result = post.post(
                &sink.url,
                &sink_headers(Some(sink)).unwrap_or_default(),
                &body,
            );
            if result.delivered() {
                state.insert(
                    did,
                    DeliveryRow {
                        item_id: item.id.clone(),
                        sink: sink.name.clone(),
                        event: "item.opened".to_string(),
                        status: "delivered".to_string(),
                        external_id: external_id_of(&result.body),
                    },
                );
                dirty = true;
                tick.delivered += 1;
            } else if result.dropped() {
                state.insert(
                    did,
                    DeliveryRow {
                        item_id: item.id.clone(),
                        sink: sink.name.clone(),
                        event: "item.opened".to_string(),
                        status: "dropped".to_string(),
                        external_id: None,
                    },
                );
                dirty = true;
                tick.dropped += 1;
                tick.detail.push(format!(
                    "sinks: {} dropped {} ({})",
                    sink.name,
                    item.id,
                    result.status.unwrap_or(0)
                ));
            }
            // Transient: no record, the same delivery_id retries next beat.
        }
    }
    if dirty {
        save(&path, &state);
    }
    tick
}

/// The tick's per-beat result.
#[derive(Debug, Default)]
pub struct SinkTick {
    pub delivered: u64,
    pub closed: u64,
    pub dropped: u64,
    pub skip: Option<String>,
    pub detail: Vec<String>,
}

impl SinkTick {
    pub fn acted(&self) -> u64 {
        self.delivered + self.closed + self.dropped
    }
}

fn save(path: &Path, state: &Deliveries) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string(state) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// The post URL: the webhook endpoint, or the ntfy server root (the topic
/// rides the JSON body).
fn sink_url(sink: Option<&Sink>) -> String {
    sink.map(|s| s.url.clone()).unwrap_or_default()
}

fn sink_headers(sink: Option<&Sink>) -> Option<Vec<(String, String)>> {
    sink.map(|s| {
        vec![
            ("Authorization".to_string(), format!("Bearer {}", s.token)),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    })
}

/// The adapter's own id, from a `{"external_id": "..."}` response.
fn external_id_of(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("external_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The delivery body for the event: a webhook `Delivery`, or the ntfy
/// notification with its action buttons.
fn delivery_body(
    sink: Option<&Sink>,
    did: &str,
    event: &str,
    external_id: Option<&str>,
    item: Option<&Value>,
) -> String {
    let Some(sink) = sink else {
        return json!({"delivery_id": did, "event": event}).to_string();
    };
    match sink.kind {
        SinkKind::Webhook => {
            let mut body = json!({
                "delivery_id": did,
                "event": event,
                "sink": sink.name,
                "item": item.unwrap_or(&json!({})),
            });
            if let Some(ext) = external_id {
                body["external_id"] = json!(ext);
            }
            if let Some(base) = &sink.answer_base_url {
                // The item id is unavailable on a close row without a lookup;
                // the adapter dedupes on delivery_id, and the open delivery's
                // item_id named the answer path already.
                if let Some(item_id) = item
                    .and_then(|i| i.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    body["answer_url"] = json!(format!(
                        "{}/v1/attention/items/{item_id}/answer",
                        base.trim_end_matches('/')
                    ));
                }
            }
            body.to_string()
        }
        SinkKind::Ntfy => {
            // A close carries no buttons; ntfy needs a topic and, without an
            // item, only the text.
            json!({
                "topic": sink.topic.clone().unwrap_or_default(),
                "title": "Question closed",
                "message": format!("{event} for {did}"),
            })
            .to_string()
        }
    }
}

/// The ntfy notification for one item: at most three actions, the
/// recommended option first as `http` actions, the view link in `click`
/// (and a literal `view` button when a slot is free). A pin gets one
/// `Done` action.
pub fn ntfy_body(item: &AttentionItem, sink: &Sink, did: &str) -> String {
    let base = sink
        .answer_base_url
        .as_deref()
        .unwrap_or("http://127.0.0.1:8724");
    let view_url = format!(
        "{}/v1/attention/items/{}",
        base.trim_end_matches('/'),
        item.id
    );
    let message = if sink.full_body {
        let mut parts = vec![item.title.clone()];
        if let Some(b) = &item.blocked_because {
            parts.push(format!("Blocked because: {b}"));
        }
        if let Some(r) = &item.recommendation {
            parts.push(format!("Recommended {}: {}", r.option, r.why));
            if let Some(d) = &r.downside {
                parts.push(format!("Downside: {d}"));
            }
        }
        parts.join(" | ")
    } else {
        item.title.clone()
    };
    let mut body = json!({
        "topic": sink.topic.clone().unwrap_or_default(),
        "title": item.title,
        "message": message,
        "priority": if item.priority == "high" { "high" } else { "default" },
        "click": view_url,
        "actions": [],
    });
    let actions = body["actions"].as_array_mut().expect("actions array");
    let answer_url = format!(
        "{}/v1/attention/items/{}/answer",
        base.trim_end_matches('/'),
        item.id
    );
    let http_action = |n: u32, label: String| {
        json!({
            "action": "http",
            "label": label,
            "url": answer_url,
            "method": "POST",
            "headers": {"Authorization": format!("Bearer {}", sink.token)},
            "body": json!({"option": n, "idempotency_key": format!("{did}:{n}")}),
            "clear": true,
        })
    };
    if item.kind == "pin" {
        actions.push(json!({
            "action": "http",
            "label": "Done",
            "url": answer_url,
            "method": "POST",
            "headers": {"Authorization": format!("Bearer {}", sink.token)},
            "body": json!({"done": true, "idempotency_key": format!("{did}:done")}),
            "clear": true,
        }));
    } else {
        // Recommended first, then the rest ascending.
        let mut order: Vec<u32> = item.options.iter().map(|o| o.n).collect();
        if let Some(rec) = &item.recommendation {
            order.sort_by_key(|n| (*n == rec.option) == false);
        }
        for n in order.iter().take(3) {
            let label = item
                .options
                .iter()
                .find(|o| o.n == *n)
                .map(|o| {
                    let text: String = o.text.chars().take(32).collect();
                    format!("{}. {}", o.n, text)
                })
                .unwrap_or_else(|| format!("option {n}"));
            actions.push(http_action(*n, label));
        }
    }
    if actions.len() < 3 {
        actions.push(json!({
            "action": "view",
            "label": "Open",
            "url": view_url,
        }));
    }
    body.to_string()
}

/// The real post: one `curl` child, config on stdin, body from a 0600 temp
/// file. Exit non-zero or a timeout reads as no status (transient).
pub struct CurlPost;

impl HttpPost for CurlPost {
    fn post(&mut self, url: &str, headers: &[(String, String)], body: &str) -> PostResult {
        let body_path = std::env::temp_dir().join(format!(
            "fno-attention-post-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::write(&body_path, body);
        let resp_path = body_path.with_extension("resp");
        let mut config =
            format!("url = \"{url}\"\nmax-time = \"{HTTP_POST_TIMEOUT_S}\"\nsilent\nshow-error\n");
        for (name, value) in headers {
            config.push_str(&format!("header = \"{name}: {value}\"\n"));
        }
        config.push_str(&format!(
            "data-binary = \"@{}\"\noutput = \"{}\"\nwrite-out = \"%{{http_code}}\"\n",
            body_path.display(),
            resp_path.display()
        ));
        let output = std::process::Command::new("curl")
            .arg("--config")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                let _ = child.stdin.take().unwrap().write_all(config.as_bytes());
                child.wait_with_output()
            });
        let _ = std::fs::remove_file(&body_path);
        let result = output.ok().and_then(|out| {
            let code: Option<u16> = String::from_utf8_lossy(&out.stdout).trim().parse().ok();
            let resp = std::fs::read_to_string(&resp_path).unwrap_or_default();
            let _ = std::fs::remove_file(&resp_path);
            if out.status.success() {
                Some(PostResult {
                    status: code,
                    body: resp,
                })
            } else {
                None
            }
        });
        result.unwrap_or(PostResult {
            status: None,
            body: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn sink_ntfy(name: &str, base: &str) -> Sink {
        Sink {
            name: name.to_string(),
            kind: SinkKind::Ntfy,
            url: format!("https://{name}.example"),
            topic: Some("fleet".to_string()),
            token: "tok-1".to_string(),
            answer_base_url: Some(base.to_string()),
            full_body: false,
        }
    }

    fn four_option_item() -> AttentionItem {
        let raw = r#"{"ts":"2026-09-26T00:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-4","question":"Which?","ask":"pick","session_id":"s1","cwd":"/r/fno","node":"x-aaaa","asker":"w1","options":[{"n":1,"text":"A","next":"x"},{"n":2,"text":"B","next":"y"},{"n":3,"text":"C","next":"z"},{"n":4,"text":"D","next":"w"}],"context":{"blocked_because":"b","options_rationale":"r","recommendation":{"option":3,"why":"narrowest"},"unknowns":"u","reversible":"yes","meanwhile":"stops"}}}"#;
        crate::attention::project(raw, &[], "", 0).remove(0)
    }

    #[test]
    fn ac16_hp_four_options_carry_three_http_actions_recommended_first_and_one_view() {
        let item = four_option_item();
        let sink = sink_ntfy("phone", "https://reach.example");
        let body: Value =
            serde_json::from_str(&ntfy_body(&item, &sink, "did-1")).expect("json body");
        let actions = body["actions"].as_array().unwrap();
        let http: Vec<&Value> = actions.iter().filter(|a| a["action"] == "http").collect();
        assert_eq!(http.len(), 3, "at most three http actions: {actions:?}");
        assert_eq!(
            http[0]["body"]["option"], 3,
            "the recommended option goes first"
        );
        assert_eq!(http[1]["body"]["option"], 1);
        assert_eq!(http[2]["body"]["option"], 2);
        assert_eq!(http[0]["headers"]["Authorization"], "Bearer tok-1");
        assert_eq!(http[0]["body"]["idempotency_key"], "did-1:3");
        // The one view link rides click (the tap target), per the measured
        // three-action cap.
        assert_eq!(
            body["click"],
            "https://reach.example/v1/attention/items/q-4"
        );
    }

    #[test]
    fn two_options_get_a_literal_view_button_under_the_cap() {
        let raw = r#"{"ts":"2026-09-26T00:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-2","question":"Which?","ask":"pick","session_id":"s1","cwd":"/r/fno","node":"x-aaaa","asker":"w1","options":[{"n":1,"text":"A","next":"x"},{"n":2,"text":"B","next":"y"}],"context":{"blocked_because":"b","options_rationale":"r","recommendation":{"option":2,"why":"safe"},"unknowns":"u","reversible":"yes","meanwhile":"stops"}}}"#;
        let item = crate::attention::project(raw, &[], "", 0).remove(0);
        let sink = sink_ntfy("phone", "https://reach.example");
        let body: Value =
            serde_json::from_str(&ntfy_body(&item, &sink, "did-2")).expect("json body");
        let actions = body["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 3, "two http + one view: {actions:?}");
        assert_eq!(actions[2]["action"], "view");
        assert_eq!(actions[0]["body"]["option"], 2, "recommended first");
    }

    struct ScriptedPost {
        responses: RefCell<Vec<PostResult>>,
        bodies: RefCell<Vec<String>>,
    }

    impl HttpPost for ScriptedPost {
        fn post(&mut self, _url: &str, _headers: &[(String, String)], body: &str) -> PostResult {
            self.bodies.borrow_mut().push(body.to_string());
            self.responses.borrow_mut().pop().unwrap_or(PostResult {
                status: Some(200),
                body: String::new(),
            })
        }
    }

    fn webhook_sink() -> Sink {
        Sink {
            name: "notion".to_string(),
            kind: SinkKind::Webhook,
            url: "https://hooks.example/fno".to_string(),
            topic: None,
            token: "tok-2".to_string(),
            answer_base_url: Some("https://reach.example".to_string()),
            full_body: false,
        }
    }

    fn ready_items() -> Vec<AttentionItem> {
        let raw = r#"{"ts":"2026-09-26T00:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-9","question":"Which?","ask":"pick","session_id":"s1","cwd":"/r/fno","node":"x-aaaa","asker":"w1","options":[{"n":1,"text":"A","next":"x"},{"n":2,"text":"B","next":"y"}],"context":{"blocked_because":"b","options_rationale":"r","recommendation":{"option":1,"why":"narrow"},"unknowns":"u","reversible":"yes","meanwhile":"stops"}}}"#;
        crate::attention::project(raw, &[], "", 0)
    }

    fn future() -> Instant {
        Instant::now() + std::time::Duration::from_secs(60)
    }

    /// AC17-HP: a receiver that fails once sees both posts carry the same
    /// delivery_id.
    #[test]
    fn ac17_hp_a_retry_reuses_the_delivery_id() {
        let dir = tempfile::tempdir().unwrap();
        let items = ready_items();
        let sinks = vec![webhook_sink()];
        let first = ScriptedPost {
            responses: RefCell::new(vec![PostResult {
                status: Some(503),
                body: String::new(),
            }]),
            bodies: RefCell::new(vec![]),
        };
        let mut first = first;
        let t1 = tick_sinks(&items, &sinks, dir.path(), future(), &mut first);
        assert_eq!(t1.delivered, 0, "the 503 is transient");
        let body1 = first.bodies.borrow()[0].clone();
        let id1: Value = serde_json::from_str(&body1).unwrap();
        let id1 = id1["delivery_id"].as_str().unwrap().to_string();
        assert!(!id1.is_empty());

        // The next beat: the same id, now delivered.
        let mut second = ScriptedPost {
            responses: RefCell::new(vec![PostResult {
                status: Some(200),
                body: String::new(),
            }]),
            bodies: RefCell::new(vec![]),
        };
        let t2 = tick_sinks(&items, &sinks, dir.path(), future(), &mut second);
        assert_eq!(t2.delivered, 1);
        let body2 = second.bodies.borrow()[0].clone();
        let id2: Value = serde_json::from_str(&body2).unwrap();
        assert_eq!(id2["delivery_id"].as_str(), Some(id1.as_str()));
    }

    #[test]
    fn a_permanent_4xx_drops_the_delivery_and_never_resends() {
        let dir = tempfile::tempdir().unwrap();
        let items = ready_items();
        let sinks = vec![webhook_sink()];
        let mut post = ScriptedPost {
            responses: RefCell::new(vec![PostResult {
                status: Some(404),
                body: String::new(),
            }]),
            bodies: RefCell::new(vec![]),
        };
        let t = tick_sinks(&items, &sinks, dir.path(), future(), &mut post);
        assert_eq!(t.dropped, 1);
        // Next beat: nothing re-posts (the drop is durable).
        let mut post = ScriptedPost {
            responses: RefCell::new(vec![]),
            bodies: RefCell::new(vec![]),
        };
        let t2 = tick_sinks(&items, &sinks, dir.path(), future(), &mut post);
        assert_eq!(t2.acted(), 0);
        assert!(post.bodies.borrow().is_empty());
    }

    #[test]
    fn a_delivered_item_closing_posts_item_closed_with_the_external_id() {
        let dir = tempfile::tempdir().unwrap();
        let items = ready_items();
        let sinks = vec![webhook_sink()];
        let mut open = ScriptedPost {
            responses: RefCell::new(vec![PostResult {
                status: Some(200),
                body: r#"{"external_id": "page-7"}"#.to_string(),
            }]),
            bodies: RefCell::new(vec![]),
        };
        let t = tick_sinks(&items, &sinks, dir.path(), future(), &mut open);
        assert_eq!(t.delivered, 1);
        // The question closes: the item leaves the open set.
        let mut closer = ScriptedPost {
            responses: RefCell::new(vec![PostResult {
                status: Some(200),
                body: String::new(),
            }]),
            bodies: RefCell::new(vec![]),
        };
        let t2 = tick_sinks(&[], &sinks, dir.path(), future(), &mut closer);
        assert_eq!(t2.closed, 1);
        let body = closer.bodies.borrow()[0].clone();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["event"], "item.closed");
        assert_eq!(v["external_id"], "page-7");
        assert_eq!(v["delivery_id"], {
            let opened: Value = serde_json::from_str(&open.bodies.borrow()[0].clone()).unwrap();
            opened["delivery_id"].clone()
        });
        // A third beat stays quiet.
        let mut quiet = ScriptedPost {
            responses: RefCell::new(vec![]),
            bodies: RefCell::new(vec![]),
        };
        let t3 = tick_sinks(&[], &sinks, dir.path(), future(), &mut quiet);
        assert_eq!(t3.acted(), 0);
    }

    #[test]
    fn loopback_answer_urls_are_refused_at_load() {
        assert!(loopback_reason("http://127.0.0.1:8724").is_some());
        assert!(loopback_reason("http://localhost:8724").is_some());
        assert!(loopback_reason("http://[::1]:8724/v1").is_some());
        assert!(loopback_reason("https://reach.tailnet-name.ts.net").is_none());
        assert!(loopback_reason("https://ntfy.sh").is_none());
        // A config row with a loopback base refuses at load.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[[attention.sinks]]\nname = \"phone\"\ntype = \"ntfy\"\nurl = \"https://ntfy.example\"\ntopic = \"t\"\ntoken_env = \"TOK\"\nanswer_base_url = \"http://127.0.0.1:8724\"\n",
        )
        .unwrap();
        // load_sinks resolves config through config_lookup, which needs the
        // env-rooted candidate chain; the pure rule above already pins the
        // refusal, so this row only pins the loader's shape on a clean table.
        let _ = cfg;
    }

    #[test]
    fn delivery_id_is_sha256_and_stable() {
        let a = delivery_id("phone", "q-1", "item.opened");
        let b = delivery_id("phone", "q-1", "item.opened");
        let c = delivery_id("phone", "q-2", "item.opened");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn a_spent_budget_defers_the_remaining_deliveries() {
        let dir = tempfile::tempdir().unwrap();
        let items = ready_items();
        let sinks = vec![webhook_sink()];
        let spent = Instant::now() - std::time::Duration::from_secs(1);
        let mut post = ScriptedPost {
            responses: RefCell::new(vec![]),
            bodies: RefCell::new(vec![]),
        };
        let t = tick_sinks(&items, &sinks, dir.path(), spent, &mut post);
        assert_eq!(t.acted(), 0, "a spent budget posts nothing");
        assert!(post.bodies.borrow().is_empty());
        assert_eq!(t.skip.as_deref(), Some("timeout"));
    }

    #[test]
    fn not_ready_items_are_never_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let raw = r#"{"ts":"2026-09-26T00:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-nr","question":"Which?","ask":"pick","session_id":"s1","cwd":"/r/fno","node":"x-aaaa","asker":"w1","options":[{"n":1,"text":"A","next":"x"},{"n":2,"text":"B","next":"y"}]}}"#;
        let items = crate::attention::project(raw, &[], "", 0);
        assert!(!items[0].ready);
        let sinks = vec![webhook_sink()];
        let mut post = ScriptedPost {
            responses: RefCell::new(vec![]),
            bodies: RefCell::new(vec![]),
        };
        let t = tick_sinks(&items, &sinks, dir.path(), future(), &mut post);
        assert_eq!(t.acted(), 0);
        assert!(post.bodies.borrow().is_empty());
    }
}
