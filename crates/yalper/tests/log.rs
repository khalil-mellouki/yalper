//! `yalper log`: its output for a session recorded from the hook fixtures, untrusted rows, the empty and
//! not-set-up cases, and reading while hooks record.

use std::fs;
use std::path::Path;
use std::process::Output;
use std::thread;

use jiff::Timestamp;
use jiff::tz::{self, TimeZone};
use serde_json::{Value, json};
use yalper::hook::{HookInput, YALPER_DIR, find_yalper_dir};
use yalper::log::{Selection, log};
use yalper::record::record;
use yalper::repo::YalperDir;
use yalper::snapshot;
use yalper::store::{DATABASE_FILE, Event, LOCK_TIMEOUT, Session, Store, WriterLock};

mod common;

const SESSION: &str = "00893aaf-19fa-41d2-8238-13269b9b3ca0";
const OLDER_SESSION: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

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

/// Gives the steps of `session` fixed times, 20 seconds apart from `first` on, so the output does not depend
/// on when the test runs.
fn set_times(root: &Path, session: &str, first: &str) {
    let first = first.parse::<Timestamp>().unwrap().as_millisecond();
    let conn = rusqlite::Connection::open(root.join(YALPER_DIR).join(DATABASE_FILE)).unwrap();
    conn.execute(
        "UPDATE events SET ts_ms = ?1 + (step - 1) * 20000 WHERE session_id = ?2",
        rusqlite::params![first, session],
    )
    .unwrap();
    conn.execute(
        "UPDATE sessions SET started_at_ms = ?1,
                             ended_at_ms = CASE WHEN ended_at_ms IS NULL THEN NULL ELSE ?1 + 600000 END
         WHERE id = ?2",
        rusqlite::params![first, session],
    )
    .unwrap();
}

/// A project with two recorded sessions, built by feeding the hook fixtures (and a few more payloads) to the
/// recorder, with file changes between the steps as the tools would make them.
fn recorded_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("README.md"), "# Demo\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn old_name() {}\n").unwrap();
    {
        // What `yalper init` sets up, including its baseline snapshot.
        let dir = common::init(root);
        let store = Store::open(&dir.dir, &dir.token).unwrap();
        let lock = WriterLock::acquire(&dir.dir, LOCK_TIMEOUT).unwrap();
        snapshot::snapshot(&dir.dir, &store, &lock).unwrap();
    }
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();

    // An older session that has not ended.
    let older = |event: &str, fields: Value| {
        let mut payload = payload(event, fields);
        payload["session_id"] = OLDER_SESSION.into();
        payload
    };
    record_payload(&dir, older("SessionStart", json!({"source": "startup"})));
    record_payload(
        &dir,
        older(
            "UserPromptSubmit",
            json!({"prompt": "Fix the typo in README.md"}),
        ),
    );
    fs::write(root.join("README.md"), "# Demo project\n").unwrap();
    let readme = root.join("README.md");
    record_payload(
        &dir,
        older(
            "PostToolUse",
            json!({"tool_name": "Edit", "tool_input": {"file_path": readme.to_str().unwrap(), "old_string": "Demo", "new_string": "Demo project"}}),
        ),
    );
    set_times(root, OLDER_SESSION, "2026-10-05T08:00:00Z");

    // The fixture session, with paths inside this project.
    record_payload(&dir, fixture("session_start.json"));
    let mut prompt = fixture("user_prompt_submit.json");
    prompt["prompt"] =
        "Write a function to calculate the factorial of a number\nand test it".into();
    record_payload(&dir, prompt);
    fs::write(root.join("src/factorial.rs"), "pub fn factorial() {}\n").unwrap();
    let mut write = fixture("post_tool_use_write_windows.json");
    write["tool_input"]["file_path"] = root
        .join("src")
        .join("factorial.rs")
        .to_str()
        .unwrap()
        .into();
    record_payload(&dir, write);
    fs::write(root.join("src/lib.rs"), "pub fn new_name() {}\n").unwrap();
    record_payload(&dir, fixture("post_tool_use_bash_subagent.json"));
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({"tool_name": "Grep", "tool_input": {"pattern": "fn factorial", "path": root.to_str().unwrap()}, "tool_response": {"numFiles": 1}}),
        ),
    );
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {"file_path": "/opt/toolchains/rust/lib/rustlib/src/rust/library/core/src/num/mod.rs"}}),
        ),
    );
    record_payload(&dir, fixture("post_tool_use_failure.json"));
    record_payload(
        &dir,
        payload(
            "PostToolUse",
            json!({"tool_name": "mcp__github__create_issue", "tool_input": {"title": "Add factorial"}}),
        ),
    );
    record_payload(&dir, fixture("stop.json"));
    record_payload(&dir, fixture("session_end.json"));
    // Starts at 23:59 in the time zone the output is shown in, so the session runs past midnight.
    set_times(root, SESSION, "2026-10-06T21:59:00Z");
    // The Grep step's snapshot failed: the step is recorded without a tree.
    rusqlite::Connection::open(root.join(YALPER_DIR).join(DATABASE_FILE))
        .unwrap()
        .execute(
            "UPDATE events SET tree_id = NULL, files_changed = NULL WHERE session_id = ?1 AND step = 5",
            [SESSION],
        )
        .unwrap();
    project
}

/// The output of `yalper log` in `root`, with times shown two hours ahead of UTC.
fn log_output(root: &Path, selection: Selection) -> Result<String, String> {
    let mut out = Vec::new();
    log(root, &selection, &TimeZone::fixed(tz::offset(2)), &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

#[test]
fn the_latest_session_is_listed_with_its_steps() {
    let project = recorded_project();
    let output = log_output(project.path(), Selection::Latest).unwrap();
    insta::assert_snapshot!(output, @r"
    Session 00893aaf, started 2026-10-06 23:59:00, ended (prompt_input_exit), 10 steps
      step  time      files  action               summary
         1  23:59:00      0  start                startup
         2  23:59:20      0  prompt               Write a function to calculate the factorial of a number...
         3  23:59:40      1  Write                src/factorial.rs
      (2026-10-07)
         4  00:00:00      1  Bash                 sed -i 's/old_name/new_name/g' src/lib.rs
         5  00:00:20      ?  Grep                 fn factorial
         6  00:00:40      0  Read                 ...ins/rust/lib/rustlib/src/rust/library/core/src/num/mod.rs
         7  00:01:00      0  Bash                 FAILED npm test
         8  00:01:20      0  github:create_issue
         9  00:01:40         reply                I added `factorial` in src/factorial.rs. The test suite f...
        10  00:02:00         end                  prompt_input_exit

    Earlier sessions (`yalper log --session <id>` shows one):
      Session 0a1b2c3d, started 2026-10-05 10:00:00, not ended, 3 steps
    ");
}

#[test]
fn every_session_or_one_chosen_by_id_prefix() {
    let project = recorded_project();
    let all = log_output(project.path(), Selection::All).unwrap();
    insta::assert_snapshot!(all, @r"
    Session 00893aaf, started 2026-10-06 23:59:00, ended (prompt_input_exit), 10 steps
      step  time      files  action               summary
         1  23:59:00      0  start                startup
         2  23:59:20      0  prompt               Write a function to calculate the factorial of a number...
         3  23:59:40      1  Write                src/factorial.rs
      (2026-10-07)
         4  00:00:00      1  Bash                 sed -i 's/old_name/new_name/g' src/lib.rs
         5  00:00:20      ?  Grep                 fn factorial
         6  00:00:40      0  Read                 ...ins/rust/lib/rustlib/src/rust/library/core/src/num/mod.rs
         7  00:01:00      0  Bash                 FAILED npm test
         8  00:01:20      0  github:create_issue
         9  00:01:40         reply                I added `factorial` in src/factorial.rs. The test suite f...
        10  00:02:00         end                  prompt_input_exit

    Session 0a1b2c3d, started 2026-10-05 10:00:00, not ended, 3 steps
      step  time      files  action  summary
         1  10:00:00      0  start   startup
         2  10:00:20      0  prompt  Fix the typo in README.md
         3  10:00:40      1  Edit    README.md
    ");

    let older = log_output(project.path(), Selection::Session("0a".to_owned())).unwrap();
    assert!(all.ends_with(&older), "{older}");
    let exact = log_output(project.path(), Selection::Session(SESSION.to_owned())).unwrap();
    assert!(all.starts_with(&exact), "{exact}");

    let error = log_output(project.path(), Selection::Session("0".to_owned())).unwrap_err();
    assert_eq!(
        error,
        "2 sessions start with 0: 00893aaf, 0a1b2c3d. Give more of the id."
    );
    let error = log_output(project.path(), Selection::Session("ff".to_owned())).unwrap_err();
    assert_eq!(
        error,
        "no recorded session id starts with ff. `yalper log --all` lists every session."
    );
}

/// Whether `text` holds a control character other than a line break: the start of a terminal escape
/// sequence, a carriage return that rewrites the line, a bell.
fn has_control_characters(text: &str) -> bool {
    text.chars().any(|c| c.is_control() && c != '\n')
}

#[test]
fn control_characters_in_stored_rows_never_reach_the_terminal() {
    let project = common::project();
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    let store = Store::open(&dir.dir, &dir.token).unwrap();
    // Values as a crafted payload could make them: escape sequences (7-bit, and the 8-bit CSI), a carriage
    // return, a bell and a tab, in every value that is printed.
    let evil = "\u{1b}]0;owned\u{7}\u{1b}[2J\u{9b}31m\r\tX";
    let id = format!("X{evil}");
    let mut session = Session::new(id.clone(), 0);
    session.ended_at_ms = Some(1);
    session.end_reason = Some(evil.to_owned());
    store.upsert_session(&session).unwrap();
    let step = |step: u32, kind: &str, tool_name: Option<&str>, payload: Value| Event {
        session_id: id.clone(),
        step,
        ts_ms: 0,
        kind: kind.to_owned(),
        tool_name: tool_name.map(str::to_owned),
        tool_use_id: None,
        agent_id: None,
        success: None,
        tree_id: None,
        files_changed: None,
        payload,
    };
    let path = format!("/{evil}/a.rs");
    for event in [
        step(1, "SessionStart", None, json!({"source": evil})),
        step(2, "UserPromptSubmit", None, json!({"prompt": evil})),
        step(
            3,
            "PostToolUse",
            Some(evil),
            json!({"tool_input": {"command": evil}}),
        ),
        step(
            4,
            "PostToolUse",
            Some("Read"),
            json!({"tool_input": {"file_path": path}}),
        ),
        step(5, "Stop", None, json!({"last_assistant_message": evil})),
        step(6, "SessionEnd", None, json!({"reason": evil})),
        step(7, evil, None, json!({})),
        // A right-to-left override would show `echo txt.exe` as `echo exe.txt`.
        step(
            8,
            "PostToolUse",
            Some("Bash\u{202E}"),
            json!({"tool_input": {"command": "echo \u{202E}txt.exe"}}),
        ),
    ] {
        store.insert_event(&event).unwrap();
    }

    for selection in [
        Selection::Latest,
        Selection::All,
        Selection::Session(id.chars().take(3).collect()),
    ] {
        let output = log_output(project.path(), selection).unwrap();
        assert!(!has_control_characters(&output), "{output:?}");
        // Every value is still shown: the session id, its end reason, and each step's action or summary.
        assert_eq!(output.matches('X').count(), 10, "{output}");
        assert!(!output.contains('\u{202E}'), "{output}");
        assert!(output.contains("Bash<U+202E>"), "{output}");
        assert!(output.contains("echo <U+202E>txt.exe"), "{output}");
    }
    let error = log_output(project.path(), Selection::Session(format!("{evil}-none"))).unwrap_err();
    assert!(!has_control_characters(&error), "{error:?}");
}

#[test]
fn nothing_recorded_yet_says_how_to_check_the_setup() {
    let project = common::project();
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    let store = Store::open(&dir.dir, &dir.token).unwrap();
    let output = common::yalper(project.path(), &["log"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("No sessions recorded yet."), "{stdout}");
    assert!(stdout.contains("trusted in Claude Code"), "{stdout}");
    assert!(output.stderr.is_empty());

    // A session whose first step is not recorded yet.
    store.upsert_session(&Session::new(SESSION, 0)).unwrap();
    let output = log_output(project.path(), Selection::Latest).unwrap();
    insta::assert_snapshot!(output, @r"
    Session 00893aaf, started 1970-01-01 02:00:00, not ended, 0 steps
      No steps recorded yet.
    ");
}

/// The error a failed `yalper` run printed.
fn error_of(output: &Output) -> String {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn a_project_where_yalper_is_not_set_up_gets_a_clear_error() {
    let outside = tempfile::tempdir().unwrap();
    let error = error_of(&common::yalper(outside.path(), &["log"]));
    assert!(error.contains("is not inside a git repository"), "{error}");
    // Paths in error messages are filtered too.
    let odd = outside.path().join("txt\u{202E}.exe");
    fs::create_dir(&odd).unwrap();
    let error = error_of(&common::yalper(&odd, &["log"]));
    assert!(error.contains("txt<U+202E>.exe"), "{error}");
    assert!(!error.contains('\u{202E}'), "{error}");

    let repository = common::repository();
    let error = error_of(&common::yalper(repository.path(), &["log"]));
    assert!(error.contains("Yalper is not set up in"), "{error}");
    assert!(error.contains("yalper init"), "{error}");

    // A `.yalper/` that `yalper init` did not create, as a cloned repository could commit one.
    fs::create_dir(repository.path().join(YALPER_DIR)).unwrap();
    fs::write(
        repository.path().join(YALPER_DIR).join(DATABASE_FILE),
        "planted",
    )
    .unwrap();
    let error = error_of(&common::yalper(repository.path(), &["log"]));
    assert!(error.contains("Yalper cannot use"), "{error}");
    assert!(error.contains("not created by `yalper init`"), "{error}");
}

#[test]
fn a_missing_or_empty_database_is_reported_and_never_created() {
    let project = common::project();
    let database = project.path().join(YALPER_DIR).join(DATABASE_FILE);
    let error = error_of(&common::yalper(project.path(), &["log"]));
    assert!(error.contains("run `yalper init` first"), "{error}");
    assert!(!database.exists());

    fs::write(&database, "").unwrap();
    let error = error_of(&common::yalper(project.path(), &["log"]));
    assert!(error.contains("run `yalper init` first"), "{error}");
    assert_eq!(fs::metadata(&database).unwrap().len(), 0);
}

#[test]
fn the_default_view_lists_at_most_five_earlier_sessions() {
    let project = common::project();
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    let store = Store::open(&dir.dir, &dir.token).unwrap();
    for minute in 0..8 {
        let id = format!("session{minute}-0000");
        store
            .upsert_session(&Session::new(id, minute * 60_000))
            .unwrap();
    }
    let output = log_output(project.path(), Selection::Latest).unwrap();
    insta::assert_snapshot!(output, @r"
    Session session7, started 1970-01-01 02:07:00, not ended, 0 steps
      No steps recorded yet.

    Earlier sessions (`yalper log --session <id>` shows one):
      Session session6, started 1970-01-01 02:06:00, not ended, 0 steps
      Session session5, started 1970-01-01 02:05:00, not ended, 0 steps
      Session session4, started 1970-01-01 02:04:00, not ended, 0 steps
      Session session3, started 1970-01-01 02:03:00, not ended, 0 steps
      Session session2, started 1970-01-01 02:02:00, not ended, 0 steps
      2 more (`yalper log --all`)
    ");
}

#[test]
fn listing_writes_nothing_into_the_yalper_dir() {
    let project = recorded_project();
    let yalper_dir = project.path().join(YALPER_DIR);
    // The test changed the database behind Yalper's back, and closing that connection deleted the WAL. A
    // store open, as any hook does, brings it back: SQLite creates it when it opens a WAL database, for
    // readers too.
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    drop(Store::open(&dir.dir, &dir.token).unwrap());
    drop(dir);
    let before = common::files(&yalper_dir);
    for args in [&["log"][..], &["log", "--all"], &["log", "--session", "0a"]] {
        let output = common::yalper(project.path(), args);
        assert!(output.status.success(), "{args:?}");
        assert!(!output.stdout.is_empty());
    }
    let after = common::files(&yalper_dir);
    let changed: Vec<_> = after
        .keys()
        .chain(before.keys())
        .filter(|path| after.get(*path) != before.get(*path))
        .map(|path| {
            (
                path,
                before.get(path).map(Vec::len),
                after.get(path).map(Vec::len),
            )
        })
        .collect();
    assert!(changed.is_empty(), "{changed:?}");
}

/// The step numbers listed in an output of `yalper log` for one session.
fn listed_steps(output: &str) -> Vec<u32> {
    output
        .lines()
        .skip(2)
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

#[test]
fn listing_works_while_hooks_record() {
    const STEPS: u32 = 40;
    let project = common::project();
    let root = project.path();
    let dir = find_yalper_dir([root.to_path_buf()]).unwrap();
    record_payload(&dir, payload("SessionStart", json!({"source": "startup"})));

    thread::scope(|scope| {
        // If the writer panics, it finishes and the scope fails the test.
        let writer = scope.spawn(|| {
            for step in 0..STEPS {
                fs::write(root.join("file.txt"), step.to_string()).unwrap();
                let input =
                    json!({"tool_name": "Bash", "tool_input": {"command": format!("echo {step}")}});
                record_payload(&dir, payload("PostToolUse", input));
            }
        });

        let mut listed = 0;
        loop {
            let output = log_output(root, Selection::Latest).unwrap();
            let steps = listed_steps(&output);
            // Every read sees whole steps, numbered from 1 with no gap, and never fewer than before.
            assert_eq!(
                steps,
                (1..=steps.len() as u32).collect::<Vec<_>>(),
                "{output}"
            );
            assert!(steps.len() >= listed, "{output}");
            listed = steps.len();
            if writer.is_finished() {
                break;
            }
        }
    });

    let output = log_output(root, Selection::Latest).unwrap();
    assert_eq!(listed_steps(&output), (1..=STEPS + 1).collect::<Vec<_>>());
}
