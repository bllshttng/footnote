//! parity-stage: characterization
//! parity-oracle: fno.decide
//!
//! Characterization harness for the native decide verb. The corpus was
//! driven through both implementations before the Python leg was deleted
//! (captured through `fno backlog decide`, the deleted leg's canonical
//! spelling); its frozen answers are the six identity-independent answers
//! (refusals, receipts, exits) every caller shares. The provenance lanes
//! resolve process truth with no test seam on the Python side, so the
//! lanes live in the door's injected-identity unit tests
//! (law_match::scope_tests::decide_*) while the goldens pin the texts.
//! Goldens live under tests/golden/decide_record/; the minted decision
//! ids are masked d-ID on both legs so a random id never freezes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// The worker's one-shot law lane: the front door (`fno inbox decide`)
/// spawns exactly this lane with the decide request.
const WORKER: &str = env!("CARGO_BIN_EXE_fno-agents-worker");

static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// The frozen corpus: key, and the decide argv after the verb tokens.
const CASES: &[(&str, &[&str])] = &[
    (
        "happy_no_node",
        &[
            "area:deploy",
            "Record the deploy ruling",
            "--authority",
            "agent",
        ],
    ),
    ("missing_args", &[]),
    ("bad_authority", &["x-n1", "do it", "--authority", "king"]),
    (
        "bad_graduation",
        &["x-n1", "do it", "--graduation", "banana"],
    ),
    (
        "supersedes_unknown",
        &["x-n1", "Override", "--supersedes", "d-ffffffff"],
    ),
    (
        "waiver_agent",
        &["review-coverage-waiver", "waive it", "--authority", "agent"],
    ),
];

/// Mask the minted decision ids: `d-<hex>` reads `d-ID` on both legs.
fn mask(text: &str) -> String {
    static RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"d-[0-9a-f]{4,32}").expect("parses"));
    RE.replace_all(text, "d-ID").into_owned()
}

/// One hermetic fixture per case: the backlog store sandbox moves with
/// FNO_CONFIG state_dir (FNO_HOME does not move the backlog), the journal
/// with FNO_REPO_ROOT. The env is process-global, so the lock serializes.
struct Fixture {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join(".fno")).expect("mkdir .fno");
        let config = format!("state_dir = {:?}\n", home.path());
        std::fs::write(root.path().join(".fno/config.toml"), config).expect("config");
        Self {
            home,
            root,
            _guard: guard,
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "FNO_CONFIG",
                self.root
                    .path()
                    .join(".fno/config.toml")
                    .display()
                    .to_string(),
            ),
            ("FNO_HOME", self.home.path().display().to_string()),
            ("FNO_REPO_ROOT", self.root.path().display().to_string()),
        ]
    }
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/decide_record")
}

/// Drive the door the way the front door does: one request through the
/// worker's `--law-exec-arg` lane, the door owning stdout and the exit.
fn run_worker(argv: &[&str], env: &[(&'static str, String)]) -> (i32, String, String) {
    let request = serde_json::json!({"mode": "decide", "argv": argv});
    let out = Command::new(WORKER)
        .arg("--law-exec-arg")
        .arg(request.to_string())
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .output()
        .expect("spawns the worker");
    (
        out.status.code().unwrap_or(1),
        mask(&String::from_utf8_lossy(&out.stdout)),
        mask(&String::from_utf8_lossy(&out.stderr)),
    )
}

#[test]
fn decide_corpus_matches_the_frozen_goldens() {
    for (key, argv) in CASES {
        let fixture = Fixture::new();
        let (exit, out, err) = run_worker(argv, &fixture.env());
        let expected_exit: i32 = std::fs::read_to_string(golden_dir().join(format!("{key}.exit")))
            .expect("golden exit")
            .trim()
            .parse()
            .expect("exit parses");
        assert_eq!(
            exit, expected_exit,
            "[{key}] exit\nstdout: {out}\nstderr: {err}"
        );
        let golden_out =
            std::fs::read_to_string(golden_dir().join(format!("{key}.out"))).expect("golden out");
        assert_eq!(out, golden_out, "[{key}] stdout\nstderr: {err}");
        let golden_err =
            std::fs::read_to_string(golden_dir().join(format!("{key}.err"))).expect("golden err");
        assert_eq!(err, golden_err, "[{key}] stderr\nstdout: {out}");
    }
}
