//! `yalper bisect`: names the step of a recorded session that broke the tests.
//!
//! It reads the session's snapshots ([`session_entries`]), opens the project's cache folder, and lets the engine
//! ([`bisect`]) pick the entries to test. Each run goes through the same protections: the step's copy and
//! dependencies from [`DependencySets::prepare`], the cache folder's root cleaned and its boundary files written,
//! the environment of [`SetCopy::run_env`](super::SetCopy::run_env), and [`run`] with the stop flag of Ctrl-C.
//! Nothing is written into the project: only the `meta` table of its event log records where the bisect data is.
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

use jiff::tz::TimeZone;

use super::entries::{Entries, Entry, session_entries};
use super::{
    Answer, CACHE_DIR_KEY, CacheDir, DependencySets, End, Exit, Failure, InstallFailure, Logs,
    Outcome, Prepared, Reason, Report, Run, SKIP_EXIT_CODE, Settings, Skip, Tested, Trial, bisect,
    clean_root, log_tail, remove_stale, run, write_boundaries,
};
use crate::init::{printable, say};
use crate::log::{NOTHING_RECORDED, find, find_session, plural, short_id, short_id_chars};
use crate::redact::{redact_str, strip_escapes};
use crate::repo::Token;
use crate::show::{self, Changes, Format, StepView};
use crate::snapshot::ShadowStore;
use crate::store::Store;

/// The most `--retries` accepted: more runs of a failing step would rather hide a real failure.
pub const MAX_RETRIES: u32 = 10;

/// How many lines of the end of a log are shown when an end of the bisect does not behave as expected.
const TAIL_LINES: usize = 20;

/// The prefix of each line of a log shown on screen, so that no line a test prints can pass for Yalper's own.
const TAIL_PREFIX: &str = "  | ";

/// What the user asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The command that runs the tests.
    pub test: String,
    /// The session whose id starts with this, instead of the most recently active one.
    pub session: Option<String>,
    /// `--good`: a step where the tests pass.
    pub good: Option<u32>,
    /// `--bad`: a step where the tests fail.
    pub bad: Option<u32>,
    /// `--setup`: the command that installs the dependencies of a copy.
    pub setup: Option<String>,
    pub settings: Settings,
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
    let events = store.events_without_output(&session.id).map_err(failed)?;
    let shadow = ShadowStore::open(&yalper.dir, &yalper.token)
        .map_err(|error| format!("cannot read the snapshots: {error}"))?;
    let Entries {
        entries,
        bad_step,
        missing,
    } = session_entries(&events, &shadow, options.good, options.bad)
        .map_err(|why| format!("cannot bisect session {short}: {why}"))?;
    let (good_end, bad_end) = (entries[0], entries[entries.len() - 1]);
    if entries.len() < 2 || good_end.tree == bad_end.tree {
        write_missing(out, &missing);
        say(
            out,
            &format!(
                "Nothing to test: the code at step {bad_step} of session {short} is the same as at {}, \
                 the good end.",
                good_end.label()
            ),
        );
        return Ok(Ended::NotNamed);
    }

    let project = yalper
        .dir
        .path()
        .parent()
        .ok_or("cannot find the project folder above .yalper")?;
    let cache = open_cache(project, &yalper.token).map_err(|error| error.to_string())?;
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
    write_missing(out, &missing);
    store
        .set_meta(CACHE_DIR_KEY, Some(&cache.meta_value()))
        .map_err(|error| format!("cannot record where the bisect data is: {error}"))?;
    remove_stale(&cache);
    let io_failed = |error: io::Error| error.to_string();
    clean_root(&cache).map_err(io_failed)?;
    write_boundaries(&cache).map_err(io_failed)?;
    let logs = Logs::open(&cache).map_err(io_failed)?;
    let existed = copies_in(&cache).map_err(io_failed)?;
    let sets = DependencySets::new(
        &cache,
        &store,
        options.setup.clone(),
        std::env::vars_os().collect(),
    )
    .map_err(|error| error.to_string())?;
    let candidates = entries.len() - 1;
    say(
        out,
        &format!(
            "Bisecting session {short}: {candidates} step{} changed files after {} (the good end), up to \
             {} (the bad end).",
            plural(candidates),
            good_end.label(),
            bad_end.label()
        ),
    );

    let mut tester = Tester {
        options,
        entries: &entries,
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
    let report = bisect(&trees, options.settings, |trial| tester.test(trial))
        .map_err(|error| error.to_string())?;
    let history = tester.history;
    say(out, "");
    let ended = write_answer(out, &report, &entries, &history, &short);
    if ended == Ended::Stopped {
        return Ok(ended);
    }

    if let Answer::Found { entry, .. } = report.answer {
        let (good, breaking) = (entries[entry - 1], entries[entry]);
        if breaking.base != Some(good.tree) {
            say(
                out,
                &format!(
                    "Note: the diff below goes from {}, the last code that passed, so it also holds changes \
                     made outside step {} (by steps without a snapshot, or by another session).",
                    good.label(),
                    breaking.step
                ),
            );
        }
        say(out, "");
        write_breaking_step(
            out,
            start,
            &yalper,
            &store,
            &shadow,
            (&session.id, &short),
            (good, breaking),
            screen,
        )?;
    }
    Ok(ended)
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
    (session_id, short): (&str, &str),
    (good, breaking): (Entry, Entry),
    screen: &Screen,
) -> Result<(), String> {
    let failed = |error: crate::store::Error| format!("cannot read the recordings: {error}");
    let event = store
        .event(session_id, breaking.step)
        .map_err(failed)?
        .ok_or("the step that broke the tests is no longer in the event log")?;
    let steps = store.step_count(&event.session_id).map_err(failed)?;
    let prompt = show::prompt_of(store, &event).map_err(failed)?;
    let roots = show::project_roots(start, yalper);
    let view = StepView {
        event: &event,
        session: short,
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

/// Writes the answer of `report` on `entries` of the session `short`, then what the result should be read with:
/// flaky entries, entries that could not be tested, runs over the time limit. When an end did not behave as
/// expected, the end of its log comes first. Returns how the bisect ended.
fn write_answer(
    out: &mut dyn Write,
    report: &Report,
    entries: &[Entry],
    history: &History,
    short: &str,
) -> Ended {
    let label = |entry: usize| entries[entry].label();
    let runs = format!(
        "Found in {} run{}.",
        report.runs,
        plural(report.runs as usize)
    );
    let last = entries.len() - 1;
    let ended = match &report.answer {
        Answer::Found { entry, failure } => {
            let breaking = entries[*entry].step;
            say(
                out,
                &format!(
                    "Step {breaking} broke the tests: they pass at {} and fail at step {breaking} ({failure}). \
                     {runs}",
                    label(entry - 1)
                ),
            );
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
            say(
                out,
                &format!(
                    "One of steps {} to {} broke the tests, but some of them could not be tested: they pass \
                     at {} and fail at {} ({failure}). {runs}",
                    entries[first].step,
                    entries[end].step,
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
                say(out, &format!("  {}: {what}", label(entry)));
            }
            say(
                out,
                &format!("`yalper show <step> --session {short}` shows each step."),
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
                     <step>, or check the test command.",
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
                     where the tests fail with --bad <step>, or check the test command.",
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
                     {which} end with {option} <step>.",
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

/// Writes a note naming the steps between the ends whose snapshot is no longer in the snapshot store.
fn write_missing(out: &mut dyn Write, missing: &[u32]) {
    if missing.is_empty() {
        return;
    }
    let steps: Vec<String> = missing.iter().map(u32::to_string).collect();
    say(
        out,
        &format!(
            "Note: the snapshot of step{} {} is no longer in the snapshot store, so {} not tested.",
            plural(missing.len()),
            steps.join(", "),
            if missing.len() == 1 {
                "it is"
            } else {
                "they are"
            }
        ),
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
        let entry = self.entries[trial.entry];
        let end = match trial.entry {
            0 => ", the good end",
            entry if entry == self.entries.len() - 1 => ", the bad end",
            _ => "",
        };
        let label = entry.label();
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
        let prepared = self
            .sets
            .prepare(self.store, self.shadow, entry.tree, self.logs, self.stop)
            .map_err(|error| {
                io::Error::other(format!(
                    "cannot prepare the test copy of {}: {error}",
                    entry.label()
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
    use crate::bisect::testing::{state, token, write};
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
    fn the_stop_flag_kills_the_run_and_leaves_the_project_unchanged() {
        let project = tempfile::tempdir().unwrap();
        let markers = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        recorded(
            project.path(),
            &[&[("a.txt", "a")], &[("b.txt", "b")], &[("c.txt", "c")]],
        );
        write(project.path(), "src/main.rs", "fn main() {}\n");
        let before = state(project.path());

        let (started, late) = (markers.path().join("started"), markers.path().join("late"));
        let test = if cfg!(windows) {
            format!(
                "echo.>\"{}\"& ping -n 6 127.0.0.1 >nul& echo.>\"{}\"",
                started.display(),
                late.display()
            )
        } else {
            format!(
                "echo > '{}'; sleep 5; echo > '{}'",
                started.display(),
                late.display()
            )
        };
        let options = Options {
            test,
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
        assert!(out.contains("Testing step 1, the good end"), "{out}");
        // The whole tree was killed: the command never got to its last part, 5 s after its first.
        thread::sleep(
            (started_at + Duration::from_secs(6)).saturating_duration_since(Instant::now()),
        );
        assert!(started.exists() && !late.exists(), "{out}");
        assert_eq!(state(project.path()), before);
    }
}
