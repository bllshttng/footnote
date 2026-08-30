use fno_agents::cursor_agent::{
    attach_argv, chat_id_error, is_chat_id, is_cursor_worker_server_command,
    select_owned_worker_server_pids,
};

const CHAT_ID: &str = "fadad56b-8008-45f5-b809-f9fab7074534";

#[test]
fn full_uuid_is_an_explicit_resume_identity() {
    assert!(is_chat_id(CHAT_ID));
    assert_eq!(
        attach_argv(CHAT_ID),
        vec!["cursor-agent", "--resume", CHAT_ID, "--trust",]
    );
}

#[test]
fn fno_handle_is_refused_as_a_chat_id() {
    let error = chat_id_error("74db359a").expect("short handle must be refused");
    assert!(error.contains("'74db359a' is 8 hex characters"));
    assert!(error.contains("an fno session handle, not a chat id"));
    assert!(error.contains("cursor-agent create-chat"));
}

#[test]
fn empty_and_malformed_chat_ids_are_refused() {
    assert!(chat_id_error("").is_some());
    assert!(chat_id_error("not-a-chat-id").is_some());
    assert!(!is_chat_id("74db359a"));
}

#[test]
fn detached_reaper_only_targets_cursor_worker_servers() {
    assert!(is_cursor_worker_server_command(
        "/Users/bb16/.local/share/cursor-agent/index.js worker-server"
    ));
    assert!(!is_cursor_worker_server_command(
        "/usr/local/bin/node index.js worker-server"
    ));
}

#[test]
fn detached_reaper_selector_is_exclusive_to_owner_process_tree() {
    let rows = vec![
        (100, 1, "/bin/zsh".to_string()),
        (
            200,
            100,
            "/Users/test/cursor-agent worker-server".to_string(),
        ),
        (
            300,
            999,
            "/Users/test/cursor-agent worker-server".to_string(),
        ),
        (
            400,
            200,
            "/Users/test/cursor-agent worker-server".to_string(),
        ),
    ];

    assert_eq!(select_owned_worker_server_pids(&rows, 100), vec![200, 400]);
}
