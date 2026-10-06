//! The session record and its sidecar. The record is `<fno_id>.jsonl`
//! beside the `<fno_id>/` sidecar dir, Claude Code style; every file in the
//! session carries the id. `<fno_id>.index.db` holds one small row per
//! line, so a store failure never loses a record. Field lists live in
//! `schema.md` beside this file.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_V: u64 = 1;
const DIAG_CAP: u64 = 4 * 1024 * 1024;

/// Every record type a reader understands. An unknown type without
/// `ignorable` refuses the session: guessing at it could hide an effect.
pub const KNOWN_TYPES: &[&str] = &[
    "header",
    "turn_context",
    "user_input",
    "model_request",
    "model_attempt",
    "model_response",
    "usage",
    "tool_call",
    "effect_decision",
    "tool_start",
    "tool_result",
    "hook_decision",
    "compaction",
    "terminal",
];

/// `~/.fno/sessions`, beside the spaces root so test fences on that root
/// cover it too.
pub fn sessions_root() -> PathBuf {
    let spaces = crate::paths::spaces_root();
    spaces
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(spaces)
        .join("sessions")
}

/// The project key: the canonical checkout's space slug, so every worktree
/// of a repo shares one; `_none` outside a repo.
pub fn project_slug(cwd: &Path) -> String {
    crate::paths::canonical_repo_root(cwd)
        .map(|root| crate::paths::space_slug(&root))
        .unwrap_or_else(|| "_none".to_string())
}

pub fn session_dir(root: &Path, cwd: &Path, fno_id: &str) -> PathBuf {
    root.join(project_slug(cwd)).join(fno_id)
}

/// The record: `<fno_id>.jsonl` beside the sidecar dir.
pub fn transcript_file(dir: &Path, fno_id: &str) -> PathBuf {
    dir.parent().unwrap_or(dir).join(format!("{fno_id}.jsonl"))
}

/// A top-level record file: `<fno_id>.jsonl` directly under a project
/// slug. Sidecar dirs carry no extension, and child transcripts live one
/// level down, inside them.
pub fn is_record_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

/// Find a session's directory under any project slug: the sidecar dir
/// whose record `<fno_id>.jsonl` exists beside it.
pub fn find_session_dir(root: &Path, fno_id: &str) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path().join(fno_id))
        .find(|p| transcript_file(p, fno_id).is_file())
}

pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn mint() -> Result<String, String> {
    crate::identity::mint_fno_id()
}

/// Read every record, refusing an unknown non-ignorable type or a bad line.
pub fn read_records(path: &Path) -> Result<Vec<Value>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: Value = serde_json::from_str(&line)
            .map_err(|e| format!("{}:{}: not JSON: {e}", path.display(), n + 1))?;
        let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");
        let ignorable = rec.get("ignorable").and_then(Value::as_bool) == Some(true);
        if !KNOWN_TYPES.contains(&ty) && !ignorable {
            return Err(format!(
                "{}:{}: unknown record type {ty:?}",
                path.display(),
                n + 1
            ));
        }
        out.push(rec);
    }
    Ok(out)
}

pub struct Writer {
    dir: PathBuf,
    session_id: String,
    run_id: String,
    file: File,
    _lock: File,
    seq: u64,
    secrets: Vec<String>,
}

impl Writer {
    /// Create a new session directory. An existing directory refuses: an id
    /// names one session, never a reused one.
    pub fn create(dir: &Path, session_id: &str) -> Result<Writer, String> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::create_dir(dir).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("session {session_id} already exists"),
            _ => format!("{}: {e}", dir.display()),
        })?;
        let _ = crate::paths::set_dir_mode_0700(dir);
        Self::open_inner(dir, session_id, 0)
    }

    /// Open an existing session for resume. Returns the writer and its records.
    /// A last line with no newline is a write the killed writer never
    /// finished: it is cut off under the lock, never parsed.
    pub fn open(dir: &Path, session_id: &str) -> Result<(Writer, Vec<Value>), String> {
        let path = transcript_file(dir, session_id);
        let mut w = Self::open_inner(dir, session_id, 0)?;
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            w.file
                .set_len(keep as u64)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            w.diag(
                "warn",
                &format!("cut a torn last line ({} bytes)", bytes.len() - keep),
            );
        }
        let records = read_records(&path)?;
        w.seq = records.len() as u64;
        Ok((w, records))
    }

    fn open_inner(dir: &Path, session_id: &str, seq: u64) -> Result<Writer, String> {
        let lock = crate::harness_daemon::try_acquire_lock(&dir.join(format!("{session_id}.lock")))
            .ok_or_else(|| {
                format!(
                    "session {session_id} has a live writer; stop it with `fno agents stop <name>`"
                )
            })?;
        let path = transcript_file(dir, session_id);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = crate::paths::set_file_mode_0600(&path);
        Ok(Writer {
            dir: dir.to_path_buf(),
            session_id: session_id.to_string(),
            run_id: mint()?,
            file,
            _lock: lock,
            seq,
            secrets: Vec::new(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn transcript_path(&self) -> PathBuf {
        transcript_file(&self.dir, &self.session_id)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Values `diag` must never print (the resolved API key).
    pub fn add_secret(&mut self, secret: &str) {
        if !secret.is_empty() {
            self.secrets.push(secret.to_string());
        }
    }

    /// Append one record and fsync it. A short write truncates back so the
    /// file never ends mid-line. Returns the full record.
    pub fn append(&mut self, ty: &str, data: Value, ignorable: bool) -> Result<Value, String> {
        let mut rec = json!({
            "v": SCHEMA_V,
            "seq": self.seq,
            "id": mint()?,
            "ts": now_ts(),
            "session_id": self.session_id,
            "type": ty,
            "data": data,
        });
        if ignorable {
            rec["ignorable"] = json!(true);
        }
        let mut line = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
        line.push('\n');
        let offset = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|e| e.to_string())?;
        if let Err(e) = self
            .file
            .write_all(line.as_bytes())
            .and_then(|_| self.file.sync_data())
        {
            let _ = self.file.set_len(offset);
            return Err(format!("transcript append failed: {e}"));
        }
        self.seq += 1;
        self.index(&rec, offset, line.len() as u64);
        Ok(rec)
    }

    fn index(&self, rec: &Value, offset: u64, len: u64) {
        let data = &rec["data"];
        let row = json!({
            "ts": rec["ts"],
            "type": "footnote_index_record",
            "source": format!("footnote:{}", self.session_id),
            "data": {
                "session_id": self.session_id,
                "record_type": rec["type"].as_str().unwrap_or(""),
                "seq": rec["seq"],
                "record_id": rec["id"],
                "turn": data.get("turn").cloned().unwrap_or(Value::Null),
                "tool_call_uid": data.get("tool_call_uid").cloned().unwrap_or(Value::Null),
                "offset": offset,
                "len": len,
            },
        });
        let id = rec["id"].as_str().unwrap_or("");
        if let Err(e) = crate::event_store::append_envelope(
            &self.dir.join(format!("{}.index.jsonl", self.session_id)),
            &row.to_string(),
            Some(id),
        ) {
            self.diag("warn", &format!("index row failed: {e}"));
        }
    }

    /// Write a full output to `<fno_id>/<name>.out`; returns (path, size, sha256).
    pub fn spill(&self, name: &str, bytes: &[u8]) -> Result<(PathBuf, u64, String), String> {
        let path = self.dir.join(format!("{name}.out"));
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok((path, bytes.len() as u64, sha256_hex(bytes)))
    }

    /// One line in `diag.log`, secrets redacted, capped with one rotation.
    pub fn diag(&self, level: &str, text: &str) {
        let mut text = text.replace('\n', " ");
        for s in &self.secrets {
            text = text.replace(s.as_str(), "[redacted]");
        }
        let path = self.dir.join(format!("{}.diag.log", self.session_id));
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > DIAG_CAP {
            let _ = std::fs::rename(
                &path,
                self.dir.join(format!("{}.diag.log.1", self.session_id)),
            );
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(
                f,
                "{} run={} session={} {level} {text}",
                now_ts(),
                self.run_id,
                self.session_id
            );
        }
    }
}
