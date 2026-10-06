//! `yalper uninstall`: undoes `yalper init` for the git repository around the current directory.
//!
//! It removes Yalper's hook registration from Claude Code's personal settings file, deleting the file if
//! nothing else is left in it, and the git exclude lines `yalper init` adds once the paths they exclude are
//! gone. Recordings in `.yalper/` are kept, with the init token that ties them to the repository, so a later
//! `yalper init` picks them up again. `--purge` also deletes `.yalper/` and the token. The shared
//! `.claude/settings.json` is never touched, and running it again changes nothing.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde_json::Value;

use crate::hook::YALPER_DIR;
use crate::init::{
    self, BOM, CLAUDE_DIR, EXCLUDE_LINES, Repository, SETTINGS_FILE, is_registration, matches_all,
    replace_file, say,
};
use crate::repo::{self, GIT_ID_FILE, ID_FILE, Refusal};
use crate::safe_fs::OwnedDir;

/// Removes Yalper from the git repository around `start` and reports each step to `out`. With `purge`,
/// `.yalper/` and its recordings are deleted too.
///
/// Nothing is changed when the settings file or the git exclude file cannot be edited safely, or when
/// `purge` is set and `.yalper/` is not a folder `yalper init` created for this repository. Returns the
/// message to show when uninstall fails.
pub fn uninstall(start: &Path, purge: bool, out: &mut dyn Write) -> Result<(), String> {
    let repository = Repository::find(
        start,
        "Run `yalper uninstall` inside the git project Yalper was set up in.",
    )?;
    let root = repository.root.as_path();

    // Everything that can stop uninstall is checked before anything is changed.
    let settings_path = root.join(CLAUDE_DIR).join(SETTINGS_FILE);
    let settings = unregistered_settings(&settings_path)?;
    let yalper_path = root.join(YALPER_DIR);
    let yalper = inspect(root, &yalper_path);
    if purge && let Yalper::Foreign(why) = &yalper {
        return Err(format!(
            "{} was not deleted: {why}. It may come from the repository itself, so Yalper will not delete \
             it. Delete it by hand if you are sure, or run `yalper uninstall` without --purge. Nothing was \
             changed.",
            yalper_path.display()
        ));
    }
    let exclude_path = repository.exclude_file();
    init::check_exclude_path(&exclude_path).map_err(|error| {
        format!("cannot update the git exclude file: {error}. Nothing was changed.")
    })?;
    let token_path = repository.git_dir.join(GIT_ID_FILE);
    // Without any of these, the project was never set up (or is already cleaned up): the exclude lines are
    // then the user's own and are left alone.
    let was_set_up = settings != Settings::Unchanged
        || matches!(yalper, Yalper::Ours | Yalper::Empty)
        || fs::symlink_metadata(&token_path).is_ok();

    // What was done, and whether anything changed: a line saying what was kept is not a change.
    let mut report = Vec::new();
    let mut changed = settings != Settings::Unchanged;
    match settings {
        Settings::Unchanged => {}
        Settings::Updated(text) => {
            let existing = fs::symlink_metadata(&settings_path).ok();
            replace_file(&settings_path, text.as_bytes(), existing.as_ref())
                .map_err(|error| format!("cannot write {}: {error}", settings_path.display()))?;
            report.push("  Claude Code hooks: removed from .claude/settings.local.json".to_owned());
        }
        Settings::Emptied => {
            fs::remove_file(&settings_path)
                .map_err(|error| format!("cannot delete {}: {error}", settings_path.display()))?;
            // `yalper init` creates the folder when it is missing. Only an empty one is removed.
            let _ = fs::remove_dir(root.join(CLAUDE_DIR));
            report.push(
                "  Claude Code hooks: removed (.claude/settings.local.json held nothing else and was \
                 deleted)"
                    .to_owned(),
            );
        }
    }

    match (&yalper, purge) {
        (Yalper::Missing, _) => {}
        (Yalper::Ours | Yalper::Empty, true) => {
            delete_yalper_dir(&yalper_path).map_err(|error| {
                format!(
                    "cannot delete {}: {error}. Close the Claude Code sessions running in this project \
                     and run `yalper uninstall --purge` again.",
                    yalper_path.display()
                )
            })?;
            report.push("  .yalper/: deleted".to_owned());
            changed = true;
        }
        (Yalper::Ours, false) => report.push(
            "  .yalper/: kept, with your recordings (`yalper uninstall --purge` deletes them)"
                .to_owned(),
        ),
        // With `purge`, a foreign one was refused above.
        (Yalper::Empty | Yalper::Foreign(_), _) => report.push("  .yalper/: kept".to_owned()),
    }

    // The token binds `.yalper/` to the repository: it goes when `.yalper/` is gone.
    let yalper_gone = fs::symlink_metadata(&yalper_path).is_err();
    if yalper_gone && fs::symlink_metadata(&token_path).is_ok_and(|metadata| !metadata.is_dir()) {
        fs::remove_file(&token_path)
            .map_err(|error| format!("cannot delete {}: {error}", token_path.display()))?;
        report.push("  Init token: removed from the git directory".to_owned());
        changed = true;
    }

    if was_set_up {
        let unneeded: Vec<&str> = EXCLUDE_LINES
            .into_iter()
            .filter(|line| fs::symlink_metadata(root.join(line.trim_end_matches('/'))).is_err())
            .collect();
        match remove_exclude_lines(&repository, &exclude_path, &unneeded)? {
            Exclude::Unchanged => {}
            Exclude::Removed(lines) => {
                report.push(format!("  Git exclude: removed {}", lines.join(", ")));
                changed = true;
            }
            Exclude::Shared(lines) => report.push(format!(
                "  Git exclude: kept {} (other worktrees of this repository share the exclude file; \
                 remove the lines by hand once none of them uses Yalper)",
                lines.join(", ")
            )),
        }
    }

    if !changed {
        say(
            out,
            &if matches!(yalper, Yalper::Ours) {
                format!(
                    "Yalper's hooks are not registered in {}, nothing to remove. Recordings are kept in \
                     .yalper/ (`yalper uninstall --purge` deletes them).",
                    root.display()
                )
            } else {
                format!(
                    "Yalper is not set up in {}, nothing to remove.",
                    root.display()
                )
            },
        );
        return Ok(());
    }
    say(out, &format!("Removing Yalper from {}", root.display()));
    for line in report {
        say(out, &line);
    }
    say(out, "Done.");
    Ok(())
}

/// What is at `.yalper/`.
#[derive(Debug, PartialEq, Eq)]
enum Yalper {
    Missing,
    /// An empty folder (git never creates one, so a clone cannot plant it).
    Empty,
    /// A folder `yalper init` created for this repository: its init token matches.
    Ours,
    /// Anything else, with the reason it is not Yalper's to delete.
    Foreign(String),
}

/// Looks at `path`, the `.yalper/` of the repository at `root`, without changing anything or following a
/// link.
fn inspect(root: &Path, path: &Path) -> Yalper {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Yalper::Missing,
        Err(error) => return Yalper::Foreign(format!("cannot read it ({error})")),
        Ok(metadata) if !metadata.is_dir() => {
            return Yalper::Foreign("it is a link or a file, not a folder".to_owned());
        }
        Ok(_) => {}
    }
    match fs::read_dir(path).map(|mut entries| entries.next().is_none()) {
        Ok(true) => return Yalper::Empty,
        Ok(false) => {}
        Err(error) => return Yalper::Foreign(format!("cannot read it ({error})")),
    }
    // Who may write to it does not matter here (`repo::open_yalper_dir` checks that for recording), only
    // where it comes from.
    match OwnedDir::open(path) {
        Ok(dir) if repo::init_token(root, &dir).is_some() => Yalper::Ours,
        Ok(_) => Yalper::Foreign(Refusal::TokenMismatch.to_string()),
        Err(error) => Yalper::Foreign(error.to_string()),
    }
}

/// Deletes the `.yalper/` folder at `path` without following any link inside it. Its `id` file goes last:
/// if a file in use stops the deletion halfway (Windows), the folder still carries its init token, so
/// running `yalper uninstall --purge` again recognizes it and finishes the job.
fn delete_yalper_dir(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() != ID_FILE {
            init::remove(&entry.path())?;
        }
    }
    match fs::remove_file(path.join(ID_FILE)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    fs::remove_dir(path)
}

/// What [`remove_exclude_lines`] did.
enum Exclude<'a> {
    /// None of the lines is in the file.
    Unchanged,
    /// These lines were removed.
    Removed(Vec<&'a str>),
    /// These lines were kept, because other worktrees read the same exclude file.
    Shared(Vec<&'a str>),
}

/// Removes from the exclude file at `path` every line that is exactly one of `lines` (as `yalper init` writes
/// them). The exclude file is shared by every worktree of the repository, and another one may still need
/// them: then they are kept.
fn remove_exclude_lines<'a>(
    repository: &Repository,
    path: &Path,
    lines: &[&'a str],
) -> Result<Exclude<'a>, String> {
    let failed = |error: io::Error| format!("cannot update the git exclude file: {error}");
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Exclude::Unchanged),
        Err(error) => return Err(failed(error)),
    };
    let (kept, found) = without_lines(&bytes, lines);
    if found.is_empty() {
        return Ok(Exclude::Unchanged);
    }
    if shares_exclude_file(repository) {
        return Ok(Exclude::Shared(found));
    }
    let existing = fs::symlink_metadata(path).ok();
    replace_file(path, &kept, existing.as_ref()).map_err(failed)?;
    Ok(Exclude::Removed(found))
}

/// `text` without the lines that are exactly one of `lines` (a line ending in `\r\n` counts too), and the
/// lines of `lines` that were found, in their order.
fn without_lines<'a>(text: &[u8], lines: &[&'a str]) -> (Vec<u8>, Vec<&'a str>) {
    let mut kept = Vec::with_capacity(text.len());
    let mut found = Vec::new();
    for line in text.split_inclusive(|byte| *byte == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        match lines.iter().find(|wanted| wanted.as_bytes() == content) {
            Some(wanted) => {
                if !found.contains(wanted) {
                    found.push(*wanted);
                }
            }
            None => kept.extend_from_slice(line),
        }
    }
    found.sort_by_key(|line| lines.iter().position(|wanted| wanted == line));
    (kept, found)
}

/// Whether another worktree reads the same exclude file: this is a linked worktree, or the repository has
/// linked worktrees.
fn shares_exclude_file(repository: &Repository) -> bool {
    repository.git_dir != repository.common_dir
        || fs::read_dir(repository.common_dir.join("worktrees"))
            .is_ok_and(|mut entries| entries.next().is_some())
}

/// What removing the hooks does to the settings file.
#[derive(Debug, PartialEq, Eq)]
enum Settings {
    /// There is no file, or no registration of Yalper's in it.
    Unchanged,
    /// The new content of the file.
    Updated(String),
    /// Nothing is left in the file: it is deleted.
    Emptied,
}

/// Reads the settings file at `path` (if it exists) and removes Yalper's hooks from it, see
/// [`unregister_hooks`]. A leading byte order mark is kept.
fn unregistered_settings(path: &Path) -> Result<Settings, String> {
    let Some(text) = init::read_settings(path)? else {
        return Ok(Settings::Unchanged);
    };
    let (bom, text) = match text.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    match unregister_hooks(text) {
        Ok(Settings::Updated(text)) if bom => Ok(Settings::Updated(format!("{BOM}{text}"))),
        Ok(settings) => Ok(settings),
        Err(why) => Err(format!(
            "cannot remove the hooks from {}: {why}. Fix the file, or remove Yalper's hooks from it by hand. \
             Nothing was changed.",
            path.display()
        )),
    }
}

/// Removes every handler that `yalper init` would count as its registration (see [`is_registration`]), with
/// whatever binary path it has, from the settings JSON `text`.
///
/// A matcher group that held nothing but such handlers and no other key (the shape `yalper init` adds) is
/// removed with them, then an event array left empty by that, then a `hooks` object left empty. Every other
/// key, group and handler is kept as it is, in its order, empty or not. The result is formatted like
/// `yalper init` writes it.
fn unregister_hooks(text: &str) -> Result<Settings, String> {
    let mut settings: Value =
        serde_json::from_str(text).map_err(|error| format!("it is not valid JSON ({error})"))?;
    // Claude Code would not load a file of another shape, and `yalper init` never writes into one.
    let Some(top) = settings.as_object_mut() else {
        return Ok(Settings::Unchanged);
    };
    let Some(Value::Object(hooks)) = top.get_mut("hooks") else {
        return Ok(Settings::Unchanged);
    };

    let mut removed = false;
    hooks.retain(|_, groups| {
        let Value::Array(groups) = groups else {
            return true;
        };
        let mut dropped_group = false;
        groups.retain_mut(|group| {
            let Value::Object(group) = group else {
                return true;
            };
            let all = matches_all(group);
            let Some(Value::Array(handlers)) = group.get_mut("hooks") else {
                return true;
            };
            let count = handlers.len();
            handlers.retain(|handler| !is_registration(all, handler));
            if handlers.len() == count {
                return true;
            }
            removed = true;
            let drop = handlers.is_empty() && group.len() == 1;
            dropped_group |= drop;
            !drop
        });
        !(dropped_group && groups.is_empty())
    });
    if !removed {
        return Ok(Settings::Unchanged);
    }
    if hooks.is_empty() {
        top.retain(|key, _| key != "hooks");
        if top.is_empty() {
            return Ok(Settings::Emptied);
        }
    }
    let mut text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(Settings::Updated(text))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const EXE: &str = "/opt/yalper/bin/yalper";

    fn yalper(timeout: bool) -> Value {
        let mut handler = json!({"type": "command", "command": EXE, "args": ["hook"]});
        if timeout {
            handler["timeout"] = json!(30);
        }
        handler
    }

    fn unregistered(settings: &Value) -> Settings {
        unregister_hooks(&settings.to_string()).unwrap()
    }

    fn updated(settings: &Value) -> Value {
        match unregistered(settings) {
            Settings::Updated(text) => {
                assert!(text.ends_with("}\n"), "{text}");
                serde_json::from_str(&text).unwrap()
            }
            other => panic!("expected a change, got {other:?}"),
        }
    }

    #[test]
    fn a_file_with_only_yalper_hooks_is_emptied() {
        let settings = json!({"hooks": {
            "SessionStart": [{"hooks": [yalper(true)]}],
            "SessionEnd": [{"hooks": [yalper(false)]}]
        }});
        assert_eq!(unregistered(&settings), Settings::Emptied);
    }

    #[test]
    fn other_keys_groups_and_handlers_are_kept_in_order() {
        let fmt = json!({"type": "command", "command": "cargo fmt"});
        let settings = json!({
            "permissions": {"allow": ["Bash(cargo test)"]},
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard"}]}],
                "PostToolUse": [
                    {"matcher": "Edit", "hooks": [fmt.clone()]},
                    {"hooks": [yalper(true)]}
                ],
                "Stop": [{"hooks": [fmt.clone(), yalper(true)]}],
                "SessionEnd": [{"hooks": [yalper(false)]}]
            },
            "model": "x"
        });
        let result = updated(&settings);
        assert_eq!(
            result,
            json!({
                "permissions": {"allow": ["Bash(cargo test)"]},
                "hooks": {
                    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard"}]}],
                    "PostToolUse": [{"matcher": "Edit", "hooks": [fmt.clone()]}],
                    "Stop": [{"hooks": [fmt]}]
                },
                "model": "x"
            })
        );
        let keys: Vec<&String> = result.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["permissions", "hooks", "model"]);
        let events: Vec<&String> = result["hooks"].as_object().unwrap().keys().collect();
        assert_eq!(events, ["PreToolUse", "PostToolUse", "Stop"]);
    }

    #[test]
    fn containers_yalper_did_not_shape_are_kept_even_when_empty() {
        // A group with a matcher of its own, and an event array that was already empty.
        let settings = json!({"hooks": {
            "Stop": [{"matcher": "*", "hooks": [yalper(true)]}],
            "Notification": []
        }});
        assert_eq!(
            updated(&settings),
            json!({"hooks": {"Stop": [{"matcher": "*", "hooks": []}], "Notification": []}})
        );
        // An empty `hooks` object the user had stays when nothing was removed.
        assert_eq!(unregistered(&json!({"hooks": {}})), Settings::Unchanged);
    }

    #[test]
    fn yalper_handlers_init_does_not_count_as_its_registration_are_kept() {
        let decoys = [
            json!({"matcher": "Bash", "hooks": [yalper(true)]}),
            json!({"hooks": [{"type": "command", "command": EXE, "args": ["hook"], "if": "Bash(x)"}]}),
            json!({"hooks": [{"type": "command", "command": EXE, "args": ["hook"], "async": true}]}),
            json!({"hooks": [{"type": "command", "command": "yalper hook"}]}),
        ];
        for decoy in decoys {
            let settings = json!({"hooks": {"Stop": [decoy]}});
            assert_eq!(unregistered(&settings), Settings::Unchanged, "{settings}");
        }
    }

    #[test]
    fn a_registration_with_another_binary_path_is_removed() {
        let settings = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "C:\\old\\Yalper.exe", "args": ["hook"], "async": false}
        ]}]}});
        assert_eq!(unregistered(&settings), Settings::Emptied);
    }

    #[test]
    fn unexpected_shapes_are_left_alone_and_invalid_json_is_refused() {
        for text in [
            "[]",
            r#"{"hooks": []}"#,
            r#"{"hooks": {"Stop": {}}}"#,
            r#"{"hooks": {"Stop": ["x", {"hooks": {}}]}}"#,
        ] {
            assert_eq!(
                unregister_hooks(text).unwrap(),
                Settings::Unchanged,
                "{text}"
            );
        }
        assert!(unregister_hooks("{").is_err());
        assert!(unregister_hooks("").is_err());
    }

    #[test]
    fn a_byte_order_mark_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        let settings = json!({"model": "x", "hooks": {"Stop": [{"hooks": [yalper(true)]}]}});
        fs::write(&path, format!("{BOM}{settings}")).unwrap();
        let Settings::Updated(text) = unregistered_settings(&path).unwrap() else {
            panic!("expected a change");
        };
        let rest = text.strip_prefix(BOM).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(rest).unwrap(),
            json!({"model": "x"})
        );
    }

    #[test]
    fn only_exact_lines_are_removed() {
        let text = b"# mine\n/.yalper\n.yalper/\r\ntarget\n.claude/settings.local.json\n.yalper/";
        let (kept, found) = without_lines(text, &EXCLUDE_LINES);
        assert_eq!(kept, b"# mine\n/.yalper\ntarget\n");
        assert_eq!(found, EXCLUDE_LINES);

        let (kept, found) = without_lines(text, &[".claude/settings.local.json"]);
        assert_eq!(
            kept,
            b"# mine\n/.yalper\n.yalper/\r\ntarget\n.yalper/".to_vec()
        );
        assert_eq!(found, [".claude/settings.local.json"]);

        let (kept, found) = without_lines(b"", &EXCLUDE_LINES);
        assert!(kept.is_empty() && found.is_empty());
    }
}
