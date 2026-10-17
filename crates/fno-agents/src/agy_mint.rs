//! Owner of the agy conversation mint's execution: run the stream-json print
//! turn (argv from the agy_launch owner), read the conversation id from the
//! stream, and never wait for an exit. On agy 1.3.3 the turn answers but the
//! harness Stop hook holds the process, so the id is taken from the init
//! line and the process is killed; the stderr capture and refusal map keep
//! every diagnostic the Python envelope read had. Std threads and channels
//! only, so the read deadline needs no async runtime.
//!
//! The Python side (cli/src/fno/agents/harnesses/agy.py) validates the
//! answer's id and raises the harness-facing refusals; this module owns the
//! process mechanics.

use std::io::{BufRead, BufReader, Read, Seek};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The mint ceiling mirrored from the Python row's binding window
/// (MINT_TIMEOUT_S = 45.0); the ask may carry its own.
const DEFAULT_TIMEOUT_S: f64 = 45.0;

/// The conversation_id of one stream-json line, or None when the line is not
/// JSON or carries no such key (harness chatter, the final result event).
fn conversation_id_from_line(line: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let id = value.get("conversation_id")?.as_str()?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// One mint run, the spawn-axes answer envelope: `{"conversation_id": ...}`
/// or `{"refused": "..."}`. The read returns on the first id-bearing line
/// and the process is killed either way; a kill on an already-exited child
/// is an ignored error.
pub fn run(ask: &serde_json::Value) -> serde_json::Value {
    let s = |k: &str| {
        ask.get(k)
            .and_then(serde_json::Value::as_str)
            .map(|v| v.to_string())
    };
    let cwd = s("cwd").unwrap_or_default();
    if cwd.is_empty() {
        return serde_json::json!({ "refused": "agy mint ask carried no cwd" });
    }
    let timeout = ask
        .get("timeout_s")
        .and_then(serde_json::Value::as_f64)
        .filter(|t| *t > 0.0)
        .unwrap_or(DEFAULT_TIMEOUT_S);
    let argv = match crate::agy_launch::agy_mint_argv(
        s("model").as_deref(),
        s("effort").as_deref(),
        s("permission_mode").as_deref(),
        ask.get("yolo")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    ) {
        Ok(argv) => argv,
        Err(reason) => return serde_json::json!({ "refused": reason }),
    };
    let stderr_capture = match tempfile::tempfile() {
        Ok(f) => f,
        Err(e) => {
            return serde_json::json!({
                "refused": format!("agy mint could not stage a stderr capture: {e}")
            })
        }
    };
    let stderr_child = match stderr_capture.try_clone() {
        Ok(f) => f,
        Err(e) => {
            return serde_json::json!({
                "refused": format!("agy mint could not stage a stderr capture: {e}")
            })
        }
    };
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&cwd)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_child))
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return serde_json::json!({ "refused": format!("agy could not start: {e}") }),
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            crate::bounded_spawn::kill_process_group(&mut child);
            let _ = child.wait();
            return serde_json::json!({ "refused": "agy mint produced no stdout pipe" });
        }
    };
    let (line_tx, line_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let mut conversation_id = String::new();
    let mut eof = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match line_rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(line) => {
                if let Some(id) = conversation_id_from_line(&line) {
                    conversation_id = id;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eof = true;
                break;
            }
        }
    }
    // The holder's children (a script's `sleep`, agy's own helpers) share the
    // group: a lone child.kill() orphans them with the session cwd, where the
    // test-run sweep reads every rooted stray as this session's leak.
    crate::bounded_spawn::kill_process_group(&mut child);
    let status = child.wait();
    if !conversation_id.is_empty() {
        return serde_json::json!({ "conversation_id": conversation_id });
    }
    let mut stderr_text = String::new();
    let mut capture = stderr_capture;
    let _ = capture.seek(std::io::SeekFrom::Start(0));
    let _ = capture.read_to_string(&mut stderr_text);
    let detail = stderr_text.trim();
    if eof {
        let code = status.ok().and_then(|s| s.code()).unwrap_or(0);
        if code != 0 {
            return serde_json::json!({
                "refused": format!(
                    "agy exited {code} while minting a conversation id: {}",
                    if detail.is_empty() { "no output" } else { detail }
                )
            });
        }
        return serde_json::json!({
            "refused": format!(
                "agy print-mode output carried no conversation_id: {}",
                if detail.is_empty() { "no output" } else { detail }
            )
        });
    }
    serde_json::json!({
        "refused": format!(
            "agy minted no conversation id within {timeout}s; an \
             unauthenticated or wedged binary never prints its init line"
        )
    })
}
