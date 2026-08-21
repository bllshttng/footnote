//! The needs-me-queue events-fold leg (x-feec): a bounded, fail-open shell-out
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

/// One event-derived need, as emitted by `fno-agents needs --json`. The `live`
/// bit is the claim-liveness stamp (x-feec 1.4): the client renders an item
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

/// One open operator question, as emitted by `fno inbox outstanding --json`'s
/// `questions` array (x-7979's record: asker/options/blocks/liveness, already
/// rank-ordered). Richer than the bare `operator_question` event the events
/// leg carries - this is what the overlay renders and answers; the events leg
/// still carries a plain `NeedKind::Question` badge for the roster.
#[derive(Debug, Clone, Deserialize)]
pub struct QuestionItem {
    pub id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub ask: Option<String>,
    #[serde(default)]
    pub asker: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    /// `None` = liveness unresolved (render as normal); `Some(false)` = the
    /// asker no longer resolves (render STALE - the answer still records,
    /// but reaches no live session); `Some(true)` = live.
    #[serde(default)]
    pub live: Option<bool>,
    #[serde(default)]
    pub rank: Option<u32>,
}

#[derive(Deserialize)]
struct QuestionsResponse {
    questions: Vec<QuestionItem>,
}

/// Both independent overlay reads. Each leg carries its own failure so one
/// unavailable command never hides the other lanes.
pub struct FoldOutcome {
    pub needs: Option<Vec<FoldItem>>,
    pub mine: Option<Vec<MineItem>>,
    pub questions: Option<Vec<QuestionItem>>,
}

/// Fold the needs-me events leg over the `since_epoch` window. `None` on any
/// failure (timeout, nonzero exit, unparseable JSON) - the caller shows the
/// degraded notice; `Some(vec)` (possibly empty) is a clean fold.
pub async fn fold_now(since_epoch: &str) -> Option<Vec<FoldItem>> {
    let fut = tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())
        .args(["needs", "--since-epoch", since_epoch, "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Dropped on timeout; kill_on_drop reaps the child so a slow fold can't
        // orphan a process on each overlay open.
        .kill_on_drop(true)
        .output();
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
    let fut = tokio::process::Command::new(crate::server::fno_bin())
        .args(["inbox", "outstanding", "mine", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_mine(&output.stdout)
}

/// Fold open operator questions through `fno inbox outstanding --json` - the
/// SAME store `fno outstanding ask`/`clear` write, already rank-ordered and
/// liveness-resolved (a 50ms budget, well inside this leg's own timeout).
/// Same bounded/fail-open shape as the other legs.
pub async fn questions_now() -> Option<Vec<QuestionItem>> {
    let fut = tokio::process::Command::new(crate::server::fno_bin())
        .args(["inbox", "outstanding", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
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
    let mut args: Vec<String> = vec!["inbox".into(), "outstanding".into(), "mine".into()];
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
    let fut = tokio::process::Command::new(crate::server::fno_bin())
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .map_err(|_| "timed out".to_string())?
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

fn parse_questions(stdout: &[u8]) -> Option<Vec<QuestionItem>> {
    serde_json::from_slice::<QuestionsResponse>(stdout)
        .ok()
        .map(|response| response.questions)
}

/// Answer or withdraw one open question through the installed/current `fno`
/// binary - `fno inbox outstanding clear <id> --answer "..."`, the same verb
/// `outstanding`'s own CLI help names. Bounded, single writer: the client
/// never records the decision itself. `Ok(())` on a clean exit; `Err(message)`
/// on a timeout, spawn failure, or a nonzero exit (stderr captured) - the
/// operator sees WHY a write failed, never a silent no-op.
pub async fn answer_question(question_id: &str, answer: &str) -> Result<(), String> {
    let fut = tokio::process::Command::new(crate::server::fno_bin())
        .args([
            "inbox",
            "outstanding",
            "clear",
            question_id,
            "--answer",
            answer,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .map_err(|_| "timed out".to_string())?
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
        let json = br#"{"mine":[{"n":1,"text":"ship tonight","done":false,"node":null},{"n":2,"text":"cut verbs","done":true,"node":"x-c1b9"}]}"#;
        let items = parse_mine(json).expect("valid mine response parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].n, 1);
        assert_eq!(items[0].text, "ship tonight");
        assert!(!items[0].done);
        assert_eq!(items[1].node.as_deref(), Some("x-c1b9"));
    }

    #[test]
    fn torn_mine_json_fails_quiet() {
        assert!(parse_mine(br#"{"mine":[{"n":1"#).is_none());
    }

    #[test]
    fn parses_required_questions_json() {
        let json = br#"{"questions":[{"id":"q-1","question":"which auth?","ask":"pick one","asker":"fno-peer","node":null,"options":["oauth","apikey"],"live":true,"rank":1},{"id":"q-2","question":"free text one","ask":null,"asker":null,"node":null,"options":[],"live":false,"rank":2}]}"#;
        let items = parse_questions(json).expect("valid questions response parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "q-1");
        assert_eq!(items[0].ask.as_deref(), Some("pick one"));
        assert_eq!(items[0].options, vec!["oauth", "apikey"]);
        assert_eq!(items[0].live, Some(true));
        assert_eq!(items[1].live, Some(false));
        assert_eq!(items[1].ask, None);
    }

    #[test]
    fn torn_questions_json_fails_quiet() {
        assert!(parse_questions(br#"{"questions":[{"id":"q-1""#).is_none());
    }
}
