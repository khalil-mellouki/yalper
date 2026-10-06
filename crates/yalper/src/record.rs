//! Records one hook call as the next step of its session: the redacted payload in the event log and, after
//! a prompt or a tool call, a snapshot of the working tree.
//!
//! Everything written comes out of [`redact_json`] first: the whole payload, and the short values also kept
//! in their own columns (session id, tool name, paths). Those columns are taken from the parsed input rather
//! than from the redacted payload, so a redaction that runs out of time or budget can never lose them, and
//! are redacted on their own, so none of them bypasses masking.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::hook::{self, ERRORS_LOG_MAX_BYTES, HookEvent, HookInput};
use crate::redact::redact_json;
use crate::safe_fs::OwnedDir;
use crate::snapshot;
use crate::store::{Event, LOCK_TIMEOUT, Session, Store, WriterLock};

/// Records `input` in the `.yalper/` directory `dir`, which must be one `yalper init` created (see
/// [`crate::repo::is_initialized`]). Events Yalper does not register for are ignored.
///
/// `SessionStart`, `UserPromptSubmit`, `PostToolUse` and `PostToolUseFailure` take a snapshot. `Stop` and
/// `SessionEnd` do not: nothing ran since the last tool call, and `SessionEnd` hooks share a 1.5 s budget.
/// A session the event log does not know yet (Yalper was set up mid-session) is created by its first event.
///
/// If the snapshot fails, the step is still recorded, without a tree, and the error is returned for the
/// hook to log. Skipped files are logged here as one line.
pub fn record(dir: &OwnedDir, input: &HookInput) -> Result<(), String> {
    let Some(kind) = input.event.name() else {
        return Ok(());
    };
    let takes_snapshot = matches!(
        input.event,
        HookEvent::SessionStart
            | HookEvent::UserPromptSubmit
            | HookEvent::PostToolUse
            | HookEvent::PostToolUseFailure
    );
    let columns = Columns::redacted(input);
    let mut payload = input.raw.clone();
    redact_json(&mut payload);

    // Opened before taking the lock: opening a new database takes the lock for a moment itself.
    let store = Store::open(dir).map_err(|error| error.to_string())?;
    let lock = WriterLock::acquire(dir, LOCK_TIMEOUT).map_err(|error| error.to_string())?;
    let now = now_ms();

    let ends = input.event == HookEvent::SessionEnd;
    store
        .upsert_session(&Session {
            id: columns.session_id.clone(),
            started_at_ms: now,
            ended_at_ms: ends.then_some(now),
            end_reason: if ends { columns.reason } else { None },
            source: columns.source,
            model: columns.model,
            cwd: columns.cwd,
            transcript_path: columns.transcript_path,
        })
        .map_err(|error| error.to_string())?;
    if input.event == HookEvent::SessionStart {
        store
            .reopen_session(&columns.session_id)
            .map_err(|error| error.to_string())?;
    }

    let mut snapshot_error = None;
    let (tree_id, files_changed) = if takes_snapshot {
        match snapshot::snapshot(dir, &store, &lock) {
            Ok(snapshot) => {
                if let Some(problems) = snapshot.problems() {
                    let _ = hook::append_error(dir, Some(kind), &problems, ERRORS_LOG_MAX_BYTES);
                }
                let changed = u32::try_from(snapshot.changed.len()).unwrap_or(u32::MAX);
                (Some(snapshot.tree_id.to_string()), Some(changed))
            }
            Err(error) => {
                snapshot_error = Some(error.to_string());
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let step = store
        .next_step(&columns.session_id)
        .map_err(|error| error.to_string())?;
    store
        .insert_event(&Event {
            session_id: columns.session_id,
            step,
            ts_ms: now,
            kind: kind.to_owned(),
            tool_name: columns.tool_name,
            tool_use_id: columns.tool_use_id,
            agent_id: columns.agent_id,
            success: match input.event {
                HookEvent::PostToolUse => Some(true),
                HookEvent::PostToolUseFailure => Some(false),
                _ => None,
            },
            tree_id,
            files_changed,
            payload,
        })
        .map_err(|error| error.to_string())?;
    drop(lock);

    match snapshot_error {
        Some(error) => Err(format!("step {step} recorded without a snapshot: {error}")),
        None => Ok(()),
    }
}

/// The values of the input stored in their own columns, redacted.
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
        let values = [
            Some(&input.session_id),
            input.tool_name.as_ref(),
            input.tool_use_id.as_ref(),
            input.agent_id.as_ref(),
            input.transcript_path.as_ref(),
            input.cwd.as_ref(),
            input.source.as_ref(),
            input.model.as_ref(),
            input.reason.as_ref(),
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
            session_id: next().unwrap_or_default(),
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}
