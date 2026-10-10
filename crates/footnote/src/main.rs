//! `footnote`: read one launch spec from stdin, run the loop to a terminal
//! state, print the reply, and exit with the loop's code. fno-agents is the
//! only caller; it resolves everything the loop needs before the launch.

use std::io::{Read, Write};

fn main() {
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("footnote: cannot read the launch spec from stdin: {e}");
        std::process::exit(2);
    }
    let spec: fno::footnote_transcript::LaunchSpec = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("footnote: the launch spec on stdin is not valid: {e}; run `fno doctor update --rust` so fno-agents and footnote match");
            std::process::exit(2);
        }
    };
    footnote::catch_sigint();
    let out = footnote::run(&spec);
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    let _ = std::io::stdout().flush();
    std::process::exit(out.exit_code);
}
