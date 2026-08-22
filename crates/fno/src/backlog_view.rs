//! Work-queue reader for the sideline backlog lane (x-6f77).
//!
//! Sibling of [`crate::agents_view`]: an off-loop interval task (in server.rs)
//! parses `~/.fno/graph.json` and hands the core loop a board-ordered card set
//! the sideline renders under the "Backlog" header. Same discipline as the
//! registry reader - the core loop and the render path never touch the file;
//! the mtime+len gate skips the 4M read until the graph actually changes (a
//! claim/close mutation bumps mtime, so a card flips to in-flight for free).
//!
//! The graph is dual-language (Python `fno backlog` + the fno-agents daemon)
//! and its FILE is the contract: parsed via `serde_json::Value` with tolerant
//! field access rather than importing the graph crate - the mux needs four
//! fields per node, not the whole model. A malformed document keeps the
//! last-good cards (a torn concurrent write must not blank the lane).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::proto::{BacklogCard, CardState};

/// Board-order cap: the sideline shows the head of the queue, not all 1900
/// nodes. Bounds the wire frame and the render loop; the rest live on the full
/// board (`fno backlog`). Kept generous so a real ready/blocked set is never
/// truncated in practice.
const CARD_CAP: usize = 40;

/// The graph path, resolved as `fno.paths` does: `FNO_GRAPH_JSON` >
/// `$HOME/.fno/graph.json` > `./.fno/graph.json`.
pub fn graph_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_GRAPH_JSON") {
        return PathBuf::from(v);
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("graph.json")
}

/// Whether an external tracker backend is selected, resolved exactly as the
/// Python side resolves it (`FNO_TRACKER_BACKEND`, default `graph`). Shared
/// resolution so the reader and `get_tracker` can never disagree about which
/// store is live - the mux reading graph.json while selection reads GitHub
/// would display rows the dispatcher will never pick.
pub fn external_backend_selected() -> bool {
    match std::env::var("FNO_TRACKER_BACKEND") {
        Ok(v) => !v.trim().is_empty() && v.trim() != "graph",
        Err(_) => false,
    }
}

/// Seconds between snapshot refreshes in external mode (Risk 4: a remote
/// backend has no mtime to gate on, so the refresh must be bounded by the
/// clock instead - a backend outage must not become a hot exec loop).
pub const SNAPSHOT_REFRESH_SECS: u64 = 10;

/// Execute the backend-neutral snapshot verb (`fno backlog status --snapshot`)
/// and return its stdout. `None` on any failure - missing binary, non-zero
/// exit, unparseable stdout - so the caller's last-good/stale machinery treats
/// a failed snapshot exactly like a failed file read.
///
/// The snapshot document is the SAME shape `derive_queue` consumes
/// (`{"backend": ..., "entries": [...]}` with graph-compatible entry fields),
/// so both reader modes feed the same pure derivation functions; the mux
/// classification, lanes, and read-time dependency readiness are unchanged.
pub fn read_snapshot() -> Option<String> {
    // fno_bin (FNO_BIN override, else the running binary) - the same resolver
    // every other fno-subprocess site in the crate uses, so the snapshot is
    // read from the binary version that owns this document's schema.
    let out = std::process::Command::new(crate::server::fno_bin())
        .args(["backlog", "status", "--snapshot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    // Parse gate: a truncated/torn stdout is a failed read, not a document to
    // cache (mirrors the torn-file retry the graph path gets for free).
    serde_json::from_str::<serde_json::Value>(&s).ok()?;
    Some(s)
}

/// Read a node's derived status, tolerating the pre-rename `_status` key so a
/// graph.json not yet re-written by the Python side still classifies.
fn node_status(e: &serde_json::Value) -> Option<&str> {
    e.get("status")
        .or_else(|| e.get("_status"))
        .and_then(|v| v.as_str())
}

/// Classify a node's `status` into a queue state, or `None` to drop it (done /
/// idea / deferred / superseded are not actionable queue work). Authoritative
/// on `status` alone: a claimed node with a stale `blocked_by` is in-flight,
/// not blocked.
///
/// Every status in the Python vocabulary gets an explicit arm, including the
/// ones that drop, so the `_` catch-all means "a status this reader has never
/// heard of" rather than doubling as the drop bucket for known-but-unlisted
/// ones. That distinction is what makes the drift check possible:
/// `cli/tests/unit/test_graph_status_vocabulary.py` asserts this match names
/// every member of `VALID_STATUSES` + `STATUS_MIGRATION`, so a status added on
/// the Python side fails CI here until someone decides where it belongs.
/// Extra arms are fine (they carry legacy spellings the Python set has dropped).
fn classify(status: &str) -> Option<CardState> {
    match status {
        "claimed" | "in-progress" | "in_progress" => Some(CardState::InFlight),
        "ready" | "next" => Some(CardState::Ready),
        "blocked" => Some(CardState::Blocked),
        // `design` (plan doc linked but not yet blueprinted) is deliberately
        // NOT queue work: it is not autonomously dispatchable, so showing it as
        // Ready would advertise work the dispatcher will never pick up. Drops
        // alongside `idea`, the rung below it.
        "design" => None,
        // Terminal or pre-queue rungs: real statuses, deliberately not cards.
        // `in_review` is work that has already left the queue for a PR.
        "idea" | "in_review" | "done" | "deferred" | "superseded" => None,
        _ => None,
    }
}

/// Priority rank for the board sort (`p0` first). Unknown sorts last.
fn priority_rank(p: &str) -> u8 {
    match p {
        "p0" => 0,
        "p1" => 1,
        "p2" => 2,
        "p3" => 3,
        _ => 9,
    }
}

/// Which projects the board renders (x-20f1).
///
/// `All` is the historical behavior: every actionable card in the graph. The
/// graph is ONE store tagged by project, so a person working in one checkout
/// read every other project's work too, every time they looked at the board.
///
/// `Projects` keeps only cards whose `project` is in the set. Unscoped cards
/// (null / empty / whitespace `project`) are ALWAYS kept: a node with no
/// project is not another project's work, so hiding it would make work
/// disappear rather than filter it.
///
/// An EMPTY set keeps the unscoped lane and nothing else. It is reachable only
/// from an explicit wire value that names no project; it is NOT what an
/// unresolvable selector produces. That case is deliberately different:
/// [`resolve_board_scope`] returns the empty set to mean "nothing scoped this",
/// [`board_scope_wire`] writes it as the empty string, and the server reads
/// that as "no client latched a scope" and renders every project. A board that
/// silently blanked would be a worse failure than an unscoped one, so the
/// refusal is reported through `fno mux doctor` (a `warn` plus the remedy)
/// rather than by hiding work. Every reason string on that path says which
/// board you actually get.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BoardScope {
    /// The historical whole-graph board, and the `Default` so a reader built
    /// without a resolved scope never silently hides work.
    #[default]
    All,
    Projects(HashSet<String>),
}

impl BoardScope {
    /// Whether a card carrying `project` (already trimmed; `None` = unscoped)
    /// belongs on this board.
    fn keeps(&self, project: Option<&str>) -> bool {
        match (self, project) {
            (BoardScope::All, _) => true,
            (_, None) => true,
            (BoardScope::Projects(set), Some(p)) => set.contains(p),
        }
    }

    /// One line naming the scope, for the server log and `fno mux doctor`.
    pub fn describe(&self) -> String {
        match self {
            BoardScope::All => "all projects".to_string(),
            BoardScope::Projects(set) if set.is_empty() => {
                "unscoped cards only (no project resolved; refusing to widen)".to_string()
            }
            BoardScope::Projects(set) => {
                let mut names: Vec<&str> = set.iter().map(String::as_str).collect();
                names.sort_unstable();
                format!("{} + unscoped", names.join(", "))
            }
        }
    }
}

/// Parse the `FNO_BOARD_SCOPE` wire value: `all`, or a comma-separated project
/// list (an empty list is the refusal above). Pure, so the resolution ladder
/// and the parse are testable apart.
pub fn parse_board_scope(raw: &str) -> BoardScope {
    if raw.trim() == "all" {
        return BoardScope::All;
    }
    BoardScope::Projects(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The `FNO_BOARD_SCOPE` wire value for a resolved scope: the inverse of
/// [`parse_board_scope`], so what the client latches is what the server reads.
///
/// A refusal (the empty set) serializes to the empty string, which
/// [`board_scope_from_spawn_env`] reads as "not latched" and widens to `All`.
/// That is deliberate: an unresolvable scope must not silently narrow a board
/// the operator never configured. `fno mux doctor` is where the refusal is
/// reported, with its remedy.
pub fn board_scope_wire(scope: &BoardScope) -> String {
    match scope {
        BoardScope::All => "all".to_string(),
        BoardScope::Projects(set) => {
            let mut names: Vec<&str> = set.iter().map(String::as_str).collect();
            names.sort_unstable();
            names.join(",")
        }
    }
}

/// Project names declared by `work.workspaces.<name>`, read from the JSON
/// block `fno config get` prints for it. `None` when the block is missing or
/// names nothing - the caller REFUSES on `None` rather than falling back to
/// every project.
pub fn workspace_projects(json: &str) -> Option<HashSet<String>> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    let names: HashSet<String> = doc
        .get("projects")?
        .as_array()?
        .iter()
        .filter_map(|p| p.get("name").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// The scope a running server uses, read ONLY from the env the client latched
/// at spawn (`FNO_BOARD_SCOPE`).
///
/// The server resolves nothing itself. Resolution shells out to `fno config
/// get`, and a subprocess on the server's startup path is not free: it delayed
/// shutdown past the SIGTERM grace (a healthy server reported as an unclean
/// death) and perturbed frame ordering in the multiclient e2e. The client
/// already pays exactly this cost for `FNO_MUX_SHELL_INTEGRATION`, on a path
/// built to tolerate it, so the read happens there and rides in on the env.
///
/// No env means no client latched one - a hand-started `fno mux server`. That
/// gets the historical whole-graph board rather than a silent narrowing, and
/// the reason says so. `fno mux doctor` resolves live and reports what a fresh
/// spawn WOULD latch.
pub fn board_scope_from_spawn_env() -> (BoardScope, String) {
    match std::env::var("FNO_BOARD_SCOPE") {
        Ok(v) if !v.trim().is_empty() => {
            let scope = parse_board_scope(&v);
            let d = scope.describe();
            (scope, format!("latched at spawn: {d}"))
        }
        _ => (
            BoardScope::All,
            "not latched by a client spawn (FNO_BOARD_SCOPE unset): all projects".into(),
        ),
    }
}

/// Resolve the board scope once, at CLIENT spawn time (and for `mux doctor`).
///
/// Ladder, first hit wins:
///   1. `FNO_BOARD_SCOPE` (the test hatch, and what a spawner latches).
///   2. `config.mux.board_scope`:
///      - `all`               -> every project
///      - `workspace:<name>`  -> the projects `work.workspaces.<name>` declares
///      - `repo` / unset      -> the current repo's `config.project.id`
///   3. Anything that fails to resolve -> an empty set (unscoped lane only).
///
/// `config.project.id` resolves through git, so every worktree layout of a
/// checkout answers with the same project and no path table is needed here.
///
/// Returns the scope plus a one-line reason the caller logs. The reason is the
/// whole point on the refusal path: a board that quietly went empty and a board
/// correctly scoped to a project with no ready work look identical.
pub fn resolve_board_scope(config_get: impl Fn(&str) -> Option<String>) -> (BoardScope, String) {
    if let Ok(v) = std::env::var("FNO_BOARD_SCOPE") {
        if !v.trim().is_empty() {
            let scope = parse_board_scope(&v);
            let d = scope.describe();
            return (scope, format!("FNO_BOARD_SCOPE={}: {d}", v.trim()));
        }
    }
    let setting = config_get("mux.board_scope").unwrap_or_default();
    let setting = setting.trim();
    if setting == "all" {
        return (BoardScope::All, "mux.board_scope=all: all projects".into());
    }
    if let Some(name) = setting.strip_prefix("workspace:") {
        let name = name.trim();
        let key = format!("work.workspaces.{name}");
        return match config_get(&key).as_deref().and_then(workspace_projects) {
            Some(set) => {
                let scope = BoardScope::Projects(set);
                let d = scope.describe();
                (scope, format!("mux.board_scope=workspace:{name}: {d}"))
            }
            None => (
                BoardScope::Projects(HashSet::new()),
                format!(
                    "mux.board_scope=workspace:{name}: {key} names no project, so \
                     nothing scopes the board and it shows EVERY project; name a \
                     project in that workspace, or set mux.board_scope=repo|all"
                ),
            ),
        };
    }
    if !setting.is_empty() && setting != "repo" {
        return (
            BoardScope::Projects(HashSet::new()),
            format!(
                "mux.board_scope={setting:?} is not one of repo|all|workspace:<name>, \
                 so nothing scopes the board and it shows EVERY project"
            ),
        );
    }
    match config_get("project.id") {
        Some(p) if !p.trim().is_empty() => {
            let p = p.trim().to_string();
            (
                BoardScope::Projects(HashSet::from([p.clone()])),
                format!("repo project {p}: {p} + unscoped"),
            )
        }
        _ => (
            BoardScope::Projects(HashSet::new()),
            "no config.project.id here, so nothing scopes the board and it shows EVERY \
             project; set project.id, or mux.board_scope=all to mean it on purpose"
                .into(),
        ),
    }
}

/// Derive the board-ordered card set from raw graph JSON. Pure so the ordering
/// and classification are unit-testable without a file. `None` on a malformed
/// document (the caller keeps its last-good cards). The order mirrors the board
/// (`docs/architecture/backlog-board-ordering.md`): project lane (unscoped
/// last), rank band (ranked before unranked, ascending), priority, created_at.
pub fn derive_cards(raw: &str) -> Option<Vec<BacklogCard>> {
    derive_queue(raw, None, &BoardScope::All).map(|q| q.cards)
}

/// The card set plus the UNCAPPED per-lane counts it was cut from (x-1d91).
///
/// Both consumers of "how much work is really there" read these: the section's
/// `+N more` (total minus what it shows) and the mini-kanban's per-lane headers.
/// One field rather than a separate scalar total, so the two can never disagree.
/// Sorted by lane name for a stable render; `cards.len() <= total()` always.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queue {
    pub cards: Vec<BacklogCard>,
    pub lanes: Vec<(String, usize)>,
    /// The graph read has been failing long enough that these cards are memory,
    /// not fact. The section says so rather than presenting stale work as
    /// current; it clears the moment a read succeeds again.
    pub stale: bool,
}

/// The lane bucket for a card whose column the wire did not carry. Unreachable
/// from this reader (an excluded node is dropped, never bucketed) - it exists so
/// a card from an older/other producer still lands somewhere visible.
pub const UNLANED: &str = "unlaned";

/// Canonical board column order, mirroring `KANBAN_COLUMNS` in
/// `graph/render.py`: Now leads (genuine today-work), Triage holds the
/// awaiting-ack queue, Done is terminal.
const KANBAN_COLUMNS: [&str; 5] = ["Now", "Next", "Later", "Triage", "Done"];

/// A lane's position in [`KANBAN_COLUMNS`]; anything unrecognized sorts last.
fn lane_rank(lane: &str) -> usize {
    KANBAN_COLUMNS
        .iter()
        .position(|c| *c == lane)
        .unwrap_or(KANBAN_COLUMNS.len())
}

/// The board column for a queue node, mirroring `_kanban_column` in
/// `cli/src/fno/graph/render.py`. `None` excludes the node from the board.
///
/// This is DERIVED, not read: no graph node carries a column field. The Python
/// board computes it from intent on every render, so the mux computes the same
/// function rather than inventing a second answer - the two boards must name the
/// same lane for the same node or the sideline is quietly lying about where work
/// sits. Kept deliberately close to the Python, ordering included, so a change
/// there is easy to mirror here.
///
/// `claimed` folds the graph `status` and the live-lockfile claim together (a
/// node another session drives may never write a graph status - x-4845);
/// `underway` is [`in_progress_epics`] membership.
fn kanban_column(e: &serde_json::Value, claimed: bool, underway: bool) -> Option<&'static str> {
    if e.get("type").and_then(|v| v.as_str()) == Some("roadmap") {
        return None;
    }
    if has_stamp(e, "completed_at") {
        return Some("Done");
    }
    let status = node_status(e).unwrap_or("ready");
    if matches!(status, "deferred" | "superseded") {
        return None; // off-board until reactivated
    }
    if claimed || underway {
        return Some("Now");
    }
    // Queued is orthogonal to `status`: a node awaiting human ack is not active
    // work, so it must not inflate Now - but a claimed node stays in Now.
    if has_stamp(e, "queued_at") {
        return Some("Triage");
    }
    match e.get("priority").and_then(|v| v.as_str()).unwrap_or("p2") {
        "p0" | "p1" => Some("Now"),
        "p3" => Some("Later"),
        _ => Some("Next"),
    }
}

/// Whether a timestamp field carries an actual stamp. The Python board tests
/// these with bare truthiness (`if entry.get("completed_at")`), which is false
/// for an empty string as well as for null and absent - so a null check alone
/// would classify `""` as Done/Triage where the board would not, and the two
/// boards would name different lanes for the same node.
fn has_stamp(e: &serde_json::Value, field: &str) -> bool {
    e.get(field)
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
}

/// Whether a node's declared dependencies are satisfied, mirroring Python's
/// `compute_readiness` (`cli/src/fno/graph/statuses.py`) - the file is the
/// contract, and the Python side no longer persists `status: "blocked"` at
/// write time, so this reader must derive it itself rather than trust the
/// raw `status` field. Fails closed like the Python: a `blocked_by` id absent
/// from `id_to_entry` counts as blocked, never as satisfied.
fn has_open_dependency(
    e: &serde_json::Value,
    id_to_entry: &HashMap<&str, &serde_json::Value>,
) -> bool {
    let Some(blocked_by) = e.get("blocked_by").and_then(|v| v.as_array()) else {
        return false;
    };
    blocked_by.iter().any(|b| {
        let Some(blocker_id) = b.as_str() else {
            return false; // a malformed entry in the list is not evidence of a real blocker
        };
        match id_to_entry.get(blocker_id) {
            None => true, // unknown dep: fail closed
            Some(blocker) => !has_stamp(blocker, "completed_at"),
        }
    })
}

/// Parent ids whose work is underway: an epic with a done or claimed child.
/// Mirrors `in_progress_epic_ids` in `graph/render.py` - sessions claim an epic's
/// leaf CHILDREN, never the container, so an in-progress epic carries no claim of
/// its own and would otherwise sit in its priority column.
fn in_progress_epics(entries: &[serde_json::Value]) -> HashSet<&str> {
    let mut underway = HashSet::new();
    for e in entries {
        let Some(parent) = e.get("parent").and_then(|v| v.as_str()) else {
            continue;
        };
        let done = has_stamp(e, "completed_at") || node_status(e) == Some("claimed");
        if done {
            underway.insert(parent);
        }
    }
    underway
}

impl Queue {
    /// Every queue card the graph held, cap included.
    pub fn total(&self) -> usize {
        self.lanes.iter().map(|(_, n)| n).sum()
    }
}

/// [`derive_cards`] plus the uncapped lane counts. The single derivation;
/// `derive_cards` is the cards-only view of it.
///
/// `live` is the claim sweep's node-id -> holder map (x-54fa). It is folded in
/// HERE, not applied to the finished card list, so a claim reaches the lane
/// counts too - those are computed over every queue node, and a card past the
/// render cap would otherwise be counted in the wrong lane.
///
/// `scope` filters by project BEFORE the lane counts and the render cap, not
/// after: a cap applied to every project's cards and then filtered would starve
/// this project's board of anything past the 40th card in the global order.
pub fn derive_queue(
    raw: &str,
    live: Option<&HashMap<String, String>>,
    scope: &BoardScope,
) -> Option<Queue> {
    let doc: serde_json::Value = serde_json::from_str(raw).ok()?;
    let entries = doc
        .get("entries")
        .or_else(|| doc.get("nodes"))?
        .as_array()?;
    let underway = in_progress_epics(entries);
    let id_to_entry: HashMap<&str, &serde_json::Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(|id| (id, e)))
        .collect();

    // (project, is_unscoped, rank, prio, created_at, card) - the tuple carries
    // the sort keys so the comparator never re-reads the JSON.
    let mut rows: Vec<(String, bool, Option<f64>, u8, String, BacklogCard)> =
        Vec::with_capacity(entries.len().min(CARD_CAP * 2));
    for e in entries {
        let raw_status = node_status(e).unwrap_or("");
        // done/superseded/deferred/in_review are direct facts about THIS node
        // and outrank blocked, exactly as in the Python overlay
        // (_apply_graph_defaults) - everything else is a candidate for the
        // blocked override, including idea/design, which `classify` would
        // otherwise drop before a blocked reason ever gets a chance to show.
        let terminal = matches!(raw_status, "done" | "superseded" | "deferred" | "in_review");
        let status = if !terminal && has_open_dependency(e, &id_to_entry) {
            "blocked"
        } else {
            raw_status
        };
        let Some(state) = classify(status) else {
            continue;
        };
        let Some(id) = e.get("id").and_then(|v| v.as_str()) else {
            continue; // a node with no id is unrenderable; skip it
        };
        let slug = e.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let priority = e.get("priority").and_then(|v| v.as_str()).unwrap_or("p2");
        // Unscoped (null/absent/empty/whitespace project) sorts into the last
        // lane, like the board's UNSCOPED_LABEL - trim so `""`/`"  "` are not
        // treated as a named lane sorting before real projects.
        let project = e
            .get("project")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let unscoped = project.is_none();
        if !scope.keeps(project) {
            continue;
        }
        let rank = e.get("rank").and_then(|v| v.as_f64());
        let created = e.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        // The board's column authority (x-1d91). DERIVED, never read: no node
        // carries a column field - `_kanban_column` is a function of intent in
        // `graph/render.py`, and this mirrors it.
        let claimed = status == "claimed" || live.is_some_and(|l| l.contains_key(id));
        // `None` EXCLUDES the node from the board (a roadmap row), exactly as it
        // does in the Python. Dropping the card here rather than bucketing it as
        // unlaned keeps the two boards agreeing on what is even on the board -
        // an excluded node rendered as an actionable card would be a row the
        // canonical board says does not exist.
        let Some(lane) = kanban_column(e, claimed, underway.contains(id)) else {
            continue;
        };
        rows.push((
            project.unwrap_or("").to_string(),
            unscoped,
            rank,
            priority_rank(priority),
            created.to_string(),
            BacklogCard {
                id: id.to_string(),
                slug: slug.to_string(),
                priority: priority.to_string(),
                // A live lockfile claim marks a node in flight even when its
                // graph `status` never says so (x-54fa / x-4845): the claim is
                // the fact, the status is a report.
                state: if claimed { CardState::InFlight } else { state },
                // Routes are a publish-time server join (panes/registry),
                // not graph state - the reader always derives them empty.
                pane_id: None,
                attach_id: None,
                where_hint: None,
                project: project.map(str::to_string),
                lane: Some(lane.to_string()),
                plan_path: e
                    .get("plan_path")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                // Set below, once the board order is known.
                head: false,
            },
        ));
    }

    rows.sort_by(|a, b| {
        // Lane: named projects alphabetical, the unscoped lane last.
        a.1.cmp(&b.1)
            .then_with(|| a.0.cmp(&b.0))
            // Rank band: any finite rank (band 0, ascending) before unranked.
            .then_with(|| rank_band(a.2).cmp(&rank_band(b.2)))
            .then_with(|| match (a.2, b.2) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.3.cmp(&b.3))
            .then_with(|| a.4.cmp(&b.4))
    });

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (.., card) in &rows {
        *counts
            .entry(card.lane.as_deref().unwrap_or(UNLANED))
            .or_default() += 1;
    }
    let mut lanes: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(l, n)| (l.to_string(), n))
        .collect();
    // Canonical board order, not alphabetical: the overlay reads left-to-right
    // as a lifecycle, and sorting by name would render it backwards
    // (Later, Next, Now).
    lanes.sort_by_key(|(l, _)| (lane_rank(l), l.clone()));
    let mut cards: Vec<BacklogCard> = rows.into_iter().take(CARD_CAP).map(|r| r.5).collect();
    mark_head(&mut cards);
    // A fresh derivation is by definition current; only `ReaderState` (which
    // knows the read history) ever sets this.
    Some(Queue {
        cards,
        lanes,
        stale: false,
    })
}

/// Mark the head of the queue: the first Ready card in BOARD order. Runs after
/// the live-claim fold, so a claimed head hands the marker to the next genuinely
/// -ready card rather than pointing at work already in flight.
///
/// This is the board's head, NOT the dispatcher's next pick - see
/// [`BacklogCard::head`] for why the two orderings differ.
fn mark_head(cards: &mut [BacklogCard]) {
    let mut seen = false;
    for c in cards.iter_mut() {
        c.head = !seen && c.state == CardState::Ready;
        seen |= c.head;
    }
}

/// node id -> `pr_number` from the same graph read `derive_cards`
/// consumes, for the peek header's `PR #N` label (server-joins holder -> node ->
/// pr at layout time). A sibling of `derive_cards`: parses `entries[].pr_number`
/// via `.as_u64()`, so a string/float/absent value is skipped (matching
/// `AgentRow.pr: Option<u64>`). Pure; a malformed doc yields an empty map (the
/// label simply never appears). `pr_number` is NOT unique across entries, but the
/// map is keyed by node id, so that is irrelevant.
pub fn derive_pr_map(raw: &str) -> HashMap<String, u64> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(raw) else {
        return HashMap::new();
    };
    let Some(entries) = doc
        .get("entries")
        .or_else(|| doc.get("nodes"))
        .and_then(|v| v.as_array())
    else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for e in entries {
        let (Some(id), Some(pr)) = (
            e.get("id").and_then(|v| v.as_str()),
            e.get("pr_number").and_then(|v| v.as_u64()),
        ) else {
            continue;
        };
        out.insert(id.to_string(), pr);
    }
    out
}

/// One active mission (an epic with `mission_active: true`): its slug names the
/// squad, `done`/`total` count its leaf descendants. Counts are recomputed here,
/// not read - the graph node never carries them (they land on the plan doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mission {
    pub epic_id: String,
    pub slug: String,
    pub done: u32,
    pub total: u32,
}

/// Active missions from one graph read: the headers to render, plus a
/// `node id -> epic id` index that groups a worker row into its mission by
/// ancestor (an epic is never its own member). `None` on a malformed document,
/// so the caller renders workers ungrouped rather than hiding them; an empty map
/// is the valid "nothing active" state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissionMap {
    pub missions: Vec<Mission>,
    pub node_to_epic: HashMap<String, String>,
}

/// Depth cap for the rollup recursion (the ancestor walk relies on its `seen`
/// guard alone). The mission tree is only mission -> epic -> leaf; the slack
/// plus the `seen` set terminates a malformed epic-parent cycle.
const MISSION_DEPTH_CAP: usize = 8;

/// Consecutive failed reads before the section is marked stale. The reader ticks
/// about once a second, so this is a few seconds of a genuinely unreadable graph -
/// past any single write race, well short of the operator acting on old work.
const STALE_AFTER_FAILED_READS: u32 = 3;

struct MissionNode<'a> {
    parent: Option<&'a str>,
    slug: &'a str,
    is_epic: bool,
    mission_active: bool,
    done: bool,
}

/// Derive the active missions from raw graph JSON. Pure; see [`MissionMap`].
pub fn derive_missions(raw: &str) -> Option<MissionMap> {
    let doc: serde_json::Value = serde_json::from_str(raw).ok()?;
    let entries = doc
        .get("entries")
        .or_else(|| doc.get("nodes"))?
        .as_array()?;

    let mut nodes: HashMap<&str, MissionNode> = HashMap::with_capacity(entries.len());
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entries {
        let Some(id) = e.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let parent = e.get("parent").and_then(|v| v.as_str());
        if let Some(p) = parent {
            children.entry(p).or_default().push(id);
        }
        nodes.insert(
            id,
            MissionNode {
                parent,
                slug: e.get("slug").and_then(|v| v.as_str()).unwrap_or(""),
                is_epic: e.get("type").and_then(|v| v.as_str()) == Some("epic"),
                mission_active: e.get("mission_active").and_then(|v| v.as_bool()) == Some(true),
                done: node_status(e) == Some("done"),
            },
        );
    }

    let active: HashSet<&str> = nodes
        .iter()
        .filter(|(_, n)| n.mission_active)
        .map(|(id, _)| *id)
        .collect();
    if active.is_empty() {
        return Some(MissionMap::default());
    }

    // Nearest active-mission ancestor; start at the parent so the epic is never
    // its own member. The full parent chain is walked - mission scope is all
    // transitive descendants - and the `seen` set is the only bound (it makes
    // even a malformed parent cycle terminate); a fixed depth cap would drop a
    // deeply-nested but valid worker.
    let mut node_to_epic = HashMap::new();
    for (&id, node) in &nodes {
        let mut cur = node.parent;
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(a) = cur {
            if !seen.insert(a) {
                break; // cycle
            }
            if active.contains(a) {
                node_to_epic.insert(id.to_string(), a.to_string());
                break;
            }
            cur = nodes.get(a).and_then(|n| n.parent);
        }
    }

    let mut missions: Vec<Mission> = active
        .iter()
        .map(|&epic| {
            let (done, total) = rollup(epic, &nodes, &children, &mut HashSet::new(), 0);
            Mission {
                epic_id: epic.to_string(),
                slug: nodes.get(epic).map(|n| n.slug).unwrap_or("").to_string(),
                done,
                total,
            }
        })
        .collect();
    // Deterministic sideline order (the active set iterates arbitrarily).
    missions.sort_by(|a, b| a.epic_id.cmp(&b.epic_id));
    Some(MissionMap {
        missions,
        node_to_epic,
    })
}

/// Leaf done/total under `epic`: a leaf child counts once (done iff `status ==
/// "done"`); an epic child recurses and folds its leaves in, never counting an
/// epic as a unit. `seen`/`depth` bound a malformed parent cycle.
fn rollup<'a>(
    epic: &'a str,
    nodes: &HashMap<&'a str, MissionNode<'a>>,
    children: &HashMap<&'a str, Vec<&'a str>>,
    seen: &mut HashSet<&'a str>,
    depth: usize,
) -> (u32, u32) {
    if depth >= MISSION_DEPTH_CAP || !seen.insert(epic) {
        return (0, 0);
    }
    let (mut done, mut total) = (0u32, 0u32);
    for &child in children.get(epic).map(Vec::as_slice).unwrap_or(&[]) {
        let Some(cn) = nodes.get(child) else { continue };
        if cn.is_epic {
            let (d, t) = rollup(child, nodes, children, seen, depth + 1);
            done += d;
            total += t;
        } else {
            total += 1;
            if cn.done {
                done += 1;
            }
        }
    }
    (done, total)
}

/// Ranked nodes (band 0) sort ahead of unranked ones (band 1).
fn rank_band(rank: Option<f64>) -> u8 {
    match rank {
        Some(r) if r.is_finite() => 0,
        _ => 1,
    }
}

/// Parse `fno-agents claim sweep --json` stdout into the live-claim map the
/// overlay consumes: node id -> claim holder, for claims whose `state` is
/// `"live"` under a `node:` / `dispatch:` key (x-54fa). Only `"live"` counts
/// as in-flight (Locked 2); the sweep's other states (`stale`/`suspect`/...)
/// never flip a card. `None` on unparseable output so the caller keeps its
/// last-good sweep — a flaky tick must not downgrade in-flight cards.
///
/// The mux deliberately parses only this pinned JSON verdict: claim YAML,
/// classification, and liveness live in `fno-agents` alone.
pub fn live_claims_from_sweep(stdout: &str) -> Option<HashMap<String, String>> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let arr = v.get("claims")?.as_array()?;
    let mut live: HashMap<String, String> = HashMap::new();
    for c in arr {
        if c.get("state").and_then(|s| s.as_str()) != Some("live") {
            continue;
        }
        let Some(key) = c.get("key").and_then(|k| k.as_str()) else {
            continue;
        };
        let holder = c
            .get("holder")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        // A node held by both a `dispatch:` and a `node:` claim keeps the
        // `node:` holder (the worker session) for display; either alone
        // marks the id in-flight.
        if let Some(id) = key.strip_prefix("node:") {
            live.insert(id.to_string(), holder);
        } else if let Some(id) = key.strip_prefix("dispatch:") {
            live.entry(id.to_string()).or_insert(holder);
        }
    }
    Some(live)
}

/// The reader's between-tick memory (mtime-gated document cache + last-sent
/// cards), mirroring [`crate::agents_view::ReaderState`]. The interval task
/// lives in server.rs (it owns the `CoreMsg` sender); this keeps the derivation
/// pure and unit-testable.
#[derive(Default)]
pub struct ReaderState {
    cached_raw: Option<String>,
    cached_stamp: Option<(std::time::SystemTime, u64)>,
    last_sent: Option<Queue>,
    /// The live-claims map as of the last publish. Holders ride the publish
    /// (they feed the server's `where_hint` join), so a holder-only change -
    /// same card states, different/new holder - must republish too, not wait
    /// for a card flip (codex peer review of the v18 routes).
    last_live: Option<HashMap<String, String>>,
    /// node id -> pr_number, recomputed ONLY when the graph read
    /// refreshes (not per tick), so a second full JSON parse per second is
    /// avoided. Cloned onto every publish; a pr-only change (a node gets a
    /// pr_number, same card set + holders) still republishes via the gate below.
    pr: HashMap<String, u64>,
    /// The pr map as of the last publish, so a pr-only change is detected.
    last_pr: Option<HashMap<String, u64>>,
    /// Active missions, recomputed only on a fresh read (mirrors `pr`).
    missions: MissionMap,
    /// The mission map as of the last publish, so a mission-only change is
    /// detected (a mission activating/completing with the same cards/prs).
    last_missions: Option<MissionMap>,
    /// Consecutive ticks whose read failed while the file was still there. Feeds
    /// [`Queue::stale`]; reset by any read that lands.
    read_failures: u32,
    /// Which projects this board renders (x-20f1). Fixed for the reader's life:
    /// the server resolves it once at birth, so a card set never changes shape
    /// under a live board for a reason the operator cannot see.
    scope: BoardScope,
}

impl ReaderState {
    /// A reader scoped to `scope`. `ReaderState::default()` keeps
    /// [`BoardScope::All`], the historical whole-graph board.
    pub fn with_scope(scope: BoardScope) -> Self {
        Self {
            scope,
            ..Self::default()
        }
    }

    /// The stamp of the currently-cached document (mtime+len gate: the caller
    /// skips the file read when this matches a fresh stat).
    pub fn cached_stamp(&self) -> Option<(std::time::SystemTime, u64)> {
        self.cached_stamp
    }

    /// One tick: fold in a fresh stat/read (both taken OFF the core loop),
    /// overlay live claims, and return the card set to publish, or `None` when
    /// nothing changed. A malformed document keeps the last-good cards (a torn
    /// concurrent write must not blank the lane); a vanished file empties them.
    ///
    /// `live` is the last-good claim sweep (`None` = no sweep has ever
    /// succeeded: render un-overlaid, today's behavior). The overlay applies
    /// INSIDE the change gate so a claim appearing/releasing republishes even
    /// when the graph file itself is untouched (x-54fa AC1-HP / AC1-EDGE).
    pub fn tick(
        &mut self,
        stamp: Option<(std::time::SystemTime, u64)>,
        read_if_changed: impl FnOnce() -> Option<String>,
        live: Option<&HashMap<String, String>>,
    ) -> Option<(Queue, HashMap<String, u64>, MissionMap)> {
        // Whether THIS tick pulled fresh bytes off disk. Only a fresh read that
        // also parses clears the failure counter: re-deriving the cached document
        // succeeds every tick by definition, so treating that as success would
        // erase a torn-read run before it ever reached the stale threshold.
        let mut fresh_read = false;
        if stamp != self.cached_stamp {
            match (read_if_changed(), stamp) {
                // Only commit the new stamp once we have matching content, so a
                // torn read (raced a writer: file exists but read yielded None)
                // is RE-TRIED on the next tick instead of pinning the stale card
                // set until the graph's mtime changes again (gemini HIGH; this
                // improves on the older agents_view reader's advance-anyway).
                (Some(raw), _) => {
                    self.cached_stamp = stamp;
                    fresh_read = true;
                    // The pr map derives from the SAME read; recompute it
                    // only here, not per tick, so we never parse the 4M graph
                    // twice a second.
                    self.pr = derive_pr_map(&raw);
                    self.missions = derive_missions(&raw).unwrap_or_default();
                    self.cached_raw = Some(raw);
                }
                (None, None) => {
                    self.cached_stamp = stamp;
                    self.pr = HashMap::new();
                    self.missions = MissionMap::default();
                    self.cached_raw = None; // file vanished: empty the lane
                }
                // Torn read: keep last-good AND retry next tick. A single one is
                // a normal race with a writer, so only a RUN of them earns the
                // stale marker - a marker that flashes every time the graph is
                // written would be noise, not signal.
                (None, Some(_)) => self.read_failures = self.read_failures.saturating_add(1),
            }
        }
        // Re-derived every tick (not only on a fresh read) so a claim
        // appearing/releasing reaches the cards, their lanes, AND the lane counts
        // even when the graph file itself never changed.
        //
        // A read that lands but will not PARSE counts as a failure too. It
        // commits a stamp, so treating a committed stamp as success would reset
        // the counter and leave a persistently corrupt graph showing old cards
        // forever with no stale marker - the reader cannot derive current state
        // either way, which is exactly what the marker is for.
        let mut queue = match &self.cached_raw {
            Some(raw) => match derive_queue(raw, live, &self.scope) {
                Some(q) => {
                    if fresh_read {
                        self.read_failures = 0;
                    }
                    q
                }
                None => {
                    self.read_failures = self.read_failures.saturating_add(1);
                    self.last_sent.clone().unwrap_or_default()
                }
            },
            None => {
                // A vanished file is a KNOWN state (an empty lane), not a
                // failure to read one.
                self.read_failures = 0;
                Queue::default()
            }
        };
        queue.stale = self.read_failures >= STALE_AFTER_FAILED_READS;
        // A holder-only change (same cards, new/different claim holder) also
        // republishes: the holders map travels with the cards and feeds the
        // publish-time `where_hint` join. `live: None` (no sweep yet, or a
        // failing sweep with the caller retaining last-good) is never a
        // change - retention must not churn publishes.
        let live_changed = live.is_some_and(|l| self.last_live.as_ref() != Some(l));
        if live_changed {
            self.last_live = live.cloned();
        }
        // A pr-only change (a node gains a pr_number while its card + holder stay
        // put) must republish too, else the `PR #N` label would lag until an
        // unrelated card/claim flip.
        let pr_changed = self.last_pr.as_ref() != Some(&self.pr);
        let missions_changed = self.last_missions.as_ref() != Some(&self.missions);
        if live_changed || pr_changed || missions_changed || self.last_sent.as_ref() != Some(&queue)
        {
            self.last_sent = Some(queue.clone());
            self.last_pr = Some(self.pr.clone());
            self.last_missions = Some(self.missions.clone());
            Some((queue, self.pr.clone(), self.missions.clone()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(nodes: &str) -> String {
        format!(r#"{{"entries": [{nodes}]}}"#)
    }

    /// The graph as five projects' work plus one unscoped node, the shape the
    /// operator actually reads (x-20f1).
    fn multi_project_graph() -> String {
        graph(
            r#"{"id":"x-fno","slug":"f","priority":"p1","status":"ready","project":"fno"},
               {"id":"x-etl","slug":"e","priority":"p1","status":"ready","project":"etl"},
               {"id":"x-web","slug":"w","priority":"p1","status":"ready","project":"web"},
               {"id":"x-pad","slug":"p","priority":"p1","status":"ready","project":"   "},
               {"id":"x-non","slug":"n","priority":"p1","status":"ready"}"#,
        )
    }

    fn scoped_ids(scope: &BoardScope) -> Vec<String> {
        derive_queue(&multi_project_graph(), None, scope)
            .unwrap()
            .cards
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn board_scope_keeps_one_project_and_never_hides_the_unscoped_lane() {
        // AC1-HP: scoped to fno, only fno's card and the unscoped lane render.
        // AC1-EDGE: `project` absent AND `project` whitespace both count as
        // unscoped and survive every scope - a node with no project is not
        // another project's work, so filtering must never make it disappear.
        let scope = BoardScope::Projects(HashSet::from(["fno".to_string()]));
        let mut ids = scoped_ids(&scope);
        ids.sort();
        assert_eq!(ids, vec!["x-fno", "x-non", "x-pad"]);
    }

    #[test]
    fn board_scope_all_is_the_historical_whole_graph_board() {
        assert_eq!(scoped_ids(&BoardScope::All).len(), 5);
        assert_eq!(scoped_ids(&BoardScope::default()).len(), 5);
    }

    #[test]
    fn an_unresolvable_scope_narrows_to_unscoped_rather_than_widening() {
        // The refusal: an empty project set shows the unscoped lane only. The
        // failure this exists to prevent is the opposite one - a selector that
        // could not resolve falling back to every project's work.
        let mut ids = scoped_ids(&BoardScope::Projects(HashSet::new()));
        ids.sort();
        assert_eq!(ids, vec!["x-non", "x-pad"]);
    }

    #[test]
    fn board_scope_filters_before_the_render_cap() {
        // A cap applied to the GLOBAL order and filtered afterwards would starve
        // a scoped board: here 60 foreign cards sort ahead of the one this
        // project cares about, so a post-cap filter renders nothing at all.
        let mut nodes: Vec<String> = (0..60)
            .map(|i| {
                format!(
                    r#"{{"id":"x-e{i:02}","slug":"e","priority":"p0","status":"ready","project":"etl"}}"#
                )
            })
            .collect();
        nodes.push(
            r#"{"id":"x-mine","slug":"m","priority":"p3","status":"ready","project":"fno"}"#
                .to_string(),
        );
        let raw = graph(&nodes.join(","));
        let scope = BoardScope::Projects(HashSet::from(["fno".to_string()]));
        let cards = derive_queue(&raw, None, &scope).unwrap().cards;
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, "x-mine");
    }

    #[test]
    fn board_scope_lane_counts_are_scoped_too() {
        // The lane counts are computed over every QUEUE node, uncapped, so a
        // filter applied only to the card list would leave the mini-kanban
        // counting five projects' work under a one-project board.
        let scope = BoardScope::Projects(HashSet::from(["fno".to_string()]));
        let q = derive_queue(&multi_project_graph(), None, &scope).unwrap();
        assert_eq!(q.total(), 3);
    }

    #[test]
    fn the_server_reads_only_what_the_client_latched() {
        // The server resolves nothing: a subprocess on its startup path delayed
        // shutdown past the SIGTERM grace and perturbed multiclient frame
        // ordering. It parses the env the client already resolved.
        let key = "FNO_BOARD_SCOPE";
        let guard = std::env::var(key).ok();

        std::env::set_var(key, "fno,etl");
        let (scope, why) = board_scope_from_spawn_env();
        assert_eq!(
            scope,
            BoardScope::Projects(HashSet::from(["fno".to_string(), "etl".to_string()]))
        );
        assert!(why.contains("latched at spawn"), "{why}");

        // No latch means a hand-started `fno mux server`. That gets the
        // historical board, never a silent narrowing, and says so.
        std::env::remove_var(key);
        let (scope, why) = board_scope_from_spawn_env();
        assert_eq!(scope, BoardScope::All);
        assert!(why.contains("not latched"), "{why}");

        match guard {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn the_wire_round_trips_what_the_client_resolved() {
        // What the client latches is what the server reads, or the scope the
        // operator configured is not the board they get.
        for scope in [
            BoardScope::All,
            BoardScope::Projects(HashSet::from(["fno".to_string()])),
            BoardScope::Projects(HashSet::from(["fno".to_string(), "etl".to_string()])),
        ] {
            assert_eq!(parse_board_scope(&board_scope_wire(&scope)), scope);
        }
        // The refusal serializes to empty, which the server reads as "not
        // latched" and widens to All. Deliberate: an unresolvable scope must
        // not silently narrow a board nobody configured. `mux doctor` reports
        // the refusal with its remedy.
        let refused = BoardScope::Projects(HashSet::new());
        assert_eq!(board_scope_wire(&refused), "");
    }

    #[test]
    fn parse_board_scope_reads_the_wire_value() {
        assert_eq!(parse_board_scope("all"), BoardScope::All);
        assert_eq!(parse_board_scope("  all "), BoardScope::All);
        assert_eq!(
            parse_board_scope("fno, etl ,"),
            BoardScope::Projects(HashSet::from(["fno".to_string(), "etl".to_string()]))
        );
        // The refusal spelling: nothing to keep, so nothing but the unscoped
        // lane renders.
        assert_eq!(parse_board_scope(""), BoardScope::Projects(HashSet::new()));
    }

    #[test]
    fn workspace_projects_reads_the_config_block_or_refuses() {
        let json = r#"{"projects":[{"name":"web"},{"name":" etl "},{"name":""}]}"#;
        assert_eq!(
            workspace_projects(json),
            Some(HashSet::from(["web".to_string(), "etl".to_string()]))
        );
        // Every miss is `None`, and every `None` is a refusal upstream - never
        // a fall-back to all projects.
        assert_eq!(workspace_projects("{}"), None);
        assert_eq!(workspace_projects(r#"{"projects":[]}"#), None);
        assert_eq!(workspace_projects(r#"{"projects":[{"name":"  "}]}"#), None);
        assert_eq!(workspace_projects("not json"), None);
    }

    #[test]
    fn resolve_board_scope_defaults_to_the_repo_project() {
        let (scope, why) = resolve_board_scope(|k| match k {
            "mux.board_scope" => Some("repo\n".into()),
            "project.id" => Some("fno\n".into()),
            _ => None,
        });
        assert_eq!(
            scope,
            BoardScope::Projects(HashSet::from(["fno".to_string()]))
        );
        assert!(why.contains("fno"), "{why}");
    }

    #[test]
    fn resolve_board_scope_expands_a_workspace_and_refuses_an_empty_one() {
        let ws = r#"{"projects":[{"name":"web"},{"name":"etl"},{"name":"fno"}]}"#;
        let (scope, _) = resolve_board_scope(|k| match k {
            "mux.board_scope" => Some("workspace:main".into()),
            "work.workspaces.main" => Some(ws.into()),
            _ => None,
        });
        assert_eq!(
            scope,
            BoardScope::Projects(HashSet::from([
                "web".to_string(),
                "etl".to_string(),
                "fno".to_string()
            ]))
        );
        // An unreadable / absent workspace REFUSES. The whole point: a config
        // typo must not silently restore the five-project board.
        let (scope, why) = resolve_board_scope(|k| match k {
            "mux.board_scope" => Some("workspace:typo".into()),
            _ => None,
        });
        assert_eq!(scope, BoardScope::Projects(HashSet::new()));
        assert!(why.contains("work.workspaces.typo"), "{why}");
    }

    #[test]
    fn resolve_board_scope_refuses_rather_than_widening() {
        // No project.id resolves (a cwd outside any repo): resolve to the empty
        // set, and SAY which board that produces.
        let (scope, why) = resolve_board_scope(|_| None);
        assert_eq!(scope, BoardScope::Projects(HashSet::new()));
        assert!(why.contains("project.id"), "{why}");
        // An out-of-vocabulary setting is a refusal too, not a coerce-to-all.
        let (scope, why) =
            resolve_board_scope(|k| (k == "mux.board_scope").then(|| "everything".to_string()));
        assert_eq!(scope, BoardScope::Projects(HashSet::new()));
        assert!(why.contains("repo|all|workspace:<name>"), "{why}");
    }

    #[test]
    fn every_refusal_reason_names_the_board_it_actually_produces() {
        // The reason is the ONLY place the fallback is visible: `mux doctor`
        // prints it verbatim and the server logs it. Each of these three paths
        // resolves to the empty set, which the client writes to the wire as ""
        // and the server reads back as All - so a reason promising an
        // unscoped-only board would be describing a board nobody gets. That is
        // the receipt-can-lie shape, and it shipped here once already.
        let refusals = [
            resolve_board_scope(|_| None),
            resolve_board_scope(|k| (k == "mux.board_scope").then(|| "everything".to_string())),
            resolve_board_scope(|k| (k == "mux.board_scope").then(|| "workspace:typo".to_string())),
        ];
        for (scope, why) in &refusals {
            assert_eq!(*scope, BoardScope::Projects(HashSet::new()), "{why}");
            assert!(
                why.contains("EVERY project"),
                "refusal reason must name the board it produces: {why}"
            );
        }
    }

    #[test]
    fn a_latched_refusal_widens_at_the_server_rather_than_blanking_the_board() {
        // The client ALWAYS sets the var, so a refusal arrives as a set-but-
        // empty value, not an absent one. Both must read as "nothing latched":
        // narrowing here would leave an upgrading user with a board showing
        // only their handful of unscoped cards and no visible cause.
        let key = "FNO_BOARD_SCOPE";
        let guard = std::env::var(key).ok();

        let refused = BoardScope::Projects(HashSet::new());
        std::env::set_var(key, board_scope_wire(&refused));
        let (scope, why) = board_scope_from_spawn_env();
        assert_eq!(scope, BoardScope::All);
        assert!(why.contains("not latched"), "{why}");

        match guard {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn resolve_board_scope_honors_all() {
        let (scope, _) =
            resolve_board_scope(|k| (k == "mux.board_scope").then(|| "all".to_string()));
        assert_eq!(scope, BoardScope::All);
    }

    #[test]
    fn external_backend_selected_follows_the_python_resolution() {
        // Same resolution as fno.tracker.active_backend_name: unset -> graph,
        // "graph" -> graph, anything else -> external. The two sides must
        // never disagree about which store is live.
        let key = "FNO_TRACKER_BACKEND";
        let guard = std::env::var(key).ok();
        std::env::remove_var(key);
        assert!(!external_backend_selected());
        std::env::set_var(key, "graph");
        assert!(!external_backend_selected());
        std::env::set_var(key, "github");
        assert!(external_backend_selected());
        match guard {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn snapshot_document_feeds_the_same_derivation() {
        // The backend-neutral snapshot (`fno backlog status --snapshot`) is the
        // same document shape the graph file is: entries[] with graph-compatible
        // fields plus an extra `backend` key the reader must tolerate. Readiness
        // stays derived: a closed blocker arrives as a tombstone row, and an
        // open blocker keeps the card Blocked.
        let raw = r#"{
            "backend": "github",
            "entries": [
                {"id":"EXT-1","slug":"free","title":"Free work","status":"ready",
                 "priority":"p1","blocked_by":[],"parent":null,
                 "plan_path":"/plans/one.md","pr_number":null,"created_at":"2026-01-02T00:00:00Z"},
                {"id":"EXT-2","slug":"waiting","title":"Waiting","status":"ready",
                 "priority":"p1","blocked_by":["EXT-9"],
                 "created_at":"2026-01-03T00:00:00Z"},
                {"id":"EXT-3","slug":"reviewed","title":"In review","status":"in_review",
                 "priority":"p1","blocked_by":[],"pr_number":31,
                 "created_at":"2026-01-04T00:00:00Z"},
                {"id":"EXT-9","status":"done","completed_at":"closed"}
            ]
        }"#;
        let queue = derive_queue(raw, None, &BoardScope::All).unwrap();
        let ids: Vec<_> = queue.cards.iter().map(|c| c.id.as_str()).collect();
        // EXT-2 sorts after EXT-1 on created_at; the tombstone and the
        // in-review row never render as cards.
        assert_eq!(ids, ["EXT-1", "EXT-2"]);
        assert_eq!(queue.cards[0].state, CardState::Ready);
        // EXT-9 is closed (tombstone), so EXT-2's dependency is satisfied and
        // it renders Ready, not Blocked - the read-time derivation resolving
        // through tombstone evidence rather than a stored flag.
        assert_eq!(queue.cards[1].state, CardState::Ready);
        assert_eq!(queue.cards[0].plan_path.as_deref(), Some("/plans/one.md"));
        // the pr map derives from the same document.
        let prs = derive_pr_map(raw);
        assert_eq!(prs.get("EXT-3"), Some(&31u64));
        assert!(!prs.contains_key("EXT-1"));
    }

    #[test]
    fn snapshot_mode_failures_keep_last_good_and_clear_on_recovery() {
        // A failing snapshot exec is a read failure like a torn file read:
        // the run counts toward the stale threshold without publishing, and a
        // landed read clears it (Risk 4: a backend outage is never a blank
        // mux, and recovery is immediate).
        let mut state = ReaderState::default();
        let stamp = Some((std::time::SystemTime::UNIX_EPOCH, 0u64));
        let first = state.tick(
            stamp,
            || {
                Some(
                    r#"{"entries":[{"id":"E","slug":"e","status":"ready","priority":"p1"}]}"#
                        .into(),
                )
            },
            None,
        );
        assert!(first.is_some());
        // Three failing refreshes with strictly increasing stamps (the exec
        // path mints a new clock stamp per window; a failed read commits none
        // of them, so every window differs from the cached stamp). The first
        // two publish nothing; the third crosses STALE_AFTER_FAILED_READS and
        // publishes exactly once, with the stale marker set - that is how the
        // marker reaches the UI while the cards stay last-good.
        let mut stale_publish: Option<Queue> = None;
        for i in 1..=3u64 {
            let s = Some((std::time::UNIX_EPOCH + std::time::Duration::from_secs(i), 0));
            let out = state.tick(s, || None, None);
            if i < 3 {
                assert!(out.is_none(), "failures below the threshold never publish");
            } else {
                let (queue, _, _) = out.expect("the stale crossing publishes");
                assert!(queue.stale, "the crossing publish carries the stale marker");
                assert!(
                    !queue.cards.is_empty(),
                    "last-good cards survive the outage"
                );
                stale_publish = Some(queue);
            }
        }
        assert!(stale_publish.is_some());
        let recovered = state.tick(
            Some((std::time::UNIX_EPOCH + std::time::Duration::from_secs(9), 0)),
            || {
                Some(
                    r#"{"entries":[{"id":"E","slug":"e","status":"ready","priority":"p1"}]}"#
                        .into(),
                )
            },
            None,
        );
        let (queue, _, _) = recovered.expect("recovery republishes");
        assert!(!queue.stale, "a landed read clears the failure run");
    }

    #[test]
    fn only_queue_states_survive_classification() {
        let raw = graph(
            r#"{"id":"a","slug":"ready-one","priority":"p1","status":"ready"},
               {"id":"b","slug":"blocked-one","priority":"p2","status":"blocked"},
               {"id":"c","slug":"live-one","priority":"p0","status":"claimed"},
               {"id":"d","slug":"done-one","priority":"p1","status":"done"},
               {"id":"e","slug":"idea-one","priority":"p2","status":"idea"}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        let ids: Vec<_> = cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["c", "a", "b"],
            "done/idea dropped; board order by priority"
        );
        assert_eq!(cards[0].state, CardState::InFlight);
        assert_eq!(cards[1].state, CardState::Ready);
        assert_eq!(cards[2].state, CardState::Blocked);
    }

    #[test]
    fn open_blocked_by_overrides_a_persisted_ready_status() {
        // The Python write path no longer persists `status: "blocked"` - it is
        // a read-time-only overlay now - so a real graph.json carries a
        // dependency-blocked node as `status: "ready"`/"idea"/etc with a
        // non-empty `blocked_by`. This reader must derive blocked itself
        // rather than trust the raw status, or the sideline silently drops
        // the Blocked badge for every such node.
        let raw = graph(
            r#"{"id":"done","slug":"d","priority":"p2","status":"done","completed_at":"2026-01-01T00:00:00Z"},
               {"id":"open","slug":"o","priority":"p2","status":"ready"},
               {"id":"blocked-on-open","slug":"b1","priority":"p2","status":"ready","blocked_by":["open"]},
               {"id":"blocked-on-unknown","slug":"b2","priority":"p2","status":"idea","blocked_by":["no-such-id"]},
               {"id":"unblocked","slug":"u","priority":"p2","status":"ready","blocked_by":["done"]}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        let by_id = |id: &str| cards.iter().find(|c| c.id == id).map(|c| c.state);
        assert_eq!(by_id("blocked-on-open"), Some(CardState::Blocked));
        assert_eq!(
            by_id("blocked-on-unknown"),
            Some(CardState::Blocked),
            "an unknown dependency id must fail closed to Blocked, never drop as idea"
        );
        assert_eq!(
            by_id("unblocked"),
            Some(CardState::Ready),
            "a completed blocker must not hold the dependent as blocked"
        );
        assert_eq!(by_id("open"), Some(CardState::Ready));
    }

    #[test]
    fn a_completed_node_stays_done_despite_an_open_blocked_by() {
        // Terminal statuses are direct facts about the node itself and
        // outrank the blocked overlay, exactly as in the Python overlay.
        let raw = graph(
            r#"{"id":"open","slug":"o","priority":"p2","status":"ready"},
               {"id":"finished","slug":"f","priority":"p2","status":"done","completed_at":"2026-01-01T00:00:00Z","blocked_by":["open"]}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        assert!(
            cards.iter().all(|c| c.id != "finished"),
            "done nodes are not queue cards regardless of blocked_by"
        );
    }

    #[test]
    fn board_order_project_then_rank_then_priority() {
        // fno before (unscoped); within fno, ranked before unranked; then prio.
        let raw = graph(
            r#"{"id":"unscoped","slug":"u","priority":"p0","status":"ready"},
               {"id":"fno-unranked","slug":"fu","priority":"p1","status":"ready","project":"fno"},
               {"id":"fno-ranked","slug":"fr","priority":"p3","status":"ready","project":"fno","rank":1.0}"#,
        );
        let ids: Vec<_> = derive_cards(&raw)
            .unwrap()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        // fno lane first: ranked (band 0) beats the higher-priority unranked one,
        // then the unscoped card last.
        assert_eq!(ids, ["fno-ranked", "fno-unranked", "unscoped"]);
    }

    #[test]
    fn empty_or_whitespace_project_sorts_as_unscoped_last() {
        // A `""`/whitespace project must land in the unscoped lane (last), not
        // sort as a named lane before real projects (gemini/codex P2).
        let raw = graph(
            r#"{"id":"blank","slug":"b","priority":"p0","status":"ready","project":"  "},
               {"id":"fno","slug":"f","priority":"p3","status":"ready","project":"fno"}"#,
        );
        let ids: Vec<_> = derive_cards(&raw)
            .unwrap()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(
            ids,
            ["fno", "blank"],
            "named lane first; blank-project last"
        );
    }

    #[test]
    fn malformed_document_is_none_not_empty() {
        assert!(derive_cards("not json").is_none());
        // last-good is kept across a torn write by ReaderState.
        let mut st = ReaderState::default();
        let good = graph(r#"{"id":"a","slug":"s","priority":"p1","status":"ready"}"#);
        let s1 = Some((std::time::SystemTime::UNIX_EPOCH, good.len() as u64));
        assert_eq!(
            st.tick(s1, || Some(good.clone()), None)
                .unwrap()
                .0
                .cards
                .len(),
            1
        );
        // A changed stamp but a torn (None) read keeps the last-good card AND
        // does not commit the new stamp, so the read is retried next tick.
        let s2 = Some((std::time::SystemTime::UNIX_EPOCH, 999));
        assert!(st.tick(s2, || None, None).is_none()); // last-good unchanged -> no republish
                                                       // Retry at the same stamp now succeeds with two cards -> republished
                                                       // (proves the torn read did not pin the stale set).
        let two = graph(
            r#"{"id":"a","slug":"s","priority":"p1","status":"ready"},
               {"id":"b","slug":"t","priority":"p2","status":"blocked"}"#,
        );
        assert_eq!(
            st.tick(s2, || Some(two.clone()), None)
                .unwrap()
                .0
                .cards
                .len(),
            2
        );
    }

    // ---- attribution, on-deck, and the uncapped total (x-1d91) -------------

    #[test]
    fn attribution_derives_the_column_it_never_reads() {
        // No graph node carries a column field - the lane is `_kanban_column`'s
        // intent mapping, computed here to match `graph/render.py`. Getting this
        // wrong renders every attribution blank, so pin the mapping.
        let raw = graph(
            r#"{"id":"x-now","slug":"n","priority":"p1","status":"ready","project":"fno"},
               {"id":"x-next","slug":"x","priority":"p2","status":"ready"},
               {"id":"x-later","slug":"l","priority":"p3","status":"ready"},
               {"id":"x-tri","slug":"t","priority":"p2","status":"ready","queued_at":"2026-07-19T00:00:00Z"},
               {"id":"x-held","slug":"h","priority":"p2","status":"claimed"}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        let lane = |id: &str| {
            cards
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .lane
                .clone()
                .unwrap()
        };
        assert_eq!(lane("x-now"), "Now", "p0/p1 is today-ish");
        assert_eq!(lane("x-next"), "Next");
        assert_eq!(lane("x-later"), "Later");
        assert_eq!(
            lane("x-tri"),
            "Triage",
            "queued awaits ack, never inflates Now"
        );
        assert_eq!(lane("x-held"), "Now", "a claimed node is underway");
        // The project half is the node's own, absent when unscoped.
        let project = |id: &str| cards.iter().find(|c| c.id == id).unwrap().project.clone();
        assert_eq!(project("x-now").as_deref(), Some("fno"));
        assert_eq!(project("x-next"), None);
    }

    #[test]
    fn a_live_claim_moves_the_node_to_now_and_takes_its_count_with_it() {
        // x-4845: a node another session drives holds a live lockfile but may
        // never write a graph status. Folding the claim into the DERIVATION (not
        // onto the finished card list) is what keeps the lane counts honest - a
        // card past the render cap would otherwise be counted in the wrong lane.
        let raw = graph(
            r#"{"id":"x-a","slug":"a","priority":"p3","status":"ready"},
               {"id":"x-b","slug":"b","priority":"p3","status":"ready"}"#,
        );
        let cold = derive_queue(&raw, None, &BoardScope::All).unwrap();
        assert_eq!(cold.lanes, vec![("Later".to_string(), 2)]);
        let hot = derive_queue(
            &raw,
            Some(&live(&[("x-a", "target-session:abc")])),
            &BoardScope::All,
        )
        .unwrap();
        assert_eq!(
            hot.lanes,
            vec![("Now".to_string(), 1), ("Later".to_string(), 1)],
            "the claimed node moves lanes, count and all"
        );
        assert_eq!(hot.cards[0].state, CardState::InFlight);
    }

    #[test]
    fn an_empty_timestamp_is_no_timestamp() {
        // The Python board tests these fields with bare truthiness, so `""` is
        // absent there. A null-only check here would send an empty-stamped node
        // to Done/Triage while the board left it in its priority column - the two
        // boards must never name different lanes for the same node.
        let raw = graph(
            r#"{"id":"x-a","slug":"a","priority":"p2","status":"ready","completed_at":""},
               {"id":"x-b","slug":"b","priority":"p2","status":"ready","queued_at":""},
               {"id":"x-kid","slug":"k","priority":"p2","status":"ready","parent":"x-e","completed_at":""},
               {"id":"x-e","slug":"e","type":"epic","priority":"p2","status":"ready"}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        let lane = |id: &str| cards.iter().find(|c| c.id == id).unwrap().lane.clone();
        assert_eq!(
            lane("x-a").as_deref(),
            Some("Next"),
            "empty completed_at is not Done"
        );
        assert_eq!(
            lane("x-b").as_deref(),
            Some("Next"),
            "empty queued_at is not Triage"
        );
        assert_eq!(
            lane("x-e").as_deref(),
            Some("Next"),
            "an empty-stamped child does not make its epic underway"
        );
    }

    #[test]
    fn an_epic_with_a_claimed_child_is_underway() {
        // Sessions claim an epic's leaf CHILDREN, never the container, so an
        // in-progress epic carries no claim of its own and would otherwise sit in
        // its priority column (mirrors in_progress_epic_ids).
        let raw = graph(
            r#"{"id":"x-epic","slug":"e","type":"epic","priority":"p3","status":"ready"},
               {"id":"x-kid","slug":"k","priority":"p2","status":"claimed","parent":"x-epic"}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        let epic = cards.iter().find(|c| c.id == "x-epic").unwrap();
        assert_eq!(
            epic.lane.as_deref(),
            Some("Now"),
            "a p3 epic with a claimed child is underway, not long-tail"
        );
    }

    #[test]
    fn head_is_the_first_ready_card_only() {
        // AC1-HP: exactly one `next`, on the first READY card in board order -
        // an in-flight card ahead of it never takes the marker.
        let raw = graph(
            r#"{"id":"x-fly","slug":"f","priority":"p0","status":"claimed","project":"fno"},
               {"id":"x-r1","slug":"r1","priority":"p1","status":"ready","project":"fno"},
               {"id":"x-r2","slug":"r2","priority":"p2","status":"ready","project":"fno"}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        let marked: Vec<&str> = cards
            .iter()
            .filter(|c| c.head)
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(marked, ["x-r1"], "one on-deck card, the first ready one");
    }

    #[test]
    fn claiming_the_head_card_hands_the_marker_down() {
        // The overlay can flip on-deck to InFlight; the section must then point
        // at the next card that is genuinely up for grabs, not at work already
        // running.
        let mut st = ReaderState::default();
        let raw = graph(
            r#"{"id":"x-r1","slug":"r1","priority":"p1","status":"ready","project":"fno"},
               {"id":"x-r2","slug":"r2","priority":"p2","status":"ready","project":"fno"}"#,
        );
        let s = Some((std::time::SystemTime::UNIX_EPOCH, raw.len() as u64));
        let first = st.tick(s, || Some(raw.clone()), None).unwrap().0;
        assert!(first.cards[0].head && !first.cards[1].head);
        let claimed = live(&[("x-r1", "target-session:abc")]);
        let after = st.tick(s, || None, Some(&claimed)).unwrap().0;
        assert!(
            !after.cards[0].head && after.cards[1].head,
            "the marker moves to the next ready card"
        );
    }

    #[test]
    fn lanes_render_in_canonical_board_order() {
        // The overlay reads left-to-right as a lifecycle, so the lane list must
        // arrive in KANBAN_COLUMNS order. Sorting by name would render it
        // backwards (Later, Next, Now) - the board's own order is the contract.
        let raw = graph(
            r#"{"id":"x-l","slug":"l","priority":"p3","status":"ready"},
               {"id":"x-t","slug":"t","priority":"p2","status":"ready","queued_at":"2026-07-19T00:00:00Z"},
               {"id":"x-n","slug":"n","priority":"p1","status":"ready"},
               {"id":"x-x","slug":"x","priority":"p2","status":"ready"}"#,
        );
        let names: Vec<String> = derive_queue(&raw, None, &BoardScope::All)
            .unwrap()
            .lanes
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        assert_eq!(names, ["Now", "Next", "Later", "Triage"]);
    }

    #[test]
    fn an_excluded_node_is_not_a_card_at_all() {
        // `kanban_column` returning None EXCLUDES a node from the board in the
        // Python. Bucketing it as unlaned instead would render a row the
        // canonical board says does not exist.
        let raw = graph(
            r#"{"id":"x-road","slug":"r","type":"roadmap","priority":"p1","status":"ready"},
               {"id":"x-real","slug":"n","priority":"p1","status":"ready"}"#,
        );
        let q = derive_queue(&raw, None, &BoardScope::All).unwrap();
        let ids: Vec<&str> = q.cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["x-real"], "the roadmap row is off the board");
        assert_eq!(q.total(), 1, "and is not counted in any lane");
        assert!(
            q.cards.iter().all(|c| c.lane.is_some()),
            "every surviving card knows its lane"
        );
    }

    #[test]
    fn total_counts_the_whole_board_not_the_capped_list() {
        // AC5-EDGE: the section's `+N more` needs the uncapped count, so `total`
        // must survive the cap.
        let nodes: Vec<String> = (0..CARD_CAP + 7)
            .map(|i| format!(r#"{{"id":"x-{i}","slug":"s{i}","priority":"p2","status":"ready"}}"#))
            .collect();
        let q = derive_queue(&graph(&nodes.join(",")), None, &BoardScope::All).unwrap();
        assert_eq!(q.cards.len(), CARD_CAP, "the wire frame stays bounded");
        assert_eq!(q.total(), CARD_CAP + 7, "but the count is the whole board");
    }

    #[test]
    fn a_run_of_failed_reads_marks_stale_and_recovery_clears_it() {
        // AC7-FR: a failing read keeps the last-known cards (never a blank
        // section) but stops presenting them as current - and a read that lands
        // again clears the marker with no restart. One torn read is a normal
        // write race and must NOT trip it.
        let mut st = ReaderState::default();
        let raw = graph(r#"{"id":"x-a","slug":"s","priority":"p1","status":"ready"}"#);
        let s0 = Some((std::time::SystemTime::UNIX_EPOCH, raw.len() as u64));
        assert!(!st.tick(s0, || Some(raw.clone()), None).unwrap().0.stale);

        // Each failing tick presents a new stamp (the file keeps changing) but
        // no content. The first is just a race.
        let bump = |n: u64| Some((std::time::SystemTime::UNIX_EPOCH, 900 + n));
        st.tick(bump(1), || None, None);
        assert!(
            !st.last_sent.as_ref().unwrap().stale,
            "one torn read is a write race, not staleness"
        );
        for i in 2..=STALE_AFTER_FAILED_READS as u64 {
            st.tick(bump(i), || None, None);
        }
        let stale = st.last_sent.clone().unwrap();
        assert!(stale.stale, "a run of failed reads earns the marker");
        assert_eq!(
            stale.cards.len(),
            1,
            "and the cards are KEPT, never blanked"
        );

        let fresh = st.tick(bump(9), || Some(raw.clone()), None).unwrap().0;
        assert!(!fresh.stale, "a landed read clears the marker in place");
    }

    #[test]
    fn a_graph_that_reads_but_will_not_parse_also_goes_stale() {
        // A corrupt graph lands bytes and commits a stamp, so "the read landed"
        // is not the same question as "we can derive current state". Treating a
        // committed stamp as success left a persistently corrupt graph showing
        // old cards forever with no marker - the exact situation the marker is
        // for.
        let mut st = ReaderState::default();
        let good = graph(r#"{"id":"x-a","slug":"s","priority":"p1","status":"ready"}"#);
        let s0 = Some((std::time::SystemTime::UNIX_EPOCH, good.len() as u64));
        assert!(!st.tick(s0, || Some(good.clone()), None).unwrap().0.stale);

        // Successive reads land, but the document is garbage each time.
        for i in 1..=STALE_AFTER_FAILED_READS as u64 {
            let stamp = Some((std::time::SystemTime::UNIX_EPOCH, 800 + i));
            st.tick(stamp, || Some("{ not json".to_string()), None);
        }
        let held = st.last_sent.clone().unwrap();
        assert!(
            held.stale,
            "unparseable is a failure, not a successful read"
        );
        assert_eq!(held.cards.len(), 1, "and the last-good cards are kept");

        // A parseable read clears it in place.
        let ok = Some((std::time::SystemTime::UNIX_EPOCH, 999));
        assert!(!st.tick(ok, || Some(good.clone()), None).unwrap().0.stale);
    }

    #[test]
    fn derive_pr_map_takes_only_u64_pr_numbers() {
        // node id -> pr_number, skipping a missing / non-u64 value
        // (matching AgentRow.pr: Option<u64>). Keyed by node id, not unique pr.
        let raw = graph(
            r#"{"id":"x-a","slug":"a","priority":"p1","status":"claimed","pr_number":385},
               {"id":"x-b","slug":"b","priority":"p2","status":"ready"},
               {"id":"x-c","slug":"c","priority":"p2","status":"claimed","pr_number":"nope"}"#,
        );
        let m = derive_pr_map(&raw);
        assert_eq!(m.get("x-a"), Some(&385));
        assert_eq!(m.get("x-b"), None, "no pr_number -> absent");
        assert_eq!(m.get("x-c"), None, "non-u64 pr_number -> skipped");
        assert!(derive_pr_map("not json").is_empty());
    }

    #[test]
    fn pr_only_change_republishes() {
        // US8: a node gaining a pr_number (same card set + holders) must
        // republish so the PR label is not stale until an unrelated flip.
        let mut st = ReaderState::default();
        let raw0 = graph(r#"{"id":"x-a","slug":"s","priority":"p1","status":"claimed"}"#);
        let s0 = Some((std::time::SystemTime::UNIX_EPOCH, raw0.len() as u64));
        assert!(st.tick(s0, || Some(raw0.clone()), None).is_some());
        // Same claimed card, now with a pr_number: a new stamp (mtime bumped),
        // same card state -> still republishes because the pr map changed.
        let raw1 =
            graph(r#"{"id":"x-a","slug":"s","priority":"p1","status":"claimed","pr_number":42}"#);
        let s1 = Some((std::time::SystemTime::UNIX_EPOCH, raw1.len() as u64));
        let out = st.tick(s1, || Some(raw1.clone()), None);
        assert!(out.is_some(), "pr-only change republishes");
        assert_eq!(out.unwrap().1.get("x-a"), Some(&42));
    }

    // ---- claims overlay (x-54fa) -----------------------------------------

    fn live(ids: &[(&str, &str)]) -> HashMap<String, String> {
        ids.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn overlay_flips_ready_and_blocked_to_in_flight() {
        // AC1-HP: ready + live claim renders InFlight. AC2-EDGE: overlay
        // beats Blocked. Ids not in the live map are untouched; live ids not
        // in the card set are ignored (no phantom cards).
        let raw = graph(
            r#"{"id":"x-rdy","slug":"r","priority":"p1","status":"ready"},
               {"id":"x-blk","slug":"b","priority":"p2","status":"blocked","blocked_by":["x-rdy"]},
               {"id":"x-free","slug":"f","priority":"p2","status":"ready"}"#,
        );
        let cards = derive_queue(
            &raw,
            Some(&live(&[
                ("x-rdy", "target-session:abc"),
                ("x-blk", "dispatch-node:1"),
                ("x-ghost", "nobody"),
            ])),
            &BoardScope::All,
        )
        .unwrap()
        .cards;
        let by_id = |id: &str| cards.iter().find(|c| c.id == id).unwrap().state;
        assert_eq!(by_id("x-rdy"), CardState::InFlight);
        assert_eq!(by_id("x-blk"), CardState::InFlight);
        assert_eq!(by_id("x-free"), CardState::Ready);
        assert_eq!(cards.len(), 3, "no phantom card for x-ghost");
    }

    #[test]
    fn overlay_change_republishes_without_graph_change() {
        // AC1-HP/AC1-EDGE: a claim appearing (and later releasing) flips the
        // card within a tick even though the graph stamp never moves.
        let mut st = ReaderState::default();
        let raw = graph(r#"{"id":"x-a","slug":"s","priority":"p1","status":"ready"}"#);
        let s = Some((std::time::SystemTime::UNIX_EPOCH, raw.len() as u64));
        let first = st.tick(s, || Some(raw.clone()), None).unwrap().0;
        assert_eq!(first.cards[0].state, CardState::Ready);
        // Claim appears: same stamp, no re-read, card republishes InFlight.
        let claimed = live(&[("x-a", "target-session:abc")]);
        let flipped = st.tick(s, || None, Some(&claimed)).unwrap().0;
        assert_eq!(flipped.cards[0].state, CardState::InFlight);
        // Unchanged claim set: no republish.
        assert!(st.tick(s, || None, Some(&claimed)).is_none());
        // Claim released: card reverts to Ready and republishes.
        let released = live(&[]);
        let reverted = st.tick(s, || None, Some(&released)).unwrap().0;
        assert_eq!(reverted.cards[0].state, CardState::Ready);
    }

    #[test]
    fn holder_only_change_republishes_and_retention_does_not_churn() {
        // Codex peer review: holders feed the publish-time where_hint join, so
        // a holder-only change (same card states - here a graph-native
        // in-flight card) must republish; a `None` tick (no sweep yet /
        // failing sweep retaining last-good) must NOT churn publishes.
        let mut st = ReaderState::default();
        let raw = graph(r#"{"id":"x-a","slug":"s","priority":"p1","status":"claimed"}"#);
        let s = Some((std::time::SystemTime::UNIX_EPOCH, raw.len() as u64));
        let first = st.tick(s, || Some(raw.clone()), None).unwrap().0;
        assert_eq!(
            first.cards[0].state,
            CardState::InFlight,
            "graph-native in-flight"
        );
        // First successful sweep: card state unchanged, holders newly known ->
        // republish so the server's hint join sees them.
        let a = live(&[("x-a", "target-session:abc")]);
        assert!(st.tick(s, || None, Some(&a)).is_some());
        // Same holders: no republish.
        assert!(st.tick(s, || None, Some(&a)).is_none());
        // Holder handoff, card still in flight: republish.
        let b = live(&[("x-a", "target-session:def")]);
        assert!(st.tick(s, || None, Some(&b)).is_some());
        // Sweep failure (caller passes last-good again) then no-sweep tick:
        // neither is a change.
        assert!(st.tick(s, || None, Some(&b)).is_none());
        assert!(st.tick(s, || None, None).is_none());
    }

    #[test]
    fn sweep_parse_takes_only_live_node_and_dispatch_claims() {
        let stdout = r#"{"claims":[
            {"key":"node:x-a","state":"live","holder":"target-session:abc","host":"h","pid":1},
            {"key":"dispatch:x-a","state":"live","holder":"dispatch-node:9","host":"h","pid":9},
            {"key":"dispatch:x-b","state":"live","holder":"advance:2","host":"h","pid":2},
            {"key":"node:x-c","state":"stale","holder":"gone","host":"h","pid":3},
            {"key":"node:x-d","state":"suspect","holder":"maybe","host":"h","pid":4}
        ]}"#;
        let live = live_claims_from_sweep(stdout).unwrap();
        // Only live claims count; node: holder preferred over dispatch:.
        assert_eq!(live.len(), 2);
        assert_eq!(live["x-a"], "target-session:abc");
        assert_eq!(live["x-b"], "advance:2");
        // Unparseable output is None (keep last-good), not an empty map.
        assert!(live_claims_from_sweep("not json").is_none());
        assert!(live_claims_from_sweep(r#"{"no_claims":1}"#).is_none());
    }

    // ---- mission derivation ----------------------------------------------

    #[test]
    fn active_mission_membership_and_counts() {
        // An active-mission epic's leaf children map to it and its done/total
        // count them; the epic is not its own member; a node under an inactive
        // epic is unmapped.
        let raw = graph(
            r#"{"id":"x-e","slug":"mission-e","type":"epic","mission_active":true},
               {"id":"x-c1","slug":"c1","status":"done","parent":"x-e"},
               {"id":"x-c2","slug":"c2","status":"claimed","parent":"x-e"},
               {"id":"x-off","slug":"off","type":"epic","parent":null},
               {"id":"x-c3","slug":"c3","status":"ready","parent":"x-off"}"#,
        );
        let m = derive_missions(&raw).unwrap();
        assert_eq!(m.node_to_epic.get("x-c1"), Some(&"x-e".to_string()));
        assert_eq!(m.node_to_epic.get("x-c2"), Some(&"x-e".to_string()));
        assert_eq!(
            m.node_to_epic.get("x-e"),
            None,
            "epic is not its own member"
        );
        assert_eq!(
            m.node_to_epic.get("x-c3"),
            None,
            "inactive-epic child unmapped"
        );
        assert_eq!(m.missions.len(), 1);
        let mission = &m.missions[0];
        assert_eq!(mission.epic_id, "x-e");
        assert_eq!(mission.slug, "mission-e");
        assert_eq!((mission.done, mission.total), (1, 2));
    }

    #[test]
    fn empty_active_mission_counts_survivors() {
        // A mission whose only child is done still renders 1/1 - it exists even
        // with no in-flight work.
        let raw = graph(
            r#"{"id":"x-e","slug":"m","type":"epic","mission_active":true},
               {"id":"x-c1","status":"done","parent":"x-e"}"#,
        );
        let m = derive_missions(&raw).unwrap();
        assert_eq!(m.missions.len(), 1);
        assert_eq!((m.missions[0].done, m.missions[0].total), (1, 1));
    }

    #[test]
    fn no_active_mission_is_empty_not_none() {
        // A valid graph with nothing active is an empty map, not None (None is
        // reserved for a malformed doc).
        let raw = graph(r#"{"id":"x-e","slug":"m","type":"epic"}"#);
        let m = derive_missions(&raw).unwrap();
        assert!(m.missions.is_empty() && m.node_to_epic.is_empty());
    }

    #[test]
    fn malformed_document_is_none() {
        // A torn/malformed graph yields None so the caller renders workers
        // ungrouped rather than hiding them.
        assert!(derive_missions("not json").is_none());
    }

    #[test]
    fn epic_child_folds_leaves_one_level() {
        // A mission epic over a sub-epic folds the sub-epic's leaves in
        // (mission -> epic -> leaf), never counting the sub-epic as a unit.
        let raw = graph(
            r#"{"id":"x-m","slug":"mission","type":"epic","mission_active":true},
               {"id":"x-sub","slug":"sub","type":"epic","parent":"x-m"},
               {"id":"x-l1","status":"done","parent":"x-sub"},
               {"id":"x-l2","status":"ready","parent":"x-sub"},
               {"id":"x-direct","status":"done","parent":"x-m"}"#,
        );
        let m = derive_missions(&raw).unwrap();
        assert_eq!(m.missions.len(), 1);
        // 2 done (x-l1, x-direct) of 3 leaves (x-l1, x-l2, x-direct); the
        // sub-epic itself is not a unit.
        assert_eq!((m.missions[0].done, m.missions[0].total), (2, 3));
        // A leaf under the sub-epic walks UP past it to the active mission.
        assert_eq!(m.node_to_epic.get("x-l1"), Some(&"x-m".to_string()));
    }

    #[test]
    fn parent_cycle_terminates() {
        // A malformed parent cycle must not loop the ancestor walk or the
        // rollup recursion.
        let raw = graph(
            r#"{"id":"x-a","type":"epic","mission_active":true,"parent":"x-b"},
               {"id":"x-b","type":"epic","parent":"x-a"}"#,
        );
        // Terminates (does not hang) and produces a well-formed map.
        let m = derive_missions(&raw).unwrap();
        assert_eq!(m.missions.len(), 1, "x-a is the active mission");
    }

    #[test]
    fn deep_worker_maps_past_the_old_depth_cap() {
        // A worker more than the old fixed cap (8) parent-hops below the active
        // epic must still map to it - mission scope is all transitive
        // descendants, bounded only by the cycle guard (codex P2).
        let mut parts =
            vec![r#"{"id":"x-m","slug":"m","type":"epic","mission_active":true}"#.to_string()];
        for i in 0..12 {
            let parent = if i == 0 {
                "x-m".to_string()
            } else {
                format!("n{}", i - 1)
            };
            parts.push(format!(
                r#"{{"id":"n{i}","status":"ready","parent":"{parent}"}}"#
            ));
        }
        let raw = graph(&parts.join(","));
        let m = derive_missions(&raw).unwrap();
        assert_eq!(
            m.node_to_epic.get("n11"),
            Some(&"x-m".to_string()),
            "a 12-deep worker still maps to its mission"
        );
    }
}
