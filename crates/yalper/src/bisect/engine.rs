//! The bisect engine: given the entries of a session in order, finds the first one whose tests fail, by
//! binary search, the way `git bisect run` does.
//!
//! It knows nothing about files or processes. The caller gives it the tree of each entry (only to tell
//! whether the two ends hold the same code) and a `test` callback that runs the tests on one entry and says
//! how the run ended. Index 0 is the good end (the code at the start), the last index is the bad end.
//! The caller passes only entries that changed files, so two entries next to each other never hold the
//! same tree.
//!
//! - Both ends are tested first, the good end and then the bad end. Then the entry in the middle of the
//!   entries still in doubt is tested, until the first failing entry is found.
//! - A run that cannot test its entry (exit code 125, too much output, dependencies that failed to install,
//!   a linker file held by Windows) skips the entry. The next one tested is the nearest entry to the middle
//!   that is not skipped, after then before it (middle + 1, middle - 1, middle + 2, ...): unlike git, which
//!   picks it pseudo-randomly, the same session is always tested in the same order. When only skipped
//!   entries are left in doubt, the answer is the range of them plus the first failing entry, as in git.
//! - Any exit code other than 0 and 125 is a failure, codes above 127 included (git aborts on those).
//! - A failing or timed-out run is repeated up to [`Settings::retries`] times. Any pass makes the entry
//!   pass, and it is listed as flaky. Skips are never repeated.
//! - Each run has a time limit, from [`Settings::timeout`] or else picked from the runs of the two ends (see
//!   [`automatic_limit`]). A run over the limit counts as a failure.

use std::fmt;
use std::io;
use std::time::Duration;

use super::run::{Exit, MAX_LOG_BYTES, Outcome, Run};

/// How many times a run is repeated when it failed only because the linker could not write a file still
/// held by Windows (see [`Run::linker_file_held`]). If it still fails that way, the entry cannot be tested:
/// counting it as failed could name a good step as the breaking one.
pub const LINKER_RERUNS: u32 = 2;

/// The least room [`automatic_limit`] gives over the longest end run.
const MIN_HEADROOM: Duration = Duration::from_secs(30);

/// What the user chose for a bisect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// `--timeout`: the time limit of every run, the ends included. `None` picks it automatically.
    pub timeout: Option<Duration>,
    /// `--retries`: how many times a failing or timed-out run is repeated.
    pub retries: u32,
}

/// One run the engine asks the `test` callback for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trial {
    /// The index of the entry to test.
    pub entry: usize,
    /// The time limit of the run. `None` only for the good end without `--timeout`.
    pub limit: Option<Duration>,
    /// About how many entries are left to test after this one, as `git bisect` estimates it. Repeated runs
    /// are not counted.
    pub runs_left: u32,
    /// Why the entry is run.
    pub reason: Reason,
}

/// Why an entry is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    First,
    /// The previous run failed or timed out: the retry with this number, from 1 to [`Settings::retries`].
    Retry(u32),
    /// The previous run failed because the linker could not write a file held by Windows.
    LinkerFileHeld,
}

/// What the `test` callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tested {
    /// The test command ran.
    Ran(Run),
    /// The entry cannot be tested at all, for example because its dependencies failed to install. The entry
    /// is skipped and not asked for again.
    CannotTest,
    /// The bisect was stopped (Ctrl-C) before the test command ran.
    Stopped,
}

/// How an entry failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Exit(Exit),
    /// The run reached its time limit.
    TimedOut(Duration),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exit(exit) => write!(f, "{exit}"),
            Self::TimedOut(limit) if limit.subsec_nanos() == 0 => {
                write!(f, "timed out after {} s", limit.as_secs())
            }
            Self::TimedOut(limit) => write!(f, "timed out after {:.1} s", limit.as_secs_f64()),
        }
    }
}

/// Why an entry could not be tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The test command exited with 125.
    ExitCode,
    /// The test command printed more than [`MAX_LOG_BYTES`].
    OutputTooLarge,
    /// The `test` callback returned [`Tested::CannotTest`].
    CannotTest,
    /// Every run failed because the linker could not write a file held by Windows (see [`LINKER_RERUNS`]).
    LinkerFileHeld,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExitCode => write!(f, "the test command exited with 125 (cannot test)"),
            Self::OutputTooLarge => write!(
                f,
                "the test command printed more than {} GiB",
                MAX_LOG_BYTES >> 30
            ),
            Self::CannotTest => write!(f, "its dependencies could not be installed"),
            Self::LinkerFileHeld => {
                write!(f, "the linker could not write a file still held by Windows")
            }
        }
    }
}

/// One end of the bisect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Good,
    Bad,
}

/// The answer of a bisect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The first failing entry: the one before it passes.
    Found { entry: usize, failure: Failure },
    /// Skipped entries hide the answer: the first failing entry is one of these, in order. All are skipped
    /// except the last, which failed.
    Range(Vec<usize>),
    /// The good end already fails.
    GoodEndFails(Failure),
    /// The bad end passes.
    BadEndPasses,
    /// An end cannot be tested.
    EndSkipped(End, Skip),
    /// The two ends hold the same code, or there is only one entry.
    NothingToTest,
    /// The bisect was stopped (Ctrl-C).
    Stopped,
}

/// What [`bisect`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub answer: Answer,
    /// Entries that failed and then passed on a retry, in the order they were tested.
    pub flaky: Vec<usize>,
    /// Entries that could not be tested, in the order they were tested.
    pub skipped: Vec<(usize, Skip)>,
    /// How many times the test command ran, repeated runs included.
    pub runs: u32,
}

/// The time limit of the runs after an end run, when the longest end run that finished took `longest`:
/// three times as long, and at least 30 s more. Room for a step that adds tests or recompiles, while a step
/// that makes the tests hang costs at most a few normal runs.
pub fn automatic_limit(longest: Duration) -> Duration {
    (longest * 3).max(longest + MIN_HEADROOM)
}

/// Finds the first failing entry of `trees` (see the module documentation). `test` runs the tests on the
/// entry of a [`Trial`], with its time limit, and checks the stop flag. Fails only with the errors of `test`.
pub fn bisect<T, F>(trees: &[T], settings: Settings, test: F) -> io::Result<Report>
where
    T: PartialEq,
    F: FnMut(&Trial) -> io::Result<Tested>,
{
    let mut engine = Engine {
        settings,
        test,
        longest_end_run: None,
        report: Report {
            answer: Answer::NothingToTest,
            flaky: Vec::new(),
            skipped: Vec::new(),
            runs: 0,
        },
    };
    engine.report.answer = engine.search(trees)?;
    Ok(engine.report)
}

/// What the runs of one entry say about it.
enum Verdict {
    Passed,
    Failed(Failure),
    Skipped(Skip),
    Stopped,
}

struct Engine<F> {
    settings: Settings,
    test: F,
    /// The longest end run that finished (not over its time limit), for [`automatic_limit`].
    longest_end_run: Option<Duration>,
    report: Report,
}

impl<F: FnMut(&Trial) -> io::Result<Tested>> Engine<F> {
    fn search<T: PartialEq>(&mut self, trees: &[T]) -> io::Result<Answer> {
        let (mut good, mut bad) = match trees {
            [first, .., last] if first != last => (0, trees.len() - 1),
            _ => return Ok(Answer::NothingToTest),
        };
        let after_ends = runs_to_search(bad - good);
        match self.verdict(good, 1 + after_ends, true)? {
            Verdict::Passed => {}
            Verdict::Failed(failure) => return Ok(Answer::GoodEndFails(failure)),
            Verdict::Skipped(skip) => return Ok(Answer::EndSkipped(End::Good, skip)),
            Verdict::Stopped => return Ok(Answer::Stopped),
        }
        let mut bad_failure = match self.verdict(bad, after_ends, true)? {
            Verdict::Failed(failure) => failure,
            Verdict::Passed => return Ok(Answer::BadEndPasses),
            Verdict::Skipped(skip) => return Ok(Answer::EndSkipped(End::Bad, skip)),
            Verdict::Stopped => return Ok(Answer::Stopped),
        };
        let mut skipped = vec![false; trees.len()];
        loop {
            let Some(entry) = next_entry(good, bad, &skipped) else {
                return Ok(if bad == good + 1 {
                    Answer::Found {
                        entry: bad,
                        failure: bad_failure,
                    }
                } else {
                    Answer::Range((good + 1..=bad).collect())
                });
            };
            match self.verdict(entry, estimate_steps(bad - good), false)? {
                Verdict::Passed => good = entry,
                Verdict::Failed(failure) => (bad, bad_failure) = (entry, failure),
                Verdict::Skipped(_) => skipped[entry] = true,
                Verdict::Stopped => return Ok(Answer::Stopped),
            }
        }
    }

    /// Runs the tests on `entry`, repeated as the settings and the linker rule say, and records what the
    /// report lists about it.
    fn verdict(&mut self, entry: usize, runs_left: u32, end: bool) -> io::Result<Verdict> {
        let limit = self.limit();
        let mut reason = Reason::First;
        let mut retries = 0;
        let mut linker_reruns = 0;
        // The latest failure: a later run that cannot test the entry does not hide it.
        let mut failure = None;
        loop {
            let trial = Trial {
                entry,
                limit,
                runs_left,
                reason,
            };
            let run = match (self.test)(&trial)? {
                Tested::Ran(run) => run,
                Tested::CannotTest => {
                    return Ok(self.cannot_test(entry, failure, Skip::CannotTest));
                }
                Tested::Stopped => return Ok(Verdict::Stopped),
            };
            self.report.runs += 1;
            if end && !matches!(run.outcome, Outcome::TimedOut | Outcome::OutputTooLarge) {
                self.longest_end_run = self.longest_end_run.max(Some(run.elapsed));
            }
            let failed = match run.outcome {
                Outcome::Passed => {
                    if failure.is_some() {
                        self.report.flaky.push(entry);
                    }
                    return Ok(Verdict::Passed);
                }
                Outcome::Stopped => return Ok(Verdict::Stopped),
                Outcome::Skipped => return Ok(self.cannot_test(entry, failure, Skip::ExitCode)),
                Outcome::OutputTooLarge => {
                    return Ok(self.cannot_test(entry, failure, Skip::OutputTooLarge));
                }
                Outcome::Failed(_) if run.linker_file_held => {
                    if linker_reruns == LINKER_RERUNS {
                        return Ok(self.cannot_test(entry, failure, Skip::LinkerFileHeld));
                    }
                    linker_reruns += 1;
                    reason = Reason::LinkerFileHeld;
                    continue;
                }
                Outcome::Failed(exit) => Failure::Exit(exit),
                Outcome::TimedOut => Failure::TimedOut(limit.unwrap_or(run.elapsed)),
            };
            if retries == self.settings.retries {
                return Ok(Verdict::Failed(failed));
            }
            failure = Some(failed);
            retries += 1;
            reason = Reason::Retry(retries);
        }
    }

    /// The verdict on `entry` when a run could not test it: an earlier failure of the entry stands,
    /// otherwise the entry is skipped.
    fn cannot_test(&mut self, entry: usize, failure: Option<Failure>, skip: Skip) -> Verdict {
        match failure {
            Some(failure) => Verdict::Failed(failure),
            None => {
                self.report.skipped.push((entry, skip));
                Verdict::Skipped(skip)
            }
        }
    }

    /// The time limit of the next run: `--timeout`, or else none before any end run finished (the good end
    /// runs first), or else [`automatic_limit`].
    fn limit(&self) -> Option<Duration> {
        self.settings
            .timeout
            .or(self.longest_end_run.map(automatic_limit))
    }
}

/// The entry to test between `good` and `bad` (both excluded): the middle one, or when it is skipped the
/// nearest one that is not, looking after then before it. `None` when there is none.
fn next_entry(good: usize, bad: usize, skipped: &[bool]) -> Option<usize> {
    let middle = good + (bad - good) / 2;
    (0..bad - good)
        .flat_map(|distance| [Some(middle + distance), middle.checked_sub(distance)])
        .flatten()
        .find(|&entry| good < entry && entry < bad && !skipped[entry])
}

/// About how many entries are left to test after the next one, when `all` entries may be the first failing
/// one: `estimate_bisect_steps` of git's `bisect.c`, which is about log2 of `all`, one less when `all` is
/// just above a power of two.
fn estimate_steps(all: usize) -> u32 {
    if all < 3 {
        return 0;
    }
    let n = all.ilog2();
    let below = 1 << n;
    if below < 3 * (all - below) { n } else { n - 1 }
}

/// About how many entries are tested between the ends, when `all` entries may be the first failing one.
fn runs_to_search(all: usize) -> u32 {
    if all < 2 { 0 } else { estimate_steps(all) + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn ran(outcome: Outcome) -> Tested {
        ran_in(outcome, SECOND)
    }

    fn ran_in(outcome: Outcome, elapsed: Duration) -> Tested {
        Tested::Ran(Run {
            outcome,
            elapsed,
            linker_file_held: false,
        })
    }

    fn linker_file_held() -> Tested {
        Tested::Ran(Run {
            outcome: FAILED,
            elapsed: SECOND,
            linker_file_held: true,
        })
    }

    const FAILED: Outcome = Outcome::Failed(Exit::Code(1));

    /// Distinct trees for `n` entries.
    fn trees(n: usize) -> Vec<usize> {
        (0..n).collect()
    }

    /// Bisects `n` entries whose tests pass before `breaking` and fail from it on, except for the entries
    /// `outcome` gives another result for. Also returns every trial, in order.
    fn bisect_with(
        n: usize,
        breaking: usize,
        settings: Settings,
        mut outcome: impl FnMut(&Trial) -> Option<Tested>,
    ) -> (Report, Vec<Trial>) {
        let mut trials = Vec::new();
        let report = bisect(&trees(n), settings, |trial| {
            trials.push(*trial);
            Ok(outcome(trial).unwrap_or_else(|| {
                ran(if trial.entry < breaking {
                    Outcome::Passed
                } else {
                    FAILED
                })
            }))
        })
        .unwrap();
        (report, trials)
    }

    fn entries(trials: &[Trial]) -> Vec<usize> {
        trials.iter().map(|trial| trial.entry).collect()
    }

    #[test]
    fn finds_every_breaking_entry_in_about_log2_runs() {
        for n in 2..=200_usize {
            let most = n.next_power_of_two().trailing_zeros() + 2;
            for breaking in 1..n {
                let (report, trials) = bisect_with(n, breaking, Settings::default(), |_| None);
                assert_eq!(
                    report.answer,
                    Answer::Found {
                        entry: breaking,
                        failure: Failure::Exit(Exit::Code(1))
                    },
                    "{n} entries, breaking at {breaking}"
                );
                assert!(report.runs <= most, "{n} entries, breaking at {breaking}");
                assert_eq!(report.runs as usize, trials.len());
                assert_eq!(entries(&trials[..2]), [0, n - 1], "the ends go first");
                assert!(report.flaky.is_empty() && report.skipped.is_empty());
            }
        }
    }

    #[test]
    fn nothing_to_test_when_the_ends_hold_the_same_code() {
        for trees in [vec![], vec![7], vec![7, 8, 7]] {
            let report = bisect(&trees, Settings::default(), |_| panic!("no run")).unwrap();
            assert_eq!(report.answer, Answer::NothingToTest);
            assert_eq!(report.runs, 0);
        }
    }

    /// The range `git bisect` gives: from the entry after the last one not skipped before the break, to the
    /// first one not skipped from the break on.
    fn git_answer(n: usize, breaking: usize, skipped: &[bool]) -> Answer {
        let good = (0..breaking).rev().find(|&i| !skipped[i]).unwrap();
        let bad = (breaking..n).find(|&i| !skipped[i]).unwrap();
        if bad == good + 1 {
            Answer::Found {
                entry: bad,
                failure: Failure::Exit(Exit::Code(1)),
            }
        } else {
            Answer::Range((good + 1..=bad).collect())
        }
    }

    #[test]
    fn skips_give_the_range_git_gives() {
        for n in 3..=10 {
            // Every set of skipped entries between the ends.
            for mask in 0..1u32 << (n - 2) {
                let skipped: Vec<bool> = (0..n)
                    .map(|i| i > 0 && i < n - 1 && mask & 1 << (i - 1) != 0)
                    .collect();
                for breaking in 1..n {
                    let skip = |trial: &Trial| skipped[trial.entry].then(|| ran(Outcome::Skipped));
                    let settings = Settings {
                        retries: 2,
                        ..Settings::default()
                    };
                    let (report, trials) = bisect_with(n, breaking, settings, skip);
                    let case = format!("{n} entries, skipped {mask:b}, breaking at {breaking}");
                    assert_eq!(report.answer, git_answer(n, breaking, &skipped), "{case}");
                    // A skipped entry runs once, and is not tested again.
                    for (entry, _) in &report.skipped {
                        assert!(skipped[*entry], "{case}");
                        let runs = trials.iter().filter(|trial| trial.entry == *entry);
                        assert_eq!(runs.count(), 1, "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn after_a_skip_the_nearest_entry_to_the_middle_is_tested() {
        // Entries 0 to 10: the middle is 5, then 6, 4, 7, 3.
        let skipped = [5, 6, 4, 7];
        let (report, trials) = bisect_with(11, 2, Settings::default(), |trial| {
            skipped
                .contains(&trial.entry)
                .then(|| ran(Outcome::Skipped))
        });
        assert_eq!(entries(&trials[..7]), [0, 10, 5, 6, 4, 7, 3]);
        assert_eq!(
            report.skipped,
            skipped.map(|entry| (entry, Skip::ExitCode)).to_vec()
        );
        assert!(matches!(report.answer, Answer::Found { entry: 2, .. }));
    }

    #[test]
    fn every_kind_of_cannot_test_is_a_skip() {
        let cases = [
            (ran(Outcome::Skipped), Skip::ExitCode),
            (ran(Outcome::OutputTooLarge), Skip::OutputTooLarge),
            (Tested::CannotTest, Skip::CannotTest),
            (linker_file_held(), Skip::LinkerFileHeld),
        ];
        for (tested, skip) in cases {
            let (report, _) = bisect_with(3, 2, Settings::default(), |trial| {
                (trial.entry == 1).then_some(tested)
            });
            assert_eq!(report.answer, Answer::Range(vec![1, 2]));
            assert_eq!(report.skipped, [(1, skip)]);
        }
    }

    #[test]
    fn an_end_that_cannot_be_tested_ends_the_bisect() {
        let (report, trials) = bisect_with(5, 2, Settings::default(), |trial| {
            (trial.entry == 0).then_some(ran(Outcome::Skipped))
        });
        assert_eq!(report.answer, Answer::EndSkipped(End::Good, Skip::ExitCode));
        assert_eq!(entries(&trials), [0]);
        let (report, trials) = bisect_with(5, 2, Settings::default(), |trial| {
            (trial.entry == 4).then_some(Tested::CannotTest)
        });
        assert_eq!(
            report.answer,
            Answer::EndSkipped(End::Bad, Skip::CannotTest)
        );
        assert_eq!(entries(&trials), [0, 4]);
    }

    #[test]
    fn a_failing_good_end_or_a_passing_bad_end_ends_the_bisect() {
        let (report, trials) = bisect_with(5, 0, Settings::default(), |_| None);
        assert_eq!(
            report.answer,
            Answer::GoodEndFails(Failure::Exit(Exit::Code(1)))
        );
        assert_eq!(entries(&trials), [0]);
        let (report, trials) = bisect_with(5, 5, Settings::default(), |_| None);
        assert_eq!(report.answer, Answer::BadEndPasses);
        assert_eq!(entries(&trials), [0, 4]);
    }

    #[test]
    fn exit_codes_above_127_and_signals_are_failures() {
        let crash = Exit::Code(0xC000_0005_u32 as i32);
        for exit in [Exit::Code(139), Exit::Code(255), crash, Exit::Signal(9)] {
            let (report, _) = bisect_with(9, 3, Settings::default(), |trial| {
                (trial.entry >= 3).then_some(ran(Outcome::Failed(exit)))
            });
            assert_eq!(
                report.answer,
                Answer::Found {
                    entry: 3,
                    failure: Failure::Exit(exit)
                }
            );
        }
    }

    #[test]
    fn a_failure_that_passes_on_a_retry_is_a_pass_listed_as_flaky() {
        // Entry 2 fails on its first run only.
        let flaky = |trial: &Trial| {
            (trial.entry == 2 && trial.reason == Reason::First).then_some(ran(FAILED))
        };
        let settings = Settings {
            retries: 1,
            ..Settings::default()
        };
        let (report, trials) = bisect_with(5, 3, settings, flaky);
        assert!(matches!(report.answer, Answer::Found { entry: 3, .. }));
        assert_eq!(report.flaky, [2]);
        let reasons: Vec<_> = trials.iter().filter(|trial| trial.entry == 2).collect();
        assert_eq!(reasons[1].reason, Reason::Retry(1));
        // Without retries, the same failure counts.
        let (report, _) = bisect_with(5, 3, Settings::default(), flaky);
        assert!(matches!(report.answer, Answer::Found { entry: 2, .. }));
        assert!(report.flaky.is_empty());
    }

    #[test]
    fn retries_apply_to_the_ends() {
        let settings = Settings {
            retries: 2,
            ..Settings::default()
        };
        // The good end passes on its last retry.
        let (report, trials) = bisect_with(3, 2, settings, |trial| {
            (trial.entry == 0 && trial.reason != Reason::Retry(2)).then_some(ran(FAILED))
        });
        assert!(matches!(report.answer, Answer::Found { entry: 2, .. }));
        assert_eq!(report.flaky, [0]);
        // The bad end fails every time: three runs.
        assert_eq!(entries(&trials), [0, 0, 0, 2, 2, 2, 1]);
        assert_eq!(report.runs, 7);
    }

    #[test]
    fn a_timed_out_run_is_a_failure_and_is_retried() {
        let settings = Settings {
            timeout: Some(Duration::from_secs(30)),
            retries: 1,
        };
        let (report, trials) = bisect_with(9, 9, settings, |trial| {
            (trial.entry >= 5).then_some(ran(Outcome::TimedOut))
        });
        let failure = Failure::TimedOut(Duration::from_secs(30));
        assert_eq!(report.answer, Answer::Found { entry: 5, failure });
        assert_eq!(failure.to_string(), "timed out after 30 s");
        assert_eq!(
            trials.iter().filter(|trial| trial.entry == 5).count(),
            2,
            "retried once"
        );
    }

    #[test]
    fn a_held_linker_file_is_run_again_then_counts_as_cannot_test() {
        // Held once: the rerun decides, and the entry is not flaky.
        let mut held = true;
        let (report, trials) = bisect_with(3, 2, Settings::default(), |trial| {
            (trial.entry == 1 && std::mem::take(&mut held)).then(linker_file_held)
        });
        assert!(matches!(report.answer, Answer::Found { entry: 2, .. }));
        assert!(report.flaky.is_empty());
        assert_eq!(trials[3].reason, Reason::LinkerFileHeld);
        // Always held: the first run and LINKER_RERUNS more, then a skip. Retries are not used for it.
        let settings = Settings {
            retries: 3,
            ..Settings::default()
        };
        let (report, trials) = bisect_with(3, 2, settings, |trial| {
            (trial.entry == 1).then(linker_file_held)
        });
        assert_eq!(report.answer, Answer::Range(vec![1, 2]));
        assert_eq!(report.skipped, [(1, Skip::LinkerFileHeld)]);
        let runs = trials.iter().filter(|trial| trial.entry == 1).count();
        assert_eq!(runs as u32, 1 + LINKER_RERUNS);
        // A real failure before it stands.
        let (report, _) = bisect_with(3, 2, settings, |trial| {
            (trial.entry == 1).then(|| match trial.reason {
                Reason::First => ran(FAILED),
                _ => linker_file_held(),
            })
        });
        assert!(matches!(report.answer, Answer::Found { entry: 1, .. }));
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn a_skip_after_a_failure_keeps_the_failure() {
        let settings = Settings {
            retries: 1,
            ..Settings::default()
        };
        let (report, _) = bisect_with(3, 2, settings, |trial| {
            (trial.entry == 1).then(|| match trial.reason {
                Reason::First => ran(FAILED),
                _ => ran(Outcome::Skipped),
            })
        });
        assert!(matches!(report.answer, Answer::Found { entry: 1, .. }));
    }

    #[test]
    fn a_stop_ends_the_bisect() {
        for stopped in [Tested::Stopped, ran(Outcome::Stopped)] {
            let (report, trials) = bisect_with(9, 4, Settings::default(), |trial| {
                (trial.entry == 4).then_some(stopped)
            });
            assert_eq!(report.answer, Answer::Stopped);
            assert_eq!(trials.last().unwrap().entry, 4);
        }
    }

    #[test]
    fn the_automatic_limit_is_three_times_the_longest_end_run_and_at_least_30_s_more() {
        let secs = Duration::from_secs;
        assert_eq!(automatic_limit(secs(0)), secs(30));
        assert_eq!(automatic_limit(secs(5)), secs(35));
        assert_eq!(automatic_limit(secs(15)), secs(45));
        assert_eq!(automatic_limit(secs(20)), secs(60));
        assert_eq!(automatic_limit(secs(100)), secs(300));
    }

    #[test]
    fn the_limit_comes_from_the_ends() {
        let secs = Duration::from_secs;
        let elapsed = |entry: usize| match entry {
            0 => secs(10),
            8 => secs(20),
            _ => SECOND,
        };
        let (_, trials) = bisect_with(9, 3, Settings::default(), |trial| {
            let outcome = if trial.entry < 3 {
                Outcome::Passed
            } else {
                FAILED
            };
            Some(ran_in(outcome, elapsed(trial.entry)))
        });
        // The good end has no limit, the bad end one from the good end, the others one from the longer end.
        let limits: Vec<_> = trials.iter().map(|trial| trial.limit).collect();
        assert_eq!(limits[..3], [None, Some(secs(40)), Some(secs(60))]);
        assert!(limits[2..].iter().all(|limit| *limit == Some(secs(60))));

        // A bad end over its limit does not count: the good end alone sets it.
        let (_, trials) = bisect_with(9, 3, Settings::default(), |trial| {
            let tested = match trial.entry {
                0 => ran_in(Outcome::Passed, secs(10)),
                8 => ran_in(Outcome::TimedOut, secs(40)),
                _ => return None,
            };
            Some(tested)
        });
        assert!(
            trials[1..]
                .iter()
                .all(|trial| trial.limit == Some(secs(40)))
        );
    }

    #[test]
    fn timeout_overrides_the_automatic_limit_for_every_run() {
        let settings = Settings {
            timeout: Some(Duration::from_secs(7)),
            retries: 0,
        };
        let (_, trials) = bisect_with(9, 3, settings, |trial| {
            let outcome = if trial.entry < 3 {
                Outcome::Passed
            } else {
                FAILED
            };
            Some(ran_in(outcome, Duration::from_secs(100)))
        });
        assert!(
            trials
                .iter()
                .all(|trial| trial.limit == Some(Duration::from_secs(7)))
        );
    }

    #[test]
    fn runs_left_follows_git_and_does_not_count_retries() {
        // estimate_bisect_steps of git for these numbers of candidates.
        let git = [
            (1, 0),
            (2, 0),
            (3, 1),
            (4, 1),
            (5, 1),
            (6, 2),
            (8, 2),
            (40, 4),
        ];
        for (all, steps) in git {
            assert_eq!(estimate_steps(all), steps, "{all} candidates");
        }
        assert_eq!(estimate_steps(1024), 9);
        assert_eq!(estimate_steps(1100), 9);
        assert_eq!(estimate_steps(1500), 10);

        // 41 entries: 40 candidates after the good end.
        let settings = Settings {
            retries: 1,
            ..Settings::default()
        };
        let (_, trials) = bisect_with(41, 23, settings, |_| None);
        let left: Vec<_> = trials
            .iter()
            .filter(|trial| trial.reason == Reason::First)
            .map(|trial| (trial.entry, trial.runs_left))
            .collect();
        assert_eq!(left[..3], [(0, 6), (40, 5), (20, 4)]);
        // A retry repeats the estimate of its entry.
        for pair in trials.windows(2) {
            if let Reason::Retry(_) = pair[1].reason {
                assert_eq!(pair[0].runs_left, pair[1].runs_left);
            }
        }
    }

    #[test]
    fn reasons_and_failures_display_clearly() {
        assert_eq!(
            Failure::TimedOut(Duration::from_millis(1500)).to_string(),
            "timed out after 1.5 s"
        );
        assert_eq!(Failure::Exit(Exit::Code(2)).to_string(), "exit code 2");
        assert_eq!(
            Skip::OutputTooLarge.to_string(),
            "the test command printed more than 1 GiB"
        );
    }
}
