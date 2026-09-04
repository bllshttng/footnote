//! x-0f42 external-lifecycle sync and x-54fa routed-backlog families: moved verbatim out of server.rs
//! (file budget shrink). Parent helpers resolve through the glob.
use super::*;


    #[test]
    fn external_lifecycle_row_carries_cwd_base_when_squad_matched() {
        // x-6851 US3 (codex review): a squad-matched external-lifecycle tombstone
        // must carry its cwd_base (the every-row wire contract), so a foreign-cwd
        // child directory still renders the exception subline instead of reading
        // as same-project. Before the fix this branch left cwd_base None for a
        // matched row.
        use crate::squad_store::{ExternalLifecycle, ExternalState};
        let mut core = placement_core(); // squad 7 owns /repo/default
        core.external_lifecycle = vec![ExternalLifecycle {
            attach_id: "abc123".into(),
            name: "dead-worker".into(),
            cwd: "/repo/default/worktrees/x-6851".into(), // child of the squad root
            state: ExternalState::Stopped,
            generation: 0,
            updated_at: String::new(),
            reason: None,
        }];
        let rows = core.agent_rows();
        let row = rows.iter().find(|r| r.name == "dead-worker").unwrap();
        assert_eq!(
            row.squad,
            Some(7),
            "a cwd under the squad root is squad-matched"
        );
        assert_eq!(
            row.cwd_base.as_deref(),
            Some("x-6851"),
            "a squad-matched external row now carries its cwd basename"
        );
    }

    #[test]
    fn external_lifecycle_sync_does_not_flush_the_dirty_map() {
        // x-0f42: reemit=true's frame-flush (`d.clear()` then reseed only the
        // client's CURRENTLY VIEWED pane ids) is the actual resize-storm-shaped
        // defect: it drops any dirty-map entry that isn't part of the live
        // view, same failure class as x-0296's quiet-pane loss. A currently
        // VIEWED pane is a bad probe for this (reemit=true immediately reseeds
        // it with a fresh frame, so `contains_key` stays true either way) - the
        // probe has to be a dirty entry that push_layout can never reseed: a
        // pane id belonging to no live tab. reemit=false never touches `dirty`
        // at all, so it survives; reemit=true's unconditional `d.clear()` wipes
        // it with nothing to put back.
        use crate::squad_store::{ExternalLifecycle, ExternalState};
        let mut core = empty_core();
        core.shells = vec!["/bin/cat".into()];
        let p1 = core.spawn_pane(24, 40, "/tmp").expect("pane 1");
        core.session.add_squad(
            1,
            vec!["/tmp/x0f42".into()],
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
            "/tmp/x0f42".into(),
            "/tmp/x0f42".into(),
            tx,
            dirty.clone(),
            Arc::new(Notify::new()),
        );
        while rx.try_recv().is_ok() {}
        // A dangling dirty-map entry under a pane id that belongs to no tab -
        // standing in for a background/quiet pane whose only copy of a frame
        // sits in a client's dirty map outside its current view. No real
        // geometry pass can ever reseed this key.
        const DANGLING_PANE_ID: u64 = 999_999;
        let sentinel = core.panes.get(&p1).unwrap().vt.frame();
        dirty.lock().unwrap().insert(DANGLING_PANE_ID, sentinel);

        let existing = vec![ExternalLifecycle {
            attach_id: "abc123".into(),
            name: "worker".into(),
            cwd: "/tmp/other".into(),
            state: ExternalState::Stopping,
            generation: 3,
            updated_at: "t0".into(),
            reason: None,
        }];
        core.external_lifecycle = existing.clone();
        // A late sync carrying the IDENTICAL record set (a stale action's
        // late completion, or the startup reconcile re-observing the same
        // state) is exactly the no-op-content case this handler must treat
        // as sideline-only.
        core.handle_msg(CoreMsg::ExternalLifecycleSync {
            to: None,
            records: existing,
            notices: vec![],
        });
        assert!(
            dirty.lock().unwrap().contains_key(&DANGLING_PANE_ID),
            "an ExternalLifecycleSync must not flush the dirty map: rects \
             never depend on external_lifecycle content, so this is exactly \
             the reemit=false case AgentRows/BacklogCards already use (x-0f42)"
        );
    }

    #[test]
    fn external_lifecycle_sync_with_changed_records_still_pushes_layout_to_clients() {
        // x-0f42: the fix must not turn this into a no-op path entirely - a
        // genuinely different sideline (a real state transition, e.g.
        // Stopping -> Stopped) still has to reach clients as a fresh Layout
        // so the sideline agent list actually updates. reemit=false still
        // sends Layout unconditionally; only the frame flush is skipped.
        use crate::squad_store::{ExternalLifecycle, ExternalState};
        let mut core = empty_core();
        core.shells = vec!["/bin/cat".into()];
        let p1 = core.spawn_pane(24, 40, "/tmp").expect("pane 1");
        core.session.add_squad(
            1,
            vec!["/tmp/x0f42b".into()],
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
            "/tmp/x0f42b".into(),
            "/tmp/x0f42b".into(),
            tx,
            dirty.clone(),
            Arc::new(Notify::new()),
        );
        while rx.try_recv().is_ok() {}

        core.external_lifecycle = vec![ExternalLifecycle {
            attach_id: "abc123".into(),
            name: "worker".into(),
            cwd: "/tmp/other".into(),
            state: ExternalState::Stopping,
            generation: 3,
            updated_at: "t0".into(),
            reason: None,
        }];
        let changed = vec![ExternalLifecycle {
            attach_id: "abc123".into(),
            name: "worker".into(),
            cwd: "/tmp/other".into(),
            state: ExternalState::Stopped,
            generation: 4,
            updated_at: "t1".into(),
            reason: None,
        }];
        core.handle_msg(CoreMsg::ExternalLifecycleSync {
            to: None,
            records: changed,
            notices: vec![],
        });
        let saw_layout = std::iter::from_fn(|| rx.try_recv().ok())
            .any(|m| matches!(m, ServerMsg::Layout { .. }));
        assert!(
            saw_layout,
            "a genuinely different external_lifecycle set must still re-push \
             Layout to clients, even though no pane resizes (x-0f42)"
        );
    }

    #[test]
    fn routed_backlog_joins_attach_then_hint_and_leaves_ready_alone() {
        // x-54fa Phase B publish-time join, minus the pane arm (a live pane
        // needs a real PTY; the pane join key - FNO_NODE provenance equality -
        // is covered by the extract_fno_node tests + node_pane's trivial scan).
        let card = |id: &str, state| BacklogCard {
            id: id.into(),
            slug: format!("{id}-slug"),
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
        let mut core = empty_core();
        core.backlog = vec![
            card("x-aaa", CardState::InFlight), // attach via name token
            card("x-bbb", CardState::InFlight), // hint via matched row, no jobId
            card("x-ccc", CardState::InFlight), // hint via claim holder
            card("x-ddd", CardState::InFlight), // unroutable, nothing known
            card("x-eee", CardState::Ready),    // never joined
        ];
        core.agents = vec![
            bg_row("tgt-x-aaa", "/w/other", Some("deadbee1")),
            // cwd-basename match (worktree-per-node convention), no jobId.
            bg_row("worker", "/w/x-bbb", None),
            // Rows that must NOT route: exited, pane-hosted, ready-card match.
            RegistryAgent {
                session_id: None,
                harness_session_id: None,
                predecessor_session_ids: Vec::new(),
                related_session_id: None,
                forked_from_session_id: None,
                exited: true,
                ..bg_row("tgt-x-ddd", "/w", Some("deadbee2"))
            },
            RegistryAgent {
                session_id: None,
                harness_session_id: None,
                predecessor_session_ids: Vec::new(),
                related_session_id: None,
                forked_from_session_id: None,
                mux: Some(("test".into(), 5)),
                ..bg_row("tgt-x-ddd", "/w", Some("deadbee3"))
            },
            bg_row("tgt-x-eee", "/w", Some("deadbee4")),
        ];
        core.backlog_holders
            .insert("x-ccc".into(), "target-session:abc".into());
        let cards = core.routed_backlog();
        assert_eq!(cards[0].attach_id.as_deref(), Some("deadbee1"));
        assert_eq!(cards[0].where_hint, None, "attach route wins over hint");
        assert_eq!(
            cards[1].where_hint.as_deref(),
            Some("in flight - session worker")
        );
        assert_eq!(
            cards[2].where_hint.as_deref(),
            Some("in flight - worked by target-session:abc")
        );
        let bare = &cards[3];
        assert!(
            bare.pane_id.is_none() && bare.attach_id.is_none() && bare.where_hint.is_none(),
            "exited/pane-hosted rows never route"
        );
        let ready = &cards[4];
        assert!(
            ready.attach_id.is_none() && ready.where_hint.is_none(),
            "a ready card is never joined"
        );
    }

    #[test]
    fn inflight_route_resolves_by_id_or_slug_and_fails_closed() {
        // The stale-client DispatchNode re-check (AC2-ERR): an in-flight card
        // with an attach target routes; ready/unknown/unroutable stay None so
        // the handler falls through to dispatch or the refusal notice.
        let mut core = empty_core();
        core.backlog = vec![
            BacklogCard {
                id: "x-aaa".into(),
                slug: "aaa-slug".into(),
                priority: "p2".into(),
                state: CardState::InFlight,
                pane_id: None,
                attach_id: None,
                where_hint: None,
                project: None,
                lane: None,
                plan_path: None,
                head: false,
            },
            BacklogCard {
                id: "x-rdy".into(),
                slug: "rdy-slug".into(),
                priority: "p2".into(),
                state: CardState::Ready,
                pane_id: None,
                attach_id: None,
                where_hint: None,
                project: None,
                lane: None,
                plan_path: None,
                head: false,
            },
        ];
        core.agents = vec![bg_row("tgt-x-aaa", "/w", Some("deadbee1"))];
        assert_eq!(
            core.inflight_route("x-aaa"),
            Some(Command::attach_agent("deadbee1"))
        );
        assert_eq!(
            core.inflight_route("aaa-slug"),
            Some(Command::attach_agent("deadbee1")),
            "slug names the same card"
        );
        assert_eq!(core.inflight_route("x-rdy"), None, "ready is not routed");
        assert_eq!(core.inflight_route("x-nope"), None, "unknown fails closed");
        core.agents.clear();
        assert_eq!(
            core.inflight_route("x-aaa"),
            None,
            "unroutable in-flight falls through to the refusal notice"
        );
    }

    #[test]
    fn inflight_hint_names_session_then_holder_then_default() {
        // Codex peer review: a stale-client DispatchNode on an in-flight card
        // with NO route must get the situated hint, not the bare not-ready
        // refusal. Hint precedence: matched registry row's session name >
        // claim holder > the client's default copy.
        let mut core = empty_core();
        core.backlog = vec![BacklogCard {
            id: "x-aaa".into(),
            slug: "aaa-slug".into(),
            priority: "p2".into(),
            state: CardState::InFlight,
            pane_id: None,
            attach_id: None,
            where_hint: None,
            project: None,
            lane: None,
            plan_path: None,
            head: false,
        }];
        // Nothing known at all: the default copy.
        assert_eq!(
            core.inflight_hint("x-aaa").as_deref(),
            Some("card in flight - no session visible here")
        );
        // A claim holder is known: name it.
        core.backlog_holders
            .insert("x-aaa".into(), "target-session:abc".into());
        assert_eq!(
            core.inflight_hint("aaa-slug").as_deref(),
            Some("in flight - worked by target-session:abc"),
            "slug names the same card"
        );
        // A matched (unattachable) registry row outranks the holder.
        core.agents = vec![bg_row("tgt-x-aaa", "/w", None)];
        assert_eq!(
            core.inflight_hint("x-aaa").as_deref(),
            Some("in flight - session tgt-x-aaa")
        );
        // Not in flight / unknown: None (caller falls through to not-ready).
        assert_eq!(core.inflight_hint("x-nope"), None);
    }

    #[test]
    fn classify_guard_registry_fails_closed_on_a_row_with_no_readable_pane_binding() {
        // AC4-ERR (x-0b40), the positive marker: the malformed row IS the
        // defect. A registry holding an agent whose pane cannot be read must
        // refuse - a nameless row used to be skipped outright and read as
        // "no row for this pane, it is a shell", so the guarded send wrote
        // into a working agent.
        let raw =
            r#"{"agents":[{"name":"ok","cwd":"/w","status":"live"},{"cwd":"/w","status":"live"}]}"#;
        let err = classify_guard_registry(raw, 0).unwrap_err();
        assert!(
            err.contains("no readable pane binding"),
            "refusal names the row-level cause: {err}"
        );
    }

    #[test]
    fn classify_guard_registry_fails_closed_on_a_present_but_unparseable_mux() {
        // The fold arm (x-0b40): the row SURVIVES derivation with `mux: None`,
        // so this is not a skip and a drop count alone would miss it. The
        // half-read `session` is this pane's own; the missing `pane_id` is
        // exactly the part that cannot be attributed.
        let raw =
            r#"{"agents":[{"name":"half","cwd":"/w","status":"live","mux":{"session":"sess"}}]}"#;
        let err = classify_guard_registry(raw, 0).unwrap_err();
        assert!(
            err.contains("no readable pane binding"),
            "refusal names the row-level cause: {err}"
        );
    }

