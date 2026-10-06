//! Records one hook call as the next step of its session: the redacted payload in the event log and, after
//! a prompt or a tool call, a snapshot of the working tree.
//!
//! Everything written comes out of [`redact_json`] first: the whole payload, and the short values also kept
//! in their own columns (session id, tool name, paths). Those columns are taken from the parsed input rather
//! than from the redacted payload, so a redaction that runs out of time or budget can never lose them, and
//! are redacted on their own, so none of them bypasses masking.
//!
//! Claude Code waits for the hook, so the snapshot has a deadline (see [`DEADLINE`]). Redaction runs on its
//! own thread meanwhile, with its own deadline.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::hook::{self, ERRORS_LOG_MAX_BYTES, HookEvent, HookInput};
use crate::redact::redact_json;
use crate::repo::YalperDir;
use crate::snapshot::{self, Base, Pending};
use crate::store::{Error as StoreError, Event, Session, Store, WriterLock};

/// How long after the hook starts its snapshot must be done. A snapshot that is not done by then is abandoned:
/// the step is recorded without a snapshot, the latest snapshot and the stat cache stay as they were (the
/// next snapshot includes this step's changes), and the reason is logged. Waiting for the writer lock, held by
/// the hook of a parallel tool call, counts against it too.
///
/// A normal step takes 10 to 60 ms (1,000 files, see the `hook_latency` benchmark), and walking 20,000 files
/// takes a few hundred milliseconds on a slow Windows machine, so one second leaves room for large projects
/// and slow disks while a pathological project (a huge unignored tree, a slow network drive) costs the agent
/// at most about a second per step. Blobs that an abandoned snapshot already stored are kept, so the next
/// attempt has less to write and usually succeeds.
pub const DEADLINE: Duration = Duration::from_secs(1);

/// Debug builds walk and hash many times slower, and tests run many of them at once.
const DEBUG_DEADLINE_FACTOR: u32 = 10;

/// How long a hook that starts now has for its snapshot: [`DEADLINE`], longer in debug builds.
pub fn snapshot_deadline() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(millis) = std::env::var("YALPER_TEST_DEADLINE_MS")
        .ok()
        .and_then(|millis| millis.parse().ok())
    {
        return Duration::from_millis(millis);
    }
    if cfg!(debug_assertions) {
        DEADLINE * DEBUG_DEADLINE_FACTOR
    } else {
        DEADLINE
    }
}

/// Records `input` in the `.yalper/` directory `yalper`, which must be one `yalper init` created (see
/// [`crate::repo::open_yalper_dir`]), with a snapshot deadline starting now. See [`record_until`].
pub fn record(yalper: &YalperDir, input: HookInput) -> Result<(), String> {
    record_until(yalper, input, Instant::now() + snapshot_deadline())
}

/// Records `input` in `yalper`. Events Yalper does not register for are ignored.
///
/// `SessionStart`, `UserPromptSubmit`, `PostToolUse` and `PostToolUseFailure` take a snapshot, which must be
/// done by `deadline`. `Stop` and `SessionEnd` do not: nothing ran since the last tool call, and `SessionEnd`
/// hooks share a 1.5 s budget. A session the event log does not know yet (Yalper was set up mid-session) is
/// created by its first event.
///
/// If the snapshot fails or is not done in time, the step is still recorded, without a tree, and the reason
/// is returned for the hook to log. Skipped files are logged here as one line. The new latest snapshot and the
/// step are saved in one transaction.
pub fn record_until(yalper: &YalperDir, input: HookInput, deadline: Instant) -> Result<(), String> {
    let dir = &yalper.dir;
    let event = input.event.clone();
    let Some(kind) = event.name() else {
        return Ok(());
    };
    let takes_snapshot = matches!(
        event,
        HookEvent::SessionStart
            | HookEvent::UserPromptSubmit
            | HookEvent::PostToolUse
            | HookEvent::PostToolUseFailure
    );
    let redaction = Redaction::start(input);
    let task = takes_snapshot.then(|| SnapshotTask::start(yalper));

    // Opened before taking the lock: opening a new database takes the lock for a moment itself.
    let store = Store::open(dir, &yalper.token).map_err(|error| error.to_string())?;
    store.set_deadline(Some(deadline));
    let (lock, mut outcome) = match task {
        Some(Ok(task)) => task.finish(yalper, &store, deadline),
        Some(Err(reason)) => (None, Outcome::NotTaken(Some(reason))),
        None => (None, Outcome::NotTaken(None)),
    };
    if let Outcome::Taken(pending) = &outcome
        && let Some(problems) = pending.snapshot.problems()
    {
        let _ = hook::append_error(dir, Some(kind), &problems, ERRORS_LOG_MAX_BYTES);
    }
    let (columns, payload) = redaction.finish()?;
    let (step, late_streak) = store
        .write_transaction(|| {
            let now = now_ms();
            store.upsert_session(&Session {
                id: columns.session_id.clone(),
                started_at_ms: now,
                ended_at_ms: (event == HookEvent::SessionEnd).then_some(now),
                end_reason: columns.reason.clone(),
                source: columns.source.clone(),
                model: columns.model.clone(),
                cwd: columns.cwd.clone(),
                transcript_path: columns.transcript_path.clone(),
            })?;
            if event == HookEvent::SessionStart {
                store.reopen_session(&columns.session_id)?;
            }
            if let (Some(lock), Outcome::Taken(pending)) = (&lock, &outcome) {
                // A large stat cache to save after starting over can take longer than the time left: then the
                // snapshot is dropped and the step recorded without it.
                match store.with_savepoint(|| pending.save(&store, lock)) {
                    Ok(()) => {}
                    Err(StoreError::DeadlinePassed) => {
                        outcome = Outcome::Late(format!(
                            "abandoned, not saved within the deadline of {} ms",
                            snapshot_deadline().as_millis()
                        ));
                    }
                    // The step is still recorded.
                    Err(error) => {
                        outcome = Outcome::NotTaken(Some(format!("not saved: {error}")));
                    }
                }
            }
            if let (Some(lock), Outcome::Failed(error)) = (&lock, &outcome) {
                snapshot::remember_failure(&store, lock, error)?;
            }
            let late_streak = record_late_streak(&store, &outcome)?;
            let snapshot = match &outcome {
                Outcome::Taken(pending) => Some(&pending.snapshot),
                _ => None,
            };
            let step = store.next_step(&columns.session_id)?;
            store.insert_event(&Event {
                session_id: columns.session_id.clone(),
                step,
                ts_ms: now,
                kind: kind.to_owned(),
                tool_name: columns.tool_name.clone(),
                tool_use_id: columns.tool_use_id.clone(),
                agent_id: columns.agent_id.clone(),
                success: match event {
                    HookEvent::PostToolUse => Some(true),
                    HookEvent::PostToolUseFailure => Some(false),
                    _ => None,
                },
                tree_id: snapshot.map(|snapshot| snapshot.tree_id.to_string()),
                base_tree_id: snapshot.map(|snapshot| snapshot.base_tree_id.to_string()),
                files_changed: snapshot
                    .map(|snapshot| u32::try_from(snapshot.changed.len()).unwrap_or(u32::MAX)),
                payload,
            })?;
            Ok((step, late_streak))
        })
        .map_err(|error| format!("the step was not recorded: {error}"))?;
    drop(lock);

    match outcome {
        Outcome::Failed(error) => Err(format!("step {step} recorded without a snapshot: {error}")),
        Outcome::NotTaken(Some(reason)) => {
            Err(format!("step {step} recorded without a snapshot: {reason}"))
        }
        Outcome::Late(reason) => {
            let mut line = format!(
                "step {step} recorded without a snapshot: {reason} ({late_streak} in a row)"
            );
            if late_streak == LATE_STREAK_ADVICE {
                line.push_str(
                    ". Snapshots keep missing the deadline: the project probably has large folders that are \
                     not ignored (downloaded data, build output, dependencies). Add them to .gitignore, or \
                     to .git/info/exclude, so that each step does not wait about a second for nothing",
                );
            }
            Err(line)
        }
        Outcome::Taken(_) | Outcome::NotTaken(None) => Ok(()),
    }
}

/// The `meta` key counting the snapshot steps in a row that missed the deadline.
const LATE_STREAK_KEY: &str = "snapshot_deadline_misses";

/// After this many snapshots in a row missed the deadline, the logged line says what to do about it (once).
const LATE_STREAK_ADVICE: u64 = 3;

/// Counts a snapshot that missed the deadline, or ends the count when one was taken, and returns the count.
fn record_late_streak(store: &Store, outcome: &Outcome) -> crate::store::Result<u64> {
    let streak = || {
        store.meta(LATE_STREAK_KEY).map(|count| {
            count
                .and_then(|count| count.parse::<u64>().ok())
                .unwrap_or(0)
        })
    };
    match outcome {
        Outcome::Late(_) => {
            let count = streak()?.saturating_add(1);
            store.set_meta(LATE_STREAK_KEY, Some(&count.to_string()))?;
            Ok(count)
        }
        Outcome::Taken(_) => {
            store.set_meta(LATE_STREAK_KEY, None)?;
            Ok(0)
        }
        Outcome::Failed(_) | Outcome::NotTaken(_) => streak(),
    }
}

/// What became of a step's snapshot.
enum Outcome {
    /// Taken in time, to be saved with the step.
    Taken(Pending),
    /// [`snapshot::take`] failed.
    Failed(snapshot::Error),
    /// Not done by the deadline (the reason): not taken, or not saved.
    Late(String),
    /// No snapshot: none is taken for this event (`None`), or one could not be started (the reason).
    NotTaken(Option<String>),
}
/// A step's snapshot, taken on its own thread so that the hook can stop waiting for it at the deadline.
///
/// The thread starts first and prepares what needs no lock (see [`snapshot::take`]) while the hook opens the
/// event log and takes the writer lock; then it gets the base and walks. A thread left running at the deadline
/// does no harm: nothing it does is saved, it only adds objects to the shadow store (moved into place by
/// renaming, so never partial), and it stops when the hook process exits.
struct SnapshotTask {
    base: mpsc::Sender<Base>,
    result: mpsc::Receiver<snapshot::Result<Pending>>,
}

impl SnapshotTask {
    fn start(yalper: &YalperDir) -> Result<Self, String> {
        let dir = yalper.dir.try_clone().map_err(|error| error.to_string())?;
        let token = yalper.token.clone();
        let (base, base_receiver) = mpsc::channel();
        let (sender, result) = mpsc::channel();
        thread::Builder::new()
            .name("yalper-snapshot".into())
            .spawn(move || {
                let taken = snapshot::take(&dir, &token, || base_receiver.recv().ok());
                // The receiver is gone if the hook stopped waiting.
                let _ = sender.send(taken);
            })
            .map_err(|error| format!("cannot start the snapshot thread: {error}"))?;
        Ok(Self { base, result })
    }

    /// Takes the writer lock and waits for the snapshot, both at most until `deadline`. The lock is returned
    /// with a snapshot that was taken or failed, to save either with the step.
    fn finish(
        self,
        yalper: &YalperDir,
        store: &Store,
        deadline: Instant,
    ) -> (Option<WriterLock>, Outcome) {
        let left = || deadline.saturating_duration_since(Instant::now());
        let late = |reason: String| (None, Outcome::Late(reason));
        let abandoned = || {
            late(format!(
                "abandoned, not done within the deadline of {} ms",
                snapshot_deadline().as_millis()
            ))
        };
        let lock = match WriterLock::acquire(&yalper.dir, left()) {
            Ok(lock) => lock,
            Err(error) => return late(format!("the writer lock was not free in time: {error}")),
        };
        // Reading a very large stat cache stops at the deadline too (see `Store::set_deadline`).
        match Base::read(store, &lock) {
            Ok(base) => {
                // A send fails only if the thread already stopped, which `recv_timeout` reports.
                let _ = self.base.send(base);
            }
            Err(snapshot::Error::Store(StoreError::DeadlinePassed)) => return abandoned(),
            Err(error) => return (Some(lock), Outcome::Failed(error)),
        }
        match self.result.recv_timeout(left()) {
            Ok(Ok(pending)) => (Some(lock), Outcome::Taken(pending)),
            Ok(Err(error)) => (Some(lock), Outcome::Failed(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => abandoned(),
            Err(mpsc::RecvTimeoutError::Disconnected) => (
                None,
                Outcome::NotTaken(Some("the snapshot thread stopped unexpectedly".to_owned())),
            ),
        }
    }
}

/// The redaction of a hook's payload and columns, running on its own thread (or on the calling thread if no
/// thread can be started). It ends within the redaction deadline (see [`crate::redact::DEADLINE`]).
enum Redaction {
    Running(thread::JoinHandle<Option<(Columns, Value)>>),
    Done(Box<(Columns, Value)>),
}

impl Redaction {
    fn start(input: HookInput) -> Self {
        // The input is sent once the thread runs, so it is still here if the thread cannot start.
        let (sender, receiver) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("yalper-redact-step".into())
            .spawn(move || receiver.recv().ok().map(redact));
        match spawned {
            Ok(handle) => {
                let _ = sender.send(input);
                Self::Running(handle)
            }
            Err(_) => Self::Done(Box::new(redact(input))),
        }
    }

    /// The redacted columns and payload. Fails if the redaction panicked: then nothing is recorded, since
    /// nothing unredacted may be written.
    fn finish(self) -> Result<(Columns, Value), String> {
        match self {
            Self::Running(handle) => handle
                .join()
                .ok()
                .flatten()
                .ok_or_else(|| "redaction failed, so the step was not recorded".to_owned()),
            Self::Done(done) => Ok(*done),
        }
    }
}

fn redact(mut input: HookInput) -> (Columns, Value) {
    let columns = Columns::redacted(&input);
    let mut payload = std::mem::take(&mut input.raw);
    redact_json(&mut payload);
    (columns, payload)
}

/// The values of the input stored in their own columns, redacted. `source` and `model` are only taken from
/// `SessionStart`, and `reason` only from `SessionEnd`.
struct Columns {
    session_id: String,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    agent_id: Option<String>,
    transcript_path: Option<String>,
    cwd: Option<String>,
    source: Option<String>,
    model: Option<String>,
    reason: Option<String>,
}

impl Columns {
    /// Redacts all the values in one call, which costs one keyword scan instead of one per value.
    fn redacted(input: &HookInput) -> Self {
        let starts = input.event == HookEvent::SessionStart;
        let ends = input.event == HookEvent::SessionEnd;
        let values = [
            Some(&input.session_id),
            input.tool_name.as_ref(),
            input.tool_use_id.as_ref(),
            input.agent_id.as_ref(),
            input.transcript_path.as_ref(),
            input.cwd.as_ref(),
            input.source.as_ref().filter(|_| starts),
            input.model.as_ref().filter(|_| starts),
            input.reason.as_ref().filter(|_| ends),
        ];
        let mut batch = Value::Array(
            values
                .iter()
                .map(|value| value.map_or(Value::Null, |text| Value::String(text.clone())))
                .collect(),
        );
        redact_json(&mut batch);
        // An array keeps its length and the position of each value through redaction.
        let mut redacted = match batch {
            Value::Array(items) => items.into_iter(),
            _ => Vec::new().into_iter(),
        }
        .map(|value| match value {
            Value::String(text) => Some(text),
            _ => None,
        });
        let mut next = || redacted.next().flatten();
        Self {
            session_id: match next() {
                Some(id) if id == input.session_id => id,
                _ => session_stand_in(&input.session_id),
            },
            tool_name: next(),
            tool_use_id: next(),
            agent_id: next(),
            transcript_path: next(),
            cwd: next(),
            source: next(),
            model: next(),
            reason: next(),
        }
    }
}

/// What is stored instead of a session id that redaction changed (masked, or replaced by a marker at the
/// deadline): `sha1:` and the SHA-1 of the id. Every event of the session gets the same one, so sessions never
/// merge, and the id itself is never stored.
pub fn session_stand_in(session_id: &str) -> String {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(session_id.as_bytes());
    match hasher.try_finalize() {
        Ok(digest) => format!("sha1:{digest}"),
        // Only input crafted as a SHA-1 collision attack gets here.
        Err(_) => "sha1:unavailable".to_owned(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_is_kept_unless_redaction_changes_it() {
        let input = |id: &str| {
            HookInput::from_value(serde_json::json!({"session_id": id, "hook_event_name": "Stop"}))
                .unwrap()
        };
        let id = "00893aaf-19fa-41d2-8238-13269b9b3ca0";
        assert_eq!(Columns::redacted(&input(id)).session_id, id);

        let secret = format!("ghp_{}", "q7W2e9R4t1Y6u3I8o5P0".repeat(2).split_at(36).0);
        let stored = Columns::redacted(&input(&secret)).session_id;
        assert_eq!(stored, session_stand_in(&secret));
        // SHA-1 of "abc", a published test vector.
        assert_eq!(
            session_stand_in("abc"),
            "sha1:a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn source_and_model_come_only_from_session_start_and_reason_only_from_session_end() {
        let columns = |event: &str| {
            Columns::redacted(
                &HookInput::from_value(serde_json::json!({
                    "session_id": "s",
                    "hook_event_name": event,
                    "source": "startup",
                    "model": "m",
                    "reason": "other",
                }))
                .unwrap(),
            )
        };
        let start = columns("SessionStart");
        assert_eq!(
            (
                start.source.as_deref(),
                start.model.as_deref(),
                start.reason
            ),
            (Some("startup"), Some("m"), None)
        );
        let end = columns("SessionEnd");
        assert_eq!(
            (end.source, end.model, end.reason.as_deref()),
            (None, None, Some("other"))
        );
        let tool = columns("PostToolUse");
        assert_eq!((tool.source, tool.model, tool.reason), (None, None, None));
    }
}
