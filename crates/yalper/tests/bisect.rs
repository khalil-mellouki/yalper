//! `yalper bisect` through the real binary, on sessions recorded the way Claude Code records them: `yalper init`
//! in a git project, then the hook payloads of each step while the files on disk change.
//!
//! Each test sets `YALPER_CACHE_DIR` to a temporary folder, and checks that the project, `.git` included, is
//! the same after the bisect: only the event log in `.yalper/` may change (where the bisect data is).

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use common::{EXE, files, git};
use serde_json::{Value, json};

const SESSION: &str = "5f3c2a1e-8b4d-4e6f-9a7b-1c2d3e4f5a6b";
const EARLIER_SESSION: &str = "0a9b8c7d-6e5f-4a3b-2c1d-0e9f8a7b6c5d";
const LATER_SESSION: &str = "7e6d5c4b-3a2f-4e1d-8c9b-0a1f2e3d4c5b";

/// The tests pass while `status.txt` says `good` (and the matching line is printed).
const TEST: &str = if cfg!(windows) {
    "findstr /c:good status.txt"
} else {
    "grep good status.txt"
};

/// The same test from a script of the project, which can also skip, hang, fail once, print a file or wait (see
/// [`script`]). On Windows with its path: `cmd` does not look in the current folder when
/// `NoDefaultCurrentDirectoryInExePath` is set.
const SCRIPT_TEST: &str = if cfg!(windows) {
    r".\check.cmd"
} else {
    "sh check.sh"
};

/// The test script: exit 125 when `skip.txt` exists, hang when `hang.txt` exists, fail once (the first time
/// the file `$FLAKY_MARKER` is missing) when `flaky.txt` exists, print the file `$PRINT_FILE` if set, and wait
/// until `$HOLD/release` exists after writing `$HOLD/started` if `HOLD` is set; then pass while `status.txt`
/// says `good`.
fn script() -> (&'static str, String) {
    if cfg!(windows) {
        let lines = [
            "@echo off",
            "if defined PRINT_FILE type \"%PRINT_FILE%\"",
            "if defined HOLD call :hold",
            "if exist skip.txt exit /b 125",
            "if exist hang.txt ping -n 31 127.0.0.1 >nul",
            "if exist flaky.txt if defined FLAKY_MARKER if not exist \"%FLAKY_MARKER%\" (",
            "  echo.>\"%FLAKY_MARKER%\"",
            "  exit /b 1",
            ")",
            "findstr /c:good status.txt >nul",
            "exit /b %errorlevel%",
            ":hold",
            "echo.>\"%HOLD%\\started\"",
            ":wait",
            "if exist \"%HOLD%\\release\" exit /b 0",
            "ping -n 2 127.0.0.1 >nul",
            "goto wait",
        ];
        ("check.cmd", lines.join("\r\n") + "\r\n")
    } else {
        let lines = [
            "if [ -n \"$PRINT_FILE\" ]; then cat \"$PRINT_FILE\"; fi",
            "if [ -n \"$HOLD\" ]; then",
            "  : > \"$HOLD/started\"",
            "  while [ ! -f \"$HOLD/release\" ]; do sleep 0.1; done",
            "fi",
            "if [ -f skip.txt ]; then exit 125; fi",
            "if [ -f hang.txt ]; then sleep 30; fi",
            "if [ -f flaky.txt ] && [ -n \"$FLAKY_MARKER\" ] && [ ! -f \"$FLAKY_MARKER\" ]; then",
            "  : > \"$FLAKY_MARKER\"",
            "  exit 1",
            "fi",
            "grep -q good status.txt",
        ];
        ("check.sh", lines.join("\n") + "\n")
    }
}

/// A git project with one commit, set up with `yalper init`, whose `status.txt` says `status`.
struct Project {
    root: PathBuf,
    /// `YALPER_CACHE_DIR` of every command.
    cache: tempfile::TempDir,
    /// Files the test command reads or writes, outside the project.
    outside: tempfile::TempDir,
    /// The temporary folder of every command, which a bisect leaves empty.
    temp: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

/// What one tool step does: files written (`Some`) or deleted (`None`).
type Edits<'a> = &'a [(&'a str, Option<&'a str>)];

impl Project {
    fn new(status: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // The path the hooks and the agent see: on macOS the temporary folder is reached through `/var`.
        #[cfg(unix)]
        let root = fs::canonicalize(dir.path()).unwrap();
        #[cfg(not(unix))]
        let root = dir.path().to_owned();
        let (name, content) = script();
        fs::write(root.join(name), content).unwrap();
        fs::write(root.join("status.txt"), format!("{status}\n")).unwrap();
        fs::write(root.join("README.md"), "# demo\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "--all"]);
        git(&root, &["commit", "--quiet", "--message", "Initial commit"]);
        let project = Self {
            root,
            cache: tempfile::tempdir().unwrap(),
            outside: tempfile::tempdir().unwrap(),
            temp: tempfile::tempdir().unwrap(),
            _dir: dir,
        };
        let output = project.yalper(&["init"], &[]);
        assert!(output.status.success(), "{output:?}");
        project
    }

    /// Records a session: its start, the prompt `prompt`, then one Bash step per element of `steps`, each changing
    /// the files on disk before its hook runs, and the end of the turn. Step 3 is the first tool step.
    fn record(&self, session: &str, prompt: &str, steps: &[Edits]) {
        self.start(session);
        self.prompt(session, prompt);
        for (index, edits) in steps.iter().enumerate() {
            self.tool(session, index + 3, edits);
        }
        self.stop(session);
    }

    fn start(&self, session: &str) {
        self.hook(
            session,
            "SessionStart",
            json!({"source": "startup", "model": "claude-opus-5"}),
        );
    }

    fn prompt(&self, session: &str, prompt: &str) {
        self.hook(session, "UserPromptSubmit", json!({"prompt": prompt}));
    }

    /// A Bash step, which runs `./edit.sh <step>`, making `edits` on disk before its hook runs.
    fn tool(&self, session: &str, step: usize, edits: Edits) {
        self.edit(edits);
        self.hook(
            session,
            "PostToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": format!("./edit.sh {step}"), "description": "Edit"},
                "tool_response": {"stdout": "", "stderr": "", "interrupted": false, "isImage": false},
                "tool_use_id": format!("toolu_{step}"),
                "duration_ms": 12
            }),
        );
    }

    fn stop(&self, session: &str) {
        self.hook(
            session,
            "Stop",
            json!({"last_assistant_message": "Done.", "stop_hook_active": false}),
        );
    }

    /// Writes (`Some`) or deletes (`None`) files of the project.
    fn edit(&self, edits: Edits) {
        for (path, content) in edits {
            let path = self.root.join(path);
            match content {
                Some(content) => fs::write(path, content).unwrap(),
                None => fs::remove_file(path).unwrap(),
            }
        }
    }

    /// Moves the time of every step of `session` to January 1, 2000.
    fn move_to_2000(&self, session: &str) {
        let database = self.root.join(".yalper").join("yalper.db");
        let connection = rusqlite::Connection::open(database).unwrap();
        connection
            .execute(
                "UPDATE events SET ts_ms = 946684800000 + step WHERE session_id = ?1",
                [session],
            )
            .unwrap();
    }

    /// Runs `yalper hook` with the payload of `event` for `session`, as Claude Code does.
    fn hook(&self, session: &str, event: &str, fields: Value) {
        let mut payload = json!({
            "session_id": session,
            "transcript_path": format!("/home/dev/.claude/projects/demo/{session}.jsonl"),
            "cwd": self.root,
            "permission_mode": "default",
            "hook_event_name": event,
        });
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        let mut child = Command::new(EXE)
            .arg("hook")
            .current_dir(&self.root)
            .env("CLAUDE_PROJECT_DIR", &self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(payload.to_string().as_bytes()).unwrap();
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    }

    fn command(&self, args: &[&str], envs: &[(&str, &Path)]) -> Command {
        let mut command = Command::new(EXE);
        command
            .args(args)
            .current_dir(&self.root)
            .env("YALPER_CACHE_DIR", self.cache.path())
            .env("TMPDIR", self.temp.path())
            .env("TMP", self.temp.path())
            .env("TEMP", self.temp.path())
            .env_remove("NO_COLOR")
            .stdin(Stdio::null());
        for (name, value) in envs {
            command.env(name, value);
        }
        command
    }

    fn yalper(&self, args: &[&str], envs: &[(&str, &Path)]) -> Output {
        self.command(args, envs).output().unwrap()
    }

    /// Runs `yalper bisect` with `args`, checking that the project did not change and that nothing was written in
    /// the temporary folder.
    fn bisect(&self, args: &[&str], envs: &[(&str, &Path)]) -> (Option<i32>, String, String) {
        let before = self.files();
        let mut all = vec!["bisect"];
        all.extend_from_slice(args);
        let output = self.yalper(&all, envs);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        println!("$ yalper {}\n{stdout}{stderr}", all.join(" "));
        assert_eq!(
            self.files(),
            before,
            "the project changed: {stdout}{stderr}"
        );
        // Besides the event log, everything is written in the cache folder.
        assert!(fs::read_dir(self.temp.path()).unwrap().next().is_none());
        (output.status.code(), stdout, stderr)
    }

    /// Every file of the project and its content, `.git` included, except the event log (the bisect records
    /// where its data is in its `meta` table).
    fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let log = self.root.join(".yalper");
        let mut all = files(&self.root);
        all.retain(|path, _| {
            !(path.parent() == Some(log.as_path())
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("yalper.db"))
        });
        all
    }

    /// The per-project folder in the cache folder.
    fn cache_folder(&self) -> PathBuf {
        let yalper = self.cache.path();
        let mut folders = fs::read_dir(yalper).unwrap();
        let folder = folders.next().unwrap().unwrap().path();
        assert!(folders.next().is_none());
        // As Yalper shows it: canonical on Unix.
        if cfg!(unix) {
            fs::canonicalize(&folder).unwrap()
        } else {
            folder
        }
    }

    fn outside(&self, name: &str) -> PathBuf {
        self.outside.path().join(name)
    }
}

/// Ten tool steps, steps 3 to 12, each changing `notes.txt`; step `breaking` also says `bad` in `status.txt`.
fn notes_steps(breaking: usize) -> Vec<Vec<(String, Option<String>)>> {
    (3..=12)
        .map(|step| {
            let mut edits = vec![("notes.txt".to_owned(), Some(format!("note {step}\n")))];
            if step == breaking {
                edits.push(("status.txt".to_owned(), Some("bad\n".to_owned())));
            }
            edits
        })
        .collect()
}

/// Ten tool steps like [`notes_steps`] that break nothing, writing other notes, so that the code at their end
/// differs from the code after [`notes_steps`].
fn later_steps() -> Vec<Vec<(String, Option<String>)>> {
    (3..=12)
        .map(|step| vec![("notes.txt".to_owned(), Some(format!("later note {step}\n")))])
        .collect()
}

/// `steps` as the borrowed form [`Project::record`] takes.
fn borrowed(steps: &[Vec<(String, Option<String>)>]) -> Vec<Vec<(&str, Option<&str>)>> {
    steps
        .iter()
        .map(|edits| {
            edits
                .iter()
                .map(|(path, content)| (path.as_str(), content.as_deref()))
                .collect()
        })
        .collect()
}

fn record(project: &Project, session: &str, steps: &[Vec<(String, Option<String>)>]) {
    let steps = borrowed(steps);
    let steps: Vec<Edits> = steps.iter().map(Vec::as_slice).collect();
    project.record(session, "Tidy the notes and keep the checks green.", &steps);
}

/// The number of lines of `text` that start with `Testing `: one per run.
fn runs(text: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with("Testing "))
        .count()
}

#[test]
fn names_the_step_that_broke_the_tests_with_its_prompt_and_diff() {
    let project = Project::new("good");
    // The breaking step also writes a secret into a file: the diff shows it masked.
    let token = fake_token();
    let mut steps = notes_steps(8);
    steps[8 - 3].push((
        "settings.txt".to_owned(),
        Some(format!("github_token = \"{token}\"\n")),
    ));
    record(&project, SESSION, &steps);
    let (code, out, err) = project.bisect(&["--test", TEST], &[]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(err.is_empty(), "{err}");

    let lines: Vec<&str> = out.lines().collect();
    let cache = project.cache_folder();
    assert_eq!(
        lines[0],
        format!(
            "Bisect data (test copies and run logs): {}",
            cache.display()
        )
    );
    assert_eq!(
        lines[1],
        "Bisecting session 5f3c2a1e: 10 steps changed files after step 1 (the good end), up to step 12 \
         (the bad end)."
    );
    assert!(lines[2].starts_with("Testing step 1, the good end (about 4 runs left)... passed in "));
    assert!(lines[3].starts_with(
        "Testing step 12, the bad end (about 3 runs left)... failed (exit code 1) in "
    ));
    // 11 entries: the two ends and about log2(10) runs between them.
    assert!(runs(&out) <= 6, "{out}");
    assert!(
        out.contains(
            "Step 8 broke the tests: they pass at step 7 and fail at step 8 (exit code 1). Found in "
        ),
        "{out}"
    );
    assert!(out.contains("Session 5f3c2a1e, step 8 of 13"), "{out}");
    assert!(
        out.contains("Prompt (step 2):\n  Tidy the notes and keep the checks green."),
        "{out}"
    );
    assert!(out.contains("Command:\n  ./edit.sh 8"), "{out}");
    assert!(
        out.contains(
            "3 files changed:\n  modified  notes.txt\n  added     settings.txt\n  modified  status.txt"
        ),
        "{out}"
    );
    assert!(out.contains("-good\n+bad"), "{out}");
    assert!(!out.contains(&token), "{out}");
    assert!(out.contains("+github_token = \"[REDACTED:"), "{out}");
    // The cache folder holds the bisect data; the project's event log says where it is.
    assert!(cache.join("copy-0").join("status.txt").is_file());
    assert!(cache.join("logs").join("run-1.log").is_file());
}

#[test]
fn a_good_end_that_fails_shows_the_end_of_its_output_masked_and_prefixed() {
    let project = Project::new("bad");
    record(&project, SESSION, &notes_steps(0));
    let token = fake_token();
    let printed = project.outside("printed.txt");
    fs::write(
        &printed,
        format!(
            "running 1 test\n\u{1b}[31mGITHUB_TOKEN={token}\u{1b}[0m\nerror: Step 3 broke the tests\n\
             \u{1b}]0;title\u{7}FAILED\n"
        ),
    )
    .unwrap();
    let (code, out, _) = project.bisect(&["--test", SCRIPT_TEST], &[("PRINT_FILE", &printed)]);
    assert_eq!(code, Some(1), "{out}");
    assert!(!out.contains(&token), "{out}");
    assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
    let log = project.cache_folder().join("logs").join("run-1.log");
    let heading = format!("Last 4 lines of the test output, from {}:", log.display());
    let tail = out.split_once(&heading).expect(&out).1;
    let tail: Vec<&str> = tail.lines().skip(1).collect();
    assert_eq!(tail[0], "  | running 1 test");
    assert!(
        tail[1].starts_with("  | GITHUB_TOKEN=") && tail[1].contains("REDACTED"),
        "{out}"
    );
    // A line of the test that looks like Yalper's own stays inside the frame.
    assert_eq!(tail[2], "  | error: Step 3 broke the tests");
    assert!(
        tail[3].starts_with("  | ") && tail[3].ends_with("FAILED"),
        "{out}"
    );
    assert!(
        tail[4].starts_with("The good end, step 1, already fails (exit code 1)"),
        "{out}"
    );
}

#[test]
fn a_bad_end_that_passes_or_nothing_to_test_names_no_step() {
    let project = Project::new("good");
    record(&project, SESSION, &notes_steps(0));
    let (code, out, _) = project.bisect(&["--test", TEST], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(runs(&out), 2, "{out}");
    assert!(
        out.contains("Last 1 line of the test output, from "),
        "{out}"
    );
    assert!(
        out.contains("The bad end, step 12, passes: the tests do not fail there"),
        "{out}"
    );

    // A session whose steps change no file.
    let quiet = Project::new("good");
    quiet.record(SESSION, "Just look around.", &[&[], &[]]);
    let (code, out, _) = quiet.bisect(&["--test", TEST], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        out,
        "Nothing to test: the code at step 5 of session 5f3c2a1e is the same as at step 1, the good end.\n"
    );
    // Nothing to test: the cache folder was not even created.
    assert!(fs::read_dir(quiet.cache.path()).unwrap().next().is_none());
}

#[test]
fn steps_that_cannot_be_tested_give_a_range() {
    let project = Project::new("good");
    let mut steps = notes_steps(8);
    steps[7 - 3].push(("skip.txt".to_owned(), Some("x\n".to_owned())));
    steps[9 - 3].push(("skip.txt".to_owned(), None));
    record(&project, SESSION, &steps);
    let (code, out, _) = project.bisect(&["--test", SCRIPT_TEST], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "One of steps 7 to 9 broke the tests, but some of them could not be tested: they pass at step \
             6 and fail at step 9 (exit code 1)."
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "  step 7: could not be tested: the test command exited with 125 (cannot test)\n  step 8: could \
             not be tested: the test command exited with 125 (cannot test)\n  step 9: fails (exit code 1)\n"
        ),
        "{out}"
    );
}

#[test]
fn a_run_over_the_time_limit_counts_as_a_failure() {
    let project = Project::new("good");
    let mut steps = notes_steps(0);
    steps[8 - 3].push(("hang.txt".to_owned(), Some("x\n".to_owned())));
    record(&project, SESSION, &steps);
    let started = Instant::now();
    let (code, out, _) = project.bisect(&["--test", SCRIPT_TEST, "--timeout", "2s"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("Testing step 12, the bad end (about 3 runs left)... timed out after 2 s"),
        "{out}"
    );
    assert!(
        out.contains(
            "Step 8 broke the tests: they pass at step 7 and fail at step 8 (timed out after 2 s)."
        ),
        "{out}"
    );
    assert!(out.contains("reached the time limit. A run over the time limit counts as a failure"));
    // Each hanging run was stopped at its limit, not after the 30 s of the command.
    assert!(started.elapsed() < Duration::from_secs(25), "{out}");
}

#[test]
fn retries_rule_out_a_flaky_step() {
    let project = Project::new("good");
    let mut steps = notes_steps(8);
    steps[5 - 3].push(("flaky.txt".to_owned(), Some("x\n".to_owned())));
    steps[8 - 3].push(("flaky.txt".to_owned(), None));
    record(&project, SESSION, &steps);
    let marker = project.outside("failed-once");

    // Without retries, the first run of a flaky step fails and that step is named.
    let (code, out, _) = project.bisect(&["--test", SCRIPT_TEST], &[("FLAKY_MARKER", &marker)]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("Step 7 broke the tests"), "{out}");

    fs::remove_file(&marker).unwrap();
    let (code, out, _) = project.bisect(
        &["--test", SCRIPT_TEST, "--retries", "2"],
        &[("FLAKY_MARKER", &marker)],
    );
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("Testing step 7 again (retry 1 of 2)... passed in "),
        "{out}"
    );
    assert!(out.contains("Step 8 broke the tests"), "{out}");
    assert!(
        out.contains("Flaky: step 7 failed, then passed when run again, so it counts as passing."),
        "{out}"
    );

    let output = project.yalper(&["bisect", "--test", TEST, "--retries", "11"], &[]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("11 is not in 0..=10"), "{stderr}");
    let output = project.yalper(&["bisect", "--test", TEST, "--timeout", "5h"], &[]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn good_bad_and_session_choose_what_is_bisected() {
    let project = Project::new("good");
    // The earlier session breaks the tests at step 6 and fixes them at step 9.
    let mut earlier = notes_steps(6);
    earlier[9 - 3].push(("status.txt".to_owned(), Some("good\n".to_owned())));
    record(&project, EARLIER_SESSION, &earlier);
    record(&project, SESSION, &notes_steps(5));

    let (code, out, _) = project.bisect(&["--test", TEST], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("Bisecting session 5f3c2a1e"), "{out}");
    assert!(out.contains("Step 5 broke the tests"), "{out}");

    let (code, out, _) = project.bisect(&["--test", TEST, "--session", "0a9b"], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The bad end, step 12, passes"), "{out}");

    let (code, out, _) = project.bisect(&["--test", TEST, "--session", "0a9b", "--bad", "8"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("up to step 8 (the bad end)"), "{out}");
    assert!(out.contains("Step 6 broke the tests"), "{out}");
    assert!(out.contains("Session 0a9b8c7d, step 6 of 13"), "{out}");

    let (code, out, _) = project.bisect(
        &[
            "--test",
            TEST,
            "--session",
            "0a9b",
            "--good",
            "7",
            "--bad",
            "13",
        ],
        &[],
    );
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("Testing step 7, the good end"), "{out}");
    assert!(out.contains("The good end, step 7, already fails"), "{out}");

    let (code, _, err) = project.bisect(&["--test", TEST, "--good", "9", "--bad", "4"], &[]);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("the good end (step 9) must come before the bad end (step 4)"),
        "{err}"
    );
}

/// Waits until `path` exists, for at most 60 s.
fn wait_for(path: &Path, child: &mut Child) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "the bisect ended early"
        );
        assert!(started.elapsed() < Duration::from_secs(60), "never started");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A bisect running in the background, held by its test command until released. Dropping it releases and stops
/// it, so a failed assertion never leaves processes running.
struct Held {
    child: Child,
    release: PathBuf,
}

impl Held {
    /// Releases the test command and waits for the bisect to end.
    fn finish(&mut self) -> ExitStatus {
        fs::write(&self.release, "").unwrap();
        self.child.wait().unwrap()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, "");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_second_bisect_and_a_purge_are_refused_while_one_runs() {
    let project = Project::new("good");
    record(&project, SESSION, &notes_steps(8));
    let hold = project.outside.path();
    let child = project
        .command(&["bisect", "--test", SCRIPT_TEST], &[("HOLD", hold)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut running = Held {
        child,
        release: hold.join("release"),
    };
    wait_for(&hold.join("started"), &mut running.child);

    let second = project.yalper(&["bisect", "--test", TEST], &[]);
    assert_eq!(second.status.code(), Some(1));
    let stderr = String::from_utf8(second.stderr).unwrap();
    assert!(
        stderr.starts_with("error: a bisect of this project is running: it holds "),
        "{stderr}"
    );
    assert!(
        stderr.contains("Only one bisect of a project runs at a time"),
        "{stderr}"
    );

    let before = files(&project.root);
    let purge = project.yalper(&["uninstall", "--purge"], &[]);
    assert_eq!(purge.status.code(), Some(1));
    let stderr = String::from_utf8(purge.stderr).unwrap();
    assert!(stderr.contains("Wait for that bisect to end"), "{stderr}");
    assert!(stderr.contains("Nothing was changed."), "{stderr}");
    assert_eq!(files(&project.root), before);
    let cache = project.cache_folder();
    assert!(cache.join("copy-0").is_dir());

    let status = running.finish();
    // Once released, the runs go on to the status check and the bisect ends as usual.
    assert_eq!(status.code(), Some(0));

    let purge = project.yalper(&["uninstall", "--purge"], &[]);
    assert!(purge.status.success(), "{purge:?}");
    assert!(!cache.exists());
    assert!(!project.root.join(".yalper").exists());
}

/// The test of the env file tests: it fails without `ready` in `.env`, and when `.env` holds `dirty`, which it
/// then appends to it, so a run that finds the `.env` an earlier run changed fails; then it passes while
/// `status.txt` says `good`.
const ENV_TEST: &str = if cfg!(windows) {
    r".\envcheck.cmd"
} else {
    "sh envcheck.sh"
};

fn env_script() -> (&'static str, String) {
    if cfg!(windows) {
        let lines = [
            "@echo off",
            "findstr /c:ready .env >nul || exit /b 1",
            "findstr /c:dirty .env >nul && exit /b 1",
            "echo dirty>>.env",
            "findstr /c:good status.txt >nul",
            "exit /b %errorlevel%",
        ];
        ("envcheck.cmd", lines.join("\r\n") + "\r\n")
    } else {
        let lines = [
            "grep -q ready .env || exit 1",
            "if grep -q dirty .env; then exit 1; fi",
            "echo dirty >> .env",
            "grep -q good status.txt",
        ];
        ("envcheck.sh", lines.join("\n") + "\n")
    }
}

impl Project {
    /// Checks that no file of the cache folder is named one of `names` or holds `secret`.
    fn assert_nothing_copied_left(&self, names: &[&str], secret: &str) {
        for (path, bytes) in files(self.cache.path()) {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(
                !names.contains(&name.as_ref()),
                "{} is left",
                path.display()
            );
            assert!(
                !contains(&bytes, secret),
                "{} holds the secret",
                path.display()
            );
        }
    }
}

fn contains(bytes: &[u8], text: &str) -> bool {
    bytes
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

#[test]
fn env_files_are_copied_only_when_asked_remembered_and_never_recorded() {
    let project = Project::new("good");
    let (name, content) = env_script();
    fs::write(project.root.join(name), content).unwrap();
    fs::write(project.root.join(".gitignore"), ".env\n").unwrap();
    // No secret keyword in its key and a low-entropy value, so redaction would leave it as it is: redaction
    // cannot be what keeps it out of the recordings.
    let secret = "plainnote".repeat(4);
    fs::write(
        project.root.join(".env"),
        format!("READY=ready\nREADY_NOTE={secret}\n"),
    )
    .unwrap();
    assert_eq!(
        yalper::redact::redact_str(&format!("READY_NOTE={secret}")),
        None
    );
    record(&project, SESSION, &notes_steps(8));

    // Without --with-env the test copies have no `.env`.
    let (code, out, _) = project.bisect(&["--test", ENV_TEST], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The good end, step 1, already fails"), "{out}");
    assert!(!out.contains("Note: the tests used"), "{out}");

    let (code, out, _) = project.bisect(&["--test", ENV_TEST, "--with-env"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "Copied into the test copies until the bisect ends: env files (1 found). Remembered for this \
             project; --no-env turns it off.\n"
        ),
        "{out}"
    );
    // Every run after the first found the `.env` of the project again, not the one the run before changed.
    assert!(runs(&out) >= 4, "{out}");
    assert!(out.contains("Step 8 broke the tests"), "{out}");
    assert!(
        out.contains(
            "Note: the tests used your current env files, not the ones from the time of each step."
        ),
        "{out}"
    );
    project.assert_nothing_copied_left(&[".env"], &secret);

    // Remembered for the next bisect, until --no-env.
    let (code, out, _) = project.bisect(&["--test", ENV_TEST], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("Step 8 broke the tests"), "{out}");
    let (code, out, _) = project.bisect(&["--test", ENV_TEST, "--no-env"], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The good end, step 1, already fails"), "{out}");
    let (code, _, _) = project.bisect(&["--test", ENV_TEST], &[]);
    assert_eq!(code, Some(1));
    project.assert_nothing_copied_left(&[".env"], &secret);

    // The secret is nowhere in the recordings: not in the database or its WAL, nor in any object of the snapshot
    // store, read uncompressed through git.
    let yalper = project.root.join(".yalper");
    let mut recorded: Vec<u8> = files(&yalper).into_values().flatten().collect();
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(yalper.join("snapshots.git"))
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    recorded.extend(output.stdout);
    assert!(
        contains(&recorded, "notes.txt"),
        "the search sees the recordings"
    );
    assert!(!contains(&recorded, &secret), "the secret reached .yalper/");
}

#[test]
fn with_paths_are_copied_remembered_refused_and_forgotten() {
    let project = Project::new("good");
    fs::write(project.root.join(".gitignore"), "local/\n").unwrap();
    fs::create_dir(project.root.join("local")).unwrap();
    fs::write(project.root.join("local").join("settings.txt"), "ready\n").unwrap();
    record(&project, SESSION, &notes_steps(8));
    let test = if cfg!(windows) {
        r"findstr /c:ready local\settings.txt >nul && findstr /c:good status.txt"
    } else {
        "grep -q ready local/settings.txt && grep good status.txt"
    };

    let (code, out, _) = project.bisect(&["--test", test], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The good end, step 1, already fails"), "{out}");

    let (code, out, _) = project.bisect(&["--test", test, "--with", "local"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "Copied into the test copies until the bisect ends: local. Remembered for this project; \
             --no-with turns it off.\n"
        ),
        "{out}"
    );
    assert!(out.contains("Step 8 broke the tests"), "{out}");
    assert!(
        out.contains(
            "Note: the tests used the current files given with --with, not the ones from the time of each step."
        ),
        "{out}"
    );
    project.assert_nothing_copied_left(&["settings.txt"], "ready");

    let (code, out, _) = project.bisect(&["--test", test], &[]);
    assert_eq!(code, Some(0), "{out}");

    for (path, why) in [
        (
            "README.md",
            "error: --with README.md: the project's ignore rules do not ignore it, so the snapshots already \
             hold it.",
        ),
        (
            "../elsewhere.txt",
            "error: --with ../elsewhere.txt: it is outside the project.",
        ),
    ] {
        let (code, out, err) = project.bisect(&["--test", test, "--with", path], &[]);
        assert_eq!(code, Some(1), "{out}{err}");
        assert!(out.is_empty(), "{out}");
        assert!(err.starts_with(why), "{err}");
    }

    let (code, out, _) = project.bisect(&["--test", test, "--no-with"], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The good end, step 1, already fails"), "{out}");
    project.assert_nothing_copied_left(&["settings.txt"], "ready");
}

#[test]
fn an_ignored_cargo_config_given_with_with_is_used_by_cargo() {
    let project = Project::new("good");
    fs::write(project.root.join(".gitignore"), ".cargo/\n").unwrap();
    fs::create_dir(project.root.join(".cargo")).unwrap();
    fs::write(
        project.root.join(".cargo").join("config.toml"),
        "[alias]\nyalper-probe = \"version\"\n",
    )
    .unwrap();
    record(&project, SESSION, &notes_steps(8));
    // The cargo that builds these tests; the alias exists only where cargo finds the config.
    let test = format!("\"{}\" yalper-probe && {TEST}", env!("CARGO"));

    let (code, out, _) = project.bisect(&["--test", &test], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The good end, step 1, already fails"), "{out}");
    let (code, out, _) = project.bisect(&["--test", &test, "--with", ".cargo/config.toml"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("Step 8 broke the tests"), "{out}");
    project.assert_nothing_copied_left(&["config.toml"], "yalper-probe");
}

/// The lines of `out` that show a run.
fn testing_lines(out: &str) -> Vec<&str> {
    out.lines()
        .filter(|line| line.starts_with("Testing "))
        .collect()
}

#[test]
fn changes_made_between_sessions_are_an_entry_of_their_own() {
    let project = Project::new("good");
    record(&project, EARLIER_SESSION, &notes_steps(0));
    // Between the first two sessions, outside any of them, an edit breaks the tests.
    project.edit(&[("status.txt", Some("bad\n"))]);
    record(&project, SESSION, &notes_steps(0));
    record(&project, LATER_SESSION, &later_steps());

    // The latest session alone starts broken.
    let (code, out, _) = project.bisect(&["--test", TEST], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The good end, step 1, already fails"), "{out}");

    let (code, out, err) = project.bisect(&["--test", TEST, "--all"], &[]);
    assert_eq!(code, Some(0), "{out}{err}");
    // Ten tool steps in each session, and the start of the second one.
    assert!(
        out.contains(
            "Bisecting 3 sessions, in the order their snapshots were taken: 31 steps changed files after the \
             code before step 1 of session 0a9b8c7d (the good end), up to step 12 of session 7e6d5c4b (the bad \
             end).\n"
        ),
        "{out}"
    );
    let testing = testing_lines(&out);
    assert!(
        testing[0].starts_with(
            "Testing the code before step 1 of session 0a9b8c7d, the good end (about 6 runs left)... passed"
        ),
        "{out}"
    );
    assert!(
        testing[1].starts_with("Testing step 12 of session 7e6d5c4b, the bad end"),
        "{out}"
    );
    assert!(testing.iter().all(|line| line.contains(" of session ")));
    assert!(
        out.contains("Testing step 1 of session 5f3c2a1e, changes made between sessions (about "),
        "{out}"
    );
    assert!(
        out.contains(
            "The changes made between sessions, before session 5f3c2a1e (recorded at its step 1), broke the \
             tests: they pass at step 12 of session 0a9b8c7d and fail at step 1 of session 5f3c2a1e (exit code \
             1). Found in "
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "No agent step made them: they were made outside the recorded sessions (by you or another \
             program). The diff below shows them.\n"
        ),
        "{out}"
    );
    assert!(runs(&out) <= 8, "{out}");
    // The diff from the last code that passed: the edit made between the sessions.
    assert!(out.contains("Session 5f3c2a1e, step 1 of 13"), "{out}");
    assert!(
        out.contains("Session started (startup), model claude-opus-5"),
        "{out}"
    );
    assert!(
        out.contains("1 file changed:\n  modified  status.txt"),
        "{out}"
    );
    assert!(out.contains("-good\n+bad"), "{out}");
}

#[test]
fn changes_made_before_a_prompt_are_an_entry_of_their_own() {
    let project = Project::new("good");
    project.start(SESSION);
    project.prompt(SESSION, "Add a note.");
    project.tool(SESSION, 3, &[("notes.txt", Some("one\n"))]);
    project.tool(SESSION, 4, &[("notes.txt", Some("two\n"))]);
    project.stop(SESSION);
    // Between two turns of the agent.
    project.edit(&[("status.txt", Some("bad\n"))]);
    project.prompt(SESSION, "Add another note.");
    project.tool(SESSION, 7, &[("notes.txt", Some("three\n"))]);
    project.stop(SESSION);

    let (code, out, _) = project.bisect(&["--test", TEST], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("Testing step 6, changes made before this prompt (about "),
        "{out}"
    );
    assert!(
        out.contains(
            "The changes made before the prompt at step 6 broke the tests: they pass at step 4 and fail at \
             step 6 (exit code 1)."
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "No tool step of the agent made them: they were made between two of its turns"
        ),
        "{out}"
    );
    assert!(
        out.contains("Prompt:\n  Add another note.\n\n1 file changed:\n  modified  status.txt"),
        "{out}"
    );
}

#[test]
fn a_break_in_an_earlier_session_is_found_with_all() {
    let project = Project::new("good");
    record(&project, EARLIER_SESSION, &notes_steps(6));
    record(&project, SESSION, &notes_steps(0));
    record(&project, LATER_SESSION, &later_steps());

    let (code, out, _) = project.bisect(&["--test", TEST], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("The good end, step 1, already fails"), "{out}");

    let (code, out, _) = project.bisect(&["--test", TEST, "--all"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "Step 6 of session 0a9b8c7d broke the tests: they pass at step 5 of session 0a9b8c7d and fail at \
             step 6 of session 0a9b8c7d (exit code 1). Found in "
        ),
        "{out}"
    );
    assert!(out.contains("Session 0a9b8c7d, step 6 of 13"), "{out}");
    assert!(out.contains("Command:\n  ./edit.sh 6"), "{out}");

    // Ends in other sessions, as <session>:<step>; a plain step is of the latest session.
    let (code, out, _) = project.bisect(
        &[
            "--test", TEST, "--all", "--good", "0a9b:4", "--bad", "5f3c:3",
        ],
        &[],
    );
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "Bisecting 2 sessions, in the order their snapshots were taken: 9 steps changed files after step 4 \
             of session 0a9b8c7d (the good end), up to step 3 of session 5f3c2a1e (the bad end)."
        ),
        "{out}"
    );
    assert!(
        out.contains("Step 6 of session 0a9b8c7d broke the tests"),
        "{out}"
    );
    let (code, out, _) = project.bisect(&["--test", TEST, "--all", "--good", "4"], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("after step 4 of session 7e6d5c4b (the good end)"),
        "{out}"
    );
    assert!(
        out.contains(
            "The good end, step 4 of session 7e6d5c4b, already fails (exit code 1): the tests fail before any \
             step that is bisected, so none of them can be named. Pick a step where the tests pass with --good \
             <session>:<step>, or check the test command."
        ),
        "{out}"
    );

    // Without --all, a step of another session is refused.
    let (code, out, err) = project.bisect(&["--test", TEST, "--good", "0a9b:4"], &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.contains(
            "error: --good names a step of session 0a9b8c7d, but the bisect is of session 7e6d5c4b. Add --all \
             to bisect across sessions."
        ),
        "{err}"
    );
    let (code, _, err) = project.bisect(&["--test", TEST, "--all", "--bad", "0a9b:14"], &[]);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("session 0a9b8c7d has no step 14 (--bad): its steps are 1 to 13."),
        "{err}"
    );

    // --session names one session, --all and --since several; --since and --good both pick the good end.
    for args in [
        &["--all", "--session", "0a9b"][..],
        &["--since", "2001-01-01", "--session", "0a9b"],
        &["--since", "2001-01-01", "--good", "3"],
        &["--since", "01/01/2001"],
        &["--good", "0a9b:"],
    ] {
        let mut all = vec!["bisect", "--test", TEST];
        all.extend_from_slice(args);
        let output = project.yalper(&all, &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

#[test]
fn since_leaves_out_the_older_sessions() {
    let project = Project::new("good");
    // The older session breaks the tests at step 6 and fixes them at step 9.
    let mut earlier = notes_steps(6);
    earlier[9 - 3].push(("status.txt".to_owned(), Some("good\n".to_owned())));
    record(&project, EARLIER_SESSION, &earlier);
    record(&project, SESSION, &notes_steps(5));
    project.move_to_2000(EARLIER_SESSION);

    let (code, out, _) = project.bisect(&["--test", TEST, "--since", "2001-01-01"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "Bisecting 1 session since 2001-01-01 00:00, in the order their snapshots were taken: 10 steps \
             changed files after the code before step 1 of session 5f3c2a1e (the good end), up to step 12 of \
             session 5f3c2a1e (the bad end)."
        ),
        "{out}"
    );
    assert!(
        testing_lines(&out)
            .iter()
            .all(|line| !line.contains("0a9b8c7d")),
        "{out}"
    );
    assert!(
        out.contains("Step 5 of session 5f3c2a1e broke the tests"),
        "{out}"
    );

    // With --all, the older session's steps are entries too.
    let (_, out, _) = project.bisect(&["--test", TEST, "--all"], &[]);
    assert!(
        out.contains("20 steps changed files after the code before step 1 of session 0a9b8c7d"),
        "{out}"
    );

    let (code, _, err) = project.bisect(&["--test", TEST, "--since", "2999-01-01 10:30"], &[]);
    assert_eq!(code, Some(1));
    assert!(
        err.contains(
            "error: cannot bisect the sessions since 2999-01-01 10:30: no snapshot was taken since then."
        ),
        "{err}"
    );
}

#[test]
fn overlapping_sessions_interleave_in_snapshot_order() {
    let project = Project::new("good");
    let (a, b) = (EARLIER_SESSION, SESSION);
    project.start(a);
    project.start(b);
    project.prompt(a, "Work on a.");
    project.prompt(b, "Work on b.");
    project.tool(a, 3, &[("a.txt", Some("1\n"))]);
    project.tool(b, 3, &[("b.txt", Some("1\n"))]);
    project.tool(a, 4, &[("a.txt", Some("2\n"))]);
    // Session b breaks the tests while session a is still working.
    project.tool(b, 4, &[("status.txt", Some("bad\n"))]);
    project.tool(a, 5, &[("a.txt", Some("3\n"))]);
    project.tool(b, 5, &[("b.txt", Some("2\n"))]);
    project.stop(a);
    project.stop(b);

    let (code, out, _) = project.bisect(&["--test", TEST, "--all"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "Bisecting 2 sessions, in the order their snapshots were taken: 6 steps changed files after the \
             code before step 1 of session 0a9b8c7d (the good end), up to step 5 of session 5f3c2a1e (the bad \
             end)."
        ),
        "{out}"
    );
    // The step before it in snapshot order is a step of the other session.
    assert!(
        out.contains(
            "Step 4 of session 5f3c2a1e broke the tests: they pass at step 4 of session 0a9b8c7d and fail at \
             step 4 of session 5f3c2a1e (exit code 1)."
        ),
        "{out}"
    );
    assert!(
        out.contains("1 file changed:\n  modified  status.txt"),
        "{out}"
    );

    // Session a alone: its step 5 holds the change of session b made just before.
    let (code, out, _) = project.bisect(&["--test", TEST, "--session", "0a9b"], &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(
            "Step 5 broke the tests: they pass at step 4 and fail at step 5 (exit code 1)."
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "Note: other sessions changed files between step 4 and step 5, so the code tested at step 5 also \
             holds their changes (and so does the diff below): session 5f3c2a1e (step 4). `yalper bisect --all` \
             tests their steps too."
        ),
        "{out}"
    );
    assert!(
        out.contains("2 files changed:\n  modified  a.txt\n  modified  status.txt"),
        "{out}"
    );
}

/// A fake GitHub token, built at run time so that no token-shaped literal is in the repository.
fn fake_token() -> String {
    let body: String = "q7W2e9R4t1Y6u3I8o5P0".chars().cycle().take(36).collect();
    format!("ghp_{body}")
}
