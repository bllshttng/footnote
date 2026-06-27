//! Hold a `claude --bg` session live via a programmatic `control.sock` attach.
//!
//! G1 held-attach substrate (epic x-07c1, node x-26df). This module speaks
//! Claude's daemon `control.sock` (an `op`-dispatched server) to ATTACH to an
//! adopted worker -- the attach is what (per the load-bearing bet) keeps the
//! session from auto-suspending. The roster read + path resolution is in
//! [`crate::claude_roster`]; the adopt orchestration (mint + claim + hold) is in
//! [`crate::claude_adopt`].
//!
//! Wire contracts pinned to claude-code **2.1.195** (readiness brief
//! `2026-06-27-phase0-held-attach-readiness.md`):
//!   - framing: newline-delimited JSON over `control.sock` (candidate A). `[corroborated]`
//!   - attach request zod: `{proto:1, op:'attach', short:/^[a-f0-9]{8}$/,
//!     auth?:string, cols:int, rows:int, caps:{terminal,mux,ssh,...}}`. `[corroborated]`
//!   - attach-OK reply: `{ok:true, op:'attach', decModes, via, tempo, state}`. `[corroborated]`
//!   - a non-TTY attach is ACCEPTED (`ok:true`) and held open. `[corroborated]`
//!
//! The live socket sits behind [`ControlTransport`] so the handshake + frame
//! classification are unit-tested with a fake. The contested bits (ping/pong
//! frame shape) are isolated and marked `TODO(spike)`; correcting them after the
//! Phase-0 spike is a localized edit.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// `proto` field value the 2.1.195 daemon expects (`gp=1`); a mismatch yields an
/// `EPROTO` "restart claude". `[corroborated]`
pub const ATTACH_PROTO: u32 = 1;

/// An `op:'attach'` request. `auth` is optional: a same-uid socket attach may
/// omit it ("legacy client, allowed via peerUid"); when the daemon `control.key`
/// is readable we present it.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachRequest {
    /// 8-hex worker short id (the roster map key).
    pub short: String,
    /// Daemon control key, or `None` for the same-uid no-auth path.
    pub auth: Option<String>,
    pub cols: u32,
    pub rows: u32,
}

impl AttachRequest {
    /// Build the request for a non-renderable holder: a small fixed window (we
    /// never render in G1 -- that is G2's drive half), minimal `caps`.
    pub fn for_hold(short: impl Into<String>, auth: Option<String>) -> Self {
        AttachRequest {
            short: short.into(),
            auth,
            cols: 80,
            rows: 24,
        }
    }

    /// Serialize to the newline-terminated JSON line the daemon reads. `caps` is a
    /// REQUIRED object; we send the minimal valid shape (`terminal`/`mux` are
    /// nullable-required, `ssh` required) -- the finding's `colorLevel`/`browser`
    /// keys are NOT in the schema and are omitted. `[corroborated]`
    pub fn to_json_line(&self) -> String {
        let mut obj = serde_json::Map::new();
        obj.insert("proto".into(), ATTACH_PROTO.into());
        obj.insert("op".into(), "attach".into());
        obj.insert("short".into(), self.short.clone().into());
        if let Some(a) = &self.auth {
            obj.insert("auth".into(), a.clone().into());
        }
        obj.insert("cols".into(), self.cols.into());
        obj.insert("rows".into(), self.rows.into());
        obj.insert(
            "caps".into(),
            serde_json::json!({"terminal": null, "mux": null, "ssh": false}),
        );
        let mut line = serde_json::Value::Object(obj).to_string();
        line.push('\n');
        line
    }
}

/// The daemon's attach-OK reply (`ok:true`). Response-only fields per the lane-c
/// correction: `decModes`/`tempo`/`state`/`via` are NOT request fields.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AttachOk {
    #[serde(default)]
    pub dec_modes: Vec<String>,
    #[serde(default)]
    pub via: Option<String>,
    /// `"active" | "blocked"` -- the session's input tempo at attach time.
    #[serde(default)]
    pub tempo: Option<String>,
    /// `"running"` for a live session.
    #[serde(default)]
    pub state: Option<String>,
}

/// Why an attach did not complete. `Refused` carries the daemon's own code where
/// it sent one (`EPROTO`, `ERESPAWNING`, ...); `Malformed` is a reply we could not
/// parse (or an I/O error around the handshake).
#[derive(Debug, Clone, PartialEq)]
pub enum AttachError {
    Refused {
        code: Option<String>,
        detail: String,
    },
    Malformed(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::Refused { code, detail } => match code {
                Some(c) => write!(f, "attach refused ({c}): {detail}"),
                None => write!(f, "attach refused: {detail}"),
            },
            AttachError::Malformed(m) => write!(f, "malformed attach reply: {m}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// Parse one reply line into an `AttachOk` or an `AttachError`. `ok:true` ->
/// `AttachOk`; `ok:false` (or absent) -> `Refused`, mining `code`/`error`/`reason`
/// for the daemon's reason; non-JSON -> `Malformed`.
pub fn parse_attach_reply(line: &str) -> Result<AttachOk, AttachError> {
    let v: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => return Err(AttachError::Malformed(format!("{e}: {line:?}"))),
    };
    let ok = v
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if ok {
        // Field rename: decModes -> dec_modes via an explicit pull (the reply is
        // camelCase; we keep AttachOk snake-cased without forcing a rename attr on
        // a hand-parsed value).
        let dec_modes = v
            .get("decModes")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let str_field = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        Ok(AttachOk {
            dec_modes,
            via: str_field("via"),
            tempo: str_field("tempo"),
            state: str_field("state"),
        })
    } else {
        let code = v
            .get("code")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let detail = v
            .get("error")
            .or_else(|| v.get("reason"))
            .or_else(|| v.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("attach not accepted")
            .to_string();
        Err(AttachError::Refused { code, detail })
    }
}

/// A frame arriving from the daemon while a hold is open.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A heartbeat the holder must answer (else dropped after ~3 missed). The
    /// exact frame shape is `[strings-only, spike to confirm]`.
    Ping,
    /// A PTY render frame (drained, ignored in G1 -- rendering is G2).
    PtyFrame(String),
    /// Anything else (control replies, status), kept for forensics.
    Other(String),
}

/// Classify a drained line. `TODO(spike)`: confirm the ping frame shape on
/// 2.1.195; the most-evidenced guess is an `op:"ping"` control frame.
pub fn classify_incoming(line: &str) -> Incoming {
    let trimmed = line.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        match v.get("op").and_then(serde_json::Value::as_str) {
            Some("ping") => return Incoming::Ping,
            Some(_) => return Incoming::Other(trimmed.to_string()),
            None => {}
        }
    }
    // Non-JSON (or no `op`) is a raw PTY frame.
    Incoming::PtyFrame(line.to_string())
}

/// The pong a holder writes back for each [`Incoming::Ping`]. `TODO(spike)`:
/// confirm the expected ack shape on 2.1.195.
pub fn pong_line() -> String {
    "{\"op\":\"pong\"}\n".to_string()
}

/// The newline-delimited JSON transport over a daemon socket. Behind a trait so
/// the handshake/hold logic is exercised with a fake in unit tests and the real
/// `UnixStream` path is the only thing that needs a live daemon.
pub trait ControlTransport {
    /// Write one already-newline-terminated line.
    fn send_line(&mut self, line: &str) -> io::Result<()>;
    /// Read the next line (without the trailing newline). `Ok(None)` == EOF (the
    /// daemon closed the socket -- a dropped hold silently re-enables suspend, so
    /// the supervisor must reattach).
    fn recv_line(&mut self) -> io::Result<Option<String>>;
}

/// Live `control.sock` transport: a `UnixStream` with a buffered line reader.
pub struct UnixControlTransport {
    write: UnixStream,
    read: BufReader<UnixStream>,
}

impl UnixControlTransport {
    /// Connect to the daemon `control.sock` at `path`. A non-draining reader gets
    /// `c.destroy()`'d (lane-a backpressure), so the holder must keep calling
    /// `recv_line`.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        // A read timeout lets the holder loop wake to answer pings / notice a
        // dropped hold rather than blocking forever on a quiet socket.
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let read = BufReader::new(stream.try_clone()?);
        Ok(UnixControlTransport {
            write: stream,
            read,
        })
    }
}

impl ControlTransport for UnixControlTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.write.write_all(line.as_bytes())?;
        self.write.flush()
    }

    fn recv_line(&mut self) -> io::Result<Option<String>> {
        let mut buf = String::new();
        let n = self.read.read_line(&mut buf)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        Ok(Some(buf.trim_end_matches(['\n', '\r']).to_string()))
    }
}

/// Perform the attach handshake over `t`: send the request, read + parse the
/// first reply. Used by the holder to (re)establish a hold.
pub fn perform_attach<T: ControlTransport>(
    t: &mut T,
    req: &AttachRequest,
) -> Result<AttachOk, AttachError> {
    let line = req.to_json_line();
    t.send_line(&line)
        .map_err(|e| AttachError::Malformed(format!("send: {e}")))?;
    match t.recv_line() {
        Ok(Some(reply)) => parse_attach_reply(&reply),
        Ok(None) => Err(AttachError::Refused {
            code: Some("EOF".into()),
            detail: "daemon closed before attach reply".into(),
        }),
        Err(e) => Err(AttachError::Malformed(format!("recv: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted in-memory transport: replays queued reply lines, records sends.
    struct FakeTransport {
        replies: VecDeque<Option<String>>,
        sent: Vec<String>,
        recv_err: bool,
    }
    impl FakeTransport {
        fn new(replies: Vec<Option<&str>>) -> Self {
            FakeTransport {
                replies: replies.into_iter().map(|r| r.map(str::to_string)).collect(),
                sent: Vec::new(),
                recv_err: false,
            }
        }
    }
    impl ControlTransport for FakeTransport {
        fn send_line(&mut self, line: &str) -> io::Result<()> {
            self.sent.push(line.to_string());
            Ok(())
        }
        fn recv_line(&mut self) -> io::Result<Option<String>> {
            if self.recv_err {
                return Err(io::Error::other("boom"));
            }
            Ok(self.replies.pop_front().flatten())
        }
    }

    #[test]
    fn attach_request_serializes_to_pinned_schema() {
        let req = AttachRequest::for_hold("a1b2c3d4", Some("deadbeef".into()));
        let line = req.to_json_line();
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["proto"], 1);
        assert_eq!(v["op"], "attach");
        assert_eq!(v["short"], "a1b2c3d4");
        assert_eq!(v["auth"], "deadbeef");
        assert_eq!(v["cols"], 80);
        assert_eq!(v["rows"], 24);
        // caps: terminal/mux null, ssh false, no stray keys.
        assert!(v["caps"]["terminal"].is_null());
        assert!(v["caps"]["mux"].is_null());
        assert_eq!(v["caps"]["ssh"], false);
        assert!(v["caps"].get("colorLevel").is_none());
    }

    #[test]
    fn attach_request_omits_auth_for_same_uid_path() {
        let req = AttachRequest::for_hold("a1b2c3d4", None);
        let v: serde_json::Value = serde_json::from_str(req.to_json_line().trim()).unwrap();
        assert!(v.get("auth").is_none(), "no-auth path must omit the key");
    }

    #[test]
    fn parse_ok_reply() {
        let ok = parse_attach_reply(
            r#"{"ok":true,"op":"attach","decModes":["1049","2004"],"via":"spare","tempo":"active","state":"running"}"#,
        )
        .unwrap();
        assert_eq!(ok.dec_modes, vec!["1049", "2004"]);
        assert_eq!(ok.via.as_deref(), Some("spare"));
        assert_eq!(ok.tempo.as_deref(), Some("active"));
        assert_eq!(ok.state.as_deref(), Some("running"));
    }

    #[test]
    fn parse_refused_reply_mines_code_and_reason() {
        let err = parse_attach_reply(r#"{"ok":false,"code":"EPROTO","error":"restart claude"}"#)
            .unwrap_err();
        assert_eq!(
            err,
            AttachError::Refused {
                code: Some("EPROTO".into()),
                detail: "restart claude".into()
            }
        );
    }

    #[test]
    fn parse_non_json_is_malformed() {
        assert!(matches!(
            parse_attach_reply("not a frame"),
            Err(AttachError::Malformed(_))
        ));
    }

    #[test]
    fn classify_ping_frame_and_pty_frame() {
        assert_eq!(classify_incoming(r#"{"op":"ping"}"#), Incoming::Ping);
        assert_eq!(
            classify_incoming(r#"{"op":"status","state":"running"}"#),
            Incoming::Other(r#"{"op":"status","state":"running"}"#.to_string())
        );
        assert_eq!(
            classify_incoming("\x1b[2J raw bytes"),
            Incoming::PtyFrame("\x1b[2J raw bytes".to_string())
        );
    }

    #[test]
    fn perform_attach_happy_path() {
        let mut t =
            FakeTransport::new(vec![Some(r#"{"ok":true,"op":"attach","state":"running"}"#)]);
        let req = AttachRequest::for_hold("a1b2c3d4", None);
        let ok = perform_attach(&mut t, &req).unwrap();
        assert_eq!(ok.state.as_deref(), Some("running"));
        // The request was actually written.
        assert_eq!(t.sent.len(), 1);
        assert!(t.sent[0].contains("\"op\":\"attach\""));
    }

    #[test]
    fn perform_attach_eof_is_refused() {
        let mut t = FakeTransport::new(vec![None]);
        let req = AttachRequest::for_hold("a1b2c3d4", None);
        let err = perform_attach(&mut t, &req).unwrap_err();
        assert!(matches!(err, AttachError::Refused { .. }));
    }

    #[test]
    fn perform_attach_recv_error_is_malformed() {
        let mut t = FakeTransport::new(vec![]);
        t.recv_err = true;
        let req = AttachRequest::for_hold("a1b2c3d4", None);
        let err = perform_attach(&mut t, &req).unwrap_err();
        assert!(matches!(err, AttachError::Malformed(_)));
    }
}
