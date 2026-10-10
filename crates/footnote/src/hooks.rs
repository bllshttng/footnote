//! The hooks.json host: runs `<plugin_root>/hooks/hooks.json` with the
//! Claude hook contract, as the opencode bridge's `runHooksJson` does in
//! JavaScript. A hook that cannot run fails open; the effect guard inside
//! the hook scripts fails closed on its own.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub struct HookHost {
    root: Option<PathBuf>,
    doc: Value,
}

#[derive(Default, Debug)]
pub struct Outcome {
    /// `(reason, rule)` of the first hook that denied or blocked.
    pub deny: Option<(String, String)>,
    pub context: Vec<String>,
    pub errors: Vec<String>,
}

impl HookHost {
    pub fn load(root: Option<PathBuf>) -> HookHost {
        let doc = root
            .as_ref()
            .and_then(|r| std::fs::read_to_string(r.join("hooks").join("hooks.json")).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        HookHost { root, doc }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Run every group of `event` whose matcher matches the payload's
    /// `tool_name` (an empty matcher matches all). Stops at the first deny.
    pub fn run(&self, event: &str, payload: &Value, cwd: &Path, fno_id: &str) -> Outcome {
        let mut out = Outcome::default();
        let Some(root) = &self.root else {
            return out;
        };
        let tool = payload["tool_name"].as_str().unwrap_or("");
        let stdin = payload.to_string();
        for group in self.doc["hooks"][event].as_array().into_iter().flatten() {
            if !matches(group["matcher"].as_str().unwrap_or(""), tool) {
                continue;
            }
            for hook in group["hooks"].as_array().into_iter().flatten() {
                let command = hook["command"]
                    .as_str()
                    .unwrap_or("")
                    .replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy());
                let timeout = Duration::from_secs(hook["timeout"].as_u64().unwrap_or(10));
                let mut cmd = Command::new("/bin/sh");
                cmd.arg("-c")
                    .arg(&command)
                    .current_dir(cwd)
                    .env("CLAUDE_PLUGIN_ROOT", root);
                super::tools::child_env(&mut cmd, fno_id);
                let (code, stdout, stderr) =
                    match super::tools::run_bounded(cmd, Some(&stdin), timeout) {
                        Ok(r) => r,
                        Err(e) => {
                            out.errors.push(format!("{event} {command}: {e}"));
                            continue;
                        }
                    };
                match code {
                    None => out.errors.push(format!("{event} {command}: timed out")),
                    Some(2) => {
                        let reason = stderr.trim();
                        let reason = if reason.is_empty() {
                            "denied by hook"
                        } else {
                            reason
                        };
                        out.deny = Some((reason.to_string(), command));
                        return out;
                    }
                    Some(c) => {
                        if c != 0 && !stderr.trim().is_empty() {
                            out.errors
                                .push(format!("{event} {command}: exit {c}: {}", stderr.trim()));
                        }
                        let (deny, ctx) = parse_decision(event, &stdout);
                        if let Some(ctx) = ctx {
                            out.context.push(ctx);
                        }
                        if let Some(reason) = deny {
                            out.deny = Some((reason, command));
                            return out;
                        }
                    }
                }
            }
        }
        out
    }
}

fn matches(matcher: &str, tool: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    regex::Regex::new(matcher)
        .map(|r| r.is_match(tool))
        .unwrap_or(false)
}

/// `(deny reason, additional context)` from a hook's stdout.
fn parse_decision(event: &str, stdout: &str) -> (Option<String>, Option<String>) {
    let t = stdout.trim();
    if t.is_empty() || t == "{}" {
        return (None, None);
    }
    let Ok(v) = serde_json::from_str::<Value>(t) else {
        // Claude feeds plain stdout to the model for these two events only.
        let plain = matches!(event, "UserPromptSubmit" | "SessionStart");
        return (None, plain.then(|| t.to_string()));
    };
    let hso = &v["hookSpecificOutput"];
    let ctx = hso["additionalContext"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    let deny = hso["permissionDecision"] == "deny" || v["decision"] == "block";
    let reason = hso["permissionDecisionReason"]
        .as_str()
        .or(v["reason"].as_str())
        .unwrap_or("denied by hook")
        .to_string();
    (deny.then_some(reason), ctx)
}
