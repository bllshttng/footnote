//! A registry write queues on the graph flock instead of racing the
//! 5 s busy handler. Parent helpers resolve through the glob.
use super::*;

#[test]
fn registry_write_queues_behind_a_held_graph_lock() {
    let dir = tmpdir("registry-queues");
    let path = dir.join("agents/registry.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let (_, before) = crate::registry_store::read_versioned(&path).unwrap();
    // Another writer holds the graph flock the registry write queues on;
    // the ticket queue serves this writer after the holder releases
    // instead of BEGIN IMMEDIATE dying with "database is locked".
    let graph = dir.join("graph.json");
    let holder =
        crate::graph_store::BoundedLock::acquire(&graph, std::time::Duration::from_secs(1))
            .unwrap();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        crate::state::update_registry(&writer_path, |r| {
            r.entries.push(sample_entry("queue-worker"))
        })
        .unwrap()
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !writer.is_finished(),
        "the registry write queues behind the holder; it must not fail or finish"
    );
    drop(holder);
    writer.join().unwrap();
    let (_, after) = crate::registry_store::read_versioned(&path).unwrap();
    assert!(after > before, "the queued write landed once served");
    std::fs::remove_dir_all(dir).ok();
}
