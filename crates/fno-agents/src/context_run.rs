//! `fno-agents context-run`: one runner for every fno context producer.
//!
//! Producers are declared once in `<plugin-root>/hooks/context-hooks.json`,
//! grouped per harness and event. The runner starts every producer of the
//! selected group at once, measures each delivered directive, prints one JSON
//! object on stdout (the hook output contract), and appends exactly one
//! `context_snapshot` event. It replaces the 17-per-session
//! `context-observe-hook.sh` bash chains and the Python
//! `context_observation.py` record/collect pair.
//!
//! Direct dispatch (no daemon RPC): a session-start hook must work when
//! nothing else on the machine is answering. Same treatment as `loop-check`.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::bounded_spawn::{kill_process_group, spawn_bounded};

const PRODUCER_BOUND_SECONDS: u64 = 45;
const DEFAULT_NATIVE_BOUND_SECONDS: f64 = 3.0;
const ENTRY_STATES: &[&str] = &["startup", "resume", "clear", "post_compact"];

struct Producer {
    id: String,
    argv: Vec<String>,
    deliver: bool,
    sources: Option<Vec<String>>,
}

impl Clone for Producer {
    fn clone(&self) -> Self {
        Producer {
            id: self.id.clone(),
            argv: self.argv.clone(),
            deliver: self.deliver,
            sources: self.sources.clone(),
        }
    }
}

struct Group {
    harness: String,
    event: String,
    entry: Option<String>,
    producers: Vec<Producer>,
}

/// The pieces of one run the caller (or a test) inspects.
struct CoreOutput {
    stdout: String,
    snapshot: Option<Value>,
}

struct CoreInput<'a> {
    group_name: &'a str,
    plugin_root: &'a Path,
    payload: Vec<u8>,
    producer_bound: Duration,
    native_bound: Duration,
    /// Tests override the `fno doctor --context-audit` command with a fixture.
    native_cmd: Option<Vec<String>>,
    /// The project directory the hook ran in: host of the native census and
    /// the space root the snapshot event appends to.
    host_dir: &'a Path,
}

pub fn run_context_run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("--probe") {
        return run_context_probe(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("--effective-window") {
        return run_effective_window(&args[1..]);
    }
    let mut group_name: Option<&str> = None;
    let mut plugin_root: Option<&str> = None;
    let mut rest = args.iter();
    while let Some(tok) = rest.next() {
        match tok.as_str() {
            "--group" => group_name = rest.next().map(String::as_str),
            "--plugin-root" => plugin_root = rest.next().map(String::as_str),
            other => {
                eprintln!("context-run: unknown argument `{other}`");
                eprintln!("usage: fno-agents context-run --group <name> --plugin-root <dir>");
                return 2;
            }
        }
    }
    let (Some(group_name), Some(plugin_root)) = (group_name, plugin_root) else {
        eprintln!("usage: fno-agents context-run --group <name> --plugin-root <dir>");
        return 2;
    };

    let mut payload = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut payload);
    let host_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let input = CoreInput {
        group_name,
        plugin_root: Path::new(plugin_root),
        payload,
        producer_bound: Duration::from_secs(PRODUCER_BOUND_SECONDS),
        native_bound: native_bound(),
        native_cmd: None,
        host_dir: &host_dir,
    };
    let out = run_core(&input);
    if !out.stdout.is_empty() {
        println!("{}", out.stdout);
    }
    if let Some(snapshot) = out.snapshot {
        let events = crate::paths::events_path(&host_dir);
        if let Err(e) =
            crate::claims::append_event_line(&events, &snapshot, Duration::from_millis(500))
        {
            eprintln!("context-run: snapshot append failed: {e}");
        }
    }
    0
}

fn run_effective_window(args: &[String]) -> i32 {
    let mut model = None;
    let mut configured = None;
    let mut cap = None;
    let mut percent = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(value) = rest.next() else { return 2 };
        match arg.as_str() {
            "--model" => model = Some(value.as_str()),
            "--configured" => configured = value.parse::<u64>().ok(),
            "--cap" => cap = value.parse::<u64>().ok(),
            "--percent" => percent = value.parse::<u64>().ok(),
            _ => return 2,
        }
    }
    let Some(model) = model else { return 2 };
    let receipt = crate::context_window::ContextWindowReceipt {
        model: model.to_string(),
        context_window: configured,
        max_context_window: cap,
        effective_context_window_percent: percent,
    };
    let effective = match crate::context_window::effective_window(&receipt) {
        Ok(effective) => effective,
        Err(_) => return 3,
    };
    println!(
        "{}",
        json!({
            "model": model,
            "configured": configured,
            "max_context_window": cap,
            "percent": percent.unwrap_or(100),
            "effective": effective,
        })
    );
    0
}

fn run_context_probe(args: &[String]) -> i32 {
    let mut transcript = None;
    let mut session = std::env::var("CODEX_THREAD_ID")
        .or_else(|_| std::env::var("FNO_HARNESS_SESSION_ID"))
        .unwrap_or_default();
    let mut json_output = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--transcript" => transcript = rest.next().map(String::as_str),
            "--session" => {
                session = rest
                    .next()
                    .map(String::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
            "--json" => json_output = true,
            other => {
                eprintln!("context-run --probe: unknown argument `{other}`");
                return 2;
            }
        }
    }
    let Some(transcript) = transcript else {
        eprintln!("context-run --probe: --transcript is required");
        return 2;
    };
    let usage = match crate::context_window::read_last_usage(Path::new(transcript)) {
        Ok(Some(usage)) => usage,
        Ok(None) | Err(_) => return 3,
    };
    let used_tokens = match usage.used_tokens() {
        Some(tokens) => tokens,
        None => return 3,
    };
    let window_tokens =
        match crate::context_window::effective_window_for_model(&usage.model, &session) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("context-run --probe: effective context window unreadable: {error:?}");
                return 3;
            }
        };
    let used_pct =
        ((used_tokens as u128 * 100 + (window_tokens as u128 / 2)) / window_tokens as u128) as u64;
    let band = crate::context_window::compaction_band(&usage.model, used_tokens, window_tokens);
    let payload = json!({
        "used_tokens": used_tokens,
        "window_tokens": window_tokens,
        "used_pct": used_pct,
        "model": usage.model,
        "compaction_band": format!("{band:?}").to_ascii_lowercase(),
    });
    if json_output {
        println!("{payload}");
    } else {
        println!(
            "{}% used ({} of {} tokens), model {}",
            used_pct, used_tokens, window_tokens, payload["model"]
        );
    }
    0
}

fn native_bound() -> Duration {
    let secs = std::env::var("FNO_CONTEXT_OBSERVER_TIMEOUT_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| *s > 0.0 && s.is_finite())
        .unwrap_or(DEFAULT_NATIVE_BOUND_SECONDS);
    Duration::from_secs_f64(secs)
}

fn load_declaration(plugin_root: &Path) -> Result<Map<String, Value>, String> {
    let path = plugin_root.join("hooks").join("context-hooks.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let data: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    data.get("groups")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| format!("{}: no `groups` object", path.display()))
}

fn parse_group(name: &str, value: &Value, file: &str) -> Result<Group, String> {
    let fail = |msg: String| format!("{file}: group `{name}`: {msg}");
    let harness = value
        .get("harness")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("missing `harness`".into()))?;
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("missing `event`".into()))?;
    let mut producers = Vec::new();
    for (index, item) in value
        .get("producers")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("missing `producers` array".into()))?
        .iter()
        .enumerate()
    {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(format!("producer {index}: missing `id`")))?;
        let argv: Vec<String> = item
            .get("argv")
            .and_then(Value::as_array)
            .ok_or_else(|| fail(format!("producer `{id}`: missing `argv`")))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if argv.is_empty() {
            return Err(fail(format!("producer `{id}`: empty `argv`")));
        }
        let sources = item.get("sources").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });
        producers.push(Producer {
            id: id.to_string(),
            argv,
            deliver: item.get("deliver").and_then(Value::as_bool) != Some(false),
            sources,
        });
    }
    Ok(Group {
        harness: harness.to_string(),
        event: event.to_string(),
        entry: value
            .get("entry")
            .and_then(Value::as_str)
            .map(str::to_string),
        producers,
    })
}

fn entry_state_for(group_entry: Option<&str>, payload_source: Option<&str>) -> String {
    let raw = group_entry
        .map(str::to_string)
        .or_else(|| payload_source.map(str::to_string))
        .unwrap_or_else(|| "startup".to_string());
    let state = raw.to_lowercase().replace('-', "_");
    let state = match state.as_str() {
        "compact" | "postcompact" => "post_compact".to_string(),
        other => other.to_string(),
    };
    if ENTRY_STATES.contains(&state.as_str()) {
        state
    } else {
        "startup".to_string()
    }
}

fn run_core(input: &CoreInput) -> CoreOutput {
    let file = input.plugin_root.join("hooks").join("context-hooks.json");
    let groups = match load_declaration(input.plugin_root) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("context-run: {e}");
            return CoreOutput {
                stdout: String::new(),
                snapshot: None,
            };
        }
    };
    let Some(group_value) = groups.get(input.group_name) else {
        eprintln!(
            "context-run: {}: unknown group `{}`",
            file.display(),
            input.group_name
        );
        return CoreOutput {
            stdout: String::new(),
            snapshot: None,
        };
    };
    let group = match parse_group(input.group_name, group_value, &file.display().to_string()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("context-run: {e}");
            return CoreOutput {
                stdout: String::new(),
                snapshot: None,
            };
        }
    };

    let payload: Value = serde_json::from_slice(&input.payload).unwrap_or(Value::Null);
    let payload_source = payload.get("source").and_then(Value::as_str);
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let entry_state = entry_state_for(group.entry.as_deref(), payload_source);

    let selected: Vec<&Producer> = group
        .producers
        .iter()
        .filter(|p| match &p.sources {
            None => true,
            Some(sources) => payload_source
                .map(|src| sources.iter().any(|s| s == src))
                .unwrap_or(false),
        })
        .collect();

    // Start every producer and the native census at the same moment; one
    // slow producer must never delay the others.
    let slots: Vec<Arc<std::sync::Mutex<Option<ProducerOut>>>> = (0..selected.len())
        .map(|_| Arc::new(std::sync::Mutex::new(None)))
        .collect();
    let mut handles = Vec::new();
    for (index, producer) in selected.iter().enumerate() {
        let payload_bytes = input.payload.clone();
        let bound = input.producer_bound;
        let producer: Producer = (*producer).clone();
        let plugin_root = input.plugin_root.to_path_buf();
        let slot = Arc::clone(&slots[index]);
        handles.push(std::thread::spawn(move || {
            let out = run_producer(&producer, &payload_bytes, bound, &plugin_root);
            *slot.lock().expect("producer result lock") = Some(out);
        }));
    }
    let native = fetch_native_manifest(input, &group.harness, &entry_state);
    for handle in handles {
        let _ = handle.join();
    }

    let mut manifest: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    match native {
        Ok((rows, mut native_errors)) => {
            manifest.extend(rows.iter().cloned());
            errors.append(&mut native_errors);
        }
        Err(reason) => errors.push(format!("native-manifest: {reason}")),
    }

    let mut context_texts: Vec<String> = Vec::new();
    let mut message_texts: Vec<String> = Vec::new();
    for (index, producer) in selected.iter().enumerate() {
        let out = slots[index]
            .lock()
            .expect("producer result lock")
            .take()
            .unwrap_or_else(|| ProducerOut {
                status: "unreadable".to_string(),
                error: Some("runner lost the producer result".to_string()),
                context: String::new(),
                message: String::new(),
            });
        let text_len = out.context.len() + out.message.len();
        let (status, error, bytes, content_hash) = if out.status == "observed" {
            let mut hasher = Sha256::new();
            hasher.update(out.context.as_bytes());
            hasher.update(out.message.as_bytes());
            let hash = format!("{:x}", hasher.finalize());
            ("observed", None, text_len, Some(hash))
        } else {
            let error = out.error.unwrap_or_else(|| "unobserved".to_string());
            ("unreadable", Some(error), 0usize, None)
        };
        if status != "observed" {
            errors.push(format!(
                "{}: {}",
                producer.id,
                error.clone().unwrap_or_else(|| "unobserved".to_string())
            ));
        }
        manifest.push(json!({
            "source_id": producer.id,
            "carrier": producer.argv.join(" "),
            "status": status,
            "error": error,
            "bytes": bytes,
            "estimated_tokens": (bytes + 3) / 4,
            "content_hash": content_hash,
        }));
        if status == "observed" {
            if producer.deliver && !out.context.is_empty() {
                context_texts.push(out.context);
            }
            if !out.message.is_empty() {
                message_texts.push(out.message);
            }
        }
    }

    let stdout = build_hook_output(&group.event, &context_texts, &message_texts);
    let snapshot = build_snapshot(
        &session_id,
        &group.harness,
        &entry_state,
        &manifest,
        &errors,
    );
    CoreOutput { stdout, snapshot }
}

struct ProducerOut {
    status: String,
    error: Option<String>,
    context: String,
    message: String,
}

fn run_producer(
    producer: &Producer,
    payload: &[u8],
    bound: Duration,
    plugin_root: &Path,
) -> ProducerOut {
    // Own process group so a hung producer's grandchildren die with it.
    // Not `spawn_bounded`: producers take the payload on stdin and keep
    // stderr inherited (today's wrapper passes both through).
    let argv: Vec<String> = producer
        .argv
        .iter()
        .map(|a| a.replace("${PLUGIN_ROOT}", &plugin_root.display().to_string()))
        .collect();
    let spawned = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return ProducerOut {
                status: "unreadable".to_string(),
                error: Some(format!("spawn failed: {e}")),
                context: String::new(),
                message: String::new(),
            };
        }
    };
    let outcome = wait_bounded(&mut child, Some(payload.to_vec()), bound);
    if outcome.timed_out {
        return ProducerOut {
            status: "unreadable".to_string(),
            error: Some(format!("timed out after {}s", bound.as_secs())),
            context: String::new(),
            message: String::new(),
        };
    }
    let rc = outcome.rc.unwrap_or(1);
    if rc != 0 {
        return ProducerOut {
            status: "unreadable".to_string(),
            error: Some(format!("hook exited {rc}")),
            context: String::new(),
            message: String::new(),
        };
    }
    match split_directive(&outcome.stdout) {
        Ok((context, message)) => ProducerOut {
            status: "observed".to_string(),
            error: None,
            context,
            message,
        },
        Err(e) => ProducerOut {
            status: "unreadable".to_string(),
            error: Some(e),
            context: String::new(),
            message: String::new(),
        },
    }
}

struct BoundedOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    rc: Option<i32>,
    timed_out: bool,
}

/// Wait on `child` up to `bound`, draining stdout/stderr while it runs so a
/// chatty child cannot deadlock on a full pipe. Past the bound the whole
/// process group is SIGKILLed and `timed_out` is set.
fn wait_bounded(
    child: &mut std::process::Child,
    stdin_bytes: Option<Vec<u8>>,
    bound: Duration,
) -> BoundedOutcome {
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let stdin_writer = stdin_bytes.map(|bytes| {
        let mut stdin_pipe = child.stdin.take();
        std::thread::spawn(move || {
            if let Some(p) = stdin_pipe.as_mut() {
                let _ = p.write_all(&bytes);
            }
        })
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if start.elapsed() >= bound => {
                timed_out = true;
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                timed_out = true;
                break None;
            }
        }
    };
    if timed_out {
        kill_process_group(child);
    }
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    let rc = status.map(|st| {
        st.code()
            .or_else(|| st.signal().map(|sig| 128 + sig))
            .unwrap_or(1)
    });
    BoundedOutcome {
        stdout,
        stderr,
        rc,
        timed_out,
    }
}

/// Split one producer's stdout into (context, message). JSON-object output
/// gives `hookSpecificOutput.additionalContext` (else top-level
/// `additionalContext`/`additional_context`) plus `systemMessage`; anything
/// else is raw context text with trailing whitespace trimmed.
fn split_directive(stdout: &[u8]) -> Result<(String, String), String> {
    let text = std::str::from_utf8(stdout).map_err(|e| format!("Utf8Error: {e}"))?;
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Ok((text.trim_end().to_string(), String::new())),
    };
    let Some(obj) = parsed.as_object() else {
        return Ok((text.trim_end().to_string(), String::new()));
    };
    let mut context = String::new();
    if let Some(nested) = obj.get("hookSpecificOutput").and_then(Value::as_object) {
        if let Some(ctx) = nested.get("additionalContext").and_then(Value::as_str) {
            context = ctx.to_string();
        }
    }
    if context.is_empty() {
        for key in ["additionalContext", "additional_context"] {
            if let Some(ctx) = obj.get(key).and_then(Value::as_str) {
                context = ctx.to_string();
                break;
            }
        }
    }
    let message = obj
        .get("systemMessage")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((context, message))
}

fn build_hook_output(event: &str, contexts: &[String], messages: &[String]) -> String {
    if contexts.is_empty() && messages.is_empty() {
        return String::new();
    }
    let mut out = Map::new();
    if !contexts.is_empty() {
        out.insert(
            "hookSpecificOutput".to_string(),
            json!({
                "hookEventName": event,
                "additionalContext": contexts.join("\n\n"),
            }),
        );
    }
    if !messages.is_empty() {
        out.insert("systemMessage".to_string(), json!(messages.join("\n\n")));
    }
    Value::Object(out).to_string()
}

/// Native rows come from the existing `fno doctor --context-audit` verb so
/// Rust and the Python audit keep one owner for host-source measurement.
fn fetch_native_manifest(
    input: &CoreInput,
    harness: &str,
    entry_state: &str,
) -> Result<(Vec<Value>, Vec<String>), String> {
    let host = input.host_dir.display().to_string();
    let plugin = input.plugin_root.display().to_string();
    let cmd: Vec<String> = match &input.native_cmd {
        Some(cmd) => cmd.clone(),
        None => vec![
            "fno".to_string(),
            "doctor".to_string(),
            "--context-audit".to_string(),
            "--source".to_string(),
            plugin,
            "--context-host".to_string(),
            host.clone(),
            "--context-harness".to_string(),
            harness.to_string(),
            "--context-entry".to_string(),
            entry_state.to_string(),
            "--json".to_string(),
        ],
    };
    let child = spawn_bounded(
        OsStr::new(&cmd[0]),
        &cmd[1..].iter().map(String::as_str).collect::<Vec<_>>(),
        input.host_dir,
    )
    .map_err(|e| format!("spawn failed: {e}"))?;
    let mut child = child;
    let outcome = wait_bounded(&mut child, None, input.native_bound);
    if outcome.timed_out {
        return Err(format!(
            "timed out after {}s",
            input.native_bound.as_secs_f64()
        ));
    }
    let rc = outcome.rc.unwrap_or(1);
    if rc != 0 {
        let tail = String::from_utf8_lossy(&outcome.stderr);
        let tail = tail.lines().last().unwrap_or("").to_string();
        return Err(format!(
            "exited {rc}{}",
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        ));
    }
    let data: Value =
        serde_json::from_slice(&outcome.stdout).map_err(|e| format!("unparseable json: {e}"))?;
    let cells = data
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| "no `cells` array".to_string())?;
    let cell = cells
        .iter()
        .find(|c| {
            c.get("harness").and_then(Value::as_str) == Some(harness)
                && c.get("entry_state").and_then(Value::as_str) == Some(entry_state)
        })
        .or_else(|| cells.first())
        .ok_or_else(|| "empty `cells` array".to_string())?;
    let rows = cell
        .get("compiled")
        .and_then(|c| c.get("source_manifest"))
        .and_then(Value::as_array)
        .ok_or_else(|| "no `compiled.source_manifest`".to_string())?;
    let mut public_rows = Vec::new();
    let mut errors = Vec::new();
    for row in rows {
        let lifecycle = row.get("lifecycle").and_then(Value::as_str).unwrap_or("");
        let measurement = row.get("measurement").and_then(Value::as_str).unwrap_or("");
        let packet_eligible = row.get("packet_eligible").and_then(Value::as_bool) == Some(true);
        if lifecycle != "harness_native" || measurement != "directive_bytes" || !packet_eligible {
            continue;
        }
        let source_id = row
            .get("source_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let status = row
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let status = if status == "reachable" {
            "observed".to_string()
        } else {
            status
        };
        if status != "observed" {
            let error = row
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unobserved")
                .to_string();
            errors.push(format!("{source_id}: {error}"));
        } else if row.get("bytes").and_then(Value::as_u64).is_none() {
            // The context sum reads only u64 bytes; an observed row without
            // one would silently drop out of context_bytes. Flag it instead.
            errors.push(format!("{source_id}: observed row has non-integer bytes"));
        }
        public_rows.push(json!({
            "source_id": source_id,
            "carrier": row.get("carrier"),
            "status": status,
            "error": row.get("error"),
            "bytes": row.get("bytes"),
            "estimated_tokens": row.get("estimated_tokens"),
            "content_hash": row.get("content_hash"),
        }));
    }
    Ok((public_rows, errors))
}

fn build_snapshot(
    session_id: &str,
    harness: &str,
    entry_state: &str,
    manifest: &[Value],
    errors: &[String],
) -> Option<Value> {
    if session_id.is_empty() {
        return None;
    }
    let source_hashes: Vec<&str> = manifest
        .iter()
        .filter(|row| row.get("status").and_then(Value::as_str) == Some("observed"))
        .filter_map(|row| row.get("content_hash").and_then(Value::as_str))
        .collect();
    let context_bytes: usize = manifest
        .iter()
        .filter(|row| row.get("status").and_then(Value::as_str) == Some("observed"))
        .filter_map(|row| row.get("bytes").and_then(Value::as_u64))
        .map(|b| b as usize)
        .sum();
    let context_hash = if source_hashes.is_empty() {
        None
    } else {
        let mut hasher = Sha256::new();
        hasher.update(source_hashes.join("\n").as_bytes());
        Some(format!("{:x}", hasher.finalize()))
    };
    Some(json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "type": "context_snapshot",
        "source": "hook",
        "data": {
            "session_id": session_id,
            "harness": harness,
            "entry_state": entry_state,
            "context_bytes": context_bytes,
            "estimated_tokens": (context_bytes + 3) / 4,
            "context_hash": context_hash,
            "source_hashes": source_hashes,
            "source_manifest": manifest,
            "measurement_complete": errors.is_empty(),
            "measurement_errors": errors,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new(group_name: &str, group: Value, producers: &[(&str, &str)]) -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let hooks = dir.path().join("hooks");
            std::fs::create_dir_all(&hooks).expect("hooks dir");
            let declaration = json!({"groups": {group_name: group}});
            std::fs::write(
                hooks.join("context-hooks.json"),
                serde_json::to_vec_pretty(&declaration).expect("declaration json"),
            )
            .expect("write declaration");
            for (name, body) in producers {
                let path = hooks.join(name);
                std::fs::write(&path, body).expect("write producer");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .expect("chmod");
                }
                let _ = &path;
            }
            Fixture { dir }
        }

        fn native_cmd(&self, rows: Value) -> Vec<String> {
            let native_json = json!({
                "cells": [{
                    "harness": "claude",
                    "entry_state": "startup",
                    "compiled": {"source_manifest": rows},
                }]
            });
            let path = self.dir.path().join("native-fixture.json");
            std::fs::write(&path, native_json.to_string()).expect("write native fixture");
            vec!["cat".to_string(), path.display().to_string()]
        }

        fn run(&self, group_name: &str, payload: Value, producer_bound_ms: u64) -> CoreOutput {
            run_core(&CoreInput {
                group_name,
                plugin_root: self.dir.path(),
                payload: serde_json::to_vec(&payload).expect("payload json"),
                producer_bound: Duration::from_millis(producer_bound_ms),
                native_bound: Duration::from_secs(2),
                native_cmd: Some(self.native_cmd(json!([]))),
                host_dir: self.dir.path(),
            })
        }

        fn run_with_native(
            &self,
            group_name: &str,
            payload: Value,
            native_rows: Value,
        ) -> CoreOutput {
            run_core(&CoreInput {
                group_name,
                plugin_root: self.dir.path(),
                payload: serde_json::to_vec(&payload).expect("payload json"),
                producer_bound: Duration::from_secs(5),
                native_bound: Duration::from_secs(2),
                native_cmd: Some(self.native_cmd(native_rows)),
                host_dir: self.dir.path(),
            })
        }

        fn manifest(&self, out: &CoreOutput) -> Vec<Value> {
            out.snapshot
                .as_ref()
                .expect("snapshot present")
                .get("data")
                .expect("data")
                .get("source_manifest")
                .and_then(Value::as_array)
                .cloned()
                .expect("manifest")
        }
    }

    fn echo_group(producers: Value) -> Value {
        json!({"harness": "claude", "event": "SessionStart", "producers": producers})
    }

    fn producer_row(manifest: &[Value], id: &str) -> Value {
        manifest
            .iter()
            .find(|row| row.get("source_id").and_then(Value::as_str) == Some(id))
            .cloned()
            .unwrap_or_else(|| panic!("no row for {id}"))
    }

    #[test]
    fn exact_directive_text_survives_in_declared_order() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "raw", "argv": ["${PLUGIN_ROOT}/hooks/raw.sh"]},
                {"id": "json", "argv": ["${PLUGIN_ROOT}/hooks/json.sh"]}
            ])),
            &[
                ("raw.sh", "#!/bin/sh\nprintf 'hello world'"),
                (
                    "json.sh",
                    "#!/bin/sh\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"abc\"}}'",
                ),
            ],
        );
        let out = fx.run("g", json!({"session_id": "s1", "source": "startup"}), 5000);
        let parsed: Value = serde_json::from_str(&out.stdout).expect("stdout json");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            "hello world\n\nabc"
        );
        let rows = fx.manifest(&out);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["source_id"], "raw");
        assert_eq!(rows[0]["bytes"], 11);
    }

    #[test]
    fn a_nonreturning_producer_is_killed_without_changing_others_output() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "sleeper", "argv": ["${PLUGIN_ROOT}/hooks/sleeper-x21e1.sh"]},
                {"id": "echoer", "argv": ["${PLUGIN_ROOT}/hooks/echoer.sh"]}
            ])),
            &[
                ("sleeper-x21e1.sh", "#!/bin/sh\nsleep 30"),
                ("echoer.sh", "#!/bin/sh\nprintf 'alive'"),
            ],
        );
        let out = fx.run("g", json!({"session_id": "s1"}), 1000);
        assert!(out.stdout.contains("alive"));
        let row = producer_row(&fx.manifest(&out), "sleeper");
        assert_eq!(row["status"], "unreadable");
        assert_eq!(row["error"], "timed out after 1s");
        // The sleeper's whole process group is gone, not just reaped later.
        let marker = "sleeper-x21e1.sh";
        let mut gone = false;
        for _ in 0..40 {
            let probe = Command::new("ps")
                .args(["-axo", "command="])
                .output()
                .expect("ps");
            let text = String::from_utf8_lossy(&probe.stdout).to_string();
            if !text.contains(marker) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(gone, "sleeper process still alive after the bound");
    }

    #[test]
    fn a_payload_without_a_session_id_emits_no_snapshot() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "echoer", "argv": ["${PLUGIN_ROOT}/hooks/echoer.sh"]}
            ])),
            &[("echoer.sh", "#!/bin/sh\nprintf 'x'")],
        );
        let out = fx.run("g", json!({"source": "startup"}), 5000);
        assert!(out.snapshot.is_none());
    }

    #[test]
    fn native_rows_join_first_and_only_the_native_filter_passes() {
        let native_row = json!({
            "source_id": "project-instructions",
            "carrier": "CLAUDE.md",
            "lifecycle": "harness_native",
            "measurement": "directive_bytes",
            "packet_eligible": true,
            "status": "reachable",
            "error": null,
            "bytes": 100,
            "estimated_tokens": 25,
            "content_hash": "native-hash",
        });
        let non_native = json!({
            "source_id": "using-fno",
            "lifecycle": "session_start",
            "measurement": "directive_bytes",
            "packet_eligible": true,
            "status": "reachable",
            "bytes": 5,
        });
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "echoer", "argv": ["${PLUGIN_ROOT}/hooks/echoer.sh"]}
            ])),
            &[("echoer.sh", "#!/bin/sh\nprintf 'x'")],
        );
        let out = fx.run_with_native(
            "g",
            json!({"session_id": "s1"}),
            json!([native_row, non_native]),
        );
        let rows = fx.manifest(&out);
        assert_eq!(rows[0]["source_id"], "project-instructions");
        assert_eq!(rows[0]["status"], "observed");
        assert_eq!(rows[1]["source_id"], "echoer");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn valid_json_output_counts_its_directive_not_its_json_bytes() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "json", "argv": ["${PLUGIN_ROOT}/hooks/json.sh"]}
            ])),
            &[(
                "json.sh",
                "#!/bin/sh\nprintf '{\"hookSpecificOutput\":{\"additionalContext\":\"abc\"}}'",
            )],
        );
        let out = fx.run("g", json!({"session_id": "s1"}), 5000);
        let row = producer_row(&fx.manifest(&out), "json");
        assert_eq!(row["bytes"], 3);
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(row["content_hash"], format!("{:x}", hasher.finalize()));
        let parsed: Value = serde_json::from_str(&out.stdout).expect("stdout json");
        assert_eq!(parsed["hookSpecificOutput"]["additionalContext"], "abc");
    }

    #[test]
    fn plain_text_counts_its_raw_trimmed_bytes_and_empty_json_is_observed() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "raw", "argv": ["${PLUGIN_ROOT}/hooks/raw.sh"]},
                {"id": "empty", "argv": ["${PLUGIN_ROOT}/hooks/empty.sh"]}
            ])),
            &[
                ("raw.sh", "#!/bin/sh\nprintf 'abc  \\n\\n'"),
                ("empty.sh", "#!/bin/sh\nprintf '{}'"),
            ],
        );
        let out = fx.run("g", json!({"session_id": "s1"}), 5000);
        let raw = producer_row(&fx.manifest(&out), "raw");
        assert_eq!(raw["bytes"], 3);
        let empty = producer_row(&fx.manifest(&out), "empty");
        assert_eq!(empty["status"], "observed");
        assert_eq!(empty["bytes"], 0);
        assert_eq!(empty["content_hash"], {
            let mut hasher = Sha256::new();
            hasher.update(b"");
            format!("{:x}", hasher.finalize())
        });
    }

    #[test]
    fn a_deliver_false_producer_is_measured_but_not_delivered() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "big", "argv": ["${PLUGIN_ROOT}/hooks/big.sh"], "deliver": false},
                {"id": "small", "argv": ["${PLUGIN_ROOT}/hooks/small.sh"]}
            ])),
            &[
                ("big.sh", "#!/bin/sh\nprintf 'LOUD'"),
                ("small.sh", "#!/bin/sh\nprintf 'quiet'"),
            ],
        );
        let out = fx.run("g", json!({"session_id": "s1"}), 5000);
        let row = producer_row(&fx.manifest(&out), "big");
        assert_eq!(row["status"], "observed");
        assert_eq!(row["bytes"], 4);
        assert!(!out.stdout.contains("LOUD"));
        assert!(out.stdout.contains("quiet"));
    }

    #[test]
    fn an_exiting_producer_is_unreadable_but_others_deliver_and_exit_is_zero() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "fails", "argv": ["${PLUGIN_ROOT}/hooks/fails.sh"]},
                {"id": "echoer", "argv": ["${PLUGIN_ROOT}/hooks/echoer.sh"]}
            ])),
            &[
                ("fails.sh", "#!/bin/sh\nexit 3"),
                ("echoer.sh", "#!/bin/sh\nprintf 'still here'"),
            ],
        );
        let out = fx.run("g", json!({"session_id": "s1"}), 5000);
        let row = producer_row(&fx.manifest(&out), "fails");
        assert_eq!(row["status"], "unreadable");
        assert_eq!(row["error"], "hook exited 3");
        assert!(out.stdout.contains("still here"));
        let data = out
            .snapshot
            .as_ref()
            .expect("snapshot")
            .get("data")
            .unwrap();
        assert_eq!(data["measurement_complete"], false);
        assert!(data["measurement_errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e.as_str().unwrap_or("").contains("fails: hook exited 3")));
    }

    #[test]
    fn sources_filter_gates_compact_only_producers() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "always", "argv": ["${PLUGIN_ROOT}/hooks/always.sh"]},
                {"id": "oncompact", "argv": ["${PLUGIN_ROOT}/hooks/oncompact.sh"], "sources": ["compact"]}
            ])),
            &[
                ("always.sh", "#!/bin/sh\nprintf 'a'"),
                ("oncompact.sh", "#!/bin/sh\nprintf 'c'"),
            ],
        );
        let startup = fx.run("g", json!({"session_id": "s1", "source": "startup"}), 5000);
        assert_eq!(fx.manifest(&startup).len(), 1);
        let compact = fx.run("g", json!({"session_id": "s1", "source": "compact"}), 5000);
        let rows = fx.manifest(&compact);
        assert_eq!(rows.len(), 2);
        let data = compact.snapshot.as_ref().unwrap().get("data").unwrap();
        assert_eq!(data["entry_state"], "post_compact");
    }

    #[test]
    fn a_system_message_producer_joins_the_system_message_channel() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([
                {"id": "msg", "argv": ["${PLUGIN_ROOT}/hooks/msg.sh"]}
            ])),
            &[("msg.sh", "#!/bin/sh\nprintf '{\"systemMessage\":\"m1\"}'")],
        );
        let out = fx.run("g", json!({"session_id": "s1"}), 5000);
        let row = producer_row(&fx.manifest(&out), "msg");
        assert_eq!(row["bytes"], 2);
        let parsed: Value = serde_json::from_str(&out.stdout).expect("stdout json");
        assert_eq!(parsed["systemMessage"], "m1");
        assert!(parsed.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn an_unknown_group_prints_nothing_and_builds_no_snapshot() {
        let fx = Fixture::new(
            "g",
            echo_group(json!([{"id": "echoer", "argv": ["${PLUGIN_ROOT}/hooks/echoer.sh"]}])),
            &[("echoer.sh", "#!/bin/sh\nprintf 'x'")],
        );
        let out = fx.run("missing-group", json!({"session_id": "s1"}), 5000);
        assert!(out.stdout.is_empty());
        assert!(out.snapshot.is_none());
    }
}
