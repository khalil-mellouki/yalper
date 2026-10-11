//! The entries of a bisect: the code at each step that changed files, in the order the snapshots were taken,
//! from the good end to the bad end. A bisect covers one recorded session, or several ([`Scope`]).
//!
//! - The snapshots of all sessions form one chain in the order they were taken (`events.id`): each is built from
//!   the one before, whatever its session. Sessions that ran at the same time interleave in that order.
//! - The good end is, by default, the code at the start of what is bisected. One session: the snapshot of its
//!   first step when that step is the session's start (the user's edits made before the session included),
//!   otherwise the tree its first snapshot was built from (Yalper was set up during the session). Several
//!   sessions: the tree the first snapshot covered was built from (the code when Yalper was set up, or at the
//!   date asked), so the changes made before the first session are an entry too. `--good <step>` picks the code
//!   at that step instead.
//! - The bad end is the code at the last step covered, or at the step `--bad <step>` names.
//! - A step without a snapshot of its own (the end of a turn or of the session, a missed deadline) has the code
//!   of the latest snapshot at or before it: its changes are in the next snapshot.
//! - Between the ends, only the steps whose code differs from the entry before are entries, so the engine never
//!   tests the same code twice. Besides the agent's tool steps, a prompt's snapshot holds the changes made
//!   between two turns, and a session start's snapshot the changes made between sessions, or, by its source,
//!   before the session resumed or before its context was compacted or cleared ([`Kind`]). A step whose
//!   snapshot is no longer in the snapshot store is left out and named.

use std::collections::HashSet;

use gix::ObjectId;

use crate::hook::HookEvent;
use crate::snapshot::ShadowStore;
use crate::store::Event;

/// Which steps a bisect covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// The steps of the session with this id.
    Session(&'a str),
    /// The steps of every session; with `since_ms` (milliseconds since the Unix epoch), from the first snapshot
    /// taken at or after that time.
    All { since_ms: Option<i64> },
}

/// What the code of an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The code before a step: the tree its snapshot was built from. Only a good end.
    Before,
    /// A step of the agent, such as a tool call.
    Step,
    /// The snapshot taken when a prompt was sent: the changes made between two turns of the agent.
    Prompt,
    /// The snapshot taken when a session started (Claude Code's `startup` source, or one Yalper does not know):
    /// the changes made since the snapshot before the session, by no recorded step.
    SessionStart,
    /// The snapshot taken when a session was resumed: the changes made while it was not running.
    Resume,
    /// The snapshot taken when a session's context was compacted or cleared (`compact`, `clear`): like a
    /// prompt's, the changes made between two turns.
    Compact,
}

/// The code of one entry: the snapshot of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// Where the step is in the steps given to [`entries`].
    pub order: usize,
    /// The step's number in its session.
    pub step: u32,
    pub kind: Kind,
    pub tree: ObjectId,
    /// The tree the step's snapshot was built from, when it is known and in the snapshot store.
    pub base: Option<ObjectId>,
}

impl Entry {
    /// How the entry is named on screen: `step 12`, or `the code before step 3`, with ` of session <session>`
    /// after the step when `session` is given.
    pub fn label(&self, session: Option<&str>) -> String {
        let step = step_name(self.step, session);
        match self.kind {
            Kind::Before => format!("the code before {step}"),
            _ => step,
        }
    }

    /// What the entry changed, when no step of the agent made it.
    pub fn changes(&self) -> Option<&'static str> {
        match self.kind {
            Kind::Prompt => Some("changes made before this prompt"),
            Kind::Compact => Some("changes made before this point of the session"),
            Kind::Resume => Some("changes made before the session resumed"),
            Kind::SessionStart => Some("changes made between sessions"),
            Kind::Before | Kind::Step => None,
        }
    }
}

/// `step 12`, or `step 12 of session 5f3c2a1e` when `session` is given.
pub fn step_name(step: u32, session: Option<&str>) -> String {
    match session {
        Some(session) => format!("step {step} of session {session}"),
        None => format!("step {step}"),
    }
}

/// What [`entries`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entries {
    /// The good end first, then each step up to the bad end whose code differs from the entry before. The bad
    /// end is the last one, unless no step after the good end changed files.
    pub entries: Vec<Entry>,
    /// Where the step the bad end names, as asked, is in the steps: its code is the last entry's.
    pub bad: usize,
    /// Where the steps between the ends whose snapshot is not in the snapshot store are in the steps.
    pub missing: Vec<usize>,
}

/// The entries of `scope` among `steps` (every recorded step of the project, in the order they were recorded),
/// with the snapshots of `shadow`, between the steps at `good` and `bad` (positions in `steps`; see the module
/// documentation). `name` names the step at a position in messages, and `pick_good` says how to pick another
/// good end. Returns why there are none when the ends are in the wrong order or no snapshot gives the code of an
/// end.
pub fn entries(
    steps: &[Event],
    shadow: &ShadowStore,
    scope: Scope<'_>,
    (good, bad): (Option<usize>, Option<usize>),
    name: &dyn Fn(usize) -> String,
    pick_good: &str,
) -> Result<Entries, String> {
    let covered: Vec<usize> = match scope {
        Scope::Session(id) => (0..steps.len())
            .filter(|&order| steps[order].session_id == id)
            .collect(),
        Scope::All { since_ms } => {
            let first = since_ms.map_or(Some(0), |since| {
                steps
                    .iter()
                    .position(|event| event.tree_id.is_some() && event.ts_ms >= since)
            });
            first.map_or_else(Vec::new, |first| (first..steps.len()).collect())
        }
    };
    let (Some(&first), Some(&last)) = (covered.first(), covered.last()) else {
        return Err(match scope {
            Scope::Session(_) => "it has no steps recorded yet.",
            Scope::All { since_ms: Some(_) } => "no snapshot was taken since then.",
            Scope::All { since_ms: None } => "no steps are recorded yet.",
        }
        .to_owned());
    };

    let present = |hex: &Option<String>| {
        hex.as_deref()
            .and_then(|hex| ObjectId::from_hex(hex.as_bytes()).ok())
            .filter(|&id| shadow.has_object(id))
    };
    let mut snapshots = Vec::new();
    let mut missing = Vec::new();
    for &order in &covered {
        let event = &steps[order];
        if event.tree_id.is_none() {
            continue;
        }
        match present(&event.tree_id) {
            Some(tree) => snapshots.push(Entry {
                order,
                step: event.step,
                kind: kind(event),
                tree,
                base: present(&event.base_tree_id),
            }),
            None => missing.push(order),
        }
    }

    let starts_the_session = matches!(scope, Scope::Session(_)) && {
        let event = &steps[first];
        event.step == 1 && matches!(HookEvent::from_name(&event.kind), HookEvent::SessionStart)
    };
    // The step whose code is the good end, if it is not the code before the first snapshot.
    let good_at = good.or(starts_the_session.then_some(first));
    let good_end = match good_at
        .and_then(|at| snapshots.iter().rev().find(|entry| entry.order <= at))
    {
        Some(entry) => *entry,
        None => {
            let Some(first) = snapshots
                .iter()
                .find(|entry| good_at.is_none_or(|at| entry.order > at))
            else {
                return Err(
                    "no step has a snapshot in the snapshot store, so there is no code to test."
                        .to_owned(),
                );
            };
            let Some(base) = first.base else {
                return Err(format!(
                    "the code before {} is not known (its snapshot does not say what it was built from). \
                     {pick_good}",
                    name(first.order)
                ));
            };
            Entry {
                kind: Kind::Before,
                tree: base,
                base: None,
                ..*first
            }
        }
    };
    // The first step whose snapshot can come after the good end: its own when it is the code before it.
    let start = match good_end.kind {
        Kind::Before => good_end.order,
        _ => good_end.order + 1,
    };
    let bad_at = bad.unwrap_or(last);
    let in_order = match good_at {
        Some(at) => at < bad_at,
        None => start <= bad_at,
    };
    // With the default ends, a session of one step has nothing to test (one entry, below).
    if !in_order && (good.is_some() || bad.is_some()) {
        let good_name = match good_at {
            Some(at) => name(at),
            None => format!("the code before {}", name(good_end.order)),
        };
        return Err(format!(
            "the good end ({good_name}) must come before the bad end ({}).",
            name(bad_at)
        ));
    }

    let mut entries = vec![good_end];
    for entry in snapshots
        .iter()
        .filter(|entry| entry.order >= start && entry.order <= bad_at)
    {
        if entries.last().is_some_and(|last| last.tree != entry.tree) {
            entries.push(*entry);
        }
    }
    missing.retain(|&order| order >= start && order <= bad_at);
    Ok(Entries {
        entries,
        bad: bad_at,
        missing,
    })
}

/// What the snapshot of `event` holds (see [`Kind`]).
fn kind(event: &Event) -> Kind {
    match HookEvent::from_name(&event.kind) {
        HookEvent::SessionStart => match event.payload.get("source").and_then(|s| s.as_str()) {
            Some("resume") => Kind::Resume,
            Some("compact" | "clear") => Kind::Compact,
            _ => Kind::SessionStart,
        },
        HookEvent::UserPromptSubmit => Kind::Prompt,
        _ => Kind::Step,
    }
}

/// Whether a session other than the one of the entry at `order` in `steps` was running when that entry's
/// snapshot was taken: it has steps both before and after it. Its agent may then have made the entry's changes
/// in a step without a snapshot of its own.
pub fn another_session_running(steps: &[Event], order: usize) -> bool {
    let session = &steps[order].session_id;
    let after: HashSet<&str> = steps[order + 1..]
        .iter()
        .map(|event| event.session_id.as_str())
        .filter(|id| id != session)
        .collect();
    steps[..order]
        .iter()
        .any(|event| after.contains(event.session_id.as_str()))
}

/// Where the steps of sessions other than `session` whose snapshot changed files, taken after entry `from` and
/// before entry `to`, are in `steps`. In a bisect of one session, the code tested at `to` also holds their
/// changes.
pub fn others_between(steps: &[Event], session: &str, from: &Entry, to: &Entry) -> Vec<usize> {
    (from.order + 1..to.order)
        .filter(|&order| {
            let event = &steps[order];
            event.session_id != session
                && event.tree_id.is_some()
                && event.tree_id != event.base_tree_id
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bisect::testing::Project;
    use serde_json::{Value, json};

    fn event(step: u32, kind: &str, tree: Option<ObjectId>, base: Option<ObjectId>) -> Event {
        in_session("s", step, kind, tree, base)
    }

    fn in_session(
        session: &str,
        step: u32,
        kind: &str,
        tree: Option<ObjectId>,
        base: Option<ObjectId>,
    ) -> Event {
        Event {
            session_id: session.to_owned(),
            step,
            ts_ms: 0,
            kind: kind.to_owned(),
            tool_name: None,
            tool_use_id: None,
            agent_id: None,
            success: None,
            tree_id: tree.map(|tree| tree.to_string()),
            base_tree_id: base.map(|base| base.to_string()),
            files_changed: None,
            payload: Value::Null,
        }
    }

    /// A session: start (tree a), prompt (a), edit (b), stop (none), edit (c), edit that changes nothing (c),
    /// edit (d).
    fn session(project: &Project) -> (Vec<Event>, [ObjectId; 5]) {
        let tree = |text| project.tree(&[("f", text)]);
        let (before, a, b, c, d) = (tree("0"), tree("a"), tree("b"), tree("c"), tree("d"));
        let events = vec![
            event(1, "SessionStart", Some(a), Some(before)),
            event(2, "UserPromptSubmit", Some(a), Some(a)),
            event(3, "PostToolUse", Some(b), Some(a)),
            event(4, "Stop", None, None),
            event(5, "PostToolUse", Some(c), Some(b)),
            event(6, "PostToolUse", Some(c), Some(c)),
            event(7, "PostToolUse", Some(d), Some(c)),
        ];
        (events, [before, a, b, c, d])
    }

    /// The steps of one session as `entries` takes them, with `good` and `bad` as step numbers.
    fn of_session(
        events: &[Event],
        project: &Project,
        good: Option<u32>,
        bad: Option<u32>,
    ) -> Result<Entries, String> {
        let at = |step: Option<u32>| step.map(|step| step as usize - 1);
        let name = |order: usize| step_name(events[order].step, None);
        entries(
            events,
            &project.store,
            Scope::Session("s"),
            (at(good), at(bad)),
            &name,
            "Pick a good end.",
        )
    }

    fn steps(entries: &Entries) -> Vec<(u32, Kind)> {
        entries
            .entries
            .iter()
            .map(|entry| (entry.step, entry.kind))
            .collect()
    }

    #[test]
    fn the_whole_session_from_its_start_to_its_last_step() {
        let project = Project::new();
        let (events, [_, a, b, c, d]) = session(&project);
        let found = of_session(&events, &project, None, None).unwrap();
        assert_eq!(
            steps(&found),
            [
                (1, Kind::SessionStart),
                (3, Kind::Step),
                (5, Kind::Step),
                (7, Kind::Step)
            ]
        );
        let trees: Vec<ObjectId> = found.entries.iter().map(|entry| entry.tree).collect();
        assert_eq!(trees, [a, b, c, d]);
        assert_eq!(found.bad, 6);
        assert!(found.missing.is_empty());
        assert_eq!(found.entries[0].label(None), "step 1");
        assert_eq!(
            found.entries[1].label(Some("5f3c2a1e")),
            "step 3 of session 5f3c2a1e"
        );
        assert_eq!(found.entries[1].changes(), None);
    }

    #[test]
    fn a_step_without_a_tree_means_the_latest_tree_at_or_before_it() {
        let project = Project::new();
        let (events, _) = session(&project);
        let found = of_session(&events, &project, Some(4), Some(6)).unwrap();
        assert_eq!(steps(&found), [(3, Kind::Step), (5, Kind::Step)]);
        assert_eq!(found.bad, 5);
        let found = of_session(&events, &project, Some(2), Some(4)).unwrap();
        assert_eq!(steps(&found), [(2, Kind::Prompt), (3, Kind::Step)]);
    }

    #[test]
    fn without_a_session_start_the_good_end_is_the_code_before_the_first_snapshot() {
        let project = Project::new();
        let (mut events, [before, ..]) = session(&project);
        events[0] = event(
            1,
            "UserPromptSubmit",
            Some(project.tree(&[("f", "a")])),
            Some(before),
        );
        let found = of_session(&events, &project, None, None).unwrap();
        assert_eq!(found.entries[0].tree, before);
        assert_eq!(found.entries[0].label(None), "the code before step 1");
        assert_eq!(
            found.entries[0].label(Some("5f3c2a1e")),
            "the code before step 1 of session 5f3c2a1e"
        );
        assert_eq!(
            steps(&found),
            [
                (1, Kind::Before),
                (1, Kind::Prompt),
                (3, Kind::Step),
                (5, Kind::Step),
                (7, Kind::Step)
            ]
        );
        assert_eq!(
            found.entries[1].changes(),
            Some("changes made before this prompt")
        );
    }

    #[test]
    fn a_session_start_is_named_by_its_source() {
        let project = Project::new();
        let (mut events, [before, ..]) = session(&project);
        let cases = [
            (
                Some("startup"),
                Kind::SessionStart,
                "changes made between sessions",
            ),
            (None, Kind::SessionStart, "changes made between sessions"),
            (
                Some("other"),
                Kind::SessionStart,
                "changes made between sessions",
            ),
            (
                Some("resume"),
                Kind::Resume,
                "changes made before the session resumed",
            ),
            (
                Some("compact"),
                Kind::Compact,
                "changes made before this point of the session",
            ),
            (
                Some("clear"),
                Kind::Compact,
                "changes made before this point of the session",
            ),
        ];
        for (source, kind, changes) in cases {
            // A session start that is not the first step: the code before it is the good end.
            events[0].kind = "UserPromptSubmit".to_owned();
            events[1].kind = "SessionStart".to_owned();
            events[1].tree_id = Some(project.tree(&[("f", "z")]).to_string());
            events[1].payload = source.map_or(Value::Null, |source| json!({ "source": source }));
            let found = of_session(&events, &project, None, None).unwrap();
            let entry = found.entries.iter().find(|entry| entry.step == 2).unwrap();
            assert_eq!(
                (entry.kind, entry.changes()),
                (kind, Some(changes)),
                "{source:?}"
            );
            assert_eq!(entry.label(None), "step 2");
            assert_eq!(found.entries[0].tree, before);
        }
    }

    #[test]
    fn another_session_is_running_when_it_has_steps_before_and_after() {
        let project = Project::new();
        let steps = sessions(&project);
        // a starts (0), b starts (1) while a runs, ..., c starts (9) after a and b ended.
        assert!(!another_session_running(&steps, 0));
        assert!(another_session_running(&steps, 1));
        assert!(another_session_running(&steps, 4));
        assert!(!another_session_running(&steps, 9));
        assert!(!another_session_running(&steps, 11));
    }

    #[test]
    fn a_good_end_whose_code_is_not_known_says_how_to_pick_another() {
        let project = Project::new();
        let (mut events, _) = session(&project);
        events[0].kind = "UserPromptSubmit".to_owned();
        events[0].base_tree_id = None;
        let error = of_session(&events, &project, None, None).unwrap_err();
        assert_eq!(
            error,
            "the code before step 1 is not known (its snapshot does not say what it was built from). Pick a \
             good end."
        );
    }

    #[test]
    fn a_snapshot_missing_from_the_store_is_left_out_and_named() {
        let project = Project::new();
        let (mut events, _) = session(&project);
        events[4].tree_id = Some("1".repeat(40));
        events[2].tree_id = Some("not hex".to_owned());
        let found = of_session(&events, &project, None, None).unwrap();
        assert_eq!(
            steps(&found),
            [(1, Kind::SessionStart), (6, Kind::Step), (7, Kind::Step)]
        );
        assert_eq!(found.missing, [2, 4]);
    }

    #[test]
    fn ends_in_the_wrong_order_are_refused() {
        let project = Project::new();
        let (events, _) = session(&project);
        for (good, bad, message) in [
            (
                Some(5),
                Some(5),
                "the good end (step 5) must come before the bad end (step 5)",
            ),
            (
                Some(7),
                None,
                "the good end (step 7) must come before the bad end (step 7)",
            ),
            (
                Some(6),
                Some(2),
                "the good end (step 6) must come before the bad end (step 2)",
            ),
        ] {
            let error = of_session(&events, &project, good, bad).unwrap_err();
            assert!(error.contains(message), "{error}");
        }
        assert!(of_session(&[], &project, None, None).is_err());
    }

    #[test]
    fn steps_that_change_nothing_give_one_entry() {
        let project = Project::new();
        let a = project.tree(&[("f", "a")]);
        let events = vec![
            event(1, "SessionStart", Some(a), None),
            event(2, "PostToolUse", Some(a), Some(a)),
            event(3, "Stop", None, None),
        ];
        let found = of_session(&events, &project, None, None).unwrap();
        assert_eq!(steps(&found), [(1, Kind::SessionStart)]);
    }

    #[test]
    fn a_session_of_only_its_start_has_one_entry_unless_ends_were_asked_for() {
        let project = Project::new();
        let events = vec![event(
            1,
            "SessionStart",
            Some(project.tree(&[("f", "a")])),
            None,
        )];
        let found = of_session(&events, &project, None, None).unwrap();
        assert_eq!(steps(&found), [(1, Kind::SessionStart)]);
        assert_eq!(found.bad, 0);
        for (good, bad) in [(Some(1), None), (None, Some(1))] {
            let error = of_session(&events, &project, good, bad).unwrap_err();
            assert!(error.contains("must come before the bad end"), "{error}");
        }
    }

    /// Two sessions that overlap, then a third after an edit made between sessions. Trees: x (init), then each
    /// snapshot builds on the one before, whatever its session.
    fn sessions(project: &Project) -> Vec<Event> {
        let tree = |text| project.tree(&[("f", text)]);
        let [x, a1, b1, a3, b3, b4, c1, c3] =
            ["x", "a1", "b1", "a3", "b3", "b4", "c1", "c3"].map(tree);
        let mut steps = vec![
            in_session("a", 1, "SessionStart", Some(a1), Some(x)),
            in_session("b", 1, "SessionStart", Some(b1), Some(a1)),
            in_session("a", 2, "UserPromptSubmit", Some(b1), Some(b1)),
            in_session("a", 3, "PostToolUse", Some(a3), Some(b1)),
            in_session("b", 2, "UserPromptSubmit", Some(a3), Some(a3)),
            in_session("b", 3, "PostToolUse", Some(b3), Some(a3)),
            in_session("a", 4, "Stop", None, None),
            in_session("b", 4, "PostToolUse", Some(b4), Some(b3)),
            in_session("b", 5, "Stop", None, None),
            in_session("c", 1, "SessionStart", Some(c1), Some(b4)),
            in_session("c", 2, "UserPromptSubmit", Some(c1), Some(c1)),
            in_session("c", 3, "PostToolUse", Some(c3), Some(c1)),
        ];
        for (index, step) in steps.iter_mut().enumerate() {
            step.ts_ms = index as i64 * 1_000;
        }
        steps
    }

    fn named(steps: &[Event], found: &Entries) -> Vec<String> {
        found
            .entries
            .iter()
            .map(|entry| entry.label(Some(&steps[entry.order].session_id)))
            .collect()
    }

    fn across(
        steps: &[Event],
        project: &Project,
        since_ms: Option<i64>,
        good: Option<usize>,
        bad: Option<usize>,
    ) -> Result<Entries, String> {
        let name = |order: usize| step_name(steps[order].step, Some(&steps[order].session_id));
        entries(
            steps,
            &project.store,
            Scope::All { since_ms },
            (good, bad),
            &name,
            "Pick a good end.",
        )
    }

    #[test]
    fn every_session_in_the_order_the_snapshots_were_taken() {
        let project = Project::new();
        let steps = sessions(&project);
        let found = across(&steps, &project, None, None, None).unwrap();
        assert_eq!(
            named(&steps, &found),
            [
                "the code before step 1 of session a",
                "step 1 of session a",
                "step 1 of session b",
                "step 3 of session a",
                "step 3 of session b",
                "step 4 of session b",
                "step 1 of session c",
                "step 3 of session c",
            ]
        );
        let kinds: Vec<Kind> = found.entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(kinds[6], Kind::SessionStart);
        assert_eq!(
            found.entries[6].changes(),
            Some("changes made between sessions")
        );
        assert_eq!(found.bad, 11);
    }

    #[test]
    fn since_starts_at_the_first_snapshot_taken_then() {
        let project = Project::new();
        let steps = sessions(&project);
        // Step 4 of session a (at 6 s) has no snapshot: the first one since then is step 4 of session b.
        let found = across(&steps, &project, Some(6_000), None, None).unwrap();
        assert_eq!(
            named(&steps, &found),
            [
                "the code before step 4 of session b",
                "step 4 of session b",
                "step 1 of session c",
                "step 3 of session c",
            ]
        );
        let error = across(&steps, &project, Some(60_000), None, None).unwrap_err();
        assert_eq!(error, "no snapshot was taken since then.");
        // A bad end before the date.
        let error = across(&steps, &project, Some(6_000), None, Some(3)).unwrap_err();
        assert_eq!(
            error,
            "the good end (the code before step 4 of session b) must come before the bad end (step 3 of \
             session a)."
        );
    }

    #[test]
    fn ends_across_sessions_and_the_steps_of_other_sessions_in_between() {
        let project = Project::new();
        let steps = sessions(&project);
        // From step 3 of session a to step 4 of session b.
        let found = across(&steps, &project, None, Some(3), Some(7)).unwrap();
        assert_eq!(
            named(&steps, &found),
            [
                "step 3 of session a",
                "step 3 of session b",
                "step 4 of session b"
            ]
        );

        // Session a alone: between its steps 1 and 3, session b took the snapshot of its step 1.
        let name = |order: usize| step_name(steps[order].step, None);
        let found = entries(
            &steps,
            &project.store,
            Scope::Session("a"),
            (None, None),
            &name,
            "Pick a good end.",
        )
        .unwrap();
        let labels: Vec<String> = found
            .entries
            .iter()
            .map(|entry| entry.label(None))
            .collect();
        assert_eq!(labels, ["step 1", "step 2", "step 3"]);
        let between = |from: usize, to: usize| {
            others_between(&steps, "a", &found.entries[from], &found.entries[to])
        };
        assert_eq!(between(0, 1), [1]);
        assert!(between(1, 2).is_empty());
        assert_eq!(between(0, 2), [1]);
    }
}
