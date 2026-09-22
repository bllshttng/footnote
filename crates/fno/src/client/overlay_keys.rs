//! The overlay precedence chain - the one list deciding which surface owns
//! the client keyboard - moved out of `client.rs` `handle_stdin` for the
//! shrink-only ratchet, plus the quiet-window flush that releases a lone
//! ESC carry to the overlay on top.

use super::{
    answer_keys, attach_place_keys, confirm_keys, connections_keys, create_keys, is_sideline_verb,
    keys_modal_keys, move_pick_keys, move_to_keys, nav_keys, peek_keys, portal_pick_keys,
    recruit_keys, rename_keys, row_menu_keys, search_keys, selector_keys, yard_keys, StdinFlow,
    View,
};
use super::{aux_keys, node_detail, sideline};

/// Route one stdin chunk to the overlay that owns the keyboard, in
/// precedence order. `None` when no overlay owns it: the caller falls
/// through to the chord scanner. An empty `bytes` is the quiet-window
/// flush; overlays with no esc carry (digest, confirm, move-to, the
/// hover-armed selector, the launcher dock) are unchanged by it - they act
/// on real bytes only - and the overlays with a carry release a lone ESC
/// as their Esc action through their fold's empty-read arm.
pub(super) async fn route(
    view: &mut View,
    scanner: &mut crate::keys::Scanner,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Option<Result<StdinFlow, String>> {
    if view.digest.is_some() {
        // any key dismisses the catch-up digest into the normal view.
        // Same whole-chunk swallow as the key-table overlay below. A flush
        // is not a key: leave the digest up.
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        view.digest = None;
        return Some(Ok(StdinFlow::Continue));
    }
    if view.keys_modal.is_some() {
        // US3 which-key: a bound key executes through the shared dispatch,
        // arrows/pgup scroll+select, Enter runs the selected row, Esc/unbound
        // dismiss. Routed here (same precedence as the old poster) so its keys
        // never leak to a pane.
        return Some(keys_modal_keys(view, scanner, bytes, sock_w).await);
    }
    if view.row_menu.is_some() {
        // US2: the row context menu consumes keys while open (arrows walk
        // the entries + grid, Enter runs, Esc/q close) - never leaks to a pane.
        return Some(row_menu_keys(view, bytes, sock_w).await);
    }
    if view.aux.is_some() {
        // US4/US5: the MENU popup / settings modal consumes keys.
        return Some(aux_keys(view, bytes, sock_w).await);
    }
    if view.connections.is_some() {
        // the Connections modal consumes all keys while open (Tab
        // switches tabs, j/k move, R refreshes, Esc closes) - never leaks to a
        // pane. Routed here (top-level modal, like the MENU it opened from).
        return Some(connections_keys(view, bytes, sock_w).await);
    }
    if view.confirm.is_some() {
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        return Some(confirm_keys(view, bytes, sock_w).await);
    }
    if view.move_pick.is_some() {
        // Modal like confirm: a single digit/Esc resolves it. Ahead of
        // the selector (which it replaced on open) so its keys can't leak there.
        return Some(move_pick_keys(view, bytes, sock_w).await);
    }
    if view.attach_place.is_some() {
        return Some(attach_place_keys(view, bytes, sock_w).await);
    }
    if view.portal_pick.is_some() {
        // The portal picker consumes keys while open, ahead of the
        // selector it replaced - same precedence slot as its sibling.
        return Some(portal_pick_keys(view, bytes, sock_w).await);
    }
    if view.node_detail.is_some() {
        // Node detail (Enter on a card): peek's precedence slot - its keys
        // never leak to the selector underneath; Esc drops back one layer.
        return Some(node_detail::detail_keys(view, bytes, sock_w).await);
    }
    if view.peek.is_some() {
        // peek sits ON TOP of the selector; routed BEFORE it so its keys
        // (j/k, Esc, later digit/attach) never leak to the selector underneath.
        return Some(peek_keys(view, bytes, sock_w).await);
    }
    // A hover-armed selector is motion-fresh: only the action-verb set
    // acts on the pointed-at row; the first key OUTSIDE it disarms the arm and
    // falls through to the pane, so a pointer parked over the sideline never
    // swallows typing into the focused shell (AC2-EDGE). An explicitly-opened
    // selector (sel_hover_armed=false) stays fully modal below.
    if view.selector.is_some() && view.sel_hover_armed {
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        if bytes.first().is_some_and(|&b| is_sideline_verb(b)) {
            return Some(selector_keys(view, bytes, sock_w).await);
        }
        view.selector = None;
        view.sel_hover_armed = false;
        // fall through: forward this chunk to the focused pane.
    }
    if view.selector.is_some() {
        return Some(selector_keys(view, bytes, sock_w).await);
    }
    if view.answers.is_some() {
        return Some(answer_keys(view, bytes, sock_w).await);
    }
    if view.yard.is_some() {
        return Some(yard_keys(view, bytes, sock_w).await);
    }
    // The feed panel is chrome and consumes no keys UNTIL the
    // operator focuses it with `E`, or opens a row's provenance. Both are
    // explicit, and both release back to the pane on Esc, so the property
    // this slot protects - typing reaches the focused pane - holds by
    // default and is set aside only on request.
    if view.feed_detail_of.is_some() || view.feed.as_ref().is_some_and(|f| f.focused) {
        return Some(super::feed_view::feed_keys(view, bytes, sock_w).await);
    }
    if view.create.is_some() {
        return Some(create_keys(view, bytes, sock_w).await);
    }
    if view.rename.is_some() {
        // Same precedence slot as create_keys: AFTER selector/answers, so a
        // lingering overlay never swallows the typed name (finding).
        return Some(rename_keys(view, bytes, sock_w).await);
    }
    if view.move_to.is_some() {
        // Same precedence slot as the rename overlay it mirrors. No esc
        // carry: the first byte decides the prompt, so a flush is a no-op.
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        return Some(move_to_keys(view, bytes, sock_w).await);
    }
    if view.recruit.is_some() {
        return Some(recruit_keys(view, bytes, sock_w).await);
    }
    if view.search.is_some() {
        return Some(search_keys(view, bytes, sock_w).await);
    }
    if view.nav.is_some() {
        return Some(nav_keys(view, bytes, sock_w).await);
    }
    if view.launcher.is_some() {
        // The composer is a modal like every other: prefix chords still
        // resolve while it holds the keyboard (which-key parity), so the
        // chunk scans first and only the plain-byte chunks feed the
        // composer's own folder. Nothing here reaches a pane. Its quiet
        // window is the scanner's, upstream: a flush chunk scans to
        // nothing and must not reach the dock's folder.
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        return Some(sideline::route_launcher_keys(view, scanner, bytes, sock_w).await);
    }
    None
}

/// The quiet window elapsed while a raw-fed overlay may hold a lone ESC in
/// its carry: route an empty read through the overlay chain so the owning
/// fold releases it as the Esc action. No overlay open: nothing to do.
pub(super) async fn flush_lone_esc(
    view: &mut View,
    scanner: &mut crate::keys::Scanner,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    match route(view, scanner, &[], sock_w).await {
        Some(flow) => flow,
        None => Ok(StdinFlow::Continue),
    }
}
