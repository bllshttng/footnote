//! The x-eb79 staging test family: a non-claude gesture spawns the argv the
//! off-loop `fno-agents resume-argv` resolution staged, and a failed
//! resolution fails OPEN with the degradation named. Shrinks server.rs
//! under the file-budget gate; same treatment as the other families here.

use super::*;

#[test]
fn resume_agent_runs_the_staged_resume_argv() {
    // AC3-HP + AC3-LOOP (the codex positive): the gesture spawns the
    // argv the off-loop resolution staged - grant and --cd included -
    // never the bare declared form.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-eb79-staged");
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
        name: "t-eb79-staged".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        exited: true,
        liveness: agents_view::Liveness::Dead,
        ..Default::default()
    }];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.staged_resume_argv = Some(vec![
        // argv[0] is an absolute benign binary (the house /bin/cat shape):
        // the spawn gate refuses a program not on PATH, and a CI runner has
        // no codex. The grant and --cd tokens ride positions 1.. verbatim;
        // the grant SPLICE itself is pinned by the fno-agents parity tests.
        "/bin/cat".into(),
        "-c".into(),
        "sandbox_workspace_write.writable_roots=[\"/wt/.fno/plans\"]".into(),
        "--cd".into(),
        "/wt".into(),
        "resume".into(),
        "01a027ad-fe00-7c12-a116-9ee37c6bdfec".into(),
    ]);
    core.command(
        1,
        Command::ResumeAgent {
            name: "t-eb79-staged".into(),
        },
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(notices.contains("resumed t-eb79-staged"), "{notices}");
    let new_panes: Vec<u64> = core
        .panes
        .keys()
        .filter(|&&p| p != shell)
        .copied()
        .collect();
    assert_eq!(new_panes.len(), 1, "exactly one resumed pane");
    let entry = core.panes.get(&new_panes[0]).unwrap();
    assert_eq!(
        entry.cmd.as_deref(),
        Some("cat"),
        "the staged argv's program ran - the staging reached the spawn, not the declared form"
    );
    for pid in new_panes {
        core.reap_pane(pid);
    }
    core.reap_pane(shell);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn resume_agent_fails_open_when_resume_argv_unavailable() {
    // AC3-FALLBACK: the verb failing is NOT silent. The gesture still
    // resumes on the declared-form render (the fail-open fallback) and a
    // pane notice names the degradation. The ready-handler is driven
    // directly: the fire path itself needs a live runtime.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-eb79-fallback");
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
        name: "t-eb79-fallback".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        exited: true,
        liveness: agents_view::Liveness::Dead,
        ..Default::default()
    }];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.handle(CoreMsg::ResumeArgvReady {
        id: 1,
        argv: Ok((
            // Absolute argv[0]: the spawn gate refuses a program not on
            // PATH, and a CI runner has no codex.
            vec![
                "/bin/cat".to_string(),
                "resume".to_string(),
                "01a027ad-fe00-7c12-a116-9ee37c6bdfec".into(),
            ],
            true,
        )),
        replay: Box::new(ResumeReplay::Gesture {
            name: "t-eb79-fallback".into(),
        }),
    });
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("without the writable-roots grant"),
        "the degradation is named, never silent: {notices}"
    );
    assert!(notices.contains("resumed t-eb79-fallback"), "{notices}");
    let new_panes: Vec<u64> = core
        .panes
        .keys()
        .filter(|&&p| p != shell)
        .copied()
        .collect();
    assert_eq!(new_panes.len(), 1, "the fallback still resumes");
    for pid in new_panes {
        core.reap_pane(pid);
    }
    core.reap_pane(shell);
    let _ = std::fs::remove_dir_all(&cwd);
}
