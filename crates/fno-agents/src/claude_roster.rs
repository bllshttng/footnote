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

use std::path::{Path, PathBuf};

use serde::Deserialize;

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
}
