//! `yalper hook`: the command Claude Code runs on every agent step.
//!
//! Yalper must never change how the agent behaves. Claude Code reads a hook's exit code and output: exit 2
//! blocks the prompt or tool, any other nonzero code shows a "hook error" notice, and stdout on some events
//! is added to the model's context. So this command always exits 0, never writes to stdout or stderr, and
//! appends every error or panic as one line to `.yalper/errors.log` instead.

mod input;

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub use input::{HookEvent, HookInput, InputError};

/// The folder holding a project's recordings.
pub const YALPER_DIR: &str = ".yalper";

/// The error log inside [`YALPER_DIR`].
pub const ERRORS_LOG: &str = "errors.log";

/// When appending a line would make `errors.log` larger than this, the file is emptied first.
pub const ERRORS_LOG_MAX_BYTES: u64 = 1024 * 1024;

const MAX_MESSAGE_CHARS: usize = 2000;

/// The panic message and location, saved by the panic hook because `catch_unwind` only returns the payload.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Handles one hook call: reads the payload from `stdin` and records it. Never panics and never prints.
pub fn run(stdin: &mut dyn Read) {
    panic::set_hook(Box::new(|info| {
        let message = info.payload_as_str().unwrap_or("non-string panic payload");
        let text = match info.location() {
            Some(location) => format!("panic at {location}: {message}"),
            None => format!("panic: {message}"),
        };
        if let Ok(mut slot) = LAST_PANIC.lock() {
            *slot = Some(text);
        }
    }));

    let mut call = Call::default();
    let error = match panic::catch_unwind(AssertUnwindSafe(|| handle(stdin, &mut call))) {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error,
        Err(_) => LAST_PANIC
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or_else(|| "panic".to_owned()),
    };
    if let Some(dir) = &call.yalper_dir {
        // Nowhere is left to report a failure to write the error log, so it is ignored.
        let _ = append_error(
            &dir.join(ERRORS_LOG),
            call.event.as_deref(),
            &error,
            ERRORS_LOG_MAX_BYTES,
        );
    }
}

/// What is known about the current call, kept outside `catch_unwind` so errors can still be logged.
#[derive(Default)]
struct Call {
    yalper_dir: Option<PathBuf>,
    event: Option<String>,
}

fn handle(stdin: &mut dyn Read, call: &mut Call) -> Result<(), String> {
    let mut bytes = Vec::new();
    let payload = match stdin.read_to_end(&mut bytes) {
        Ok(_) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| format!("invalid hook input: {error}")),
        Err(error) => Err(format!("cannot read hook input: {error}")),
    };

    let payload_cwd = payload
        .as_ref()
        .ok()
        .and_then(|value| value.get("cwd"))
        .and_then(Value::as_str);
    let starts = [
        env::var_os("CLAUDE_PROJECT_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from),
        payload_cwd.map(PathBuf::from),
        env::current_dir().ok(),
    ];
    // A project without `.yalper/` is not recorded, and there is nowhere to log to.
    let Some(dir) = find_yalper_dir(starts.into_iter().flatten()) else {
        return Ok(());
    };
    call.yalper_dir = Some(dir);

    let payload = payload?;
    call.event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    HookInput::from_value(payload).map_err(|error| error.to_string())?;

    #[cfg(debug_assertions)]
    if env::var_os("YALPER_TEST_PANIC").is_some() {
        panic!("forced by YALPER_TEST_PANIC");
    }

    Ok(())
}

/// Returns the first `.yalper` directory found by walking up from each start directory in turn.
pub fn find_yalper_dir(starts: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    starts.into_iter().find_map(|start| {
        start
            .ancestors()
            .map(|dir| dir.join(YALPER_DIR))
            .find(|candidate| candidate.is_dir())
    })
}

/// Appends one line to the error log, emptying the log first if the line would push it past `max_bytes`.
pub fn append_error(
    path: &Path,
    event: Option<&str>,
    message: &str,
    max_bytes: u64,
) -> io::Result<()> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let message: String = message
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let line = format!("{timestamp_ms} {} {message}\n", event.unwrap_or("-"));

    let current_len = path.metadata().map_or(0, |metadata| metadata.len());
    let mut options = OpenOptions::new();
    options.create(true);
    if current_len + line.len() as u64 > max_bytes {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    options.open(path)?.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_yalper_dir_in_an_ancestor() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(YALPER_DIR)).unwrap();
        let deep = root.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_yalper_dir([deep]), Some(root.path().join(YALPER_DIR)));
    }

    #[test]
    fn tries_start_directories_in_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let third = tempfile::tempdir().unwrap();
        fs::create_dir(second.path().join(YALPER_DIR)).unwrap();
        fs::create_dir(third.path().join(YALPER_DIR)).unwrap();

        let starts = [first.path(), second.path(), third.path()].map(Path::to_path_buf);
        assert_eq!(
            find_yalper_dir(starts),
            Some(second.path().join(YALPER_DIR))
        );
    }

    #[test]
    fn ignores_a_yalper_file() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(YALPER_DIR), "not a directory").unwrap();
        assert_eq!(find_yalper_dir([root.path().to_path_buf()]), None);
    }

    #[test]
    fn error_lines_are_single_lines_with_the_event_name() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(ERRORS_LOG);
        append_error(&log, Some("Stop"), "first\nsecond\r\nthird", 1024).unwrap();
        append_error(&log, None, "again", 1024).unwrap();

        let text = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" Stop first second  third"), "{text}");
        assert!(lines[1].ends_with(" - again"), "{text}");
    }

    #[test]
    fn error_log_is_emptied_when_it_would_exceed_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(ERRORS_LOG);
        fs::write(&log, "x".repeat(90)).unwrap();

        append_error(&log, None, "fits under the cap", 200).unwrap();
        assert!(
            fs::read_to_string(&log)
                .unwrap()
                .starts_with(&"x".repeat(90))
        );

        append_error(&log, None, &"y".repeat(150), 200).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains(&"y".repeat(150)));
    }

    #[test]
    fn long_messages_are_shortened() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(ERRORS_LOG);
        append_error(&log, None, &"z".repeat(10 * MAX_MESSAGE_CHARS), u64::MAX).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.len() < MAX_MESSAGE_CHARS + 100);
    }
}
