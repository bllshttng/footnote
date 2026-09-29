//! Managed Claude credential freshness and ownership actions.
//!
//! The vault answers one question before a credential reaches the store or
//! shared slot: is it current, spendable, and proven to be the named account?

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const PROFILE_BETA: &str = "oauth-2025-04-20";
const FRESH_WINDOW_MS: i64 = 5 * 60 * 1000;
const SECURITY_ITEM_NOT_FOUND: i32 = 44;
const SECURITY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Eq)]
pub(crate) struct Principal {
    account_uuid: String,
    organization_uuid: String,
    // Carried for receipts ("wrong-account (<email>)"); identity is the uuids.
    email: Option<String>,
}

impl PartialEq for Principal {
    fn eq(&self, other: &Self) -> bool {
        self.account_uuid == other.account_uuid && self.organization_uuid == other.organization_uuid
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveClaude {
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalFailure {
    Rejected,
    Unavailable,
    Malformed,
}

pub(crate) trait External {
    fn keychain(&self, service: &str) -> Result<Option<String>, ExternalFailure>;
    fn profile(&self, bearer: &str) -> Result<Principal, ExternalFailure>;
    fn refresh(&self, refresh_token: &str) -> Result<Value, ExternalFailure>;
    fn live_claude(&self) -> Vec<LiveClaude>;
    // Interactive: runs `claude auth login --claudeai` with stdio inherited.
    // None config_dir means the unscoped item (no CLAUDE_CONFIG_DIR).
    fn login(&self, config_dir: Option<&Path>, email: Option<&str>) -> Result<(), ExternalFailure>;
}

struct SystemExternal;

impl External for SystemExternal {
    fn keychain(&self, service: &str) -> Result<Option<String>, ExternalFailure> {
        if !cfg!(target_os = "macos") {
            return Ok(None);
        }
        let account = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "user".to_string());
        let mut child = Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", &account, "-w"])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| ExternalFailure::Unavailable)?;
        let deadline = Instant::now() + SECURITY_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ExternalFailure::Unavailable);
                }
            }
        };
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .ok_or(ExternalFailure::Unavailable)?
            .read_to_string(&mut stdout)
            .map_err(|_| ExternalFailure::Unavailable)?;
        if status.code() == Some(SECURITY_ITEM_NOT_FOUND) {
            return Ok(None);
        }
        if !status.success() {
            return Err(ExternalFailure::Unavailable);
        }
        let blob = stdout.trim().to_string();
        Ok(has_credential(&blob).then_some(blob))
    }

    fn profile(&self, bearer: &str) -> Result<Principal, ExternalFailure> {
        let (status, body) = curl_json(&format!(
            "url = \"{}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"anthropic-beta: {}\"\n",
            PROFILE_URL,
            curl_escape(bearer),
            PROFILE_BETA
        ))?;
        if status == 401 || status == 403 {
            return Err(ExternalFailure::Rejected);
        }
        if status != 200 {
            return Err(ExternalFailure::Unavailable);
        }
        principal_from_profile(&body).ok_or(ExternalFailure::Malformed)
    }

    fn refresh(&self, refresh_token: &str) -> Result<Value, ExternalFailure> {
        let form = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            form_escape(refresh_token),
            CLIENT_ID
        );
        let config = format!(
            "url = \"{}\"\nrequest = \"POST\"\nheader = \"Content-Type: application/x-www-form-urlencoded\"\ndata = \"{}\"\n",
            TOKEN_URL,
            curl_escape(&form)
        );
        let (status, body) = curl_json(&config)?;
        if (status == 400 || status == 401)
            && body.get("error").and_then(Value::as_str) == Some("invalid_grant")
        {
            return Err(ExternalFailure::Rejected);
        }
        if status != 200 {
            return Err(ExternalFailure::Unavailable);
        }
        Ok(body)
    }

    fn live_claude(&self) -> Vec<LiveClaude> {
        let (rows, _unreadable) = crate::census::process_table();
        rows.into_iter()
            .filter(|row| looks_like_claude(&row.command))
            .map(|row| LiveClaude {
                // None is deliberately conservative: the process environment is
                // absent or unreadable, so it may own the slot.
                config_dir: crate::spawn_context::ancestor_env_marker(row.pid, "CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from),
            })
            .collect()
    }

    fn login(&self, config_dir: Option<&Path>, email: Option<&str>) -> Result<(), ExternalFailure> {
        let mut command = Command::new("claude");
        command.args(["auth", "login", "--claudeai"]);
        if let Some(email) = email {
            command.args(["--email", email]);
        }
        match config_dir {
            Some(dir) => {
                command.env("CLAUDE_CONFIG_DIR", dir);
            }
            None => {
                command.env_remove("CLAUDE_CONFIG_DIR");
            }
        }
        command
            .status()
            .map_err(|_| ExternalFailure::Unavailable)?
            .success()
            .then_some(())
            .ok_or(ExternalFailure::Unavailable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Receipt {
    action: String,
    verdict: String,
    record: Option<String>,
}

impl Receipt {
    fn new(action: &str, verdict: impl Into<String>, record: Option<String>) -> Self {
        Self {
            action: action.to_string(),
            verdict: verdict.into(),
            record,
        }
    }

    fn json(&self) -> Value {
        json!({
            "action": self.action,
            "verdict": self.verdict,
            "record": self.record,
        })
    }
}

#[derive(Debug)]
struct Options {
    store: PathBuf,
    slot_dir: PathBuf,
    config_dir: PathBuf,
    id: Option<String>,
    json: bool,
    lock_held: bool,
}

pub fn run(args: &[String]) -> i32 {
    let action = match args.first().map(String::as_str) {
        Some("sync") => "sync",
        Some("refresh") => "refresh",
        Some("login") => "login",
        _ => {
            eprintln!("usage: provider-cap vault sync|refresh|login --json [options]");
            return 2;
        }
    };
    let options = match parse_options(action, &args[1..]) {
        Ok(options) => options,
        Err(reason) => {
            eprintln!("provider-cap vault {action}: {reason}");
            return 2;
        }
    };
    let json_output = options.json;
    let (code, receipt) = run_with_options(action, options, &SystemExternal);
    if json_output {
        println!("{}", receipt.json());
    } else {
        println!("provider-cap vault {action}: {}", receipt.verdict);
    }
    code
}

fn parse_options(action: &str, args: &[String]) -> Result<Options, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let default_slot = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let mut options = Options {
        store: home.join(".fno/providers"),
        slot_dir: default_slot.clone(),
        config_dir: default_slot,
        id: None,
        json: false,
        lock_held: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--store" | "--slot-dir" | "--config-dir" | "--id" => {
                let flag = args[i].clone();
                let value = args
                    .get(i + 1)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                match flag.as_str() {
                    "--store" => options.store = PathBuf::from(value),
                    "--slot-dir" => options.slot_dir = PathBuf::from(value),
                    "--config-dir" => options.config_dir = PathBuf::from(value),
                    "--id" => options.id = Some(value.clone()),
                    _ => unreachable!(),
                }
                i += 2;
            }
            "--json" | "-J" => {
                options.json = true;
                i += 1;
            }
            "--lock-held" => {
                options.lock_held = true;
                i += 1;
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    if matches!(action, "refresh" | "login") && options.id.is_none() {
        return Err(format!("--id is required for {action}"));
    }
    Ok(options)
}

fn run_with_options(action: &str, options: Options, external: &dyn External) -> (i32, Receipt) {
    if let Err(reason) = fs::create_dir_all(&options.store) {
        return (
            1,
            Receipt::new(action, format!("store-unavailable: {reason}"), None),
        );
    }
    if options.lock_held {
        return execute(action, &options, external);
    }
    let lock_path = options.store.join(".switch.lock");
    let lock = match OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
    {
        Ok(lock) => lock,
        Err(reason) => {
            return (
                1,
                Receipt::new(action, format!("lock-unavailable: {reason}"), None),
            )
        }
    };
    if lock.lock().is_err() {
        return (1, Receipt::new(action, "lock-unavailable", None));
    }
    execute(action, &options, external)
}

fn execute(action: &str, options: &Options, external: &dyn External) -> (i32, Receipt) {
    match action {
        "sync" => sync(options, external),
        "refresh" => refresh(options, external),
        "login" => login(options, external),
        _ => (2, Receipt::new(action, "unsupported", None)),
    }
}

fn sync(options: &Options, external: &dyn External) -> (i32, Receipt) {
    let action = "sync";
    if !is_ascii_path(&options.slot_dir) {
        return (4, Receipt::new(action, "non-ascii-slot-dir", None));
    }
    let blobs = match slot_blobs(&options.slot_dir, external) {
        Ok(blobs) => blobs,
        Err(verdict) => return (4, Receipt::new(action, verdict, None)),
    };
    if blobs.is_empty() {
        return (4, Receipt::new(action, "empty-slot", None));
    }
    // The two required logins leave one unscoped and one scoped item whose
    // tokens differ but prove the SAME account (each item refreshes on its
    // own). All principals equal -> the latest expiry is the live token; a
    // principal mismatch or any profile failure keeps today's refusal.
    let (blob, proven): (&String, Option<Principal>) = if blobs.len() == 1 {
        (&blobs[0], None)
    } else {
        let mut principals: Vec<Principal> = Vec::with_capacity(blobs.len());
        for item in &blobs {
            let proof = oauth(item)
                .and_then(|oauth| oauth.access_token)
                .ok_or(())
                .and_then(|token| external.profile(&token).map_err(|_| ()));
            match proof {
                Ok(principal) => principals.push(principal),
                Err(_) => return (4, Receipt::new(action, "ambiguous-slot", None)),
            }
        }
        if principals.iter().any(|p| p != &principals[0]) {
            return (4, Receipt::new(action, "ambiguous-slot", None));
        }
        let best = newest_blob(&blobs);
        (&blobs[best], Some(principals.swap_remove(best)))
    };
    let principal = match proven {
        Some(principal) => principal,
        None => {
            let bearer = match oauth(blob).and_then(|oauth| oauth.access_token) {
                Some(token) => token,
                None => return (4, Receipt::new(action, "credential-rejected", None)),
            };
            match external.profile(&bearer) {
                Ok(principal) => principal,
                Err(ExternalFailure::Rejected) => {
                    return (4, Receipt::new(action, "credential-rejected", None))
                }
                Err(ExternalFailure::Malformed) => {
                    return (4, Receipt::new(action, "malformed-profile", None))
                }
                Err(ExternalFailure::Unavailable) => {
                    return (4, Receipt::new(action, "profile-unavailable", None))
                }
            }
        }
    };
    let matches = matching_records(&options.store, &principal);
    if matches.is_empty() {
        return (4, Receipt::new(action, "unproven", None));
    }
    if matches.len() > 1 {
        return (4, Receipt::new(action, "ambiguous-record", None));
    }
    let record = &matches[0];
    let stored_path = options.store.join(record).join("blob");
    let stored = match fs::read_to_string(&stored_path) {
        Ok(stored) => stored,
        Err(_) => {
            return (
                4,
                Receipt::new(action, "record-missing", Some(record.clone())),
            )
        }
    };
    let slot_oauth = match oauth(blob) {
        Some(oauth) => oauth,
        None => {
            return (
                4,
                Receipt::new(action, "malformed-credential", Some(record.clone())),
            )
        }
    };
    if slot_oauth.refresh_token.is_none() {
        return (
            4,
            Receipt::new(action, "empty-refresh-token", Some(record.clone())),
        );
    }
    let stored_expiry = oauth(&stored).and_then(|o| o.expires_at);
    if (stored_expiry.is_some() && slot_oauth.expires_at.is_none())
        || slot_oauth
            .expires_at
            .zip(stored_expiry)
            .is_some_and(|(slot_expiry, stored_expiry)| slot_expiry < stored_expiry)
    {
        return (4, Receipt::new(action, "stale-slot", Some(record.clone())));
    }
    if stored == *blob {
        return (0, Receipt::new(action, "unchanged", Some(record.clone())));
    }
    if atomic_write(&stored_path, blob).is_err() {
        return (
            1,
            Receipt::new(action, "store-write-failed", Some(record.clone())),
        );
    }
    (0, Receipt::new(action, "written", Some(record.clone())))
}

fn refresh(options: &Options, external: &dyn External) -> (i32, Receipt) {
    let action = "refresh";
    let id = match options.id.as_deref() {
        Some(id) => id,
        None => return (2, Receipt::new(action, "missing-id", None)),
    };
    if !is_ascii_path(&options.config_dir) {
        return (
            4,
            Receipt::new(action, "non-ascii-config-dir", Some(id.to_string())),
        );
    }
    let meta = match read_json(&options.store.join(id).join("meta.json")) {
        Some(meta) => meta,
        None => return (4, Receipt::new(action, "unproven", Some(id.to_string()))),
    };
    let principal = match principal_from_meta(&meta) {
        Some(principal) => principal,
        None => return (4, Receipt::new(action, "unproven", Some(id.to_string()))),
    };
    if live_owner(&options.slot_dir, &options.config_dir, &principal, external) {
        return (4, Receipt::new(action, "live-owner", Some(id.to_string())));
    }
    refresh_stored(options, id, external)
}

/// The token-rotation half of `refresh`, after the live-session gate: read the
/// stored blob, rotate it when it is near expiry, write it back. The health
/// check calls this directly because its question is "does the stored login
/// still work", not "does a live session sit on this config dir" - the
/// session-suppressing verdict would silence the early alert exactly while
/// the fleet works.
fn refresh_stored(options: &Options, id: &str, external: &dyn External) -> (i32, Receipt) {
    let action = "refresh";
    let path = options.store.join(id).join("blob");
    let blob = match fs::read_to_string(&path) {
        Ok(blob) => blob,
        Err(_) => {
            return (
                4,
                Receipt::new(action, "record-missing", Some(id.to_string())),
            )
        }
    };
    let stored_oauth = match oauth(&blob) {
        Some(oauth) if oauth.refresh_token.is_some() => oauth,
        _ => return (4, Receipt::new(action, "unproven", Some(id.to_string()))),
    };
    if stored_oauth
        .expires_at
        .is_some_and(|expires_at| expires_at > now_ms() + FRESH_WINDOW_MS)
    {
        return (0, Receipt::new(action, "fresh", Some(id.to_string())));
    }
    let token = stored_oauth.refresh_token.expect("checked above");
    let response = match external.refresh(&token) {
        Ok(response) => response,
        Err(ExternalFailure::Rejected) => {
            return (3, Receipt::new(action, "dead", Some(id.to_string())))
        }
        Err(_) => return (1, Receipt::new(action, "unavailable", Some(id.to_string()))),
    };
    let updated = match merge_refresh(&blob, &response) {
        Some(updated) => updated,
        None => return (1, Receipt::new(action, "unavailable", Some(id.to_string()))),
    };
    if atomic_write(&path, &updated).is_err() {
        return (
            1,
            Receipt::new(action, "store-write-failed", Some(id.to_string())),
        );
    }
    (0, Receipt::new(action, "refreshed", Some(id.to_string())))
}

fn live_owner(
    slot_dir: &Path,
    config_dir: &Path,
    record_principal: &Principal,
    external: &dyn External,
) -> bool {
    if let Ok(blobs) = slot_blobs(slot_dir, external) {
        if blobs.len() == 1 {
            if let Some(access_token) = oauth(&blobs[0]).and_then(|o| o.access_token) {
                if external
                    .profile(&access_token)
                    .is_ok_and(|principal| principal == *record_principal)
                {
                    return true;
                }
            }
        }
    }
    let wanted = normalized_path(config_dir);
    external.live_claude().into_iter().any(|process| {
        process
            .config_dir
            .map(|path| normalized_path(&path) == wanted)
            .unwrap_or(true)
    })
}

/// `vault login --id <id>`: put any Claude account on the shared slot with two
/// browser logins. Runs only when `fno config accounts use <id>` hit a dead or
/// missing stored credential: sync the outgoing account, sign in for the
/// unscoped then the scoped reader, prove each item before the next login, and
/// write the store so the next `use` is a plain switch.
fn login(options: &Options, external: &dyn External) -> (i32, Receipt) {
    let action = "login";
    let Some(id) = options.id.as_deref() else {
        return (2, Receipt::new(action, "missing-id", None));
    };
    let meta = read_json(&options.store.join(id).join("meta.json"));
    let expected = meta.as_ref().and_then(principal_from_meta);
    let expected_email = expected
        .as_ref()
        .and_then(|principal| principal.email.clone());
    let scoped = match scoped_service(&options.slot_dir) {
        Ok(service) => service,
        Err(verdict) => return (4, Receipt::new(action, verdict, Some(id.to_string()))),
    };
    // 1. Save the outgoing account first: the logins below overwrite both slot
    // items, and a credential fno has not saved would be lost.
    let (code, receipt) = sync(options, external);
    if !matches!(
        receipt.verdict.as_str(),
        "written" | "unchanged" | "empty-slot" | "unproven"
    ) {
        return (code, Receipt::new(action, receipt.verdict, receipt.record));
    }
    let readers: [(&str, Option<&Path>); 2] = [
        ("Claude Code-credentials", None),
        (scoped.as_str(), Some(options.slot_dir.as_path())),
    ];
    let mut finished: Vec<&str> = Vec::new();
    let mut proven: Vec<(String, Principal)> = Vec::new();
    for (step, (service, config_dir)) in readers.into_iter().enumerate() {
        let label = if step == 0 { "unscoped" } else { "scoped" };
        let email = expected_email.as_deref();
        match email {
            Some(email) => eprintln!("Sign in as {email} (step {} of 2)", step + 1),
            None => eprintln!("Sign in as the {id} account (step {} of 2)", step + 1),
        }
        if external.login(config_dir, email).is_err() {
            let finished = if finished.is_empty() {
                "none".to_string()
            } else {
                finished.join(", ")
            };
            return (
                1,
                Receipt::new(
                    action,
                    format!("login-aborted (finished: {finished})"),
                    Some(id.to_string()),
                ),
            );
        }
        let blob = match external.keychain(service) {
            Ok(Some(blob)) if has_credential(&blob) => blob,
            _ => {
                return (
                    1,
                    Receipt::new(
                        action,
                        format!("login-aborted ({label} unread)"),
                        Some(id.to_string()),
                    ),
                )
            }
        };
        let bearer = match oauth(&blob).and_then(|item| item.access_token) {
            Some(token) => token,
            None => {
                return (
                    1,
                    Receipt::new(
                        action,
                        format!("login-aborted ({label} unread)"),
                        Some(id.to_string()),
                    ),
                )
            }
        };
        let principal = match external.profile(&bearer) {
            Ok(principal) => principal,
            Err(_) => {
                return (
                    1,
                    Receipt::new(
                        action,
                        format!("login-aborted ({label} unproven)"),
                        Some(id.to_string()),
                    ),
                )
            }
        };
        if let Some(expected) = &expected {
            if principal != *expected {
                let who = principal
                    .email
                    .clone()
                    .unwrap_or_else(|| principal.account_uuid.clone());
                return (
                    4,
                    Receipt::new(
                        action,
                        format!("wrong-account ({who})"),
                        Some(id.to_string()),
                    ),
                );
            }
        } else {
            let others: Vec<String> = matching_records(&options.store, &principal)
                .into_iter()
                .filter(|record| record != id)
                .collect();
            if let Some(other) = others.first() {
                return (
                    4,
                    Receipt::new(action, format!("claimed-by-{other}"), Some(id.to_string())),
                );
            }
        }
        finished.push(label);
        proven.push((blob, principal));
    }
    // The blob with the later expiry is the live token; write it into the store
    // so the next `use` is a plain switch.
    let proven_blobs: Vec<&String> = proven.iter().map(|(blob, _)| blob).collect();
    let best = newest_blob(&proven_blobs);
    let (blob, principal) = proven.swap_remove(best);
    let dir = options.store.join(id);
    if fs::create_dir_all(&dir).is_err() || crate::paths::set_dir_mode_0700(&dir).is_err() {
        return (
            1,
            Receipt::new(action, "store-unavailable", Some(id.to_string())),
        );
    }
    if atomic_write(&dir.join("blob"), &blob).is_err() {
        return (
            1,
            Receipt::new(action, "store-write-failed", Some(id.to_string())),
        );
    }
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut meta = json!({
        "harness": "claude",
        "account_id": id,
        "captured_at": now,
        "kind": "keychain",
    });
    let mut principal_json = json!({
        "account_uuid": principal.account_uuid,
        "organization_uuid": principal.organization_uuid,
    });
    if let Some(email) = &principal.email {
        principal_json["email"] = json!(email);
    }
    meta["principal"] = principal_json;
    meta["principal_at"] = json!(now);
    if atomic_write(
        &dir.join("meta.json"),
        &serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .is_err()
    {
        return (
            1,
            Receipt::new(action, "store-write-failed", Some(id.to_string())),
        );
    }
    if atomic_write(&options.store.join(".active-claude"), id).is_err() {
        return (
            1,
            Receipt::new(action, "stamp-write-failed", Some(id.to_string())),
        );
    }
    match principal.email.as_deref().or(expected_email.as_deref()) {
        Some(email) => eprintln!(
            "Both keychain items hold {id} ({email}). Check usage: fno config accounts usage --refresh"
        ),
        None => eprintln!(
            "Both keychain items hold {id}. Check usage: fno config accounts usage --refresh"
        ),
    }
    (0, Receipt::new(action, "logged-in", Some(id.to_string())))
}

/// The stored-login verdict for `<id>`, for the daemon's early alert. `None`
/// means "cannot judge now": the switch lock is held, or the slot items are
/// unreadable, so a refresh here could spend the slot's live token.
pub(crate) fn stored_health(
    store: &Path,
    slot_dir: &Path,
    id: &str,
    external: &dyn External,
) -> Option<String> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(store.join(".switch.lock"))
        .ok()?;
    if lock.try_lock().is_err() {
        return None;
    }
    // Held to the end of this function on purpose: the refresh below must not
    // race a switch.
    let blobs = match slot_blobs(slot_dir, external) {
        Ok(blobs) => blobs,
        Err(_) => return None,
    };
    let stored_refresh = fs::read_to_string(store.join(id).join("blob"))
        .ok()
        .and_then(|blob| oauth(&blob).and_then(|item| item.refresh_token));
    // Refreshing the record whose stored token IS the slot's live token would
    // spend that token; skip before any network call.
    if stored_refresh.is_some_and(|token| {
        blobs.iter().any(|blob| {
            oauth(blob).and_then(|item| item.refresh_token).as_deref() == Some(token.as_str())
        })
    }) {
        return Some("slot-owner".to_string());
    }
    let options = Options {
        store: store.to_path_buf(),
        slot_dir: slot_dir.to_path_buf(),
        config_dir: slot_dir.to_path_buf(),
        id: Some(id.to_string()),
        json: false,
        lock_held: true,
    };
    Some(refresh_stored(&options, id, external).1.verdict)
}

/// The store + slot paths the daemon's health check reads, resolved the way
/// Python's `store_root()` does: `$FNO_STATE_DIR` > config `state_dir` > `~/.fno`.
pub fn stored_health_for(config_cwd: &Path, id: &str) -> Option<String> {
    let mut state = std::env::var_os("FNO_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if state.is_none() {
        state = crate::agents_config::config_lookup(config_cwd, &["state_dir"])
            .and_then(|value| value.as_str().map(str::to_owned))
            .filter(|value| !value.is_empty())
            .map(|raw| {
                let expanded = raw.strip_prefix("~/").map(|rest| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_default()
                        .join(rest)
                });
                expanded.unwrap_or_else(|| PathBuf::from(raw))
            });
    }
    if state.is_none() {
        state = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".fno"));
    }
    let state = state.unwrap_or_else(|| PathBuf::from(".fno"));
    let slot_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".claude"))
                .unwrap_or_else(|| PathBuf::from(".claude"))
        });
    stored_health(&state.join("providers"), &slot_dir, id, &SystemExternal)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OAuth {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
}
fn oauth(blob: &str) -> Option<OAuth> {
    let value: Value = serde_json::from_str(blob).ok()?;
    let oauth = value.get("claudeAiOauth")?.as_object()?;
    Some(OAuth {
        access_token: nonempty(oauth.get("accessToken")),
        refresh_token: nonempty(oauth.get("refreshToken")),
        expires_at: integer(oauth.get("expiresAt")),
    })
}

fn has_credential(blob: &str) -> bool {
    oauth(blob).is_some_and(|oauth| oauth.access_token.is_some() || oauth.refresh_token.is_some())
}

fn nonempty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
}

fn integer(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64).or_else(|| {
        value
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok())
    })
}

fn principal_from_profile(value: &Value) -> Option<Principal> {
    let account = value.get("account")?;
    let account_uuid = account.get("uuid")?.as_str()?.to_string();
    let organization_uuid = value
        .get("organization")?
        .get("uuid")?
        .as_str()?
        .to_string();
    if account_uuid.is_empty() || organization_uuid.is_empty() {
        return None;
    }
    Some(Principal {
        account_uuid,
        organization_uuid,
        email: account
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn principal_from_meta(value: &Value) -> Option<Principal> {
    let principal = value.get("principal")?;
    let account_uuid = principal.get("account_uuid")?.as_str()?.to_string();
    let organization_uuid = principal.get("organization_uuid")?.as_str()?.to_string();
    if account_uuid.is_empty() || organization_uuid.is_empty() {
        return None;
    }
    Some(Principal {
        account_uuid,
        organization_uuid,
        email: principal
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn matching_records(store: &Path, wanted: &Principal) -> Vec<String> {
    let entries = match fs::read_dir(store) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Some(meta) = read_json(&path.join("meta.json")) else {
            continue;
        };
        if meta.get("harness").and_then(Value::as_str) != Some("claude") {
            continue;
        }
        if principal_from_meta(&meta).is_some_and(|principal| principal == *wanted) {
            matches.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    matches.sort_unstable();
    matches
}

fn slot_blobs(slot_dir: &Path, external: &dyn External) -> Result<Vec<String>, String> {
    let scoped = scoped_service(slot_dir)?;
    let services = [scoped, "Claude Code-credentials".to_string()];
    let mut blobs = Vec::new();
    for service in services {
        let blob = external
            .keychain(&service)
            .map_err(|_| "keychain-unavailable".to_string())?;
        if let Some(blob) = blob.filter(|blob| has_credential(blob)) {
            if !blobs.contains(&blob) {
                blobs.push(blob);
            }
        }
    }
    let file = slot_dir.join(".credentials.json");
    if let Ok(blob) = fs::read_to_string(file) {
        if has_credential(&blob) && !blobs.contains(&blob) {
            blobs.push(blob);
        }
    }
    Ok(blobs)
}

fn scoped_service(slot_dir: &Path) -> Result<String, String> {
    if !is_ascii_path(slot_dir) {
        return Err("non-ascii-slot-dir".to_string());
    }
    let mut digest = Sha256::new();
    digest.update(slot_dir.to_string_lossy().as_bytes());
    let hex = format!("{:x}", digest.finalize());
    Ok(format!("Claude Code-credentials-{}", &hex[..8]))
}

/// Index of the blob whose expiresAt is the latest; a missing expiry loses to
/// one that has one.
fn newest_blob(blobs: &[impl AsRef<str>]) -> usize {
    let mut best = 0usize;
    let mut best_expiry = oauth(blobs[0].as_ref()).and_then(|item| item.expires_at);
    for (index, blob) in blobs.iter().enumerate().skip(1) {
        let expiry = oauth(blob.as_ref()).and_then(|item| item.expires_at);
        if expiry > best_expiry {
            best = index;
            best_expiry = expiry;
        }
    }
    best
}

fn merge_refresh(blob: &str, response: &Value) -> Option<String> {
    let mut root: Value = serde_json::from_str(blob).ok()?;
    let oauth = root.get_mut("claudeAiOauth")?.as_object_mut()?;
    let access = response.get("access_token").and_then(Value::as_str)?;
    let expires_in = response.get("expires_in").and_then(Value::as_i64)?;
    if access.is_empty() || expires_in <= 0 {
        return None;
    }
    oauth.insert("accessToken".to_string(), Value::String(access.to_string()));
    if let Some(refresh) = nonempty(response.get("refresh_token")) {
        oauth.insert("refreshToken".to_string(), Value::String(refresh));
    }
    if let Some(scope) = nonempty(response.get("scope")) {
        oauth.insert(
            "scopes".to_string(),
            Value::Array(
                scope
                    .split_whitespace()
                    .map(|item| Value::String(item.to_string()))
                    .collect(),
            ),
        );
    }
    oauth.insert(
        "expiresAt".to_string(),
        Value::Number((now_ms() + expires_in.saturating_mul(1000)).into()),
    );
    serde_json::to_string(&root).ok()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("blob"),
        std::process::id(),
        now_ms()
    ));
    let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
    set_private(&file)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    let file = OpenOptions::new().write(true).open(path)?;
    set_private(&file)?;
    Ok(())
}

fn set_private(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn is_ascii_path(path: &Path) -> bool {
    path.to_str().is_some_and(str::is_ascii)
}

fn normalized_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn looks_like_claude(command: &str) -> bool {
    command
        .split_whitespace()
        .take(4)
        .any(|part| Path::new(part).file_name().and_then(|name| name.to_str()) == Some("claude"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn curl_json(config: &str) -> Result<(u16, Value), ExternalFailure> {
    let mut child = Command::new("curl")
        .args(["-sS", "--max-time", "10", "-K", "-", "-w", "\n%{http_code}"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ExternalFailure::Unavailable)?;
    child
        .stdin
        .take()
        .ok_or(ExternalFailure::Unavailable)?
        .write_all(config.as_bytes())
        .map_err(|_| ExternalFailure::Unavailable)?;
    let output = child
        .wait_with_output()
        .map_err(|_| ExternalFailure::Unavailable)?;
    if !output.status.success() {
        return Err(ExternalFailure::Unavailable);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status) = text.rsplit_once('\n').ok_or(ExternalFailure::Malformed)?;
    let status = status
        .trim()
        .parse::<u16>()
        .map_err(|_| ExternalFailure::Malformed)?;
    let body = serde_json::from_str(body).map_err(|_| ExternalFailure::Malformed)?;
    Ok((status, body))
}

fn curl_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn form_escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[derive(Default, Clone)]
    struct MockExternal {
        keychain: HashMap<String, Option<String>>,
        // Blobs the mocked logins leave behind, served before the static map.
        post_login: Arc<Mutex<HashMap<String, String>>>,
        login_result: Arc<Mutex<Option<Result<(), ExternalFailure>>>>,
        logins: Arc<Mutex<Vec<(Option<PathBuf>, Option<String>)>>>,
        profiles: HashMap<String, Result<Principal, ExternalFailure>>,
        refresh: Arc<Mutex<Option<Result<Value, ExternalFailure>>>>,
        live: Vec<LiveClaude>,
    }

    impl External for MockExternal {
        fn keychain(&self, service: &str) -> Result<Option<String>, ExternalFailure> {
            if let Some(blob) = self.post_login.lock().unwrap().get(service).cloned() {
                return Ok(Some(blob));
            }
            Ok(self.keychain.get(service).cloned().flatten())
        }

        fn profile(&self, bearer: &str) -> Result<Principal, ExternalFailure> {
            self.profiles
                .get(bearer)
                .cloned()
                .unwrap_or(Err(ExternalFailure::Unavailable))
        }

        fn refresh(&self, _refresh_token: &str) -> Result<Value, ExternalFailure> {
            self.refresh
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Err(ExternalFailure::Unavailable))
        }

        fn live_claude(&self) -> Vec<LiveClaude> {
            self.live.clone()
        }

        fn login(
            &self,
            config_dir: Option<&Path>,
            email: Option<&str>,
        ) -> Result<(), ExternalFailure> {
            self.logins
                .lock()
                .unwrap()
                .push((config_dir.map(PathBuf::from), email.map(str::to_string)));
            self.login_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Err(ExternalFailure::Unavailable))
        }
    }

    fn principal(account: &str, organization: &str) -> Principal {
        Principal {
            account_uuid: account.to_string(),
            organization_uuid: organization.to_string(),
            email: None,
        }
    }

    fn blob(access: &str, refresh: &str, expires_at: i64) -> String {
        json!({
            "unknown": "preserved",
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at
            }
        })
        .to_string()
    }

    fn record(store: &Path, id: &str, who: &Principal, credential: &str) {
        let dir = store.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("meta.json"),
            json!({
                "harness": "claude",
                "principal": {
                    "account_uuid": who.account_uuid,
                    "organization_uuid": who.organization_uuid
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(dir.join("blob"), credential).unwrap();
    }

    fn options(store: &Path, slot_dir: &Path, id: Option<&str>) -> Options {
        Options {
            store: store.to_path_buf(),
            slot_dir: slot_dir.to_path_buf(),
            config_dir: slot_dir.to_path_buf(),
            id: id.map(str::to_string),
            json: true,
            lock_held: true,
        }
    }

    #[test]
    fn refresh_updates_an_expired_record_with_a_rotated_token() {
        let temp = TempDir::new().unwrap();
        let who = principal("acct-ready", "org-ready");
        record(
            temp.path(),
            "readyrule",
            &who,
            &blob("old", "old-refresh", now_ms() - 1),
        );
        let external = MockExternal::default();
        *external.refresh.lock().unwrap() = Some(Ok(json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
            "scope": "user:inference"
        })));

        let (code, receipt) = execute(
            "refresh",
            &options(temp.path(), &temp.path().join("slot"), Some("readyrule")),
            &external,
        );
        assert_eq!(code, 0);
        assert_eq!(receipt.verdict, "refreshed");
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(temp.path().join("readyrule/blob")).unwrap())
                .unwrap();
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "new-access");
        assert_eq!(saved["claudeAiOauth"]["refreshToken"], "new-refresh");
        assert!(saved["claudeAiOauth"]["expiresAt"].as_i64().unwrap() > now_ms());
        assert_eq!(saved["unknown"], "preserved");
        assert_eq!(
            fs::metadata(temp.path().join("readyrule/blob"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_grant_leaves_the_record_byte_identical() {
        let temp = TempDir::new().unwrap();
        let who = principal("acct-makers", "org-makers");
        let original = blob("old", "spent", now_ms() - 1);
        record(temp.path(), "makers", &who, &original);
        let external = MockExternal::default();
        *external.refresh.lock().unwrap() = Some(Err(ExternalFailure::Rejected));

        let (code, receipt) = execute(
            "refresh",
            &options(temp.path(), &temp.path().join("slot"), Some("makers")),
            &external,
        );
        assert_eq!(code, 3);
        assert_eq!(receipt.verdict, "dead");
        assert_eq!(
            fs::read_to_string(temp.path().join("makers/blob")).unwrap(),
            original
        );
    }

    #[test]
    fn sync_matches_the_proven_slot_account_not_the_active_stamp() {
        let temp = TempDir::new().unwrap();
        let slot = temp.path().join("slot");
        let account_a = principal("acct-a", "org-a");
        let account_b = principal("acct-b", "org-b");
        let old_a = blob("a", "a-refresh", now_ms());
        record(temp.path(), "account-a", &account_a, &old_a);
        record(
            temp.path(),
            "account-b",
            &account_b,
            &blob("old-b", "b-refresh", now_ms() - 1),
        );
        fs::write(temp.path().join(".active-claude"), "account-a").unwrap();
        let live_blob = blob("b", "b-new-refresh", now_ms());
        let mut external = MockExternal::default();
        external
            .keychain
            .insert(scoped_service(&slot).unwrap(), Some(live_blob.clone()));
        external
            .profiles
            .insert("b".to_string(), Ok(account_b.clone()));

        let (code, receipt) = execute("sync", &options(temp.path(), &slot, None), &external);
        assert_eq!(code, 0);
        assert_eq!(receipt.verdict, "written");
        assert_eq!(
            fs::read_to_string(temp.path().join("account-b/blob")).unwrap(),
            live_blob
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("account-a/blob")).unwrap(),
            old_a
        );
    }

    #[test]
    fn sync_accepts_two_blobs_of_one_account_and_takes_the_newest() {
        let temp = TempDir::new().unwrap();
        let slot = temp.path().join("slot");
        let who = principal("acct-same", "org-same");
        let older = blob("a-access", "a-refresh", now_ms() + 1_000);
        let newer = blob("b-access", "b-refresh", now_ms() + 2_000);
        record(temp.path(), "same", &who, &older);
        let mut external = MockExternal::default();
        external
            .keychain
            .insert(scoped_service(&slot).unwrap(), Some(older.clone()));
        external
            .keychain
            .insert("Claude Code-credentials".to_string(), Some(newer.clone()));
        external
            .profiles
            .insert("a-access".to_string(), Ok(who.clone()));
        external
            .profiles
            .insert("b-access".to_string(), Ok(who.clone()));

        let (code, receipt) = execute("sync", &options(temp.path(), &slot, None), &external);
        assert_eq!(code, 0);
        assert_eq!(receipt.verdict, "written");
        assert_eq!(
            fs::read_to_string(temp.path().join("same/blob")).unwrap(),
            newer
        );
    }

    #[test]
    fn sync_still_refuses_two_blobs_of_different_accounts() {
        let temp = TempDir::new().unwrap();
        let slot = temp.path().join("slot");
        let mut external = MockExternal::default();
        external.keychain.insert(
            scoped_service(&slot).unwrap(),
            Some(blob("b", "b-refresh", now_ms())),
        );
        external.keychain.insert(
            "Claude Code-credentials".to_string(),
            Some(blob("a", "a-refresh", now_ms())),
        );
        external
            .profiles
            .insert("b".to_string(), Ok(principal("acct-b", "org-b")));
        external
            .profiles
            .insert("a".to_string(), Ok(principal("acct-a", "org-a")));

        let (code, receipt) = execute("sync", &options(temp.path(), &slot, None), &external);
        assert_eq!(code, 4);
        assert_eq!(receipt.verdict, "ambiguous-slot");
    }

    #[test]
    fn refresh_refuses_when_the_record_owns_the_live_slot() {
        let temp = TempDir::new().unwrap();
        let slot = temp.path().join("slot");
        let who = principal("acct-live", "org-live");
        record(
            temp.path(),
            "live",
            &who,
            &blob("stored", "refresh", now_ms() - 1),
        );
        let mut external = MockExternal::default();
        external.keychain.insert(
            scoped_service(&slot).unwrap(),
            Some(blob("live-access", "refresh", now_ms())),
        );
        external.profiles.insert("live-access".to_string(), Ok(who));

        let (code, receipt) = execute(
            "refresh",
            &options(temp.path(), &slot, Some("live")),
            &external,
        );
        assert_eq!(code, 4);
        assert_eq!(receipt.verdict, "live-owner");

        // A live process pinning the config dir refuses too.
        {
            let temp = TempDir::new().unwrap();
            let slot = temp.path().join("slot");
            let who = principal("acct-process", "org-process");
            record(
                temp.path(),
                "process",
                &who,
                &blob("stored", "refresh", now_ms() - 1),
            );
            let mut external = MockExternal::default();
            external.live.push(LiveClaude {
                config_dir: Some(slot.clone()),
            });

            let (code, receipt) = execute(
                "refresh",
                &options(temp.path(), &slot, Some("process")),
                &external,
            );
            assert_eq!(code, 4);
            assert_eq!(receipt.verdict, "live-owner");
        }
    }

    #[test]
    fn login_scenarios_cover_the_happy_wrong_account_and_abort_paths() {
        // Happy path: two proven logins write the store and the stamp.
        {
            let temp = TempDir::new().unwrap();
            let slot = temp.path().join("slot");
            let who = Principal {
                account_uuid: "acct-makers".to_string(),
                organization_uuid: "org-makers".to_string(),
                email: Some("jn@makersof.xyz".to_string()),
            };
            record(
                temp.path(),
                "makers",
                &who,
                &blob("dead", "dead-refresh", now_ms() - 1),
            );
            fs::write(
                temp.path().join("makers/meta.json"),
                json!({
                    "harness": "claude",
                    "account_id": "makers",
                    "kind": "keychain",
                    "principal": {
                        "account_uuid": who.account_uuid,
                        "organization_uuid": who.organization_uuid,
                        "email": "jn@makersof.xyz"
                    }
                })
                .to_string(),
            )
            .unwrap();
            let unscoped = blob("m1", "m1-refresh", now_ms() + 1_000);
            let scoped = blob("m2", "m2-refresh", now_ms() + 2_000);
            let mut external = MockExternal::default();
            let mut post = external.post_login.lock().unwrap();
            post.insert("Claude Code-credentials".to_string(), unscoped);
            post.insert(scoped_service(&slot).unwrap(), scoped.clone());
            drop(post);
            external.profiles.insert("m1".to_string(), Ok(who.clone()));
            external.profiles.insert("m2".to_string(), Ok(who.clone()));
            *external.login_result.lock().unwrap() = Some(Ok(()));

            let (code, receipt) = execute(
                "login",
                &options(temp.path(), &slot, Some("makers")),
                &external,
            );
            assert_eq!(code, 0);
            assert_eq!(receipt.verdict, "logged-in");
            assert_eq!(receipt.record.as_deref(), Some("makers"));
            let logins = external.logins.lock().unwrap().clone();
            assert_eq!(logins.len(), 2);
            assert_eq!(logins[0], (None, Some("jn@makersof.xyz".to_string())));
            assert_eq!(
                logins[1],
                (Some(slot.clone()), Some("jn@makersof.xyz".to_string()))
            );
            assert_eq!(
                fs::read_to_string(temp.path().join("makers/blob")).unwrap(),
                scoped
            );
            let meta: Value = serde_json::from_str(
                &fs::read_to_string(temp.path().join("makers/meta.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(meta["account_id"], "makers");
            assert_eq!(meta["kind"], "keychain");
            assert_eq!(meta["principal"]["email"], "jn@makersof.xyz");
            assert_eq!(
                fs::read_to_string(temp.path().join(".active-claude")).unwrap(),
                "makers"
            );
        }
        // Wrong account: the second login never runs, the store is unchanged.
        {
            let temp = TempDir::new().unwrap();
            let slot = temp.path().join("slot");
            let who = principal("acct-makers", "org-makers");
            let original = blob("dead", "dead-refresh", now_ms() - 1);
            record(temp.path(), "makers", &who, &original);
            let wrong = Principal {
                account_uuid: "acct-jason".to_string(),
                organization_uuid: "org-jason".to_string(),
                email: Some("jason@readyrule.com".to_string()),
            };
            let mut external = MockExternal::default();
            external.post_login.lock().unwrap().insert(
                "Claude Code-credentials".to_string(),
                blob("w1", "w1-refresh", now_ms()),
            );
            external.profiles.insert("w1".to_string(), Ok(wrong));
            *external.login_result.lock().unwrap() = Some(Ok(()));

            let (code, receipt) = execute(
                "login",
                &options(temp.path(), &slot, Some("makers")),
                &external,
            );
            assert_eq!(code, 4);
            assert!(receipt.verdict.starts_with("wrong-account"));
            assert!(receipt.verdict.contains("jason@readyrule.com"));
            assert_eq!(external.logins.lock().unwrap().len(), 1);
            assert_eq!(
                fs::read_to_string(temp.path().join("makers/blob")).unwrap(),
                original
            );
            assert!(!temp.path().join(".active-claude").exists());
        }
        // Abort: a failed first login touches nothing.
        {
            let temp = TempDir::new().unwrap();
            let slot = temp.path().join("slot");
            let who = principal("acct-makers", "org-makers");
            let original = blob("dead", "dead-refresh", now_ms() - 1);
            record(temp.path(), "makers", &who, &original);
            let external = MockExternal::default();
            *external.login_result.lock().unwrap() = Some(Err(ExternalFailure::Unavailable));

            let (code, receipt) = execute(
                "login",
                &options(temp.path(), &slot, Some("makers")),
                &external,
            );
            assert_eq!(code, 1);
            assert!(receipt.verdict.starts_with("login-aborted"));
            assert!(receipt.verdict.contains("finished: none"));
            assert_eq!(external.logins.lock().unwrap().len(), 1);
            assert_eq!(
                fs::read_to_string(temp.path().join("makers/blob")).unwrap(),
                original
            );
            assert!(!temp.path().join(".active-claude").exists());
        }
    }
    #[test]
    fn stored_health_scenarios_skip_the_slot_owner_and_judge_the_rest() {
        // The slot owner's own record: never refreshed, never judged.
        {
            let temp = TempDir::new().unwrap();
            let slot = temp.path().join("slot");
            let who = principal("acct-live", "org-live");
            record(
                temp.path(),
                "makers",
                &who,
                &blob("slot-access", "shared-refresh", now_ms()),
            );
            let mut external = MockExternal::default();
            external.keychain.insert(
                scoped_service(&slot).unwrap(),
                Some(blob("slot-access", "shared-refresh", now_ms())),
            );
            *external.refresh.lock().unwrap() = Some(Err(ExternalFailure::Unavailable));

            let verdict = stored_health(temp.path(), &slot, "makers", &external);
            assert_eq!(verdict, Some("slot-owner".to_string()));
        }
        // A live session on the slot must not read as the standby being
        // healthy: the alert would never fire during work.
        {
            let temp = TempDir::new().unwrap();
            let slot = temp.path().join("slot");
            let who = principal("acct-makers", "org-makers");
            record(
                temp.path(),
                "makers",
                &who,
                &blob("stored", "spent-refresh", now_ms() - 1),
            );
            let mut external = MockExternal::default();
            external.live.push(LiveClaude {
                config_dir: Some(slot.clone()),
            });
            *external.refresh.lock().unwrap() = Some(Err(ExternalFailure::Rejected));

            let verdict = stored_health(temp.path(), &slot, "makers", &external);
            assert_eq!(verdict, Some("dead".to_string()));
        }
        // A rejected refresh is a dead saved login.
        {
            let temp = TempDir::new().unwrap();
            let slot = temp.path().join("slot");
            let who = principal("acct-makers", "org-makers");
            record(
                temp.path(),
                "makers",
                &who,
                &blob("stored", "spent-refresh", now_ms() - 1),
            );
            let external = MockExternal::default();
            *external.refresh.lock().unwrap() = Some(Err(ExternalFailure::Rejected));

            let verdict = stored_health(temp.path(), &slot, "makers", &external);
            assert_eq!(verdict, Some("dead".to_string()));
        }
    }
}
