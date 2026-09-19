//! The node detail overlay: one backlog card's node read large - its
//! sessions with a launch action per row, its plan, its king, and the
//! fields that act as comments on it (progress notes, decisions). Enter on
//! a card opens it; Esc closes.
//!
//! The fold copies [`crate::feed_overlay`]'s shape: a bounded, fail-open
//! shell-out to `fno backlog get --json` off the UI loop, a typed error
//! instead of a bare `None` (which failure fired is the whole point of
//! rendering one), and the generation/single-flight discipline
//! [`super::feed_view`] runs - one fold in flight, a result that lands
//! after a close or a re-open is dropped by generation.
//!
//! Every launch routes through the agents section's own hit cascade
//! (`agent_hit`): focus the pane, else attach, else resume, else the
//! refusal is the notice. The overlay derives WHICH of those a row can do
//! and says why a row can do none - it never invents a route.

use super::*;
use serde::Deserialize;
use std::time::Duration;

/// Ten seconds, the feed/court budget for a comparable store read. The fold
/// runs off the UI loop, one at a time, and `kill_on_drop` reaps the child.
const SHELLOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// One session row of a node, from `fno backlog get --json`'s `sessions[]`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NodeSession {
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub observed_model: Option<ObservedModel>,
}

/// The model observed for a session AT EVENT TIME (`observed_model.model`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ObservedModel {
    #[serde(default)]
    pub model: Option<String>,
}

/// One progress note (a machine-written comment on the node). The canonical
/// writer stores `{ts, text}`; `body` rides as the alias.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NodeNote {
    #[serde(rename = "text", alias = "body", default)]
    pub body: String,
}

/// One decision reference on the node.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NodeDecision {
    #[serde(default)]
    pub decision_id: String,
}

/// The node record the overlay renders. Only the fields the pane shows are
/// decoded; everything else on the record is ignored, so a store-side field
/// addition never breaks the fold.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NodeDetail {
    pub id: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub plan_path: Option<String>,
    /// The session id holding the node's live claim, per the store. A
    /// session row whose id matches reads basis `claim`; every other row is
    /// in the detail because the graph's `sessions[]` says so.
    #[serde(default)]
    pub locked_by_harness_session: Option<String>,
    #[serde(default)]
    pub sessions: Vec<NodeSession>,
    #[serde(default)]
    pub progress_notes: Vec<NodeNote>,
    #[serde(default)]
    pub decisions: Vec<NodeDecision>,
}

/// Why a detail fold failed. The four variants are the feed's four causes;
/// the noun names THIS shell-out so a rendered line never says "feed" about
/// a backlog read.
#[derive(Debug, Clone)]
pub(crate) enum NodeDetailError {
    Timeout,
    Spawn(String),
    Exit(String),
    Malformed(String),
}

impl std::fmt::Display for NodeDetailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeDetailError::Timeout => {
                write!(
                    f,
                    "node read timed out after {}s",
                    SHELLOUT_TIMEOUT.as_secs()
                )
            }
            NodeDetailError::Spawn(e) => write!(f, "node read could not start: {e}"),
            NodeDetailError::Exit(stderr) => write!(f, "node read failed: {stderr}"),
            NodeDetailError::Malformed(e) => write!(f, "node read returned malformed JSON: {e}"),
        }
    }
}

pub(crate) type FoldResult = Result<NodeDetail, NodeDetailError>;

/// Run the node read. `fno backlog get <id>` prints the record as one JSON
/// object; a failure carries its typed reason.
pub(crate) async fn detail_now(id: &str) -> FoldResult {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args(["backlog", "get", id])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .map_err(|_| NodeDetailError::Timeout)?
        .map_err(|e| NodeDetailError::Spawn(e.to_string()))?;
    if !output.status.success() {
        let stderr = std::str::from_utf8(&output.stderr)
            .ok()
            .and_then(|t| t.lines().next())
            .filter(|l| !l.is_empty())
            .unwrap_or("no stderr");
        return Err(NodeDetailError::Exit(stderr.chars().take(160).collect()));
    }
    parse_detail(&output.stdout)
}

fn parse_detail(stdout: &[u8]) -> FoldResult {
    let mut doc: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| NodeDetailError::Malformed(e.to_string()))?;
    // The canonical list deserializers skip non-object rows (obj_list!);
    // the overlay matches that tolerance instead of failing the whole fold
    // on one legacy string entry.
    for key in ["sessions", "progress_notes", "decisions"] {
        if let Some(rows) = doc.get_mut(key).and_then(|v| v.as_array_mut()) {
            rows.retain(|r| r.is_object());
        }
    }
    serde_json::from_value(doc).map_err(|e| NodeDetailError::Malformed(e.to_string()))
}

/// The overlay's open state: the node id it was opened for, the last fold's
/// record (`None` until the first fold lands, so a slow read never paints a
/// fabricated empty pane), the typed failure, and the same
/// generation/single-flight discipline the feed panel runs. `sel` indexes
/// the SESSION ROWS (display order), never raw lines.
pub(crate) struct NodeDetailOverlay {
    pub(crate) node_id: String,
    pub(crate) detail: Option<NodeDetail>,
    pub(crate) error: Option<NodeDetailError>,
    pub(crate) inflight: bool,
    pub(crate) want: bool,
    pub(crate) gen: u64,
    pub(crate) sel: usize,
}

/// Open (or re-focus) the overlay for a node. A re-open of the SAME node
/// keeps the prior record as instant content and re-arms the fold - history
/// may have moved. A DIFFERENT node starts fresh: yesterday's record under
/// a new id would be a lie with a head start.
pub(crate) fn open_for(view: &mut View, node_id: String) {
    let gen = view
        .node_detail
        .as_ref()
        .map(|o| o.gen.wrapping_add(1))
        .unwrap_or(0);
    let same = view
        .node_detail
        .as_ref()
        .is_some_and(|o| o.node_id == node_id);
    let (detail, sel) = match (&view.node_detail, same) {
        (Some(o), true) => (o.detail.clone(), o.sel),
        _ => (None, 0),
    };
    view.node_detail = Some(NodeDetailOverlay {
        node_id,
        detail,
        error: None,
        inflight: false,
        want: true,
        gen,
        sel,
    });
}

/// Close the overlay. The selector (if one is open underneath) stays put -
/// Esc unwinds one layer, exactly as peek does.
pub(crate) fn close(view: &mut View) {
    view.node_detail = None;
}

/// The fold result channel the run loop hands [`maybe_kick`] and reads in
/// its node-detail arm. The tuple carries the NODE ID beside the generation:
/// both wrap, so a close-then-reopen inside one fold's budget would reuse
/// gen 0 and a stale record would land on the wrong node's pane. The id is
/// the second guard the feed's gen-only tuple lacks.
pub(crate) type FoldTx = tokio::sync::mpsc::UnboundedSender<(u64, String, FoldResult)>;

/// At most ONE fold in flight, armed by `want` (the feed's discipline).
pub(crate) fn maybe_kick(view: &mut View, tx: &FoldTx) {
    let Some(o) = view.node_detail.as_mut() else {
        return;
    };
    if !o.want || o.inflight {
        return;
    }
    o.want = false;
    o.inflight = true;
    let tx = tx.clone();
    let gen = o.gen;
    let id = o.node_id.clone();
    tokio::spawn(async move {
        let result = detail_now(&id).await;
        let _ = tx.send((gen, id, result));
    });
}

/// A fold landed: apply only to the still-open, same-generation overlay for
/// the SAME node.
pub(crate) fn apply_fold(view: &mut View, gen: u64, node_id: &str, outcome: FoldResult) {
    let Some(o) = view.node_detail.as_mut() else {
        return;
    };
    if gen != o.gen || o.node_id != node_id {
        return;
    }
    o.inflight = false;
    match outcome {
        Ok(detail) => {
            o.detail = Some(detail);
            o.error = None;
            o.sel = 0;
        }
        Err(e) => o.error = Some(e),
    }
}

/// What a session row can DO, derived from the joined registry row - never
/// a fixed verb. The refusal half carries the reason the row renders dim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionAction {
    /// A live pane here: focus it. A paneless attachable row: attach.
    Attach,
    /// A paneless row its harness can resume.
    Resume,
    /// No launch; the reason renders in the action cell and a press
    /// answers with it as the notice.
    Dim(String),
}

/// Derive the row's action from the joined `AgentRow`. Absent row: no
/// registry row. Present: pane or attach id wins (the reach verbs), then
/// the resume form - but never on a row the registry reports Working or
/// Done, which the resume door refuses anyway; saying so here is the whole
/// point of the state-aware cell.
pub(crate) fn session_action(a: Option<&AgentRow>) -> SessionAction {
    let Some(a) = a else {
        return SessionAction::Dim("no registry row".into());
    };
    if a.pane_id.is_some() || a.attach_id.is_some() {
        return SessionAction::Attach;
    }
    match a.badge {
        Some(AgentBadge::Done) => return SessionAction::Dim("done".into()),
        Some(AgentBadge::Working) => return SessionAction::Dim("working".into()),
        _ => {}
    }
    if a.resumable {
        return SessionAction::Resume;
    }
    SessionAction::Dim("not resumable".into())
}

/// The row's basis (R1): `claim` when the store says THIS session holds the
/// live claim, else `graph` - the row is in the detail because the graph's
/// `sessions[]` recorded it. A basis names where the fact came from; the
/// pane never invents a third source it did not read.
pub(crate) fn session_basis(detail: &NodeDetail, s: &NodeSession) -> &'static str {
    if detail.locked_by_harness_session.as_deref() == Some(s.session_id.as_str()) {
        "claim"
    } else {
        "graph"
    }
}

/// The node's king: the first crowned row whose territory names the node,
/// its parent epic, or its project (crown scopes split on `,`, the level-0
/// separator). Direct membership only - a grandchild epic resolves through
/// no scope here, and the pane says `none` rather than guessing.
pub(crate) fn king_of(
    agents: &[AgentRow],
    node_id: &str,
    parent: Option<&str>,
    project: Option<&str>,
) -> Option<(String, u32)> {
    for a in agents {
        let (Some(scope), Some(level)) = (&a.crown_scope, a.crown_level) else {
            continue;
        };
        for member in scope.split(',') {
            let m = member.trim();
            if m == node_id || parent == Some(m) || project == Some(m) {
                return Some((a.name.clone(), level));
            }
        }
    }
    None
}

/// The row's age, human-short. `None` renders `-`: the probe did not
/// answer, and a fabricated number is the one thing the pane must never do.
fn age_cell(age_s: Option<u64>) -> String {
    match age_s {
        None => "-".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86400),
    }
}

/// The row's state word, from the registry's own report. An unmeasured row
/// says so - never a guessed `idle`.
fn state_cell(a: Option<&AgentRow>) -> String {
    match a {
        None => "-".to_string(),
        Some(a) if a.exited => "dead".to_string(),
        Some(a) => match a.badge {
            Some(AgentBadge::Working) => "working".to_string(),
            Some(AgentBadge::Blocked) => "blocked".to_string(),
            Some(AgentBadge::Done) => "done".to_string(),
            None => "unmeasured".to_string(),
        },
    }
}

fn action_cell(action: &SessionAction) -> String {
    match action {
        SessionAction::Attach => "attach".to_string(),
        SessionAction::Resume => "resume".to_string(),
        SessionAction::Dim(reason) => reason.clone(),
    }
}

/// Lines before the first session row (header, meta, plan, rule, column
/// header) - the constant that maps `sel` to a body index for the painter.
const SESSIONS_START: usize = 5;

/// The overlay body. `w` truncates each line (the painter wraps nothing).
/// Structure: header, meta (lane/project/king), plan, rule, the session
/// table, rule, the comment counts and the newest notes.
pub(crate) fn overlay_lines(o: &NodeDetailOverlay, agents: &[AgentRow], w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(d) = &o.detail else {
        // Before the first fold lands: the load line, or the typed failure.
        // Never a fabricated empty pane (AC4-EDGE's blank-pane half).
        if let Some(e) = &o.error {
            lines.push(truncate(&format!("   {e}"), w));
        } else if o.inflight {
            lines.push(truncate("   reading node...", w));
        }
        return lines;
    };
    // Header: id FIRST (the handle every verb takes), then the title.
    let title = if d.title.is_empty() {
        d.slug.clone()
    } else {
        d.title.clone()
    };
    lines.push(truncate(&format!("{}  {}", d.id, title), w));
    // Meta: status, project, priority, and the king.
    let king = match king_of(agents, &d.id, d.parent.as_deref(), d.project.as_deref()) {
        Some((name, level)) => format!("{name} (L{level})"),
        None => "none".to_string(),
    };
    let project = d.project.as_deref().unwrap_or("-");
    lines.push(truncate(
        &format!(
            "{} · {} · {} · king: {}",
            d.status, project, d.priority, king
        ),
        w,
    ));
    // Plan: the same path the card menu's Open plan resolves.
    lines.push(truncate(
        &format!("plan: {}", d.plan_path.as_deref().unwrap_or("none")),
        w,
    ));
    lines.push("".to_string());
    lines.push(truncate(
        &format!(
            "{:<9} {:<7} {:<9} {:<12} {:<9} {:<4} {:<9} {}",
            "phase", "harness", "id", "model", "state", "age", "action", "basis"
        ),
        w,
    ));
    for (i, s) in d.sessions.iter().enumerate() {
        let joined = agents
            .iter()
            .find(|a| a.harness_session_id.as_deref() == Some(s.session_id.as_str()));
        let marker = if i == o.sel { "▸" } else { " " };
        let model = s
            .observed_model
            .as_ref()
            .and_then(|m| m.model.clone())
            .or_else(|| joined.and_then(|a| a.model.clone()))
            .unwrap_or_else(|| "-".to_string());
        lines.push(truncate(
            &format!(
                "{marker} {:<9} {:<7} {:<9} {:<12} {:<9} {:<4} {:<9} {}",
                s.phase.as_deref().unwrap_or("-"),
                s.harness.as_deref().unwrap_or("-"),
                short_id(&s.session_id),
                model,
                state_cell(joined),
                age_cell(joined.and_then(|a| a.last_activity_age_s)),
                action_cell(&session_action(joined)),
                session_basis(d, s),
            ),
            w,
        ));
    }
    if d.sessions.is_empty() {
        lines.push(truncate("   no sessions recorded", w));
    }
    lines.push("".to_string());
    lines.push(truncate(
        &format!(
            "notes ({}) · decisions ({})",
            d.progress_notes.len(),
            d.decisions.len()
        ),
        w,
    ));
    for note in d.progress_notes.iter().rev().take(3) {
        lines.push(truncate(&format!("   {}", note.body), w));
    }
    for dec in d.decisions.iter().take(3) {
        lines.push(truncate(&format!("   decision: {}", dec.decision_id), w));
    }
    lines
}

/// The body index of the selected session row, for the painter's follow.
/// The overlay clamps `sel` against the CURRENT record before painting, so
/// this stays in range whenever the overlay does.
pub(crate) fn selected_line(o: &NodeDetailOverlay) -> Option<usize> {
    let sessions = o.detail.as_ref()?.sessions.len();
    if sessions == 0 {
        return None;
    }
    Some(SESSIONS_START + o.sel.min(sessions - 1))
}

/// The overlay's keys. Only ever called while the overlay is open - the
/// stdin router hands over exactly then. Esc closes; j/k (and arrows) move
/// the selection; Enter/a/r run the SELECTED row's derived action (a dim
/// row answers with its reason as the notice and launches nothing); any
/// other byte falls through to the focused pane.
pub(crate) async fn detail_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    // A lone-Esc chunk closes instantly: the modal fold would hold the byte
    // pending a possible escape sequence (the contract its doc names, the
    // drag guards mirror).
    if bytes == [0x1b] && view.node_detail_esc.is_empty() {
        close(view);
        return Ok(StdinFlow::Continue);
    }
    let mut esc = std::mem::take(&mut view.node_detail_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.node_detail_esc = esc;
    for tok in toks {
        if view.node_detail.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc | ModalKey::Byte(b'q') => close(view),
            ModalKey::Up | ModalKey::Byte(b'k') => move_sel(view, false),
            ModalKey::Down | ModalKey::Byte(b'j') => move_sel(view, true),
            ModalKey::Enter | ModalKey::Byte(b'a') | ModalKey::Byte(b'r') => {
                activate_selected(view, sock_w).await?;
            }
            // `d` dispatches the NODE (the click path's confirm, armed on
            // top; the confirm's own Enter sends, its Esc returns here).
            ModalKey::Byte(b'd') => dispatch_node(view, sock_w).await?,
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// The compose-pass draw (the peek branch's shape): one call from the
/// client's overlay chain, the geometry and chrome owned here so client.rs
/// keeps one line, not this pane's body.
impl View {
    pub(super) fn draw_node_detail(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        origin: (usize, usize),
        dims: (usize, usize),
    ) {
        let Some(nd) = &self.node_detail else {
            return;
        };
        let lines = overlay_lines(
            nd,
            &self.layout.agents,
            dims.1.saturating_sub(crate::chrome::Chrome::FRAME_COLS),
        );
        let chrome = crate::chrome::Chrome::new(&nd.node_id, crate::popup::Anchor::Center)
            .footer("enter/a act · esc close");
        super::draw_lines_overlay(
            cells,
            rows,
            cols,
            origin,
            dims,
            &chrome,
            &lines,
            &self.theme,
            selected_line(nd),
        );
    }
}

/// Enter on a backlog card opens the node detail overlay - the card's menu
/// (float/defer/open plan/plan) stays on `m` and right-click. The selector
/// stays open underneath, so Esc from the detail drops back into it. True
/// when the overlay opened.
pub(crate) fn open_from_selector(view: &mut View, cur: usize) -> bool {
    let rows = view.display_rows();
    let id = match rows.get(cur) {
        Some(DisplayRow::Card(c)) => c.id.clone(),
        _ => return false,
    };
    open_for(view, id);
    true
}

/// The card menu's Plan entry: the dispatch door pinned to the architect
/// sub-agent and the blueprint message. The card must still be in the feed
/// (the server re-checks freshness anyway); the door's spawn gate answers
/// as always, and its refusal renders verbatim through the dispatch notice
/// path.
pub(crate) async fn plan_spawn_send(
    view: &mut View,
    node: String,
    account: Option<String>,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    if !view.layout.backlog.iter().any(|c| c.id == node) {
        view.set_notice(format!("{node} is no longer in the backlog"));
        return Ok(());
    }
    write_msg(
        sock_w,
        &ClientMsg::Command(Command::DispatchPlan { node, account }),
    )
    .await
    .map_err(|e| format!("plan spawn send failed: {e}"))
}

fn move_sel(view: &mut View, down: bool) {
    let Some(o) = view.node_detail.as_mut() else {
        return;
    };
    let Some(d) = &o.detail else {
        return;
    };
    let len = d.sessions.len();
    if len == 0 {
        return;
    }
    o.sel = if down {
        (o.sel + 1).min(len - 1)
    } else {
        o.sel.saturating_sub(1)
    };
}

/// Run the selected session row's derived action through the agents
/// section's own hit cascade. The row's registry row is re-resolved against
/// the LIVE roster, so a session that exited between paint and press
/// answers as what it now is - a dim reason - never a stale launch.
async fn activate_selected(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(o) = view.node_detail.as_ref() else {
        return Ok(());
    };
    let Some(d) = &o.detail else {
        return Ok(());
    };
    let Some(s) = d.sessions.get(o.sel) else {
        return Ok(());
    };
    let joined = view
        .layout
        .agents
        .iter()
        .find(|a| a.harness_session_id.as_deref() == Some(s.session_id.as_str()));
    match session_action(joined) {
        SessionAction::Attach | SessionAction::Resume => {
            let a = joined.expect("a derivable action has a row");
            let hit = agent_hit(a, view.layout.active_squad);
            close(view);
            apply_hit(view, hit, sock_w).await?;
        }
        SessionAction::Dim(reason) => {
            view.set_notice(reason);
        }
    }
    Ok(())
}

/// First 8 chars of a session id - the join key `fno agents top` prints.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Arm the node's dispatch confirm - the same hit the card CLICK takes, so
/// the confirm's one-keypress safety gates keyboard dispatch exactly as it
/// gates the mouse. A card no longer in the feed answers with a notice.
async fn dispatch_node(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(o) = view.node_detail.as_ref() else {
        return Ok(());
    };
    let id = o.node_id.clone();
    let Some(card) = view.layout.backlog.iter().find(|c| c.id == id).cloned() else {
        view.set_notice(format!("{id} is no longer in the backlog"));
        return Ok(());
    };
    let hit = view.card_hit(&card);
    apply_hit(view, hit, sock_w).await
}

fn truncate(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    s.chars().take(w).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(harness_session: Option<&str>) -> AgentRow {
        // Only the fields WITHOUT `#[serde(default)]` are present; every
        // defaulted field rides the wire the same way it does in production.
        serde_json::from_str::<AgentRow>(&format!(
            r#"{{"squad":null,"name":"w1","pane_id":null,"badge":null,"reason":null,"exited":false{}}}"#,
            harness_session
                .map(|s| format!(r#","harness_session_id":"{s}""#))
                .unwrap_or_default()
        ))
        .expect("a minimal row decodes")
    }

    fn detail() -> NodeDetail {
        serde_json::from_str(
            r#"{"id":"x-1","slug":"feat","title":"Feat","priority":"p1","status":"ready",
                "project":"fno","parent":null,"plan_path":"/p.md",
                "locked_by_harness_session":"cccccccc-2",
                "sessions":[
                  {"phase":"do","harness":"claude","session_id":"cccccccc-2",
                   "observed_model":{"kind":"observed","model":"glm","samples":3}},
                  {"phase":"do","harness":"claude","session_id":"dddddddd-3"}
                ],
                "progress_notes":[{"body":"started","kind":"progress"}],
                "decisions":[{"decision_id":"d-1","ts":"2026-09-18T00:00:00Z"}]}"#,
        )
        .expect("a record decodes")
    }

    #[test]
    fn the_record_deserializes_from_the_stores_payload() {
        let d = detail();
        assert_eq!(d.id, "x-1");
        assert_eq!(d.sessions.len(), 2);
        assert_eq!(
            d.sessions[0]
                .observed_model
                .as_ref()
                .unwrap()
                .model
                .as_deref(),
            Some("glm")
        );
        // Fields the store has not started emitting (encounters) are absent,
        // and absent decodes empty - never a fold failure.
        assert!(d.progress_notes.len() == 1);
    }

    #[test]
    fn canonical_note_text_and_legacy_rows_survive() {
        // The writer stores {ts, text}; the note renders its text. The
        // legacy string session row is skipped by the parse, never fatal.
        let d = parse_detail(
            br#"{"id":"x-1","progress_notes":[{"ts":"2026-09-19T00:00:00Z","text":"shipped"}],
                "sessions":["legacy-string-id",{"session_id":"cccccccc-9"}]}"#,
        )
        .expect("the canonical shapes decode");
        assert_eq!(d.progress_notes[0].body, "shipped");
        assert_eq!(d.sessions.len(), 1);
        assert_eq!(d.sessions[0].session_id, "cccccccc-9");
    }

    #[test]
    fn garbage_is_a_typed_error() {
        let err = parse_detail(b"not json").expect_err("garbage is typed, never silent");
        assert!(matches!(err, NodeDetailError::Malformed(_)));
        assert!(err.to_string().contains("malformed JSON"));
    }

    #[test]
    fn timeout_names_the_budget() {
        let line = NodeDetailError::Timeout.to_string();
        assert!(line.contains("timed out after 10s"), "{line}");
    }

    #[test]
    fn the_action_is_derived_never_fixed() {
        // No registry row: the honest dim answer, never a guessed verb.
        assert_eq!(
            session_action(None),
            SessionAction::Dim("no registry row".into())
        );
        // A pane wins: focus the live pane.
        let mut a = row(Some("s1"));
        a.pane_id = Some(3);
        assert_eq!(session_action(Some(&a)), SessionAction::Attach);
        // Paneless but attachable: attach.
        let mut a = row(Some("s1"));
        a.attach_id = Some("job1".into());
        assert_eq!(session_action(Some(&a)), SessionAction::Attach);
        // Resumable paneless: resume.
        let mut a = row(Some("s1"));
        a.resumable = true;
        assert_eq!(session_action(Some(&a)), SessionAction::Resume);
        // Registry says Done: resume would be refused at the door; the cell
        // says so first.
        let mut a = row(Some("s1"));
        a.resumable = true;
        a.badge = Some(AgentBadge::Done);
        assert_eq!(session_action(Some(&a)), SessionAction::Dim("done".into()));
        // Registry says Working: same refusal, different word.
        let mut a = row(Some("s1"));
        a.resumable = true;
        a.badge = Some(AgentBadge::Working);
        assert_eq!(
            session_action(Some(&a)),
            SessionAction::Dim("working".into())
        );
        // A registry row with no route: the named refusal.
        let a = row(Some("s1"));
        assert_eq!(
            session_action(Some(&a)),
            SessionAction::Dim("not resumable".into())
        );
    }

    #[test]
    fn the_basis_names_where_the_fact_came_from() {
        let d = detail();
        assert_eq!(session_basis(&d, &d.sessions[0]), "claim");
        assert_eq!(session_basis(&d, &d.sessions[1]), "graph");
    }

    #[test]
    fn the_king_is_a_reverse_lookup_over_crowned_rows() {
        let mut king = row(None);
        king.name = "kd".into();
        king.crown_level = Some(2);
        king.crown_scope = Some("x-9".into());
        // Node member.
        assert_eq!(
            king_of(&[king.clone()], "x-9", None, None),
            Some(("kd".into(), 2))
        );
        // Parent epic member.
        assert_eq!(
            king_of(std::slice::from_ref(&king), "x-1", Some("x-9"), None),
            Some(("kd".into(), 2))
        );
        // Project member via the comma-split.
        let mut king = row(None);
        king.crown_level = Some(0);
        king.crown_scope = Some("other,fno".into());
        assert_eq!(
            king_of(std::slice::from_ref(&king), "x-1", None, Some("fno")),
            Some((row(None).name, 0))
        );
        // No crowned row names it: none is the answer.
        assert_eq!(king_of(&[row(None)], "x-1", None, None), None);
    }

    #[test]
    fn the_render_answers_with_words_never_a_blank_pane() {
        let o = NodeDetailOverlay {
            node_id: "x-1".into(),
            detail: None,
            error: None,
            inflight: true,
            want: false,
            gen: 0,
            sel: 0,
        };
        let lines = overlay_lines(&o, &[], 80);
        assert!(lines[0].contains("reading node"), "{lines:?}");
        let mut o = o;
        o.inflight = false;
        o.error = Some(NodeDetailError::Exit("store unreadable".into()));
        let lines = overlay_lines(&o, &[], 80);
        assert!(lines[0].contains("store unreadable"), "{lines:?}");
    }

    #[test]
    fn the_render_paints_header_plan_sessions_and_counts() {
        let o = NodeDetailOverlay {
            node_id: "x-1".into(),
            detail: Some(detail()),
            error: None,
            inflight: false,
            want: false,
            gen: 1,
            sel: 0,
        };
        let agents = vec![row(Some("cccccccc-2"))];
        let lines = overlay_lines(&o, &agents, 200);
        let joined = lines.join("\n");
        // AC3: id first, plan, one row per session.
        assert!(joined.contains("x-1  Feat"), "{joined}");
        assert!(joined.contains("plan: /p.md"), "{joined}");
        assert!(joined.contains("cccccccc"), "{joined}");
        assert!(joined.contains("dddddddd"), "{joined}");
        // King: no crowned row in the roster -> none.
        assert!(joined.contains("king: none"), "{joined}");
        // Counts render what the store measured.
        assert!(joined.contains("notes (1) · decisions (1)"), "{joined}");
        // Basis columns.
        assert!(joined.contains("claim"), "{joined}");
        assert!(joined.contains("graph"), "{joined}");
        // The selected session row is the one the painter follows.
        assert_eq!(selected_line(&o), Some(SESSIONS_START));
    }

    #[test]
    fn age_and_state_refuse_to_invent() {
        assert_eq!(age_cell(None), "-");
        assert_eq!(age_cell(Some(45)), "45s");
        assert_eq!(age_cell(Some(125)), "2m");
        assert_eq!(age_cell(Some(7200)), "2h");
        assert_eq!(age_cell(Some(172800)), "2d");
        assert_eq!(state_cell(None), "-");
        let mut a = row(None);
        assert_eq!(state_cell(Some(&a)), "unmeasured");
        a.badge = Some(AgentBadge::Blocked);
        assert_eq!(state_cell(Some(&a)), "blocked");
        a.exited = true;
        assert_eq!(state_cell(Some(&a)), "dead");
    }
}
