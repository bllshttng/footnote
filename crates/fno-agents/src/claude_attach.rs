//! Speak Claude's daemon `control.sock`: the `op:'attach'` handshake + the
//! newline-delimited JSON transport.
//!
//! G1 substrate (epic x-07c1, node x-26df). The Phase-0 spike retired the
//! held-attach *keepalive* premise (an idle `claude --bg` session and an un-held
//! control both survived 65min idle, so the attach was not what kept it live).
//! So footnote does NOT hold a session for liveness. `op:'attach'` survives for a
//! different reason: G2/grid attaches to pull the PTY frame STREAM for rendering a
//! tile. The drive primitive (`op:'reply'` + transcript confirm) lives in
//! [`crate::claude_drive`]; the roster read is [`crate::claude_roster`].
//!
//! Wire contracts pinned to claude-code **2.1.195** (readiness brief
//! `2026-06-27-phase0-held-attach-readiness.md`):
//!   - framing: newline-delimited JSON over `control.sock` (candidate A). `[corroborated]`
//!   - attach request zod: `{proto:1, op:'attach', short:/^[a-f0-9]{8}$/,
//!     auth?:string, cols:int, rows:int, caps:{terminal,mux,ssh,...}}`. `[corroborated]`
//!   - attach-OK reply: `{ok:true, op:'attach', decModes, via, tempo, state}`. `[corroborated]`
//!   - auth = the daemon `control.key` (32-hex), NOT the per-worker `ptyAuth`.
//!
//! The live socket sits behind [`ControlTransport`] so the handshake is
//! unit-tested with a fake; the live `UnixStream` path is one thin type.

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
    /// 8-hex worker short id (the roster map key) -- a wire value, used only here
    /// at the `control.sock` boundary, never as a footnote-side identity.
    pub short: String,
    /// Daemon control key, or `None` for the same-uid no-auth path.
    pub auth: Option<String>,
    pub cols: u32,
    pub rows: u32,
}

impl AttachRequest {
    /// Construct an attach with an explicit window size.
    pub fn new(short: impl Into<String>, auth: Option<String>, cols: u32, rows: u32) -> Self {
        AttachRequest {
            short: short.into(),
            auth,
            cols,
            rows,
        }
    }

    /// Attach to pull the PTY frame stream (G2 tile render). A modest default
    /// window; the daemon accepts a non-TTY attacher and streams frames.
    pub fn for_frame_stream(short: impl Into<String>, auth: Option<String>) -> Self {
        Self::new(short, auth, 80, 24)
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
        // The reply is camelCase; pull decModes -> dec_modes explicitly so AttachOk
        // stays snake-cased without a rename attr on a hand-parsed value.
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

/// The newline-delimited JSON transport over a daemon socket. Behind a trait so
/// the handshake + the drive inject ([`crate::claude_drive`]) are exercised with a
/// fake in unit tests and the real `UnixStream` path is the only thing that needs
/// a live daemon.
pub trait ControlTransport {
    /// Write one already-newline-terminated line.
    fn send_line(&mut self, line: &str) -> io::Result<()>;
    /// Read the next line (without the trailing newline). `Ok(None)` == EOF.
    fn recv_line(&mut self) -> io::Result<Option<String>>;
}

/// Live `control.sock` transport: a `UnixStream` with a buffered line reader.
pub struct UnixControlTransport {
    write: UnixStream,
    read: BufReader<UnixStream>,
}

impl UnixControlTransport {
    /// Connect to the daemon `control.sock` at `path`. A non-draining reader gets
    /// `c.destroy()`'d (lane-a backpressure), so a frame-stream consumer must keep
    /// calling `recv_line`.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
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
        // Trim trailing newline in place rather than allocating a fresh String.
        let len = buf.trim_end_matches(['\n', '\r']).len();
        buf.truncate(len);
        Ok(Some(buf))
    }
}

/// Perform the attach handshake over `t`: send the request, read + parse the
/// first reply. The precursor a frame-stream consumer (G2) or the drive primitive
/// runs before it streams frames / injects a turn.
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

/// A raw PTY byte stream from a `control.sock` `op:'attach'`. After the single
/// newline-terminated attach-OK JSON line, the daemon streams the terminal's raw
/// VT/ANSI bytes -- pinned live on 2.1.195: NOT JSON-wrapped, NOT length-prefixed
/// (the daemon unwraps the internal ptySock framing before streaming to
/// attachers). So G2 renders a tile by feeding these bytes straight into its
/// terminal-emulator pane. `\r`/`\n` are ordinary terminal content here, so the
/// stream is read as raw bytes, never line-split.
#[derive(Debug)]
pub struct FrameStream<R: io::Read> {
    reader: BufReader<R>,
}

impl<R: io::Read> io::Read for FrameStream<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Attach for the frame stream: send the `op:'attach'` request on `writer`, read
/// + parse the one handshake line from `reader`, then hand back a [`FrameStream`]
/// positioned at the first raw PTY byte. Bytes the handshake read buffered past
/// the reply's `\n` are preserved -- they are the first frame bytes, so they must
/// not be dropped. Reader and writer are split so a live caller passes a
/// `try_clone`'d `UnixStream` writer plus the stream as the reader, while tests
/// pass a `Vec`/`Cursor`.
pub fn attach_for_frames<R: io::Read, W: Write>(
    mut writer: W,
    reader: R,
    req: &AttachRequest,
) -> Result<(AttachOk, FrameStream<R>), AttachError> {
    writer
        .write_all(req.to_json_line().as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|e| AttachError::Malformed(format!("send: {e}")))?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Err(AttachError::Refused {
            code: Some("EOF".into()),
            detail: "daemon closed before attach reply".into(),
        }),
        // A reply without the protocol newline is a truncated handshake (the
        // daemon closed mid-write): `read_line` returns `Ok(n>0)` and the JSON
        // may even parse, but the stream that follows is empty. Treat it as a
        // refusal rather than handing back a FrameStream that instantly EOFs.
        Ok(_) if !line.ends_with('\n') => Err(AttachError::Refused {
            code: Some("EOF".into()),
            detail: "daemon closed before complete attach reply".into(),
        }),
        Ok(_) => {
            let ok = parse_attach_reply(&line)?;
            Ok((ok, FrameStream { reader }))
        }
        Err(e) => Err(AttachError::Malformed(format!("recv: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Cursor, Read};

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
        let req = AttachRequest::for_frame_stream("a1b2c3d4", Some("deadbeef".into()));
        let line = req.to_json_line();
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["proto"], 1);
        assert_eq!(v["op"], "attach");
        assert_eq!(v["short"], "a1b2c3d4");
        assert_eq!(v["auth"], "deadbeef");
        assert_eq!(v["cols"], 80);
        assert_eq!(v["rows"], 24);
        assert!(v["caps"]["terminal"].is_null());
        assert!(v["caps"]["mux"].is_null());
        assert_eq!(v["caps"]["ssh"], false);
        assert!(v["caps"].get("colorLevel").is_none());
    }

    #[test]
    fn attach_request_omits_auth_for_same_uid_path() {
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
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
    fn perform_attach_happy_path() {
        let mut t =
            FakeTransport::new(vec![Some(r#"{"ok":true,"op":"attach","state":"running"}"#)]);
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let ok = perform_attach(&mut t, &req).unwrap();
        assert_eq!(ok.state.as_deref(), Some("running"));
        assert_eq!(t.sent.len(), 1);
        assert!(t.sent[0].contains("\"op\":\"attach\""));
    }

    #[test]
    fn perform_attach_eof_is_refused() {
        let mut t = FakeTransport::new(vec![None]);
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let err = perform_attach(&mut t, &req).unwrap_err();
        assert!(matches!(err, AttachError::Refused { .. }));
    }

    #[test]
    fn perform_attach_recv_error_is_malformed() {
        let mut t = FakeTransport::new(vec![]);
        t.recv_err = true;
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let err = perform_attach(&mut t, &req).unwrap_err();
        assert!(matches!(err, AttachError::Malformed(_)));
    }

    // Build the server-side bytes a daemon would write back: one attach-OK line
    // then the raw PTY tail.
    fn server_bytes(reply: &str, raw_tail: &[u8]) -> Cursor<Vec<u8>> {
        let mut v = format!("{reply}\n").into_bytes();
        v.extend_from_slice(raw_tail);
        Cursor::new(v)
    }

    #[test]
    fn attach_for_frames_returns_ok_then_raw_tail() {
        let raw = b"\x1b[2J\x1b[Hhello world";
        let reader = server_bytes(r#"{"ok":true,"op":"attach","state":"running"}"#, raw);
        let mut writer: Vec<u8> = Vec::new();
        let req = AttachRequest::for_frame_stream("a1b2c3d4", Some("k".into()));
        let (ok, mut stream) = attach_for_frames(&mut writer, reader, &req).unwrap();
        assert_eq!(ok.state.as_deref(), Some("running"));
        // The attach request went out on the writer.
        assert!(String::from_utf8_lossy(&writer).contains("\"op\":\"attach\""));
        // Everything after the handshake line is the raw frame stream, intact.
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        assert_eq!(got, raw);
    }

    #[test]
    fn attach_for_frames_keeps_tail_buffered_with_handshake() {
        // The raw tail arrives in the SAME recv as the handshake line; it must not
        // be lost when read_line stops at the newline.
        let raw = b"first-frame-bytes";
        let reader = server_bytes(r#"{"ok":true,"op":"attach"}"#, raw);
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let (_ok, mut stream) = attach_for_frames(Vec::new(), reader, &req).unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        assert_eq!(got, raw);
    }

    #[test]
    fn attach_for_frames_tail_with_embedded_newlines_is_not_split() {
        // PTY content contains \r and \n as ordinary bytes; the stream must hand
        // them back verbatim, never line-framed.
        let raw = b"line1\r\nline2\nline3";
        let reader = server_bytes(r#"{"ok":true,"op":"attach"}"#, raw);
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let (_ok, mut stream) = attach_for_frames(Vec::new(), reader, &req).unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        assert_eq!(got, raw);
    }

    #[test]
    fn attach_for_frames_refused_propagates() {
        let reader = server_bytes(
            r#"{"ok":false,"code":"EPROTO","error":"restart claude"}"#,
            b"",
        );
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let err = attach_for_frames(Vec::new(), reader, &req).unwrap_err();
        assert_eq!(
            err,
            AttachError::Refused {
                code: Some("EPROTO".into()),
                detail: "restart claude".into()
            }
        );
    }

    #[test]
    fn attach_for_frames_eof_before_reply_is_refused() {
        let reader = Cursor::new(Vec::new());
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let err = attach_for_frames(Vec::new(), reader, &req).unwrap_err();
        assert!(matches!(err, AttachError::Refused { .. }));
    }

    #[test]
    fn attach_for_frames_truncated_reply_without_newline_is_refused() {
        // Valid JSON but the daemon closed before the protocol newline: the
        // handshake is incomplete and the frame stream would be empty, so this
        // must refuse rather than report a successful attach.
        let reader = Cursor::new(br#"{"ok":true,"op":"attach","state":"running"}"#.to_vec());
        let req = AttachRequest::for_frame_stream("a1b2c3d4", None);
        let err = attach_for_frames(Vec::new(), reader, &req).unwrap_err();
        assert!(matches!(err, AttachError::Refused { .. }));
    }
}
