//! `yalper show <step>`: one recorded step in full: the prompt, reply or tool call (input and output, as
//! stored, so already redacted), then the files the step changed, with a unified diff of each changed text
//! file.
//!
//! The files a step changed are the difference between its snapshot and the snapshot taken just before it
//! (see [`Store::previous_tree_id`]), so a change made by a shell command shows up like an edit.
//!
//! Everything shown comes from the event log or the snapshot store, which hold what agents and repositories
//! wrote, so every line goes through [`printable`] (tabs are kept in content lines: they move the cursor but
//! cannot start an escape sequence), every path is checked with the rules of [`validate_path`], and every
//! blob is read through the hash-checking reader of the snapshot store. The tree diff, the number of files
//! listed and the size of the diff are capped, so a crafted store cannot make the command run for long or
//! flood the terminal. Nothing is written: the event log is opened for reading only, and no tree is ever
//! written to disk.

use std::io::{self, Write};
use std::path::Path;

use gix::ObjectId;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics};
use jiff::tz::TimeZone;
use serde_json::Value;

use crate::hook::HookEvent;
use crate::init::{printable, say};
use crate::log::{
    ELLIPSIS, NOTHING_RECORDED, action, cut_end, find, find_session, local_time, plural, short_id,
    short_id_chars, takes_snapshot,
};
use crate::repo::YalperDir;
use crate::snapshot::{FileChange, FileKind, ShadowStore, validate_path};
use crate::store::{Event, Store};

/// Without `--full`, a prompt, reply, tool input or output shows at most this many lines.
const MAX_TEXT_LINES: usize = 20;

/// Without `--full`, a line of text or of a diff is cut to this many characters.
const MAX_LINE_CHARS: usize = 200;

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

/// How much diff is shown: lines and bytes printed, and bytes of file content read to compute it.
#[derive(Debug, Clone, Copy)]
struct Budget {
    lines: usize,
    bytes: usize,
    read_bytes: usize,
}

const SHORT_BUDGET: Budget = Budget {
    lines: 300,
    bytes: 64 * 1024,
    read_bytes: 32 * 1024 * 1024,
};

/// With `--full`: still bounded, so a crafted snapshot store cannot flood the terminal for minutes.
const FULL_BUDGET: Budget = Budget {
    lines: usize::MAX,
    bytes: 32 * 1024 * 1024,
    read_bytes: 512 * 1024 * 1024,
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

    let time = local_time(event.ts_ms, time_zone).map_or_else(
        || "at an unknown time".to_owned(),
        |time| time.strftime("%Y-%m-%d %H:%M:%S").to_string(),
    );
    say(
        out,
        &printable(&format!("Session {short}, step {step} of {steps}, {time}")),
    );
    write_step(out, &event, options.full);
    if takes_snapshot(&event.kind) {
        say(out, "");
        write_files(out, &yalper, &store, &event, options)?;
    }
    Ok(())
}

/// Writes what the step was: a prompt, a reply, the start or end of the session, or a tool call with its
/// input and output.
fn write_step(out: &mut dyn Write, event: &Event, full: bool) {
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
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => write_tool_call(out, event, full),
        HookEvent::Other(kind) => say(out, &printable(&kind)),
    }
}

fn write_tool_call(out: &mut dyn Write, event: &Event, full: bool) {
    let payload = &event.payload;
    let duration = payload
        .get("duration_ms")
        .and_then(Value::as_u64)
        .map(duration);
    let mut line = field(&action(event));
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
    say(out, &printable(&line));

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
        Some(response) => json_block(out, "Output", response, full),
        None => {}
    }
}

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
fn content(text: &str) -> String {
    text.split('\t')
        .map(printable)
        .collect::<Vec<_>>()
        .join("\t")
}

/// Writes the files the step changed: a list, then a unified diff of each changed text file.
fn write_files(
    out: &mut dyn Write,
    yalper: &YalperDir,
    store: &Store,
    event: &Event,
    options: &Options,
) -> Result<(), String> {
    let Some(tree) = &event.tree_id else {
        say(
            out,
            "No snapshot was recorded for this step (taking it failed, see .yalper/errors.log). Its file \
             changes are in the next step that has a snapshot.",
        );
        return Ok(());
    };
    let not_available = |out: &mut dyn Write| {
        say(
            out,
            "Snapshot not available: the snapshot store no longer has it (the store was created again \
             since this step was recorded).",
        );
    };
    let failed = |error: crate::snapshot::Error| format!("cannot read the snapshots: {error}");
    let shadow = match ShadowStore::open(&yalper.dir, &yalper.token) {
        Ok(shadow) => shadow,
        Err(crate::snapshot::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            not_available(out);
            return Ok(());
        }
        Err(error) => return Err(failed(error)),
    };
    let tree = tree_id(tree)?;
    let previous = store
        .previous_tree_id(&event.session_id, event.step)
        .map_err(|error| format!("cannot read the recordings: {error}"))?;
    let Some(previous) = previous else {
        // The step before it was `yalper init`'s baseline, which the event log does not keep.
        match event.files_changed {
            Some(0) => say(out, "No files changed."),
            Some(files) => say(
                out,
                &format!(
                    "{files} file{} changed since the snapshot `yalper init` took, which is not kept, so \
                     the changes cannot be shown.",
                    plural(files as usize)
                ),
            ),
            None => say(out, "The snapshot before this step is not kept."),
        }
        return Ok(());
    };
    let previous = tree_id(&previous)?;
    if !shadow.has_object(tree) || !shadow.has_object(previous) {
        not_available(out);
        return Ok(());
    }
    if previous == tree {
        say(out, "No files changed.");
        return Ok(());
    }
    let changes = shadow
        .file_changes(previous, tree, MAX_TREE_ENTRIES)
        .map_err(failed)?;
    let budget = if options.full {
        FULL_BUDGET
    } else {
        SHORT_BUDGET
    };
    let listed = if options.full {
        changes.files.len()
    } else {
        changes.files.len().min(MAX_LISTED_FILES)
    };
    let files = classify(&shadow, &changes.files[..listed], budget).map_err(failed)?;

    let count = changes.files.len();
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

    let mut writer = DiffWriter {
        out,
        color: options.color,
        full: options.full,
        budget,
        truncated: false,
    };
    for file in &files {
        let Some((old, new)) = &file.texts else {
            continue;
        };
        if !writer.diff(&file.path, old, new) {
            break;
        }
    }
    if writer.truncated || files.iter().any(|file| file.unread) {
        let marker = if options.full {
            format!("{ELLIPSIS} diff truncated: it is larger than Yalper shows")
        } else {
            format!("{ELLIPSIS} diff truncated (--full shows more)")
        };
        say(writer.out, &marker);
    }
    Ok(())
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

/// A changed file, ready to show.
struct ShownFile {
    verb: &'static str,
    /// Printable, or the path with a note that it is not valid.
    path: String,
    note: Option<&'static str>,
    /// The old and new content of a changed text file, to diff.
    texts: Option<(Vec<u8>, Vec<u8>)>,
    /// A changed file whose content was not read: the diff budget ran out.
    unread: bool,
}

/// Turns each change into a line of the list, and reads the old and new content of modified files to diff,
/// as long as `budget` allows.
fn classify(
    shadow: &ShadowStore,
    changes: &[FileChange],
    budget: Budget,
) -> crate::snapshot::Result<Vec<ShownFile>> {
    let mut read_left = budget.read_bytes;
    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        let (verb, mut note) = match (change.old, change.new) {
            (None, _) => ("added", None),
            (_, None) => ("deleted", None),
            (Some(old), Some(new)) => ("modified", kind_change(old.kind, new.kind)),
        };
        let mut file = ShownFile {
            verb,
            path: String::new(),
            note,
            texts: None,
            unread: false,
        };
        let path = match std::str::from_utf8(&change.path) {
            Ok(path) => path,
            Err(_) => {
                file.path = printable(&String::from_utf8_lossy(&change.path));
                file.note = Some("not valid UTF-8, not shown");
                files.push(file);
                continue;
            }
        };
        let kind = change
            .new
            .or(change.old)
            .map_or(FileKind::Regular, |file| file.kind);
        file.path = printable(path);
        if validate_path(path, kind).is_err() {
            file.note = Some("not a valid path on this system, not shown");
            files.push(file);
            continue;
        }

        // Content is diffed when it changed and the file did not turn into or out of a symlink.
        if let (Some(old), Some(new)) = (change.old, change.new)
            && old.blob != new.blob
            && (old.kind == FileKind::Symlink) == (new.kind == FileKind::Symlink)
        {
            match (
                read(shadow, old.blob, &mut read_left)?,
                read(shadow, new.blob, &mut read_left)?,
            ) {
                (Some(old), Some(new)) if is_binary(&old) || is_binary(&new) => {
                    note = Some("binary file changed");
                }
                (Some(old), Some(new)) => file.texts = Some((old, new)),
                _ => {
                    file.unread = true;
                    note = note.or(Some("diff not shown"));
                }
            }
            file.note = note;
        }
        files.push(file);
    }
    Ok(files)
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

/// The content of blob `id` if it fits in `left` bytes of the read budget, which it then uses. `None` when
/// it does not fit, or the store does not have it.
fn read(
    shadow: &ShadowStore,
    id: ObjectId,
    left: &mut usize,
) -> crate::snapshot::Result<Option<Vec<u8>>> {
    if *left == 0 {
        return Ok(None);
    }
    let Some(bytes) = shadow.read_blob(id)? else {
        return Ok(None);
    };
    if bytes.len() > *left {
        *left = 0;
        return Ok(None);
    }
    *left -= bytes.len();
    Ok(Some(bytes))
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
}

impl DiffWriter<'_> {
    /// Writes the diff of the file at `path` (already printable) from `old` to `new`. Returns `false` once
    /// the budget ran out.
    fn diff(&mut self, path: &str, old: &[u8], new: &[u8]) -> bool {
        if !self.line(Style::Plain, "")
            || !self.line(Style::Header, &format!("--- a/{path}"))
            || !self.line(Style::Header, &format!("+++ b/{path}"))
        {
            return false;
        }
        let input = InternedInput::new(old, new);
        let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
        let hunks = Hunks { writer: self };
        // An error only means the budget ran out, which `truncated` already says.
        let _ = UnifiedDiff::new(
            &diff,
            &input,
            hunks,
            ContextSize::symmetrical(CONTEXT_LINES),
        )
        .consume();
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

/// Receives the hunks of one file's diff from gix and writes them.
struct Hunks<'a, 'b> {
    writer: &'a mut DiffWriter<'b>,
}

impl ConsumeHunk for Hunks<'_, '_> {
    type Out = ();

    fn consume_hunk(
        &mut self,
        header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> io::Result<()> {
        let budget_out = || io::Error::other("diff budget used up");
        if !self.writer.line(Style::Hunk, &header.to_string()) {
            return Err(budget_out());
        }
        for &(kind, line) in lines {
            let style = match kind {
                DiffLineKind::Add => Style::Added,
                DiffLineKind::Remove => Style::Removed,
                DiffLineKind::Context => Style::Plain,
            };
            let text = String::from_utf8_lossy(line);
            let (text, newline) = match text.strip_suffix('\n') {
                Some(text) => (text, true),
                None => (text.as_ref(), false),
            };
            if !self
                .writer
                .line(style, &format!("{}{text}", kind.to_prefix()))
            {
                return Err(budget_out());
            }
            if !newline
                && !self
                    .writer
                    .line(Style::Plain, "\\ No newline at end of file")
            {
                return Err(budget_out());
            }
        }
        Ok(())
    }

    fn finish(self) -> Self::Out {}
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
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let token = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
        let shadow = ShadowStore::init(&owned, &token).unwrap();
        let (old, new) = (
            shadow.write_blob(b"old\n").unwrap(),
            shadow.write_blob(b"new\n").unwrap(),
        );
        let file = |blob| {
            Some(TreeFile {
                kind: FileKind::Regular,
                blob,
            })
        };
        let change = |path: &[u8]| FileChange {
            path: path.to_vec(),
            old: file(old),
            new: file(new),
        };
        // `..` is invalid everywhere, `aux.txt` only on Windows.
        let changes = [
            change(b"src/../../escape"),
            change(b"bad\xffname"),
            change(b".git/config"),
            change(b"aux.txt"),
            change(b"ok\xe2\x80\xaename"),
        ];
        let files = classify(&shadow, &changes, SHORT_BUDGET).unwrap();
        let shown: Vec<(&str, Option<&str>, bool)> = files
            .iter()
            .map(|file| (file.path.as_str(), file.note, file.texts.is_some()))
            .collect();
        let invalid = Some("not a valid path on this system, not shown");
        let aux = if cfg!(windows) {
            ("aux.txt", invalid, false)
        } else {
            ("aux.txt", None, true)
        };
        assert_eq!(
            shown,
            [
                ("src/../../escape", invalid, false),
                ("bad\u{FFFD}name", Some("not valid UTF-8, not shown"), false),
                (".git/config", invalid, false),
                aux,
                ("ok<U+202E>name", None, true),
            ]
        );
    }

    #[test]
    fn the_read_budget_limits_how_much_content_is_diffed() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let token = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
        let shadow = ShadowStore::init(&owned, &token).unwrap();
        let blob = |text: &str| {
            Some(TreeFile {
                kind: FileKind::Regular,
                blob: shadow.write_blob(text.as_bytes()).unwrap(),
            })
        };
        let changes = [
            FileChange {
                path: b"a.txt".to_vec(),
                old: blob("12345\n"),
                new: blob("67890\n"),
            },
            FileChange {
                path: b"b.txt".to_vec(),
                old: blob("abcde\n"),
                new: blob("fghij\n"),
            },
        ];
        let budget = Budget {
            read_bytes: 20,
            ..SHORT_BUDGET
        };
        let files = classify(&shadow, &changes, budget).unwrap();
        assert!(files[0].texts.is_some() && !files[0].unread);
        assert!(files[1].texts.is_none() && files[1].unread);
        assert_eq!(files[1].note, Some("diff not shown"));
    }
}
