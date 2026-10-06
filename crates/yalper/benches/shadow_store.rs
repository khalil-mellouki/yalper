//! Measures the shadow store work of one agent step: open the store, write 3 changed files as blobs, build
//! the new tree from the previous one, and list the changed paths.
//!
//! The store starts with a snapshot of 1,000 files in 111 directories, like the project of the hook latency
//! benchmark. Run with `cargo bench --bench shadow_store`. Set `YALPER_BENCH_CALLS` to change the number of
//! steps.

use std::env;
use std::time::{Duration, Instant};

use yalper::safe_fs::OwnedDir;
use yalper::snapshot::{Change, FileKind, ShadowStore};

const FILES: usize = 1000;
const CHANGED_PER_STEP: usize = 3;
const WARMUP_STEPS: usize = 5;
const DEFAULT_STEPS: usize = 50;

fn file_path(index: usize) -> String {
    format!("pkg{}/mod{}/file{index}.rs", index / 100, index / 10 % 10)
}

fn content(index: usize, version: usize) -> Vec<u8> {
    format!("pub fn value_{index}() -> usize {{\n    {version}\n}}\n")
        .repeat(20)
        .into_bytes()
}

/// One step: returns the new tree and how long the step took.
fn step(dir: &OwnedDir, tree: gix::ObjectId, step: usize) -> (gix::ObjectId, Duration) {
    let start = Instant::now();
    let store = ShadowStore::open(dir).unwrap();
    let changes: Vec<Change> = (0..CHANGED_PER_STEP)
        .map(|offset| {
            let index = (step * CHANGED_PER_STEP + offset) % FILES;
            Change::Upsert {
                path: file_path(index),
                kind: FileKind::Regular,
                blob: store.write_blob(&content(index, step + 1)).unwrap(),
            }
        })
        .collect();
    let new_tree = store.edit_tree(tree, &changes).unwrap();
    let changed = store.changed_paths(tree, new_tree).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(changed.modified.len(), CHANGED_PER_STEP);
    (new_tree, elapsed)
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn main() {
    let steps = env::var("YALPER_BENCH_CALLS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&steps: &usize| steps > 0)
        .unwrap_or(DEFAULT_STEPS);

    let yalper = tempfile::tempdir().unwrap();
    let dir = OwnedDir::open(yalper.path()).unwrap();
    let store = ShadowStore::init(&dir).unwrap();
    let baseline: Vec<Change> = (0..FILES)
        .map(|index| Change::Upsert {
            path: file_path(index),
            kind: FileKind::Regular,
            blob: store.write_blob(&content(index, 0)).unwrap(),
        })
        .collect();
    let mut tree = store.edit_tree(store.empty_tree(), &baseline).unwrap();
    drop(store);

    for index in 0..WARMUP_STEPS {
        tree = step(&dir, tree, index).0;
    }
    let mut times: Vec<Duration> = (0..steps)
        .map(|index| {
            let (new_tree, elapsed) = step(&dir, tree, WARMUP_STEPS + index);
            tree = new_tree;
            elapsed
        })
        .collect();
    times.sort();

    let median = if steps % 2 == 1 {
        millis(times[steps / 2])
    } else {
        (millis(times[steps / 2 - 1]) + millis(times[steps / 2])) / 2.0
    };
    let p90 = millis(times[(steps * 9).div_ceil(10) - 1]);
    println!(
        "shadow_store ({os}, {FILES} files, {CHANGED_PER_STEP} changed per step, {steps} steps, open + \
         write blobs + edit tree + changed paths): median {median:.2} ms, p90 {p90:.2} ms, min {min:.2} ms, \
         max {max:.2} ms",
        os = env::consts::OS,
        min = millis(times[0]),
        max = millis(times[steps - 1]),
    );
}
