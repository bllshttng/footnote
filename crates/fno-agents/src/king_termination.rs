//! The king termination board read: what work and operator questions remain.

use serde_json::Value;
use std::path::Path;

pub(crate) struct KingBoard {
    pub(crate) actionable: i64,
    pub(crate) top_row: Option<String>,
    pub(crate) unreadable: i64,
    pub(crate) over_budget: i64,
    pub(crate) actionable_ids: Vec<String>,
    pub(crate) operator_question_sessions: Vec<String>,
    pub(crate) operator_questions_unreadable: bool,
}

fn row_identity(queue: &str, row: &Value) -> String {
    let id = row
        .get("id")
        .or_else(|| row.get("key"))
        .or_else(|| row.get("number"))
        .map(|v| v.to_string())
        .unwrap_or_else(|| row.to_string());
    format!("{queue}:{}", id.trim_matches('"'))
}

pub(crate) fn parse_king_board_value(value: &Value) -> Option<KingBoard> {
    let actionable = value.get("actionable")?.as_i64()?;
    let unreadable = value
        .get("unreadable")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let over_budget = value
        .get("over_budget")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut top_row = None;
    let mut actionable_ids: Vec<String> = Vec::new();
    let mut operator_question_sessions: Vec<String> = Vec::new();
    let mut operator_questions_unreadable = false;
    if let Some(queues) = value.get("queues").and_then(|q| q.as_array()) {
        for queue in queues {
            let name = queue.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let status = queue.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if name == "operator_question" && crate::king_board::not_read_status(status) {
                operator_questions_unreadable = true;
            }
            if crate::king_board::not_read_status(status) {
                if top_row.is_none() {
                    let err = queue.get("error").and_then(|v| v.as_str()).unwrap_or("");
                    top_row = Some(if status == "over_budget" {
                        format!("{name} not read: {err}")
                    } else {
                        format!("{name} is unreadable: {err}")
                    });
                }
                continue;
            }
            if name == "operator_question" {
                operator_question_sessions.extend(
                    queue
                        .get("rows")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|row| row.get("session_id").and_then(|v| v.as_str()))
                        .map(str::to_owned),
                );
            }
            if queue.get("actionable").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            for row in queue
                .get("rows")
                .and_then(|v| v.as_array())
                .unwrap_or(&vec![])
            {
                let identity = row_identity(name, row);
                if top_row.is_none() {
                    top_row = Some(identity.clone());
                }
                actionable_ids.push(identity);
            }
        }
    }
    Some(KingBoard {
        actionable,
        top_row,
        unreadable,
        over_budget,
        actionable_ids,
        operator_question_sessions,
        operator_questions_unreadable,
    })
}

pub(crate) fn read_king_board(
    fno_bin: &str,
    cwd: &Path,
    state_path: &Path,
) -> Result<KingBoard, String> {
    let _ = fno_bin;
    let opts = crate::king_board::BoardOpts {
        budget_ms: crate::loopcheck::stopgate_read_timeout().as_millis() as u64,
        max_pr_reads: 20,
        state_path: Some(state_path.to_path_buf()),
        cwd: Some(cwd.to_path_buf()),
    };
    let payload = crate::king_board::read_board(&opts);
    parse_king_board_value(&payload).ok_or_else(|| {
        "unparseable board payload: the collector returned a shape parse_king_board_value cannot read"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn board_with_queues(queues: Value) -> Value {
        json!({
            "actionable": 0,
            "unreadable": 1,
            "over_budget": 1,
            "queues": queues,
        })
    }

    #[test]
    fn an_over_budget_top_row_says_not_read_never_unreadable() {
        let board = board_with_queues(json!([
            {"name": "undispatched", "status": "over_budget",
             "error": "killed at its 28.5s slice of the board budget; the source did not fail",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        let top = parsed.top_row.unwrap();
        assert!(top.starts_with("undispatched not read:"), "{top}");
        assert!(!top.contains("unreadable"), "{top}");
    }

    #[test]
    fn an_unreadable_top_row_still_says_unreadable() {
        let board = board_with_queues(json!([
            {"name": "claims", "status": "unreadable", "error": "exit 1: boom",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        let top = parsed.top_row.unwrap();
        assert!(top.contains("is unreadable: exit 1"), "{top}");
    }

    #[test]
    fn the_two_kinds_count_apart() {
        let board = board_with_queues(json!([
            {"name": "claims", "status": "unreadable", "error": "exit 1: boom",
             "actionable": true, "rows": []},
            {"name": "undispatched", "status": "over_budget",
             "error": "killed at its 28.5s slice of the board budget",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert_eq!(parsed.unreadable, 1);
        assert_eq!(parsed.over_budget, 1);
    }
}
