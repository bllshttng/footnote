//! The needs-me-queue events-fold leg : a bounded, fail-open shell-out
//! to `fno-agents needs --json`, mirroring [`crate::digest_overlay`]'s idiom.
//!
//! The client owns the live badge leg (blocked/done-unseen rows from the
//! layout) and renders it instantly; this module supplies the event-derived
//! leg (`review_wedged` / `budget_stop`) the client cannot see from badges. The
//! call is off the UI loop: it runs on a spawned task and reports back over a
//! channel, so a slow `fno-agents` never blocks the overlay from opening.

use serde::Deserialize;
use std::time::Duration;

/// Same 800ms cap as the digest overlay: a fold slower than this degrades the
/// queue to its live badge leg with a visible notice, never blocks the UI.
const SHELLOUT_TIMEOUT: Duration = Duration::from_millis(800);
const WRITE_TIMEOUT: Duration = Duration::from_secs(45);
const ANSWER_TIMEOUT_MESSAGE: &str =
    "timed out after 45s; the answer may have landed - rerun it, a rerun resumes";

/// One event-derived need, as emitted by `fno-agents needs --json`. The `live`
/// bit is the claim-liveness stamp (1.4): the client renders an item
/// that joins no roster row only when it is `live`, so a dead session's stale
/// stop never nags.
#[derive(Debug, Clone, Deserialize)]
pub struct FoldItem {
    pub kind: String,
    pub session_id: String,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub live: bool,
}

/// One operator-owned priority, as emitted by `fno inbox outstanding mine
/// --json`. `n` is the stable file index used by later mutation tasks; this
/// task only folds and renders it.
#[derive(Debug, Clone, Deserialize)]
pub struct MineItem {
    pub n: usize,
    pub text: String,
    pub done: bool,
    pub node: Option<String>,
}

#[derive(Deserialize)]
struct MineResponse {
    mine: Vec<MineItem>,
}

/// One option of a [`QuestionItem`], mirroring the projection's ItemOption.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuestionOption {
    pub n: u32,
    pub text: String,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub pros: Vec<String>,
    #[serde(default)]
    pub cons: Vec<String>,
}

/// One context field of a [`QuestionItem`], mirroring the projection's
/// Recommendation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuestionRecommendation {
    pub option: u32,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub downside: Option<String>,
}

/// One asker of a [`QuestionItem`], mirroring the projection's Asker.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuestionAsker {
    pub handle: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub live: Option<bool>,
    #[serde(default)]
    pub reach: Option<String>,
}

/// One open or answered operator question, mirroring the projection item
/// `fno-agents needs --items --json` prints. This is what the overlay and the
/// sideline's questions block render and answer. All fields but `id` are
/// optional or defaulted, so a projection field added later never breaks an
/// older client.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuestionItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub asker: Option<QuestionAsker>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub missing: Vec<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub blocked_because: Option<String>,
    #[serde(default)]
    pub options_rationale: Option<String>,
    #[serde(default)]
    pub recommendation: Option<QuestionRecommendation>,
    #[serde(default)]
    pub unknowns: Option<String>,
    #[serde(default)]
    pub reversible: Option<String>,
    #[serde(default)]
    pub meanwhile: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
}

/// One recently answered item beside the open list, from the payload's
/// top-level `answered` array.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnsweredItem {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub asker: Option<String>,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub rung: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub at: String,
}

/// The questions fold shape: open-or-answered items plus the recently
/// answered rows, one shape for the overlay and the sideline block.
#[derive(Debug, Clone, Default)]
pub struct QuestionsFold {
    pub items: Vec<QuestionItem>,
    pub answered: Vec<AnsweredItem>,
    pub as_of: Option<u64>,
}

#[derive(Deserialize)]
struct QuestionsResponse {
    items: Vec<QuestionItem>,
    #[serde(default)]
    answered: Vec<AnsweredItem>,
    #[serde(default)]
    as_of: Option<u64>,
}

/// Which items of the projection payload this fold keeps: real questions and
/// pins in an open or answered state (mine rows are the user lane, not a
/// question for the user).
fn keep_question(item: &QuestionItem) -> bool {
    matches!(item.kind.as_str(), "question" | "pin")
        && matches!(item.state.as_str(), "open" | "answered")
}

/// Both independent overlay reads. Each leg carries its own failure so one
/// unavailable command never hides the other lanes.
pub struct FoldOutcome {
    pub needs: Option<Vec<FoldItem>>,
    pub mine: Option<Vec<MineItem>>,
    pub questions: Option<QuestionsFold>,
}

/// Fold the needs-me events leg over the `since_epoch` window. `None` on any
/// failure (timeout, nonzero exit, unparseable JSON) - the caller shows the
/// degraded notice; `Some(vec)` (possibly empty) is a clean fold.
pub async fn fold_now(since_epoch: &str) -> Option<Vec<FoldItem>> {
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    command
        .args(["needs", "--since-epoch", since_epoch, "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Dropped on timeout; kill_on_drop reaps the child so a slow fold can't
        // orphan a process on each overlay open.
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&output.stdout)
}

/// Fold the operator-owned lane through the installed/current `fno` binary.
/// It has the same timeout and kill-on-drop discipline as the needs fold.
pub async fn mine_now() -> Option<Vec<MineItem>> {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args(["inbox", "outstanding", "mine", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_mine(&output.stdout)
}

/// Fold open questions through the Rust projection verb - 0.13 s against the
/// 800 ms cap (the Python verb measured 1.77 s and degraded the lane). Same
/// bounded/fail-open shape as the other legs.
pub async fn questions_now() -> Option<QuestionsFold> {
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    command
        .args(["needs", "--items", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_questions(&output.stdout)
}

/// Run all three bounded reads concurrently under the client's one
/// single-flight.
pub async fn fold_both(since_epoch: &str) -> FoldOutcome {
    let (needs, mine, questions) = tokio::join!(fold_now(since_epoch), mine_now(), questions_now());
    FoldOutcome {
        needs,
        mine,
        questions,
    }
}

/// One MINE mutation the panel can send, addressed by `MineItem::n` (the
/// stable file index `mine_now` already carries for this purpose). The verb
/// is the one writer; the client never edits the file or `mine_fold` itself -
/// it re-folds on success so the render always reflects what the file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MineMutation {
    Toggle(usize),
    Drop(usize),
    Add(String),
}

/// Run one MINE mutation through the installed/current `fno` binary, bounded
/// by the same timeout as every other overlay shell-out. `Ok(())` on a clean
/// exit; `Err(message)` on a timeout, spawn failure, or a nonzero exit (the
/// CLI writes `mine: failed: ...` to stderr, which is captured here) - the
/// operator sees WHY a write failed, never a silent no-op.
pub async fn mine_mutate(mutation: MineMutation) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "inbox".into(),
        "outstanding".into(),
        "mine".into(),
        "do".into(),
    ];
    match mutation {
        MineMutation::Toggle(n) => {
            args.push("done".into());
            args.push(n.to_string());
        }
        MineMutation::Drop(n) => {
            args.push("drop".into());
            args.push(n.to_string());
        }
        MineMutation::Add(text) => {
            args.push("add".into());
            args.push(text);
        }
    }
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(WRITE_TIMEOUT, fut)
        .await
        .map_err(|_| "timed out after 45s; the change may have landed - rerun it".to_string())?
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("exit {}", output.status)
        } else {
            stderr
        })
    }
}

/// Parse the verb's JSON array. Fails quiet (returns `None`) on unparseable
/// output so a torn stdout degrades the overlay rather than crashing it.
fn parse(stdout: &[u8]) -> Option<Vec<FoldItem>> {
    serde_json::from_slice(stdout).ok()
}

fn parse_mine(stdout: &[u8]) -> Option<Vec<MineItem>> {
    serde_json::from_slice::<MineResponse>(stdout)
        .ok()
        .map(|response| response.mine)
}

fn parse_questions(stdout: &[u8]) -> Option<QuestionsFold> {
    serde_json::from_slice::<QuestionsResponse>(stdout)
        .ok()
        .map(|response| QuestionsFold {
            items: response.items.into_iter().filter(keep_question).collect(),
            answered: response.answered,
            as_of: response.as_of,
        })
}

/// The pick the overlay sends through the door: an option number (1-based),
/// free-text words, or a pin's done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerPick {
    Option(u32),
    Words(String),
    Done,
}

/// Answer one open question through the door - `fno-agents needs
/// --answer <id> ... --sink mux`, the one writer of a mux answer. Bounded,
/// single writer: the client never records the decision itself. `Ok(receipt)`
/// returns the door's JSON receipt; `Err(message)` on a timeout, spawn
/// failure, or a nonzero exit (stderr captured) - the operator sees WHY a
/// write failed, never a silent no-op.
pub async fn answer(item_id: &str, pick: AnswerPick) -> Result<String, String> {
    let mut args: Vec<String> = vec!["needs".into(), "--answer".into(), item_id.to_string()];
    match pick {
        AnswerPick::Option(n) => {
            args.push("--option".into());
            args.push(n.to_string());
        }
        AnswerPick::Words(text) => {
            args.push("--words".into());
            args.push(text);
        }
        AnswerPick::Done => {
            args.push("--done".into());
        }
    }
    args.push("--sink".into());
    args.push("mux".into());
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(WRITE_TIMEOUT, fut)
        .await
        .map_err(|_| ANSWER_TIMEOUT_MESSAGE.to_string())?
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok("recorded, delivering".to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("exit {}", output.status)
        } else {
            stderr
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_fold_array() {
        let json = br#"[{"kind":"review_wedged","session_id":"s","node":"x-1","name":"x-1","title":"t","ts":"2026-07-03T02:00:00Z","evidence":"green PR wedged","live":true}]"#;
        let items = parse(json).expect("valid array parses");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "review_wedged");
        assert_eq!(items[0].node.as_deref(), Some("x-1"));
        assert!(items[0].live);
    }

    #[test]
    fn empty_array_is_a_clean_empty_fold() {
        assert_eq!(parse(b"[]").expect("empty array parses").len(), 0);
    }

    #[test]
    fn missing_optional_fields_default() {
        // node/name/title/live absent -> defaults, not a parse failure.
        let json = br#"[{"kind":"budget_stop","session_id":"s","ts":"","evidence":"stopped"}]"#;
        let items = parse(json).expect("parses with defaults");
        assert_eq!(items[0].node, None);
        assert!(!items[0].live);
    }

    #[test]
    fn torn_json_fails_quiet() {
        assert!(parse(b"[{not json").is_none());
    }

    #[test]
    fn parses_required_mine_json() {
        let json = br#"{"mine":[{"n":1,"text":"ship tonight","done":false,"node":null},{"n":2,"text":"cut verbs","done":true,"node":"x-aaaa"}]}"#;
        let items = parse_mine(json).expect("valid mine response parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].n, 1);
        assert_eq!(items[0].text, "ship tonight");
        assert!(!items[0].done);
        assert_eq!(items[1].node.as_deref(), Some("x-aaaa"));
    }

    #[test]
    fn torn_mine_json_fails_quiet() {
        assert!(parse_mine(br#"{"mine":[{"n":1"#).is_none());
    }

    #[test]
    fn parses_required_questions_json() {
        // AC4-HP: the projection payload with items and the answered array.
        let json = br#"{"as_of":1000,"items":[
            {"id":"q-1","kind":"question","title":"which auth?","state":"open","ready":false,"missing":["unknowns"],"priority":"normal","created_at":"2026-09-26T05:00:00Z","options":[{"n":1,"text":"oauth"},{"n":2,"text":"apikey"}],"asker":{"handle":"w1","live":true}},
            {"id":"m-1","kind":"mine","title":"ship tonight","state":"open","ready":true,"missing":[],"priority":"normal","created_at":"","options":[]},
            {"id":"q-2","kind":"pin","title":"publish the crate","state":"open","ready":false,"missing":["node"],"priority":"high","created_at":"","options":[]}
        ],"answered":[{"id":"q-0","title":"old one","asker":"w9","answer":"narrow","rung":"mail","outcome":"landed","at":"2026-09-26T04:00:00Z"}]}"#;
        let fold = parse_questions(json).expect("valid projection payload parses");
        assert_eq!(fold.items.len(), 2, "mine rows are not questions");
        assert_eq!(fold.items[0].id, "q-1");
        assert_eq!(fold.items[0].options[0].text, "oauth");
        assert_eq!(fold.items[0].asker.as_ref().unwrap().live, Some(true));
        assert_eq!(fold.items[0].missing, vec!["unknowns"]);
        assert_eq!(fold.answered.len(), 1);
        assert_eq!(fold.answered[0].rung.as_deref(), Some("mail"));
    }

    #[test]
    fn torn_questions_json_fails_quiet() {
        assert!(parse_questions(br#"{"items":[{"id":"q-1""#).is_none());
    }

    #[test]
    fn write_wait_and_timeout_receipt_match_the_clear_bound() {
        assert_eq!(WRITE_TIMEOUT, Duration::from_secs(45));
        assert!(ANSWER_TIMEOUT_MESSAGE.contains("the answer may have landed"));
        assert!(ANSWER_TIMEOUT_MESSAGE.contains("a rerun resumes"));
    }
}
