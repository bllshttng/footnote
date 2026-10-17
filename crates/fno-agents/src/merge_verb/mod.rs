//! Native `fno do pr merge`: the argv flow from parse through the merge lock
//! and the authorized-merge decide/effect passes.
//!
//! Staged port. The incarnation fence is a silent no-op at this stage (it
//! lands with the fence stage; absence fails open exactly like a crashed
//! check does on the Python leg). The coverage pre-read and the gh-backed
//! status publish stay Python-side until the follow-ups stage, so the covered
//! head this flow pins is the empty string end to end. The merged-branch
//! cleanup (note fetch, confirmed-merge finish, graph sync) also stays
//! Python-side; no captured golden at this stage reaches it.
//!
//! The flow is transport-only: every merge-permission decision lives in
//! `authorized_merge.rs`, reached through the same `authorized-merge` verb
//! hop the terminal arm uses, so this verb and the terminal arm cannot
//! disagree about who may merge.

use serde_json::{json, Map, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MERGE_LOCK_WAIT_S: u64 = 120;
const MERGE_LOCK_POLL_S: u64 = 5;
const VERB_TIMEOUT: Duration = Duration::from_secs(30);
const FLAKE_PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const BIN_MISSING: &str =
    "the fno-agents binary was not found; reinstall fno, run `fno doctor update --rust`, or set FNO_AGENTS_BIN";

/// One flow's collected output: every stdout/stderr line in write order, the
/// final receipt, and the authorized receipt's observed head when one rendered.
pub struct FlowResult {
    pub exit_code: i32,
    /// `(stderr?, line)` in write order; each line carries its own trailing
    /// newline so the operand door can replay the stream interleaving.
    pub lines: Vec<(bool, String)>,
    pub receipt: Option<Value>,
    pub observed_head: Option<Value>,
}

impl FlowResult {
    fn new() -> Self {
        Self {
            exit_code: 0,
            lines: Vec::new(),
            receipt: None,
            observed_head: None,
        }
    }
}

pub(crate) struct VerbErr {
    message: String,
    returncode: Option<i32>,
}

pub(crate) enum ClaimErr {
    /// rc 1 with `held_by_other`: a real holder, retried by polling callers.
    Held {
        holder: String,
        pid: Value,
        host: String,
    },
    /// The transport itself is down (binary missing, spawn failed): callers
    /// with a retry loop keep polling, like `CLAIM_UNAVAILABLE` on the
    /// Python side.
    Unavailable(String),
    /// An answer that failed to decode or a failed operation: terminal for
    /// the merge lock, which degrades to unlocked rather than blocking a
    /// merge on its own tooling.
    Verdict(String),
}

struct MergeLockGuard {
    key: String,
    holder: String,
}

enum LockOutcome {
    Acquired(MergeLockGuard),
    Held(String),
    Unavailable(String),
}

struct Ask<'a> {
    approved: Option<bool>,
    source: &'a str,
    require_checks: bool,
    covered_head: String,
    authority: &'a str,
    accept_flake: bool,
}

/// The `fno do pr merge` operand door. `args` is post-verb: `["merge", ...argv]`.
pub(crate) fn run_operand(args: &[String]) -> i32 {
    let authority = std::env::var("FNO_MERGE_AUTHORITY").unwrap_or_else(|_| "manifest".to_string());
    match args.first().map(String::as_str) {
        Some("merge") => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let mut flow = run_flow(&args[1..], &cwd, &authority, 300);
            for (err, line) in std::mem::take(&mut flow.lines) {
                if err {
                    eprint!("{}", line);
                } else {
                    print!("{}", line);
                }
            }
            flow.exit_code
        }
        Some("hold-check") => run_hold_check_operand(&args[1..]),
        // merge-result-check stays on its own verb (`pr-merge-result-check`);
        // no operand door asks for it here.
        _ => {
            eprintln!(
                "fno-agents authorized-merge: not native yet; the Python leg still owns this verb"
            );
            2
        }
    }
}

/// The dispatch-lane operation handler (`authorized-merge` with
/// `{"op": "merge", ...}`). The answer carries the flow receipt plus `exit`.
pub(crate) fn run_op(op: &str, payload: &Value) -> Result<Value, String> {
    match op {
        "merge" => {
            let argv: Vec<String> = payload
                .get("argv")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let authority = payload
                .get("authority")
                .and_then(Value::as_str)
                .unwrap_or("manifest");
            let cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let mut flow = run_flow(&argv, &cwd, authority, 300);
            // A flow that returns before any emit leaves the caller's last
            // receipt untouched on the Python side; carry only `exit` here.
            let mut answer = match flow.receipt.take() {
                Some(Value::Object(map)) => map,
                _ => Map::new(),
            };
            answer.insert("exit".into(), json!(flow.exit_code));
            if let Some(v) = flow.observed_head.take() {
                answer.insert("observed_head".into(), v);
            }
            Ok(Value::Object(answer))
        }
        // The check-stage door: the status cache-key op and the seams probe
        // ride the payload form; the answer carries the door's streams and
        // exit, the same shape the spawned leaf answered.
        "hold-check" => {
            let pr = payload.get("pr").and_then(Value::as_i64).unwrap_or(0);
            let cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let mut flow = hold_check_flow(pr, &cwd);
            let mut stdout = String::new();
            let mut stderr = String::new();
            for (err, line) in std::mem::take(&mut flow.lines) {
                if err {
                    stderr.push_str(&line);
                } else {
                    stdout.push_str(&line);
                }
            }
            Ok(json!({
                "exit": flow.exit_code,
                "stdout": stdout,
                "stderr": stderr,
            }))
        }
        other => Err(format!(
            "fno-agents authorized-merge: unsupported op {other}\n"
        )),
    }
}

// ---------------------------------------------------------------------------
// hold-check: the plan-hold door the git-protection hook and the status
// probes read. The decision bodies all live native already (`pr_binding_keys`,
// the `hold-verdict` op, the worktree resolver); this flow is the
// orchestration the deleted Python leaf performed, byte-shaped after the
// goldens frozen from it under tests/golden/merge_verb/hold_*.
// ---------------------------------------------------------------------------

/// The `authorized-merge hold-check <pr> [--repo <path>]` operand door.
fn run_hold_check_operand(args: &[String]) -> i32 {
    let mut pr_raw: Option<&str> = None;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--repo" && i + 1 < args.len() {
            cwd = PathBuf::from(&args[i + 1]);
            i += 2;
        } else {
            pr_raw = Some(args[i].as_str());
            i += 1;
        }
    }
    let Some(raw) = pr_raw else {
        eprintln!("hold-check: pr number required\n");
        return 1;
    };
    let Ok(pr) = raw.parse::<i64>() else {
        eprintln!("hold-check: invalid pr number: {raw}\n");
        return 1;
    };
    let mut flow = hold_check_flow(pr, &cwd);
    for (err, line) in std::mem::take(&mut flow.lines) {
        if err {
            eprint!("{line}");
        } else {
            print!("{line}");
        }
    }
    flow.exit_code
}

/// The plan-hold read the merge guard, the hook and the status probes share:
/// resolve the PR's local worktree, read the bound plan ancestry's hold, echo
/// + exit 3 on a hold, or print the clear line. Refusals fail closed and
/// disarm any queued GitHub auto-merge on the way out.
pub(crate) fn hold_check_flow(pr: i64, cwd: &Path) -> FlowResult {
    let mut flow = FlowResult::new();
    // A hold is a graph-resident concept: under an external tracker backend
    // the graph is not this repo's delivery record, so there is no plan hold
    // to read - never "unreadable".
    let backend = std::env::var("FNO_TRACKER_BACKEND").unwrap_or_else(|_| "graph".to_string());
    if backend != "graph" {
        hold_clear(&mut flow, pr);
        return flow;
    }
    let worktree = match resolve_pr_worktree(pr, cwd) {
        Ok(wt) => wt,
        Err(message) => {
            flow.lines.push((true, format!("{message}\n")));
            flow.exit_code = 1;
            return flow;
        }
    };
    // "" = no local worktree on the PR branch: the hold read runs from the
    // caller's checkout, whose cwd only resolves the repo root.
    let base: PathBuf = if worktree.as_os_str().is_empty() {
        cwd.to_path_buf()
    } else {
        worktree
    };
    let rows = match worker_rows(&base) {
        Ok(rows) => rows,
        Err(error) => {
            hold_refusal(
                &mut flow,
                pr,
                &base,
                &format!("dispatch-hold-invalid: backlog graph is unreadable: {error}"),
            );
            return flow;
        }
    };
    let mut by_id: std::collections::BTreeMap<String, &Value> = Default::default();
    for row in &rows {
        if let Some(id) = row.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                by_id.insert(id.to_string(), row);
            }
        }
    }
    if by_id.is_empty() {
        hold_clear(&mut flow, pr);
        return flow;
    }
    let root = hold_repo_root(&base);
    let root_prefix = format!("{}/", root.trim_end_matches('/'));
    // The live work-map answers before a stored cwd: a moved or repointed
    // checkout leaves graph rows carrying the old path while their project
    // name resolves to the current root, and matching only the stale
    // snapshot silently reads a genuinely-held node as not-this-repo's.
    let workmap = crate::territory::workspace_paths(&base);
    if !rows
        .iter()
        .any(|row| cwd_in_repo(row, &root, &root_prefix, &workmap))
    {
        hold_clear(&mut flow, pr);
        return flow;
    }
    let stamped = rows
        .iter()
        .any(|row| node_pr_refs(row).iter().any(|(n, _)| *n == pr));
    let ctx = match verb_call(
        "pr-closure-context",
        &json!({"pr": pr, "repo": Value::Null, "cwd": cwd.to_string_lossy()}),
        Duration::from_secs(60),
    ) {
        Ok(v) => v,
        Err(e) => {
            hold_refusal(
                &mut flow,
                pr,
                &base,
                &format!(
                    "dispatch-hold-invalid: hold lookup unavailable: gh call failed before \
                     PR content was read: {}; retry the hold lookup; do not edit the PR body",
                    e.message
                ),
            );
            return flow;
        }
    };
    let url = ctx.get("url").and_then(Value::as_str).unwrap_or("");
    let body = ctx.get("body").and_then(Value::as_str).unwrap_or("");
    let mut candidates: Vec<String> = Vec::new();
    if stamped {
        match find_pr_node_id(&rows, pr, url) {
            Some(id) => candidates.push(id),
            None => {
                // The ref match collapsed to None - a safe skip for
                // reconcile, but here None reads as "no binding". Surface the
                // ambiguity and fail closed instead of assuming unheld.
                if let Some(our_slug) = repo_slug_from_url(url) {
                    let ambiguous = repo_scoped_number_matches(&rows, pr, &our_slug);
                    if ambiguous.len() > 1 {
                        hold_refusal(
                            &mut flow,
                            pr,
                            &base,
                            &format!(
                                "dispatch-hold-invalid: PR {pr} maps ambiguously to graph \
                                 nodes {}; refusing to assume unheld; disambiguate the \
                                 entries' pr_url and retry",
                                ambiguous.join(", ")
                            ),
                        );
                        return flow;
                    }
                }
            }
        }
    }
    // A parse read that fails refuses like every other hold-read fault: the
    // leg crashed here, which is the same fail-closed answer in a different
    // shape - never a silent narrowing of the candidate set.
    let parsed = match verb_call(
        "pr-closure-parse",
        &json!({"body": body}),
        Duration::from_secs(30),
    ) {
        Ok(v) => v,
        Err(e) => {
            hold_refusal(
                &mut flow,
                pr,
                &base,
                &format!(
                    "dispatch-hold-invalid: hold lookup unavailable: the closure-line parse \
                     could not run: {}; retry the hold lookup; do not edit the PR body",
                    e.message
                ),
            );
            return flow;
        }
    };
    for claimed in parsed
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[])
    {
        if let Some(id) = claimed.as_str() {
            if !candidates.iter().any(|c| c == id) {
                candidates.push(id.to_string());
            }
        }
    }
    if candidates.is_empty() {
        hold_clear(&mut flow, pr);
        return flow;
    }
    let graph_path = hold_graph_path(&base);
    for id in &candidates {
        if !by_id.contains_key(id.as_str()) {
            if stamped && candidates.first() == Some(id) {
                disarm_queued_auto_merge(
                    &mut flow,
                    pr,
                    &base,
                    &format!(
                        "dispatch-hold-invalid: graph node {id} disappeared during hold lookup"
                    ),
                );
                let why = format!(
                    "dispatch-hold-invalid: graph node {id} disappeared during hold lookup"
                );
                flow.lines
                    .push((true, format!("{why}; refusing to assume unheld\n")));
                flow.exit_code = 3;
                return flow;
            }
            // A trailer can name an id this graph slice does not carry.
            continue;
        }
        let receipt = match verb_call(
            "authorized-merge",
            &json!({"op": "hold-verdict", "node": id, "graph": graph_path.to_string_lossy()}),
            Duration::from_secs(15),
        ) {
            Ok(v) => v,
            Err(e) => json!({
                "outcome": "invalid",
                "owner": id,
                "detail": format!("hold verdict read failed: {}", e.message),
            }),
        };
        let outcome = receipt.get("outcome").and_then(Value::as_str).unwrap_or("");
        if outcome == "absent" {
            continue;
        }
        let held = outcome == "held";
        let owner = match receipt.get("owner") {
            None | Some(Value::Null) => "unknown".to_string(),
            Some(Value::String(s)) if s.is_empty() => "unknown".to_string(),
            Some(v) => py_str(v),
        };
        let field = |name: &str| -> String {
            match receipt.get(name) {
                None | Some(Value::Null) => String::new(),
                Some(v) => py_str(v),
            }
        };
        let prefix = if held {
            "dispatch-hold"
        } else {
            "dispatch-hold-invalid"
        };
        let guard = format!("{prefix}:{owner}");
        if held {
            let reason = field("reason");
            let why = format!("{guard}: {reason}");
            disarm_queued_auto_merge(&mut flow, pr, &base, &why);
            let mut line = format!(
                "{why}; set_by={}; release_when={}; review_on={}",
                field("set_by"),
                field("release_when"),
                field("review_on")
            );
            let exempt = field("exempt");
            if !exempt.is_empty() {
                line.push_str(&format!("; exempt={exempt}"));
            }
            flow.lines.push((true, format!("{line}\n")));
        } else {
            let why = format!("{guard}: {}", field("detail"));
            disarm_queued_auto_merge(&mut flow, pr, &base, &why);
            flow.lines
                .push((true, format!("{why}; refusing to assume unheld\n")));
        }
        flow.exit_code = 3;
        return flow;
    }
    hold_clear(&mut flow, pr);
    flow
}

fn hold_clear(flow: &mut FlowResult, pr: i64) {
    flow.lines
        .push((false, format!("PR {pr}: no plan dispatch hold\n")));
    flow.exit_code = 0;
}

/// One fail-closed hold refusal: disarm any queued auto-merge, name the
/// reason on stderr, exit 3. `why` already carries the `dispatch-hold-invalid:`
/// prefix the caller words.
fn hold_refusal(flow: &mut FlowResult, pr: i64, base: &Path, why: &str) {
    disarm_queued_auto_merge(flow, pr, base, why);
    flow.lines
        .push((true, format!("{why}; refusing to assume unheld\n")));
    flow.exit_code = 3;
}

/// Best-effort `gh pr merge <n> --disable-auto` when a hold is seen: the
/// queue fires server-side with no re-check, so a hold added after arming was
/// silently ignored. Fires on every positive hold/invalid verdict, never on
/// an unheld read; any failure notes and returns.
fn disarm_queued_auto_merge(flow: &mut FlowResult, pr: i64, cwd: &Path, why: &str) {
    let child = match Command::new("gh")
        .args(["pr", "merge", &pr.to_string(), "--disable-auto"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let output = wait_output_with_timeout(child, Duration::from_secs(15));
    let Some(output) = output else {
        flow.lines.push((
            true,
            format!("hold: auto-merge disarm probe failed for PR {pr}: timed out after 15s\n"),
        ));
        return;
    };
    if output.status.success() {
        flow.lines.push((
            true,
            format!(
                "hold: disabled an armed auto-merge for PR {pr} ({why}); re-arm with \
                 `gh pr merge {pr} --auto` once the hold clears\n"
            ),
        ));
        return;
    }
    // A non-zero exit includes the benign "auto-merge not enabled" AND the
    // dangerous auth/network failure - gh's own output is the only thing that
    // distinguishes them, so surface it.
    let text = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).to_string()
    } else {
        String::from_utf8_lossy(&output.stderr).to_string()
    };
    let tail: Vec<&str> = text.trim().lines().collect();
    let detail = tail
        .last()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .unwrap_or_else(|| format!("exit {}", output.status.code().unwrap_or(-1)));
    flow.lines.push((
        true,
        format!(
            "hold: auto-merge disarm for PR {pr} returned nonzero ({detail}); an armed \
             queue entry may still exist - re-run `gh pr merge --disable-auto` by hand \
             if auth or network failed\n"
        ),
    ));
}

/// The graph store path the deleted leg read (`fno.paths.graph_json()`): a
/// pinned `FNO_STATE_DIR` relocates the state root, so it wins before the
/// configured/default home resolver - otherwise a relocated state dir leaves
/// this door reading a graph nobody wrote.
fn hold_graph_path(cwd: &Path) -> PathBuf {
    if let Some(pinned) = std::env::var_os("FNO_STATE_DIR").filter(|v| !v.is_empty()) {
        return crate::state_layout::place(&PathBuf::from(pinned), "graph.json");
    }
    crate::org_board::scope::graph_json_path(cwd)
}

/// One graph rows read through the same worker door the Python leg used:
/// `fno-agents-worker --store-exec --graph <path>`, the api rows request on
/// stdin, the `{ok, result|error}` envelope on stdout.
fn worker_rows(cwd: &Path) -> Result<Vec<Value>, String> {
    let graph = hold_graph_path(cwd);
    let bin = std::env::var_os("FNO_AGENTS_WORKER")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            resolve_agents_bin()
                .map(|exe| exe.with_file_name("fno-agents-worker"))
                .filter(|p| p.exists())
        })
        .ok_or_else(|| {
            // Not the classified refusal word (product_boundary owns it, and
            // this crate may not link fno in production): this door names the
            // same component and repair in its own sentence.
            "fno-agents-worker missing (set FNO_AGENTS_WORKER or install the runtime)".to_string()
        })?;
    let mut child = Command::new(&bin)
        .args(["--store-exec", "--graph"])
        .arg(&graph)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("fno-agents-worker failed: {e}"))?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let _ = stdin.write_all(
            json!({"id": 1, "method": "api", "params": {"op": "rows", "include_archived": false}})
                .to_string()
                .as_bytes(),
        );
    }
    let output = wait_output_with_timeout(child, Duration::from_secs(60))
        .ok_or_else(|| "store-exec timed out after 60s".to_string())?;
    if !output.status.success() {
        return Err(format!(
            "fno-agents-worker exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    let reply: Value = serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .map_err(|e| format!("store reply did not parse: {e}"))?;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        let message = reply
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("unreadable store reply");
        return Err(message.to_string());
    }
    let rows = reply
        .pointer("/result/rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The rows op serves archived residents raw; the default read hides them.
    Ok(rows
        .into_iter()
        .filter(|row| {
            !row.get("archived_at")
                .map(|v| !v.is_null())
                .unwrap_or(false)
        })
        .collect())
}

/// The bounded one-command wait shared by the flow's subprocess hops: the
/// exit status plus whatever the pipes captured, or None on a timeout (the
/// child is killed, the partial pipes dropped). A drainer thread reads both
/// pipes for the whole wait, so a child whose output outgrows the OS pipe
/// buffer can always write and exit - a wait-for-exit-then-read order would
/// deadlock exactly there, at the pipe capacity.
fn wait_output_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel();
    // The handle is unread by design: the recv below is the join point, and
    // on the timeout path this thread returns while the drainer finishes at
    // the killed child's EOF.
    std::thread::spawn(move || {
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = pipe.read_to_string(&mut stdout);
        }
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let _ = tx.send((stdout, stderr));
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    };
    // The child has exited, so both pipes are at EOF and the drainer's send
    // has already landed (or the child held no pipes at all).
    let (stdout, stderr) = rx.recv().unwrap_or((String::new(), String::new()));
    Some(std::process::Output {
        status,
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
    })
}

/// The hold read's repo root: the canonical resolution when the workspace
/// config knows one, else the checkout the read runs from. Node `cwd` fields
/// store canonical roots, so a linked-worktree caller must land there.
fn hold_repo_root(base: &Path) -> String {
    match crate::paths::canonical_repo_root(base) {
        Some(root) => root.to_string_lossy().to_string(),
        None => base.to_string_lossy().to_string(),
    }
}

/// The leaf's in-repo gate: is this entry plausibly THIS repo's node? The
/// live work-map root for the entry's project answers over its stored cwd
/// (an intake-time snapshot); with neither, the entry cannot be proven
/// foreign and is included - only binding-key matches can ever trigger a
/// gh call.
fn cwd_in_repo(
    entry: &Value,
    root: &str,
    root_prefix: &str,
    workmap: &std::collections::HashMap<String, String>,
) -> bool {
    let project = entry
        .get("project")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    let resolved = project.and_then(|p| workmap.get(p)).map(String::as_str);
    let raw = match resolved {
        Some(path) if !path.is_empty() => path,
        _ => match entry.get("cwd").and_then(Value::as_str) {
            Some(raw) if !raw.is_empty() => raw,
            _ => return true,
        },
    };
    let norm = normalize_path(raw);
    norm == root || norm.starts_with(root_prefix)
}

/// `os.path.normpath(os.path.expanduser(raw))` for the shapes graph cwd
/// fields carry: `~` expansion and trailing-separator cleanup.
fn normalize_path(raw: &str) -> String {
    let expanded = if raw == "~" {
        std::env::var("HOME").unwrap_or_else(|_| raw.to_string())
    } else if let Some(rest) = raw.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => raw.to_string(),
        }
    } else {
        raw.to_string()
    };
    expanded.trim_end_matches('/').to_string()
}

/// `(pr_number, pr_url)` pairs for a node, primary first, dedup by number.
fn node_pr_refs(entry: &Value) -> Vec<(i64, Option<String>)> {
    let mut refs: Vec<(i64, Option<String>)> = Vec::new();
    if let Some(n) = entry.get("pr_number").and_then(Value::as_i64) {
        refs.push((
            n,
            entry
                .get("pr_url")
                .and_then(Value::as_str)
                .map(str::to_string),
        ));
    }
    if let Some(extras) = entry.get("additional_prs").and_then(Value::as_array) {
        for extra in extras {
            let Some(n) = extra.get("number").and_then(Value::as_i64) else {
                continue;
            };
            if refs.iter().any(|(seen, _)| *seen == n) {
                continue;
            }
            refs.push((
                n,
                extra.get("url").and_then(Value::as_str).map(str::to_string),
            ));
        }
    }
    refs
}

/// The graph node linked to this PR, resolved by REPOSITORY: an exact
/// `pr_url` (primary or additional) match wins; else a number match scoped
/// to the url's `owner/repo` slug (a url-less node counts as best-effort).
/// None when nothing resolves unambiguously.
fn find_pr_node_id(rows: &[Value], pr: i64, pr_url: &str) -> Option<String> {
    let url = pr_url.trim();
    if !url.is_empty() {
        for row in rows {
            if row.get("pr_url").and_then(Value::as_str).map(str::trim) == Some(url) {
                return row.get("id").and_then(Value::as_str).map(str::to_string);
            }
            for extra in row
                .get("additional_prs")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                if extra.get("url").and_then(Value::as_str).map(str::trim) == Some(url) {
                    return row.get("id").and_then(Value::as_str).map(str::to_string);
                }
            }
        }
    }
    let our_slug = repo_slug_from_url(url)?;
    let uniq = repo_scoped_number_matches(rows, pr, &our_slug);
    if uniq.len() == 1 {
        uniq.into_iter().next()
    } else {
        None
    }
}

/// Every node id whose PR number matches in the slug's repo. A url-less
/// same-number node counts (its repo cannot be proven foreign); slugs
/// compare case-insensitively.
fn repo_scoped_number_matches(rows: &[Value], pr: i64, our_slug: &str) -> Vec<String> {
    let mut matches: Vec<String> = Vec::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        for (num, url) in node_pr_refs(row) {
            if num != pr {
                continue;
            }
            match url.as_deref().and_then(repo_slug_from_url) {
                Some(node_slug) if !node_slug.eq_ignore_ascii_case(our_slug) => {}
                _ => matches.push(id.to_string()),
            }
            break;
        }
    }
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    matches.retain(|m| seen.insert(m.clone()));
    matches
}

/// `owner/repo` from a GitHub PR url, or None: scheme and credentials
/// optional, host exact, path exactly `owner/repo(.git)?/(pull|issues)/<n>`,
/// case-insensitive - the `_PR_URL_RE` grammar the Python leaf used.
fn repo_slug_from_url(url: &str) -> Option<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"^(?:[A-Za-z][A-Za-z0-9+.-]*://)?(?:[^/@]*@)?github\.com(?::\d+)?/([^/\s]+)/([^/\s]+?)(?:\.git)?/(?:pull|issues)/(\d+)(?:[/?#]|$)",
        )
        .expect("pr url grammar")
    });
    let caps = re.captures(url.trim())?;
    Some(format!("{}/{}", &caps[1], &caps[2]))
}

pub fn run_flow(argv: &[String], cwd: &Path, authority: &str, timeout_s: u64) -> FlowResult {
    let mut flow = FlowResult::new();
    let mut repo = cwd.to_path_buf();
    let mut pr_raw: Option<String> = None;
    let mut accept_flake = false;
    for arg in argv {
        if arg.starts_with("--invoker=") {
            continue;
        }
        if arg == "--accept-flake" {
            accept_flake = true;
            continue;
        }
        match arg.chars().next() {
            Some(c) if c.is_numeric() => pr_raw = Some(arg.clone()),
            _ => {
                flow.lines
                    .push((true, format!("Error: unknown arg '{arg}'\n")));
                flow.exit_code = 1;
                return flow;
            }
        }
    }
    let Some(raw) = pr_raw.as_deref() else {
        flow.lines.push((true, "pr_number required\n".into()));
        flow.exit_code = 1;
        return flow;
    };
    if !is_pr_number(raw) {
        emit(
            &mut flow,
            0,
            "failed",
            &format!("invalid pr number: {raw}"),
            "none",
            true,
        );
        flow.exit_code = 1;
        return flow;
    }
    let pr: i64 = raw.parse().expect("validated pr digits");

    match resolve_pr_worktree(pr, &repo) {
        Ok(wt) if !wt.as_os_str().is_empty() => repo = wt,
        Ok(_) => {}
        Err(message) => {
            emit(&mut flow, pr, "held", &message, "none", true);
            flow.exit_code = 2;
            return flow;
        }
    }

    // Incarnation fence: silent no-op at this stage. The fence stage adds the
    // claim check; today absence fails open, exactly like a crashed Python
    // check does.

    // Posture: which auto-merge posture to hand the owner. The durable-grant
    // arm replaces the manifest for a parked worker; everything else rides
    // the manifest fields as written.
    let approved_field = manifest_field(&repo, "auto_merge_approved").unwrap_or_default();
    let mut posture_source = manifest_field(&repo, "auto_merge_source")
        .unwrap_or_default()
        .trim()
        .to_string();
    let posture_approved: Option<bool>;
    if authority == "durable_grant" {
        let (state, reason) = resolve_durable_grant(pr, &repo);
        if state != "granted" {
            let (word, err) = match state.as_str() {
                "refused" | "absent" => ("skipped", false),
                "held" => ("held", false),
                _ => ("blocked", true),
            };
            emit(
                &mut flow,
                pr,
                word,
                &format!("durable-grant merge not executable: {reason}"),
                "none",
                err,
            );
            flow.exit_code = 2;
            return flow;
        }
        // The resolver already said eligible and re-checked the standing
        // config itself; naming its own source keeps it out of the
        // manifest-posture arm so the owner still holds it to the live
        // switch and the floor.
        posture_approved = Some(true);
        posture_source = "durable-grant".to_string();
    } else if approved_field.is_empty() {
        // No manifest, or a manifest predating the field: the live config
        // decides on its own. A manual merge outside a target session is
        // legitimate.
        posture_approved = None;
    } else {
        posture_approved = Some(approved_true(&approved_field));
    }

    // (2) gh must be installed.
    if !gh_on_path() {
        emit(
            &mut flow,
            pr,
            "failed",
            "gh CLI not installed",
            "none",
            true,
        );
        flow.exit_code = 127;
        return flow;
    }

    let require_checks = crate::agents_config::auto_merge_require_checks_pass(&repo);

    // (2a) Coverage read, staged port: the Python gate reads the PR head here
    // (its own verb hop) plus two REST reads and returns the covered head.
    // This stage keeps the verb hop and ignores the answer - under the test
    // stub the answer never comes, which is the exact shape every captured
    // golden already exercises - and the covered head is the empty string end
    // to end. The REST reads and the coverage verdict land with the
    // follow-ups stage.
    let _ = verb_call(
        "authorized-merge",
        &json!({"op": "pr-head", "pr": pr, "cwd": repo.to_string_lossy()}),
        VERB_TIMEOUT,
    );
    let covered_head = "";

    // (2b) The flake probe, pinned to the covered head (immutable history at
    // that SHA), outside the lock so the read never stretches the merge
    // window. The hold/journal decision stays in the flow below, ahead of
    // the stamp.
    let flake: Option<Value> = if require_checks {
        match verb_call(
            "authorized-merge",
            &json!({"op": "status-rerun", "cwd": repo.to_string_lossy(), "pr": pr, "sha": covered_head}),
            FLAKE_PROBE_TIMEOUT,
        ) {
            Ok(v) => Some(v),
            Err(e) => {
                flow.lines.push((
                    true,
                    format!(
                        "pr-merge: rerun-recovery probe unavailable ({}); merging without the flake hold\n",
                        e.message
                    ),
                ));
                None
            }
        }
    } else {
        None
    };

    // (2c) Merge serialization: builds run parallel, merges run one at a
    // time. The authorized-merge gate reads CI freshness under the lock, so
    // a peer merge cannot land between that read and this merge.
    let lock = acquire_merge_lock(&repo, pr);
    match &lock {
        LockOutcome::Unavailable(exc) => {
            flow.lines.push((
                true,
                format!("pr-merge: merge lock unavailable ({exc}); proceeding\n"),
            ));
        }
        LockOutcome::Held(reason) => {
            emit(&mut flow, pr, "held", reason, "none", false);
            flow.exit_code = 2;
            return flow;
        }
        // Binding the guard counts as the field's read; its Drop in `lock`
        // releases the claim only after the merge below finishes.
        LockOutcome::Acquired(_guard) => {}
    }
    let code = do_merge(
        &mut flow,
        pr,
        &repo,
        authority,
        accept_flake,
        timeout_s,
        flake.as_ref(),
        posture_approved,
        &posture_source,
        require_checks,
        covered_head,
    );
    drop(lock);
    flow.exit_code = code;
    flow
}

#[allow(clippy::too_many_arguments)]
fn do_merge(
    flow: &mut FlowResult,
    pr: i64,
    repo: &Path,
    authority: &str,
    accept_flake: bool,
    timeout_s: u64,
    flake: Option<&Value>,
    posture_approved: Option<bool>,
    posture_source: &str,
    require_checks: bool,
    covered_head: &str,
) -> i32 {
    let strategy = crate::agents_config::auto_merge_strategy(repo);
    let ask = Ask {
        approved: posture_approved,
        source: posture_source,
        require_checks,
        covered_head: covered_pin(covered_head),
        authority,
        accept_flake,
    };

    // Authorize BEFORE publishing anything. The coverage status greens the
    // head for the web button, which enforces only the coverage context, so
    // stamping it ahead of a refusal would open the door this flow is about
    // to close.
    let decision = authorized_merge(pr, repo, &ask, true, timeout_s);
    if decision.get("outcome").and_then(Value::as_str) != Some("authorized") {
        return emit_authorized_outcome(flow, pr, &decision, &strategy);
    }

    // The flake hold: a rerun-recovered green is not a clean green. The probe
    // ran before the lock; this is only the receipt. It fires on an accepted
    // rerun recovery and lands with the follow-ups stage (event + note).
    if accept_flake
        && flake
            .and_then(|f| f.get("recovered"))
            .and_then(Value::as_bool)
            == Some(true)
    {
        // follow-ups stage: the merge-flake-accepted event and the
        // "flake override accepted: <checks>" stderr note.
    }

    // Server-visible receipt of the verdict the gate acted on, posted to the
    // head GitHub names for the PR. The head resolution stays the staged verb
    // hop the captured goldens record - the fold parity reads it - and the
    // POST rides the in-process publisher. Best-effort: the durable verdict
    // is the event row, and a failed POST leaves the status absent, which the
    // ruleset reads as not-passing.
    let publish_head = match verb_call(
        "authorized-merge",
        &json!({"op": "pr-head", "pr": pr, "cwd": repo.to_string_lossy()}),
        VERB_TIMEOUT,
    ) {
        Ok(v) => v
            .get("head")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    };
    let (posted, note) = if publish_head.is_empty() {
        (false, "no PR head to publish a status on".to_string())
    } else {
        match u64::try_from(pr) {
            Ok(pr_u64) => {
                crate::loopcheck::coverage_gate::publish::publish(repo, pr_u64, Some(&publish_head))
            }
            Err(_) => (false, format!("pr number {pr} is not a u64")),
        }
    };
    if !posted {
        flow.lines.push((
            true,
            format!("pr-merge: coverage status not posted: {note}\n"),
        ));
    }

    // The effect re-runs the whole decision, so the window the receipt opened
    // is closed again by the same guards, and the merge stays pinned to the
    // head those guards read. The second pass is the price of that window,
    // not an oversight: do not fold it into one pass.
    let receipt = authorized_merge(pr, repo, &ask, false, timeout_s);
    if receipt.get("outcome").and_then(Value::as_str) == Some("merged") {
        // follow-ups stage: the confirmed-merge finish (origin fetch, note,
        // branch cleanup, node close) lands there. At this stage the merged
        // receipt renders through the same table as every other outcome.
    }
    emit_authorized_outcome(flow, pr, &receipt, &strategy)
}

/// The head a COVERED gate pins its merge to. Staged port: the REST head read
/// (and the gate's own fallback) land with the follow-ups stage, so an empty
/// gate head stays empty - the owner reads that as unknown and holds, the
/// fail-closed answer every other unpinnable merge gets.
fn covered_pin(covered_head: &str) -> String {
    covered_head.to_string()
}

/// Ask the one authorized-merge operation. The decision lives in
/// `authorized_merge.rs`; this module is the transport and the renderer. An
/// unavailable binary is reported as `unknown`, never as a clear answer: a
/// merge whose authorization could not be read has not been authorized.
fn authorized_merge(pr: i64, repo: &Path, ask: &Ask, decide_only: bool, timeout_s: u64) -> Value {
    let mut payload = Map::new();
    payload.insert("cwd".into(), json!(repo.to_string_lossy()));
    payload.insert("pr".into(), json!(pr));
    payload.insert("effect".into(), json!("merge"));
    payload.insert("auto_merge_source".into(), json!(ask.source));
    payload.insert("require_checks".into(), json!(ask.require_checks));
    payload.insert("decide_only".into(), json!(decide_only));
    payload.insert("authority".into(), json!(ask.authority));
    payload.insert("accept_flake".into(), json!(ask.accept_flake));
    if let Some(approved) = ask.approved {
        payload.insert("approved".into(), json!(approved));
    }
    if !ask.covered_head.is_empty() {
        payload.insert("covered_head".into(), json!(ask.covered_head));
    }
    match verb_call(
        "authorized-merge",
        &Value::Object(payload),
        Duration::from_secs(timeout_s),
    ) {
        Ok(v) => v,
        Err(e) => json!({
            "outcome": "unknown",
            "detail": format!(
                "the authorized-merge owner could not be reached ({}); refusing to merge on an unread authorization",
                e.message
            ),
        }),
    }
}

/// Receipt word -> (emit word, exit code, stderr?). `failed` is the only arm
/// that spends the failure budget; every other refusal is the retry-or-
/// escalate 2 the merge lane already speaks.
fn outcome_emit(outcome: &str) -> (&'static str, i32, bool) {
    match outcome {
        "merged" => ("merged", 0, false),
        "armed" | "authorized" | "refused" => ("skipped", 2, false),
        "held" | "head_changed" | "unknown" => ("held", 2, false),
        "failed" => ("failed", 1, true),
        _ => ("held", 2, false),
    }
}

/// Render one authorized-merge receipt onto this verb's surface.
fn emit_authorized_outcome(flow: &mut FlowResult, pr: i64, receipt: &Value, strategy: &str) -> i32 {
    let outcome = match receipt.get("outcome") {
        None | Some(Value::Null) => "unknown".to_string(),
        Some(v) => py_str(v),
    }
    .pipe_if_empty("unknown");
    let (word, code, err) = outcome_emit(&outcome);
    let detail = match receipt.get("detail") {
        None | Some(Value::Null) => "no detail".to_string(),
        Some(v) => py_str(v),
    }
    .pipe_if_empty("no detail");
    emit(
        flow,
        pr,
        word,
        &format!("{outcome}: {detail}"),
        strategy,
        err,
    );
    flow.observed_head = receipt.get("observed_head").cloned();
    // code == 1: the failed arm also syncs the graph merge row; lands with
    // the follow-ups stage. No captured golden at this stage reaches it.
    code
}

trait PipeIfEmpty {
    fn pipe_if_empty(self, fallback: &str) -> String;
}

impl PipeIfEmpty for String {
    fn pipe_if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

fn resolve_durable_grant(pr: i64, repo: &Path) -> (String, String) {
    const UNKNOWN: &str = "unknown";
    let payload = json!({"op": "grant-verdict", "pr": pr, "cwd": repo.to_string_lossy()});
    let out = match verb_call("authorized-merge", &payload, VERB_TIMEOUT) {
        Ok(v) => v,
        Err(e) => {
            return (
                UNKNOWN.to_string(),
                format!(
                    "the durable-grant resolver could not be reached ({}); an unread grant never grants",
                    e.message
                ),
            )
        }
    };
    let state = match out.get("state") {
        None | Some(Value::Null) => UNKNOWN.to_string(),
        Some(v) => py_str(v),
    }
    .pipe_if_empty(UNKNOWN);
    let reason = match out.get("reason") {
        None | Some(Value::Null) => String::new(),
        Some(v) => py_str(v),
    }
    .pipe_if_empty("");
    (state, reason)
}

fn resolve_pr_worktree(pr: i64, repo: &Path) -> Result<PathBuf, String> {
    let payload = json!({"cwd": repo.to_string_lossy(), "pr": pr});
    match verb_call("pr-worktree", &payload, VERB_TIMEOUT) {
        Ok(v) => match v.get("worktree") {
            Some(w) => Ok(PathBuf::from(py_str(w))),
            // A zero-exit answer without the key surfaces as the KeyError
            // text the Python leg would raise.
            None => Err("'worktree'".to_string()),
        },
        Err(e) => {
            if e.returncode == Some(3) && e.message.contains("no local worktree") {
                Ok(PathBuf::new())
            } else {
                Err(e.message)
            }
        }
    }
}

fn emit(flow: &mut FlowResult, pr: i64, outcome: &str, reason: &str, strategy: &str, err: bool) {
    let mut row = Map::new();
    row.insert("pr".into(), json!(pr));
    row.insert("outcome".into(), json!(outcome));
    row.insert("reason".into(), json!(reason));
    row.insert("strategy".into(), json!(strategy));
    flow.receipt = Some(Value::Object(row.clone()));
    flow.lines.push((err, format!("{}\n", Value::Object(row))));
}

fn is_pr_number(raw: &str) -> bool {
    let mut chars = raw.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_digit() && c != '0')
        && chars.all(|c| c.is_ascii_digit())
}

/// Python truthy spellings for `auto_merge_approved`. Mirrored, not shared:
/// the finalize gate accepts only the literal `true`, so a widening here must
/// not silently move that gate.
fn approved_true(value: &str) -> bool {
    matches!(value.trim().to_lowercase().as_str(), "true" | "yes" | "1")
}

fn manifest_field(repo: &Path, field: &str) -> Option<String> {
    let path = crate::state_path::resolve("target-state", repo)?;
    let content = fs::read_to_string(path).ok()?;
    crate::loopcheck::scan_manifest_field(&content, field)
}

fn resolve_agents_bin() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("FNO_AGENTS_BIN").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    std::env::current_exe().ok()
}

/// One verb hop into the fno-agents binary: `[bin, verb]` with the JSON
/// payload on stdin. Stdout must be one JSON value; a non-zero exit or an
/// unreadable answer is a `VerbErr` naming the binary, never a panic.
fn verb_call(verb: &str, payload: &Value, timeout: Duration) -> Result<Value, VerbErr> {
    let bin = resolve_agents_bin().ok_or_else(|| VerbErr {
        message: BIN_MISSING.to_string(),
        returncode: None,
    })?;
    let wire = payload.to_string();
    let mut child = Command::new(&bin)
        .arg(verb)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| VerbErr {
            message: format!("fno-agents {verb} failed: {e}"),
            returncode: None,
        })?;
    {
        // The claim stub lanes never read stdin, so an early EPIPE here is
        // the normal case, not an error.
        let mut stdin = child.stdin.take().expect("piped stdin");
        let _ = stdin.write_all(wire.as_bytes());
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_string(&mut stdout);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                let rc = status.code().unwrap_or(-1);
                if rc != 0 {
                    return Err(VerbErr {
                        message: format!(
                            "fno-agents {verb} exited {rc}: {}",
                            stderr.trim().chars().take(200).collect::<String>()
                        ),
                        returncode: Some(rc),
                    });
                }
                return serde_json::from_str(stdout.trim()).map_err(|e| VerbErr {
                    message: format!("fno-agents {verb} bad output: {e}"),
                    returncode: None,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(VerbErr {
                        message: format!(
                            "fno-agents {verb} failed: timed out after {}s",
                            timeout.as_secs()
                        ),
                        returncode: None,
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(VerbErr {
                    message: format!("fno-agents {verb} failed: {e}"),
                    returncode: None,
                })
            }
        }
    }
}

/// `shutil.which("gh")`: a PATH scan for an executable file named `gh`.
fn gh_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("gh");
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if meta.permissions().mode() & 0o111 != 0 {
                        return true;
                    }
                    continue;
                }
                #[cfg(not(unix))]
                return true;
            }
        }
    }
    false
}

fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `urllib.parse.quote(key, safe="")`: unreserved bytes pass, everything
/// else becomes uppercase `%XX`.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn configured_claim_root() -> Option<PathBuf> {
    std::env::var_os("FNO_CLAIMS_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Mirror of `claims.io.claims_dir(None)` through the shared resolver: the
/// env root wins and nests `.fno/claims` beneath it; without one the repo
/// space holds the claims directory directly (no nested `.fno` segment).
/// The merge-lock key carries no global-id prefix, so the resolver never
/// routes it to the machine-wide root.
fn claim_path(cwd: &Path, key: &str) -> PathBuf {
    crate::claims_root::claims_dir_in(
        key,
        None,
        || Ok(cwd.to_path_buf()),
        std::env::var_os("FNO_CLAIMS_ROOT"),
        None,
    )
    .expect("cwd is in hand, the space fallback cannot fail")
    .join(format!("{}.lock", encode_key(key)))
}

fn claim_root_flags() -> Vec<String> {
    match configured_claim_root() {
        Some(root) => vec!["--root".to_string(), root.to_string_lossy().to_string()],
        None => Vec::new(),
    }
}

/// One claim operation through the binary: `[bin, claim, operation]` (+ key,
/// flags, `--json`), JSON on stdout. Empty stdout is an empty object, a
/// corrupted status row is an answer, and rc 1 with `held_by_other` is the
/// typed held error the merge lock polls on.
fn native_claim(operation: &str, key: Option<&str>, flags: &[String]) -> Result<Value, ClaimErr> {
    let bin = resolve_agents_bin().ok_or_else(|| {
        ClaimErr::Unavailable(
            "fno-agents claim unavailable: set FNO_AGENTS_BIN or reinstall fno".into(),
        )
    })?;
    let mut argv: Vec<String> = vec!["claim".into(), operation.to_string()];
    if let Some(k) = key {
        // The root op's key IS the question and an empty key is legal input;
        // every other operation skips an empty key. The merge verb never
        // calls root.
        if !k.is_empty() || operation == "root" {
            argv.push(k.to_string());
        }
    }
    argv.extend(flags.iter().cloned());
    argv.push("--json".into());
    let output = Command::new(&bin)
        .args(&argv)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| ClaimErr::Unavailable(format!("fno-agents claim could not run: {e}")))?;
    let rc = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let payload: Value = if stdout.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        match serde_json::from_str(stdout.trim()) {
            Ok(v) => v,
            Err(e) => {
                return Err(ClaimErr::Verdict(format!(
                    "fno-agents claim returned invalid JSON: {e}"
                )))
            }
        }
    };
    if rc == 1 && payload.get("outcome").and_then(Value::as_str) == Some("held_by_other") {
        return Err(ClaimErr::Held {
            holder: payload
                .get("holder")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown")
                .to_string(),
            pid: payload.get("pid").cloned().unwrap_or(Value::Null),
            host: payload
                .get("host")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown")
                .to_string(),
        });
    }
    // A corrupted row is an answer, not a failure: status prints its verdict
    // and exits 2 so shell callers notice.
    if operation == "status" && payload.get("state").and_then(Value::as_str) == Some("corrupted") {
        return Ok(payload);
    }
    if rc != 0 {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("exit {rc}")
        };
        return Err(ClaimErr::Verdict(format!(
            "fno-agents claim {operation} failed: {detail}"
        )));
    }
    if operation == "list" && payload.is_array() {
        return Ok(json!({ "rows": payload }));
    }
    if !payload.is_object() {
        return Err(ClaimErr::Verdict(
            "fno-agents claim returned a non-object JSON value".into(),
        ));
    }
    Ok(payload)
}

fn acquire_flags(holder: &str) -> Vec<String> {
    // acquire_claim(key, holder, reason="serialized PR merge (LD#9)"): no
    // ttl, no metadata, no host/harness; pid defaults to this process.
    let mut flags = vec![
        "--holder".to_string(),
        holder.to_string(),
        "--reason".to_string(),
        "serialized PR merge (LD#9)".to_string(),
        "--pid".to_string(),
        std::process::id().to_string(),
    ];
    flags.extend(claim_root_flags());
    flags
}

/// The merge lock's repo-wide key: one `merge:<canonical-root>` claim per
/// project, so every worktree lane contends on the SAME lock.
fn lock_repo_root(cwd: &Path) -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_REPO_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    match crate::paths::canonical_repo_root(cwd) {
        Some(p) => p,
        None => crate::paths::worktree_repo_root(cwd),
    }
}

/// Best-effort claim age for the held report; any failure just drops the
/// ", held Ns" suffix from the reason.
fn read_lock_age(cwd: &Path, key: &str) -> Option<i64> {
    let path = claim_path(cwd, key);
    let payload = native_claim("read-path", Some(&path.to_string_lossy()), &[]).ok()?;
    let acquired_at = payload.get("acquired_at").and_then(Value::as_i64)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(0.max((now_ms - acquired_at) / 1000))
}

impl Drop for MergeLockGuard {
    fn drop(&mut self) {
        // release_claim strict=False; pid-liveness frees the lock anyway, so
        // errors are ignored.
        let mut flags = vec![
            "--holder".to_string(),
            self.holder.clone(),
            "--with-claim".to_string(),
        ];
        flags.extend(claim_root_flags());
        let _ = native_claim("release", Some(&self.key), &flags);
    }
}

fn acquire_merge_lock(cwd: &Path, pr: i64) -> LockOutcome {
    let key = format!("merge:{}", lock_repo_root(cwd).display());
    let holder = format!("pr-merge:pr{pr}:{}", std::process::id());
    let deadline = Instant::now() + Duration::from_secs(MERGE_LOCK_WAIT_S);
    let mut held_detail: Option<(String, Value, String)> = None;
    loop {
        match native_claim("acquire", Some(&key), &acquire_flags(&holder)) {
            Ok(_) => return LockOutcome::Acquired(MergeLockGuard { key, holder }),
            Err(ClaimErr::Held {
                holder: h,
                pid,
                host,
            }) => held_detail = Some((h, pid, host)),
            // Stale claims are stolen by the claims layer, so a held report
            // always names a holder live at the last poll. A decode failure
            // degrades to unlocked: the lock is coordination, GitHub stays
            // the merge authority, and our own tooling failing must never
            // block a merge.
            Err(ClaimErr::Verdict(message)) => return LockOutcome::Unavailable(message),
            Err(ClaimErr::Unavailable(message)) => {
                // The transport being down still waits out the window: at
                // the exact moment two mergers race hardest is when the
                // serialization matters.
                let _ = message;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_secs(MERGE_LOCK_POLL_S));
    }
    let reason = match held_detail {
        Some((h, _pid, _host)) => {
            let mut age_part = String::new();
            if let Some(age) = read_lock_age(cwd, &key) {
                if age > 0 {
                    age_part = format!(", held {age}s");
                }
            }
            format!(
                "merge serialized: held by {h} (live at last poll{age_part}; waited {MERGE_LOCK_WAIT_S}s); retry"
            )
        }
        None => "merge serialized: another merge holds the lock; retry".to_string(),
    };
    LockOutcome::Held(reason)
}
