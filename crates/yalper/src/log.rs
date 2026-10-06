//! `yalper log`: lists the recorded sessions of the project around the current directory, with their steps.
//!
//! Everything shown comes from the event log, which holds what agents and repositories sent (prompts, tool
//! input, paths, even session ids and tool names), so every value goes through [`printable`]: a crafted row
//! cannot send escape sequences to the terminal. The store is opened for reading only: nothing is created,
//! migrated or written, and the writer lock is never taken, so it works while hooks are recording (SQLite's
//! WAL lets readers and the writer run together).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use serde_json::Value;

use crate::hook::{self, HookEvent, YALPER_DIR};
use crate::init::{printable, say};
use crate::repo::{self, Refusal, YalperDir};
use crate::store::{Event, Session, Store};

/// A step's summary is cut to this many characters.
const MAX_SUMMARY_CHARS: usize = 60;

/// A step's action (its tool name, or what kind of step it is) is cut to this many characters.
const MAX_ACTION_CHARS: usize = 24;

/// Session ids are shown with this many characters, or more when two sessions would look the same.
const SHORT_ID_CHARS: usize = 8;

/// Session ids are never shown with more characters than this. Claude Code's are 36.
const MAX_ID_CHARS: usize = 64;

/// The default view lists at most this many earlier sessions after the latest one.
const MAX_EARLIER_SESSIONS: usize = 5;

/// Marks text left out of a value that was cut.
pub(crate) const ELLIPSIS: &str = "...";

/// The keys of a tool's input that summarize the call, most telling first: the first one present is shown.
const SUMMARY_KEYS: [&str; 10] = [
    "command",
    "file_path",
    "notebook_path",
    "pattern",
    "url",
    "query",
    "description",
    "skill",
    "path",
    "prompt",
];

/// The keys of [`SUMMARY_KEYS`] that hold a path, shown relative to the project root.
const PATH_KEYS: [&str; 3] = ["file_path", "notebook_path", "path"];

/// Which sessions `yalper log` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The most recently active session.
    Latest,
    /// Every session, the most recently active first.
    All,
    /// The session whose id starts with this text.
    Session(String),
}

/// Lists the sessions of the project around `start` chosen by `selection`, each with its steps, and writes
/// them to `out` with times in `time_zone`. Returns the message to show when the recordings cannot be read:
/// outside a git repository, or where Yalper is not set up.
pub fn log(
    start: &Path,
    selection: &Selection,
    time_zone: &TimeZone,
    out: &mut dyn Write,
) -> Result<(), String> {
    let yalper = find(start, "yalper log")?;
    // Tool paths are absolute. The project root is tried as found from `start` and as `.yalper/` was opened
    // (canonical on Unix), since the agent may have reached the project either way.
    let roots: Vec<String> = [repo::find_root(start), yalper.dir.path().parent()]
        .into_iter()
        .flatten()
        .filter_map(|root| root.to_str().map(str::to_owned))
        .collect();

    let failed = |error: crate::store::Error| format!("cannot read the recordings: {error}");
    let store = Store::open_for_reading(&yalper.dir, &yalper.token).map_err(failed)?;
    let sessions = store.sessions().map_err(failed)?;
    if sessions.is_empty() {
        say(out, NOTHING_RECORDED);
        return Ok(());
    }

    let ids: Vec<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let id_chars = short_id_chars(&ids);
    let chosen = match selection {
        Selection::Latest => vec![&sessions[0]],
        Selection::All => sessions.iter().collect(),
        Selection::Session(prefix) => vec![find_session(&sessions, prefix, id_chars)?],
    };
    for (index, session) in chosen.iter().enumerate() {
        if index > 0 {
            say(out, "");
        }
        let events = store.events_without_output(&session.id).map_err(failed)?;
        write_session(out, session, &events, id_chars, &roots, time_zone);
    }
    if *selection == Selection::Latest && sessions.len() > 1 {
        say(out, "");
        say(
            out,
            "Earlier sessions (`yalper log --session <id>` shows one):",
        );
        let earlier = &sessions[1..];
        for session in earlier.iter().take(MAX_EARLIER_SESSIONS) {
            let steps = store.step_count(&session.id).map_err(failed)?;
            let header = session_header(session, steps as usize, id_chars, time_zone);
            say(out, &format!("  {header}"));
        }
        if earlier.len() > MAX_EARLIER_SESSIONS {
            let more = earlier.len() - MAX_EARLIER_SESSIONS;
            say(out, &format!("  {more} more (`yalper log --all`)"));
        }
    }
    Ok(())
}

pub(crate) const NOTHING_RECORDED: &str = "No sessions recorded yet. Sessions are recorded while Claude Code runs in \
     this project. If one already ran, check that Yalper's hooks are registered (run `yalper init`), that \
     this folder is trusted in Claude Code, and that hooks are not disabled in Claude Code's settings.";

/// The `.yalper/` directory of the project around `start`, found the way the hook finds it, so the same
/// checks apply (see [`hook::find_yalper_dir`]). When there is none, says why, naming `command` (such as
/// `yalper log`) as what to run inside a project set up with `yalper init`.
pub(crate) fn find(start: &Path, command: &str) -> Result<YalperDir, String> {
    if let Some(yalper) = hook::find_yalper_dir([start.to_path_buf()]) {
        return Ok(yalper);
    }
    let root = repo::find_root(start).ok_or_else(|| {
        format!(
            "{} is not inside a git repository. Run `{command}` inside a project set up with `yalper init`.",
            start.display()
        )
    })?;
    let path = root.join(YALPER_DIR);
    if fs::symlink_metadata(&path).is_err() {
        return Err(format!(
            "Yalper is not set up in {}, so nothing is recorded. Run `yalper init` to record the Claude Code \
             sessions of this project.",
            root.display()
        ));
    }
    // Usually fails again, now with the reason. It only succeeds if `.yalper/` changed since the search.
    repo::open_yalper_dir(root, &path).map_err(|why| {
        let hint = match why {
            Refusal::TokenMismatch => " Run `yalper init` to see what to do.",
            Refusal::WritableByOthers => " Run `chmod 700 .yalper` to make it private.",
            Refusal::NotOwnedDir(_) => "",
        };
        format!("Yalper cannot use {}: {why}.{hint}", path.display())
    })
}

/// The session `prefix` names: the one with exactly that id, or else the only one whose id starts with it.
pub(crate) fn find_session<'a>(
    sessions: &'a [Session],
    prefix: &str,
    id_chars: usize,
) -> Result<&'a Session, String> {
    if let Some(session) = sessions.iter().find(|session| session.id == prefix) {
        return Ok(session);
    }
    let matching: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id.starts_with(prefix))
        .collect();
    match matching.as_slice() {
        [session] => Ok(session),
        [] => Err(format!(
            "no recorded session id starts with {}. `yalper log --all` lists every session.",
            printable(prefix)
        )),
        several => {
            let ids: Vec<String> = several
                .iter()
                .take(5)
                .map(|session| short_id(&session.id, id_chars))
                .collect();
            Err(format!(
                "{} sessions start with {}: {}{}. Give more of the id.",
                several.len(),
                printable(prefix),
                ids.join(", "),
                if several.len() > ids.len() {
                    ", ..."
                } else {
                    ""
                }
            ))
        }
    }
}

/// One line about a session: its short id, local start time, whether it ended, and its number of steps.
fn session_header(
    session: &Session,
    steps: usize,
    id_chars: usize,
    time_zone: &TimeZone,
) -> String {
    let started = local_time(session.started_at_ms, time_zone).map_or_else(
        || "at an unknown time".to_owned(),
        |time| time.strftime("%Y-%m-%d %H:%M:%S").to_string(),
    );
    let status = match (session.ended_at_ms, &session.end_reason) {
        (None, _) => "not ended".to_owned(),
        (Some(_), Some(reason)) => format!("ended ({})", cut_end(&printable(reason), 40, false)),
        (Some(_), None) => "ended".to_owned(),
    };
    format!(
        "Session {}, started {started}, {status}, {steps} step{}",
        short_id(&session.id, id_chars),
        plural(steps)
    )
}

/// Writes a session's header line, then its steps as aligned columns.
fn write_session(
    out: &mut dyn Write,
    session: &Session,
    events: &[Event],
    id_chars: usize,
    roots: &[String],
    time_zone: &TimeZone,
) {
    say(
        out,
        &session_header(session, events.len(), id_chars, time_zone),
    );
    if events.is_empty() {
        say(out, "  No steps recorded yet.");
        return;
    }

    let rows: Vec<StepRow> = events
        .iter()
        .map(|event| StepRow::new(event, roots, time_zone))
        .collect();
    let step_width = rows
        .iter()
        .map(|row| row.step.len())
        .chain(["step".len()])
        .max()
        .unwrap_or(0);
    let action_width = rows
        .iter()
        .map(|row| row.action.chars().count())
        .chain(["action".len()])
        .max()
        .unwrap_or(0);
    let line = |step: &str, time: &str, files: &str, action: &str, summary: &str| {
        format!("  {step:>step_width$}  {time:<8}  {files:>5}  {action:<action_width$}  {summary}")
            .trim_end()
            .to_owned()
    };
    say(out, &line("step", "time", "files", "action", "summary"));
    let mut day = local_time(session.started_at_ms, time_zone).map(|time| time.date());
    for row in &rows {
        if row.date.is_some() && row.date != day {
            if let Some(date) = row.date {
                say(out, &format!("  ({date})"));
            }
            day = row.date;
        }
        say(
            out,
            &line(&row.step, &row.time, &row.files, &row.action, &row.summary),
        );
    }
}

/// One step, as shown in its columns.
struct StepRow {
    step: String,
    /// The local date of the step. A line with the date comes before a step on another day than the step
    /// before it.
    date: Option<Date>,
    time: String,
    files: String,
    action: String,
    summary: String,
}

impl StepRow {
    fn new(event: &Event, roots: &[String], time_zone: &TimeZone) -> Self {
        let time = local_time(event.ts_ms, time_zone);
        Self {
            step: event.step.to_string(),
            date: time.map(|time| time.date()),
            time: time.map_or_else(
                || "?".to_owned(),
                |time| time.strftime("%H:%M:%S").to_string(),
            ),
            // Empty for steps that take no snapshot, and for a snapshot that failed.
            // `?` for a step that should have a snapshot but has none (the snapshot failed), empty for steps
            // that never take one.
            files: match event.files_changed {
                Some(files) => files.to_string(),
                None if takes_snapshot(&event.kind) => "?".to_owned(),
                None => String::new(),
            },
            action: cut_end(&action(event), MAX_ACTION_CHARS, false),
            summary: summary(event, roots),
        }
    }
}

/// What a step did: its tool's name, or the kind of step.
pub(crate) fn action(event: &Event) -> String {
    match HookEvent::from_name(&event.kind) {
        HookEvent::SessionStart => "start".to_owned(),
        HookEvent::UserPromptSubmit => "prompt".to_owned(),
        HookEvent::Stop => "reply".to_owned(),
        HookEvent::SessionEnd => "end".to_owned(),
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => event
            .tool_name
            .as_deref()
            .map_or_else(|| "tool".to_owned(), |name| printable(&tool_label(name))),
        HookEvent::Other(kind) => printable(&kind),
    }
}

/// A tool's name as shown: MCP tools, named `mcp__<server>__<tool>`, become `<server>:<tool>`.
fn tool_label(name: &str) -> String {
    match name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
            format!("{server}:{tool}")
        }
        _ => name.to_owned(),
    }
}

/// Whether the hook records a snapshot with this kind of step (see `record::record`).
pub(crate) fn takes_snapshot(kind: &str) -> bool {
    matches!(
        HookEvent::from_name(kind),
        HookEvent::SessionStart
            | HookEvent::UserPromptSubmit
            | HookEvent::PostToolUse
            | HookEvent::PostToolUseFailure
    )
}

/// A one-line summary of a step: the first line of its prompt or reply, how the session started or ended,
/// or for a tool call its main input (a shell command, a file path relative to the project, a search
/// pattern), after `FAILED` if the call failed.
fn summary(event: &Event, roots: &[String]) -> String {
    let failed = event.success == Some(false);
    let max = if failed {
        MAX_SUMMARY_CHARS - "FAILED ".len()
    } else {
        MAX_SUMMARY_CHARS
    };
    let text = |key: &str| event.payload.get(key).and_then(Value::as_str);
    let summary = match HookEvent::from_name(&event.kind) {
        HookEvent::SessionStart => text("source").map(|source| first_line(source, max)),
        HookEvent::UserPromptSubmit => text("prompt").map(|prompt| first_line(prompt, max)),
        HookEvent::Stop => text("last_assistant_message").map(|reply| first_line(reply, max)),
        HookEvent::SessionEnd => text("reason").map(|reason| first_line(reason, max)),
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            tool_summary(event.payload.get("tool_input"), roots, max)
        }
        HookEvent::Other(_) => None,
    }
    .unwrap_or_default();
    match (failed, summary.is_empty()) {
        (false, _) => summary,
        (true, true) => "FAILED".to_owned(),
        (true, false) => format!("FAILED {summary}"),
    }
}

/// The first value of [`SUMMARY_KEYS`] found in a tool's input, at most `max` characters.
fn tool_summary(input: Option<&Value>, roots: &[String], max: usize) -> Option<String> {
    let input = input?.as_object()?;
    SUMMARY_KEYS.iter().find_map(|&key| {
        let value = input.get(key)?.as_str()?;
        if PATH_KEYS.contains(&key) {
            Some(cut_start(&printable(&relative_path(value, roots)), max))
        } else {
            Some(first_line(value, max))
        }
    })
}

/// `path` relative to the first of `roots` it is inside, with `/` as separator, or `path` itself when it is
/// outside all of them. Either separator is accepted in both, and on Windows letter case is ignored, so the
/// result does not depend on how the agent wrote the path.
pub(crate) fn relative_path(path: &str, roots: &[String]) -> String {
    let normalized = path.replace('\\', "/");
    for root in roots {
        let root = root.replace('\\', "/");
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            continue;
        }
        let inside = normalized
            .get(..root.len())
            .is_some_and(|head| same_path_text(head, root));
        if inside && let Some(rest) = normalized[root.len()..].strip_prefix('/') {
            return rest.to_owned();
        }
    }
    path.to_owned()
}

fn same_path_text(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// The first line of `text` that is not blank, made printable and cut to `max` characters. An ellipsis marks
/// what was left out, including any further lines.
fn first_line(text: &str, max: usize) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    cut_end(&printable(first), max, lines.next().is_some())
}

/// `text` cut to `max` characters, ending with an ellipsis when something was left out: part of `text`, or
/// more text after it (`more`).
pub(crate) fn cut_end(text: &str, max: usize, more: bool) -> String {
    if !more && text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text
        .chars()
        .take(max.saturating_sub(ELLIPSIS.len()))
        .collect();
    format!("{}{ELLIPSIS}", kept.trim_end())
}

/// `text` cut to its last `max` characters, starting with an ellipsis when something was left out, so the
/// end of a long path, its file name, stays visible.
fn cut_start(text: &str, max: usize) -> String {
    let chars = text.chars().count();
    if chars <= max {
        return text.to_owned();
    }
    let kept: String = text
        .chars()
        .skip(chars - max.saturating_sub(ELLIPSIS.len()))
        .collect();
    format!("{ELLIPSIS}{kept}")
}

/// The first `chars` characters of a session id, made printable.
pub(crate) fn short_id(id: &str, chars: usize) -> String {
    let mut shown: String = id.chars().take(chars).collect();
    if chars >= MAX_ID_CHARS && id.chars().nth(chars).is_some() {
        shown.push_str(ELLIPSIS);
    }
    printable(&shown)
}

/// How many characters of the session ids make them all look different, at least [`SHORT_ID_CHARS`] and at
/// most [`MAX_ID_CHARS`] (ids that only differ later look the same, and the work stays bounded).
pub(crate) fn short_id_chars(ids: &[&str]) -> usize {
    let longest = ids
        .iter()
        .map(|id| id.chars().take(MAX_ID_CHARS).count())
        .max()
        .unwrap_or(0);
    (SHORT_ID_CHARS..longest)
        .find(|&chars| {
            let mut seen = HashSet::new();
            ids.iter()
                .all(|id| seen.insert(id.chars().take(chars).collect::<String>()))
        })
        .unwrap_or(longest.max(SHORT_ID_CHARS))
}

/// The local date and time of a timestamp in milliseconds, if it is in the range of dates jiff supports.
pub(crate) fn local_time(ms: i64, time_zone: &TimeZone) -> Option<DateTime> {
    Timestamp::from_millisecond(ms)
        .ok()
        .map(|timestamp| time_zone.to_datetime(timestamp))
}

pub(crate) fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_shown_relative_to_the_project() {
        let roots = [
            "/home/dev/project".to_owned(),
            "C:\\Users\\dev\\project\\".to_owned(),
        ];
        assert_eq!(
            relative_path("/home/dev/project/src/a.rs", &roots),
            "src/a.rs"
        );
        assert_eq!(
            relative_path("C:\\Users\\dev\\project\\src\\a.rs", &roots),
            "src/a.rs"
        );
        assert_eq!(relative_path("C:/Users/dev/project/b.rs", &roots), "b.rs");
        // Not inside: a sibling folder whose name starts like the project's, the root itself, elsewhere.
        assert_eq!(
            relative_path("/home/dev/project2/a.rs", &roots),
            "/home/dev/project2/a.rs"
        );
        assert_eq!(
            relative_path("/home/dev/project", &roots),
            "/home/dev/project"
        );
        assert_eq!(relative_path("/etc/hosts", &roots), "/etc/hosts");
        // A root cut inside a multi-byte character does not panic.
        assert_eq!(
            relative_path("/home/dé", &["/home/d".to_owned()]),
            "/home/dé"
        );
        let windows_case = relative_path("c:\\users\\DEV\\project\\x.rs", &roots);
        if cfg!(windows) {
            assert_eq!(windows_case, "x.rs");
        } else {
            assert_eq!(windows_case, "c:\\users\\DEV\\project\\x.rs");
        }
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis() {
        assert_eq!(first_line("short", 10), "short");
        assert_eq!(
            first_line("\n\n  first line  \nsecond", 20),
            "first line..."
        );
        assert_eq!(first_line("0123456789abc", 10), "0123456...");
        assert_eq!(first_line("ééééééééééé", 10), "ééééééé...");
        assert_eq!(first_line("", 10), "");
        assert_eq!(cut_start("/a/very/long/path/file.rs", 12), "...h/file.rs");
        assert_eq!(cut_start("file.rs", 12), "file.rs");
    }

    #[test]
    fn short_ids_grow_until_they_differ() {
        assert_eq!(short_id_chars(&[]), SHORT_ID_CHARS);
        assert_eq!(short_id_chars(&["abc"]), SHORT_ID_CHARS);
        assert_eq!(
            short_id_chars(&["00893aaf-19fa", "1234abcd-0000"]),
            SHORT_ID_CHARS
        );
        assert_eq!(short_id_chars(&["00893aaf-19fa", "00893aaf-29fa"]), 10);
        assert_eq!(short_id_chars(&["00893aaf-1", "00893aaf-12"]), 11);

        // Long ids sharing a long prefix stop at the cap and are shown cut.
        let (a, b) = ("x".repeat(10_000) + "a", "x".repeat(10_000) + "b");
        assert_eq!(short_id_chars(&[&a, &b]), MAX_ID_CHARS);
        assert_eq!(
            short_id(&a, MAX_ID_CHARS),
            "x".repeat(MAX_ID_CHARS) + ELLIPSIS
        );
        let exact = "y".repeat(MAX_ID_CHARS);
        assert_eq!(short_id(&exact, MAX_ID_CHARS), exact);
        assert_eq!(short_id("00893aaf-19fa", 8), "00893aaf");
    }

    #[test]
    fn mcp_tools_are_shown_as_server_and_tool() {
        assert_eq!(
            tool_label("mcp__github__create_issue"),
            "github:create_issue"
        );
        assert_eq!(tool_label("mcp__my_server__a__b"), "my_server:a__b");
        for name in ["Bash", "mcp__", "mcp__github", "mcp____x", "mcp__x__"] {
            assert_eq!(tool_label(name), name);
        }
    }
}
