//! `-H footnote`: footnote's own turn loop over a borrowed model endpoint.
//! Every model call and every tool call is written to the session's
//! transcript before it acts, so a reader (or a resume) never guesses.
//!
//! The supervisor (fno-agents) launches the `footnote` binary with one
//! `LaunchSpec` on stdin and keeps the registry row; this crate never links
//! the supervisor. Shared file and effect contracts come from the fno crate.

pub mod hooks;
pub mod model;
pub mod resume;
pub mod tools;
pub mod transcript;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fno::footnote_transcript::{LaunchSpec, LAUNCH_SPEC_V};
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
    pub context_window: u64,
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Flag SIGINT instead of dying, so a running tool is reaped and the
/// transcript records the interrupt. Only an atomic store runs in the
/// handler, which is async-signal-safe.
pub fn catch_sigint() {
    // SAFETY: on_sigint only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}

/// True once SIGINT arrived (the supervisor forwards Ctrl-C to this group).
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Whole-percent context use, rounded half up.
fn used_percent(used: u64, window: u64) -> Option<u64> {
    (window != 0).then(|| ((used as u128 * 100 + window as u128 / 2) / window as u128) as u64)
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub state: &'static str,
    pub reason: String,
}

fn term(state: &'static str, reason: impl Into<String>) -> Terminal {
    Terminal {
        state,
        reason: reason.into(),
    }
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
            self.append(
                "hook_decision",
                json!({"event": "UserPromptSubmit", "verdict": "block",
                "rule": rule, "reason": reason}),
            )?;
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
                    self.append(
                        "tool_result",
                        json!({"tool_call_uid": call["tool_call_uid"],
                        "disposition": "unknown", "is_error": true, "model_text": text,
                        "size": 0, "sha256": null, "spill_path": null}),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn check_budget(&self, estimated_input_tokens: u64) -> Option<Terminal> {
        if let Some(cap) = self.budget.wall_cap {
            if self.started.elapsed() >= cap {
                return Some(term(
                    "budget",
                    format!("wall cap {}s reached", cap.as_secs()),
                ));
            }
        }
        if let (Some(cap), Some((pin, _, _))) = (self.budget.cost_cap_usd, self.budget.price) {
            let spent = self.totals()["cost_usd"].as_f64().unwrap_or(0.0);
            let next = estimated_input_tokens as f64 * pin / 1e6;
            if spent + next > cap {
                return Some(term(
                    "budget",
                    format!("dollar cap ${cap:.4}: spent ${spent:.4}, next call ~${next:.4}"),
                ));
            }
        }
        None
    }

    fn maybe_compact(&mut self) -> Result<(), String> {
        let Some(at) = self.records.iter().rposition(|r| r["type"] == "usage") else {
            return Ok(());
        };
        // One compaction per usage reading: a failed call after a compaction
        // leaves the same reading, and compacting again would drop its tail.
        if self.records[at..].iter().any(|r| r["type"] == "compaction") {
            return Ok(());
        }
        let d = &self.records[at]["data"];
        let used = d["input_tokens"].as_u64().unwrap_or(0)
            + d["cache_read_tokens"].as_u64().unwrap_or(0)
            + d["cache_write_tokens"].as_u64().unwrap_or(0);
        if used_percent(used, self.context_window).unwrap_or(0) < COMPACT_PERCENT {
            return Ok(());
        }
        let data = resume::compact(
            &self.records,
            self.plan_path.as_deref(),
            self.node.as_deref(),
            used,
        );
        self.append("compaction", data)
    }

    /// Run turns until the model ends its turn and Stop allows, or a budget,
    /// an interrupt or an error ends the run.
    pub fn run_turns(&mut self) -> Result<Terminal, String> {
        loop {
            if interrupted() {
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
            self.append(
                "model_request",
                json!({"turn": turn, "provider_id": ep.provider_id,
                "endpoint_host": ep.host(), "wire": ep.wire.as_str(), "route": ep.route,
                "requested_model": self.model, "message_count": msg_count,
                "estimated_input_tokens": estimate,
                "body_sha256": transcript::sha256_hex(body.as_bytes())}),
            )?;
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
            let (i, o) = (
                u["input_tokens"].as_u64().unwrap_or(0),
                u["output_tokens"].as_u64().unwrap_or(0),
            );
            let (cr, cw) = (
                u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            );
            let cost = self.budget.price.map(|(pi, po, pc)| {
                (i as f64 * pi + cw as f64 * pi + o as f64 * po + cr as f64 * pc) / 1e6
            });
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
                // Claude's meaning: this turn already continues a Stop block.
                let active = self
                    .records
                    .iter()
                    .rev()
                    .take_while(|r| {
                        !(r["type"] == "user_input" && r["data"]["origin"] == "operator")
                    })
                    .any(|r| r["type"] == "hook_decision" && r["data"]["event"] == "Stop");
                let out = self.run_hook(
                    "Stop",
                    json!({"last_assistant_message": last, "stop_hook_active": active}),
                );
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
                let raw = b["raw_input"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| b["input"].to_string());
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
        let class = fno::effect_map::map_tool_call(&name, &input).map(|m| m.effect_class);
        if !call["parse_error"].is_null() {
            self.append("effect_decision", json!({"tool_call_uid": uid, "effect_class": class,
                "effect_capable": effect, "verdict": "deny", "rule": "parse_error", "principal": "policy"}))?;
            return self.result(
                &uid,
                "none",
                true,
                format!("{}: {}", call["parse_error"], call["raw_input"]).as_bytes(),
            );
        }
        let out = self.run_hook(
            "PreToolUse",
            json!({"tool_name": name, "tool_input": input, "tool_use_id": uid}),
        );
        let (verdict, rule, reason) = match &out.deny {
            Some((reason, rule)) => ("deny", rule.clone(), Some(reason.clone())),
            None => ("allow", "default-allow".to_string(), None),
        };
        self.append("effect_decision", json!({"tool_call_uid": uid, "effect_class": class,
            "effect_capable": effect, "verdict": verdict, "rule": rule, "reason": reason, "principal": "policy"}))?;
        if let Some(reason) = reason {
            return self.result(
                &uid,
                "none",
                true,
                format!("Denied by a footnote hook: {reason}").as_bytes(),
            );
        }
        self.append("tool_start", json!({"tool_call_uid": uid}))?;
        let fno_id = self.fno_id();
        let (text, is_error) = if name == "Skill" {
            match load_skill(
                self.hooks.root(),
                input["skill"].as_str().unwrap_or(""),
                input["args"].as_str().unwrap_or(""),
            ) {
                Some(body) => (body, false),
                None => ("no such skill".to_string(), true),
            }
        } else {
            tools::run(
                &name,
                &input,
                &tools::Ctx {
                    cwd: &self.cwd,
                    fno_id: &fno_id,
                },
            )
        };
        let killed = interrupted();
        let disposition = match (effect, killed) {
            (false, _) => "none",
            (true, true) => "unknown",
            (true, false) => "applied",
        };
        self.result(&uid, disposition, is_error, text.as_bytes())?;
        let post = self.run_hook(
            "PostToolUse",
            json!({"tool_name": name, "tool_input": input,
            "tool_use_id": uid, "tool_response": cap(&text, 4096)}),
        );
        for c in post.context {
            self.append(
                "hook_decision",
                json!({"event": "PostToolUse", "verdict": "allow", "context": c}),
            )?;
            self.append("user_input", json!({"text": c, "origin": "hook"}))?;
        }
        Ok(())
    }

    fn result(
        &mut self,
        uid: &Value,
        disposition: &str,
        is_error: bool,
        out: &[u8],
    ) -> Result<(), String> {
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
                model_text.push_str(&format!(
                    "\n[output truncated: {} bytes total; full output at {p}]",
                    out.len()
                ));
            }
        }
        self.append(
            "tool_result",
            json!({"tool_call_uid": uid, "disposition": disposition,
            "is_error": is_error, "model_text": model_text, "size": out.len(), "sha256": sha,
            "spill_path": spill_path}),
        )
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
        .map(|b| {
            b.iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn load_skill(root: Option<&Path>, verb: &str, args: &str) -> Option<String> {
    let verb = verb.strip_prefix("fno:").unwrap_or(verb);
    if verb.is_empty() || verb.contains('/') || verb.contains("..") {
        return None;
    }
    let dir = root?.join("skills").join(verb);
    let body = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    Some(format!(
        "Base directory for this skill: {}\n\n{body}\n\nARGUMENTS: {args}",
        dir.display()
    ))
}

/// A first token `/fno:<verb>` or `$fno:<verb>` expands to that SKILL.md.
pub fn expand_skill(text: &str, root: Option<&Path>) -> Option<String> {
    let trimmed = text.trim_start();
    let (tok, rest) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    let (verb, namespaced) = parse_verb_token(tok)?;
    if !namespaced {
        return None;
    }
    load_skill(root, verb, rest.trim())
}

/// The verb-token shape (copied from fno-agents' `provider::parse_verb_token`):
/// a leading `/` or `$`, no second `/`, an optional `fno:` namespace, and a
/// lowercase-word remainder. Returns `(verb, namespaced)`.
fn parse_verb_token(tok: &str) -> Option<(&str, bool)> {
    let sigil = tok.chars().next()?;
    if sigil != '/' && sigil != '$' {
        return None;
    }
    let body = &tok[1..];
    if body.is_empty() || body.contains('/') {
        return None;
    }
    let (verb, namespaced) = match body.strip_prefix("fno:") {
        Some(rest) => (rest, true),
        None => (body, false),
    };
    let mut it = verb.chars();
    match it.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return None,
    }
    if !verb
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return None;
    }
    Some((verb, namespaced))
}

/// The system prompt: the base text plus the cwd's AGENTS.md.
fn system_prompt(cwd: &Path) -> String {
    match std::fs::read_to_string(cwd.join("AGENTS.md")) {
        Ok(a) => format!(
            "{BASE_PROMPT}\n\ncwd: {}\n\n# AGENTS.md\n\n{a}",
            cwd.display()
        ),
        Err(_) => format!("{BASE_PROMPT}\n\ncwd: {}", cwd.display()),
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// USD per million tokens from the models.dev cache: the route provider's
/// entry first, then the first provider listing the model.
pub fn price_for(cache: &Path, provider: Option<&str>, model: &str) -> Option<(f64, f64, f64)> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(cache).ok()?).ok()?;
    let from = |p: &Value| -> Option<(f64, f64, f64)> {
        let c = &p["models"][model]["cost"];
        let i = c["input"].as_f64()?;
        Some((
            i,
            c["output"].as_f64().unwrap_or(i),
            c["cache_read"].as_f64().unwrap_or(i),
        ))
    };
    if let Some(hit) = provider.and_then(|p| from(&doc[p])) {
        return Some(hit);
    }
    doc.as_object()?.values().find_map(from)
}

/// One launch, as the supervisor resolved it: the session id and dir, the
/// endpoint, and the caps and plan it read from the target manifest.
pub struct Launch<'a> {
    pub fno_id: String,
    pub session_dir: PathBuf,
    pub cwd: &'a Path,
    pub model: &'a str,
    pub endpoint: model::Endpoint,
    pub plugin_root: Option<PathBuf>,
    pub timeout: Option<Duration>,
    pub node: Option<&'a str>,
    pub parent_session_id: Option<String>,
    pub price_cache: PathBuf,
    pub context_window: u64,
    pub cost_cap_usd: Option<f64>,
    pub wall_cap_minutes: Option<u64>,
    pub plan_path: Option<String>,
}

fn budget_for(l: &Launch) -> Result<Budget, String> {
    let cost_cap = l.cost_cap_usd.filter(|v| *v > 0.0);
    let wall_min = l.wall_cap_minutes.filter(|v| *v > 0);
    let wall = [l.timeout, wall_min.map(|m| Duration::from_secs(m * 60))]
        .into_iter()
        .flatten()
        .min();
    let price = price_for(&l.price_cache, l.endpoint.provider_id.as_deref(), l.model);
    if cost_cap.is_some() && price.is_none() {
        return Err(format!(
            "a dollar cap is set but {} has no price in {}; refresh the models.dev cache or drop the cap",
            l.model,
            l.price_cache.display()
        ));
    }
    Ok(Budget {
        wall_cap: wall,
        cost_cap_usd: cost_cap,
        price,
    })
}

/// Start a new session under the supervisor's id: header, SessionStart.
pub fn start(l: Launch) -> Result<Session, String> {
    let budget = budget_for(&l)?;
    let fno_id = l.fno_id.clone();
    let mut w = Writer::create(&l.session_dir, &fno_id)?;
    w.add_secret(&l.endpoint.key);
    let node = l.node.map(str::to_string);
    let header = w.append(
        "header",
        json!({
            "session_id": fno_id, "cwd": l.cwd, "harness": "footnote",
            "commit": git(l.cwd, &["rev-parse", "HEAD"]),
            "branch": git(l.cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "fno_version": fno::version::version_json(),
            "parent_session_id": l.parent_session_id, "forked_from": null,
            "node": node, "plugin_root": l.plugin_root,
            "model": l.model, "wire": l.endpoint.wire.as_str(),
            "system_prompt": system_prompt(l.cwd),
        }),
        false,
    )?;
    let mut s = Session {
        w,
        records: vec![header],
        cwd: l.cwd.to_path_buf(),
        hooks: hooks::HookHost::load(l.plugin_root),
        client: model::Client {
            endpoint: l.endpoint,
            max_time: Duration::from_secs(600),
            backoff: Duration::from_secs(1),
        },
        budget,
        started: Instant::now(),
        node,
        plan_path: l.plan_path,
        model: l.model.to_string(),
        context_window: l.context_window,
    };
    let out = s.run_hook("SessionStart", json!({"source": "startup"}));
    s.append(
        "hook_decision",
        json!({"event": "SessionStart", "verdict": "allow", "context": out.context.join("\n\n")}),
    )?;
    Ok(s)
}

/// Reopen a session for one more input: lock, read, settle dead calls.
pub fn reopen(l: Launch) -> Result<Session, String> {
    let budget = budget_for(&l)?;
    let (mut w, records) = Writer::open(&l.session_dir, &l.fno_id)?;
    w.add_secret(&l.endpoint.key);
    let header = records
        .iter()
        .find(|r| r["type"] == "header")
        .map(|r| r["data"].clone())
        .unwrap_or_default();
    let mut s = Session {
        w,
        records,
        cwd: l.cwd.to_path_buf(),
        hooks: hooks::HookHost::load(l.plugin_root),
        client: model::Client {
            endpoint: l.endpoint,
            max_time: Duration::from_secs(600),
            backoff: Duration::from_secs(1),
        },
        budget,
        started: Instant::now(),
        node: header["node"]
            .as_str()
            .map(str::to_string)
            .or(l.node.map(str::to_string)),
        plan_path: l.plan_path,
        model: header["model"].as_str().unwrap_or(l.model).to_string(),
        context_window: l.context_window,
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

/// What one launch prints and exits with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

fn outcome(code: i32, stdout: String, stderr: String) -> Outcome {
    Outcome {
        stdout,
        stderr,
        exit_code: code,
    }
}

fn reply_for(s: &Session, t: &Terminal) -> Outcome {
    let text = last_text(&s.records);
    let line = format!(
        "session_id={} state={} transcript={}",
        s.fno_id(),
        t.state,
        s.w.transcript_path().display()
    );
    let code = if t.state == "done" { 0 } else { 1 };
    let stderr = if code == 0 {
        String::new()
    } else {
        format!("{line} {}\n", t.reason)
    };
    outcome(code, format!("{text}\n{line}\n"), stderr)
}

fn launch_from(spec: &LaunchSpec) -> Result<Launch<'_>, String> {
    let e = &spec.endpoint;
    let wire = match e.wire.as_str() {
        "anthropic" => model::Wire::Anthropic,
        "openai" => model::Wire::OpenAi,
        other => return Err(format!("unknown wire {other:?} in the launch spec")),
    };
    Ok(Launch {
        fno_id: spec.fno_id.clone(),
        session_dir: spec.session_dir.clone(),
        cwd: &spec.cwd,
        model: &spec.model,
        endpoint: model::Endpoint {
            base_url: e.base_url.clone(),
            key: e.key.clone(),
            bearer: e.bearer,
            wire,
            provider_id: e.provider_id.clone(),
            route: if e.route == "config" { "config" } else { "env" },
        },
        plugin_root: spec.plugin_root.clone(),
        timeout: spec.timeout_secs.map(Duration::from_secs),
        node: spec.node.as_deref(),
        parent_session_id: spec.parent_session_id.clone(),
        price_cache: spec.price_cache.clone(),
        context_window: spec.context_window,
        cost_cap_usd: spec.cost_cap_usd,
        wall_cap_minutes: spec.wall_cap_minutes,
        plan_path: spec.plan_path.clone(),
    })
}

/// Run one launch to a terminal state. Exit codes: 0 done, 1 any other
/// terminal state, 2 a refusal before the session ran, 12 a transcript or
/// runtime error mid-run.
pub fn run(spec: &LaunchSpec) -> Outcome {
    if spec.v != LAUNCH_SPEC_V {
        return outcome(
            2,
            String::new(),
            format!(
                "footnote: launch spec v{} but this binary reads v{LAUNCH_SPEC_V}; run `fno doctor update --rust` so both binaries match\n",
                spec.v
            ),
        );
    }
    let opened = launch_from(spec).and_then(|l| match spec.mode.as_str() {
        "create" => start(l),
        "resume" => reopen(l),
        other => Err(format!("unknown launch mode {other:?}")),
    });
    let mut s = match opened {
        Ok(s) => s,
        Err(e) => return outcome(2, String::new(), format!("footnote: {e}\n")),
    };
    let prompt = match (spec.mode.as_str(), spec.message.is_empty()) {
        ("create", true) => "hello",
        _ => spec.message.as_str(),
    };
    match drive(&mut s, prompt, "operator") {
        Ok(t) => reply_for(&s, &t),
        Err(e) => outcome(12, String::new(), format!("footnote: {e}\n")),
    }
}

#[cfg(test)]
mod tests;
