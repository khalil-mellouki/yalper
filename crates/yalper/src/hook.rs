//! `yalper hook`: the command Claude Code runs on every agent step.
//!
//! Yalper must never change how the agent behaves. Claude Code reads a hook's exit code and output: exit 2
//! blocks the prompt or tool, any other nonzero code shows a "hook error" notice, and stdout on some events
//! is added to the model's context. So this command always exits 0, never writes to stdout or stderr, and
//! appends every error or panic as one line to `.yalper/errors.log` instead.

mod input;

use std::any::Any;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::safe_fs::{Access, OwnedDir};

pub use input::{HookEvent, HookInput, InputError};

/// The folder holding a project's recordings.
pub const YALPER_DIR: &str = ".yalper";

/// The error log inside [`YALPER_DIR`].
pub const ERRORS_LOG: &str = "errors.log";

/// When appending a line would make `errors.log` larger than this, the file is emptied first.
pub const ERRORS_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Payloads larger than this are not parsed (the rest of stdin is drained and discarded).
pub const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// When set, every hook call takes a real snapshot. A test entry point for the latency benchmark until
/// M1-T06 records every step (and checks that `yalper init` created the `.yalper/` it writes to). Only in
/// builds with the `bench-snapshot` feature, which release builds never enable.
#[cfg(feature = "bench-snapshot")]
pub const BENCH_SNAPSHOT_ENV: &str = "YALPER_BENCH_SNAPSHOT";

const MAX_MESSAGE_CHARS: usize = 2000;
const MAX_EVENT_CHARS: usize = 64;

/// The panic message and location, saved by the panic hook because `catch_unwind` only returns the payload.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Replaces the process-wide panic hook with one that prints nothing and keeps the message for
/// [`run`] to log. Only the `yalper` binary calls this, so tests and other callers keep their own hook.
pub fn silence_panics() {
    panic::set_hook(Box::new(|info| {
        let message = panic_text(info.payload());
        let text = match info.location() {
            Some(location) => format!("panic at {location}: {message}"),
            None => format!("panic: {message}"),
        };
        if let Ok(mut slot) = LAST_PANIC.lock() {
            *slot = Some(text);
        }
    }));
}

/// Handles one hook call: reads the payload from `stdin` and records it. Never panics and never prints
/// (once [`silence_panics`] has been called).
pub fn run(stdin: &mut dyn Read) {
    let mut call = Call::default();
    let error = match panic::catch_unwind(AssertUnwindSafe(|| handle(stdin, &mut call))) {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error,
        Err(payload) => LAST_PANIC
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or_else(|| format!("panic: {}", panic_text(payload.as_ref()))),
    };
    if let Some(dir) = &call.yalper_dir {
        // Nowhere is left to report a failure to write the error log, so it is ignored.
        let _ = append_error(dir, call.event.as_deref(), &error, ERRORS_LOG_MAX_BYTES);
    }
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// What is known about the current call, kept outside `catch_unwind` so errors can still be logged.
#[derive(Default)]
struct Call {
    yalper_dir: Option<OwnedDir>,
    event: Option<String>,
}

fn handle(stdin: &mut dyn Read, call: &mut Call) -> Result<(), String> {
    let payload = read_limited(stdin, MAX_PAYLOAD_BYTES).and_then(|bytes| {
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| format!("invalid hook input: {error}"))
    });

    let starts: Vec<PathBuf> = match env::var_os("CLAUDE_PROJECT_DIR").filter(|d| !d.is_empty()) {
        Some(project_dir) => vec![PathBuf::from(project_dir)],
        None => {
            let payload_cwd = payload
                .as_ref()
                .ok()
                .and_then(|value| value.get("cwd"))
                .and_then(Value::as_str)
                .map(PathBuf::from);
            payload_cwd
                .into_iter()
                .chain(env::current_dir().ok())
                .collect()
        }
    };
    // A project without `.yalper/` is not recorded, and there is nowhere to log to.
    let Some(dir) = find_yalper_dir(starts) else {
        return Ok(());
    };
    call.yalper_dir = Some(dir);

    let payload = payload?;
    call.event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    HookInput::from_value(payload).map_err(|error| error.to_string())?;

    #[cfg(feature = "bench-snapshot")]
    if let Some(dir) = &call.yalper_dir
        && env::var_os(BENCH_SNAPSHOT_ENV).is_some()
    {
        take_snapshot(dir, call.event.as_deref())?;
    }

    #[cfg(debug_assertions)]
    if env::var_os("YALPER_TEST_PANIC").is_some() {
        panic!("forced by YALPER_TEST_PANIC");
    }

    Ok(())
}

/// Takes a snapshot of the project the way recording a step will: open the event log, take the writer lock,
/// snapshot, and log one line if files could not be read or stored.
#[cfg(feature = "bench-snapshot")]
fn take_snapshot(dir: &OwnedDir, event: Option<&str>) -> Result<(), String> {
    use crate::store::{LOCK_TIMEOUT, Store, WriterLock};
    let store = Store::open(dir).map_err(|error| error.to_string())?;
    let lock = WriterLock::acquire(dir, LOCK_TIMEOUT).map_err(|error| error.to_string())?;
    let snapshot =
        crate::snapshot::snapshot(dir, &store, &lock).map_err(|error| error.to_string())?;
    if let Some(problems) = snapshot.problems() {
        let _ = append_error(dir, event, &problems, ERRORS_LOG_MAX_BYTES);
    }
    Ok(())
}

/// Reads `reader` to the end, but keeps at most `limit` bytes. A longer input is drained (so the writer
/// never sees a broken pipe) and reported as an error.
fn read_limited(reader: &mut dyn Read, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    Read::take(&mut *reader, limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read hook input: {error}"))?;
    if bytes.len() as u64 > limit {
        let _ = io::copy(reader, &mut io::sink());
        return Err(format!("hook input too large: more than {limit} bytes"));
    }
    Ok(bytes)
}

/// Returns the project's `.yalper` directory, trying each start directory in turn.
///
/// From each start, the search walks up only as far as the nearest git root (the first directory that
/// contains `.git`), so a `.yalper` in a parent folder or in another project is never used. Outside a git
/// repository nothing is found. A `.yalper` that is a symlink, or (on Unix) owned by another user, is
/// ignored. The directory is returned open, see [`OwnedDir`].
pub fn find_yalper_dir(starts: impl IntoIterator<Item = PathBuf>) -> Option<OwnedDir> {
    starts.into_iter().find_map(|start| {
        let git_root = start
            .ancestors()
            .find(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())?;
        start
            .ancestors()
            .take_while(|dir| dir.starts_with(git_root))
            .find_map(|dir| OwnedDir::open(&dir.join(YALPER_DIR)).ok())
    })
}

/// Appends one line to [`ERRORS_LOG`] in `dir`, emptying the log first if the line would push it past
/// `max_bytes`.
///
/// `message` must never contain payload content (prompts, tool input or output): the log is not redacted.
/// Only Yalper's own error texts and panic messages are passed here.
pub fn append_error(
    dir: &OwnedDir,
    event: Option<&str>,
    message: &str,
    max_bytes: u64,
) -> io::Result<()> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let event = event.map_or_else(|| "-".to_owned(), |e| one_line(e, MAX_EVENT_CHARS));
    let message = one_line(message, MAX_MESSAGE_CHARS);
    let line = format!("{timestamp_ms} {event} {message}\n");

    let mut log = dir.open_file(ERRORS_LOG, Access::Append)?;
    if log.metadata()?.len() + line.len() as u64 > max_bytes {
        // An append-only handle cannot shorten the file on Windows, so a second handle empties it. Writes
        // through `log` still go to the new end of the file.
        dir.open_file(ERRORS_LOG, Access::ReadWrite)?.set_len(0)?;
    }
    log.write_all(line.as_bytes())
}

/// Keeps at most `max_chars` characters and turns line breaks into spaces, so one entry stays one line.
fn one_line(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A temporary git project (it only needs a `.git` entry) with `.yalper/` in it.
    fn project() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir(root.path().join(YALPER_DIR)).unwrap();
        root
    }

    /// The directory found, canonical so that it compares equal on macOS, where temporary directories are
    /// reached through a symlink.
    fn found(starts: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
        find_yalper_dir(starts).map(|dir| fs::canonicalize(dir.path()).unwrap())
    }

    #[test]
    fn finds_yalper_dir_from_a_subdirectory() {
        let root = project();
        let deep = root.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            found([deep]),
            Some(fs::canonicalize(root.path().join(YALPER_DIR)).unwrap())
        );
    }

    #[test]
    fn a_git_file_marks_the_root_like_a_git_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        fs::create_dir(root.path().join(YALPER_DIR)).unwrap();
        assert_eq!(
            found([root.path().to_path_buf()]),
            Some(fs::canonicalize(root.path().join(YALPER_DIR)).unwrap())
        );
    }

    #[test]
    fn tries_start_directories_in_order() {
        let first = tempfile::tempdir().unwrap();
        let second = project();
        let third = project();
        let starts = [first.path(), second.path(), third.path()].map(Path::to_path_buf);
        assert_eq!(
            found(starts),
            Some(fs::canonicalize(second.path().join(YALPER_DIR)).unwrap())
        );
    }

    #[test]
    fn stops_at_the_git_root() {
        let parent = tempfile::tempdir().unwrap();
        fs::create_dir(parent.path().join(YALPER_DIR)).unwrap();
        let repo = parent.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("src");
        fs::create_dir(&deep).unwrap();
        assert_eq!(found([repo, deep]), None);
    }

    #[test]
    fn nothing_is_found_outside_a_git_repository() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(YALPER_DIR)).unwrap();
        assert_eq!(found([root.path().to_path_buf()]), None);
    }

    #[test]
    fn ignores_a_yalper_file() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(root.path().join(YALPER_DIR), "not a directory").unwrap();
        assert_eq!(found([root.path().to_path_buf()]), None);
    }

    #[test]
    fn input_over_the_limit_is_refused_and_drained() {
        let mut reader: &[u8] = b"0123456789";
        assert_eq!(read_limited(&mut reader, 10).unwrap(), b"0123456789");

        let mut reader: &[u8] = b"0123456789A";
        let error = read_limited(&mut reader, 10).unwrap_err();
        assert!(error.contains("too large"), "{error}");
        assert!(reader.is_empty());
    }

    #[test]
    fn error_lines_are_single_lines_with_the_event_name() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(ERRORS_LOG);
        let owned = OwnedDir::open(dir.path()).unwrap();
        append_error(&owned, Some("Stop"), "first\nsecond\r\nthird", 1024).unwrap();
        append_error(&owned, None, "again", 1024).unwrap();
        append_error(&owned, Some("Bad\nEvent"), "x", 1024).unwrap();

        let text = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(" Stop first second  third"), "{text}");
        assert!(lines[1].ends_with(" - again"), "{text}");
        assert!(lines[2].ends_with(" Bad Event x"), "{text}");
    }

    #[test]
    fn event_names_are_shortened() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(ERRORS_LOG);
        let owned = OwnedDir::open(dir.path()).unwrap();
        append_error(&owned, Some(&"E".repeat(1000)), "x", u64::MAX).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains(&format!(" {} x", "E".repeat(MAX_EVENT_CHARS))));
        assert!(!text.contains(&"E".repeat(MAX_EVENT_CHARS + 1)));
    }

    #[test]
    fn error_log_is_emptied_when_it_would_exceed_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(ERRORS_LOG);
        let owned = OwnedDir::open(dir.path()).unwrap();
        fs::write(&log, "x".repeat(90)).unwrap();

        append_error(&owned, None, "fits under the cap", 200).unwrap();
        assert!(
            fs::read_to_string(&log)
                .unwrap()
                .starts_with(&"x".repeat(90))
        );

        append_error(&owned, None, &"y".repeat(150), 200).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains(&"y".repeat(150)));
    }

    #[test]
    fn long_messages_are_shortened() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(ERRORS_LOG);
        let owned = OwnedDir::open(dir.path()).unwrap();
        append_error(&owned, None, &"z".repeat(10 * MAX_MESSAGE_CHARS), u64::MAX).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.len() < MAX_MESSAGE_CHARS + 100);
    }
}
