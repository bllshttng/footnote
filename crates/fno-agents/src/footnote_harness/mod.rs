//! `-H footnote`: footnote's own turn loop over a borrowed model endpoint.
//! Every model call and every tool call is written to the session's
//! transcript before it acts, so a reader (or a resume) never guesses.

pub mod hooks;
pub mod model;
pub mod resume;
pub mod source;
pub mod tools;
pub mod transcript;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::claude_ask::AskOutcome;
use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry, RegistryEntry};
use transcript::Writer;

/// Inline tool output past this spills to `spill/<uid>.out`.
const SPILL_AT: usize = 8 * 1024;
/// The text a tool result shows the model, and keeps in its record.
const MODEL_TEXT_CAP: usize = 30_000;
const COMPACT_PERCENT: u64 = 80;

const BASE_PROMPT: &str = "You are a coding agent run by footnote. Work in the given cwd with the tools provided. \
Read before you edit. Keep changes small and verify them. When the task is done, reply with a short summary and stop calling tools.";

/// Spend so far and the caps, checked before every model call.
#[derive(Debug, Default, Clone)]
pub struct Budget {
    pub wall_cap: Option<Duration>,
    pub cost_cap_usd: Option<f64>,
    /// USD per million tokens: (input, output, cache_read).
    pub price: Option<(f64, f64, f64)>,
}

pub struct Session {
    pub w: Writer,
    pub records: Vec<Value>,
    pub cwd: PathBuf,
    pub hooks: hooks::HookHost,
    pub client: model::Client,
    pub budget: Budget,
    pub started: Instant,
    pub node: Option<String>,
    pub plan_path: Option<String>,
    pub model: String,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub state: &'static str,
    pub reason: String,
}

fn term(state: &'static str, reason: impl Into<String>) -> Terminal {
    Terminal { state, reason: reason.into() }
}

impl Session {
    fn fno_id(&self) -> String {
        self.w.session_id().to_string()
    }

    fn append(&mut self, ty: &str, data: Value) -> Result<(), String> {
        let rec = self.w.append(ty, data, false)?;
        self.records.push(rec);
        Ok(())
    }

    fn count(&self, ty: &str) -> usize {
        self.records.iter().filter(|r| r["type"] == ty).count()
    }

    fn totals(&self) -> Value {
        let (mut i, mut o, mut c) = (0u64, 0u64, 0f64);
        for r in self.records.iter().filter(|r| r["type"] == "usage") {
            i += r["data"]["input_tokens"].as_u64().unwrap_or(0);
            o += r["data"]["output_tokens"].as_u64().unwrap_or(0);
            c += r["data"]["cost_usd"].as_f64().unwrap_or(0.0);
        }
        json!({"input_tokens": i, "output_tokens": o, "cost_usd": c,
            "turns": self.count("turn_context"), "tool_calls": self.count("tool_call")})
    }

    fn hook_payload(&self, event: &str) -> Value {
        json!({"hook_event_name": event, "session_id": self.fno_id(), "cwd": self.cwd,
            "transcript_path": self.w.transcript_path()})
    }

    fn run_hook(&mut self, event: &str, extra: Value) -> hooks::Outcome {
        let mut payload = self.hook_payload(event);
        if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        let fno_id = self.fno_id();
        let out = self.hooks.run(event, &payload, &self.cwd, &fno_id);
        for e in &out.errors {
            self.w.diag("warn", &format!("hook: {e}"));
        }
        out
    }

    /// The user's input, after UserPromptSubmit and the SKILL.md loader.
    /// Returns false when a hook blocked it.
    pub fn submit(&mut self, text: &str, origin: &str) -> Result<bool, String> {
        let out = self.run_hook("UserPromptSubmit", json!({"prompt": text}));
        if let Some((reason, rule)) = &out.deny {
            self.append("hook_decision", json!({"event": "UserPromptSubmit", "verdict": "block",
                "rule": rule, "reason": reason}))?;
            return Ok(false);
        }
        self.append("user_input", json!({"text": text, "origin": origin}))?;
        if let Some(body) = expand_skill(text, self.hooks.root()) {
            self.append("user_input", json!({"text": body, "origin": "skill"}))?;
        }
        for c in out.context {
            self.append("user_input", json!({"text": c, "origin": "hook"}))?;
        }
        Ok(true)
    }

    /// Settle every call a dead writer left without a result.
    pub fn settle_pending(&mut self) -> Result<(), String> {
        for p in resume::pending(&self.records) {
            match p {
                resume::Pending::Run(call) => self.execute(&call)?,
                resume::Pending::Unknown(call) => {
                    let text = format!(
                        "The outcome of this {} call is unknown: the previous run stopped after it started and before it reported. It was not re-run; check its effect before repeating it.",
                        call["name"].as_str().unwrap_or("tool")
                    );
                    self.append("tool_result", json!({"tool_call_uid": call["tool_call_uid"],
                        "disposition": "unknown", "is_error": true, "model_text": text,
                        "size": 0, "sha256": null, "spill_path": null}))?;
                }
            }
        }
        Ok(())
    }

    fn check_budget(&self, estimated_input_tokens: u64) -> Option<Terminal> {
        if let Some(cap) = self.budget.wall_cap {
            if self.started.elapsed() >= cap {
                return Some(term("budget", format!("wall cap {}s reached", cap.as_secs())));
            }
        }
        if let (Some(cap), Some((pin, _, _))) = (self.budget.cost_cap_usd, self.budget.price) {
            let spent = self.totals()["cost_usd"].as_f64().unwrap_or(0.0);
            let next = estimated_input_tokens as f64 * pin / 1e6;
            if spent + next > cap {
                return Some(term("budget", format!("dollar cap ${cap:.4}: spent ${spent:.4}, next call ~${next:.4}")));
            }
        }
        None
    }

    fn maybe_compact(&mut self) -> Result<(), String> {
        let Some(last) = self.records.iter().rev().find(|r| r["type"] == "usage") else {
            return Ok(());
        };
        let d = &last["data"];
        let used = d["input_tokens"].as_u64().unwrap_or(0)
            + d["cache_read_tokens"].as_u64().unwrap_or(0)
            + d["cache_write_tokens"].as_u64().unwrap_or(0);
        let window = crate::context_window::window_for_model(&self.model);
        if crate::context_window::used_percent(used, window).unwrap_or(0) < COMPACT_PERCENT {
            return Ok(());
        }
        // Compacting twice on one reading would drop the tail it just kept.
        if self.records.last().is_some_and(|r| r["type"] == "compaction") {
            return Ok(());
        }
        let data = resume::compact(&self.records, self.plan_path.as_deref(), self.node.as_deref(), used);
        self.append("compaction", data)
    }

    /// Run turns until the model ends its turn and Stop allows, or a budget,
    /// an interrupt or an error ends the run.
    pub fn run_turns(&mut self) -> Result<Terminal, String> {
        loop {
            if crate::subprocess_ask::ask_interrupted() {
                return Ok(term("interrupted", "SIGINT"));
            }
            let turn = self.count("turn_context") + 1;
            self.append("turn_context", json!({"turn": turn, "model": self.model, "cwd": self.cwd,
                "spent_usd": self.totals()["cost_usd"], "wall_secs": self.started.elapsed().as_secs(),
                "cost_cap_usd": self.budget.cost_cap_usd, "wall_cap_secs": self.budget.wall_cap.map(|d| d.as_secs())}))?;
            self.maybe_compact()?;
            let body = resume::build_request(&self.records);
            let estimate = (body.len() / 4) as u64;
            if let Some(t) = self.check_budget(estimate) {
                return Ok(t);
            }
            let msg_count = resume::messages(&self.records).len();
            let ep = &self.client.endpoint;
            self.append("model_request", json!({"turn": turn, "provider_id": ep.provider_id,
                "endpoint_host": ep.host(), "wire": ep.wire.as_str(), "route": ep.route,
                "requested_model": self.model, "message_count": msg_count,
                "estimated_input_tokens": estimate,
                "body_sha256": transcript::sha256_hex(body.as_bytes())}))?;
            let mut attempts = Vec::new();
            let sent = self.client.send(&body, &mut |a| attempts.push(a));
            for a in attempts.drain(..) {
                let rec = self.w.append("model_attempt", a, true)?;
                self.records.push(rec);
            }
            let reply = match sent {
                Ok(r) => r,
                Err(e) => {
                    self.w.diag("error", &e);
                    return Ok(term("error", e));
                }
            };
            let resp = reply.response;
            self.append("model_response", json!({"turn": turn, "provider_response_id": resp["id"],
                "reported_model": resp["model"], "content": resp["content"], "stop_reason": resp["stop_reason"],
                "latency_ms": reply.latency_ms as u64, "attempts": reply.attempts}))?;
            let u = &resp["usage"];
            let (i, o) = (u["input_tokens"].as_u64().unwrap_or(0), u["output_tokens"].as_u64().unwrap_or(0));
            let (cr, cw) = (
                u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            );
            let cost = self
                .budget
                .price
                .map(|(pi, po, pc)| (i as f64 * pi + cw as f64 * pi + o as f64 * po + cr as f64 * pc) / 1e6);
            self.append("usage", json!({"turn": turn, "provider_response_id": resp["id"],
                "input_tokens": i, "output_tokens": o, "cache_read_tokens": cr, "cache_write_tokens": cw,
                "cost_usd": cost}))?;

            let uses: Vec<Value> = resp["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
                .cloned()
                .collect();
            if uses.is_empty() {
                let last = last_text(&self.records);
                let out = self.run_hook("Stop", json!({"last_assistant_message": last, "stop_hook_active": turn > 1}));
                if let Some((reason, rule)) = out.deny {
                    self.append("hook_decision", json!({"event": "Stop", "verdict": "block", "rule": rule, "reason": reason}))?;
                    self.append("user_input", json!({"text": reason, "origin": "hook"}))?;
                    continue;
                }
                return Ok(term("done", ""));
            }
            let mut seen = std::collections::HashSet::new();
            let mut calls = Vec::new();
            for (n, b) in uses.iter().enumerate() {
                let mut wire_id = b["id"].as_str().unwrap_or("").to_string();
                if wire_id.is_empty() || !seen.insert(wire_id.clone()) {
                    wire_id = format!("footnote_{turn}_{n}");
                }
                let raw = b["raw_input"].as_str().map(str::to_string).unwrap_or_else(|| b["input"].to_string());
                let parse_error = (b["input"].is_null()).then_some("tool input is not valid JSON");
                let call = json!({"turn": turn, "tool_call_uid": transcript::mint()?,
                    "provider_tool_call_id": wire_id, "name": b["name"], "raw_input": raw,
                    "input": b["input"], "parse_error": parse_error});
                self.append("tool_call", call.clone())?;
                calls.push(call);
            }
            for call in calls {
                self.execute(&call)?;
            }
        }
    }

    /// PreToolUse, then start, run and record one tool call.
    fn execute(&mut self, call: &Value) -> Result<(), String> {
        let uid = call["tool_call_uid"].clone();
        let name = call["name"].as_str().unwrap_or("").to_string();
        let input = call["input"].clone();
        let effect = tools::effect_capable(&name);
        let class = crate::effect_gate::map_tool_call(&name, &input).map(|m| m.effect_class);
        if !call["parse_error"].is_null() {
            self.append("effect_decision", json!({"tool_call_uid": uid, "effect_class": class,
                "effect_capable": effect, "verdict": "deny", "rule": "parse_error", "principal": "policy"}))?;
            return self.result(&uid, "none", true, format!("{}: {}", call["parse_error"], call["raw_input"]).as_bytes());
        }
        let out = self.run_hook("PreToolUse", json!({"tool_name": name, "tool_input": input, "tool_use_id": uid}));
        let (verdict, rule, reason) = match &out.deny {
            Some((reason, rule)) => ("deny", rule.clone(), Some(reason.clone())),
            None => ("allow", "default-allow".to_string(), None),
        };
        self.append("effect_decision", json!({"tool_call_uid": uid, "effect_class": class,
            "effect_capable": effect, "verdict": verdict, "rule": rule, "reason": reason, "principal": "policy"}))?;
        if let Some(reason) = reason {
            return self.result(&uid, "none", true, format!("Denied by a footnote hook: {reason}").as_bytes());
        }
        self.append("tool_start", json!({"tool_call_uid": uid}))?;
        let fno_id = self.fno_id();
        let (text, is_error) = if name == "Skill" {
            match load_skill(self.hooks.root(), input["skill"].as_str().unwrap_or(""), input["args"].as_str().unwrap_or("")) {
                Some(body) => (body, false),
                None => ("no such skill".to_string(), true),
            }
        } else {
            tools::run(&name, &input, &tools::Ctx { cwd: &self.cwd, fno_id: &fno_id })
        };
        let killed = crate::subprocess_ask::ask_interrupted();
        let disposition = match (effect, killed) {
            (false, _) => "none",
            (true, true) => "unknown",
            (true, false) => "applied",
        };
        self.result(&uid, disposition, is_error, text.as_bytes())?;
        let post = self.run_hook("PostToolUse", json!({"tool_name": name, "tool_input": input,
            "tool_use_id": uid, "tool_response": cap(&text, 4096)}));
        for c in post.context {
            self.append("hook_decision", json!({"event": "PostToolUse", "verdict": "allow", "context": c}))?;
            self.append("user_input", json!({"text": c, "origin": "hook"}))?;
        }
        Ok(())
    }

    fn result(&mut self, uid: &Value, disposition: &str, is_error: bool, out: &[u8]) -> Result<(), String> {
        let text = String::from_utf8_lossy(out);
        let (spill_path, sha) = if out.len() > SPILL_AT {
            let (p, _, sha) = self.w.spill(uid.as_str().unwrap_or("call"), out)?;
            (Some(p.to_string_lossy().into_owned()), sha)
        } else {
            (None, transcript::sha256_hex(out))
        };
        let mut model_text = cap(&text, MODEL_TEXT_CAP);
        if let Some(p) = &spill_path {
            if out.len() > MODEL_TEXT_CAP {
                model_text.push_str(&format!("\n[output truncated: {} bytes total; full output at {p}]", out.len()));
            }
        }
        self.append("tool_result", json!({"tool_call_uid": uid, "disposition": disposition,
            "is_error": is_error, "model_text": model_text, "size": out.len(), "sha256": sha,
            "spill_path": spill_path}))
    }

    pub fn finish(&mut self, t: &Terminal) -> Result<(), String> {
        let mut data = self.totals();
        data["state"] = json!(t.state);
        data["reason"] = json!(t.reason);
        self.append("terminal", data)
    }
}

fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn last_text(records: &[Value]) -> String {
    records
        .iter()
        .rev()
        .find(|r| r["type"] == "model_response")
        .and_then(|r| r["data"]["content"].as_array())
        .map(|b| b.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

fn load_skill(root: Option<&Path>, verb: &str, args: &str) -> Option<String> {
    let verb = verb.strip_prefix("fno:").unwrap_or(verb);
    if verb.is_empty() || verb.contains('/') || verb.contains("..") {
        return None;
    }
    let dir = root?.join("skills").join(verb);
    let body = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    Some(format!("Base directory for this skill: {}\n\n{body}\n\nARGUMENTS: {args}", dir.display()))
}

/// A first token `/fno:<verb>` or `$fno:<verb>` expands to that SKILL.md.
pub fn expand_skill(text: &str, root: Option<&Path>) -> Option<String> {
    let trimmed = text.trim_start();
    let (tok, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
    let (verb, namespaced) = crate::provider::parse_verb_token(tok)?;
    if !namespaced {
        return None;
    }
    load_skill(root, verb, rest.trim())
}

/// The system prompt: the base text plus the cwd's AGENTS.md.
fn system_prompt(cwd: &Path) -> String {
    match std::fs::read_to_string(cwd.join("AGENTS.md")) {
        Ok(a) => format!("{BASE_PROMPT}\n\ncwd: {}\n\n# AGENTS.md\n\n{a}", cwd.display()),
        Err(_) => format!("{BASE_PROMPT}\n\ncwd: {}", cwd.display()),
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// USD per million tokens from the models.dev cache: the route provider's
/// entry first, then the first provider listing the model.
pub fn price_for(cache: &Path, provider: Option<&str>, model: &str) -> Option<(f64, f64, f64)> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(cache).ok()?).ok()?;
    let from = |p: &Value| -> Option<(f64, f64, f64)> {
        let c = &p["models"][model]["cost"];
        let i = c["input"].as_f64()?;
        Some((i, c["output"].as_f64().unwrap_or(i), c["cache_read"].as_f64().unwrap_or(i)))
    };
    if let Some(hit) = provider.and_then(|p| from(&doc[p])) {
        return Some(hit);
    }
    doc.as_object()?.values().find_map(from)
}

fn price_cache() -> PathBuf {
    transcript::sessions_root()
        .parent()
        .map(|p| p.join("cache").join("models-dev.json"))
        .unwrap_or_default()
}

/// Budget caps and the claimed plan from the target manifest, when one exists.
fn manifest_fields(manifest: Option<&Path>) -> (Option<f64>, Option<u64>, Option<String>, Option<String>) {
    let Some(content) = manifest.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return (None, None, None, None);
    };
    let f = |k: &str| crate::loopcheck::scan_manifest_field(&content, k);
    (
        f("budget_cost_cap_usd").and_then(|v| v.parse().ok()).filter(|v: &f64| *v > 0.0),
        f("budget_wall_clock_cap_minutes").and_then(|v| v.parse().ok()).filter(|v: &u64| *v > 0),
        f("plan_path"),
        f("graph_node_id"),
    )
}

pub struct Launch<'a> {
    pub root: PathBuf,
    pub cwd: &'a Path,
    pub model: &'a str,
    pub endpoint: model::Endpoint,
    pub plugin_root: Option<PathBuf>,
    pub timeout: Option<Duration>,
    pub node: Option<&'a str>,
    pub parent_session_id: Option<String>,
    pub price_cache: PathBuf,
    /// The target manifest that carries the caps and the plan, if any.
    pub manifest: Option<PathBuf>,
}

fn budget_for(l: &Launch) -> Result<(Budget, Option<String>, Option<String>), String> {
    let (cost_cap, wall_min, plan_path, node) = manifest_fields(l.manifest.as_deref());
    let wall = [l.timeout, wall_min.map(|m| Duration::from_secs(m * 60))].into_iter().flatten().min();
    let price = price_for(&l.price_cache, l.endpoint.provider_id.as_deref(), l.model);
    if cost_cap.is_some() && price.is_none() {
        return Err(format!(
            "a dollar cap is set but {} has no price in {}; refresh the models.dev cache or drop the cap",
            l.model,
            l.price_cache.display()
        ));
    }
    Ok((Budget { wall_cap: wall, cost_cap_usd: cost_cap, price }, plan_path, node))
}

/// Start a new session: mint, header, SessionStart. Returns the session.
pub fn start(l: Launch) -> Result<Session, String> {
    let (budget, plan_path, manifest_node) = budget_for(&l)?;
    let fno_id = transcript::mint()?;
    let dir = transcript::session_dir(&l.root, l.cwd, &fno_id);
    let mut w = Writer::create(&dir, &fno_id)?;
    w.add_secret(&l.endpoint.key);
    let node = l.node.map(str::to_string).or(manifest_node);
    let header = w.append("header", json!({
        "session_id": fno_id, "cwd": l.cwd, "harness": "footnote",
        "commit": git(l.cwd, &["rev-parse", "HEAD"]),
        "branch": git(l.cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "fno_version": crate::version::version_json(),
        "parent_session_id": l.parent_session_id, "forked_from": null,
        "node": node, "plugin_root": l.plugin_root,
        "model": l.model, "wire": l.endpoint.wire.as_str(),
        "system_prompt": system_prompt(l.cwd),
    }), false)?;
    let mut s = Session {
        w,
        records: vec![header],
        cwd: l.cwd.to_path_buf(),
        hooks: hooks::HookHost::load(l.plugin_root),
        client: model::Client { endpoint: l.endpoint, max_time: Duration::from_secs(600), backoff: Duration::from_secs(1) },
        budget,
        started: Instant::now(),
        node,
        plan_path,
        model: l.model.to_string(),
    };
    let out = s.run_hook("SessionStart", json!({"source": "startup"}));
    s.append("hook_decision", json!({"event": "SessionStart", "verdict": "allow", "context": out.context.join("\n\n")}))?;
    Ok(s)
}

/// Reopen a session for one more input: lock, read, settle dead calls.
pub fn reopen(dir: &Path, fno_id: &str, l: Launch) -> Result<Session, String> {
    let (budget, plan_path, manifest_node) = budget_for(&l)?;
    let (mut w, records) = Writer::open(dir, fno_id)?;
    w.add_secret(&l.endpoint.key);
    let header = records.iter().find(|r| r["type"] == "header").map(|r| r["data"].clone()).unwrap_or_default();
    let mut s = Session {
        w,
        records,
        cwd: l.cwd.to_path_buf(),
        hooks: hooks::HookHost::load(l.plugin_root),
        client: model::Client { endpoint: l.endpoint, max_time: Duration::from_secs(600), backoff: Duration::from_secs(1) },
        budget,
        started: Instant::now(),
        node: header["node"].as_str().map(str::to_string).or(manifest_node),
        plan_path,
        model: header["model"].as_str().unwrap_or(l.model).to_string(),
    };
    s.settle_pending()?;
    Ok(s)
}

/// Feed one input and run to a terminal state.
pub fn drive(s: &mut Session, input: &str, origin: &str) -> Result<Terminal, String> {
    let t = if s.submit(input, origin)? {
        s.run_turns()?
    } else {
        term("refused", "UserPromptSubmit blocked the input")
    };
    s.finish(&t)?;
    Ok(t)
}

fn launch_for<'a>(cwd: &'a Path, model: &'a str, timeout: Option<Duration>, node: Option<&'a str>) -> Result<Launch<'a>, String> {
    let endpoint = model::resolve_endpoint(cwd, &|k| std::env::var(k).ok())?;
    Ok(Launch {
        root: transcript::sessions_root(),
        cwd,
        model,
        endpoint,
        plugin_root: crate::provider::plugin_root(),
        timeout,
        node,
        parent_session_id: std::env::var("FNO_HARNESS_SESSION_ID").ok().filter(|v| !v.is_empty()),
        price_cache: price_cache(),
        manifest: crate::state_path::resolve("target-state", cwd),
    })
}

fn outcome(code: i32, stdout: String, stderr: String) -> AskOutcome {
    AskOutcome { stdout, stderr, exit_code: code }
}

fn reply_for(s: &Session, t: &Terminal) -> AskOutcome {
    let text = last_text(&s.records);
    let line = format!(
        "session_id={} state={} transcript={}",
        s.fno_id(),
        t.state,
        s.w.transcript_path().display()
    );
    let code = if t.state == "done" { 0 } else { 1 };
    let stderr = if code == 0 { String::new() } else { format!("{line} {}\n", t.reason) };
    outcome(code, format!("{text}\n{line}\n"), stderr)
}

fn mark_row(home: &AgentsHome, name: &str, s: &Session) {
    let reported = s
        .records
        .iter()
        .find(|r| r["type"] == "model_response")
        .and_then(|r| r["data"]["reported_model"].as_str())
        .map(str::to_string);
    let _ = update_registry(&home.registry_json(), |reg| {
        let Some(e) = reg.find_mut(name) else { return false };
        e.status = crate::AgentStatus::Exited;
        e.pid = None;
        e.last_message_at = Some(crate::daemon::now_rfc3339_like());
        if e.model_name.is_none() {
            e.model_name = reported.clone();
        }
        true
    });
}

/// `fno agents spawn -H footnote --substrate headless`: register the row,
/// run the loop in this process, mark the row Exited at the terminal state.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
    timeout: Option<Duration>,
    node: Option<&str>,
) -> AskOutcome {
    if let Err(msg) = crate::claude_ask::validate_spawn_inputs(name, from_name) {
        return outcome(2, String::new(), format!("{msg}\n"));
    }
    let Some(model) = model.filter(|m| !m.is_empty()) else {
        return outcome(2, String::new(), "-H footnote needs -m <model>\n".into());
    };
    match load_registry(&home.registry_json()) {
        Ok(reg) if reg.find(name).is_some() => {
            return outcome(2, String::new(), format!("agent {name} already exists; use 'fno agents rm {name}' first\n"));
        }
        Err(e) => return outcome(12, String::new(), format!("registry read failed: {e}\n")),
        Ok(_) => {}
    }
    let mut s = match launch_for(cwd, model, timeout, node).and_then(start) {
        Ok(s) => s,
        Err(e) => return outcome(2, String::new(), format!("{e}\n")),
    };
    let fno_id = s.fno_id();
    let mut entry = RegistryEntry {
        name: name.to_string(),
        short_id: fno_id.clone(),
        provider: Some("footnote".into()),
        harness: Some("footnote".into()),
        substrate: Some("headless".into()),
        session_id: Some(fno_id.clone()),
        fno_id: Some(fno_id.clone()),
        requested_model: Some(model.to_string()),
        route_provider_id: s.client.endpoint.provider_id.clone(),
        node: s.node.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        origin: Some("spawn".into()),
        status: crate::AgentStatus::Busy,
        pid: Some(std::process::id()),
        created_at: crate::daemon::now_rfc3339_like(),
        log_path: Some(s.w.transcript_path().to_string_lossy().to_string()),
        ..RegistryEntry::new(Some(fno_id.clone()), crate::spawn_lineage::ambient_lineage())
    };
    entry.account_record_id = Some("default".into());
    match update_registry(&home.registry_json(), |reg| {
        if reg.find(name).is_some() {
            return false;
        }
        reg.entries.push(entry.clone());
        true
    }) {
        Ok(true) => {}
        Ok(false) => return outcome(2, String::new(), format!("agent {name} already exists\n")),
        Err(e) => return outcome(12, String::new(), format!("registry write failed: {e}\n")),
    }
    let prompt = if message.is_empty() { "hello" } else { message };
    let res = drive(&mut s, prompt, "operator");
    mark_row(home, name, &s);
    match res {
        Ok(t) => reply_for(&s, &t),
        Err(e) => outcome(12, String::new(), format!("footnote: {e}\n")),
    }
}

/// `fno agents ask <name>` on a footnote row: resume by name, then the input.
/// Returns None for any other harness (fall through).
pub fn maybe_run_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    let reg = load_registry(&home.registry_json()).ok()?;
    let entry = reg.find_name_or_full_session_id(name)?;
    if entry.harness_name() != "footnote" {
        return None;
    }
    let fno_id = entry.fno_id.clone().or_else(|| entry.harness_session_id.clone())?;
    let row_name = entry.name.clone();
    let cwd = PathBuf::from(&entry.cwd);
    let model = entry.requested_model.clone().unwrap_or_default();
    let message = params["message"].as_str().unwrap_or("").to_string();
    let timeout = params["timeout"].as_u64().map(Duration::from_secs);
    let Some(dir) = transcript::find_session_dir(&transcript::sessions_root(), &fno_id) else {
        eprintln!("fno-agents: footnote session {fno_id} has no transcript on disk");
        return Some(2);
    };
    let mut s = match launch_for(&cwd, &model, timeout, None).and_then(|l| reopen(&dir, &fno_id, l)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fno-agents: {e}");
            return Some(2);
        }
    };
    let res = drive(&mut s, &message, "operator");
    mark_row(home, &row_name, &s);
    match res {
        Ok(t) => {
            let o = reply_for(&s, &t);
            print!("{}", o.stdout);
            eprint!("{}", o.stderr);
            Some(o.exit_code)
        }
        Err(e) => {
            eprintln!("fno-agents: {e}");
            Some(12)
        }
    }
}

/// The roster's `footnote` row. Spawn and ask never reach these argv
/// methods: the client runs the loop in-process (`dispatch_once`,
/// `maybe_run_ask`), so they render the capability table's forms only.
pub struct FootnoteProvider;

impl crate::provider::Provider for FootnoteProvider {
    fn name(&self) -> &'static str {
        "footnote"
    }

    fn create_argv(&self, _ctx: &crate::provider::CreateContext) -> Vec<String> {
        crate::harness_capabilities::render_session_argv("footnote", "headless_create", None)
            .expect("embedded footnote headless-create capability")
    }

    fn resume_argv(&self, ctx: &crate::provider::ResumeContext) -> Vec<String> {
        crate::harness_capabilities::render_session_argv("footnote", "headless_resume", Some(&ctx.session_id))
            .expect("embedded footnote headless-resume capability")
    }

    fn parse_stream_event(&self, chunk: &str) -> crate::ParsedEvent {
        crate::ParsedEvent::Unknown { raw: chunk.to_string() }
    }

    fn reachability(
        &self,
        _entry: &crate::provider::AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, crate::provider::ReachabilityProbeError> {
        Err(crate::provider::ReachabilityProbeError::new(
            "footnote",
            "the loop runs inside a client process; read the transcript's terminal record",
        ))
    }
}

#[cfg(test)]
mod tests;
