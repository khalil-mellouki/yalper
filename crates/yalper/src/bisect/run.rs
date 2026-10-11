//! Runs a test command in a test copy, with a time limit, a stop flag for Ctrl-C, and its output in a log file.
//!
//! - The shell is `/bin/sh -c "<command>"` on Unix and `cmd.exe /d /s /c "<command>"` on Windows (PowerShell
//!   takes about 0.9 s to start, and Windows PowerShell 5.1 has no `&&`).
//! - The command runs in its own process group (Unix) or job object (Windows), so that the whole tree of
//!   processes it starts can be killed: when the time limit is reached, when the stop flag is set, when its
//!   output grows over [`MAX_LOG_BYTES`], and as soon as its main process exits, so that whatever it left
//!   running (a server, a watcher) cannot leak into the next run. On Windows the job is also closed, and so its
//!   processes killed, when Yalper exits in any way, a crash included.
//! - It is not in the terminal's foreground process group (Unix) and its process group is a new one (Windows,
//!   `CREATE_NEW_PROCESS_GROUP`): the terminal's Ctrl-C does not reach the tests, Yalper kills them itself (see
//!   [`handle_ctrl_c`]).
//! - Its stdin is empty, so test runners do not start a watch mode, and stdout and stderr both go to the log
//!   file through the same handle, in the order they were written: no pipes, no reader threads.
//! - Yalper waits on the main process itself, polling it every few milliseconds, with no extra thread.
//!
//! Known limits: on Unix, a process that moves itself to another process group or session (`setsid`, a
//! daemon) is not killed, and if Yalper itself is killed (SIGKILL) or crashes during a run, the run's
//! processes keep running. On Windows, processes that a system service starts for the tests (WMI, the Task
//! Scheduler, a COM server) are outside the job.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::CacheDir;
use super::copy::{open_private_dir, remove_entry};
use crate::safe_fs::{Access, OwnedDir};

/// Set by the first Ctrl-C once [`handle_ctrl_c`] is installed: the run going on is stopped, its processes
/// killed, and the bisect ends.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// The number of runs going on, for the Ctrl-C handler.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// The exit code of Yalper after Ctrl-C, like a shell's for a command stopped by SIGINT.
pub const STOPPED_EXIT_CODE: i32 = 130;

/// The exit code with which a test command says it cannot test the code it was given, as in `git bisect run`.
pub const SKIP_EXIT_CODE: i32 = 125;

/// The most output a run may write to its log before it is stopped (see [`Outcome::OutputTooLarge`]): far more
/// than any test suite prints, so only a run stuck printing in a loop reaches it.
pub const MAX_LOG_BYTES: u64 = 1 << 30;

/// The folder in the project's cache folder that holds the output logs of the latest bisect.
pub(super) const LOGS_DIR: &str = "logs";

/// The longest pause between two checks of the main process. Pauses start at 1 ms and double, so a command
/// that ends quickly is not kept waiting.
const MAX_PAUSE: Duration = Duration::from_millis(50);

/// How long a second Ctrl-C waits for a run going on to kill its processes before Yalper exits.
const SECOND_PRESS_WAIT: Duration = Duration::from_secs(1);

/// How much of the end of a failed run's log is searched for a linker error (see [`Run::linker_file_held`]).
/// Cargo stops after the error, so it is near the end.
const LOG_TAIL_BYTES: u64 = 1 << 20;

/// Installs the handler of Ctrl-C, and also of SIGTERM and SIGHUP on Unix and of closing the console on
/// Windows. The first one sets [`STOP`]: the run going on is stopped and its processes killed, and the bisect
/// ends. A second one makes Yalper exit at once with [`STOPPED_EXIT_CODE`], after giving a run going on up to
/// 1 s to kill its processes. Install it once per process.
pub fn handle_ctrl_c() -> io::Result<()> {
    ctrlc::set_handler(|| {
        if pressed_before(&STOP) {
            wait_for_runs(&RUNNING, SECOND_PRESS_WAIT);
            std::process::exit(STOPPED_EXIT_CODE);
        }
    })
    .map_err(|error| io::Error::other(format!("cannot handle Ctrl-C: {error}")))
}

/// Sets `flag`, returning whether it was already set.
fn pressed_before(flag: &AtomicBool) -> bool {
    flag.swap(true, Ordering::SeqCst)
}

/// Waits until no run is going on (each one saw [`STOP`] and killed its processes), or at most `bound`.
fn wait_for_runs(running: &AtomicUsize, bound: Duration) {
    let start = Instant::now();
    while running.load(Ordering::SeqCst) > 0 && start.elapsed() < bound {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Counts a run in [`RUNNING`] while it lives.
struct Running;

impl Running {
    fn start() -> Self {
        RUNNING.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    }
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
    /// The run wrote more than [`MAX_LOG_BYTES`] of output; the processes were killed. It tells nothing about
    /// the code: count it as "cannot test".
    OutputTooLarge,
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
            // Windows crash codes are NTSTATUS values, negative as an `i32` and known by their hexadecimal form
            // (0xC0000005).
            Self::Code(code) if cfg!(windows) && code < 0 => {
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
    /// The run failed and its output shows that the MSVC linker could not write a program or its debug
    /// information (`LNK1104`, `LNK1168` or `LNK1201` naming an `.exe`, `.dll` or `.pdb` file). On Windows this
    /// happens when a test program that ran a moment before is still held by the system or a virus scanner:
    /// the failure then says nothing about the code, and the run is worth repeating or counting as "cannot
    /// test". A missing `.lib` (also `LNK1104`) is a real build failure and does not set it.
    pub linker_file_held: bool,
}

/// Runs `command` through the system shell in the folder `cwd`, with exactly the environment variables `env`,
/// its output in `log` (opened for reading and writing, as [`Logs::create`] does), and kills its whole
/// process tree when it ends, when `limit` is reached, when `stop` is set, or when the log grows over
/// [`MAX_LOG_BYTES`].
///
/// Fails only when the command cannot be started, waited for or killed; whatever was started is then killed.
pub fn run(
    command: &str,
    cwd: &Path,
    env: &[(OsString, OsString)],
    log: &File,
    limit: Option<Duration>,
    stop: &AtomicBool,
) -> io::Result<Run> {
    run_capped(command, cwd, env, log, limit, stop, MAX_LOG_BYTES)
}

fn run_capped(
    command: &str,
    cwd: &Path,
    env: &[(OsString, OsString)],
    log: &File,
    limit: Option<Duration>,
    stop: &AtomicBool,
    max_log_bytes: u64,
) -> io::Result<Run> {
    let mut shell = shell(command)?;
    shell
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let _running = Running::start();
    let started = Instant::now();
    let mut tree = Tree::spawn(shell).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot start the test command: {error}"),
        )
    })?;
    let stopped = wait(&mut tree, started, limit, stop, log, max_log_bytes);
    let elapsed = started.elapsed();
    let status = tree.kill();
    let stopped = stopped?;
    let status = status?;
    let outcome = stopped.unwrap_or_else(|| outcome(status));
    // The log is only read to tell a held file apart: if that fails, the failure stands as it is.
    let linker_file_held =
        matches!(outcome, Outcome::Failed(_)) && log_shows_linker_file_held(log).unwrap_or(false);
    Ok(Run {
        outcome,
        elapsed,
        linker_file_held,
    })
}

/// The shell command that runs `command`.
fn shell(command: &str) -> io::Result<Command> {
    #[cfg(unix)]
    {
        let mut shell = Command::new("/bin/sh");
        shell.arg("-c").arg(command);
        Ok(shell)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let program = cmd_path(|name| std::env::var_os(name)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "cannot find cmd.exe: neither ComSpec nor SystemRoot is set to a full path",
            )
        })?;
        let mut shell = Command::new(program);
        // `/d`: no AutoRun command from the registry. `/s /c "<command>"`: cmd removes the outer quotes and
        // runs the rest exactly as typed, inner quotes included.
        shell.raw_arg(format!("/d /s /c \"{command}\""));
        Ok(shell)
    }
}

/// The full path of `cmd.exe`, from Yalper's own environment (`var`): `ComSpec`, else
/// `%SystemRoot%\System32\cmd.exe`. Never a bare `cmd.exe`, which would first be searched in the run's
/// `PATH`, where folders of the copy can be.
#[cfg(any(windows, test))]
fn cmd_path(var: impl Fn(&str) -> Option<OsString>) -> Option<std::path::PathBuf> {
    let full_path = |name| {
        var(name)
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    full_path("ComSpec")
        .or_else(|| full_path("SystemRoot").map(|root| root.join("System32").join("cmd.exe")))
}

/// Waits until the main process of `tree` exits (`None`), or until the run must be stopped: the reason.
fn wait(
    tree: &mut Tree,
    started: Instant,
    limit: Option<Duration>,
    stop: &AtomicBool,
    log: &File,
    max_log_bytes: u64,
) -> io::Result<Option<Outcome>> {
    let mut pause = Duration::from_millis(1);
    loop {
        if tree.main_exited()? {
            return Ok(None);
        }
        if stop.load(Ordering::SeqCst) {
            return Ok(Some(Outcome::Stopped));
        }
        if log.metadata()?.len() > max_log_bytes {
            return Ok(Some(Outcome::OutputTooLarge));
        }
        let elapsed = started.elapsed();
        if limit.is_some_and(|limit| elapsed >= limit) {
            return Ok(Some(Outcome::TimedOut));
        }
        thread::sleep(limit.map_or(pause, |limit| pause.min(limit - elapsed)));
        pause = (pause * 2).min(MAX_PAUSE);
    }
}

/// The processes of a run (Unix): the main process leads a process group of its own, which holds every
/// process it starts.
#[cfg(unix)]
struct Tree {
    main: std::process::Child,
    /// The main process id, which is also the group id.
    pid: rustix::process::Pid,
}

#[cfg(unix)]
impl Tree {
    fn spawn(mut command: Command) -> io::Result<Self> {
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let main = command.spawn()?;
        let pid = rustix::process::Pid::from_child(&main);
        Ok(Self { main, pid })
    }

    /// Whether the main process exited. It is not reaped (`WNOWAIT`): until [`kill`](Self::kill) reaps it, its
    /// process id, which is also the group id, cannot be given to another process, so killing the group can
    /// only reach processes of the run.
    fn main_exited(&mut self) -> io::Result<bool> {
        use rustix::process::{WaitId, WaitIdOptions, waitid};
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        Ok(waitid(WaitId::Pid(self.pid), options)?.is_some())
    }

    /// [`main_exited`](Self::main_exited) for [`kill`](Self::kill): if the check itself fails, the main process
    /// is killed directly and reaped before the error is returned, so a live main process is never left
    /// running.
    fn main_exited_or_reap(&mut self) -> io::Result<bool> {
        let result = self.main_exited();
        if result.is_err() {
            let _ = self.main.kill();
            let _ = self.main.wait();
        }
        result
    }

    /// Kills every process of the group, and the main process, then reaps the main process: its exit status.
    fn kill(&mut self) -> io::Result<ExitStatus> {
        use rustix::io::Errno;
        use rustix::process::{Signal, kill_process, kill_process_group};
        let group = match kill_process_group(self.pid, Signal::KILL) {
            // Nothing is left in the group.
            Err(Errno::SRCH) => Ok(()),
            // macOS refuses to signal a group holding a zombie, as the exited main process is until it is
            // reaped. The same error can also mean that live members remain which Yalper may not signal (for
            // example processes protected by a sandbox or a MAC policy): those survive silently, as they
            // would on any other path.
            #[cfg(target_vendor = "apple")]
            Err(Errno::PERM) if self.main_exited_or_reap()? => Ok(()),
            result => result,
        };
        // The main process may have moved itself to another group (`setpgid`). It is not reaped yet, so its
        // process id is still its own.
        if !self.main_exited_or_reap()? {
            match kill_process(self.pid, Signal::KILL) {
                Ok(()) => {}
                // It exited in the meantime (macOS refuses to signal a zombie).
                Err(_) if self.main_exited_or_reap()? => {}
                // Waiting for it would never end.
                Err(error) => return Err(error.into()),
            }
        }
        let status = self.main.wait()?;
        group?;
        Ok(status)
    }
}

/// The processes of a run (Windows): the main process, started suspended in a job object of `process-wrap`
/// (which then resumes it), so that every process it starts is in that job too.
#[cfg(windows)]
struct Tree {
    child: Box<dyn process_wrap::std::ChildWrapper>,
    /// A second job, holding the first. Its limit `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` kills every process of
    /// the run when its last handle is closed: when the run ends, or when Yalper exits in any way, a crash
    /// included.
    job: std::sync::Arc<win32job::Job>,
}

#[cfg(windows)]
impl Tree {
    fn spawn(command: Command) -> io::Result<Self> {
        use process_wrap::std::{CommandWrap, CreationFlags, JobObject};
        use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        let mut limits = win32job::ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close();
        let job = win32job::Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
        let job = std::sync::Arc::new(job);
        let mut command = CommandWrap::from(command);
        command
            .wrap(KillOnClose(std::sync::Arc::clone(&job)))
            .wrap(JobObject)
            .wrap(CreationFlags(CREATE_NEW_PROCESS_GROUP));
        let child = command.spawn()?;
        Ok(Self { child, job })
    }

    /// Whether the main process exited. It is checked itself, not through `process-wrap`, whose wait also
    /// reads the messages of its job and can block forever once they were all read.
    fn main_exited(&mut self) -> io::Result<bool> {
        Ok(self.child.inner_mut().try_wait()?.is_some())
    }

    /// Kills every process of the job, the main process included, then waits for the main process (its exit
    /// status) and, at most 2 s, until no process of the job is left, so that none still holds a file of the
    /// copy at the next switch. If this fails, dropping the tree closes the job, which kills its processes.
    fn kill(&mut self) -> io::Result<ExitStatus> {
        const BOUND: Duration = Duration::from_secs(2);
        self.child.start_kill()?;
        let status = self.child.inner_mut().wait()?;
        let start = Instant::now();
        while start.elapsed() < BOUND
            && !self
                .job
                .query_process_id_list()
                .is_ok_and(|ids| ids.is_empty())
        {
            thread::sleep(Duration::from_millis(5));
        }
        Ok(status)
    }
}

/// Puts the main process into the kill-on-close job right after it is created, while it is still suspended:
/// `post_spawn` runs before the `JobObject` wrapper resumes it.
#[cfg(windows)]
#[derive(Debug)]
struct KillOnClose(std::sync::Arc<win32job::Job>);

#[cfg(windows)]
impl process_wrap::std::CommandWrapper for KillOnClose {
    fn post_spawn(
        &mut self,
        _command: &mut Command,
        child: &mut std::process::Child,
        _core: &process_wrap::std::CommandWrap,
    ) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        if let Err(error) = self.0.assign_process(child.as_raw_handle() as isize) {
            // Never resumed, it would stay suspended forever.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(error));
        }
        Ok(())
    }
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

/// Whether a line of `output` is a `LNK1104`, `LNK1168` or `LNK1201` error naming an `.exe`, `.dll` or `.pdb`
/// file, such as `LINK : fatal error LNK1104: cannot open file 'C:\p\target\debug\deps\app-1a2b.exe'`.
fn shows_linker_file_held(output: &[u8]) -> bool {
    output.split(|&byte| byte == b'\n').any(|line| {
        let line = line.to_ascii_lowercase();
        let held_error = [&b"lnk1104"[..], b"lnk1168", b"lnk1201"]
            .iter()
            .any(|code| contains(&line, code));
        held_error
            && line
                .split(|&byte| byte.is_ascii_whitespace() || byte == b'\'' || byte == b'"')
                .any(|word| {
                    [&b".exe"[..], b".dll", b".pdb"]
                        .iter()
                        .any(|end| word.ends_with(end))
                })
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The folder `logs/` in the project's cache folder, which holds one output log per run of the latest bisect.
/// Logs may hold values the tests print (secrets from env files included): they stay in the project's cache
/// folder, outside the project, private to their owner on Unix, and each bisect removes the logs of the one
/// before.
#[derive(Debug)]
pub struct Logs {
    dir: OwnedDir,
}

impl Logs {
    /// Opens the logs folder of the project's cache folder `cache`, creating it if it is missing (a real folder,
    /// 0700 on Unix), and removes everything in it, never following a link.
    pub fn open(cache: &CacheDir) -> io::Result<Self> {
        cache.dir().check_still_at_path()?;
        let dir = open_private_dir(&cache.path().join(LOGS_DIR))?;
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
    use std::path::PathBuf;
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
        // Unix exit codes stop at 255; on Windows a positive code is shown as it is.
        if cfg!(windows) {
            assert_eq!(outcome_of("exit 300"), Outcome::Failed(Exit::Code(300)));
        }
        assert_eq!(Exit::Code(300).to_string(), "exit code 300");
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
        // From the start to the return, killing included.
        let returned_after = started.elapsed();
        assert_eq!(run.outcome, Outcome::TimedOut);
        assert!(run.elapsed >= limit, "{:?}", run.elapsed);
        assert!(
            returned_after < limit + Duration::from_secs(1),
            "{returned_after:?}"
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
        // Sets the flag once the grandchild runs. Without the grandchild, it fails after 10 s; the run then ends
        // by itself.
        let setter = {
            let stop = Arc::clone(&stop);
            let started = dir.path().join("started");
            let deadline = Instant::now() + Duration::from_secs(10);
            thread::spawn(move || {
                while !started.exists() {
                    assert!(Instant::now() < deadline, "the grandchild never started");
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
    fn only_the_second_press_is_seen_as_repeated() {
        let flag = AtomicBool::new(false);
        assert!(!pressed_before(&flag));
        assert!(flag.load(Ordering::SeqCst));
        assert!(pressed_before(&flag));
    }

    #[test]
    fn a_second_ctrl_c_gives_a_run_up_to_its_bound_to_kill_its_processes() {
        // No run going on: no wait.
        let started = Instant::now();
        wait_for_runs(&AtomicUsize::new(0), Duration::from_secs(10));
        assert!(started.elapsed() < Duration::from_secs(1));

        // A run that ends after 100 ms.
        let running = Arc::new(AtomicUsize::new(1));
        // Taken before the ender starts its sleep, so the wait can never look shorter than 100 ms.
        let started = Instant::now();
        let ender = {
            let running = Arc::clone(&running);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                running.store(0, Ordering::SeqCst);
            })
        };
        wait_for_runs(&running, Duration::from_secs(10));
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(5));
        ender.join().unwrap();

        // A run that does not end: the bound.
        let started = Instant::now();
        wait_for_runs(&AtomicUsize::new(1), Duration::from_millis(200));
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn a_run_printing_without_end_is_stopped_at_the_output_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = tempfile::tempfile().unwrap();
        let command = by_os(
            "while true; do echo 0123456789; done",
            "for /l %i in (1,0,2) do @echo 0123456789",
        );
        let run = run_capped(
            command,
            dir.path(),
            &environment(),
            &log,
            Some(Duration::from_secs(30)),
            &AtomicBool::new(false),
            10_000,
        )
        .unwrap();
        assert_eq!(run.outcome, Outcome::OutputTooLarge);
        assert!(run.elapsed < Duration::from_secs(10), "{:?}", run.elapsed);
        let mut output = String::new();
        log.rewind().unwrap();
        log.read_to_string(&mut output).unwrap();
        assert!(output.len() > 10_000);
        assert!(output.starts_with("0123456789"));
    }

    #[test]
    fn cmd_exe_is_always_named_by_its_full_path() {
        fn lookup(vars: Vec<(&'static str, PathBuf)>) -> impl Fn(&str) -> Option<OsString> {
            move |name| {
                vars.iter()
                    .find(|(var, _)| *var == name)
                    .map(|(_, path)| path.clone().into_os_string())
            }
        }
        // Full paths on this system.
        let comspec = std::env::temp_dir().join("cmd.exe");
        let root = std::env::temp_dir().join("Windows");
        let both = lookup(vec![
            ("ComSpec", comspec.clone()),
            ("SystemRoot", root.clone()),
        ]);
        assert_eq!(cmd_path(both), Some(comspec));
        let relative = lookup(vec![
            ("ComSpec", "cmd.exe".into()),
            ("SystemRoot", root.clone()),
        ]);
        assert_eq!(
            cmd_path(relative),
            Some(root.join("System32").join("cmd.exe"))
        );
        assert_eq!(
            cmd_path(lookup(vec![("SystemRoot", "Windows".into())])),
            None
        );
        assert_eq!(cmd_path(lookup(Vec::new())), None);
    }

    /// Yalper exiting in any way, a crash included, closes the second job, which kills the whole tree.
    #[cfg(windows)]
    #[test]
    fn closing_the_job_kills_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = shell(GRANDCHILD.1).unwrap();
        command
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = Instant::now();
        let tree = Tree::spawn(command).unwrap();
        let deadline = started + Duration::from_secs(10);
        while !dir.path().join("started").exists() {
            assert!(Instant::now() < deadline, "the grandchild never started");
            thread::sleep(Duration::from_millis(10));
        }
        // Dropped without being killed: only the job's limit ends the processes.
        drop(tree);
        assert_grandchild_killed(dir.path(), started);
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
        assert!(held(
            "LINK : fatal error LNK1168: cannot open C:\\p\\target\\debug\\deps\\app-1a2b.exe for writing"
        ));
        assert!(held(
            "LINK : fatal error LNK1201: error writing to program database 'C:\\p\\target\\debug\\deps\\app.pdb'; check for insufficient disk space"
        ));
        assert!(!held(
            "LINK : fatal error LNK1104: cannot open file 'ws2_32.lib'"
        ));
        assert!(!held(
            "LINK : fatal error LNK1181: cannot open input file 'app.exe'"
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
        let logs = Logs::open(&project.cache).unwrap();
        assert_eq!(logs.path().parent(), Some(project.cache.path()));
        write(logs.path(), "1.log", "old");
        write(logs.path(), "folder/2.log", "old");
        let linked = crate::bisect::link::make_link(canary.path(), &logs.path().join("link"));
        linked.unwrap();

        let logs = Logs::open(&project.cache).unwrap();
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
        let logs = Logs::open(&project.cache).unwrap();
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
        let logs = Logs::open(&project.cache).unwrap();
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
