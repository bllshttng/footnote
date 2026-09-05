//! The wire-generation pin: the one canonical PROTO_VERSION /
//! MIN_COMPAT_PROTO assertions and the additive-decode proof. A child
//! module of the tests mod so the shrink-only budget on proto.rs stays
//! honest; everything arrives through `super::*`.
use super::*;

#[test]
fn proto_version_match_is_accepted() {
    assert!(check_attach_version(PROTO_VERSION, BUILD_VERSION).is_ok());
}

#[test]
fn compatible_proto_versions_are_accepted() {
    for version in [
        MIN_COMPAT_PROTO,
        MIN_COMPAT_PROTO + 1,
        PROTO_VERSION,
        PROTO_VERSION + 1,
    ] {
        assert!(
            check_attach_version(version, BUILD_VERSION).is_ok(),
            "compatible version {version} should attach"
        );
    }
}

#[test]
fn proto_version_below_floor_names_both_versions() {
    let err = check_attach_version(MIN_COMPAT_PROTO - 1, "9.9.9").unwrap_err();
    assert!(err.contains("9.9.9"), "{err}");
    assert!(
        err.contains(&format!("v{}", MIN_COMPAT_PROTO - 1)),
        "client proto version missing: {err}"
    );
    assert!(
        err.contains(&format!("v{PROTO_VERSION}")),
        "server proto version missing: {err}"
    );
    assert!(err.contains(BUILD_VERSION), "server build missing: {err}");
}

#[test]
fn thread_pane_placement_field_is_additive() {
    // (x-9b60, AC6-REG) A client built before v66 sends a ThreadPane
    // carrying only `name` and `portal`: it decodes to the default
    // placement and behaves exactly as v65 did. The default round-trips
    // absent, so the wire shape of every existing caller is unchanged.
    let pre_v66 = r#"{"ThreadPane":{"name":"w2","portal":1}}"#;
    let verb: ControlVerb = serde_json::from_str(pre_v66).unwrap();
    match verb {
        ControlVerb::ThreadPane {
            name,
            portal,
            placement,
        } => {
            assert_eq!(name, "w2");
            assert_eq!(portal, Some(1));
            assert_eq!(placement, PanePlacement::default());
        }
        other => panic!("wrong variant: {other:?}"),
    }
    // The new field round-trips when present.
    let verb = ControlVerb::ThreadPane {
        name: "w2".into(),
        portal: Some(1),
        placement: PanePlacement {
            tab: Some(TabSel::Id(2)),
            split: Some(Dir::Right),
            ..Default::default()
        },
    };
    let back: ControlVerb = serde_json::from_str(&serde_json::to_string(&verb).unwrap()).unwrap();
    assert_eq!(verb, back);
}

#[test]
fn thread_reseat_verb_decodes_both_portal_forms() {
    // (v69) A pre-v69 client never sends ThreadReseat; the verb is NEW, so
    // the pin is its wire shape: the bare pane form (portal defaults) and
    // the explicit index form must both decode and round-trip.
    let bare: ControlVerb = serde_json::from_str(r#"{"ThreadReseat":{"pane":42}}"#).unwrap();
    match bare {
        ControlVerb::ThreadReseat { pane, portal } => {
            assert_eq!(pane, 42);
            assert_eq!(portal, None, "an absent index takes the next free portal");
        }
        other => panic!("wrong variant: {other:?}"),
    }
    let verb = ControlVerb::ThreadReseat {
        pane: 42,
        portal: Some(3),
    };
    let back: ControlVerb = serde_json::from_str(&serde_json::to_string(&verb).unwrap()).unwrap();
    assert_eq!(verb, back);
}
