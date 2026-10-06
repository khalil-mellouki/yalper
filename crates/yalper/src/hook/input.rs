//! The JSON payload Claude Code sends on stdin to every hook.

use std::fmt;

use serde_json::Value;

/// The hook events Yalper registers for. Any other event name is kept as [`HookEvent::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    PostToolUseFailure,
    Stop,
    SessionEnd,
    Other(String),
}

impl HookEvent {
    pub fn from_name(name: &str) -> Self {
        match name {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PostToolUse" => Self::PostToolUse,
            "PostToolUseFailure" => Self::PostToolUseFailure,
            "Stop" => Self::Stop,
            "SessionEnd" => Self::SessionEnd,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The event's name as Claude Code sends it, or `None` for an event Yalper does not register for.
    pub fn name(&self) -> Option<&'static str> {
        match self {
            Self::SessionStart => Some("SessionStart"),
            Self::UserPromptSubmit => Some("UserPromptSubmit"),
            Self::PostToolUse => Some("PostToolUse"),
            Self::PostToolUseFailure => Some("PostToolUseFailure"),
            Self::Stop => Some("Stop"),
            Self::SessionEnd => Some("SessionEnd"),
            Self::Other(_) => None,
        }
    }
}

/// One hook call from Claude Code.
///
/// Parsing is lenient so that a change in Claude Code's payload never breaks recording: only `session_id`
/// and `hook_event_name` are required, and an optional field with an unexpected type is treated as absent.
/// The whole payload, including fields Yalper does not know, stays available in `raw`. The tool input and
/// response, which can be large, are not copied out of it: see [`tool_input`](Self::tool_input) and
/// [`tool_response`](Self::tool_response).
#[derive(Debug, Clone, PartialEq)]
pub struct HookInput {
    pub session_id: String,
    pub event: HookEvent,

    // Common fields.
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub permission_mode: Option<String>,
    pub prompt_id: Option<String>,
    pub scratchpad_dir: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,

    // SessionStart and UserPromptSubmit.
    pub source: Option<String>,
    pub model: Option<String>,
    pub session_title: Option<String>,
    pub prompt: Option<String>,

    // PostToolUse and PostToolUseFailure.
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub error: Option<String>,
    pub is_interrupt: Option<bool>,
    pub duration_ms: Option<u64>,

    // Stop and SessionEnd.
    pub stop_hook_active: Option<bool>,
    pub last_assistant_message: Option<String>,
    pub reason: Option<String>,

    /// The payload exactly as received.
    pub raw: Value,
}

/// Why a payload could not be turned into a [`HookInput`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    NotAnObject,
    MissingField(&'static str),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => write!(f, "hook input is not a JSON object"),
            Self::MissingField(name) => write!(f, "hook input has no string field `{name}`"),
        }
    }
}

impl std::error::Error for InputError {}

impl HookInput {
    /// The `tool_input` of `PostToolUse` and `PostToolUseFailure`, read from [`raw`](Self::raw).
    pub fn tool_input(&self) -> Option<&Value> {
        self.raw.get("tool_input").filter(|value| !value.is_null())
    }

    /// The `tool_response` of `PostToolUse`, read from [`raw`](Self::raw).
    pub fn tool_response(&self) -> Option<&Value> {
        self.raw
            .get("tool_response")
            .filter(|value| !value.is_null())
    }

    pub fn from_value(raw: Value) -> Result<Self, InputError> {
        if !raw.is_object() {
            return Err(InputError::NotAnObject);
        }
        let string = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_owned);
        let boolean = |key: &str| raw.get(key).and_then(Value::as_bool);
        let required = |key: &'static str| string(key).ok_or(InputError::MissingField(key));

        Ok(Self {
            session_id: required("session_id")?,
            event: HookEvent::from_name(&required("hook_event_name")?),
            transcript_path: string("transcript_path"),
            cwd: string("cwd"),
            permission_mode: string("permission_mode"),
            prompt_id: string("prompt_id"),
            scratchpad_dir: string("scratchpad_dir"),
            agent_id: string("agent_id"),
            agent_type: string("agent_type"),
            source: string("source"),
            model: string("model"),
            session_title: string("session_title"),
            prompt: string("prompt"),
            tool_name: string("tool_name"),
            tool_use_id: string("tool_use_id"),
            error: string("error"),
            is_interrupt: boolean("is_interrupt"),
            duration_ms: raw.get("duration_ms").and_then(as_millis),
            stop_hook_active: boolean("stop_hook_active"),
            last_assistant_message: string("last_assistant_message"),
            reason: string("reason"),
            raw,
        })
    }
}

fn as_millis(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .map(|ms| ms.round() as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_tool_call_with_every_field() {
        let raw = json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUseFailure",
            "transcript_path": "/t.jsonl",
            "cwd": "/repo",
            "permission_mode": "default",
            "prompt_id": "p1",
            "agent_id": "a1",
            "agent_type": "Explore",
            "tool_name": "Bash",
            "tool_use_id": "toolu_1",
            "tool_input": {"command": "npm test"},
            "error": "Exit code 1\nfailed",
            "is_interrupt": false,
            "duration_ms": 4187
        });
        let input = HookInput::from_value(raw.clone()).unwrap();
        assert_eq!(input.session_id, "s1");
        assert_eq!(input.event, HookEvent::PostToolUseFailure);
        assert_eq!(input.cwd.as_deref(), Some("/repo"));
        assert_eq!(input.agent_id.as_deref(), Some("a1"));
        assert_eq!(input.tool_name.as_deref(), Some("Bash"));
        assert_eq!(input.tool_input(), Some(&json!({"command": "npm test"})));
        assert_eq!(input.tool_response(), None);
        assert_eq!(input.error.as_deref(), Some("Exit code 1\nfailed"));
        assert_eq!(input.is_interrupt, Some(false));
        assert_eq!(input.duration_ms, Some(4187));
        assert_eq!(input.raw, raw);
    }

    #[test]
    fn only_session_id_and_event_name_are_required() {
        let input =
            HookInput::from_value(json!({"session_id": "s1", "hook_event_name": "Stop"})).unwrap();
        assert_eq!(input.event, HookEvent::Stop);
        assert_eq!(input.cwd, None);
        assert_eq!(input.last_assistant_message, None);

        assert_eq!(
            HookInput::from_value(json!({"hook_event_name": "Stop"})),
            Err(InputError::MissingField("session_id"))
        );
        assert_eq!(
            HookInput::from_value(json!({"session_id": "s1"})),
            Err(InputError::MissingField("hook_event_name"))
        );
        assert_eq!(
            HookInput::from_value(json!({"session_id": 7, "hook_event_name": "Stop"})),
            Err(InputError::MissingField("session_id"))
        );
        assert_eq!(
            HookInput::from_value(json!(["not", "an", "object"])),
            Err(InputError::NotAnObject)
        );
    }

    #[test]
    fn fields_with_unexpected_types_are_treated_as_absent() {
        let input = HookInput::from_value(json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "cwd": 42,
            "tool_name": null,
            "is_interrupt": "no",
            "duration_ms": "12",
            "tool_response": "plain text output"
        }))
        .unwrap();
        assert_eq!(input.cwd, None);
        assert_eq!(input.tool_name, None);
        assert_eq!(input.is_interrupt, None);
        assert_eq!(input.duration_ms, None);
        assert_eq!(input.tool_response(), Some(&json!("plain text output")));
    }

    #[test]
    fn fractional_duration_is_rounded() {
        let input = HookInput::from_value(json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "duration_ms": 12.6
        }))
        .unwrap();
        assert_eq!(input.duration_ms, Some(13));
    }

    #[test]
    fn known_event_names_round_trip() {
        for name in [
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PostToolUseFailure",
            "Stop",
            "SessionEnd",
        ] {
            assert_eq!(HookEvent::from_name(name).name(), Some(name));
        }
        assert_eq!(HookEvent::from_name("PostToolBatch").name(), None);
    }

    #[test]
    fn unknown_fields_and_events_are_kept() {
        let raw = json!({
            "session_id": "s1",
            "hook_event_name": "PostToolBatch",
            "effort": {"level": "high"},
            "a_future_field": [1, 2, 3]
        });
        let input = HookInput::from_value(raw.clone()).unwrap();
        assert_eq!(input.event, HookEvent::Other("PostToolBatch".to_owned()));
        assert_eq!(input.raw["a_future_field"], json!([1, 2, 3]));
        assert_eq!(input.raw, raw);
    }
}
