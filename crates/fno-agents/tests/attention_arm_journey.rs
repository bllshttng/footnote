//! The attention arm journey over real disk IO (temp dir) with fake
//! record/clear/notify: deliver on first sight, nothing before settle,
//! one row + clear + flip after it, the guarded rewrite, and the
//! close-elsewhere flip.

use fno_agents::attention::project;
use fno_agents::attention_arm::{tick_sink, SinkConfig, SinkIo};
use fno_agents::attention_file::FileAnswer;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn workdir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("attention-journey-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn question_row() -> String {
    let row = serde_json::json!({
        "ts": "2026-09-18T12:00:00Z",
        "type": "operator_question",
        "source": "test",
        "data": {
            "question_id": "q-j1",
            "question": "Which reading of the law?",
            "ask": "pick one",
            "session_id": "s1",
            "cwd": "/repo/fno",
            "node": "x-aaaa",
            "options": [
                {"n": 1, "text": "Narrow", "next": "unblocks today", "pros": ["fast"], "cons": ["strict"]},
                {"n": 2, "text": "Wide", "next": "waits", "pros": ["safe"], "cons": ["slow"]}
            ],
            "context": {
                "asker": "king-fno-g6",
                "blocked_because": "two repairs are Python edits",
                "options_rationale": "the readings kings acted on",
                "recommendation": {"option": 1, "why": "narrowest", "downside": "hides a feature"},
                "unknowns": "whether a net-zero move counts",
                "reversible": "costly",
                "cost_if_wrong": "allowance drops",
                "meanwhile": "stops"
            }
        }
    });
    row.to_string()
}

/// Real disk read/append/write, fake record/clear/notify.
struct JourneyIo {
    records: u32,
    clears: Vec<String>,
}

impl SinkIo for JourneyIo {
    fn read(&mut self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn append(&mut self, path: &Path, block: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(block.as_bytes())
    }
    fn write_atomic(&mut self, path: &Path, content: &str) -> std::io::Result<()> {
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)
    }
    fn record(
        &mut self,
        _item: &fno_agents::attention::AttentionItem,
        _sink: &str,
        _answer: &FileAnswer,
    ) -> Result<String, String> {
        self.records += 1;
        Ok("Recorded: option 1 (file)".to_string())
    }
    fn clear(&mut self, id: &str, answer: &str) -> Result<(), String> {
        self.clears.push(format!("{id}:{answer}"));
        Ok(())
    }
    fn notify(&mut self, _title: &str, _body: &str) {}
}

fn sink(path: &Path) -> SinkConfig {
    SinkConfig {
        name: "test".to_string(),
        path: path.to_path_buf(),
        tag: "#fno".to_string(),
        line: crate_file_line(),
        option_line: "    - [ ] {n}. {text}. Pro: {pros}. Con: {cons}".to_string(),
        settle_secs: 120,
        ready_only: false,
        kinds: vec!["question".to_string(), "pin".to_string()],
        match_project: None,
    }
}

fn crate_file_line() -> String {
    "- [ ] {title}. Blocks {blocks}. Recommended: {recommend} {tag} {priority_mark} 📅 {due} ^{id}"
        .to_string()
}

fn items() -> Vec<fno_agents::attention::AttentionItem> {
    project(&question_row(), &[], "", 0)
}

fn state() -> HashMap<String, fno_agents::attention_arm::BlockState> {
    HashMap::new()
}

#[test]
fn deliver_settle_record_flip_journey() {
    let dir = workdir("main");
    let file = dir.join("board.md");
    let mut io = JourneyIo {
        records: 0,
        clears: vec![],
    };
    let mut st = state();
    // Beat 1: deliver.
    let t1 = tick_sink(&items(), &sink(&file), &mut st, 1000, &mut io);
    assert_eq!(t1.delivered, 1, "first beat delivers");
    assert!(file.exists());
    // Beat 2 at t+30: tick not made; nothing records.
    let t2 = tick_sink(&items(), &sink(&file), &mut st, 1030, &mut io);
    assert_eq!(t2.recorded, 0);
    // User ticks option 2 at t+60 (file edited externally).
    let text = std::fs::read_to_string(&file).unwrap();
    let ticked = text.replacen("    - [ ] 2.", "    - [x] 2.", 1);
    std::fs::write(&file, ticked).unwrap();
    // Beat 3 at t+60: the edit is new; settle window restarts.
    let t3 = tick_sink(&items(), &sink(&file), &mut st, 1060, &mut io);
    assert_eq!(t3.recorded, 0, "fresh tick restarts the settle window");
    // Beat 4 at t+60+120: settle passed; record + clear + flip.
    let t4 = tick_sink(&items(), &sink(&file), &mut st, 1180, &mut io);
    assert_eq!(t4.recorded, 1);
    assert_eq!(io.clears.len(), 1);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("- [x]"), "top line flipped");
    assert!(text.contains("Recorded: option 1 (file)"));
}

/// A writer that mutates the file between the arm's read and its write.
struct Saboteur {
    inner: JourneyIo,
    file: PathBuf,
}

impl SinkIo for Saboteur {
    fn read(&mut self, path: &Path) -> std::io::Result<String> {
        self.inner.read(path)
    }
    fn append(&mut self, path: &Path, block: &str) -> std::io::Result<()> {
        self.inner.append(path, block)
    }
    fn write_atomic(&mut self, _path: &Path, _content: &str) -> std::io::Result<()> {
        std::fs::write(&self.file, "another writer won the race\n").unwrap();
        Err(std::io::Error::other("race lost"))
    }
    fn record(
        &mut self,
        i: &fno_agents::attention::AttentionItem,
        s: &str,
        a: &FileAnswer,
    ) -> Result<String, String> {
        self.inner.record(i, s, a)
    }
    fn clear(&mut self, id: &str, a: &str) -> Result<(), String> {
        self.inner.clear(id, a)
    }
    fn notify(&mut self, t: &str, b: &str) {
        self.inner.notify(t, b)
    }
}

#[test]
fn concurrent_writer_is_protected_by_the_guarded_rewrite() {
    let dir = workdir("race");
    let file = dir.join("board.md");
    let mut st = state();
    // Deliver and let the user tick option 1, settle it.
    let mut io = JourneyIo {
        records: 0,
        clears: vec![],
    };
    let _ = tick_sink(&items(), &sink(&file), &mut st, 1000, &mut io);
    let text = std::fs::read_to_string(&file).unwrap();
    let ticked = text.replacen("    - [ ] 1.", "    - [x] 1.", 1);
    std::fs::write(&file, ticked).unwrap();
    let _ = tick_sink(&items(), &sink(&file), &mut st, 1120, &mut io);
    // The first tick after delivery resets `since`; settle from that beat.
    // Now a race: the write fails because another writer changed the file.
    let mut io2 = Saboteur {
        inner: JourneyIo {
            records: 0,
            clears: vec![],
        },
        file: file.clone(),
    };
    let t = tick_sink(&items(), &sink(&file), &mut st, 1300, &mut io2);
    // The record path: the row lands; the guarded rewrite refused.
    assert_eq!(t.skip.as_deref(), Some("file_changed"));
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        after.contains("another writer won the race"),
        "other writer intact"
    );
}

#[test]
fn conflict_markers_refuse_the_beat() {
    let dir = workdir("conflict");
    let file = dir.join("board.md");
    std::fs::write(&file, "- [ ] mine #jc\n<<<<<<< HEAD\n").unwrap();
    let mut st = state();
    let mut io = JourneyIo {
        records: 0,
        clears: vec![],
    };
    let t = tick_sink(&items(), &sink(&file), &mut st, 1000, &mut io);
    assert_eq!(t.skip.as_deref(), Some("conflict_markers"));
}
