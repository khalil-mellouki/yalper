//! Runs the real `yalper init` binary in temporary git repositories.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{EXE, files, git, repository, yalper};
use serde_json::{Value, json};
use yalper::hook::{YALPER_DIR, find_yalper_dir};
use yalper::init::{EVENTS, is_yalper_handler};
use yalper::repo::{GIT_ID_FILE, ID_FILE, Token};
use yalper::snapshot::SNAPSHOTS_DIR;
use yalper::store::{DATABASE_FILE, Store};

fn init(dir: &Path) -> Output {
    yalper(dir, &["init"])
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!stdout(output).contains("warning"), "{output:?}");
}

fn settings_path(root: &Path) -> PathBuf {
    root.join(".claude").join("settings.local.json")
}

fn settings(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(settings_path(root)).unwrap()).unwrap()
}

/// The token in `path`, which must hold exactly the token and a newline.
fn token_file(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    let token = text.strip_suffix('\n').unwrap();
    assert!(Token::parse(token).is_some(), "{text:?}");
    token.to_owned()
}

#[test]
fn a_fresh_repository_is_set_up_for_recording() {
    let repo = repository();
    let root = repo.path();
    let output = init(root);
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains(".yalper/: created"), "{text}");
    assert!(text.contains("Baseline snapshot: 1 file"), "{text}");
    assert!(text.contains("trusted"), "{text}");
    assert!(text.lines().count() <= 8, "{text}");

    // The same init token on both sides, from a fresh random source.
    let token = token_file(&root.join(YALPER_DIR).join(ID_FILE));
    assert_eq!(token_file(&root.join(".git").join(GIT_ID_FILE)), token);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.join(YALPER_DIR))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    // The hooks are registered in exec form for the six events, synchronous.
    let settings = settings(root);
    let hooks = settings["hooks"].as_object().unwrap();
    assert_eq!(hooks.len(), EVENTS.len());
    for event in EVENTS {
        let name = event.name().unwrap();
        let mut expected =
            json!({"type": "command", "command": EXE, "args": ["hook"], "timeout": 30});
        if name == "SessionEnd" {
            expected.as_object_mut().unwrap().remove("timeout");
        }
        assert_eq!(hooks[name], json!([{"hooks": [expected]}]), "{name}");
    }
    assert!(!root.join(".claude").join("settings.json").exists());

    // Git ignores everything init created: only the user's file is untracked.
    let exclude = fs::read_to_string(root.join(".git").join("info").join("exclude")).unwrap();
    assert!(
        exclude.ends_with("\n.yalper/\n.claude/settings.local.json\n"),
        "{exclude}"
    );
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "?? main.rs\n");

    // The hook finds the folder, and the baseline snapshot holds the project's file.
    let found = find_yalper_dir([root.to_path_buf()]).unwrap();
    assert_eq!(found.token.as_str(), token);
    let store = Store::open(&found.dir, &found.token).unwrap();
    let cache = store.file_cache().unwrap().unwrap();
    let paths: Vec<&str> = cache.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["main.rs"]);
    drop(store);

    // A hook call is recorded.
    let mut hook = Command::new(EXE)
        .arg("hook")
        .current_dir(root)
        .env("CLAUDE_PROJECT_DIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut hook.stdin.take().unwrap(),
        br#"{"session_id": "s1", "hook_event_name": "Stop"}"#,
    )
    .unwrap();
    let output = hook.wait_with_output().unwrap();
    assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    let store = Store::open(&found.dir, &found.token).unwrap();
    assert_eq!(store.events("s1").unwrap().len(), 1);
}

#[test]
fn running_init_twice_changes_nothing() {
    let repo = repository();
    let root = repo.path();
    assert_success(&init(root));
    let before = files(root);

    let output = init(root);
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains("already set up"), "{text}");
    assert!(text.contains("hooks: already registered"), "{text}");
    assert!(!text.contains("Baseline"), "{text}");
    assert_eq!(files(root), before);
}

#[test]
fn existing_settings_and_hooks_are_kept() {
    let repo = repository();
    let root = repo.path();
    fs::create_dir(root.join(".claude")).unwrap();
    let shared = "{\"hooks\": {}}\n";
    fs::write(root.join(".claude").join("settings.json"), shared).unwrap();
    let existing = json!({
        "permissions": {"allow": ["Bash(cargo test)"], "deny": []},
        "hooks": {
            "PostToolUse": [
                {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "cargo fmt"}]}
            ],
            "Stop": [{"hooks": [{"type": "command", "command": "notify-send done", "timeout": 5}]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]
        },
        "env": {"RUST_LOG": "debug"}
    });
    fs::write(settings_path(root), existing.to_string()).unwrap();

    assert_success(&init(root));
    let settings = settings(root);
    let keys: Vec<&String> = settings.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["permissions", "hooks", "env"]);
    assert_eq!(settings["permissions"], existing["permissions"]);
    assert_eq!(settings["env"], existing["env"]);
    assert_eq!(
        settings["hooks"]["PreToolUse"],
        existing["hooks"]["PreToolUse"]
    );
    for event in ["PostToolUse", "Stop"] {
        let groups = settings["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 2, "{event}");
        assert_eq!(groups[0], existing["hooks"][event][0], "{event}");
        assert!(is_yalper_handler(&groups[1]["hooks"][0]), "{event}");
    }
    assert_eq!(
        fs::read_to_string(root.join(".claude").join("settings.json")).unwrap(),
        shared
    );

    let before = fs::read(settings_path(root)).unwrap();
    assert_success(&init(root));
    assert_eq!(fs::read(settings_path(root)).unwrap(), before);
}

#[test]
fn a_moved_binary_is_updated_in_place() {
    let repo = repository();
    let root = repo.path();
    assert_success(&init(root));
    let old = fs::read_to_string(settings_path(root)).unwrap();
    let moved = old.replace(
        &serde_json::to_string(EXE).unwrap(),
        &serde_json::to_string("/old/place/yalper").unwrap(),
    );
    assert_ne!(moved, old);
    fs::write(settings_path(root), moved).unwrap();

    assert_success(&init(root));
    assert_eq!(fs::read_to_string(settings_path(root)).unwrap(), old);
}

#[test]
fn a_settings_file_yalper_cannot_edit_stops_init_with_nothing_written() {
    for content in ["{ \"hooks\": ", "[]", "{\"hooks\": {\"Stop\": {}}}"] {
        let repo = repository();
        let root = repo.path();
        fs::create_dir(root.join(".claude")).unwrap();
        fs::write(settings_path(root), content).unwrap();
        let git_before = files(&root.join(".git"));

        let output = init(root);
        assert!(!output.status.success(), "{content}: {output:?}");
        assert!(output.stdout.is_empty(), "{content}: {output:?}");
        let message = stderr(&output);
        assert!(message.contains("settings.local.json"), "{message}");
        assert!(message.contains("Nothing was changed"), "{message}");
        assert_eq!(fs::read_to_string(settings_path(root)).unwrap(), content);
        assert!(!root.join(YALPER_DIR).exists(), "{content}");
        assert_eq!(files(&root.join(".git")), git_before, "{content}");
    }
}

#[test]
fn init_from_a_subdirectory_sets_up_the_repository_root() {
    let repo = repository();
    let root = repo.path();
    let deep = root.join("src").join("nested");
    fs::create_dir_all(&deep).unwrap();
    assert_success(&init(&deep));
    assert!(root.join(YALPER_DIR).join(ID_FILE).is_file());
    assert!(settings_path(root).is_file());
    assert!(!deep.join(YALPER_DIR).exists());
    assert!(!deep.join(".claude").exists());
    assert!(!root.join("src").join(".claude").exists());
}

#[test]
fn a_linked_worktree_gets_its_own_token_and_uses_the_common_exclude_file() {
    let main = repository();
    git(main.path(), &["add", "main.rs"]);
    git(main.path(), &["commit", "--quiet", "-m", "first"]);
    let parent = tempfile::tempdir().unwrap();
    let worktree = parent.path().join("wt");
    git(
        main.path(),
        &["worktree", "add", "--quiet", worktree.to_str().unwrap()],
    );

    assert_success(&init(&worktree));
    let worktree_git_dir = main.path().join(".git").join("worktrees").join("wt");
    assert_eq!(
        token_file(&worktree_git_dir.join(GIT_ID_FILE)),
        token_file(&worktree.join(YALPER_DIR).join(ID_FILE))
    );
    assert!(!main.path().join(".git").join(GIT_ID_FILE).exists());
    assert!(!worktree_git_dir.join("info").exists());
    let exclude =
        fs::read_to_string(main.path().join(".git").join("info").join("exclude")).unwrap();
    assert!(
        exclude.contains("\n.yalper/\n.claude/settings.local.json\n"),
        "{exclude}"
    );
    assert!(settings_path(&worktree).is_file());
    assert!(!main.path().join(YALPER_DIR).exists());
    assert!(find_yalper_dir([worktree.clone()]).is_some());

    // The main working tree is set up on its own, with another token.
    assert_success(&init(main.path()));
    assert_ne!(
        token_file(&main.path().join(".git").join(GIT_ID_FILE)),
        token_file(&worktree_git_dir.join(GIT_ID_FILE))
    );
    assert!(find_yalper_dir([worktree]).is_some());
}

#[test]
fn outside_a_git_repository_init_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let output = init(dir.path());
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        stderr(&output).contains("not inside a git repository"),
        "{output:?}"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_yalper_dir_init_did_not_create_is_refused_unless_recreated() {
    let repo = repository();
    let root = repo.path();
    // What a repository could commit: a `.yalper/` with its own id and database.
    let planted = root.join(YALPER_DIR);
    fs::create_dir(&planted).unwrap();
    fs::write(planted.join(ID_FILE), "0123456789abcdef0123456789abcdef\n").unwrap();
    fs::write(planted.join(DATABASE_FILE), "planted").unwrap();
    let git_before = files(&root.join(".git"));

    let output = init(root);
    assert!(!output.status.success(), "{output:?}");
    let message = stderr(&output);
    assert!(message.contains("--recreate"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
    assert_eq!(fs::read(planted.join(DATABASE_FILE)).unwrap(), b"planted");
    assert_eq!(files(&root.join(".git")), git_before);
    assert!(!root.join(".claude").exists());

    let output = yalper(root, &["init", "--recreate"]);
    assert_success(&output);
    assert!(stdout(&output).contains("deleted and created again"));
    assert_ne!(
        token_file(&planted.join(ID_FILE)),
        "0123456789abcdef0123456789abcdef"
    );
    assert!(find_yalper_dir([root.to_path_buf()]).is_some());
}

#[test]
fn contents_from_another_init_are_refused_unless_recreated() {
    let first = repository();
    let second = repository();
    assert_success(&init(first.path()));
    assert_success(&init(second.path()));
    // A pulled commit that tracks `.yalper/yalper.db` overwrites the local copy, which git ignores.
    for name in ["yalper.db", "yalper.db-wal"] {
        fs::copy(
            second.path().join(YALPER_DIR).join(name),
            first.path().join(YALPER_DIR).join(name),
        )
        .unwrap();
    }

    let output = init(first.path());
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("init token"), "{output:?}");
    // The hook refuses the database too: nothing is recorded.
    let found = find_yalper_dir([first.path().to_path_buf()]).unwrap();
    assert!(Store::open(&found.dir, &found.token).is_err());
    drop(found);

    assert_success(&yalper(first.path(), &["init", "--recreate"]));
    let found = find_yalper_dir([first.path().to_path_buf()]).unwrap();
    Store::open(&found.dir, &found.token).unwrap();
}

#[test]
fn recreate_keeps_a_working_yalper_dir() {
    let repo = repository();
    let root = repo.path();
    assert_success(&init(root));
    let token = token_file(&root.join(YALPER_DIR).join(ID_FILE));
    let output = yalper(root, &["init", "--recreate"]);
    assert_success(&output);
    assert!(stdout(&output).contains("already set up"));
    assert_eq!(token_file(&root.join(YALPER_DIR).join(ID_FILE)), token);
}

#[cfg(unix)]
#[test]
fn a_folder_others_can_write_is_refused_like_the_hook_does_without_suggesting_deletion() {
    use std::os::unix::fs::PermissionsExt;
    let repo = repository();
    let root = repo.path();
    assert_success(&init(root));
    fs::set_permissions(root.join(YALPER_DIR), fs::Permissions::from_mode(0o777)).unwrap();

    let output = init(root);
    assert!(!output.status.success(), "{output:?}");
    let message = stderr(&output);
    assert!(message.contains("other users"), "{message}");
    assert!(message.contains("chmod 700 .yalper"), "{message}");
    assert!(!message.contains("--recreate"), "{message}");
    assert!(find_yalper_dir([root.to_path_buf()]).is_none());

    fs::set_permissions(root.join(YALPER_DIR), fs::Permissions::from_mode(0o700)).unwrap();
    assert_success(&init(root));
}

#[test]
fn a_database_from_a_newer_yalper_is_an_error_without_suggesting_deletion() {
    let repo = repository();
    let root = repo.path();
    assert_success(&init(root));
    rusqlite::Connection::open(root.join(YALPER_DIR).join(DATABASE_FILE))
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();

    let output = init(root);
    assert!(!output.status.success(), "{output:?}");
    let message = stderr(&output);
    assert!(message.contains("update Yalper"), "{message}");
    assert!(!message.contains("--recreate"), "{message}");
}

#[test]
fn an_empty_yalper_dir_is_used_like_a_missing_one() {
    let repo = repository();
    let root = repo.path();
    fs::create_dir(root.join(YALPER_DIR)).unwrap();
    let output = init(root);
    assert_success(&output);
    assert!(stdout(&output).contains(".yalper/: created"));
    assert!(find_yalper_dir([root.to_path_buf()]).is_some());
}

#[test]
fn a_lost_snapshot_store_is_created_again_and_recording_works() {
    let repo = repository();
    let root = repo.path();
    assert_success(&init(root));
    fs::remove_dir_all(root.join(YALPER_DIR).join(SNAPSHOTS_DIR)).unwrap();

    let output = init(root);
    assert_success(&output);
    let text = stdout(&output);
    assert!(
        text.contains("Snapshot store: missing, created again"),
        "{text}"
    );
    assert!(text.contains("Baseline snapshot: 1 file"), "{text}");

    fs::write(root.join("main.rs"), "fn main() { println!(); }\n").unwrap();
    let payload = json!({
        "session_id": "s1",
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "tool_input": {"file_path": root.join("main.rs")},
        "tool_response": {},
    });
    let mut hook = Command::new(EXE)
        .arg("hook")
        .current_dir(root)
        .env("CLAUDE_PROJECT_DIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut hook.stdin.take().unwrap(),
        payload.to_string().as_bytes(),
    )
    .unwrap();
    assert!(hook.wait_with_output().unwrap().status.success());

    assert!(!root.join(YALPER_DIR).join("errors.log").exists());
    let found = find_yalper_dir([root.to_path_buf()]).unwrap();
    let events = Store::open(&found.dir, &found.token)
        .unwrap()
        .events("s1")
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].tree_id.is_some());
    assert_eq!(events[0].files_changed, Some(1));
}

#[test]
fn other_commands_in_the_project_settings_are_listed() {
    let repo = repository();
    let root = repo.path();
    fs::create_dir(root.join(".claude")).unwrap();
    let shared = json!({
        "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "./guard.sh"}]}]},
        "statusLine": {"type": "command", "command": "~/bin/status"}
    });
    fs::write(
        root.join(".claude").join("settings.json"),
        shared.to_string(),
    )
    .unwrap();
    let local = json!({"apiKeyHelper": "get-key.sh"});
    fs::write(settings_path(root), local.to_string()).unwrap();

    let output = init(root);
    assert_success(&output);
    let text = stdout(&output);
    assert!(
        text.contains("they also run once this folder is trusted"),
        "{text}"
    );
    assert!(
        text.contains(".claude/settings.json hooks.PreToolUse: ./guard.sh"),
        "{text}"
    );
    assert!(
        text.contains(".claude/settings.json statusLine: ~/bin/status"),
        "{text}"
    );
    assert!(
        text.contains(".claude/settings.local.json apiKeyHelper: get-key.sh"),
        "{text}"
    );
    assert!(!text.contains(EXE), "{text}");

    // Yalper's own registration alone lists nothing.
    let plain = repository();
    let text = stdout(&init(plain.path()));
    assert!(!text.contains("Other commands"), "{text}");
}

#[test]
fn repository_text_cannot_send_control_characters_to_the_terminal() {
    let repo = repository();
    let root = repo.path();
    fs::create_dir(root.join(".claude")).unwrap();
    let local = json!({
        "hooks": {"Stop\u{1b}[2K\u{1b}[1A": [{"hooks": [
            {"type": "command", "command": "run\u{1b}[31m\r\u{7}", "args": ["\u{9b}2J"]}
        ]}]},
        "statusLine\u{1b}": {"type": "command", "command": "x"},
        "apiKeyHelper": "key\u{1b}]0;title\u{7}"
    });
    fs::write(settings_path(root), local.to_string()).unwrap();

    let output = init(root);
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains("hooks.Stop [2K [1A: run [31m"), "{text}");
    assert!(text.contains("apiKeyHelper: key ]0;title "), "{text}");
    for c in text.chars() {
        assert!(
            c == '\n' || !c.is_control(),
            "control character {c:?} in {text:?}"
        );
    }
}
