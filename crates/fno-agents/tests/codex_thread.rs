use fno_agents::codex_thread::{
    parse_thread_start_response, thread_start_request_json, ThreadStartError,
};

#[test]
fn thread_start_reads_nested_thread_id_and_rejects_top_level_alias() {
    let request: serde_json::Value =
        serde_json::from_str(&thread_start_request_json("/tmp/worktree", "never"))
            .expect("valid request JSON");
    assert_eq!(request["method"], "thread/start");
    assert_eq!(request["params"]["cwd"], "/tmp/worktree");
    assert_eq!(request["params"]["approvalPolicy"], "never");

    assert_eq!(
        parse_thread_start_response(
            r#"{"id":1,"result":{"thread":{"id":"thread-1","path":"/tmp/rollout"}}}"#
        ),
        Ok(("thread-1".to_string(), "/tmp/rollout".to_string()))
    );
    assert_eq!(
        parse_thread_start_response(r#"{"id":1,"result":{"threadId":"thread-1"}}"#),
        Err(ThreadStartError::NotConfirmed)
    );
}
