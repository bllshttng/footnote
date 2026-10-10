//! The session record and its sidecar. The record is `<fno_id>.jsonl`
//! beside the `<fno_id>/` sidecar dir, Claude Code style; every file in the
//! session carries the id. `<fno_id>.index.db` holds one small row per
//! line, so a store failure never loses a record. Field lists live in
//! `schema.md` beside this file; the reader is fno's `footnote_transcript`.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub use fno::footnote_transcript::{read_records, transcript_file, SCHEMA_V};

const DIAG_CAP: u64 = 4 * 1024 * 1024;

pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A random v4 UUID for record, run and tool-call ids. The session id itself
/// is minted by the supervisor and arrives in the launch spec.
pub fn mint() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| format!("could not mint an id: {e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
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
        let _ = set_mode(dir, 0o700);
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
        let lock = try_lock(&dir.join(format!("{session_id}.lock"))).ok_or_else(|| {
            format!("session {session_id} has a live writer; stop it with `fno agents stop <name>`")
        })?;
        let path = transcript_file(dir, session_id);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = set_mode(&path, 0o600);
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
        if let Err(e) = fno::event_store::append_envelope(
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

/// Owner-only permissions, regardless of umask.
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = (path, mode);
    Ok(())
}

/// The session's writer lock, non-blocking: `None` while another process
/// holds it.
fn try_lock(path: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}
