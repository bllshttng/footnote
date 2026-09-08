//! (x-688b) The served-liveness corroboration rule: a stale served dead
//! on a pid-less terminal row still proves the row dead.

use super::*;

#[test]
fn a_stale_served_dead_still_corroborates_a_pid_less_terminal_row() {
    // (x-688b) The t-f90d shape: status exited, NO pid, a short_id, and a
    // served "dead" the daemon stamped from an observed exit days ago.
    // The age gate demotes the served reading to the ladder, and the
    // ladder has no pid to check - without this rule the row reads
    // Unmeasured forever and its squad member strands (one reaped-row
    // member per such worker). A live pid still wins (reused-pid guard),
    // and a stale non-dead served word keeps the ladder's verdict.
    let raw = reg(
        r#"{"name":"cx-stamp","cwd":"/w","status":"exited","harness":"claude",
            "short_id":"a3946018","liveness":"dead",
            "liveness_measured_at":"2027-01-10T00:00:00Z"}"#,
    );
    let rows = derive_rows(&raw, NOW).unwrap();
    let row = rows.iter().find(|r| r.name == "cx-stamp").unwrap();
    assert_eq!(row.liveness, Liveness::Dead);

    let me = std::process::id();
    let raw = reg(&format!(
        r#"{{"name":"cx-livepid","cwd":"/w","status":"exited","harness":"claude",
            "pid":{me},"short_id":"bb77","liveness":"dead",
            "liveness_measured_at":"2027-01-10T00:00:00Z"}}"#
    ));
    let rows = derive_rows(&raw, NOW).unwrap();
    let row = rows.iter().find(|r| r.name == "cx-livepid").unwrap();
    assert_eq!(
        row.liveness,
        Liveness::Unmeasured,
        "a live pid still contradicts"
    );

    let raw = reg(
        r#"{"name":"cx-freshless","cwd":"/w","status":"exited","harness":"claude",
            "short_id":"cc88","liveness":"unmeasured",
            "liveness_measured_at":"2027-01-10T00:00:00Z"}"#,
    );
    let rows = derive_rows(&raw, NOW).unwrap();
    let row = rows.iter().find(|r| r.name == "cx-freshless").unwrap();
    assert_eq!(
        row.liveness,
        Liveness::Unmeasured,
        "only the word dead corroborates"
    );
}
