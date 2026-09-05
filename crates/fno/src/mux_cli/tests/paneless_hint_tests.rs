//! The paneless route-hint test, extracted from the parent's inline
//! tests under the file-budget gate (the reseat verb lands in a sibling
//! child module and the parent stays shrink-only).
use super::*;

    fn paneless_row(name: &str, attach: Option<&str>) -> crate::agents_view::RegistryAgent {
        paneless_row_with_harness(name, attach, None)
    }

    fn paneless_row_with_harness(
        name: &str,
        attach: Option<&str>,
        harness: Option<&str>,
    ) -> crate::agents_view::RegistryAgent {
        crate::agents_view::RegistryAgent {
            name: name.into(),
            cwd: "/tmp/seen".into(),
            attach_id: attach.map(str::to_owned),
            harness: harness.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn paneless_route_hint_names_both_routes_for_a_drive_tier_row() {
        // AC9-HP: the exit-17 line a paneless row prints names the peek route
        // every row has AND the drive route a live attach-carrying row has -
        // never the bare "hosts no live pane" the incident hit. `where`,
        // `view`, and `focus` share this one builder, so one assertion covers
        // all three doors.
        let drive = paneless_route_hint("fno mux where", &paneless_row("t-live", Some("deadbee1")));
        assert!(drive.contains("fno agents peek t-live --follow"), "{drive}");
        assert!(drive.contains("fno agents attach t-live"), "{drive}");
        assert!(drive.contains("hosts no live pane"), "{drive}");

        // Follow tier (a peek-capable harness, no attach id): the peek route
        // only, still not the bare line.
        let follow = paneless_route_hint(
            "fno mux where",
            &paneless_row_with_harness("t-codex", None, Some("codex")),
        );
        assert!(
            follow.contains("fno agents peek t-codex --follow"),
            "{follow}"
        );
        assert!(!follow.contains("fno agents attach"), "{follow}");
        assert!(follow.contains("hosts no live pane"), "{follow}");

        // Locate tier (no attach id, no peek reader - e.g. gemini): peek
        // --follow is a route guaranteed to fail there, so the hint must
        // name attach instead, never peek.
        let locate = paneless_route_hint(
            "fno mux where",
            &paneless_row_with_harness("t-gemini", None, Some("gemini")),
        );
        assert!(locate.contains("fno agents attach t-gemini"), "{locate}");
        assert!(!locate.contains("--follow"), "{locate}");
        assert!(locate.contains("hosts no live pane"), "{locate}");
    }
