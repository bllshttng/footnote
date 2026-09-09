//! The control_roundtrip typed-error test, moved verbatim out of mux_cli.rs
//! (file budget shrink; the v75 retire-session verb rides the same file).
//! Parent helpers resolve through the glob.
use super::*;

#[test]
fn control_roundtrip_surfaces_unanswered_not_a_flattened_string() {
    // `where_` and `block pipe` both dispatch through control_roundtrip,
    // not send_control directly, so a fix that only touches send_control's
    // callers in `dispatch`/`run_on_existing_server` leaves this path
    // asserting no pane exists on a mere timeout - exactly the P1 review
    // found. Pin the typed error, not a stringified one, all the way
    // through control_roundtrip.
    let sock = control_test_sock("roundtrip-unanswered");
    let _ = std::fs::remove_file(&sock);
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let _msg: ClientMsg = read_msg_sync(&mut s).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    });

    let result = control_roundtrip_with_timeouts(
        &sock,
        "test-session",
        ControlVerb::PaneLs,
        Duration::from_millis(50),
        Duration::from_millis(150),
    );
    server.join().unwrap();
    let _ = std::fs::remove_file(&sock);

    assert!(
        matches!(result, Err(ControlError::Unanswered(_))),
        "expected Unanswered, got {result:?}"
    );
}
