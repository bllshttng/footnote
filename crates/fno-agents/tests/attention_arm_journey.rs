//! The attention arm journey over real disk IO (temp dir) with fake
//! record/clear/notify: deliver two pages on first sight, nothing before
//! settle, one row + clear + close + move after it, the guarded close, the
//! close-elsewhere close, a conflicted copy ignored, and a hand-authored
//! index kept.

use fno_agents::attention::project;
use fno_agents::attention_arm::{tick_pages, SinkIo};
use fno_agents::attention_file::{parse_page, render_index, render_page, FileAnswer, BASE};
use fno_agents::attention_route::Routing;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn workdir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("attention-journey-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct JourneyIo {
    records: Vec<String>,
    clears: Vec<String>,
    broken: bool,
}

impl JourneyIo {
    fn new() -> JourneyIo {
        JourneyIo {
            records: Vec::new(),
            clears: Vec::new(),
            broken: false,
        }
    }
}

impl SinkIo for JourneyIo {
    fn read(&mut self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn write_atomic(&mut self, path: &Path, content: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, content)
    }
    fn create_new(&mut self, path: &Path, content: &str) -> std::io::Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, content)?;
        Ok(true)
    }
    fn rename(&mut self, from: &Path, to: &Path) -> std::io::Result<()> {
        if let Some(parent) = to.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::rename(from, to)
    }
    fn list_md(&mut self, dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("md") {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }
    fn path_exists(&mut self, path: &Path) -> bool {
        path.exists()
    }
    fn route(&mut self, _item: &fno_agents::attention::AttentionItem) -> Result<Routing, String> {
        if self.broken {
            Err("graph unreadable".to_string())
        } else {
            Ok(Routing::default())
        }
    }
    fn record(
        &mut self,
        item: &fno_agents::attention::AttentionItem,
        _sink: &str,
        answer: &FileAnswer,
    ) -> Result<String, String> {
        self.records.push(format!("{}: {answer:?}", item.id));
        Ok("Recorded: option 1 (file)".to_string())
    }
    fn clear(&mut self, id: &str, answer: &str) -> Result<(), String> {
        self.clears.push(format!("{id}: {answer}"));
        Ok(())
    }
    fn notify(&mut self, _title: &str, _body: &str) {}
}

fn question_row() -> String {
    r#"{"ts":"2026-09-22T12:00:00Z","type":"operator_question","source":"test","data":{"question_id":"q-a1","question":"Which lane?","ask":"pick","session_id":"s1","cwd":"/repo/fno","node":"x-aaaa","asker":"worker-1","options":[{"n":1,"text":"A","next":"x"},{"n":2,"text":"B"}],"context":{"blocked_because":"two doors","options_rationale":"both ship","recommendation":{"option":1,"why":"narrowest"},"reversible":"yes","cost_if_wrong":"little","meanwhile":"stops"}}}"#.to_string()
}

fn items() -> Vec<fno_agents::attention::AttentionItem> {
    project(&question_row(), &[], "", 0)
}

#[test]
fn deliver_settle_record_close_move() {
    let dir = workdir("deliver");
    let mut io = JourneyIo::new();
    let items = items();
    assert_eq!(items.len(), 1);
    let mut state = HashMap::new();

    // Beat 1: the page delivers; the settle window starts.
    let t1 = tick_pages(
        &items,
        &HashMap::new(),
        &dir,
        &mut state,
        1000,
        120,
        &mut io,
    );
    assert_eq!(t1.delivered, 1);
    let page = dir.join("20260922-q-a1-which-lane-x-aaaa.md");
    assert!(
        page.exists(),
        "the page name: {:?}",
        std::fs::read_dir(&dir).map(|d| d.collect::<Vec<_>>())
    );

    // Beat 2: the user ticks option 2. The hash restarts the window.
    let text = std::fs::read_to_string(&page).unwrap();
    let ticked = text.replacen("- [ ] 2.", "- [x] 2.", 1);
    std::fs::write(&page, ticked).unwrap();
    let t2 = tick_pages(
        &items,
        &HashMap::new(),
        &dir,
        &mut state,
        1030,
        120,
        &mut io,
    );
    assert_eq!(t2.recorded, 0, "nothing records before settle");

    // Beat 3: past the window: the answer records, clears, closes, moves.
    let t3 = tick_pages(
        &items,
        &HashMap::new(),
        &dir,
        &mut state,
        1300,
        120,
        &mut io,
    );
    assert_eq!(t3.recorded, 1);
    assert_eq!(t3.closed, 1);
    assert_eq!(io.records.len(), 1);
    assert_eq!(io.clears[0], "q-a1: B");
    assert!(!page.exists(), "the open page moved away");
    let done = dir.join("done/q-a1.md");
    assert!(done.exists());
    let (front, _) = parse_page(&std::fs::read_to_string(&done).unwrap()).unwrap();
    assert_eq!(front.status, "answered");
    assert_eq!(front.answer.as_deref(), Some("B"));
    assert_eq!(front.recorded_by.as_deref(), Some("file_edit"));
    // The index lists the closed page under Done.
    let index = std::fs::read_to_string(dir.join("questions.md")).unwrap();
    assert!(index.contains("[[q-a1|"));
    // The Base exists.
    assert!(dir.join("questions.base").exists());
}

#[test]
fn elsewhere_close_conflicted_copy_hand_authored_index() {
    let dir = workdir("elsewhere");
    let mut io = JourneyIo::new();
    let items = items();
    let mut state = HashMap::new();
    tick_pages(
        &items,
        &HashMap::new(),
        &dir,
        &mut state,
        1000,
        120,
        &mut io,
    );

    // A conflicted copy sits beside the real page; it must never parse.
    let page = dir.join("20260922-q-a1-which-lane-x-aaaa.md");
    let conflicted = dir.join("q-a1 (conflicted copy).md");
    std::fs::copy(&page, &conflicted).unwrap();

    // The question clears at a terminal (answer `narrow` by s9).
    let raw = r#"{"ts":"2026-09-22T20:00:00Z","type":"operator_question_closed","source":"t","data":{"question_id":"q-a1","answer":"narrow","closed_by":"s9"}}"#;
    let closes = fno_agents::attention::closes(raw);
    let t = tick_pages(&[], &closes, &dir, &mut state, 1300, 120, &mut io);
    assert_eq!(t.closed, 1);
    assert!(dir.join("done/q-a1.md").exists());
    let (front, _) =
        parse_page(&std::fs::read_to_string(&dir.join("done/q-a1.md")).unwrap()).unwrap();
    assert_eq!(front.answer.as_deref(), Some("narrow"));
    assert!(conflicted.exists(), "the conflicted copy stays");
    let (cfront, _) = parse_page(&std::fs::read_to_string(&conflicted).unwrap()).unwrap();
    assert_eq!(cfront.status, "open", "the conflicted copy is untouched");
    // The index lists the closed page under Done.
    let index = std::fs::read_to_string(dir.join("questions.md")).unwrap();
    assert!(index.contains("[[q-a1|"));

    // A hand-authored index is left alone.
    std::fs::write(dir.join("questions.md"), "# mine\n").unwrap();
    tick_pages(&[], &HashMap::new(), &dir, &mut state, 1600, 120, &mut io);
    assert_eq!(
        std::fs::read_to_string(dir.join("questions.md")).unwrap(),
        "# mine\n"
    );
}

#[test]
fn routing_failure_delivers_nothing_then_recovers() {
    let dir = workdir("routing");
    let mut io = JourneyIo::new();
    io.broken = true;
    let items = items();
    let mut state = HashMap::new();
    let t = tick_pages(
        &items,
        &HashMap::new(),
        &dir,
        &mut state,
        1000,
        120,
        &mut io,
    );
    assert_eq!(t.delivered, 0);
    assert_eq!(t.skip.as_deref(), Some("routing_unreadable"));
    // The next beat with a readable graph delivers.
    io.broken = false;
    let t2 = tick_pages(
        &items,
        &HashMap::new(),
        &dir,
        &mut state,
        1030,
        120,
        &mut io,
    );
    assert_eq!(t2.delivered, 1);
}

#[test]
fn render_helpers_smoke() {
    let _ = render_index(&[], &[]);
    let _ = render_page(
        &items().remove(0),
        &Routing {
            crown: Some("fno".into()),
            ..Default::default()
        },
    );
    let _ = BASE;
}
