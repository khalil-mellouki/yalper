//! `yalper show`: its output for each kind of step of a session recorded from the hook fixtures, file changes
//! made by a child process, binary files, cut diffs, missing snapshots, untrusted rows and file contents,
//! and the errors.

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use jiff::Timestamp;
use jiff::tz::{self, TimeZone};
use serde_json::{Value, json};
use yalper::hook::{HookInput, YALPER_DIR, find_yalper_dir};
use yalper::record::record;
use yalper::repo::YalperDir;
use yalper::show::{Options, show};
use yalper::snapshot::{self, SNAPSHOTS_DIR, ShadowStore};
use yalper::store::{DATABASE_FILE, LOCK_TIMEOUT, Store, WriterLock};

mod common;

const SESSION: &str = "00893aaf-19fa-41d2-8238-13269b9b3ca0";

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(name);
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// A hook payload of `event` in [`SESSION`], with `fields` added.
fn payload(event: &str, fields: Value) -> Value {
    let mut payload =
        json!({"session_id": SESSION, "hook_event_name": event, "cwd": "/home/dev/project"});
    for (key, value) in fields.as_object().unwrap() {
        payload[key] = value.clone();
    }
    payload
}

fn record_payload(dir: &YalperDir, payload: Value) {
    record(dir, HookInput::from_value(payload).unwrap()).unwrap();
}

fn database(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join(YALPER_DIR).join(DATABASE_FILE)).unwrap()
}

/// Gives the steps of [`SESSION`] fixed times, 20 seconds apart from `first` on.
fn set_times(root: &Path, first: &str) {
    let first = first.parse::<Timestamp>().unwrap().as_millisecond();
    database(root)
        .execute(
            "UPDATE events SET ts_ms = ?1 + (step - 1) * 20000 WHERE session_id = ?2",
            rusqlite::params![first, SESSION],
        )
        .unwrap();
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

/// A text file of `lines` numbered lines.
fn numbered(lines: usize, prefix: &str) -> String {
    (1..=lines).map(|n| format!("{prefix} {n}\n")).collect()
}

const LIB_BEFORE: &str = "pub fn old_name() {}\npub fn helper() -> u32 {\n\t1\n}\n";
const LIB_AFTER: &str = "pub fn new_name() {}\npub fn helper() -> u32 {\n\t1\n}\n";
const FACTORIAL: &str = "pub fn factorial(n: u64) -> u64 {\n    (1..=n).product()\n}\n";

/// A project with one recorded session of every kind of step, with file changes between the steps as the
/// tools would make them:
///
/// 1 start, 2 prompt, 3 Edit of `src/lib.rs`, 4 Bash that changes `README.md` from a child process and adds
/// `src/gen.rs`, 5 failed Bash, 6 Read, 7 Write of a new file, 8 Bash that changes a binary file, deletes a
/// file and rewrites a long text file, 9 reply, 10 end.
fn recorded_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("README.md"), "# Demo\n").unwrap();
    fs::write(root.join("src/lib.rs"), LIB_BEFORE).unwrap();
    fs::write(root.join("logo.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
    fs::write(root.join("long.txt"), numbered(400, "old line")).unwrap();
    fs::write(root.join("obsolete.txt"), "gone soon\n").unwrap();
    {
        // What `yalper init` sets up, including its baseline snapshot.
        let dir = common::init(root);
        let store = Store::open(&dir.dir, &dir.token).unwrap();
        let lock = WriterLock::acquire(&dir.dir, LOCK_TIMEOUT).unwrap();
        snapshot::snapshot(&dir.dir, &store, &lock).unwrap();
    }
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    // Tool paths are absolute, under the project: `show_output` shows the project as /home/dev/project.
    let (lib, factorial) = (
        format!("{}/src/lib.rs", root.display()),
        format!("{}/src/factorial.rs", root.display()),
    );

    record_payload(&dir, fixture("session_start.json"));
    let mut prompt = fixture("user_prompt_submit.json");
    prompt["prompt"] = "Rename old_name to new_name\nand update the README".into();
    record_payload(&dir, prompt);

    fs::write(root.join("src/lib.rs"), LIB_AFTER).unwrap();
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({
                "tool_name": "Edit",
                "tool_input": {
                    "file_path": lib,
                    "old_string": "pub fn old_name() {}",
                    "new_string": "pub fn new_name() {}",
                    "replace_all": false
                },
                "tool_response": {
                    "filePath": lib,
                    "oldString": "pub fn old_name() {}",
                    "newString": "pub fn new_name() {}",
                    "originalFile": LIB_BEFORE,
                    "structuredPatch": [{
                        "oldStart": 1,
                        "oldLines": 4,
                        "newStart": 1,
                        "newLines": 4,
                        "lines": ["-pub fn old_name() {}", "+pub fn new_name() {}", " pub fn helper() -> u32 {", " \t1"]
                    }],
                    "userModified": false,
                    "replaceAll": false
                },
                "duration_ms": 12
            }),
        ),
    );

    let elsewhere = tempfile::tempdir().unwrap();
    let new_readme = elsewhere.path().join("README.md");
    fs::write(&new_readme, "# Demo\n\nRenamed `old_name` to `new_name`.\n").unwrap();
    copy_in_child_process(&new_readme, &root.join("README.md"));
    fs::write(root.join("src/gen.rs"), "// generated\n").unwrap();
    let mut bash = fixture("post_tool_use_bash_subagent.json");
    bash["tool_input"]["command"] = "./scripts/update.sh && echo done".into();
    bash["tool_response"]["stdout"] = "updated README.md\ngenerated src/gen.rs\ndone\n".into();
    record_payload(&dir, bash);

    record_payload(&dir, fixture("post_tool_use_failure.json"));
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({
                "tool_name": "Read",
                "tool_input": {"file_path": lib},
                "tool_response": "pub fn new_name() {}\n",
                "duration_ms": 3
            }),
        ),
    );

    fs::write(root.join("src/factorial.rs"), FACTORIAL).unwrap();
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({
                "tool_name": "Write",
                "tool_input": {"file_path": factorial, "content": FACTORIAL},
                "tool_response": {
                    "type": "create",
                    "filePath": factorial,
                    "content": FACTORIAL,
                    "structuredPatch": [],
                    "originalFile": null
                },
                "duration_ms": 9
            }),
        ),
    );

    fs::write(
        root.join("logo.png"),
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR changed",
    )
    .unwrap();
    fs::remove_file(root.join("obsolete.txt")).unwrap();
    fs::write(root.join("long.txt"), numbered(400, "new line")).unwrap();
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "./scripts/regenerate.sh"},
                "tool_response": {"stdout": "", "stderr": "warning: slow\n", "interrupted": false},
                "duration_ms": 2345
            }),
        ),
    );
    record_payload(&dir, fixture("stop.json"));
    record_payload(&dir, fixture("session_end.json"));
    set_times(root, "2026-10-06T21:59:00Z");
    project
}

/// The output of `yalper show` in `root`, with times shown two hours ahead of UTC.
fn show_output(root: &Path, step: u32, options: &Options) -> Result<String, String> {
    let mut out = Vec::new();
    show(
        root,
        step,
        options,
        &TimeZone::fixed(tz::offset(2)),
        &mut out,
    )?;
    // The temporary project root, as written in tool paths and as escaped in JSON, shown as a fixed path.
    let root = root.to_str().unwrap();
    let escaped = serde_json::to_string(root).unwrap();
    let output = String::from_utf8(out).unwrap();
    Ok(output
        .replace(&escaped[1..escaped.len() - 1], "/home/dev/project")
        .replace(root, "/home/dev/project"))
}

fn short(root: &Path, step: u32) -> String {
    show_output(root, step, &Options::default()).unwrap()
}

#[test]
fn a_session_start_and_a_prompt() {
    let project = recorded_project();
    insta::assert_snapshot!(short(project.path(), 1), @r"
    Session 00893aaf, step 1 of 10, 2026-10-06 23:59:00
    Session started (startup), model claude-opus-5

    No files changed.
    ");
    insta::assert_snapshot!(short(project.path(), 2), @r"
    Session 00893aaf, step 2 of 10, 2026-10-06 23:59:20

    Prompt:
      Rename old_name to new_name
      and update the README

    No files changed.
    ");
}

#[test]
fn an_edit_shows_its_input_output_and_diff() {
    let project = recorded_project();
    // The response's original file and patch are left out: the diff shows them.
    insta::assert_snapshot!(short(project.path(), 3), @r#"
    Session 00893aaf, step 3 of 10, 2026-10-06 23:59:40
    Edit, succeeded in 12 ms

    Input:
      {
        "file_path": "/home/dev/project/src/lib.rs",
        "old_string": "pub fn old_name() {}",
        "new_string": "pub fn new_name() {}",
        "replace_all": false
      }

    Output:
      {
        "filePath": "/home/dev/project/src/lib.rs",
        "oldString": "pub fn old_name() {}",
        "newString": "pub fn new_name() {}",
        "userModified": false,
        "replaceAll": false
      }

    1 file changed:
      modified  src/lib.rs

    --- a/src/lib.rs
    +++ b/src/lib.rs
    @@ -1,4 +1,4 @@
    -pub fn old_name() {}
    +pub fn new_name() {}
     pub fn helper() -> u32 {
     	1
     }
    "#);
}

#[test]
fn a_write_shows_the_new_file_as_added_lines() {
    let project = recorded_project();
    insta::assert_snapshot!(short(project.path(), 7), @r#"
    Session 00893aaf, step 7 of 10, 2026-10-07 00:01:00
    Write, succeeded in 9 ms

    Input:
      {
        "file_path": "/home/dev/project/src/factorial.rs",
        "content": "pub fn factorial(n: u64) -> u64 {\n    (1..=n).product()\n}\n"
      }

    Output:
      {
        "type": "create",
        "filePath": "/home/dev/project/src/factorial.rs"
      }

    1 file changed:
      added     src/factorial.rs

    --- /dev/null
    +++ b/src/factorial.rs
    @@ -0,0 +1,3 @@
    +pub fn factorial(n: u64) -> u64 {
    +    (1..=n).product()
    +}
    "#);
}

#[test]
fn a_shell_command_shows_the_files_it_changed_including_from_a_child_process() {
    let project = recorded_project();
    insta::assert_snapshot!(short(project.path(), 4), @r"
    Session 00893aaf, step 4 of 10, 2026-10-07 00:00:00
    Bash, succeeded in 87 ms, subagent a1b2c3d4

    Command:
      ./scripts/update.sh && echo done

    Output:
      updated README.md
      generated src/gen.rs
      done

    2 files changed:
      modified  README.md
      added     src/gen.rs

    --- a/README.md
    +++ b/README.md
    @@ -1,1 +1,3 @@
     # Demo
    +
    +Renamed `old_name` to `new_name`.

    --- /dev/null
    +++ b/src/gen.rs
    @@ -0,0 +1,1 @@
    +// generated
    ");
}

#[test]
fn a_failed_step_and_a_step_with_no_file_changes() {
    let project = recorded_project();
    insta::assert_snapshot!(short(project.path(), 5), @r"
    Session 00893aaf, step 5 of 10, 2026-10-07 00:00:20
    Bash, FAILED after 4.1 s

    Command:
      npm test

    Error:
      Exit code 1
      Error: Cannot find module 'express'

    No files changed.
    ");
    insta::assert_snapshot!(short(project.path(), 6), @r#"
    Session 00893aaf, step 6 of 10, 2026-10-07 00:00:40
    Read, succeeded in 3 ms

    Input:
      {
        "file_path": "/home/dev/project/src/lib.rs"
      }

    Output:
      pub fn new_name() {}

    No files changed.
    "#);
}

#[test]
fn binary_deleted_and_large_changes() {
    let project = recorded_project();
    let output = short(project.path(), 8);
    // The diff of `long.txt` (800 lines) is cut once the diff output reaches 300 lines, the blank lines,
    // the binary file's note and the file header included: 294 lines of the hunk are shown.
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 15 + 300 + 1, "{output}");
    assert_eq!(lines[lines.len() - 2], "-old line 294");
    let head_and_tail = [&lines[..24], &["[...]"], &lines[lines.len() - 3..]].concat();
    insta::assert_snapshot!(head_and_tail.join("\n"), @r"
    Session 00893aaf, step 8 of 10, 2026-10-07 00:01:20
    Bash, succeeded in 2.3 s

    Command:
      ./scripts/regenerate.sh

    Output: (none)

    Error output:
      warning: slow

    3 files changed:
      modified  logo.png
      modified  long.txt
      deleted   obsolete.txt

    logo.png: binary file changed

    --- a/long.txt
    +++ b/long.txt
    @@ -1,400 +1,400 @@
    -old line 1
    -old line 2
    -old line 3
    [...]
    -old line 293
    -old line 294
    ... diff truncated (--full shows more)
    ");

    // `--full` shows the whole diff.
    let full = show_output(
        project.path(),
        8,
        &Options {
            full: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(!full.contains("truncated"), "{full}");
    for n in [1, 200, 400] {
        assert!(full.contains(&format!("\n-old line {n}\n")), "{n}");
        assert!(full.contains(&format!("\n+new line {n}\n")), "{n}");
    }
}

#[test]
fn a_reply_and_the_end_of_the_session_take_no_snapshot() {
    let project = recorded_project();
    insta::assert_snapshot!(short(project.path(), 9), @r"
    Session 00893aaf, step 9 of 10, 2026-10-07 00:01:40

    Reply:
      I added `factorial` in src/factorial.rs. The test suite fails because `express` is not installed.
    ");
    insta::assert_snapshot!(short(project.path(), 10), @r"
    Session 00893aaf, step 10 of 10, 2026-10-07 00:02:00
    Session ended (prompt_input_exit)
    ");
}

#[test]
fn a_step_whose_snapshot_failed_or_whose_base_is_not_known() {
    let project = recorded_project();
    database(project.path())
        .execute(
            "UPDATE events SET tree_id = NULL, base_tree_id = NULL, files_changed = NULL WHERE step = 6",
            [],
        )
        .unwrap();
    insta::assert_snapshot!(short(project.path(), 6), @r#"
    Session 00893aaf, step 6 of 10, 2026-10-07 00:00:40
    Read, succeeded in 3 ms

    Input:
      {
        "file_path": "/home/dev/project/src/lib.rs"
      }

    Output:
      pub fn new_name() {}

    No snapshot was recorded for this step (taking it failed, see .yalper/errors.log). Its file changes are in the next step that has a snapshot.
    "#);

    // A step recorded before Yalper kept each step's base.
    database(project.path())
        .execute("UPDATE events SET base_tree_id = NULL WHERE step = 3", [])
        .unwrap();
    let output = short(project.path(), 3);
    assert!(
        output.ends_with(
            "\nThe snapshot before this step is not known, so its file changes cannot be shown.\n"
        ),
        "{output}"
    );
    // Without the diff, the Edit's own patch and original file stay in its output.
    assert!(
        output.contains("\n    \"structuredPatch\": [\n"),
        "{output}"
    );
    assert!(output.contains("\n    \"originalFile\": "), "{output}");
}

#[test]
fn a_snapshot_the_store_no_longer_has() {
    let project = recorded_project();
    // As after the store was lost and created again: the trees named in the event log are gone.
    let gone = "0123456789abcdef0123456789abcdef01234567";
    let conn = database(project.path());
    conn.execute("UPDATE events SET tree_id = ?1 WHERE step = 3", [gone])
        .unwrap();
    conn.execute("UPDATE events SET base_tree_id = ?1 WHERE step = 4", [gone])
        .unwrap();
    for step in [3, 4] {
        let output = short(project.path(), step);
        assert!(output.contains("\nSnapshot not available: "), "{output}");
    }
    assert!(short(project.path(), 3).contains("\"structuredPatch\""));
    // A missing store too.
    fs::remove_dir_all(project.path().join(YALPER_DIR).join(SNAPSHOTS_DIR)).unwrap();
    let output = short(project.path(), 8);
    assert!(output.contains("\nSnapshot not available: "), "{output}");
    assert!(output.contains("Command:"), "{output}");
}

/// Whether `text` holds a control character other than a line break or a tab.
fn has_control_characters(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
}

#[test]
fn control_characters_never_reach_the_terminal() {
    let project = common::project();
    let root = project.path();
    fs::write(root.join("a.txt"), "first\n").unwrap();
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    let evil = "\u{1b}]0;owned\u{7}\u{1b}[2J\u{9b}31m\r\u{202E}X";
    record_payload(&dir, payload("UserPromptSubmit", json!({"prompt": evil})));
    // File names and contents from the repository, and every value of the tool call.
    fs::write(root.join("a.txt"), format!("first\n{evil}\tend\n")).unwrap();
    fs::write(root.join(format!("{}b.txt", "\u{202E}")), evil).unwrap();
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({
                "tool_name": format!("Bash{evil}"),
                "agent_id": evil,
                "tool_input": {"command": evil, evil: evil},
                "tool_response": {"stdout": evil, "stderr": evil},
            }),
        ),
    );
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": evil}, "tool_response": evil}),
        ),
    );
    record_payload(
        &dir,
        payload(
            "PostToolUseFailure",
            json!({"tool_name": "Bash", "error": evil}),
        ),
    );
    record_payload(
        &dir,
        payload("Stop", json!({"last_assistant_message": evil})),
    );
    record_payload(&dir, payload("SessionEnd", json!({"reason": evil})));

    for step in 1..=6 {
        for full in [false, true] {
            let options = Options {
                full,
                ..Options::default()
            };
            let output = show_output(root, step, &options).unwrap();
            assert!(!has_control_characters(&output), "{step}: {output:?}");
            assert!(!output.contains('\u{202E}'), "{step}: {output}");
            assert!(output.contains("X"), "{step}: {output}");
        }
    }
    let output = short(root, 2);
    assert!(output.contains("<U+202E>b.txt"), "{output}");
    // The tab of the file content is kept.
    assert!(
        output.contains("+ ]0;owned  [2J 31m <U+202E>X\tend"),
        "{output}"
    );

    // Colors add escape sequences of Yalper's own, and only those.
    let options = Options {
        color: true,
        ..Options::default()
    };
    let colored = show_output(root, 2, &options).unwrap();
    assert!(colored.contains("\u{1b}[32m+ ]0;owned"), "{colored}");
    let mut stripped = colored.clone();
    for code in [
        "\u{1b}[1m",
        "\u{1b}[36m",
        "\u{1b}[32m",
        "\u{1b}[31m",
        "\u{1b}[0m",
    ] {
        stripped = stripped.replace(code, "");
    }
    assert!(!has_control_characters(&stripped), "{stripped:?}");
    assert_eq!(stripped, output);
}

#[test]
fn an_agent_step_keeps_the_content_of_its_answer() {
    let project = common::project();
    let root = project.path();
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    // The subagent changed a file too: only file tools have their response trimmed when a diff is shown.
    fs::write(root.join("notes.txt"), "callers\n").unwrap();
    record_payload(&dir, fixture("post_tool_use_agent.json"));
    let output = short(root, 1);
    assert!(output.contains("\n    \"content\": [\n"), "{output}");
    assert!(
        output.contains("Found 2 callers of old_name: src/main.rs:4 and src/cli.rs:17."),
        "{output}"
    );
}

#[test]
fn many_changed_files_are_listed_up_to_a_limit() {
    let project = common::project();
    let root = project.path();
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    record_payload(&dir, payload("SessionStart", json!({"source": "startup"})));
    fs::create_dir(root.join("gen")).unwrap();
    for n in 0..60 {
        fs::write(root.join(format!("gen/file{n:02}.txt")), "x\n").unwrap();
    }
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "make"}}),
        ),
    );
    let output = short(root, 2);
    assert!(output.contains("60 files changed:"), "{output}");
    assert!(output.contains("  added     gen/file49.txt\n"), "{output}");
    assert!(!output.contains("gen/file50.txt"), "{output}");
    assert!(
        output.contains("... 10 more files (--full lists all)"),
        "{output}"
    );
    let full = show_output(
        root,
        2,
        &Options {
            full: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(full.contains("  added     gen/file59.txt\n"), "{full}");
    assert!(!full.contains("more files"), "{full}");
}

/// Takes a snapshot the way `yalper init` takes its baseline.
fn baseline(dir: &YalperDir) {
    let store = Store::open(&dir.dir, &dir.token).unwrap();
    let lock = WriterLock::acquire(&dir.dir, LOCK_TIMEOUT).unwrap();
    snapshot::snapshot(&dir.dir, &store, &lock).unwrap();
}

/// The number of files changed `yalper log` lists for step `step`.
fn files_changed(root: &Path, step: u32) -> u32 {
    database(root)
        .query_row(
            "SELECT files_changed FROM events WHERE step = ?1",
            [step],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn the_first_step_after_init_shows_the_changes_since_the_baseline() {
    let project = common::project();
    let root = project.path();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    baseline(&dir);
    // The developer edits a file between `yalper init` and the first session.
    fs::write(root.join("notes.txt"), "edited before the session\n").unwrap();
    record_payload(&dir, payload("SessionStart", json!({"source": "startup"})));
    let output = short(root, 1);
    assert!(
        output.ends_with(
            "1 file changed:\n  added     notes.txt\n\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1,1 @@\n\
             +edited before the session\n"
        ),
        "{output}"
    );
}

#[test]
fn a_step_after_the_store_was_created_again_shows_its_changes() {
    let project = common::project();
    let root = project.path();
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(root.join("b.txt"), "b\n").unwrap();
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    baseline(&dir);
    fs::write(root.join("a.txt"), "a2\n").unwrap();
    record_payload(&dir, payload("PostToolUse", json!({"tool_name": "Bash"})));
    drop(dir);

    // The store is lost; `yalper init` creates it again, forgets the latest snapshot and takes a baseline.
    fs::remove_dir_all(root.join(YALPER_DIR).join(SNAPSHOTS_DIR)).unwrap();
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    ShadowStore::init(&dir.dir, &dir.token).unwrap();
    {
        let store = Store::open(&dir.dir, &dir.token).unwrap();
        let lock = WriterLock::acquire(&dir.dir, LOCK_TIMEOUT).unwrap();
        store.forget_snapshot(&lock).unwrap();
        snapshot::snapshot(&dir.dir, &store, &lock).unwrap();
    }
    fs::write(root.join("b.txt"), "b2\n").unwrap();
    record_payload(&dir, payload("PostToolUse", json!({"tool_name": "Bash"})));

    assert!(short(root, 1).contains("\nSnapshot not available: "));
    let output = short(root, 2);
    assert!(
        output.ends_with("1 file changed:\n  modified  b.txt\n\n--- a/b.txt\n+++ b/b.txt\n@@ -1,1 +1,1 @@\n-b\n+b2\n"),
        "{output}"
    );
    assert_eq!(files_changed(root, 2), 1);
}

#[test]
fn a_snapshot_that_started_over_lists_what_log_counts() {
    let project = common::project();
    let root = project.path();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(root.join(name), name).unwrap();
    }
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    baseline(&dir);
    // A stat cache the snapshot cannot use: it starts over from the empty tree, so every file counts.
    database(root)
        .execute("UPDATE latest_snapshot SET tree_id = X'07'", [])
        .unwrap();
    record_payload(&dir, payload("PostToolUse", json!({"tool_name": "Bash"})));
    assert_eq!(files_changed(root, 1), 3);
    let output = short(root, 1);
    assert!(output.contains("\n3 files changed:\n"), "{output}");
    assert_eq!(output.matches("\n  added     ").count(), 3, "{output}");
}

/// The error a failed `yalper` run printed.
fn error_of(output: &Output) -> String {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn an_unknown_step_or_session_is_a_clear_error() {
    let project = recorded_project();
    let root = project.path();
    let error = error_of(&common::yalper(root, &["show", "11"]));
    assert_eq!(
        error,
        "error: session 00893aaf has no step 11: its steps are 1 to 10. `yalper log` lists them.\n"
    );
    let error = error_of(&common::yalper(root, &["show", "1", "--session", "ff"]));
    assert!(
        error.contains("no recorded session id starts with ff"),
        "{error}"
    );
    // Not a step number at all: refused by the argument parser.
    let error = error_of(&common::yalper(root, &["show", "first"]));
    assert!(error.contains("invalid value 'first'"), "{error}");

    // The real binary, with a session chosen by its id prefix.
    let output = common::yalper(root, &["show", "3", "--session", "0089"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("Session 00893aaf, step 3 of 10, "),
        "{stdout}"
    );
    assert!(stdout.contains("+pub fn new_name() {}"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn nothing_recorded_or_not_set_up() {
    let project = common::project();
    let database = project.path().join(YALPER_DIR).join(DATABASE_FILE);
    let error = error_of(&common::yalper(project.path(), &["show", "1"]));
    assert!(error.contains("run `yalper init` first"), "{error}");
    assert!(!database.exists());

    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    drop(Store::open(&dir.dir, &dir.token).unwrap());
    let error = error_of(&common::yalper(project.path(), &["show", "1"]));
    assert!(error.contains("No sessions recorded yet."), "{error}");

    let outside = tempfile::tempdir().unwrap();
    let error = error_of(&common::yalper(outside.path(), &["show", "1"]));
    assert!(
        error.contains("Run `yalper show` inside a project"),
        "{error}"
    );
}

#[test]
fn showing_writes_nothing_into_the_yalper_dir() {
    let project = recorded_project();
    let yalper_dir = project.path().join(YALPER_DIR);
    // See `listing_writes_nothing_into_the_yalper_dir` in the log tests: a store open brings back the WAL.
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    drop(Store::open(&dir.dir, &dir.token).unwrap());
    drop(dir);
    let before = common::files(&yalper_dir);
    for step in ["1", "3", "4", "7", "9"] {
        let output = common::yalper(project.path(), &["show", step, "--full"]);
        assert!(output.status.success(), "{step}");
    }
    assert!(common::files(&yalper_dir) == before);
}
