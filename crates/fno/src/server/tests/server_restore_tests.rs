//! (x-b64e) The restore test family: moved verbatim out of server.rs
//! (file budget shrink). Parent helpers resolve through the glob.
use super::*;

#[test]
fn restore_policy_resume_runs_the_bulk_driver_and_idle_spawns_nothing() {
    // (x-7b5e) The widened knob, both new states. `resume` walks the same
    // idle path as hold and THEN runs the bulk driver, so the stored
    // workers come back through their own harness at startup. `idle`
    // spawns nothing and claims no harness process - the explicit
    // opt-out. The default stays byte-identical (the pinning test above).
    let _guard = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    let names = ["t-codex-one", "t-codex-two"];
    let rows_for = || -> Vec<RegistryAgent> {
        [
            ("t-codex-one", "codex-session-one"),
            ("t-codex-two", "codex-session-two"),
        ]
        .iter()
        .map(|(n, sid)| {
            let mut row = exited_claude_row(n, None);
            row.harness = Some("codex".into());
            row.harness_session_id = Some((*sid).into());
            row
        })
        .collect()
    };
    let seed = |scratch: &str| {
        let s = StoreScratch::new(scratch);
        let origin = s.dir.join("repo");
        std::fs::create_dir_all(&origin).unwrap();
        crate::squad_store::upsert(
            "",
            &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
            &[origin.to_string_lossy().into_owned()],
            &[
                crate::squad_store::StoredMember {
                    attach_id: String::new(),
                    tombstone: false,
                    detached: false,
                    tab_name: None,
                    cwd: None,
                    worker: Some("t-codex-one".into()),
                    harness: Some("codex".into()),
                    harness_session_id: Some("codex-session-one".into()),
                },
                crate::squad_store::StoredMember {
                    attach_id: String::new(),
                    tombstone: false,
                    detached: false,
                    tab_name: None,
                    cwd: None,
                    worker: Some("t-codex-two".into()),
                    harness: Some("codex".into()),
                    harness_session_id: Some("codex-session-two".into()),
                },
            ],
        )
        .unwrap();
        s
    };

    // policy = resume: the driver runs at the end of restore and each
    // member is resumed through the (overridden) harness form.
    let _s1 = seed("restore-resume");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    core.agents = rows_for();
    let _known = KnownWorkersGuard;
    set_known_workers(&names);
    // The verb's own registry read is pinned to the same fake rows, so it
    // cannot clobber them with the real machine registry.
    let _rows = RestoreRegistryRowsGuard;
    set_restore_registry_rows(rows_for());
    {
        let _policy = RestorePolicyGuard;
        set_restore_policy(crate::digest_overlay::MuxRestorePolicy::Resume);
        core.restore_squads(24, 80, 999);
    }
    assert_eq!(
        core.worker_pane.len(),
        2,
        "the bulk driver resumed both stored workers: {:?}",
        core.worker_pane
    );
    let resumed: Vec<u64> = core.worker_pane.values().flatten().copied().collect();
    assert_eq!(resumed.len(), 2, "one pane per resumed member");
    for pid in resumed {
        core.reap_pane(pid);
    }

    // policy = idle: the same members restore as idle rows and nothing
    // claims a harness process.
    let _s2 = seed("restore-policy-idle");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    core.agents = rows_for();
    let _known2 = KnownWorkersGuard;
    set_known_workers(&names);
    {
        let _policy = RestorePolicyGuard;
        set_restore_policy(crate::digest_overlay::MuxRestorePolicy::Idle);
        core.restore_squads(24, 80, 999);
    }
    assert!(
        core.worker_pane.is_empty(),
        "idle policy claims no harness process"
    );
    let members: Vec<String> = core
        .squad_members
        .values()
        .flat_map(|ms| ms.iter().filter_map(|m| m.worker.clone()))
        .collect();
    assert_eq!(
        members,
        vec!["t-codex-one".to_string(), "t-codex-two".to_string()],
        "both worker members stay as idle rows"
    );
}

#[test]
fn restore_builds_named_held_panes_without_resuming_workers() {
    // x-5f7f task 4: worker members are ALWAYS dead after a restart (their
    // pty was a child of the previous server). Restore must not spawn
    // them, must keep them as members so the rows stay idle, and must
    // name the count (the positive-marker rule: an operator who sees no
    // resumed worker can tell zero-recorded from never-ran).
    let s = StoreScratch::new("restore-idle");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
        &[origin.to_string_lossy().into_owned()],
        &[
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-codex-one".into()),
                harness: Some("codex".into()),
                harness_session_id: Some("codex-session-one".into()),
            },
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-codex-two".into()),
                harness: Some("codex".into()),
                harness_session_id: Some("codex-session-two".into()),
            },
        ],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut one = exited_claude_row("t-codex-one", None);
    one.harness = Some("codex".into());
    one.harness_session_id = Some("codex-session-one".into());
    let mut two = exited_claude_row("t-codex-two", None);
    two.harness = Some("codex".into());
    two.harness_session_id = Some("codex-session-two".into());
    core.agents = vec![one, two];
    // Pin the registry name set: restore reads the real registry, which a
    // unit test cannot reach deterministically.
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-codex-one", "t-codex-two"]);
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    // One held shell exists per worker, but no codex process was resumed.
    assert_eq!(
        core.panes.len(),
        2,
        "every stored worker position becomes a held pane"
    );
    assert!(
        core.worker_pane.is_empty(),
        "holding a position must not claim the harness process exists"
    );
    assert_eq!(
        core.held_workers.len(),
        2,
        "both panes wait for first focus"
    );
    assert!(
        core.held_workers.keys().all(|pane| {
            core.panes[pane].vt.text().contains("held across restart")
                && !core.panes[pane].vt.is_pristine_idle_shell()
        }),
        "held panes carry visible state and cannot be pruned as pristine shells"
    );
    let members: Vec<String> = core
        .squad_members
        .values()
        .flat_map(|ms| ms.iter().filter_map(|m| m.worker.clone()))
        .collect();
    assert_eq!(
        members,
        vec!["t-codex-one".to_string(), "t-codex-two".to_string()],
        "both worker members stay as idle rows"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("held 2 worker pane(s)"),
        "the count is named, not silence: {notices}"
    );

    let held_pid = core
        .held_workers
        .iter()
        .find_map(|(pane, worker)| (worker.name == "t-codex-one").then_some(*pane))
        .unwrap();
    assert_eq!(
        core.resolve_local_pane("t-codex-one"),
        Some(held_pid),
        "template restore resolves the held slot instead of making a shell"
    );
    let (sid, ti) = core.session.find_pane(held_pid).unwrap();
    let (trees, _) = core.stored_tab_trees(sid).unwrap();
    assert!(
        trees.iter().flat_map(|tree| &tree.slots).any(|slot| {
            matches!(&slot.binding, LayoutBinding::Fno(id) if id == "worker:codex:codex-session-one")
        }),
        "topology capture keeps the exact held worker binding, not a name-only join or shell"
    );
    let tid = core.session.squad(sid).unwrap().tabs[ti].id;
    core.clients[0].view = (sid, tid);
    set_resume_program(&["/bin/cat"]);
    let _resume_guard = ResumeProgramGuard;
    core.command(1, Command::FocusPane(held_pid));
    let resumed_pid = core.worker_pane["t-codex-one"][0];
    assert_ne!(resumed_pid, held_pid, "focus swaps in the harness process");
    assert!(
        !core.panes.contains_key(&held_pid),
        "the held shell is reaped"
    );
    assert_eq!(
        core.panes.len(),
        2,
        "the fixed position is replaced, not split"
    );
    assert_eq!(core.held_workers.len(), 1, "the marker is one-shot");
    core.agents[0].name = "rewritten-registry-name".into();
    assert_eq!(
        core.agent_rows()
            .into_iter()
            .find(|row| row.name == "rewritten-registry-name")
            .and_then(|row| row.pane_id),
        Some(resumed_pid),
        "full session id keeps the resumed pane joined after a name rewrite"
    );
    let next = core.next_pane_id;
    core.command(1, Command::FocusPane(resumed_pid));
    assert_eq!(core.next_pane_id, next, "a second focus spawns nothing");
}

#[test]
fn restore_skips_done_members_and_prunes_their_tree_leaves() {
    // x-9052 AC2-HP / AC3-HP: a worker whose node is done-and-merged is
    // shipped work. It earns no held pane, no refused pane, and no shell
    // substitute; the receipt names it once.
    let s = StoreScratch::new("restore-done");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let origin_str = origin.to_string_lossy().into_owned();
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin_str.clone()]),
        &[origin_str.clone()],
        &[
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-done-one".into()),
                harness: Some("codex".into()),
                harness_session_id: Some("done-session".into()),
            },
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-live-one".into()),
                harness: Some("codex".into()),
                harness_session_id: Some("live-session".into()),
            },
        ],
    )
    .unwrap();
    // Two one-slot tabs: one for the done member, one for the live one.
    let slot_for = |session: &str| {
        crate::proto::LayoutSlot::new(
            "s0".into(),
            LayoutBinding::Fno(format!("worker:codex:{session}")),
        )
    };
    let tree_for = |session: &str| crate::proto::LayoutTreeSpec::Slot("s0".into());
    crate::squad_store::set_tab_trees(
        "",
        &crate::squad_store::origin_key(&[origin_str.clone()]),
        &[],
        &[
            crate::squad_store::StoredTabTree {
                tab_name: None,
                tree: tree_for("done-session"),
                slots: vec![slot_for("done-session")],
                focus: None,
            },
            crate::squad_store::StoredTabTree {
                tab_name: None,
                tree: tree_for("live-session"),
                slots: vec![slot_for("live-session")],
                focus: None,
            },
        ],
        None,
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut one = exited_claude_row("t-done-one", None);
    one.harness = Some("codex".into());
    one.harness_session_id = Some("done-session".into());
    core.agents = vec![one];
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-done-one", "t-live-one"]);
    let _done = DoneSessionsGuard;
    set_done_sessions(
        [("codex".to_string(), "done-session".to_string())]
            .into_iter()
            .collect(),
    );
    set_restore_policy(crate::digest_overlay::MuxRestorePolicy::Hold);
    let _pol = RestorePolicyGuard;
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    assert_eq!(
        core.panes.len(),
        1,
        "one pane: the live member's held pane; the done member earns none"
    );
    assert_eq!(core.held_workers.len(), 1, "only the live member is held");
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("skipped 1 done worker pane(s)"),
        "the skip is named once: {notices}"
    );
    assert!(
        notices.contains("t-done-one"),
        "the done member is named: {notices}"
    );
    assert!(
        notices.contains("1 done tab(s)"),
        "the skipped tab is counted: {notices}"
    );
    // The live member's tree came back with its held pane bound.
    let held_pid = core.panes.keys().copied().next().unwrap();
    let (sid, _ti) = core.session.find_pane(held_pid).unwrap();
    let members: Vec<String> = core
        .squad_members
        .values()
        .flat_map(|ms| ms.iter().filter_map(|m| m.worker.clone()))
        .collect();
    assert_eq!(
        members,
        vec!["t-done-one".to_string(), "t-live-one".to_string()],
        "both members stay as rows (history, not garbage)"
    );
}

#[test]
fn restore_retires_members_the_registry_forgot_but_keeps_exited_rows() {
    // x-2990: a member whose attach-id NO row names is dead weight; one
    // whose EXITED row still names it is the resumable dim card and stays.
    let s = StoreScratch::new("restore-retire");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let origin_str = origin.to_string_lossy().into_owned();
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin_str.clone()]),
        &[origin_str.clone()],
        &[
            crate::squad_store::StoredMember {
                attach_id: "deadbeef".into(),
                tombstone: true,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: None,
                harness: None,
                harness_session_id: None,
            },
            crate::squad_store::StoredMember {
                attach_id: "c0ffee00".into(),
                tombstone: true,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: None,
                harness: None,
                harness_session_id: None,
            },
        ],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut exited = exited_claude_row("t-exited-agent", None);
    exited.attach_id = Some("c0ffee00".into());
    exited.exited = true;
    core.agents = vec![exited.clone()];
    let _reg = RestoreRegistryRowsGuard;
    set_restore_registry_rows(vec![exited]);
    let _known = KnownWorkersGuard;
    set_known_workers(&[]);
    set_restore_policy(crate::digest_overlay::MuxRestorePolicy::Hold);
    let _pol = RestorePolicyGuard;
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("retired 1 member(s) the registry no longer names"),
        "the retirement is named: {notices}"
    );
    let members: Vec<String> = core
        .squad_members
        .values()
        .flat_map(|ms| ms.iter().map(|m| m.attach_id.clone()))
        .collect();
    assert!(
        !members.contains(&"deadbeef".to_string()),
        "the forgotten member is gone: {members:?}"
    );
    assert!(
        members.contains(&"c0ffee00".to_string()),
        "the exited-row member stays: {members:?}"
    );
}

#[test]
fn restore_refusal_names_the_never_bound_marker() {
    let never_bound = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("residue".into()),
        harness: None,
        harness_session_id: None,
    };
    let markers = HashMap::from([(
        String::from("residue"),
        String::from("missing harness session identity"),
    )]);
    assert_eq!(
        restore_worker_refusal_reason(&never_bound, None, None, &HashMap::new(), &markers),
        "never bound: missing harness session identity",
        "the placeholder pane says WHY the member can never bind"
    );
    // AC7-EDGE: a member carrying a session id keeps the session-keyed
    // path; the name marker is last-resort identity only.
    let mut bound = never_bound.clone();
    bound.harness = Some("codex".into());
    bound.harness_session_id = Some("s".into());
    let receipts = HashMap::from([(
        (String::from("codex"), String::from("s")),
        HeldWorker {
            name: "residue".into(),
            harness: "codex".into(),
            harness_session_id: "s".into(),
            cwd: String::new(),
        },
    )]);
    assert_eq!(
        restore_worker_refusal_reason(&bound, None, None, &receipts, &markers),
        "codex session s is not resumable"
    );
}

#[test]
fn restore_legacy_member_uses_unique_receipt_harness() {
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("worker".into()),
        harness: None,
        harness_session_id: Some("full-session".into()),
    };
    let receipts = HashMap::from([(
        (String::from("codex"), String::from("full-session")),
        HeldWorker {
            name: "worker".into(),
            harness: "codex".into(),
            harness_session_id: "full-session".into(),
            cwd: "/repo".into(),
        },
    )]);
    let receipt = receipt_for_member(&receipts, &member).expect("unique receipt");
    assert_eq!(receipt.harness, "codex");
    assert_eq!(
        restore_worker_refusal_reason(&member, None, None, &receipts, &HashMap::new()),
        "codex session full-session is not resumable"
    );
}

#[test]
fn restore_prunes_worker_members_whose_registry_row_is_gone() {
    // x-5f7f, x-b64e: a worker member whose name no longer exists in the
    // registry and which no spawn receipt vouches for can never resume,
    // so restore retires it and says so - otherwise every restart holds
    // a corpse (a reaped worker, an `fno agents rm`). A name that still
    // exists stays, exited or not.
    let s = StoreScratch::new("restore-prune");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
        &[origin.to_string_lossy().into_owned()],
        &[
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-codex-live".into()),
                harness: None,
                harness_session_id: None,
            },
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-codex-reaped".into()),
                harness: None,
                harness_session_id: None,
            },
        ],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let _known = KnownWorkersGuard;
    let _hold = HoldWorkersGuard;
    set_hold_workers(false);
    set_known_workers(&["t-codex-live"]);
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    assert!(
        core.held_workers.is_empty(),
        "hold_workers=false keeps the legacy idle-row-only behavior"
    );
    let members: Vec<String> = core
        .squad_members
        .values()
        .flat_map(|ms| ms.iter().filter_map(|m| m.worker.clone()))
        .collect();
    assert_eq!(
        members,
        vec!["t-codex-live".to_string()],
        "the known name stays, the reaped one is pruned"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("retired 1 worker member(s) whose registry row is gone"),
        "the retirement is named, never silent: {notices}"
    );
    assert!(
        notices.contains("1 worker row(s) idle"),
        "the survivor still counts as idle: {notices}"
    );
    // The retirement is persisted, not just in-memory: the next load sees one.
    let stored = crate::squad_store::load();
    let all: Vec<&str> = stored
        .squads
        .iter()
        .flat_map(|sq| sq.members.iter().filter_map(|m| m.worker.as_deref()))
        .collect();
    assert_eq!(all, vec!["t-codex-live"], "the prune reaches the store");
}

#[test]
fn restore_retires_a_gone_worker_before_the_hold_branch_and_skips_its_tab() {
    // (x-b64e) The default hold policy used to hold every corpse: the
    // member_resume_facts fallback made a registry-forgotten, receipt-less
    // member resumable forever. The classifier runs before the policy
    // branch, so a Gone member earns no pane, leaves the store, and its
    // tab is skipped whole instead of shell-substituted.
    let s = StoreScratch::new("restore-gone-hold");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let origin_str = origin.to_string_lossy().into_owned();
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin_str.clone()]),
        &[origin_str.clone()],
        &[crate::squad_store::StoredMember {
            attach_id: String::new(),
            tombstone: false,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("t-corpse".into()),
            harness: Some("codex".into()),
            harness_session_id: Some("corpse-session".into()),
        }],
    )
    .unwrap();
    crate::squad_store::set_tab_trees(
        "",
        &crate::squad_store::origin_key(&[origin_str.clone()]),
        &[],
        &[crate::squad_store::StoredTabTree {
            tab_name: None,
            tree: crate::proto::LayoutTreeSpec::Slot("s0".into()),
            slots: vec![crate::proto::LayoutSlot::new(
                "s0".into(),
                LayoutBinding::Fno("worker:codex:corpse-session".into()),
            )],
            focus: None,
        }],
        None,
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let _known = KnownWorkersGuard;
    set_known_workers(&[]);
    set_restore_policy(crate::digest_overlay::MuxRestorePolicy::Hold);
    let _pol = RestorePolicyGuard;
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    assert!(
        core.held_workers.is_empty(),
        "a corpse is held nowhere: {:?}",
        core.held_workers
    );
    let stored = crate::squad_store::load();
    let workers: Vec<&str> = stored
        .squads
        .iter()
        .flat_map(|sq| sq.members.iter().filter_map(|m| m.worker.as_deref()))
        .collect();
    assert!(
        workers.is_empty(),
        "the Gone member left the store: {workers:?}"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("retired 1 worker member(s) whose registry row is gone"),
        "the retirement is named, never silent: {notices}"
    );
    assert!(
        notices.contains("1 done tab(s)"),
        "the tab is skipped whole, never shell-substituted: {notices}"
    );
}

#[test]
fn restore_skips_the_prune_entirely_when_the_registry_is_unreadable() {
    // The fail-safe half of the prune: an unreadable registry must delete
    // NOTHING. Mapping a failed read to an empty set would prune every
    // worker member and persist the deletion - ghosts are cheap, deletion
    // on a transient IO error is not. Every member stays, idle-counted,
    // and the skip is named in a notice.
    let s = StoreScratch::new("restore-prune-skip");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
        &[origin.to_string_lossy().into_owned()],
        &[
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-codex-one".into()),
                harness: None,
                harness_session_id: None,
            },
            crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: Some("t-codex-two".into()),
                harness: None,
                harness_session_id: None,
            },
        ],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let _known = KnownWorkersGuard;
    let _hold = HoldWorkersGuard;
    set_hold_workers(false);
    set_known_workers_unreadable();
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let members: Vec<String> = core
        .squad_members
        .values()
        .flat_map(|ms| ms.iter().filter_map(|m| m.worker.clone()))
        .collect();
    assert_eq!(
        members.len(),
        2,
        "an unreadable registry keeps every member: {members:?}"
    );
    let stored = crate::squad_store::load();
    let all: Vec<&str> = stored
        .squads
        .iter()
        .flat_map(|sq| sq.members.iter().filter_map(|m| m.worker.as_deref()))
        .collect();
    assert_eq!(all.len(), 2, "nothing was deleted from the store");
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("prune skipped"),
        "the skip is named, never silent: {notices}"
    );
}

#[test]
fn restore_member_cwd_prefers_the_stored_cwd_when_it_still_exists() {
    // x-caef case 2: a worktree worker restores into its own worktree, not
    // the squad's origins[0].
    let (cwd, notice) = restore_member_cwd(Some("/worktrees/x-caef"), "/repo", |p| {
        p == "/worktrees/x-caef"
    });
    assert_eq!(cwd, "/worktrees/x-caef");
    assert!(notice.is_none(), "no fallback happened, no notice");
}

#[test]
fn restore_member_cwd_falls_back_and_names_the_gone_path_on_a_vanished_worktree() {
    // x-caef case 3: an archived worktree is not silently swallowed - the
    // pane still lands (at origins[0]) and the caller gets both paths to
    // notice, not just a bare fallback.
    let (cwd, notice) = restore_member_cwd(Some("/worktrees/archived"), "/repo", |_| false);
    assert_eq!(cwd, "/repo", "falls back to cwd0");
    assert_eq!(notice.as_deref(), Some("/worktrees/archived"));
}

#[test]
fn restore_member_cwd_falls_back_silently_for_a_pre_xcaef_member() {
    // A member persisted before this field existed has no stored cwd at
    // all - that is not a vanished path, so no notice.
    let (cwd, notice) = restore_member_cwd(None, "/repo", |_| true);
    assert_eq!(cwd, "/repo");
    assert!(notice.is_none());
}

#[test]
fn restore_zero_live_squad_gets_a_shell_and_tombstones_dead_members() {
    // AC1-EDGE: a persisted workspace whose members are all dead
    // materializes with one shell pane, each dead member a tombstone; the
    // reconciled tombstone is written back to the store.
    let _s = StoreScratch::new("restore-dead");
    crate::squad_store::upsert(
        "dead-ws",
        "",
        &["/tmp".into()],
        &[stored_member("deadbeef", false)],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = shell_candidates(std::env::var_os("SHELL").as_deref());
    // No live set (no registry/roster under the scratch home).
    core.restore_squads(24, 80, 999);
    assert_eq!(core.session.squads.len(), 1);
    let sq = &core.session.squads[0];
    assert_eq!(sq.name.as_deref(), Some("dead-ws"));
    assert_eq!(sq.tabs.len(), 1, "zero live members -> one shell tab");
    let sid = sq.id;
    assert!(
        core.squad_members[&sid][0].tombstone,
        "the dead member is tombstoned at restore"
    );
    let loaded = crate::squad_store::load();
    assert!(
        loaded.squads[0].members[0].tombstone,
        "the tombstone is persisted"
    );
    // Reap the spawned shell so the test leaks no process.
    let pids: Vec<u64> = core.panes.keys().copied().collect();
    for pid in pids {
        core.reap_pane(pid);
    }
}

#[test]
fn restore_is_a_noop_on_an_empty_store() {
    let _s = StoreScratch::new("restore-empty");
    let mut core = empty_core();
    core.restore_squads(24, 80, 999);
    assert!(
        core.session.squads.is_empty(),
        "nothing persisted -> nothing restored"
    );
}

#[test]
fn restore_self_heal_sweeps_an_unnamed_dead_origin_orphan() {
    // x-a572 US4: at restore, an unnamed squad whose every origin is gone
    // and which hosts no restorable member is removed (the store converges
    // without a manual prune) and skipped. A named squad in the same store
    // is never touched (Locked Decision 3).
    let _s = StoreScratch::new("restore-selfheal");
    crate::squad_store::upsert(
        "",
        "orphan",
        &["/no/such/selfheal".into()],
        &[stored_member("deadbeef", false)],
    )
    .unwrap();
    crate::squad_store::upsert(
        "real",
        "",
        &["/no/such/selfheal".into()],
        &[stored_member("deadbeef", false)],
    )
    .unwrap();

    let mut core = empty_core();
    core.shells = shell_candidates(std::env::var_os("SHELL").as_deref());
    core.restore_squads(24, 80, 999);

    // The orphan was swept from the store; the named squad remains.
    let loaded = crate::squad_store::load();
    assert!(
        !loaded.squads.iter().any(|s| s.key == "orphan"),
        "unnamed dead-origin orphan swept: {:?}",
        loaded.squads
    );
    assert!(
        loaded.squads.iter().any(|s| s.name == "real"),
        "named squad kept: {:?}",
        loaded.squads
    );
    // Only the named squad was restored into the session.
    assert_eq!(core.session.squads.len(), 1, "the orphan is not restored");
    assert_eq!(core.session.squads[0].name.as_deref(), Some("real"));

    // Reap the spawned shell so the test leaks no process.
    let pids: Vec<u64> = core.panes.keys().copied().collect();
    for pid in pids {
        core.reap_pane(pid);
    }
}

fn run_workspace_restore(core: &mut Core, dry_run: bool) -> Vec<RestoreRow> {
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.handle(CoreMsg::WorkspaceRestoreApply {
        dry_run,
        harness: None,
        plans: HashMap::new(),
        reply: tx,
    });
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::WorkspaceRestored { rows } => rows,
        other => panic!("expected WorkspaceRestored, got {other:?}"),
    }
}

fn stored_worker(
    name: &str,
    harness: &str,
    sid: &str,
    cwd: &str,
) -> crate::squad_store::StoredMember {
    crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        detached: false,
        tab_name: None,
        cwd: Some(cwd.into()),
        worker: Some(name.into()),
        harness: Some(harness.into()),
        harness_session_id: Some(sid.into()),
    }
}

#[test]
fn workspace_restore_before_the_first_attach_refuses_not_reports_empty() {
    // The persisted squads reach memory only on the first real attach, so
    // a pre-attach verb must name the precondition rather than answer an
    // empty member list that reads as "nothing to restore".
    let mut core = empty_core();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.handle(CoreMsg::WorkspaceRestore {
        dry_run: false,
        harness: None,
        reply: tx,
    });
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, crate::proto::err_code::RESTORE_NOT_RUN);
            assert!(
                msg.contains("attach"),
                "refusal must name the remedy: {msg}"
            );
        }
        other => panic!("expected Err, got {other:?}"),
    }
    // After the first real attach ran the startup restore, the same verb
    // proceeds instead of refusing.
    core.restored = true;
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.handle(CoreMsg::WorkspaceRestore {
        dry_run: true,
        harness: None,
        reply: tx,
    });
    assert!(
        matches!(
            rx.blocking_recv().expect("a reply"),
            ServerMsg::WorkspaceRestored { .. }
        ),
        "a post-attach restore must not be refused"
    );
}

#[test]
fn workspace_restore_refuses_duplicated_worker_names_up_front() {
    // Two stored members may share one display name with distinct session
    // identities (a supported store state). The bulk path refuses both by
    // name instead of letting the second twin find the first one's pane
    // through the name-only map and report "focused" while its own
    // session was never restored.
    let _guard = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    // (x-b64e) The verb classifies candidates like the startup path, so
    // the test pins the registry seam its members must survive.
    let _known = KnownWorkersGuard;
    set_known_workers(&["twin"]);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-ws-restore-dup");
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
    core.squad_members.insert(
        7,
        vec![
            stored_worker("twin", "codex", "codex-session-one", &cwd.to_string_lossy()),
            stored_worker("twin", "codex", "codex-session-two", &cwd.to_string_lossy()),
        ],
    );
    let rows = run_workspace_restore(&mut core, false);
    assert_eq!(rows.len(), 2, "both twins report");
    for row in &rows {
        assert_eq!(row.outcome, "refused");
        assert!(
            row.reason
                .as_deref()
                .is_some_and(|r| r.contains("ambiguous")),
            "the refusal names the ambiguity: {:?}",
            row.reason
        );
    }
    assert!(
        core.worker_pane.is_empty(),
        "the guard refuses before any spawn: {:?}",
        core.worker_pane
    );
}

#[test]
fn workspace_restore_resumes_members_and_a_rerun_focuses() {
    // AC1-HP + AC6-ERR: one apply resumes the stored worker through the
    // (overridden) harness form; a second apply FOCUSES the live pane and
    // spawns nothing. Tombstoned and non-worker members are not
    // candidates.
    let _guard = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    // (x-b64e) The verb classifies candidates like the startup path, so
    // the test pins the registry seam its members must survive.
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-codex-one"]);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-ws-restore");
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
    core.squad_members.insert(
        7u64,
        vec![
            stored_worker(
                "t-codex-one",
                "codex",
                "01a027ad-fe00-7c12-a116-9ee37c6bdfec",
                cwd.to_string_lossy().as_ref(),
            ),
            // Not candidates: an attach-recorded member carries no worker
            // name, and a tombstoned member is dead by operator ruling.
            crate::squad_store::StoredMember {
                attach_id: "deadbee1".into(),
                tombstone: false,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: None,
                harness: Some("claude".into()),
                harness_session_id: None,
            },
            {
                let mut dead = stored_worker("gone-row", "codex", "sid-gone", "/x");
                dead.tombstone = true;
                dead
            },
        ],
    );

    let rows = run_workspace_restore(&mut core, false);
    assert_eq!(rows.len(), 1, "exactly the live worker is a candidate");
    assert_eq!(rows[0].member, "t-codex-one");
    assert_eq!(rows[0].outcome, "resumed", "{:?}", rows[0]);
    let resumed_pane = rows[0].pane.expect("resumed row names its pane");
    let new_panes: Vec<u64> = core
        .panes
        .keys()
        .filter(|&&p| p != shell)
        .copied()
        .collect();
    assert_eq!(
        new_panes,
        vec![resumed_pane],
        "one new pane, the reported one"
    );

    // The rerun focuses the SAME pane: no second writer ever starts.
    let rows = run_workspace_restore(&mut core, false);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "focused", "{:?}", rows[0]);
    assert_eq!(rows[0].pane, Some(resumed_pane), "the live pane is focused");
    let still_one: Vec<u64> = core
        .panes
        .keys()
        .filter(|&&p| p != shell)
        .copied()
        .collect();
    assert_eq!(still_one, vec![resumed_pane], "the rerun spawned nothing");

    core.reap_pane(resumed_pane);
    core.reap_pane(shell);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn workspace_restore_dry_run_classifies_without_spawning() {
    // --dry-run is load-bearing: the plan is readable before twenty
    // processes start. Every gate runs; nothing does.
    let _guard = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-codex-one"]);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-ws-restore-dry");
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
    core.squad_members.insert(
        7u64,
        vec![stored_worker(
            "t-codex-one",
            "codex",
            "01a027ad-fe00-7c12-a116-9ee37c6bdfec",
            cwd.to_string_lossy().as_ref(),
        )],
    );

    let rows = run_workspace_restore(&mut core, true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "planned", "{:?}", rows[0]);
    assert!(rows[0].pane.is_none(), "a plan names no pane");
    assert_eq!(
        core.panes.len(),
        1,
        "the dry run spawned nothing beyond the seed shell"
    );
    core.reap_pane(shell);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn workspace_restore_names_every_refused_member_and_restores_the_rest() {
    // AC5-ERR: a member the table gives no form for, and a claude member
    // whose plan never resolved, are NAMED with their reasons while the
    // resumable member still resumes. A silent skip would look identical
    // to "the code never ran".
    let _guard = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    // (x-b64e) The verb classifies candidates like the startup path, so
    // the test pins the registry seam its members must survive.
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-codex-one", "mystery", "routed-glm"]);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join("fno-ws-restore-refused");
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
    core.squad_members.insert(
        7u64,
        vec![
            stored_worker(
                "t-codex-one",
                "codex",
                "01a027ad-fe00-7c12-a116-9ee37c6bdfec",
                cwd.to_string_lossy().as_ref(),
            ),
            // A harness no table row declares: the negative arm names it.
            stored_worker("mystery", "iambad", "sid-9", "/x"),
            // A claude member whose plan never resolved on the bulk path.
            stored_worker("routed-glm", "claude", "uuid-1", "/x"),
        ],
    );

    let rows = run_workspace_restore(&mut core, false);
    let by_name = |n: &str| {
        rows.iter()
            .find(|r| r.member == n)
            .unwrap_or_else(|| panic!("no row for {n} in {rows:?}"))
    };
    assert_eq!(by_name("t-codex-one").outcome, "resumed", "{rows:?}");
    let mystery = by_name("mystery");
    assert_eq!(mystery.outcome, "refused");
    let reason = mystery
        .reason
        .as_deref()
        .expect("the refusal names a reason");
    assert!(reason.contains("iambad"), "the harness is named: {reason}");
    let routed = by_name("routed-glm");
    assert_eq!(routed.outcome, "refused");
    let reason = routed
        .reason
        .as_deref()
        .expect("the refusal names a reason");
    assert!(
        reason.contains("re-entry plan unresolved"),
        "the missing plan is named: {reason}"
    );

    let resumed: Vec<u64> = rows.iter().filter_map(|r| r.pane).collect();
    for pid in resumed {
        core.reap_pane(pid);
    }
    core.reap_pane(shell);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn restore_merges_unnamed_lane_into_home_squad() {
    // Operator decision: an unnamed lane persists and comes back. Because
    // restore runs AFTER attach() minted the connecting client's home squad,
    // a restored unnamed lane whose origins match home merges its members INTO
    // home rather than duplicating it. (A dead member stands in for the live
    // re-attach, whose spawn path the named-restore tests already cover.)
    let _s = StoreScratch::new("restore-home-merge");
    // A real cwd so the self-heal sweep sees a SURVIVING origin and keeps the
    // lane (a gone-origin lane would be reaped at restore, x-a572 US4).
    let home = std::env::temp_dir().join(format!("fno-restore-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let home_str = home.to_str().unwrap().to_string();
    crate::squad_store::upsert(
        "",
        "homekey1",
        std::slice::from_ref(&home_str),
        &[stored_member("deadbeef", false)],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = shell_candidates(std::env::var_os("SHELL").as_deref());
    // A freshly-minted home squad for the SAME cwd, exactly as attach() leaves it.
    let home_pid = core.spawn_pane(24, 80, &home_str).expect("home shell");
    core.session.add_squad(
        1,
        vec![home_str.clone()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(home_pid),
            focus: home_pid,
        },
    );
    let squads_before = core.session.squads.len();

    core.restore_squads(24, 80, 1);

    assert_eq!(
        core.session.squads.len(),
        squads_before,
        "no duplicate squad - the lane merged into home"
    );
    assert_eq!(core.session.squads[0].id, 1, "still the home squad");
    assert_eq!(
        core.session.squads[0].tabs.len(),
        1,
        "home keeps its one shell tab - no extra fallback shell"
    );
    assert!(
        core.squad_members[&1]
            .iter()
            .any(|m| m.attach_id == "deadbeef"),
        "the lane's member folded into home"
    );
    let pids: Vec<u64> = core.panes.keys().copied().collect();
    for pid in pids {
        core.reap_pane(pid);
    }
}

#[test]
fn restore_reconstructs_a_separate_unnamed_lane_as_its_own_squad() {
    // A persisted unnamed lane whose origins DON'T match home restores as its
    // own squad (not merged) - every squad remains, TUI or API.
    let _s = StoreScratch::new("restore-separate-lane");
    // A real cwd so the self-heal sweep keeps the lane (a gone-origin lane
    // would be reaped at restore, x-a572 US4).
    let lane_cwd =
        std::env::temp_dir().join(format!("fno-restore-separate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&lane_cwd);
    std::fs::create_dir_all(&lane_cwd).unwrap();
    let lane_cwd_str = lane_cwd.to_str().unwrap().to_string();
    crate::squad_store::upsert(
        "",
        "lanekey1",
        std::slice::from_ref(&lane_cwd_str),
        &[stored_member("deadbeef", false)],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = shell_candidates(std::env::var_os("SHELL").as_deref());
    let home_pid = core.spawn_pane(24, 80, "/tmp/home").expect("home shell");
    core.session.add_squad(
        1,
        vec!["/tmp/home".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(home_pid),
            focus: home_pid,
        },
    );

    core.restore_squads(24, 80, 1);

    assert_eq!(
        core.session.squads.len(),
        2,
        "the separate lane restored as its own squad"
    );
    let lane = core
        .session
        .squads
        .iter()
        .find(|s| s.origins == vec![lane_cwd_str.clone()])
        .expect("lane restored");
    assert!(lane.name.is_none(), "restored unnamed");
    assert_eq!(lane.tabs.len(), 1, "zero-live lane gets its fallback shell");
    let lane_sid = lane.id;
    assert!(
        core.squad_members[&lane_sid]
            .iter()
            .any(|m| m.attach_id == "deadbeef" && m.tombstone),
        "the dead member restored as a tombstone under its own lane"
    );
    let pids: Vec<u64> = core.panes.keys().copied().collect();
    for pid in pids {
        core.reap_pane(pid);
    }
}
