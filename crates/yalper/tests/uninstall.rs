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

/// `root` as `yalper` prints it: the current directory the process sees, which on macOS resolves the
/// temporary folder's `/var` link to `/private/var`. Not canonicalized on Windows, where that adds `\\?\`.
fn shown(root: &Path) -> String {
    #[cfg(unix)]
    let root = fs::canonicalize(root).unwrap();
    root.display().to_string()
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
            format!("Removing Yalper from {}", shown(root)).as_str(),
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
            shown(root)
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
    // An empty folder is no sign of Yalper either (init uses it like a missing one).
    fs::create_dir(root.join(YALPER_DIR)).unwrap();
    let before = files(root);

    for args in [&["uninstall"][..], &["uninstall", "--purge"]] {
        let text = run(root, args);
        assert_eq!(
            text,
            format!(
                "Yalper is not set up in {}, nothing to remove.\n",
                shown(root)
            ),
            "{args:?}"
        );
        assert_eq!(files(root), before, "{args:?}");
        assert!(root.join(YALPER_DIR).is_dir(), "{args:?}");
    }
}

#[test]
fn a_yalper_dir_deleted_by_hand_takes_its_token_and_exclude_line_with_it() {
    let repo = repository();
    let root = repo.path();
    let before = files(root);
    run(root, &["init"]);
    fs::remove_dir_all(root.join(YALPER_DIR)).unwrap();

    let text = run(root, &["uninstall"]);
    assert!(text.contains("  Init token: removed"), "{text}");
    assert!(
        text.contains("  Git exclude: removed .yalper/, .claude/settings.local.json\n"),
        "{text}"
    );
    assert_eq!(files(root), before);
}

#[test]
fn an_interrupted_purge_is_finished_by_running_it_again() {
    // What a purge stopped by a file in use leaves: `id` alone (deleted last), or an empty folder.
    for keep_id in [true, false] {
        let repo = repository();
        let root = repo.path();
        let before = files(root);
        run(root, &["init"]);
        run(root, &["uninstall"]);
        for entry in fs::read_dir(root.join(YALPER_DIR)).unwrap() {
            let path = entry.unwrap().path();
            if !(keep_id && path.ends_with(ID_FILE)) {
                if path.is_dir() {
                    fs::remove_dir_all(&path).unwrap();
                } else {
                    fs::remove_file(&path).unwrap();
                }
            }
        }

        let text = run(root, &["uninstall", "--purge"]);
        assert!(text.contains("  .yalper/: deleted"), "{keep_id}: {text}");
        assert!(text.contains("  Init token: removed"), "{keep_id}: {text}");
        assert!(!root.join(YALPER_DIR).exists(), "{keep_id}");
        assert_eq!(files(root), before, "{keep_id}");
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

#[test]
fn a_linked_claude_folder_is_skipped() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("settings.local.json");
    let hooks = r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/x/yalper", "args": ["hook"]}]}]}}"#;
    fs::write(&target, hooks).unwrap();

    // Never set up: nothing to do.
    let repo = repository();
    let root = repo.path();
    link_dir(outside.path(), &root.join(".claude"));
    let text = run(root, &["uninstall"]);
    assert!(text.contains("not set up"), "{text}");

    // Set up, then `.claude` replaced by a link: the rest is still removed.
    let repo = repository();
    let root = repo.path();
    run(root, &["init"]);
    fs::remove_dir_all(root.join(".claude")).unwrap();
    link_dir(outside.path(), &root.join(".claude"));
    let text = run(root, &["uninstall", "--purge"]);
    assert!(
        text.contains("  Claude Code hooks: .claude/settings.local.json skipped"),
        "{text}"
    );
    assert!(text.contains("  .yalper/: deleted"), "{text}");
    assert!(!root.join(YALPER_DIR).exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), hooks);
}

#[cfg(unix)]
#[test]
fn a_linked_settings_file_is_skipped_and_a_linked_exclude_file_refused() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("target");
    fs::write(&target, "{}").unwrap();

    let repo = repository();
    let root = repo.path();
    run(root, &["init"]);
    fs::remove_file(settings_path(root)).unwrap();
    std::os::unix::fs::symlink(&target, settings_path(root)).unwrap();
    let text = run(root, &["uninstall"]);
    assert!(text.contains("settings.local.json skipped"), "{text}");
    assert!(
        fs::symlink_metadata(settings_path(root))
            .unwrap()
            .is_symlink()
    );
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

/// A repository with one commit and a linked worktree at `<parent>/wt`: (main, parent, worktree).
fn with_worktree() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let main = repository();
    git(main.path(), &["add", "main.rs"]);
    git(main.path(), &["commit", "--quiet", "-m", "first"]);
    let parent = tempfile::tempdir().unwrap();
    let worktree = parent.path().join("wt");
    git(
        main.path(),
        &["worktree", "add", "--quiet", worktree.to_str().unwrap()],
    );
    (main, parent, worktree)
}

#[test]
fn a_worktree_alone_set_up_removes_its_yalper_exclude_line() {
    let (main, _parent, worktree) = with_worktree();
    let exclude_before = exclude(&main.path().join(".git"));
    run(&worktree, &["init"]);

    let text = run(&worktree, &["uninstall", "--purge"]);
    assert!(text.contains("  Git exclude: removed .yalper/\n"), "{text}");
    assert!(
        text.contains("  Git exclude: kept .claude/settings.local.json (other worktrees"),
        "{text}"
    );
    assert_eq!(
        exclude(&main.path().join(".git")),
        exclude_before + ".claude/settings.local.json\n"
    );
}

#[test]
fn a_linked_worktree_is_removed_alone_and_keeps_the_shared_exclude_lines() {
    let (main, _parent, worktree) = with_worktree();
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

#[test]
fn a_git_file_naming_another_repository_is_refused() {
    let (main, _parent, worktree) = with_worktree();
    run(main.path(), &["init"]);
    run(&worktree, &["init"]);
    let main_git = main.path().join(".git");
    let before = files(&main_git);

    // A folder whose `.git` file names another repository's git directory, or another worktree's.
    for target in [main_git.clone(), main_git.join("worktrees").join("wt")] {
        let folder = tempfile::tempdir().unwrap();
        fs::write(
            folder.path().join(".git"),
            format!("gitdir: {}\n", target.display()),
        )
        .unwrap();
        for args in [&["uninstall", "--purge"][..], &["init"]] {
            let message = fails(folder.path(), args);
            assert!(
                message.contains("does not name this folder back"),
                "{message}"
            );
            assert!(message.contains("Nothing was changed"), "{message}");
        }
        assert_eq!(files(&main_git), before, "{}", target.display());
        assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
    }
}

#[test]
fn a_submodule_is_set_up_and_removed_in_its_own_git_dir() {
    let library = repository();
    git(library.path(), &["add", "main.rs"]);
    git(library.path(), &["commit", "--quiet", "-m", "first"]);
    let app = repository();
    let source = library.path().to_str().unwrap().replace('\\', "/");
    git(
        app.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            &source,
            "lib",
        ],
    );
    let submodule = app.path().join("lib");
    let git_dir = app.path().join(".git").join("modules").join("lib");

    run(&submodule, &["init"]);
    assert!(git_dir.join(GIT_ID_FILE).exists());
    assert!(exclude(&git_dir).contains("\n.yalper/\n"));
    let text = run(&submodule, &["uninstall", "--purge"]);
    assert!(text.contains("  .yalper/: deleted"), "{text}");
    assert!(!git_dir.join(GIT_ID_FILE).exists());
    assert!(!exclude(&git_dir).contains(".yalper/"));
    assert!(!app.path().join(YALPER_DIR).exists());
}
