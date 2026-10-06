//! Measures the shadow store work of one agent step: open the store, write 3 changed files as blobs, build
//! the new tree from the previous one, then (reported separately) list the changed paths.
//!
//! The store starts with a snapshot of 1,000 files in 111 directories, like the project of the hook latency
//! benchmark. Run with `cargo bench --bench shadow_store`. Set `YALPER_BENCH_CALLS` to change the number of
//! steps.

use std::env;
use std::time::{Duration, Instant};

use yalper::repo::Token;
use yalper::safe_fs::OwnedDir;
use yalper::snapshot::{Change, FileKind, ShadowStore};

const FILES: usize = 1000;
const CHANGED_PER_STEP: usize = 3;
const WARMUP_STEPS: usize = 5;
const DEFAULT_STEPS: usize = 50;

fn token() -> Token {
    Token::parse("0123456789abcdef0123456789abcdef").unwrap()
}

fn file_path(index: usize) -> String {
    format!("pkg{}/mod{}/file{index}.rs", index / 100, index / 10 % 10)
}

fn content(index: usize, version: usize) -> Vec<u8> {
    format!("pub fn value_{index}() -> usize {{\n    {version}\n}}\n")
        .repeat(20)
        .into_bytes()
}

/// One step: returns the new tree, the time to build it (open, blobs, tree), and the time including the
/// changed paths listing.
fn step(dir: &OwnedDir, tree: gix::ObjectId, step: usize) -> (gix::ObjectId, Duration, Duration) {
    let start = Instant::now();
    let store = ShadowStore::open(dir, &token()).unwrap();
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
    let built = start.elapsed();
    let changed = store.changed_paths(tree, new_tree).unwrap();
    let with_diff = start.elapsed();
    assert_eq!(changed.modified.len(), CHANGED_PER_STEP);
    (new_tree, built, with_diff)
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Median and p90 of `times`, in milliseconds.
fn summary(times: &mut [Duration]) -> String {
    times.sort();
    let steps = times.len();
    let median = if steps % 2 == 1 {
        millis(times[steps / 2])
    } else {
        (millis(times[steps / 2 - 1]) + millis(times[steps / 2])) / 2.0
    };
    let p90 = millis(times[(steps * 9).div_ceil(10) - 1]);
    format!("median {median:.2} ms, p90 {p90:.2} ms")
}

fn main() {
    let steps = env::var("YALPER_BENCH_CALLS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&steps: &usize| steps > 0)
        .unwrap_or(DEFAULT_STEPS);

    let yalper = tempfile::tempdir().unwrap();
    let dir = OwnedDir::open(yalper.path()).unwrap();
    let store = ShadowStore::init(&dir, &token()).unwrap();
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
    let (mut built, mut with_diff) = (Vec::new(), Vec::new());
    for index in 0..steps {
        let (new_tree, build_time, diff_time) = step(&dir, tree, WARMUP_STEPS + index);
        tree = new_tree;
        built.push(build_time);
        with_diff.push(diff_time);
    }

    println!(
        "shadow_store ({os}, {FILES} files, {CHANGED_PER_STEP} changed per step, {steps} steps): open + write \
         blobs + edit tree: {built}; with changed paths: {with_diff}",
        os = env::consts::OS,
        built = summary(&mut built),
        with_diff = summary(&mut with_diff),
    );
}
