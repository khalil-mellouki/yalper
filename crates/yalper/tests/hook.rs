//! Runs the real `yalper hook` binary the way Claude Code does and checks that it never signals anything
//! back: exit code 0, nothing on stdout or stderr, errors only in `.yalper/errors.log`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::json;
use yalper::hook::{
    ERRORS_LOG, HookEvent, HookInput, MAX_PAYLOAD_BYTES, YALPER_DIR, find_yalper_dir,
};
use yalper::repo::{GIT_ID_FILE, ID_FILE};
use yalper::snapshot::SNAPSHOTS_DIR;
use yalper::store::{DATABASE_FILE, Event, Store};

mod common;
use common::{TOKEN, project};

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks");
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
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
        input.tool_input()?["file_path"]
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
fn panic_is_caught_logged_as_one_line_without_its_message_and_exits_zero() {
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
    assert!(lines[0].contains("hook.rs"), "{lines:?}");
    // The message quotes the session id, and the log only gets the location.
    assert!(!lines[0].contains("forced by"), "{lines:?}");
    assert!(!lines[0].contains("00893aaf"), "{lines:?}");
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
    let parent = project();
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
    fs::write(dir.path().join(YALPER_DIR).join(ID_FILE), TOKEN).unwrap();
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

/// The steps recorded in the project at `root`, session by session.
fn recorded_events(root: &Path) -> Vec<Event> {
    let found = find_yalper_dir([root.to_path_buf()]).unwrap();
    let store = Store::open(&found.dir, &found.token).unwrap();
    store
        .sessions()
        .unwrap()
        .iter()
        .flat_map(|session| store.events(&session.id).unwrap())
        .collect()
}

/// The names of the entries of `dir`, sorted.
fn entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(name)
}

#[test]
fn every_fixture_is_recorded_as_a_step() {
    let project = project();
    for fixture in fixtures() {
        assert_silent_success(&run_in(project.path(), &fs::read(&fixture).unwrap()));
    }
    let events = recorded_events(project.path());
    let steps: Vec<u32> = events.iter().map(|event| event.step).collect();
    assert_eq!(steps, (1..=7).collect::<Vec<u32>>());
    assert_eq!(error_lines(project.path()), Vec::<String>::new());
}

#[test]
fn a_yalper_dir_init_did_not_create_is_never_written() {
    let other = "fedcba9876543210fedcba9876543210";
    // (token in `.yalper/id`, token in `.git/yalper-id`)
    for (yalper_token, git_token) in [
        (None, Some(TOKEN)),
        (Some(other), Some(TOKEN)),
        (Some(TOKEN), None),
    ] {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        let yalper_dir = root.path().join(YALPER_DIR);
        fs::create_dir(&yalper_dir).unwrap();
        if let Some(token) = yalper_token {
            fs::write(yalper_dir.join(ID_FILE), token).unwrap();
        }
        if let Some(token) = git_token {
            fs::write(root.path().join(".git").join(GIT_ID_FILE), token).unwrap();
        }
        fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let before = entry_names(&yalper_dir);

        for fixture in fixtures() {
            assert_silent_success(&run_in(root.path(), &fs::read(&fixture).unwrap()));
        }
        assert_silent_success(&run_in(root.path(), b"not json"));
        assert_eq!(
            entry_names(&yalper_dir),
            before,
            "{yalper_token:?} {git_token:?}"
        );
    }
}

#[test]
fn a_failed_snapshot_still_records_the_step_and_is_logged() {
    let project = project();
    // Without its snapshot store, the snapshot cannot store the new file.
    fs::remove_dir_all(project.path().join(YALPER_DIR).join(SNAPSHOTS_DIR)).unwrap();
    fs::write(project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let stdin = fs::read(fixture("post_tool_use_bash_subagent.json")).unwrap();

    assert_silent_success(&run_in(project.path(), &stdin));
    let lines = error_lines(project.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(" PostToolUse "), "{lines:?}");
    assert!(
        lines[0].contains("step 1 recorded without a snapshot"),
        "{lines:?}"
    );
    let events = recorded_events(project.path());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tree_id, None);
    assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
}

/// Starts the hook the way Claude Code does, with `CLAUDE_PROJECT_DIR` and the current directory set to
/// `dir`, and writes `stdin` to it.
fn start_in(dir: &Path, stdin: &[u8]) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_yalper"))
        .arg("hook")
        .current_dir(dir)
        .env("CLAUDE_PROJECT_DIR", dir)
        .env_remove("YALPER_TEST_PANIC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child
}

#[test]
fn concurrent_post_tool_use_hooks_both_record_a_step() {
    const ROUNDS: u32 = 5;
    let project = project();
    let payload = |round: u32, call: u32| {
        json!({
            "session_id": "parallel",
            "hook_event_name": "PostToolUse",
            "cwd": project.path(),
            "tool_name": "Write",
            "tool_input": {"file_path": format!("f{round}_{call}.txt"), "content": "x"},
            "tool_response": {"type": "create"},
            "tool_use_id": format!("toolu_{round}_{call}"),
        })
        .to_string()
    };
    for round in 0..ROUNDS {
        // Two tool calls ran in parallel, then both of their PostToolUse hooks run at once.
        for call in 0..2 {
            fs::write(project.path().join(format!("f{round}_{call}.txt")), "x").unwrap();
        }
        let first = start_in(project.path(), payload(round, 0).as_bytes());
        let second = start_in(project.path(), payload(round, 1).as_bytes());
        assert_silent_success(&first.wait_with_output().unwrap());
        assert_silent_success(&second.wait_with_output().unwrap());
    }

    assert_eq!(error_lines(project.path()), Vec::<String>::new());
    let events = recorded_events(project.path());
    let steps: Vec<u32> = events.iter().map(|event| event.step).collect();
    assert_eq!(steps, (1..=2 * ROUNDS).collect::<Vec<u32>>());
    let mut tool_use_ids: Vec<String> = events
        .iter()
        .map(|event| event.tool_use_id.clone().unwrap())
        .collect();
    tool_use_ids.sort();
    let mut expected: Vec<String> = (0..ROUNDS)
        .flat_map(|round| (0..2).map(move |call| format!("toolu_{round}_{call}")))
        .collect();
    expected.sort();
    assert_eq!(tool_use_ids, expected);
    // Each round's two new files are counted once, by whichever hook took its snapshot first.
    let changed: u32 = events
        .iter()
        .map(|event| event.files_changed.unwrap())
        .sum();
    assert_eq!(changed, 2 * ROUNDS);
}

/// A fake GitHub token, built at run time so that no token-shaped literal is in the repository.
fn fake_github_token(seed: &str) -> String {
    format!("ghp_{}", seed.chars().cycle().take(36).collect::<String>())
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[test]
fn secrets_never_reach_the_database_its_wal_or_the_error_log() {
    let project = project();
    let in_prompt = fake_github_token("q7W2e9R4t1Y6u3I8o5P0");
    let in_output = fake_github_token("Z8x3C6v1B9n4M7a2S5d0");
    let in_columns = fake_github_token("L4k8J2h6G1f5D9s3A7p0");
    let session = json!({
        "session_id": "secret-session",
        "hook_event_name": "SessionStart",
        "cwd": project.path(),
        "transcript_path": format!("/home/dev/{in_columns}/t.jsonl"),
        "source": "startup",
    });
    let prompt = json!({
        "session_id": "secret-session",
        "hook_event_name": "UserPromptSubmit",
        "cwd": project.path(),
        "prompt": format!("Use the token {in_prompt} to clone the repository"),
    });
    let tool = json!({
        "session_id": "secret-session",
        "hook_event_name": "PostToolUse",
        "cwd": project.path(),
        "agent_id": in_columns,
        "tool_name": "Bash",
        "tool_input": {"command": "cat .env"},
        "tool_response": {"stdout": format!("GITHUB_TOKEN={in_output}\n"), "stderr": ""},
        "tool_use_id": "toolu_secret",
    });
    for payload in [&session, &prompt, &tool] {
        assert_silent_success(&run_in(project.path(), payload.to_string().as_bytes()));
    }
    // Calls that fail are logged, and their payload must not reach the log.
    let unparsable = format!("{{\"hook_event_name\": \"Stop\", \"prompt\": \"{in_prompt}\"");
    let no_session = json!({"session_id": 7, "hook_event_name": "Stop", "x": in_output});
    let unknown_event = json!({"session_id": "s", "hook_event_name": in_output});
    for stdin in [
        unparsable,
        no_session.to_string(),
        unknown_event.to_string(),
    ] {
        assert_silent_success(&run_in(project.path(), stdin.as_bytes()));
    }

    let yalper_dir = project.path().join(YALPER_DIR);
    let read = |name: &str| fs::read(yalper_dir.join(name)).unwrap_or_default();
    let mut database = read(DATABASE_FILE);
    database.extend(read(&format!("{DATABASE_FILE}-wal")));
    let errors_log = read(ERRORS_LOG);
    assert_eq!(
        error_lines(project.path()).len(),
        2,
        "the failed calls are logged"
    );
    for secret in [&in_prompt, &in_output, &in_columns] {
        let body = &secret["ghp_".len()..];
        assert!(!contains(&database, body), "{secret} reached the database");
        assert!(!contains(&errors_log, body), "{secret} reached errors.log");
    }
    // The steps were recorded, with the secrets masked.
    assert!(contains(&database, "[REDACTED:github-pat]"));
    let events = recorded_events(project.path());
    assert_eq!(events.len(), 3);
    assert_eq!(events[2].agent_id.as_deref(), Some("[REDACTED:github-pat]"));
}
