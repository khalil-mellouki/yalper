//! `yalper init`: sets up recording for the git repository around the current directory.
//!
//! It creates `.yalper/` at the repository root with a new init token (see [`crate::repo`]), the event log
//! and the snapshot store, takes the baseline snapshot, excludes `.yalper/` and Claude Code's personal
//! settings file from git, and registers `yalper hook` for Claude Code in that settings file. Running it again
//! changes nothing, and nothing the user already had in the settings file is removed.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::hook::{HookEvent, YALPER_DIR};
use crate::repo::{self, GIT_ID_FILE, ID_FILE, Token, YalperDir};
use crate::safe_fs::{self, Access, OwnedDir};
use crate::snapshot::{self, SNAPSHOTS_DIR, ShadowStore};
use crate::store::{LOCK_TIMEOUT, Store, WriterLock};

/// Claude Code's settings folder at the repository root.
pub const CLAUDE_DIR: &str = ".claude";

/// Claude Code's personal (never committed) project settings file, inside [`CLAUDE_DIR`].
pub const SETTINGS_FILE: &str = "settings.local.json";

/// The events Yalper registers `yalper hook` for. No `PreToolUse`: the Post events carry the tool input too,
/// and a call denied before it ran changed nothing.
pub const EVENTS: [HookEvent; 6] = [
    HookEvent::SessionStart,
    HookEvent::UserPromptSubmit,
    HookEvent::PostToolUse,
    HookEvent::PostToolUseFailure,
    HookEvent::Stop,
    HookEvent::SessionEnd,
];

/// How long Claude Code lets the hook run, in seconds. `SessionEnd` gets none: its hooks share a fixed
/// budget, which Yalper leaves as it is.
pub const HOOK_TIMEOUT_SECONDS: u64 = 30;

/// The lines `yalper init` adds to the repository's `info/exclude`.
pub const EXCLUDE_LINES: [&str; 2] = [".yalper/", ".claude/settings.local.json"];

/// A settings file larger than this is not read.
const MAX_SETTINGS_BYTES: u64 = 16 * 1024 * 1024;

/// Sets up Yalper in the git repository around `start`, registering `exe` as the hook command, and reports
/// each step to `out`. A `.yalper/` that Yalper cannot use (not created by `yalper init` for this repository,
/// or with contents that belong to another one) is refused, unless `recreate` is set: then it is deleted and
/// created again.
///
/// Nothing is written when the repository root, its git directory, the settings file, or (without `recreate`)
/// an existing `.yalper/` is unusable. Returns the message to show when setup fails.
pub fn init(start: &Path, exe: &Path, recreate: bool, out: &mut dyn Write) -> Result<(), String> {
    let root = repo::find_root(start).ok_or_else(|| {
        format!(
            "{} is not inside a git repository. Run `yalper init` inside a git project (or run `git init` \
             first).",
            start.display()
        )
    })?;
    let unreadable_git = || {
        format!(
            "cannot find the git directory of {}: its .git file names a folder that does not exist",
            root.display()
        )
    };
    let git_dir = repo::git_dir(root)
        .filter(|dir| dir.is_dir())
        .ok_or_else(unreadable_git)?;
    let common_dir = repo::git_common_dir(root)
        .filter(|dir| dir.is_dir())
        .ok_or_else(unreadable_git)?;
    let exe = exe.to_str().ok_or_else(|| {
        format!(
            "the path of the yalper binary is not valid UTF-8: {}",
            exe.display()
        )
    })?;

    // Everything that can stop init is checked before anything is written.
    let settings_path = root.join(CLAUDE_DIR).join(SETTINGS_FILE);
    let settings = registered_settings(&settings_path, exe)?;
    let yalper_path = root.join(YALPER_DIR);
    let existing = inspect(root, &yalper_path)?;
    if let Existing::Unusable(why) = &existing
        && !recreate
    {
        return Err(format!(
            "{} already exists, but Yalper cannot use it: {why}. It may come from the repository itself, \
             so Yalper will not record into it. Move it away, or run `yalper init --recreate` to delete it \
             and start over. Nothing was changed.",
            yalper_path.display()
        ));
    }

    say(out, &format!("Setting up Yalper in {}", root.display()));
    let added = add_excludes(&common_dir.join("info").join("exclude"))
        .map_err(|error| format!("cannot update the git exclude file: {error}"))?;
    let yalper = match existing {
        Existing::Usable(yalper) => {
            say(out, "  .yalper/: already set up, kept");
            yalper
        }
        Existing::Missing => {
            let yalper = create_yalper_dir(&yalper_path, &git_dir)?;
            say(out, "  .yalper/: created");
            yalper
        }
        Existing::Unusable(_) => {
            remove(&yalper_path)
                .map_err(|error| format!("cannot delete {}: {error}", yalper_path.display()))?;
            let yalper = create_yalper_dir(&yalper_path, &git_dir)?;
            say(out, "  .yalper/: deleted and created again");
            yalper
        }
    };
    take_baseline(&yalper, out)?;
    drop(yalper);
    say(
        out,
        &if added.is_empty() {
            "  Git exclude: already set".to_owned()
        } else {
            format!("  Git exclude: added {}", added.join(", "))
        },
    );

    match settings {
        Settings::Unchanged => say(out, "  Claude Code hooks: already registered"),
        Settings::Updated(text) => {
            write_atomically(&settings_path, &text)
                .map_err(|error| format!("cannot write {}: {error}", settings_path.display()))?;
            say(
                out,
                "  Claude Code hooks: registered in .claude/settings.local.json",
            );
        }
    }

    if warn_if_the_hook_cannot_use(root, out) {
        say(out, "Done, but see the warning above.");
    } else {
        say(
            out,
            "Done. Claude Code sessions in this project are now recorded. Hooks need Claude Code 2.1.139 \
             or later, and only run once this folder is trusted in Claude Code.",
        );
    }
    Ok(())
}

/// Whether `handler` (one entry of a matcher group's `hooks` array) runs `yalper hook`: its first argument is
/// `hook` and its command is a file named `yalper` (with any extension, such as `.exe`).
pub fn is_yalper_handler(handler: &Value) -> bool {
    let first_arg = handler
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| args.first())
        .and_then(Value::as_str);
    let stem = handler
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| Path::new(command).file_stem())
        .and_then(|stem| stem.to_str());
    first_arg == Some("hook") && stem.is_some_and(|stem| stem.eq_ignore_ascii_case("yalper"))
}

/// The handler `yalper init` registers for `event`.
fn handler(event: HookEvent, exe: &str) -> Value {
    let mut handler = json!({"type": "command", "command": exe, "args": ["hook"]});
    if event != HookEvent::SessionEnd {
        handler["timeout"] = json!(HOOK_TIMEOUT_SECONDS);
    }
    handler
}

/// What registering the hooks does to the settings file.
#[derive(Debug, PartialEq, Eq)]
enum Settings {
    /// Every event already has a Yalper handler with this command.
    Unchanged,
    /// The new content of the file.
    Updated(String),
}

/// Reads the settings file at `path` (if it exists) and registers the hooks in it, see [`register_hooks`].
fn registered_settings(path: &Path, exe: &str) -> Result<Settings, String> {
    let claude_dir = path.parent().unwrap_or(path);
    if fs::symlink_metadata(claude_dir).is_ok_and(|metadata| !metadata.is_dir()) {
        return Err(format!(
            "{} is not a folder (it may be a link), so Yalper will not write its settings there",
            claude_dir.display()
        ));
    }
    let existing = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!(
                "{} is not a regular file (it may be a link), so Yalper will not edit it",
                path.display()
            ));
        }
        Ok(_) => {
            let bytes = safe_fs::read_small_regular_file(path, MAX_SETTINGS_BYTES)
                .ok_or_else(|| format!("cannot read {}", path.display()))?;
            Some(String::from_utf8(bytes).map_err(|_| {
                format!(
                    "{} is not valid UTF-8. Nothing was changed.",
                    path.display()
                )
            })?)
        }
    };
    register_hooks(existing.as_deref(), exe).map_err(|why| {
        format!(
            "cannot register the hooks in {}: {why}. Nothing was changed.",
            path.display()
        )
    })
}

/// Registers `exe` for every event of [`EVENTS`] in the settings JSON `existing` (`None`: no file yet).
///
/// An event whose matcher groups already contain a Yalper handler (see [`is_yalper_handler`]) keeps it, with
/// its command set to `exe`; any other event gets one more matcher group, without a matcher, holding only
/// Yalper's handler. Every other key, group and handler is kept as it is, in its order. The result is
/// formatted with 2-space indentation and a trailing newline.
fn register_hooks(existing: Option<&str>, exe: &str) -> Result<Settings, String> {
    let mut settings = match existing {
        Some(text) => {
            serde_json::from_str(text).map_err(|error| format!("it is not valid JSON ({error})"))?
        }
        None => Value::Object(Map::new()),
    };
    let mut changed = existing.is_none();
    let Value::Object(top) = &mut settings else {
        return Err("it does not hold a JSON object".to_owned());
    };
    let Value::Object(hooks) = top.entry("hooks").or_insert_with(|| json!({})) else {
        return Err("`hooks` is not an object".to_owned());
    };
    for event in EVENTS {
        let name = event.name().unwrap_or_default();
        let Value::Array(groups) = hooks.entry(name).or_insert_with(|| json!([])) else {
            return Err(format!("`hooks.{name}` is not an array"));
        };
        let mut found = false;
        for group in groups.iter_mut() {
            let handlers = match group.as_object_mut().map(|group| group.get_mut("hooks")) {
                Some(Some(Value::Array(handlers))) => handlers,
                Some(None) => continue,
                _ => {
                    return Err(format!(
                        "`hooks.{name}` holds an entry that is not a matcher group"
                    ));
                }
            };
            for handler in handlers.iter_mut() {
                if !handler.is_object() {
                    return Err(format!("`hooks.{name}` holds a hook that is not an object"));
                }
                if is_yalper_handler(handler) {
                    found = true;
                    if handler["command"] != exe {
                        handler["command"] = json!(exe);
                        changed = true;
                    }
                }
            }
        }
        if !found {
            groups.push(json!({"hooks": [handler(event, exe)]}));
            changed = true;
        }
    }
    if !changed {
        return Ok(Settings::Unchanged);
    }
    let mut text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(Settings::Updated(text))
}

/// Writes `text` to a temporary file next to `path`, then renames it over `path`, so the file is never seen
/// half written. Creates the parent folder if needed.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".yalper-tmp");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

/// Appends each line of [`EXCLUDE_LINES`] that the exclude file at `path` does not already have (in any
/// equivalent form, such as `/.yalper/`), creating the file and its folder if needed. Returns the lines
/// added.
fn add_excludes(path: &Path) -> io::Result<Vec<&'static str>> {
    let text = match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    // `/.yalper`, `.yalper/` and `.yalper` all exclude the folder at the root.
    fn pattern(line: &str) -> &str {
        let line = line.trim_end();
        line.strip_prefix('/').unwrap_or(line).trim_end_matches('/')
    }
    let missing: Vec<&'static str> = EXCLUDE_LINES
        .into_iter()
        .filter(|wanted| !text.lines().any(|line| pattern(line) == pattern(wanted)))
        .collect();
    if missing.is_empty() {
        return Ok(missing);
    }
    let mut addition = String::new();
    if !text.is_empty() && !text.ends_with('\n') {
        addition.push('\n');
    }
    for line in &missing {
        addition.push_str(line);
        addition.push('\n');
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(addition.as_bytes())?;
    Ok(missing)
}

/// What is at `.yalper/` before init changes anything.
enum Existing {
    Missing,
    /// `yalper init` created it for this repository, and its event log and snapshot store belong to it.
    Usable(YalperDir),
    /// Anything else, with the reason.
    Unusable(String),
}

/// Looks at `path`, the `.yalper/` of the repository at `root`. An existing one is usable only if
/// `yalper init` created it for this repository (matching init tokens) and its event log and snapshot store
/// carry the same token; a missing event log or store is created then.
fn inspect(root: &Path, path: &Path) -> Result<Existing, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Existing::Missing),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        Ok(_) => Ok(match adopt(root, path) {
            Ok(yalper) => Existing::Usable(yalper),
            Err(why) => Existing::Unusable(why),
        }),
    }
}

/// Opens the existing `.yalper/` at `path` if `yalper init` created it for the repository at `root`, and
/// makes sure its event log and snapshot store belong to it (creating them if they are missing).
fn adopt(root: &Path, path: &Path) -> Result<YalperDir, String> {
    let dir = OwnedDir::open(path).map_err(|error| error.to_string())?;
    let token = repo::init_token(root, &dir).ok_or_else(|| {
        "it was not created by `yalper init` for this repository (its init token is missing or different)"
            .to_owned()
    })?;
    let yalper = YalperDir { dir, token };
    create_contents(&yalper)?;
    Ok(yalper)
}

/// Creates `.yalper/` at `path` (on Unix readable only by its owner) with a new init token written to it and
/// to `git_dir`, then its event log and snapshot store.
fn create_yalper_dir(path: &Path, git_dir: &Path) -> Result<YalperDir, String> {
    let failed = |error: io::Error| format!("cannot create {}: {error}", path.display());
    #[cfg_attr(windows, allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path).map_err(failed)?;
    let dir = OwnedDir::open(path).map_err(failed)?;
    let token = Token::generate().map_err(failed)?;
    let line = format!("{token}\n");
    fs::write(git_dir.join(GIT_ID_FILE), &line).map_err(|error| {
        format!(
            "cannot write {}: {error}",
            git_dir.join(GIT_ID_FILE).display()
        )
    })?;
    dir.open_file(ID_FILE, Access::ReadWrite)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .map_err(failed)?;
    let yalper = YalperDir { dir, token };
    create_contents(&yalper)?;
    Ok(yalper)
}

/// Opens (creating it if missing) the event log and the snapshot store of `yalper`, which fails if either
/// was created for another init token.
fn create_contents(yalper: &YalperDir) -> Result<(), String> {
    Store::open(&yalper.dir, &yalper.token).map_err(|error| error.to_string())?;
    let store = match fs::symlink_metadata(yalper.dir.path().join(SNAPSHOTS_DIR)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ShadowStore::init(&yalper.dir, &yalper.token)
        }
        _ => ShadowStore::open(&yalper.dir, &yalper.token),
    };
    store.map(drop).map_err(|error| error.to_string())
}

/// Deletes whatever is at `path`: a folder with everything in it, or a file or link (never its target).
fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        // A link to a folder (a Windows junction or directory symlink) is removed like a folder.
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    }
}

/// Takes the first snapshot, so the first session only stores what changed. A `.yalper/` that already has
/// one is left as it is: a new snapshot now would hide the changes made since then from the next session.
fn take_baseline(yalper: &YalperDir, out: &mut dyn Write) -> Result<(), String> {
    let store = Store::open(&yalper.dir, &yalper.token).map_err(|error| error.to_string())?;
    if store.has_snapshot().map_err(|error| error.to_string())? {
        return Ok(());
    }
    let _ = write!(out, "  Baseline snapshot: ");
    let _ = out.flush();
    let lock = WriterLock::acquire(&yalper.dir, LOCK_TIMEOUT).map_err(|error| error.to_string())?;
    match snapshot::snapshot(&yalper.dir, &store, &lock) {
        Ok(snapshot) => {
            let files = snapshot.changed.len();
            say(
                out,
                &format!("{files} file{}", if files == 1 { "" } else { "s" }),
            );
            if let Some(problems) = snapshot.problems() {
                say(out, &format!("  warning: {problems}"));
            }
        }
        Err(error) => say(
            out,
            &format!(
                "failed ({error}). Recording still works: each step tries to take a snapshot again."
            ),
        ),
    }
    Ok(())
}

/// Runs the checks the hook makes before it records (see [`repo::open_yalper_dir`]) and prints a warning if
/// they fail, for example on a file system that does not keep Unix owners and permissions (WSL's `/mnt/c`,
/// some network shares), or when the project belongs to another user (a dev container). Returns whether a
/// warning was printed.
fn warn_if_the_hook_cannot_use(root: &Path, out: &mut dyn Write) -> bool {
    let mut warned = false;
    if let Err(why) = repo::open_yalper_dir(root, &root.join(YALPER_DIR)) {
        say(
            out,
            &format!(
                "warning: the hook will not record into .yalper/: {why}. File systems that do not keep Unix \
                 owners and permissions (WSL's /mnt drives, network shares) cause this: keep the project on \
                 a local file system, or run `chmod 700 .yalper` where permissions are kept."
            ),
        );
        warned = true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = fs::metadata(root).map(|metadata| metadata.uid());
        if owner.is_ok_and(|owner| owner != rustix::process::geteuid().as_raw()) {
            say(
                out,
                &format!(
                    "warning: {} belongs to another user. Hooks run as the user running Claude Code and only \
                     record into a .yalper folder that user owns, so run `yalper init` as that user.",
                    root.display()
                ),
            );
            warned = true;
        }
    }
    warned
}

/// Prints one line of the report. A closed output does not stop init.
fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/yalper/bin/yalper";

    fn updated(existing: Option<&str>, exe: &str) -> Value {
        match register_hooks(existing, exe).unwrap() {
            Settings::Updated(text) => {
                assert!(text.ends_with("}\n"), "{text}");
                serde_json::from_str(&text).unwrap()
            }
            Settings::Unchanged => panic!("expected a change"),
        }
    }

    #[test]
    fn a_new_file_gets_one_group_per_event_with_the_exec_form_handler() {
        let settings = updated(None, EXE);
        let hooks = settings["hooks"].as_object().unwrap();
        let names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "SessionStart",
                "UserPromptSubmit",
                "PostToolUse",
                "PostToolUseFailure",
                "Stop",
                "SessionEnd"
            ]
        );
        assert_eq!(
            hooks["PostToolUse"],
            json!([{"hooks": [{"type": "command", "command": EXE, "args": ["hook"], "timeout": 30}]}])
        );
        assert_eq!(
            hooks["SessionEnd"],
            json!([{"hooks": [{"type": "command", "command": EXE, "args": ["hook"]}]}])
        );
        for handler in hooks.values() {
            assert!(handler[0]["hooks"][0].get("async").is_none());
            assert!(handler[0].get("matcher").is_none());
        }
    }

    #[test]
    fn registering_twice_changes_nothing() {
        let Settings::Updated(text) = register_hooks(None, EXE).unwrap() else {
            panic!("expected a change");
        };
        assert_eq!(
            register_hooks(Some(&text), EXE).unwrap(),
            Settings::Unchanged
        );
    }

    #[test]
    fn a_handler_with_another_path_is_updated_in_place() {
        let Settings::Updated(text) = register_hooks(None, "/old/yalper").unwrap() else {
            panic!("expected a change");
        };
        let settings = updated(Some(&text), EXE);
        for event in EVENTS {
            let groups = &settings["hooks"][event.name().unwrap()];
            assert_eq!(groups.as_array().unwrap().len(), 1);
            assert_eq!(groups[0]["hooks"][0]["command"], EXE);
        }
    }

    #[test]
    fn other_settings_and_hooks_are_kept_in_order() {
        let existing = r#"{
            "permissions": {"allow": ["Bash(cargo test)"]},
            "hooks": {
                "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt.sh"}]}],
                "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "guard"}]}]
            },
            "model": "x"
        }"#;
        let settings = updated(Some(existing), EXE);
        let keys: Vec<&str> = settings
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["permissions", "hooks", "model"]);
        assert_eq!(
            settings["permissions"],
            json!({"allow": ["Bash(cargo test)"]})
        );
        let post = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            post[0],
            json!({"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt.sh"}]})
        );
        assert_eq!(post[1]["hooks"][0]["command"], EXE);
        assert_eq!(
            settings["hooks"]["PreToolUse"],
            json!([{"matcher": "*", "hooks": [{"type": "command", "command": "guard"}]}])
        );
    }

    #[test]
    fn unexpected_shapes_are_refused() {
        for existing in [
            "",
            "{",
            "[]",
            r#"{"hooks": []}"#,
            r#"{"hooks": {"Stop": {}}}"#,
            r#"{"hooks": {"Stop": ["x"]}}"#,
            r#"{"hooks": {"Stop": [{"hooks": {}}]}}"#,
            r#"{"hooks": {"Stop": [{"hooks": ["x"]}]}}"#,
        ] {
            assert!(register_hooks(Some(existing), EXE).is_err(), "{existing}");
        }
    }

    #[test]
    fn yalper_handlers_are_recognized_by_file_stem_and_first_argument() {
        let handler = |command: &str, args: Value| json!({"type": "command", "command": command, "args": args});
        assert!(is_yalper_handler(&handler(EXE, json!(["hook"]))));
        assert!(is_yalper_handler(&handler("yalper", json!(["hook", "x"]))));
        assert!(is_yalper_handler(&handler(
            "/x/Yalper.exe",
            json!(["hook"])
        )));
        assert!(!is_yalper_handler(&handler(EXE, json!(["log"]))));
        assert!(!is_yalper_handler(&handler(EXE, json!([]))));
        assert!(!is_yalper_handler(&handler(
            "/x/yalper-dev",
            json!(["hook"])
        )));
        assert!(!is_yalper_handler(
            &json!({"type": "command", "command": "yalper hook"})
        ));
    }

    #[test]
    fn exclude_lines_are_added_once_after_a_last_line_without_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("info").join("exclude");
        assert_eq!(add_excludes(&path).unwrap(), EXCLUDE_LINES);
        assert_eq!(add_excludes(&path).unwrap(), Vec::<&str>::new());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            ".yalper/\n.claude/settings.local.json\n"
        );

        fs::write(&path, "# mine\n/.yalper\r\ntarget").unwrap();
        assert_eq!(
            add_excludes(&path).unwrap(),
            [".claude/settings.local.json"]
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# mine\n/.yalper\r\ntarget\n.claude/settings.local.json\n"
        );
    }
}
