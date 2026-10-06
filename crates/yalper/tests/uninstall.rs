//! Runs the real `yalper uninstall` binary in temporary git repositories set up with `yalper init`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{files, git, repository, yalper};
use serde_json::{Value, json};
use yalper::hook::{YALPER_DIR, find_yalper_dir};
use yalper::repo::{GIT_ID_FILE, ID_FILE};

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// Runs `yalper` with `args`, checks that it succeeds without errors or warnings, and returns its output.
fn run(dir: &Path, args: &[&str]) -> String {
    let output = yalper(dir, args);
    assert!(output.status.success(), "{args:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    let text = stdout(&output);
    assert!(!text.contains("warning"), "{args:?}: {text}");
    text
}

/// Runs `yalper` with `args` and checks that it fails with nothing on stdout. Returns the error message.
fn fails(dir: &Path, args: &[&str]) -> String {
    let output = yalper(dir, args);
    assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
    assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
    stderr(&output)
}

fn settings_path(root: &Path) -> PathBuf {
    root.join(".claude").join("settings.local.json")
}

fn exclude(git_dir: &Path) -> String {
    fs::read_to_string(git_dir.join("info").join("exclude")).unwrap()
}

/// Links `link` to the folder `target`: a symlink on Unix, a junction on Windows (which needs no privilege).
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[test]
fn init_then_uninstall_restores_the_project_and_purge_leaves_no_trace() {
    let repo = repository();
    let root = repo.path();
    let before = files(root);

    run(root, &["init"]);
    let text = run(root, &["uninstall"]);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            format!("Removing Yalper from {}", root.display()).as_str(),
            "  Claude Code hooks: removed (.claude/settings.local.json held nothing else and was deleted)",
            "  .yalper/: kept, with your recordings (`yalper uninstall --purge` deletes them)",
            "  Git exclude: removed .claude/settings.local.json",
            "Done.",
        ]
    );
    assert!(!root.join(".claude").exists());
    assert!(!root.join(".claude").join("settings.json").exists());
    // The recordings stay usable, and git still ignores them.
    assert!(find_yalper_dir([root.to_path_buf()]).is_some());
    let lines = exclude(&root.join(".git"));
    assert!(lines.ends_with("\n.yalper/\n"), "{lines}");

    // Running it again changes nothing.
    let kept = files(root);
    let text = run(root, &["uninstall"]);
    assert!(
        text.starts_with("Yalper's hooks are not registered in"),
        "{text}"
    );
    assert!(text.contains("--purge"), "{text}");
    assert_eq!(text.lines().count(), 1, "{text}");
    assert_eq!(files(root), kept);

    // A later init picks the recordings up again.
    let text = run(root, &["init"]);
    assert!(text.contains(".yalper/: already set up, kept"), "{text}");
    assert!(!text.contains("Baseline"), "{text}");

    let text = run(root, &["uninstall", "--purge"]);
    assert!(text.contains("  .yalper/: deleted\n"), "{text}");
    assert!(text.contains("  Init token: removed"), "{text}");
    assert!(
        text.contains("  Git exclude: removed .yalper/, .claude/settings.local.json\n"),
        "{text}"
    );
    assert_eq!(files(root), before);
    assert!(!root.join(YALPER_DIR).exists());
    assert!(!root.join(".claude").exists());

    let text = run(root, &["uninstall", "--purge"]);
    assert_eq!(
        text,
        format!(
            "Yalper is not set up in {}, nothing to remove.\n",
            root.display()
        )
    );
    assert_eq!(files(root), before);
}

#[test]
fn existing_settings_and_hooks_come_back_unchanged() {
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
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}],
            // Not Yalper's registration (it has a matcher), so `yalper init` adds its own and this one stays.
            "SessionStart": [{"matcher": "resume", "hooks": [{"type": "command", "command": common::EXE, "args": ["hook"]}]}]
        },
        "env": {"RUST_LOG": "debug"}
    });
    fs::write(settings_path(root), existing.to_string()).unwrap();

    run(root, &["init"]);
    let text = run(root, &["uninstall"]);
    assert!(
        text.contains("  Claude Code hooks: removed from .claude/settings.local.json\n"),
        "{text}"
    );
    // The settings file still needs its exclude line.
    assert!(!text.contains("Git exclude"), "{text}");
    assert!(exclude(&root.join(".git")).contains("\n.claude/settings.local.json\n"));

    let settings: Value = serde_json::from_slice(&fs::read(settings_path(root)).unwrap()).unwrap();
    assert_eq!(settings, existing);
    let keys: Vec<&String> = settings.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["permissions", "hooks", "env"]);
    assert_eq!(
        fs::read_to_string(root.join(".claude").join("settings.json")).unwrap(),
        shared
    );

    let before = files(root);
    run(root, &["uninstall"]);
    assert_eq!(files(root), before);
}

#[test]
fn a_project_that_was_never_set_up_is_left_alone() {
    let repo = repository();
    let root = repo.path();
    // The user's own lines, identical to the ones `yalper init` adds.
    let git_dir = root.join(".git");
    let mut lines = exclude(&git_dir);
    lines.push_str(".yalper/\n.claude/settings.local.json\n");
    fs::write(git_dir.join("info").join("exclude"), lines).unwrap();
    let before = files(root);

    for args in [&["uninstall"][..], &["uninstall", "--purge"]] {
        let text = run(root, args);
        assert_eq!(
            text,
            format!(
                "Yalper is not set up in {}, nothing to remove.\n",
                root.display()
            ),
            "{args:?}"
        );
        assert_eq!(files(root), before, "{args:?}");
    }
}

#[test]
fn outside_a_git_repository_uninstall_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let message = fails(dir.path(), &["uninstall"]);
    assert!(message.contains("not inside a git repository"), "{message}");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_settings_file_yalper_cannot_read_stops_uninstall_with_nothing_changed() {
    let repo = repository();
    let root = repo.path();
    run(root, &["init"]);
    fs::write(settings_path(root), "{ \"hooks\": ").unwrap();
    let before = files(root);

    let message = fails(root, &["uninstall", "--purge"]);
    assert!(message.contains("settings.local.json"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
    assert_eq!(files(root), before);
}

#[test]
fn purge_refuses_a_yalper_dir_init_did_not_create() {
    let repo = repository();
    let root = repo.path();
    // What a repository could commit: a `.yalper/` with its own id.
    let planted = root.join(YALPER_DIR);
    fs::create_dir(&planted).unwrap();
    fs::write(planted.join(ID_FILE), "0123456789abcdef0123456789abcdef\n").unwrap();
    fs::write(planted.join("notes.txt"), "tracked by the repository").unwrap();
    let before = files(root);

    let message = fails(root, &["uninstall", "--purge"]);
    assert!(message.contains("will not delete"), "{message}");
    assert!(message.contains("init token"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
    assert_eq!(files(root), before);

    let text = run(root, &["uninstall"]);
    assert!(text.contains("not set up"), "{text}");
    assert_eq!(files(root), before);
}

#[test]
fn purge_never_follows_a_link() {
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.txt");
    fs::write(&victim, "keep me").unwrap();

    // A `.yalper` that is a link is refused, and nothing is changed.
    let repo = repository();
    let root = repo.path();
    run(root, &["init"]);
    fs::rename(root.join(YALPER_DIR), root.join("recordings")).unwrap();
    link_dir(outside.path(), &root.join(YALPER_DIR));
    let settings = fs::read(settings_path(root)).unwrap();
    let message = fails(root, &["uninstall", "--purge"]);
    assert!(message.contains("link"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
    assert_eq!(fs::read(settings_path(root)).unwrap(), settings);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");

    // A link inside `.yalper/` is deleted, never what it points to.
    let repo = repository();
    let root = repo.path();
    run(root, &["init"]);
    link_dir(outside.path(), &root.join(YALPER_DIR).join("planted"));
    let text = run(root, &["uninstall", "--purge"]);
    assert!(text.contains(".yalper/: deleted"), "{text}");
    assert!(!root.join(YALPER_DIR).exists());
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
}

#[cfg(unix)]
#[test]
fn a_linked_settings_file_or_exclude_file_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("target");
    fs::write(&target, "{}").unwrap();

    let repo = repository();
    let root = repo.path();
    run(root, &["init"]);
    fs::remove_file(settings_path(root)).unwrap();
    std::os::unix::fs::symlink(&target, settings_path(root)).unwrap();
    let message = fails(root, &["uninstall"]);
    assert!(message.contains("not a regular file"), "{message}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "{}");

    let repo = repository();
    let root = repo.path();
    run(root, &["init"]);
    let exclude_path = root.join(".git").join("info").join("exclude");
    fs::remove_file(&exclude_path).unwrap();
    std::os::unix::fs::symlink(&target, &exclude_path).unwrap();
    let before = fs::read(settings_path(root)).unwrap();
    let message = fails(root, &["uninstall", "--purge"]);
    assert!(message.contains("git exclude file"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
    assert_eq!(fs::read(settings_path(root)).unwrap(), before);
    assert!(root.join(YALPER_DIR).exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
}

#[test]
fn a_linked_worktree_is_removed_alone_and_keeps_the_shared_exclude_lines() {
    let main = repository();
    git(main.path(), &["add", "main.rs"]);
    git(main.path(), &["commit", "--quiet", "-m", "first"]);
    let parent = tempfile::tempdir().unwrap();
    let worktree = parent.path().join("wt");
    git(
        main.path(),
        &["worktree", "add", "--quiet", worktree.to_str().unwrap()],
    );
    let worktree_git_dir = main.path().join(".git").join("worktrees").join("wt");
    run(main.path(), &["init"]);
    run(&worktree, &["init"]);
    let exclude_before = exclude(&main.path().join(".git"));

    let text = run(&worktree, &["uninstall", "--purge"]);
    assert!(text.contains(".yalper/: deleted"), "{text}");
    assert!(
        text.contains(
            "  Git exclude: kept .yalper/, .claude/settings.local.json (other worktrees of this repository \
             share the exclude file"
        ),
        "{text}"
    );
    assert!(!worktree.join(YALPER_DIR).exists());
    assert!(!worktree.join(".claude").exists());
    assert!(!worktree_git_dir.join(GIT_ID_FILE).exists());
    assert_eq!(exclude(&main.path().join(".git")), exclude_before);

    // The main working tree is still set up and recording.
    assert!(main.path().join(".git").join(GIT_ID_FILE).exists());
    assert!(settings_path(main.path()).exists());
    assert!(find_yalper_dir([main.path().to_path_buf()]).is_some());

    let before = files(&worktree);
    let text = run(&worktree, &["uninstall", "--purge"]);
    assert!(text.contains("nothing to remove"), "{text}");
    assert_eq!(files(&worktree), before);
}
