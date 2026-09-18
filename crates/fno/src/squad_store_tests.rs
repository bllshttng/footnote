//! `squad_store`'s tests, file-backed so the store itself stays small.

    use super::*;
    use std::path::PathBuf;

    /// A scratch store dir installed via the per-thread path override, so the
    /// store never touches a real file AND never mutates the process
    /// environment (no cross-test env race). Cleared on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fno-squadstore-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            super::set_test_path(&dir);
            Scratch(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join("squads.json")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            super::clear_test_path();
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn m(id: &str) -> StoredMember {
        StoredMember {
            attach_id: id.into(),
            tombstone: false,
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
    fn stored_member_roundtrips_detached_marker() {
        let raw = r#"{
            "attach_id":"",
            "worker":"worker-a",
            "harness":"codex",
            "harness_session_id":"session-a",
            "detached":true
        }"#;
        let member: StoredMember = serde_json::from_str(raw).unwrap();
        let encoded = serde_json::to_value(member).unwrap();
        assert_eq!(
            encoded.get("detached"),
            Some(&serde_json::Value::Bool(true)),
            "restore needs a positive persisted detached marker"
        );
    }

    #[test]
    fn default_test_store_never_targets_the_user_home() {
        super::clear_test_path();
        let path = squads_path();
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert!(
            !path.starts_with(&home),
            "an unscoped unit test must not persist mux squads under HOME: {}",
            path.display()
        );
    }

    #[test]
    fn pane_id_reservation_survives_restart_and_advances_past_floor() {
        let _s = Scratch::new("pane-counter");

        assert_eq!(reserve_next_pane_id(1).unwrap(), 1);
        assert_eq!(reserve_next_pane_id(1).unwrap(), 2);
        assert_eq!(reserve_next_pane_id(9).unwrap(), 9);
        assert_eq!(load().next_pane_id, 10);
    }

    #[test]
    fn missing_file_is_a_fresh_store() {
        let _s = Scratch::new("missing");
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.is_none(), "a missing file is silent");
    }

    #[test]
    fn pre_xc4d4_store_loads_without_tab_specs_field() {
        // AC9: a store written before has no `tab_specs` key. It must load
        // unquarantined (STORE_VERSION unchanged), defaulting tab_specs to empty.
        let s = Scratch::new("no-tab-specs");
        // Hand-write a v1 squad object WITHOUT the tab_specs key.
        let raw = r#"{"version":1,"squads":[{"name":"w","origins":[],"members":[],"created_at":"2026-07-11T00:00:00Z"}]}"#;
        std::fs::write(s.file(), raw).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        assert!(
            loaded.squads[0].tab_specs.is_empty(),
            "tab_specs defaults to empty"
        );
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn set_tab_specs_persists_and_upsert_preserves_it() {
        use crate::proto::{LayoutSpec, SlotBinding, TemplateName};
        let _s = Scratch::new("tab-specs");
        let spec = StoredTabSpec {
            tab_name: "grid".into(),
            spec: LayoutSpec {
                template: TemplateName::MainLeft,
                slots: vec![SlotBinding::Fno("S1".into()), SlotBinding::Shell],
            },
        };
        upsert("w", "", &["/r".into()], &[m("c19cd2c3")]).unwrap();
        set_tab_specs("w", std::slice::from_ref(&spec)).unwrap();
        assert_eq!(load().squads[0].tab_specs, vec![spec.clone()]);
        // A later membership upsert must NOT wipe the template specs (they are
        // owned by set_tab_specs, and upsert rebuilds the struct fresh).
        upsert("w", "", &["/r".into()], &[m("c19cd2c3"), m("deadbeef")]).unwrap();
        let after = load();
        assert_eq!(after.squads[0].members.len(), 2, "membership updated");
        assert_eq!(
            after.squads[0].tab_specs,
            vec![spec],
            "tab_specs preserved across upsert"
        );
    }

    #[test]
    fn tab_trees_persist_by_key_for_an_unnamed_squad_and_survive_upsert() {
        // gate 2: topology keys by squad IDENTITY (name or key), so an
        // unnamed squad - the operator's `commanders` case, a named tab in an
        // unnamed squad - holds a layout. A membership upsert must not wipe it.
        use crate::proto::{LayoutBinding, LayoutSlot, LayoutTreeChild, LayoutTreeSpec};
        use crate::tree::Axis;
        let _s = Scratch::new("tab-trees");
        let tree = StoredTabTree {
            tab_name: Some("commanders".into()),
            tree: LayoutTreeSpec::Split {
                axis: Axis::Horizontal,
                children: vec![
                    LayoutTreeChild {
                        weight: 0.407,
                        tree: LayoutTreeSpec::Slot("aaaaaaaa".into()),
                    },
                    LayoutTreeChild {
                        weight: 0.593,
                        tree: LayoutTreeSpec::Split {
                            axis: Axis::Vertical,
                            children: vec![
                                LayoutTreeChild {
                                    weight: 0.5,
                                    tree: crate::proto::LayoutTreeSpec::Slot("p1".into()),
                                },
                                LayoutTreeChild {
                                    weight: 0.5,
                                    tree: LayoutTreeSpec::Slot("p2".into()),
                                },
                            ],
                        },
                    },
                ],
            },
            slots: vec![
                LayoutSlot {
                    name: "aaaaaaaa".into(),
                    binding: LayoutBinding::Fno("aaaaaaaa".into()),
                    cwd: None,
                    portal: None,
                    pane_id: None,
                },
                LayoutSlot {
                    name: "p1".into(),
                    binding: LayoutBinding::Shell,
                    cwd: None,
                    portal: None,
                    pane_id: None,
                },
                LayoutSlot {
                    name: "p2".into(),
                    binding: LayoutBinding::Shell,
                    cwd: None,
                    portal: None,
                    pane_id: None,
                },
            ],
            focus: Some("p1".into()),
        };
        // An UNNAMED squad, keyed by its durable key with no name.
        set_tab_trees(
            "",
            "k1",
            &["/repo".into()],
            std::slice::from_ref(&tree),
            Some(0),
        )
        .unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads.len(),
            1,
            "minimal row minted for the keyed lane"
        );
        assert_eq!(loaded.squads[0].tab_trees, vec![tree.clone()]);
        assert_eq!(loaded.squads[0].active_tab, Some(0));
        // A membership upsert (same identity) preserves the tree lane.
        upsert("", "k1", &["/repo".into()], &[m("aaaaaaaa")]).unwrap();
        let after = load();
        assert_eq!(after.squads.len(), 1);
        assert_eq!(
            after.squads[0].tab_trees,
            vec![tree.clone()],
            "trees survive upsert"
        );
        assert_eq!(after.squads[0].members.len(), 1);
        // An identity-less squad is skipped, exactly like upsert.
        set_tab_trees("", "", &[], std::slice::from_ref(&tree), None).unwrap();
        assert_eq!(load().squads.len(), 1, "no row minted without identity");
    }

    #[test]
    fn pre_xcaef_store_loads_without_tab_trees_field() {
        // Wire tolerance: a store written before has neither key. It
        // must load unquarantined (STORE_VERSION unchanged), trees empty, and
        // restore takes the legacy member/template lanes.
        let s = Scratch::new("no-tab-trees");
        let raw = r#"{"version":1,"squads":[{"name":"w","origins":[],"members":[],"created_at":"2026-08-11T00:00:00Z","tab_specs":[]}]}"#;
        std::fs::write(s.file(), raw).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        assert!(loaded.squads[0].tab_trees.is_empty());
        assert_eq!(loaded.squads[0].active_tab, None);
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn pre_xcaef_member_loads_without_cwd_field() {
        // Wire tolerance for StoredMember.cwd, same rule as tab_trees above: a
        // member row written before has no "cwd" key and must load
        // unquarantined with cwd defaulting to None.
        let s = Scratch::new("no-member-cwd");
        let raw = r#"{"version":1,"squads":[{"name":"w","origins":[],"members":[{"attach_id":"c19cd2c3","tombstone":false}],"created_at":"2026-08-11T00:00:00Z","tab_specs":[]}]}"#;
        std::fs::write(s.file(), raw).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        assert_eq!(loaded.squads[0].members[0].cwd, None);
    }

    #[test]
    fn pre_v68_slot_loads_without_cwd_field_and_serializes_byte_identically() {
        // AC1-HP / AC2-EDGE: a tab_trees slot written before v68 has
        // no "cwd" key. It must load unquarantined with cwd defaulting to
        // None, and a None-cwd `LayoutSlot` must never emit a "cwd" key
        // (`skip_serializing_if`) - so a pre-v68 store round-trips byte for
        // byte through this version.
        use crate::proto::{LayoutBinding, LayoutSlot, LayoutTreeSpec};
        let s = Scratch::new("no-slot-cwd");
        let tree = StoredTabTree {
            tab_name: None,
            tree: crate::proto::LayoutTreeSpec::Slot("p1".into()),
            slots: vec![crate::proto::LayoutSlot {
                name: "p1".into(),
                binding: crate::proto::LayoutBinding::Shell,
                cwd: None,
                portal: None,
                pane_id: None,
            }],
            focus: None,
        };
        let raw = serde_json::json!({
            "version": 1,
            "squads": [{
                "name": "w",
                "origins": [],
                "members": [],
                "created_at": "2026-08-11T00:00:00Z",
                "tab_specs": [],
                "tab_trees": [tree],
                "active_tab": 0,
            }]
        })
        .to_string();
        assert!(
            !raw.contains("\"cwd\""),
            "a None-cwd slot must carry no cwd key: {raw}"
        );
        std::fs::write(s.file(), raw).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        assert_eq!(loaded.squads[0].tab_trees[0].slots[0].cwd, None);
    }

    #[test]
    fn portal_slot_field_loads_and_stays_absent_when_none() {
        // AC2-HP: a slot carrying "portal" (written by an a9b4
        // build) loads unquarantined, and a build WITHOUT the field reading
        // this store decodes the slot as the plain Shell slot it names -
        // pinned here by the serde contract: an unknown-field-tolerant
        // decode is the same wire rule a pre-a9b4 build applies. The
        // reverse direction is pinned too: a None-portal slot emits no
        // "portal" key, so a pre-a9b4 store round-trips byte for byte.
        use crate::proto::{LayoutBinding, LayoutSlot, LayoutTreeSpec, PortalSlot};
        let s = Scratch::new("portal-slot");
        let raw_with_portal = r#"{"version":1,"squads":[{"name":"w","origins":[],"members":[],"created_at":"2026-08-11T00:00:00Z","tab_specs":[],"tab_trees":[{"tab_name":"watch","tree":{"slot":"portal1"},"slots":[{"name":"portal1","binding":"shell","cwd":null,"portal":{"index":1,"row":"deadbee1"}}],"focus":null}],"active_tab":0}]}"#;
        std::fs::write(s.file(), raw_with_portal).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        let slot = &loaded.squads[0].tab_trees[0].slots[0];
        assert_eq!(
            slot.portal,
            Some(PortalSlot {
                index: 1,
                row: "deadbee1".into(),
                harness: None,
                session_id: None
            }),
            "the portal slot decodes with its index and row"
        );
        assert!(
            matches!(slot.binding, LayoutBinding::Shell),
            "the binding stays Shell"
        );

        // Reverse direction: a None-portal slot emits no "portal" key.
        let plain = StoredTabTree {
            tab_name: None,
            tree: crate::proto::LayoutTreeSpec::Slot("p1".into()),
            slots: vec![crate::proto::LayoutSlot {
                name: "p1".into(),
                binding: crate::proto::LayoutBinding::Shell,
                cwd: None,
                portal: None,
                pane_id: None,
            }],
            focus: None,
        };
        let raw = serde_json::json!({ "tab_trees": [plain] }).to_string();
        assert!(
            !raw.contains("\"portal\""),
            "a None-portal slot must carry no portal key: {raw}"
        );
    }

    #[test]
    fn upsert_then_load_roundtrips_and_preserves_created_at() {
        let _s = Scratch::new("roundtrip");
        upsert("harden", "", &["/repo".into()], &[m("c19cd2c3")]).unwrap();
        let first = load();
        assert_eq!(first.squads.len(), 1);
        let created = first.squads[0].created_at.clone();
        assert!(created.ends_with('Z') && created.len() == 20, "{created}");
        // A second upsert (new members) keeps the original created_at.
        upsert(
            "harden",
            "",
            &["/repo".into()],
            &[m("c19cd2c3"), m("deadbeef")],
        )
        .unwrap();
        let second = load();
        assert_eq!(second.squads.len(), 1, "upsert replaces, never dupes");
        assert_eq!(second.squads[0].members.len(), 2);
        assert_eq!(second.squads[0].created_at, created, "created_at preserved");
    }

    #[test]
    fn valid_worker_name_gate() {
        // the worker field is a registry name that keys a resume, so
        // the same argv-safety posture as valid_attach_id - a hostile value
        // must never survive load. Registry names are slugs; anything else
        // (separator, whitespace, metachar, overlong, non-ascii) is refused.
        assert!(valid_worker_name("probe-x5f7f"));
        assert!(valid_worker_name("t-xf730-sonnet"));
        assert!(valid_worker_name("a.b_c"));
        assert!(!valid_worker_name(""));
        assert!(!valid_worker_name("a/b"), "path separator");
        assert!(!valid_worker_name("a b"), "whitespace");
        assert!(!valid_worker_name("a;rm"), "shell metacharacter");
        assert!(!valid_worker_name("$(x)"), "command substitution");
        assert!(!valid_worker_name(&"x".repeat(65)), "overlong");
        assert!(!valid_worker_name("héllo"), "non-ascii");
    }

    #[test]
    fn worker_member_roundtrips_without_a_jobid() {
        // a worker member carries a registry NAME and an EMPTY
        // attach_id (a codex/agy pane has no claude jobId). It must round-trip
        // through the store and survive the load gate, which previously
        // dropped every member whose attach_id was not 8 hex digits - the
        // measured reason a widened field alone would have shipped nothing.
        let _s = Scratch::new("worker-roundtrip");
        let worker = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: Some("lane".into()),
            cwd: Some("/repo/wt".into()),
            worker: Some("probe-x5f7f".into()),
            harness: None,
            harness_session_id: None,
            pane_id: None,
        };
        upsert("work", "", &["/repo".into()], &[worker, m("c19cd2c3")]).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "no quarantine");
        assert!(loaded.notice.is_none(), "{:?}", loaded.notice);
        let members = &loaded.squads[0].members;
        assert_eq!(members.len(), 2, "worker member survives the load gate");
        let w = members
            .iter()
            .find(|m| m.worker.is_some())
            .expect("worker member present");
        assert_eq!(w.worker.as_deref(), Some("probe-x5f7f"));
        assert_eq!(w.attach_id, "", "no claude jobId on a worker member");
        assert_eq!(w.tab_name.as_deref(), Some("lane"), "tab name round-trips");
        assert_eq!(w.cwd.as_deref(), Some("/repo/wt"));
    }

    #[test]
    fn worker_member_roundtrips_full_harness_session_id() {
        let raw = r#"{
            "name":"targets",
            "members":[{
                "attach_id":"",
                "worker":"t-x8a01-restore",
                "harness":"codex",
                "harness_session_id":"01a03a85-1111-7222-8333-444455556666"
            }]
        }"#;
        let squad: StoredSquad = serde_json::from_str(raw).expect("stored squad");
        let encoded = serde_json::to_value(&squad).expect("serialize stored squad");
        assert_eq!(
            encoded["members"][0]["harness_session_id"], "01a03a85-1111-7222-8333-444455556666",
            "the full resume key must survive a squads.json read/write"
        );
        assert_eq!(encoded["members"][0]["harness"], "codex");
    }

    #[test]
    fn retire_session_members_tombstones_only_the_matching_identity() {
        // task 3: the exact-session retirement retires ONLY the
        // member whose (harness, session id) matches; a live sibling, an
        // already-tombstoned member and a shared-workspace plain pane all
        // survive, and a second call retires nothing (idempotent).
        let _s = Scratch::new("retire-session");
        let mut target = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("t-abcd-worker".into()),
            harness: Some("codex".into()),
            harness_session_id: Some("01a03a85-1111-7222-8333-444455556666".into()),
            pane_id: None,
        };
        let sibling = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("t-abcd-sibling".into()),
            harness: Some("codex".into()),
            harness_session_id: Some("22222222-1111-7222-8333-444455556666".into()),
            pane_id: None,
        };
        let already_gone = StoredMember {
            tombstone: true,
            tombstone_reason: None,
            harness: Some("codex".into()),
            harness_session_id: Some("01a03a85-1111-7222-8333-444455556666".into()),
            ..target.clone()
        };
        let _ = &mut target;
        upsert(
            "work",
            "",
            &["/repo".into()],
            &[target, sibling, already_gone],
        )
        .unwrap();

        let retired =
            super::retire_session_members("codex", "01a03a85-1111-7222-8333-444455556666").unwrap();
        assert_eq!(retired, 1, "only the live matching member retires");

        let loaded = load();
        assert_eq!(loaded.squads.len(), 1);
        let members = &loaded.squads[0].members;
        let target = members
            .iter()
            .find(|m| {
                m.harness_session_id.as_deref() == Some("01a03a85-1111-7222-8333-444455556666")
                    && m.worker.as_deref() == Some("t-abcd-worker")
            })
            .expect("target member still in the store");
        assert!(target.tombstone, "the target is tombstoned");
        let sibling = members
            .iter()
            .find(|m| m.worker.as_deref() == Some("t-abcd-sibling"))
            .expect("sibling survives");
        assert!(!sibling.tombstone, "the sibling is untouched");

        let again =
            super::retire_session_members("codex", "01a03a85-1111-7222-8333-444455556666").unwrap();
        assert_eq!(again, 0, "a second retirement retires nothing new");
    }
    #[test]
    fn hostile_worker_name_is_dropped_at_load() {
        // The load gate's argv-safety half: a worker name carrying a path
        // separator or a metacharacter never reaches a resume, exactly like a
        // malformed attach_id never reaches `claude attach`.
        let _s = Scratch::new("worker-hostile");
        let hostile = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("a;rm -rf".into()),
            harness: None,
            harness_session_id: None,
            pane_id: None,
        };
        upsert("work", "", &["/repo".into()], &[hostile, m("c19cd2c3")]).unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads[0].members.len(),
            1,
            "hostile worker member dropped at load"
        );
        assert_eq!(
            loaded.squads[0].members[0].attach_id, "c19cd2c3",
            "the healthy member is untouched"
        );
        assert!(loaded.notice.is_some(), "the drop is named, never silent");
    }

    #[test]
    fn four_squad_store_on_disk_loads_whole() {
        // the exact store shape measured on the operator's disk -
        // four squads, three holding ZERO members (the empty-squads defect:
        // worker panes never entered the membership funnel) and one holding
        // six claude attach members. The widened member must not quarantine or
        // notice on any of them; the fourth squad exercises a named row.
        let s = Scratch::new("four-squads");
        let members = [
            "119e3c52", "cbd219bd", "f5996a81", "3d9938aa", "a1b2c3d4", "e5f60718",
        ]
        .iter()
        .map(|id| {
            serde_json::json!({
                "attach_id": id,
                "tombstone": false,
                "tab_name": null,
                "cwd": "/wt/a"
            })
        })
        .collect::<Vec<_>>();
        let raw = serde_json::json!({
            "version": 1,
            "squads": [
                {"name": "", "key": "1111111111111111", "origins": ["/repo"],
                 "members": members, "created_at": "2026-08-21T00:00:00Z"},
                {"name": "", "key": "2222222222222222", "origins": ["/gone"],
                 "members": [], "created_at": "2026-08-21T00:00:00Z"},
                {"name": "", "key": "3333333333333333", "origins": ["/gone2"],
                 "members": [], "created_at": "2026-08-21T00:00:00Z"},
                {"name": "x-bbbb", "key": "", "origins": ["/repo"],
                 "members": [], "created_at": "2026-08-21T00:00:00Z"}
            ]
        });
        std::fs::write(s.file(), serde_json::to_string(&raw).unwrap()).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 4, "all four load");
        assert!(loaded.notice.is_none(), "{:?}", loaded.notice);
        assert_eq!(
            loaded.squads[0].members.len(),
            6,
            "the six claude members survive"
        );
        assert!(loaded.squads.iter().any(|sq| sq.name == "x-bbbb"));
    }

    #[test]
    fn tab_name_roundtrips_and_absent_field_loads_none() {
        // US4: a member's tab_name persists and reloads; a pre-change
        // store written without the field is wire-tolerant (loads as None ->
        // the tab restores unnamed), so STORE_VERSION stays 1.
        let s = Scratch::new("tabname");
        let mut named = m("c19cd2c3");
        named.tab_name = Some("reviews".into());
        upsert("work", "", &["/repo".into()], &[named]).unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads[0].members[0].tab_name.as_deref(),
            Some("reviews"),
            "chosen tab name round-trips"
        );

        // A hand-written v1 store with no tab_name field must not quarantine.
        std::fs::write(
            s.file(),
            r#"{"version":1,"squads":[{"name":"legacy","origins":[],"members":[{"attach_id":"deadbeef","tombstone":false}],"created_at":""}]}"#,
        )
        .unwrap();
        let loaded = load();
        assert!(loaded.notice.is_none(), "absent field is not corruption");
        assert_eq!(
            loaded.squads[0].members[0].tab_name, None,
            "absent tab_name -> None"
        );
    }

    #[test]
    fn corrupt_file_is_quarantined_and_read_empty() {
        // AC1-ERR: invalid JSON is renamed aside, not fatal.
        let s = Scratch::new("corrupt");
        std::fs::write(s.file(), "{not valid json").unwrap();
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.as_deref().unwrap().contains("quarantined"));
        assert!(!s.file().exists(), "the corrupt file was moved aside");
        let asides: Vec<_> = std::fs::read_dir(&s.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("squads.json.corrupt-")
            })
            .collect();
        assert_eq!(asides.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn a_quarantine_that_did_not_happen_is_never_claimed() {
        // The fallback shape from head-review round nine: bytes whose source
        // file is gone. The rename fails, so the notice must say so instead
        // of claiming a quarantine that repeats falsely on every load.
        let loaded = loaded_from_raw(
            std::path::Path::new("/nonexistent-squads-source"),
            "{not valid json".into(),
        );
        let notice = loaded.notice.expect("corrupt content must notice");
        assert!(!notice.contains("quarantined corrupt"), "{notice}");
        assert!(notice.contains("could not be quarantined"), "{notice}");
    }

    #[test]
    fn the_seed_degrades_legacy_corruption_and_refuses_primary_corruption() {
        // parse_seed mirrors load(): legacy-sourced corruption starts fresh
        // (load() quarantines-and-empties the same bytes), while corruption
        // at the primary path refuses the write rather than clobbering.
        let corrupt = "{not valid json".to_string();
        assert!(parse_seed(Some(corrupt.clone()), true)
            .unwrap()
            .squads
            .is_empty());
        assert!(parse_seed(Some(corrupt), false).is_err());
        assert!(parse_seed(None, false).unwrap().squads.is_empty());
        assert!(parse_seed(Some("  ".into()), true)
            .unwrap()
            .squads
            .is_empty());
        // An unknown version takes the same arms: refuse at the primary (the
        // read path quarantines it), degrade from the legacy fallback. The
        // rewrite at STORE_VERSION must never downgrade a future store.
        let future = r#"{"version":99,"squads":[]}"#.to_string();
        assert!(parse_seed(Some(future.clone()), false).is_err());
        assert!(parse_seed(Some(future), true).unwrap().squads.is_empty());
    }

    #[test]
    fn unknown_version_is_quarantined() {
        // Discretion 5: a version this build does not understand takes the
        // quarantine path, never a best-effort parse.
        let s = Scratch::new("version");
        std::fs::write(s.file(), r#"{"version":999,"squads":[]}"#).unwrap();
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.as_deref().unwrap().contains("quarantined"));
    }

    #[test]
    fn hostile_attach_ids_are_dropped_at_load() {
        // AC2-ERR: a member whose attach_id is not 8-hex never survives load,
        // so restore can never spawn it.
        let s = Scratch::new("hostile");
        let file = StoreFile {
            version: STORE_VERSION,
            generations: Default::default(),
            squads: vec![StoredSquad {
                name: "w".into(),
                key: String::new(),
                origins: vec![],
                members: vec![
                    m("c19cd2c3"),  // good
                    m("; rm -rf"),  // shell metachar
                    m("deadbeef9"), // 9 chars
                    m("GHIJKLmn"),  // non-hex
                ],
                created_at: "2026-07-11T00:00:00Z".into(),
                tab_specs: vec![],
                tab_trees: Vec::new(),
                active_tab: None,
            }],
            ..StoreFile::default()
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads[0].members, vec![m("c19cd2c3")]);
        assert!(loaded.notice.as_deref().unwrap().contains("dropped 3"));
    }

    #[test]
    fn upsert_never_persists_a_key_on_a_named_row() {
        // The invariant, enforced at the one write path every caller
        // funnels through: a named squad keys by name and leaves the key empty.
        let _s = Scratch::new("x6b0b-upsert-invariant");
        upsert("w", "stalekey", &["/repo".into()], &[m("c19cd2c3")]).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1);
        assert_eq!(loaded.squads[0].name, "w");
        assert!(
            loaded.squads[0].key.is_empty(),
            "a named row never carries a key"
        );
    }

    #[test]
    fn two_unnamed_squads_with_different_keys_remain_two_rows() {
        // AC15-EDGE: unnamed squads share the empty name and must never be
        // merged by it - the key is their only identity.
        let _s = Scratch::new("x6b0b-unnamed-distinct");
        upsert("", "aaaa1111bbbb2222", &["/a".into()], &[]).unwrap();
        upsert("", "cccc3333dddd4444", &["/b".into()], &[]).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 2);
        let mut keys: Vec<&str> = loaded.squads.iter().map(|sq| sq.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["aaaa1111bbbb2222", "cccc3333dddd4444"]);
    }

    #[test]
    fn load_repairs_a_name_and_key_row_by_folding_its_unnamed_twin() {
        // AC14-EDGE + the live-store shape: a rename of an unnamed
        // squad minted a named row that KEPT the key, leaving the old key row
        // alive beside it. Load clears the key, folds the twin into the named
        // row (the one the operator chose), keeps the EARLIER created_at, and
        // says so in the notice.
        let s = Scratch::new("x6b0b-fold-twin");
        let file = StoreFile {
            version: STORE_VERSION,
            generations: Default::default(),
            squads: vec![
                StoredSquad {
                    name: String::new(),
                    key: "c5bf8f5419a350e8".into(),
                    origins: vec!["/repo".into()],
                    members: vec![m("c19cd2c3")],
                    created_at: "2026-09-01T10:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
                StoredSquad {
                    name: "oss".into(),
                    key: "c5bf8f5419a350e8".into(),
                    origins: vec!["/repo".into()],
                    members: vec![m("deadbeef")],
                    created_at: "2026-09-01T10:01:21Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
            ],
            ..StoreFile::default()
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert!(loaded.notice.as_deref().unwrap().contains("repaired 1"));
        assert_eq!(loaded.squads.len(), 1, "the key twin folds away");
        let row = &loaded.squads[0];
        assert_eq!(row.name, "oss", "the named row survives");
        assert!(row.key.is_empty());
        assert_eq!(row.members.len(), 2, "members from both rows survive");
        assert_eq!(
            row.created_at, "2026-09-01T10:00:00Z",
            "the earlier stamp wins"
        );
    }

    #[test]
    fn load_repair_finds_the_key_twin_whichever_row_comes_first() {
        // The same pair as the fold test, listed in reverse order: the repair
        // strips every key before matching, so the twin is found regardless
        // of store row order and the result is identical.
        let s = Scratch::new("x6b0b-fold-twin-reversed");
        let file = StoreFile {
            version: STORE_VERSION,
            generations: Default::default(),
            squads: vec![
                StoredSquad {
                    name: "oss".into(),
                    key: "c5bf8f5419a350e8".into(),
                    origins: vec!["/repo".into()],
                    members: vec![m("deadbeef")],
                    created_at: "2026-09-01T10:01:21Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
                StoredSquad {
                    name: String::new(),
                    key: "c5bf8f5419a350e8".into(),
                    origins: vec!["/repo".into()],
                    members: vec![m("c19cd2c3")],
                    created_at: "2026-09-01T10:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
            ],
            ..StoreFile::default()
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert!(loaded.notice.as_deref().unwrap().contains("repaired 1"));
        assert_eq!(loaded.squads.len(), 1);
        let row = &loaded.squads[0];
        assert_eq!(row.name, "oss");
        assert!(row.key.is_empty());
        assert_eq!(row.members.len(), 2);
        assert_eq!(row.created_at, "2026-09-01T10:00:00Z");
    }

    #[test]
    fn load_repairs_a_name_and_key_row_onto_its_name_twin_without_a_key_twin() {
        let s = Scratch::new("x6b0b-fold-name-twin");
        let file = StoreFile {
            version: STORE_VERSION,
            generations: Default::default(),
            squads: vec![
                StoredSquad {
                    name: "oss".into(),
                    key: String::new(),
                    origins: vec!["/a".into()],
                    members: vec![m("c19cd2c3")],
                    created_at: "2026-09-01T10:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
                StoredSquad {
                    name: "oss".into(),
                    key: "leftover".into(),
                    origins: vec!["/a".into()],
                    members: vec![m("deadbeef")],
                    created_at: "2026-09-01T09:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
            ],
            ..StoreFile::default()
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert!(loaded.notice.as_deref().unwrap().contains("repaired 1"));
        assert_eq!(loaded.squads.len(), 1);
        let row = &loaded.squads[0];
        assert_eq!(row.name, "oss");
        assert!(row.key.is_empty());
        assert_eq!(row.members.len(), 2);
        assert_eq!(row.created_at, "2026-09-01T09:00:00Z");
    }

    #[test]
    fn a_name_marker_kills_only_an_identity_less_member() {
        // AC4/AC5/AC7-EDGE: the name is the identity of LAST resort. It kills
        // a member with no harness and no session id; absence keeps Unknown;
        // a member carrying either stronger key never reads the name set.
        use std::collections::HashSet;
        let never_bound = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("residue".into()),
            harness: None,
            harness_session_id: None,
            pane_id: None,
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        assert_eq!(
            evidence.verdict(&never_bound),
            MemberLiveness::Unknown,
            "AC5-EDGE: absence of a marker never prunes"
        );
        evidence.add_dead_name("residue");
        assert_eq!(
            evidence.verdict(&never_bound),
            MemberLiveness::Dead,
            "AC4-HP: the name marker reaches the prune verdict"
        );
        let mut bound = never_bound.clone();
        bound.harness = Some("codex".into());
        bound.harness_session_id = Some("s".into());
        assert_eq!(
            evidence.verdict(&bound),
            MemberLiveness::Unknown,
            "AC7-EDGE: a session-keyed member ignores the name marker"
        );
        evidence.add_dead_pair("codex", "s");
        assert_eq!(
            evidence.verdict(&bound),
            MemberLiveness::Dead,
            "AC7-EDGE: session-keyed evidence decides for the bound member"
        );
        let mut harness_only = never_bound.clone();
        harness_only.harness = Some("codex".into());
        assert_eq!(
            evidence.verdict(&harness_only),
            MemberLiveness::Dead,
            " the keys branch guarantees no session id, so the name \
             is all the member has even with a harness recorded"
        );
    }

    /// The reaped-row rule: a journal-spawned name that a SUCCESSFUL
    /// registry read does not carry is dead evidence. The inversion this
    /// fixes: row reaping destroyed the evidence, so the member was kept
    /// Unknown forever - one stranded squads.json row per reaped worker.
    #[test]
    fn a_spawned_name_absent_from_a_complete_registry_read_is_dead() {
        use std::collections::HashSet;
        let member = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("w1".into()),
            harness: Some("claude".into()),
            harness_session_id: None,
            pane_id: None,
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        evidence.fold_registry_rows(
            &[],
            ["w1".to_string()].into_iter().collect(),
            HashSet::new(),
            true,
        );
        assert_eq!(
            evidence.verdict(&member),
            MemberLiveness::Dead,
            "the row's absence, once the read succeeded, is positive death evidence"
        );
        let mut unreadable = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        unreadable.fold_registry_rows(
            &[],
            ["w1".to_string()].into_iter().collect(),
            HashSet::new(),
            false,
        );
        assert_eq!(
            unreadable.verdict(&member),
            MemberLiveness::Unknown,
            "a failed registry read stays fail-safe: absence is missing evidence"
        );
    }

    /// A still-held spawn receipt means the worker is resumable:
    /// registry absence must not read as its death.
    #[test]
    fn a_held_receipt_keeps_a_spawned_name_unknown() {
        use std::collections::HashSet;
        let member = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("w1".into()),
            harness: Some("claude".into()),
            harness_session_id: None,
            pane_id: None,
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        evidence.fold_registry_rows(
            &[],
            HashSet::new(),
            ["w1".to_string()].into_iter().collect(),
            true,
        );
        assert_eq!(
            evidence.verdict(&member),
            MemberLiveness::Unknown,
            "a resumable worker is not reaped by absence"
        );
    }

    /// The keeper guard: a keeper-held worker's child is alive while
    /// its registry row reads Dead, so a held spawn receipt must outrank the
    /// dead row or an automatic sweep dead-names a live worker.
    #[test]
    fn a_held_receipt_blocks_a_dead_registry_row_from_dead_naming_a_live_worker() {
        use std::collections::HashSet;
        let dead_row = |name: &str| crate::agents_view::RegistryAgent {
            name: name.into(),
            harness: Some("codex".into()),
            liveness: crate::agents_view::Liveness::Dead,
            ..Default::default()
        };
        let held = || ["w-keeper".to_string()].into_iter().collect();
        let mut guarded = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        guarded.fold_registry_rows(&[dead_row("w-keeper")], HashSet::new(), held(), true);
        assert!(
            !guarded.is_dead_name("w-keeper"),
            "the held receipt is the live child: no dead name"
        );
        let mut unguarded = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        unguarded.fold_registry_rows(
            &[dead_row("w-keeper")],
            HashSet::new(),
            HashSet::new(),
            true,
        );
        assert!(
            unguarded.is_dead_name("w-keeper"),
            "control: without the receipt the dead row kills the name"
        );
    }

    /// A cascade POSITIVE answer retires the name-only member; a
    /// name the cascade left unresolved, open, or held stays Unknown. The
    /// reuse guard is upstream (the caller folds live identities first),
    /// so this arm retires on positive evidence only.
    #[test]
    fn a_cascade_positive_answer_retires_a_name_only_member() {
        use std::collections::HashSet;
        let member = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("target-x-aaaa-worker".into()),
            harness: Some("claude".into()),
            harness_session_id: None,
            pane_id: None,
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        evidence.add_retire_eligible_name("target-x-aaaa-worker");
        assert_eq!(
            evidence.verdict(&member),
            MemberLiveness::Dead,
            "a done, PR-confirmed node is positive evidence"
        );
        let mut live_guard = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        live_guard.add_retire_eligible_name("target-x-aaaa-worker");
        live_guard.add_live("target-x-aaaa-worker");
        assert_eq!(
            live_guard.verdict(&member),
            MemberLiveness::Live,
            "a live identity outranks the cascade answer"
        );
    }

    /// The Unmeasured expiry: a row the probe never measured whose
    /// last activity is older than the bound contributes its name to the
    /// dead-row candidates under the reuse guard; a fresh or never-active
    /// row, or expiry disabled (0), stays fail-safe Unknown.
    #[test]
    fn the_unmeasured_expiry_folds_a_stale_row_under_the_reuse_guard() {
        use std::collections::HashSet;
        let member = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("w9".into()),
            harness: Some("claude".into()),
            harness_session_id: None,
            pane_id: None,
        };
        let stale = crate::agents_view::RegistryAgent {
            name: "w9".into(),
            liveness: crate::agents_view::Liveness::Unmeasured,
            updated_at: Some(1000),
            ..Default::default()
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        evidence.set_unmeasured_expiry(10_000, 5_000);
        evidence.fold_registry_rows(&[stale.clone()], HashSet::new(), HashSet::new(), true);
        assert_eq!(
            evidence.verdict(&member),
            MemberLiveness::Dead,
            "unmeasured past the bound is dead evidence for the name"
        );
        let fresh = crate::agents_view::RegistryAgent {
            name: "w9".into(),
            liveness: crate::agents_view::Liveness::Unmeasured,
            updated_at: Some(9_000),
            ..Default::default()
        };
        let mut keep = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        keep.set_unmeasured_expiry(10_000, 5_000);
        keep.fold_registry_rows(&[fresh], HashSet::new(), HashSet::new(), true);
        assert_eq!(
            keep.verdict(&member),
            MemberLiveness::Unknown,
            "a fresh unmeasured row stays fail-safe"
        );
        let mut disabled = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        disabled.fold_registry_rows(&[stale], HashSet::new(), HashSet::new(), true);
        assert_eq!(
            disabled.verdict(&member),
            MemberLiveness::Unknown,
            "expiry 0 (the default) keeps the historical fail-safe"
        );
    }

    // The converse the cross-door property pins: a NAME-ONLY member sharing
    // a live session's worker name stays Live. Its registry row reads
    // Unmeasured and stale (the expiry folds the name), but the session's
    // fresh transcript answers LIVE through the pair fold, and the live
    // pair owes its name to the live set - the transcript store, not the
    // row's write age, is the session's activity evidence.
    #[test]
    fn a_live_pair_outranks_the_expiry_folded_name() {
        use std::collections::HashSet;
        let member = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("w9".into()),
            harness: Some("claude".into()),
            harness_session_id: None,
            pane_id: None,
        };
        let stale = crate::agents_view::RegistryAgent {
            name: "w9".into(),
            harness: Some("claude".into()),
            liveness: crate::agents_view::Liveness::Unmeasured,
            updated_at: Some(1000),
            ..Default::default()
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        evidence.set_unmeasured_expiry(10_000, 5_000);
        evidence.fold_registry_rows(&[stale], HashSet::new(), HashSet::new(), true);
        assert_eq!(
            evidence.verdict(&member),
            MemberLiveness::Dead,
            "without the live answer the expiry folds the name"
        );
        evidence.add_live_pair("claude".to_string(), "uuu-1".to_string());
        evidence.add_live("w9".to_string());
        assert_eq!(
            evidence.verdict(&member),
            MemberLiveness::Live,
            "the live session's name outranks the expiry-folded name"
        );
    }

    /// A paired ALIVE row still owes its name to the live set: a
    /// session-less member sharing the name must read Live even when the row
    /// carries no harness session id (the pair path would otherwise strand
    /// it Unknown).
    #[test]
    fn a_paired_alive_row_still_marks_its_name_live() {
        use std::collections::HashSet;
        let row = crate::agents_view::RegistryAgent {
            name: "w4".into(),
            harness: Some("claude".into()),
            session_id: Some("fno-9".into()),
            liveness: crate::agents_view::Liveness::Alive,
            ..Default::default()
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        evidence.fold_registry_rows(&[row], HashSet::new(), HashSet::new(), true);
        let member = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("w4".into()),
            harness: Some("claude".into()),
            harness_session_id: None,
            pane_id: None,
        };
        assert_eq!(
            evidence.verdict(&member),
            MemberLiveness::Live,
            "the paired row's name still reaches the session-less member"
        );
    }

    /// An EXITED row proves its NAME dead (the t-f90d case: the row
    /// carries a session id, so its evidence only ever landed as a pair the
    /// session-less member could not match), and a name both exited and alive
    /// in one read is a reuse, not a death.
    #[test]
    fn an_exited_row_kills_its_name_and_a_reused_name_stays_alive() {
        use std::collections::HashSet;
        let exited = crate::agents_view::RegistryAgent {
            name: "w2".into(),
            harness: Some("claude".into()),
            harness_session_id: Some("6d6d6d6d-1d1d-4d1d-8d1d-1d1d1d1d1d1d".into()),
            exited: true,
            liveness: crate::agents_view::Liveness::Dead,
            ..Default::default()
        };
        let reused_dead = crate::agents_view::RegistryAgent {
            name: "w3".into(),
            harness: Some("claude".into()),
            harness_session_id: Some("7e7e7e7e-2e2e-4e2e-9e2e-2e2e2e2e2e2e".into()),
            exited: true,
            liveness: crate::agents_view::Liveness::Dead,
            ..Default::default()
        };
        let reused_alive = crate::agents_view::RegistryAgent {
            name: "w3".into(),
            liveness: crate::agents_view::Liveness::Alive,
            ..Default::default()
        };
        let mut evidence = MemberEvidence::from_sets(HashSet::new(), HashSet::new());
        evidence.fold_registry_rows(
            &[exited, reused_dead, reused_alive],
            HashSet::new(),
            HashSet::new(),
            true,
        );
        let member = |worker: &str| StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some(worker.into()),
            harness: Some("claude".into()),
            harness_session_id: None,
            pane_id: None,
        };
        assert_eq!(
            evidence.verdict(&member("w2")),
            MemberLiveness::Dead,
            "the exited row's name reaches the session-less member"
        );
        assert_eq!(
            evidence.verdict(&member("w3")),
            MemberLiveness::Live,
            "an alive row of the same name wins: reuse is not death"
        );
    }

    #[test]
    fn remove_and_rename_mutate_by_name() {
        let _s = Scratch::new("remove-rename");
        upsert("a", "", &[], &[m("11111111")]).unwrap();
        upsert("b", "", &[], &[m("22222222")]).unwrap();
        rename("a", "aa", &["/x".into()], &[m("11111111")]).unwrap();
        remove("b", "").unwrap();
        let loaded = load();
        let names: Vec<_> = loaded.squads.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["aa"], "a renamed, b removed");
        assert_eq!(loaded.squads[0].origins, vec!["/x".to_string()]);
    }

    #[test]
    fn unnamed_squad_persists_keyed_by_durable_key() {
        // Operator decision: every squad remains across restart, not only named
        // workspaces. An unnamed squad (empty name - the home squad, a lane) is
        // keyed by its durable KEY, not its origins - so two same-origin unnamed
        // squads stay distinct (the codex P1: two cleared names, or two sessions'
        // home for one repo). Same key -> replace in place; distinct keys with the
        // SAME origins -> two entries; a keyless unnamed squad is not persisted.
        let _s = Scratch::new("unnamed-key");
        upsert("", "k1", &["/repo".into()], &[m("aaaaaaaa")]).unwrap();
        upsert("", "k2", &["/repo".into()], &[m("bbbbbbbb")]).unwrap();
        // Same key -> replace, not duplicate.
        upsert("", "k1", &["/repo".into()], &[m("aaaaaaaa"), m("cccccccc")]).unwrap();
        // No name and no key -> no identity -> skipped silently.
        upsert("", "", &["/repo".into()], &[m("dddddddd")]).unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads.len(),
            2,
            "two same-origin unnamed squads stay distinct by key; the keyless one is skipped"
        );
        let k1 = loaded
            .squads
            .iter()
            .find(|s| s.key == "k1")
            .expect("lane k1 persisted by key");
        assert!(k1.name.is_empty(), "restored unnamed");
        assert_eq!(k1.members.len(), 2, "same-key upsert replaced in place");
        // Remove by key drops exactly that lane, leaving its same-origin sibling.
        remove("", "k1").unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1);
        assert_eq!(loaded.squads[0].key, "k2");
        assert_eq!(loaded.squads[0].origins, vec!["/repo".to_string()]);
    }

    #[test]
    fn origin_key_is_stable_order_independent_and_distinct_per_set() {
        // an unnamed squad's durable key is a pure function of its origin
        // SET, so one repo's home squad derives one key across restarts. Order-
        // independent and duplicate-insensitive (sorted + deduped), distinct per
        // distinct set, and the separator keeps adjacent-path sets apart.
        assert_eq!(
            origin_key(&["/a".into(), "/b".into()]),
            origin_key(&["/b".into(), "/a".into()]),
            "order does not matter"
        );
        assert_eq!(
            origin_key(&["/a".into(), "/a".into()]),
            origin_key(&["/a".into()]),
            "duplicate origins collapse"
        );
        assert_eq!(
            origin_key(&["/a".into()]).len(),
            16,
            "16 hex chars, mint_key shape"
        );
        assert_ne!(
            origin_key(&["/ab".into()]),
            origin_key(&["/a".into(), "b".into()]),
            "separator keeps adjacent sets distinct"
        );
        assert_ne!(
            origin_key(&["/repo".into()]),
            origin_key(&["/other".into()]),
            "distinct origins derive distinct keys"
        );
    }

    #[test]
    fn collapse_duplicate_squads_merges_same_origin_into_one() {
        // AC-HP2: the backlog rows carry distinct random keys (the old
        // mint), so a key-based upsert cannot heal them. collapse groups by
        // origin, rekeys onto origin_key, keeps the membered row, merges every
        // dropped row's members, and leaves exactly one row per origin. A named
        // squad sharing the origin is NOT absorbed (it keys by name).
        let _s = Scratch::new("collapse-same-origin");
        let repo = "/repo/backlog";
        let one = origin_key(&[repo.into()]);
        upsert("", "dead0001", &[repo.into()], &[]).unwrap();
        upsert("", "dead0002", &[repo.into()], &[m("aaaaaaaa")]).unwrap();
        upsert("", "dead0003", &[repo.into()], &[m("bbbbbbbb")]).unwrap();
        // A different origin and a named squad stay separate.
        upsert("", "other1", &["/other".into()], &[]).unwrap();
        upsert("named", "", &[repo.into()], &[m("cccccccc")]).unwrap();

        let dropped = collapse_duplicate_squads().unwrap();
        assert_eq!(dropped, 2, "collapsed the two extra same-origin rows");

        let loaded = load();
        let same_origin: Vec<_> = loaded
            .squads
            .iter()
            .filter(|s| s.name.is_empty() && s.origins == vec![repo.to_string()])
            .collect();
        assert_eq!(same_origin.len(), 1, "one row for the origin");
        assert_eq!(
            same_origin[0].key, one,
            "migrated onto the derived origin key"
        );
        assert_eq!(
            same_origin[0].members.len(),
            2,
            "members merged into the survivor"
        );
        assert_eq!(
            loaded.squads.len(),
            3,
            "named + other-origin + the one collapsed row"
        );
        assert!(
            loaded.squads.iter().any(|s| s.name == "named"),
            "named squad untouched"
        );
    }

    #[test]
    fn collapse_duplicate_squads_is_idempotent() {
        // collapse runs every restore; a second pass must be a no-op
        // (drops nothing) once the store has converged.
        let _s = Scratch::new("collapse-idempotent");
        upsert("", "k1", &["/r".into()], &[m("aaaaaaaa")]).unwrap();
        upsert("", "k2", &["/r".into()], &[]).unwrap();
        assert_eq!(collapse_duplicate_squads().unwrap(), 1);
        assert_eq!(collapse_duplicate_squads().unwrap(), 0, "second pass no-op");
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1);
        assert_eq!(loaded.squads[0].key, origin_key(&["/r".into()]));
    }

    #[test]
    fn collapse_duplicate_squads_surfaces_a_write_error() {
        // AC-ERR1: a collapse write error is not swallowed. A corrupt
        // store makes the locked read fail loud, and collapse returns Err so the
        // caller degrades restore instead of healing silently on one machine.
        let s = Scratch::new("collapse-err");
        std::fs::write(s.file(), "{garbage not json").unwrap();
        assert!(collapse_duplicate_squads().is_err());
    }

    #[test]
    fn collapse_duplicate_named_merges_a_legacy_shared_key_newest_name_wins() {
        // AC5-HP: two named rows sharing one legacy random mint key are one
        // workspace wearing two names (a mint is unique per squad). They
        // collapse to the NEWEST created_at row's name with the union of both
        // member lists. `prune --include-named` cannot reach this pair (the
        // origin still exists), so this collapse is the only heal.
        let s = Scratch::new("collapse-named-legacy");
        let legacy = "25a5abd2af1696a0";
        let origins = vec!["/repo".into()];
        assert_ne!(
            legacy,
            origin_key(&origins),
            "precondition: a legacy mint, not a derived origin key"
        );
        let file = StoreFile {
            version: STORE_VERSION,
            generations: Default::default(),
            next_pane_id: 0,
            squads: vec![
                StoredSquad {
                    name: "f[no]".into(),
                    key: legacy.into(),
                    origins: origins.clone(),
                    members: vec![m("aaaaaaaa"), m("bbbbbbbb")],
                    created_at: "2026-07-23T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
                StoredSquad {
                    name: "fno".into(),
                    key: legacy.into(),
                    origins,
                    members: vec![m("cccccccc")],
                    created_at: "2026-07-26T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
            ],
            external_lifecycle: vec![],
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();

        let dropped = collapse_duplicate_squads().unwrap();
        assert_eq!(dropped, 1, "the older duplicate row is gone");

        let loaded = load();
        let rows: Vec<_> = loaded
            .squads
            .iter()
            .filter(|r| r.name == "fno" || r.name == "f[no]")
            .collect();
        assert_eq!(rows.len(), 1, "one row survives");
        assert_eq!(rows[0].name, "fno", "the newest created_at row's name wins");
        for id in ["aaaaaaaa", "bbbbbbbb", "cccccccc"] {
            assert!(
                rows[0].members.iter().any(|x| x.attach_id == id),
                "member {id} merged into the survivor"
            );
        }
    }

    #[test]
    fn collapse_duplicate_named_spares_same_origin_derived_keys() {
        // AC6-EDGE: two named rows sharing a key that EQUALS origin_key of
        // their own origins are two same-origin squads that each derived it -
        // common key proves nothing, and both rows must survive the heal.
        let s = Scratch::new("collapse-named-derived");
        let key = origin_key(&["/repo".into()]);
        let file = StoreFile {
            version: STORE_VERSION,
            generations: Default::default(),
            next_pane_id: 0,
            squads: vec![
                StoredSquad {
                    name: "one".into(),
                    key: key.clone(),
                    origins: vec!["/repo".into()],
                    members: vec![m("aaaaaaaa")],
                    created_at: "2026-07-23T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
                StoredSquad {
                    name: "two".into(),
                    key,
                    origins: vec!["/repo".into()],
                    members: vec![m("bbbbbbbb")],
                    created_at: "2026-07-26T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
            ],
            external_lifecycle: vec![],
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();

        let dropped = collapse_duplicate_squads().unwrap();
        assert_eq!(dropped, 0, "derived shared keys are not duplicates");
        let loaded = load();
        assert!(loaded.squads.iter().any(|r| r.name == "one"));
        assert!(loaded.squads.iter().any(|r| r.name == "two"));
    }

    #[test]
    fn write_onto_a_corrupt_file_fails_loud_and_never_clobbers() {
        // gemini review: the write path must NOT clobber unreadable content. A
        // corrupt existing file makes upsert fail (Err) rather than overwrite it
        // with just this delta - the load path owns quarantine, not the writer.
        let s = Scratch::new("write-corrupt");
        std::fs::write(s.file(), "{garbage not json").unwrap();
        let before = std::fs::read_to_string(s.file()).unwrap();
        let res = upsert("w", "", &[], &[m("c19cd2c3")]);
        assert!(res.is_err(), "a write onto corrupt content fails loud");
        assert_eq!(
            std::fs::read_to_string(s.file()).unwrap(),
            before,
            "the corrupt file is left intact, not clobbered"
        );
    }

    #[test]
    fn epoch_to_iso_matches_known_stamps() {
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00Z");
        // 2026-07-11T13:00:00Z -> verified against `date -u -j`.
        assert_eq!(epoch_to_iso(1_783_774_800), "2026-07-11T13:00:00Z");
    }

    #[test]
    fn valid_attach_id_gate() {
        assert!(valid_attach_id("c19cd2c3"));
        assert!(!valid_attach_id("c19cd2c")); // 7
        assert!(!valid_attach_id("c19cd2c33")); // 9
        assert!(!valid_attach_id("c19cd2cg")); // non-hex
        assert!(!valid_attach_id(""));
    }

    #[test]
    fn build_tree_marker_detection_is_pure_over_path() {
        // The guard's detector: an ancestor `target` with a cargo marker is a
        // build tree; a coincidental `target` dir without one is not, and a
        // binary with no `target` ancestor (an install) never is. Pure over the
        // path so it needs no env and no real build tree.
        let tmp = std::env::temp_dir().join(format!("fno-guard-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let target = tmp.join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        let exe = target.join("debug").join("fno");

        // No marker -> a bare `target` dir is NOT a build tree.
        assert_eq!(build_tree_target_dir(&exe), None);

        // `.rustc_info.json` is the cargo marker written into target/.
        std::fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        assert_eq!(build_tree_target_dir(&exe), Some(target.clone()));

        // `CACHEDIR.TAG` alone also qualifies (llvm-cov / build tooling).
        std::fs::remove_file(target.join(".rustc_info.json")).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), "x").unwrap();
        assert_eq!(build_tree_target_dir(&exe), Some(target));

        // An installed binary (no `target` ancestor) is never a build tree.
        std::fs::create_dir_all(tmp.join("bin")).unwrap();
        assert_eq!(build_tree_target_dir(&tmp.join("bin").join("fno")), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn build_tree_marker_detection_covers_the_build_dir_layout() {
        // Under build.build-dir (measured 2026-09-10) a test binary lives at
        // <build-base>/<h2>/<hash>/debug/deps/<name>; the markers sit on
        // the hash dir and no ancestor is named `target`. The nearest tagged
        // ancestor is the build tree, whatever its name.
        let tmp = std::env::temp_dir().join(format!("fno-guard-bd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let hash = tmp.join("bd/bac4721f2d16ec");
        let exe = hash.join("debug/deps/probe-0123456789abcdef");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();

        // Untagged -> not a build tree, even with the deps-shaped path.
        assert_eq!(build_tree_target_dir(&exe), None);

        // Tag on the hash dir -> the hash dir IS the build tree.
        std::fs::write(hash.join("CACHEDIR.TAG"), "x").unwrap();
        assert_eq!(build_tree_target_dir(&exe), Some(hash));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn lc(id: &str) -> Option<ExternalLifecycle> {
        load()
            .external_lifecycle
            .into_iter()
            .find(|r| r.attach_id == id)
    }

    #[test]
    fn squad_and_lifecycle_collections_never_drop_each_other() {
        // The version-1 object carries both collections; a squad write must
        // preserve lifecycle records and a lifecycle CAS must preserve squads.
        let _s = Scratch::new("both-collections");
        upsert("w", "", &["/repo".into()], &[m("c19cd2c3")]).unwrap();
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Committed { generation: 1 }
        ));
        // A SECOND squad write must not clobber the lifecycle record.
        upsert("w2", "", &[], &[m("11111111")]).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 2, "both squads survive");
        assert_eq!(
            loaded.external_lifecycle.len(),
            1,
            "lifecycle survives a squad write"
        );
        // And a lifecycle CAS must not clobber the squads.
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 2, "squads survive a lifecycle write");
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopped);
    }

    #[test]
    fn begin_stop_inserts_then_bumps_generation_on_retry() {
        // A LIVE row has no record -> insert at generation 1 (stopping). A retry
        // from a rest state bumps the generation, so a stale completion cannot
        // clobber the newer action.
        let _s = Scratch::new("begin-stop-gen");
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Committed { generation: 1 }
        ));
        // Land it in `failed`, then retry the stop: generation must advance.
        complete_external(
            "deadbeef",
            1,
            ExternalState::Stopping,
            false,
            Some("boom".into()),
        )
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Failed);
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Committed { generation: 2 }
        ));
    }

    #[test]
    fn begin_stop_refused_from_stopped_and_removing() {
        // stop-then-rm: a stopped tombstone is removed, not re-stopped; a row
        // already being removed refuses a concurrent stop.
        let _s = Scratch::new("begin-stop-refused");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap();
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap(); // -> stopped
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Refused(_)
        ));
        begin_external_rm("deadbeef").unwrap(); // -> removing (gen 2)
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Refused(_)
        ));
    }

    #[test]
    fn begin_stop_refused_while_already_stopping() {
        // codex P1: a stop in flight must NOT launch a second `claude stop`. A
        // `stopping` record refuses "already stopping" (only failed/unknown rest
        // states retry) - the generation never advances, so the first
        // completion is never orphaned by a duplicate spawn.
        let _s = Scratch::new("begin-stop-inflight");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen1 Stopping
        match begin_external_stop("deadbeef", "ext", "/tmp").unwrap() {
            LifecycleCas::Refused(r) => assert!(r.contains("already stopping")),
            _ => panic!("a second stop while stopping must refuse"),
        }
        assert_eq!(
            lc("deadbeef").unwrap().generation,
            1,
            "generation must not advance"
        );
    }

    #[test]
    fn begin_rm_requires_a_stopped_record() {
        // rm is reachable ONLY from `stopped` (stop-before-rm). A live/stopping
        // row refuses "stop it first"; an unknown row refuses "retry stop"; an
        // absent id refuses.
        let _s = Scratch::new("begin-rm");
        assert!(matches!(
            begin_external_rm("deadbeef").unwrap(),
            LifecycleCas::Refused(_) // absent
        ));
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // -> stopping
        match begin_external_rm("deadbeef").unwrap() {
            LifecycleCas::Refused(r) => assert!(r.contains("stop it first")),
            _ => panic!("rm on a stopping row must refuse"),
        }
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap(); // -> stopped
        assert!(matches!(
            begin_external_rm("deadbeef").unwrap(),
            LifecycleCas::Committed { generation: 2 }
        ));
    }

    #[test]
    fn complete_external_ignores_a_stale_generation() {
        // A stale retry's late completion (older generation) must never overwrite
        // the newer action - the core anti-clobber invariant.
        let _s = Scratch::new("stale-gen");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen 1, stopping
        complete_external("deadbeef", 1, ExternalState::Stopping, false, None).unwrap(); // -> failed
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen 2, stopping
                                                                 // A gen-1 completion arriving late is ignored; the gen-2 stopping stands.
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopping);
        assert_eq!(lc("deadbeef").unwrap().generation, 2);
    }

    #[test]
    fn complete_rm_deletes_on_ok_and_retains_on_err() {
        let _s = Scratch::new("complete-rm");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap();
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap(); // stopped
        begin_external_rm("deadbeef").unwrap(); // gen 2 removing
                                                // Failure keeps the tombstone stopped (rm stays retryable).
        complete_external(
            "deadbeef",
            2,
            ExternalState::Removing,
            false,
            Some("nope".into()),
        )
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopped);
        begin_external_rm("deadbeef").unwrap(); // gen 3 removing
        complete_external("deadbeef", 3, ExternalState::Removing, true, None).unwrap();
        assert!(
            lc("deadbeef").is_none(),
            "a successful rm deletes the tombstone"
        );
    }

    #[test]
    fn reconcile_lifecycle_leaves_a_generation_advanced_record_untouched() {
        // Lost-update guard (code review): a record a concurrent operator action
        // advanced PAST the reconcile's baseline generation is excluded from the
        // reconcile and left untouched - reconciling it against a pre-action
        // liveness snapshot would drop the action's completion.
        let _s = Scratch::new("reconcile-gen-guard");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen1 Stopping
        complete_external("deadbeef", 1, ExternalState::Stopping, false, None).unwrap(); // gen1 Failed
        let baseline: std::collections::HashMap<String, u64> =
            [("deadbeef".to_string(), 1u64)].into_iter().collect();
        // A concurrent retry advances the record to gen2 BEFORE reconcile applies.
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen2 Stopping
        let notices = reconcile_lifecycle(&baseline, |recs| {
            let n = recs.len();
            let mapped = recs
                .into_iter()
                .map(|mut r| {
                    r.state = ExternalState::Stopped;
                    r
                })
                .collect();
            (mapped, (0..n).map(|_| "reconciled".to_string()).collect())
        })
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopping);
        assert_eq!(lc("deadbeef").unwrap().generation, 2);
        assert!(
            notices.is_empty(),
            "the advanced record was excluded from reconcile"
        );
    }

    #[test]
    fn reconcile_lifecycle_applies_to_a_baseline_matching_record() {
        // The other half: with no concurrent action, a baseline-matching record
        // IS reconciled and its notices flow out.
        let _s = Scratch::new("reconcile-applies");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen1 Stopping
        complete_external("deadbeef", 1, ExternalState::Stopping, false, None).unwrap(); // gen1 Failed
        let baseline: std::collections::HashMap<String, u64> =
            [("deadbeef".to_string(), 1u64)].into_iter().collect();
        let notices = reconcile_lifecycle(&baseline, |recs| {
            let mapped = recs
                .into_iter()
                .map(|mut r| {
                    r.state = ExternalState::Stopped;
                    r
                })
                .collect();
            (mapped, vec!["done".to_string()])
        })
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopped);
        assert_eq!(notices, vec!["done".to_string()]);
    }

    #[test]
    fn load_drops_a_malformed_lifecycle_attach_id() {
        // Boundaries: a malformed attach_id never survives load, so a reconcile
        // or rm can never shell it.
        let s = Scratch::new("bad-lifecycle-id");
        let file = StoreFile {
            version: STORE_VERSION,
            generations: Default::default(),
            external_lifecycle: vec![
                ExternalLifecycle {
                    attach_id: "deadbeef".into(),
                    name: "good".into(),
                    cwd: "/tmp".into(),
                    state: ExternalState::Stopped,
                    generation: 1,
                    updated_at: String::new(),
                    reason: None,
                },
                ExternalLifecycle {
                    attach_id: "; rm -rf".into(), // shell metachar
                    name: "evil".into(),
                    cwd: "/tmp".into(),
                    state: ExternalState::Stopped,
                    generation: 1,
                    updated_at: String::new(),
                    reason: None,
                },
            ],
            ..StoreFile::default()
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert_eq!(loaded.external_lifecycle.len(), 1);
        assert_eq!(loaded.external_lifecycle[0].attach_id, "deadbeef");
        assert!(loaded
            .notice
            .as_deref()
            .unwrap()
            .contains("lifecycle record"));
    }

    #[test]
    fn prune_predicate_matrix() {
        use std::collections::HashSet;
        let live: HashSet<String> = ["live0001".into()].into_iter().collect();
        let live_some = Some(&live);
        let no_cwds: Vec<String> = Vec::new();
        let gone = |_: &str| false; // no origin dir exists
        let exists = |p: &str| p == "/alive";

        let squad = |name: &str, key: &str, origins: &[&str], members: &[&str]| StoredSquad {
            name: name.into(),
            key: key.into(),
            origins: origins.iter().map(|s| (*s).to_string()).collect(),
            members: members.iter().copied().map(m).collect(),
            created_at: String::new(),
            tab_specs: Vec::new(),
            tab_trees: Vec::new(),
            active_tab: None,
        };

        // Named without --include-named -> SkipNamed (AC1-EDGE).
        assert_eq!(
            prune_decision(
                &squad("work", "", &["/g"], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::SkipNamed
        );
        // Named WITH --include-named, gone origin, dead member -> Prune.
        assert_eq!(
            prune_decision(
                &squad("work", "", &["/g"], &["deadbeef"]),
                true,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // Unnamed, gone, dead -> Prune.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // A surviving origin no longer outranks member deadness. This assertion
        // used to expect Keep, and that expectation WAS the defect: every squad
        // origin measured on this machine is a repo root, a directory that never
        // disappears, so the old arm kept 12 of 15 finished squads immortal. A
        // directory existing says nothing about whether anything runs in it.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/alive", "/g"], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &exists
            ),
            PruneDecision::Prune
        );
        // A live member -> Keep.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &["live0001"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Keep
        );
        // Liveness query failed (None) with a non-tombstone member -> KeepUnknown (AC3-FR).
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &["deadbeef"]),
                false,
                None,
                &no_cwds,
                &gone
            ),
            PruneDecision::KeepUnknown
        );
        // Empty members, gone -> Prune (a side-effect squad with nothing live).
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &[]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // Empty origins (no surviving origin), dead -> Prune.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &[], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // All-tombstone members, gone -> Prune (tombstones are not live).
        {
            let mut s = squad("", "k1", &["/g"], &[]);
            s.members = vec![StoredMember {
                attach_id: "deadbeef".into(),
                tombstone: true,
                tombstone_reason: None,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: None,
                harness: None,
                harness_session_id: None,
                pane_id: None,
            }];
            assert_eq!(
                prune_decision(&s, false, live_some, &no_cwds, &gone),
                PruneDecision::Prune
            );
        }
        // A live pane no longer overrides a squad's OWN dead members. The pane
        // is matched by "cwd sits under an origin", and with repo-root origins
        // that is nearly every pane on the machine: one live session kept 9 of
        // the 12 finished squads measured here, two of them with nine dead
        // members each. A member-less squad still gets this protection, since
        // there the pane may BE its unrecorded worker (asserted just below).
        {
            let cwds = vec!["/gone/child".to_string()];
            assert_eq!(
                prune_decision(
                    &squad("", "k1", &["/gone"], &["deadbeef"]),
                    false,
                    live_some,
                    &cwds,
                    &gone
                ),
                PruneDecision::Prune
            );
        }
        // ...but a MEMBER-LESS squad with a live pane under its origin is kept.
        {
            let cwds = vec!["/gone/child".to_string()];
            assert_eq!(
                prune_decision(
                    &squad("", "k1", &["/gone"], &[]),
                    false,
                    live_some,
                    &cwds,
                    &gone
                ),
                PruneDecision::Keep
            );
        }
    }

    // -- The empty-member grace window --------------------------------------
    //
    // Nine of the fifteen squads measured on this machine have ZERO members. A
    // squad mid-recruit and a squad whose members are long gone look identical
    // there, and only the clock separates them, so this is the case most likely
    // to destroy something a person is still using.

    /// A member-less squad stamped `created_at`, for the grace tests.
    fn empty_squad(created_at: &str) -> StoredSquad {
        StoredSquad {
            name: String::new(),
            key: "k1".into(),
            origins: vec!["/alive".into()],
            members: Vec::new(),
            created_at: created_at.into(),
            tab_specs: Vec::new(),
            tab_trees: Vec::new(),
            active_tab: None,
        }
    }

    /// 2026-08-13T12:00:00Z in epoch seconds, from an INDEPENDENT implementation
    /// (`python3 -c "datetime.fromisoformat(...).timestamp()"`), so a wrong
    /// parser cannot agree with itself. The first value written here was wrong
    /// and this assertion is what caught it.
    const T_NOON: i64 = 1_786_622_400;

    #[test]
    fn stamp_parser_matches_a_known_epoch() {
        assert_eq!(parse_stamp_epoch("2026-08-13T12:00:00Z"), Some(T_NOON));
        assert_eq!(parse_stamp_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_stamp_epoch("2000-03-01T00:00:00Z"), Some(951_868_800));
        // Leap day, the case a naive month table gets wrong.
        assert_eq!(
            parse_stamp_epoch("2024-02-29T00:00:00Z"),
            Some(1_709_164_800)
        );
    }

    #[test]
    fn unparseable_stamp_keeps_the_squad() {
        // Cannot age it -> cannot claim it is finished.
        for bad in [
            "",
            "not-a-date",
            "2026-08-13",
            "20260813T120000Z",
            "xxxx-08-13T12:00:00Z",
        ] {
            assert_eq!(parse_stamp_epoch(bad), None, "{bad:?} must not parse");
            assert_eq!(
                prune_decision_at(
                    &empty_squad(bad),
                    false,
                    Some(&std::collections::HashSet::new()),
                    &[],
                    &|_| true,
                    Some(T_NOON),
                ),
                PruneDecision::KeepUnknown,
                "{bad:?} must keep"
            );
        }
    }

    #[test]
    fn a_fresh_member_less_squad_is_never_pruned() {
        let live = std::collections::HashSet::new();
        // Created one second ago: recruiting, not finished.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2026-08-13T11:59:59Z"),
                false,
                Some(&live),
                &[],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::KeepUnknown
        );
    }

    #[test]
    fn the_grace_boundary_keeps_until_strictly_past() {
        let live = std::collections::HashSet::new();
        let decide = |created: &str, now: i64| {
            prune_decision_at(
                &empty_squad(created),
                false,
                Some(&live),
                &[],
                &|_| true,
                Some(now),
            )
        };
        // Exactly at the window: still kept.
        assert_eq!(
            decide("2026-08-13T12:00:00Z", T_NOON + EMPTY_SQUAD_GRACE_SECS),
            PruneDecision::KeepUnknown
        );
        // One second past: prunable.
        assert_eq!(
            decide("2026-08-13T12:00:00Z", T_NOON + EMPTY_SQUAD_GRACE_SECS + 1),
            PruneDecision::Prune
        );
    }

    #[test]
    fn a_clock_we_cannot_read_keeps_every_member_less_squad() {
        // `None` is "no clock". Without one, a fresh recruit and a finished squad
        // are the same thing, so nothing may be destroyed on the guess.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2020-01-01T00:00:00Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| true,
                None,
            ),
            PruneDecision::KeepUnknown
        );
    }

    #[test]
    fn a_vanished_origin_resolves_the_ambiguity_without_waiting() {
        // Nothing left to recruit INTO, so the clock is not needed.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2026-08-13T11:59:59Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| false,
                Some(T_NOON),
            ),
            PruneDecision::Prune
        );
    }

    #[test]
    fn a_tombstoned_member_is_evidence_and_needs_no_grace() {
        // The distinction the grace window turns on. A tombstone RECORDS that a
        // member registered and died; an empty list records nothing. Three of the
        // fifteen squads measured are all-tombstoned, and they are finished now,
        // not in an hour.
        let mut s = empty_squad("2026-08-13T11:59:59Z");
        s.members = vec![StoredMember {
            attach_id: "deadbeef".into(),
            tombstone: true,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }];
        assert_eq!(
            prune_decision_at(
                &s,
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::Prune
        );
    }

    #[test]
    fn grace_never_overrides_liveness_or_the_name_skip() {
        // The window may only ever DELAY a prune. It must not become a path that
        // reaps something live or something the operator named.
        let mut live = std::collections::HashSet::new();
        live.insert("live0001".to_string());
        let mut s = empty_squad("2020-01-01T00:00:00Z"); // long past grace
        s.members = vec![m("live0001")];
        assert_eq!(
            prune_decision_at(&s, false, Some(&live), &[], &|_| true, Some(T_NOON)),
            PruneDecision::Keep
        );

        let named = {
            let mut n = empty_squad("2020-01-01T00:00:00Z");
            n.name = "mine".into();
            n
        };
        assert_eq!(
            prune_decision_at(
                &named,
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::SkipNamed
        );

        // ONE CLOCK FOR BOTH DIRECTORY ARMS.
        //
        // A live pane protects a member-less squad only while it is young enough
        // for that pane to plausibly BE its unrecorded worker. Past grace it does
        // not, and this assertion is the one that changed: the pane arm used to
        // be unconditional, so a squad weeks old stayed immortal while any
        // session ran in the same repo. Measured here, that held six of them.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2020-01-01T00:00:00Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &["/alive/child".to_string()],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::Prune
        );
        // ...and WITHIN grace the same pane still protects it, so the arm is
        // aged, not deleted. Without this pair, "past grace prunes" would also
        // pass against a predicate that ignored panes entirely.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2026-08-13T11:59:00Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &["/alive/child".to_string()],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::Keep
        );
    }

    #[test]
    fn prune_decision_delegates_to_the_clocked_form() {
        // One predicate, two entry points. The clockless wrapper must not drift
        // into a second opinion.
        let live = std::collections::HashSet::new();
        let s = empty_squad("2020-01-01T00:00:00Z");
        assert_eq!(
            prune_decision(&s, false, Some(&live), &[], &|_| true),
            prune_decision_at(&s, false, Some(&live), &[], &|_| true, None)
        );
    }

    #[test]
    fn prune_removes_only_prunable_and_preserves_lifecycle() {
        let _s = Scratch::new("prune");
        upsert("", "dead", &["/gone".into()], &[m("deadbeef")]).unwrap();
        upsert("", "kept", &["/survives".into()], &[m("deadbeef")]).unwrap();
        upsert("named", "", &["/gone".into()], &[m("deadbeef")]).unwrap();
        assert!(matches!(
            begin_external_stop("cafef00d", "x", "/t").unwrap(),
            LifecycleCas::Committed { generation: 1 }
        ));

        let live = std::collections::HashSet::<String>::new(); // nothing live
        let outcome = prune(
            |sq| prune_decision(sq, false, Some(&live), &[], &|p| p == "/survives"),
            Some(&live),
        )
        .unwrap();

        // BOTH unnamed squads go. `kept` has a surviving origin, and that used to
        // save it; a squad whose every member is dead is finished wherever its
        // directory happens to live.
        assert_eq!(
            outcome.removed_count(),
            2,
            "a surviving origin no longer keeps a squad whose members are all dead"
        );
        let mut removed_keys: Vec<&str> = outcome.removed.iter().map(|r| r.key.as_str()).collect();
        removed_keys.sort_unstable();
        assert_eq!(
            removed_keys,
            vec!["dead", "kept"],
            "the receipt names the squads actually removed"
        );
        assert_eq!(
            outcome.skipped_named, 1,
            "the named squad is counted skip-named"
        );

        let after = load();
        assert!(
            !after.squads.iter().any(|s| s.key == "dead"),
            "prunable squad gone"
        );
        assert!(
            !after.squads.iter().any(|s| s.key == "kept"),
            "a surviving origin does not save a squad whose members are all dead"
        );
        assert!(
            after.squads.iter().any(|s| s.name == "named"),
            "named squad kept"
        );
        assert_eq!(
            after.external_lifecycle.len(),
            1,
            "external_lifecycle preserved byte-for-byte across a prune"
        );
    }

    fn tomb(id: &str) -> StoredMember {
        StoredMember {
            attach_id: id.into(),
            tombstone: true,
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

    fn worker_member(name: &str) -> StoredMember {
        StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some(name.into()),
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }
    }

    #[test]
    fn member_reap_drops_dead_worker_members_beside_a_live_member() {
        // AC1-HP: a live member keeps its squad, while worker members with
        // positive dead evidence are removed from that surviving squad.
        let _s = Scratch::new("member-reap-worker-hp");
        upsert(
            "",
            "keeper",
            &[],
            &[
                m("11111111"),
                worker_member("dead-worker-one"),
                worker_member("dead-worker-two"),
            ],
        )
        .unwrap();

        let mut live = std::collections::HashSet::new();
        live.insert("11111111".to_string());
        let outcome = prune(
            |sq| prune_decision(sq, false, Some(&live), &[], &|_| true),
            Some(&live),
        )
        .unwrap();

        assert_eq!(
            outcome.removed_count(),
            0,
            "the live member keeps the squad"
        );
        assert_eq!(outcome.members_reaped, 2);
        let after = load();
        assert_eq!(after.squads.len(), 1);
        assert_eq!(after.squads[0].members.len(), 1);
        assert_eq!(after.squads[0].members[0].attach_id, "11111111");
    }

    #[test]
    fn member_reap_with_evidence_keeps_an_unmeasured_worker() {
        // AC1-ERR: a missing identity verdict is retained, while an exact
        // terminal verdict in the same surviving squad is reaped.
        let _s = Scratch::new("member-reap-worker-unknown");
        upsert(
            "",
            "keeper",
            &[],
            &[
                m("11111111"),
                worker_member("dead-worker"),
                worker_member("unmeasured-worker"),
            ],
        )
        .unwrap();

        let live = ["11111111".to_string()].into_iter().collect();
        let dead = ["dead-worker".to_string()].into_iter().collect();
        let evidence = MemberEvidence::from_sets(live, dead);
        let outcome = prune_with_evidence(
            |sq| prune_decision_with_evidence(sq, false, &evidence, &[], &|_| true, None),
            &evidence,
        )
        .unwrap();

        assert_eq!(outcome.removed_count(), 0);
        assert_eq!(outcome.members_reaped, 1);
        assert_eq!(outcome.members_kept_live, 1);
        assert_eq!(outcome.members_kept_unknown, 1);
        let after = load();
        assert!(after.squads[0]
            .members
            .iter()
            .any(|m| m.worker.as_deref() == Some("unmeasured-worker")));
    }

    #[test]
    fn member_reap_drops_tombstoned_members_a_live_member_keeps_the_squad_alive() {
        // AC4-HP: a squad kept by one live member still has its tombstoned
        // members reaped in the same pass, so a dead worker beside a live one
        // does not synthesize a `cc-` row forever.
        let _s = Scratch::new("member-reap-hp");
        upsert(
            "",
            "keeper",
            &[],
            &[m("11111111"), tomb("deadbee1"), tomb("deadbee2")],
        )
        .unwrap();

        let mut live = std::collections::HashSet::new();
        live.insert("11111111".to_string());
        let outcome = prune(
            |sq| prune_decision(sq, false, Some(&live), &[], &|_| true),
            Some(&live),
        )
        .unwrap();

        assert_eq!(outcome.removed_count(), 0, "the squad itself survives");
        assert_eq!(outcome.members_reaped, 2);
        let after = load();
        assert_eq!(after.squads.len(), 1);
        assert_eq!(after.squads[0].members.len(), 1);
        assert_eq!(after.squads[0].members[0].attach_id, "11111111");
    }

    #[test]
    fn member_reap_does_nothing_when_liveness_is_unknown() {
        // AC4-EDGE: the liveness query failed (`live` is `None`). No member is
        // removed - same fail-safe direction `KeepUnknown` takes for whole
        // squads.
        let _s = Scratch::new("member-reap-edge");
        upsert(
            "",
            "keeper",
            &[],
            &[m("11111111"), tomb("deadbee1"), tomb("deadbee2")],
        )
        .unwrap();

        let outcome = prune(|sq| prune_decision(sq, false, None, &[], &|_| true), None).unwrap();

        assert_eq!(outcome.removed_count(), 0);
        assert_eq!(outcome.members_reaped, 0, "an unknown roster reaps nothing");
        let after = load();
        assert_eq!(after.squads[0].members.len(), 3);
    }

    #[test]
    fn member_reap_to_zero_does_not_get_double_pruned_in_the_same_pass() {
        // AC4-COV: a NAMED squad skipped by `include_named` policy still has
        // its tombstoned members reaped, and that can reap it to zero members
        // in this call. `decide` already ran (SkipNamed) against the ORIGINAL
        // member list before the reap, so this pass must not turn around and
        // treat the now-empty squad as prunable under the empty-squad grace
        // arm - that only happens on a LATER call, against fresh state.
        let _s = Scratch::new("member-reap-cov");
        upsert(
            "archived-crew",
            "",
            &[],
            &[tomb("deadbee1"), tomb("deadbee2")],
        )
        .unwrap();

        let live = std::collections::HashSet::<String>::new(); // nothing live
        let outcome = prune(
            |sq| prune_decision(sq, false, Some(&live), &[], &|_| true),
            Some(&live),
        )
        .unwrap();

        assert_eq!(
            outcome.removed_count(),
            0,
            "SkipNamed never proposes the squad for removal"
        );
        assert_eq!(outcome.skipped_named, 1);
        assert_eq!(outcome.members_reaped, 2);
        let after = load();
        assert_eq!(
            after.squads.len(),
            1,
            "the squad row survives with zero members, not pruned in this pass"
        );
        assert!(after.squads[0].members.is_empty());
    }

    #[test]
    fn slot_pane_id_and_portal_row_facts_roundtrip() {
        // The restart join fields: a leaf's birth pane id and a portal
        // slot's row harness + FULL session id ride the store additively
        // (STORE_VERSION unchanged) and survive a write/load cycle whole.
        let tree = StoredTabTree {
            tab_name: Some("join".into()),
            tree: crate::proto::LayoutTreeSpec::Slot("p1".into()),
            slots: vec![crate::proto::LayoutSlot {
                name: "p1".into(),
                binding: crate::proto::LayoutBinding::Shell,
                cwd: Some("/repo".into()),
                portal: Some(crate::proto::PortalSlot {
                    index: 2,
                    row: "deadbee1".into(),
                    harness: Some("codex".into()),
                    session_id: Some("01a0f1ce-1111-4c1e-8a1c-2d3e4f5a6b7c".into()),
                }),
                pane_id: Some(42),
            }],
            focus: Some("p1".into()),
        };
        set_tab_trees("join-squad", "k-join", &["/repo".into()], &[tree.clone()], Some(0)).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads[0].tab_trees, vec![tree], "round-trips whole");
    }
