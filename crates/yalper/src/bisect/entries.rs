//! The entries of a bisect of one recorded session: the code at each step that changed files, in the order the
//! snapshots were taken, from the good end to the bad end.
//!
//! - The good end is, by default, the code at the start of the session: the snapshot of its first step when that
//!   step is the session's start (the user's edits made before the session included), otherwise the tree its
//!   first snapshot was built from (Yalper was set up during the session). `--good <step>` picks the code at
//!   that step instead.
//! - The bad end is the code at the session's last step, or at the step `--bad <step>` names.
//! - A step without a snapshot of its own (the end of a turn or of the session, a missed deadline) has the code
//!   of the latest snapshot at or before it: its changes are in the next snapshot.
//! - Between the ends, only the steps whose code differs from the entry before are entries, so the engine never
//!   tests the same code twice. A step whose snapshot is no longer in the snapshot store is left out and named.

use gix::ObjectId;

use crate::hook::HookEvent;
use crate::snapshot::ShadowStore;
use crate::store::Event;

/// The code of one entry: the snapshot of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The step whose snapshot holds the code.
    pub step: u32,
    /// The entry is the code before `step`, the tree that step's snapshot was built from: only for a good end
    /// that comes before the session's first snapshot.
    pub before: bool,
    pub tree: ObjectId,
    /// The tree the step's snapshot was built from, when it is known and in the snapshot store.
    pub base: Option<ObjectId>,
}

impl Entry {
    /// How the entry is named on screen: `step 12`, or `the code before step 3`.
    pub fn label(&self) -> String {
        if self.before {
            format!("the code before step {}", self.step)
        } else {
            format!("step {}", self.step)
        }
    }
}

/// What [`session_entries`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entries {
    /// The good end first, then each step up to the bad end whose code differs from the entry before. The bad
    /// end is the last one, unless no step after the good end changed files.
    pub entries: Vec<Entry>,
    /// The step the bad end names, as asked: its code is the last entry's.
    pub bad_step: u32,
    /// Steps between the ends whose snapshot is not in the snapshot store, left out.
    pub missing: Vec<u32>,
}

/// The entries of the session whose steps are `events` (in order, step 1 first), with the snapshots of
/// `shadow`, between the steps `good` and `bad` (see the module documentation). Returns why there are none when
/// a step asked for does not exist, the ends are in the wrong order, or no snapshot gives the code of an end.
pub fn session_entries(
    events: &[Event],
    shadow: &ShadowStore,
    good: Option<u32>,
    bad: Option<u32>,
) -> Result<Entries, String> {
    let Some(last) = events.last().map(|event| event.step) else {
        return Err("it has no steps recorded yet.".to_owned());
    };
    for (option, step) in [("--good", good), ("--bad", bad)] {
        if let Some(step) = step.filter(|&step| step == 0 || step > last) {
            return Err(format!(
                "it has no step {step} ({option}): its steps are 1 to {last}. `yalper log` lists them."
            ));
        }
    }
    let starts_the_session = events.first().is_some_and(|event| {
        event.step == 1 && matches!(HookEvent::from_name(&event.kind), HookEvent::SessionStart)
    });
    let good_step = good.unwrap_or(u32::from(starts_the_session));
    let bad_step = bad.unwrap_or(last);
    if good_step >= bad_step {
        return Err(format!(
            "the good end (step {good_step}) must come before the bad end (step {bad_step})."
        ));
    }

    let present = |hex: &Option<String>| {
        hex.as_deref()
            .and_then(|hex| ObjectId::from_hex(hex.as_bytes()).ok())
            .filter(|&id| shadow.has_object(id))
    };
    let mut snapshots = Vec::new();
    let mut missing = Vec::new();
    for event in events.iter().filter(|event| event.tree_id.is_some()) {
        match present(&event.tree_id) {
            Some(tree) => snapshots.push(Entry {
                step: event.step,
                before: false,
                tree,
                base: present(&event.base_tree_id),
            }),
            None => missing.push(event.step),
        }
    }

    let good_end = match snapshots.iter().rev().find(|entry| entry.step <= good_step) {
        Some(entry) => *entry,
        None => {
            let Some(first) = snapshots.iter().find(|entry| entry.step > good_step) else {
                return Err(
                    "no step has a snapshot in the snapshot store, so there is no code to test."
                        .to_owned(),
                );
            };
            let Some(base) = first.base else {
                return Err(format!(
                    "the code before step {} is not known (its snapshot does not say what it was built \
                     from). Pick a good end with --good <step>.",
                    first.step
                ));
            };
            Entry {
                step: first.step,
                before: true,
                tree: base,
                base: None,
            }
        }
    };
    // The steps after the good end: from its own step when it is the code before that step.
    let after = if good_end.before {
        good_end.step - 1
    } else {
        good_end.step
    };
    let mut entries = vec![good_end];
    for entry in snapshots
        .iter()
        .filter(|entry| entry.step > after && entry.step <= bad_step)
    {
        if entries.last().is_some_and(|last| last.tree != entry.tree) {
            entries.push(*entry);
        }
    }
    missing.retain(|&step| step > after && step <= bad_step);
    Ok(Entries {
        entries,
        bad_step,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bisect::testing::Project;
    use serde_json::Value;

    fn event(step: u32, kind: &str, tree: Option<ObjectId>, base: Option<ObjectId>) -> Event {
        Event {
            session_id: "s".to_owned(),
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

    fn steps(entries: &Entries) -> Vec<(u32, bool)> {
        entries
            .entries
            .iter()
            .map(|entry| (entry.step, entry.before))
            .collect()
    }

    #[test]
    fn the_whole_session_from_its_start_to_its_last_step() {
        let project = Project::new();
        let (events, [_, a, b, c, d]) = session(&project);
        let found = session_entries(&events, &project.store, None, None).unwrap();
        assert_eq!(
            steps(&found),
            [(1, false), (3, false), (5, false), (7, false)]
        );
        let trees: Vec<ObjectId> = found.entries.iter().map(|entry| entry.tree).collect();
        assert_eq!(trees, [a, b, c, d]);
        assert_eq!(found.bad_step, 7);
        assert!(found.missing.is_empty());
        assert_eq!(found.entries[0].label(), "step 1");
    }

    #[test]
    fn a_step_without_a_tree_means_the_latest_tree_at_or_before_it() {
        let project = Project::new();
        let (events, _) = session(&project);
        let found = session_entries(&events, &project.store, Some(4), Some(6)).unwrap();
        assert_eq!(steps(&found), [(3, false), (5, false)]);
        assert_eq!(found.bad_step, 6);
        let found = session_entries(&events, &project.store, Some(2), Some(4)).unwrap();
        assert_eq!(steps(&found), [(2, false), (3, false)]);
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
        let found = session_entries(&events, &project.store, None, None).unwrap();
        assert_eq!(found.entries[0].tree, before);
        assert_eq!(found.entries[0].label(), "the code before step 1");
        assert_eq!(
            steps(&found),
            [(1, true), (1, false), (3, false), (5, false), (7, false)]
        );
    }

    #[test]
    fn a_snapshot_missing_from_the_store_is_left_out_and_named() {
        let project = Project::new();
        let (mut events, _) = session(&project);
        events[4].tree_id = Some("1".repeat(40));
        events[2].tree_id = Some("not hex".to_owned());
        let found = session_entries(&events, &project.store, None, None).unwrap();
        assert_eq!(steps(&found), [(1, false), (6, false), (7, false)]);
        assert_eq!(found.missing, [3, 5]);
    }

    #[test]
    fn ends_that_do_not_exist_or_are_in_the_wrong_order_are_refused() {
        let project = Project::new();
        let (events, _) = session(&project);
        for (good, bad, message) in [
            (Some(8), None, "no step 8 (--good)"),
            (None, Some(0), "no step 0 (--bad)"),
            (Some(5), Some(5), "must come before the bad end"),
            (Some(7), None, "must come before the bad end"),
        ] {
            let error = session_entries(&events, &project.store, good, bad).unwrap_err();
            assert!(error.contains(message), "{error}");
        }
        assert!(session_entries(&[], &project.store, None, None).is_err());
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
        let found = session_entries(&events, &project.store, None, None).unwrap();
        assert_eq!(steps(&found), [(1, false)]);
    }
}
