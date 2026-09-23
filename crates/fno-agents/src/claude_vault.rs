//! Managed Claude credential freshness and ownership actions.
//!
//! The vault answers one question before a credential reaches the store or
//! shared slot: is it current, spendable, and proven to be the named account?

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const PROFILE_BETA: &str = "oauth-2025-04-20";
const FRESH_WINDOW_MS: i64 = 5 * 60 * 1000;
const SECURITY_ITEM_NOT_FOUND: i32 = 44;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Principal {
    account_uuid: String,
    organization_uuid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveClaude {
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExternalFailure {
    Rejected,
    Unavailable,
    Malformed,
}

trait External {
    fn keychain(&self, service: &str) -> Result<Option<String>, ExternalFailure>;
    fn profile(&self, bearer: &str) -> Result<Principal, ExternalFailure>;
    fn refresh(&self, refresh_token: &str) -> Result<Value, ExternalFailure>;
    fn live_claude(&self) -> Vec<LiveClaude>;
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
        let output = Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", &account, "-w"])
            .output()
            .map_err(|_| ExternalFailure::Unavailable)?;
        if output.status.code() == Some(SECURITY_ITEM_NOT_FOUND) {
            return Ok(None);
        }
        if !output.status.success() {
            return Err(ExternalFailure::Unavailable);
        }
        let blob = String::from_utf8_lossy(&output.stdout).trim().to_string();
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
        let default_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
            .unwrap_or_else(|| PathBuf::from(".claude"));
        rows.into_iter()
            .filter(|row| looks_like_claude(&row.command))
            .map(|row| LiveClaude {
                // None is deliberately conservative: the process environment
                // is absent or unreadable, so it may own the default slot.
                config_dir: crate::spawn_context::ancestor_env_marker(row.pid, "CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .or_else(|| Some(default_dir.clone())),
            })
            .collect()
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
        _ => {
            eprintln!("usage: provider-cap vault sync|refresh --json [options]");
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
            "--json" => {
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
    if action == "refresh" && options.id.is_none() {
        return Err("--id is required for refresh".to_string());
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
    if blobs.len() > 1 {
        return (4, Receipt::new(action, "ambiguous-slot", None));
    }
    let blob = &blobs[0];
    let bearer = match oauth(blob).and_then(|oauth| oauth.access_token) {
        Some(token) => token,
        None => return (4, Receipt::new(action, "credential-rejected", None)),
    };
    let principal = match external.profile(&bearer) {
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
            .unwrap_or(false)
    })
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
    let account_uuid = value.get("account")?.get("uuid")?.as_str()?;
    let organization_uuid = value.get("organization")?.get("uuid")?.as_str()?;
    if account_uuid.is_empty() || organization_uuid.is_empty() {
        return None;
    }
    Some(Principal {
        account_uuid: account_uuid.to_string(),
        organization_uuid: organization_uuid.to_string(),
    })
}

fn principal_from_meta(value: &Value) -> Option<Principal> {
    let principal = value.get("principal")?;
    let account_uuid = principal.get("account_uuid")?.as_str()?;
    let organization_uuid = principal.get("organization_uuid")?.as_str()?;
    if account_uuid.is_empty() || organization_uuid.is_empty() {
        return None;
    }
    Some(Principal {
        account_uuid: account_uuid.to_string(),
        organization_uuid: organization_uuid.to_string(),
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
    matches.sort();
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
        profiles: HashMap<String, Result<Principal, ExternalFailure>>,
        refresh: Arc<Mutex<Option<Result<Value, ExternalFailure>>>>,
        live: Vec<LiveClaude>,
    }

    impl External for MockExternal {
        fn keychain(&self, service: &str) -> Result<Option<String>, ExternalFailure> {
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
    }

    fn principal(account: &str, organization: &str) -> Principal {
        Principal {
            account_uuid: account.to_string(),
            organization_uuid: organization.to_string(),
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
    fn sync_refuses_two_distinct_live_slot_blobs() {
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
    }

    #[test]
    fn refresh_refuses_for_a_live_process_on_the_config_dir() {
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
