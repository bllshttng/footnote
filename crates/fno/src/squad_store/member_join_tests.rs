//! The declared member-to-row join tests. Moved verbatim out of
//! squad_store.rs (file budget shrink). Parent items resolve through
//! the glob.
use super::*;

#[test]
fn member_joins_row_prefers_the_full_session_id_then_the_short_id() {
    //  The declared join, both keys: a member carrying a full
    // harness session id joins on it and ignores the short ids; a member
    // without one joins by attach id against the row's short id; an
    // id-less member joins nothing.
    let m_session = StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: None,
        harness: Some("claude".into()),
        harness_session_id: Some("sess-full".into()),
        pane_id: None,
    };
    assert!(member_joins_row(
        &m_session,
        Some("other-short"),
        Some("sess-full")
    ));
    assert!(!member_joins_row(
        &m_session,
        Some("sess-full"),
        Some("other-session")
    ));
    let m_short = StoredMember {
        attach_id: "abc12345".into(),
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
    assert!(member_joins_row(&m_short, Some("abc12345"), None));
    assert!(!member_joins_row(&m_short, None, Some("abc12345")));
    assert!(!member_joins_row(&m_short, Some("zzzzzzzz"), None));
    let m_bare = StoredMember {
        attach_id: String::new(),
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
    assert!(!member_joins_row(&m_bare, Some("abc12345"), Some("sess")));
}
