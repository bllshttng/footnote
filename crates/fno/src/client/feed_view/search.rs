//! The feed's search surface: the one shared grammar (`crate::search_query`)
//! parsed once per gesture; the pushable half travels as projection flags,
//! the rest is matched client-side over [`event_fields`]. The keys overlay
//! and the search bar's completion read the same shared table, so the feed
//! never carries its own copy of the grammar.

use crate::feed_overlay::FeedFilter;

/// The flat flags one parsed query pushes into the projection, `None` when
/// any term falls outside the pushable keys. The projection then receives
/// nothing at all, and the client matches every landed page itself - the one
/// rule that keeps a half-pushed query from lying about its scope.
pub(crate) fn prefilter(q: &crate::search_query::Parsed) -> Option<FeedFilter> {
    let pd = q.pushdown()?;
    let mut f = FeedFilter::default();
    for (key, vals) in pd.vals {
        let joined = vals.join(",");
        match key {
            "id" => f.node = Some(joined),
            "kind" => f.kind = Some(joined),
            "area" => f.area = Some(joined),
            "session" => f.session = Some(joined),
            "agent" => f.agent = Some(joined),
            "harness" => f.harness = Some(joined),
            "lead" => f.lead = Some(joined),
            // A key with no projection flag is not pushable; pushdown
            // already refuses it, so this is the belt under the belt.
            _ => return None,
        }
    }
    if let Some((lo, hi)) = pd.ts {
        f.since = lo.map(|v| v.max(0) as u64);
        f.until = hi.map(|v| v.max(0) as u64);
    }
    Some(f)
}

/// Parse one query for the feed surface; `Err` carries the shared refusal
/// (an unknown key, a node-only key) verbatim for the footer.
pub(crate) fn parse_query(text: &str) -> Result<crate::search_query::Parsed, String> {
    crate::search_query::parse(
        text,
        crate::search_query::Surface::Event,
        crate::search_query::now_secs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_overlay::{event_fields, EventCtx, FeedItem};

    fn row(ts: &str, kind: &str, node: Option<&str>, session: Option<&str>) -> FeedItem {
        FeedItem {
            ts: ts.to_string(),
            kind: kind.to_string(),
            node: node.map(str::to_string),
            session_id: session.map(str::to_string),
            title: format!("{kind} on {:?}", node.unwrap_or("-")),
            ..Default::default()
        }
    }

    fn fields(items: &[FeedItem]) -> Vec<crate::search_query::Fields> {
        let ctx = EventCtx::from_rows(&[], &[]);
        items.iter().map(|i| event_fields(i, &ctx)).collect()
    }

    fn keeps(q: &str, f: &crate::search_query::Fields) -> bool {
        match crate::search_query::parse(q, crate::search_query::Surface::Event, 1_791_854_400) {
            Ok(p) => p.keeps(f),
            Err(e) => panic!("{q} refused: {e}"),
        }
    }

    #[test]
    fn feed_row_cases_through_the_shared_matcher() {
        let items = vec![
            row("2026-10-01T08:00:00Z", "node_started", Some("x-1234"), None),
            row("2026-10-01T09:00:00Z", "node_started", Some("x-abcd"), None),
            row(
                "2026-10-01T10:00:00Z",
                "question_closed",
                None,
                Some("00bde302-e6cc-4d75-a393-4208778b8314"),
            ),
            row("2026-10-01T11:00:00Z", "session_spawned", None, None),
            row("2026-10-01T12:00:00Z", "node_started", Some("x-1"), None),
            row("2026-10-01T13:00:00Z", "pr_merged", None, None),
            row("2026-09-30T23:00:00Z", "node_started", Some("x-old"), None),
        ];
        // Per-row tweaks the case table names.
        let mut items = items;
        items[3].harness = Some("claude".into());
        items[4].harness = Some("codex".into());
        items[4].title = "stall on x-1".into();
        items[5].harness = Some("claude".into());
        items[6].harness = Some("claude".into());
        let fs = fields(&items);

        assert!(keeps("x-1234", &fs[0]), "a bare node id keeps its row");
        assert!(!keeps("x-1234", &fs[1]), "a bare id is exact, not fuzzy");
        assert!(keeps("id:x-1234,x-abcd", &fs[1]), "comma OR inside a key");
        assert!(!keeps("id:x-1234", &fs[1]), "the other id drops");
        assert!(keeps("k:question", &fs[2]), "kind matches by prefix");
        assert!(
            keeps("-k:question h:claude", &fs[3]),
            "negation plus harness keeps the spawned row"
        );
        assert!(
            !keeps("-k:question h:claude", &fs[2]),
            "the question row drops under the negation"
        );
        assert!(
            keeps("h:codex k:node stall", &fs[4]),
            "keyed terms AND with the bare word"
        );
        assert!(
            keeps("h:codex | h:claude k:pr", &fs[5]),
            "the OR group keeps the merged row"
        );
        assert!(
            keeps("sid:00bde302", &fs[2]),
            "a session tail matches by prefix"
        );
        assert!(
            !keeps("ts:>=2026-10-01", &fs[6]),
            "the row before the date drops"
        );
        assert!(
            keeps("ts:>=2026-10-01", &fs[0]),
            "the row after the date keeps"
        );
    }

    #[test]
    fn node_only_key_refuses_on_the_feed_surface() {
        // AC8-EDGE: the shared refusal names `s:` as node-only, and the
        // caller arms no fetch from a refused parse.
        let err = crate::search_query::parse(
            "s:ready",
            crate::search_query::Surface::Event,
            1_791_854_400,
        )
        .expect_err("s: refuses on events");
        assert!(err.contains("s:"), "the refusal names the key: {err}");
    }

    #[test]
    fn prefilter_cases() {
        let pf = |q: &str| prefilter(&parse_query(q).unwrap());
        // h:codex k:question -> --harness codex --kind question
        let f = pf("h:codex k:question").expect("one positive group pushes");
        assert_eq!(f.harness.as_deref(), Some("codex"));
        assert_eq!(f.kind.as_deref(), Some("question"));
        // Two groups push nothing.
        assert!(pf("id:x-1 | k:pane").is_none(), "two groups push none");
        // A negation pushes nothing.
        assert!(pf("-k:question").is_none(), "negation pushes none");
        // Free text pushes nothing.
        assert!(pf("stall").is_none(), "bare words push none");
        // A date pushes the epoch bounds.
        let f = pf("ts:>=2026-10-01").expect("a date pushes");
        assert!(f.since.is_some());
        assert_eq!(f.until, None);
        // A bare node id pushes --node.
        let f = pf("x-1234").expect("an id-shaped bare word pushes");
        assert_eq!(f.node.as_deref(), Some("x-1234"));
    }
}
