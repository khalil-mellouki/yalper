//! `yalper bisect`: names the step of a recorded session, or of several, that broke the tests.
//!
//! It reads the snapshots of what is bisected ([`entries`]), opens the project's cache folder and takes its
//! [`BisectLock`], and lets the engine ([`bisect`]) pick the entries to test. Each run goes through the same
//! protections: the step's copy and dependencies from [`DependencySets::prepare`] (with the env files and
//! `--with` files of [`LocalFiles`], deleted when the bisect ends, like the links of `copy-0` into the project,
//! see [`DependencySets::remove_project_links`]), the cache folder's root cleaned and its
//! boundary files written, the environment of [`SetCopy::run_env`](super::SetCopy::run_env), and [`run`] with the
//! stop flag of Ctrl-C. Nothing is written into the project: only the `meta` table of its event log records
//! where the bisect data is and which files to copy (see [`choose`]).
//!
//! What a test prints is never shown as is: the end of a log is shown through [`log_tail`] (secrets masked,
//! escape sequences removed), each line prefixed, and every verdict comes from how the run ended, never from
//! its output.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;

use super::deps::remove_left_links;
use super::entries::{
    Entries, Entry, Kind, Scope, another_session_running, entries, others_between, step_name,
};
use super::{
    Answer, BisectLock, CACHE_DIR_KEY, CacheDir, Choice, DependencySets, End, Exit, Failure,
    InstallFailure, LocalFiles, Logs, Outcome, Prepared, Reason, Report, Run, SKIP_EXIT_CODE,
    Settings, Skip, Tested, Trial, Used, bisect, choose, clean_root, log_tail, remove_left_over,
    remove_stale, run, write_boundaries,
};
use crate::init::{printable, say};
use crate::log::{
    ELLIPSIS, NOTHING_RECORDED, find, find_session, plural, short_id, short_id_chars,
};
use crate::redact::{redact_str, strip_escapes};
use crate::repo::Token;
use crate::show::{self, Changes, Format, StepView};
use crate::snapshot::ShadowStore;
use crate::store::{Event, Session, StepRange, Store};

/// The most `--retries` accepted: more runs of a failing step would rather hide a real failure.
pub const MAX_RETRIES: u32 = 10;

/// How many lines of the end of a log are shown when an end of the bisect does not behave as expected.
const TAIL_LINES: usize = 20;

/// The prefix of each line of a log shown on screen, so that no line a test prints can pass for Yalper's own.
const TAIL_PREFIX: &str = "  | ";

/// The "other sessions changed files" note names at most this many sessions.
const MAX_OTHER_SESSIONS: usize = 5;

/// How `--since` is shown.
const SINCE_FORMAT: &str = "%Y-%m-%d %H:%M";

/// What the user asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The command that runs the tests.
    pub test: String,
    /// The session whose id starts with this, instead of the most recently active one.
    pub session: Option<String>,
    /// `--all`: every session of the project, instead of one.
    pub all: bool,
    /// `--since`: the sessions since this local date and time, from the first snapshot taken then, instead of
    /// one session.
    pub since: Option<DateTime>,
    /// `--good`: a step where the tests pass.
    pub good: Option<StepRef>,
    /// `--bad`: a step where the tests fail.
    pub bad: Option<StepRef>,
    /// `--setup`: the command that installs the dependencies of a copy.
    pub setup: Option<String>,
    pub settings: Settings,
    /// `--with-env`, `--no-env`, `--with`, `--no-with`: the ignored files of the project to copy into the test
    /// copies.
    pub files: Choice,
}

/// A step named by `--good` or `--bad`: `12`, or `5f3c:12` for step 12 of the session whose id starts with
/// `5f3c`. Without a session, a step of the session bisected, or of the most recently active one when the bisect
/// covers several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRef {
    pub session: Option<String>,
    pub step: u32,
}

/// Parses `--good` and `--bad`: a step number, or a session (its id or the start of it) and a step number as
/// `<session>:<step>`.
pub fn parse_step(text: &str) -> Result<StepRef, String> {
    let (session, step) = match text.rsplit_once(':') {
        Some((session, step)) => (Some(session), step),
        None => (None, text),
    };
    match (session, step.parse::<u32>()) {
        (Some(""), _) | (_, Err(_)) => Err(
            "expected a step number, or a session and a step number as <session>:<step>, such as 12 or \
             5f3c2a1e:12"
                .to_owned(),
        ),
        (session, Ok(step)) => Ok(StepRef {
            session: session.map(str::to_owned),
            step,
        }),
    }
}

/// Parses `--since`: a local date (`YYYY-MM-DD`, from its start) or a local date and time (`YYYY-MM-DD HH:MM`).
pub fn parse_since(text: &str) -> Result<DateTime, String> {
    DateTime::strptime("%Y-%m-%d %H:%M", text)
        .or_else(|_| Date::strptime("%Y-%m-%d", text).map(|date| date.at(0, 0, 0, 0)))
        .map_err(|_| {
            "expected a local date, or a date and time, as YYYY-MM-DD or \"YYYY-MM-DD HH:MM\", such as \
             2026-10-11 or \"2026-10-11 14:30\""
                .to_owned()
        })
}

/// Where the output goes.
#[derive(Debug, Clone)]
pub struct Screen {
    /// The output is a terminal: the time of a run going on is shown, updated every second, on standard output
    /// (which must then be the output given to [`command`]).
    pub live: bool,
    pub time_zone: TimeZone,
}

/// How a bisect ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// A step, or a range of steps, was named.
    Named,
    /// No step could be named: an end did not behave as expected, or there was nothing to test.
    NotNamed,
    /// Stopped by Ctrl-C.
    Stopped,
}

impl Ended {
    /// The exit code of `yalper bisect`.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Named => 0,
            Self::NotNamed => 1,
            Self::Stopped => 130,
        }
    }
}

/// Parses `--timeout`: whole seconds, or a number followed by `s` (seconds) or `m` (minutes).
pub fn parse_timeout(text: &str) -> Result<Duration, String> {
    let (number, unit) = match text.strip_suffix('m') {
        Some(number) => (number, 60),
        None => (text.strip_suffix('s').unwrap_or(text), 1),
    };
    match number.parse::<u64>().ok().and_then(|n| n.checked_mul(unit)) {
        Some(seconds) if seconds > 0 => Ok(Duration::from_secs(seconds)),
        _ => Err("expected a number of seconds greater than 0, such as 90, 30s or 5m".to_owned()),
    }
}

/// Bisects a session of the project around `start` as `options` say, writing progress and the result to `out`,
/// and stopping cleanly when `stop` is set. Returns the message to show when the bisect cannot run.
pub fn command(
    start: &Path,
    options: &Options,
    screen: &Screen,
    out: &mut dyn Write,
    stop: &AtomicBool,
) -> Result<Ended, String> {
    command_in(start, options, screen, out, stop, &CacheDir::open)
}

/// [`command`], opening the project's cache folder with `open_cache`.
fn command_in(
    start: &Path,
    options: &Options,
    screen: &Screen,
    out: &mut dyn Write,
    stop: &AtomicBool,
    open_cache: &dyn Fn(&Path, &Token) -> io::Result<CacheDir>,
) -> Result<Ended, String> {
    let yalper = find(start, "yalper bisect")?;
    let failed = |error: crate::store::Error| format!("cannot read the recordings: {error}");
    let across = options.all || options.since.is_some();
    if across && options.session.is_some() {
        return Err(
            "--session bisects one session, --all and --since several: give only one of them."
                .to_owned(),
        );
    }
    if options.since.is_some() && options.good.is_some() {
        return Err(
            "--since and --good both choose the good end: give only one of them.".to_owned(),
        );
    }
    let store = Store::open_for_reading(&yalper.dir, &yalper.token).map_err(failed)?;
    let sessions = store.sessions().map_err(failed)?;
    if sessions.is_empty() {
        return Err(NOTHING_RECORDED.to_owned());
    }
    let ids: Vec<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let id_chars = short_id_chars(&ids);
    // The session bisected, or, across sessions, the one a step without a session names.
    let session = match &options.session {
        Some(prefix) => find_session(&sessions, prefix, id_chars)?,
        None => &sessions[0],
    };
    let short = short_id(&session.id, id_chars);
    let project = yalper
        .dir
        .path()
        .parent()
        .ok_or("cannot find the project folder above .yalper")?;
    let chosen = choose(&store, project, start, &options.files)?;
    for warning in &chosen.warnings {
        say(out, &printable(&format!("warning: {warning}")));
    }
    let since = options
        .since
        .map(|since| since.strftime(SINCE_FORMAT).to_string());
    let since_ms = match options.since {
        Some(since) => Some(
            screen
                .time_zone
                .to_timestamp(since)
                .map_err(|error| format!("--since {}: {error}", since.strftime(SINCE_FORMAT)))?
                .as_millisecond(),
        ),
        None => None,
    };
    // Only the steps the bisect can need are read: those of the session, with the steps of other sessions
    // recorded in between, or those from the date asked.
    let (range, scope, what, pick_good) = match (across, since_ms, &since) {
        (false, ..) => (
            StepRange::Spanning(&session.id),
            Scope::Session(&session.id),
            format!("session {short}"),
            "Pick a good end with --good <step>.",
        ),
        (true, Some(ms), Some(since)) => (
            StepRange::Since(ms),
            Scope::All { since_ms },
            format!("the sessions since {since}"),
            "Pick a later date with --since, or a good end with --all --good <session>:<step>.",
        ),
        (true, ..) => (
            StepRange::All,
            Scope::All { since_ms: None },
            "across sessions".to_owned(),
            "Pick a good end with --good <session>:<step>.",
        ),
    };
    let steps = store.steps_in_recorded_order(range).map_err(failed)?;
    let names = Names {
        steps: &steps,
        id_chars,
        across,
    };
    let good = find_end(
        &store,
        &names,
        &sessions,
        session,
        "--good",
        options.good.as_ref(),
    )?;
    let bad = find_end(
        &store,
        &names,
        &sessions,
        session,
        "--bad",
        options.bad.as_ref(),
    )?;
    let shadow = ShadowStore::open(&yalper.dir, &yalper.token)
        .map_err(|error| format!("cannot read the snapshots: {error}"))?;
    let Entries {
        entries,
        bad,
        missing,
    } = entries(
        &steps,
        &shadow,
        scope,
        (good, bad),
        &|order| names.step(order),
        pick_good,
    )
    .map_err(|why| format!("cannot bisect {what}: {why}"))?;
    let (good_end, bad_end) = (entries[0], entries[entries.len() - 1]);
    if entries.len() < 2 || good_end.tree == bad_end.tree {
        write_missing(out, &names, &missing);
        say(
            out,
            &format!(
                "Nothing to test: the code at {} is the same as at {}, the good end.",
                step_name(steps[bad].step, Some(&names.short(bad))),
                names.entry(&good_end)
            ),
        );
        return Ok(Ended::NotNamed);
    }

    let cache = open_cache(project, &yalper.token).map_err(|error| error.to_string())?;
    let _lock = BisectLock::acquire(&cache).map_err(|error| {
        if error.kind() == io::ErrorKind::ResourceBusy {
            format!("{error}. Only one bisect of a project runs at a time: wait for it to end, or stop it.")
        } else {
            error.to_string()
        }
    })?;
    say(
        out,
        &printable(&format!(
            "Bisect data (test copies and run logs): {}",
            cache.path().display()
        )),
    );
    if let Some(warning) = cache.warning() {
        say(out, &printable(&format!("warning: {warning}")));
    }
    write_missing(out, &names, &missing);
    store
        .set_meta(CACHE_DIR_KEY, Some(&cache.meta_value()))
        .map_err(|error| format!("cannot record where the bisect data is: {error}"))?;
    remove_stale(&cache);
    let io_failed = |error: io::Error| error.to_string();
    clean_root(&cache).map_err(io_failed)?;
    write_boundaries(&cache).map_err(io_failed)?;
    // After `clean_root`, which removes a link left in place of a copy, and before any copy is used: files an
    // earlier bisect could not delete would apply to this one.
    for warning in remove_left_over(&cache, &store) {
        say(out, &printable(&format!("warning: {warning}")));
    }
    if let Some(warning) = remove_left_links(&cache) {
        say(out, &printable(&format!("warning: {warning}")));
    }
    let logs = Logs::open(&cache).map_err(io_failed)?;
    let existed = copies_in(&cache).map_err(io_failed)?;
    // Its own connection, so it can update the list of copied files until it is dropped.
    let files_events = Store::open_for_reading(&yalper.dir, &yalper.token).map_err(failed)?;
    let (local_files, warnings) = LocalFiles::new(&cache, &chosen, files_events);
    for warning in warnings {
        say(out, &printable(&format!("warning: {warning}")));
    }
    if let Some(summary) = local_files.summary() {
        say(out, &printable(&summary));
    }
    let sets = DependencySets::new(
        &cache,
        &store,
        options.setup.clone(),
        std::env::vars_os().collect(),
    )
    .map_err(|error| error.to_string())?
    .with_local_files(local_files);
    let candidates = entries.len() - 1;
    let bisected = if across {
        let count = entries
            .iter()
            .map(|entry| steps[entry.order].session_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        let since = since
            .map(|since| format!(" since {since}"))
            .unwrap_or_default();
        format!(
            "{count} session{}{since}, in the order their snapshots were taken",
            plural(count)
        )
    } else {
        format!("session {short}")
    };
    say(
        out,
        &format!(
            "Bisecting {bisected}: {candidates} step{} changed files after {} (the good end), up to {} \
             (the bad end).",
            plural(candidates),
            names.entry(&good_end),
            names.entry(&bad_end)
        ),
    );

    let mut tester = Tester {
        options,
        entries: &entries,
        names: &names,
        store: &store,
        shadow: &shadow,
        cache: &cache,
        logs: &logs,
        sets,
        stop,
        out: &mut *out,
        live: screen.live,
        runs: 0,
        existed,
        seen: HashSet::new(),
        history: History::default(),
    };
    let trees: Vec<_> = entries.iter().map(|entry| entry.tree).collect();
    let report = match bisect(&trees, options.settings, |trial| tester.test(trial)) {
        Ok(report) => report,
        Err(error) => {
            let mut message = error.to_string();
            let not_deleted = tester.sets.remove_local_files();
            for not_deleted in not_deleted
                .into_iter()
                .chain(tester.sets.remove_project_links())
            {
                message.push_str(&format!(". Also: {not_deleted}"));
            }
            return Err(message);
        }
    };
    // Deleted before the result is shown, so they stay no longer than the runs need them.
    let used = tester.sets.local_files().map(LocalFiles::used);
    let mut not_deleted = tester.sets.remove_local_files();
    not_deleted.extend(tester.sets.remove_project_links());
    let history = tester.history;
    say(out, "");
    let ended = write_answer(out, &report, &entries, &history, &names);
    for message in not_deleted {
        say(out, &printable(&format!("warning: {message}")));
    }
    if ended == Ended::Stopped {
        return Ok(ended);
    }
    write_used(out, used.unwrap_or_default());

    // The last entry that passed and the first that failed.
    let around = match &report.answer {
        Answer::Found { entry, .. } => Some((*entry - 1, *entry)),
        Answer::Range { entries: range, .. } => Some((range[0] - 1, range[range.len() - 1])),
        _ => None,
    };
    let Some((passed, failed)) = around else {
        return Ok(ended);
    };
    let (good, breaking) = (entries[passed], entries[failed]);
    let found = matches!(report.answer, Answer::Found { .. });
    // Across sessions, every snapshot between two entries is an entry, or is missing and named before.
    let others = if across {
        Vec::new()
    } else {
        others_between(&steps, &session.id, &good, &breaking)
    };
    if !others.is_empty() {
        write_others(out, &names, &others, (&good, &breaking), found);
    } else if found && breaking.base != Some(good.tree) {
        say(
            out,
            &format!(
                "Note: the diff below goes from {}, the last code that passed, so it also holds changes made \
                 outside {} (by steps without a snapshot, or by another session).",
                names.entry(&good),
                names.entry(&breaking)
            ),
        );
    }
    if found {
        say(out, "");
        write_breaking_step(
            out,
            start,
            &yalper,
            &store,
            &shadow,
            &names,
            (good, breaking),
            screen,
        )?;
    }
    Ok(ended)
}

/// Names steps and entries on screen, each with its session when the bisect covers several.
struct Names<'a> {
    /// Every recorded step of the project, in the order they were recorded.
    steps: &'a [Event],
    /// How many characters of a session id are shown (see [`short_id_chars`]).
    id_chars: usize,
    /// The bisect covers several sessions.
    across: bool,
}

impl Names<'_> {
    /// The short id of the session of the step at `order` in [`Self::steps`].
    fn short(&self, order: usize) -> String {
        short_id(&self.steps[order].session_id, self.id_chars)
    }

    /// The session to name with the step at `order`, if any.
    fn session(&self, order: usize) -> Option<String> {
        self.across.then(|| self.short(order))
    }

    /// `step 12`, or `step 12 of session 5f3c2a1e` across sessions.
    fn step(&self, order: usize) -> String {
        step_name(self.steps[order].step, self.session(order).as_deref())
    }

    /// The name of `entry` (see [`Entry::label`]).
    fn entry(&self, entry: &Entry) -> String {
        entry.label(self.session(entry.order).as_deref())
    }

    /// The name of `entry`, followed by what it changed when no step of the agent made it.
    fn described(&self, entry: &Entry) -> String {
        match entry.changes() {
            Some(changes) => format!("{}, {changes}", self.entry(entry)),
            None => self.entry(entry),
        }
    }
}

/// Where the step `end`, given with `option`, is in the steps of `names`. A step without a session is of
/// `session`: the session bisected, or the most recently active one across sessions. In a bisect of one session
/// it must be a step of that session.
fn find_end(
    store: &Store,
    names: &Names<'_>,
    sessions: &[Session],
    session: &Session,
    option: &str,
    end: Option<&StepRef>,
) -> Result<Option<usize>, String> {
    let Some(end) = end else {
        return Ok(None);
    };
    let named = match &end.session {
        Some(prefix) => find_session(sessions, prefix, names.id_chars)?,
        None => session,
    };
    let short = short_id(&named.id, names.id_chars);
    if !names.across && named.id != session.id {
        return Err(format!(
            "{option} names a step of session {short}, but the bisect is of session {}. Add --all to bisect \
             across sessions.",
            short_id(&session.id, names.id_chars)
        ));
    }
    if let Some(order) = names
        .steps
        .iter()
        .position(|event| event.session_id == named.id && event.step == end.step)
    {
        return Ok(Some(order));
    }
    // Not among the steps read: recorded before the date of `--since`, or not recorded at all.
    let failed = |error: crate::store::Error| format!("cannot read the recordings: {error}");
    if store.event(&named.id, end.step).map_err(failed)?.is_some() {
        return Err(format!(
            "{option} names step {} of session {short}, which was recorded before the first snapshot since the \
             date given with --since.",
            end.step
        ));
    }
    Err(match store.step_count(&named.id).map_err(failed)? {
        0 => format!("session {short} has no steps recorded yet ({option})."),
        count => format!(
            "session {short} has no step {} ({option}): its steps are 1 to {count}. `yalper log --session \
             {short}` lists them.",
            end.step
        ),
    })
}

/// Writes a note naming the steps of other sessions (at `others` in the steps) that changed files between the
/// last entry that passed and the first that failed (`between`), in a bisect of one session: their changes are
/// in the code tested at the failing entry. `found`: the failing entry is the answer, and its diff follows.
fn write_others(
    out: &mut dyn Write,
    names: &Names<'_>,
    others: &[usize],
    (passed, failed): (&Entry, &Entry),
    found: bool,
) {
    // Each session with the number of its steps, and its first and last step, in the order they come.
    let mut sessions: Vec<(&str, usize, u32, u32)> = Vec::new();
    for &order in others {
        let event = &names.steps[order];
        match sessions.iter_mut().find(|(id, ..)| *id == event.session_id) {
            Some((_, count, _, last)) => {
                *count += 1;
                *last = event.step;
            }
            None => sessions.push((&event.session_id, 1, event.step, event.step)),
        }
    }
    let mut listed: Vec<String> = sessions
        .iter()
        .take(MAX_OTHER_SESSIONS)
        .map(|&(id, count, first, last)| {
            let short = short_id(id, names.id_chars);
            match count {
                1 => format!("session {short} (step {first})"),
                count => format!("session {short} ({count} steps, {first} to {last})"),
            }
        })
        .collect();
    if sessions.len() > MAX_OTHER_SESSIONS {
        listed.push(ELLIPSIS.to_owned());
    }
    let failed = names.entry(failed);
    let diff = if found {
        " (and so does the diff below)"
    } else {
        ""
    };
    say(
        out,
        &format!(
            "Note: other sessions changed files between {} and {failed}, so the code tested at {failed} also \
             holds their changes{diff}: {}. `yalper bisect --all` tests their steps too.",
            names.entry(passed),
            listed.join(", ")
        ),
    );
}

/// Writes the step that broke the tests as `yalper show` does, with the prompt that led to it and the diff from
/// the last code that passed.
#[allow(clippy::too_many_arguments)]
fn write_breaking_step(
    out: &mut dyn Write,
    start: &Path,
    yalper: &crate::repo::YalperDir,
    store: &Store,
    shadow: &ShadowStore,
    names: &Names<'_>,
    (good, breaking): (Entry, Entry),
    screen: &Screen,
) -> Result<(), String> {
    let failed = |error: crate::store::Error| format!("cannot read the recordings: {error}");
    let short = names.short(breaking.order);
    let event = store
        .event(&names.steps[breaking.order].session_id, breaking.step)
        .map_err(failed)?
        .ok_or("the step that broke the tests is no longer in the event log")?;
    let steps = store.step_count(&event.session_id).map_err(failed)?;
    let prompt = show::prompt_of(store, &event).map_err(failed)?;
    let roots = show::project_roots(start, yalper);
    let view = StepView {
        event: &event,
        session: &short,
        steps,
        prompt: prompt.as_ref(),
        roots: &roots,
        time_zone: &screen.time_zone,
    };
    let changes = Changes::Between {
        shadow,
        from: good.tree,
        to: breaking.tree,
    };
    // Without color: the output is redacted as a whole, and escape sequences are removed first.
    let format = Format {
        full: false,
        color: false,
    };
    let mut shown = Vec::new();
    let result = show::write_step(&mut shown, &view, changes, format);
    // The prompt and the tool call were redacted when they were recorded, but the diff shows the step's files
    // as they are, so a secret the agent wrote into a file is masked here.
    let text = String::from_utf8_lossy(&shown);
    let text = strip_escapes(&text);
    let text = redact_str(&text).unwrap_or_else(|| text.into_owned());
    let _ = out.write_all(text.as_bytes());
    result
}

/// What the runs left behind that the answer shows.
#[derive(Debug, Default)]
struct History {
    /// The latest log of each entry tested.
    last_log: HashMap<usize, (PathBuf, Output)>,
    /// The entries whose run reached the time limit.
    timed_out: Vec<usize>,
}

/// Writes the answer of `report` on `entries`, named by `names`, then what the result should be read with:
/// flaky entries, entries that could not be tested, runs over the time limit. When an end did not behave as
/// expected, the end of its log comes first. Returns how the bisect ended.
fn write_answer(
    out: &mut dyn Write,
    report: &Report,
    entries: &[Entry],
    history: &History,
    names: &Names<'_>,
) -> Ended {
    let label = |entry: usize| names.entry(&entries[entry]);
    let runs = format!(
        "Found in {} run{}.",
        report.runs,
        plural(report.runs as usize)
    );
    // How `--good` and `--bad` name a step.
    let step = if names.across {
        "<session>:<step>"
    } else {
        "<step>"
    };
    let last = entries.len() - 1;
    let ended = match &report.answer {
        Answer::Found { entry, failure } => {
            let breaking = &entries[*entry];
            let name = label(*entry);
            let session = names.short(breaking.order);
            // Changes found when a session started or resumed: made by no recorded step, unless another
            // session was running then, whose agent may have made them in a step without a snapshot.
            let outside_sessions = if another_session_running(names.steps, breaking.order) {
                "No tool step of an agent made them (they may come from you, another program, or a step of the \
                 session that was running then). The diff below shows them."
            } else {
                "No agent step made them: they were made outside the recorded sessions (by you or another \
                 program). The diff below shows them."
            };
            let between_turns = "No tool step of the agent made them: they were made between two of its turns \
                                 (by you or another program). The diff below shows them.";
            let (cause, outside) = match breaking.kind {
                Kind::SessionStart => (
                    format!(
                        "The changes made between sessions, before session {session} (recorded at its step {}), \
                         broke the tests",
                        breaking.step
                    ),
                    Some(outside_sessions),
                ),
                Kind::Resume => (
                    format!(
                        "The changes made before session {session} resumed (at its step {}) broke the tests",
                        breaking.step
                    ),
                    Some(outside_sessions),
                ),
                Kind::Compact => (
                    format!(
                        "The changes made before this point of the session, at {name}, broke the tests"
                    ),
                    Some(between_turns),
                ),
                Kind::Prompt => (
                    format!("The changes made before the prompt at {name} broke the tests"),
                    Some(between_turns),
                ),
                Kind::Step | Kind::Before => {
                    (format!("{} broke the tests", capitalized(&name)), None)
                }
            };
            say(
                out,
                &format!(
                    "{cause}: they pass at {} and fail at {name} ({failure}). {runs}",
                    label(entry - 1)
                ),
            );
            if let Some(outside) = outside {
                say(out, outside);
            }
            if let Some((log, _)) = history.last_log.get(entry) {
                say(
                    out,
                    &printable(&format!("Its test output: {}", log.display())),
                );
            }
            Ended::Named
        }
        Answer::Range {
            entries: range,
            failure,
        } => {
            let (first, end) = (range[0], range[range.len() - 1]);
            let steps = if names.across {
                format!("the steps from {} to {}", label(first), label(end))
            } else {
                format!("steps {} to {}", entries[first].step, entries[end].step)
            };
            say(
                out,
                &format!(
                    "One of {steps} broke the tests, but some of them could not be tested: they pass at {} and \
                     fail at {} ({failure}). {runs}",
                    label(first - 1),
                    label(end)
                ),
            );
            for &entry in range {
                // Every entry of the range was skipped, except the last, which failed.
                let what = match report.skipped.iter().find(|(skipped, _)| *skipped == entry) {
                    Some((_, skip)) => format!("could not be tested: {skip}"),
                    None => format!("fails ({failure})"),
                };
                say(
                    out,
                    &format!("  {}: {what}", names.described(&entries[entry])),
                );
            }
            let session = if names.across {
                "<session>".to_owned()
            } else {
                names.short(entries[end].order)
            };
            say(
                out,
                &format!("`yalper show <step> --session {session}` shows each step."),
            );
            Ended::Named
        }
        Answer::GoodEndFails(failure) => {
            write_tail(out, history.last_log.get(&0));
            say(
                out,
                &format!(
                    "The good end, {}, already fails ({failure}): the tests fail before any step that is \
                     bisected, so none of them can be named. Pick a step where the tests pass with --good \
                     {step}, or check the test command.",
                    label(0)
                ),
            );
            Ended::NotNamed
        }
        Answer::BadEndPasses => {
            write_tail(out, history.last_log.get(&last));
            say(
                out,
                &format!(
                    "The bad end, {}, passes: the tests do not fail there, so no step broke them. Pick a step \
                     where the tests fail with --bad {step}, or check the test command.",
                    label(last)
                ),
            );
            Ended::NotNamed
        }
        Answer::EndSkipped(end, skip) => {
            let (which, entry, option) = match end {
                End::Good => ("good", 0, "--good"),
                End::Bad => ("bad", last, "--bad"),
            };
            write_tail(out, history.last_log.get(&entry));
            say(
                out,
                &format!(
                    "The {which} end, {}, cannot be tested ({skip}), so the bisect cannot go on. Pick another \
                     {which} end with {option} {step}.",
                    label(entry)
                ),
            );
            Ended::NotNamed
        }
        Answer::NothingToTest => {
            say(out, "Nothing to test: the two ends hold the same code.");
            Ended::NotNamed
        }
        Answer::Stopped => {
            say(out, "Stopped. Your project was not changed.");
            return Ended::Stopped;
        }
    };

    for &entry in &report.flaky {
        say(
            out,
            &format!(
                "Flaky: {} failed, then passed when run again, so it counts as passing.",
                label(entry)
            ),
        );
    }
    let in_answer = |entry: usize| match &report.answer {
        Answer::Range { entries, .. } => entries.contains(&entry),
        _ => false,
    };
    for (entry, skip) in report
        .skipped
        .iter()
        .filter(|(entry, _)| !in_answer(*entry))
    {
        say(
            out,
            &format!("Note: {} could not be tested: {skip}.", label(*entry)),
        );
    }
    if !history.timed_out.is_empty() {
        let labels: Vec<String> = history
            .timed_out
            .iter()
            .map(|&entry| label(entry))
            .collect();
        say(
            out,
            &format!(
                "Note: {} reached the time limit. A run over the time limit counts as a failure; --timeout \
                 <time> changes the limit.",
                labels.join(", ")
            ),
        );
    }
    ended
}

/// `text` with its first letter in upper case.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Writes a note naming the steps between the ends (at `missing` in the steps) whose snapshot is no longer in
/// the snapshot store.
fn write_missing(out: &mut dyn Write, names: &Names<'_>, missing: &[usize]) {
    if missing.is_empty() {
        return;
    }
    let steps: Vec<String> = missing.iter().map(|&order| names.step(order)).collect();
    let (are, they) = if missing.len() == 1 {
        ("is", "it is")
    } else {
        ("are", "they are")
    };
    say(
        out,
        &format!(
            "Note: the snapshot{} of {} {are} no longer in the snapshot store, so {they} not tested.",
            plural(missing.len()),
            steps.join(", ")
        ),
    );
}

/// Writes a note saying that the tests used the current env files or `--with` files, if they used any: Yalper
/// never keeps their content, so those from the time of each step are not known.
fn write_used(out: &mut dyn Write, used: Used) {
    let what = match (used.env, used.with) {
        (true, true) => "your current env files and the current files given with --with",
        (true, false) => "your current env files",
        (false, true) => "the current files given with --with",
        (false, false) => return,
    };
    say(
        out,
        &format!("Note: the tests used {what}, not the ones from the time of each step."),
    );
}

/// The names of the test copies (`copy-<n>`) in the project's cache folder `cache`.
fn copies_in(cache: &CacheDir) -> io::Result<HashSet<OsString>> {
    let mut copies = HashSet::new();
    for entry in fs::read_dir(cache.path())? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.file_name().to_string_lossy().starts_with("copy-") {
            copies.insert(entry.file_name());
        }
    }
    Ok(copies)
}

/// Which output a log holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    Test,
    Install,
}

/// Writes the end of a log (see [`log_tail`]) under a heading that names it, each line prefixed with
/// [`TAIL_PREFIX`].
fn write_tail(out: &mut dyn Write, log: Option<&(PathBuf, Output)>) {
    let Some((path, output)) = log else {
        return;
    };
    let what = match output {
        Output::Test => "test",
        Output::Install => "install",
    };
    match log_tail(path, TAIL_LINES) {
        Ok(lines) if lines.is_empty() => say(
            out,
            &printable(&format!(
                "The {what} output in {} is empty.",
                path.display()
            )),
        ),
        Ok(lines) => {
            say(
                out,
                &printable(&format!(
                    "Last {} line{} of the {what} output, from {}:",
                    lines.len(),
                    plural(lines.len()),
                    path.display()
                )),
            );
            for line in lines {
                say(out, &format!("{TAIL_PREFIX}{line}"));
            }
        }
        Err(error) => say(
            out,
            &printable(&format!("Cannot show the {what} output: {error}")),
        ),
    }
}

/// Runs the tests of the entries the engine asks for.
struct Tester<'a> {
    options: &'a Options,
    entries: &'a [Entry],
    names: &'a Names<'a>,
    store: &'a Store,
    shadow: &'a ShadowStore,
    cache: &'a CacheDir,
    logs: &'a Logs,
    sets: DependencySets,
    stop: &'a AtomicBool,
    out: &'a mut dyn Write,
    live: bool,
    /// Runs started, which number their logs.
    runs: usize,
    /// The copies that were in the cache folder when the bisect started.
    existed: HashSet<OsString>,
    /// The copies used in this bisect.
    seen: HashSet<OsString>,
    history: History,
}

impl Tester<'_> {
    /// Tests the entry of `trial`, showing one line for it.
    fn test(&mut self, trial: &Trial) -> io::Result<Tested> {
        if self.stop.load(Ordering::SeqCst) {
            return Ok(Tested::Stopped);
        }
        let heading = self.heading(trial);
        let _ = write!(self.out, "{heading}");
        let _ = self.out.flush();
        let live = self.live;
        let (attempt, ticked) = ticking(&heading, live, || self.attempt(trial));
        let (tested, text) = attempt.inspect_err(|_| say(self.out, " could not run"))?;
        // On a terminal the ticker wrote the time after the heading: the line is written again, whole.
        let line = if live {
            let width = ticked.saturating_sub(heading.len() + text.len() + 1);
            format!("\r{heading} {text}{}", " ".repeat(width))
        } else {
            format!(" {text}")
        };
        say(self.out, &line);
        Ok(tested)
    }

    /// `Testing step 31 (about 3 runs left)...`.
    fn heading(&self, trial: &Trial) -> String {
        let entry = &self.entries[trial.entry];
        let end = match trial.entry {
            0 => ", the good end",
            entry if entry == self.entries.len() - 1 => ", the bad end",
            _ => "",
        };
        // What the good end changed does not matter: it is the code the others are compared with.
        let label = match trial.entry {
            0 => self.names.entry(entry),
            _ => self.names.described(entry),
        };
        match trial.reason {
            Reason::First => format!(
                "Testing {label}{end} (about {} run{} left)...",
                trial.runs_left,
                plural(trial.runs_left as usize)
            ),
            Reason::Retry(retry) => format!(
                "Testing {label} again (retry {retry} of {})...",
                self.options.settings.retries
            ),
            Reason::LinkerFileHeld => format!(
                "Testing {label} again (the linker could not write a file held by Windows)..."
            ),
        }
    }

    /// Prepares the copy of the entry of `trial` and runs the tests there. Returns what the engine is told and
    /// the text that ends the entry's line.
    fn attempt(&mut self, trial: &Trial) -> io::Result<(Tested, String)> {
        let entry = self.entries[trial.entry];
        let filled = self.sets.copies_filled();
        let started = Instant::now();
        let names = self.names;
        let prepared = self
            .sets
            .prepare(self.store, self.shadow, entry.tree, self.logs, self.stop)
            .map_err(|error| {
                io::Error::other(format!(
                    "cannot prepare the test copy of {}: {error}",
                    names.entry(&entry)
                ))
            })?;
        let (copy, env) = match prepared {
            Prepared::Ready(set_copy) => {
                clean_root(self.cache)?;
                write_boundaries(self.cache)?;
                let env = set_copy.run_env(std::env::vars_os())?;
                (set_copy.copy().path().to_owned(), env)
            }
            Prepared::CannotTest(failure) => {
                if let InstallFailure::Failed { log, .. } = failure {
                    self.history
                        .last_log
                        .insert(trial.entry, (log.clone(), Output::Install));
                }
                let reason = printable(&failure.to_string());
                let text = format!("cannot test: {reason}");
                return Ok((Tested::CannotTest(reason), text));
            }
            Prepared::Stopped => return Ok((Tested::Stopped, "stopped".to_owned())),
        };
        let installed = (self.sets.copies_filled() != filled).then(|| started.elapsed());
        if self.stop.load(Ordering::SeqCst) {
            return Ok((Tested::Stopped, "stopped".to_owned()));
        }

        let name = copy.file_name().unwrap_or_default().to_owned();
        let first_use = self.seen.insert(name.clone());
        let cold = CopyUse {
            installed: installed.is_some(),
            first_use,
            existed: self.existed.contains(&name),
        };
        let limit = run_limit(trial.limit, self.options.settings.timeout.is_some(), cold);

        self.runs += 1;
        let log_name = format!("run-{}.log", self.runs);
        let log = self.logs.create(&log_name)?;
        let ran = run(
            &self.options.test,
            &copy,
            &env,
            &log,
            limit.limit,
            self.stop,
        )?;
        self.history
            .last_log
            .insert(trial.entry, (self.logs.path().join(log_name), Output::Test));
        if ran.outcome == Outcome::TimedOut && !self.history.timed_out.contains(&trial.entry) {
            self.history.timed_out.push(trial.entry);
        }
        let mut text = outcome_text(&ran, limit.limit);
        if let Some(took) = installed {
            text = format!("installed its dependencies in {}, {text}", seconds(took));
        }
        if let (true, Some(raised)) = (limit.raised, limit.limit) {
            text = format!(
                "{text} (first run in a new test copy: time limit raised to {} s)",
                raised.as_secs()
            );
        }
        Ok((Tested::Ran(ran), text))
    }
}

/// The least time limit of a cold run (see [`run_limit`]).
const COLD_MIN_LIMIT: Duration = Duration::from_secs(10 * 60);

/// What a run needs to know about its copy to pick its time limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CopyUse {
    /// Its dependencies were just installed (the copy was deleted and filled anew).
    installed: bool,
    /// It is used for the first time in this bisect.
    first_use: bool,
    /// It was in the cache folder when the bisect started.
    existed: bool,
}

/// The time limit of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limit {
    limit: Option<Duration>,
    /// Raised because the run is cold.
    raised: bool,
}

/// The time limit of a run in `copy`, whose limit from the engine is `limit`. A cold run, the first in a copy
/// just installed or created in this bisect, holds no build yet: it may compile the whole project (Rust
/// especially), so a limit picked from warm runs would time it out and name the wrong step. Its automatic limit
/// is raised to 10 times as long, and at least 10 minutes. A `--timeout` (`explicit`) applies as it is.
fn run_limit(limit: Option<Duration>, explicit: bool, copy: CopyUse) -> Limit {
    let cold = copy.installed || (copy.first_use && !copy.existed);
    match limit {
        Some(limit) if cold && !explicit => Limit {
            limit: Some(limit.saturating_mul(10).max(COLD_MIN_LIMIT)),
            raised: true,
        },
        limit => Limit {
            limit,
            raised: false,
        },
    }
}

/// How a run ended, as the end of its line says it.
fn outcome_text(ran: &Run, limit: Option<Duration>) -> String {
    let took = seconds(ran.elapsed);
    match ran.outcome {
        Outcome::Passed => format!("passed in {took}"),
        Outcome::Failed(_) if ran.linker_file_held => {
            "failed: the linker could not write a file held by Windows".to_owned()
        }
        Outcome::Failed(exit) => {
            let hint = ran
                .outcome
                .hint()
                .map(|hint| format!(", {hint}"))
                .unwrap_or_default();
            format!("failed ({exit}{hint}) in {took}")
        }
        Outcome::Skipped => format!("cannot test ({}) in {took}", Exit::Code(SKIP_EXIT_CODE)),
        Outcome::TimedOut => Failure::TimedOut(limit.unwrap_or(ran.elapsed)).to_string(),
        Outcome::Stopped => "stopped".to_owned(),
        Outcome::OutputTooLarge => format!("cannot test: {}", Skip::OutputTooLarge),
    }
}

/// `duration` as `4.2 s`.
fn seconds(duration: Duration) -> String {
    format!("{:.1} s", duration.as_secs_f64())
}

/// Runs `work`. With `live`, meanwhile shows the seconds since it started after `heading`, on standard output,
/// rewriting the line every second. Returns what `work` returned and the length of the longest line shown.
fn ticking<T>(heading: &str, live: bool, work: impl FnOnce() -> T) -> (T, usize) {
    if !live {
        return (work(), 0);
    }
    let (done, wait) = mpsc::channel::<()>();
    thread::scope(|scope| {
        let ticker = scope.spawn(move || {
            let started = Instant::now();
            let mut longest = 0;
            for second in 1_u64.. {
                let next = Duration::from_secs(second).saturating_sub(started.elapsed());
                if !matches!(
                    wait.recv_timeout(next),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
                let line = format!("{heading} {second} s");
                longest = longest.max(line.len());
                let mut stdout = io::stdout();
                let _ = write!(stdout, "\r{line}");
                let _ = stdout.flush();
            }
            longest
        });
        let value = work();
        drop(done);
        (value, ticker.join().unwrap_or(0))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bisect::COPIED_KEY;
    use crate::bisect::testing::{links_under, state, token, write};
    use crate::repo::{GIT_ID_FILE, ID_FILE};
    use crate::safe_fs::OwnedDir;
    use crate::snapshot::{Change, FileKind};
    use crate::store::{Event, Session};
    use serde_json::json;

    #[test]
    fn timeouts_are_seconds_or_minutes() {
        assert_eq!(parse_timeout("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_timeout("5m"), Ok(Duration::from_secs(300)));
        for wrong in [
            "",
            "0",
            "0s",
            "-5",
            "1.5",
            "5h",
            "m",
            "s5",
            "99999999999999999999m",
        ] {
            assert!(parse_timeout(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn the_note_on_other_sessions_names_at_most_five() {
        let step = |session: &str, step: u32| Event {
            session_id: session.to_owned(),
            step,
            ts_ms: 0,
            kind: "PostToolUse".to_owned(),
            tool_name: None,
            tool_use_id: None,
            agent_id: None,
            success: None,
            tree_id: None,
            base_tree_id: None,
            files_changed: None,
            payload: serde_json::Value::Null,
        };
        let steps: Vec<Event> = ["s1", "s2", "s3", "s4", "s5", "s6", "s7"]
            .iter()
            .flat_map(|session| [step(session, 3), step(session, 4)])
            .collect();
        let names = Names {
            steps: &steps,
            id_chars: 8,
            across: false,
        };
        let entry = |step| Entry {
            order: 0,
            step,
            kind: super::Kind::Step,
            tree: gix::ObjectId::null(gix::hash::Kind::Sha1),
            base: None,
        };
        let mut out = Vec::new();
        let others: Vec<usize> = (0..steps.len()).collect();
        write_others(&mut out, &names, &others, (&entry(7), &entry(8)), false);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains(
                ": session s1 (2 steps, 3 to 4), session s2 (2 steps, 3 to 4), session s3 (2 steps, 3 to 4), \
                 session s4 (2 steps, 3 to 4), session s5 (2 steps, 3 to 4), .... `yalper bisect --all`"
            ),
            "{out}"
        );
        assert!(!out.contains("s6"), "{out}");
    }

    #[test]
    fn steps_are_numbers_or_a_session_and_a_number() {
        let step = |session: Option<&str>, step| StepRef {
            session: session.map(str::to_owned),
            step,
        };
        assert_eq!(parse_step("12"), Ok(step(None, 12)));
        assert_eq!(parse_step("5f3c:12"), Ok(step(Some("5f3c"), 12)));
        assert_eq!(parse_step("a:b:3"), Ok(step(Some("a:b"), 3)));
        for wrong in ["", ":3", "5f3c:", "5f3c", "-1", "1.5", "5f3c:x"] {
            assert!(parse_step(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn since_is_a_local_date_or_a_date_and_time() {
        let at = |text| parse_since(text).map(|time| time.to_string());
        assert_eq!(at("2026-10-11"), Ok("2026-10-11T00:00:00".to_owned()));
        assert_eq!(at("2026-10-11 14:30"), Ok("2026-10-11T14:30:00".to_owned()));
        for wrong in [
            "",
            "2026-10",
            "2026-13-01",
            "2026-10-11T14:30",
            "2026-10-11 14:30:15",
            "2026-10-11 25:00",
            "11/10/2026",
            "yesterday",
        ] {
            assert!(at(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn the_first_run_in_a_new_or_just_installed_copy_gets_a_raised_limit() {
        let warm = Duration::from_secs(40);
        let raised = |limit| Limit {
            limit: Some(limit),
            raised: true,
        };
        let copy = |installed, first_use, existed| CopyUse {
            installed,
            first_use,
            existed,
        };
        // copy-0 created by this bisect (the project's first): its first run, then the next ones.
        assert_eq!(
            run_limit(Some(warm), false, copy(false, true, false)),
            raised(Duration::from_secs(600))
        );
        assert_eq!(
            run_limit(
                Some(Duration::from_secs(90)),
                false,
                copy(false, true, false)
            ),
            raised(Duration::from_secs(900))
        );
        let as_is = Limit {
            limit: Some(warm),
            raised: false,
        };
        assert_eq!(
            run_limit(Some(warm), false, copy(false, false, false)),
            as_is
        );
        // copy-0 kept from an earlier bisect: its build is there.
        assert_eq!(run_limit(Some(warm), false, copy(false, true, true)), as_is);
        // A copy just installed, used before in this bisect or not.
        assert_eq!(
            run_limit(Some(warm), false, copy(true, false, true)),
            raised(Duration::from_secs(600))
        );
        // `--timeout` applies as it is, and the good end has no limit to raise.
        assert_eq!(run_limit(Some(warm), true, copy(true, true, false)), as_is);
        let none = Limit {
            limit: None,
            raised: false,
        };
        assert_eq!(run_limit(None, false, copy(true, true, false)), none);
    }

    #[test]
    fn exit_codes_follow_git_bisect_and_the_shell() {
        assert_eq!(Ended::Named.exit_code(), 0);
        assert_eq!(Ended::NotNamed.exit_code(), 1);
        assert_eq!(Ended::Stopped.exit_code(), 130);
    }

    /// A git project set up for Yalper with one recorded session: a start, then one step per element of
    /// `steps`, each writing its files (path, content) into the snapshot of the step before.
    fn recorded(root: &Path, steps: &[&[(&str, &str)]]) {
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join(".yalper")).unwrap();
        fs::write(root.join(".yalper").join(ID_FILE), token().as_str()).unwrap();
        fs::write(root.join(".git").join(GIT_ID_FILE), token().as_str()).unwrap();
        let dir = OwnedDir::open(&root.join(".yalper")).unwrap();
        let shadow = ShadowStore::init(&dir, &token()).unwrap();
        let store = Store::open(&dir, &token()).unwrap();
        store.upsert_session(&Session::new("session-1", 0)).unwrap();
        let mut tree = shadow.empty_tree();
        for (index, files) in [&[("README.md", "hello\n")][..]]
            .iter()
            .chain(steps)
            .enumerate()
        {
            let changes: Vec<Change> = files
                .iter()
                .map(|&(path, content)| Change::Upsert {
                    path: path.to_owned(),
                    kind: FileKind::Regular,
                    blob: shadow.write_blob(content.as_bytes()).unwrap(),
                })
                .collect();
            let base = tree;
            tree = shadow.edit_tree(base, &changes).unwrap();
            let kind = if index == 0 {
                "SessionStart"
            } else {
                "PostToolUse"
            };
            store
                .insert_event(&Event {
                    session_id: "session-1".to_owned(),
                    step: index as u32 + 1,
                    ts_ms: 0,
                    kind: kind.to_owned(),
                    tool_name: (index > 0).then(|| "Bash".to_owned()),
                    tool_use_id: None,
                    agent_id: None,
                    success: Some(true),
                    tree_id: Some(tree.to_string()),
                    base_tree_id: Some(base.to_string()),
                    files_changed: Some(files.len() as u32),
                    payload: json!({"tool_input": {"command": "edit"}}),
                })
                .unwrap();
        }
    }

    #[test]
    fn the_stop_flag_kills_the_run_deletes_the_copied_env_files_and_links_and_leaves_the_project_unchanged()
     {
        let project = tempfile::tempdir().unwrap();
        let markers = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let pyproject = "[project]\nname = \"demo\"\n";
        recorded(
            project.path(),
            &[
                &[("pyproject.toml", pyproject)],
                &[("b.txt", "b")],
                &[("c.txt", "c")],
            ],
        );
        write(project.path(), "src/main.rs", "fn main() {}\n");
        write(
            project.path(),
            ".gitignore",
            ".env\nnode_modules/\n.venv/\n",
        );
        write(project.path(), ".env", "SECRET=stop-test-secret\n");
        // Installed dependencies, which `copy-0` links to.
        write(project.path(), "pyproject.toml", pyproject);
        write(project.path(), "node_modules/dep/index.js", "");
        write(project.path(), ".venv/pyvenv.cfg", "home = /usr/bin\n");
        let before = state(project.path());

        let (started, late) = (markers.path().join("started"), markers.path().join("late"));
        let test = if cfg!(windows) {
            format!(
                "if exist .env if exist node_modules\\dep\\index.js if exist .venv\\pyvenv.cfg \
                 (echo.>\"{}\")& ping -n 6 127.0.0.1 >nul& echo.>\"{}\"",
                started.display(),
                late.display()
            )
        } else {
            format!(
                "test -f .env && test -f node_modules/dep/index.js && test -f .venv/pyvenv.cfg && echo > '{}'; \
                 sleep 5; echo > '{}'",
                started.display(),
                late.display()
            )
        };
        let options = Options {
            test,
            // The first step with `pyproject.toml`, next to which `.venv` is linked.
            good: Some(StepRef {
                session: None,
                step: 2,
            }),
            files: Choice {
                env: Some(true),
                ..Choice::default()
            },
            ..Options::default()
        };
        let screen = Screen {
            live: false,
            time_zone: TimeZone::UTC,
        };
        let stop = AtomicBool::new(false);
        let folder = cache_base.path().join("yalper");
        let open_cache = |project: &Path, token: &Token| CacheDir::open_in(&folder, project, token);
        let mut out = Vec::new();
        let begun = Instant::now();
        let (ended, started_at, returned_at) = thread::scope(|scope| {
            let watcher = scope.spawn(|| {
                while !started.exists() && begun.elapsed() < Duration::from_secs(30) {
                    thread::sleep(Duration::from_millis(10));
                }
                let started_at = Instant::now();
                stop.store(true, Ordering::SeqCst);
                started_at
            });
            let ended = command_in(
                project.path(),
                &options,
                &screen,
                &mut out,
                &stop,
                &open_cache,
            );
            let returned_at = Instant::now();
            (ended, watcher.join().unwrap(), returned_at)
        });
        let out = String::from_utf8(out).unwrap();
        assert_eq!(ended, Ok(Ended::Stopped), "{out}");
        assert!(
            returned_at.duration_since(started_at) < Duration::from_secs(3),
            "{out}"
        );
        assert!(
            out.ends_with("Stopped. Your project was not changed.\n"),
            "{out}"
        );
        assert!(out.contains("Testing step 2, the good end"), "{out}");
        // The whole tree was killed: the command never got to its last part, 5 s after its first.
        thread::sleep(
            (started_at + Duration::from_secs(6)).saturating_duration_since(Instant::now()),
        );
        // The run started with the env file and the links to the project's packages and environment in its
        // copy (it writes `started` only then), which are gone.
        assert!(started.exists() && !late.exists(), "{out}");
        assert_eq!(state(project.path()), before);
        assert_eq!(links_under(cache_base.path()), Vec::<PathBuf>::new());
        let cached = state(cache_base.path());
        assert!(cached.keys().any(|path| path.ends_with("copy-0/README.md")));
        assert!(
            !cached.keys().any(|path| path.ends_with(".env")
                || fs::read(path)
                    .is_ok_and(|bytes| bytes.windows(16).any(|w| w == b"stop-test-secret"))),
            "{cached:#?}"
        );
    }

    #[test]
    fn env_files_left_by_a_bisect_that_did_not_end_are_deleted_before_the_next_one_runs() {
        let project = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        // From step 2 on, the step's rules ignore `.env`, so switches keep one left in a copy.
        recorded(
            project.path(),
            &[
                &[(".gitignore", ".env\n")],
                &[("a.txt", "a")],
                &[("b.txt", "b")],
            ],
        );
        write(project.path(), ".gitignore", ".env\n");
        write(project.path(), ".env", "LEFT=1\n");
        let folder = cache_base.path().join("yalper");
        let copy_env = {
            let cache = CacheDir::open_in(&folder, project.path(), &token()).unwrap();
            let yalper = OwnedDir::open(&project.path().join(".yalper")).unwrap();
            let shadow = ShadowStore::open(&yalper, &token()).unwrap();
            let events = Store::open(&yalper, &token()).unwrap();
            let chosen = crate::bisect::Chosen {
                env: true,
                ..Default::default()
            };
            let (mut files, _) = LocalFiles::new(&cache, &chosen, events);
            let mut copy = crate::bisect::TestCopy::open(&cache, 0).unwrap();
            copy.switch(&shadow, shadow.empty_tree()).unwrap();
            assert_eq!(files.sync(&mut copy).unwrap(), 1);
            // Yalper ends without deleting it: killed, or its console closed.
            std::mem::forget(files);
            copy.path().join(".env")
        };
        assert!(copy_env.is_file());

        // The test fails when the copy holds an `.env`.
        let test = if cfg!(windows) {
            "if exist .env (exit /b 1)"
        } else {
            "test ! -f .env"
        };
        let options = Options {
            test: test.to_owned(),
            good: Some(StepRef {
                session: None,
                step: 2,
            }),
            files: Choice {
                env: Some(false),
                ..Choice::default()
            },
            ..Options::default()
        };
        let screen = Screen {
            live: false,
            time_zone: TimeZone::UTC,
        };
        let open_cache = |project: &Path, token: &Token| CacheDir::open_in(&folder, project, token);
        let mut out = Vec::new();
        let ended = command_in(
            project.path(),
            &options,
            &screen,
            &mut out,
            &AtomicBool::new(false),
            &open_cache,
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(ended, Ok(Ended::NotNamed), "{out}");
        assert!(
            out.contains("Testing step 2, the good end (about 2 runs left)... passed"),
            "{out}"
        );
        assert!(out.contains("The bad end, step 4, passes"), "{out}");
        assert!(!copy_env.exists());
        let yalper = OwnedDir::open(&project.path().join(".yalper")).unwrap();
        let events = Store::open(&yalper, &token()).unwrap();
        assert_eq!(events.meta(COPIED_KEY).unwrap(), None);
    }

    #[test]
    fn links_left_in_copy_0_are_removed_at_the_start_of_a_bisect_that_never_uses_it() {
        let project = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let package = r#"{"dependencies": {"dep": "1"}}"#;
        let pyproject = "[project]\nname = \"demo\"\n";
        // Every step has another lockfile than the project: none runs in `copy-0`.
        recorded(
            project.path(),
            &[
                &[("package.json", package), ("package-lock.json", "2")],
                &[("b.txt", "b")],
                &[("c.txt", "c")],
            ],
        );
        write(project.path(), ".gitignore", "node_modules/\n.venv/\n");
        write(project.path(), "package.json", package);
        write(project.path(), "package-lock.json", "1");
        write(project.path(), "pyproject.toml", pyproject);
        write(project.path(), "node_modules/dep/index.js", "");
        write(project.path(), ".venv/pyvenv.cfg", "home = /usr/bin\n");
        let before = state(project.path());
        let folder = cache_base.path().join("yalper");
        let copy_0 = {
            let cache = CacheDir::open_in(&folder, project.path(), &token()).unwrap();
            let yalper = OwnedDir::open(&project.path().join(".yalper")).unwrap();
            let shadow = ShadowStore::open(&yalper, &token()).unwrap();
            let events = Store::open(&yalper, &token()).unwrap();
            let files = [
                ("package.json", package),
                ("package-lock.json", "1"),
                ("pyproject.toml", pyproject),
            ];
            let changes: Vec<Change> = files
                .iter()
                .map(|&(path, content)| Change::Upsert {
                    path: path.to_owned(),
                    kind: FileKind::Regular,
                    blob: shadow.write_blob(content.as_bytes()).unwrap(),
                })
                .collect();
            let tree = shadow.edit_tree(shadow.empty_tree(), &changes).unwrap();
            let mut sets = DependencySets::new(&cache, &events, None, Vec::new()).unwrap();
            let logs = Logs::open(&cache).unwrap();
            let prepared = sets
                .prepare(&events, &shadow, tree, &logs, &AtomicBool::new(false))
                .unwrap();
            let Prepared::Ready(set_copy) = prepared else {
                panic!("not ready");
            };
            let copy_0 = set_copy.copy().path().to_owned();
            // Yalper ends without removing them: killed, or crashed.
            sets.leak_project_links();
            copy_0
        };
        assert_eq!(links_under(&copy_0).len(), 2);

        let options = Options {
            test: "exit 0".to_owned(),
            good: Some(StepRef {
                session: None,
                step: 2,
            }),
            setup: Some("exit 0".to_owned()),
            ..Options::default()
        };
        let screen = Screen {
            live: false,
            time_zone: TimeZone::UTC,
        };
        let open_cache = |project: &Path, token: &Token| CacheDir::open_in(&folder, project, token);
        let mut out = Vec::new();
        let ended = command_in(
            project.path(),
            &options,
            &screen,
            &mut out,
            &AtomicBool::new(false),
            &open_cache,
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(ended, Ok(Ended::NotNamed), "{out}");
        assert!(out.contains("The bad end, step 4, passes"), "{out}");
        assert!(!out.contains("warning"), "{out}");
        assert!(copy_0.with_file_name("copy-1").is_dir());
        assert_eq!(links_under(&copy_0), Vec::<PathBuf>::new());
        assert_eq!(state(project.path()), before);
    }
}
