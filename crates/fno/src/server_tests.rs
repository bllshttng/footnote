use super::*;
use crate::proto::AgentRow;
use crate::pty::ChildGuard;
use crate::restore_gate::{set_restore_registry_rows, RestoreRegistryRowsGuard};

#[path = "server/server_thread_viewer_tests.rs"]
mod thread_viewer_tests;
use crate::proto::TemplateName;
// The portal test family lives in its own module; this file is shrink-only.
#[path = "server/tests/portal_tests.rs"]
mod portal_tests;
// Same treatment: the lifecycle-resolution test family.
#[path = "server/tests/lifecycle_tests.rs"]
mod lifecycle_tests;
// The (v72) re-seat test family, same treatment.
#[path = "server/tests/reseat_tests.rs"]
mod reseat_tests;

// The (x-eb79) resume-argv staging family, same treatment.
#[path = "server/tests/resume_argv_staging_tests.rs"]
mod resume_argv_staging_tests;

// The sideline rename test family, same treatment.
#[path = "server/tests/rename_tests.rs"]
mod rename_tests;

// The squad-store sync family (prune reload marker + negative control).
#[path = "server/tests/squad_sync_tests.rs"]
mod squad_sync_tests;

// (v75, x-7649) The exact-session retirement handler family.
#[path = "server/tests/retire_session_tests.rs"]
mod retire_session_tests;

// The per-pane orphan verdict family.
#[path = "server/tests/pane_identity_tests.rs"]
mod pane_identity_tests;

// (x-b64e) The restore test family, same treatment: the file is
// shrink-only under the file-budget gate. Moved verbatim.
#[path = "server/tests/server_restore_tests.rs"]
mod server_restore_tests;
#[path = "server/tests/shutdown_tests.rs"]
mod shutdown_tests;
// The keeper re-adoption test family, same treatment.
#[path = "server/tests/keeper_adopt_tests.rs"]
mod keeper_adopt_tests;
// The pane_send fail-closed gate family.
#[path = "server/tests/pane_send_gate_tests.rs"]
mod pane_send_gate_tests;
// The dead-row resume disposition family.
#[path = "server/tests/dead_row_resume_tests.rs"]
mod dead_row_resume_tests;

#[test]
fn account_from_argv_reads_the_fno_account_token() {
    // x-c914: the birth account rides the same env(1) wrapper as FNO_NODE.
    let from = |a: &[&str]| account_from_argv(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(
        from(&["env", "FNO_NODE=x-1", "FNO_ACCOUNT=readyrule", "claude"]),
        Some("readyrule".to_string())
    );
    // Default account (no token) / ad-hoc pane / empty value -> None.
    assert_eq!(from(&["env", "FNO_NODE=x-1", "claude"]), None);
    assert_eq!(from(&["claude"]), None);
    assert_eq!(from(&["env", "FNO_ACCOUNT=", "claude"]), None);
}

#[test]
fn name_attached_pane_titles_an_attached_pane_from_its_registered_name() {
    // x-ed59: an attached/driven pane reads its registered name from the live
    // catalog (the sidepane's source) so its tab/pane title matches the row,
    // not the `claude` command basename. Falls through unchanged for an ad-hoc
    // attach with no matching worker.
    let (mut core, _client, p1, _p2, _rx) = seen_test_core();
    assert_eq!(
        core.panes.get(&p1).unwrap().name,
        None,
        "shell pane starts unnamed"
    );
    core.agents = vec![bg_row("build", "/tmp", Some("deadbee2"))];
    core.name_attached_pane(p1, "deadbee2", None);
    assert_eq!(core.panes.get(&p1).unwrap().name.as_deref(), Some("build"));
    // An ad-hoc attach (no matching worker) leaves the name unset.
    let (mut core2, _, q1, _, _) = seen_test_core();
    core2.name_attached_pane(q1, "no-such-worker", None);
    assert_eq!(core2.panes.get(&q1).unwrap().name, None);
}

#[test]
fn attach_argv_routes_isolated_account_to_its_daemon(/* codex P1 */) {
    set_attach_program(&["claude", "attach"]); // pin the base (no leak)
                                               // Default account: no env wrapper (byte-identical to the bare attach).
    assert_eq!(
        attach_argv("job1", None, None),
        vec![
            "claude".to_string(),
            "attach".to_string(),
            "job1".to_string()
        ]
    );
    // Isolated account: wrapped so `claude attach` hits THAT daemon, with the
    // birth account stamped for the re-attached pane's glyph.
    let dir = std::path::Path::new("/home/u/.claude-alt");
    assert_eq!(
        attach_argv("job1", Some("readyrule"), Some(dir)),
        vec![
            "env".to_string(),
            "CLAUDE_CONFIG_DIR=/home/u/.claude-alt".to_string(),
            "FNO_ACCOUNT=readyrule".to_string(),
            "claude".to_string(),
            "attach".to_string(),
            "job1".to_string(),
        ]
    );
}

/// AC14-HP, AC15-HP (x-6678, x-296f): the argv builder is keyed on the
/// row's DECLARED attach form. A codex thread execs codex's own TUI after
/// its daemon pre-exec; claude is byte-identical to what it was, account
/// wrapper included.
#[test]
fn attach_argv_execs_each_harness_own_interface() {
    set_attach_program(&["claude", "attach"]);
    let dir = std::path::Path::new("/home/u/.claude-alt");

    // AC15: claude, both postures, unchanged.
    assert_eq!(
        attach_argv_for(Some("claude"), "job1", None, None),
        attach_argv("job1", None, None)
    );
    assert_eq!(
        attach_argv_for(Some("claude"), "job1", Some("readyrule"), Some(dir)),
        attach_argv("job1", Some("readyrule"), Some(dir))
    );
    // A row with no harness recorded keeps the claude shape it had before
    // a harness was passed at all.
    assert_eq!(
        attach_argv_for(None, "job1", None, None),
        attach_argv("job1", None, None)
    );

    // AC14: codex, rendered from the declaration. No account wrapper - the
    // control socket, and therefore CODEX_HOME, is what decides which
    // daemon this reaches; a non-claude harness carrying a config_dir must
    // not inherit a CLAUDE_CONFIG_DIR prefix.
    let uuid = "01a04546-28b2-7a41-ae4c-892bbeb8e295";
    let form = agents_view::attach_form("codex").expect("codex declares an attach form");
    assert_eq!(
        attach_argv_for(Some("codex"), uuid, Some("readyrule"), Some(dir)),
        form.render(uuid)
    );
    assert_eq!(form.render(uuid)[..2], ["sh".to_string(), "-c".to_string()]);

    // Cursor Agent declares NO attach form: a second --resume process is
    // a rival TUI on the same remote chat, not a join. Its rows are
    // driven by pane or by mail, and the re-entry form (--resume with
    // --trust) belongs to resume, not to the attach door.
    assert!(agents_view::attach_form("cursor-agent").is_none());
    assert!(
        agents_view::resume_form("cursor-agent").is_some(),
        "the resume lane stays the honest re-entry"
    );
}

#[test]
fn env_provenance_survives_the_account_scrub_prefix() {
    // The REAL `_mesh_env_wrapper` output for an --account spawn: the auth-var
    // scrub (`-u VAR` pairs) leads the assignments. Both FNO_ACCOUNT AND
    // FNO_NODE must still parse past it (codex P1: a naive scan stopped on
    // `-u` and dropped both, so the badge never showed and node provenance
    // was lost for every routed spawn).
    let argv: Vec<String> = [
        "env",
        "-u",
        "ANTHROPIC_API_KEY",
        "-u",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "FNO_AGENT_SELF=w",
        "FNO_NODE=x-1",
        "FNO_ACCOUNT=readyrule",
        "CLAUDE_CONFIG_DIR=/home/u/.claude-alt",
        "claude",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(node_from_argv(&argv), Some("x-1".to_string()));
    assert_eq!(account_from_argv(&argv), Some("readyrule".to_string()));
    assert_eq!(agent_self_from_argv(&argv), Some("w".to_string()));
    assert_eq!(cmd_from_argv(&argv).as_deref(), Some("claude"));
}

#[test]
fn pane_label_prefers_cmd_then_node_then_cwd_when_no_registered_name() {
    // The navigator's pane label (v22, x-653d) when no registered name: cmd
    // is the intra-tab discriminator, then node, then the cwd basename, else
    // "shell". (x-0ba1: a registered name leads when present - see
    // pane_label_prefers_the_registered_name.)
    assert_eq!(
        pane_label(None, Some("x-abcd"), "/home/u/proj", Some("claude")),
        "claude"
    );
    assert_eq!(
        pane_label(None, Some("x-abcd"), "/home/u/proj", None),
        "x-abcd"
    );
    assert_eq!(pane_label(None, None, "/home/u/proj", None), "proj");
    assert_eq!(pane_label(None, None, "", None), "shell");
    // A control-only candidate sanitizes to empty and falls through.
    assert_eq!(
        pane_label(None, None, "/home/u/proj", Some("\u{7}")),
        "proj"
    );
}

#[test]
fn cmd_from_argv_takes_the_command_basename_past_the_env_wrapper() {
    let cmd = |a: &[&str]| cmd_from_argv(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(
        cmd(&["env", "FNO_NODE=x-1", "claude", "--bg"]),
        Some("claude".into())
    );
    assert_eq!(cmd(&["/usr/bin/htop"]), Some("htop".into()));
    // An env run with no command yields None; spawn never fails on labeling.
    assert_eq!(cmd(&["env", "A=1"]), None);
    assert_eq!(cmd(&[]), None);
}

#[test]
fn tab_label_resolves_the_locked_derivation_chain() {
    // Explicit rename wins outright.
    assert_eq!(
        tab_label(
            Some("debug"),
            Some((None, Some("x-1"), "/w/x-2", Some("claude"))),
            "/w",
            0
        ),
        "debug"
    );
    // FNO_NODE provenance beats cwd + cmd (AC1-HP). name absent here.
    assert_eq!(
        tab_label(
            None,
            Some((None, Some("x-abcd"), "/w/x-2", Some("claude"))),
            "/w",
            0
        ),
        "x-abcd"
    );
    // A spawn cwd whose basename differs from the squad's outranks the
    // cmd label (AC2-EDGE: the worktree-per-node case).
    assert_eq!(
        tab_label(
            None,
            Some((
                None,
                None,
                "/conductor/workspaces/footnote/x-9f21",
                Some("claude")
            )),
            "/code/footnote",
            1
        ),
        "x-9f21"
    );
    // Same basename would just echo the squad label -> cmd.
    assert_eq!(
        tab_label(
            None,
            Some((None, None, "/code/footnote", Some("htop"))),
            "/code/footnote",
            1
        ),
        "htop"
    );
    // Every source empty -> the bare 1-based index, exactly today's
    // label (AC1-EDGE, AC2-FR: nothing errors, logs, or bells).
    assert_eq!(
        tab_label(
            None,
            Some((None, None, "/code/footnote", None)),
            "/code/footnote",
            2
        ),
        "3"
    );
}

#[test]
fn tab_label_stale_focused_pane_falls_back_to_index() {
    // AC3-FR: tab.focus names a reaped pane (mid-reap race) - the chain
    // skips provenance/cwd/cmd and terminates at the index, no panic.
    assert_eq!(tab_label(None, None, "/w", 0), "1");
}

#[test]
fn tab_label_sanitizes_derived_candidates_and_skips_empty_ones() {
    // codex peer review: derived sources (FNO_NODE, dir names, argv) admit
    // control bytes and land in chrome cells - sanitize like a rename.
    assert_eq!(
        tab_label(None, Some((None, Some("\x1b[31mx-1"), "/w", None)), "/w", 0),
        "[31mx-1"
    );
    // A whitespace-only node sanitizes to empty and falls through to the
    // next source instead of rendering a blank label.
    assert_eq!(
        tab_label(None, Some((None, Some("   "), "/w/x-2", None)), "/w", 0),
        "x-2"
    );
    // A control-char-only dir basename falls through to cmd.
    assert_eq!(
        tab_label(
            None,
            Some((None, None, "/w/\x01\x02", Some("htop"))),
            "/w",
            0
        ),
        "htop"
    );
}

#[test]
fn agent_self_from_argv_reads_the_registered_worker_name_past_qos_wrappers() {
    // x-0ba1: the registered name lives in the env(1) prefix like FNO_NODE
    // and parses past the same -u scrub. argv[0] past the env run is the
    // QoS wrapper (taskpolicy), not the provider - the title must not read it.
    let to_argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let argv = to_argv(&[
        "env",
        "FNO_AGENT_SELF=build",
        "FNO_NODE=x-2af5",
        "/usr/sbin/taskpolicy",
        "-c",
        "utility",
        "--",
        "claude",
    ]);
    assert_eq!(agent_self_from_argv(&argv).as_deref(), Some("build"));
    // Empty value falls through (env_token_from_argv filters empties).
    assert_eq!(
        agent_self_from_argv(&to_argv(&["env", "FNO_AGENT_SELF=", "claude"])),
        None
    );
    // An ad-hoc `pane run -- htop` carries no FNO_AGENT_SELF.
    assert_eq!(agent_self_from_argv(&to_argv(&["htop"])), None);
}

#[test]
fn tab_label_reads_the_registered_name_over_a_qos_wrapper() {
    // x-0ba1 decision b: a QoS-wrapped worker's argv[0] is taskpolicy/nice,
    // so titling from the process table collapses every worker to one label.
    // The registered name is the top derived source and wins over node/cwd/cmd.
    let argv: Vec<String> = [
        "env",
        "FNO_AGENT_SELF=build",
        "FNO_NODE=x-2af5",
        "/usr/sbin/taskpolicy",
        "-c",
        "utility",
        "--",
        "claude",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let pane = (
        agent_self_from_argv(&argv),
        node_from_argv(&argv),
        String::from("/w"),
        cmd_from_argv(&argv),
    );
    // What a process-table title would show (and must NOT be used):
    assert_eq!(pane.3.as_deref(), Some("taskpolicy"));
    assert_eq!(
        tab_label(
            None,
            Some((
                pane.0.as_deref(),
                pane.1.as_deref(),
                pane.2.as_str(),
                pane.3.as_deref()
            )),
            "/w",
            0,
        ),
        "build"
    );
}

#[test]
fn tab_label_distinguishes_two_registered_workers_in_one_session() {
    // x-0ba1 verify: registered names are unique per session, so a second
    // worker yields a different title. cwd basenames are not unique (two
    // workers share a worktree), so name - not cwd - distinguishes them.
    let mk = |self_name: &'static str| (Some(self_name), None, "/w", Some("taskpolicy"));
    assert_eq!(tab_label(None, Some(mk("build")), "/w", 0), "build");
    assert_eq!(tab_label(None, Some(mk("lint")), "/w", 0), "lint");
}

#[test]
fn pane_label_prefers_the_registered_name() {
    // x-0ba1: the navigator discriminator reads the registered name first,
    // so it no longer shows the QoS wrapper (taskpolicy) either.
    assert_eq!(
        pane_label(Some("build"), Some("x-1"), "/w", Some("taskpolicy")),
        "build"
    );
}

#[test]
fn unknown_control_verb_gets_typed_wire_refusal() {
    let request = serde_json::json!({
        "Control": {
            "proto": crate::proto::PROTO_VERSION + 1,
            "build": crate::proto::BUILD_VERSION,
            "verb": {"FutureVerb": null},
        }
    });

    match unknown_control_refusal(&request) {
        Some(ServerMsg::Err { code, msg }) => {
            assert_eq!(code, err_code::BAD_REQUEST);
            assert!(msg.contains("FutureVerb"), "{msg}");
            assert!(
                msg.contains(&format!("v{}", crate::proto::PROTO_VERSION)),
                "{msg}"
            );
        }
        other => panic!("expected typed unknown-verb refusal, got {other:?}"),
    }
}

#[test]
fn server_control_dead_pane_err_carries_the_code_and_id() {
    match dead_pane(99) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::DEAD_PANE);
            assert!(msg.contains("99"), "{msg}");
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn server_control_wait_tick_default_is_empty_and_live() {
    let t = WaitTick::default();
    assert!(!t.exited);
    assert!(t.text.is_empty());
}

fn mouse_mode() -> Modes {
    Modes {
        mouse_click: true,
        sgr_mouse: true,
        ..Modes::default()
    }
}

#[test]
fn route_mouse_passes_through_kinds_the_app_requested() {
    // AC3-HP: a click-mode app (?1000) gets wheel/press/release passthrough
    // and consumes nothing mux-side...
    for kind in [
        MouseKind::WheelUp,
        MouseKind::Press(MouseButton::Left),
        MouseKind::Release(MouseButton::Left),
    ] {
        assert_eq!(route_mouse(mouse_mode(), kind), MouseAction::Passthrough);
    }
    // ...but a drag it never requested is ignored, not forwarded and not
    // mux-interpreted (fighting the app's own handling).
    assert_eq!(
        route_mouse(mouse_mode(), MouseKind::Drag(MouseButton::Left)),
        MouseAction::Ignore
    );
    // A drag-mode app (?1002) does get the drag.
    let drag_mode = Modes {
        mouse_drag: true,
        sgr_mouse: true,
        ..Modes::default()
    };
    assert_eq!(
        route_mouse(drag_mode, MouseKind::Drag(MouseButton::Left)),
        MouseAction::Passthrough
    );
}

#[test]
fn route_mouse_interprets_when_pane_has_no_mouse_mode() {
    // US1/US2: a plain shell pane scrolls and selects mux-side.
    let plain = Modes::default();
    assert_eq!(
        route_mouse(plain, MouseKind::WheelUp),
        MouseAction::Scroll(MOUSE_WHEEL_LINES)
    );
    assert_eq!(
        route_mouse(plain, MouseKind::WheelDown),
        MouseAction::Scroll(-MOUSE_WHEEL_LINES)
    );
    assert_eq!(
        route_mouse(plain, MouseKind::Press(MouseButton::Left)),
        MouseAction::SelectStart
    );
    assert_eq!(
        route_mouse(plain, MouseKind::Drag(MouseButton::Left)),
        MouseAction::SelectUpdate
    );
    assert_eq!(
        route_mouse(plain, MouseKind::Release(MouseButton::Left)),
        MouseAction::SelectRelease
    );
    assert_eq!(
        route_mouse(plain, MouseKind::Press(MouseButton::Right)),
        MouseAction::Ignore
    );
}

#[test]
fn route_mouse_non_sgr_mouse_app_falls_through_to_interpretation() {
    // A mouse-reporting app that never negotiated SGR is not sent garbage;
    // the mux interprets instead (Domain: SGR-only passthrough).
    let legacy = Modes {
        mouse_click: true,
        sgr_mouse: false,
        ..Modes::default()
    };
    assert_eq!(
        route_mouse(legacy, MouseKind::WheelUp),
        MouseAction::Scroll(MOUSE_WHEEL_LINES)
    );
}

#[test]
fn sgr_mouse_bytes_encodes_button_coords_and_terminator() {
    // Left press at pane-local (row 4, col 9) -> SGR button 0, 1-based coords.
    let press = sgr_mouse_bytes(&MouseEvent {
        row: 4,
        col: 9,
        kind: MouseKind::Press(MouseButton::Left),
    });
    assert_eq!(press, b"\x1b[<0;10;5M");
    // Release terminates with lowercase m.
    let release = sgr_mouse_bytes(&MouseEvent {
        row: 4,
        col: 9,
        kind: MouseKind::Release(MouseButton::Left),
    });
    assert_eq!(release, b"\x1b[<0;10;5m");
    // Drag adds the motion bit (32).
    let drag = sgr_mouse_bytes(&MouseEvent {
        row: 0,
        col: 0,
        kind: MouseKind::Drag(MouseButton::Left),
    });
    assert_eq!(drag, b"\x1b[<32;1;1M");
    // Wheel up is button 64.
    let wheel = sgr_mouse_bytes(&MouseEvent {
        row: 2,
        col: 3,
        kind: MouseKind::WheelUp,
    });
    assert_eq!(wheel, b"\x1b[<64;4;3M");
}

// -- Rerun idle guard (x-38c4) ---------------------------------------------

fn agent_in(sess: &str, pane: u64, badge: Option<AgentBadge>, exited: bool) -> RegistryAgent {
    RegistryAgent {
        model: None,
        route: None,
        spawned_by_session: None,
        session_id: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        related_session_id: None,
        forked_from_session_id: None,
        name: "w".into(),
        cwd: "/w".into(),
        exited,
        dnd: false,
        badge,
        reason: None,
        mux: Some((sess.into(), pane)),
        answerable: None,
        attach_id: None,
        external: false,
        account: None,
        claude_session_uuid: None,
        log_path: None,
        updated_at: None,
        crown_level: None,
        crown_scope: None,
        liveness: if exited {
            agents_view::Liveness::Dead
        } else {
            agents_view::Liveness::Alive
        },
        harness: None,
        ..Default::default()
    }
}

#[test]
fn pane_id_floor_seeds_from_legacy_registry_refs() {
    let row = agent_in("old", 41, Some(AgentBadge::Done), true);

    assert_eq!(pane_id_floor(0, &[row]), 42);
}

fn agent(pane: u64, badge: Option<AgentBadge>, exited: bool) -> RegistryAgent {
    agent_in("main", pane, badge, exited)
}

#[test]
fn tab_close_guard_requires_positive_dead_liveness() {
    let mut alive = agent_in("main", 7, None, false);
    alive.session_id = Some("alive-worker".into());
    let mut unmeasured = agent_in("main", 8, None, true);
    unmeasured.session_id = Some("uncertain-worker".into());
    unmeasured.liveness = agents_view::Liveness::Unmeasured;
    let mut dead = agent_in("main", 9, None, true);
    dead.session_id = Some("dead-worker".into());
    let mut foreign = agent_in("other", 7, None, false);
    foreign.session_id = Some("foreign-worker".into());
    let mut identity_less = agent_in("main", 10, None, false);
    identity_less.name = "identity-less-worker".into();

    let blockers = tab_close_blockers(
        "main",
        &[7, 8, 9, 10],
        &[alive, unmeasured, dead, foreign, identity_less],
    );
    assert_eq!(blockers.len(), 3);
    assert!(blockers
        .iter()
        .any(|b| b.contains("pane 7") && b.contains("alive-worker")));
    assert!(blockers
        .iter()
        .any(|b| b.contains("pane 8") && b.contains("uncertain-worker")));
    assert!(
        !blockers.iter().any(|b| b.contains("dead-worker")),
        "positive Dead allows close"
    );
    assert!(
        !blockers.iter().any(|b| b.contains("foreign-worker")),
        "a different mux session cannot guard this tab"
    );
    assert!(
        blockers
            .iter()
            .any(|b| b.contains("pane 10") && b.contains("identity-less-worker")),
        "an identity-less live row still blocks destructive cleanup"
    );
}

#[test]
fn tab_close_unreadable_registry_refuses_before_mutation() {
    let (mut core, pane) = template_core();
    let result = core.tab_close(&PaneTarget::SquadId(1), &TabSel::Id(5), false, None);
    assert!(matches!(result, Err((code, _)) if code == err_code::REGISTRY_UNAVAILABLE));
    assert!(core.panes.contains_key(&pane));
    assert!(core.session.find_tab(5).is_some());
}

#[test]
fn tab_close_allows_positive_dead_row_and_returns_exact_receipt_data() {
    let (mut core, pane) = template_core();
    let mut dead = agent_in("test", pane, None, true);
    dead.session_id = Some("dead-worker".into());
    let result = core
        .tab_close(
            &PaneTarget::SquadId(1),
            &TabSel::Id(5),
            false,
            Some(&[dead]),
        )
        .unwrap();
    assert_eq!(result.0, 5);
    assert_eq!(result.1, vec![pane]);
    assert_eq!(result.2, RemoveOutcome::SessionEmpty);
    assert!(!core.panes.contains_key(&pane));
    assert!(!core.tab_areas.contains_key(&5));
}

#[test]
fn pane_send_refuses_when_registry_name_disagrees_with_pane_identity() {
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("hosted".into());
    let mut addressed = agent_in("sess", pane, Some(AgentBadge::Done), false);
    addressed.name = "addressed".into();
    addressed.harness_session_id = Some("target-id".into());

    match core.pane_send(
        pane,
        b"payload",
        false,
        Some("target-id"),
        Ok(vec![addressed]),
    ) {
        ServerMsg::Err { msg, .. } => {
            assert!(msg.contains("addressed"), "refusal names addressee: {msg}");
            assert!(msg.contains("hosted"), "refusal names pane host: {msg}");
        }
        other => panic!("expected identity refusal before typing, got {other:?}"),
    }
}

#[test]
fn pane_send_deduplicates_equivalent_registry_occupants() {
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("worker".into());
    let mut first = agent_in("sess", pane, Some(AgentBadge::Done), false);
    first.name = "worker".into();
    first.harness_session_id = Some("target-id".into());
    let duplicate = first.clone();

    assert!(matches!(
        core.pane_send(
            pane,
            b"payload",
            false,
            Some("target-id"),
            Ok(vec![first, duplicate]),
        ),
        ServerMsg::Ok
    ));
}

#[test]
fn pane_send_refuses_when_the_registry_carries_an_unattributable_row() {
    // AC4-ERR (x-0b40), the guard seam: the registry carries a malformed
    // row for the target pane's session; the classified read refuses, and
    // the guarded send answers TARGET_NOT_IDLE with that reason instead of
    // reading the pane as a shell and writing into a working agent. The
    // assertion is the refusal itself, never "the bytes did not land".
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    let raw = r#"{"agents":[{"name":"half","cwd":"/w","status":"live","mux":{"session":"sess"}}]}"#;
    let reason = classify_guard_registry(raw, 0).unwrap_err();
    match core.pane_send(pane, b"payload", true, None, Err(reason)) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::TARGET_NOT_IDLE);
            assert!(
                msg.contains("no readable pane binding"),
                "refusal carries the row-level cause: {msg}"
            );
        }
        other => panic!("expected guard refusal, got {other:?}"),
    }
}

#[test]
fn pane_send_identity_check_carries_the_registry_refusal_reason() {
    // The identity check consumes the same Result (x-0b40): its Err arm
    // reports the carried reason, not the old hardcoded "unreadable".
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("hosted".into());
    let reason = classify_guard_registry("not json", 0).unwrap_err();
    match core.pane_send(pane, b"payload", false, Some("target-id"), Err(reason)) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::TARGET_IDENTITY_MISMATCH);
            assert!(
                msg.contains("malformed"),
                "carried reason, not hardcoded text: {msg}"
            );
        }
        other => panic!("expected identity refusal, got {other:?}"),
    }
}

#[test]
fn pane_send_on_an_empty_registry_proceeds_like_a_shell() {
    // AC5-EDGE (x-0b40): a missing registry reads as Ok(empty) - no
    // daemon, no agents - and the guarded send proceeds exactly as today.
    // Empty stays permission; only an unreadable or unattributable read
    // refuses.
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    assert!(matches!(
        core.pane_send(pane, b"payload", true, None, Ok(Vec::new())),
        ServerMsg::Ok
    ));
}

#[test]
fn watch_only_bg_row_surfaces_while_foreign_pane_is_skipped() {
    // An `fno agents spawn --substrate bg` worker writes a paneless
    // (`mux: None`) registry row. It MUST surface as a watch-only AgentRow,
    // even alongside a pane row hosted by another mux session. The
    // session-id skip in `agent_rows()` only eats ANOTHER session's live
    // pane; it must never drop a paneless bg/headless row. Guards a future
    // membership-first rewrite of `agent_rows()` from re-dropping bg rows.
    let mut core = empty_core();
    core.session_name = "main".into();
    core.agents = vec![
        // A pane hosted by ANOTHER session -> that session's server renders
        // it; correctly skipped here.
        RegistryAgent {
            name: "foreign-pane".into(),
            cwd: "/other".into(),
            mux: Some(("other".into(), 5)),
            liveness: agents_view::Liveness::Alive,
            ..Default::default()
        },
        // A bg worker: paneless, no squad match -> watch-only orphan, and
        // it carries a claude jobId so the sideline can attach it.
        RegistryAgent {
            name: "bg-worker".into(),
            cwd: "/bg".into(),
            attach_id: Some("c19cd2c3".into()),
            liveness: agents_view::Liveness::Alive,
            ..Default::default()
        },
        // A live codex worker with a session identity but no pane or attach
        // target must project the typed branch-four recovery reason.
        RegistryAgent {
            harness_session_id: Some("codex-live-id".into()),
            name: "live-paneless".into(),
            cwd: "/live".into(),
            liveness: agents_view::Liveness::Alive,
            harness: Some("codex".into()),
            ..Default::default()
        },
    ];
    let rows = core.agent_rows();
    assert!(
        !rows.iter().any(|r| r.name == "foreign-pane"),
        "a pane hosted by another session must be skipped"
    );
    let bg = rows
        .iter()
        .find(|r| r.name == "bg-worker")
        .expect("a paneless bg row must surface as a watch-only row");
    assert_eq!(
        bg.squad, None,
        "an unmatched bg row is an orphan (squad None)"
    );
    assert_eq!(bg.pane_id, None, "a watch-only row has no pane");
    assert!(!bg.exited);
    assert_eq!(
        bg.attach_id.as_deref(),
        Some("c19cd2c3"),
        "the claude jobId must carry through so the sideline can attach it"
    );
    assert_eq!(bg.no_pane_reason, None, "attachable rows carry no reason");
    let live = rows
        .iter()
        .find(|r| r.name == "live-paneless")
        .expect("the live paneless row must surface");
    assert_eq!(
        live.no_pane_reason,
        Some(AgentNoPaneReason::LivePaneless),
        "registry truth projects the typed live-paneless reason"
    );
}

#[test]
fn bare_pane_row_carries_its_own_activity_and_age() {
    // (x-d401, x-9d03) A bare pane with no registry row can still be a
    // full agent running a real workload - not-in-registry is not
    // is-a-shell. The row must carry the pane's own OSC 133 reading and a
    // real last_activity_age_s from the drain-path stamp, never the
    // badge-None-means-idle fold that rendered four working panes and one
    // idle shell as the same circle.
    let mut core = empty_core();
    core.session_name = "main".into();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(2, 4, "/w").expect("pane");
    core.session.add_squad(
        1,
        vec!["/w".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(pid),
            focus: pid,
        },
    );
    core.agents = vec![];
    // Feed an open command block (OSC 133 A then C, no D): Running.
    let (tx, mut rx) = mpsc::channel::<(u64, PaneChunk)>(8);
    tx.try_send((
        pid,
        PaneChunk::Output(b"\x1b]133;A\x07\x1b]133;C\x07workload".to_vec()),
    ))
    .unwrap();
    drop(tx);
    let mut first_out = HashSet::new();
    drain_pty_output(&mut core, &mut rx, None, &mut first_out);
    let rows = core.agent_rows();
    let bare = rows.iter().find(|r| r.pane_id == Some(pid)).unwrap();
    assert_eq!(
        bare.pane_activity,
        Some(vt::ShellActivity::Running),
        "a bare pane running a command must report Running, not a blind idle"
    );
    assert!(
        bare.last_activity_age_s.is_some(),
        "a bare pane must report a real activity age from the drain stamp"
    );
}

#[test]
fn agent_rows_tombstoned_member_decorates_its_row_instead_of_minting_one() {
    // AC3-HP: one registry row + one tombstoned member that
    // joins it -> exactly ONE row, the registry row, decorated dimmed +
    // dismissable under the member's squad. The old synthesized
    // `cc-<id>` row was the ghost-minter this rewrite removes.
    let mut core = empty_core();
    core.session_name = "main".into();
    let mut row = exited_claude_row("cc-decorated", None);
    row.exited = true;
    row.attach_id = Some("c0ffee00".into());
    core.agents = vec![row];
    core.session.add_squad(
        1,
        vec!["/repo".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(10),
            focus: 10,
        },
    );
    core.squad_members.insert(
        1,
        vec![crate::squad_store::StoredMember {
            attach_id: "c0ffee00".into(),
            tombstone: true,
            tombstone_reason: Some("member pane died".into()),
            detached: false,
            tab_name: None,
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    let rows = core.agent_rows();
    let matches: Vec<&AgentRow> = rows.iter().filter(|r| r.name == "cc-decorated").collect();
    assert_eq!(matches.len(), 1, "exactly one row: {rows:?}");
    assert!(
        matches[0].tombstone,
        "the row is decorated as the tombstone"
    );
    assert_eq!(
        matches[0].attach_id.as_deref(),
        Some("c0ffee00"),
        "the dismiss affordance carries the attach target"
    );
}

#[test]
fn agent_rows_never_renders_a_member_that_joins_no_row() {
    // AC3-EDGE: a tombstoned member joining NO registry row by
    // either key renders nothing. It is a stale member; the member-retirement
    // path removes it at restore. Never a synthesized ghost.
    let mut core = empty_core();
    core.session_name = "main".into();
    core.agents = vec![];
    let _ = core.session.add_squad(
        1,
        vec!["/repo".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(10),
            focus: 10,
        },
    );
    core.squad_members.insert(
        1,
        vec![crate::squad_store::StoredMember {
            attach_id: "d15ea5e".into(),
            tombstone: true,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    let rows = core.agent_rows();
    assert!(
        rows.iter()
            .all(|r| r.name != "d15ea5e" && !r.name.starts_with("cc-")),
        "no row for an unjoined member: {rows:?}"
    );
}

#[test]
fn agent_rows_never_dims_a_live_row_behind_a_stale_tombstone() {
    // The liveness kill criterion : a tombstoned member whose
    // registry row is LIVE must never dim that row. The stale tombstone
    // is invisible here (restore lifts it with a notice); the live row
    // renders alive.
    let mut core = empty_core();
    core.session_name = "main".into();
    let mut row = exited_claude_row("cc-live-under-tombstone", None);
    row.exited = false;
    row.attach_id = Some("beeff00d".into());
    core.agents = vec![row];
    let _ = core.session.add_squad(
        1,
        vec!["/repo".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(10),
            focus: 10,
        },
    );
    core.squad_members.insert(
        1,
        vec![crate::squad_store::StoredMember {
            attach_id: "beeff00d".into(),
            tombstone: true,
            tombstone_reason: Some("member pane death".into()),
            detached: false,
            tab_name: None,
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    let rows = core.agent_rows();
    let live_row = rows
        .iter()
        .find(|r| r.name == "cc-live-under-tombstone")
        .unwrap();
    assert!(
        !live_row.tombstone,
        "a live row is never dimmed behind a stale tombstone"
    );
    assert!(!live_row.exited, "the live row renders alive");
}

#[test]
fn agent_rows_decorates_the_joined_generation_not_a_name_twin() {
    // Review round 1, P1: exited and live generations can share a display
    // name. The decoration must match the produced row by the same
    // session identity the join used, and must never dim the live twin.
    let mut core = empty_core();
    core.session_name = "main".into();
    let mut exited = exited_claude_row("gen-twin", None);
    exited.exited = true;
    exited.attach_id = Some("c0ffee01".into());
    exited.harness_session_id = Some("sess-old".into());
    let mut live = exited_claude_row("gen-twin", None);
    live.exited = false;
    live.attach_id = Some("c0ffee02".into());
    live.harness_session_id = Some("sess-new".into());
    core.agents = vec![exited, live];
    core.session.add_squad(
        1,
        vec!["/repo".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(10),
            focus: 10,
        },
    );
    core.squad_members.insert(
        1,
        vec![crate::squad_store::StoredMember {
            attach_id: "c0ffee01".into(),
            tombstone: true,
            tombstone_reason: Some("member pane died".into()),
            detached: false,
            tab_name: None,
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: Some("sess-old".into()),
            pane_id: None,
        }],
    );
    let rows = core.agent_rows();
    let dead_row = rows
        .iter()
        .find(|r| r.harness_session_id.as_deref() == Some("sess-old"))
        .expect("the exited generation renders");
    assert!(
        dead_row.tombstone,
        "the joined exited generation is decorated"
    );
    assert_eq!(dead_row.attach_id.as_deref(), Some("c0ffee01"));
    assert_eq!(dead_row.squad, Some(1), "decoration keeps the stored squad");
    let live_row = rows
        .iter()
        .find(|r| r.harness_session_id.as_deref() == Some("sess-new"))
        .expect("the live generation renders");
    assert!(
        !live_row.tombstone,
        "the live name-twin is never dimmed behind the stale tombstone"
    );
}

#[test]
fn agent_rows_match_pane_hosted_by_membership_and_watch_only_by_origins() {
    // Change #5. A pane-hosted agent's row renders under the squad its pane
    // lives in (membership), REGARDLESS of the pane's cwd (AC1-HP). A
    // watch-only row falls back to cwd, now against ANY origin exact-or-child
    // (AC2-EDGE), so a multi-origin squad claims a worker under origins[1].
    let mut core = empty_core();
    core.session_name = "main".into();
    // Squad 1: origin far from the pane's registry cwd ("/w" via agent_in).
    core.session.add_squad(
        1,
        vec!["/origins/one".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(42),
            focus: 42,
        },
    );
    // Squad 2: two origins; a watch-only row's cwd is a child of the SECOND.
    core.session.add_squad(
        2,
        vec!["/grp/frontend".into(), "/grp/backend".into()],
        Some("stack".into()),
        Tab {
            name: None,
            id: 2,
            root: Node::Leaf(50),
            focus: 50,
        },
    );
    core.agents = vec![
        // Pane-hosted in THIS session at pane 42 (which lives in squad 1),
        // but its registry cwd "/w" matches no origin - membership must win.
        agent_in("main", 42, None, false),
        RegistryAgent {
            name: "watcher".into(),
            cwd: "/grp/backend/sub/dir".into(),
            liveness: agents_view::Liveness::Alive,
            ..Default::default()
        },
    ];
    let rows = core.agent_rows();
    let hosted = rows.iter().find(|r| r.pane_id == Some(42)).unwrap();
    assert_eq!(
        hosted.squad,
        Some(1),
        "a pane-hosted agent matches by membership even when its cwd matches no origin"
    );
    let watcher = rows.iter().find(|r| r.name == "watcher").unwrap();
    assert_eq!(
        watcher.squad,
        Some(2),
        "a watch-only row matches a squad via a child of origins[1]"
    );
}

#[test]
fn agent_rows_pane_dead_corroborates_over_an_unmeasured_registry_liveness() {
    // x-9de7: pane-exit fact beats any badge. `empty_core()` has no live
    // pane, so the matched row's pane is always confirmed gone
    // (`pane_dead`) here - itself positive corroboration of death, even
    // when the registry row's own liveness read is Unmeasured. `unmeasured`
    // must stay false: the join layer has independent proof, so it must
    // never downgrade a corroborated dead row to the softer glyph.
    let mut core = empty_core();
    core.session_name = "main".into();
    core.session.add_squad(
        1,
        vec!["/w".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(42),
            focus: 42,
        },
    );
    core.agents = vec![RegistryAgent {
        liveness: agents_view::Liveness::Unmeasured,
        ..agent_in("main", 42, None, false)
    }];
    let rows = core.agent_rows();
    let hosted = rows.iter().find(|r| r.pane_id == Some(42)).unwrap();
    assert!(hosted.exited, "a gone pane still forces exited");
    assert!(
        !hosted.unmeasured,
        "a confirmed-gone pane corroborates death outright, regardless of \
             the registry row's own Unmeasured liveness read"
    );
}

#[test]
fn agent_rows_watch_only_appendix_carries_unmeasured_from_registry_liveness() {
    // x-9de7: the paneless join has no pane fact; `unmeasured` passes the registry read.
    let paneless = |name: &str, liveness: agents_view::Liveness| RegistryAgent {
        name: name.into(),
        cwd: "/w".into(),
        exited: true,
        liveness,
        ..Default::default()
    };
    let mut core = empty_core();
    core.agents = vec![
        paneless("uncorroborated", agents_view::Liveness::Unmeasured),
        paneless("confirmed-dead", agents_view::Liveness::Dead),
    ];
    let rows = core.agent_rows();
    let unmeasured = rows.iter().find(|r| r.name == "uncorroborated").unwrap();
    assert!(unmeasured.exited);
    assert!(
        unmeasured.unmeasured,
        "an uncorroborated terminal row renders the softer, dim glyph"
    );
    let dead = rows.iter().find(|r| r.name == "confirmed-dead").unwrap();
    assert!(dead.exited);
    assert!(
        !dead.unmeasured,
        "a corroborated-dead row keeps the confirmed exit glyph"
    );
}

#[test]
fn external_synthesized_row_passes_the_attach_catalog_gate() {
    // AC2-HP: a roster-synthesized foreign row (mux None, !exited, attach_id
    // set, external true) is attachable through the EXISTING catalog gate,
    // with no new spawn path. An exited or pane-hosted row is refused, like
    // any non-attachable registry row.
    let mut core = empty_core();
    core.agents = vec![
        RegistryAgent {
            name: "think-x-9999".into(),
            cwd: "/w".into(),
            attach_id: Some("ab12cd34".into()),
            external: true,
            liveness: agents_view::Liveness::Alive,
            ..Default::default()
        },
        // An exited external row (dead pane beat the upgrade): not attachable.
        RegistryAgent {
            name: "dead-ext".into(),
            cwd: "/w".into(),
            exited: true,
            attach_id: Some("ffffffff".into()),
            external: true,
            liveness: agents_view::Liveness::Dead,
            ..Default::default()
        },
    ];
    assert!(
        core.attachable_agent("ab12cd34"),
        "a live foreign row is attachable"
    );
    assert!(
        !core.attachable_agent("ffffffff"),
        "an exited foreign row is refused"
    );
    assert!(
        !core.attachable_agent("deadbeef"),
        "an id naming no surfaced row is refused"
    );
}

#[test]
fn dead_pane_beats_roster_liveness_upgrade() {
    // AC2-EDGE: an upgraded (roster-present, external) registry row whose
    // mux ref points to a dead pane in THIS session renders exited - the
    // pane fact stays senior over the merge's un-exit.
    let mut core = empty_core();
    core.session_name = "main".into();
    core.session.add_squad(
        1,
        vec!["/w".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    // merge_rows would have set exited=false + external=true on this row,
    // but its mux pane (77) is absent from core.panes -> pane_dead.
    core.agents = vec![RegistryAgent {
        name: "upgraded".into(),
        cwd: "/w".into(),
        liveness: agents_view::Liveness::Alive,
        mux: Some(("main".into(), 77)),
        attach_id: Some("ab12cd34".into()),
        external: true,
        ..Default::default()
    }];
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "upgraded").unwrap();
    assert!(row.exited, "a dead pane forces exited despite the upgrade");
    assert!(row.external, "provenance still rides through");
    assert_eq!(row.attach_id, None, "an exited row drops its attach target");
}

#[test]
fn new_squad_rejects_a_blank_name_and_creates_nothing() {
    // Change #2 / AC1-ERR: a whitespace-only name is refused fail-closed -
    // no squad, no pane. PTY-free: the reject returns before any spawn.
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.clients.push(client(1, 5, (24, 80), false));
    let flow = core.command(
        1,
        Command::NewSquad {
            name: "   ".into(),
            origin: None,
        },
    );
    assert!(matches!(flow, Flow::Continue));
    assert_eq!(core.session.squads.len(), 1, "blank name creates no squad");
    assert!(core.panes.is_empty(), "blank name spawns no pane");
}

#[test]
fn rename_tab_round_trips_and_blank_clears() {
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.clients.push(client(1, 5, (24, 80), false));
    // The rename stores the trimmed name (AC2-HP's server half)...
    core.command(
        1,
        Command::RenameTab {
            tab: 5,
            name: "  debug ".into(),
        },
    );
    assert_eq!(
        core.session.squads[0].tabs[0].name.as_deref(),
        Some("debug")
    );
    // ...and a blank rename CLEARS it back to the derived label (AC3-HP,
    // Locked 2) - a clear, never an error. (Re-register the sender: the
    // test client's dropped receiver made the rename's own layout push
    // reap it, exactly like a real disconnect.)
    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameTab {
            tab: 5,
            name: "   ".into(),
        },
    );
    assert_eq!(core.session.squads[0].tabs[0].name, None);
}

#[test]
fn rename_tab_stale_id_is_refused_without_mutation() {
    // AC1-ERR: a RenameTab naming a closed tab mutates nothing.
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: Some("keep".into()),
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.clients.push(client(1, 5, (24, 80), false));
    let flow = core.command(
        1,
        Command::RenameTab {
            tab: 999,
            name: "x".into(),
        },
    );
    assert!(matches!(flow, Flow::Continue));
    assert_eq!(
        core.session.squads[0].tabs[0].name.as_deref(),
        Some("keep"),
        "a stale id must not touch any live tab"
    );
}

#[test]
fn rename_tab_sanitizes_hostile_wire_names() {
    // AC2-ERR: control chars are stripped and the stored name is capped -
    // the wire is not the overlay, so the server owns the guarantee.
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameTab {
            tab: 5,
            name: format!("\x1b[31m{}", "a".repeat(200)),
        },
    );
    let stored = core.session.squads[0].tabs[0].name.clone().unwrap();
    assert_eq!(stored, format!("[31m{}", "a".repeat(MAX_TAB_NAME - 4)));
}

// -- x-96e8 squad management verbs ----------------------------------

pub(super) fn leaf_tab(id: TabId, pane: u64) -> Tab {
    Tab {
        name: None,
        id,
        root: Node::Leaf(pane),
        focus: pane,
    }
}

// -- x-3e38 pane placement (target resolution + atomic commit) ------

#[test]
fn resolve_placement_target_current_route_passes_through() {
    // CurrentRoute yields the caller's default (a cwd/owner squad, or None
    // when a squad must still be born) with no lookup.
    let core = empty_core();
    assert_eq!(
        core.resolve_placement_target(&PaneTarget::CurrentRoute, Some(7))
            .unwrap(),
        Some(7)
    );
    assert_eq!(
        core.resolve_placement_target(&PaneTarget::CurrentRoute, None)
            .unwrap(),
        None
    );
}

#[test]
fn resolve_placement_target_explicit_hit_miss_and_id() {
    // AC2-HP + AC4: an exact name/id resolves; a missing one fails closed.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], Some("review".into()), leaf_tab(5, 1));
    core.session
        .add_squad(2, vec!["/repos/default".into()], None, leaf_tab(6, 2));
    assert_eq!(
        core.resolve_placement_target(&PaneTarget::SquadName(" review ".into()), None)
            .unwrap(),
        Some(1),
        "name is trimmed before match"
    );
    assert!(core
        .resolve_placement_target(&PaneTarget::SquadName("ghost".into()), None)
        .is_err());
    assert_eq!(
        core.resolve_placement_target(&PaneTarget::SquadName("default".into()), None)
            .unwrap(),
        Some(2),
        "derived display names are targetable"
    );
    assert_eq!(
        core.resolve_placement_target(&PaneTarget::SquadId(1), None)
            .unwrap(),
        Some(1)
    );
    assert!(core
        .resolve_placement_target(&PaneTarget::SquadId(99), None)
        .is_err());
}

#[test]
fn place_spawned_pane_new_tab_then_directional_split() {
    // AC1-HP + AC2-HP: omitted split mints a new tab; a direction inserts
    // beside the destination's active-tab focus in that same tab.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));

    let (sid, _tid, _) = core.place_spawned_pane(Some(1), "/a", 2, None).unwrap();
    assert_eq!(sid, 1);
    assert_eq!(
        core.session.squad(1).unwrap().tabs.len(),
        2,
        "omitted split pushes a new tab"
    );

    let tabs_before = core.session.squad(1).unwrap().tabs.len();
    let (_sid, tid, _) = core
        .place_spawned_pane(Some(1), "/a", 3, Some(Dir::Right))
        .unwrap();
    assert_eq!(
        core.session.squad(1).unwrap().tabs.len(),
        tabs_before,
        "a directional split adds no tab"
    );
    let tab = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == tid)
        .unwrap();
    assert_eq!(
        tree::leaves(&tab.root),
        vec![1, 3],
        "right places the new pane after the focused leaf"
    );
    assert_eq!(tab.focus, 3, "the new pane takes focus");
}

#[test]
fn place_spawned_pane_current_route_miss_births_first_tab() {
    // AC6-EDGE: no squad yet + a split request -> the squad is born from the
    // route with the pane as its lone first tab (split collapses).
    let mut core = empty_core();
    let (sid, tid, _) = core
        .place_spawned_pane(None, "/fresh", 9, Some(Dir::Left))
        .unwrap();
    let sq = core.session.squad(sid).unwrap();
    assert_eq!(sq.tabs.len(), 1);
    assert_eq!(sq.tabs[0].id, tid);
    assert_eq!(tree::leaves(&sq.tabs[0].root), vec![9]);
    assert_eq!(sq.origins, vec!["/fresh".to_string()]);
}

#[test]
fn place_spawned_pane_min_size_refusal_falls_back_to_new_tab() {
    // AC3-FR (x-9f75): a split that would violate minimum size no longer reaps - the pane lands as a new
    // tab in the same squad, the crowded tab is untouched, and the caller is signaled to notice.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    // 8 cols cannot hold two MIN_COLS(8)-wide halves -> horizontal refusal.
    core.tab_areas.insert(5, (40, 8));
    let before = core.session.squad(1).unwrap().tabs[0].root.clone();

    let (_sid, tid, fell_back) = core
        .place_spawned_pane(Some(1), "/a", 3, Some(Dir::Right))
        .unwrap();
    assert!(fell_back, "the split refusal signals a fallback");
    assert_eq!(
        core.session.squad(1).unwrap().tabs[0].root,
        before,
        "the crowded tab is untouched"
    );
    let squad = core.session.squad(1).unwrap();
    assert_eq!(squad.tabs.len(), 2, "the pane landed as a new tab");
    assert_eq!(
        squad.tabs.iter().find(|t| t.id == tid).unwrap().root,
        Node::Leaf(3)
    );
}

// ---- v41 (x-d865) layout script API server ops ----------------------

/// squad 1: tab 10 = panes [1,2] (H-split); tab 20 "bee" = pane [3].
fn two_tab_core() -> Core {
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/a".into()],
        None,
        Tab {
            name: None,
            id: 10,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        },
    );
    core.session.squad_mut(1).unwrap().tabs.push(Tab {
        name: Some("bee".into()),
        id: 20,
        root: Node::Leaf(3),
        focus: 3,
    });
    core.tab_areas.insert(10, (24, 80));
    core.tab_areas.insert(20, (24, 80));
    core.next_pane_id = 100;
    core
}

// ---- v42 (x-c4d4) declarative layout templates -----------------------

/// A registry row binding fno id `sess_id` to live `pane` in the test
/// session ("test"), so `resolve_local_pane` can find it.
fn bound_agent(sess_id: &str, pane: u64) -> RegistryAgent {
    let mut a = agent_in("test", pane, None, false);
    a.session_id = Some(sess_id.into());
    a
}

/// A one-tab squad (id 1) whose single tab (id 5) holds one real spawned
/// shell pane, plus a scratch shell so template shell slots can spawn.
fn template_core() -> (Core, u64) {
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    core.next_pane_id = 100;
    let p = core.spawn_pane(24, 80, "/a").unwrap();
    core.session
        .add_squad(1, vec!["/a".into()], Some("sq".into()), leaf_tab(5, p));
    core.tab_areas.insert(5, (24, 80));
    (core, p)
}

fn shell_spec(t: TemplateName, k: usize) -> LayoutSpec {
    LayoutSpec {
        template: t,
        slots: (0..k).map(|_| SlotBinding::Shell).collect(),
    }
}

#[test]
fn apply_realizes_the_template_topology_over_shells() {
    // AC1/AC2 shape: main-left with 4 shell slots -> H[ leaf, V[leaf,leaf,leaf] ].
    let (mut core, _p) = template_core();
    let results = core
        .apply_spec(
            1,
            &TabSel::Id(5),
            &shell_spec(TemplateName::MainLeft, 4),
            false,
        )
        .unwrap();
    assert_eq!(results.len(), 4);
    assert!(results
        .iter()
        .all(|r| r.outcome == SlotOutcome::Shell && r.pane_id.is_some()));
    let root = &core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 5)
        .unwrap()
        .root;
    match root {
        Node::Branch {
            axis: Axis::Horizontal,
            children,
        } => {
            assert!(
                matches!(children[0].1, Node::Leaf(_)),
                "slot 0 is the main leaf"
            );
            assert!(
                matches!(&children[1].1, Node::Branch { axis: Axis::Vertical, children } if children.len() == 3),
                "the rest stack vertically"
            );
        }
        other => panic!("expected H[leaf, V[..]], got {other:?}"),
    }
    assert_eq!(tree::leaves(root).len(), 4, "four live panes");
}

#[test]
fn reapply_same_spec_is_byte_identical_and_reuses_panes() {
    // AC3: re-applying the same spec spawns nothing, closes nothing, and the
    // tree comes back byte-identical (the FIFO spare-drain contract).
    let (mut core, _p) = template_core();
    let spec = shell_spec(TemplateName::MainLeft, 4);
    core.apply_spec(1, &TabSel::Id(5), &spec, false).unwrap();
    let root1 = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 5)
        .unwrap()
        .root
        .clone();
    let panes1 = core.panes.len();

    let results = core.apply_spec(1, &TabSel::Id(5), &spec, false).unwrap();
    let root2 = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 5)
        .unwrap()
        .root
        .clone();
    assert_eq!(root1, root2, "re-apply is byte-identical");
    assert_eq!(core.panes.len(), panes1, "no pane spawned or reaped");
    assert!(results.iter().all(|r| r.outcome == SlotOutcome::Shell));
}

#[test]
fn bound_fno_slot_reuses_its_live_pane_and_empties_its_source_tab() {
    // AC1 core: a live session S1 in its own tab; apply main-left binding
    // slot 0 to it -> S1's pane becomes the left main, its source tab empties
    // and is removed, and no pane is spawned for that slot.
    let (mut core, p1) = template_core(); // p1 lives in tab 5
    core.agents = vec![bound_agent("S1", p1)];
    // A second, empty target tab to apply into.
    let tid = core.create_tab_in(1, Some("grid".into())).unwrap();
    core.tab_areas.insert(tid, (24, 80));

    let spec = LayoutSpec {
        template: TemplateName::MainLeft,
        slots: vec![
            SlotBinding::Fno("S1".into()),
            SlotBinding::Shell,
            SlotBinding::Shell,
            SlotBinding::Shell,
        ],
    };
    let results = core.apply_spec(1, &TabSel::Id(tid), &spec, false).unwrap();
    assert_eq!(results[0].outcome, SlotOutcome::Reused);
    assert_eq!(results[0].pane_id, Some(p1), "S1's pane, reused in place");
    assert!(results[1..].iter().all(|r| r.outcome == SlotOutcome::Shell));
    // Source tab 5 emptied (its only pane relocated) -> removed.
    assert!(
        core.session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .all(|t| t.id != 5),
        "source tab removed"
    );
    let target = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == tid)
        .unwrap();
    assert_eq!(
        tree::leaves(&target.root)[0],
        p1,
        "p1 is the main-left leaf"
    );
}

#[test]
fn dead_binding_reconciles_to_a_shell_never_a_duplicate() {
    // AC4: slot 1 bound to S2; S2 exits; re-apply -> slot 1 Unbound (shell),
    // no second S2 pane, the surviving bound pane keeps running.
    let (mut core, p1) = template_core();
    let p2 = core.spawn_pane(24, 80, "/a").unwrap();
    core.agents = vec![bound_agent("S1", p1), bound_agent("S2", p2)];
    let spec = LayoutSpec {
        template: TemplateName::MainLeft,
        slots: vec![
            SlotBinding::Fno("S1".into()),
            SlotBinding::Fno("S2".into()),
            SlotBinding::Shell,
            SlotBinding::Shell,
        ],
    };
    core.apply_spec(1, &TabSel::Id(5), &spec, false).unwrap();

    // S2 exits: drop its registry row and reap its pane.
    core.agents
        .retain(|a| a.session_id.as_deref() != Some("S2"));
    core.reap_pane(p2);

    let results = core.apply_spec(1, &TabSel::Id(5), &spec, false).unwrap();
    assert_eq!(
        results[1].outcome,
        SlotOutcome::Unbound,
        "dead S2 slot is a reported shell"
    );
    assert!(results[1].pane_id.is_some(), "the unbound slot got a shell");
    assert!(!core.panes.contains_key(&p2), "no resurrected S2 pane");
    assert!(
        core.panes.contains_key(&p1),
        "the surviving bound pane keeps running"
    );
    assert_eq!(results[0].pane_id, Some(p1));
}

#[test]
fn reshape_never_kills_the_live_bound_pane() {
    // AC5: a grid-2x2 with a bound slot 0; reshape to main-left keeps that
    // pane's id (its PTY untouched, only relocated).
    let (mut core, p1) = template_core();
    core.agents = vec![bound_agent("S1", p1)];
    let grid = LayoutSpec {
        template: TemplateName::Grid2x2,
        slots: vec![
            SlotBinding::Fno("S1".into()),
            SlotBinding::Shell,
            SlotBinding::Shell,
            SlotBinding::Shell,
        ],
    };
    core.apply_spec(1, &TabSel::Id(5), &grid, false).unwrap();
    assert!(core.panes.contains_key(&p1));

    let main_left = LayoutSpec {
        template: TemplateName::MainLeft,
        slots: vec![
            SlotBinding::Fno("S1".into()),
            SlotBinding::Shell,
            SlotBinding::Shell,
        ],
    };
    let results = core
        .apply_spec(1, &TabSel::Id(5), &main_left, false)
        .unwrap();
    assert_eq!(
        results[0].pane_id,
        Some(p1),
        "the bound pane survives the reshape"
    );
    assert!(core.panes.contains_key(&p1));
    let root = &core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 5)
        .unwrap()
        .root;
    assert_eq!(tree::leaves(root)[0], p1, "and lands as the main-left leaf");
}

#[test]
fn unfittable_template_is_refused_atomically() {
    // AC6: a tab too small to tile grid-2x2 -> TEMPLATE_UNFITTABLE, tab
    // unchanged (the pre-mutation atomic refuse).
    let (mut core, _p) = template_core();
    core.tab_areas.insert(5, (3, 8)); // far too small for four tiles
    let before = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 5)
        .unwrap()
        .root
        .clone();
    let err = core
        .apply_spec(
            1,
            &TabSel::Id(5),
            &shell_spec(TemplateName::Grid2x2, 4),
            false,
        )
        .unwrap_err();
    assert_eq!(err.0, err_code::TEMPLATE_UNFITTABLE);
    let after = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 5)
        .unwrap()
        .root
        .clone();
    assert_eq!(before, after, "the tab is left completely unchanged");
}

#[test]
fn arity_mismatch_is_refused_before_any_mutation() {
    // AC7: grid-2x2 with three slots -> TEMPLATE_ARITY, no mutation.
    let (mut core, _p) = template_core();
    let panes_before = core.panes.len();
    let err = core
        .apply_spec(
            1,
            &TabSel::Id(5),
            &shell_spec(TemplateName::Grid2x2, 3),
            false,
        )
        .unwrap_err();
    assert_eq!(err.0, err_code::TEMPLATE_ARITY);
    assert_eq!(
        core.panes.len(),
        panes_before,
        "arity refuse spawns nothing"
    );
}

#[test]
fn dropping_a_bound_slot_rehomes_the_live_pane_never_reaps_it() {
    // Codex P1 regression: when a re-apply's slots are all bound (no shell
    // slot to absorb it), a dropped bound session's pane must NOT be reaped -
    // it is broken into its own tab, still running (the never-kill invariant).
    let (mut core, p1) = template_core();
    let p2 = core.spawn_pane(24, 80, "/a").unwrap();
    let p3 = core.spawn_pane(24, 80, "/a").unwrap();
    core.agents = vec![
        bound_agent("S1", p1),
        bound_agent("S2", p2),
        bound_agent("S3", p3),
    ];
    let thirds = LayoutSpec {
        template: TemplateName::RowThirds,
        slots: vec![
            SlotBinding::Fno("S1".into()),
            SlotBinding::Fno("S2".into()),
            SlotBinding::Fno("S3".into()),
        ],
    };
    core.apply_spec(1, &TabSel::Id(5), &thirds, false).unwrap();

    // Re-apply main-left binding ONLY S2 and S3 (both slots bound, no shell):
    // S1 is dropped with nowhere to be reused.
    let two = LayoutSpec {
        template: TemplateName::MainLeft,
        slots: vec![SlotBinding::Fno("S2".into()), SlotBinding::Fno("S3".into())],
    };
    core.apply_spec(1, &TabSel::Id(5), &two, false).unwrap();

    assert!(
        core.panes.contains_key(&p1),
        "S1's live pane is never reaped"
    );
    let tab5 = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 5)
        .unwrap();
    assert!(
        !tree::leaves(&tab5.root).contains(&p1),
        "S1 left the template tab"
    );
    let hosted = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .any(|t| tree::leaves(&t.root).contains(&p1));
    assert!(hosted, "S1's pane lives on in a rehomed tab");
}

#[test]
fn recycle_never_absorbs_a_live_leftover_into_a_shell_slot() {
    // x-3f39: the reap path was guarded in b7cff6d0, but the recycle-as-shell
    // path was not. A re-apply whose new spec carries a Shell slot must NOT
    // hand a dropped live agent's pane to it (step 7); the live leftover
    // rehomes to its own tab and the Shell slot gets a genuinely fresh shell.
    let (mut core, p1) = template_core();
    let p2 = core.spawn_pane(24, 80, "/a").unwrap();
    let p3 = core.spawn_pane(24, 80, "/a").unwrap();
    core.agents = vec![
        bound_agent("S1", p1),
        bound_agent("S2", p2),
        bound_agent("S3", p3),
    ];
    let thirds = LayoutSpec {
        template: TemplateName::RowThirds,
        slots: vec![
            SlotBinding::Fno("S1".into()),
            SlotBinding::Fno("S2".into()),
            SlotBinding::Fno("S3".into()),
        ],
    };
    core.apply_spec(1, &TabSel::Id(5), &thirds, false).unwrap();

    // Re-apply main-left binding S2 + a SHELL slot: S1 and S3 are dropped
    // live leftovers and the shell slot is a recycle target.
    let spec = LayoutSpec {
        template: TemplateName::MainLeft,
        slots: vec![SlotBinding::Fno("S2".into()), SlotBinding::Shell],
    };
    let results = core.apply_spec(1, &TabSel::Id(5), &spec, false).unwrap();

    // Slot 0 reuses S2; slot 1 is a genuine fresh shell, never a live pane.
    assert_eq!(results[0].pane_id, Some(p2), "slot 0 reuses S2");
    assert_eq!(results[1].outcome, SlotOutcome::Shell);
    let shell_pane = results[1].pane_id.expect("shell slot filled");
    assert_ne!(shell_pane, p1, "shell slot must not be S1's live pane");
    assert_ne!(shell_pane, p3, "shell slot must not be S3's live pane");

    // AC1-FR: both live leftovers survive (never reaped) and each rehomes to
    // a tab of its own, out of the template tab.
    assert!(
        core.panes.contains_key(&p1),
        "S1's live pane is never reaped"
    );
    assert!(
        core.panes.contains_key(&p3),
        "S3's live pane is never reaped"
    );
    let sq = core.session.squad(1).unwrap();
    let tab5_leaves = tree::leaves(&sq.tabs.iter().find(|t| t.id == 5).unwrap().root);
    assert!(
        !tab5_leaves.contains(&p1) && !tab5_leaves.contains(&p3),
        "dropped live panes left the template tab"
    );
    for p in [p1, p3] {
        let hosted = sq
            .tabs
            .iter()
            .any(|t| t.id != 5 && tree::leaves(&t.root).contains(&p));
        assert!(hosted, "live leftover {p} lives on in its own rehomed tab");
    }
}

#[test]
fn idempotent_reapply_recycles_a_genuine_shell_not_a_new_pane() {
    // AC3-EDGE: the step-6 partition must not disturb genuine-shell
    // recycling. A real shell is not live-bound, so it stays in the recycle
    // pool and a re-apply of the same spec reuses it FIFO - no new spawn, no
    // rehome tab.
    let (mut core, p1) = template_core();
    core.agents = vec![bound_agent("S1", p1)];
    let spec = LayoutSpec {
        template: TemplateName::MainLeft,
        slots: vec![SlotBinding::Fno("S1".into()), SlotBinding::Shell],
    };
    let r1 = core.apply_spec(1, &TabSel::Id(5), &spec, false).unwrap();
    let shell1 = r1[1].pane_id.expect("shell filled");
    let tabs_after_first = core.session.squad(1).unwrap().tabs.len();

    let r2 = core.apply_spec(1, &TabSel::Id(5), &spec, false).unwrap();
    assert_eq!(r2[0].pane_id, Some(p1), "S1 still reused in slot 0");
    assert_eq!(
        r2[1].pane_id,
        Some(shell1),
        "the same genuine shell recycles, not a new spawn"
    );
    assert_eq!(
        core.session.squad(1).unwrap().tabs.len(),
        tabs_after_first,
        "no rehome tab created for a genuine shell"
    );
}

#[test]
fn overlay_rename_repersists_template_spec_under_new_name() {
    // x-cde1 AC1-HP: Command::RenameTab (the interactive overlay path, not
    // the ControlVerb::TabRename wire API) must re-persist a template tab's
    // spec so restore finds it under the NEW name. persist_squad alone
    // preserves tab_specs byte-for-byte, keeping the stale old key.
    let _s = StoreScratch::new("cde1-rename");
    let (mut core, _p) = template_core();
    core.clients.push(client(1, 5, (24, 80), false));
    core.session.squad_mut(1).unwrap().tabs[0].name = Some("grid".into());
    core.apply_spec(
        1,
        &TabSel::Id(5),
        &shell_spec(TemplateName::MainLeft, 2),
        false,
    )
    .unwrap();
    let loaded = crate::squad_store::load();
    let specs = &loaded
        .squads
        .iter()
        .find(|s| s.name == "sq")
        .unwrap()
        .tab_specs;
    assert_eq!(specs.len(), 1);
    assert_eq!(
        specs[0].tab_name, "grid",
        "persisted under the original name"
    );

    // apply_spec's layout push reaps the test client (dropped receiver);
    // re-register it so the rename command has a live sender to act on.
    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameTab {
            tab: 5,
            name: "reviews".into(),
        },
    );

    let loaded = crate::squad_store::load();
    let specs = &loaded
        .squads
        .iter()
        .find(|s| s.name == "sq")
        .unwrap()
        .tab_specs;
    assert_eq!(specs.len(), 1, "still exactly one template spec");
    assert_eq!(specs[0].tab_name, "reviews", "re-keyed under the new name");
    assert!(
        !specs.iter().any(|s| s.tab_name == "grid"),
        "no stale old key survives"
    );
}

#[test]
fn implicit_tab_teardown_drops_template_spec() {
    // x-cde1 AC2-HP: closing a template tab's last pane via close_pane removes
    // the tab and must drop its stored spec, or restore resurrects the closed
    // tab. A second tab keeps the session alive so the removal hits the
    // tab-removed branch (not SessionEmpty/shutdown).
    let _s = StoreScratch::new("cde1-close");
    let (mut core, _p) = template_core();
    core.clients.push(client(1, 5, (24, 80), false));
    core.create_tab_in(1, None)
        .expect("second tab keeps the session alive");
    core.session.squad_mut(1).unwrap().tabs[0].name = Some("grid".into());
    core.apply_spec(
        1,
        &TabSel::Id(5),
        &shell_spec(TemplateName::MainLeft, 2),
        false,
    )
    .unwrap();
    let loaded = crate::squad_store::load();
    let specs = &loaded
        .squads
        .iter()
        .find(|s| s.name == "sq")
        .unwrap()
        .tab_specs;
    assert_eq!(specs.len(), 1, "spec persisted before teardown");

    // Close tab 5's panes one at a time; the last close removes the tab.
    let leaves = tree::leaves(
        &core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|t| t.id == 5)
            .unwrap()
            .root,
    );
    for p in leaves {
        core.close_pane(p);
    }

    let loaded = crate::squad_store::load();
    let specs = &loaded
        .squads
        .iter()
        .find(|s| s.name == "sq")
        .unwrap()
        .tab_specs;
    assert!(
        !specs.iter().any(|s| s.tab_name == "grid"),
        "the closed template tab's spec is dropped from the store"
    );
}

#[test]
fn two_slots_binding_the_same_session_are_refused_atomically() {
    // Codex P1 regression: the same fno id in two slots would commit a
    // duplicate PaneId leaf. Refuse pre-mutation.
    let (mut core, p1) = template_core();
    core.agents = vec![bound_agent("S1", p1)];
    let panes_before = core.panes.len();
    let dup = LayoutSpec {
        template: TemplateName::MainLeft,
        slots: vec![
            SlotBinding::Fno("S1".into()),
            SlotBinding::Fno("S1".into()),
            SlotBinding::Shell,
            SlotBinding::Shell,
        ],
    };
    let err = core.apply_spec(1, &TabSel::Id(5), &dup, false).unwrap_err();
    assert_eq!(err.0, err_code::BAD_REQUEST);
    assert_eq!(core.panes.len(), panes_before, "the refusal spawns nothing");
}

#[test]
fn pane_break_moves_pane_to_new_tab_keeping_siblings() {
    let mut core = two_tab_core();
    let new_tid = core.pane_break(1, Some("solo".into())).unwrap();
    let sq = core.session.squad(1).unwrap();
    let a = sq.tabs.iter().find(|t| t.id == 10).unwrap();
    assert_eq!(
        tree::leaves(&a.root),
        vec![2],
        "sibling 2 stays in the source tab"
    );
    let nt = sq.tabs.iter().find(|t| t.id == new_tid).unwrap();
    assert_eq!(tree::leaves(&nt.root), vec![1], "1 broke into its own tab");
    assert_eq!(nt.name.as_deref(), Some("solo"));
}

#[test]
fn pane_break_last_pane_removes_the_emptied_source_tab() {
    // AC1-EDGE: pane 3 is tab 20's only leaf.
    let mut core = two_tab_core();
    let new_tid = core.pane_break(3, None).unwrap();
    let sq = core.session.squad(1).unwrap();
    assert!(
        sq.tabs.iter().all(|t| t.id != 20),
        "the emptied source tab is removed, not left blank"
    );
    let nt = sq.tabs.iter().find(|t| t.id == new_tid).unwrap();
    assert_eq!(tree::leaves(&nt.root), vec![3]);
}

#[test]
fn tab_join_round_trips_a_break() {
    // AC4-HP (tree half): break 1 into its own tab, then join it back next
    // to sibling 2. The transient tab is gone; the pane set is preserved.
    let mut core = two_tab_core();
    let brk = core.pane_break(1, None).unwrap();
    core.tab_join(&TabSel::Id(brk), 2, Dir::Right).unwrap();
    let sq = core.session.squad(1).unwrap();
    assert!(sq.tabs.iter().all(|t| t.id != brk), "the break tab is gone");
    let a = sq.tabs.iter().find(|t| t.id == 10).unwrap();
    let mut ls = tree::leaves(&a.root);
    ls.sort_unstable();
    assert_eq!(ls, vec![1, 2], "1 rejoined 2 in the original tab");
    crate::tree::check_invariants(a).unwrap();
}

#[test]
fn tab_join_into_self_is_refused_bad_request() {
    // AC2-EDGE: anchor 1 lives in tab 10; joining tab 10 into itself refuses.
    let mut core = two_tab_core();
    let before = core.session.squad(1).unwrap().tabs.clone();
    let err = core.tab_join(&TabSel::Id(10), 1, Dir::Right).unwrap_err();
    assert_eq!(err.0, err_code::BAD_REQUEST);
    assert_eq!(
        core.session.squad(1).unwrap().tabs,
        before,
        "a self-join mutates nothing"
    );
}

#[test]
fn pane_where_distinguishes_found_absent_and_paneless() {
    // AC1-ERR: three DISTINCT outcomes. F is pane-hosted (mux -> pane 1),
    // G is a paneless bg row, Z is unknown.
    let mut core = two_tab_core();
    core.session_name = "sess".into();
    let mut f = agent_in("sess", 1, None, false);
    f.session_id = Some("F".into());
    let mut g = agent(3, None, false);
    g.mux = None; // paneless bg
    g.session_id = Some("G".into());
    core.agents = vec![f, g];

    match core.pane_where("F") {
        Ok(ServerMsg::PaneLocation { panes, tabs, .. }) => {
            assert_eq!(panes, vec![1]);
            assert_eq!(tabs, vec![(10, None)]);
        }
        other => panic!("F should resolve, got {other:?}"),
    }
    assert_eq!(core.pane_where("Z"), Err(err_code::NOT_FOUND));
    assert_eq!(core.pane_where("G"), Err(err_code::NOT_PANE_HOSTED));
}

// -- x-1499 reverse location lookup -----------------------------------

#[test]
fn tab_location_resolves_all_forms_and_joins_occupants() {
    // AC3-HP: the receipt names the workspace, both identifier forms,
    // every pane id, and every joined worker.
    let mut core = two_tab_core();
    core.session_name = "sess".into();
    let mut w1 = agent_in("sess", 1, None, false);
    w1.session_id = Some("W1".into());
    let mut w2 = agent_in("sess", 2, None, false);
    w2.session_id = Some("W2".into());
    core.agents = vec![w1, w2];

    let loc = core.tab_where("id:10", &PaneTarget::CurrentRoute, &core.agents);
    match loc {
        Ok(ServerMsg::TabLocation {
            squad_id,
            squad_name,
            tab_id,
            name,
            ordinal,
            focus,
            panes,
        }) => {
            assert_eq!((squad_id, tab_id, ordinal, focus), (1, 10, 1, 1));
            assert_eq!(squad_name, None);
            assert_eq!(name, None);
            let joined: Vec<_> = panes
                .iter()
                .map(|o| (o.pane_id, o.fno_id.as_deref()))
                .collect();
            assert_eq!(joined, vec![(1, Some("W1")), (2, Some("W2"))]);
        }
        other => panic!("id:10 should resolve, got {other:?}"),
    }

    // ordinal: and name: land on the same tabs as id:.
    let by_ord = core.tab_where("ordinal:1", &PaneTarget::CurrentRoute, &core.agents);
    assert!(matches!(
        &by_ord,
        Ok(ServerMsg::TabLocation { tab_id: 10, .. })
    ));
    let by_name = core.tab_where("name:bee", &PaneTarget::CurrentRoute, &core.agents);
    assert!(matches!(
        &by_name,
        Ok(ServerMsg::TabLocation {
            tab_id: 20,
            ordinal: 2,
            name: Some(n),
            ..
        }) if n == "bee"
    ));

    // AC3-ERR: a pane with no registry worker is an EXPLICIT empty
    // occupant in a successful reply, never a failed resolution.
    assert!(matches!(
        &by_name,
        Ok(ServerMsg::TabLocation { panes, .. })
            if panes.iter().all(|o| o.pane_id == 3 && o.fno_id.is_none())
    ));

    // AC1-ERR: 0 and beyond-count ordinals refuse with the named errors.
    assert_eq!(
        core.tab_where("ordinal:0", &PaneTarget::CurrentRoute, &[])
            .unwrap_err()
            .1,
        "tab ordinal starts at 1"
    );
    assert_eq!(
        core.tab_where("ordinal:9", &PaneTarget::CurrentRoute, &[])
            .unwrap_err()
            .1,
        "no tab at ordinal 9"
    );
    assert_eq!(
        core.tab_where("id:99", &PaneTarget::CurrentRoute, &[])
            .unwrap_err()
            .1,
        "no tab with id 99"
    );
}

#[test]
fn tab_location_refuses_workspace_and_bare_number_ambiguity() {
    let mut core = two_tab_core(); // squad 1: tabs 10, 20; squad 2: tabs 30, 2
    core.session
        .add_squad(2, vec!["/b".into()], None, leaf_tab(30, 7));
    core.session.squad_mut(2).unwrap().tabs.push(leaf_tab(2, 8));

    // An unqualified ordinal repeating across workspaces refuses and
    // prints every candidate with workspace, label, and tab_id.
    let (code, msg) = core
        .tab_where("ordinal:2", &PaneTarget::CurrentRoute, &[])
        .unwrap_err();
    assert_eq!(code, err_code::BAD_REQUEST);
    assert!(msg.contains("tab=bee tab_id=20"), "msg: {msg}");
    assert!(msg.contains("tab=·2 tab_id=2"), "msg: {msg}");
    assert!(msg.contains("workspace="), "msg: {msg}");

    // Qualification resolves it.
    assert!(matches!(
        core.tab_where("ordinal:2", &PaneTarget::SquadId(2), &[]),
        Ok(ServerMsg::TabLocation { tab_id: 2, .. })
    ));

    // AC4-ERR: a bare number that names DIFFERENT live tabs as an ordinal
    // and as a stable id refuses with both explicit forms, never a
    // first-pick by iteration order.
    let (code, msg) = core
        .tab_where("2", &PaneTarget::CurrentRoute, &[])
        .unwrap_err();
    assert_eq!(code, err_code::BAD_REQUEST);
    assert!(msg.contains("ordinal:2"), "msg: {msg}");
    assert!(msg.contains("id:2"), "msg: {msg}");

    // A bare number whose ordinal reading alone spans workspaces gets the
    // USABLE refusal (qualify the workspace), never bare-number advice
    // naming an id form that matches nothing.
    let (code, msg) = core
        .tab_where("1", &PaneTarget::CurrentRoute, &[])
        .unwrap_err();
    assert_eq!(code, err_code::BAD_REQUEST);
    assert!(msg.contains("matches 2 workspaces"), "msg: {msg}");
    assert!(!msg.contains("as an id"), "msg: {msg}");

    // A bare number with only one live reading resolves that reading.
    assert!(matches!(
        core.tab_where("1", &PaneTarget::SquadId(1), &[]),
        Ok(ServerMsg::TabLocation { tab_id: 10, .. })
    ));
}

#[test]
fn fno_id_for_pane_forward_join() {
    // AC3-HP reverse direction: pane -> fno_id via the registry join.
    let mut core = two_tab_core();
    core.session_name = "sess".into();
    let mut f = agent_in("sess", 1, None, false);
    f.session_id = Some("F".into());
    core.agents = vec![f];
    assert_eq!(core.fno_id_for_pane(1), Some("F".into()));
    assert_eq!(
        core.fno_id_for_pane(2),
        None,
        "no registry row -> no fno_id"
    );

    // Python-authored pane rows carry the canonical harness identity and
    // leave the legacy fno session slot empty.
    core.agents[0].session_id = None;
    core.agents[0].harness_session_id = Some("CODEX-THREAD".into());
    assert_eq!(core.fno_id_for_pane(1), Some("CODEX-THREAD".into()));
    core.backlog_holders
        .insert("x-identity".into(), "CODEX-THREAD".into());
    assert_eq!(
        core.fno_id_for_pane(1).as_deref(),
        core.backlog_holders.get("x-identity").map(String::as_str),
        "mux identity and node claim holder must be the same peer"
    );
}

#[test]
fn resolve_tab_index_by_id_name_and_ordinal() {
    let core = two_tab_core();
    assert_eq!(core.resolve_tab_index(1, &TabSel::Id(20)).unwrap(), 1);
    assert_eq!(
        core.resolve_tab_index(1, &TabSel::Name("bee".into()))
            .unwrap(),
        1
    );
    // `Index` is the 1-based ordinal the UI shows: 1 names the first tab,
    // 0 is a refusal, never a silent zero-based vector index (x-1499).
    assert_eq!(core.resolve_tab_index(1, &TabSel::Index(1)).unwrap(), 0);
    assert_eq!(core.resolve_tab_index(1, &TabSel::Index(2)).unwrap(), 1);
    assert_eq!(
        core.resolve_tab_index(1, &TabSel::Index(0)).unwrap_err(),
        "tab ordinal starts at 1"
    );
    assert!(core
        .resolve_tab_index(1, &TabSel::Name("nope".into()))
        .is_err());
    assert_eq!(
        core.resolve_tab_index(1, &TabSel::Index(9)).unwrap_err(),
        "no tab at ordinal 9"
    );
}

#[test]
fn layout_get_carries_nested_tree_and_geometry() {
    // Locked Decision 5: structure AND rects.
    let core = two_tab_core();
    let squads = core.layout_get(&LayoutScope::Session, None).unwrap();
    assert_eq!(squads.len(), 1);
    let tab10 = squads[0].tabs.iter().find(|t| t.tab_id == 10).unwrap();
    assert!(
        matches!(tab10.root, Node::Branch { .. }),
        "the nested tree is carried, not flattened"
    );
    assert_eq!(tab10.panes.len(), 2, "both panes are tiled with rects");
    assert!(tab10.panes.iter().all(|(_, r)| r.cols > 0 && r.rows > 0));
    // (x-1499) The worker join is OPT-IN: absent (None) unless the caller
    // asked, so the machine JSON shape never grows a key on a healthy
    // reply; asked for, it joins the registry rows by pane.
    assert!(
        squads[0].tabs.iter().all(|t| t.workers.is_none()),
        "no workers key without the request"
    );
    let mut core = two_tab_core();
    core.session_name = "sess".into();
    let mut w = agent_in("sess", 1, None, false);
    w.session_id = Some("W1".into());
    core.agents = vec![w];
    let squads = core
        .layout_get(&LayoutScope::Session, Some(&core.agents.clone()))
        .unwrap();
    let tab10 = squads[0].tabs.iter().find(|t| t.tab_id == 10).unwrap();
    assert_eq!(
        tab10.workers.as_ref().map(|ws| ws.first().cloned()),
        Some(Some(TabPaneOccupant {
            pane_id: 1,
            fno_id: Some("W1".into())
        })),
        "the requested join names the pane's worker"
    );
}

#[test]
fn split_pane_script_splits_arbitrary_pane_without_stealing_focus() {
    // AC1-HP + AC1-FR: split the NON-focused pane 2 (tab focus is 1); the
    // new pane appears but the viewer's focus stays put.
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    assert_eq!(core.session.squad(1).unwrap().tabs[0].focus, 1);
    let new_pid = core.split_pane_script(2, Dir::Right, true).unwrap();
    let tab = &core.session.squad(1).unwrap().tabs[0];
    let mut ls = tree::leaves(&tab.root);
    ls.sort_unstable();
    assert_eq!(ls, vec![1, 2, new_pid], "the 3rd pane joined the tab");
    assert_eq!(tab.focus, 1, "a scripted split never steals focus");
    core.reap_pane(new_pid);
}

// The pane-run placement family (x-18c4 receipt plus the named-tab/anchor
// placement test) moved verbatim into its own module: this file is over
// the shrink-only line, and test motion is the sanctioned shrink.
#[path = "server/tests/pane_run_receipt_tests.rs"]
mod pane_run_receipt_tests;

// The fit placement tests (x-ae47) moved out for the same reason.
#[path = "server/tests/placement_fit_tests.rs"]
mod placement_fit_tests;

#[test]
fn exact_current_refuses_conflicting_tab_selector() {
    // AC1-EDGE: --at current pins pane 1 (tab 10); an explicit --tab id:20
    // names a tab the anchor is NOT in. Strict placement refuses rather than
    // redirecting, reaps the pre-spawned child, and changes no tree.
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    let err = core
        .run_pane(
            "/a".into(),
            "/a".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadId(1),
                split: Some(Dir::Down),
                tab: Some(TabSel::Id(20)),
                at: Some(1),
                fallback: PlacementFallback::Refuse,
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert_eq!(err.0, err_code::BAD_REQUEST);
    assert!(
        err.1.contains("not in the requested tab"),
        "conflict message: {}",
        err.1
    );
    let s = core.session.squad(1).unwrap();
    assert_eq!(s.tabs.len(), 2, "no new tab minted");
    assert!(tree::leaves(&s.tabs.iter().find(|t| t.id == 10).unwrap().root).contains(&1));
}

#[test]
fn exact_current_refuses_when_the_anchor_tab_is_at_capacity() {
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    let viewport = core.tab_rect(10);
    {
        let tab = core
            .session
            .squad_mut(1)
            .unwrap()
            .tabs
            .iter_mut()
            .find(|tab| tab.id == 10)
            .unwrap();
        for (anchor, pane) in [(1, 101), (101, 102)] {
            tree::split_at(tab, viewport, anchor, Dir::Right, pane).unwrap();
        }
    }
    let before = tree::leaves(
        &core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|tab| tab.id == 10)
            .unwrap()
            .root,
    );
    assert_eq!(before.len(), 4);
    let placement: PanePlacement =
        serde_json::from_str(r#"{"at":1,"split":"Right","fallback":"refuse","max_panes":4}"#)
            .unwrap();

    let err = core
        .run_pane(
            "/a".into(),
            "/a".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            placement,
            None,
        )
        .unwrap_err();

    assert_eq!(err.0, err_code::BAD_REQUEST);
    assert!(
        err.1.contains("4 panes") && err.1.contains("cap is 4"),
        "{}",
        err.1
    );
    assert!(err.1.contains("--workspace <name>"), "{}", err.1);
    let after = tree::leaves(
        &core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|tab| tab.id == 10)
            .unwrap()
            .root,
    );
    assert_eq!(after, before, "capacity refusal changed the target tab");
}

#[test]
fn selector_tab_refuses_when_the_target_tab_is_at_capacity() {
    // An explicit --tab (or a numeric anchor with a non-Refuse fallback)
    // resolves through the selector path, not the strict one; the cap the
    // caller set must bind there too, or the same flag that guards one
    // spelling of placement grows a crowded tab under another.
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    let viewport = core.tab_rect(10);
    {
        let tab = core
            .session
            .squad_mut(1)
            .unwrap()
            .tabs
            .iter_mut()
            .find(|tab| tab.id == 10)
            .unwrap();
        for (anchor, pane) in [(1, 101), (101, 102)] {
            tree::split_at(tab, viewport, anchor, Dir::Right, pane).unwrap();
        }
    }
    let before = tree::leaves(
        &core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|tab| tab.id == 10)
            .unwrap()
            .root,
    );
    assert_eq!(before.len(), 4);
    let placement: PanePlacement = serde_json::from_str(
        r#"{"tab":{"Id":10},"split":"Right","fallback":"new_tab","max_panes":4}"#,
    )
    .unwrap();

    let err = core
        .run_pane(
            "/a".into(),
            "/a".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            placement,
            None,
        )
        .unwrap_err();

    assert_eq!(err.0, err_code::BAD_REQUEST);
    assert!(
        err.1.contains("4 panes") && err.1.contains("cap is 4"),
        "{}",
        err.1
    );
    assert!(err.1.contains("--workspace <name>"), "{}", err.1);
    let after = tree::leaves(
        &core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|tab| tab.id == 10)
            .unwrap()
            .root,
    );
    assert_eq!(after, before, "capacity refusal changed the target tab");
}

#[test]
fn exact_current_accepts_the_last_slot_below_capacity() {
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    let viewport = core.tab_rect(10);
    {
        let tab = core
            .session
            .squad_mut(1)
            .unwrap()
            .tabs
            .iter_mut()
            .find(|tab| tab.id == 10)
            .unwrap();
        tree::split_at(tab, viewport, 1, Dir::Right, 101).unwrap();
    }
    let placement: PanePlacement =
        serde_json::from_str(r#"{"at":1,"split":"Right","fallback":"refuse","max_panes":4}"#)
            .unwrap();

    core.run_pane(
        "/a".into(),
        "/a".into(),
        vec!["/bin/cat".into()],
        24,
        80,
        false,
        placement,
        None,
    )
    .unwrap();

    let leaves = tree::leaves(
        &core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|tab| tab.id == 10)
            .unwrap()
            .root,
    );
    assert_eq!(leaves.len(), 4);
}

#[test]
fn graft_materialization_rollback_preserves_tree_on_shell_failure() {
    // AC4-EDGE-SHELL-ROLLBACK / AC2-FR: when a Shell slot cannot spawn, the
    // graft refuses - no shell, no tree mutation, no focus change survives,
    // and the anchor's tab is byte-unchanged. Forced by an empty
    // shell-candidate list so spawn_pane fails.
    use crate::proto::{
        AnchoredLayoutSpec, LayoutBinding, LayoutSlot, LayoutTreeChild, LayoutTreeSpec,
    };
    use crate::tree::Axis;

    let mut core = two_tab_core();
    core.shells = Vec::new(); // every spawn_pane fails
    let tab10_before = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 10)
        .unwrap()
        .clone();
    let spec = AnchoredLayoutSpec {
        version: 1,
        tree: LayoutTreeSpec::Split {
            axis: Axis::Vertical,
            children: vec![
                LayoutTreeChild {
                    weight: 0.5,
                    tree: LayoutTreeSpec::Slot("a".into()),
                },
                LayoutTreeChild {
                    weight: 0.5,
                    tree: LayoutTreeSpec::Slot("b".into()),
                },
            ],
        },
        slots: vec![
            LayoutSlot::new("a".into(), LayoutBinding::Anchor),
            LayoutSlot::new("b".into(), LayoutBinding::Shell),
        ],
    };
    let err = core
        .layout_graft(&PaneTarget::SquadId(1), 1, &spec, false)
        .unwrap_err();
    assert!(
        err.1.contains("failed to spawn") && err.1.contains("rolled back"),
        "rollback message: {}",
        err.1
    );
    let tab10_after = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 10)
        .unwrap()
        .clone();
    assert_eq!(tab10_after.root, tab10_before.root, "tree byte-unchanged");
    assert_eq!(tab10_after.focus, tab10_before.focus, "focus unchanged");
    // No leaked pane id: only the original 1 and 2 remain.
    let mut leaves = tree::leaves(&tab10_after.root);
    leaves.sort_unstable();
    assert_eq!(leaves, vec![1, 2]);
}

#[test]
fn graft_re_resolves_anchor_tab_after_detaching_earlier_source_tab() {
    // P1 (codex review): an Fno binding whose pane lives in an EARLIER
    // single-pane tab than the anchor's. Detaching it removes that source
    // tab and shifts the anchor's tab index; the graft must re-resolve the
    // anchor's tab by stable id, or it grafts into the wrong tab / panics.
    use crate::proto::{
        AnchoredLayoutSpec, LayoutBinding, LayoutSlot, LayoutTreeChild, LayoutTreeSpec, ServerMsg,
    };
    use crate::tree::Axis;

    let (mut core, p1) = template_core(); // p1 lives alone in tab 5
    core.agents = vec![bound_agent("S1", p1)];
    // A second tab holding the anchor; squad tabs are now [5, anchor_tid],
    // so the Fno pane's tab 5 is EARLIER than the anchor's.
    let anchor_tid = core.create_tab_in(1, Some("anchor-tab".into())).unwrap();
    core.tab_areas.insert(anchor_tid, (24, 80));
    let anchor_pane = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == anchor_tid)
        .unwrap()
        .focus;

    let spec = AnchoredLayoutSpec {
        version: 1,
        tree: LayoutTreeSpec::Split {
            axis: Axis::Vertical,
            children: vec![
                LayoutTreeChild {
                    weight: 0.5,
                    tree: LayoutTreeSpec::Slot("a".into()),
                },
                LayoutTreeChild {
                    weight: 0.5,
                    tree: LayoutTreeSpec::Slot("b".into()),
                },
            ],
        },
        slots: vec![
            LayoutSlot::new("a".into(), LayoutBinding::Anchor),
            LayoutSlot::new("b".into(), LayoutBinding::Fno("S1".into())),
        ],
    };
    let msg = core
        .layout_graft(&PaneTarget::SquadId(1), anchor_pane, &spec, false)
        .expect("graft commits into the anchor's tab");
    let ServerMsg::LayoutGrafted { tab, .. } = msg else {
        panic!("not LayoutGrafted");
    };
    assert_eq!(
        tab, anchor_tid,
        "grafted into the anchor's tab, not the removed source"
    );
    // Source tab 5 emptied (p1 relocated) -> removed.
    assert!(
        core.session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .all(|t| t.id != 5),
        "the earlier single-pane source tab was removed"
    );
    let anchor_tab = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == anchor_tid)
        .unwrap();
    let mut leaves = tree::leaves(&anchor_tab.root);
    leaves.sort_unstable();
    let mut expected = vec![anchor_pane, p1];
    expected.sort_unstable();
    assert_eq!(
        leaves, expected,
        "the anchor + the relocated fno pane, no stale index"
    );
}

/// A client with a LIVE reliable receiver, so `push_layout` never drops it
/// as dead mid-test (the bare `client()` helper drops its receiver). Returns
/// the receiver to keep in scope for the test's lifetime.
fn live_client(id: u64, view_tab: TabId) -> (Client, mpsc::Receiver<ServerMsg>) {
    let (tx, rx) = mpsc::channel::<ServerMsg>(RELIABLE_CAP);
    let mut c = client(id, view_tab, (24, 80), false);
    c.reliable_tx = tx;
    (c, rx)
}

#[test]
fn pane_break_reanchors_a_client_on_the_emptied_source_tab() {
    // codex P1: breaking a tab's last pane while a client views it must
    // re-anchor that client, never leave a dangling view push_layout skips.
    let mut core = two_tab_core();
    let (c, _rx) = live_client(1, 20); // viewing tab 20 (pane 3 only)
    core.clients.push(c);
    core.pane_break(3, None).unwrap();
    let view = core.clients[0].view;
    assert!(core.viewed_tab(view).is_some(), "re-anchored to a live tab");
    assert_ne!(view.1, 20, "not stranded on the removed tab");
}

#[test]
fn tab_join_reanchors_a_client_on_the_removed_source_tab() {
    // codex P1: the join removes the source tab; a viewer of it re-anchors.
    let mut core = two_tab_core();
    let brk = core.pane_break(1, None).unwrap(); // src tab `brk` holds [1]
    let (c, _rx) = live_client(1, brk);
    core.clients.push(c);
    core.tab_join(&TabSel::Id(brk), 2, Dir::Right).unwrap();
    let view = core.clients[0].view;
    assert!(core.viewed_tab(view).is_some(), "re-anchored to a live tab");
    assert_ne!(view.1, brk, "not stranded on the removed source tab");
}

// ---- v43 (x-d6a8) US9 interactive drag Commands -------------------------
// (drain_notice helper is defined once below, near the StopAgent tests.)

#[test]
fn break_pane_command_focuses_the_acting_client_on_the_new_tab() {
    // AC1-HP: the interactive break drops pane 1 (of tab 10's [1,2]) onto the
    // strip. The acting client's focus follows the gesture onto the new tab.
    let mut core = two_tab_core();
    let (c, _rx) = live_client(7, 10); // viewing tab 10
    core.clients.push(c);
    core.command(7, Command::BreakPane { pane: 1 });
    let view = core.client_view(7).unwrap();
    let tab = core.viewed_tab(view).expect("focused a live tab");
    assert_ne!(view.1, 10, "the acting client left the source tab");
    assert_eq!(
        tree::leaves(&tab.root),
        vec![1],
        "and landed on the freshly broken-out tab holding pane 1"
    );
    // The source tab survives with the sibling.
    let src = core.session.squad(1).unwrap();
    let a = src.tabs.iter().find(|t| t.id == 10).unwrap();
    assert_eq!(tree::leaves(&a.root), vec![2], "sibling 2 stays in tab 10");
}

#[test]
fn pane_break_script_path_leaves_the_viewer_focus_unchanged() {
    // AC1-HP (the "and": the script path does NOT move focus). The CoreMsg
    // path a scripted ControlVerb::PaneBreak takes is pane_break itself; it
    // never touches a view. A viewer of the (surviving) source tab stays put.
    let mut core = two_tab_core();
    let (c, _rx) = live_client(7, 10); // viewing tab 10 [1,2]
    core.clients.push(c);
    core.pane_break(1, None).unwrap(); // the script/CoreMsg path
    assert_eq!(
        core.client_view(7),
        Some((1, 10)),
        "the script break leaves the viewer on tab 10, unmoved"
    );
}

#[test]
fn pane_break_carries_a_name_only_when_it_empties_the_source_tab() {
    // Breaking out a pane that is ALONE in its tab rebuilds that tab around
    // the same pane, so dropping the operator's name is data loss. Breaking
    // one pane out of several is a genuinely new tab and stays unnamed.
    let mut core = two_tab_core(); // tab 20 = "bee", a single leaf 3
    let new_tid = core.pane_break(3, None).expect("break a solo pane");
    let sq = core.session.squad(1).unwrap();
    assert_eq!(
        sq.tabs
            .iter()
            .find(|t| t.id == new_tid)
            .unwrap()
            .name
            .as_deref(),
        Some("bee"),
        "the emptied tab's name carries to the tab rebuilt around its pane"
    );
    assert!(
        sq.tabs.iter().all(|t| t.id != 20),
        "the emptied source tab is gone, so no two tabs share the name"
    );

    // Tab 10 holds [1, 2]: breaking 1 out leaves 2 behind, so the source
    // survives and keeps its name while the new tab gets none.
    let mut core = two_tab_core();
    core.session
        .squad_mut(1)
        .unwrap()
        .tabs
        .iter_mut()
        .find(|t| t.id == 10)
        .unwrap()
        .name = Some("keep".into());
    let new_tid = core.pane_break(1, None).expect("break one of two panes");
    let sq = core.session.squad(1).unwrap();
    assert_eq!(
        sq.tabs.iter().find(|t| t.id == new_tid).unwrap().name,
        None,
        "a surviving source tab keeps its name; the break-out is a new tab"
    );
    assert_eq!(
        sq.tabs.iter().find(|t| t.id == 10).unwrap().name.as_deref(),
        Some("keep")
    );
}

#[test]
fn move_pane_cross_tab_grafts_into_viewed_tab_and_empties_the_source() {
    // AC3-HP: a sideline-row drop names a mover (pane 3) living in tab 20,
    // not the viewed tab 10 where target pane 2 lives. The cross-tab branch
    // detaches 3 from 20 and grafts it beside 2; tab 20 empties and is
    // removed. The pane id is preserved across trees (detach never reaps the
    // PTY - the "child pid unchanged" invariant at the tree level).
    let mut core = two_tab_core();
    let (c, _rx) = live_client(7, 10); // viewing tab 10 [1,2]
    core.clients.push(c);
    core.command(
        7,
        Command::MovePane {
            mover: Some(3),
            target: Some(2),
            dir: Dir::Right,
        },
    );
    let sq = core.session.squad(1).unwrap();
    assert!(
        sq.tabs.iter().all(|t| t.id != 20),
        "the emptied source tab 20 is removed"
    );
    let a = sq.tabs.iter().find(|t| t.id == 10).unwrap();
    let mut ls = tree::leaves(&a.root);
    ls.sort_unstable();
    assert_eq!(
        ls,
        vec![1, 2, 3],
        "pane 3 moved into the viewed tab, id kept"
    );
    assert_eq!(a.focus, 3, "the moved pane is focused in its new home");
    crate::tree::check_invariants(a).unwrap();
}

#[test]
fn within_tab_move_pane_is_unchanged_by_the_cross_tab_branch() {
    // The cross-tab branch must not perturb the ordinary within-tab drag: a
    // move whose mover and target share the viewed tab still routes through
    // move_leaf.
    let mut core = two_tab_core();
    let (c, _rx) = live_client(7, 10); // viewing tab 10 [1,2]
    core.clients.push(c);
    core.command(
        7,
        Command::MovePane {
            mover: Some(2),
            target: Some(1),
            dir: Dir::Up, // 2 above 1: a real reshape within tab 10
        },
    );
    let a = core.session.squad(1).unwrap();
    let t = a.tabs.iter().find(|t| t.id == 10).unwrap();
    let mut ls = tree::leaves(&t.root);
    ls.sort_unstable();
    assert_eq!(ls, vec![1, 2], "both panes still in tab 10");
    assert!(
        matches!(
            t.root,
            Node::Branch {
                axis: Axis::Vertical,
                ..
            }
        ),
        "the within-tab move reshaped to a vertical split"
    );
    assert!(
        core.session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .any(|t| t.id == 20),
        "tab 20 is untouched by a within-tab move"
    );
}

#[test]
fn join_tab_command_min_size_refusal_surfaces_a_named_notice_and_mutates_nothing() {
    // AC1-ERR: a join that would push a pane below min-size is refused with a
    // NAMED notice (not a bare "Error") and leaves BOTH trees exactly as they
    // were (all-or-nothing).
    let mut core = two_tab_core();
    let (c, mut rx) = live_client(7, 10); // viewing tab 10 [1,2]
    core.clients.push(c);
    // The viewer's dims clamp the tab area (tab_area prefers a live viewer's
    // dims over tab_areas): 8 cols cannot hold three MIN_COLS(8)-wide children.
    core.clients[0].dims = (40, 8);
    let before = core.session.squad(1).unwrap().tabs.clone();
    core.command(
        7,
        Command::JoinTab {
            src_tab: 20,
            anchor_pane: 2,
            dir: Dir::Right,
        },
    );
    assert_eq!(
        core.session.squad(1).unwrap().tabs,
        before,
        "a refused join mutates neither tree"
    );
    let notice = drain_notice(&mut rx).expect("a refused join notices the sender");
    assert!(
        notice.contains("minimum") || notice.contains("smaller"),
        "the notice names the min-size reason, got: {notice:?}"
    );
}

#[test]
fn join_tab_command_into_self_surfaces_a_named_notice() {
    // AC1-ERR (self-join half): joining a tab into its own anchor pane is
    // refused BAD_REQUEST server-side with a reason that names "itself".
    let mut core = two_tab_core();
    let (c, mut rx) = live_client(7, 10);
    core.clients.push(c);
    let before = core.session.squad(1).unwrap().tabs.clone();
    // anchor pane 1 lives in tab 10; joining tab 10 into itself.
    core.command(
        7,
        Command::JoinTab {
            src_tab: 10,
            anchor_pane: 1,
            dir: Dir::Right,
        },
    );
    assert_eq!(
        core.session.squad(1).unwrap().tabs,
        before,
        "a self-join mutates nothing"
    );
    let notice = drain_notice(&mut rx).expect("a refused self-join notices the sender");
    assert!(
        notice.contains("itself"),
        "the notice names the self-join reason, got: {notice:?}"
    );
}

#[test]
fn break_then_failed_join_keeps_the_broken_pane_where_the_break_left_it() {
    // AC2-FR: pane 1 breaks to its own tab; a following join of that tab that
    // fails min-size leaves the broken pane exactly where the break put it
    // (the pane id survives both ops - the tree-level "pid unchanged" claim).
    let mut core = two_tab_core();
    let (c, _rx) = live_client(7, 10);
    core.clients.push(c);
    core.command(7, Command::BreakPane { pane: 1 });
    let brk = core.client_view(7).unwrap().1; // the new tab holding [1]
                                              // Now cram the anchor tab so a join back would breach min-size.
    core.tab_areas.insert(20, (40, 8));
    let before_brk = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == brk)
        .unwrap()
        .clone();
    core.command(
        7,
        Command::JoinTab {
            src_tab: brk,
            anchor_pane: 3, // pane 3 lives in the crammed tab 20
            dir: Dir::Right,
        },
    );
    let after = core.session.squad(1).unwrap();
    let brk_tab = after
        .tabs
        .iter()
        .find(|t| t.id == brk)
        .expect("the broken-out tab still exists after the failed join");
    assert_eq!(
        brk_tab.root, before_brk.root,
        "the failed join left the broken pane exactly as the break produced it"
    );
    assert_eq!(tree::leaves(&brk_tab.root), vec![1], "pane 1 kept its id");
}

#[test]
fn tab_create_and_rename_sanitize_names() {
    // codex P2: control bytes stripped and length capped at the wire
    // boundary, exactly like Command::RenameTab.
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core
        .tab_create(&PaneTarget::SquadId(1), Some("\x1b[31mwork".into()))
        .unwrap();
    let (sid, ti) = core.session.find_pane(pid).unwrap();
    let name = core.session.squad(sid).unwrap().tabs[ti]
        .name
        .clone()
        .unwrap();
    assert!(!name.contains('\x1b'), "escape byte stripped: {name:?}");
    core.reap_pane(pid);

    core.tab_rename(
        &PaneTarget::SquadId(1),
        &TabSel::Id(10),
        "x".repeat(MAX_TAB_NAME + 50),
    )
    .unwrap();
    let renamed = core.session.squad(1).unwrap().tabs[0].name.clone().unwrap();
    assert!(renamed.len() <= MAX_TAB_NAME, "oversized name capped");
}

#[test]
fn pane_where_rejects_ambiguous_prefix_but_exact_wins() {
    // codex P2: two identities share a prefix -> refuse; an exact id resolves.
    let mut core = two_tab_core();
    core.session_name = "sess".into();
    let mut a = agent_in("sess", 1, None, false);
    a.session_id = Some("abc111".into());
    let mut b = agent_in("sess", 2, None, false);
    b.session_id = Some("abc222".into());
    core.agents = vec![a, b];
    assert_eq!(core.pane_where("abc"), Err(err_code::NOT_FOUND));
    match core.pane_where("abc111") {
        Ok(ServerMsg::PaneLocation { panes, .. }) => assert_eq!(panes, vec![1]),
        other => panic!("exact prefix should resolve: {other:?}"),
    }
}

#[test]
fn pane_where_rejects_ambiguous_harness_only_prefix() {
    let mut core = two_tab_core();
    core.session_name = "sess".into();
    let mut a = agent_in("sess", 1, None, false);
    a.harness_session_id = Some("019fb024-one".into());
    let mut b = agent_in("sess", 2, None, false);
    b.harness_session_id = Some("019fb024-two".into());
    core.agents = vec![a, b];

    assert_eq!(core.pane_where("019fb024"), Err(err_code::NOT_FOUND));
    assert_eq!(core.resolve_local_pane("019fb024"), None);
}

#[test]
fn pane_ls_can_join_a_fresh_registry_snapshot_without_viewers() {
    let (mut core, pane_id) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane_id).unwrap().name = Some("worker".into());
    let mut fresh = agent_in("sess", pane_id, None, false);
    fresh.harness_session_id = Some("019fb024-fresh".into());
    fresh.name = "worker".into();
    core.panes
        .get_mut(&pane_id)
        .unwrap()
        .vt
        .feed("\x1b]0;⠋ Working\x07".as_bytes());

    assert_eq!(
        core.fno_id_for_pane_with_agents(pane_id, &[fresh.clone()])
            .as_deref(),
        Some("019fb024-fresh")
    );
    assert!(core.agents.is_empty(), "zero-viewer cache stays untouched");
    match core.pane_ls_from_fresh_agents(Some(&[fresh.clone()])) {
        ServerMsg::PaneList { panes } => {
            let pane = panes.iter().find(|pane| pane.pane_id == pane_id).unwrap();
            assert_eq!(pane.title.as_deref(), Some("⠋ Working"));
            assert_eq!(pane.name.as_deref(), Some("worker"));
            assert_eq!(pane.fno_id.as_deref(), Some("019fb024-fresh"));
        }
        other => panic!("pane ls should carry OSC title, got {other:?}"),
    }
    match core.pane_ls_from_fresh_agents(None) {
        ServerMsg::Err { code, .. } => assert_eq!(code, err_code::REGISTRY_UNAVAILABLE),
        other => panic!("registry failure must surface, got {other:?}"),
    }
    match core.pane_where_from_fresh_agents("019fb024-fresh", None) {
        ServerMsg::Err { code, .. } => assert_eq!(code, err_code::REGISTRY_UNAVAILABLE),
        other => panic!("registry failure must surface, got {other:?}"),
    }
}

#[test]
fn session_lineage_pane_ls_reports_thread_and_current_beside_each_other() {
    // AC7-HP (mux half): the listing carries the stable thread id AND the
    // current harness session, with the succession chain and fork edge
    // alongside, so a retired id can never read as current.
    let (mut core, pane_id) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane_id).unwrap().name = Some("worker".into());
    let mut successor = agent_in("sess", pane_id, None, false);
    successor.harness_session_id = Some("session-b".into());
    successor.predecessor_session_ids = vec!["session-a".into()];
    successor.forked_from_session_id = None;
    successor.name = "worker".into();

    match core.pane_ls_from_fresh_agents(Some(&[successor.clone()])) {
        ServerMsg::PaneList { panes } => {
            let pane = panes.iter().find(|p| p.pane_id == pane_id).unwrap();
            assert_eq!(pane.harness_session_id.as_deref(), Some("session-b"));
            assert_eq!(pane.predecessor_session_ids, vec!["session-a"]);
            assert!(pane.forked_from_session_id.is_none());
        }
        other => panic!("pane ls should join lineage, got {other:?}"),
    }

    // A branch row shows its fork edge.
    let mut branch = agent_in("sess", pane_id, None, false);
    branch.harness_session_id = Some("session-c".into());
    branch.forked_from_session_id = Some("session-b".into());
    branch.name = "worker-branch".into();
    match core.pane_ls_from_fresh_agents(Some(&[branch.clone()])) {
        ServerMsg::PaneList { panes } => {
            let pane = panes.iter().find(|p| p.pane_id == pane_id).unwrap();
            assert_eq!(pane.forked_from_session_id.as_deref(), Some("session-b"));
            assert!(pane.predecessor_session_ids.is_empty());
        }
        other => panic!("pane ls should join the fork edge, got {other:?}"),
    }
}

#[test]
fn rename_squad_blank_clears_origin_squad_and_refuses_origin_less() {
    // A blank rename clears an origin-backed squad to its derived label. An
    // origin-less squad has no derivable label, so the blank is refused.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/x".into()], Some("work".into()), leaf_tab(5, 1));
    core.session
        .add_squad(2, vec![], Some("scratch".into()), leaf_tab(6, 2));

    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameSquad {
            squad: 1,
            name: "  oss ".into(),
        },
    );
    assert_eq!(core.session.squads[0].name.as_deref(), Some("oss"));

    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameSquad {
            squad: 1,
            name: "   ".into(),
        },
    );
    assert_eq!(
        core.session.squads[0].name, None,
        "blank clears an origin-backed squad to its derived label"
    );

    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameSquad {
            squad: 2,
            name: "".into(),
        },
    );
    assert_eq!(
        core.session.squads[1].name.as_deref(),
        Some("scratch"),
        "an origin-less squad refuses a blank (nothing to derive)"
    );
}

#[test]
fn rename_squad_onto_a_taken_name_is_refused_with_a_notice() {
    // Reported as "renaming a workspace to a name another workspace
    // already carries fails silently". named_squad_taken already guards
    // this - the invariant this test pins is that the rename never
    // applies, so the operator's row keeps its old name rather than reading
    // as a no-op when it is actually a name collision with nothing renamed.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/x".into()], Some("work".into()), leaf_tab(5, 1));
    core.session
        .add_squad(2, vec!["/y".into()], Some("scratch".into()), leaf_tab(6, 2));
    let (tx, mut rx) = mpsc::channel(4);
    core.clients.push(Client {
        reliable_tx: tx,
        ..client(1, 5, (24, 80), false)
    });

    core.command(
        1,
        Command::RenameSquad {
            squad: 1,
            name: "scratch".into(),
        },
    );

    assert_eq!(
        core.session.squads[0].name.as_deref(),
        Some("work"),
        "a rename onto a taken name must not apply"
    );
    match rx.try_recv() {
        Ok(ServerMsg::Notice { text }) => assert_eq!(text, "name taken"),
        other => panic!("expected a name-taken notice, got {other:?}"),
    }
}

#[test]
fn rename_unnamed_to_named_mutates_the_row_instead_of_minting_a_second() {
    // AC12/AC13-HP (x-6b0b): an unnamed->named rename used to fall through
    // the plain persist: upsert wrote the new name AND the old key, and
    // `same_squad` matching named rows by name alone left the old key row
    // alive - one rename, two rows. Now the old key row is removed, the
    // live key is cleared, and every shape holds exactly one row.
    let _s = StoreScratch::new("x6b0b-rename-unnamed");
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/repo".into()], None, leaf_tab(5, 1));
    core.squad_members.insert(1, vec![]);
    core.persist_squad(1);
    let key_before = crate::squad_store::load().squads[0].key.clone();
    assert!(
        !key_before.is_empty(),
        "the unnamed squad persists under a durable key"
    );

    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameSquad {
            squad: 1,
            name: "oss".into(),
        },
    );

    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads.len(),
        1,
        "AC12-HP: one row, not a minted twin"
    );
    assert_eq!(loaded.squads[0].name, "oss");
    assert!(
        loaded.squads[0].key.is_empty(),
        "AC12-HP: a named row keys by name"
    );
    assert_eq!(core.session.squads[0].key, String::new());

    // AC13-HP: clearing the name re-derives the key from origins.
    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RenameSquad {
            squad: 1,
            name: "   ".into(),
        },
    );
    let loaded = crate::squad_store::load();
    assert_eq!(loaded.squads.len(), 1, "AC13-HP: still exactly one row");
    assert_eq!(loaded.squads[0].name, "");
    assert_eq!(
        loaded.squads[0].key,
        crate::squad_store::origin_key(&["/repo".to_string()]),
        "AC13-HP: the key re-derives from origins"
    );
}

#[test]
fn remove_squad_reanchors_then_last_ends_the_session() {
    // AC2-HP / AC2-EDGE (server half): removing a squad drops it and re-
    // anchors active_squad; removing the last squad ends the session.
    // Store half (de-persist contract): both rows leave the store too,
    // the SessionEmpty path included.
    let _s = StoreScratch::new("x361b-remove-reanchor");
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    core.session
        .add_squad(2, vec!["/b".into()], None, leaf_tab(6, 2));
    core.session.active_squad = Some(1);
    core.persist_squad(1);
    core.persist_squad(2);
    let ident1 = core.squad_identity(1).expect("identity before removal");
    let ident2 = core.squad_identity(2).expect("identity before removal");
    assert!(
        crate::squad_store::load()
            .squads
            .iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident1),
        "row 1 written before the removal"
    );

    core.clients.push(client(1, 5, (24, 80), false));
    let flow = core.command(1, Command::RemoveSquad(1));
    assert!(matches!(flow, Flow::Continue));
    assert_eq!(core.session.squads.len(), 1);
    assert_eq!(core.session.squad(1), None);
    assert_eq!(
        core.session.active_squad,
        Some(2),
        "active re-anchors to a survivor"
    );

    core.clients.push(client(1, 6, (24, 80), false));
    let flow = core.command(1, Command::RemoveSquad(2));
    assert!(
        matches!(flow, Flow::Shutdown),
        "removing the last squad ends the session (Locked Decision 8)"
    );
    assert!(core.session.squads.is_empty());
    let rows = crate::squad_store::load().squads;
    assert!(
        !rows
            .iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident1
                || (s.name.clone(), s.key.clone()) == ident2),
        "both dismissed workspaces left the store, SessionEmpty included"
    );
}

#[test]
fn close_tab_depersists_a_memberless_workspace() {
    // The reported leak: a workspace of plain shell panes has no member
    // context, so CloseTab's reconcile loop did nothing and the row
    // survived in the store. Positives on both sides: the closed identity
    // is gone AND the sibling's row is still there.
    let _s = StoreScratch::new("x361b-closetab");
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], Some("one".into()), leaf_tab(5, 1));
    core.session
        .add_squad(2, vec!["/b".into()], Some("two".into()), leaf_tab(6, 2));
    core.persist_squad(1);
    core.persist_squad(2);
    let ident1 = core.squad_identity(1).expect("identity before close");
    let ident2 = core.squad_identity(2).expect("identity before close");
    assert!(
        crate::squad_store::load()
            .squads
            .iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident1),
        "row written before the close"
    );

    core.clients.push(client(1, 5, (24, 80), false));
    let flow = core.command(1, Command::CloseTab);
    assert!(matches!(flow, Flow::Continue), "squad 2 keeps the session");
    let rows = crate::squad_store::load().squads;
    assert!(
        !rows
            .iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident1),
        "the memberless workspace's row left the store"
    );
    assert!(
        rows.iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident2),
        "the sibling's row survives"
    );
}

#[test]
fn closing_a_shell_tab_writes_its_tree_removal() {
    // x-9052 AC4-EDGE: a shell-only tab close used to persist NOTHING on
    // a surviving squad, so its stored tree replayed at every restart.
    // Positives both sides: the closed tree leaves tab_trees, the
    // sibling's tree stays.
    let _s = StoreScratch::new("x9052-shellclose");
    let mut core = empty_core();
    let mut one = leaf_tab(5, 1);
    let two = leaf_tab(6, 2);
    one.name = Some("shell-a".into());
    core.session
        .add_squad(1, vec!["/a".into()], Some("one".into()), one);
    core.session.squad_mut(1).expect("squad").tabs.push(two);
    core.persist_squad(1);
    let ident = core.squad_identity(1).expect("identity");
    assert_eq!(
        crate::squad_store::load()
            .squads
            .iter()
            .find(|s| (s.name.clone(), s.key.clone()) == ident)
            .map(|s| s.tab_trees.len()),
        Some(2),
        "both trees captured before the close"
    );
    core.close_tab_cascade(1, 1);
    assert_eq!(
        crate::squad_store::load()
            .squads
            .iter()
            .find(|s| (s.name.clone(), s.key.clone()) == ident)
            .map(|s| s.tab_trees.len()),
        Some(1),
        "the closed tab's tree left the store"
    );
}

#[test]
fn churn_writes_the_tree_removal_when_the_squad_survives() {
    // x-9052 AC4-HP: a churned worker's persist used to be members-only,
    // so its collapsed tab stayed in tab_trees forever.
    let _s = StoreScratch::new("x9052-churn");
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], Some("one".into()), leaf_tab(5, 1));
    let member = crate::squad_store::StoredMember {
        attach_id: "a1b2c3d4".into(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: None,
        harness: None,
        harness_session_id: None,
        pane_id: None,
    };
    core.squad_members.insert(1, vec![member.clone()]);
    core.attached.insert("a1b2c3d4".into(), 5);
    core.persist_squad(1);
    let ident = core.squad_identity(1).expect("identity");
    assert_eq!(
        crate::squad_store::load()
            .squads
            .iter()
            .find(|s| (s.name.clone(), s.key.clone()) == ident)
            .map(|s| s.tab_trees.len()),
        Some(1),
        "tree captured while the member pane lives"
    );
    // The pane and its tab die with the server's session bookkeeping;
    // the squad survives (a sibling tab keeps it).
    let two = leaf_tab(6, 2);
    core.session.squad_mut(1).expect("squad").tabs.push(two);
    core.session.remove_tab(1, 0);
    core.attached.remove("a1b2c3d4");
    let ctx = (
        1u64,
        ident.0.clone(),
        ident.1.clone(),
        vec!["/a".into()],
        "a1b2c3d4".to_string(),
    );
    core.reconcile_member_close(Some(ctx), true);
    assert_eq!(
        crate::squad_store::load()
            .squads
            .iter()
            .find(|s| (s.name.clone(), s.key.clone()) == ident)
            .map(|s| s.tab_trees.len()),
        Some(1),
        "the churned member's tree left the store; the sibling's stays"
    );
}

#[test]
fn prune_done_slots_collapses_and_skips() {
    // x-9052 AC3-EDGE: pure collapse semantics - a done leaf vanishes, a
    // one-child split unwraps, an emptied tree is None, weights and
    // siblings survive.
    let done: HashSet<String> = ["gone-slot".to_string()].into_iter().collect();
    let tree = LayoutTreeSpec::Split {
        axis: crate::tree::Axis::Horizontal,
        children: vec![
            LayoutTreeChild {
                weight: 2.0,
                tree: LayoutTreeSpec::Slot("gone-slot".into()),
            },
            LayoutTreeChild {
                weight: 1.0,
                tree: LayoutTreeSpec::Slot("kept".into()),
            },
        ],
    };
    let pruned = prune_done_slots(&tree, &done).expect("one leaf survives");
    match pruned {
        LayoutTreeSpec::Slot(name) => assert_eq!(name, "kept"),
        other => panic!("one-child split should unwrap, got {other:?}"),
    }
    let all_done: HashSet<String> = ["gone-slot".to_string(), "kept".to_string()]
        .into_iter()
        .collect();
    assert!(prune_done_slots(&tree, &all_done).is_none());
}

#[test]
fn close_last_pane_depersists_its_workspace() {
    // Same contract through close_pane (the mouse pane-close path), which
    // de-persisted never before: it handled template specs and nothing
    // else when remove_tab dropped the squad.
    let _s = StoreScratch::new("x361b-closepane");
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], Some("one".into()), leaf_tab(5, 1));
    core.session
        .add_squad(2, vec!["/b".into()], Some("two".into()), leaf_tab(6, 2));
    core.persist_squad(1);
    core.persist_squad(2);
    let ident1 = core.squad_identity(1).expect("identity before close");
    let ident2 = core.squad_identity(2).expect("identity before close");

    let flow = core.close_pane(1);
    assert!(matches!(flow, Flow::Continue), "squad 2 keeps the session");
    let rows = crate::squad_store::load().squads;
    assert!(
        !rows
            .iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident1),
        "closing the last pane cleared the workspace's row"
    );
    assert!(
        rows.iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident2),
        "the sibling's row survives"
    );
}

#[test]
fn remove_squad_depersists_an_untracked_workspace() {
    // The gate this drops: a squad the store holds but squad_members does
    // not (restore's per-squad isolation can produce exactly that) took
    // the false branch and its row survived the dismiss.
    let _s = StoreScratch::new("x361b-remove-untracked");
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], Some("one".into()), leaf_tab(5, 1));
    core.persist_squad(1);
    let ident1 = core.squad_identity(1).expect("identity before removal");
    assert!(
        !core.squad_members.contains_key(&1),
        "precondition: the squad is in the store but untracked"
    );

    core.clients.push(client(1, 5, (24, 80), false));
    let flow = core.command(1, Command::RemoveSquad(1));
    assert!(matches!(flow, Flow::Shutdown));
    let rows = crate::squad_store::load().squads;
    assert!(
        !rows
            .iter()
            .any(|s| (s.name.clone(), s.key.clone()) == ident1),
        "the untracked workspace's row left the store"
    );
}

#[test]
fn remove_squad_unknown_id_is_refused_without_mutation() {
    // AC2-ERR: a RemoveSquad naming a dead id touches nothing.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    core.clients.push(client(1, 5, (24, 80), false));
    let flow = core.command(1, Command::RemoveSquad(999));
    assert!(matches!(flow, Flow::Continue));
    assert_eq!(core.session.squads.len(), 1, "no squad removed");
}

#[test]
fn move_squad_reorders_and_edge_bump_is_silent_noop() {
    // AC3-HP + Boundaries: reorder clamps to the list, and an at-edge move
    // is a silent no-op (holding a reorder key at the top must not churn).
    let mut core = empty_core();
    for (sid, tid, pid) in [(1u64, 5u64, 1u64), (2, 6, 2), (3, 7, 3)] {
        core.session
            .add_squad(sid, vec![format!("/{sid}")], None, leaf_tab(tid, pid));
    }
    let order = |c: &Core| c.session.squads.iter().map(|s| s.id).collect::<Vec<_>>();

    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::MoveSquad {
            squad: 3,
            delta: -1,
        },
    );
    assert_eq!(order(&core), vec![1, 3, 2], "squad 3 moved up one");

    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::MoveSquad {
            squad: 1,
            delta: -1,
        },
    );
    assert_eq!(
        order(&core),
        vec![1, 3, 2],
        "an at-edge bump changes nothing"
    );
}

#[test]
fn reorder_tab_moves_within_its_squad_and_keeps_the_same_tab_active() {
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    core.session
        .squad_mut(1)
        .unwrap()
        .tabs
        .extend([leaf_tab(6, 2), leaf_tab(7, 3)]);
    core.session.squad_mut(1).unwrap().active_tab = 1;
    let (client, mut rx) = client_with_rx(1);
    core.clients.push(client);

    core.command(
        1,
        Command::ReorderTab {
            squad: 1,
            tab: 6,
            delta: 1,
        },
    );

    let squad = core.session.squad(1).unwrap();
    assert_eq!(
        squad.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        vec![5, 7, 6]
    );
    assert_eq!(squad.tabs[squad.active_tab].id, 6);
    assert!(rx.try_recv().is_ok(), "a successful reorder pushes Layout");
}

#[test]
fn tab_reorder_control_verb_lands_at_the_named_position() {
    // (x-cf97) The `fno mux tab move` door: `to` names a 1-based POSITION,
    // the server computes the delta, and the same trunk moves the tab
    // while holding the squad's active tab. The ordinal grammar is the
    // shared one: 0 is a refusal, never a silent zero-based index
    // (x-1499), and past-the-end is refused rather than clamped.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    core.session
        .squad_mut(1)
        .unwrap()
        .tabs
        .extend([leaf_tab(6, 2), leaf_tab(7, 3)]);
    core.session.squad_mut(1).unwrap().active_tab = 2;

    core.tab_reorder(
        &PaneTarget::SquadId(1),
        &TabSel::Index(3),
        &TabSel::Index(1),
    )
    .unwrap();
    let squad = core.session.squad(1).unwrap();
    assert_eq!(
        squad.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        vec![7, 5, 6]
    );
    assert_eq!(
        squad.tabs[squad.active_tab].id, 7,
        "the active tab survives the move"
    );

    assert!(
        core.tab_reorder(
            &PaneTarget::SquadId(1),
            &TabSel::Index(0),
            &TabSel::Index(1)
        )
        .is_err(),
        "ordinal 0 is refused"
    );
    assert!(
        core.tab_reorder(
            &PaneTarget::SquadId(1),
            &TabSel::Index(1),
            &TabSel::Index(9)
        )
        .is_err(),
        "past-the-end is refused"
    );
}

#[test]
fn reorder_tab_at_an_edge_is_a_silent_noop() {
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    core.session.squad_mut(1).unwrap().tabs.push(leaf_tab(6, 2));
    let (client, mut rx) = client_with_rx(1);
    core.clients.push(client);

    core.command(
        1,
        Command::ReorderTab {
            squad: 1,
            tab: 5,
            delta: -1,
        },
    );
    core.command(
        1,
        Command::ReorderTab {
            squad: 1,
            tab: 6,
            delta: 1,
        },
    );

    let squad = core.session.squad(1).unwrap();
    assert_eq!(
        squad.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        vec![5, 6]
    );
    assert!(
        rx.try_recv().is_err(),
        "edge bumps push neither Layout nor Notice"
    );
}

#[test]
fn reorder_tab_recovers_from_an_invalid_active_index() {
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    let squad = core.session.squad_mut(1).unwrap();
    squad.tabs.push(leaf_tab(6, 2));
    squad.active_tab = usize::MAX;
    core.clients.push(client(1, 5, (24, 80), false));

    core.command(
        1,
        Command::ReorderTab {
            squad: 1,
            tab: 5,
            delta: 1,
        },
    );

    let squad = core.session.squad(1).unwrap();
    assert_eq!(
        squad.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        vec![6, 5]
    );
    assert_eq!(squad.active_tab, 1);
}

#[test]
fn reorder_tab_refuses_a_stale_tab_id() {
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    let (client, mut rx) = client_with_rx(1);
    core.clients.push(client);

    core.command(
        1,
        Command::ReorderTab {
            squad: 1,
            tab: 999,
            delta: 1,
        },
    );

    assert_eq!(core.session.find_tab(5), Some((1, 0)));
    assert_eq!(drain_notice(&mut rx).as_deref(), Some("no such tab"));
}

#[test]
fn reorder_tab_refuses_when_the_tab_moved_to_another_squad() {
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    core.session.squad_mut(1).unwrap().tabs.push(leaf_tab(6, 2));
    core.session
        .add_squad(2, vec!["/b".into()], None, leaf_tab(7, 3));
    core.session.squad_mut(2).unwrap().tabs.push(leaf_tab(8, 4));
    let (client, mut rx) = client_with_rx(1);
    core.clients.push(client);

    core.command(1, Command::MoveTab { tab: 6, squad: 2 });
    while rx.try_recv().is_ok() {}
    core.command(
        1,
        Command::ReorderTab {
            squad: 1,
            tab: 6,
            delta: -1,
        },
    );

    assert_eq!(
        core.session
            .squad(2)
            .unwrap()
            .tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<Vec<_>>(),
        vec![7, 8, 6],
        "a stale reorder must not mutate the destination squad"
    );
    assert!(drain_notice(&mut rx).unwrap().contains("moved"));
}

#[test]
fn move_tab_follows_the_viewing_client_into_dst() {
    // Invariant (view validity): a viewer of the moved tab follows it into
    // the destination squad - content continuity beats spatial position.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
    core.session
        .add_squad(2, vec!["/b".into()], None, leaf_tab(6, 2));
    // A live receiver so push_layout does not reap the client before we
    // read its post-move view.
    let (tx, _rx) = mpsc::channel(8);
    core.clients.push(Client {
        id: 1,
        reliable_tx: tx,
        dirty: Arc::default(),
        notify: Arc::new(Notify::new()),
        synced_modes: Modes::default(),
        view: (1, 5),
        visible: HashSet::new(),
        dims: (24, 80),
        passive: false,
        last_press: None,
    });

    core.command(1, Command::MoveTab { tab: 5, squad: 2 });
    assert_eq!(
        core.session.find_tab(5),
        Some((2, 1)),
        "tab 5 re-homed into squad 2"
    );
    assert_eq!(
        core.clients[0].view,
        (2, 5),
        "the viewer follows the moved tab into its new squad"
    );
}

#[test]
fn attach_agent_refuses_unknown_or_malformed_jobid() {
    // The jobId lands in `claude attach <id>`'s argv, so an out-of-shape id
    // is refused before any pane spawns (argv defense in depth). A
    // well-formed id that names no surfaced watch-only row is refused too
    // (catalog membership, like the sibling FocusPane/SelectTab commands) -
    // here `empty_core` has no agents, so even valid-shape "deadbeef" fails.
    for bad in [
        "short",
        "toolongxx",
        "ZZZZZZZZ",
        "; rm -rf /",
        "c19cd2c",
        "deadbeef",
    ] {
        let mut core = empty_core();
        let flow = core.command(1, Command::attach_agent(bad));
        assert!(matches!(flow, Flow::Continue));
        assert!(
            core.panes.is_empty(),
            "un-surfaced/malformed jobId {bad:?} must not spawn a pane"
        );
    }
}

pub(super) fn client_with_rx(id: u64) -> (Client, mpsc::Receiver<ServerMsg>) {
    let (tx, rx) = mpsc::channel::<ServerMsg>(8);
    let mut c = client(id, 5, (24, 80), false);
    c.reliable_tx = tx;
    (c, rx)
}

fn drain_notice(rx: &mut mpsc::Receiver<ServerMsg>) -> Option<String> {
    let mut out = None;
    while let Ok(ServerMsg::Notice { text }) = rx.try_recv() {
        out = Some(text);
    }
    out
}

#[test]
fn stop_agent_unknown_name_refused() {
    // US5 / AC3-ERR: a StopAgent naming a row absent from the catalog is
    // refused fail-closed with a notice. A plain #[test] has no tokio
    // runtime, so reaching agent_action's spawn would panic - the clean
    // refusal is also proof the happy path is never taken here.
    let mut core = empty_core();
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    let flow = core.command(
        1,
        Command::StopAgent {
            harness_session_id: None,
            name: "ghost".into(),
            pane_id: None,
        },
    );
    assert!(matches!(flow, Flow::Continue));
    assert!(drain_notice(&mut rx).unwrap().contains("no such agent"));
}

/// A helper for the respawn refusal tests: an EXITED registry row with an
/// optional recorded claude session uuid.
pub(super) fn exited_claude_row(name: &str, uuid: Option<&str>) -> RegistryAgent {
    RegistryAgent {
        model: None,
        route: None,
        spawned_by_session: None,
        session_id: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        related_session_id: None,
        forked_from_session_id: None,
        name: name.into(),
        cwd: "/w".into(),
        exited: true,
        dnd: false,
        liveness: agents_view::Liveness::Dead,
        badge: None,
        reason: None,
        mux: None,
        answerable: None,
        attach_id: None,
        external: false,
        account: None,
        claude_session_uuid: uuid.map(str::to_owned),
        log_path: None,
        updated_at: None,
        crown_level: None,
        crown_scope: None,
        harness: None,
        ..Default::default()
    }
}

#[test]
fn run_pane_with_worker_records_a_resumable_member() {
    // x-5f7f task 1, positive marker: a pane run carrying --worker records
    // a StoredMember joined to that registry NAME, in the store, for the
    // squad the pane landed in. This is the capture funnel the empty
    // squads lacked - before the change the squad row lands with
    // members: [].
    let _s = StoreScratch::new("run-pane-worker");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut row = exited_claude_row("probe-x5f7f", None);
    row.harness = Some("codex".into());
    row.harness_session_id = Some("01a03a85-1111-7222-8333-444455556666".into());
    core.agents = vec![row];
    let pid = core
        .run_pane(
            "/repo/proj".into(),
            "/repo/proj".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("work".into()),
                ..Default::default()
            },
            Some("probe-x5f7f".into()),
        )
        .unwrap();
    assert!(core.panes.contains_key(&pid));
    let stored = crate::squad_store::load();
    let sq = stored
        .squads
        .iter()
        .find(|s| s.name == "work")
        .expect("the named squad persisted");
    let member = sq
        .members
        .iter()
        .find(|m| m.worker.is_some())
        .expect("a worker member was recorded");
    assert_eq!(member.worker.as_deref(), Some("probe-x5f7f"));
    assert_eq!(member.attach_id, "", "no claude jobId on a worker member");
    assert_eq!(
        member.cwd.as_deref(),
        Some("/repo/proj"),
        "the spawn cwd is captured for the resume"
    );
    assert_eq!(
        member.harness_session_id.as_deref(),
        Some("01a03a85-1111-7222-8333-444455556666"),
        "the full resume key survives registry-row loss"
    );
}

#[test]
fn run_pane_without_worker_records_no_member() {
    // x-5f7f task 1, the no-flag acceptance: a plain pane run stays
    // byte-identical - the squad persists, membership stays empty.
    let _s = StoreScratch::new("run-pane-plain");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    core.run_pane(
        "/repo/proj".into(),
        "/repo/proj".into(),
        vec!["/bin/cat".into()],
        24,
        80,
        false,
        PanePlacement {
            target: PaneTarget::SquadName("work".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let stored = crate::squad_store::load();
    let sq = stored
        .squads
        .iter()
        .find(|s| s.name == "work")
        .expect("the named squad persisted");
    assert!(
        sq.members.is_empty(),
        "a plain run records no member: {:?}",
        sq.members
    );
}

#[test]
fn run_pane_refuses_a_hostile_worker_name_before_spawning() {
    // The server-side gate: the control socket is reachable by any client,
    // so the CLI's own --worker validation is not the authority. A hostile
    // name is refused with no pane spawned and no squad minted.
    let _s = StoreScratch::new("run-pane-hostile");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let err = core
        .run_pane(
            "/repo/proj".into(),
            "/repo/proj".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("work".into()),
                ..Default::default()
            },
            Some("a;rm -rf".into()),
        )
        .unwrap_err();
    assert_eq!(err.0, err_code::BAD_REQUEST);
    assert!(core.panes.is_empty(), "no pane spawned");
    assert!(core.session.squads.is_empty(), "no squad minted");
}

#[test]
fn refused_placeholder_marker_roundtrips_through_argv() {
    // AC8-HP: the keeper re-adoption parse recovers the refused worker.
    let argv = vec![
        "env".to_string(),
        "FNO_REFUSED_WORKER=t-a0cd-identity-residue-gpt".to_string(),
        "/bin/sh".to_string(),
    ];
    assert_eq!(
        refused_worker_from_argv(&argv).as_deref(),
        Some("t-a0cd-identity-residue-gpt")
    );
    // AC11-EDGE: a bare shell is never mistaken for a placeholder.
    assert_eq!(refused_worker_from_argv(&["/bin/sh".to_string()]), None);
    // An unrelated env token does not match either.
    let other = vec![
        "env".to_string(),
        "FNO_NODE=ab-1234".to_string(),
        "/bin/sh".to_string(),
    ];
    assert_eq!(refused_worker_from_argv(&other), None);
}

#[test]
fn refused_placeholder_mints_through_the_real_spawn_path() {
    // The mint goes through the argv path (not the bare shell spawn), so
    // the registered pane derives its own refusal marker from the env
    // wrapper token.
    let mut core = empty_core();
    core.shells = vec!["/bin/sh".into()];
    let pid = core
        .refused_worker_pane("w", "test reason", 24, 80, "/tmp")
        .expect("placeholder spawns");
    let entry = core.panes.get(&pid).expect("registered");
    assert_eq!(entry.refused_worker.as_deref(), Some("w"));
}

#[test]
fn worker_restore_match_prefers_the_persisted_identity_pair_over_name() {
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("reused-name".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("old-session".into()),
        pane_id: None,
    };
    let mut wrong = bg_row("reused-name", "/repo", None);
    wrong.harness = Some("codex".into());
    wrong.harness_session_id = Some("new-session".into());
    let mut right = wrong.clone();
    right.harness_session_id = Some("old-session".into());
    assert!(!worker_registry_match(&member, &wrong, "reused-name"));
    assert!(worker_registry_match(&member, &right, "reused-name"));
}

#[test]
fn member_resume_facts_survive_reaped_row_and_purged_receipt() {
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: Some("/Users/wt/worker".into()),
        worker: Some("t-worker".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("01a04191-07ec-7080-aa78-843eb56996e5".into()),
        pane_id: None,
    };
    let facts = Core::member_resume_facts(&member, "t-worker").expect("durable member");
    assert_eq!(facts.harness, "codex");
    assert_eq!(
        facts.harness_session_id,
        "01a04191-07ec-7080-aa78-843eb56996e5"
    );
    assert_eq!(facts.cwd, "/Users/wt/worker");
    assert_eq!(facts.name, "t-worker");

    // A member with no harness resume form must not mint facts: the pane
    // it would feed has no resume command to run. The table declares a
    // form for every harness today, so the negative arm is simulated by
    // overriding agy's availability to none.
    let mut agy = member.clone();
    agy.harness = Some("agy".into());
    {
        let _guard = DeclaredResumeFormsGuard;
        set_declared_resume_form("agy", None);
        assert!(Core::member_resume_facts(&agy, "t-worker").is_none());
    }
    // x-7b5e: with the declared form restored, an agy member mints facts
    // like any other declared harness - the old two-arm match skipped it.
    assert!(Core::member_resume_facts(&agy, "t-worker").is_some());

    let mut no_id = member;
    no_id.harness_session_id = None;
    assert!(Core::member_resume_facts(&no_id, "t-worker").is_none());
}

#[test]
fn spawn_receipt_removal_events_revoke_resume_facts() {
    let raw = concat!(
        r#"{"type":"agent_spawned","data":{"name":"removed","provider":"codex","harness_session_id":"removed-session","cwd":"/repo","substrate":"pane"}}"#,
        "\n",
        r#"{"type":"agent_removed","data":{"name":"removed","provider":"codex","harness_session_id":"removed-session"}}"#,
        "\n",
        r#"{"type":"agent_spawned","data":{"name":"reaped","provider":"codex","harness_session_id":"reaped-session","cwd":"/repo","substrate":"pane"}}"#,
        "\n",
        r#"{"type":"agent_row_reaped","data":{"name":"reaped","provider":"codex","harness_session_id":"reaped-session"}}"#,
    );
    assert!(parse_spawn_receipts(raw).is_empty());

    let dormant = concat!(
        r#"{"type":"agent_spawned","data":{"name":"dormant","provider":"claude","harness_session_id":"dormant-session","cwd":"/repo","substrate":"pane"}}"#,
        "\n",
        r#"{"type":"agent_row_reaped","data":{"name":"dormant","provider":"claude","harness_session_id":"dormant-session","resumable":true}}"#,
    );
    assert_eq!(parse_spawn_receipts(dormant).len(), 1);
}

#[test]
fn existing_worker_member_persists_a_new_session_identity() {
    let _scratch = StoreScratch::new("member-session-id");
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/repo".into()],
        Some("workers".into()),
        leaf_tab(1, 1),
    );
    core.squad_members.insert(
        1,
        vec![crate::squad_store::StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("worker".into()),
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );

    core.record_worker_member(1, "worker", 1, "/repo", Some("session-new"));

    let stored = crate::squad_store::load();
    assert_eq!(
        stored.squads[0].members[0].harness_session_id.as_deref(),
        Some("session-new")
    );
}

#[test]
fn focusing_a_held_pane_refuses_a_session_that_became_live() {
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(24, 80, "/tmp").unwrap();
    core.session
        .add_squad(1, vec!["/tmp".into()], None, leaf_tab(1, pid));
    core.held_workers.insert(
        pid,
        HeldWorker {
            name: "live-worker".into(),
            harness: "codex".into(),
            harness_session_id: "full-session".into(),
            cwd: "/tmp".into(),
        },
    );
    let mut live = bg_row("renamed-live-worker", "/tmp", None);
    live.harness = Some("codex".into());
    live.harness_session_id = Some("full-session".into());
    core.agents = vec![live];
    let (mut client, mut rx) = client_with_rx(1);
    client.view = (1, 1);
    core.clients.push(client);

    core.command(1, Command::FocusPane(pid));

    assert!(
        core.panes.contains_key(&pid),
        "the refusal keeps its named shell"
    );
    assert!(core.worker_pane.is_empty(), "no second writer is spawned");
    assert!(
        !core.held_workers.contains_key(&pid),
        "the refusal is one-shot"
    );
    assert!(
        core.panes[&pid].vt.text().contains("live elsewhere"),
        "the pane itself carries the refusal"
    );
    assert!(
        drain_notices(&mut rx).join("\n").contains("live elsewhere"),
        "the client receives the same reason"
    );
}

#[test]
fn resume_agent_spawns_the_harness_form_and_records_the_member() {
    // x-5f7f: a dead paneless codex row resumes through codex's own form
    // in the recorded cwd. x-eb79: the argv now resolves off-loop (the
    // staged seam here), so the test stages what `fno-agents resume-argv`
    // would return; /bin/cat keeps the spawn hermetic.
    let _guard = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-resume-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let shell = core
        .spawn_pane(24, 80, cwd.to_string_lossy().as_ref())
        .unwrap();
    core.session.add_squad(
        7,
        vec![cwd.to_string_lossy().into_owned()],
        None,
        Tab {
            name: None,
            id: 70,
            root: Node::Leaf(shell),
            focus: shell,
        },
    );
    core.agents = vec![RegistryAgent {
        harness_session_id: Some("01a027ad-fe00-7c12-a116-9ee37c6bdfec".into()),
        harness: Some("codex".into()),
        name: "t-codex-one".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        exited: true,
        liveness: agents_view::Liveness::Dead,
        ..Default::default()
    }];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    // The gesture consumes the staged argv (what the off-loop
    // `fno-agents resume-argv` shell-out would deliver).
    core.staged_resume_argv = Some(vec![
        "/bin/cat".into(),
        "01a027ad-fe00-7c12-a116-9ee37c6bdfec".into(),
    ]);
    core.command(
        1,
        Command::ResumeAgent {
            name: "t-codex-one".into(),
        },
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(notices.contains("resumed t-codex-one"), "{notices}");
    // One NEW pane beyond the seed shell, running the staged program,
    // titled from the registry row, placed in the squad owning the cwd.
    let new_panes: Vec<&u64> = core.panes.keys().filter(|&&p| p != shell).collect();
    assert_eq!(new_panes.len(), 1, "exactly one resumed pane");
    let entry = core.panes.get(new_panes[0]).unwrap();
    assert_eq!(entry.cmd.as_deref(), Some("cat"), "the override ran");
    assert_eq!(
        entry.name.as_deref(),
        Some("t-codex-one"),
        "the pane is titled from the registry row"
    );
    let members = core.squad_members.get(&7).expect("member recorded");
    assert!(
        members
            .iter()
            .any(|m| m.worker.as_deref() == Some("t-codex-one")),
        "the resumed pane persists as a worker member: {members:?}"
    );
}

#[test]
fn resume_agent_twice_focuses_the_existing_pane() {
    // The external-review P1: the resume argv carries no registry binding,
    // so before the worker_pane map a second Resume for the same row
    // launched a SECOND session on the same rollout. The map binds row to
    // pane for the pane's lifetime; a second gesture focuses, and the
    // panel presents the row pane-hosted while it lives. x-eb79: the
    // argv arrives through the staged seam (what `fno-agents
    // resume-argv` would deliver), so /bin/cat keeps the spawn hermetic.
    let _guard = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-resume-cwd2");
    std::fs::create_dir_all(&cwd).unwrap();
    let shell = core
        .spawn_pane(24, 80, cwd.to_string_lossy().as_ref())
        .unwrap();
    core.session.add_squad(
        7,
        vec![cwd.to_string_lossy().into_owned()],
        None,
        Tab {
            name: None,
            id: 70,
            root: Node::Leaf(shell),
            focus: shell,
        },
    );
    core.agents = vec![RegistryAgent {
        harness_session_id: Some("01a027ad-fe00-7c12-a116-9ee37c6bdfec".into()),
        harness: Some("codex".into()),
        name: "t-codex-one".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        exited: true,
        liveness: agents_view::Liveness::Dead,
        ..Default::default()
    }];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.staged_resume_argv = Some(vec![
        "/bin/cat".into(),
        "01a027ad-fe00-7c12-a116-9ee37c6bdfec".into(),
    ]);
    core.command(
        1,
        Command::ResumeAgent {
            name: "t-codex-one".into(),
        },
    );
    drain_notices(&mut rx);
    let first: Vec<u64> = core.panes.keys().copied().filter(|&p| p != shell).collect();
    assert_eq!(first.len(), 1, "the first resume spawns one pane");
    core.command(
        1,
        Command::ResumeAgent {
            name: "t-codex-one".into(),
        },
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("already resumed; focused existing pane"),
        "{notices}"
    );
    let second: Vec<u64> = core.panes.keys().copied().filter(|&p| p != shell).collect();
    assert_eq!(second, first, "the second resume spawns nothing");
    // The panel presents the row pane-hosted through the same map.
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "t-codex-one").unwrap();
    assert_eq!(
        row.pane_id,
        Some(first[0]),
        "the row is pane-hosted, not idle"
    );
    // Pane death releases the binding: the row returns to idle.
    core.reap_pane(first[0]);
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "t-codex-one").unwrap();
    assert_eq!(
        row.pane_id, None,
        "the row returns to idle when the pane dies"
    );
}

#[test]
fn resume_agent_refusals_name_the_reason() {
    // Fail-closed catalog gates, same posture as AttachAgent: an unknown
    // name, and a row whose pane is still live (focus, never a second
    // spawn), never reach an argv.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let live = core.spawn_pane(24, 80, "/tmp").unwrap();
    core.session.add_squad(
        7,
        vec!["/tmp".into()],
        None,
        Tab {
            name: None,
            id: 70,
            root: Node::Leaf(live),
            focus: live,
        },
    );
    let live_row = RegistryAgent {
        harness_session_id: Some("01a027ad-fe00-7c12-a116-9ee37c6bdfec".into()),
        harness: Some("codex".into()),
        name: "live-codex".into(),
        cwd: "/tmp".into(),
        mux: Some(("test".into(), live)),
        liveness: agents_view::Liveness::Alive,
        ..Default::default()
    };
    core.agents = vec![live_row];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::ResumeAgent {
            name: "ghost".into(),
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("no such agent"));
    core.command(
        1,
        Command::ResumeAgent {
            name: "live-codex".into(),
        },
    );
    assert!(
        drain_notice(&mut rx).unwrap().contains("live pane"),
        "a row with a live pane is refused, never double-spawned"
    );
    assert_eq!(core.panes.len(), 1, "nothing was spawned by either refusal");
}

#[test]
fn row_resume_disposition_gates_on_harness_form_and_session_id() {
    // One table of registry facts owns both the dead-row resume decision
    // and the branch-four reason. A LIVE claude bg row with a jobId still
    // uses attach, but its disposition remains live-paneless.
    let base = || RegistryAgent {
        harness_session_id: Some("01a027ad".into()),
        harness: Some("codex".into()),
        name: "w".into(),
        cwd: "/w".into(),
        exited: true,
        liveness: agents_view::Liveness::Alive,
        ..Default::default()
    };
    assert_eq!(
        Core::row_resume_disposition(&base()),
        RowResumeDisposition::Resumable
    );
    assert!(Core::row_resumable(&base()), "a dead codex row resumes");
    let mut live_codex = base();
    live_codex.exited = false;
    assert_eq!(
        Core::row_resume_disposition(&live_codex),
        RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless)
    );
    assert!(
            !Core::row_resumable(&live_codex),
            "a live codex row has a process writing its rollout: resuming under it opens a second writer"
        );
    let mut not_exited = base();
    not_exited.exited = false;
    not_exited.liveness = agents_view::Liveness::Unmeasured;
    assert!(
        !matches!(
            Core::row_resume_disposition(&not_exited),
            RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless)
        ),
        "an unmeasured backend must not be labeled live"
    );
    assert_eq!(
        Core::row_resume_disposition(&not_exited),
        RowResumeDisposition::NoPane(AgentNoPaneReason::LivenessUnmeasured),
        "(x-d401) unmeasured names the absent reading, not a dead backend"
    );
    not_exited.liveness = agents_view::Liveness::Dead;
    assert_eq!(
        Core::row_resume_disposition(&not_exited),
        RowResumeDisposition::Resumable,
        "a positive dead reading resumes whatever the status word says"
    );
    let mut agy = base();
    agy.harness = Some("agy".into());
    agy.harness_session_id = None;
    // The no-form arm needs an override: the table declares interactive_resume
    // for agy today (x-7b5e), so a bare agy row now fails on its missing
    // session id instead.
    {
        let _guard = DeclaredResumeFormsGuard;
        set_declared_resume_form("agy", None);
        assert_eq!(
            Core::row_resume_disposition(&agy),
            RowResumeDisposition::NoPane(AgentNoPaneReason::UnsupportedHarness)
        );
        assert!(
            !Core::row_resumable(&agy),
            "no resume form and no session id: no Resume offered"
        );
    }
    // With the declared form, the same row fails one step later - and a
    // dead agy row with a session id IS resumable, which is the AC3-HP
    // change: the old two-arm match offered no Resume at all.
    assert_eq!(
        Core::row_resume_disposition(&agy),
        RowResumeDisposition::NoPane(AgentNoPaneReason::MissingSessionId)
    );
    let mut dead_agy = base();
    dead_agy.harness = Some("agy".into());
    assert_eq!(
        Core::row_resume_disposition(&dead_agy),
        RowResumeDisposition::Resumable,
        "a declared harness with a session id is resumable without a Rust change"
    );
    let mut live_claude = base();
    live_claude.harness = Some("claude".into());
    live_claude.exited = false;
    live_claude.attach_id = Some("c19cd2c3".into());
    assert_eq!(
        Core::row_resume_disposition(&live_claude),
        RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless)
    );
    assert!(
        !Core::row_resumable(&live_claude),
        "a live claude bg row attaches; its daemon owns the session"
    );
    let mut dead_claude = live_claude;
    dead_claude.exited = true;
    assert!(
        Core::row_resumable(&dead_claude),
        "a dead claude row resumes from its transcript"
    );
    let mut no_sid = base();
    no_sid.harness_session_id = None;
    assert_eq!(
        Core::row_resume_disposition(&no_sid),
        RowResumeDisposition::NoPane(AgentNoPaneReason::MissingSessionId)
    );
    assert!(
        !Core::row_resumable(&no_sid),
        "no session id means nothing to resume"
    );
    let mut no_harness = base();
    no_harness.harness = None;
    assert_eq!(
        Core::row_resume_disposition(&no_harness),
        RowResumeDisposition::NoPane(AgentNoPaneReason::MissingHarness)
    );
}

#[test]
fn resume_target_from_argv_parses_both_harness_forms_anchored() {
    // (x-d401) The row-to-pane join key: the session id a pane-run argv
    // resumes. Claude's flag form, codex's subcommand form, both behind
    // the env(1) wrapper; a command that merely mentions the token never
    // parses.
    use super::resume_target_from_argv;
    let argv = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        resume_target_from_argv(&argv(&["claude", "--resume", "01a03a4e-b862"])),
        Some("01a03a4e-b862".into())
    );
    assert_eq!(
        resume_target_from_argv(&argv(&["codex", "resume", "f00dcaf3"])),
        Some("f00dcaf3".into())
    );
    assert_eq!(
        resume_target_from_argv(&argv(&[
            "env",
            "FNO_NODE=x-9d03",
            "FNO_AGENT_SELF=peer",
            "claude",
            "--resume",
            "01a03a4e-b862"
        ])),
        Some("01a03a4e-b862".into())
    );
    assert_eq!(
        resume_target_from_argv(&argv(&["grep", "--resume", "file"])),
        None,
        "a non-harness command never parses"
    );
    assert_eq!(
        resume_target_from_argv(&argv(&["claude", "--resume"])),
        None,
        "a resume flag with no following token parses nothing"
    );
    // (x-d401) A FLAG is not a session id. `codex resume --last` names no
    // session, so storing `--last` yields a join key matching no row, and
    // `pane_resumes_session` then fails to keep that row non-resumable
    // while a pane runs it. One tap opens a second writer on a live
    // rollout, so this filter is a safety guard, not tidiness.
    assert_eq!(
        resume_target_from_argv(&argv(&["codex", "resume", "--last"])),
        None,
        "a flag is not a session id"
    );
    assert_eq!(
        resume_target_from_argv(&argv(&["claude", "--resume", "--foo"])),
        None,
        "the claude form rejects a flag too"
    );
    assert_eq!(
        resume_target_from_argv(&argv(&["/bin/zsh"])),
        None,
        "a shell pane has no resume target"
    );
    // The detector is the DECLARED form, not a harness-name list: every
    // harness the table gives a form parses, so a live pane running one
    // keeps its row non-resumable exactly as claude/codex panes do.
    assert_eq!(
        resume_target_from_argv(&argv(&["gemini", "--resume", "gem-1234"])),
        Some("gem-1234".into()),
        "a declared flag form beyond the old two parses"
    );
    assert_eq!(
        resume_target_from_argv(&argv(&["agy", "--conversation", "agy-5678"])),
        Some("agy-5678".into()),
        "a session_flag form parses"
    );
    assert_eq!(
        resume_target_from_argv(&argv(&["gemini", "chat", "not-a-target"])),
        None,
        "an argv that never walks the form's literals parses nothing"
    );
    assert_eq!(
        resume_target_from_argv(&argv(&[
            "env",
            "FNO_AGENT_SELF=peer",
            "opencode",
            "--session",
            "oc-90ab"
        ])),
        Some("oc-90ab".into()),
        "the env(1) wrapper is skipped for every declared form"
    );
}

#[test]
fn unbound_pane_running_a_session_keeps_its_row_live_paneless() {
    // (x-d401, AC2-HP) A pane in this session whose argv resumes the
    // row's session id is DIRECT OBSERVATION the backend is live. The
    // row must read LivePaneless (peek, do not resume - a resume opens a
    // second writer on the live rollout), whatever the registry's
    // liveness field says.
    let mut core = empty_core();
    core.session_name = "main".into();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(2, 4, "/w").expect("pane");
    // "of THIS session" is both halves: registered AND placed in the
    // layout. `spawn_pane` only does the first, so a pane left out of the
    // layout must not answer the join.
    core.session.add_squad(
        1,
        vec!["/w".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(pid),
            focus: pid,
        },
    );
    core.panes.get_mut(&pid).unwrap().resume_target = Some("01a03a4e-b862".into());
    let mut row = RegistryAgent {
        harness_session_id: Some("01a03a4e-b862".into()),
        harness: Some("claude".into()),
        name: "worker".into(),
        cwd: "/w".into(),
        // The reading that used to print "backend is not live".
        liveness: agents_view::Liveness::Unmeasured,
        ..Default::default()
    };
    assert_eq!(
        core.row_resume_disposition_in_session(&row),
        RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless)
    );
    row.liveness = agents_view::Liveness::Dead;
    assert_eq!(
        core.row_resume_disposition_in_session(&row),
        RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless),
        "the observed pane outranks even a Dead registry reading"
    );
    assert!(
        !core.row_resumable_in_session(&row),
        "a session a pane is already running must not be resumed again"
    );
    // A pane resuming a DIFFERENT session leaves the row's own dead
    // reading, which resumes.
    core.panes.get_mut(&pid).unwrap().resume_target = Some("other-session".into());
    assert_eq!(
        core.row_resume_disposition_in_session(&row),
        RowResumeDisposition::Resumable
    );
}

#[test]
fn mail_agent_unknown_name_refused() {
    // AC1-ERR: MailAgent naming an absent row is refused fail-closed.
    let mut core = empty_core();
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::MailAgent {
            name: "ghost".into(),
            text: "hi".into(),
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("no such agent"));
}

#[test]
fn mail_agent_blank_text_refused_after_resolve() {
    // AC3-ERR: a valid target but blank-after-sanitize text is refused (the
    // resolve succeeds, so the refusal proves the sanitize gate, not the
    // resolver, caught it - and no subprocess is reached in a plain #[test]).
    let mut core = empty_core();
    core.agents = vec![bg_row("worker", "/w", None)];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::MailAgent {
            name: "worker".into(),
            text: "   ".into(),
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("empty"));
}

#[test]
fn sanitize_mail_text_strips_trims_and_bounds() {
    // Control chars stripped, trimmed; blank refused; over-cap refused (never
    // truncated - Locked Decision 7).
    assert_eq!(sanitize_mail_text("  hi \x07there \n").unwrap(), "hi there");
    assert!(sanitize_mail_text("").is_err());
    assert!(sanitize_mail_text("\x07\x08 \t").is_err());
    let ok = "x".repeat(crate::proto::MAX_MAIL_TEXT);
    assert_eq!(
        sanitize_mail_text(&ok).unwrap().len(),
        crate::proto::MAX_MAIL_TEXT
    );
    assert!(sanitize_mail_text(&"x".repeat(crate::proto::MAX_MAIL_TEXT + 1)).is_err());
}

#[test]
fn derive_failure_leaves_workers_ungrouped() {
    // A malformed/absent graph read leaves workers rendering via their
    // normal path: no squad matches, so the rows stay ungrouped.
    let mut core = empty_core();
    core.agents = vec![bg_row("target-x-bbbb-foo", "/w", None)];
    let msg = core.layout_msg_for((0, 0), &[], 0, (0, 0));
    let squads = match &msg {
        ServerMsg::Layout { squads, .. } => squads,
        _ => unreachable!(),
    };
    assert!(squads.is_empty());
    let rows = core.agent_rows();
    assert_eq!(rows[0].squad, None);
}

#[test]
fn external_row_stop_and_remove_refused() {
    // US4: an external roster row belongs to the claude daemon, not the fno
    // registry, so BOTH verbs refuse with a notice rather than fire a doomed
    // `fno-agents` call. The external arm is checked before the live/exited
    // arms, so a dead external row still refuses on provenance.
    let ext_live = RegistryAgent {
        session_id: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        related_session_id: None,
        forked_from_session_id: None,
        external: true,
        ..bg_row("ext-a", "/tmp", Some("deadbee1"))
    };
    let ext_dead = RegistryAgent {
        session_id: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        related_session_id: None,
        forked_from_session_id: None,
        external: true,
        exited: true,
        ..bg_row("ext-b", "/tmp", Some("deadbee2"))
    };
    for (row, cmd) in [
        (
            ext_live,
            Command::StopAgent {
                harness_session_id: None,
                name: "ext-a".into(),
                pane_id: None,
            },
        ),
        (
            ext_dead,
            Command::RemoveAgent {
                harness_session_id: None,
                name: "ext-b".into(),
                pane_id: None,
                measure: false,
            },
        ),
    ] {
        let mut core = empty_core();
        core.agents = vec![row];
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(1, cmd);
        assert!(drain_notice(&mut rx).unwrap().contains("external"));
    }
}

fn ext_record(
    id: &str,
    state: crate::squad_store::ExternalState,
) -> crate::squad_store::ExternalLifecycle {
    crate::squad_store::ExternalLifecycle {
        attach_id: id.into(),
        name: format!("ext-{id}"),
        cwd: "/tmp".into(),
        state,
        generation: 1,
        updated_at: String::new(),
        reason: None,
    }
}

#[test]
fn stop_external_stale_id_refused_without_spawn() {
    // AC1-ERR: a StopExternal whose attach id names neither a live external
    // row nor a retry-eligible tombstone is refused fail-closed - no
    // subprocess. A plain #[test] has no tokio runtime, so reaching the
    // spawn would panic; the clean refusal proves it never does.
    let mut core = empty_core();
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::StopExternal {
            attach_id: "deadbeef".into(),
            name: "ext".into(),
        },
    );
    assert!(drain_notice(&mut rx)
        .unwrap()
        .contains("no longer a live external row"));
}

#[test]
fn external_lifecycle_invalid_id_refused_before_spawn() {
    // codex P2: a non-8-hex attach id from the client is rejected before it
    // is persisted or reaches a `claude` argv (a dash-prefixed id could be
    // read as a CLI option). Both verbs guard; the refusal precedes any
    // resolve/CAS, so a #[test] with no tokio runtime never panics.
    for cmd in [
        Command::StopExternal {
            attach_id: "--oops".into(),
            name: "x".into(),
        },
        Command::RemoveExternal {
            attach_id: "nothex!".into(),
            name: "x".into(),
        },
    ] {
        let mut core = empty_core();
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(1, cmd);
        assert!(drain_notice(&mut rx)
            .unwrap()
            .contains("invalid external id"));
    }
}

#[test]
fn remove_external_without_stopped_record_refused() {
    // AC2-ERR: rm is reachable only from a persisted `stopped` tombstone. An
    // absent record refuses; a `stopping` record refuses "stop it first".
    // Both stay off the spawn path (no tokio runtime in a #[test]).
    let _s = StoreScratch::new("rm-external-refused");
    let mut core = empty_core();
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    // Absent record.
    core.command(
        1,
        Command::RemoveExternal {
            attach_id: "deadbeef".into(),
            name: "ext".into(),
        },
    );
    assert!(drain_notice(&mut rx)
        .unwrap()
        .contains("no external lifecycle record"));
    // A stopping record refuses with the stop-first ordering.
    crate::squad_store::begin_external_stop("deadbeef", "ext", "/tmp").unwrap();
    core.command(
        1,
        Command::RemoveExternal {
            attach_id: "deadbeef".into(),
            name: "ext".into(),
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("stop it first"));
}

#[test]
fn agent_rows_render_external_tombstones_by_state() {
    // A stopped record renders an EXITED external row carrying its attach_id
    // (so `x` sends RemoveExternal); a failed record renders `!exited` (so
    // `x` retries the stop). Both are external.
    use crate::squad_store::ExternalState as S;
    let mut core = empty_core();
    core.external_lifecycle = vec![
        ext_record("deadbeef", S::Stopped),
        ext_record("cafef00d", S::Failed),
    ];
    let rows = core.agent_rows();
    let stopped = rows
        .iter()
        .find(|r| r.attach_id.as_deref() == Some("deadbeef"))
        .expect("a stopped tombstone row");
    assert!(
        stopped.external && stopped.exited,
        "stopped -> exited external"
    );
    let failed = rows
        .iter()
        .find(|r| r.attach_id.as_deref() == Some("cafef00d"))
        .expect("a failed tombstone row");
    assert!(
        failed.external && !failed.exited,
        "failed -> live-ish external"
    );
}

#[test]
fn agent_rows_dedup_external_tombstone_against_live_row() {
    // A record whose attach_id is ALSO a live external roster row is skipped
    // (the live row wins) so a stop mid-flight never double-renders.
    use crate::squad_store::ExternalState as S;
    let mut core = empty_core();
    core.agents = vec![RegistryAgent {
        session_id: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        related_session_id: None,
        forked_from_session_id: None,
        external: true,
        ..bg_row("ext-live", "/tmp", Some("deadbeef"))
    }];
    core.external_lifecycle = vec![ext_record("deadbeef", S::Stopping)];
    let n = core
        .agent_rows()
        .iter()
        .filter(|r| r.attach_id.as_deref() == Some("deadbeef"))
        .count();
    assert_eq!(n, 1, "the live row wins; the record row is deduped away");
}

#[test]
fn lifecycle_name_collision_refused_fail_closed() {
    // codex review: `name` is not a unique catalog key (dedup is by
    // attach_id). When an external roster row shares a name with a registry
    // row, the verb must refuse on provenance and NEVER act on the registry
    // agent the external shadows; two same-named registry rows are ambiguous.
    // Both are fail-closed refusals, so no unrelated agent is ever stopped.
    let shared = |external, exited, attach: &str| RegistryAgent {
        session_id: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        related_session_id: None,
        forked_from_session_id: None,
        external,
        exited,
        ..bg_row("dup", "/tmp", Some(attach))
    };

    // External shadows a registry row -> refuse as external, act on neither.
    let mut core = empty_core();
    core.agents = vec![
        shared(false, false, "reg00001"),
        shared(true, false, "ext00001"),
    ];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::StopAgent {
            name: "dup".into(),
            harness_session_id: None,
            pane_id: None,
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("external"));

    // Two non-external rows with one name -> ambiguous refusal.
    let mut core = empty_core();
    core.agents = vec![
        shared(false, true, "reg00001"),
        shared(false, true, "reg00002"),
    ];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::RemoveAgent {
            name: "dup".into(),
            harness_session_id: None,
            pane_id: None,
            measure: false,
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("ambiguous"));
}

#[test]
fn attach_reconcile_focuses_mapped_pane_no_second_tab() {
    // x-0090 AC2-HP / AC2-FR: an attach_id already mapped to a live pane
    // focuses it, never mints a second tab. The seeded (pane + map entry)
    // stands in for a successful first attach (the real spawn needs a live
    // `claude`), so the live command under test is attach #2 - and a third
    // is still a focus, covering the double-action guard.
    let (mut core, client_id, _p1, p2, _rx) = seen_test_core();
    core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
    core.attached.insert("deadbee1".into(), p2);
    let panes_before = core.panes.len();

    core.command(client_id, Command::attach_agent("deadbee1"));
    assert_eq!(
        core.panes.len(),
        panes_before,
        "reconcile-focus spawns no new pane"
    );
    // The view jumped to the mapped pane's tab (p2 is tab id 2).
    assert_eq!(core.client_view(client_id), Some((1, 2)), "view follows p2");

    core.command(client_id, Command::attach_agent("deadbee1"));
    assert_eq!(
        core.panes.len(),
        panes_before,
        "a second action stays a focus - exactly one pane"
    );
}

#[test]
fn repeated_attach_focuses_existing_pane_with_notice() {
    // x-3e38 AC3-HP: a second attach of a mapped agent focuses the live
    // pane and says so, never minting a second pane. The notice makes the
    // idempotent focus visible to the operator.
    let (mut core, client_id, _p1, p2, mut rx) = seen_test_core();
    core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
    core.attached.insert("deadbee1".into(), p2);
    let panes_before = core.panes.len();

    core.command(client_id, Command::attach_agent("deadbee1"));
    assert_eq!(core.panes.len(), panes_before, "reconcile spawns no pane");
    let mut saw_notice = false;
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::Notice { text } = msg {
            saw_notice |= text.contains("already attached");
        }
    }
    assert!(saw_notice, "a repeated attach reports the idempotent focus");
}

#[test]
fn fresh_attach_unknown_target_fails_closed_before_spawn() {
    // x-3e38 AC4: an explicit target that names no live squad refuses BEFORE
    // any PTY spawn - no pane, no attach mapping, a visible reason.
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
    let panes_before = core.panes.len();

    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee1".into(),
            placement: PanePlacement {
                target: PaneTarget::SquadName("ghost".into()),
                ..Default::default()
            },
        },
    );
    assert_eq!(core.panes.len(), panes_before, "no PTY spawned");
    assert!(
        !core.attached.contains_key("deadbee1"),
        "no attach mapping recorded on a refused target"
    );
    let mut saw = false;
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::Notice { text } = msg {
            saw |= text.contains("no such squad");
        }
    }
    assert!(saw, "the refusal names the missing squad");
}

// -- x-9f75 open-here (PanePlacement.here) ---------------------------

/// Collect every notice text still queued on `rx`.
pub(super) fn drain_notices(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::Notice { text } = msg {
            out.push(text);
        }
    }
    out
}

#[test]
fn open_here_swaps_focused_viewer_and_detaches_displaced() {
    // AC1-HP: the focused viewer of session A is repointed at B - the tab's tree slot now hosts B's
    // viewer, focus is the new pane, A's viewer is reaped (A resurfaces watch-only), and B is mapped.
    set_attach_program(&["/bin/cat"]); // stand in for `claude attach`
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    let view = core.client_view(client_id).unwrap();
    let focus = core.viewed_tab(view).unwrap().focus;
    // A occupies the focused pane; B is a watch-only row to open here.
    core.attached.insert("deadbee1".into(), focus);
    core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
    let new_pid = core.next_pane_id;

    core.command(client_id, Command::attach_agent_here("deadbee2"));

    let tab = core.viewed_tab(view).unwrap();
    assert_eq!(tab.root, Node::Leaf(new_pid), "slot repointed at B");
    assert_eq!(tab.focus, new_pid, "focus follows the swap");
    assert!(!core.panes.contains_key(&focus), "A's viewer pane reaped");
    assert_eq!(core.attached.get("deadbee2"), Some(&new_pid), "B mapped");
    assert!(
        !core.attached.contains_key("deadbee1"),
        "A's mapping swept - it resurfaces watch-only"
    );
    assert!(
        drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("opened here")),
        "notice names the displaced session"
    );
    core.reap_pane(new_pid); // don't leak the stand-in child
}

#[test]
fn open_here_takes_over_lone_idle_shell() {
    // x-fbb1 (the reported bug): the focused pane is a lone idle shell (not in `attached`).
    // `.`=here reaps it and lands B as the tab's only pane - "take over the empty tab".
    set_attach_program(&["/bin/cat"]); // stand in for `claude attach`
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    let view = core.client_view(client_id).unwrap();
    let shell = core.viewed_tab(view).unwrap().focus;
    // Make the shell a pristine idle shell: a real mux shell draws its first prompt (OSC 133 A)
    // and has run nothing. Without this the pane never emitted markers and take-over refuses.
    core.panes
        .get_mut(&shell)
        .unwrap()
        .vt
        .feed(b"\x1b]133;A\x07");
    core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
    let new_pid = core.next_pane_id;

    core.command(client_id, Command::attach_agent_here("deadbee2"));

    let tab = core.viewed_tab(view).unwrap();
    assert_eq!(tab.root, Node::Leaf(new_pid), "B took the tab's only slot");
    assert_eq!(tab.focus, new_pid, "focus follows the take-over");
    assert!(
        !core.panes.contains_key(&shell),
        "the idle shell was reaped"
    );
    assert_eq!(core.attached.get("deadbee2"), Some(&new_pid), "B mapped");
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("took over tab")));
    core.reap_pane(new_pid); // don't leak the stand-in child
}

#[test]
fn open_here_refuses_conflicting_placement_before_spawn() {
    // AC2-ERR: `here` with a split or a non-CurrentRoute target is a
    // contradiction - refused with no spawn.
    for placement in [
        PanePlacement {
            tab: None,
            at: None,
            here: true,
            split: Some(Dir::Right),
            ..Default::default()
        },
        PanePlacement {
            tab: None,
            at: None,
            here: true,
            target: PaneTarget::SquadName("review".into()),
            ..Default::default()
        },
    ] {
        let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
        core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
        let panes_before = core.panes.len();
        core.command(
            client_id,
            Command::AttachAgent {
                id: "deadbee2".into(),
                placement,
            },
        );
        assert_eq!(core.panes.len(), panes_before, "no pane spawned");
        assert!(drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("open-here takes no split or target")));
    }
}

// -----------------------------------------------------------------------
// (x-d545) The thread viewport's tab outlives its viewer: a recorded
// viewer dying in a lone-leaf tab swaps in an idle shell instead of
// deleting the one tab every reach shares, and the next reach repoints
// the stand-in in the SAME tab.
// -----------------------------------------------------------------------

#[test]
fn attach_agent_anchored_drop_lands_beside_the_anchor_not_the_active_tab() {
    // (x-d6a8 G3) Regression for the codex P1 finding: a paneless bg row
    // dropped beside a SPECIFIC pane attaches in THAT pane's tab, beside it -
    // honoring the drop slot - rather than splitting beside the squad's
    // active-tab focus (the pre-fix place_spawned_pane behavior). The anchor
    // determines both squad and tab, overriding owner routing.
    set_attach_program(&["/bin/cat"]); // stand in for `claude attach`
    let (mut core, client_id, p1, p2, _rx) = seen_test_core();
    // View + activate the OTHER tab (id 2), so a non-anchored attach would
    // land there; the anchor (p1, tab 1) must override that.
    core.set_view(client_id, 1, 2);
    core.session.squad_mut(1).unwrap().active_tab = 1; // index 1 == tab id 2
    core.agents = vec![bg_row("bg", "/tmp/seen", Some("deadbee2"))];
    let new_pid = core.next_pane_id;

    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee2".into(),
            placement: PanePlacement {
                at: Some(p1),
                split: Some(Dir::Right),
                ..Default::default()
            },
        },
    );

    let sq = core.session.squad(1).unwrap();
    let tab1 = sq.tabs.iter().find(|t| t.id == 1).unwrap();
    let mut ls = tree::leaves(&tab1.root);
    ls.sort_unstable();
    let mut expected = vec![p1, new_pid];
    expected.sort_unstable();
    assert_eq!(ls, expected, "attach landed beside the anchor p1 in tab 1");
    let tab2 = sq.tabs.iter().find(|t| t.id == 2).unwrap();
    assert_eq!(
        tree::leaves(&tab2.root),
        vec![p2],
        "the active/viewed tab is untouched - the drop honored its anchor"
    );
    assert_eq!(core.attached.get("deadbee2"), Some(&new_pid), "B mapped");
    core.reap_pane(new_pid); // don't leak the stand-in child
}

#[test]
fn open_here_spawn_failure_leaves_layout_untouched() {
    // AC3-ERR: the attach spawn fails - no pane is displaced, `attached` is
    // unchanged, and the sender gets `attach failed`.
    set_attach_program(&["/nonexistent/definitely-not-a-real-binary-xyz"]);
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    let view = core.client_view(client_id).unwrap();
    let focus = core.viewed_tab(view).unwrap().focus;
    core.attached.insert("deadbee1".into(), focus);
    core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
    let root_before = core.viewed_tab(view).unwrap().root.clone();

    core.command(client_id, Command::attach_agent_here("deadbee2"));

    assert_eq!(
        core.viewed_tab(view).unwrap().root,
        root_before,
        "nothing displaced on spawn failure"
    );
    assert!(core.panes.contains_key(&focus), "A's viewer still live");
    assert_eq!(
        core.attached.get("deadbee1"),
        Some(&focus),
        "A's mapping untouched"
    );
    assert!(!core.attached.contains_key("deadbee2"), "B never mapped");
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("attach failed")));
}

#[test]
fn open_here_reconcile_focuses_existing_no_displacement() {
    // AC1-EDGE: B already has a live pane. Reconcile focuses it (no spawn,
    // no displacement of the current focus) - reconcile beats open-here.
    let (mut core, client_id, p1, p2, mut rx) = seen_test_core();
    let view = core.client_view(client_id).unwrap();
    let focus = core.viewed_tab(view).unwrap().focus;
    // The focused pane is A's viewer; B is already paned at the OTHER pane.
    let other = if focus == p1 { p2 } else { p1 };
    core.attached.insert("deadbee1".into(), focus);
    core.attached.insert("deadbee2".into(), other);
    core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
    let panes_before = core.panes.len();

    core.command(client_id, Command::attach_agent_here("deadbee2"));

    assert_eq!(core.panes.len(), panes_before, "reconcile spawns nothing");
    assert!(core.panes.contains_key(&focus), "A's viewer not displaced");
    assert_eq!(
        core.attached.get("deadbee2"),
        Some(&other),
        "B still at its pane"
    );
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("already attached")));
}

#[test]
fn open_here_double_click_focuses_no_second_displacement() {
    // AC1-FR: open-here on B twice quickly - the first swaps, the second hits reconcile (B now paned)
    // and focuses it. Exactly one viewer of B, no second displacement.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    let view = core.client_view(client_id).unwrap();
    let focus = core.viewed_tab(view).unwrap().focus;
    core.attached.insert("deadbee1".into(), focus);
    core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
    let new_pid = core.next_pane_id;

    core.command(client_id, Command::attach_agent_here("deadbee2"));
    let panes_after_first = core.panes.len();
    core.command(client_id, Command::attach_agent_here("deadbee2"));

    assert_eq!(
        core.panes.len(),
        panes_after_first,
        "the second open-here mints no pane"
    );
    assert_eq!(
        core.attached.values().filter(|&&p| p == new_pid).count(),
        1,
        "exactly one viewer of B"
    );
    assert_eq!(core.attached.get("deadbee2"), Some(&new_pid));
    core.reap_pane(new_pid);
}

#[test]
fn open_here_reads_current_focus_never_the_elsewhere_viewer() {
    // AC2-FR: the displacement guard evaluates whatever pane is focused NOW (re-resolved
    // server-side). Focus is a lone idle shell, so open-here takes it over (x-fbb1); a viewer
    // sits on ANOTHER tab and must be left completely untouched - open-here never displaces a
    // pane the operator is not looking at.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, p1, p2, mut rx) = seen_test_core();
    let view = core.client_view(client_id).unwrap();
    let focus = core.viewed_tab(view).unwrap().focus;
    // A viewer exists, but on the OTHER (unviewed) tab's pane; focus is a pristine idle shell.
    let other = if focus == p1 { p2 } else { p1 };
    core.panes
        .get_mut(&focus)
        .unwrap()
        .vt
        .feed(b"\x1b]133;A\x07");
    core.attached.insert("deadbee1".into(), other);
    core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
    let new_pid = core.next_pane_id;

    core.command(client_id, Command::attach_agent_here("deadbee2"));

    assert!(
        !core.panes.contains_key(&focus),
        "the focused idle shell was taken over"
    );
    assert_eq!(
        core.attached.get("deadbee2"),
        Some(&new_pid),
        "B landed on the focused slot"
    );
    assert!(
        core.panes.contains_key(&other),
        "the elsewhere viewer is never touched"
    );
    assert_eq!(
        core.attached.get("deadbee1"),
        Some(&other),
        "the elsewhere viewer's mapping is intact"
    );
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("took over tab")));
    core.reap_pane(new_pid);
}

// -- git diff side pane ----------------------------------------------

/// A scratch dir under the temp root, keyed per test thread (one test ==
/// one thread) so parallel tests never share one. Callers drop it via
/// `remove_dir_all`; nothing outside the temp root is touched.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "fno-diff-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// An initialized repo with one commit, so `HEAD` resolves.
fn scratch_repo(tag: &str) -> std::path::PathBuf {
    let d = scratch_dir(tag);
    git(&d, &["init", "-q"]);
    git(&d, &["config", "user.email", "t@t"]);
    git(&d, &["config", "user.name", "t"]);
    std::fs::write(d.join("tracked.txt"), "one\n").unwrap();
    git(&d, &["add", "tracked.txt"]);
    git(&d, &["commit", "-qm", "seed"]);
    d
}

/// Run the REAL diff script with `cat` standing in for the pager, so the
/// shipped script's own behavior is what gets asserted (a pager would block
/// on a pipe and hide it).
fn run_diff_script(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(diff_script("cat"))
        .current_dir(dir)
        .output()
        .expect("script runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn diff_script_reports_no_changes_and_counts_untracked() {
    // AC1-EDGE: a clean worktree must SAY it is clean and note the
    // untracked files `git diff` cannot see. A zero-output pane here is
    // the feature's signature silent failure.
    let d = scratch_repo("clean");
    std::fs::write(d.join("new-a.txt"), "a\n").unwrap();
    std::fs::write(d.join("new-b.txt"), "b\n").unwrap();

    let out = run_diff_script(&d);

    assert!(
        out.contains("no changes vs HEAD"),
        "clean worktree states itself: {out:?}"
    );
    assert!(
        out.contains("2 untracked file(s) not shown"),
        "untracked count present: {out:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn diff_script_renders_staged_and_unstaged_changes() {
    // AC1-HP (content half): the pane shows the working diff, and it is
    // `diff HEAD` - staged changes included, not just unstaged ones.
    let d = scratch_repo("changes");
    std::fs::write(d.join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::write(d.join("staged.txt"), "staged\n").unwrap();
    git(&d, &["add", "staged.txt"]);

    let out = run_diff_script(&d);

    // Color escapes sit between the `+` and the text, so assert on the
    // content and the file headers rather than a contiguous `+two`.
    assert!(
        out.contains("tracked.txt") && out.contains("two"),
        "unstaged change present: {out:?}"
    );
    assert!(out.contains("staged.txt"), "staged change present: {out:?}");
    assert!(
        !out.contains("no changes"),
        "a dirty worktree never claims clean: {out:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn diff_script_unborn_head_diffs_against_the_empty_tree() {
    // AC3-ERR: `git diff HEAD` errors outright in a zero-commit repo. The
    // script falls back to the empty tree so a fresh repo shows its staged
    // content instead of a blank pane.
    let d = scratch_dir("unborn");
    git(&d, &["init", "-q"]);
    git(&d, &["config", "user.email", "t@t"]);
    git(&d, &["config", "user.name", "t"]);
    std::fs::write(d.join("first.txt"), "hello\n").unwrap();
    git(&d, &["add", "first.txt"]);

    let out = run_diff_script(&d);

    assert!(!out.trim().is_empty(), "never a blank pane");
    assert!(
        out.contains("first.txt") && out.contains("hello"),
        "the empty-tree fallback shows the staged content: {out:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn diff_script_unborn_head_with_nothing_staged_still_speaks() {
    // AC3-ERR, the emptier half: a repo with no commits AND nothing staged
    // must still print something naming why.
    let d = scratch_dir("unborn-empty");
    git(&d, &["init", "-q"]);

    let out = run_diff_script(&d);

    assert!(
        out.contains("no commits yet"),
        "the unborn state is named, not blank: {out:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn diff_script_outside_a_repo_shows_gits_own_error() {
    // AC1-ERR: a non-repo cwd renders git's own message. Honest visible
    // failure beats a blank pane the operator has to guess about.
    let d = scratch_dir("norepo");

    let out = run_diff_script(&d);

    assert!(
        out.to_lowercase().contains("not a git repository"),
        "git's own error reaches the pane: {out:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn diff_renderer_chain_prefers_delta_and_falls_back_to_less() {
    // AC3-EDGE: delta is preferred when present, git-color + less when not.
    // `--paging=always` is asserted because without it delta dumps and
    // exits on a short diff, which reads as a broken pane.
    let d = scratch_dir("pathprobe");
    assert!(
        !delta_in_path(Some(d.as_os_str())),
        "an empty dir has no delta"
    );
    // A non-executable file of the right name must NOT be selected: it
    // would be picked as the renderer and then fail to exec, losing the
    // diff into a dead pipe.
    let bin = d.join("delta");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    assert!(
        !delta_in_path(Some(d.as_os_str())),
        "a non-executable delta is not a renderer"
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        delta_in_path(Some(d.as_os_str())),
        "an executable delta on PATH is seen"
    );
    // A dangling symlink names nothing runnable either.
    let broken = scratch_dir("pathprobe-broken");
    std::os::unix::fs::symlink(broken.join("absent"), broken.join("delta")).unwrap();
    assert!(
        !delta_in_path(Some(broken.as_os_str())),
        "a dangling delta symlink is not a renderer"
    );
    let _ = std::fs::remove_dir_all(&broken);
    assert!(!delta_in_path(None), "no PATH at all is not a delta");

    assert!(diff_script("less -R").ends_with("| less -R"));
    assert!(diff_script("delta --paging=always").contains("--paging=always"));
    assert!(
        diff_script("cat").contains("color.ui=always"),
        "git colors into a pipe only when forced"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A `seen_test_core` whose single agent row points at a real repo, so the
/// toggle's cwd resolution and spawn both run for real.
///
/// The pane program is stubbed to one that exits immediately. These tests
/// are about the toggle and the layout, and the real chain ends in a pager
/// that parks on the PTY waiting for input no test will send - which hangs
/// the run rather than failing it. The script's own behavior is covered by
/// the `diff_script_*` tests, which execute it for real with `cat`.
fn diff_test_core(tag: &str) -> (Core, u64, std::path::PathBuf, mpsc::Receiver<ServerMsg>) {
    let repo = scratch_repo(tag);
    set_diff_shell("/bin/echo");
    let (mut core, client_id, _p1, _p2, rx) = seen_test_core();
    core.agents = vec![bg_row("worker", repo.to_str().unwrap(), None)];
    (core, client_id, repo, rx)
}

fn toggle_diff(core: &mut Core, client_id: u64) {
    core.command(
        client_id,
        Command::ToggleDiffPane {
            agent: Some("worker".into()),
            pane: None,
        },
    );
}

#[test]
fn diff_pane_opens_a_split_beside_the_focused_pane() {
    // AC1-HP: the toggle opens a second pane in the viewed tab, spawned in
    // the row's worktree, with the original pane still in the tree.
    let (mut core, client_id, repo, _rx) = diff_test_core("open");
    let view = core.client_view(client_id).unwrap();
    let focus = core.viewed_tab(view).unwrap().focus;
    let new_pid = core.next_pane_id;

    toggle_diff(&mut core, client_id);

    assert!(core.panes.contains_key(&new_pid), "diff pane spawned");
    assert!(core.panes.contains_key(&focus), "the source pane survives");
    assert_eq!(
        core.panes.get(&new_pid).map(|p| p.cwd.as_str()),
        Some(repo.to_str().unwrap()),
        "spawned in the row's worktree"
    );
    assert_eq!(
        core.diff_pane.as_ref().map(|(_, p)| *p),
        Some(new_pid),
        "the toggle records its pane"
    );
    let panes: Vec<u64> = tree::layout(
        &core.viewed_tab(view).unwrap().root,
        tree::Rect {
            x: 0,
            y: 0,
            rows: 24,
            cols: 80,
        },
    )
    .into_iter()
    .map(|(p, _)| p)
    .collect();
    assert!(
        panes.contains(&focus) && panes.contains(&new_pid),
        "both panes are in the tree: {panes:?}"
    );
    core.reap_pane(new_pid);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn diff_pane_second_press_closes_and_restores_the_layout() {
    // AC2-HP + AC1-FR: press-press converges to exactly the pre-press
    // layout with no diff pane left behind - a second press is always a
    // close, never a second pane.
    let (mut core, client_id, repo, _rx) = diff_test_core("toggle");
    let view = core.client_view(client_id).unwrap();
    let root_before = core.viewed_tab(view).unwrap().root.clone();
    let panes_before = core.panes.len();

    toggle_diff(&mut core, client_id);
    let opened = core.diff_pane.as_ref().map(|(_, p)| *p).expect("opened");
    toggle_diff(&mut core, client_id);

    assert!(
        core.diff_pane.is_none(),
        "no diff pane recorded after close"
    );
    assert!(!core.panes.contains_key(&opened), "its PTY is reaped");
    assert_eq!(
        core.viewed_tab(view).unwrap().root,
        root_before,
        "the layout is restored exactly"
    );
    assert_eq!(core.panes.len(), panes_before, "no pane leaked");

    // A third press opens again - the toggle never wedges closed.
    toggle_diff(&mut core, client_id);
    let reopened = core.diff_pane.as_ref().map(|(_, p)| *p).expect("reopened");
    assert_ne!(reopened, opened, "a fresh pane, not the old id");
    core.reap_pane(reopened);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn diff_pane_spawn_failure_leaves_the_layout_untouched() {
    // AC2-ERR: the spawn fails - no split, no dead pane, and the sender is
    // told. Spawn-first ordering is what makes this hold with no rollback.
    let (mut core, client_id, repo, mut rx) = diff_test_core("spawnfail");
    // After the helper, which sets its own stub program.
    set_diff_shell("/nonexistent/definitely-not-a-shell-xyz");
    let view = core.client_view(client_id).unwrap();
    let root_before = core.viewed_tab(view).unwrap().root.clone();
    let panes_before = core.panes.len();

    toggle_diff(&mut core, client_id);

    assert_eq!(
        core.viewed_tab(view).unwrap().root,
        root_before,
        "layout untouched on spawn failure"
    );
    assert_eq!(core.panes.len(), panes_before, "no pane registered");
    assert!(core.diff_pane.is_none(), "nothing recorded");
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("diff pane failed")));
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn diff_pane_refuses_a_reaped_worktree_before_spawn() {
    // A spawn silently ignores a cwd that is not a directory and lands in
    // the server's own cwd - which would render some OTHER repo's diff
    // under this row's name. Refuse instead, visibly.
    let (mut core, client_id, repo, mut rx) = diff_test_core("gone");
    let _ = std::fs::remove_dir_all(&repo);
    let panes_before = core.panes.len();

    toggle_diff(&mut core, client_id);

    assert_eq!(core.panes.len(), panes_before, "nothing spawned");
    assert!(core.diff_pane.is_none());
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("worktree is gone")));
}

#[test]
fn diff_pane_unknown_row_says_so_rather_than_diffing_something_else() {
    // AC1-UI: a press that cannot resolve a worktree still produces
    // feedback. A silent no-op reads as a dead keybind.
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    let panes_before = core.panes.len();

    core.command(
        client_id,
        Command::ToggleDiffPane {
            agent: Some("nobody-here".into()),
            pane: None,
        },
    );

    assert_eq!(core.panes.len(), panes_before, "nothing spawned");
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("no worktree to diff")));
}

#[test]
fn diff_pane_on_a_non_repo_still_opens_and_closes() {
    // AC1-ERR, the half the script test cannot reach: a source that is not
    // a git repo is a real directory, so the pane opens (rendering git's
    // error) and the toggle must still be able to close it. A pane the
    // toggle cannot clear would strand the layout.
    let dir = scratch_dir("norepo-cycle");
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    core.agents = vec![bg_row("worker", dir.to_str().unwrap(), None)];
    let view = core.client_view(client_id).unwrap();
    let root_before = core.viewed_tab(view).unwrap().root.clone();

    toggle_diff(&mut core, client_id);
    let pid = core
        .diff_pane
        .as_ref()
        .map(|(_, p)| *p)
        .expect("opens on a non-repo dir");
    toggle_diff(&mut core, client_id);

    assert!(core.diff_pane.is_none(), "the toggle closes it");
    assert!(!core.panes.contains_key(&pid));
    assert_eq!(core.viewed_tab(view).unwrap().root, root_before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diff_pane_refuses_a_name_two_rows_share() {
    // Registry names are not unique. Resolving to the first match would
    // render one worker's worktree under another worker's row - a wrong
    // answer the operator has no way to spot. Refuse instead.
    let repo_a = scratch_repo("dupe-a");
    let repo_b = scratch_repo("dupe-b");
    set_diff_shell("/bin/echo");
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    core.agents = vec![
        bg_row("twin", repo_a.to_str().unwrap(), None),
        bg_row("twin", repo_b.to_str().unwrap(), None),
    ];
    let panes_before = core.panes.len();

    core.command(
        client_id,
        Command::ToggleDiffPane {
            agent: Some("twin".into()),
            pane: None,
        },
    );

    assert_eq!(core.panes.len(), panes_before, "nothing spawned");
    assert!(core.diff_pane.is_none());
    assert!(
        drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("more than one row")),
        "the ambiguity is named, not silently resolved"
    );
    let _ = std::fs::remove_dir_all(&repo_a);
    let _ = std::fs::remove_dir_all(&repo_b);
}

#[test]
fn diff_pane_resolves_a_pinned_pane_the_registry_never_had() {
    // The sideline synthesizes rows from the pane tree, so a row can be
    // advertised with no `self.agents` entry at all. The pinned pane
    // carries its own cwd, which is what keeps the menu entry from being
    // dead on exactly those rows.
    let repo = scratch_repo("synth");
    set_diff_shell("/bin/echo");
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    core.agents.clear();
    let host = core
        .spawn_pane_cmd(&["/bin/echo".to_string()], 24, 40, repo.to_str().unwrap())
        .expect("host pane");

    core.command(
        client_id,
        Command::ToggleDiffPane {
            agent: Some("not-in-the-registry".into()),
            pane: Some(host),
        },
    );

    assert_eq!(
        core.diff_pane.as_ref().map(|(c, _)| c.as_str()),
        Some(repo.to_str().unwrap()),
        "the pane's own cwd resolved it despite an unknown name"
    );
    if let Some((_, p)) = core.diff_pane.clone() {
        core.reap_pane(p);
    }
    core.reap_pane(host);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn diff_pane_switching_source_never_leaves_two_panes() {
    // Invariant: at most one live diff pane. Toggling a second row closes
    // the first's pane rather than accumulating one per source.
    let (mut core, client_id, repo_a, _rx) = diff_test_core("srca");
    let repo_b = scratch_repo("srcb");
    core.agents
        .push(bg_row("worker-b", repo_b.to_str().unwrap(), None));

    toggle_diff(&mut core, client_id);
    let first = core.diff_pane.as_ref().map(|(_, p)| *p).expect("first");
    core.command(
        client_id,
        Command::ToggleDiffPane {
            agent: Some("worker-b".into()),
            pane: None,
        },
    );

    let second = core.diff_pane.as_ref().map(|(_, p)| *p).expect("second");
    assert_ne!(second, first, "a new pane for the new source");
    assert!(!core.panes.contains_key(&first), "the first pane is closed");
    assert_eq!(
        core.diff_pane.as_ref().map(|(c, _)| c.as_str()),
        Some(repo_b.to_str().unwrap()),
        "keyed to the new source"
    );
    core.reap_pane(second);
    let _ = std::fs::remove_dir_all(&repo_a);
    let _ = std::fs::remove_dir_all(&repo_b);
}

#[test]
fn diff_pane_closed_by_another_path_does_not_wedge_the_toggle() {
    // AC2-FR neighbour: the pane can die outside the toggle (close-pane,
    // tab teardown). A stale recorded id must read as closed so the next
    // press opens rather than trying to close a ghost.
    let (mut core, client_id, repo, _rx) = diff_test_core("stale");

    toggle_diff(&mut core, client_id);
    let first = core.diff_pane.as_ref().map(|(_, p)| *p).expect("opened");
    core.close_pane(first);
    assert!(!core.panes.contains_key(&first), "closed out of band");

    toggle_diff(&mut core, client_id);

    let second = core.diff_pane.as_ref().map(|(_, p)| *p).expect("reopened");
    assert_ne!(second, first, "the next press opens, not closes a ghost");
    core.reap_pane(second);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn diff_pane_says_so_when_the_tab_dies_under_the_toggle() {
    // AC1-UI, the path that is easiest to leave silent: switching source
    // closes the old pane first, and if that pane was the tab's last one
    // the tab retires and the view re-anchors - so the reopen has no tab
    // to split. The press must still say something.
    let (mut core, client_id, repo_a, mut rx) = diff_test_core("tabdies");
    let repo_b = scratch_repo("tabdies-b");
    core.agents
        .push(bg_row("worker-b", repo_b.to_str().unwrap(), None));
    let view = core.client_view(client_id).unwrap();

    toggle_diff(&mut core, client_id);
    let first = core.diff_pane.as_ref().map(|(_, p)| *p).expect("first");
    // Close everything else in the tab, leaving the diff pane alone in it.
    let others: Vec<u64> = tree::layout(&core.viewed_tab(view).unwrap().root, vp_of(&core, view))
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| *p != first)
        .collect();
    for p in others {
        core.close_pane(p);
    }
    let _ = drain_notices(&mut rx);

    core.command(
        client_id,
        Command::ToggleDiffPane {
            agent: Some("worker-b".into()),
            pane: None,
        },
    );

    assert!(
        drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("diff pane")),
        "a press that cannot land still reports"
    );
    assert!(
        core.diff_pane.is_none(),
        "no phantom record for a pane that never landed"
    );
    let _ = std::fs::remove_dir_all(&repo_a);
    let _ = std::fs::remove_dir_all(&repo_b);
}

/// The viewport the core would tile `view`'s tab into.
fn vp_of(core: &Core, view: (u64, TabId)) -> tree::Rect {
    core.tab_rect(view.1)
}

#[test]
fn diff_pane_refuses_a_split_too_narrow_to_fit() {
    // AC2-EDGE: below the tree's minimum the split is refused pre-spawn,
    // the layout is untouched, and a notice says why. Silence here reads
    // as a dead keybind.
    let (mut core, client_id, repo, mut rx) = diff_test_core("narrow");
    let view = core.client_view(client_id).unwrap();
    // Clamp the viewed tab to a width two panes cannot share.
    for c in core.clients.iter_mut().filter(|c| c.view.1 == view.1) {
        c.dims = (24, tree::MIN_COLS);
    }
    let root_before = core.viewed_tab(view).unwrap().root.clone();
    let panes_before = core.panes.len();

    toggle_diff(&mut core, client_id);

    assert_eq!(
        core.viewed_tab(view).unwrap().root,
        root_before,
        "layout untouched by a refused split"
    );
    assert_eq!(
        core.panes.len(),
        panes_before,
        "the pre-spawned pane is reaped"
    );
    assert!(core.diff_pane.is_none());
    assert!(
        drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("split refused")),
        "the refusal is visible"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn attach_split_fallback_lands_new_tab_with_notice() {
    // AC3-FR (x-9f75): a same-workspace row click sends AttachAgent with a Right split; at min-size the
    // split is refused and the pane lands as a NEW TAB with a `tab full` notice - never a reap+dead-end.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    let view = core.client_view(client_id).unwrap();
    // Force a horizontal-split refusal on the split target tab: the viewing client's dims drive its area
    // (tab_area prefers the client clamp), so shrink them below a two-pane horizontal minimum.
    for c in core.clients.iter_mut().filter(|c| c.id == client_id) {
        c.dims = (24, 12);
    }
    // Align the split target (squad.active_tab) with the tab the client
    // views, so the shrunk-client clamp governs that tab's area.
    let sq = core.session.squad_mut(view.0).unwrap();
    sq.active_tab = sq.tabs.iter().position(|t| t.id == view.1).unwrap();
    core.agents = vec![bg_row("sib", "/tmp/seen", Some("deadbee2"))];
    let squad_tabs_before = core.session.squad(view.0).unwrap().tabs.len();
    let new_pid = core.next_pane_id;

    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee2".into(),
            placement: PanePlacement {
                split: Some(Dir::Right),
                ..Default::default()
            },
        },
    );

    assert_eq!(
        core.session.squad(view.0).unwrap().tabs.len(),
        squad_tabs_before + 1,
        "the pane landed as a new tab"
    );
    assert_eq!(core.attached.get("deadbee2"), Some(&new_pid), "B mapped");
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("tab full - opened as tab")));
    core.reap_pane(new_pid);
}

#[test]
fn agent_rows_presents_mapped_watch_only_pane_hosted() {
    // x-0090 AC1-HP (presentation): a watch-only row whose attach maps to a
    // live pane renders pane-hosted under the pane's squad, with attach_id
    // dropped - so agent_hit sends FocusPane, not a duplicate AttachAgent.
    let (mut core, _client_id, _p1, p2, _rx) = seen_test_core();
    core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
    core.attached.insert("deadbee1".into(), p2);

    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "spawn-fix-c3d4").unwrap();
    assert_eq!(row.pane_id, Some(p2), "presents pane-hosted");
    assert_eq!(row.squad, Some(1), "under the pane's squad");
    assert_eq!(row.attach_id, None, "attach target dropped on a pane row");
}

#[test]
fn detached_live_worker_row_is_paneless_but_not_exited() {
    let (mut core, _client_id, p1, _p2, _rx) = seen_test_core();
    let mut worker = agent_in("test", p1, Some(AgentBadge::Working), false);
    worker.name = "detached-worker".into();
    worker.harness = Some("codex".into());
    worker.harness_session_id = Some("session-detached".into());
    core.agents = vec![worker];

    core.detach_worker_pane(p1).unwrap();

    let row = core
        .agent_rows()
        .into_iter()
        .find(|row| row.name == "detached-worker")
        .expect("detached worker remains visible");
    assert_eq!(row.pane_id, None, "detached worker is paneless");
    assert!(!row.exited, "detached worker is still alive");
    assert!(
        core.panes.contains_key(&p1),
        "detachment did not reap the PTY"
    );
    assert!(
        core.session.find_pane(p1).is_none(),
        "detachment removed the pane from every visible tree"
    );

    core.reap_pane(p1);
}

#[test]
fn resume_reattaches_the_same_detached_pane_without_spawning() {
    let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
    let mut worker = agent_in("test", p1, Some(AgentBadge::Working), false);
    worker.name = "detached-worker".into();
    worker.harness = Some("codex".into());
    worker.harness_session_id = Some("session-detached".into());
    core.agents = vec![worker];
    core.detach_worker_pane(p1).unwrap();

    let result = core.resume_one("detached-worker", None, client_id, (1, 1), (24, 40), false);
    match result {
        ResumeOutcome::Resumed { pane, squad, .. } => {
            assert_eq!(pane, p1, "resume grafted the original pane");
            assert_eq!(squad, 1, "resume returned it to its owning squad");
        }
        other => panic!("expected same-pane reattach, got {other:?}"),
    }
    assert!(
        core.session.find_pane(p1).is_some(),
        "pane is back in a tree"
    );
    assert!(!core.detached_panes.contains_key(&p1));
    assert_eq!(core.worker_pane.get("detached-worker"), Some(&vec![p1]));
    let row = core
        .agent_rows()
        .into_iter()
        .find(|row| row.name == "detached-worker")
        .unwrap();
    assert_eq!(row.pane_id, Some(p1), "reattached row is pane-hosted");

    for pane in core.panes.keys().copied().collect::<Vec<_>>() {
        core.reap_pane(pane);
    }
}

#[test]
fn detached_child_exit_tombstones_member_and_clears_roster() {
    let (mut core, _client_id, p1, _p2, _rx) = seen_test_core();
    let mut worker = agent_in("test", p1, Some(AgentBadge::Working), false);
    worker.name = "detached-worker".into();
    worker.harness = Some("codex".into());
    worker.harness_session_id = Some("session-detached".into());
    core.agents = vec![worker];
    core.detach_worker_pane(p1).unwrap();
    let child = core.panes[&p1].pty.child_pid().unwrap();
    assert_eq!(
        unsafe { libc::kill(child as libc::pid_t, libc::SIGKILL) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !core.panes[&p1].pty.is_reap_ready() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        core.panes[&p1].pty.is_reap_ready(),
        "child exit was observed"
    );

    assert!(matches!(core.reap_dead_children(vec![p1]), Flow::Continue));
    assert!(!core.panes.contains_key(&p1));
    assert!(!core.detached_panes.contains_key(&p1));
    let member = core.squad_members[&1]
        .iter()
        .find(|member| member.worker.as_deref() == Some("detached-worker"))
        .unwrap();
    assert!(
        member.tombstone,
        "natural detached exit leaves a dead marker"
    );
    assert!(!member.detached, "dead member is no longer detached");

    for pane in core.panes.keys().copied().collect::<Vec<_>>() {
        core.reap_pane(pane);
    }
}

#[test]
fn reap_sweeps_attach_map_and_row_reverts_to_watch_only() {
    // x-0090 AC1-FR: killing the mapped pane sweeps the map (eager) AND the
    // lazy `panes` liveness check reverts the row to watch-only attachable,
    // so a next click re-attaches into a fresh pane rather than a corpse.
    let (mut core, _client_id, _p1, p2, _rx) = seen_test_core();
    core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
    core.attached.insert("deadbee1".into(), p2);

    core.reap_pane(p2);
    assert!(
        !core.attached.values().any(|&p| p == p2),
        "eager sweep drops the dead pane's mapping"
    );
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "spawn-fix-c3d4").unwrap();
    assert_eq!(row.pane_id, None, "reverts to watch-only");
    assert_eq!(
        row.attach_id.as_deref(),
        Some("deadbee1"),
        "re-attachable after the pane dies"
    );
}

#[test]
fn agent_rows_union_covers_merged_bare_and_watch_only() {
    // x-0090 US2: agent_rows() is a pane union. seen_test_core gives squad 1
    // with p1 in tab 1 and p2 in tab 2 (both bare `/bin/cat` panes). Enrich
    // p1 from the registry, leave p2 bare, add one paneless watch-only row.
    let (mut core, _c, p1, p2, _rx) = seen_test_core();
    core.agents = vec![
        agent_in("test", p1, Some(AgentBadge::Working), false),
        bg_row("spawn-fix-c3d4", "/elsewhere", Some("deadbee1")),
    ];
    let rows = core.agent_rows();
    // Pane rows first in (tab, pane) order, watch-only appended last.
    assert_eq!(rows.len(), 3, "two pane rows + one watch-only");

    // Merged row: named + badged from the registry, carries its tab ref.
    assert_eq!(rows[0].pane_id, Some(p1));
    assert_eq!(rows[0].name, "w", "registry name wins on a merged row");
    assert_eq!(rows[0].badge, Some(AgentBadge::Working));
    assert_eq!(rows[0].tab, Some(1));
    assert_eq!(rows[0].attach_id, None, "a pane row never carries attach");

    // Bare pane: labelled from its own entry (cwd basename here), no badge.
    assert_eq!(rows[1].pane_id, Some(p2));
    assert_eq!(rows[1].name, "seen", "bare pane labelled from PaneEntry");
    assert_eq!(rows[1].badge, None);
    assert_eq!(rows[1].tab, Some(2));

    // Watch-only appended last: paneless, orphan squad, still attachable.
    assert_eq!(rows[2].pane_id, None, "watch-only has no pane");
    assert_eq!(rows[2].name, "spawn-fix-c3d4");
    assert_eq!(rows[2].squad, None, "cwd matches no squad -> orphan");
    assert_eq!(rows[2].attach_id.as_deref(), Some("deadbee1"));
    assert_eq!(rows[2].tab, None);
}

#[test]
fn agent_rows_one_row_per_entity_no_watch_only_double() {
    // x-0090 Invariant: a registry row merged onto a pane never ALSO renders
    // watch-only. A bg row that IS pane-hosted this session appears once.
    let (mut core, _c, p1, _p2, _rx) = seen_test_core();
    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
    let rows = core.agent_rows();
    let hits = rows.iter().filter(|r| r.name == "w").count();
    assert_eq!(hits, 1, "the merged agent renders exactly once");
}

#[test]
fn card_ready_gate_only_passes_ready_cards() {
    // x-a496 (codex peer review): a targeted dispatch only proceeds for a
    // READY card named by id or slug; blocked / in-flight / unknown ids are
    // refused, so a click can't start work prefix+g would skip.
    let card = |id: &str, slug: &str, state| BacklogCard {
        id: id.into(),
        slug: slug.into(),
        priority: "p2".into(),
        state,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    };
    let backlog = [
        card("x-rdy", "ready-slug", CardState::Ready),
        card("x-blk", "blk-slug", CardState::Blocked),
        card("x-fly", "fly-slug", CardState::InFlight),
    ];
    assert!(card_ready_to_dispatch(&backlog, "x-rdy"), "ready by id");
    assert!(
        card_ready_to_dispatch(&backlog, "ready-slug"),
        "ready by slug"
    );
    assert!(
        !card_ready_to_dispatch(&backlog, "x-blk"),
        "blocked refused"
    );
    assert!(
        !card_ready_to_dispatch(&backlog, "x-fly"),
        "in-flight refused"
    );
    assert!(
        !card_ready_to_dispatch(&backlog, "x-nope"),
        "unknown refused"
    );
    assert!(!card_ready_to_dispatch(&backlog, ""), "empty refused");
    assert!(
        !card_ready_to_dispatch(&[], "x-rdy"),
        "empty backlog refused"
    );
}

#[test]
fn node_token_matches_whole_ids_only() {
    // Locked 6: exact node-id token, non-alphanumeric boundaries. `-` is
    // part of the id shape, so it cannot be the boundary test.
    assert!(name_has_node_token("x-54fa", "x-54fa"));
    assert!(name_has_node_token("tgt-x-54fa", "x-54fa"));
    assert!(name_has_node_token("run x-54fa now", "x-54fa"));
    assert!(name_has_node_token("x-54fa.retry", "x-54fa"));
    // Prefix/suffix of a longer token never matches.
    assert!(!name_has_node_token("x-54fab", "x-54fa"));
    assert!(!name_has_node_token("ax-54fa", "x-54fa"));
    assert!(!name_has_node_token("x-54fa", "x-54f"));
    // Second occurrence with clean boundaries still matches.
    assert!(name_has_node_token("x-54fab x-54fa", "x-54fa"));
    assert!(!name_has_node_token("anything", ""));
    // A node whose FIRST char is multi-byte (a non-ASCII `id_prefix` is
    // legal config) must not panic when a rejected match forces the scan
    // to advance - `start + 1` would land inside the char (gemini review
    // of PR #211). Both the advance-then-match and the pure-reject walk.
    assert!(name_has_node_token(
        "a\u{3093}-54fa \u{3093}-54fa",
        "\u{3093}-54fa"
    ));
    assert!(!name_has_node_token("a\u{3093}-54fa", "\u{3093}-54fa"));
}

#[tokio::test]
async fn remove_on_an_alive_row_is_not_refused_on_the_server() {
    // (x-a33f) The measure gate is gone: an Alive row is not refused on
    // this server - the command reaches the off-loop rm dispatch, whose
    // daemon-side rm ends the process itself. The runtime absorbs the
    // spawn; the outcome rides DispatchResult, which these unit tests
    // do not pump. A refusal notice here would mean a gate grew back.
    let mut core = empty_core();
    core.agents = vec![bg_row("corpse", "/tmp", None)];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::RemoveAgent {
            name: "corpse".into(),
            harness_session_id: None,
            pane_id: None,
            measure: true,
        },
    );
    assert!(drain_notice(&mut rx).is_none(), "no refusal notice");
}

/// A paneless registry row for the routing tests: `name`/`cwd`/`attach_id`
/// are the join surfaces; everything else is the quiet default.
pub(super) fn bg_row(name: &str, cwd: &str, attach: Option<&str>) -> RegistryAgent {
    RegistryAgent {
        model: None,
        route: None,
        spawned_by_session: None,
        session_id: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        related_session_id: None,
        forked_from_session_id: None,
        name: name.into(),
        cwd: cwd.into(),
        exited: false,
        dnd: false,
        liveness: agents_view::Liveness::Alive,
        badge: None,
        reason: None,
        mux: None,
        answerable: None,
        attach_id: attach.map(str::to_owned),
        external: false,
        account: None,
        claude_session_uuid: None,
        log_path: None,
        updated_at: None,
        crown_level: None,
        crown_scope: None,
        harness: None,
        ..Default::default()
    }
}

#[test]
fn subline_from_joins_branch_and_tail_and_degrades() {
    // Both present -> "branch · tail".
    assert_eq!(
        subline_from(Some("main"), "/code/footnote"),
        Some("main · footnote".into())
    );
    // Branch unresolved -> tail alone (AC1-ERR degradation).
    assert_eq!(
        subline_from(None, "/code/footnote"),
        Some("footnote".into())
    );
    // Trailing slash is trimmed before taking the tail.
    assert_eq!(
        subline_from(None, "/code/footnote/"),
        Some("footnote".into())
    );
    // No cwd -> no subline (AC1-EDGE: no sub-row emitted).
    assert_eq!(subline_from(None, ""), None);
    assert_eq!(subline_from(Some("main"), ""), Some("main".into()));
}

#[test]
fn cwd_basename_extracts_tail_and_handles_empty() {
    // x-6851 US3: the basename every row carries; empty -> None (AC4-EDGE:
    // no fabricated subline); trailing slash trimmed.
    assert_eq!(cwd_basename("/code/footnote"), Some("footnote".into()));
    assert_eq!(cwd_basename("/code/footnote/"), Some("footnote".into()));
    assert_eq!(cwd_basename("footnote"), Some("footnote".into()));
    assert_eq!(cwd_basename(""), None);
}

#[test]
fn agent_rows_composes_subline_from_branch_map() {
    // A paneless orphan row joins the off-loop branch map on its cwd; a cwd
    // absent from the map degrades to the tail alone (US4 wire composition).
    let mut core = empty_core();
    core.agents = vec![
        bg_row("worker", "/tmp/repos/footnote", Some("j1")),
        bg_row("other", "/tmp/repos/regready", Some("j2")),
    ];
    core.branch_by_cwd = [("/tmp/repos/footnote".to_string(), "main".to_string())]
        .into_iter()
        .collect();
    let rows = core.agent_rows();
    let footnote = rows.iter().find(|r| r.name == "worker").unwrap();
    assert_eq!(footnote.subline.as_deref(), Some("main · footnote"));
    // (x-6851 US3) Every row now carries its cwd basename on the wire.
    assert_eq!(footnote.cwd_base.as_deref(), Some("footnote"));
    let regready = rows.iter().find(|r| r.name == "other").unwrap();
    assert_eq!(
        regready.subline.as_deref(),
        Some("regready"),
        "no branch in map -> tail only"
    );
    assert_eq!(regready.cwd_base.as_deref(), Some("regready"));
}

#[test]
fn subline_with_title_joins_the_harness_title_beside_the_label() {
    // A row whose harness title differs from its label
    // renders the title in the subline slot, joined onto the branch
    // subline; a title that EQUALS the label renders nothing (the label
    // already says it), and no title falls back to the base subline.
    let mut a = bg_row("w1", "/tmp/repos/footnote", Some("j1"));
    a.harness_title = Some("king-title".into());
    assert_eq!(
        subline_with_title(&a, Some("main · footnote".into())),
        Some("king-title · main · footnote".into())
    );
    assert_eq!(
        subline_with_title(&a, None),
        Some("king-title".into()),
        "no base subline -> title alone"
    );
    a.harness_title = Some("w1".into());
    assert_eq!(
        subline_with_title(&a, Some("main · footnote".into())),
        Some("main · footnote".into()),
        "title == label -> base subline untouched"
    );
    a.harness_title = None;
    assert_eq!(
        subline_with_title(&a, None),
        None,
        "no title, no base -> none"
    );
}

#[test]
fn agent_rows_join_tail_from_session_map_and_leave_others_empty() {
    // (x-b186 AC2-HP / AC4-ERR) The tail joins on the row's claude session
    // uuid. Data honesty is the point: a row with no uuid, or a uuid with no
    // readable transcript, carries None so the table renders an EMPTY cell -
    // never an inferred or placeholder message.
    let mut core = empty_core();
    let mut with_tail = bg_row("worker", "/tmp/repos/footnote", Some("j1"));
    with_tail.claude_session_uuid = Some("uuid-live".into());
    let mut no_transcript = bg_row("silent", "/tmp/repos/footnote", Some("j2"));
    no_transcript.claude_session_uuid = Some("uuid-missing".into());
    // A codex row never carries a claude session uuid at all.
    let no_uuid = bg_row("codexer", "/tmp/repos/footnote", None);
    core.agents = vec![with_tail, no_transcript, no_uuid];
    core.tail_by_session = [("uuid-live".to_string(), "wired the reader".to_string())]
        .into_iter()
        .collect();

    let rows = core.agent_rows();
    let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
    assert_eq!(get("worker").tail.as_deref(), Some("wired the reader"));
    assert_eq!(
        get("silent").tail,
        None,
        "uuid with no readable transcript -> empty cell, not a placeholder"
    );
    assert_eq!(get("codexer").tail, None, "no uuid -> no tail");
}

#[test]
fn agent_tails_push_updates_rows_without_a_row_change() {
    // (codex P1) A transcript grows independently of the registry, so the
    // tail must be able to land with no row change behind it. Before this,
    // the tail pass only ran when the merged row set moved, which left the
    // column stale (or blank) indefinitely while an agent kept talking.
    let mut core = empty_core();
    let mut row = bg_row("worker", "/tmp/repos/footnote", Some("j1"));
    row.claude_session_uuid = Some("uuid-live".into());
    core.agents = vec![row];
    assert_eq!(core.agent_rows()[0].tail, None);

    core.handle_msg(CoreMsg::AgentTails {
        tails: [("uuid-live".to_string(), "said something new".to_string())]
            .into_iter()
            .collect(),
    });
    assert_eq!(
        core.agent_rows()[0].tail.as_deref(),
        Some("said something new"),
        "a tail-only push reaches the row"
    );
}
// (x-0f42 external-lifecycle sync and x-54fa routed-backlog families) moved verbatim into its own module: this file is over the
// shrink-only line, and test motion is the sanctioned shrink.
#[path = "server/tests/external_lifecycle_and_backlog_tests.rs"]
mod external_lifecycle_and_backlog_tests;

#[path = "server/tests/row_set_tests.rs"]
mod row_set_tests;

#[path = "server/tests/agent_launcher_tests.rs"]
mod agent_launcher_tests;

#[test]
fn classify_guard_registry_keeps_document_and_row_failures_distinct() {
    // Document-level malformation already failed closed and keeps its own
    // reason (x-0b40 leaves that arm untouched); a clean registry still
    // proceeds, the control that keeps the two refusals above honest.
    assert_eq!(
        classify_guard_registry("not json", 0).unwrap_err(),
        "agents registry malformed - target agent state unknown"
    );
    assert!(classify_guard_registry(
        r#"{"agents":[{"name":"a","cwd":"/w","status":"live","mux":{"session":"s","pane_id":1}}]}"#,
        0
    )
    .is_ok());
}

#[test]
fn rerun_allowed_on_a_plain_shell_pane() {
    // AC-HP (x-0b40): over a LOSSLESS read, a pane with no agent row is a
    // shell - rerun is always safe. The losslessness is the precondition,
    // not a given: `classify_guard_registry` is what refuses a registry
    // carrying a row with no readable pane binding BEFORE this predicate
    // runs, so absence reaching here really does mean absence.
    assert_eq!(rerun_allowed(&[], "main", 7), Ok(()));
    // Another agent on a different pane does not gate this one.
    assert_eq!(
        rerun_allowed(&[agent(9, Some(AgentBadge::Working), false)], "main", 7),
        Ok(())
    );
}

#[test]
fn idle_shell_takeover_verdicts() {
    // Take-over: the tab's only pane is a plain, pristine idle shell (the reported bug).
    assert!(idle_shell_takeover(1, None, true));
    // Refuse: the shell has run or started something (foreground program, exec, or a
    // background/stopped job) - not pristine, so `.` must not reap it.
    assert!(!idle_shell_takeover(1, None, false));
    // Refuse: more than one leaf - `.` is scoped to the "only pane" case; splits use h/j/k/l.
    assert!(!idle_shell_takeover(2, None, true));
    // Refuse: an agent / `pane run` pane (cmd set) is never a disposable shell.
    assert!(!idle_shell_takeover(1, Some("claude attach ab12"), true));
}

/// A scratch store dir for write-through tests, installed via the per-thread
/// path override so no test mutates the shared environment (no env race).
struct StoreScratch {
    dir: std::path::PathBuf,
}
impl StoreScratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-srv-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::squad_store::set_test_path(&dir);
        StoreScratch { dir }
    }
}
impl Drop for StoreScratch {
    fn drop(&mut self) {
        crate::squad_store::clear_test_path();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn named_member_squad(core: &mut Core, sid: u64, name: &str, pid: u64, attach: &str) {
    core.session.add_squad(
        sid,
        vec!["/repo".into()],
        Some(name.into()),
        Tab {
            name: None,
            id: sid,
            root: Node::Leaf(pid),
            focus: pid,
        },
    );
    core.squad_members.insert(
        sid,
        vec![crate::squad_store::StoredMember {
            attach_id: attach.into(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    core.attached.insert(attach.into(), pid);
}

fn stored_member(id: &str, tombstone: bool) -> crate::squad_store::StoredMember {
    crate::squad_store::StoredMember {
        attach_id: id.into(),
        tombstone,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: None,
        harness: None,
        harness_session_id: None,
        pane_id: None,
    }
}

#[test]
fn live_ids_from_marks_live_registry_and_roster_rows() {
    // AC1-HP hinges on a FRESH liveness read at first attach (self.agents is
    // still empty then). Pure over the raw file contents: an exited registry
    // row is dead, a live one and a roster worker are live.
    let reg = r#"{"agents":[
            {"name":"w","cwd":"/x","status":"live","provider":"claude","short_id":"c19cd2c3"},
            {"name":"d","cwd":"/x","status":"exited","provider":"claude","short_id":"deadbeef"}
        ]}"#;
    let roster = r#"{"workers":{"k":{"sessionId":"aa11bb22-xyz","cwd":"/y"}}}"#;
    let live = live_ids_from(Some(reg), Some(roster), 0);
    assert!(live.contains("c19cd2c3"), "a live registry row is live");
    assert!(!live.contains("deadbeef"), "an exited row is not live");
    assert!(
        live.contains("aa11bb22"),
        "a roster worker's short_id is live"
    );
    // Missing files (None) yield an empty live set.
    assert!(live_ids_from(None, None, 0).is_empty());
}

#[test]
fn picker_attach_into_unnamed_home_squad_persists_member() {
    // US2 root cause: a fresh picker-attach lands in the UNNAMED home squad
    // (seen_test_core's squad 1, origins=["/tmp/seen"], name=None), and its
    // membership now persists keyed by the squad's origins - so restore
    // rebuilds its pane and click == focus after a restart. Before the fix
    // persist_squad no-op'd on the unnamed squad and the store stayed empty.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    core.agents = vec![bg_row("bg", "/tmp/seen", Some("deadbee2"))];
    let new_pid = core.next_pane_id;

    core.command(client_id, Command::attach_agent("deadbee2"));

    assert_eq!(core.attached.get("deadbee2"), Some(&new_pid), "attached");
    assert!(
        core.squad_members[&1]
            .iter()
            .any(|m| m.attach_id == "deadbee2" && !m.tombstone),
        "picker-attached session recorded as a live member of the home squad"
    );
    // A second identical attach is idempotent - no duplicate member (AC1-EDGE).
    core.command(client_id, Command::attach_agent("deadbee2"));
    assert_eq!(
        core.squad_members[&1]
            .iter()
            .filter(|m| m.attach_id == "deadbee2")
            .count(),
        1,
        "re-attach writes membership idempotently"
    );
    // Persisted to the store keyed by origins (the squad has no name).
    let loaded = crate::squad_store::load();
    let lane = loaded
        .squads
        .iter()
        .find(|s| s.origins == vec!["/tmp/seen".to_string()])
        .expect("unnamed home lane persisted by origins");
    assert!(lane.name.is_empty(), "persisted unnamed");
    assert!(lane.members.iter().any(|m| m.attach_id == "deadbee2"));
    core.reap_pane(new_pid);
}

#[test]
fn picker_attach_spawn_failure_persists_no_member() {
    // AC1-ERR: a fresh attach whose spawn fails records no mapping AND no
    // member - the row stays watch-only, recoverable on retry, and no phantom
    // is written to the store.
    set_attach_program(&["/nonexistent/definitely-not-a-real-binary-xyz"]);
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    core.agents = vec![bg_row("bg", "/tmp/seen", Some("deadbee2"))];

    core.command(client_id, Command::attach_agent("deadbee2"));

    assert!(
        !core.attached.contains_key("deadbee2"),
        "no mapping on spawn failure"
    );
    assert!(
        core.squad_members
            .get(&1)
            .is_none_or(|m| m.iter().all(|x| x.attach_id != "deadbee2")),
        "no phantom member persisted"
    );
    let loaded = crate::squad_store::load();
    assert!(
        loaded
            .squads
            .iter()
            .all(|s| s.members.iter().all(|m| m.attach_id != "deadbee2")),
        "nothing written to the store"
    );
}
// ---- (x-d285) mux gestures through the canonical re-entry plan -------

/// A verdict shaped like the resolver's machine output for a routed
/// claude row: the account's env assignments prefix the provider argv.
/// `FNO_ACCOUNT` is the positive marker - only the verdict's env prefix
/// ever sets it on an attach pane (the reconstructed argv carries none
/// for a catalog row with no recorded account).
fn staged_reentry_verdict() -> ReentryVerdict {
    ReentryVerdict {
        argv: vec!["/bin/cat".into(), "deadbee1".into()],
        env: vec![
            "FNO_ACCOUNT=makers".into(),
            "CLAUDE_CONFIG_DIR=/acct/makers/cfg".into(),
        ],
        config_dir: Some("/acct/makers/cfg".into()),
    }
}

fn claude_bg_row(name: &str, cwd: &str, attach: Option<&str>) -> RegistryAgent {
    let mut row = bg_row(name, cwd, attach);
    row.harness = Some("claude".into());
    row
}

#[test]
fn reentry_verdict_parse_accepts_only_a_resolved_plan() {
    // The machine grammar: a resolved verdict with argv + env parses; an
    // unresolved one, malformed JSON, a missing argv, or an empty argv is
    // the same refusal - nothing may spawn off it.
    let ok = br#"{"resolved":true,"argv":["claude","attach","deadbee1"],"env":{"FNO_ACCOUNT":"makers"},"claude_config_dir":"/acct/cfg"}"#;
    let verdict = ReentryVerdict::from_plan_json(ok).expect("a resolved plan parses");
    assert_eq!(verdict.argv, vec!["claude", "attach", "deadbee1"]);
    assert_eq!(verdict.env, vec!["FNO_ACCOUNT=makers"]);
    assert_eq!(
        verdict.config_dir.as_deref(),
        Some(std::path::Path::new("/acct/cfg"))
    );
    assert_eq!(
        verdict.prefixed_argv(),
        vec!["env", "FNO_ACCOUNT=makers", "claude", "attach", "deadbee1"],
        "the env assignments prefix the provider argv"
    );
    for bad in [
        br#"{"resolved":false}"#.as_slice(),
        br#"{"argv":["claude"]}"#.as_slice(),
        br#"not json"#.as_slice(),
        br#"{"resolved":true,"argv":[]}"#.as_slice(),
        // A non-string env value parsed as an empty assignment would
        // launch with a blanked namespace; it refuses like the rest.
        br#"{"resolved":true,"argv":["claude"],"env":{"CLAUDE_CONFIG_DIR":5}}"#.as_slice(),
    ] {
        assert!(
            ReentryVerdict::from_plan_json(bad).is_err(),
            "refused: {}",
            String::from_utf8_lossy(bad)
        );
    }
    // No env means the argv runs bare, with no env(1) prefix.
    let bare = br#"{"resolved":true,"argv":["claude"]}"#;
    assert_eq!(
        ReentryVerdict::from_plan_json(bare)
            .unwrap()
            .prefixed_argv(),
        vec!["claude"]
    );
}

#[test]
fn attach_gesture_runs_the_staged_reentry_verdict() {
    // AC5-HP: an AttachAgent gesture for a claude row spawns the
    // canonical verdict's env-prefixed argv, never a locally
    // reconstructed one. The staged verdict stands in for what the
    // ReentryPlanReady continuation delivers.
    set_attach_program(&["/bin/cat"]); // the legacy argv, which must NOT run
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    core.agents = vec![claude_bg_row("routed-glm", "/tmp/seen", Some("deadbee1"))];
    core.reentry_verdict = Some(staged_reentry_verdict());
    let new_pid = core.next_pane_id;

    core.command(client_id, Command::attach_agent("deadbee1"));

    let entry = core.panes.get(&new_pid).expect("the attach spawned");
    assert_eq!(
        entry.account.as_deref(),
        Some("makers"),
        "the verdict's env prefix ran; a reconstructed argv carries no account"
    );
    assert_eq!(entry.cmd.as_deref(), Some("cat"));
    assert_eq!(core.attached.get("deadbee1"), Some(&new_pid));
    assert!(
        core.reentry_verdict.is_none(),
        "the verdict was consumed, not left staged"
    );
    core.reap_pane(new_pid);
}

#[test]
fn attach_gesture_refusal_is_a_notice_and_starts_no_pane() {
    // The resolver's refusal routes back as a one-line notice; no pane,
    // no mapping, and no staged verdict leaks into a later gesture.
    let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
    core.agents = vec![claude_bg_row("routed-glm", "/tmp/seen", Some("deadbee1"))];
    let panes_before = core.panes.len();

    core.handle(CoreMsg::ReentryPlanReady {
        id: client_id,
        request: Box::new(ReentrySpawnRequest::Attach {
            attach_id: "deadbee1".into(),
            placement: PanePlacement::default(),
        }),
        verdict: Err("route settings missing: floor-only file".into()),
    });

    assert_eq!(core.panes.len(), panes_before, "no pane started");
    assert!(!core.attached.contains_key("deadbee1"));
    assert!(core.reentry_verdict.is_none());
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("floor-only file"),
        "the refusal names its reason: {notices}"
    );
}

#[test]
fn resume_agent_runs_the_staged_reentry_verdict() {
    // AC5-HP: a ResumeAgent gesture for a dead claude row spawns the
    // canonical verdict's argv (env prefix + the recorded session id),
    // never the bare `claude --resume` form.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-reentry-resume");
    std::fs::create_dir_all(&cwd).unwrap();
    let shell = core
        .spawn_pane(24, 80, cwd.to_string_lossy().as_ref())
        .unwrap();
    core.session.add_squad(
        7,
        vec![cwd.to_string_lossy().into_owned()],
        None,
        Tab {
            name: None,
            id: 70,
            root: Node::Leaf(shell),
            focus: shell,
        },
    );
    let mut row = exited_claude_row("routed-glm", Some("01a027ad-fe00-7c12-a116-9ee37c6bdfec"));
    row.cwd = cwd.to_string_lossy().into_owned();
    row.harness = Some("claude".into());
    row.harness_session_id = Some("01a027ad-fe00-7c12-a116-9ee37c6bdfec".into());
    core.agents = vec![row];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.reentry_verdict = Some(ReentryVerdict {
        argv: vec![
            "/bin/cat".into(),
            "--resume".into(),
            "01a027ad-fe00-7c12-a116-9ee37c6bdfec".into(),
        ],
        env: vec!["FNO_ACCOUNT=makers".into()],
        config_dir: Some("/acct/makers/cfg".into()),
    });

    core.command(
        1,
        Command::ResumeAgent {
            name: "routed-glm".into(),
        },
    );

    let notices = drain_notices(&mut rx).join("\n");
    assert!(notices.contains("resumed routed-glm"), "{notices}");
    let new_panes: Vec<u64> = core
        .panes
        .keys()
        .filter(|&&p| p != shell)
        .copied()
        .collect();
    assert_eq!(new_panes.len(), 1, "exactly one resumed pane");
    let entry = core.panes.get(&new_panes[0]).unwrap();
    assert_eq!(
        entry.account.as_deref(),
        Some("makers"),
        "the verdict's env prefix ran"
    );
    assert_eq!(
        entry.name.as_deref(),
        Some("routed-glm"),
        "the resumed pane is titled from the registry row"
    );
    for pid in new_panes {
        core.reap_pane(pid);
    }
    core.reap_pane(shell);
    let _ = std::fs::remove_dir_all(&cwd);
}

/// Run the bulk-restore apply half synchronously and return its rows.
/// The production claude-plan split (WorkspaceRestore -> off-loop
/// resolution -> apply) is exercised by the handler split itself; these
/// tests drive the apply half directly so the gates, rows and rerun
/// semantics are deterministic.
#[test]
fn recruit_consumes_staged_batch_plans() {
    // The picker is N spawns under one gesture: every claude id's plan
    // arrives keyed by attach id. A verdict spawns with its account; a
    // refusal skips the id with the reason named. (The batch fire itself
    // needs a runtime; this stages the replay state directly.)
    set_attach_program(&["/bin/cat"]);
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.agents = vec![
        claude_bg_row("routed-a", "/x", Some("deadbee1")),
        claude_bg_row("routed-b", "/x", Some("deadbee2")),
    ];
    core.batch_plans
        .insert("deadbee1".into(), Ok(staged_reentry_verdict()));
    core.batch_plans.insert(
        "deadbee2".into(),
        Err("row records no launch account".into()),
    );
    let new_pid = core.next_pane_id;

    core.command(
        1,
        Command::RecruitAgents {
            squad: "team".into(),
            ids: vec!["deadbee1".into(), "deadbee2".into()],
        },
    );

    assert_eq!(
        core.panes[&new_pid].account.as_deref(),
        Some("makers"),
        "the planned id spawned with its verdict account"
    );
    assert!(
        !core.attached.contains_key("deadbee2"),
        "the refused id started no pane"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("no launch account"),
        "the refusal is visible in the recruit report: {notices}"
    );
    core.reap_pane(new_pid);
}

#[test]
fn batch_plans_ready_stages_and_replays_the_recruit() {
    // The BatchPlansReady continuation stages the map and re-enters the
    // recruit command, which then consumes the staged plans end to end.
    set_attach_program(&["/bin/cat"]);
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.agents = vec![claude_bg_row("routed-a", "/x", Some("deadbee1"))];
    let new_pid = core.next_pane_id;
    let mut plans = HashMap::new();
    plans.insert("deadbee1".into(), Ok(staged_reentry_verdict()));

    core.handle(CoreMsg::BatchPlansReady {
        id: 1,
        plans,
        replay: Box::new(BatchReplay::Recruit {
            squad: "team".into(),
            ids: vec!["deadbee1".into()],
        }),
    });

    assert_eq!(
        core.panes[&new_pid].account.as_deref(),
        Some("makers"),
        "the replayed recruit spawned the planned pane"
    );
    core.reap_pane(new_pid);
}

#[test]
fn recruit_refuses_blank_name_and_empty_ids() {
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RecruitAgents {
            squad: "  ".into(),
            ids: vec!["c19cd2c3".into()],
        },
    );
    core.command(
        1,
        Command::RecruitAgents {
            squad: "team".into(),
            ids: vec![],
        },
    );
    assert_eq!(
        core.session.squads.len(),
        1,
        "no workspace created on refusal"
    );
    assert!(core.squad_members.is_empty());
}

#[test]
fn recruit_skips_bad_unattachable_and_deduped_ids() {
    // All ids fail a gate before any spawn: a bad-shape id, a not-attachable
    // id, and one already recruited (in self.attached). No squad, no panes.
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.clients.push(client(1, 5, (24, 80), false));
    core.attached.insert("aaaaaaaa".into(), 1); // already paned -> dedup skip
    core.command(
        1,
        Command::RecruitAgents {
            squad: "team".into(),
            ids: vec![
                "nothex!!".into(), // bad shape
                "deadbeef".into(), // not in the catalog -> not attachable
                "aaaaaaaa".into(), // already recruited
            ],
        },
    );
    assert_eq!(core.session.squads.len(), 1, "no new workspace on all-skip");
    assert!(!core.squad_members.contains_key(&2), "no squad 2 minted");
}

#[test]
fn recruit_refuses_a_name_persisted_but_not_live() {
    // codex P2: recruiting into a name that exists only in the store (another
    // server created it, or restore skipped it) must NOT create a new live
    // squad - that would upsert by name and drop the persisted members.
    let _s = StoreScratch::new("recruit-persisted");
    crate::squad_store::upsert(
        "ghost",
        "",
        &["/repo".into()],
        &[stored_member("c19cd2c3", false)],
    )
    .unwrap();
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec!["/x".into()],
        None,
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.clients.push(client(1, 5, (24, 80), false));
    core.command(
        1,
        Command::RecruitAgents {
            squad: "ghost".into(),
            ids: vec!["deadbeef".into()],
        },
    );
    // The persisted entry is untouched; no live squad was minted for it.
    let loaded = crate::squad_store::load();
    assert_eq!(loaded.squads.len(), 1);
    assert_eq!(
        loaded.squads[0].members,
        vec![stored_member("c19cd2c3", false)],
        "the persisted members are not clobbered"
    );
    assert!(
        !core
            .session
            .squads
            .iter()
            .any(|s| s.name.as_deref() == Some("ghost")),
        "no live squad created for the persisted name"
    );
}

#[test]
fn renaming_a_members_tab_persists_the_tab_name() {
    // x-0f9d US4: renaming the tab hosting a persisted member writes the
    // chosen name into the store, re-derived at persist time, so a restart
    // can restore the tab named (AC1-HP persistence half).
    let _s = StoreScratch::new("tab-name-persist");
    let mut core = empty_core();
    // A live named squad whose member's pane (1) is the leaf of tab id 1.
    named_member_squad(&mut core, 1, "harden", 1, "c19cd2c3");
    core.clients.push(client(1, 1, (24, 80), false));

    // Unnamed first: the store carries no tab name.
    core.persist_squad(1);
    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads[0].members[0].tab_name, None,
        "unnamed -> None"
    );

    // Rename the hosting tab; the rename handler persists it.
    core.command(
        1,
        Command::RenameTab {
            tab: 1,
            name: "reviews".into(),
        },
    );
    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads[0].members[0].tab_name.as_deref(),
        Some("reviews"),
        "the chosen tab name is persisted for restore"
    );

    // Clearing the name (blank rename) drops it from the store too.
    // Re-register the sender: the prior rename's layout push reaped the
    // test client's dropped receiver (as in rename_tab_round_trips).
    core.clients.push(client(1, 1, (24, 80), false));
    core.command(
        1,
        Command::RenameTab {
            tab: 1,
            name: "".into(),
        },
    );
    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads[0].members[0].tab_name, None,
        "clearing the name clears the stored tab_name"
    );
}

#[test]
fn persist_preserves_tab_name_when_member_pane_is_unresolvable() {
    // x-0f9d US4 / codex P1: a member whose pane cannot be resolved (a
    // transient restore reattach failure, or a tombstone) keeps its stored
    // tab_name across a persist - member_tab_name returning an unresolvable
    // None must PRESERVE the stored name, not clobber it to None.
    let _s = StoreScratch::new("preserve-unresolvable-tab-name");
    let mut core = empty_core();
    named_member_squad(&mut core, 1, "harden", 1, "c19cd2c3");
    // Stored name present, but drop the pane mapping so the pane is
    // unresolvable (as after a failed reattach at restore).
    core.squad_members.get_mut(&1).unwrap()[0].tab_name = Some("reviews".into());
    core.attached.remove("c19cd2c3");
    core.persist_squad(1);
    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads[0].members[0].tab_name.as_deref(),
        Some("reviews"),
        "an unresolvable pane preserves the stored tab name"
    );
}

#[test]
fn squad_rename_after_tab_rename_keeps_the_tab_name() {
    // x-0f9d US4 / codex review: persist_squad refreshes the AUTHORITATIVE
    // in-memory member list, so a later RenameSquad (which persists that
    // list verbatim through squad_store::rename) does not erase a
    // freshly-renamed tab name.
    let _s = StoreScratch::new("tab-name-survives-squad-rename");
    let mut core = empty_core();
    named_member_squad(&mut core, 1, "harden", 1, "c19cd2c3");
    core.clients.push(client(1, 1, (24, 80), false));

    // Name the tab (persists tab_name AND refreshes squad_members in place).
    core.command(
        1,
        Command::RenameTab {
            tab: 1,
            name: "reviews".into(),
        },
    );
    // Rename the squad; it writes the in-memory member list to the store.
    core.clients.push(client(1, 1, (24, 80), false));
    core.command(
        1,
        Command::RenameSquad {
            squad: 1,
            name: "hardened".into(),
        },
    );

    let loaded = crate::squad_store::load();
    let sq = loaded
        .squads
        .iter()
        .find(|s| s.name == "hardened")
        .expect("renamed squad persisted");
    assert_eq!(
        sq.members[0].tab_name.as_deref(),
        Some("reviews"),
        "the tab name survives the squad rename"
    );
}

#[test]
fn dismiss_member_removes_a_tombstone_and_refuses_unknown() {
    let _s = StoreScratch::new("dismiss");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.squad_members.insert(
        7,
        vec![
            stored_member("c19cd2c3", true),
            stored_member("deadbeef", false),
        ],
    );
    core.persist_squad(7);
    core.clients.push(client(1, 5, (24, 80), false));
    // Dismiss the live (non-tombstone) member: refused, nothing removed.
    core.command(
        1,
        Command::DismissMember {
            squad: 7,
            attach_id: "deadbeef".into(),
        },
    );
    assert_eq!(
        core.squad_members[&7].len(),
        2,
        "a live member is not dismissable"
    );
    // Dismiss the tombstone: removed + persisted.
    core.command(
        1,
        Command::DismissMember {
            squad: 7,
            attach_id: "c19cd2c3".into(),
        },
    );
    assert_eq!(core.squad_members[&7].len(), 1);
    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads[0].members,
        vec![stored_member("deadbeef", false)]
    );
    // An unknown workspace is refused.
    core.command(
        1,
        Command::DismissMember {
            squad: 999,
            attach_id: "c19cd2c3".into(),
        },
    );
}

#[test]
fn agent_rows_tombstoned_members_render_through_their_registry_row_only() {
    // supersedes the synthesized cc- ghost A tombstoned member
    // joining NO registry row renders NOTHING (never a synthesized
    // ghost); a re-paned id renders pane-hosted, never doubled.
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.squad_members.insert(
        7,
        vec![
            stored_member("c19cd2c3", true),
            stored_member("deadbeef", true),
        ],
    );
    // "deadbeef" is re-paned this session -> skipped by the attach guard.
    core.attached.insert("deadbeef".into(), 99);
    let rows = core.agent_rows();
    let tomb: Vec<_> = rows.iter().filter(|r| r.tombstone).collect();
    assert_eq!(
        tomb.len(),
        0,
        "no member joins a registry row here, so no tombstone row renders"
    );
    assert!(
        rows.iter().all(|r| !r.name.starts_with("cc-")),
        "the synthesized cc- ghost name is gone: {rows:?}"
    );
}

#[test]
fn persist_squad_refreshes_worker_harness_identity_from_registry() {
    let _s = StoreScratch::new("persist-worker-identity");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("workers".into()),
        Tab {
            name: None,
            id: 7,
            root: Node::Leaf(100),
            focus: 100,
        },
    );
    core.squad_members.insert(
        7,
        vec![crate::squad_store::StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("worker".into()),
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    let mut row = bg_row("worker", "/repo", None);
    row.harness = Some("codex".into());
    row.harness_session_id = Some("full-codex-session".into());
    core.agents = vec![row];

    core.persist_squad(7);

    let member = crate::squad_store::load().squads[0].members[0].clone();
    assert_eq!(member.harness.as_deref(), Some("codex"));
    assert_eq!(
        member.harness_session_id.as_deref(),
        Some("full-codex-session")
    );
}

#[test]
fn persist_squad_does_not_bind_an_ambiguous_worker_name() {
    let _s = StoreScratch::new("persist-worker-ambiguous");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("workers".into()),
        leaf_tab(7, 100),
    );
    core.squad_members.insert(
        7,
        vec![crate::squad_store::StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("reused-name".into()),
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    let mut first = bg_row("reused-name", "/repo", None);
    first.harness = Some("codex".into());
    first.harness_session_id = Some("session-one".into());
    let mut second = first.clone();
    second.harness = Some("claude".into());
    second.harness_session_id = Some("session-two".into());
    core.agents = vec![first, second];

    core.persist_squad(7);

    let member = crate::squad_store::load().squads[0].members[0].clone();
    assert!(member.harness.is_none());
    assert!(member.harness_session_id.is_none());
}

#[test]
fn record_worker_member_keeps_repeated_names_as_distinct_members() {
    let _s = StoreScratch::new("record-worker-repeated-name");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("workers".into()),
        leaf_tab(7, 100),
    );
    let mut first = bg_row("reused-name", "/repo", None);
    first.harness = Some("codex".into());
    first.harness_session_id = Some("session-one".into());
    let mut second = first.clone();
    second.harness = Some("claude".into());
    second.harness_session_id = Some("session-two".into());
    core.agents = vec![first, second];

    core.record_worker_member(7, "reused-name", 100, "/repo", Some("session-one"));
    core.record_worker_member(7, "reused-name", 101, "/repo", Some("session-two"));

    let members = &core.squad_members[&7];
    assert_eq!(members.len(), 2);
    assert!(members.iter().any(|member| {
        member.harness.as_deref() == Some("codex")
            && member.harness_session_id.as_deref() == Some("session-one")
    }));
    assert!(members.iter().any(|member| {
        member.harness.as_deref() == Some("claude")
            && member.harness_session_id.as_deref() == Some("session-two")
    }));
}

#[test]
fn record_worker_member_does_not_collapse_ambiguous_registry_names() {
    let _s = StoreScratch::new("record-worker-ambiguous-name");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("workers".into()),
        leaf_tab(7, 100),
    );
    let first = bg_row("reused-name", "/repo", None);
    let mut second = first.clone();
    second.harness = Some("claude".into());
    second.harness_session_id = Some("session-two".into());
    core.agents = vec![first, second];

    core.record_worker_member(7, "reused-name", 100, "/repo", None);
    core.record_worker_member(7, "reused-name", 101, "/repo", None);

    assert_eq!(core.squad_members[&7].len(), 2);
}

#[test]
fn persist_squad_does_not_copy_one_identity_to_repeated_name_members() {
    let _s = StoreScratch::new("persist-worker-repeated-name");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("workers".into()),
        leaf_tab(7, 100),
    );
    core.squad_members.insert(
        7,
        vec![
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                tombstone_reason: None,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("reused-name".into()),
                harness: None,
                harness_session_id: None,
                pane_id: None,
            },
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                tombstone_reason: None,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("reused-name".into()),
                harness: None,
                harness_session_id: None,
                pane_id: None,
            },
        ],
    );
    let mut row = bg_row("reused-name", "/repo", None);
    row.harness = Some("codex".into());
    row.harness_session_id = Some("session-one".into());
    core.agents = vec![row];

    core.persist_squad(7);

    let members = &crate::squad_store::load().squads[0].members;
    assert!(members
        .iter()
        .all(|member| member.harness.is_none() && member.harness_session_id.is_none()));
}

#[test]
fn agent_rows_assign_repeated_worker_names_by_exact_identity() {
    let mut core = empty_core();
    core.session_name = "main".into();
    core.session
        .add_squad(7, vec!["/one".into()], Some("one".into()), leaf_tab(7, 100));
    core.session
        .add_squad(8, vec!["/two".into()], Some("two".into()), leaf_tab(8, 101));
    let member = |harness: &str, session_id: &str| crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: Some("/elsewhere".into()),
        worker: Some("reused-name".into()),
        harness: Some(harness.into()),
        harness_session_id: Some(session_id.into()),
        pane_id: None,
    };
    core.squad_members
        .insert(7, vec![member("codex", "session-one")]);
    core.squad_members
        .insert(8, vec![member("claude", "session-two")]);
    let mut first = bg_row("reused-name", "/elsewhere", None);
    first.harness = Some("codex".into());
    first.harness_session_id = Some("session-one".into());
    let mut second = first.clone();
    second.harness = Some("claude".into());
    second.harness_session_id = Some("session-two".into());
    core.agents = vec![first, second];

    let rows: Vec<_> = core
        .agent_rows()
        .into_iter()
        .filter(|row| row.name == "reused-name")
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].squad, Some(7));
    assert_eq!(rows[1].squad, Some(8));
}

#[test]
fn member_pane_uses_harness_session_pair_when_worker_names_repeat() {
    let mut core = empty_core();
    core.worker_pane.insert("reused-name".into(), vec![10, 11]);
    core.worker_session_pane
        .insert(("codex".into(), "session-one".into()), 10);
    core.worker_session_pane
        .insert(("claude".into(), "session-two".into()), 11);
    let first = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("reused-name".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("session-one".into()),
        pane_id: None,
    };
    let second = crate::squad_store::StoredMember {
        harness: Some("claude".into()),
        harness_session_id: Some("session-two".into()),
        ..first.clone()
    };
    assert_eq!(core.member_pane(&first), Some(10));
    assert_eq!(core.member_pane(&second), Some(11));
    assert!(core.unique_worker_pane_by_name("reused-name").is_err());
}

#[test]
fn resumed_pane_resolves_fno_id_from_its_resume_birthright() {
    // (x-b029) AC3-HP: a pane the daemon re-homed through the resume path
    // resolves its fno_id from the (harness, session) record the resume
    // stamped at birth, even though the registry FILE's row still points
    // at the pre-restart pane. The id is the one the resume argv carries,
    // never a guess.
    let mut core = empty_core();
    let uuid = "01a05fce-0000-7ccc-8000-000000000000";
    let mut row = bg_row("t-resumed-agy", "/repo", None);
    row.harness = Some("codex".into());
    row.harness_session_id = Some(uuid.into());
    row.mux = Some(("test".into(), 999));
    core.agents = vec![row];
    core.worker_session_pane
        .insert(("codex".into(), uuid.into()), 77);
    // The stale registry ref alone does not resolve the new pane...
    assert_eq!(
        core.fno_id_for_pane(999),
        Some(uuid.to_string()),
        "the stale row still resolves ITS OWN recorded pane"
    );
    assert_eq!(core.fno_id_for_pane(77), Some(uuid.to_string()));
    // A pane the daemon never re-homed and no row hosts stays untracked.
    assert_eq!(core.fno_id_for_pane(78), None);
}

#[test]
fn persist_squad_writes_named_workspace_and_dupe_is_taken() {
    let _s = StoreScratch::new("persist-named");
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 100, "c19cd2c3");
    core.persist_squad(7);
    let loaded = crate::squad_store::load();
    assert_eq!(loaded.squads.len(), 1);
    assert_eq!(loaded.squads[0].name, "harden");
    assert_eq!(loaded.squads[0].origins, vec!["/repo".to_string()]);
    // A live named squad is taken; a persisted-but-not-live one is too.
    assert!(core.named_squad_taken("harden"), "live name is taken");
    core.session.squads.clear();
    assert!(core.named_squad_taken("harden"), "persisted name is taken");
    assert!(!core.named_squad_taken("nope-zzz"));
}

#[test]
fn persist_squad_unnamed_same_origin_upserts_one_row_across_persists() {
    // x-e447 AC-HP1: a repo's home squad derives its durable key from its
    // origin, so two server lifetimes (a fresh in-memory squad each) upsert
    // onto ONE row instead of appending. The old random mint minted a fresh
    // key per lifetime, so upsert never matched the prior row.
    let _s = StoreScratch::new("persist-unnamed-origin");
    let repo = "/repo/e447";
    let expected_key = crate::squad_store::origin_key(&[repo.to_string()]);
    // Lifetime 1: a fresh unnamed home squad, persisted.
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec![repo.into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.persist_squad(1);
    assert_eq!(crate::squad_store::load().squads.len(), 1);
    // Lifetime 2: the in-memory squad is GONE (process restarted); a fresh
    // one for the same repo persists again. The derived key matches, so it
    // upserts onto the same row instead of appending.
    let mut core = empty_core();
    core.session.add_squad(
        1,
        vec![repo.into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.persist_squad(1);
    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads.len(),
        1,
        "second lifetime upserts onto one row, not a second"
    );
    assert_eq!(
        loaded.squads[0].key, expected_key,
        "key is the derived origin key, not a fresh mint"
    );
    assert_eq!(loaded.squads[0].origins, vec![repo.to_string()]);
}

#[test]
fn persist_squad_unnamed_originless_mints_a_random_key() {
    // x-e447 AC-EDGE1: an unnamed squad with NO origins has no stable referent,
    // so it mints a random key (not the fixed hash of the empty set). Two such
    // squads get distinct keys and stay distinct rows.
    let _s = StoreScratch::new("persist-unnamed-originless");
    let mk = |core: &mut Core, sid: u64| {
        core.session.add_squad(
            sid,
            vec![],
            None,
            Tab {
                name: None,
                id: sid,
                root: Node::Leaf(sid),
                focus: sid,
            },
        );
    };
    let mut core = empty_core();
    mk(&mut core, 1);
    core.persist_squad(1);
    mk(&mut core, 2);
    core.persist_squad(2);
    let loaded = crate::squad_store::load();
    assert_eq!(
        loaded.squads.len(),
        2,
        "two originless squads stay distinct"
    );
    assert_ne!(loaded.squads[0].key, loaded.squads[1].key);
    assert!(
        loaded.squads.iter().all(|s| !s.key.is_empty()),
        "each minted a key"
    );
}

#[test]
fn churn_tombstones_and_user_close_de_recruits() {
    // AC4-EDGE: a worker dying on its own tombstones its member (survives as
    // a persisted, tombstoned entry). AC3-EDGE: the user closing the pane
    // de-recruits it (gone from the store).
    let _s = StoreScratch::new("churn-vs-user");
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 100, "c19cd2c3");
    core.persist_squad(7);

    // Churn: the member is tombstoned, not removed.
    let ctx = core.member_ctx(100);
    assert!(ctx.is_some(), "pane 100 resolves to a persisted member");
    core.reconcile_member_close(ctx, true);
    let after_churn = crate::squad_store::load();
    assert_eq!(after_churn.squads[0].members.len(), 1);
    assert!(
        after_churn.squads[0].members[0].tombstone,
        "churn tombstones"
    );

    // User close of the still-live squad de-recruits the member.
    let ctx = core.member_ctx(100);
    core.reconcile_member_close(ctx, false);
    let after_user = crate::squad_store::load();
    assert_eq!(after_user.squads.len(), 1, "workspace survives");
    assert!(
        after_user.squads[0].members.is_empty(),
        "member de-recruited"
    );
}

#[test]
fn cross_squad_member_move_de_recruits_from_the_source() {
    // (x-d6a8, codex P1) Moving a persisted member's pane into another
    // squad's tab de-recruits it from the source workspace - its
    // squad_members entry must not linger under a squad it no longer lives
    // in. Regression for the codex re-review finding on move_pane_cross_tab.
    let _s = StoreScratch::new("cross-squad-member-move");
    let mut core = empty_core();
    // Source workspace "harden" (squad 7): member pane 100 in tab 7, plus a
    // second tab (pane 101) so the squad SURVIVES the move.
    named_member_squad(&mut core, 7, "harden", 100, "c19cd2c3");
    core.session.squad_mut(7).unwrap().tabs.push(Tab {
        name: None,
        id: 70,
        root: Node::Leaf(101),
        focus: 101,
    });
    core.persist_squad(7);
    // Destination squad 8 with a tab to drop into.
    core.session.add_squad(
        8,
        vec!["/other".into()],
        Some("review".into()),
        Tab {
            name: None,
            id: 8,
            root: Node::Leaf(200),
            focus: 200,
        },
    );

    let src = core.session.find_pane(100).expect("member pane live");
    let dst = core.session.find_pane(200).expect("anchor pane live");
    assert_ne!(src.0, dst.0, "precondition: a cross-squad move");
    core.move_pane_cross_tab(100, src, 200, dst, Dir::Right)
        .expect("cross-squad move");

    // The pane moved into squad 8...
    assert_eq!(
        core.session.find_pane(100).map(|(s, _)| s),
        Some(8),
        "pane moved to squad 8"
    );
    // ...and its membership no longer lingers in the persisted source.
    let loaded = crate::squad_store::load();
    let harden = loaded
        .squads
        .iter()
        .find(|s| s.name == "harden")
        .expect("source workspace survives its second tab");
    assert!(
        harden.members.iter().all(|m| m.attach_id != "c19cd2c3"),
        "the moved member is de-recruited from the source workspace"
    );
}

#[test]
fn same_squad_member_move_refreshes_the_stored_tab_name() {
    // (x-d6a8, codex P1) Relocating a persisted member BETWEEN tabs in the
    // same squad keeps its membership but must refresh its stored tab_name,
    // else a restart before the next persisting action restores it to the old
    // tab.
    let _s = StoreScratch::new("same-squad-member-tabname");
    let mut core = empty_core();
    // Workspace "harden" (squad 7): member pane 100 in tab "old", plus a tab
    // "new" (pane 101) to relocate it beside.
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: Some("old".into()),
            id: 7,
            root: Node::Leaf(100),
            focus: 100,
        },
    );
    core.session.squad_mut(7).unwrap().tabs.push(Tab {
        name: Some("new".into()),
        id: 70,
        root: Node::Leaf(101),
        focus: 101,
    });
    core.squad_members.insert(
        7,
        vec![crate::squad_store::StoredMember {
            attach_id: "c19cd2c3".into(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: Some("old".into()),
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    core.attached.insert("c19cd2c3".into(), 100);
    core.persist_squad(7);

    // Move member pane 100 from tab "old" beside pane 101 in tab "new".
    let src = core.session.find_pane(100).expect("member pane");
    let dst = core.session.find_pane(101).expect("dst pane");
    assert_eq!(src.0, dst.0, "precondition: a same-squad move");
    assert_ne!(src.1, dst.1, "precondition: a cross-tab move");
    core.move_pane_cross_tab(100, src, 101, dst, Dir::Right)
        .expect("same-squad member move");

    let loaded = crate::squad_store::load();
    let harden = loaded
        .squads
        .iter()
        .find(|s| s.name == "harden")
        .expect("workspace survives");
    let member = harden
        .members
        .iter()
        .find(|m| m.attach_id == "c19cd2c3")
        .expect("member kept");
    assert_eq!(
        member.tab_name.as_deref(),
        Some("new"),
        "the stored tab_name follows the member into its new tab"
    );
}

#[test]
fn cross_squad_move_of_a_named_workspace_shell_depersists_it() {
    // (x-d6a8, codex P1) Dragging a named workspace's own (non-member) shell
    // into another squad empties and removes the workspace; its persisted
    // entry and reserved name must not survive to resurrect it on restart.
    let _s = StoreScratch::new("depersist-emptied-workspace");
    let mut core = empty_core();
    // A named workspace "review" (squad 7) with a single NON-member shell.
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("review".into()),
        Tab {
            name: None,
            id: 7,
            root: Node::Leaf(100),
            focus: 100,
        },
    );
    core.squad_members.insert(7, Vec::new()); // named, no recruited members
    core.persist_squad(7);
    assert!(
        crate::squad_store::load()
            .squads
            .iter()
            .any(|s| s.name == "review"),
        "precondition: the workspace is persisted"
    );
    core.session.add_squad(
        8,
        vec!["/other".into()],
        Some("other".into()),
        Tab {
            name: None,
            id: 8,
            root: Node::Leaf(200),
            focus: 200,
        },
    );

    let src = core.session.find_pane(100).expect("shell pane");
    let dst = core.session.find_pane(200).expect("dst pane");
    core.move_pane_cross_tab(100, src, 200, dst, Dir::Right)
        .expect("cross-squad shell move");

    assert!(core.session.squad(7).is_none(), "source squad removed");
    assert!(
        !core.squad_members.contains_key(&7),
        "in-memory members entry cleared"
    );
    assert!(
        crate::squad_store::load()
            .squads
            .iter()
            .all(|s| s.name != "review"),
        "the emptied workspace is depersisted, not resurrected on restart"
    );
    assert!(
        !core.named_squad_taken("review"),
        "its name is no longer reserved"
    );
}

#[test]
fn break_of_a_member_pane_refreshes_the_stored_tab_name() {
    // (x-d6a8, codex P1) Breaking a persisted member's pane into a new tab
    // changes its hosting tab; the stored tab_name must refresh (inside the
    // shared pane_break helper, so the script and drag paths agree), else a
    // restart restores the member to its old tab.
    let _s = StoreScratch::new("break-member-tabname");
    let mut core = empty_core();
    // Workspace "harden" (squad 7): member pane 100 sharing tab "home" with 101.
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: Some("home".into()),
            id: 7,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(100)), (0.5, Node::Leaf(101))],
            },
            focus: 100,
        },
    );
    core.squad_members.insert(
        7,
        vec![crate::squad_store::StoredMember {
            attach_id: "c19cd2c3".into(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: Some("home".into()),
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    core.attached.insert("c19cd2c3".into(), 100);
    core.tab_areas.insert(7, (24, 80));
    core.persist_squad(7);

    core.pane_break(100, Some("solo".into()))
        .expect("break the member pane");

    let loaded = crate::squad_store::load();
    let member = loaded
        .squads
        .iter()
        .find(|s| s.name == "harden")
        .expect("workspace persisted")
        .members
        .iter()
        .find(|m| m.attach_id == "c19cd2c3")
        .expect("member kept");
    assert_eq!(
        member.tab_name.as_deref(),
        Some("solo"),
        "the stored tab_name follows the member into the broken-out tab"
    );
}

#[test]
fn join_of_a_member_tab_refreshes_the_stored_tab_name() {
    // (x-d6a8, codex P1) Joining a persisted member's tab into another changes
    // its hosting tab; the stored tab_name must refresh in the shared tab_join
    // helper (same reconcile as pane_break).
    let _s = StoreScratch::new("join-member-tabname");
    let mut core = empty_core();
    // Squad 7: tab "src" holds member pane 100; tab "dst" holds anchor 200.
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: Some("src".into()),
            id: 7,
            root: Node::Leaf(100),
            focus: 100,
        },
    );
    core.session.squad_mut(7).unwrap().tabs.push(Tab {
        name: Some("dst".into()),
        id: 20,
        root: Node::Leaf(200),
        focus: 200,
    });
    core.squad_members.insert(
        7,
        vec![crate::squad_store::StoredMember {
            attach_id: "c19cd2c3".into(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: Some("src".into()),
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    );
    core.attached.insert("c19cd2c3".into(), 100);
    core.tab_areas.insert(7, (24, 80));
    core.tab_areas.insert(20, (24, 80));
    core.persist_squad(7);

    core.tab_join(&TabSel::Id(7), 200, Dir::Right)
        .expect("join the member's tab into dst");

    let loaded = crate::squad_store::load();
    let member = loaded
        .squads
        .iter()
        .find(|s| s.name == "harden")
        .expect("workspace persisted")
        .members
        .iter()
        .find(|m| m.attach_id == "c19cd2c3")
        .expect("member kept");
    assert_eq!(
        member.tab_name.as_deref(),
        Some("dst"),
        "the stored tab_name follows the member into the join destination"
    );
}

#[test]
fn user_close_of_last_member_pane_de_persists_the_workspace() {
    // AC3-EDGE corner: closing the workspace's last pane (squad removed from
    // the session) drops the whole entry, so it never returns at restart.
    let _s = StoreScratch::new("last-pane");
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 100, "c19cd2c3");
    core.persist_squad(7);
    let ctx = core.member_ctx(100);
    // Simulate the squad already gone (its last tab was removed by close).
    core.session.squads.clear();
    core.reconcile_member_close(ctx, false);
    let loaded = crate::squad_store::load();
    assert!(loaded.squads.is_empty(), "de-persisted with its last pane");
}

#[test]
fn rerun_refused_for_a_busy_or_unknown_agent_pane() {
    // AC-ERR: false-ready is the forbidden direction - a working/blocked
    // agent pane refuses, and an unknown (liveness-only) badge fails closed.
    assert!(rerun_allowed(&[agent(7, Some(AgentBadge::Working), false)], "main", 7).is_err());
    assert!(rerun_allowed(&[agent(7, Some(AgentBadge::Blocked), false)], "main", 7).is_err());
    assert!(rerun_allowed(&[agent(7, None, false)], "main", 7).is_err());
}

#[test]
fn rerun_allowed_for_an_idle_or_exited_agent_pane() {
    // A done agent (idle) or an exited one (the pane is a shell again) allows.
    assert_eq!(
        rerun_allowed(&[agent(7, Some(AgentBadge::Done), false)], "main", 7),
        Ok(())
    );
    assert_eq!(
        rerun_allowed(&[agent(7, Some(AgentBadge::Working), true)], "main", 7),
        Ok(())
    );
}

#[test]
fn rerun_guard_is_scoped_to_the_current_session() {
    // Pane ids collide across sessions: a FOREIGN session's idle (Done) agent
    // on the same pane number must NOT clear the guard for THIS session's busy
    // agent - that would be the forbidden write into a working composer.
    let rows = [
        agent_in("other", 5, Some(AgentBadge::Done), false),
        agent_in("main", 5, Some(AgentBadge::Working), false),
    ];
    assert!(
        rerun_allowed(&rows, "main", 5).is_err(),
        "our busy agent must gate regardless of a foreign idle row on pane 5"
    );
    // And a foreign busy row must not spuriously gate our plain-shell pane.
    let foreign_only = [agent_in("other", 5, Some(AgentBadge::Working), false)];
    assert_eq!(rerun_allowed(&foreign_only, "main", 5), Ok(()));
}

#[test]
fn pane_send_refuses_a_dnd_agent_even_when_unguarded() {
    let (mut core, _client_id, p1, _p2, _rx) = seen_test_core();
    let raw = format!(
        r#"{{"agents":[{{"name":"held","cwd":"/w","status":"live",
                "delivery_policy":"bus-only",
                "mux":{{"session":"test","pane_id":{p1}}}}}]}}"#
    );
    let rows = agents_view::derive_rows(&raw, 0).unwrap();
    match core.pane_send(p1, b"must-not-land", false, None, Ok(rows)) {
        ServerMsg::Err { msg, .. } => assert!(msg.contains("DND"), "wording: {msg}"),
        other => panic!("expected DND refusal, got {other:?}"),
    }
}

// -- x-9454 wheel-passthrough rate gate --------------------------------

// AC1-HP / AC2-HP: a 30-tick same-direction flood inside one window
// forwards at most WHEEL_GATE_BUDGET; ticks spaced past the window all pass.
#[test]
fn wheel_gate_bounds_flood_and_passes_notch_rate() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    let allowed = (0..30)
        .filter(|_| wheel_gate(&mut g, 7, MouseKind::WheelDown, t0))
        .count();
    assert_eq!(
        allowed, WHEEL_GATE_BUDGET as usize,
        "a same-window flood forwards exactly the budget, drops the rest"
    );

    // Notch rate: one tick per window, none dropped.
    let mut g2 = HashMap::new();
    let passed = (0..30)
        .filter(|i| {
            wheel_gate(
                &mut g2,
                7,
                MouseKind::WheelDown,
                t0 + WHEEL_GATE_WINDOW * (*i as u32),
            )
        })
        .count();
    assert_eq!(passed, 30, "notch-rate input is forwarded 1:1");
}

// AC1-EDGE: exactly budget ticks pass, the (budget+1)th in the window drops.
#[test]
fn wheel_gate_exact_budget_boundary() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    for i in 0..WHEEL_GATE_BUDGET {
        assert!(
            wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
            "tick {i} within budget forwards"
        );
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
        "the tick past budget drops"
    );
}

// AC1-UI: a reversal mid-flood forwards immediately and resets the budget.
#[test]
fn wheel_gate_reversal_passes_immediately() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    // Exhaust the down budget so drops are occurring.
    for _ in 0..WHEEL_GATE_BUDGET {
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0);
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelDown, t0),
        "same-direction is dropping"
    );
    assert!(
        wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
        "the opposite tick forwards immediately (reversal is fresh intent)"
    );
    // Reversal reset the window: a fresh up budget is available.
    for _ in 1..WHEEL_GATE_BUDGET {
        assert!(wheel_gate(&mut g, 1, MouseKind::WheelUp, t0));
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
        "the reset up budget then exhausts"
    );
}

// AC1-FR: a tick at or after exactly window_start + window re-admits
// (no permanent mute), testing the boundary instant itself.
#[test]
fn wheel_gate_readmits_at_window_boundary() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    for _ in 0..WHEEL_GATE_BUDGET {
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0);
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelDown, t0),
        "budget exhausted inside the window"
    );
    assert!(
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0 + WHEEL_GATE_WINDOW),
        "the exact boundary instant counts as a fresh window and forwards"
    );
}

// AC2-ERR: a now behind window_start (virtualized clock) saturates instead
// of panicking, and treats the tick as inside the window.
#[test]
fn wheel_gate_clock_skew_saturates() {
    let mut g = HashMap::new();
    let t0 = Instant::now() + WHEEL_GATE_WINDOW * 10;
    assert!(wheel_gate(&mut g, 1, MouseKind::WheelDown, t0));
    // A now BEFORE the stored window_start: saturating_duration_since is 0,
    // so the tick is inside the window and consumes budget (never panics).
    let earlier = t0 - WHEEL_GATE_WINDOW * 5;
    for _ in 1..WHEEL_GATE_BUDGET {
        assert!(wheel_gate(&mut g, 1, MouseKind::WheelDown, earlier));
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelDown, earlier),
        "skewed-early ticks stay inside the window and hit the budget"
    );
}

// Per-pane independence: one pane's flood never spends another's budget.
#[test]
fn wheel_gate_per_pane() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    for _ in 0..WHEEL_GATE_BUDGET {
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0);
    }
    assert!(!wheel_gate(&mut g, 1, MouseKind::WheelDown, t0));
    assert!(
        wheel_gate(&mut g, 2, MouseKind::WheelDown, t0),
        "a different pane draws from its own budget"
    );
}

// AC1-ERR: a wheel tick routed to a pane absent from the panes map is a
// no-op through mouse() - the top-of-fn early return fires, so no PTY write
// and no gate-state entry for the dead id.
#[test]
fn mouse_wheel_dead_pane_is_noop() {
    let mut core = empty_core();
    core.mouse(
        1,
        999,
        MouseEvent {
            row: 0,
            col: 0,
            kind: MouseKind::WheelDown,
        },
    );
    assert!(
        core.wheel_gate.is_empty(),
        "no gate state is created for a dead pane"
    );
}

// AC2-EDGE: reaping a pane drops its gate entry (touch_last_emit pattern).
#[test]
fn reap_pane_clears_wheel_gate() {
    let mut core = empty_core();
    core.wheel_gate.insert(
        42,
        WheelGateState {
            window_start: Instant::now(),
            count: 3,
            dir: MouseKind::WheelDown,
        },
    );
    core.reap_pane(42);
    assert!(
        !core.wheel_gate.contains_key(&42),
        "the closed pane's gate entry is removed"
    );
}

#[test]
fn node_id_shape_check() {
    assert!(node_id_shaped("x-aff6"));
    assert!(node_id_shaped("ab-1234abcd"));
    assert!(
        !node_id_shaped("footnote"),
        "a plain squad basename is never a node id"
    );
    assert!(!node_id_shaped("feature-x-aff6"));
    assert!(!node_id_shaped("x-123"), "hex run too short");
    assert!(!node_id_shaped("x-AFF6"), "node ids are lowercase hex");
    assert!(!node_id_shaped("x-aff6789012"), "hex run too long");
    assert!(!node_id_shaped("-aff6"), "empty prefix");
}

// -- Observer attach (x-6a14 web read-only bridge) --------------------------

pub(super) fn empty_core() -> Core {
    let (out_tx, _out_rx) = mpsc::channel::<(u64, PaneChunk)>(8);
    let (exit_tx, _exit_rx) = mpsc::channel::<u64>(8);
    let (self_tx, _self_rx) = mpsc::channel::<CoreMsg>(8);
    Core {
        session: Session::default(),
        panes: HashMap::new(),
        pane_watch: HashMap::new(),
        pane_stats: Arc::new(RwLock::new(HashMap::new())),
        pane_stats_emit_failures: Arc::new(AtomicU64::new(0)),
        pane_children: Arc::new(Mutex::new(HashSet::new())),
        clients: Vec::new(),
        next_pane_id: 1,
        next_squad_id: 1,
        tab_areas: HashMap::new(),
        session_name: "test".into(),
        shells: Vec::new(),
        out_tx,
        exit_tx,
        self_tx,
        agents: Vec::new(),
        agents_read_ok: false,
        journal: crate::spawn_journal::JournalCache::default(),
        launch_desk: Default::default(),
        branch_by_cwd: HashMap::new(),
        tail_by_session: HashMap::new(),
        truth_by_name: HashMap::new(),
        truth_seq: 0,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
        backlog_holders: HashMap::new(),
        backlog_pr: HashMap::new(),
        backlog_driver: HashMap::new(),
        claim_eligible: HashSet::new(),
        claims: HashMap::new(),
        touch_last_emit: HashMap::new(),
        wheel_gate: HashMap::new(),
        touch_emit_failures: Arc::new(AtomicU64::new(0)),
        started_at: crate::server_stats::stamp_now(),
        client_count: watch::channel(0).0,
        seen: HashSet::new(),
        attached: HashMap::new(),
        worker_pane: HashMap::new(),
        worker_session_pane: HashMap::new(),
        held_workers: HashMap::new(),
        detached_panes: HashMap::new(),
        diff_pane: None,
        portals: BTreeMap::new(),
        portal_noticed: false,
        squad_members: HashMap::new(),
        template_specs: HashMap::new(),
        pending_template_restores: Vec::new(),
        external_lifecycle: Vec::new(),
        persist_degraded_notified: false,
        shared_identity_notified: HashSet::new(),
        restored: false,
        restore_pending: false,
        store_generations: HashMap::new(),
        pre_restore_squads: HashSet::new(),
        topology_dirty: false,
        last_topology_flush: None,
        reentry_verdict: None,
        staged_resume_argv: None,
        batch_plans: HashMap::new(),
        pending_thread_reply: None,
        keeper_adopted: Vec::new(),
        shell_rc_dirs: std::collections::HashMap::new(),
        portal_session_guards: std::collections::BTreeMap::new(),
    }
}

fn placement_core() -> Core {
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo/default".into()],
        Some("review".into()),
        Tab {
            name: None,
            id: 11,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.next_pane_id = 2;
    core.next_squad_id = 8;
    core
}

#[test]
fn defensive_reaper_sweeps_a_dead_child_without_an_exit_event() {
    let mut core = empty_core();
    core.shells = vec!["/bin/sh".into()];
    let pane = core.spawn_pane(24, 80, "/tmp").expect("shell pane");
    core.session.add_squad(
        1,
        vec!["/tmp".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(pane),
            focus: pane,
        },
    );
    let child = core.panes[&pane].pty.child_pid().expect("child pid");
    // SAFETY: the pid came from this test's child and SIGKILL is followed
    // by bounded liveness polling before any assertion continues.
    assert_eq!(
        unsafe { libc::kill(child as libc::pid_t, libc::SIGKILL) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !core.panes[&pane].pty.is_reap_ready() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        core.panes[&pane].pty.is_reap_ready(),
        "child or PTY reader never exited"
    );

    let dead = core.dead_children_ready_to_reap();
    assert!(matches!(core.reap_dead_children(dead), Flow::Shutdown));
    assert!(core.panes.is_empty(), "dead pane registry entry survived");
    assert!(core.session.squads.is_empty(), "dead pane tree survived");
}

#[test]
fn pane_placement_resolves_named_and_stale_targets_before_spawn() {
    let core = placement_core();
    assert_eq!(
        core.resolve_placement_target(&PaneTarget::SquadName("review".into()), None),
        Ok(Some(7))
    );
    assert_eq!(
        core.resolve_placement_target(&PaneTarget::SquadId(7), None),
        Ok(Some(7))
    );
    assert!(core
        .resolve_placement_target(&PaneTarget::SquadName("missing".into()), None)
        .is_err());
    assert!(core
        .resolve_placement_target(&PaneTarget::SquadId(99), None)
        .is_err());
}

#[test]
fn pane_placement_splits_on_requested_side_and_focuses_new_pane() {
    let mut core = placement_core();
    core.tab_areas.insert(11, (24, 80));
    let landed = core
        .place_spawned_pane(Some(7), "/repo/child", 2, Some(Dir::Left))
        .unwrap();
    assert_eq!(landed, (7, 11, false));
    let tab = &core.session.squad(7).unwrap().tabs[0];
    assert_eq!(tree::leaves(&tab.root), vec![2, 1]);
    assert_eq!(tab.focus, 2);
}

#[test]
fn pane_placement_split_refusal_falls_back_to_new_tab() {
    // AC3-FR (x-9f75): a split refused at min-size no longer reaps and dead-ends - the pane lands as a
    // NEW TAB in the same squad, the original tab is untouched, and the caller is told to notice.
    let mut core = placement_core();
    core.tab_areas.insert(11, (24, 16));
    core.claim_eligible.insert(2);
    let before = core.session.squad(7).unwrap().tabs[0].clone();
    let (sid, tid, fell_back) = core
        .place_spawned_pane(Some(7), "/repo/child", 2, Some(Dir::Right))
        .unwrap();
    assert!(
        fell_back,
        "the caller must know to emit the tab-full notice"
    );
    assert_eq!(sid, 7);
    let squad = core.session.squad(7).unwrap();
    assert_eq!(squad.tabs.len(), 2, "a new tab was added");
    assert_eq!(squad.tabs[0], before, "the crowded tab is untouched");
    let landed = squad.tabs.iter().find(|t| t.id == tid).unwrap();
    assert_eq!(
        landed.root,
        Node::Leaf(2),
        "pane landed as the new tab's leaf"
    );
    assert!(
        core.claim_eligible.contains(&2),
        "the pane is not reaped, so its claim eligibility survives"
    );
}

#[test]
fn pane_placement_split_without_existing_route_creates_first_tab() {
    // A fresh unnamed lane now persists on creation (every squad remains,
    // TUI or API), so scratch the store off the real home.
    let _s = StoreScratch::new("place-split-new-lane");
    let mut core = empty_core();
    let landed = core
        .place_spawned_pane(None, "/repo/new", 1, Some(Dir::Down))
        .unwrap();
    let squad = core.session.squad(landed.0).unwrap();
    assert_eq!(squad.canonical_cwd(), "/repo/new");
    assert_eq!(squad.tabs.len(), 1);
    assert_eq!(squad.tabs[0].root, Node::Leaf(1));
    assert_eq!(squad.tabs[0].focus, 1);
    // P2: the lane persisted immediately, even with no member, keyed by its
    // durable key (name empty).
    let loaded = crate::squad_store::load();
    let lane = loaded
        .squads
        .iter()
        .find(|s| s.origins == vec!["/repo/new".to_string()])
        .expect("the new unnamed lane persisted on creation");
    assert!(
        lane.name.is_empty() && !lane.key.is_empty(),
        "unnamed, durable key"
    );
}

#[test]
fn pane_placement_target_does_not_replace_child_cwd() {
    let mut core = placement_core();
    let root = std::env::temp_dir().join(format!("fno-placement-cwd-{}", std::process::id()));
    let child_cwd = root.join("child");
    std::fs::create_dir_all(&child_cwd).unwrap();
    let marker = child_cwd.join("cwd.txt");
    let pid = core
        .run_pane(
            "/repo/default".into(),
            child_cwd.to_string_lossy().into_owned(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "pwd > cwd.txt; sleep 30".into(),
            ],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("review".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

    // A loaded CI runner can take several seconds just to spawn the PTY +
    // start the shell; 15s matches the PTY-wait convention elsewhere and
    // keeps this off the flake list. Readiness is NON-EMPTY CONTENT, not
    // existence: `pwd > cwd.txt` creates the file on redirect, BEFORE pwd
    // writes into it, so an exists() gate can hand the read an empty string
    // and the canonicalize below then fails as a confusing NotFound.
    let content = || {
        std::fs::read_to_string(&marker)
            .ok()
            .filter(|s| !s.trim().is_empty())
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while content().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let reported =
        content().expect("pane shell never wrote cwd.txt within 15s (spawn slow or failed)");
    assert_eq!(
        std::fs::canonicalize(reported.trim()).unwrap(),
        std::fs::canonicalize(&child_cwd).unwrap()
    );
    let (sid, _) = core.session.find_pane(pid).unwrap();
    assert_eq!(sid, 7);

    core.reap_pane(pid);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn run_pane_create_if_absent_mints_persisted_named_squad() {
    // AC2-HP (x-9f75): a `pane run --squad <name>` naming no existing squad mints a persisted named squad
    // (origins = the spawn's repo root) and lands the pane as its first tab. A second run with the same
    // name joins it - no duplicate mint.
    let _s = StoreScratch::new("run-create-if-absent");
    let mut core = empty_core();
    let run = |core: &mut Core| {
        core.run_pane(
            "/repo/proj".into(),
            "/repo/proj".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("readyrule".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap()
    };
    let pid = run(&mut core);
    let (sid, _) = core.session.find_pane(pid).unwrap();
    let sq = core.session.squad(sid).unwrap();
    assert_eq!(sq.name.as_deref(), Some("readyrule"));
    assert_eq!(sq.origins, vec!["/repo/proj".to_string()]);
    assert_eq!(tree::leaves(&sq.tabs[0].root), vec![pid]);
    assert!(
        crate::squad_store::load()
            .squads
            .iter()
            .any(|s| s.name == "readyrule"),
        "the named squad is persisted (write-through)"
    );

    let pid2 = run(&mut core);
    let (sid2, _) = core.session.find_pane(pid2).unwrap();
    assert_eq!(sid2, sid, "the second run joins the existing named squad");
    assert_eq!(
        core.session
            .squads
            .iter()
            .filter(|s| s.name.as_deref() == Some("readyrule"))
            .count(),
        1,
        "no duplicate squad minted"
    );

    core.reap_pane(pid);
    core.reap_pane(pid2);
}

#[test]
fn run_pane_create_if_absent_rejects_blank_name_before_spawn() {
    // A blank/whitespace SquadName is still refused (never a minted squad),
    // and no pane is spawned - fail-closed, mirroring resolve_placement.
    let _s = StoreScratch::new("run-create-blank");
    let mut core = empty_core();
    let before = core.panes.len();
    let err = core
        .run_pane(
            "/repo/proj".into(),
            "/repo/proj".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("   ".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert!(err.1.contains("blank"), "{err:?}");
    assert_eq!(err.0, err_code::BAD_REQUEST, "blank name is a bad request");
    assert_eq!(core.panes.len(), before, "no pane spawned on a blank name");
    assert!(core.session.squads.is_empty(), "no squad minted");
}

#[test]
fn attach_new_tab_anchors_pane_in_row_cwd() {
    // US5 contract (x-9f75): attaching a watch-only row spawns the pane in the ROW's own cwd, not the
    // viewer's squad cwd. Asserting the existing behavior so it becomes contract, not accident (the
    // interactive cwd chooser is a deferred follow-up; these defaults are the floor).
    let root = std::env::temp_dir().join(format!("fno-row-cwd-{}", std::process::id()));
    let row_cwd = root.join("agent-home");
    std::fs::create_dir_all(&row_cwd).unwrap();
    let marker = row_cwd.join("cwd.txt");
    // The attach spawn writes its pwd then idles, standing in for the real
    // `claude attach <id>` (the id rides as $0 for `sh -c`, harmless).
    set_attach_program(&["/bin/sh", "-c", "pwd > cwd.txt; sleep 30"]);
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    core.agents = vec![bg_row(
        "home-agent",
        &row_cwd.to_string_lossy(),
        Some("deadbee2"),
    )];

    core.command(client_id, Command::attach_agent("deadbee2"));

    // A loaded CI runner can take several seconds just to spawn the PTY +
    // start the shell; 15s matches the PTY-wait convention elsewhere and
    // keeps this off the flake list. Readiness is NON-EMPTY CONTENT, not
    // existence: `pwd > cwd.txt` creates the file on redirect, BEFORE pwd
    // writes into it, so an exists() gate can hand the read an empty string
    // and the canonicalize below then fails as a confusing NotFound.
    let content = || {
        std::fs::read_to_string(&marker)
            .ok()
            .filter(|s| !s.trim().is_empty())
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while content().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let reported =
        content().expect("pane shell never wrote cwd.txt within 15s (spawn slow or failed)");
    assert_eq!(
        std::fs::canonicalize(reported.trim()).unwrap(),
        std::fs::canonicalize(&row_cwd).unwrap(),
        "the attach pane is anchored in the row's own cwd"
    );
    if let Some(&pid) = core.attached.get("deadbee2") {
        core.reap_pane(pid);
    }
    let _ = std::fs::remove_dir_all(root);
}

fn client(id: u64, view_tab: TabId, dims: (u16, u16), passive: bool) -> Client {
    Client {
        id,
        reliable_tx: mpsc::channel(1).0,
        dirty: Arc::default(),
        notify: Arc::new(Notify::new()),
        synced_modes: Modes::default(),
        view: (1, view_tab),
        visible: HashSet::new(),
        dims,
        passive,
        last_press: None,
    }
}

#[test]
fn observer_attach_excluded_from_clamp() {
    // AC1-EDGE: a passive (web) viewer must never enter the smallest-client
    // reduce, so it cannot shrink the driver's PTY - even with tiny dims.
    let mut core = empty_core();
    core.clients.push(client(1, 5, (24, 80), false)); // driving client
    core.clients.push(client(2, 5, (10, 30), true)); // phone observer
    assert_eq!(
        core.tab_area(5),
        (24, 80),
        "a passive viewer must not lower the driver's clamp"
    );
}

#[test]
fn observer_attach_sole_viewer_keeps_last_or_default() {
    // AC1-EDGE: when the observer is the ONLY viewer, the tab keeps its
    // last-applied size (or VT defaults if never sized), never the phone's.
    let mut core = empty_core();
    core.clients.push(client(1, 5, (10, 30), true));
    assert_eq!(
        core.tab_area(5),
        (vt::DEFAULT_ROWS, vt::DEFAULT_COLS),
        "sole observer -> VT defaults, never its own dims"
    );
    core.tab_areas.insert(5, (40, 120));
    assert_eq!(
        core.tab_area(5),
        (40, 120),
        "sole observer -> last-applied size, never reflows to the phone"
    );
}

#[test]
fn observer_attach_never_spawns_a_pane() {
    // Locked Decision 5: an observer (0,0) attach to a session it does not
    // match must register read-only without ever creating a squad/PTY.
    let mut core = empty_core();
    let (tx, _rx) = mpsc::channel::<ServerMsg>(8);
    core.attach(
        1,
        0,
        0,
        "/nowhere".into(),
        "/nowhere".into(),
        tx,
        Arc::default(),
        Arc::new(Notify::new()),
    );
    assert_eq!(
        core.panes.len(),
        0,
        "observer attach must never spawn a PTY"
    );
    assert_eq!(core.clients.len(), 1);
    assert!(core.clients[0].passive, "the (0,0) client is passive");
}

#[test]
fn bye_flushes_the_final_dirty_frame_first() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (mut writer, mut reader) = tokio::io::duplex(4096);
            let dirty: DirtyMap = Arc::default();
            let frame = Frame {
                rows: 1,
                cols: 1,
                cells: vec![crate::proto::Cell::default()],
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: true,
                scroll_offset: 0,
            };
            dirty.lock().unwrap().insert(7, frame.clone());
            let stats: PaneStats = Arc::default();
            assert!(write_reliable(
                &mut writer,
                &ServerMsg::Bye {
                    reason: "done".into(),
                },
                &dirty,
                &stats,
            )
            .await
            .unwrap());

            assert_eq!(
                read_msg::<_, ServerMsg>(&mut reader).await.unwrap(),
                ServerMsg::Frame { pane_id: 7, frame }
            );
            assert_eq!(
                read_msg::<_, ServerMsg>(&mut reader).await.unwrap(),
                ServerMsg::Bye {
                    reason: "done".into()
                }
            );
            assert!(dirty.lock().unwrap().is_empty());
        });
}

// Pane-counter and frame-emission family (plus the x-a600 repaint
// gesture tests) moved verbatim into its own module: this file is over
// the shrink-only line, and test motion is the sanctioned shrink.
#[path = "server/tests/pane_frame_flow_tests.rs"]
mod pane_frame_flow_tests;

#[test]
fn cold_attach_delivers_every_pane_frame_on_the_reliable_channel() {
    // AC1-FR (x-0296): the cold-attach snapshot must NOT depend on the
    // droppable dirty map or later PTY output. Deterministic mechanism
    // guard: after attach, the client's reliable queue holds the Layout
    // followed by a Frame for EVERY pane in it. Pre-fix this fails 100%
    // (seeds sat only in the dirty map); no timing involved.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let p1 = core.spawn_pane(24, 40, "/tmp").expect("pane 1");
    let p2 = core.spawn_pane(24, 40, "/tmp").expect("pane 2");
    core.session.add_squad(
        1,
        vec!["/tmp/x0296".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(p1)), (0.5, Node::Leaf(p2))],
            },
            focus: p1,
        },
    );
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(32);
    core.attach(
        9,
        24,
        80,
        "/tmp/x0296".into(),
        "/tmp/x0296".into(),
        tx,
        Arc::default(),
        Arc::new(Notify::new()),
    );
    let mut msgs = Vec::new();
    while let Ok(m) = rx.try_recv() {
        msgs.push(m);
    }
    let layout_at = msgs
        .iter()
        .position(|m| matches!(m, ServerMsg::Layout { .. }))
        .expect("attach queues a Layout reliably");
    let layout_panes: HashSet<u64> = match &msgs[layout_at] {
        ServerMsg::Layout { panes, .. } => panes.iter().map(|(pid, _)| *pid).collect(),
        _ => unreachable!(),
    };
    assert_eq!(
        layout_panes,
        HashSet::from([p1, p2]),
        "both panes are in the attach Layout"
    );
    let framed: HashSet<u64> = msgs[layout_at + 1..]
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Frame { pane_id, .. } => Some(*pane_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        framed, layout_panes,
        "every Layout pane's initial frame must ride the reliable \
             channel AFTER the Layout - a dirty-map-only seed is droppable \
             and a passive reattach never recovers it (x-0296)"
    );
}

#[test]
fn focus_only_push_layout_preserves_pending_pane_frames() {
    // AC1-FR (x-0296, the CI root cause): a pane's output-driven frame
    // sits in the client's droppable dirty map until the writer drains
    // it, and it is the ONLY copy - a quiet pane produces no further
    // output to regenerate it (broadcast_pane fires on output only). A
    // focus-only push_layout(reemit=false) landing in that window must
    // not flush it: rects are unchanged, so the frame is still valid.
    // Pre-fix, the unconditional clear() destroyed it 100% here; on the
    // loaded CI runner the shell's "$ " prompt frame died in exactly
    // this window and the pane stayed blank forever.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let p1 = core.spawn_pane(24, 40, "/tmp").expect("pane 1");
    core.session.add_squad(
        1,
        vec!["/tmp/x0296".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(p1),
            focus: p1,
        },
    );
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(32);
    let dirty: DirtyMap = Arc::default();
    core.attach(
        9,
        24,
        80,
        "/tmp/x0296".into(),
        "/tmp/x0296".into(),
        tx,
        dirty.clone(),
        Arc::new(Notify::new()),
    );
    // Drain the attach traffic (not under test), then land the shell's
    // prompt: the output broadcast seeds the dirty map.
    while rx.try_recv().is_ok() {}
    core.panes.get_mut(&p1).unwrap().vt.feed(b"$ ");
    core.broadcast_pane(p1);
    assert!(
        dirty.lock().unwrap().contains_key(&p1),
        "output must seed the dirty map"
    );
    // A focus-only push races in before the writer drains the frame.
    core.push_layout(false);
    assert!(
        dirty.lock().unwrap().contains_key(&p1),
        "a reemit=false push_layout must preserve a pending frame: it is \
             the only copy of a quiet pane's latest output (x-0296)"
    );
}

/// A one-squad, two-tab (one pane each) `Core` with a client attached and
/// viewing it - the shared rig for the x-4328 seen-bit tests below. The
/// returned receiver must stay alive for the test's duration: dropping it
/// closes the client's channel, which `push_layout` reads as "gone" and
/// prunes the client, breaking every later `Command::FocusPane` (no
/// client view to act on).
fn seen_test_core() -> (Core, u64, u64, u64, mpsc::Receiver<ServerMsg>) {
    // attach() runs restore_squads() -> squad_store::load(), which defaults
    // to the real $HOME/.fno/squads.json; a dev box with a live store then
    // imports its squads and breaks squad/row-count asserts that pass on a
    // fresh-home CI runner. Point the store at a nonexistent per-thread path
    // (missing file reads as an empty store; the writer creates its parent
    // only when a real write happens) and restore becomes a no-op. TEST_PATH
    // is thread-local and one test == one thread: no leaks, no teardown.
    let scratch = std::env::temp_dir().join(format!(
        "fno-seen-store-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&scratch); // sweep any stale same-pid dir
    crate::squad_store::set_test_path(&scratch);

    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let p1 = core.spawn_pane(24, 40, "/tmp/seen").expect("pane 1");
    let p2 = core.spawn_pane(24, 40, "/tmp/seen").expect("pane 2");
    core.session.add_squad(
        1,
        vec!["/tmp/seen".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(p1),
            focus: p1,
        },
    );
    core.session.squad_mut(1).unwrap().tabs.push(Tab {
        name: None,
        id: 2,
        root: Node::Leaf(p2),
        focus: p2,
    });
    let (tx, rx) = mpsc::channel::<ServerMsg>(32);
    let client_id = 9;
    core.attach(
        client_id,
        24,
        80,
        "/tmp/seen".into(),
        "/tmp/seen".into(),
        tx,
        Arc::default(),
        Arc::new(Notify::new()),
    );
    (core, client_id, p1, p2, rx)
}

#[test]
fn scroll_delta_classifies_only_interpreted_wheel_ticks() {
    // The fold is only correct if it folds real scrolls and STOPS at anything
    // else. A plain pane's wheel is an interpreted scroll (foldable, +/-
    // MOUSE_WHEEL_LINES); a left press is a selection (not foldable) so it must
    // break the run and preserve order.
    let (core, _cid, p1, _p2, _rx) = seen_test_core();
    let ev = |kind| MouseEvent {
        row: 1,
        col: 1,
        kind,
    };
    assert_eq!(
        core.scroll_delta(p1, &ev(MouseKind::WheelUp)),
        Some(MOUSE_WHEEL_LINES)
    );
    assert_eq!(
        core.scroll_delta(p1, &ev(MouseKind::WheelDown)),
        Some(-MOUSE_WHEEL_LINES)
    );
    assert_eq!(
        core.scroll_delta(p1, &ev(MouseKind::Press(MouseButton::Left))),
        None,
        "a select must stop the fold, not coalesce into it"
    );
    assert_eq!(
        core.scroll_delta(999, &ev(MouseKind::WheelUp)),
        None,
        "an unknown pane never folds"
    );
}

#[test]
fn folded_scroll_ticks_preserve_per_tick_clamp_at_a_boundary() {
    // The fold applies ticks IN ORDER via scroll_tick, never by algebraic net:
    // at the live bottom a WheelDown clamps to 0, so a following WheelUp must
    // still move the view up. Netting (-3 + 3 = 0) would wrongly lose the
    // reversal - the exact boundary bug this guards against.
    let (mut core, _cid, p1, _p2, _rx) = seen_test_core();
    // Push content past the 24-row grid so there is scrollback to reveal.
    core.panes
        .get_mut(&p1)
        .unwrap()
        .vt
        .feed("row\r\n".repeat(60).as_bytes());
    assert_eq!(core.scroll_offset(p1), 0, "starts at the live bottom");

    // WheelDown at the bottom clamps (no-op); the reversal WheelUp reveals
    // history. Ordered application lands above the bottom, not back at it.
    core.scroll_tick(p1, -MOUSE_WHEEL_LINES);
    assert_eq!(core.scroll_offset(p1), 0, "down at the bottom clamps");
    core.scroll_tick(p1, MOUSE_WHEEL_LINES);
    assert_eq!(
        core.scroll_offset(p1),
        MOUSE_WHEEL_LINES as usize,
        "the reversal still scrolls up (netting to 0 would strand it)"
    );

    // A mid-history reversal that truly cancels returns to where it started,
    // so the drain's before==after guard skips the redundant broadcast.
    let mid = core.scroll_offset(p1);
    core.scroll_tick(p1, MOUSE_WHEEL_LINES);
    core.scroll_tick(p1, -MOUSE_WHEEL_LINES);
    assert_eq!(core.scroll_offset(p1), mid, "a real cancel nets to no move");
}

#[test]
fn bounded_scroll_target_caps_a_burst_but_spares_a_single_notch() {
    // A big same-direction fold is capped to one viewport (24) either way...
    assert_eq!(bounded_scroll_target(0, 300, 24), 24, "up burst capped");
    assert_eq!(bounded_scroll_target(300, 0, 24), 276, "down burst capped");
    // ...a move already within a screen (a lone wheel notch) is untouched...
    assert_eq!(
        bounded_scroll_target(0, MOUSE_WHEEL_LINES, 24),
        MOUSE_WHEEL_LINES
    );
    // ...a boundary reversal the in-order fold landed at +3 survives the cap
    // (it never re-introduces the netting bug)...
    assert_eq!(bounded_scroll_target(0, 3, 24), 3);
    // ...and a true no-op fold stays put.
    assert_eq!(bounded_scroll_target(10, 10, 24), 10);
}

/// Graft a second squad hosting one fresh pane onto a `seen_test_core`, so a
/// `pane focus` test has somewhere to travel TO. Returns the new pane id and
/// its tab id.
fn add_second_squad(core: &mut Core) -> (u64, TabId) {
    let p3 = core.spawn_pane(24, 40, "/tmp/other").expect("pane 3");
    core.session.add_squad(
        2,
        vec!["/tmp/other".into()],
        Some("other".into()),
        Tab {
            name: None,
            id: 3,
            root: Node::Leaf(p3),
            focus: p3,
        },
    );
    (p3, 3)
}

#[test]
fn pane_focus_moves_the_viewer_across_squads_and_reports_where() {
    // AC1-HP: the verb is a goto, not a nudge. A client viewing squad 1 ends
    // up on squad 2's tab with the named pane focused, and the receipt names
    // the RESOLVED location rather than echoing the request.
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    let (p3, t3) = add_second_squad(&mut core);
    let view_before = core
        .clients
        .iter()
        .find(|c| c.id == client_id)
        .unwrap()
        .view;
    assert_eq!(view_before.0, 1, "precondition: viewing squad 1");

    let reply = core.pane_focus(p3);
    match reply {
        ServerMsg::PaneFocused {
            pane,
            squad_id,
            squad_name,
            tab_id,
            tab_name,
            tab_ordinal,
            clients_moved,
        } => {
            assert_eq!((pane, squad_id, tab_id), (p3, 2, t3));
            assert_eq!(squad_name.as_deref(), Some("other"));
            assert_eq!((tab_name.as_deref(), tab_ordinal), (None, Some(1)));
            // The count is the anti-lie field: it must reflect a client that
            // actually ended up looking at the pane.
            assert_eq!(clients_moved, 1);
        }
        other => panic!("expected PaneFocused, got {other:?}"),
    }
    let c = core.clients.iter().find(|c| c.id == client_id).unwrap();
    assert_eq!(c.view, (2, t3), "the viewer really moved");
    assert_eq!(
        core.session.squad(2).unwrap().tabs[0].focus,
        p3,
        "and the pane is focused, not merely on screen"
    );
}

#[test]
fn pane_focus_refuses_a_dead_pane_fail_closed() {
    // AC1-EDGE: an id no live pane owns is DEAD_PANE with the same `no such
    // pane` wording `Command::FocusPane` already refuses with, and the
    // viewer does not move a millimetre on the way to the refusal.
    let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
    let before = core
        .clients
        .iter()
        .find(|c| c.id == client_id)
        .unwrap()
        .view;
    match core.pane_focus(999_999) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::DEAD_PANE);
            assert!(msg.contains("no such pane"), "wording: {msg}");
        }
        other => panic!("expected Err, got {other:?}"),
    }
    assert_eq!(
        core.clients
            .iter()
            .find(|c| c.id == client_id)
            .unwrap()
            .view,
        before,
        "a refusal moves nobody"
    );
}

#[test]
fn pane_focus_refuses_when_only_passive_observers_are_attached() {
    // AC1-ERR: no non-passive client is a REFUSAL with its OWN code, never a
    // silent pass and never DEAD_PANE. A passive observer is read-only at the
    // server with no viewport to move, so it must not satisfy the check -
    // otherwise the verb reports success for a pane nobody can see.
    let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
    core.clients.retain(|c| c.id != client_id);
    let (tx, _rx2) = mpsc::channel::<ServerMsg>(32);
    core.attach(
        77,
        0, // rows == cols == 0 is what makes a client passive
        0,
        "/tmp/seen".into(),
        "/tmp/seen".into(),
        tx,
        Arc::default(),
        Arc::new(Notify::new()),
    );
    assert!(core.is_passive(77), "precondition: the observer is passive");
    match core.pane_focus(p1) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(
                code,
                err_code::NO_CLIENT,
                "distinct from DEAD_PANE: 'nobody is watching' is not 'your \
                     pane is gone'"
            );
            assert!(msg.contains("no attached client"), "wording: {msg}");
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn pane_focus_clears_a_done_panes_unseen_bit_through_the_shared_trunk() {
    // The invariant that makes reuse load-bearing rather than tidy: routing
    // through `Command::FocusPane` gets `mark_seen_if_done` for free, and an
    // agent pointing the operator at a finished pane is exactly when the seen
    // bit should clear. A bespoke handler would silently drop it, so this
    // asserts the side effect on the CLI path, not just the TUI one.
    let (mut core, _client_id, p1, _p2, _rx) = seen_test_core();
    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
    assert!(!core.seen.contains(&p1), "precondition: unseen");
    core.pane_focus(p1);
    assert!(core.seen.contains(&p1));
}

#[test]
fn focus_pane_marks_a_done_pane_seen() {
    // AC1-HP: focusing a `Done` pane inserts it into `Core.seen`.
    let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
    core.command(client_id, Command::FocusPane(p1));
    assert!(
        core.seen.contains(&p1),
        "focusing a Done pane marks it seen"
    );
}

#[test]
fn a_re_run_evicts_and_does_not_self_reinsert() {
    // AC1-EDGE: Done(focused) -> seen; the pane re-runs to Working (the
    // level-triggered evict in push_layout drops it); it finishes to
    // Done again WITHOUT a fresh focus action - it must stay unseen
    // until re-focused, not re-arm itself just because focus never left.
    let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
    core.command(client_id, Command::FocusPane(p1));
    assert!(core.seen.contains(&p1), "precondition: seen after focus");

    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Working), false)];
    core.push_layout(true);
    assert!(!core.seen.contains(&p1), "a Working tick evicts it");

    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
    core.push_layout(true);
    assert!(
        !core.seen.contains(&p1),
        "the second Done must stay unseen until re-focused - insert is \
             a one-shot side effect of FocusPane, never a per-pass level \
             check on \"is this still the focused pane\""
    );

    core.command(client_id, Command::FocusPane(p1));
    assert!(core.seen.contains(&p1), "re-focusing re-arms seen");
}

#[test]
fn focusing_while_working_never_seeds_a_later_done() {
    // AC2-EDGE: a focus action that lands while the badge is `Working`
    // must not mark the pane seen once it later finishes unattended.
    let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Working), false)];
    core.command(client_id, Command::FocusPane(p1));
    assert!(
        !core.seen.contains(&p1),
        "a Working-time focus never sets the done-seen bit"
    );

    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
    core.push_layout(true);
    assert!(
        !core.seen.contains(&p1),
        "finishing without a fresh focus action stays unseen"
    );
}

#[test]
fn evict_is_per_pane_not_per_focus() {
    // A non-focused Done pane's seen bit (set earlier) is untouched by a
    // push_layout pass that focuses a DIFFERENT pane; eviction keys on
    // the pane's OWN current badge, never on which pane is focused.
    let (mut core, client_id, p1, p2, _rx) = seen_test_core();
    core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
    core.command(client_id, Command::FocusPane(p1));
    assert!(core.seen.contains(&p1));

    core.agents = vec![
        agent_in("test", p1, Some(AgentBadge::Done), false),
        agent_in("test", p2, Some(AgentBadge::Working), false),
    ];
    core.command(client_id, Command::FocusPane(p2));
    assert!(
        core.seen.contains(&p1),
        "p1 stays seen: it is still Done, just no longer focused"
    );
    assert!(!core.seen.contains(&p2), "p2 never reached Done");
}

#[test]
fn gone_decrements_the_published_client_count() {
    // AC4-EDGE (x-4e30): detach via CoreMsg::Gone must drop the published
    // count - the regression the original 4-site enumeration would have
    // shipped (a count stuck high means the idle gate and the FNO_E2E
    // reaper never fire).
    let (tx, rx) = watch::channel(0usize);
    let mut core = empty_core();
    core.client_count = tx;
    core.clients.push(client(1, 5, (24, 80), false));
    core.publish_client_count();
    assert_eq!(*rx.borrow(), 1);
    core.handle(CoreMsg::Gone(1));
    assert_eq!(
        *rx.borrow(),
        0,
        "Gone must publish the decremented count via the handle-tail choke point"
    );
}

#[test]
fn is_passive_flags_only_observer_clients() {
    let mut core = empty_core();
    core.clients.push(client(1, 5, (24, 80), false));
    core.clients.push(client(2, 5, (0, 0), true));
    assert!(!core.is_passive(1));
    assert!(core.is_passive(2));
    assert!(
        !core.is_passive(999),
        "an unknown id is not passive (its message is processed normally)"
    );
}

#[test]
fn handle_drops_mutating_messages_from_a_passive_client() {
    let mut core = empty_core();
    core.clients.push(client(2, 5, (0, 0), true));
    // Read-only at the server: an observer's PTY/tree-mutating messages are
    // dropped by the guard before their handler body ever runs (x-6a14).
    assert!(matches!(
        core.handle(CoreMsg::Input {
            id: 2,
            bytes: vec![b'x']
        }),
        Flow::Continue
    ));
    assert!(matches!(
        core.handle(CoreMsg::BlockNav {
            id: 2,
            pane: 1,
            op: BlockNavOp::Rerun
        }),
        Flow::Continue
    ));
}

// -- Answer freshness (x-c929) ---------------------------------------------

#[test]
fn bottom_non_empty_lines_scopes_and_joins_like_the_daemon() {
    // The server twin must reproduce the daemon's Region::extract: blank
    // lines filtered, last N joined by '\n', line content untrimmed.
    let grid = "scrollback\n\nDo you want to proceed?\n  ❯ 1. Yes\n  2. No\n";
    assert_eq!(
        bottom_non_empty_lines(grid, 8),
        "scrollback\nDo you want to proceed?\n  ❯ 1. Yes\n  2. No"
    );
    // N smaller than the non-blank count scopes to the tail.
    assert_eq!(bottom_non_empty_lines(grid, 2), "  ❯ 1. Yes\n  2. No");
}

// The freshness contract: a fingerprint over the daemon-side region verifies
// against the server's re-hash of the same grid (else every answer would
// fail closed as stale - a false negative that breaks the feature), and a
// grid that advanced hashes differently (the true-positive stale that keeps
// an answer off a moved-on pane).
#[test]
fn answer_fingerprint_matches_unchanged_grid_and_rejects_advanced() {
    let grid = "scrollback\n\nDo you want to proceed?\n  ❯ 1. Yes\n  2. No\n";
    let daemon_fp = *blake3::hash(bottom_non_empty_lines(grid, 8).as_bytes()).as_bytes();
    // Server re-reads the identical grid -> same fingerprint (answer lands).
    let server_fp = *blake3::hash(bottom_non_empty_lines(grid, 8).as_bytes()).as_bytes();
    assert_eq!(daemon_fp, server_fp, "unchanged grid must verify");
    // The pane advanced (a new line appended) -> different fingerprint.
    let advanced = format!("{grid}Running the tool now...\n");
    let advanced_fp = *blake3::hash(bottom_non_empty_lines(&advanced, 8).as_bytes()).as_bytes();
    assert_ne!(daemon_fp, advanced_fp, "advanced grid must read stale");
}

fn block(complete: bool, truncated: bool, implicit: bool, text: &str) -> vt::BlockRead {
    vt::BlockRead {
        seq: Some(0),
        exit: Some(0),
        complete,
        truncated,
        implicit,
        text: text.to_string(),
    }
}

#[test]
fn copy_source_precedence_selection_then_block_then_none() {
    // AC-happy: an active selection wins even when a completed block exists.
    assert_eq!(
        copy_source(Some("sel".into()), || Ok(block(true, false, false, "blk"))),
        Some("sel".into())
    );
    // AC-happy: no selection -> the newest completed block copies.
    assert_eq!(
        copy_source(None, || Ok(block(true, false, false, "blk"))),
        Some("blk".into())
    );
    // AC-error: no selection and no block (BLOCK_UNAVAILABLE) -> notice.
    assert_eq!(copy_source(None, || Err(())), None);
}

#[test]
fn copy_source_refuses_open_truncated_and_implicit_blocks() {
    // The open (still-running) block never copies.
    assert_eq!(
        copy_source(None, || Ok(block(false, false, false, "partial"))),
        None
    );
    // AC-edge: a truncated (head-evicted) block refuses rather than copy wrong text.
    assert_eq!(
        copy_source(None, || Ok(block(true, true, false, "trunc"))),
        None
    );
    // A markerless pane's implicit whole-output block keeps the old notice.
    assert_eq!(
        copy_source(None, || Ok(block(true, false, true, "whole"))),
        None
    );
}

// ---- the keeper contract (re-adopt, sweep, list) --------------------
// The re-adoption spawn helpers live in server/tests/keeper_adopt_tests.rs,
// their only consumers (shrink-only file budget).

#[test]
fn emergency_roster_kills_plain_child_and_spares_keeper_child() {
    let plain =
        ChildGuard::spawn(std::process::Command::new("/bin/sh").args(["-c", "exec sleep 30"]));
    let keeper =
        ChildGuard::spawn(std::process::Command::new("/bin/sh").args(["-c", "exec sleep 30"]));
    let plain_pid = plain.id();
    let keeper_pid = keeper.id();
    let roster = HashSet::from([
        PaneChild {
            pid: plain_pid,
            keeper_hosted: false,
        },
        PaneChild {
            pid: keeper_pid,
            keeper_hosted: true,
        },
    ]);

    kill_plain_children(&roster);
    assert!(
        unsafe { libc::kill(plain_pid as libc::pid_t, 0) } != 0,
        "plain roster child must be killed"
    );
    assert_eq!(
        unsafe { libc::kill(keeper_pid as libc::pid_t, 0) },
        0,
        "keeper roster child must survive"
    );

    // The guards kill and reap at scope end, after the assertions, so
    // what they read is unchanged and the test leaves no zombie.
}

#[path = "server_tests_keeper.rs"]
mod server_tests_keeper;

#[test]
fn keeper_list_reports_zero_as_zero_and_exits_zero() {
    // The empty read is a valid answer, never a failure: the done-probe
    // greps this verb's --json on machines that may hold no keepers.
    let rc = crate::mux_cli::pane_keeper_list(true, Some(std::time::Duration::from_secs(0)));
    assert_eq!(
        rc,
        crate::mux_cli::EXIT_OK,
        "an empty keeper list is exit 0"
    );
}
