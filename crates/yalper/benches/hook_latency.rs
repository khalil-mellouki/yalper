//! Measures how long one `PostToolUse` hook call takes, process spawn included, the way Claude Code runs it.
//!
//! Builds a temporary git project with 1,000 files, then for each call changes 3 files and runs the real
//! `yalper hook` binary with the `PostToolUse` payload of an Edit on stdin (about 32 KB of file content in its
//! response, as Claude Code sends it). Each call records the step: redaction, the event log, and a real snapshot
//! of the project. Prints the median and p90.
//!
//! Run with `cargo bench --bench hook_latency`. Set `YALPER_BENCH_CALLS` to change the number of calls, and
//! `YALPER_BENCH_MAX_MEDIAN_MS` to fail (exit code 1) when the median is that many milliseconds or more: CI
//! runs it as a gate with 50 ms, the limit of done criterion 7 of milestone M1.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use yalper::hook::find_yalper_dir;
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
    for index in 0..FILES {
        let path = file_path(root, index);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = format!("pub fn value_{index}() -> usize {{\n    {index}\n}}\n").repeat(20);
        fs::write(path, body).unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_yalper"))
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "yalper init failed: {output:?}");
}

fn change_files(root: &Path, call: usize) {
    for offset in 0..CHANGED_PER_CALL {
        let path = file_path(root, (call * CHANGED_PER_CALL + offset) % FILES);
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "// edit {call}").unwrap();
    }
}

/// About 32 KB of ordinary Rust, the kind of file an agent edits, with a few words that are secret rule
/// keywords (`key`, `token`, `auth`) as in real code, so redaction runs some rules.
fn original_file() -> String {
    let mut text = String::new();
    for index in 0..120 {
        if index % 30 == 0 {
            text.push_str(&format!(
                "/// Builds client {index} from the configuration: reads the API key from the environment.\n\
                 pub fn client_{index}(config: &Config) -> Result<Client, Error> {{\n    \
                 let api_key = std::env::var(\"API_KEY\")?;\n    \
                 let token = config.auth_token.clone();\n    \
                 Client::builder().api_key(&api_key).token(token).build()\n}}\n\n"
            ));
        } else {
            text.push_str(&format!(
                "/// Sums every {step}th value of `input`, scaled by {index}.\n\
                 pub fn value_{index}(input: &[u64]) -> u64 {{\n    \
                 let mut total = 0;\n    \
                 for (position, item) in input.iter().enumerate() {{\n        \
                 if position % {step} == 0 {{\n            total += item * {index};\n        }}\n    }}\n    \
                 total\n}}\n\n",
                step = index % 7 + 2
            ));
        }
    }
    text
}

/// A `PostToolUse` payload of the Edit tool for the first file `call` changes, shaped like Claude Code's:
/// the edit in `tool_input`, and in `tool_response` the whole original file and the patch.
fn edit_payload(root: &Path, call: usize, original: &str) -> Vec<u8> {
    let path = file_path(root, call * CHANGED_PER_CALL % FILES);
    let old_line = "    total\n}\n";
    let new_lines = format!("    total\n}}\n// edit {call}\n");
    serde_json::json!({
        "session_id": "bench-session",
        "transcript_path": root.join("transcript.jsonl"),
        "cwd": root,
        "permission_mode": "acceptEdits",
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "tool_input": {
            "file_path": path,
            "old_string": old_line,
            "new_string": new_lines,
            "replace_all": false,
        },
        "tool_response": {
            "filePath": path,
            "oldString": old_line,
            "newString": new_lines,
            "originalFile": original,
            "structuredPatch": [{
                "oldStart": 1203,
                "oldLines": 3,
                "newStart": 1203,
                "newLines": 4,
                "lines": ["     }", "     total", " }", format!("+// edit {call}")],
            }],
            "userModified": false,
            "replaceAll": false,
        },
        "tool_use_id": format!("toolu_bench_{call}"),
        "duration_ms": 25,
    })
    .to_string()
    .into_bytes()
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

    let original = original_file();
    for call in 0..WARMUP_CALLS {
        change_files(root, call);
        run_hook(root, &edit_payload(root, call, &original));
    }
    let mut times: Vec<Duration> = (0..calls)
        .map(|call| {
            let call = WARMUP_CALLS + call;
            change_files(root, call);
            run_hook(root, &edit_payload(root, call, &original))
        })
        .collect();
    times.sort();

    assert!(
        !root.join(".yalper").join("errors.log").exists(),
        "the hook logged an error during the benchmark"
    );
    // The last snapshot has the 1,000 files plus `.gitignore` in its stat cache.
    let yalper = find_yalper_dir([root.to_path_buf()]).unwrap();
    let store = Store::open(&yalper.dir, &yalper.token).unwrap();
    let cache = store.file_cache().unwrap();
    assert_eq!(cache.map(|cache| cache.files.len()), Some(FILES + 1));
    // Every call was recorded as a step, and found the 3 files it changed (`yalper init` took the baseline).
    let events = store.events("bench-session").unwrap();
    assert_eq!(events.len(), WARMUP_CALLS + calls);
    assert!(
        events
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
        "hook_latency ({os}, {FILES} files, {CHANGED_PER_CALL} changed per call, {payload_kb} KB Edit \
         payload, {calls} calls): median {median:.1} ms, p90 {p90:.1} ms, min {min:.1} ms, max {max:.1} ms",
        os = env::consts::OS,
        payload_kb = edit_payload(root, 0, &original).len() / 1024,
        min = millis(times[0]),
        max = millis(times[calls - 1]),
    );

    if let Some(limit) = env::var("YALPER_BENCH_MAX_MEDIAN_MS")
        .ok()
        .and_then(|limit| limit.parse::<f64>().ok())
    {
        if median >= limit {
            eprintln!(
                "hook_latency: the median of {median:.1} ms is not under the limit of {limit} ms"
            );
            std::process::exit(1);
        }
        println!("hook_latency: the median is under the limit of {limit} ms");
    }
}
