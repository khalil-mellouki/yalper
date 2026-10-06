//! One Claude Code session recorded from start to finish, through the real `yalper` binary and real git, the
//! way a developer experiences Yalper:
//!
//! 1. `yalper init` in a small Python project that already has personal Claude Code settings.
//! 2. A session: Claude Code runs the hook command `yalper init` registered after every step, with the
//!    payloads it sends, while the files on disk change exactly as the agent's tools change them. The prompt
//!    holds a token, a shell command changes files from a child process, a subagent runs the tests, and a
//!    push fails.
//! 3. `yalper log` and `yalper show` on that session.
//! 4. What `.yalper/` holds: every step's snapshot, no secret anywhere, the user's `.git` untouched.
//! 5. `yalper uninstall`, then `yalper uninstall --purge`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{files, git, yalper};
use serde_json::{Value, json};
use yalper::hook::{ERRORS_LOG, YALPER_DIR, find_yalper_dir};
use yalper::init::is_yalper_handler;
use yalper::snapshot::SNAPSHOTS_DIR;
use yalper::store::{DATABASE_FILE, Store};

const SESSION: &str = "5f3c2a1e-8b4d-4e6f-9a7b-1c2d3e4f5a6b";

/// Where the output shows the temporary project, so it reads the same on every machine.
const SHOWN_ROOT: &str = "/home/dev/greeter";

const GREET_BEFORE: &str = "def greet(name):\n    return \"Hello \" + name\n";
const GREET_AFTER: &str = "def greet(name):\n    return f\"Hello, {name}!\"\n";
const CHANGELOG: &str =
    "# Changelog\n\n## 0.2.0\n\n- `greet()` says \"Hello, Ada!\" instead of \"Hello Ada\".\n";

/// The developer's personal Claude Code settings, from before Yalper.
const USER_SETTINGS: &str = r#"{
  "permissions": {
    "allow": [
      "Bash(python -m pytest:*)"
    ]
  },
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          {
            "type": "command",
            "command": "ruff format"
          }
        ]
      }
    ]
  }
}
"#;

/// A fake GitHub token, built at run time so that no token-shaped literal is in the repository.
fn fake_github_token() -> String {
    let body: String = "q7W2e9R4t1Y6u3I8o5P0".chars().cycle().take(36).collect();
    format!("ghp_{body}")
}

#[test]
fn a_claude_code_session_is_recorded_inspected_and_uninstalled() {
    // A small Python project with one commit, whose test fails, and the developer's own Claude Code settings.
    let temp = tempfile::tempdir().unwrap();
    let root = real_path(temp.path());
    let root = root.as_path();
    write(root, ".gitignore", "__pycache__/\n");
    write(root, "README.md", "# greeter\n\nSays hello.\n");
    write(root, "VERSION", "0.1.0\n");
    write(
        root,
        "pyproject.toml",
        "[project]\nname = \"greeter\"\nversion = \"0.1.0\"\n",
    );
    write(root, "greet.py", GREET_BEFORE);
    write(
        root,
        "test_greet.py",
        "from greet import greet\n\n\ndef test_greet():\n    assert greet(\"Ada\") == \"Hello, Ada!\"\n",
    );
    git(root, &["init", "--quiet"]);
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "--message", "Initial commit"]);
    write(root, ".claude/settings.local.json", USER_SETTINGS);
    let git_dir_before_init = files(&root.join(".git"));

    // 1. One command sets up recording. Init mentions the developer's own hook: it runs too.
    insta::assert_snapshot!(shown(root, &run(root, &["init"])), @r"
    Setting up Yalper in /home/dev/greeter
      .yalper/: created
      Baseline snapshot: 6 files
      Git exclude: added .yalper/, .claude/settings.local.json
      Claude Code hooks: registered in .claude/settings.local.json
      Other commands in Claude Code's settings (they also run once this folder is trusted):
        .claude/settings.local.json hooks.PostToolUse: ruff format
    Done. Claude Code sessions in this project are now recorded. Hooks need Claude Code 2.1.139 or later, and only run once this folder is trusted in Claude Code.
    ");
    let git_dir_after_init = files(&root.join(".git"));

    // 2. A Claude Code session. Each step's file changes happen on disk before its hook runs, as with the
    // real tools.
    let claude = ClaudeCode::new(root);
    let token = fake_github_token();
    claude.hook(
        "SessionStart",
        json!({"source": "startup", "model": "claude-opus-5"}),
    );
    claude.hook(
        "UserPromptSubmit",
        json!({"prompt": format!(
            "test_greet fails. Fix greet(), add a changelog entry, bump the version to 0.2.0 and push.\n\
             Use my token {token} to push."
        )}),
    );

    claude.hook(
        "PostToolUse",
        json!({
            "tool_name": "Read",
            "tool_input": {"file_path": claude.path("greet.py")},
            "tool_response": {"type": "text", "file": {
                "filePath": claude.path("greet.py"),
                "content": GREET_BEFORE,
                "numLines": 2,
                "startLine": 1,
                "totalLines": 2
            }},
            "tool_use_id": "toolu_01",
            "duration_ms": 4
        }),
    );

    write(root, "greet.py", GREET_AFTER);
    claude.hook(
        "PostToolUse",
        json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": claude.path("greet.py"),
                "old_string": "    return \"Hello \" + name",
                "new_string": "    return f\"Hello, {name}!\"",
                "replace_all": false
            },
            "tool_response": {
                "filePath": claude.path("greet.py"),
                "oldString": "    return \"Hello \" + name",
                "newString": "    return f\"Hello, {name}!\"",
                "originalFile": GREET_BEFORE,
                "structuredPatch": [{
                    "oldStart": 1,
                    "oldLines": 2,
                    "newStart": 1,
                    "newLines": 2,
                    "lines": [
                        " def greet(name):",
                        "-    return \"Hello \" + name",
                        "+    return f\"Hello, {name}!\""
                    ]
                }],
                "userModified": false,
                "replaceAll": false
            },
            "tool_use_id": "toolu_02",
            "duration_ms": 11
        }),
    );

    write(root, "CHANGELOG.md", CHANGELOG);
    claude.hook(
        "PostToolUse",
        json!({
            "tool_name": "Write",
            "tool_input": {"file_path": claude.path("CHANGELOG.md"), "content": CHANGELOG},
            "tool_response": {
                "type": "create",
                "filePath": claude.path("CHANGELOG.md"),
                "content": CHANGELOG,
                "structuredPatch": [],
                "originalFile": null
            },
            "tool_use_id": "toolu_03",
            "duration_ms": 7
        }),
    );

    // The shell command changes two files from processes of its own, which Claude Code's file tools never
    // see.
    let sed_output = tempfile::tempdir().unwrap();
    for (name, bumped) in [
        ("VERSION", "0.2.0\n"),
        (
            "pyproject.toml",
            "[project]\nname = \"greeter\"\nversion = \"0.2.0\"\n",
        ),
    ] {
        write(sed_output.path(), name, bumped);
        copy_in_child_process(&sed_output.path().join(name), &root.join(name));
    }
    claude.hook(
        "PostToolUse",
        json!({
            "tool_name": "Bash",
            "tool_input": {
                "command": "sed -i 's/0\\.1\\.0/0.2.0/' VERSION pyproject.toml",
                "description": "Bump the version to 0.2.0"
            },
            "tool_response": {"stdout": "", "stderr": "", "interrupted": false, "isImage": false},
            "tool_use_id": "toolu_04",
            "duration_ms": 35
        }),
    );

    // A subagent runs the tests. Its tool calls carry its id. The bytecode Python writes is gitignored, so
    // the step changes no file.
    write(
        root,
        "__pycache__/greet.cpython-312.pyc",
        "\u{cb}\r\r\n bytecode",
    );
    claude.hook(
        "PostToolUse",
        json!({
            "agent_id": "a7f3e9b2",
            "agent_type": "general-purpose",
            "tool_name": "Bash",
            "tool_input": {"command": "python -m pytest -q", "description": "Run the tests"},
            "tool_response": {"stdout": ".                                                     [100%]\n1 passed in 0.01s\n", "stderr": "", "interrupted": false, "isImage": false},
            "tool_use_id": "toolu_05",
            "duration_ms": 912
        }),
    );
    claude.hook(
        "PostToolUse",
        json!({
            "tool_name": "Agent",
            "tool_input": {
                "description": "Run the tests",
                "prompt": "Run the test suite and report any failure.",
                "subagent_type": "general-purpose"
            },
            "tool_response": {
                "status": "completed",
                "content": [{"type": "text", "text": "All tests pass (1 passed)."}],
                "totalDurationMs": 4210,
                "totalToolUseCount": 1
            },
            "tool_use_id": "toolu_06",
            "duration_ms": 4214
        }),
    );

    // The agent puts the token from the prompt into a command, and the error repeats it.
    let remote = format!("https://x-access-token:{token}@github.com/acme/greeter.git");
    claude.hook(
        "PostToolUseFailure",
        json!({
            "tool_name": "Bash",
            "tool_input": {"command": format!("git push {remote} main"), "description": "Push to GitHub"},
            "error": format!(
                "Exit code 128\nfatal: unable to access '{remote}/': Could not resolve host: github.com"
            ),
            "is_interrupt": false,
            "tool_use_id": "toolu_07",
            "duration_ms": 1530
        }),
    );
    claude.hook(
        "Stop",
        json!({
            "stop_hook_active": false,
            "last_assistant_message": "Fixed greet(), added a changelog entry and bumped the version to 0.2.0. \
                The tests pass. The push failed: github.com could not be reached.",
            "background_tasks": [],
            "session_crons": []
        }),
    );
    claude.hook("SessionEnd", json!({"reason": "prompt_input_exit"}));

    // No hook ran into an error, and the session left the user's git repository as init left it.
    assert!(!root.join(YALPER_DIR).join(ERRORS_LOG).exists());
    assert_eq!(files(&root.join(".git")), git_dir_after_init);

    // 3. Inspect the session from the terminal: every step in order, with the number of files it changed.
    insta::assert_snapshot!(shown(root, &run(root, &["log"])), @r"
    Session 5f3c2a1e, started YYYY-MM-DD hh:mm:ss, ended (prompt_input_exit), 11 steps
      step  time      files  action  summary
         1  hh:mm:ss      0  start   startup
         2  hh:mm:ss      0  prompt  test_greet fails. Fix greet(), add a changelog entry, bum...
         3  hh:mm:ss      0  Read    greet.py
         4  hh:mm:ss      1  Edit    greet.py
         5  hh:mm:ss      1  Write   CHANGELOG.md
         6  hh:mm:ss      2  Bash    sed -i 's/0\.1\.0/0.2.0/' VERSION pyproject.toml
         7  hh:mm:ss      0  Bash    python -m pytest -q
         8  hh:mm:ss      0  Agent   Run the tests
         9  hh:mm:ss      0  Bash    FAILED git push https://x-access-token:[REDACTED:github-p...
        10  hh:mm:ss         reply   Fixed greet(), added a changelog entry and bumped the ver...
        11  hh:mm:ss         end     prompt_input_exit
    ");

    // The prompt, with the token masked.
    insta::assert_snapshot!(shown(root, &run(root, &["show", "2"])), @r"
    Session 5f3c2a1e, step 2 of 11, YYYY-MM-DD hh:mm:ss

    Prompt:
      test_greet fails. Fix greet(), add a changelog entry, bump the version to 0.2.0 and push.
      Use my token [REDACTED:github-pat] to push.

    No files changed.
    ");

    // A file tool's call and the diff of its change.
    insta::assert_snapshot!(shown(root, &run(root, &["show", "4"])), @r#"
    Session 5f3c2a1e, step 4 of 11, YYYY-MM-DD hh:mm:ss
    Edit, succeeded in 11 ms

    Input:
      {
        "file_path": "/home/dev/greeter/greet.py",
        "old_string": "    return \"Hello \" + name",
        "new_string": "    return f\"Hello, {name}!\"",
        "replace_all": false
      }

    Output:
      {
        "filePath": "/home/dev/greeter/greet.py",
        "oldString": "    return \"Hello \" + name",
        "newString": "    return f\"Hello, {name}!\"",
        "userModified": false,
        "replaceAll": false
      }

    1 file changed:
      modified  greet.py

    --- a/greet.py
    +++ b/greet.py
    @@ -1,2 +1,2 @@
     def greet(name):
    -    return "Hello " + name
    +    return f"Hello, {name}!"
    "#);

    // The shell command's changes, made by other processes, are in its step like any tool's.
    insta::assert_snapshot!(shown(root, &run(root, &["show", "6"])), @r#"
    Session 5f3c2a1e, step 6 of 11, YYYY-MM-DD hh:mm:ss
    Bash, succeeded in 35 ms

    Command:
      sed -i 's/0\.1\.0/0.2.0/' VERSION pyproject.toml

    Output: (none)

    2 files changed:
      modified  VERSION
      modified  pyproject.toml

    --- a/VERSION
    +++ b/VERSION
    @@ -1,1 +1,1 @@
    -0.1.0
    +0.2.0

    --- a/pyproject.toml
    +++ b/pyproject.toml
    @@ -1,3 +1,3 @@
     [project]
     name = "greeter"
    -version = "0.1.0"
    +version = "0.2.0"
    "#);

    // The subagent's step. The ignored bytecode is not a change.
    insta::assert_snapshot!(shown(root, &run(root, &["show", "7"])), @r"
    Session 5f3c2a1e, step 7 of 11, YYYY-MM-DD hh:mm:ss
    Bash, succeeded in 912 ms, subagent a7f3e9b2

    Command:
      python -m pytest -q

    Output:
      .                                                     [100%]
      1 passed in 0.01s

    No files changed.
    ");

    // The failed push, with the token masked in the command and in the error.
    insta::assert_snapshot!(shown(root, &run(root, &["show", "9"])), @r"
    Session 5f3c2a1e, step 9 of 11, YYYY-MM-DD hh:mm:ss
    Bash, FAILED after 1.5 s

    Command:
      git push https://x-access-token:[REDACTED:github-pat]@github.com/acme/greeter.git main

    Error:
      Exit code 128
      fatal: unable to access 'https://x-access-token:[REDACTED:github-pat]@github.com/acme/greeter.git/': Could not resolve host: github.com

    No files changed.
    ");

    // 4. The snapshot of the last step that changed a file, and of every step after it, is exactly the tree
    // git itself makes of the working tree: every file that is not ignored, with the shell command's
    // changes, without `.yalper/`, the settings file or the bytecode.
    let working_tree = git_tree_of_working_tree(root);
    // The open `.yalper` folder is closed at the end of the block, or Windows would not let purge delete it.
    let events = {
        let yalper_dir = find_yalper_dir([root.to_path_buf()]).unwrap();
        let store = Store::open_for_reading(&yalper_dir.dir, &yalper_dir.token).unwrap();
        store.events(SESSION).unwrap()
    };
    // Steps 6 to 9.
    for event in &events[5..9] {
        assert_eq!(
            event.tree_id.as_deref(),
            Some(working_tree.as_str()),
            "step {}",
            event.step
        );
    }

    // The token is nowhere in `.yalper/`: not in the database, its WAL, the error log, or any object of the
    // snapshot store (read uncompressed through git). The prompt is stored with the token masked in place.
    let store = root.join(YALPER_DIR).join(SNAPSHOTS_DIR);
    let recorded = files(&root.join(YALPER_DIR));
    let event_log: Vec<u8> = recorded
        .iter()
        .filter(|(path, _)| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(DATABASE_FILE)
        })
        .flat_map(|(_, bytes)| bytes.clone())
        .collect();
    let mut recorded: Vec<u8> = recorded.into_values().flatten().collect();
    recorded.extend(git_in_store(&store, &["cat-file", "--batch-all-objects", "--batch"]).stdout);
    let contains = |bytes: &[u8], text: &str| {
        bytes
            .windows(text.len())
            .any(|window| window == text.as_bytes())
    };
    assert!(contains(
        &event_log,
        "Use my token [REDACTED:github-pat] to push."
    ));
    assert!(
        contains(&recorded, GREET_AFTER),
        "the search sees the recorded code"
    );
    assert!(
        !contains(&recorded, &token["ghp_".len()..]),
        "the token reached .yalper/"
    );

    // 5. Uninstall removes the hooks and gives the developer's settings back byte for byte. The
    // recordings stay readable.
    let project_files = without_setup(files(root));
    let only_user_settings = BTreeMap::from([(
        root.join(".claude").join("settings.local.json"),
        USER_SETTINGS.as_bytes().to_vec(),
    )]);
    insta::assert_snapshot!(shown(root, &run(root, &["uninstall"])), @r"
    Removing Yalper from /home/dev/greeter
      Claude Code hooks: removed from .claude/settings.local.json
      .yalper/: kept, with your recordings (`yalper uninstall --purge` deletes them)
    Done.
    ");
    assert_eq!(files(&root.join(".claude")), only_user_settings);
    assert!(run(root, &["log"]).contains("ended (prompt_input_exit), 11 steps"));

    // `--purge` also deletes the recordings. The project is as it was before init, apart from the git
    // exclude line that keeps the personal settings file out of commits.
    insta::assert_snapshot!(shown(root, &run(root, &["uninstall", "--purge"])), @r"
    Removing Yalper from /home/dev/greeter
      .yalper/: deleted
      Init token: removed from the git directory
      Git exclude: removed .yalper/
    Done.
    ");
    assert!(!root.join(YALPER_DIR).exists());
    assert_eq!(files(&root.join(".claude")), only_user_settings);
    let mut expected_git_dir = git_dir_before_init;
    expected_git_dir
        .get_mut(&root.join(".git").join("info").join("exclude"))
        .unwrap()
        .extend_from_slice(b".claude/settings.local.json\n");
    assert_eq!(files(&root.join(".git")), expected_git_dir);
    assert_eq!(without_setup(files(root)), project_files);
}

/// Plays the part of Claude Code: runs the hook command `yalper init` registered for an event, with the
/// payload Claude Code sends on stdin.
struct ClaudeCode {
    root: PathBuf,
    /// The command and arguments of Yalper's handler for each event, read from the settings file.
    handlers: BTreeMap<String, (String, Vec<String>)>,
}

impl ClaudeCode {
    fn new(root: &Path) -> Self {
        let settings: Value =
            serde_json::from_slice(&fs::read(root.join(".claude/settings.local.json")).unwrap())
                .unwrap();
        let mut handlers = BTreeMap::new();
        for (event, groups) in settings["hooks"].as_object().unwrap() {
            let handler = groups
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .find(|handler| is_yalper_handler(handler));
            if let Some(handler) = handler {
                let args = handler["args"].as_array().unwrap();
                let args = args.iter().map(|arg| arg.as_str().unwrap().to_owned());
                let command = handler["command"].as_str().unwrap().to_owned();
                handlers.insert(event.clone(), (command, args.collect()));
            }
        }
        Self {
            root: root.to_owned(),
            handlers,
        }
    }

    /// The absolute path of a project file, as tool inputs give it.
    fn path(&self, relative: &str) -> String {
        self.root.join(relative).to_str().unwrap().to_owned()
    }

    /// Runs the hook for `event` with the fields every payload has plus `fields`, and checks that Yalper
    /// stays invisible to the agent: exit code 0 and no output at all.
    fn hook(&self, event: &str, fields: Value) {
        let mut payload = json!({
            "session_id": SESSION,
            "transcript_path": format!("/home/dev/.claude/projects/-home-dev-greeter/{SESSION}.jsonl"),
            "cwd": self.root,
            "permission_mode": "default",
            "hook_event_name": event,
        });
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        let (command, args) = &self.handlers[event];
        let mut child = Command::new(command)
            .args(args)
            .current_dir(&self.root)
            .env("CLAUDE_PROJECT_DIR", &self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
        assert!(output.stdout.is_empty(), "{event}: {output:?}");
        assert!(output.stderr.is_empty(), "{event}: {output:?}");
    }
}

/// `path` as Claude Code and Yalper see it: the current directory a process gets, which on macOS resolves
/// the temporary folder's `/var` link. Not canonicalized on Windows, where that adds `\\?\`.
fn real_path(path: &Path) -> PathBuf {
    #[cfg(unix)]
    let path = fs::canonicalize(path).unwrap();
    #[cfg(not(unix))]
    let path = path.to_owned();
    path
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Copies `from` over `to` in a separate process, the way a shell command run by the agent changes a file.
fn copy_in_child_process(from: &Path, to: &Path) {
    let status = if cfg!(windows) {
        Command::new("cmd")
            .arg("/C")
            .arg("copy")
            .arg("/Y")
            .arg(from)
            .arg(to)
            .stdout(Stdio::null())
            .status()
    } else {
        Command::new("cp").arg(from).arg(to).status()
    }
    .unwrap();
    assert!(status.success());
}

/// Runs `yalper` with `args` in `root`, checks that it succeeds with nothing on stderr, and returns its
/// output.
fn run(root: &Path, args: &[&str]) -> String {
    let output = yalper(root, args);
    assert!(output.status.success(), "{args:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// `text` with the temporary project shown as [`SHOWN_ROOT`] and the clock hidden: dates and times depend on
/// when and where the test runs.
fn shown(root: &Path, text: &str) -> String {
    let root = root.to_str().unwrap();
    let in_json = |text: &str| {
        let quoted = serde_json::to_string(text).unwrap();
        quoted[1..quoted.len() - 1].to_owned()
    };
    let inside = format!("{root}{MAIN_SEPARATOR}");
    let text = text
        .replace(&in_json(&inside), &format!("{SHOWN_ROOT}/"))
        .replace(&inside, &format!("{SHOWN_ROOT}/"))
        .replace(root, SHOWN_ROOT);
    text.lines()
        .map(without_clock)
        // `yalper log` adds a line with the date when the day changes, which depends on when the test runs.
        .filter(|line| line.trim() != "(YYYY-MM-DD)")
        .map(|line| line + "\n")
        .collect()
}

/// `line` with every date shown as `YYYY-MM-DD` and every time as `hh:mm:ss`.
fn without_clock(line: &str) -> String {
    const PATTERNS: [(&[u8], &[u8]); 2] =
        [(b"0000-00-00", b"YYYY-MM-DD"), (b"00:00:00", b"hh:mm:ss")];
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    'next: while at < bytes.len() {
        for (pattern, mask) in PATTERNS {
            let matches = bytes.len() - at >= pattern.len()
                && pattern.iter().zip(&bytes[at..]).all(|(p, b)| {
                    if *p == b'0' {
                        b.is_ascii_digit()
                    } else {
                        p == b
                    }
                });
            if matches {
                out.extend_from_slice(mask);
                at += pattern.len();
                continue 'next;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8(out).unwrap()
}

/// The project's own files: everything but the git directory, Claude Code's settings and Yalper's folder.
fn without_setup(files: BTreeMap<PathBuf, Vec<u8>>) -> BTreeMap<PathBuf, Vec<u8>> {
    files
        .into_iter()
        .filter(|(path, _)| {
            ![".git", ".claude", YALPER_DIR]
                .iter()
                .any(|dir| path.components().any(|part| part.as_os_str() == *dir))
        })
        .collect()
}

/// The id of the tree git makes of the working tree at `root` (as `git add --all` then `git write-tree`
/// would), computed with a temporary index and object folder so the user's `.git` is not touched.
fn git_tree_of_working_tree(root: &Path) -> String {
    let scratch = tempfile::tempdir().unwrap();
    fs::create_dir(scratch.path().join("objects")).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(["-c", "core.autocrlf=false"])
            .args(args)
            .current_dir(root)
            .env("GIT_INDEX_FILE", scratch.path().join("index"))
            .env("GIT_OBJECT_DIRECTORY", scratch.path().join("objects"))
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        output
    };
    git(&["add", "--all"]);
    let output = git(&["write-tree"]);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Runs git on Yalper's snapshot store, which is a bare git repository.
fn git_in_store(store: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(store)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    output
}
