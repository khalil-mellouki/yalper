//! Measures how long switching a bisect test copy to the next step takes after a test run: undo what the run
//! changed, diff the two trees, and write the files that differ.
//!
//! The project has 1,000 files in 111 folders, like the hook latency benchmark, and the copy, in the project's
//! cache folder under a temporary base folder, alternates between two trees 5 files apart. Before each switch a
//! fake test run changes 2 files of the tree and creates 1. Prints the median and p90 of the switch, and the
//! time of the first fill of an empty copy (report only).
//!
//! Run with `cargo bench --bench test_copy`. Set `YALPER_BENCH_CALLS` to change the number of switches, and
//! `YALPER_BENCH_MAX_MEDIAN_MS` to fail (exit code 1) when the median is that many milliseconds or more: CI
//! runs it as a gate with 1,000 ms, the limit of done criterion 3 of milestone M2.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use yalper::bisect::{CacheDir, Switched, TestCopy};
use yalper::repo::Token;
use yalper::safe_fs::OwnedDir;
use yalper::snapshot::{Change, FileKind, ShadowStore};

const FILES: usize = 1000;
/// The files that differ between the two trees.
const DIFFERING: [usize; 5] = [0, 200, 400, 600, 800];
const WARMUP_SWITCHES: usize = 3;
const DEFAULT_SWITCHES: usize = 30;

fn file_path(index: usize) -> String {
    format!("pkg{}/mod{}/file{index}.rs", index / 100, index / 10 % 10)
}

fn content(index: usize, version: usize) -> Vec<u8> {
    format!("pub fn value_{index}() -> usize {{\n    {version}\n}}\n")
        .repeat(20)
        .into_bytes()
}

fn upsert(store: &ShadowStore, index: usize, version: usize) -> Change {
    Change::Upsert {
        path: file_path(index),
        kind: FileKind::Regular,
        blob: store.write_blob(&content(index, version)).unwrap(),
    }
}

/// What a test run does to the copy: changes 2 files of the tree (never ones the next switch rewrites
/// anyway) and creates 1.
fn fake_run(copy: &Path, run: usize) {
    for index in [101 + run % 50, 901 + run % 50] {
        let path = copy.join(file_path(index));
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "// changed by run {run}").unwrap();
    }
    fs::write(
        copy.join("pkg5").join(format!("created{run}.txt")),
        "created",
    )
    .unwrap();
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn main() {
    let switches = env::var("YALPER_BENCH_CALLS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&switches: &usize| switches > 0)
        .unwrap_or(DEFAULT_SWITCHES);

    let project = tempfile::tempdir().unwrap();
    let yalper_path = project.path().join(".yalper");
    fs::create_dir(&yalper_path).unwrap();
    let yalper = OwnedDir::open(&yalper_path).unwrap();
    let token = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
    let store = ShadowStore::init(&yalper, &token).unwrap();
    let all: Vec<Change> = (0..FILES).map(|index| upsert(&store, index, 0)).collect();
    let tree_a = store.edit_tree(store.empty_tree(), &all).unwrap();
    let changed: Vec<Change> = DIFFERING
        .iter()
        .map(|&index| upsert(&store, index, 1))
        .collect();
    let tree_b = store.edit_tree(tree_a, &changed).unwrap();

    // The project's cache folder in a temporary base folder, as `YALPER_CACHE_DIR` would give.
    let cache_base = tempfile::tempdir().unwrap();
    let cache =
        CacheDir::open_in(&cache_base.path().join("yalper"), project.path(), &token).unwrap();
    let mut copy = TestCopy::open(&cache, 0).unwrap();
    let start = Instant::now();
    copy.switch(&store, tree_a).unwrap();
    let first_fill = start.elapsed();

    let mut times = Vec::with_capacity(switches);
    for run in 0..WARMUP_SWITCHES + switches {
        fake_run(copy.path(), run);
        let to = if run % 2 == 0 { tree_b } else { tree_a };
        let start = Instant::now();
        let switched = copy.switch(&store, to).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(
            switched,
            Switched {
                written: DIFFERING.len() + 2,
                removed: 0,
                restored: 2,
                deleted: 1,
            }
        );
        if run >= WARMUP_SWITCHES {
            times.push(elapsed);
        }
    }

    times.sort();
    let median = if switches % 2 == 1 {
        millis(times[switches / 2])
    } else {
        (millis(times[switches / 2 - 1]) + millis(times[switches / 2])) / 2.0
    };
    let p90 = millis(times[(switches * 9).div_ceil(10) - 1]);
    println!(
        "test_copy ({os}, {FILES} files, trees {differing} files apart, each run changes 2 files and creates 1, \
         {switches} switches): switch after a run: median {median:.1} ms, p90 {p90:.1} ms; first fill of an \
         empty copy: {first_fill:.1} ms",
        os = env::consts::OS,
        differing = DIFFERING.len(),
        first_fill = millis(first_fill),
    );

    if let Some(limit) = env::var("YALPER_BENCH_MAX_MEDIAN_MS")
        .ok()
        .and_then(|limit| limit.parse::<f64>().ok())
    {
        if median >= limit {
            eprintln!(
                "test_copy: the median of {median:.1} ms is not under the limit of {limit} ms"
            );
            std::process::exit(1);
        }
        println!("test_copy: the median is under the limit of {limit} ms");
    }
}
