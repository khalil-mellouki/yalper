//! Runs a test command in a test copy, with a time limit, a stop flag for Ctrl-C, and its output in a log file.
//!
//! - The shell is `/bin/sh -c "<command>"` on Unix and `cmd.exe /d /s /c "<command>"` on Windows (PowerShell
//!   takes about 0.9 s to start, and Windows PowerShell 5.1 has no `&&`).
//! - The command runs in its own process group (Unix) or job object (Windows), so that the whole tree of
//!   processes it starts can be killed: when the time limit is reached, when the stop flag is set, and as soon
//!   as its main process exits, so that whatever it left running (a server, a watcher) cannot leak into the
//!   next run. It is not in the terminal's foreground process group (Unix) and its process group is a new one
//!   (Windows, `CREATE_NEW_PROCESS_GROUP`): the terminal's Ctrl-C does not reach the tests, Yalper kills them
//!   itself (see [`STOP`]).
//! - Its stdin is empty, so test runners do not start a watch mode, and stdout and stderr both go to the log
//!   file through the same handle, in the order they were written: no pipes, no reader threads.
//! - Yalper waits on the main process itself, polling it every few milliseconds, with no extra thread.
//!
//! Known limit: on Unix, a process that moves itself to another process group or session (`setsid`, a
//! daemon) is not killed. On Windows a process cannot leave the job.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap};

use super::BISECT_DIR;
use super::copy::{open_private_dir, remove_entry};
use crate::safe_fs::{Access, OwnedDir};

/// Set by the first Ctrl-C once [`handle_ctrl_c`] is installed: the run going on is stopped, its processes
/// killed, and the bisect ends.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// The exit code of Yalper after Ctrl-C, like a shell's for a command stopped by SIGINT.
pub const STOPPED_EXIT_CODE: i32 = 130;

/// The exit code with which a test command says it cannot test the code it was given, as in `git bisect run`.
pub const SKIP_EXIT_CODE: i32 = 125;

/// The folder inside `.yalper/bisect/` that holds the output logs of the latest bisect.
const LOGS_DIR: &str = "logs";

/// The longest pause between two checks of the main process. Pauses start at 1 ms and double, so a command
/// that ends quickly is not kept waiting.
const MAX_PAUSE: Duration = Duration::from_millis(50);

/// How much of the end of a failed run's log is searched for a linker error (see [`Run::linker_file_held`]).
/// Cargo stops after the error, so it is near the end.
const LOG_TAIL_BYTES: u64 = 1 << 20;

/// Installs the Ctrl-C handler of a bisect: the first Ctrl-C sets [`STOP`], a second one makes Yalper exit
/// at once with [`STOPPED_EXIT_CODE`]. Install it once per process.
pub fn handle_ctrl_c() -> io::Result<()> {
    ctrlc::set_handler(|| {
        if pressed_before(&STOP) {
            std::process::exit(STOPPED_EXIT_CODE);
        }
    })
    .map_err(|error| io::Error::other(format!("cannot handle Ctrl-C: {error}")))
}

/// Sets `flag`, returning whether it was already set.
fn pressed_before(flag: &AtomicBool) -> bool {
    flag.swap(true, Ordering::SeqCst)
}

/// How a test run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Exit code 0.
    Passed,
    /// Any exit code other than 0 and 125, including codes above 127 (a crash), or a signal.
    Failed(Exit),
    /// Exit code 125: the command cannot test this code.
    Skipped,
    /// The time limit was reached; the processes were killed.
    TimedOut,
    /// The stop flag was set (Ctrl-C); the processes were killed.
    Stopped,
}

impl Outcome {
    /// A hint for a failure that most likely comes from the command itself rather than from the tests: exit
    /// code 126 (cannot run) or 127 (not found), as Unix shells use them. `cmd.exe` exits with 1 for a command
    /// it cannot find, and says so in the log.
    pub fn hint(&self) -> Option<&'static str> {
        match *self {
            Self::Failed(Exit::Code(126 | 127)) => Some(NOT_FOUND_HINT),
            _ => None,
        }
    }
}

const NOT_FOUND_HINT: &str = "the command was not found or cannot run";

/// How the main process of a failed run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Unix only: the signal that killed it.
    Signal(i32),
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            // Windows crash codes are NTSTATUS values, known by their hexadecimal form (0xC0000005).
            Self::Code(code) if cfg!(windows) && !(0..=255).contains(&code) => {
                write!(f, "exit code 0x{:08X}", code as u32)
            }
            Self::Code(code) => write!(f, "exit code {code}"),
            Self::Signal(signal) => write!(f, "signal {signal}"),
        }
    }
}

/// What [`run`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub outcome: Outcome,
    /// From the start of the command to its end, or to the time limit or the stop.
    pub elapsed: Duration,
    /// The run failed and its output shows that the MSVC linker could not open a program or library it had
    /// to write (`LNK1104` naming an `.exe`, `.dll` or `.pdb`). On Windows this happens when a test program
    /// that ran a moment before is still held by the system or a virus scanner: the failure then says nothing
    /// about the code, and the run is worth repeating or counting as "cannot test". A missing `.lib` (also
    /// `LNK1104`) is a real build failure and does not set it.
    pub linker_file_held: bool,
}

/// Runs `command` through the system shell in the folder `cwd`, with exactly the environment variables `env`,
/// its output in `log` (opened for reading and writing, as [`Logs::create`] does), and kills its whole
/// process tree when it ends, when `limit` is reached, or when `stop` is set.
///
/// Fails only when the command cannot be started or waited for; whatever was started is then killed.
pub fn run(
    command: &str,
    cwd: &Path,
    env: &[(OsString, OsString)],
    log: &File,
    limit: Option<Duration>,
    stop: &AtomicBool,
) -> io::Result<Run> {
    let mut shell = shell(command);
    shell
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let mut shell = CommandWrap::from(shell);
    #[cfg(unix)]
    shell.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    shell
        .wrap(process_wrap::std::JobObject)
        .wrap(process_wrap::std::CreationFlags(
            windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP,
        ));
    let started = Instant::now();
    let mut child = shell.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot start the test command: {error}"),
        )
    })?;
    let waited = wait(&mut *child, started, limit, stop);
    let elapsed = started.elapsed();
    let killed = kill(&mut *child);
    let outcome = waited?;
    killed?;
    let linker_file_held =
        matches!(outcome, Outcome::Failed(_)) && log_shows_linker_file_held(log)?;
    Ok(Run {
        outcome,
        elapsed,
        linker_file_held,
    })
}

/// The shell command that runs `command`.
fn shell(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut shell = Command::new("/bin/sh");
        shell.arg("-c").arg(command);
        shell
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `ComSpec` is the full path of cmd.exe: a bare `cmd.exe` would first be searched in the run's `PATH`,
        // which can name folders of the copy.
        let program = std::env::var_os("ComSpec")
            .filter(|path| Path::new(path).is_absolute())
            .unwrap_or_else(|| OsString::from("cmd.exe"));
        let mut shell = Command::new(program);
        // `/d`: no AutoRun command from the registry. `/s /c "<command>"`: cmd removes the outer quotes and
        // runs the rest exactly as typed, inner quotes included.
        shell.raw_arg(format!("/d /s /c \"{command}\""));
        shell
    }
}

/// Waits until the main process of `child` exits, `limit` is reached or `stop` is set.
///
/// It waits on the main process itself, not through `process-wrap`, whose wait also waits for the rest of the
/// group or job: on Windows that wait reads one message of the job, and can block forever once the polls read
/// them all.
fn wait(
    child: &mut dyn ChildWrapper,
    started: Instant,
    limit: Option<Duration>,
    stop: &AtomicBool,
) -> io::Result<Outcome> {
    let mut pause = Duration::from_millis(1);
    loop {
        if let Some(status) = child.inner_mut().try_wait()? {
            return Ok(outcome(status));
        }
        if stop.load(Ordering::SeqCst) {
            return Ok(Outcome::Stopped);
        }
        let elapsed = started.elapsed();
        if limit.is_some_and(|limit| elapsed >= limit) {
            return Ok(Outcome::TimedOut);
        }
        thread::sleep(limit.map_or(pause, |limit| pause.min(limit - elapsed)));
        pause = (pause * 2).min(MAX_PAUSE);
    }
}

/// Kills every process left in the group or job of `child`, and its main process if it still runs, then
/// waits for the main process.
fn kill(child: &mut dyn ChildWrapper) -> io::Result<()> {
    // Fails only when nothing is left to kill (Unix: no process in the group).
    let _ = child.start_kill();
    // The main process may have left the group (`exec setsid ...`). Once waited for, it is never signalled:
    // its process id could belong to another process by then.
    let main = child.inner_mut();
    if main.try_wait()?.is_none() {
        let _ = main.start_kill();
        main.wait()?;
    }
    Ok(())
}

fn outcome(status: ExitStatus) -> Outcome {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(windows)]
    let signal = None;
    match (status.code(), signal) {
        (Some(0), _) => Outcome::Passed,
        (Some(SKIP_EXIT_CODE), _) => Outcome::Skipped,
        (Some(code), _) => Outcome::Failed(Exit::Code(code)),
        (None, signal) => Outcome::Failed(Exit::Signal(signal.unwrap_or(0))),
    }
}

/// Whether the end of `log` shows the linker error of [`Run::linker_file_held`]. Leaves the log's position at
/// its end.
fn log_shows_linker_file_held(log: &File) -> io::Result<bool> {
    let mut log = log;
    let end = log.seek(SeekFrom::End(0))?;
    log.seek(SeekFrom::Start(end.saturating_sub(LOG_TAIL_BYTES)))?;
    let mut tail = Vec::new();
    log.take(LOG_TAIL_BYTES).read_to_end(&mut tail)?;
    log.seek(SeekFrom::End(0))?;
    Ok(shows_linker_file_held(&tail))
}

/// Whether a line of `output` is a `LNK1104` error naming an `.exe`, `.dll` or `.pdb` file, such as
/// `LINK : fatal error LNK1104: cannot open file 'C:\p\target\debug\deps\app-1a2b.exe'`.
fn shows_linker_file_held(output: &[u8]) -> bool {
    output.split(|&byte| byte == b'\n').any(|line| {
        let line = line.to_ascii_lowercase();
        contains(&line, b"lnk1104")
            && [&b".exe'"[..], b".dll'", b".pdb'"]
                .iter()
                .any(|end| contains(&line, end))
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The folder `.yalper/bisect/logs/`, which holds one output log per run of the latest bisect. Logs may hold
/// values the tests print (secrets from env files included): they stay inside `.yalper/`, private to their
/// owner on Unix, and each bisect removes the logs of the one before.
#[derive(Debug)]
pub struct Logs {
    dir: OwnedDir,
}

impl Logs {
    /// Opens the logs folder of the `.yalper/` folder `yalper_dir`, creating it and `.yalper/bisect/` if they
    /// are missing (real folders, 0700 on Unix), and removes everything in it, never following a link.
    pub fn open(yalper_dir: &OwnedDir) -> io::Result<Self> {
        let bisect = open_private_dir(&yalper_dir.path().join(BISECT_DIR))?;
        let dir = open_private_dir(&bisect.path().join(LOGS_DIR))?;
        for entry in fs::read_dir(dir.path())? {
            let path = entry?.path();
            remove_entry(&path, &fs::symlink_metadata(&path)?)?;
        }
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Creates the empty log `name`, a plain file name, opened for reading and writing (0600 on Unix). A link
    /// or anything but a regular file at that name is refused, never followed; a file is emptied and made
    /// private.
    pub fn create(&self, name: &str) -> io::Result<File> {
        let mut components = Path::new(name).components();
        let plain = matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(part)), None) if part == name
        );
        if !plain {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name:?} is not a plain file name"),
            ));
        }
        // A test run could have replaced the folder.
        self.dir.check_still_at_path()?;
        let log = self.dir.open_file(name, Access::ReadWrite)?;
        log.set_len(0)?;
        // A file that was already there would keep its mode otherwise.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            log.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::bisect::testing::{Project, state, write};

    /// `unix` on Unix, `windows` on Windows.
    fn by_os<'a>(unix: &'a str, windows: &'a str) -> &'a str {
        if cfg!(windows) { windows } else { unix }
    }

    /// A command that starts a second shell, the grandchild of the run, which writes `started` at once and
    /// `marker` about 3 s later.
    const GRANDCHILD: (&str, &str) = (
        "sh -c 'echo > started; sleep 3; echo late > marker'",
        "cmd /d /c \"echo.>started& ping -n 4 127.0.0.1 >nul& echo late>marker\"",
    );

    /// Long enough after the start of a run for the grandchild to have written its marker.
    const AFTER_MARKER: Duration = Duration::from_secs(5);

    fn environment() -> Vec<(OsString, OsString)> {
        std::env::vars_os().collect()
    }

    /// Runs `command` in `dir`: the result and the log.
    fn run_in(
        dir: &Path,
        command: &str,
        limit: Option<Duration>,
        stop: &AtomicBool,
    ) -> (Run, String) {
        let mut log = tempfile::tempfile().unwrap();
        let run = run(command, dir, &environment(), &log, limit, stop).unwrap();
        let mut output = String::new();
        log.rewind().unwrap();
        log.read_to_string(&mut output).unwrap();
        (run, output)
    }

    fn outcome_of(command: &str) -> Outcome {
        let dir = tempfile::tempdir().unwrap();
        run_in(dir.path(), command, None, &AtomicBool::new(false))
            .0
            .outcome
    }

    /// Waits until `since` is [`AFTER_MARKER`] old, then checks that the grandchild started and never wrote
    /// its marker.
    fn assert_grandchild_killed(dir: &Path, since: Instant) {
        assert!(dir.join("started").exists(), "the grandchild never started");
        thread::sleep(AFTER_MARKER.saturating_sub(since.elapsed()));
        assert!(
            !dir.join("marker").exists(),
            "the grandchild was not killed"
        );
    }

    #[test]
    fn exit_codes_follow_git_bisect() {
        assert_eq!(outcome_of("exit 0"), Outcome::Passed);
        assert_eq!(outcome_of("exit 1"), Outcome::Failed(Exit::Code(1)));
        assert_eq!(outcome_of("exit 125"), Outcome::Skipped);
        // Above 127: a failure, not an abort as in `git bisect run`.
        assert_eq!(outcome_of("exit 200"), Outcome::Failed(Exit::Code(200)));
        assert_eq!(Exit::Code(200).to_string(), "exit code 200");
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_crash_code_is_a_failure_named_in_hexadecimal() {
        let outcome = outcome_of("exit -1073741819");
        assert_eq!(outcome, Outcome::Failed(Exit::Code(-1073741819)));
        let Outcome::Failed(exit) = outcome else {
            unreachable!()
        };
        assert_eq!(exit.to_string(), "exit code 0xC0000005");
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_is_a_failure() {
        let outcome = outcome_of("kill -9 $$");
        assert_eq!(outcome, Outcome::Failed(Exit::Signal(9)));
        assert_eq!(Exit::Signal(9).to_string(), "signal 9");
    }

    #[test]
    fn a_missing_command_gets_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let (run, output) = run_in(
            dir.path(),
            "yalper-test-no-such-command",
            None,
            &AtomicBool::new(false),
        );
        assert!(output.contains("yalper-test-no-such-command"), "{output}");
        if cfg!(windows) {
            assert_eq!(run.outcome, Outcome::Failed(Exit::Code(1)));
        } else {
            assert_eq!(run.outcome, Outcome::Failed(Exit::Code(127)));
            assert_eq!(run.outcome.hint(), Some(NOT_FOUND_HINT));
        }
        assert_eq!(
            Outcome::Failed(Exit::Code(126)).hint(),
            Some(NOT_FOUND_HINT)
        );
        assert_eq!(Outcome::Failed(Exit::Code(1)).hint(), None);
        assert_eq!(Outcome::Passed.hint(), None);
    }

    #[test]
    fn and_and_quotes_reach_the_shell_as_typed() {
        let dir = tempfile::tempdir().unwrap();
        let (run, output) = run_in(
            dir.path(),
            "echo \"a b\" && echo c>\"out file.txt\" && exit 3",
            None,
            &AtomicBool::new(false),
        );
        assert_eq!(run.outcome, Outcome::Failed(Exit::Code(3)));
        assert_eq!(output.trim_end(), by_os("a b", "\"a b\""));
        let written = fs::read_to_string(dir.path().join("out file.txt")).unwrap();
        assert_eq!(written.trim_end(), "c");
    }

    #[test]
    fn both_streams_go_to_the_log_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (run, output) = run_in(
            dir.path(),
            by_os("echo out; echo err 1>&2", "echo out& echo err>&2"),
            None,
            &AtomicBool::new(false),
        );
        assert_eq!(run.outcome, Outcome::Passed);
        let lines: Vec<&str> = output.lines().map(str::trim_end).collect();
        assert_eq!(lines, ["out", "err"]);
    }

    #[test]
    fn the_run_gets_the_given_folder_and_environment_only() {
        let dir = tempfile::tempdir().unwrap();
        // A variable cargo sets for every test, left out (shells set some others, such as `HOME`, themselves).
        let left_out = "CARGO_MANIFEST_DIR";
        let mut env = environment();
        assert!(env.iter().any(|(name, _)| name == left_out));
        env.retain(|(name, _)| name != left_out);
        env.push(("YALPER_TEST_VALUE".into(), "given".into()));
        let mut log = tempfile::tempfile().unwrap();
        let command = by_os(
            "echo \"[$YALPER_TEST_VALUE][$CARGO_MANIFEST_DIR]\" > value.txt",
            "echo [%YALPER_TEST_VALUE%][%CARGO_MANIFEST_DIR%]>value.txt",
        );
        let run = run(
            command,
            dir.path(),
            &env,
            &log,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(run.outcome, Outcome::Passed);
        let value = fs::read_to_string(dir.path().join("value.txt")).unwrap();
        // `cmd.exe` leaves an unset variable as typed.
        assert_eq!(
            value.trim_end(),
            by_os("[given][]", "[given][%CARGO_MANIFEST_DIR%]")
        );
        let mut output = String::new();
        log.rewind().unwrap();
        log.read_to_string(&mut output).unwrap();
        assert_eq!(output, "");
    }

    #[test]
    fn stdin_is_empty() {
        // `sort` reads stdin to its end: with the terminal as stdin it would wait for the user.
        assert_eq!(outcome_of("sort"), Outcome::Passed);
    }

    #[test]
    fn a_run_over_the_limit_is_killed_with_its_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let limit = Duration::from_millis(1500);
        let started = Instant::now();
        let (run, _) = run_in(
            dir.path(),
            by_os(GRANDCHILD.0, GRANDCHILD.1),
            Some(limit),
            &AtomicBool::new(false),
        );
        assert_eq!(run.outcome, Outcome::TimedOut);
        assert!(run.elapsed >= limit, "{:?}", run.elapsed);
        assert!(
            run.elapsed < limit + Duration::from_secs(1),
            "{:?}",
            run.elapsed
        );
        assert_grandchild_killed(dir.path(), started);
    }

    #[test]
    fn a_process_left_running_after_the_main_one_exits_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        // The main process starts the grandchild in the background, waits for it to start, and exits.
        let command = by_os(
            "(echo > started; sleep 3; echo late > marker) & while [ ! -f started ]; do sleep 0.05; done",
            "start \"\" /b cmd /d /c \"echo.>started& ping -n 4 127.0.0.1 >nul& echo late>marker\"& for /l %i in (1,1,500) do @if not exist started ping -n 1 127.0.0.1 >nul",
        );
        let started = Instant::now();
        let (run, _) = run_in(dir.path(), command, None, &AtomicBool::new(false));
        assert_eq!(run.outcome, Outcome::Passed);
        assert_grandchild_killed(dir.path(), started);
    }

    #[test]
    fn the_stop_flag_kills_the_whole_tree_within_a_second() {
        let dir = tempfile::tempdir().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        // Sets the flag once the grandchild runs.
        let setter = {
            let stop = Arc::clone(&stop);
            let started = dir.path().join("started");
            thread::spawn(move || {
                while !started.exists() {
                    thread::sleep(Duration::from_millis(10));
                }
                stop.store(true, Ordering::SeqCst);
                Instant::now()
            })
        };
        let started = Instant::now();
        let (run, _) = run_in(dir.path(), by_os(GRANDCHILD.0, GRANDCHILD.1), None, &stop);
        let stopped_at = setter.join().unwrap();
        assert_eq!(run.outcome, Outcome::Stopped);
        assert!(stopped_at.elapsed() < Duration::from_secs(1));
        assert_grandchild_killed(dir.path(), started);
    }

    #[test]
    fn a_second_ctrl_c_exits_at_once() {
        let flag = AtomicBool::new(false);
        assert!(!pressed_before(&flag));
        assert!(flag.load(Ordering::SeqCst));
        assert!(pressed_before(&flag));
    }

    #[test]
    fn a_held_output_file_is_told_apart_from_a_missing_library() {
        let held = |output: &str| shows_linker_file_held(output.as_bytes());
        assert!(held(
            "   Compiling app v0.1.0\r\nLINK : fatal error LNK1104: cannot open file 'C:\\p\\target\\debug\\deps\\app-1a2b.exe'\r\n"
        ));
        assert!(held(
            "LINK : fatal error LNK1104: cannot open file 'C:\\t\\deps\\APP.PDB'"
        ));
        assert!(held(
            "LINK : fatal error LNK1104: cannot open file 'C:\\t\\deps\\app.dll'"
        ));
        assert!(!held(
            "LINK : fatal error LNK1104: cannot open file 'ws2_32.lib'"
        ));
        assert!(!held(
            "error: linking with `link.exe` failed: exit code: 1181"
        ));
        assert!(!held(
            "test app.exe ... FAILED\nnote: LNK1104 is mentioned here"
        ));
    }

    #[test]
    fn a_failed_run_reports_a_held_output_file() {
        let line = "LINK : fatal error LNK1104: cannot open file 'C:/t/app-1.exe'";
        let command = |exit: u8| {
            if cfg!(windows) {
                format!("echo {line}& exit {exit}")
            } else {
                format!("echo \"{line}\"; exit {exit}")
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let no_stop = AtomicBool::new(false);
        let (failed, output) = run_in(dir.path(), &command(1), None, &no_stop);
        assert_eq!(output.trim_end(), line);
        assert_eq!(failed.outcome, Outcome::Failed(Exit::Code(1)));
        assert!(failed.linker_file_held);
        // Only a failure is looked at.
        let (passed, _) = run_in(dir.path(), &command(0), None, &no_stop);
        assert_eq!(passed.outcome, Outcome::Passed);
        assert!(!passed.linker_file_held);
        let (other, _) = run_in(dir.path(), "exit 1", None, &no_stop);
        assert!(!other.linker_file_held);
    }

    #[test]
    fn a_command_that_cannot_start_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = tempfile::tempfile().unwrap();
        let missing = dir.path().join("missing");
        let error = run(
            "exit 0",
            &missing,
            &environment(),
            &log,
            None,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("cannot start the test command"),
            "{error}"
        );
    }

    #[test]
    fn logs_of_an_earlier_bisect_are_removed_never_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep.txt", "keep me");
        let before = state(canary.path());
        let logs = Logs::open(&project.yalper).unwrap();
        assert!(logs.path().ends_with(".yalper/bisect/logs"));
        write(logs.path(), "1.log", "old");
        write(logs.path(), "folder/2.log", "old");
        let linked = crate::bisect::link::make_link(canary.path(), &logs.path().join("link"));
        linked.unwrap();

        let logs = Logs::open(&project.yalper).unwrap();
        assert_eq!(fs::read_dir(logs.path()).unwrap().count(), 0);
        assert_eq!(state(canary.path()), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(logs.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn a_log_is_created_empty_and_only_as_a_plain_file() {
        let project = Project::new();
        let logs = Logs::open(&project.yalper).unwrap();
        write(logs.path(), "run-1.log", "from before");
        let mut log = logs.create("run-1.log").unwrap();
        let mut text = String::new();
        log.read_to_string(&mut text).unwrap();
        assert_eq!(text, "");
        for name in ["a/b.log", "..", "", "./x.log", "x/"] {
            let error = logs.create(name).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = log.metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);

            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("target.txt");
            fs::write(&target, "keep me").unwrap();
            std::os::unix::fs::symlink(&target, logs.path().join("link.log")).unwrap();
            assert!(logs.create("link.log").is_err());
            assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        }
    }

    #[test]
    fn a_run_writes_into_a_created_log() {
        let project = Project::new();
        let logs = Logs::open(&project.yalper).unwrap();
        let log = logs.create("step-1.log").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let run = run(
            "echo hello",
            dir.path(),
            &environment(),
            &log,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(run.outcome, Outcome::Passed);
        let output = fs::read_to_string(logs.path().join("step-1.log")).unwrap();
        assert_eq!(output.trim_end(), "hello");
    }
}
