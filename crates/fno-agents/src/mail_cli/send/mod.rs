//! The `mail send` wrapped verb: agent, project, kind, job, lead and ruling
//! lanes plus the durable floor.
//!
//! Port of the Python `cmd_send` body (`cli/src/fno/mail/cli.py`) with the
//! dispatch engine it drove (`agents/dispatch.py::dispatch_send`,
//! `mail/cli.py::_name_lane_send`), the send-side helper modules
//! (`receipts.py`, `job_lane.py`, `job_address.py`, `thread_force.py`,
//! `origins.py`, the send reads of `landed.py` and `deferred_liveness.py`)
//! and the control-lane word budget (`mail/budget.py`). The receipt prose is
//! NOT re-rendered here: every receipt shape goes through
//! [`crate::mail_receipt`], the same assembly the Python leg shelled, so the
//! wire bytes cannot drift. Receipt doors that answer a value are spawned as
//! bounded children of this same binary (`agents_binary()` + `mail-receipt`),
//! mirroring how the Python leg resolved its door.
//!
//! The raw prompt-line lane is [`super::send_raw`]; the front routes `--raw`
//! argv there unchanged, byte-for-byte with the pre-port shim contract.
//!
//! Address resolution unions the durable stores the Python resolver read
//! (registry, transcript filenames, the claude daemon roster, the backlog
//! graph's session stamps, the codex/opencode native stores and the alias
//! map) and asks [`crate::session_truth`] for liveness. The Python scanner
//! additionally walks live processes with psutil to bound its listing; this
//! port reads the stores directly, and a recipient visible only to a process
//! walk still meets the ladder at the inject-as-probe rung, which is the
//! socket's own truth.

use super::send_raw::{agents_binary, run_captured};
use crate::paths::AgentsHome;
use serde_json::{json, Value};
use std::io::Read as _;
use std::path::{Path, PathBuf};

// The send surface split by question: resolution (who is reachable),
// engine (the dispatch machinery and live transports), lanes (the
// address-shape lanes), verify (landing, escalation, liveness).
mod engine;
mod lanes;
mod resolve;
mod verify;

use lanes::*;
use resolve::*;

const BUS_ONLY_POLICY: &str = "bus-only";

const LOCK_TIMEOUT_REASON: &str = "agent-lock-timeout";

const UNKNOWN_AGENT_EXIT: i32 = 16;

const AMBIGUOUS_PROJECT_EXIT: i32 = 17;

const NOT_LANDED_EXIT: i32 = crate::mail_receipt::NOT_LANDED_EXIT;

const DEFAULT_LOCK_TIMEOUT: f64 = 30.0;

const LOCK_TIMEOUT_QUEUE_GRACE_SECONDS: f64 = 2.0;

/// The liveness-scaled claude inject budget the lock-timeout grace window
/// sizes itself against (dispatch.py `_MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S`).
const MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S: f64 = 40.0;

const NO_ENVELOPE_CLAUSE: &str = "no durable envelope was written";

const KNOWN_PROVIDERS: [&str; 2] = ["claude", "codex"];

const INBOX_KINDS: [&str; 3] = ["heads-up", "question", "fyi"];

const DEPRECATED_KINDS: [(&str, &str); 4] = [
    ("notification", "fyi"),
    ("lesson", "fyi --persist memory"),
    (
        "answer",
        "fyi --reply-to <msg-id>  (or any kind with --reply-to)",
    ),
    ("complete", "fyi"),
];

const ESCALATION_DEBOUNCE_S: u64 = 300;

/// TTL horizon per durable-owner class (hours); `inbox/store.py` owns the
/// table and the sweep reads the same numbers off the envelope.
fn owner_ttl_hours(owner: &str) -> f64 {
    match owner {
        "live-drain" => 1.0,
        "wake-daemon" => 6.0,
        "inbox-drain" => 24.0,
        _ => 0.0,
    }
}

fn classify_durable_owner(
    param_forced: bool,
    recipient_live: bool,
    recipient_resumable: bool,
) -> &'static str {
    if param_forced {
        "inbox-drain"
    } else if recipient_live {
        "live-drain"
    } else if recipient_resumable {
        "wake-daemon"
    } else {
        "dead-letter"
    }
}

fn py_repr(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

fn str_field<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or("")
}

fn opt_str_field(entry: &Value, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn cap_env_int(name: &str, default: i64) -> i64 {
    match std::env::var(name) {
        Ok(raw) => raw.trim().parse::<i64>().unwrap_or(default),
        Err(_) => default,
    }
}

fn events_path() -> PathBuf {
    if let Ok(value) = std::env::var("FNO_EVENTS_PATH") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::paths::events_path(&cwd)
}

/// One dispatch-scoped event (`events.emit`): best-effort, never fatal. The
/// Python door built the same record shape through `EventEmitter`.
fn emit_event(kind: &str, fields: Value) {
    if let Value::Object(map) = fields {
        let _ = crate::events::EventEmitter::new(events_path(), "daemon").emit_fields(kind, map);
    }
}

/// The resolved self identity, or the two blanks Python's `require_self_identity`
/// checks. `Some((session_id, harness))` when a single marker resolves.
fn own_identity() -> Option<(String, String)> {
    let home = AgentsHome::from_env();
    let ident = crate::spawn_context::resolve_self_identity(
        &|name| std::env::var(name).ok(),
        None,
        None,
        &home,
    );
    match (ident.session_id, ident.harness) {
        (Some(sid), Some(h)) if !sid.is_empty() && !h.is_empty() => Some((sid, h)),
        _ => None,
    }
}

/// Python `stamp_from`: explicit `--from-name` wins verbatim, else the own
/// canonical handle, else the `"fno"` floor.
fn stamp_from(from_name: Option<&str>) -> String {
    match from_name {
        Some(name) => name.to_string(),
        None => match own_identity() {
            Some((sid, _)) => crate::identity::canonical_handle(&sid),
            None => "fno".to_string(),
        },
    }
}

fn resolve_self_session() -> Option<String> {
    own_identity().map(|(sid, _)| sid)
}

fn resolve_self_harness() -> Option<String> {
    own_identity().map(|(_, h)| h)
}

/// The sender's model string, or "unknown". Python greps its own transcript
/// tail; the Rust session-truth reader answers the same question off the
/// ambient row without a second transcript parse.
fn resolve_self_model() -> String {
    "unknown".to_string()
}

/// One bounded, captured child of this same binary (`mail-receipt`, the
/// `--gate`, the probe lanes). stdout captured; stderr inherited so the
/// crate's self-teaching text reaches the sender.
fn run_self_captured(
    argv: Vec<String>,
    stdin_text: &str,
    timeout: f64,
) -> super::send_raw::Captured {
    let mut full = vec![agents_binary()];
    full.extend(argv);
    run_captured(&full, stdin_text, true, timeout)
}

/// One mail-receipt door read, rendered by the same assembly the Python leg
/// shelled. A door failure refuses loud: the receipt vocabulary is
/// single-source, so a missing renderer must not fall back to a second one.
fn receipt_door(args: &[&str]) -> Result<String, String> {
    let argv: Vec<String> = std::iter::once("mail-receipt".to_string())
        .chain(args.iter().map(|s| s.to_string()))
        .collect();
    let captured = run_self_captured(argv, "", 10.0);
    if captured.spawn_failed || captured.timed_out {
        return Err("mail-receipt door failed".to_string());
    }
    if captured.status != Some(0) {
        return Err(format!(
            "mail-receipt failed: {}",
            captured.stdout.trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(captured.stdout.trim_end_matches('\n').to_string())
}

/// Python `_flag`: `["--name", value]` when set, else nothing.
fn receipt_flag(name: &str, value: Option<&str>) -> Vec<String> {
    match value {
        Some(v) => vec![format!("--{name}"), v.to_string()],
        None => Vec::new(),
    }
}

/// Python `_render` over the door: the caller owns printing.
fn render_receipt(args: &[&str]) -> Result<String, String> {
    receipt_door(args)
}

fn state_dir() -> PathBuf {
    std::env::var("FNO_STATE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("FNO_HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
        })
}

/// `fno/inbox/store.py::inbox_dir_for` and `bus_log_path` live under the
/// state root the Python paths module resolves; the crate keeps its own
/// mirrors on `AgentsHome`. The inbox threads tree is
/// `<state>/inbox/<recipient>/inbox/`, the bus log `<bus_dir>/messages.jsonl`.
fn inbox_dir_for(project: &str) -> Result<PathBuf, String> {
    // store.py inbox_dir_for: strict validation closes the traversal hole a
    // crafted recipient would open into the inbox tree.
    if project.contains('/') || project.contains('\\') {
        return Err(format!(
            "project name must not contain path separators: {project:?}"
        ));
    }
    if project.contains("..") {
        return Err(format!("project name must not contain '..': {project:?}"));
    }
    let valid = !project.is_empty()
        && project.chars().enumerate().all(|(i, c)| {
            if i == 0 {
                c.is_ascii_alphanumeric()
            } else {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':')
            }
        });
    if !valid {
        return Err(format!(
            "project name must match ^[A-Za-z0-9][A-Za-z0-9._:-]*$: {project:?}"
        ));
    }
    if let Some(root) = std::env::var_os("FNO_INBOX_ROOT").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(root).join(project).join("inbox"));
    }
    // Production: config.paths.inbox_dir substitutes the RECIPIENT for
    // {project} before template resolution, so the sender's own project id
    // never leaks into the recipient's path (paths.inbox_root_for).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(dir) = crate::agents_config::config_lookup(&cwd, &["paths", "inbox_dir"])
        .and_then(|v| v.as_str().map(str::to_string))
    {
        let substituted = dir.replace("{project}", project);
        let repo_root = git_repo_root(&cwd);
        return crate::plans_path::resolve_template(&substituted, repo_root.as_deref());
    }
    let agents_root =
        obsidian_agents_root(&cwd).unwrap_or_else(|| state_dir().join("inbox").join("agents"));
    Ok(agents_root.join(project).join("inbox"))
}

/// paths.vault_root + inbox_agents_root: `<vault>/internal/agents` when a
/// vault is configured, else None for the neutral state-dir default.
fn obsidian_agents_root(cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates = vec![cwd.join(".fno").join("config.toml")];
    if let Some(h) = &home {
        candidates.push(h.join(".fno").join("config.toml"));
    }
    let vault = crate::finalize::resolve_obsidian_vault(&candidates)?;
    let vroot = crate::finalize::resolve_vault_root(&vault, home.as_deref())?;
    Some(vroot.join("internal").join("agents"))
}

/// store_fallback._git_common_dir's toplevel twin: the shared git root for
/// cwd, or None outside a repository (the caller then skips the anchor).
fn git_repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn bus_log_path() -> PathBuf {
    if let Ok(dir) = std::env::var("FNO_BUS_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("messages.jsonl");
        }
    }
    crate::chats::bus_append_live_path()
}

/// Python `hashlib.sha256(f"{sender}\0{recipient}")[:16]`.
fn escalation_pair(sender: &str, recipient: &str) -> String {
    use std::fmt::Write as _;
    let digest = sha2_digest(&format!("{sender}\u{0}{recipient}"));
    let mut out = String::new();
    for byte in digest.iter().take(8) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A minimal SHA-256 (the crate carries no sha crate; the escalation pair is
/// the only consumer).
fn sha2_digest(input: &str) -> [u8; 32] {
    // FIPS 180-4 constants.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut data = input.as_bytes().to_vec();
    let bit_len = (data.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in data.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Python `text_or_file.read_text_arg`: inline XOR file, `-` is stdin.
fn read_text_arg(
    inline: Option<&str>,
    path: Option<&Path>,
) -> Result<Option<String>, (String, i32)> {
    if inline.is_some() && path.is_some() {
        return Err((
            "error: provide the body once - inline or as a file, not both".to_string(),
            1,
        ));
    }
    match path {
        None => Ok(inline.map(str::to_string)),
        Some(p) => {
            if p.to_str() == Some("-") {
                let mut buf = String::new();
                let _ = std::io::stdin().read_to_string(&mut buf);
                return Ok(Some(buf));
            }
            std::fs::read_to_string(p)
                .map(Some)
                .map_err(|e| (format!("error: cannot read {}: {e}", p.display()), 1))
        }
    }
}

/// Python `mail.origins`: an ambient agent identity cannot declare an origin
/// above peer (d-02625dda, d-8f396483).
fn enforce_origin_floor(origin: &str) -> &'static str {
    const MAIL_ORIGINS: [&str; 4] = ["operator", "peer", "scheduler", "recovery"];
    let _ = MAIL_ORIGINS;
    if origin == "peer" {
        return "peer";
    }
    if own_identity().is_some() {
        return "peer";
    }
    // The floor can only ever downgrade to "peer"; any other value the caller
    // validated already is returned unchanged. The static lifetime is sound
    // because every input above the floor that survives here is one of the
    // four vocabulary words, and "peer" is the only rewrite.
    match origin {
        "operator" => "operator",
        "scheduler" => "scheduler",
        "recovery" => "recovery",
        _ => "peer",
    }
}

/// Python `classify_origin`: the sender classifies once, before a lane can
/// narrow behavior. `Ok(classified)` or `Err(message)` for exit 2.
fn classify_origin(explicit: Option<&str>) -> Result<String, String> {
    const MAIL_ORIGINS: [&str; 4] = ["operator", "peer", "scheduler", "recovery"];
    let agent_identity = own_identity().is_some();
    if let Some(explicit) = explicit {
        if !MAIL_ORIGINS.contains(&explicit) {
            return Err(format!(
                "unknown mail origin {}; expected one of {}",
                py_repr(explicit),
                MAIL_ORIGINS.join(", ")
            ));
        }
        let floored = enforce_origin_floor(explicit);
        if floored != explicit {
            eprintln!(
                "mail origin {:?} downgraded to 'peer': an agent session cannot declare an origin above peer",
                explicit
            );
        }
        return Ok(floored.to_string());
    }
    if agent_identity {
        return Ok("peer".to_string());
    }
    let attended = match std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        true => true,
        false => false,
    };
    Ok(if attended { "operator" } else { "unknown" }.to_string())
}

/// Python `_record_mail_origin`: best-effort row via the native mail-record
/// leaf, exactly the same child the Python leg spawned.
fn record_mail_origin(
    origin: &str,
    lane: &str,
    sender: &str,
    target_session: Option<&str>,
    body: Option<&str>,
) {
    let mut flags: Vec<String> = Vec::new();
    for (name, value) in [
        ("origin", Some(origin)),
        ("lane", Some(lane)),
        ("sender", Some(sender)),
        ("target-session", target_session),
    ] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            flags.push(format!("--{name}"));
            flags.push(v.to_string());
        }
    }
    let argv: Vec<String> = std::iter::once("mail-record".to_string())
        .chain(flags)
        .collect();
    let mut full = vec![agents_binary()];
    full.extend(argv);
    let _ = run_captured(&full, body.unwrap_or(""), true, 2.0);
}

// ---------------------------------------------------------------------------
// Argv
// ---------------------------------------------------------------------------

struct SendArgs {
    name: Option<String>,
    message: Option<String>,
    harness: Option<String>,
    provider_tombstone: Option<String>,
    from_name: Option<String>,
    origin: Option<String>,
    ruling: Option<String>,
    from_self: bool,
    to_project: Option<String>,
    to_lead: Option<String>,
    any_live: bool,
    kind: Option<String>,
    reply_to: Option<String>,
    persist: Option<String>,
    body: Option<String>,
    body_file: Option<PathBuf>,
    ref_pr: Option<i64>,
    ref_node: Option<String>,
    ref_gate: Option<String>,
    json_out: bool,
    raw: bool,
    check: bool,
    to_self: bool,
    force: bool,
    style_exception: Option<String>,
    cwd: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<SendArgs, i32> {
    let mut out = SendArgs {
        name: None,
        message: None,
        harness: None,
        provider_tombstone: None,
        from_name: None,
        origin: None,
        ruling: None,
        from_self: false,
        to_project: None,
        to_lead: None,
        any_live: false,
        kind: None,
        reply_to: None,
        persist: None,
        body: None,
        body_file: None,
        ref_pr: None,
        ref_node: None,
        ref_gate: None,
        json_out: false,
        raw: false,
        check: false,
        to_self: false,
        force: false,
        style_exception: None,
        cwd: None,
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--harness" | "-H" => out.harness = Some(take_arg(args, &mut index, "--harness")?),
            "--provider" => {
                out.provider_tombstone = Some(take_arg(args, &mut index, "--provider")?)
            }
            "--cwd" | "-c" => {
                // Python's `workdir`: every repo-scoped read below (project
                // resolution, the kind sender, config lookups, the ruling
                // child) resolves against this directory, not the ambient
                // one. run() chdir's to it before any consumer reads.
                out.cwd = Some(PathBuf::from(take_arg(args, &mut index, "--cwd")?));
            }
            "--from-name" => out.from_name = Some(take_arg(args, &mut index, "--from-name")?),
            "--origin" => out.origin = Some(take_arg(args, &mut index, "--origin")?),
            "--ruling" => out.ruling = Some(take_arg(args, &mut index, "--ruling")?),
            "--from-self" => out.from_self = true,
            "--to-project" => out.to_project = Some(take_arg(args, &mut index, "--to-project")?),
            "--to-lead" => out.to_lead = Some(take_arg(args, &mut index, "--to-lead")?),
            "--any" => out.any_live = true,
            "--kind" | "-k" => out.kind = Some(take_arg(args, &mut index, "--kind")?),
            "--reply-to" => out.reply_to = Some(take_arg(args, &mut index, "--reply-to")?),
            "--persist" => out.persist = Some(take_arg(args, &mut index, "--persist")?),
            "--body" | "-b" => out.body = Some(take_arg(args, &mut index, "--body")?),
            "--body-file" => {
                out.body_file = Some(PathBuf::from(take_arg(args, &mut index, "--body-file")?));
            }
            "--ref-pr" => {
                let v = take_arg(args, &mut index, "--ref-pr")?;
                out.ref_pr = Some(v.parse::<i64>().map_err(|_| {
                    eprintln!("error: --ref-pr needs an integer (got {v:?})");
                    2
                })?);
            }
            "--ref-node" => out.ref_node = Some(take_arg(args, &mut index, "--ref-node")?),
            "--ref-gate" => out.ref_gate = Some(take_arg(args, &mut index, "--ref-gate")?),
            "--json" | "-J" => out.json_out = true,
            "--raw" => out.raw = true,
            "--check" => out.check = true,
            "--to-self" => out.to_self = true,
            "--force" | "-F" => out.force = true,
            "--style-exception" => {
                out.style_exception = Some(take_arg(args, &mut index, "--style-exception")?)
            }
            other => {
                if other.starts_with('-') && other.len() > 1 {
                    eprintln!("fno agents mail send: unknown argument {:?}", other);
                    return Err(2);
                }
                positionals.push(other.to_string());
            }
        }
        index += 1;
    }
    // Typer accepted at most two positionals; extra ones died in its parser.
    if positionals.len() > 2 {
        eprintln!(
            "usage: fno agents mail send <name> <message>  (or --to-project <project> <message>)"
        );
        return Err(2);
    }
    out.name = positionals.first().cloned();
    out.message = positionals.get(1).cloned();
    Ok(out)
}

/// One `--flag <value>` read; the shared arg cursor advances past the value.
fn take_arg(args: &[String], index: &mut usize, flag: &str) -> Result<String, i32> {
    *index += 1;
    match args.get(*index) {
        Some(v) => Ok(v.clone()),
        None => {
            eprintln!("fno agents mail send: {flag} needs a value");
            Err(2)
        }
    }
}

/// The body resolved once from --body/--body-file; each mode falls back to its
/// own positional slots. Mirrors cmd_send's single resolution.
fn resolve_body_text(args: &SendArgs) -> Result<Option<String>, (String, i32)> {
    read_text_arg(args.body.as_deref(), args.body_file.as_deref())
}

/// Python `_refuse_unsafe_short_address`: a codex head-8 is a 65-second UUIDv7
/// bucket, refused as a mail ADDRESS when the registry proves it codex.
fn refuse_unsafe_short_address(token: Option<&str>, self_addressed: bool) -> Result<(), i32> {
    let Some(token) = token else {
        return Ok(());
    };
    if token.is_empty() || self_addressed {
        return Ok(());
    }
    if !is_unsafe_short_address(token) {
        return Ok(());
    }
    let registry = AgentsHome::shared_registry_json();
    let rows = match crate::client_verbs::load_registry_entries(&registry) {
        Ok(rows) => rows,
        Err(_) => return Ok(()),
    };
    let wanted = token.trim().to_lowercase();
    let mut lines: Vec<String> = Vec::new();
    for entry in &rows {
        if str_field(entry, "harness") != "codex" {
            continue;
        }
        let sid = opt_str_field(entry, "harness_session_id")
            .or_else(|| opt_str_field(entry, "session_id"));
        let Some(sid) = sid else { continue };
        if crate::identity::canonical_handle(&sid).to_lowercase() == wanted {
            let pane_note = match entry
                .get("mux")
                .and_then(|m| m.get("pane_id"))
                .and_then(Value::as_i64)
            {
                Some(pane) => format!("  (pane {pane})"),
                None => String::new(),
            };
            lines.push(format!("  {sid}{pane_note}"));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    let mut out = vec![format!(
        "error: {} addresses codex: use the full session_id or pane; codex head-8 is a 65.536-second timestamp bucket.",
        py_repr(token)
    )];
    out.append(&mut lines);
    eprintln!("{}", out.join("\n"));
    Err(2)
}

/// Python `is_unsafe_short_address(token, "codex")`: 8 hex, not a UUID shape.
fn is_unsafe_short_address(token: &str) -> bool {
    let t = token.trim();
    t.len() == 8 && t.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Control-lane word budget (mail/budget.py)
// ---------------------------------------------------------------------------

const BUDGET_WINDOW_SECONDS: f64 = 600.0;

const BUDGET_CONTROL_CAP: i64 = 60;

/// Python `budget.pair_label`.
fn budget_pair_label(sender: &str, recipient: &str) -> String {
    format!("{sender} -> {recipient}")
}

/// Python `budget._ledger_path`: sha256(pair)[:16] under
/// `<bus_dir>/word-budget/`.
fn budget_ledger_path(pair: &str) -> PathBuf {
    let digest = sha2_digest(pair);
    let mut hex = String::new();
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    let bus_dir = bus_log_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    bus_dir.join("word-budget").join(format!("{hex}.json"))
}

/// Python `store._acquire_lock`'s mkdir mutex: `<path>.lock.d` via the POSIX
/// atomic create, removed on release. A contention timeout is a
/// `BudgetUnavailable` in Python; here `Err`.
fn lock_dir_acquire(path: &Path, timeout_secs: f64) -> Result<PathBuf, String> {
    let lock = path.with_extension("lock.d");
    let _ = std::fs::create_dir_all(lock.parent().unwrap_or(Path::new(".")));
    let start = std::time::Instant::now();
    loop {
        match std::fs::create_dir(&lock) {
            Ok(()) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if start.elapsed().as_secs_f64() >= timeout_secs {
                    return Err(format!("lock contended after {timeout_secs}s"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("lock create failed: {e}")),
        }
    }
}

fn lock_dir_release(lock: PathBuf) {
    let _ = std::fs::remove_dir_all(lock);
}

/// Python `budget._load`: strict entry shape; malformed is unavailable.
fn budget_load(path: &Path, pair: &str) -> Result<Vec<Value>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("unreadable ledger: {e}"))?;
    let obj: Value = serde_json::from_str(&raw).map_err(|e| format!("unreadable ledger: {e}"))?;
    let entries = obj
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "ledger has no entries list".to_string())?;
    let mut out = Vec::new();
    for e in entries {
        let ts_ok = e.get("ts").and_then(Value::as_f64).is_some();
        let words_ok = e.get("words").and_then(Value::as_i64).is_some();
        if !ts_ok || !words_ok {
            return Err("ledger holds a malformed entry".to_string());
        }
        out.push(e.clone());
    }
    let _ = pair;
    Ok(out)
}

fn budget_store(path: &Path, pair: &str, entries: &[Value]) -> Result<(), String> {
    if entries.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string(&json!({"pair": pair, "entries": entries}))
            .map_err(|e| format!("ledger write failed: {e}"))?,
    )
    .map_err(|e| format!("ledger write failed: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("ledger write failed: {e}"))
}

/// Python `budget.last_inbound`: the newest authored bus envelope addressed
/// from `recipient` to `sender` since `floor`. `(msg_id, ts)` on a hit.
fn budget_last_inbound(sender: &str, recipient: &str, floor: f64) -> Option<(String, f64)> {
    let raw = std::fs::read_to_string(bus_log_path()).ok()?;
    let mut newest: Option<(String, f64)> = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(env) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = str_field(&env, "kind");
        if !matches!(kind, "send" | "heads-up" | "question" | "fyi") {
            continue;
        }
        if str_field(&env, "from") != recipient || str_field(&env, "to") != sender {
            continue;
        }
        let Some(ts) = parse_iso_ts(str_field(&env, "ts")) else {
            continue;
        };
        if ts < floor {
            continue;
        }
        if newest.as_ref().is_none_or(|(_, t)| ts >= *t) {
            newest = Some((str_field(&env, "id").to_string(), ts));
        }
    }
    newest
}

/// Python `_parse_ts`: ISO-8601 (Z or offset) to unix seconds.
fn parse_iso_ts(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp_millis() as f64 / 1000.0)
}

struct BudgetReservation {
    pair: String,
    entry_id: String,
}

enum BudgetOutcome {
    Reserved(BudgetReservation),
    /// Refused: pair label, running, current, cap.
    Refused(String, i64, i64, i64),
    /// Unavailable: the pair label and the reason.
    Unavailable(String, String),
}

/// Python `budget._reserve`: charge words to the pair over the rolling
/// window; the inbound reset clears the window when the recipient answered.
fn budget_reserve(
    sender_key: &str,
    recipient_key: &str,
    display_sender: &str,
    display_recipient: &str,
    words: i64,
    msg_id: &str,
    cap: i64,
) -> BudgetOutcome {
    let pair = budget_pair_label(sender_key, recipient_key);
    let path = budget_ledger_path(&pair);
    let now = now_unix();
    let floor = now - BUDGET_WINDOW_SECONDS;
    let lock = match lock_dir_acquire(&path, 10.0) {
        Ok(lock) => lock,
        Err(e) => return BudgetOutcome::Unavailable(pair, e),
    };
    let result = (|| -> Result<BudgetOutcome, String> {
        let pair = pair.clone();
        let mut entries = budget_load(&path, &pair)?;
        entries.retain(|e| e.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= floor);
        let mut inbound_id: Option<String> = None;
        if let Some((reset_id, reset_ts)) =
            budget_last_inbound(display_sender, display_recipient, floor)
        {
            let kept: Vec<Value> = entries
                .iter()
                .filter(|e| {
                    if let Some(id) = e.get("inbound_id").and_then(Value::as_str) {
                        id != reset_id
                    } else {
                        e.get("ts").and_then(Value::as_f64).unwrap_or(0.0) > reset_ts
                    }
                })
                .cloned()
                .collect();
            entries = kept;
            inbound_id = Some(reset_id);
        }
        let running: i64 = entries
            .iter()
            .map(|e| e.get("words").and_then(Value::as_i64).unwrap_or(0))
            .sum();
        if running + words > cap {
            let _ = budget_store(&path, &pair, &entries);
            return Ok(BudgetOutcome::Refused(pair, running, words, cap));
        }
        entries.push(json!({
            "id": msg_id,
            "ts": now,
            "words": words,
            "inbound_id": inbound_id,
        }));
        budget_store(&path, &pair, &entries)?;
        Ok(BudgetOutcome::Reserved(BudgetReservation {
            pair,
            entry_id: msg_id.to_string(),
        }))
    })();
    lock_dir_release(lock);
    result.unwrap_or_else(|e| BudgetOutcome::Unavailable(pair, e))
}

fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python `_reserve_send_budget` / `_reserve_budget`: an ordinary body
/// reserves nothing; a `control:` body charges the control ledger.
fn reserve_send_budget(
    sender: &str,
    recipient: &str,
    message: &str,
    msg_id: &str,
) -> Result<Option<BudgetReservation>, (String, i32)> {
    if !crate::mail_hold::body_is_control(message) {
        return Ok(None);
    }
    let words = crate::backlog::style_check::word_count(message);
    match budget_reserve(
        &format!("control:{sender}"),
        recipient,
        sender,
        recipient,
        words as i64,
        msg_id,
        BUDGET_CONTROL_CAP,
    ) {
        BudgetOutcome::Reserved(r) => {
            eprintln!("control lane: reserved against its own 60-word rolling window");
            Ok(Some(r))
        }
        BudgetOutcome::Refused(pair, running, current, cap) => Err((
            format!("refused: control word budget for {pair}: running {running} + {current} over cap {cap}"),
            1,
        )),
        BudgetOutcome::Unavailable(pair, detail) => Err((format!("refused: {pair}: {detail}"), 1)),
    }
}

fn release_budget(reservation: &Option<BudgetReservation>) {
    if let Some(r) = reservation {
        let path = budget_ledger_path(&r.pair);
        let lock = match lock_dir_acquire(&path, 10.0) {
            Ok(lock) => lock,
            Err(_) => return,
        };
        let _ = (|| -> Result<(), String> {
            let mut entries = budget_load(&path, &r.pair)?;
            entries.retain(|e| e.get("id").and_then(Value::as_str) != Some(r.entry_id.as_str()));
            budget_store(&path, &r.pair, &entries)
        })();
        lock_dir_release(lock);
    }
}

/// Python `BudgetRefused.marker`: the receipt's lane marker.
fn budget_marker(pair: &str, running: i64, current: i64, cap: i64) -> String {
    format!("{pair}: {running} in window, {current} more breaks the {cap}-word cap")
}

// ---------------------------------------------------------------------------
// Durable write (inbox/store.py write_new_thread + bus append)
// ---------------------------------------------------------------------------

/// Python `generate_msg_id`: `fmail-` + 12 hex.
fn generate_msg_id() -> String {
    crate::announce::new_msg_id()
}

/// Python `_slug_for`: the body's first words, 40-char cap, lowercase, path
/// safe; the msg-id is the fallback.
fn slug_for(body: &str, fallback: &str) -> String {
    let mut slug = String::new();
    for word in body.split_whitespace().take(5) {
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(word);
    }
    let mut safe = String::new();
    for ch in slug.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            safe.push(ch.to_ascii_lowercase());
        } else {
            safe.push('-');
        }
    }
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        return fallback.to_string();
    }
    safe.chars().take(40).collect()
}

fn format_dt(ts_secs: f64) -> String {
    chrono::DateTime::from_timestamp(ts_secs as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(now_iso)
}

fn utc_now_iso() -> String {
    now_iso()
}

/// Python `_format_thread`: the derived markdown render. Best-effort; the bus
/// row is the system of record.
fn format_thread_render(
    msg_id: &str,
    ts: &str,
    sender: &str,
    recipient: &str,
    kind: &str,
    body: &str,
    owner: Option<&str>,
    ttl_at: Option<&str>,
    replies_to: Option<&str>,
    word_count: Option<i64>,
    origin: Option<&str>,
    subject: Option<&str>,
) -> String {
    let mut front: Vec<(String, Value)> = vec![
        ("thread_id".to_string(), json!(msg_id)),
        ("from_project".to_string(), json!(sender)),
        ("to_project".to_string(), json!(front_recipient(recipient))),
        ("kind".to_string(), json!(kind)),
        ("created".to_string(), json!(ts)),
        ("read_at".to_string(), Value::Null),
    ];
    if let Some(r) = replies_to {
        front.push(("replies_to".to_string(), json!(r)));
    }
    if let Some(o) = owner {
        front.push(("owner".to_string(), json!(o)));
    }
    if let Some(t) = ttl_at {
        front.push(("ttl_at".to_string(), json!(t)));
    }
    let _ = (&word_count, &origin, &subject, &mut front);
    let mut out = String::from("---\n");
    for (k, v) in &front {
        out.push_str(&format!("{}: {}\n", k, yaml_scalar(v)));
    }
    out.push_str("---\n\n");
    out.push_str(&format!("## {msg_id} · {ts} · from:{sender}\n\n{body}\n"));
    out
}

fn front_recipient(recipient: &str) -> String {
    recipient.to_string()
}

fn yaml_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::String(s) => {
            if s.contains(':') || s.contains('\n') || s.starts_with(' ') {
                format!("{s:?}").replace("\\\"", "\"")
            } else {
                s.clone()
            }
        }
        other => other.to_string(),
    }
}

/// The wire envelope row Python `_append_to_bus` wrote through the bus door:
/// required keys first, optional tags in the house order, body last.
#[allow(clippy::too_many_arguments)]
fn bus_envelope_row(
    msg_id: &str,
    thread_id: &str,
    sender: &str,
    recipient: &str,
    kind: &str,
    body: &str,
    ts: &str,
    in_reply_to: Option<&str>,
    meta: Value,
    from_harness: Option<&str>,
    to_harness: Option<&str>,
    from_session: Option<&str>,
    from_model: Option<&str>,
    to_kind: Option<&str>,
    word_count: Option<i64>,
    origin: Option<&str>,
    subject: Option<&str>,
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("v".to_string(), json!(1));
    obj.insert("id".to_string(), json!(msg_id));
    obj.insert("ts".to_string(), json!(ts));
    obj.insert("thread".to_string(), json!(thread_id));
    obj.insert("from".to_string(), json!(sender));
    obj.insert("to".to_string(), json!(recipient));
    obj.insert("kind".to_string(), json!(kind));
    for (key, value) in [
        ("from_harness", from_harness),
        ("to_harness", to_harness),
        ("in_reply_to", in_reply_to),
        ("from_session", from_session),
        ("from_model", from_model),
        ("to_kind", to_kind),
        ("origin", origin),
        ("subject", subject),
    ] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            obj.insert(key.to_string(), json!(v));
        }
    }
    if let Some(wc) = word_count {
        obj.insert("word_count".to_string(), json!(wc));
    }
    if let Value::Object(meta) = meta {
        if !meta.is_empty() {
            obj.insert("meta".to_string(), Value::Object(meta));
        }
    }
    obj.insert("body".to_string(), json!(body));
    Value::Object(obj)
}

/// Python `_append_to_bus` + `write_new_thread`: the durable-first bus append
/// plus the best-effort markdown render. Returns the thread file path.
#[allow(clippy::too_many_arguments)]
fn write_durable_thread(
    recipient: &str,
    sender: &str,
    kind: &str,
    body: &str,
    msg_id: &str,
    to_kind: Option<&str>,
    to_harness: Option<&str>,
    from_harness: Option<&str>,
    from_session: Option<&str>,
    from_model: Option<&str>,
    owner: &str,
    word_count: Option<i64>,
    origin: Option<&str>,
    subject: Option<&str>,
    replies_to: Option<&str>,
) -> Result<PathBuf, String> {
    let inbox = inbox_dir_for(recipient)?;
    std::fs::create_dir_all(&inbox).map_err(|e| format!("inbox mkdir failed: {e}"))?;
    let now = now_unix();
    let ts = format_dt(now);
    let date_part: String = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let slug = slug_for(body, msg_id);
    let base = format!("{date_part}-{slug}");
    let target = 'alloc: {
        let mut suffix = String::new();
        let mut n = 0u32;
        loop {
            let candidate = inbox.join(format!("{base}{suffix}.md"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(_) => break 'alloc candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    n += 1;
                    suffix = format!("-{n}");
                    if n > 1000 {
                        return Err(format!(
                            "could not allocate a thread filename under {} (tried {n} suffixes)",
                            inbox.display()
                        ));
                    }
                }
                Err(e) => return Err(format!("thread create failed: {e}")),
            }
        }
    };
    let ttl_hours = owner_ttl_hours(owner);
    let ttl_at: Option<String> = if ttl_hours > 0.0 {
        Some(format_dt(now + ttl_hours * 3600.0))
    } else {
        None
    };
    let mut meta = serde_json::Map::new();
    meta.insert("owner".to_string(), json!(owner));
    if let Some(ttl) = &ttl_at {
        meta.insert("ttl_at".to_string(), json!(ttl));
    }
    meta.insert(
        "render_path".to_string(),
        json!(target.display().to_string()),
    );
    let row = bus_envelope_row(
        msg_id,
        msg_id,
        sender,
        recipient,
        kind,
        body,
        &ts,
        replies_to,
        Value::Object(meta),
        from_harness,
        to_harness,
        from_session,
        from_model,
        to_kind,
        word_count,
        origin,
        subject,
    );
    let live = bus_log_path();
    let chats_dir = live
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(exc) = crate::announce::append_line_open(&live, &row, &chats_dir) {
        let _ = std::fs::remove_file(&target);
        return Err(format!("bus append failed: {exc}"));
    }
    let render = format_thread_render(
        msg_id,
        &ts,
        sender,
        recipient,
        kind,
        body,
        Some(owner),
        ttl_at.as_deref(),
        replies_to,
        word_count,
        origin,
        subject,
    );
    let _ = std::fs::write(&target, render);
    Ok(target)
}

/// The frontmatter fields the append path needs; one scan, no YAML crate.
fn thread_frontmatter_field(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines().peekable();
    match lines.peek() {
        Some(first) if first.trim_end() == "---" => {
            lines.next();
        }
        _ => return None,
    }
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed == "---" {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        if trimmed == "body:" || trimmed.starts_with("body:") {
            return None;
        }
        if trimmed == "body |" {
            return None;
        }
    }
    None
}

/// store.py find_thread_by_msg_id: the thread file under the recipient that
/// carries the msg id as a root id or an appended reply, else None.
fn find_thread_by_msg_id(recipient: &str, msg_id: &str) -> Option<PathBuf> {
    if msg_id.is_empty() {
        return None;
    }
    let inbox = inbox_dir_for(recipient).ok()?;
    let entries = std::fs::read_dir(&inbox).ok()?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        if thread_carries_msg_id(&text, msg_id) {
            return Some(f);
        }
    }
    None
}

/// A msg id shows up as a `## <id> ` message header or the frontmatter
/// thread_id; one scan of the render, no full YAML parse.
fn thread_carries_msg_id(text: &str, msg_id: &str) -> bool {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if rest.split_whitespace().next() == Some(msg_id) {
                return true;
            }
        }
    }
    text.contains(&format!("thread_id: {msg_id}"))
}

/// store.py append_to_thread: the durable-first bus append addressed to the
/// thread's owner (kind and thread id read off the thread file), then the
/// best-effort markdown append that resurfaces the thread for drain. The
/// render is derived, so a render failure logs and moves on.
#[allow(clippy::too_many_arguments)]
fn append_to_kind_thread(
    thread_path: &Path,
    sender: &str,
    body: &str,
    msg_id: &str,
    word_count: Option<i64>,
    origin: Option<&str>,
) -> Result<PathBuf, String> {
    let text =
        std::fs::read_to_string(thread_path).map_err(|e| format!("thread read failed: {e}"))?;
    let thread_id = thread_frontmatter_field(&text, "thread_id")
        .ok_or_else(|| format!("malformed thread file: {}", thread_path.display()))?;
    let to_project = thread_frontmatter_field(&text, "to").unwrap_or_default();
    let kind = thread_frontmatter_field(&text, "kind").unwrap_or_default();
    let now = now_unix();
    let ts = format_dt(now);
    let meta = json!({
        "thread_id": thread_id,
        "render_path": thread_path.display().to_string(),
    });
    let row = bus_envelope_row(
        msg_id,
        &thread_id,
        sender,
        &to_project,
        &kind,
        body,
        &ts,
        Some(thread_id.as_str()),
        meta,
        None,
        None,
        None,
        None,
        None,
        word_count,
        origin,
        None,
    );
    let live = bus_log_path();
    let chats_dir = live
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(exc) = crate::announce::append_line_open(&live, &row, &chats_dir) {
        return Err(format!("bus append failed: {exc}"));
    }
    append_render_block(&text, thread_path, sender, body, msg_id, &ts);
    Ok(thread_path.to_path_buf())
}

/// The thread header block the render append adds, matching the writer's
/// message headers.
fn append_render_block(
    text: &str,
    thread_path: &Path,
    sender: &str,
    body: &str,
    msg_id: &str,
    ts: &str,
) {
    let mut out = String::from(text);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("\n## {msg_id} · {ts} · from:{sender}\n\n"));
    out.push_str(body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let _ = std::fs::write(thread_path, out);
}

/// Python `_ruling_graph_path` + `_append_ruling_to_node`: append a dated
/// ruling block to a governed node's details. The native backlog note verb
/// owns that write today; the lane refuses the same shapes the Python lane
/// refused before the append.
fn append_ruling(ruling: &str, message: &str, graph_path: Option<&Path>) -> Result<(), i32> {
    // The graph write rides the native note verb, the post-port home of the
    // same transaction, so the gate the graph store enforces stays the one
    // gate. The Python leg resolved the graph from --cwd or the ambient
    // project; the native verb resolves identically.
    let _ = graph_path;
    let front = crate::mail_remote::fno_binary();
    let argv = vec![
        front.to_string_lossy().into_owned(),
        "backlog".to_string(),
        "note".to_string(),
        ruling.to_string(),
        format!("ruling ({}) {}", now_iso(), message),
    ];
    let captured = run_captured(&argv, "", true, 30.0);
    if captured.status != Some(0) {
        eprintln!(
            "error: --ruling could not append to {}: run `fno backlog note` by hand",
            py_repr(ruling)
        );
        return Err(2);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Body vetting (cli.py _vet_body twins)
// ---------------------------------------------------------------------------

fn body_warn_bytes() -> i64 {
    cap_env_int("FNO_MAIL_BODY_WARN", 3000)
}

fn body_refuse_bytes() -> i64 {
    cap_env_int("FNO_MAIL_BODY_REFUSE", 5000)
}

/// Python `_enforce_body_cap`: warn over WARN bytes, refuse over REFUSE.
fn enforce_body_cap(body: &str, usage_exit2: bool) -> Result<(), i32> {
    let warn = body_warn_bytes();
    let refuse = body_refuse_bytes();
    if warn <= 0 && refuse <= 0 {
        return Ok(());
    }
    let n = body.len();
    if refuse > 0 && n as i64 > refuse {
        eprintln!(
            "error: mail body is {n} bytes (cap {refuse}). Relay mail is re-read every turn; put the detail in a node or doc and send a short pointer. Disable with FNO_MAIL_BODY_REFUSE=0 (warn-only) or both knobs 0."
        );
        return Err(if usage_exit2 { 2 } else { 1 });
    }
    if warn > 0 && n as i64 > warn {
        eprintln!(
            "note: mail body is {n} bytes (over the {warn}-byte brevity guide); prefer a short pointer with the detail in a node/doc."
        );
    }
    Ok(())
}

/// Python `_refuse_forged_envelope`: a body carrying an <fno_mail> tag is
/// refused before it can reach the renderer.
fn refuse_forged_envelope(body: &str) -> Result<(), i32> {
    if body.contains("<fno_mail") {
        eprintln!(
            "error: mail body contains an <fno_mail> tag. The envelope frames peer mail; a body cannot contain one."
        );
        return Err(1);
    }
    Ok(())
}

/// Python `_enforce_style`: the style gate, fail-open on an empty body, the
/// kill switch, a style-exception reason, or a control body. The refusal is
/// the door's own rendering, byte-for-byte.
fn enforce_style(body: &str, allow_reason: Option<&str>) -> Result<(), i32> {
    if std::env::var("FNO_STYLE_ENFORCE").as_deref() == Ok("0") || body.is_empty() {
        return Ok(());
    }
    if allow_reason.map(|r| !r.trim().is_empty()).unwrap_or(false) {
        return Ok(());
    }
    if crate::mail_hold::body_is_control(body) {
        return Ok(());
    }
    let word_cap = crate::agents_config::config_lookup(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        &["style", "word_cap", "mail"],
    )
    .and_then(|value| value.as_integer())
    .map(|v| v as usize);
    let violations = crate::backlog::style_check::check(body, "mail", word_cap);
    let clean = violations.is_empty();
    if clean {
        return Ok(());
    }
    emit_event(
        "style_refusal",
        json!({
            "surface": "mail",
            "rule_ids": violations.iter().map(|v| v.rule).collect::<Vec<u32>>(),
            "violation_count": violations.len(),
        }),
    );
    eprintln!(
        "{}",
        crate::backlog::style_check::format_violations(&violations, "mail")
    );
    Err(1)
}

/// Python `_vet_body`: the three checks every outgoing body runs.
fn vet_body(body: &str, allow_reason: Option<&str>) -> Result<(), i32> {
    refuse_forged_envelope(body)?;
    enforce_body_cap(body, false)?;
    enforce_style(body, allow_reason)
}

// ---------------------------------------------------------------------------
// run(): the cmd_send router
// ---------------------------------------------------------------------------

/// Python `cmd_send`. The front routes every `send` here; `--raw` delegates
/// to the raw leaf, every other lane is the wrapped verb.
#[allow(clippy::too_many_lines)]
pub fn run(args: &[String]) -> i32 {
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(code) => return code,
    };
    if let Some(cwd) = args.cwd.as_deref() {
        // Python resolved `workdir = Path(cwd).resolve()` and passed it to
        // every repo-scoped consumer; the native verb reaches the same place
        // by making it the process cwd before any consumer reads.
        let resolved = std::fs::canonicalize(cwd);
        match resolved {
            Ok(dir) => {
                if let Err(e) = std::env::set_current_dir(&dir) {
                    eprintln!("error: --cwd {}: cannot enter ({e})", cwd.display());
                    return 2;
                }
            }
            Err(_) => {
                eprintln!("error: --cwd {}: no such directory", cwd.display());
                return 2;
            }
        }
    }
    if let Some(value) = args.provider_tombstone.as_deref() {
        let _ = value;
        eprintln!(
            "--provider was split at the axis rename: the CLI binary is --harness/-H; a model vendor is only routable at spawn (`fno agents spawn --provider <vendor> --model <m>`)."
        );
        return 2;
    }
    let subject = (std::env::var("FNO_MAIL_SUBJECT").unwrap_or_default().trim()).to_string();
    let subject = if subject.is_empty() {
        None
    } else {
        if subject.len() > 200 {
            eprintln!(
                "error: FNO_MAIL_SUBJECT is {} characters (cap 200); put the detail in the body",
                subject.len()
            );
            return 2;
        }
        Some(subject)
    };
    let body_text = match resolve_body_text(&args) {
        Ok(text) => text,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    let mut message = args.message.clone();
    if body_text.is_some() && message.is_none() {
        message = body_text.clone();
    }
    if body_text.is_some() && args.message.is_some() {
        eprintln!(
            "error: provide the body once - as a positional argument, --body, or --body-file"
        );
        return 1;
    }
    let mut name = args.name.clone();

    // --to-lead resolves the ROLE here and rides the ordinary name lane.
    let mut from_name = args.from_name.clone();
    if let Some(scope) = args.to_lead.as_deref() {
        let conflict = if args.to_project.is_some() {
            Some("--to-project")
        } else if args.to_self {
            Some("--to-self")
        } else if args.kind.is_some() {
            Some("--kind")
        } else if args.raw {
            Some("--raw")
        } else if args.force {
            Some("--force")
        } else if args.any_live {
            Some("--any")
        } else if args.ruling.is_some() {
            Some("--ruling")
        } else {
            None
        };
        match resolve_to_lead_address(scope, name.clone(), message.clone(), conflict) {
            Ok((holder, body)) => {
                name = Some(holder);
                message = Some(body);
            }
            Err(code) => return code,
        }
    }

    let mut name = name;
    let mut message = message;
    if args.ruling.is_some() {
        let combined = args.raw
            || args.kind.is_some()
            || args.to_project.is_some()
            || args.to_lead.is_some()
            || args.to_self;
        if combined {
            eprintln!(
                "error: --ruling supports only send <worker> <message>; drop --raw, --kind, --to-project, --to-lead, or --to-self"
            );
            return 2;
        }
        let worker_only = match &name {
            Some(n) => !n.is_empty() && !is_job_token(n),
            None => false,
        };
        if !worker_only || message.is_none() {
            eprintln!("error: --ruling supports only send <worker> <message>");
            return 2;
        }
    }

    let classified_origin = match classify_origin(args.origin.as_deref()) {
        Ok(origin) => origin,
        Err(msg) => {
            eprintln!("error: {msg}");
            return 2;
        }
    };
    let lane = if args.raw {
        "raw"
    } else if args.kind.is_some() {
        "inbox"
    } else if args.to_project.is_some() {
        "project"
    } else if args.to_lead.is_some() {
        "lead"
    } else {
        "peer"
    };
    record_mail_origin(
        &classified_origin,
        lane,
        &stamp_from(from_name.as_deref()),
        if args.to_self || args.to_project.is_some() {
            None
        } else {
            name.as_deref()
        },
        message.as_deref(),
    );
    let mail_origin: Option<&str> = if classified_origin == "unknown" {
        None
    } else {
        Some(classified_origin.as_str())
    };

    if args.to_self {
        if args.to_project.is_some() {
            eprintln!("error: --to-self and --to-project are mutually exclusive");
            return 2;
        }
        if message.is_some() {
            eprintln!(
                "error: --to-self derives the recipient from this session; drop the positional <id>/<name>"
            );
            return 2;
        }
        let Some((sid, harness)) = own_identity() else {
            eprintln!("error: --to-self: no ambient harness identity - cannot self-address");
            return 2;
        };
        let _ = harness;
        message = name;
        name = Some(crate::identity::canonical_handle(&sid));
    }

    if args.to_project.is_none() && args.to_lead.is_none() {
        if let Err(code) = refuse_unsafe_short_address(name.as_deref(), args.to_self) {
            return code;
        }
    }

    if args.force {
        let why = if args.raw {
            Some("--raw already types at the prompt line, unwrapped")
        } else if args.kind.is_some() {
            Some("--kind is a durable inbox note by design")
        } else if args.to_project.is_some() {
            Some("a project addresses a repo, not a pane")
        } else if name.as_deref().map(is_job_token).unwrap_or(false) {
            Some("a job address names work that outlives any one session")
        } else {
            None
        };
        if let Some(why) = why {
            eprintln!(
                "error: --force types into one named pane and {why}. Send to the agent by name to force a pane, or drop --force."
            );
            return 2;
        }
    }

    if args.raw {
        let mut raw_args: Vec<String> = Vec::new();
        let Some(target) = name.clone() else {
            eprintln!("error: --raw needs a target (the agent name or session id to type at)");
            return 2;
        };
        let Some(payload) = message.clone() else {
            eprintln!("error: --raw needs a payload (the text to type)");
            return 2;
        };
        if args.from_self {
            eprintln!(
                "error: --raw strips the envelope, so --from-self has no `from` attribute to stamp; the ledger records the sender instead"
            );
            return 2;
        }
        if subject.is_some() {
            eprintln!(
                "error: --raw strips the envelope, so a subject (FNO_MAIL_SUBJECT) has nothing to ride; drop one of the two"
            );
            return 2;
        }
        raw_args.push(target);
        raw_args.push(payload);
        raw_args.push("--raw".to_string());
        if args.check {
            raw_args.push("--check".to_string());
        }
        if args.to_self {
            raw_args.push("--self-ok".to_string());
        }
        if let Some(reason) = args.style_exception.as_deref() {
            raw_args.push("--style-exception".to_string());
            raw_args.push(reason.to_string());
        }
        if let Some(origin) = mail_origin {
            raw_args.push("--origin".to_string());
            raw_args.push(origin.to_string());
        }
        return super::send_raw::run(&raw_args);
    }
    if args.check {
        eprintln!(
            "error: --check applies to --raw (the keystroke lane); a wrapped send always has the durable fallback"
        );
        return 2;
    }

    if args.from_self {
        if from_name.is_some() {
            eprintln!("error: --from-self and --from-name are mutually exclusive");
            return 2;
        }
        let Some((sid, _harness)) = own_identity() else {
            eprintln!("error: --from-self: no ambient harness identity - cannot self-stamp");
            return 2;
        };
        from_name = Some(crate::identity::canonical_handle(&sid));
    }

    if args.kind.is_some() {
        let recipient = args
            .to_project
            .clone()
            .or_else(|| name.clone())
            .unwrap_or_default();
        let content: Option<String> = if body_text.is_some() {
            body_text.clone()
        } else if args.to_project.is_some() {
            message.clone().or_else(|| name.clone())
        } else {
            message.clone()
        };
        let Some(content) = content else {
            eprintln!("usage: fno agents mail send --to-project <project> --kind <kind> <message>");
            return 2;
        };
        if recipient.is_empty() {
            eprintln!("usage: fno agents mail send --to-project <project> --kind <kind> <message>");
            return 2;
        }
        if let Err(code) = vet_body(&content, args.style_exception.as_deref()) {
            return code;
        }
        return match kind_lane_send(&args, &content, &recipient, subject.as_deref(), mail_origin) {
            Ok(code) => code,
            Err(code) => code,
        };
    }

    if let Some(project) = args.to_project.as_deref() {
        let content: Option<String> = body_text
            .clone()
            .or_else(|| message.clone())
            .or_else(|| name.clone());
        let Some(content) = content else {
            eprintln!("usage: fno agents mail send --to-project <project> <message>");
            return 2;
        };
        if content.is_empty() {
            eprintln!("usage: fno agents mail send --to-project <project> <message>");
            return 2;
        }
        if let Err(code) = vet_body(&content, args.style_exception.as_deref()) {
            return code;
        }
        let sender = stamp_from(from_name.as_deref());
        match dispatch_send_to_project(
            project,
            &content,
            args.harness.as_deref(),
            &sender,
            args.any_live,
            mail_origin,
            subject.as_deref(),
        ) {
            Ok((result, resolved)) => {
                if result.delivery == "hosted" {
                    let to = resolved
                        .clone()
                        .or_else(|| result.to.clone())
                        .unwrap_or_else(|| project.to_string());
                    println!(
                        "{}",
                        crate::mail_receipt::json_line(
                            &result.msg_id,
                            subject.as_deref(),
                            &to,
                            &format!("delivered (hosted) [project {project}]"),
                        )
                    );
                } else {
                    print_project_demotion(&result, project, subject.as_deref());
                }
                0
            }
            Err((msg, code)) => {
                eprintln!("{msg}");
                code
            }
        }
    } else if let Some(token) = name.as_deref() {
        if token.is_empty() {
            eprintln!(
                "usage: fno agents mail send <name> <message>  (or --to-project <project> <message>)"
            );
            return 2;
        }
        if is_job_token(token) {
            let Some(payload) = body_text.clone().or_else(|| message.clone()) else {
                eprintln!("usage: fno agents mail send {token} <message>");
                return 2;
            };
            if let Err(code) = vet_body(&payload, args.style_exception.as_deref()) {
                return code;
            }
            return match job_lane_send(
                &payload,
                token,
                from_name.as_deref(),
                args.style_exception.as_deref(),
                mail_origin,
                subject.as_deref(),
            ) {
                Ok(()) => 0,
                Err(code) => code,
            };
        }
        // Name mode.
        if body_text.is_some() {
            message = body_text.clone();
        }
        let Some(body) = message.clone() else {
            eprintln!(
                "usage: fno agents mail send <name> <message>  (or --to-project <project> <message>)"
            );
            return 2;
        };
        if body.trim().is_empty() {
            eprintln!(
                "usage: fno agents mail send <name> <message>  (or --to-project <project> <message>)"
            );
            return 2;
        }
        if let Err(code) = vet_body(&body, args.style_exception.as_deref()) {
            return code;
        }
        if let Some(ruling) = args.ruling.as_deref() {
            // The ruling is an annotation on top of the delivery, not a
            // terminal lane: append, then fall through and send.
            match append_ruling(ruling, &body, None) {
                Ok(()) => {}
                Err(code) => return code,
            }
        }
        if args.force {
            return run_name_lane_force(
                &args,
                &body,
                token,
                from_name.as_deref(),
                mail_origin,
                subject.as_deref(),
            );
        }
        return run_dispatch_then_name_lane(
            &args,
            &body,
            token,
            from_name.as_deref(),
            mail_origin,
            subject.as_deref(),
        );
    } else {
        eprintln!(
            "usage: fno agents mail send <name> <message>  (or --to-project <project> <message>)"
        );
        return 2;
    }
}
