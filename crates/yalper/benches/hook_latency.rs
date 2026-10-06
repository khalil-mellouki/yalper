//! Measures how long one `PostToolUse` hook call takes, process spawn included, the way Claude Code runs it.
//!
//! Builds a temporary git project with 1,000 files, then for each call changes 3 files and runs the real
//! `yalper hook` binary with a `PostToolUse` payload on stdin. Each call records the step: redaction, the
//! event log, and a real snapshot of the project. Prints the median and p90.
//!
//! Run with `cargo bench --bench hook_latency`. Set `YALPER_BENCH_CALLS` to change the number of calls.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use yalper::repo::{GIT_ID_FILE, ID_FILE};
use yalper::safe_fs::OwnedDir;
use yalper::snapshot::ShadowStore;
use yalper::store::Store;

const FILES: usize = 1000;
const CHANGED_PER_CALL: usize = 3;
const WARMUP_CALLS: usize = 5;
const DEFAULT_CALLS: usize = 50;

fn file_path(root: &Path, index: usize) -> PathBuf {
    root.join(format!("pkg{}", index / 100))
        .join(format!("mod{}", index / 10 % 10))
        .join(format!("file{index}.rs"))
}

fn build_project(root: &Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git must be installed to run this benchmark");
    assert!(status.success(), "git init failed");
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    fs::create_dir(root.join(".yalper")).unwrap();
    // The init token `yalper init` writes: the hook only records into a `.yalper/` that carries it.
    let token = "0123456789abcdef0123456789abcdef\n";
    fs::write(root.join(".git").join(GIT_ID_FILE), token).unwrap();
    fs::write(root.join(".yalper").join(ID_FILE), token).unwrap();
    ShadowStore::init(&OwnedDir::open(&root.join(".yalper")).unwrap()).unwrap();

    for index in 0..FILES {
        let path = file_path(root, index);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = format!("pub fn value_{index}() -> usize {{\n    {index}\n}}\n").repeat(20);
        fs::write(path, body).unwrap();
    }
}

fn change_files(root: &Path, call: usize) {
    for offset in 0..CHANGED_PER_CALL {
        let path = file_path(root, (call * CHANGED_PER_CALL + offset) % FILES);
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "// edit {call}").unwrap();
    }
}

fn run_hook(root: &Path, payload: &[u8]) -> Duration {
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_yalper"))
        .arg("hook")
        .current_dir(root)
        .env("CLAUDE_PROJECT_DIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    let output = child.wait_with_output().unwrap();
    let elapsed = start.elapsed();
    assert!(
        output.status.success() && output.stdout.is_empty() && output.stderr.is_empty(),
        "hook did not exit silently: {output:?}"
    );
    elapsed
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn main() {
    let calls = env::var("YALPER_BENCH_CALLS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&calls: &usize| calls > 0)
        .unwrap_or(DEFAULT_CALLS);

    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    build_project(root);

    let payload = serde_json::json!({
        "session_id": "bench-session",
        "transcript_path": root.join("transcript.jsonl"),
        "cwd": root,
        "permission_mode": "default",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "sed -i 's/a/b/' pkg0/mod0/file0.rs", "description": "Edit files"},
        "tool_response": {"stdout": "", "stderr": "", "interrupted": false, "isImage": false},
        "tool_use_id": "toolu_bench",
        "duration_ms": 25,
    })
    .to_string()
    .into_bytes();

    for call in 0..WARMUP_CALLS {
        change_files(root, call);
        run_hook(root, &payload);
    }
    let mut times: Vec<Duration> = (0..calls)
        .map(|call| {
            change_files(root, WARMUP_CALLS + call);
            run_hook(root, &payload)
        })
        .collect();
    times.sort();

    assert!(
        !root.join(".yalper").join("errors.log").exists(),
        "the hook logged an error during the benchmark"
    );
    // The last snapshot has the 1,000 files plus `.gitignore` in its stat cache.
    let yalper = OwnedDir::open(&root.join(".yalper")).unwrap();
    let store = Store::open(&yalper).unwrap();
    let cache = store.file_cache().unwrap();
    assert_eq!(cache.map(|cache| cache.files.len()), Some(FILES + 1));
    // Every call was recorded as a step. The first one took the first snapshot of the whole project, each
    // later one found the 3 files it changed.
    let events = store.events("bench-session").unwrap();
    assert_eq!(events.len(), WARMUP_CALLS + calls);
    assert!(
        events[1..]
            .iter()
            .all(|event| event.files_changed == Some(CHANGED_PER_CALL as u32))
    );

    let median = if calls % 2 == 1 {
        millis(times[calls / 2])
    } else {
        (millis(times[calls / 2 - 1]) + millis(times[calls / 2])) / 2.0
    };
    let p90 = millis(times[(calls * 9).div_ceil(10) - 1]);
    println!(
        "hook_latency ({os}, {FILES} files, {CHANGED_PER_CALL} changed per call, {calls} calls): \
         median {median:.1} ms, p90 {p90:.1} ms, min {min:.1} ms, max {max:.1} ms",
        os = env::consts::OS,
        min = millis(times[0]),
        max = millis(times[calls - 1]),
    );
}
