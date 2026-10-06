//! Runs the real `yalper hook` binary the way Claude Code does and checks that it never signals anything
//! back: exit code 0, nothing on stdout or stderr, errors only in `.yalper/errors.log`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::json;
use tempfile::TempDir;
use yalper::hook::{
    BENCH_SNAPSHOT_ENV, ERRORS_LOG, HookEvent, HookInput, MAX_PAYLOAD_BYTES, YALPER_DIR,
};
use yalper::safe_fs::OwnedDir;
use yalper::snapshot::ShadowStore;
use yalper::store::Store;

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks");
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
}

/// A temporary git project (it only needs a `.git` entry) with `.yalper/` in it.
fn project() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::create_dir(dir.path().join(YALPER_DIR)).unwrap();
    dir
}

struct Hook<'a> {
    stdin: &'a [u8],
    project_dir: Option<&'a Path>,
    current_dir: &'a Path,
    force_panic: bool,
}

impl Hook<'_> {
    fn run(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_yalper"));
        command
            .arg("hook")
            .current_dir(self.current_dir)
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("YALPER_TEST_PANIC")
            .env_remove(BENCH_SNAPSHOT_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = self.project_dir {
            command.env("CLAUDE_PROJECT_DIR", dir);
        }
        if self.force_panic {
            command.env("YALPER_TEST_PANIC", "1");
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(self.stdin).unwrap();
        child.wait_with_output().unwrap()
    }
}

/// Runs the hook with `CLAUDE_PROJECT_DIR` and the current directory both set to `dir`.
fn run_in(dir: &Path, stdin: &[u8]) -> Output {
    Hook {
        stdin,
        project_dir: Some(dir),
        current_dir: dir,
        force_panic: false,
    }
    .run()
}

fn assert_silent_success(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty(), "stdout: {output:?}");
    assert!(output.stderr.is_empty(), "stderr: {output:?}");
}

fn error_lines(project: &Path) -> Vec<String> {
    fs::read_to_string(project.join(YALPER_DIR).join(ERRORS_LOG))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn fixtures_cover_the_six_registered_events() {
    let inputs: Vec<HookInput> = fixtures()
        .iter()
        .map(|path| {
            let value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            HookInput::from_value(value).unwrap()
        })
        .collect();
    let events: Vec<&HookEvent> = inputs.iter().map(|input| &input.event).collect();
    for expected in [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::Stop,
        HookEvent::SessionEnd,
    ] {
        assert!(events.contains(&&expected), "no fixture for {expected:?}");
    }

    let windows_path = inputs.iter().find_map(|input| {
        input.tool_input.as_ref()?["file_path"]
            .as_str()
            .filter(|path| path.starts_with("C:\\"))
    });
    assert_eq!(
        windows_path,
        Some("C:\\Users\\dev\\project\\src\\factorial.rs")
    );

    let failure = inputs
        .iter()
        .find(|input| input.event == HookEvent::PostToolUseFailure)
        .unwrap();
    assert!(
        failure
            .error
            .as_deref()
            .unwrap()
            .starts_with("Exit code 1\n")
    );
    assert_eq!(failure.duration_ms, Some(4187));
}

#[test]
fn every_fixture_exits_zero_with_no_output_and_no_error() {
    let project = project();
    for fixture in fixtures() {
        let output = run_in(project.path(), &fs::read(&fixture).unwrap());
        assert_silent_success(&output);
        assert_eq!(
            error_lines(project.path()),
            Vec::<String>::new(),
            "{fixture:?}"
        );
    }
}

#[test]
fn invalid_json_is_logged_and_exits_zero() {
    let project = project();
    let output = run_in(project.path(), b"{\"session_id\": ");
    assert_silent_success(&output);
    let lines = error_lines(project.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("invalid hook input"), "{lines:?}");
}

#[test]
fn empty_stdin_is_logged_and_exits_zero() {
    let project = project();
    let output = run_in(project.path(), b"");
    assert_silent_success(&output);
    assert_eq!(error_lines(project.path()).len(), 1);
}

#[test]
fn missing_required_field_is_logged_with_the_event_name() {
    let project = project();
    let output = run_in(project.path(), br#"{"hook_event_name": "Stop"}"#);
    assert_silent_success(&output);
    let lines = error_lines(project.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(" Stop "), "{lines:?}");
    assert!(lines[0].contains("session_id"), "{lines:?}");
}

#[test]
fn project_without_yalper_dir_is_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    for stdin in [
        &b"not json"[..],
        b"",
        br#"{"session_id":"s","hook_event_name":"Stop"}"#,
    ] {
        let output = run_in(dir.path(), stdin);
        assert_silent_success(&output);
    }
    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [".git"]);
}

#[test]
fn unwritable_error_log_still_exits_zero() {
    let project = project();
    // A directory where the log file should be makes every write to it fail, on every platform.
    fs::create_dir(project.path().join(YALPER_DIR).join(ERRORS_LOG)).unwrap();
    let output = run_in(project.path(), b"not json");
    assert_silent_success(&output);
}

#[cfg(unix)]
#[test]
fn read_only_yalper_dir_still_exits_zero() {
    use std::os::unix::fs::PermissionsExt;

    let project = project();
    let yalper_dir = project.path().join(YALPER_DIR);
    fs::set_permissions(&yalper_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let output = run_in(project.path(), b"not json");
    fs::set_permissions(&yalper_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_silent_success(&output);
}

// The forced panic only exists in debug builds, so this test cannot run against a release binary.
#[cfg(debug_assertions)]
#[test]
fn panic_is_caught_logged_as_one_line_and_exits_zero() {
    let project = project();
    let stdin = fs::read(&fixtures()[0]).unwrap();
    let output = Hook {
        stdin: &stdin,
        project_dir: Some(project.path()),
        current_dir: project.path(),
        force_panic: true,
    }
    .run();
    assert_silent_success(&output);
    let lines = error_lines(project.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("panic at "), "{lines:?}");
    assert!(
        lines[0].contains("forced by YALPER_TEST_PANIC"),
        "{lines:?}"
    );
}

#[test]
fn yalper_dir_is_found_from_a_subdirectory_of_the_project_dir() {
    let project = project();
    let deep = project.path().join("src").join("nested");
    fs::create_dir_all(&deep).unwrap();
    let output = run_in(&deep, b"not json");
    assert_silent_success(&output);
    assert_eq!(error_lines(project.path()).len(), 1);
}

#[test]
fn payload_cwd_is_used_when_project_dir_is_not_set() {
    let project = project();
    let elsewhere = tempfile::tempdir().unwrap();
    let payload = json!({"hook_event_name": "Stop", "cwd": project.path()}).to_string();
    let output = Hook {
        stdin: payload.as_bytes(),
        project_dir: None,
        current_dir: elsewhere.path(),
        force_panic: false,
    }
    .run();
    assert_silent_success(&output);
    assert_eq!(error_lines(project.path()).len(), 1);
}

#[test]
fn current_dir_is_used_as_the_last_resort() {
    let project = project();
    let output = Hook {
        stdin: b"not json",
        project_dir: None,
        current_dir: project.path(),
        force_panic: false,
    }
    .run();
    assert_silent_success(&output);
    assert_eq!(error_lines(project.path()).len(), 1);
}

#[test]
fn full_error_log_is_emptied_before_appending() {
    let project = project();
    let log = project.path().join(YALPER_DIR).join(ERRORS_LOG);
    fs::write(&log, "x".repeat(1024 * 1024)).unwrap();
    let output = run_in(project.path(), b"not json");
    assert_silent_success(&output);
    let lines = error_lines(project.path());
    assert_eq!(
        lines.len(),
        1,
        "log has {} bytes",
        fs::metadata(&log).unwrap().len()
    );
    assert!(lines[0].contains("invalid hook input"));
}

#[test]
fn payload_over_the_size_limit_is_logged_and_exits_zero() {
    let project = project();
    let stdin = vec![b' '; usize::try_from(MAX_PAYLOAD_BYTES).unwrap() + 1];
    let output = run_in(project.path(), &stdin);
    assert_silent_success(&output);
    let lines = error_lines(project.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("hook input too large"), "{lines:?}");
}

#[test]
fn project_dir_without_yalper_does_not_fall_back_to_cwd() {
    let session_root = tempfile::tempdir().unwrap();
    fs::create_dir(session_root.path().join(".git")).unwrap();
    let other = project();
    let payload = json!({"hook_event_name": "Stop", "cwd": other.path()}).to_string();
    let output = Hook {
        stdin: payload.as_bytes(),
        project_dir: Some(session_root.path()),
        current_dir: other.path(),
        force_panic: false,
    }
    .run();
    assert_silent_success(&output);
    assert_eq!(error_lines(other.path()), Vec::<String>::new());
}

#[test]
fn yalper_dir_above_the_git_root_is_not_used() {
    let parent = tempfile::tempdir().unwrap();
    fs::create_dir(parent.path().join(YALPER_DIR)).unwrap();
    let repo = parent.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let output = run_in(&repo, b"not json");
    assert_silent_success(&output);
    assert_eq!(error_lines(parent.path()), Vec::<String>::new());
}

#[test]
fn yalper_dir_outside_a_git_repository_is_not_used() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(YALPER_DIR)).unwrap();
    let output = run_in(dir.path(), b"not json");
    assert_silent_success(&output);
    assert_eq!(error_lines(dir.path()), Vec::<String>::new());
}

/// Makes `link` point to the directory `target`: a symlink on Unix, a junction on Windows (which needs no
/// special privilege).
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let status = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[test]
fn linked_yalper_dir_is_not_used() {
    let outside = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir(repo.path().join(".git")).unwrap();
    link_dir(outside.path(), &repo.path().join(YALPER_DIR));

    let output = run_in(repo.path(), b"not json");
    assert_silent_success(&output);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn linked_error_log_is_not_written() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("victim.txt");
    fs::write(&target, "keep me").unwrap();
    let project = project();
    let link = project.path().join(YALPER_DIR).join(ERRORS_LOG);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        // Creating file symlinks needs Developer Mode or admin rights on Windows.
        return;
    }

    let output = run_in(project.path(), b"not json");
    assert_silent_success(&output);
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
}

/// Runs the hook with the benchmark's snapshot entry point turned on.
fn run_with_snapshot(dir: &Path, stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_yalper"))
        .arg("hook")
        .current_dir(dir)
        .env("CLAUDE_PROJECT_DIR", dir)
        .env(BENCH_SNAPSHOT_ENV, "1")
        .env_remove("YALPER_TEST_PANIC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn the_snapshot_entry_point_records_the_project_silently() {
    let project = project();
    let yalper = OwnedDir::open(&project.path().join(YALPER_DIR)).unwrap();
    ShadowStore::init(&yalper).unwrap();
    fs::write(project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let stdin = fs::read(&fixtures()[0]).unwrap();

    let output = run_with_snapshot(project.path(), &stdin);
    assert_silent_success(&output);
    assert_eq!(error_lines(project.path()), Vec::<String>::new());
    let cache = Store::open(&yalper).unwrap().file_cache().unwrap().unwrap();
    let paths: Vec<&str> = cache.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["main.rs"]);
}

#[test]
fn a_failed_snapshot_is_logged_and_exits_zero() {
    // No shadow store: the snapshot cannot store the new file.
    let project = project();
    fs::write(project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let stdin = fs::read(&fixtures()[0]).unwrap();

    let output = run_with_snapshot(project.path(), &stdin);
    assert_silent_success(&output);
    let lines = error_lines(project.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
}
