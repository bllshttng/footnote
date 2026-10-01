//! The effect gate: classify a tool call's external effect, file the approval
//! request, and read the verdict.
//!
//! Ported from `cli/src/fno/approvals` (models.py `classify_effect` +
//! `canonical_digest`, store.py `EffectStore.submit`): one Rust owner of the
//! effect table, the Python legs deleted in the same change and their callers
//! routed through the authorized-merge door. Unknown classes require
//! approval, so a new effect class is safe by default; every unreadable
//! store or config reads not-approved, so the gate fails closed.

use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The class table's dispositions, mirroring `EffectDisposition` in models.py.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Allow,
    RequireApproval,
    Deny,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Allow => "allow",
            Disposition::RequireApproval => "require_approval",
            Disposition::Deny => "deny",
        }
    }
}

/// Refused until a later explicit policy and adapter contract exist
/// (models.py DENIED_EFFECT_CLASSES).
const DENIED_EFFECT_CLASSES: [&str; 5] = [
    "financial.payment",
    "financial.commitment",
    "signature.contract",
    "employment.action",
    "infrastructure.destructive",
];

/// No external consequence, so no effect approval (models.py INERT_EFFECT_CLASSES).
const INERT_EFFECT_CLASSES: [&str; 2] = ["internal.draft", "internal.research"];

/// Classify an effect class. Function-agnostic: only the class is read.
pub fn classify(effect_class: &str) -> Disposition {
    if DENIED_EFFECT_CLASSES.contains(&effect_class) {
        return Disposition::Deny;
    }
    if INERT_EFFECT_CLASSES.contains(&effect_class) {
        return Disposition::Allow;
    }
    Disposition::RequireApproval
}

/// Digest a mapping so any change to any bound field changes the digest.
/// Byte-identical to models.py `canonical_digest`: sorted keys, `(",", ":")`
/// separators, raw UTF-8 (ensure_ascii=False).
pub fn canonical_digest(value: &Value) -> String {
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(*k).unwrap_or_default());
                    out.push(':');
                    write(&map[*k], out);
                }
                out.push('}');
            }
        }
    }
    let mut canonical = String::new();
    write(value, &mut canonical);
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

/// Python `datetime.isoformat()` on an aware UTC datetime: no fraction when
/// the microsecond is zero, `+00:00` suffix either way.
fn isoformat(t: chrono::DateTime<chrono::Utc>) -> String {
    if t.timestamp_subsec_micros() == 0 {
        t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    } else {
        t.to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
    }
}

// -- Tool call to effect class ----------------------------------------------

/// One tool call mapped to the effect class it would exercise.
pub struct MappedEffect {
    pub effect_class: &'static str,
    pub destination: String,
    pub action_digest: String,
}

/// First present input field among the candidates, else `unspecified`.
fn destination_from(input: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(v) = input.get(*key).and_then(Value::as_str) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "unspecified".to_string()
}

fn mapped(class: &'static str, destination: String, tool: &str, input: &Value) -> MappedEffect {
    MappedEffect {
        effect_class: class,
        destination,
        action_digest: canonical_digest(&json!({"tool": tool, "input": input})),
    }
}

/// Map one tool call to its effect class, or None when the call carries none.
/// `gh pr merge` and `git push` map to None on purpose: the merge gate and
/// git-protection own them, and this guard never decides them twice.
pub fn map_tool_call(tool_name: &str, tool_input: &Value) -> Option<MappedEffect> {
    if tool_name == "Bash" {
        return map_bash_command(
            tool_input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
    }
    let lower = tool_name.to_lowercase();
    if !tool_name.starts_with("mcp__") {
        return None;
    }
    if lower.contains("send") || lower.contains("post_message") {
        return Some(mapped(
            "external.communication",
            destination_from(
                tool_input,
                &[
                    "to",
                    "recipient",
                    "email",
                    "channel",
                    "channel_id",
                    "room",
                    "user",
                    "username",
                    "phone",
                    "webhook",
                ],
            ),
            tool_name,
            tool_input,
        ));
    }
    if lower.contains("publish") || lower.contains("create_post") || lower.contains("deploy") {
        return Some(mapped(
            "external.publication",
            destination_from(
                tool_input,
                &["site", "project", "target", "url", "domain", "page", "name"],
            ),
            tool_name,
            tool_input,
        ));
    }
    None
}

/// The Bash rows of the effect table. Tokens are matched on the command's
/// leading words; anything else is no effect and is never blocked here.
pub fn map_bash_command(command: &str) -> Option<MappedEffect> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let input = json!({ "command": command });
    // Merge effects belong to the merge gate and git-protection.
    if matches!(tokens.as_slice(), ["gh", "pr", "merge", ..])
        || matches!(tokens.as_slice(), ["git", "push", ..])
    {
        return None;
    }
    match tokens.as_slice() {
        ["gws", "gmail", "send", ..] => {
            let destination = tokens
                .iter()
                .position(|t| *t == "--to")
                .and_then(|i| tokens.get(i + 1))
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unspecified".to_string());
            Some(mapped(
                "external.communication",
                destination,
                "Bash",
                &input,
            ))
        }
        ["gh", "issue", "comment", ..] | ["gh", "pr", "comment", ..] => Some(mapped(
            "external.communication",
            "github".to_string(),
            "Bash",
            &input,
        )),
        ["gh", "repo", "delete", ..]
        | ["gh", "release", "delete", ..]
        | ["aws", "s3", "rm", ..] => Some(mapped(
            "infrastructure.destructive",
            "remote".to_string(),
            "Bash",
            &input,
        )),
        ["gcloud", ..] if tokens.iter().any(|t| *t == "delete") => Some(mapped(
            "infrastructure.destructive",
            "gcp".to_string(),
            "Bash",
            &input,
        )),
        ["stripe", verb, ..] if matches!(*verb, "charge" | "payout" | "transfer") => Some(mapped(
            "financial.payment",
            "stripe".to_string(),
            "Bash",
            &input,
        )),
        _ => None,
    }
}

// -- Submit (port of EffectStore.submit) ------------------------------------

/// Same schema as store.py `_SCHEMA`; either language may create the db first.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
    request_digest TEXT PRIMARY KEY,
    request_id     TEXT NOT NULL UNIQUE,
    principal_id   TEXT NOT NULL,
    work_order_id  TEXT NOT NULL,
    attempt_id     TEXT NOT NULL,
    effect_id      TEXT NOT NULL,
    effect_class   TEXT NOT NULL,
    destination    TEXT NOT NULL,
    action_digest  TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS decisions (
    request_digest       TEXT PRIMARY KEY REFERENCES requests(request_digest),
    deciding_principal_id TEXT NOT NULL,
    decision             TEXT NOT NULL,
    decided_at           TEXT NOT NULL,
    transport            TEXT
);
CREATE TABLE IF NOT EXISTS attempts (
    idempotency_key    TEXT PRIMARY KEY,
    effect_id          TEXT NOT NULL,
    work_order_id      TEXT NOT NULL,
    attempt_id         TEXT NOT NULL,
    request_digest     TEXT NOT NULL,
    action_digest      TEXT NOT NULL,
    destination        TEXT NOT NULL,
    effect_class       TEXT NOT NULL,
    adapter_id         TEXT NOT NULL,
    adapter_version    TEXT NOT NULL,
    state              TEXT NOT NULL,
    external_ref       TEXT,
    reconciliation_ref TEXT,
    dispatch_claimed   INTEGER NOT NULL DEFAULT 0,
    dispatch_token     TEXT,
    remote_idempotency INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS outbox (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    payload  TEXT NOT NULL
);
";

/// One exact effect request, bound field for bound field with
/// models.py `ApprovalRequest.bound_fields`.
pub struct EffectRequest {
    pub request_id: String,
    pub principal_id: String,
    pub work_order_id: String,
    pub attempt_id: String,
    pub effect_id: String,
    pub effect_class: String,
    pub destination: String,
    pub action_digest: String,
    pub created_at: String,
    pub expires_at: String,
}

impl EffectRequest {
    /// The digest a decision is bound to: `canonical_digest(bound_fields)`,
    /// the same computation models.py `request_digest` makes.
    pub fn request_digest(&self) -> String {
        canonical_digest(&json!({
            "principal_id": self.principal_id,
            "work_order_id": self.work_order_id,
            "attempt_id": self.attempt_id,
            "effect_id": self.effect_id,
            "effect_class": self.effect_class,
            "destination": self.destination,
            "action_digest": self.action_digest,
            "expires_at": self.expires_at,
        }))
    }
}

/// Open (creating if needed) the approvals db at `path`.
pub fn open_db(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=30000;",
    )
    .map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// The approvals db a hook fire uses: the payload's explicit `db` when the
/// Python store forwards one, else the state-layout resolver's
/// `approvals.db` (legacy root file until the migration moves it).
pub fn default_db_path(cwd: &Path) -> Result<PathBuf, String> {
    crate::agents_config::state_dir(cwd)
        .map(|root| crate::state_layout::place(&root, "approvals.db"))
        .ok_or_else(|| "no resolvable state root (set FNO_STATE_DIR or HOME)".to_string())
}

/// Record one exact request. A denied effect class never becomes pending;
/// a digest already on file is answered without a second row.
pub fn submit(
    conn: &Connection,
    events_path: Option<&Path>,
    request: &EffectRequest,
) -> Result<String, String> {
    if classify(&request.effect_class) == Disposition::Deny {
        return Err(format!(
            "denied_effect_class: effect class {} is denied by core policy",
            request.effect_class
        ));
    }
    let digest = request.request_digest();
    let event_id = format!(
        "{}-{}",
        &digest[..16],
        chrono::Utc::now().timestamp_micros()
    );
    let payload = json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "type": "approval_requested",
        "source": "approvals",
        "data": {
            "request_digest": digest,
            "request_id": request.request_id,
            "principal_id": request.principal_id,
            "work_order_id": request.work_order_id,
            "attempt_id": request.attempt_id,
            "effect_id": request.effect_id,
            "effect_class": request.effect_class,
            "destination": request.destination,
            "action_digest": request.action_digest,
            "expires_at": request.expires_at,
            "event_id": event_id,
        },
    })
    .to_string();
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| e.to_string())?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT request_digest FROM requests WHERE request_digest = ?",
            [&digest],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| {
            let _ = conn.execute_batch("ROLLBACK");
            e.to_string()
        })?;
    if existing.is_some() {
        conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
        return Ok(digest);
    }
    let insert = conn.execute(
        "INSERT INTO requests (request_digest, request_id, principal_id, work_order_id, \
         attempt_id, effect_id, effect_class, destination, action_digest, created_at, \
         expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        rusqlite::params![
            digest,
            request.request_id,
            request.principal_id,
            request.work_order_id,
            request.attempt_id,
            request.effect_id,
            request.effect_class,
            request.destination,
            request.action_digest,
            request.created_at,
            request.expires_at,
        ],
    );
    if let Err(e) = insert {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e.to_string());
    }
    if let Err(e) = conn.execute(
        "INSERT INTO outbox (event_id, payload) VALUES (?,?)",
        rusqlite::params![payload_of(&payload, &digest), payload],
    ) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e.to_string());
    }
    conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
    drain(conn, events_path);
    Ok(digest)
}

/// The outbox row's own id: stable per (digest, payload) so a re-drain after a
/// crash dedupes on it the way Python consumers do on `data.event_id`.
fn payload_of(payload: &str, digest: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{digest}{payload}").as_bytes())
    )
}

/// Emit owed events, then delete their rows. A failed append leaves the debt
/// for the next drain, the same at-least-once contract store.py documents.
fn drain(conn: &Connection, events_path: Option<&Path>) {
    let Some(events_path) = events_path else {
        return;
    };
    let rows: Vec<(i64, String)> = match conn
        .prepare("SELECT seq, payload FROM outbox ORDER BY seq")
        .and_then(|mut q| {
            q.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map(|iter| iter.filter_map(Result::ok).collect())
        }) {
        Ok(rows) => rows,
        Err(_) => return,
    };
    for (seq, payload) in rows {
        let event: Value = match serde_json::from_str(&payload) {
            Ok(event) => event,
            Err(_) => continue,
        };
        if crate::claims::append_event_line(events_path, &event, std::time::Duration::from_secs(2))
            .is_err()
        {
            break;
        }
        let _ = conn.execute("DELETE FROM outbox WHERE seq = ?", [seq]);
    }
}

// -- Verdict read ------------------------------------------------------------

/// Whether one request digest may run now, read the way store.py's
/// `_authorized_request` reads: approved, unexpired, class still legal, and
/// the deciding principal still named by config for that class. Anything
/// else, including an unreadable db or config, is a refusal.
pub fn verdict(
    conn: &Connection,
    cwd: &Path,
    digest: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool, String> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT effect_class, expires_at FROM requests WHERE request_digest = ?",
            [digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| e.to_string())?;
    let Some((effect_class, expires_at)) = row else {
        return Ok(false);
    };
    let decision: Option<(String, String)> = conn
        .query_row(
            "SELECT decision, deciding_principal_id FROM decisions WHERE request_digest = ?",
            [digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| e.to_string())?;
    let Some((decision, principal)) = decision else {
        return Ok(false);
    };
    if decision != "approved" {
        return Ok(false);
    }
    let expiry = chrono::DateTime::parse_from_rfc3339(&expires_at)
        .map_err(|e| format!("unreadable expires_at {expires_at}: {e}"))?
        .with_timezone(&chrono::Utc);
    if now >= expiry {
        return Ok(false);
    }
    if classify(&effect_class) == Disposition::Deny {
        return Ok(false);
    }
    Ok(authorized(cwd, &principal, &effect_class))
}

/// Whether `principal` may approve `effect_class` under
/// `config.approvals.authorized_principals` (the `*` wildcard class
/// included). Unconfigured means unauthorized: policy.py's fail-closed rule.
pub fn authorized(cwd: &Path, principal: &str, effect_class: &str) -> bool {
    // Merged across config tiers, the way Python's ConfigAuthority reads
    // them: a principal list split between the project and global config is
    // one policy, not two candidates where only the first counts.
    let Some(entries) =
        crate::agents_config::config_table_merged(cwd, &["approvals", "authorized_principals"])
    else {
        return false;
    };
    let named = |class: &str| {
        entries
            .get(class)
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .any(|p| p == principal)
            })
            .unwrap_or(false)
    };
    named(effect_class) || named("*")
}

// -- The hook verdict --------------------------------------------------------

/// The request a hook fire files for one mapped effect. The digest must be
/// STABLE across unchanged retries, so the expiry is floored to the hour and
/// the window rides 24 to 25 hours: a retry in the same hour re-reads the
/// same request, and a boundary retry re-files (fail toward re-approval).
/// ponytail: the 24h window is a constant; a config key waits for a second
/// caller.
pub fn hook_request(
    session: &str,
    effect_id: &str,
    mapped: &MappedEffect,
    now: chrono::DateTime<chrono::Utc>,
) -> EffectRequest {
    let hour_floor = {
        use chrono::Timelike;
        now.with_minute(0)
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now)
    };
    EffectRequest {
        request_id: format!("hook-{}", &mapped.action_digest[..16]),
        principal_id: session.to_string(),
        work_order_id: "unclaimed".to_string(),
        attempt_id: session.to_string(),
        effect_id: effect_id.to_string(),
        effect_class: mapped.effect_class.to_string(),
        destination: mapped.destination.clone(),
        action_digest: mapped.action_digest.clone(),
        created_at: isoformat(now),
        expires_at: isoformat(hour_floor + chrono::Duration::hours(25)),
    }
}

/// Judge one hooked tool call. `None` lets it run; `Some` is the refusal text
/// the hook returns, naming the class, the destination, the request digest,
/// and the exact approve command. The request is filed here, so a refusal is
/// recoverable by retrying the call unchanged once a principal approves.
pub fn judge(payload: &Value, cwd: &Path) -> Option<String> {
    let tool_name = payload.get("tool_name").and_then(Value::as_str)?;
    let tool_input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let mapped = map_tool_call(tool_name, &tool_input)?;
    if classify(mapped.effect_class) == Disposition::Deny {
        return Some(format!(
            "effect-guard: {} is denied by core policy (effect class {}); this call cannot be approved",
            tool_name, mapped.effect_class
        ));
    }
    if classify(mapped.effect_class) == Disposition::Allow {
        return None;
    }
    let now = chrono::Utc::now();
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown-session");
    let effect_id = payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("tool-{}", &mapped.action_digest[..16]));
    let mut request = hook_request(session, &effect_id, &mapped, now);
    // request_id is not a bound field, so it may derive from the request
    // digest: two sessions making the same call file different digests, and
    // each needs its own request row rather than a UNIQUE clash on a shared
    // action-derived id.
    request.request_id = format!("hook-{}", &request.request_digest()[..16]);
    let db_path = match payload.get("db").and_then(Value::as_str) {
        Some(db) => PathBuf::from(db),
        None => default_db_path(cwd).ok()?,
    };
    let refusal = |digest: &str, reason: &str| {
        Some(format!(
            "effect-guard: {} to {} needs approval (effect class {}, request {digest}).\n\
             Approve: fno inbox approvals decide {digest} --as <principal> --approve\n\
             Retry the same call unchanged once approved; an edited call files a new request.{reason}",
            tool_name, request.destination, mapped.effect_class,
        ))
    };
    let conn = match open_db(&db_path) {
        Ok(conn) => conn,
        Err(e) => {
            // Fail closed: an unreadable store refuses the effect.
            return Some(format!(
                "effect-guard: the approvals store is unreadable ({e}); refusing {} to {}",
                tool_name, request.destination
            ));
        }
    };
    let digest = match submit(&conn, Some(&events_for(cwd)), &request) {
        Ok(digest) => digest,
        Err(e) => {
            return Some(format!(
                "effect-guard: the request could not be recorded ({e}); refusing {} to {}",
                tool_name, request.destination
            ));
        }
    };
    match verdict(&conn, cwd, &digest, now) {
        Ok(true) => None,
        Ok(false) => refusal(&digest, ""),
        Err(e) => refusal(&digest, &format!(" (verdict unread: {e})")),
    }
}

/// The events file a hook fire drains to: the same resolution
/// `emit_guard_decision` uses.
fn events_for(cwd: &Path) -> PathBuf {
    crate::state_path::resolve("events", cwd).unwrap_or_else(|| crate::paths::events_path(cwd))
}

// -- Transports --------------------------------------------------------------

/// The `fno-agents hook effect-guard` entry: PreToolUse for non-Bash tools
/// (the `mcp__.*` matcher). Same stdin/stdout contract as pretooluse-bash.
pub fn run_hook(_args: &[String]) -> i32 {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let payload: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    match judge(&payload, &cwd) {
        None => {
            super::hook::emit_guard_decision(&cwd, "effect-guard", tool_name, false);
            super::hook::emit_allow()
        }
        Some(reason) => {
            super::hook::emit_guard_decision(&cwd, "effect-guard", tool_name, true);
            super::hook::emit_block(&reason)
        }
    }
}

/// Dispatch one `effect-` op from an `authorized-merge` payload, the same
/// transport the hold, freeze, grant and status ops ride. Always answers with
/// a JSON receipt; the verb's exit status answers only whether the op RAN.
pub fn run_op(op: &str, payload: &Value) -> String {
    match op {
        "effect-classify" => {
            let Some(class) = payload.get("effect_class").and_then(Value::as_str) else {
                return json!({"error": "payload needs effect_class"}).to_string();
            };
            json!({ "disposition": classify(class).as_str() }).to_string()
        }
        "effect-submit" => {
            let Some(request) = payload.get("request") else {
                return json!({"error": "payload needs request"}).to_string();
            };
            let Some(request) = request_from_value(request) else {
                return json!({"error": "request needs every bound field as a string"}).to_string();
            };
            let cwd = PathBuf::from(payload.get("cwd").and_then(Value::as_str).unwrap_or("."));
            let db_path = match payload.get("db").and_then(Value::as_str) {
                Some(db) => PathBuf::from(db),
                None => match default_db_path(&cwd) {
                    Ok(path) => path,
                    Err(e) => return json!({"result": "refused", "reason": "store_unavailable", "detail": e}).to_string(),
                },
            };
            let Ok(conn) = open_db(&db_path) else {
                return json!({"result": "refused", "reason": "store_unavailable", "detail": "approvals db unreadable"}).to_string();
            };
            let events_path = payload
                .get("events_path")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            match submit(&conn, events_path.as_deref(), &request) {
                Ok(digest) => json!({"result": "submitted", "request_digest": digest}).to_string(),
                Err(e) => {
                    let reason = e.split(':').next().unwrap_or("refused").to_string();
                    json!({"result": "refused", "reason": reason, "detail": e}).to_string()
                }
            }
        }
        "effect-verdict" => {
            let cwd = PathBuf::from(payload.get("cwd").and_then(Value::as_str).unwrap_or("."));
            let db_path = match payload.get("db").and_then(Value::as_str) {
                Some(db) => PathBuf::from(db),
                None => match default_db_path(&cwd) {
                    Ok(path) => path,
                    Err(e) => return json!({"verdict": "refuse", "reason": e}).to_string(),
                },
            };
            let Ok(conn) = open_db(&db_path) else {
                return json!({"verdict": "refuse", "reason": "approvals db unreadable"})
                    .to_string();
            };
            let mapped = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .and_then(|name| {
                    payload
                        .get("tool_input")
                        .cloned()
                        .map(|input| map_tool_call(name, &input))
                })
                .unwrap_or(None);
            let Some(mapped) = mapped else {
                return json!({"verdict": "allow", "reason": "no effect"}).to_string();
            };
            if classify(mapped.effect_class) == Disposition::Deny {
                return json!({"verdict": "deny", "effect_class": mapped.effect_class}).to_string();
            }
            let session = payload
                .get("session")
                .and_then(Value::as_str)
                .unwrap_or("unknown-session");
            let now = chrono::Utc::now();
            let request = hook_request(session, "door", &mapped, now);
            let digest = request.request_digest();
            match verdict(&conn, &cwd, &digest, chrono::Utc::now()) {
                Ok(true) => json!({"verdict": "allow"}).to_string(),
                Ok(false) => json!({
                    "verdict": "refuse",
                    "request_digest": digest,
                    "decide_command": format!(
                        "fno inbox approvals decide {digest} --as <principal> --approve"
                    ),
                })
                .to_string(),
                Err(e) => json!({"verdict": "refuse", "reason": e}).to_string(),
            }
        }
        other => json!({"error": format!("unknown op {other}")}).to_string(),
    }
}

fn request_from_value(v: &Value) -> Option<EffectRequest> {
    let field = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(EffectRequest {
        request_id: field("request_id")?,
        principal_id: field("principal_id")?,
        work_order_id: field("work_order_id")?,
        attempt_id: field("attempt_id")?,
        effect_id: field("effect_id")?,
        effect_class: field("effect_class")?,
        destination: field("destination")?,
        action_digest: field("action_digest")?,
        created_at: field("created_at")?,
        expires_at: field("expires_at")?,
    })
}

#[cfg(test)]
mod tests;
