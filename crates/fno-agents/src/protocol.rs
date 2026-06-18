//! Unix-socket wire format (Wave 3, task 3.1).
//!
//! ## Framing (Claude's Discretion #6)
//!
//! Length-prefixed JSON: a **4-byte little-endian `u32` length** prefix, then
//! that many bytes of UTF-8 JSON. The binary prefix is chosen over LSP-style
//! `Content-Length:\r\n\r\n` for tightness; the trade-off (less human-readable
//! on the wire) is acceptable because the daemon is not a debugging surface and
//! every frame is structured JSON regardless. Frames over [`MAX_FRAME_BYTES`]
//! are rejected before allocation, so a corrupt or hostile length prefix cannot
//! drive an unbounded allocation.
//!
//! ## Two namespaces, one socket
//!
//! Methods are namespaced by a `<namespace>.<verb>` prefix:
//!
//! - `agent.*` — `fno-agents` client (spawn / ask / list / stop / rm /
//!   reconcile / status).
//! - `channel.*` — the Phase 5 channel server (register_channel /
//!   unregister_channel / push_to_channel).
//!
//! The split is namespace-only: same socket, same Unix-permission gate,
//! different `method` prefixes. [`Namespace::of`] classifies a method so the
//! daemon can route without string-matching at every call site.
//!
//! ## Drive (Wave 4 seam)
//!
//! Interactive drive upgrades a connection to a WebSocket after the initial
//! `agent.drive` request. Wave 3 lands the request/response transport only; the
//! upgrade handshake and binary PTY frames are Wave 4. [`Namespace`] reserves no
//! special drive variant because the upgrade is signalled by the `agent.drive`
//! method, handled by the daemon, not by a distinct frame type.

use serde::{de, ser::SerializeStruct, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap on a single frame's JSON body. A length prefix larger than this is
/// rejected before any allocation (Failure Modes: "reject malformed JSON-RPC
/// frames ... never crash the daemon"). 16 MiB comfortably covers the largest
/// legitimate payload (a 64KB ask plus envelope) with headroom.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Wire/transport errors.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame exceeds max size: {0} > {MAX_FRAME_BYTES}")]
    FrameTooLarge(u32),
    #[error("malformed json frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("connection closed before a full frame was read")]
    UnexpectedEof,
}

/// A request from a client to the daemon. `id` correlates the response on a
/// multiplexed connection; `method` is `<namespace>.<verb>`; `params` is an
/// opaque JSON object the handler interprets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Request {
            id,
            method: method.into(),
            params,
        }
    }
}

/// The success-or-error payload of a [`Response`]. Making this a sum type means
/// "exactly one of result / error" is unrepresentable-as-violated: there is no
/// `{result: None, error: None}` nor `{result: Some, error: Some}` state to
/// guard against. The flat `{id, result | error}` wire shape is preserved by
/// `Response`'s hand-written [`Serialize`]/[`Deserialize`] below.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponsePayload {
    /// Success: the `result` value.
    Ok(Value),
    /// Failure: the structured `error`.
    Err(RpcError),
}

/// A response to a [`Request`]. Carries exactly one of result / error via
/// [`ResponsePayload`]. The wire shape stays flat (`{id, result}` or
/// `{id, error}`) for cross-language parity.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: u64,
    pub payload: ResponsePayload,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Response {
            id,
            payload: ResponsePayload::Ok(result),
        }
    }

    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Response {
            id,
            payload: ResponsePayload::Err(RpcError {
                code,
                message: message.into(),
            }),
        }
    }

    /// True if this response carries an error.
    pub fn is_err(&self) -> bool {
        matches!(self.payload, ResponsePayload::Err(_))
    }

    /// The success value, or `None` if this is an error response.
    pub fn result(&self) -> Option<&Value> {
        match &self.payload {
            ResponsePayload::Ok(v) => Some(v),
            ResponsePayload::Err(_) => None,
        }
    }

    /// The structured error, or `None` if this is a success response.
    pub fn error(&self) -> Option<&RpcError> {
        match &self.payload {
            ResponsePayload::Err(e) => Some(e),
            ResponsePayload::Ok(_) => None,
        }
    }
}

impl Serialize for Response {
    /// Emit the flat `{id, result}` / `{id, error}` shape. Only the populated
    /// arm is written, matching the pre-sum-type `skip_serializing_if` behavior.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut st = serializer.serialize_struct("Response", 2)?;
        st.serialize_field("id", &self.id)?;
        match &self.payload {
            ResponsePayload::Ok(v) => st.serialize_field("result", v)?,
            ResponsePayload::Err(e) => st.serialize_field("error", e)?,
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    /// Parse the flat wire shape, enforcing the exactly-one invariant: a frame
    /// with both or neither of result / error is a malformed response.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Distinguish "result field present but null" (a valid JSON-RPC success
        // carrying a null value) from "result field absent". A plain
        // `#[serde(default)] Option<Value>` collapses both to `None` because
        // serde maps JSON null -> None, which would make `{"id":1,"result":null}`
        // hit the (None, None) reject arm. `deserialize_with` only runs when the
        // field is present, so an explicit null becomes `Some(Value::Null)` while
        // a truly absent field still defaults to `None`.
        fn present_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Value::deserialize(deserializer).map(Some)
        }

        #[derive(Deserialize)]
        struct Wire {
            id: u64,
            #[serde(default, deserialize_with = "present_value")]
            result: Option<Value>,
            #[serde(default)]
            error: Option<RpcError>,
        }
        let w = Wire::deserialize(deserializer)?;
        let payload = match (w.result, w.error) {
            (Some(_), Some(_)) => {
                return Err(de::Error::custom(
                    "response carries both `result` and `error`",
                ))
            }
            (Some(r), None) => ResponsePayload::Ok(r),
            (None, Some(e)) => ResponsePayload::Err(e),
            (None, None) => {
                return Err(de::Error::custom(
                    "response carries neither `result` nor `error`",
                ))
            }
        };
        Ok(Response { id: w.id, payload })
    }
}

/// Structured error in a [`Response`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}

/// Stable machine-readable error codes. Clients map these to exit codes; the
/// design's per-verb exit codes (13/14/15/18, ...) are applied client-side from
/// these. Serialized snake_case for cross-language parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Frame/JSON could not be parsed.
    MalformedFrame,
    /// Method has no handler / unknown namespace.
    UnknownMethod,
    /// Required params missing or wrong type.
    InvalidParams,
    /// Named agent does not exist.
    AgentNotFound,
    /// Agent already exists (spawn name collision).
    AgentExists,
    /// Operation rejected for the agent's current status.
    InvalidStatus,
    /// Capacity/concurrency cap hit (drive max_concurrent, watcher cap, ...).
    Busy,
    /// Lock acquisition timed out.
    LockTimeout,
    /// Spawn failed pre-launch (binary missing, cwd inaccessible, ...).
    SpawnFailed,
    /// channel.* against an unknown cc_session_id / channel id.
    ChannelUnknown,
    /// Catch-all internal fault; daemon stays up, surfaces this.
    Internal,
}

/// Method namespace, derived from the `<namespace>.<verb>` method prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// `agent.*` — the `fno-agents` client surface.
    Agent,
    /// `channel.*` — the Phase 5 channel server surface.
    Channel,
    /// Anything else (rejected with [`ErrorCode::UnknownMethod`]).
    Unknown,
}

impl Namespace {
    /// Classify a method string by its prefix before the first `.`.
    pub fn of(method: &str) -> Namespace {
        match method.split_once('.') {
            Some(("agent", _)) => Namespace::Agent,
            Some(("channel", _)) => Namespace::Channel,
            _ => Namespace::Unknown,
        }
    }

    /// The verb portion after the namespace prefix (`agent.spawn` -> `spawn`).
    pub fn verb(method: &str) -> Option<&str> {
        method.split_once('.').map(|(_, v)| v)
    }
}

// ---------------------------------------------------------------------------
// Async frame codec.
// ---------------------------------------------------------------------------

/// Read one length-prefixed frame's raw JSON bytes. Returns
/// [`ProtocolError::UnexpectedEof`] if the connection closes between frames
/// (clean disconnect) or mid-frame (truncated). The caller treats a clean EOF
/// as "client hung up", not a daemon fault.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(ProtocolError::UnexpectedEof)
        }
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => ProtocolError::UnexpectedEof,
            _ => ProtocolError::Io(e),
        })?;
    Ok(body)
}

/// Write `body` as one length-prefixed frame and flush. Rejects a body larger
/// than [`MAX_FRAME_BYTES`] before writing so a buggy handler cannot emit an
/// un-readable frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> Result<(), ProtocolError> {
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| ProtocolError::FrameTooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read and deserialize one [`Request`].
pub async fn read_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Request, ProtocolError> {
    let body = read_frame(reader).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Serialize and write one [`Request`].
pub async fn write_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    req: &Request,
) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(req)?;
    write_frame(writer, &body).await
}

/// Read and deserialize one [`Response`].
pub async fn read_response<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Response, ProtocolError> {
    let body = read_frame(reader).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Serialize and write one [`Response`].
pub async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &Response,
) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(resp)?;
    write_frame(writer, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn namespace_classification() {
        assert_eq!(Namespace::of("agent.spawn"), Namespace::Agent);
        assert_eq!(
            Namespace::of("channel.register_channel"),
            Namespace::Channel
        );
        assert_eq!(Namespace::of("bogus.method"), Namespace::Unknown);
        assert_eq!(Namespace::of("noseparator"), Namespace::Unknown);
        assert_eq!(Namespace::verb("agent.spawn"), Some("spawn"));
        assert_eq!(Namespace::verb("nope"), None);
    }

    #[tokio::test]
    async fn request_roundtrips_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let req = Request::new(7, "agent.spawn", json!({"name": "worker-A"}));
        write_request(&mut a, &req).await.unwrap();
        let got = read_request(&mut b).await.unwrap();
        assert_eq!(got, req);
    }

    #[tokio::test]
    async fn response_ok_and_err_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let ok = Response::ok(7, json!({"status": "live"}));
        write_response(&mut a, &ok).await.unwrap();
        let got = read_response(&mut b).await.unwrap();
        assert_eq!(got, ok);
        assert!(!got.is_err());

        let err = Response::err(8, ErrorCode::AgentNotFound, "no such agent");
        write_response(&mut a, &err).await.unwrap();
        let got = read_response(&mut b).await.unwrap();
        assert!(got.is_err());
        assert_eq!(got.error().unwrap().code, ErrorCode::AgentNotFound);
    }

    #[tokio::test]
    async fn frame_too_large_is_rejected_not_allocated() {
        // Hand-write a length prefix exceeding the cap; the reader must reject
        // it without trying to allocate gigabytes.
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            let bogus_len = (MAX_FRAME_BYTES + 1).to_le_bytes();
            a.write_all(&bogus_len).await.unwrap();
            a.flush().await.unwrap();
            // keep `a` alive so the reader sees the prefix
            a
        });
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
        let _a = writer.await.unwrap();
    }

    #[tokio::test]
    async fn clean_eof_is_distinguished_from_io_error() {
        let (a, mut b) = tokio::io::duplex(64);
        drop(a); // client hangs up with no bytes
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtocolError::UnexpectedEof));
    }

    #[test]
    fn response_wire_shape_is_flat() {
        // The sum-type refactor must not change the bytes on the wire: a success
        // serializes to {id, result}, an error to {id, error}, nothing else.
        let ok = Response::ok(7, json!({"status": "live"}));
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"id": 7, "result": {"status": "live"}})
        );
        let err = Response::err(8, ErrorCode::AgentNotFound, "no such agent");
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"id": 8, "error": {"code": "agent_not_found", "message": "no such agent"}})
        );
    }

    #[test]
    fn response_rejects_both_or_neither_payload() {
        // The exactly-one invariant is now enforced at deserialize time, so a
        // malformed frame from a buggy/hostile peer is a parse error, not a
        // half-populated Response.
        let both = json!({"id": 1, "result": {}, "error": {"code": "internal", "message": "x"}});
        assert!(serde_json::from_value::<Response>(both).is_err());
        // Truly absent result AND error (not merely null) is the rejected case.
        let neither = json!({"id": 1});
        assert!(serde_json::from_value::<Response>(neither).is_err());
    }

    #[test]
    fn response_accepts_explicit_null_result() {
        // A present-but-null result is a valid JSON-RPC success and must NOT be
        // confused with an absent field (Gemini HIGH on PR #341). It parses to
        // Ok(Null) and roundtrips.
        let parsed: Response =
            serde_json::from_value(json!({"id": 7, "result": null})).expect("null result parses");
        assert!(!parsed.is_err());
        assert_eq!(parsed.result(), Some(&Value::Null));
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            json!({"id": 7, "result": null})
        );
    }

    #[tokio::test]
    async fn malformed_json_body_surfaces_json_error() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frame(&mut a, b"{not json").await.unwrap();
        let err = read_request(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Json(_)));
    }
}
