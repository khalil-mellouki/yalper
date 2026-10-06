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
///
/// The snapshot saves the new latest tree and stat cache before the step is inserted. If the insert then
/// fails, the next snapshot starts from that tree, so the files changed in this step belong to no recorded
/// step; the logged error says so.
pub fn record(dir: &OwnedDir, mut input: HookInput) -> Result<(), String> {
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
    let columns = Columns::redacted(&input);
    let mut payload = std::mem::take(&mut input.raw);
    redact_json(&mut payload);

    // Opened before taking the lock: opening a new database takes the lock for a moment itself.
    let store = Store::open(dir).map_err(|error| error.to_string())?;
    let lock = WriterLock::acquire(dir, LOCK_TIMEOUT).map_err(|error| error.to_string())?;
    let now = now_ms();

    store
        .upsert_session(&Session {
            id: columns.session_id.clone(),
            started_at_ms: now,
            ended_at_ms: (input.event == HookEvent::SessionEnd).then_some(now),
            end_reason: columns.reason,
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

    let snapshot_saved = tree_id.is_some();
    let step_failed = |error: crate::store::Error| {
        if snapshot_saved {
            format!(
                "the snapshot was saved but the step was not recorded, so its file changes belong to \
                 no step: {error}"
            )
        } else {
            format!("the step was not recorded: {error}")
        }
    };
    let step = store.next_step(&columns.session_id).map_err(step_failed)?;
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
        .map_err(step_failed)?;
    drop(lock);

    match snapshot_error {
        Some(error) => Err(format!("step {step} recorded without a snapshot: {error}")),
        None => Ok(()),
    }
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
