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
use std::process::{Command, Output, Stdio};

use common::{EXE, files, git};
use serde_json::{Value, json};

const SESSION: &str = "5f3c2a1e-8b4d-4e6f-9a7b-1c2d3e4f5a6b";

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
        self.hook(
            session,
            "SessionStart",
            json!({"source": "startup", "model": "claude-opus-5"}),
        );
        self.hook(session, "UserPromptSubmit", json!({"prompt": prompt}));
        for (index, edits) in steps.iter().enumerate() {
            for (path, content) in *edits {
                let path = self.root.join(path);
                match content {
                    Some(content) => fs::write(path, content).unwrap(),
                    None => fs::remove_file(path).unwrap(),
                }
            }
            let step = index + 3;
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
        self.hook(
            session,
            "Stop",
            json!({"last_assistant_message": "Done.", "stop_hook_active": false}),
        );
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
    record(&project, SESSION, &notes_steps(8));
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
        out.contains("2 files changed:\n  modified  notes.txt\n  modified  status.txt"),
        "{out}"
    );
    assert!(out.contains("-good\n+bad"), "{out}");
    // The cache folder holds the bisect data; the project's event log says where it is.
    assert!(cache.join("copy-0").join("status.txt").is_file());
    assert!(cache.join("logs").join("run-1.log").is_file());
}

#[test]
fn a_good_end_that_fails_shows_the_end_of_its_output_masked_and_prefixed() {
    let project = Project::new("bad");
    record(&project, SESSION, &notes_steps(0));
    let token = format!(
        "ghp_{}",
        "q7W2e9R4t1Y6u3I8o5P0"
            .chars()
            .cycle()
            .take(36)
            .collect::<String>()
    );
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
