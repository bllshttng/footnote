//! reader + frozen-wire-shape families: moved verbatim out of proto.rs
//! (file budget shrink). Parent helpers resolve through the glob.
use super::*;

#[test]
fn proto_reader_rejects_oversized_length_prefix() {
    let mut bytes = (MAX_MSG_BYTES + 1).to_be_bytes().to_vec();
    bytes.extend_from_slice(b"junk");
    let mut cursor = std::io::Cursor::new(bytes);
    let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
    assert!(matches!(res, Err(ProtoError::TooLarge(_))), "{res:?}");
}

#[test]
fn proto_reader_surfaces_malformed_body_as_error() {
    // Valid length prefix, garbage body: must error, never yield a value.
    let body = b"not json at all";
    let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(body);
    let mut cursor = std::io::Cursor::new(bytes);
    let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
    assert!(matches!(res, Err(ProtoError::Malformed(_))), "{res:?}");
}

#[test]
fn proto_clean_eof_reads_as_closed() {
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
    assert!(matches!(res, Err(ProtoError::Closed)), "{res:?}");
}

#[test]
fn proto_mid_body_eof_reads_as_closed() {
    let body = br#"{"Ok":null}"#;
    let mut bytes = ((body.len() + 1) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(body);
    let mut cursor = std::io::Cursor::new(bytes);
    let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
    assert!(matches!(res, Err(ProtoError::Closed)), "{res:?}");
}

// (x-9b60) The version-pin family lives in the child module below, named
// by the question it answers; the file-budget gate keeps this over-budget
// file shrink-only.
mod version_pin;
#[test]
fn proto_frame_geometry_check_catches_cell_count_mismatch() {
    let mut f = test_frame();
    assert!(f.geometry_ok());
    f.cells.pop();
    assert!(!f.geometry_ok(), "short cells vec must fail the check");
    f.cells.clear();
    assert!(!f.geometry_ok());
}

#[test]
fn proto_pre_attach_wire_shapes_are_frozen() {
    // Query/KillServer/Info bypass the version handshake, so their JSON
    // encodings are FROZEN forever (Invariants). This pins the exact
    // bytes: if this test breaks, you changed a frozen shape - add a new
    // variant instead.
    assert_eq!(
        serde_json::to_string(&ClientMsg::Query).unwrap(),
        r#""Query""#
    );
    assert_eq!(
        serde_json::to_string(&ClientMsg::KillServer).unwrap(),
        r#""KillServer""#
    );
    assert_eq!(
        serde_json::to_string(&ServerMsg::Info {
            session: "s".into(),
            clients: 1,
            squads: 2,
            panes: 3,
        })
        .unwrap(),
        r#"{"Info":{"session":"s","clients":1,"squads":2,"panes":3}}"#
    );
}

#[test]
fn proto_v83_agent_launch_roundtrips() {
    // The launcher exchange is additive (v83): both payloads survive a
    // wire encode/decode with every field intact, and the tagged launch
    // state keeps its snake_case kind on the wire.
    let req = AgentLaunchRequest {
        request_id: 7,
        revision: 3,
        cwd: "/tmp/proj".into(),
        harness: "codex".into(),
        substrate: "pane".into(),
        model: None,
        model_names_harness: false,
        effort: Some("high".into()),
        permission_mode: None,
        placement: Some("name:work".into()),
        portal: None,
        split: None,
        message: "line one\nline \"two\" $ ` \u{1f600}".into(),
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    write_msg_sync(&mut buf, &ClientMsg::AgentLaunch(req.clone())).unwrap();
    buf.set_position(0);
    let ClientMsg::AgentLaunch(back) = read_msg_sync(&mut buf).unwrap() else {
        panic!("wrong variant");
    };
    assert_eq!(back, req);

    for state in [
        LaunchState::Starting,
        LaunchState::Refused {
            reason: "no capacity".into(),
        },
        LaunchState::Unknown {
            reason: "timed out".into(),
        },
        LaunchState::Launched {
            name: "w".into(),
            pane: Some(9),
            seed_delivered: Some(false),
        },
    ] {
        let update = AgentLaunchUpdate {
            request_id: 7,
            state,
        };
        let mut buf = std::io::Cursor::new(Vec::new());
        write_msg_sync(&mut buf, &ServerMsg::AgentLaunch(update.clone())).unwrap();
        buf.set_position(0);
        let ServerMsg::AgentLaunch(back) = read_msg_sync(&mut buf).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(back, update);
    }
    // The tagged kind is stable on the wire for observability.
    let json = serde_json::to_string(&LaunchState::Launched {
        name: "w".into(),
        pane: None,
        seed_delivered: None,
    })
    .unwrap();
    assert!(json.contains(r#""kind":"launched""#), "{json}");
}
