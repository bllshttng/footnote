//! `update-journal`: the one Rust door for the `fno doctor update` lifecycle.
//!
//! Four event rows (started, built, installed, failed) append to the global
//! journal through `event_store::append_envelope`, and the installed and
//! failed rows mail the roles through the front binary's own mail verb. The
//! Python side composes nothing: one argv per step, one exit code back, and
//! the fail trap passes `--rc` instead of a JSON template.

use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const MAIL_TIMEOUT: Duration = Duration::from_secs(60);

struct Args {
    events: PathBuf,
    type_name: String,
    fields: Map<String, Value>,
    mail_from: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut events = None;
    let mut type_name = None;
    let mut mail_from = None;
    let mut fields = Map::new();
    let known = [
        "new-rev",
        "old-rev",
        "source-path",
        "outcome",
        "rust-rev",
        "rc",
        "reason",
        "stage",
    ];
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let (name, field) = match flag.strip_prefix("--") {
            Some(n) => (n, n.replace('-', "_")),
            None => return Err(format!("expected a --flag, got {flag:?}")),
        };
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{flag} needs a value"))?;
        i += 2;
        match name {
            "events" => events = Some(PathBuf::from(value)),
            "type" => type_name = Some(value.clone()),
            "mail-from" => mail_from = Some(value.clone()),
            other if known.contains(&other) => {
                fields.insert(field, json!(value));
            }
            other => return Err(format!("unknown flag --{other}")),
        }
    }
    Ok(Args {
        events: events.ok_or("--events <events.jsonl> is required")?,
        type_name: type_name.ok_or("--type started|built|installed|failed is required")?,
        fields,
        mail_from,
    })
}

/// One bounded mail subprocess: the front binary's mail verb is local, but a
/// wedged front door must not hang a failed install's exit past the minute
/// the old Python wrapper allowed.
fn mail_roles(fno_bin: &str, body: &str) {
    let child = Command::new(fno_bin)
        .args([
            "agents",
            "mail",
            "team",
            "--scope",
            "leads",
            "--subject",
            "fno-update",
            body,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fno doctor update: WARNING: role mail not sent: {e}");
            return;
        }
    };
    let deadline = Instant::now() + MAIL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("fno doctor update: WARNING: role mail timed out");
                return;
            }
        }
    }
}

pub fn run_update_journal(args: &[String]) -> i32 {
    let mut args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("fno-agents update-journal: {e}");
            return 2;
        }
    };
    if !matches!(
        args.type_name.as_str(),
        "started" | "built" | "installed" | "failed"
    ) {
        eprintln!("fno-agents update-journal: --type must be started, built, installed or failed");
        return 2;
    }
    // The failed row's reason is schema-required; the trap passes only --rc,
    // so the verb derives the sentence the journal reader expects.
    if args.type_name == "failed" {
        args.fields
            .entry("stage".to_string())
            .or_insert_with(|| json!("install"));
        if !args.fields.contains_key("reason") {
            let rc = args
                .fields
                .get("rc")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            args.fields
                .insert("reason".to_string(), json!(format!("install exited {rc}")));
        }
    }
    let data: Value = Value::Object(args.fields.clone());
    let envelope = json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "type": format!("fno_update_{}", args.type_name),
        "source": "python",
        "data": data,
    });
    let line = serde_json::to_string(&envelope).unwrap_or_default();
    if let Err(e) = crate::event_store::append_envelope(&args.events, &line, None) {
        let type_name = args.type_name.as_str();
        eprintln!("fno doctor update: WARNING: {type_name} event not journaled: {e}");
        return 1;
    }
    if args.mail_from.is_some() && matches!(args.type_name.as_str(), "installed" | "failed") {
        let bin = args.mail_from.as_deref().unwrap_or_default();
        let body = match args.type_name.as_str() {
            "installed" => {
                let new_rev = args
                    .fields
                    .get("new_rev")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let was = args
                    .fields
                    .get("old_rev")
                    .and_then(Value::as_str)
                    .map(|o| format!(" (was {})", &o[..o.len().min(8)]))
                    .unwrap_or_default();
                format!(
                    "fno doctor update installed {}{was}.",
                    &new_rev[..new_rev.len().min(8)]
                )
            }
            _ => {
                let reason = args
                    .fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| {
                        let rc = args
                            .fields
                            .get("rc")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        format!("install exited {rc}")
                    });
                format!("fno doctor update FAILED: {reason}.")
            }
        };
        mail_roles(bin, &body);
    }
    0
}
