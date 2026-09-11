//! The `fno mux thread --portal` argument-surface tests, in a child module
//! under the file-budget gate (the parent stays shrink-only).
use super::*;

fn argv(a: &[&str]) -> Vec<OsString> {
    a.iter().map(OsString::from).collect()
}

#[test]
fn thread_portal_takes_an_index_or_new() {
    // (x-3ea6) AC2-ERR: `new` parses as a placement (it gets as far as the
    // server dial, which for a dead test server answers NO_SERVER, never
    // USAGE); a junk value keeps the usage refusal. The flag naming a dead
    // server keeps the probe off any live mux.
    let dead = ["--server", "fno-portal-arg-test-dead"];
    let mut run = |extra: &[&str]| {
        let all: Vec<&str> = dead.iter().chain(extra.iter()).copied().collect();
        thread(&argv(&all), None)
    };
    assert_eq!(
        run(&["--portal", "nope", "row"]),
        EXIT_USAGE,
        "a junk --portal value keeps its usage refusal"
    );
    assert_eq!(
        run(&["--portal", "new", "row"]),
        EXIT_NO_SERVER,
        "`new` parses and reaches the dial"
    );
    assert_eq!(
        run(&["--portal=new", "row"]),
        EXIT_NO_SERVER,
        "`--portal=new` parses too"
    );
    assert_eq!(
        run(&["--portal", "2", "row"]),
        EXIT_NO_SERVER,
        "an explicit index still parses"
    );
}
