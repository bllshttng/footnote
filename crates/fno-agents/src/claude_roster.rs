//! Read Claude Code's own daemon roster and resolve its `control.sock` /
//! `control.key`.
//!
//! G1 held-attach substrate (epic x-07c1, node x-26df). footnote ADOPTS an
//! externally-spawned `claude --bg` worker by reading Claude's daemon roster
//! (`~/.claude/daemon/roster.json`), then holds that worker's session live via a
//! programmatic `control.sock` attach (see [`crate::claude_attach`]). This module
//! is the read side: a typed roster, daemon-socket path resolution, and adopt
//! selection. It never writes anything Claude owns.
//!
//! Wire contracts are pinned to claude-code **2.1.195** (readiness brief
//! `internal/fno/design/2026-06-27-phase0-held-attach-readiness.md`). The roster
//! schema below is `[confirmed]` against a live 14-worker roster; `control.sock`
//! framing/auth are `[corroborated]`. On a version bump, re-tap the wire format
//! first (`fno doctor` version-probe).
//!
//! ponytail: the one runtime-unverified property -- a held non-TTY attach
//! defeats the ~1h idle auto-suspend window -- is the Phase-0 spike's job, not a
//! code-shape concern. Nothing in this module asserts it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

const AGENTS_LIST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAgentRow {
    pub short_id: String,
    pub state: Option<String>,
    /// Full harness session id, name and cwd, when the listing carries them
    /// (x-aad0): the roster-side sweep needs the identity a registry row
    /// would have had, and the registry-side surfaces ignore them.
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    /// (x-c914 mirror) Which claude account root this row was read from:
    /// `None` = the ambient `~/.claude`, `Some(id)` = an isolated account's
    /// config dir from the fno accounts config. Set by the union reader,
    /// never by `parse_all_agents` (the parse stays dir-blind).
    pub account: Option<String>,
    pub pid: Option<u32>,
}

impl ClaudeAgentRow {
    pub fn new(short_id: &str, state: Option<&str>) -> Self {
        Self {
            short_id: short_id.to_string(),
            state: state.map(|value| value.to_ascii_lowercase()),
            session_id: None,
            name: None,
            cwd: None,
            account: None,
            pid: None,
        }
    }

    pub fn with_pid(mut self, pid: Option<u32>) -> Self {
        self.pid = pid;
        self
    }
}

/// The one terminal-state set, shared by every death-evidence reader (rm's
/// live gate, the reaper's stop confirmation). `blocked` is deliberately
/// absent: a blocked row may be rotated and resumed, so it holds.
pub fn is_terminal_roster_state(state: &str) -> bool {
    matches!(state, "done" | "stopped" | "failed")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeAgentsSnapshot {
    Known {
        rows: Vec<ClaudeAgentRow>,
        warnings: Vec<String>,
    },
    Unknown {
        rows: Vec<ClaudeAgentRow>,
        warnings: Vec<String>,
    },
}

impl ClaudeAgentsSnapshot {
    pub fn known(rows: Vec<ClaudeAgentRow>) -> Self {
        Self::Known {
            rows,
            warnings: Vec::new(),
        }
    }

    pub fn unknown(reason: &str) -> Self {
        Self::Unknown {
            rows: Vec::new(),
            warnings: vec![reason.to_string()],
        }
    }

    pub fn find(&self, short_id: &str) -> Option<&ClaudeAgentRow> {
        match self {
            Self::Known { rows, .. } | Self::Unknown { rows, .. } => {
                rows.iter().find(|row| row.short_id == short_id)
            }
        }
    }

    /// True when at least one row parsed. Known rows can carry warnings (a
    /// partial list): presence checks on them are sound, absence checks are
    /// not - gate those on [`Self::warning_text`] being empty.
    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }

    pub fn warning_text(&self) -> String {
        let warnings = match self {
            Self::Known { warnings, .. } | Self::Unknown { warnings, .. } => warnings,
        };
        warnings.join("; ")
    }
}

struct ClaudeCommandOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub fn read_all_agents() -> ClaudeAgentsSnapshot {
    read_all_agents_with(run_all_agents_command)
}

fn read_all_agents_with(
    run: impl FnOnce() -> Result<ClaudeCommandOutput, String>,
) -> ClaudeAgentsSnapshot {
    let output = match run() {
        Ok(output) => output,
        Err(reason) => return ClaudeAgentsSnapshot::unknown(&reason),
    };
    if !output.success {
        let detail = String::from_utf8_lossy(&output.stderr);
        return ClaudeAgentsSnapshot::unknown(&format!(
            "claude agents --json --all exited non-zero: {}",
            detail.trim()
        ));
    }
    parse_all_agents(&output.stdout)
}

fn parse_all_agents(stdout: &[u8]) -> ClaudeAgentsSnapshot {
    let parsed: serde_json::Value = match serde_json::from_slice(stdout) {
        Ok(parsed) => parsed,
        Err(error) => {
            return ClaudeAgentsSnapshot::unknown(&format!(
                "claude agents --json --all parse failure: {error}"
            ))
        }
    };
    let rows = match parsed {
        serde_json::Value::Array(rows) => rows,
        serde_json::Value::Object(mut object) => match object.remove("agents") {
            Some(serde_json::Value::Array(rows)) => rows,
            _ => {
                return ClaudeAgentsSnapshot::unknown(
                    "claude agents --json --all response missing agents array",
                )
            }
        },
        _ => {
            return ClaudeAgentsSnapshot::unknown(
                "claude agents --json --all response has an unexpected shape",
            )
        }
    };

    let mut parsed_rows = Vec::new();
    let mut warnings = Vec::new();
    let mut agent_rows = 0usize;
    for (index, row) in rows.into_iter().enumerate() {
        let Some(object) = row.as_object() else {
            warnings.push(format!(
                "claude agents row {index} is not an object; skipped"
            ));
            continue;
        };
        if object.get("kind").and_then(|value| value.as_str()) == Some("interactive") {
            continue;
        }
        agent_rows += 1;
        let short_id = ["short_id", "id"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(|value| value.as_str()))
            .filter(|value| !value.is_empty());
        let Some(short_id) = short_id else {
            warnings.push(format!(
                "claude agents row {index} has no usable short id; skipped"
            ));
            continue;
        };
        let state = ["state", "status"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(|value| value.as_str()));
        let pid = object
            .get("pid")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok());
        let mut row = ClaudeAgentRow::new(short_id, state).with_pid(pid);
        row.session_id = ["session_id", "sessionId"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(|value| value.as_str()))
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        row.name = ["name"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(|value| value.as_str()))
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        row.cwd = ["cwd"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(|value| value.as_str()))
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        parsed_rows.push(row);
    }
    if !warnings.is_empty() {
        if parsed_rows.is_empty() {
            if agent_rows > 0 {
                warnings.push(format!(
                    "0 of {agent_rows} Claude agent rows parsed; agent list is unverified"
                ));
            }
            return ClaudeAgentsSnapshot::Unknown {
                rows: parsed_rows,
                warnings,
            };
        }
        // Partial parse stays Known: the rows that did parse are real, and
        // presence/terminal-state checks on them are sound. Absence is not -
        // the skipped rows could hide the row - so absence-proofs gate on
        // warning_text() being empty, not on this verdict alone.
        return ClaudeAgentsSnapshot::Known {
            rows: parsed_rows,
            warnings,
        };
    }
    ClaudeAgentsSnapshot::Known {
        rows: parsed_rows,
        warnings,
    }
}

fn run_all_agents_command() -> Result<ClaudeCommandOutput, String> {
    run_all_agents_command_in(None)
}

/// Run `claude agents --json --all` against ONE account root. `None` is the
/// ambient root (whatever `CLAUDE_CONFIG_DIR` the process already carries);
/// `Some(dir)` pins the dir, which is how an isolated account's rows become
/// visible to a reader that would otherwise never see them.
/// The exact argv the roster snapshot shells out with, built here so the
/// regression test can assert on it without spawning. A duplicated
/// `.args(...)` chain here once issued `claude agents --json --all agents
/// --json --all`, which exits 1 and read every snapshot as Unknown - the
/// regression test exists because this line is exactly the kind a second
/// chain slips back into.
fn all_agents_command(config_dir: Option<&std::path::Path>) -> std::process::Command {
    let mut command = std::process::Command::new("claude");
    if let Some(dir) = config_dir {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    command.args(["agents", "--json", "--all"]);
    command
}

fn run_all_agents_command_in(
    config_dir: Option<&std::path::Path>,
) -> Result<ClaudeCommandOutput, String> {
    let mut command = all_agents_command(config_dir);
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("claude agents --json --all failed to start: {error}"))?;
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });
    let deadline = std::time::Instant::now() + AGENTS_LIST_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "claude agents --json --all timed out after {}s",
                    AGENTS_LIST_TIMEOUT.as_secs()
                ));
            }
            Err(error) => return Err(format!("claude agents --json --all wait failed: {error}")),
        }
    };
    let stdout = stdout
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    let stderr = stderr
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    Ok(ClaudeCommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// The isolated claude account roots, `(account_id, config_dir)`, mirrored
/// from the accounts config the same way the mux's `agents_view` reads them
/// (the crates share no types; the FILE is the contract). Managed accounts
/// carry no `config_dir` and contribute nothing, so an all-managed config
/// degrades to the single ambient read. Source precedence: project-local
/// `.fno/config.toml`, then the `$FNO_GLOBAL_SETTINGS_PATH` sibling, then
/// `~/.fno/config.toml`. Fail-open to empty: an unreadable config means no
/// known isolated roots, and the union degrades to the ambient read.
pub fn isolated_account_dirs() -> Vec<(String, std::path::PathBuf)> {
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        sources.push(cwd.join(".fno").join("config.toml"));
    }
    if let Ok(global) = std::env::var("FNO_GLOBAL_SETTINGS_PATH") {
        if let Some(parent) = std::path::Path::new(&global).parent() {
            sources.push(parent.join("config.toml"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        sources.push(
            std::path::PathBuf::from(home)
                .join(".fno")
                .join("config.toml"),
        );
    }
    for path in sources {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed = parse_isolated_config_dirs(&body, std::env::var_os("HOME").as_deref());
        if !parsed.is_empty() {
            return parsed;
        }
    }
    Vec::new()
}

/// The config dir a removal must address for this row, `None` meaning the
/// ambient root. A union row's measured account outranks the historical
/// launch account; the latter is used only when the snapshot has no row.
pub fn removal_config_dir(
    snapshot: &ClaudeAgentsSnapshot,
    short_id: &str,
    launch_account: Option<&str>,
) -> Result<Option<std::path::PathBuf>, String> {
    let account = match snapshot.find(short_id) {
        Some(row) => match &row.account {
            Some(account) => account.clone(),
            None => return Ok(None),
        },
        None => match launch_account {
            Some(account) => account.to_string(),
            None => return Ok(None),
        },
    };
    isolated_account_dirs()
        .into_iter()
        .find(|(id, _)| id == &account)
        .map(|(_, dir)| Some(dir))
        .ok_or_else(|| format!("claude account root '{account}' is not configured"))
}

/// Resolve the account root for a legacy short-id-only removal call.
pub fn removal_config_dir_for_short_id(
    short_id: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    if isolated_account_dirs().is_empty() {
        return Ok(None);
    }
    removal_config_dir(&read_all_agents_union(), short_id, None)
}

/// Parse `[[providers.records]]` / `[[accounts.records]]` entries carrying an
/// isolated `config_dir`, as `(account_id, dir)` with `~/` expanded. Malformed
/// records are skipped, never a panic.
pub fn parse_isolated_config_dirs(
    toml_body: &str,
    home: Option<&std::ffi::OsStr>,
) -> Vec<(String, std::path::PathBuf)> {
    let Ok(table) = toml_body.parse::<toml::Table>() else {
        return Vec::new();
    };
    let records = table
        .get("accounts")
        .or_else(|| table.get("providers"))
        .and_then(|section| section.get("records"))
        .and_then(|records| records.as_array());
    let mut out = Vec::new();
    for record in records.into_iter().flatten() {
        let (Some(id), Some(dir)) = (
            record.get("id").and_then(|v| v.as_str()),
            record.get("config_dir").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if id.is_empty() || dir.trim().is_empty() {
            continue;
        }
        let expanded = if let Some(rest) = dir.strip_prefix("~/") {
            let Some(home) = home else {
                continue;
            };
            std::path::PathBuf::from(home).join(rest)
        } else {
            std::path::PathBuf::from(dir)
        };
        out.push((id.to_string(), expanded));
    }
    out
}

/// The agent list across EVERY account root: the ambient read first, then
/// one pinned read per isolated account. Rows carry the account they were
/// read under, so a removal can be routed to the root that owns them.
/// The snapshot reads `Known` only when EVERY root's read parsed - a root
/// that failed leaves the whole union `Unknown`, because absence from a
/// partial union is a WRONG-ROOT absence and must never read as removal
/// evidence.
pub fn read_all_agents_union() -> ClaudeAgentsSnapshot {
    let mut rows: Vec<ClaudeAgentRow> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut all_known = true;
    let ambient = read_all_agents();
    match &ambient {
        ClaudeAgentsSnapshot::Known { rows: parsed, .. } => rows.extend(parsed.iter().cloned()),
        ClaudeAgentsSnapshot::Unknown {
            rows: parsed,
            warnings: w,
        } => {
            all_known = false;
            warnings.extend(w.iter().cloned());
            rows.extend(parsed.iter().cloned());
        }
    }
    for (account, dir) in isolated_account_dirs() {
        let output = run_all_agents_command_in(Some(&dir));
        let snapshot = match output {
            Ok(output) => parse_all_agents(&output.stdout),
            Err(reason) => ClaudeAgentsSnapshot::unknown(&reason),
        };
        match snapshot {
            ClaudeAgentsSnapshot::Known {
                rows: parsed,
                warnings: w,
            } => {
                for mut row in parsed {
                    row.account = Some(account.clone());
                    rows.push(row);
                }
                if !w.is_empty() {
                    warnings.extend(w);
                }
            }
            ClaudeAgentsSnapshot::Unknown {
                rows: parsed,
                warnings: w,
            } => {
                all_known = false;
                warnings.extend(w.iter().cloned());
                for mut row in parsed {
                    row.account = Some(account.clone());
                    rows.push(row);
                }
            }
        }
    }
    if all_known {
        ClaudeAgentsSnapshot::Known { rows, warnings }
    } else {
        ClaudeAgentsSnapshot::Unknown { rows, warnings }
    }
}

/// Env override that redirects the whole Claude daemon dir (tests, and operators
/// who run Claude with a non-default home). When unset, `$HOME/.claude/daemon`.
pub const DAEMON_DIR_ENV: &str = "FNO_CLAUDE_DAEMON_DIR";

/// Resolve the Claude daemon directory (`<home>/.claude/daemon`). Honors
/// [`DAEMON_DIR_ENV`] first so tests and alt-home setups redirect the whole tree.
pub fn daemon_dir() -> PathBuf {
    if let Some(v) = std::env::var_os(DAEMON_DIR_ENV) {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("daemon")
}

/// `<daemon_dir>/roster.json` -- the worker roster the supervisor maintains.
pub fn default_roster_path() -> PathBuf {
    daemon_dir().join("roster.json")
}

/// `<daemon_dir>/control.key` -- the daemon control key an `op:attach` presents
/// (32 hex, mode 600, same-uid). NOT the per-worker `ptyAuth` (that is the
/// ptySock DATA path). [corroborated]
pub fn control_key_path() -> PathBuf {
    daemon_dir().join("control.key")
}

/// Read and trim the daemon control key, if present. Returns `None` when the file
/// is absent or empty -- a same-uid socket attach may legally omit `auth` ("legacy
/// client, allowed via peerUid"), so the caller treats this as optional, never an
/// error. [corroborated]
pub fn read_control_key() -> Option<String> {
    let raw = std::fs::read_to_string(control_key_path()).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// One row of `~/.claude/daemon/roster.json`. Only the fields G1 consumes are
/// modeled; serde ignores the rest (`rendezvousSock`, `dispatch`, `decModes`,
/// `rvAuth`, `attempt`, `pendingRespawn`, ...), so a roster that grows new keys
/// still parses. [confirmed] against a live 14-worker roster.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RosterWorker {
    /// Full session UUID. The single adopt key; its first 8-hex segment is the
    /// roster MAP key and our `short_id`.
    pub session_id: String,
    /// The externally-owned `claude --bg` worker pid. Used for `pid_start_time`
    /// reuse-detection on the minted registry row -- NOT the pid the `pty:`
    /// claim is reanchored to (that is footnote's long-lived HOLDER pid).
    #[serde(default)]
    pub pid: Option<u32>,
    /// Worker process start time; mirrors `RegistryEntry::pid_start_time`. Only
    /// ever compared for equality.
    ///
    /// Parsed leniently: `procStart` drifted from an epoch `u64` (claude-code
    /// <=2.1.194) to a human date string (e.g. `"Mon Jun 29 00:11:16 2026"`) on
    /// later CLIs. `#[serde(default)]` tolerates a MISSING field but NOT a type
    /// mismatch, so a strict `Option<u64>` makes serde reject the ENTIRE roster on
    /// the first string value, silently zeroing every worker (the 0->visible flip
    /// this restores). [`de_lenient_opt_u64`] accepts null/absent -> None, a number
    /// -> `Some`, a numeric string -> `Some`, and any other string (a date) ->
    /// None - degrading only the equality signal rather than killing the parse.
    #[serde(default, deserialize_with = "de_lenient_opt_u64")]
    pub proc_start: Option<u64>,
    /// The internal supervisor<->worker ptySock. We never speak it directly (the
    /// substrate is the daemon `control.sock`), but we WALK UP from it to resolve
    /// the sibling `control.sock`.
    #[serde(default)]
    pub pty_sock: Option<String>,
    /// Per-worker data-path token. Recorded for completeness; the `control.sock`
    /// attach authenticates with the daemon `control.key`, not this.
    #[serde(default)]
    pub pty_auth: Option<String>,
    /// CLI version that minted the row. A mismatch vs the running daemon can mean
    /// a transient `ERESPAWNING` on attach (version-skew respawn). [corroborated]
    #[serde(default)]
    pub cli_version: Option<String>,
    /// The worker's cwd; carried onto the minted registry row.
    #[serde(default)]
    pub cwd: String,
    /// Linked-worktree path, when the worker runs in one (0/14 live, but modeled).
    #[serde(default)]
    pub worktree_path: Option<String>,
}

/// Lenient `Option<u64>` deserializer for the drifting `procStart` field (see
/// [`RosterWorker::proc_start`]). Accepts a JSON number, a numeric string, or
/// null/anything-else (-> None). Never errors, so one worker's date-string
/// `procStart` cannot fail the whole-roster parse. The field is only compared for
/// equality, so a None from an unparseable value is a safe degradation.
fn de_lenient_opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(n) => Ok(n.as_u64()),
        serde_json::Value::String(s) => Ok(s.trim().parse::<u64>().ok()),
        _ => Ok(None),
    }
}

impl RosterWorker {
    /// The roster map key == `sessionId.split('-')[0]` (first 8-hex segment). The
    /// `pty:<short_id>` claim holder and the minted registry `short_id` both key
    /// on this. [confirmed: 14/14]
    pub fn short_id(&self) -> &str {
        self.session_id
            .split('-')
            .next()
            .unwrap_or(&self.session_id)
    }

    /// Resolve this worker's daemon `control.sock` by walking up from its
    /// `ptySock`: `control.sock` is a sibling of the `pty/ rv/ spare/` dirs under
    /// `<daemonDir>`, so the first ancestor dir containing a `control.sock` child
    /// wins. Returns `None` when there is no `ptySock` or no `control.sock` is
    /// found up the chain. The brief's "walk up from ptySock past `spare/`"
    /// resolution; the `kMm()`-hashed daemon subdir is never hardcoded.
    pub fn resolve_control_sock(&self) -> Option<PathBuf> {
        let pty = self.pty_sock.as_deref()?;
        control_sock_from_ptysock(Path::new(pty))
    }
}

/// Walk ancestors of `pty_sock` for the first dir holding a `control.sock` child.
fn control_sock_from_ptysock(pty_sock: &Path) -> Option<PathBuf> {
    let mut cur = pty_sock.parent();
    while let Some(dir) = cur {
        let cand = dir.join("control.sock");
        if cand.exists() {
            return Some(cand);
        }
        cur = dir.parent();
    }
    None
}

/// Typed `~/.claude/daemon/roster.json`. Unknown top-level keys are ignored.
/// [confirmed]: `{proto:1, supervisorPid, updatedAt, workers:{<short>:WorkerEntry}}`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRoster {
    #[serde(default)]
    pub proto: u32,
    #[serde(default)]
    pub supervisor_pid: Option<u32>,
    #[serde(default)]
    pub updated_at: Option<u64>,
    /// `<short> -> WorkerEntry`. The key is informational (== `sessionId` prefix);
    /// we dedup on the full `session_id`, never the key.
    #[serde(default)]
    pub workers: std::collections::BTreeMap<String, RosterWorker>,
}

impl ClaudeRoster {
    /// Parse a roster from JSON bytes. A torn/garbage roster is a hard error here;
    /// callers degrade (an unreadable roster yields zero adoptable workers, never a
    /// panic) at the call site, not by swallowing the parse.
    pub fn parse(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }

    /// Read + parse the roster at `path`.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::parse(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Read + parse the default roster (`~/.claude/daemon/roster.json`). A missing
    /// roster (no Claude daemon ever ran) returns `Ok` with no workers, so the
    /// substrate degrades to "nothing to adopt" rather than erroring.
    pub fn load_default() -> std::io::Result<Self> {
        let path = default_roster_path();
        match std::fs::read(&path) {
            Ok(bytes) => Self::parse(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClaudeRoster {
                proto: 0,
                supervisor_pid: None,
                updated_at: None,
                workers: Default::default(),
            }),
            Err(e) => Err(e),
        }
    }

    /// Adoptable workers, deduped by `session_id` (a torn roster could in
    /// principle list a session twice under two keys; first occurrence wins),
    /// ordered by `session_id` for determinism.
    pub fn workers_deduped(&self) -> Vec<&RosterWorker> {
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<&RosterWorker> = self
            .workers
            .values()
            .filter(|w| seen.insert(w.session_id.as_str()))
            .collect();
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }

    /// Find a worker by full `session_id` OR by 8-hex `short_id`. The accepted
    /// resolution inputs the adopt entrypoint takes.
    pub fn find(&self, session_or_short: &str) -> Option<&RosterWorker> {
        self.workers
            .values()
            .find(|w| w.session_id == session_or_short || w.short_id() == session_or_short)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // x-9419: the roster snapshot shells out with EXACTLY
    // [agents, --json, --all]. A duplicated .args chain once issued
    // `claude agents ... agents --json --all`, which the CLI refuses with
    // exit 1, so every snapshot read Unknown, claude_row_provably_absent
    // was permanently false, and no claude-harness row was ever reapable.
    // Asserting the BUILT ARGV (not a snapshot) is the point: a snapshot
    // test passes on any machine where claude is absent.
    #[test]
    fn the_agents_snapshot_argv_is_not_duplicated() {
        let command = all_agents_command(None);
        assert_eq!(command.get_program().to_string_lossy(), "claude");
        let argv: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "agents".to_string(),
                "--json".to_string(),
                "--all".to_string()
            ],
            "the roster shellout argv must be exactly [agents, --json, --all]: {argv:?}"
        );
    }

    // A 2-worker roster in the confirmed live shape (extra keys present, to prove
    // they are ignored).
    const SAMPLE: &str = r#"{
      "proto": 1,
      "supervisorPid": 4242,
      "updatedAt": 1751049130000,
      "workers": {
        "a1b2c3d4": {
          "pid": 5001,
          "procStart": 99887766,
          "sessionId": "a1b2c3d4-1111-2222-3333-444455556666",
          "rendezvousSock": "/tmp/cc-daemon-501/deadbeef/rv/a1b2c3d4.rv.sock",
          "ptySock": "/tmp/cc-daemon-501/deadbeef/spare/a1b2c3d4.pty.sock",
          "cliVersion": "2.1.195",
          "startedAt": 1751049000000,
          "attempt": 1,
          "cwd": "/Users/x/code/proj",
          "dispatch": {"source": "shell"},
          "rvAuth": "aaaa1111bbbb2222",
          "ptyAuth": "cccc3333dddd4444"
        },
        "ee99ff00": {
          "pid": 5002,
          "sessionId": "ee99ff00-7777-8888-9999-aaaabbbbcccc",
          "ptySock": "/tmp/cc-daemon-501/deadbeef/pty/ee99ff00.pty.sock",
          "startedAt": 1751049050000,
          "attempt": 2,
          "cwd": "/Users/x/code/other",
          "dispatch": {"source": "fleet"}
        }
      }
    }"#;

    #[test]
    fn parses_confirmed_roster_shape() {
        let r = ClaudeRoster::parse(SAMPLE.as_bytes()).expect("parse");
        assert_eq!(r.proto, 1);
        assert_eq!(r.supervisor_pid, Some(4242));
        assert_eq!(r.workers.len(), 2);
        let w = &r.workers["a1b2c3d4"];
        assert_eq!(w.session_id, "a1b2c3d4-1111-2222-3333-444455556666");
        assert_eq!(w.pid, Some(5001));
        assert_eq!(w.proc_start, Some(99887766));
        assert_eq!(w.cli_version.as_deref(), Some("2.1.195"));
        assert_eq!(w.cwd, "/Users/x/code/proj");
        assert_eq!(w.pty_auth.as_deref(), Some("cccc3333dddd4444"));
    }

    // The live claude-code (>=2.1.195) roster shape: `procStart` is a human DATE
    // STRING, not the epoch `u64` the <=2.1.194 schema emitted. A strict
    // `Option<u64>` rejected the WHOLE roster here (every worker lost); the lenient
    // deserializer must parse all workers and degrade `proc_start` to None.
    const SAMPLE_STRING_PROCSTART: &str = r#"{
      "proto": 1,
      "supervisorPid": 77901,
      "workers": {
        "6269e385": {
          "pid": 6001,
          "procStart": "Mon Jun 29 00:11:16 2026",
          "sessionId": "6269e385-1111-2222-3333-444455556666",
          "ptySock": "/tmp/cc-daemon-501/608d3bdb/spare/6269e385.pty.sock",
          "cliVersion": "2.1.199",
          "cwd": "/Users/bb16/code/footnote/footnote"
        },
        "d712218d": {
          "pid": 6002,
          "procStart": "Mon Jun 29 00:11:22 2026",
          "sessionId": "d712218d-7777-8888-9999-aaaabbbbcccc",
          "ptySock": "/tmp/cc-daemon-501/608d3bdb/spare/d712218d.pty.sock",
          "cwd": "/Users/bb16/code/footnote/footnote"
        }
      }
    }"#;

    #[test]
    fn parses_roster_with_string_procstart_drift() {
        // Regression: before the lenient deserializer this errored, zeroing the
        // roster (mail-inject + every roster consumer saw zero claude workers).
        let r = ClaudeRoster::parse(SAMPLE_STRING_PROCSTART.as_bytes())
            .expect("string procStart must not fail the whole-roster parse");
        assert_eq!(r.workers.len(), 2, "both workers survive the drift");
        let w = &r.workers["6269e385"];
        assert_eq!(w.session_id, "6269e385-1111-2222-3333-444455556666");
        assert_eq!(w.pid, Some(6001));
        // An unparseable date string degrades to None, not a parse failure.
        assert_eq!(w.proc_start, None);
        assert_eq!(w.cwd, "/Users/bb16/code/footnote/footnote");
    }

    #[test]
    fn lenient_procstart_accepts_number_numeric_string_and_null() {
        // number -> Some
        let num = r#"{"workers":{"a":{"sessionId":"a-1","procStart":12345}}}"#;
        assert_eq!(
            ClaudeRoster::parse(num.as_bytes()).unwrap().workers["a"].proc_start,
            Some(12345)
        );
        // numeric string -> Some (a future CLI could quote the epoch)
        let numstr = r#"{"workers":{"a":{"sessionId":"a-1","procStart":"12345"}}}"#;
        assert_eq!(
            ClaudeRoster::parse(numstr.as_bytes()).unwrap().workers["a"].proc_start,
            Some(12345)
        );
        // explicit null -> None
        let null = r#"{"workers":{"a":{"sessionId":"a-1","procStart":null}}}"#;
        assert_eq!(
            ClaudeRoster::parse(null.as_bytes()).unwrap().workers["a"].proc_start,
            None
        );
        // absent -> None (the #[serde(default)] path)
        let absent = r#"{"workers":{"a":{"sessionId":"a-1"}}}"#;
        assert_eq!(
            ClaudeRoster::parse(absent.as_bytes()).unwrap().workers["a"].proc_start,
            None
        );
    }

    #[test]
    fn short_id_is_first_hex_segment() {
        let r = ClaudeRoster::parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(r.workers["a1b2c3d4"].short_id(), "a1b2c3d4");
        assert_eq!(r.workers["ee99ff00"].short_id(), "ee99ff00");
    }

    #[test]
    fn dedup_is_deterministic_by_session_id() {
        let r = ClaudeRoster::parse(SAMPLE.as_bytes()).unwrap();
        let w = r.workers_deduped();
        assert_eq!(w.len(), 2);
        // Sorted by session_id: a1b2... before ee99...
        assert_eq!(w[0].short_id(), "a1b2c3d4");
        assert_eq!(w[1].short_id(), "ee99ff00");
    }

    #[test]
    fn find_by_session_or_short() {
        let r = ClaudeRoster::parse(SAMPLE.as_bytes()).unwrap();
        assert!(r.find("a1b2c3d4").is_some());
        assert!(r.find("ee99ff00-7777-8888-9999-aaaabbbbcccc").is_some());
        assert!(r.find("nope").is_none());
    }

    #[test]
    fn malformed_roster_is_error() {
        assert!(ClaudeRoster::parse(b"{ not json").is_err());
    }

    #[test]
    fn missing_roster_loads_empty() {
        // Point the daemon dir at a nonexistent path; load_default degrades.
        let tmp = std::env::temp_dir().join(format!(
            "fno-roster-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var(DAEMON_DIR_ENV, &tmp);
        let r = ClaudeRoster::load_default().expect("degrades to empty");
        assert!(r.workers.is_empty());
        std::env::remove_var(DAEMON_DIR_ENV);
    }

    #[test]
    fn resolve_control_sock_walks_up_from_ptysock() {
        // Build .../d/spare/x.pty.sock with a sibling .../d/control.sock
        let base = std::env::temp_dir().join(format!(
            "fno-ctrlsock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ddir = base.join("cc-daemon-501").join("deadbeef");
        std::fs::create_dir_all(ddir.join("spare")).unwrap();
        let ctrl = ddir.join("control.sock");
        std::fs::write(&ctrl, b"").unwrap();
        let pty = ddir.join("spare").join("x.pty.sock");
        std::fs::write(&pty, b"").unwrap();

        let w = RosterWorker {
            session_id: "x".into(),
            pid: None,
            proc_start: None,
            pty_sock: Some(pty.to_string_lossy().into_owned()),
            pty_auth: None,
            cli_version: None,
            cwd: String::new(),
            worktree_path: None,
        };
        assert_eq!(w.resolve_control_sock().unwrap(), ctrl);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_control_sock_none_when_absent() {
        let base = std::env::temp_dir().join(format!(
            "fno-ctrlsock-none-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = base.join("d").join("spare");
        std::fs::create_dir_all(&dir).unwrap();
        let pty = dir.join("x.pty.sock");
        std::fs::write(&pty, b"").unwrap();
        let w = RosterWorker {
            session_id: "x".into(),
            pid: None,
            proc_start: None,
            pty_sock: Some(pty.to_string_lossy().into_owned()),
            pty_auth: None,
            cli_version: None,
            cwd: String::new(),
            worktree_path: None,
        };
        assert!(w.resolve_control_sock().is_none());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn all_agents_reader_keeps_background_rows_and_skips_interactive() {
        let stdout = br#"[
          {"kind":"background","id":"aaaa1111","state":"stopped"},
          {"kind":"interactive","name":"operator"},
          {"kind":"background","short_id":"bbbb2222","status":"working"}
        ]"#;
        let snapshot = read_all_agents_with(|| {
            Ok(ClaudeCommandOutput {
                success: true,
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
            })
        });

        let ClaudeAgentsSnapshot::Known { rows, warnings } = snapshot else {
            panic!("valid agent JSON must be known");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].short_id, "aaaa1111");
        assert_eq!(rows[0].state.as_deref(), Some("stopped"));
        assert_eq!(rows[1].short_id, "bbbb2222");
        assert_eq!(rows[1].state.as_deref(), Some("working"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn all_agents_timeout_is_unknown_not_successful_empty() {
        let snapshot = read_all_agents_with(|| Err("timed out after 15s".to_string()));
        let ClaudeAgentsSnapshot::Unknown { warnings, .. } = snapshot else {
            panic!("a timeout must not prove an empty agent list");
        };
        assert!(warnings.iter().any(|warning| warning.contains("timed out")));
    }

    #[test]
    fn all_agents_partial_parse_cannot_prove_a_row_absent() {
        let snapshot = parse_all_agents(
            br#"[
              {"kind":"background","id":"aaaa1111","state":"stopped"},
              {"kind":"background","state":"stopped"}
            ]"#,
        );
        // A partial list is Known but carries its warnings: the parsed rows
        // are real, the skipped ones could hide anything.
        let ClaudeAgentsSnapshot::Known { rows, warnings } = &snapshot else {
            panic!("rows that parsed are real; partial parse must not poison them");
        };
        assert_eq!(rows.len(), 1);
        assert!(!warnings.is_empty());
        assert!(snapshot.find("aaaa1111").is_some());
    }

    #[test]
    fn all_agents_zero_parsed_stays_unknown() {
        let snapshot = parse_all_agents(br#"[{"kind":"background"}]"#);
        assert!(matches!(snapshot, ClaudeAgentsSnapshot::Unknown { .. }));
        assert!(snapshot.warning_text().contains("0 of 1"));
    }

    #[test]
    fn all_agents_rows_carry_pid_when_emitted() {
        let snapshot = parse_all_agents(
            br#"[
              {"kind":"background","id":"aaaa1111","state":"done","pid":65340},
              {"kind":"background","id":"bbbb2222","state":"working"}
            ]"#,
        );
        let ClaudeAgentsSnapshot::Known { rows, .. } = snapshot else {
            panic!("clean list must be known");
        };
        assert_eq!(rows[0].pid, Some(65340));
        assert_eq!(rows[1].pid, None);
    }

    fn alternate_account_env_lock() -> &'static std::sync::Mutex<()> {
        crate::claims::test_env_lock()
    }

    fn with_alt_account_config(test: impl FnOnce(PathBuf)) {
        let _guard = alternate_account_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = std::env::temp_dir().join(format!(
            "fno-removal-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("config.toml"),
            format!(
                "[[accounts.records]]\nid = \"alt\"\nconfig_dir = \"{}\"\n",
                root.join("claude-alt").display()
            ),
        )
        .unwrap();
        let previous = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH");
        std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", root.join("settings.toml"));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test(root.join("claude-alt"));
        }));
        match previous {
            Some(value) => std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", value),
            None => std::env::remove_var("FNO_GLOBAL_SETTINGS_PATH"),
        }
        std::fs::remove_dir_all(root).ok();
        drop(_guard);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn removal_config_dir_uses_measured_isolated_row_root() {
        with_alt_account_config(|alt_dir| {
            let mut row = ClaudeAgentRow::new("aaaa1111", Some("done"));
            row.account = Some("alt".to_string());
            let snapshot = ClaudeAgentsSnapshot::known(vec![row]);
            assert_eq!(
                removal_config_dir(&snapshot, "aaaa1111", None),
                Ok(Some(alt_dir))
            );
        });
    }

    #[test]
    fn removal_config_dir_uses_ambient_root_for_missing_row_without_record() {
        with_alt_account_config(|_| {
            let snapshot = ClaudeAgentsSnapshot::known(Vec::new());
            assert_eq!(removal_config_dir(&snapshot, "bbbb2222", None), Ok(None));
        });
    }

    #[test]
    fn removal_config_dir_prefers_measured_ambient_root_over_launch_record() {
        with_alt_account_config(|_| {
            let snapshot =
                ClaudeAgentsSnapshot::known(vec![ClaudeAgentRow::new("cccc3333", Some("done"))]);
            assert_eq!(
                removal_config_dir(&snapshot, "cccc3333", Some("alt")),
                Ok(None)
            );
        });
    }

    #[test]
    fn removal_config_dir_refuses_an_unmapped_measured_account() {
        with_alt_account_config(|_| {
            let mut row = ClaudeAgentRow::new("dddd4444", Some("done"));
            row.account = Some("missing".to_string());
            let snapshot = ClaudeAgentsSnapshot::known(vec![row]);
            let got = format!("{:?}", removal_config_dir(&snapshot, "dddd4444", None));
            assert!(
                got.starts_with("Err("),
                "an unmapped measured account must refuse, got {got}"
            );
        });
    }

    #[test]
    fn alternate_account_tests_share_the_crate_environment_lock() {
        let shared = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let attempt = alternate_account_env_lock().try_lock();
        assert!(matches!(attempt, Err(std::sync::TryLockError::WouldBlock)));
        drop(shared);
    }

    #[test]
    fn alternate_account_tests_restore_environment_after_panic() {
        let previous = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH");
        let result = std::panic::catch_unwind(|| {
            with_alt_account_config(|_| panic!("injected assertion failure"));
        });
        let observed = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH");
        if observed != previous {
            if let Some(path) = &observed {
                if let Some(root) = std::path::Path::new(path).parent() {
                    std::fs::remove_dir_all(root).ok();
                }
            }
            match &previous {
                Some(value) => std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", value),
                None => std::env::remove_var("FNO_GLOBAL_SETTINGS_PATH"),
            }
        }
        assert!(result.is_err(), "the injected panic did not run");
        assert_eq!(observed, previous, "the helper leaked its environment");
    }
}
