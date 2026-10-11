//! `yalper bisect` end to end on real projects, through the real binary: a Node project (npm, one package from
//! the npm registry, `node --test`), a Python project (a virtual environment with the package installed
//! editable from a src layout, `unittest`) and a Rust project (`cargo test`, no registry crate).
//!
//! Each sample is a git project whose dependencies are installed in it, set up with `yalper init`, with a
//! recorded session of about 20 steps (the hook payloads Claude Code sends, while the files on disk change)
//! that changes the lockfile once and plants the break at a known step. Then `yalper bisect --test "<command>"`
//! must name that step, twice: the first bisect installs, once, the dependencies of the steps from before the
//! lockfile changed, and the second installs nothing. The project, `.git` and its installed dependencies
//! included, must be the same after each bisect, and `yalper uninstall --purge` must remove the project's cache
//! folder.
//!
//! They download packages from the npm registry and PyPI and build Rust code, so a plain `cargo test` skips
//! them. CI runs them in the "Bisect end-to-end" job:
//!
//! ```text
//! cargo test --test bisect_end_to_end -- --ignored --nocapture
//! ```
//!
//! A sample whose tools are missing is skipped, unless `YALPER_REQUIRE_TOOLS=1` (set by CI). Each sample prints
//! the output of its bisects and a line starting with `bisect_end_to_end` with its counts and run times, which
//! CI adds to the job summary.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{EXE, git};
use serde_json::{Value, json};

const SESSION: &str = "5f3c2a1e-8b4d-4e6f-9a7b-1c2d3e4f5a6b";

/// The step of the first tool call: step 1 starts the session, step 2 is the prompt.
const FIRST_TOOL_STEP: usize = 3;

/// What `yalper bisect` says after a run in which the Windows linker could not write a file another process
/// held (LNK1104, LNK1168, LNK1201).
const LINKER_FILE_HELD: &str = "the linker could not write a file held by Windows";

/// The first of `names` that runs with `--version`. Without one the sample is skipped, unless
/// `YALPER_REQUIRE_TOOLS=1`.
fn tool(names: &[&'static str]) -> Option<&'static str> {
    let found = names.iter().copied().find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    });
    assert!(
        found.is_some() || std::env::var("YALPER_REQUIRE_TOOLS").as_deref() != Ok("1"),
        "{names:?} is required (YALPER_REQUIRE_TOOLS=1) but does not run"
    );
    found
}

/// What the agent does in one tool step.
enum Action {
    /// Writes a file of the project with the Write tool.
    Write(&'static str, String),
    /// Runs a program in the project with the Bash tool.
    Bash(String, Vec<String>),
}

impl Action {
    fn bash(program: &str, args: &[&str]) -> Self {
        Self::Bash(
            program.to_owned(),
            args.iter().map(|&arg| arg.to_owned()).collect(),
        )
    }
}

/// A sample project in a temporary folder, with Yalper's cache folder and temporary folder outside it.
struct Sample {
    name: &'static str,
    root: PathBuf,
    /// `YALPER_CACHE_DIR` of every `yalper` command.
    cache: tempfile::TempDir,
    /// The temporary folder of every `yalper` command.
    temp: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

impl Sample {
    /// A project folder holding `files`.
    fn new(name: &'static str, files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // The path the hooks and the agent see: on macOS the temporary folder is reached through `/var`.
        #[cfg(unix)]
        let root = fs::canonicalize(dir.path()).unwrap().join(name);
        #[cfg(not(unix))]
        let root = dir.path().join(name);
        for (path, content) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        Self {
            name,
            root,
            cache: tempfile::tempdir().unwrap(),
            temp: tempfile::tempdir().unwrap(),
            _dir: dir,
        }
    }

    /// Runs `program` with `args` in the project, which must succeed.
    fn run(&self, program: &str, args: &[String]) {
        let output = Command::new(program)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Commits every file to a new git repository, then sets Yalper up with `yalper init`.
    fn commit_and_init(&self) {
        git(&self.root, &["init", "--quiet"]);
        git(&self.root, &["add", "--all"]);
        git(
            &self.root,
            &["commit", "--quiet", "--message", "Initial commit"],
        );
        let output = self.yalper(&["init"]);
        assert!(output.status.success(), "{output:?}");
    }

    /// Records a session: its start, `prompt`, one tool step per action from step 3, and the end of the turn.
    fn record(&self, prompt: &str, actions: &[Action]) {
        self.hook(
            "SessionStart",
            json!({"source": "startup", "model": "claude-opus-5"}),
        );
        self.hook("UserPromptSubmit", json!({"prompt": prompt}));
        for (index, action) in actions.iter().enumerate() {
            let id = format!("toolu_{}", FIRST_TOOL_STEP + index);
            let fields = match action {
                Action::Write(path, content) => {
                    // With the separators of the platform, as Claude Code gives it.
                    let file = path
                        .split('/')
                        .fold(self.root.clone(), |file, name| file.join(name));
                    let kind = if file.exists() { "update" } else { "create" };
                    fs::create_dir_all(file.parent().unwrap()).unwrap();
                    fs::write(&file, content).unwrap();
                    json!({
                        "tool_name": "Write",
                        "tool_input": {"file_path": file, "content": content},
                        "tool_response": {"filePath": file, "type": kind},
                        "tool_use_id": id,
                        "duration_ms": 3
                    })
                }
                Action::Bash(program, args) => {
                    self.run(program, args);
                    let shown = program.trim_end_matches(".cmd");
                    json!({
                        "tool_name": "Bash",
                        "tool_input": {"command": format!("{shown} {}", args.join(" ")), "description": "Run"},
                        "tool_response": {"stdout": "", "stderr": "", "interrupted": false, "isImage": false},
                        "tool_use_id": id,
                        "duration_ms": 1200
                    })
                }
            };
            self.hook("PostToolUse", fields);
        }
        self.hook(
            "Stop",
            json!({"last_assistant_message": "Done.", "stop_hook_active": false}),
        );
    }

    /// Runs `yalper hook` with the payload of `event`, as Claude Code does.
    fn hook(&self, event: &str, fields: Value) {
        let mut payload = json!({
            "session_id": SESSION,
            "transcript_path": format!("/home/dev/.claude/projects/demo/{SESSION}.jsonl"),
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
        assert!(
            output.status.success() && output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
    }

    fn yalper(&self, args: &[&str]) -> Output {
        Command::new(EXE)
            .args(args)
            .current_dir(&self.root)
            .env("YALPER_CACHE_DIR", self.cache.path())
            .env("TMPDIR", self.temp.path())
            .env("TMP", self.temp.path())
            .env("TEMP", self.temp.path())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs `yalper bisect --test <test>`, checking that the project did not change.
    fn bisect(&self, test: &str) -> Bisected {
        let before = state(&self.root);
        let started = Instant::now();
        let output = self.yalper(&["bisect", "--test", test]);
        let elapsed = started.elapsed();
        let out = String::from_utf8(output.stdout).unwrap();
        let err = String::from_utf8(output.stderr).unwrap();
        println!("$ yalper bisect --test \"{test}\"\n{out}{err}");
        assert_same(&before, &state(&self.root));
        assert_eq!(output.status.code(), Some(0), "{out}{err}");
        assert!(err.is_empty(), "{err}");
        Bisected { out, elapsed }
    }

    /// Bisects the recorded session twice with `test`, checks both results and what each installed, then purges
    /// Yalper from the project. `broken` is the step that plants the break.
    fn check(&self, test: &str, broken: usize) {
        let first = self.bisect(test);
        let second = self.bisect(test);
        for bisect in [&first, &second] {
            let out = &bisect.out;
            assert!(
                out.contains(&format!("Step {broken} broke the tests: ")),
                "{out}"
            );
            // A cold run of a new copy (the whole project compiled) never times out and names a wrong step.
            assert!(!out.contains("timed out"), "{out}");
            assert!(!out.contains("reached the time limit"), "{out}");
            let most = ceil_log2(bisect.entries()) + 2;
            assert!(bisect.runs().len() <= most, "{out}");
        }
        // The steps from before the lockfile changed need their own install, once; the others reuse the project's
        // installed dependencies.
        assert_eq!(first.installs().len(), 1, "{}", first.out);
        assert!(second.installs().is_empty(), "{}", second.out);
        // The bad end is the first run in the copy that reuses the project's dependencies, made by this bisect.
        let bad_end = first.runs()[1];
        assert!(bad_end.contains(", the bad end "), "{bad_end}");
        assert!(
            bad_end.contains("(first run in a new test copy: time limit raised to "),
            "{bad_end}"
        );
        assert!(!second.out.contains("first run in a new test copy"));

        let purge = self.yalper(&["uninstall", "--purge"]);
        assert!(purge.status.success(), "{purge:?}");
        assert!(fs::read_dir(self.cache.path()).unwrap().next().is_none());
        assert!(!self.root.join(".yalper").exists());

        println!(
            "bisect_end_to_end {name} ({os}): step {broken} named among {entries} entries. First bisect: {first}. \
             Second bisect: {second}.",
            name = self.name,
            os = std::env::consts::OS,
            entries = first.entries(),
            first = first.summary(),
            second = second.summary(),
        );
    }
}

/// The output of a bisect that named a step.
struct Bisected {
    out: String,
    elapsed: Duration,
}

impl Bisected {
    /// The entries bisected: the good end and the steps that changed files after it.
    fn entries(&self) -> usize {
        let (_, after) = self
            .out
            .split_once(&format!("session {}: ", &SESSION[..8]))
            .expect(&self.out);
        let candidates: usize = after.split(' ').next().unwrap().parse().unwrap();
        candidates + 1
    }

    /// The line of each run, reruns after the Windows linker error left out.
    fn runs(&self) -> Vec<&str> {
        self.out
            .lines()
            .filter(|line| line.starts_with("Testing ") && !line.contains(" again ("))
            .collect()
    }

    /// The time of each install.
    fn installs(&self) -> Vec<f64> {
        self.out
            .split("installed its dependencies in ")
            .skip(1)
            .map(|rest| seconds(rest).unwrap())
            .collect()
    }

    /// `7 runs (at most 7) in 6.5 s, installs: [3.8 s], run times in order: [0.4, 0.3, ...] s, linker file
    /// held: 0`: the runs counted, the time of the whole bisect, of each install and of every run (reruns
    /// included), and the runs that failed because the Windows linker could not write a file.
    fn summary(&self) -> String {
        let times: Vec<String> = self
            .out
            .lines()
            .filter(|line| line.starts_with("Testing "))
            .filter_map(run_time)
            .map(|time| format!("{time:.1}"))
            .collect();
        let installs: Vec<String> = self
            .installs()
            .iter()
            .map(|time| format!("{time:.1} s"))
            .collect();
        format!(
            "{runs} runs (at most {most}) in {elapsed:.1} s, installs: [{installs}], run times in order: [{times}] s, \
             linker file held: {held}",
            runs = self.runs().len(),
            most = ceil_log2(self.entries()) + 2,
            elapsed = self.elapsed.as_secs_f64(),
            installs = installs.join(", "),
            times = times.join(", "),
            held = self
                .out
                .matches(&format!("failed: {LINKER_FILE_HELD}"))
                .count(),
        )
    }
}

/// The seconds at the start of `text`, as in `4.2 s`.
fn seconds(text: &str) -> Option<f64> {
    let (number, _) = text.split_once(" s")?;
    number.parse().ok()
}

/// How long the run of a `Testing` line took: the last `in <seconds> s` of the line.
fn run_time(line: &str) -> Option<f64> {
    line.split(" in ").skip(1).filter_map(seconds).last()
}

fn ceil_log2(n: usize) -> usize {
    n.next_power_of_two().trailing_zeros() as usize
}

/// Every entry under `root`, links not followed, with its kind, and for files their size and a hash of their
/// content: what a bisect must leave as it is. The event log is left out (a bisect records in its `meta` table
/// where its data is), and so is SQLite's shared memory file.
fn state(root: &Path) -> BTreeMap<PathBuf, String> {
    let log = root.join(".yalper");
    let mut state = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let what = if metadata.is_symlink() {
                format!("link to {}", fs::read_link(&path).unwrap().display())
            } else if metadata.is_dir() {
                pending.push(path.clone());
                "folder".to_owned()
            } else {
                if dir == log
                    && path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("yalper.db")
                {
                    continue;
                }
                let mut hasher = DefaultHasher::new();
                hasher.write(&fs::read(&path).unwrap());
                format!("{} bytes, hash {:x}", metadata.len(), hasher.finish())
            };
            state.insert(path, what);
        }
    }
    state
}

/// Checks that `after` is `before`, naming the entries that differ.
fn assert_same(before: &BTreeMap<PathBuf, String>, after: &BTreeMap<PathBuf, String>) {
    let mut differ = Vec::new();
    for (path, what) in before {
        if after.get(path) != Some(what) {
            differ.push(format!(
                "{}: {what} -> {:?}",
                path.display(),
                after.get(path)
            ));
        }
    }
    for (path, what) in after {
        if !before.contains_key(path) {
            differ.push(format!("{}: created, {what}", path.display()));
        }
    }
    assert!(
        differ.is_empty(),
        "the project changed:\n{}",
        differ.join("\n")
    );
}

/// The steps from step 3 up to `last`: each writes the module `notes` with `note(step)`, except the steps of
/// `special`.
fn session(
    last: usize,
    notes: &'static str,
    note: impl Fn(usize) -> String,
    mut special: BTreeMap<usize, Action>,
) -> Vec<Action> {
    (FIRST_TOOL_STEP..=last)
        .map(|step| {
            special
                .remove(&step)
                .unwrap_or_else(|| Action::Write(notes, note(step)))
        })
        .collect()
}

const DURATION_JS: &str = "const ms = require('ms');\n\n\
    /** The number of seconds in a duration such as \"2m\". */\n\
    function seconds(text) {\n  return ms(text) / 1000;\n}\n\nmodule.exports = { seconds };\n";

#[test]
#[ignore = "installs a package from the npm registry; run by the Bisect end-to-end CI job"]
fn bisects_a_node_project() {
    let (Some(npm), Some(_)) = (tool(&["npm.cmd", "npm"]), tool(&["node"])) else {
        return;
    };
    let sample = Sample::new(
        "durations",
        &[
            (".gitignore", "node_modules/\n"),
            (
                "package.json",
                "{\n  \"name\": \"durations\",\n  \"version\": \"1.0.0\",\n  \"private\": true\n}\n",
            ),
            ("src/duration.js", DURATION_JS),
            ("src/notes.js", "module.exports = [];\n"),
            (
                "test/duration.test.js",
                "const test = require('node:test');\nconst assert = require('node:assert');\n\
                 const { seconds } = require('../src/duration');\n\n\
                 test('two minutes are 120 seconds', () => {\n  assert.strictEqual(seconds('2m'), 120);\n});\n",
            ),
        ],
    );
    let quiet = ["--no-audit", "--no-fund", "--loglevel=error"];
    let mut install = vec!["install", "--save-exact", "--package-lock-only"];
    install.extend(quiet);
    install.push("ms@2.1.2");
    sample.run(npm, &strings(&install));
    let mut ci = vec!["ci"];
    ci.extend(quiet);
    sample.run(npm, &strings(&ci));
    sample.commit_and_init();

    // Step 8 breaks the conversion; step 14 updates the package (package.json and package-lock.json). Steps 3 to
    // 13 then need their own install, which the search reuses for the steps it tests there.
    let mut upgrade = vec!["install", "--save-exact"];
    upgrade.extend(quiet);
    upgrade.push("ms@2.1.3");
    let special = BTreeMap::from([
        (14, Action::bash(npm, &upgrade)),
        (
            8,
            Action::Write(
                "src/duration.js",
                DURATION_JS.replace("ms(text) / 1000", "ms(text) / 100"),
            ),
        ),
    ]);
    let actions = session(
        22,
        "src/notes.js",
        |step| format!("module.exports = ['note {step}'];\n"),
        special,
    );
    sample.record(
        "Add notes and keep the duration helper up to date.",
        &actions,
    );
    sample.check("node --test", 8);
}

const TALLY_INIT: &str =
    "def total(values):\n    \"\"\"The sum of `values`.\"\"\"\n    return sum(values)\n";

#[test]
#[ignore = "installs packages from PyPI; run by the Bisect end-to-end CI job"]
fn bisects_a_python_project() {
    // The interpreter the install of a test copy starts.
    let Some(python) = tool(if cfg!(windows) {
        &["python"]
    } else {
        &["python3"]
    }) else {
        return;
    };
    let sample = Sample::new(
        "tally",
        &[
            (".gitignore", ".venv/\n__pycache__/\n*.egg-info/\nbuild/\n"),
            (
                "pyproject.toml",
                "[build-system]\nrequires = [\"setuptools>=64\"]\nbuild-backend = \"setuptools.build_meta\"\n\n\
                 [project]\nname = \"tally\"\nversion = \"0.1.0\"\n",
            ),
            ("requirements.txt", "-e .\n"),
            ("src/tally/__init__.py", TALLY_INIT),
            ("src/tally/notes.py", "NOTES = []\n"),
            (
                "tests/test_tally.py",
                "import unittest\n\nfrom tally import total\n\n\n\
                 class TotalTest(unittest.TestCase):\n    def test_total_adds_every_value(self):\n        \
                 self.assertEqual(total([1, 2, 3]), 6)\n",
            ),
        ],
    );
    sample.run(python, &strings(&["-m", "venv", ".venv"]));
    let venv_python = if cfg!(windows) {
        r".venv\Scripts\python.exe"
    } else {
        ".venv/bin/python"
    };
    // Absolute: `Command` resolves a relative program path differently on each platform.
    let venv_python_path = sample.root.join(venv_python).to_string_lossy().into_owned();
    let venv_python_path = venv_python_path.as_str();
    let pip = [
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        "--no-input",
        "--quiet",
    ];
    let mut editable = pip.to_vec();
    editable.extend(["-e", "."]);
    sample.run(venv_python_path, &strings(&editable));
    sample.commit_and_init();

    // Step 8 pins a new dependency in requirements.txt and step 9 installs it; step 17 breaks the sum. The steps
    // tested after step 8 reuse the project's environment, whose editable install points at the project's
    // (broken) code: they pass only if the tests import the code of the step, from the test copy.
    let mut requirements = pip.to_vec();
    requirements.extend(["-r", "requirements.txt"]);
    let special = BTreeMap::from([
        (
            17,
            Action::Write(
                "src/tally/__init__.py",
                TALLY_INIT.replace("sum(values)", "sum(values[1:])"),
            ),
        ),
        (
            8,
            Action::Write("requirements.txt", "-e .\nsix==1.16.0\n".to_owned()),
        ),
        (9, Action::bash(venv_python_path, &requirements)),
    ]);
    let actions = session(
        23,
        "src/tally/notes.py",
        |step| format!("NOTES = [\"note {step}\"]\n"),
        special,
    );
    sample.record("Add notes and keep the totals right.", &actions);
    sample.check(&format!("{venv_python} -m unittest discover -s tests"), 17);
}

const LIB_RS: &str = "pub mod notes;\n\n/// The sum of `values`.\npub fn total(values: &[i64]) -> i64 {\n    \
    values.iter().sum()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn total_adds_every_value() {\n        \
    assert_eq!(super::total(&[1, 2, 3]), 6);\n    }\n}\n";

const CARGO_TOML: &str = "[package]\nname = \"tally\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ndoctest = false\n";

#[test]
#[ignore = "builds Rust code in several folders; run by the Bisect end-to-end CI job"]
fn bisects_a_rust_project() {
    let Some(cargo) = tool(&["cargo"]) else {
        return;
    };
    let sample = Sample::new(
        "tally",
        &[
            (".gitignore", "/target\n"),
            ("Cargo.toml", CARGO_TOML),
            ("src/lib.rs", LIB_RS),
            ("src/notes.rs", "pub const NOTES: &[&str] = &[];\n"),
        ],
    );
    // Writes Cargo.lock and fills the project's target folder.
    sample.run(cargo, &strings(&["test", "--quiet"]));
    sample.commit_and_init();

    // Step 11 breaks the sum; step 16 bumps the version and step 17 builds, which updates Cargo.lock.
    let special = BTreeMap::from([
        (
            11,
            Action::Write(
                "src/lib.rs",
                LIB_RS.replace("values.iter().sum()", "values.iter().skip(1).sum()"),
            ),
        ),
        (
            16,
            Action::Write("Cargo.toml", CARGO_TOML.replace("0.1.0", "0.2.0")),
        ),
        (17, Action::bash(cargo, &["build", "--quiet"])),
    ]);
    let actions = session(
        23,
        "src/notes.rs",
        |step| format!("pub const NOTES: &[&str] = &[\"note {step}\"];\n"),
        special,
    );
    sample.record("Add notes and keep the totals right.", &actions);
    sample.check("cargo test", 11);
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|&arg| arg.to_owned()).collect()
}
