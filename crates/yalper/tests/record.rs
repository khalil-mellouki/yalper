//! Feeds the hook fixtures to the recorder in this process, in the order Claude Code sends them, with file
//! changes between the events, and checks the rows and snapshots it records.

use std::fs;
use std::path::Path;
use std::process::Command;

use gix::ObjectId;
use yalper::hook::{ERRORS_LOG, HookInput, YALPER_DIR, find_yalper_dir};
use yalper::record::{record, record_until, session_stand_in};
use yalper::repo::YalperDir;
use yalper::snapshot::{self, SNAPSHOTS_DIR, ShadowStore};
use yalper::store::{LOCK_TIMEOUT, Store, WriterLock};

mod common;

fn fixture(name: &str) -> HookInput {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(name);
    HookInput::from_value(serde_json::from_slice(&fs::read(path).unwrap()).unwrap()).unwrap()
}

fn tree_id(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).unwrap()
}

/// The content of `path` in the snapshot `tree`, read straight from the shadow store with gix.
fn snapshot_text(root: &Path, tree: &str, path: &str) -> String {
    let repo = gix::open_opts(
        root.join(YALPER_DIR).join(SNAPSHOTS_DIR),
        gix::open::Options::isolated(),
    )
    .unwrap();
    let entry = repo
        .find_tree(tree_id(tree))
        .unwrap()
        .lookup_entry_by_path(path)
        .unwrap()
        .unwrap_or_else(|| panic!("{path} is not in the snapshot"));
    String::from_utf8(entry.object().unwrap().data.clone()).unwrap()
}

/// What a shell command run through the Bash tool does: another process rewrites the file.
fn rewrite_with_a_child_process(root: &Path) {
    let status = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", "echo changed by a script> src\\lib.rs"])
            .current_dir(root)
            .status()
    } else {
        Command::new("sh")
            .args(["-c", "echo 'changed by a script' > src/lib.rs"])
            .current_dir(root)
            .status()
    };
    assert!(status.unwrap().success());
}

#[test]
fn fixtures_in_order_record_every_step_with_its_snapshot() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("README.md"), "# Demo\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn old_name() {}\n").unwrap();
    // What `yalper init` sets up, including its baseline snapshot.
    let baseline = {
        let dir = common::init(root);
        let store = Store::open(&dir.dir, &dir.token).unwrap();
        let lock = WriterLock::acquire(&dir.dir, LOCK_TIMEOUT).unwrap();
        snapshot::snapshot(&dir.dir, &store, &lock).unwrap().tree_id
    };
    let dir: YalperDir = find_yalper_dir([root.to_path_buf()]).unwrap();

    // The developer edits a file before starting Claude Code, and another one before the first prompt.
    fs::write(root.join("README.md"), "# Demo project\n").unwrap();
    record(&dir, fixture("session_start.json")).unwrap();
    fs::write(root.join("notes.txt"), "try factorial\n").unwrap();
    record(&dir, fixture("user_prompt_submit.json")).unwrap();
    // The Write tool writes its file, then its PostToolUse hook runs.
    let factorial = "pub fn factorial(n: u64) -> u64 {\n    (1..=n).product()\n}\n";
    fs::write(root.join("src/factorial.rs"), factorial).unwrap();
    record(&dir, fixture("post_tool_use_write_windows.json")).unwrap();
    rewrite_with_a_child_process(root);
    record(&dir, fixture("post_tool_use_bash_subagent.json")).unwrap();
    record(&dir, fixture("post_tool_use_failure.json")).unwrap();
    record(&dir, fixture("stop.json")).unwrap();
    record(&dir, fixture("session_end.json")).unwrap();

    assert!(!root.join(YALPER_DIR).join(ERRORS_LOG).exists());
    let store = Store::open(&dir.dir, &dir.token).unwrap();
    let sessions = store.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    let session_id = "00893aaf-19fa-41d2-8238-13269b9b3ca0";
    assert_eq!(session.id, session_id);
    assert_eq!(session.source.as_deref(), Some("startup"));
    assert_eq!(session.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(session.end_reason.as_deref(), Some("prompt_input_exit"));
    assert_eq!(
        session.transcript_path.as_deref(),
        Some(
            "/home/dev/.claude/projects/-home-dev-project/00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl"
        )
    );

    let events = store.events(session_id).unwrap();
    type Summary<'a> = (u32, &'a str, Option<&'a str>, Option<bool>, Option<u32>);
    let summary: Vec<Summary> = events
        .iter()
        .map(|event| {
            (
                event.step,
                event.kind.as_str(),
                event.tool_name.as_deref(),
                event.success,
                event.files_changed,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1, "SessionStart", None, None, Some(1)),
            (2, "UserPromptSubmit", None, None, Some(1)),
            (3, "PostToolUse", Some("Write"), Some(true), Some(1)),
            (4, "PostToolUse", Some("Bash"), Some(true), Some(1)),
            (5, "PostToolUseFailure", Some("Bash"), Some(false), Some(0)),
            (6, "Stop", None, None, None),
            (7, "SessionEnd", None, None, None),
        ]
    );
    let tool_use_ids: Vec<Option<&str>> = events
        .iter()
        .map(|event| event.tool_use_id.as_deref())
        .collect();
    assert_eq!(
        tool_use_ids,
        [
            None,
            None,
            Some("toolu_01ABC123"),
            Some("toolu_01DEF456"),
            Some("toolu_01GHI789"),
            None,
            None
        ]
    );
    let agent_ids: Vec<Option<&str>> = events.iter().map(|e| e.agent_id.as_deref()).collect();
    assert_eq!(
        agent_ids,
        [None, None, None, Some("a1b2c3d4"), None, None, None]
    );
    // The whole payload is kept (these fixtures hold no secret, so redaction leaves them as they are).
    assert_eq!(events[4].payload, fixture("post_tool_use_failure.json").raw);
    assert_eq!(events[5].payload, fixture("stop.json").raw);

    assert!(session.started_at_ms <= events[0].ts_ms);
    assert!(
        events.windows(2).all(|pair| pair[0].ts_ms <= pair[1].ts_ms),
        "timestamps go back in time"
    );
    assert_eq!(session.ended_at_ms, Some(events[6].ts_ms));

    // Each snapshot holds exactly the change made before its event.
    let shadow = ShadowStore::open(&dir.dir, &dir.token).unwrap();
    let trees: Vec<ObjectId> = events[..5]
        .iter()
        .map(|event| tree_id(event.tree_id.as_deref().unwrap()))
        .collect();
    let changes = |old, new| {
        let changed = shadow.changed_paths(old, new).unwrap();
        (changed.added, changed.modified, changed.deleted)
    };
    // Each step keeps the tree its snapshot was built from: the snapshot before it, init's baseline first.
    let bases: Vec<Option<String>> = events.iter().map(|e| e.base_tree_id.clone()).collect();
    let mut expected_bases = vec![Some(baseline.to_string())];
    expected_bases.extend(trees[..4].iter().map(|tree| Some(tree.to_string())));
    expected_bases.extend([None, None]);
    assert_eq!(bases, expected_bases);
    let none = Vec::<String>::new;
    assert_eq!(
        changes(baseline, trees[0]),
        (none(), vec!["README.md".to_owned()], none())
    );
    assert_eq!(
        changes(trees[0], trees[1]),
        (vec!["notes.txt".to_owned()], none(), none())
    );
    assert_eq!(
        changes(trees[1], trees[2]),
        (vec!["src/factorial.rs".to_owned()], none(), none())
    );
    assert_eq!(
        changes(trees[2], trees[3]),
        (none(), vec!["src/lib.rs".to_owned()], none())
    );
    assert_eq!(trees[4], trees[3]);

    let tree = events[3].tree_id.as_deref().unwrap();
    assert_eq!(
        snapshot_text(root, tree, "src/lib.rs").trim_end(),
        "changed by a script"
    );
    assert_eq!(snapshot_text(root, tree, "src/factorial.rs"), factorial);
    assert_eq!(snapshot_text(root, tree, "README.md"), "# Demo project\n");
}

#[test]
fn an_event_of_an_unknown_session_creates_it_and_a_resume_reopens_it() {
    let project = common::project();
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();

    // Yalper was set up in the middle of a session: its first event is a tool call.
    record(&dir, fixture("post_tool_use_failure.json")).unwrap();
    record(&dir, fixture("session_end.json")).unwrap();
    let store = Store::open(&dir.dir, &dir.token).unwrap();
    let session = &store.sessions().unwrap()[0];
    assert_eq!(session.source, None);
    assert!(session.ended_at_ms.is_some());

    let mut resume = fixture("session_start.json");
    resume.source = Some("resume".to_owned());
    resume.raw["source"] = "resume".into();
    record(&dir, resume).unwrap();
    let session = &store.sessions().unwrap()[0];
    assert_eq!(session.source.as_deref(), Some("resume"));
    assert_eq!(
        (session.ended_at_ms, session.end_reason.as_deref()),
        (None, None)
    );
    let steps: Vec<(u32, String)> = store
        .events(&session.id)
        .unwrap()
        .into_iter()
        .map(|event| (event.step, event.kind))
        .collect();
    assert_eq!(
        steps,
        [
            (1, "PostToolUseFailure".to_owned()),
            (2, "SessionEnd".to_owned()),
            (3, "SessionStart".to_owned())
        ]
    );
}

#[test]
fn events_yalper_does_not_register_for_are_ignored() {
    let project = common::project();
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    let mut input = fixture("stop.json");
    input.event = yalper::hook::HookEvent::Other("PostToolBatch".to_owned());
    record(&dir, input).unwrap();
    assert_eq!(
        Store::open(&dir.dir, &dir.token)
            .unwrap()
            .sessions()
            .unwrap(),
        []
    );
}

#[test]
fn a_session_id_that_redaction_changes_is_stored_as_its_hash() {
    let project = common::project();
    let dir = find_yalper_dir([project.path().to_path_buf()]).unwrap();
    let secret_id = format!("ghp_{}", "L4k8J2h6G1f5D9s3A7p0".repeat(2).split_at(36).0);
    for name in ["session_start.json", "stop.json"] {
        let mut input = fixture(name);
        input.session_id = secret_id.clone();
        input.raw["session_id"] = secret_id.clone().into();
        record(&dir, input).unwrap();
    }

    let store = Store::open(&dir.dir, &dir.token).unwrap();
    let sessions = store.sessions().unwrap();
    assert_eq!(sessions.len(), 1, "both events belong to one session");
    assert_eq!(sessions[0].id, session_stand_in(&secret_id));
    assert_eq!(store.events(&sessions[0].id).unwrap().len(), 2);
}

#[test]
fn snapshots_that_keep_missing_the_deadline_are_counted_and_explained_once() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let dir = common::init(root);

    // A deadline that has already passed: every snapshot is abandoned, and every step still recorded.
    let lines: Vec<String> = (0..4)
        .map(|_| {
            record_until(
                &dir,
                fixture("post_tool_use_bash_subagent.json"),
                std::time::Instant::now(),
            )
            .unwrap_err()
        })
        .collect();
    for (index, line) in lines.iter().enumerate() {
        assert!(
            line.contains("recorded without a snapshot: abandoned")
                && line.contains(&format!("({} in a row)", index + 1)),
            "{line}"
        );
    }
    let advice = "Snapshots keep missing the deadline";
    assert!(!lines[1].contains(advice) && lines[2].contains(advice) && !lines[3].contains(advice));
    assert!(lines[2].contains(".gitignore"), "{}", lines[2]);

    // A snapshot in time ends the count.
    record(&dir, fixture("post_tool_use_bash_subagent.json")).unwrap();
    let error = record_until(
        &dir,
        fixture("post_tool_use_bash_subagent.json"),
        std::time::Instant::now(),
    )
    .unwrap_err();
    assert!(error.contains("(1 in a row)"), "{error}");
    let store = Store::open(&dir.dir, &dir.token).unwrap();
    let session = &store.sessions().unwrap()[0];
    let events = store.events(&session.id).unwrap();
    assert_eq!(events.len(), 6);
    let trees: Vec<bool> = events.iter().map(|event| event.tree_id.is_some()).collect();
    assert_eq!(trees, [false, false, false, false, true, false]);
    // The step in time has every change since the last snapshot.
    assert_eq!(events[4].files_changed, Some(1));
}
