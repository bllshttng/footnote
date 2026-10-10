//! The borrowed model layer: one internal message shape (Anthropic content
//! blocks) over two wires, sent through curl with the whole request on
//! stdin so the key never reaches argv.

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    OpenAi,
}

impl Wire {
    pub fn as_str(self) -> &'static str {
        match self {
            Wire::Anthropic => "anthropic",
            Wire::OpenAi => "openai",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub key: String,
    pub bearer: bool,
    pub wire: Wire,
    pub provider_id: Option<String>,
    pub route: &'static str,
}

impl Endpoint {
    pub fn host(&self) -> String {
        let rest = self.base_url.split("://").nth(1).unwrap_or(&self.base_url);
        rest.split('/').next().unwrap_or("").to_string()
    }

    fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.wire {
            Wire::Anthropic => format!("{base}/v1/messages"),
            Wire::OpenAi => format!("{base}/chat/completions"),
        }
    }
}

/// Build the wire body from the internal request
/// `{model, max_tokens, system, messages, tools}`.
pub fn wire_body(wire: Wire, req: &Value) -> Value {
    match wire {
        Wire::Anthropic => req.clone(),
        Wire::OpenAi => to_openai(req),
    }
}

fn to_openai(req: &Value) -> Value {
    let mut msgs = vec![json!({"role": "system", "content": req["system"]})];
    for m in req["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        let blocks = m["content"].as_array().cloned().unwrap_or_default();
        let text: String = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if role == "assistant" {
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| {
                    json!({"id": b["id"], "type": "function", "function": {
                    "name": b["name"], "arguments": b["input"].to_string()}})
                })
                .collect();
            let mut out = json!({"role": "assistant", "content": text});
            if !calls.is_empty() {
                out["tool_calls"] = json!(calls);
            }
            msgs.push(out);
            continue;
        }
        for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
            msgs.push(
                json!({"role": "tool", "tool_call_id": b["tool_use_id"], "content": b["content"]}),
            );
        }
        if !text.is_empty() {
            msgs.push(json!({"role": "user", "content": text}));
        }
    }
    let tools: Vec<Value> = req["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            json!({"type": "function", "function": {
            "name": t["name"], "description": t["description"], "parameters": t["input_schema"]}})
        })
        .collect();
    json!({"model": req["model"], "max_tokens": req["max_tokens"], "messages": msgs, "tools": tools})
}

/// Normalize a wire response into `{id, model, content, stop_reason, usage}`
/// with Anthropic block and usage names.
pub fn normalize(wire: Wire, body: &Value) -> Value {
    if wire == Wire::Anthropic {
        return body.clone();
    }
    let choice = &body["choices"][0];
    let msg = &choice["message"];
    let mut content = Vec::new();
    if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
        content.push(json!({"type": "text", "text": t}));
    }
    for c in msg["tool_calls"].as_array().into_iter().flatten() {
        let raw = c["function"]["arguments"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let input = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
        content.push(
            json!({"type": "tool_use", "id": c["id"], "name": c["function"]["name"],
            "input": input, "raw_input": raw}),
        );
    }
    let stop = match choice["finish_reason"].as_str() {
        Some("tool_calls") => "tool_use",
        Some("length") => "max_tokens",
        _ => "end_turn",
    };
    let u = &body["usage"];
    json!({
        "id": body["id"], "model": body["model"], "content": content, "stop_reason": stop,
        "usage": {
            "input_tokens": u["prompt_tokens"].as_u64().unwrap_or(0)
                .saturating_sub(u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0)),
            "output_tokens": u["completion_tokens"],
            "cache_read_input_tokens": u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
        },
    })
}

pub struct Client {
    pub endpoint: Endpoint,
    pub max_time: Duration,
    pub backoff: Duration,
}

pub struct Reply {
    pub response: Value,
    pub latency_ms: u128,
    pub attempts: u32,
}

fn retryable(status: u16, body: &Value) -> Option<&'static str> {
    if status == 429 {
        return Some("rate_limited");
    }
    if status >= 500 {
        return Some("server_error");
    }
    if body["error"]["type"] == "overloaded_error" {
        return Some("overloaded");
    }
    let code = &body["error"]["code"];
    if code == "1234" || code == 1234 {
        return Some("business_1234");
    }
    None
}

fn empty_completion(resp: &Value) -> bool {
    resp["content"].as_array().is_none_or(|blocks| {
        blocks
            .iter()
            .all(|b| b["type"] == "text" && b["text"].as_str().unwrap_or("").trim().is_empty())
    })
}

impl Client {
    /// Send one request with retries. `on_attempt` receives each failed
    /// attempt before the retry, so the record lands first.
    pub fn send(&self, body: &str, on_attempt: &mut dyn FnMut(Value)) -> Result<Reply, String> {
        const MAX_RETRIES: u32 = 3;
        let mut empty_retried = false;
        let mut attempt = 0u32;
        let start = Instant::now();
        loop {
            attempt += 1;
            let t0 = Instant::now();
            let (class, status, detail, usage) = match self.post(body) {
                Err(e) => ("curl_error", Value::Null, e, Value::Null),
                Ok((status, json)) => {
                    if (200..300).contains(&status) {
                        let resp = normalize(self.endpoint.wire, &json);
                        if !empty_completion(&resp) || empty_retried {
                            return Ok(Reply {
                                response: resp,
                                latency_ms: start.elapsed().as_millis(),
                                attempts: attempt,
                            });
                        }
                        empty_retried = true;
                        (
                            "empty_completion",
                            json!(status),
                            String::new(),
                            resp["usage"].clone(),
                        )
                    } else if let Some(c) = retryable(status, &json) {
                        (
                            c,
                            json!(status),
                            json["error"]["message"].as_str().unwrap_or("").to_string(),
                            Value::Null,
                        )
                    } else {
                        return Err(format!(
                            "model API returned HTTP {status}: {}",
                            json["error"]
                        ));
                    }
                }
            };
            on_attempt(
                json!({"attempt": attempt, "status": status, "error_class": class,
                "detail": detail, "latency_ms": t0.elapsed().as_millis() as u64, "usage": usage}),
            );
            if attempt > MAX_RETRIES {
                return Err(format!(
                    "model call failed after {attempt} attempts: {class}"
                ));
            }
            std::thread::sleep(self.backoff * 2u32.pow(attempt - 1));
        }
    }

    fn post(&self, body: &str) -> Result<(u16, Value), String> {
        let ep = &self.endpoint;
        let esc = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"");
        let auth = if ep.bearer {
            format!("Authorization: Bearer {}", ep.key)
        } else {
            format!("x-api-key: {}", ep.key)
        };
        let config = format!(
            "url = \"{}\"\nheader = \"{}\"\nheader = \"content-type: application/json\"\nheader = \"anthropic-version: 2023-06-01\"\ndata-binary = \"{}\"\n",
            esc(&ep.url()),
            esc(&auth),
            esc(body)
        );
        let mut child = Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                &self.max_time.as_secs().max(1).to_string(),
                "-K",
                "-",
                "-w",
                "\n%{http_code}",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("curl: no stdin")?
            .write_all(config.as_bytes())
            .map_err(|e| format!("curl stdin: {e}"))?;
        let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "curl exit {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, status) = text.rsplit_once('\n').ok_or("curl: no status line")?;
        let status: u16 = status.trim().parse().map_err(|_| "curl: bad status")?;
        let json =
            serde_json::from_str(body).unwrap_or_else(|_| json!({"error": {"message": body}}));
        Ok((status, json))
    }
}
