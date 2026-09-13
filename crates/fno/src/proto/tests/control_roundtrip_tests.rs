//! control round-trip families: moved verbatim out of proto.rs
//! (file budget shrink; the v75 RetireSession entries ride the same lists).
//! Parent helpers resolve through the glob.
use super::*;

#[test]
fn proto_v4_control_verbs_roundtrip() {
    // Every control verb survives the codec inside the versioned Control
    // envelope (mirrors the v3 discipline). PROTO_VERSION rides along so a
    // skew is detectable server-side.
    for verb in [
        ControlVerb::PaneLs,
        ControlVerb::PaneRead {
            pane: 3,
            lines: Some(40),
            block: None,
        },
        ControlVerb::PaneRead {
            pane: 3,
            lines: None,
            block: Some(BlockSel::Last),
        },
        ControlVerb::PaneRead {
            pane: 3,
            lines: None,
            block: Some(BlockSel::Seq(7)),
        },
        ControlVerb::PaneRun {
            cwd: "/code/footnote".into(),
            argv: vec!["claude".into(), "--print".into()],
            cols: Some(120),
            rows: None,
            claim: true,
            placement: PanePlacement::default(),
            worker: None,
        },
        ControlVerb::PaneClaim {
            pane: 5,
            holder_pid: 4242,
        },
        ControlVerb::PaneRelease { pane: 5 },
        ControlVerb::PaneSend {
            pane: 5,
            bytes: b"hello\r".to_vec(),
            guarded: true,
            expected_identity: None,
        },
        ControlVerb::PaneWait {
            pane: 5,
            quiet_ms: Some(200),
            pattern: Some("done".into()),
            timeout_ms: 5000,
            command_done: true,
        },
        ControlVerb::PaneKill { pane: 5 },
        ControlVerb::RetireSession {
            harness: "codex".into(),
            session_id: "01a03a85-1111-7222-8333-444455556666".into(),
        },
    ] {
        let msg = ClientMsg::Control {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.into(),
            verb,
        };
        let bytes = encode(&msg).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let decoded: ClientMsg = read_msg_sync(&mut cursor).unwrap();
        assert_eq!(decoded, msg);
    }
}

#[test]
fn proto_v4_control_replies_roundtrip() {
    for msg in [
        ServerMsg::PaneList {
            panes: vec![PaneInfo {
                shell_idle: false,
                pane_id: 4,
                squad_id: 1,
                squad_name: None,
                tab_id: 7,
                cwd: "/code/footnote".into(),
                child_pid: Some(4242),
                title: None,
                pristine_idle_shell: false,
                tab_name: None,
                tab_ordinal: Some(1),
                fno_id: None,
                orphaned_worker: false,
                release: None,
                harness_session_id: None,
                predecessor_session_ids: Vec::new(),
                forked_from_session_id: None,
                name: None,
            }],
        },
        ServerMsg::PaneText {
            pane_id: 4,
            text: "marker-42\n$ ".into(),
            block: None,
            pane_name: None,
            registry_fno_id: None,
        },
        ServerMsg::PaneText {
            pane_id: 4,
            text: "$ false".into(),
            block: Some(BlockMeta {
                seq: Some(2),
                exit: Some(1),
                complete: true,
                truncated: false,
                implicit: false,
            }),
            pane_name: None,
            registry_fno_id: None,
        },
        ServerMsg::PaneSpawned {
            pane_id: 9,
            placement: None,
        },
        ServerMsg::Ok,
        ServerMsg::WaitDone {
            outcome: WaitOutcome::Quiet,
        },
        ServerMsg::WaitDone {
            outcome: WaitOutcome::Timeout,
        },
        ServerMsg::WaitDone {
            outcome: WaitOutcome::CommandDone { exit: Some(0) },
        },
        ServerMsg::Err {
            code: err_code::DEAD_PANE,
            msg: "no such pane: 99".into(),
        },
        ServerMsg::Copy {
            text: "selected lines\nincluding history".into(),
        },
        ServerMsg::SearchResult {
            pane_id: 4,
            total: 12,
            current: 3,
        },
        ServerMsg::SearchResult {
            pane_id: 4,
            total: 0,
            current: 0,
        },
        ServerMsg::SessionRetired {
            retired: 2,
            panes_closed: 1,
            closed_panes: vec!["t-r-one".into()],
            tabs_removed: vec!["targets/lanes".into()],
        },
        ServerMsg::SessionRetired {
            retired: 0,
            panes_closed: 0,
            closed_panes: Vec::new(),
            tabs_removed: Vec::new(),
        },
    ] {
        let bytes = encode(&msg).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let decoded: ServerMsg = read_msg_sync(&mut cursor).unwrap();
        assert_eq!(decoded, msg);
    }
}
