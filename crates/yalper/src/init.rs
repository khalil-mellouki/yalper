//! `yalper init`: sets up recording for the git repository around the current directory.
//!
//! It creates `.yalper/` at the repository root with a new init token (see [`crate::repo`]), the event log
//! and the snapshot store, takes the baseline snapshot, excludes `.yalper/` and Claude Code's personal
//! settings file from git, and registers `yalper hook` for Claude Code in that settings file. Running it again
//! changes nothing, and nothing the user already had in the settings file is removed.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::hook::{HookEvent, YALPER_DIR};
use crate::repo::{self, GIT_ID_FILE, ID_FILE, Refusal, Token, YalperDir};
use crate::safe_fs::{self, Access, OwnedDir};
use crate::snapshot::{self, SNAPSHOTS_DIR, ShadowStore};
use crate::store::{self, DATABASE_FILE, LOCK_TIMEOUT, Store, WriterLock};

/// Claude Code's settings folder at the repository root.
pub const CLAUDE_DIR: &str = ".claude";

/// Claude Code's personal (never committed) project settings file, inside [`CLAUDE_DIR`].
pub const SETTINGS_FILE: &str = "settings.local.json";

/// Claude Code's shared (committed) project settings file, inside [`CLAUDE_DIR`]. Yalper never writes it.
pub const SHARED_SETTINGS_FILE: &str = "settings.json";

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

/// Settings, besides hooks, whose value is a command Claude Code runs.
const COMMAND_SETTINGS: [&str; 4] = [
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "otelHeadersHelper",
];

/// A settings file larger than this is not read.
const MAX_SETTINGS_BYTES: u64 = 16 * 1024 * 1024;

/// A command listed in init's output is cut after this many characters.
const MAX_LISTED_COMMAND_CHARS: usize = 120;

pub(crate) const BOM: char = '\u{feff}';

/// Sets up Yalper in the git repository around `start`, registering `exe` as the hook command, and reports
/// each step to `out`. A `.yalper/` that Yalper cannot use because it was not created by `yalper init` for
/// this repository, or holds contents created for another one, is refused unless `recreate` is set: then it
/// is deleted and created again.
///
/// Nothing is written when the repository root, its git directory, the settings file, or an existing
/// `.yalper/` is unusable. Returns the message to show when setup fails.
pub fn init(start: &Path, exe: &Path, recreate: bool, out: &mut dyn Write) -> Result<(), String> {
    let Repository {
        root,
        git_dir,
        common_dir,
    } = Repository::find(
        start,
        "Run `yalper init` inside a git project (or run `git init` first).",
    )?;
    let root = root.as_path();
    let exe_text = exe.to_str().ok_or_else(|| {
        format!(
            "the path of the yalper binary is not valid UTF-8: {}",
            exe.display()
        )
    })?;

    // Everything that can stop init is checked before anything is written.
    let settings_path = root.join(CLAUDE_DIR).join(SETTINGS_FILE);
    let settings = registered_settings(&settings_path, exe_text)?;
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
        Existing::Missing { empty_dir } => {
            let yalper = create_yalper_dir(&yalper_path, &git_dir, empty_dir)?;
            say(out, "  .yalper/: created");
            yalper
        }
        Existing::Unusable(_) => {
            remove(&yalper_path)
                .map_err(|error| format!("cannot delete {}: {error}", yalper_path.display()))?;
            let yalper = create_yalper_dir(&yalper_path, &git_dir, false)?;
            say(out, "  .yalper/: deleted and created again");
            yalper
        }
    };
    if create_contents(&yalper)? {
        forget_lost_snapshots(&yalper, out)?;
    }
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
            let existing = fs::symlink_metadata(&settings_path)
                .ok()
                .filter(fs::Metadata::is_file);
            replace_file(&settings_path, text.as_bytes(), existing.as_ref())
                .map_err(|error| format!("cannot write {}: {error}", settings_path.display()))?;
            say(
                out,
                "  Claude Code hooks: registered in .claude/settings.local.json",
            );
        }
    }
    list_other_commands(root, exe_text, out);

    let mut warned = warn_if_the_hook_cannot_use(root, out);
    for warning in exe_warnings(exe) {
        say(out, &format!("warning: {warning}"));
        warned = true;
    }
    if warned {
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

/// The git repository around a directory, as `yalper init` and `yalper uninstall` use it.
pub(crate) struct Repository {
    /// The root of the working tree (see [`repo::find_root`]).
    pub root: PathBuf,
    /// Its git directory, where the init token is (for a linked worktree, its own one).
    pub git_dir: PathBuf,
    /// The directory holding `info/exclude` (for a linked worktree, the common one).
    pub common_dir: PathBuf,
}

impl Repository {
    /// The repository around `start`, or the message to show: outside a git repository it ends with `hint`.
    pub fn find(start: &Path, hint: &str) -> Result<Self, String> {
        let root = repo::find_root(start)
            .ok_or_else(|| format!("{} is not inside a git repository. {hint}", start.display()))?;
        let no_git_dir = || {
            format!(
                "cannot find the git directory of {}: its .git does not lead to a git directory",
                root.display()
            )
        };
        let git_dir = repo::git_dir(root)
            .filter(|dir| is_git_dir(dir))
            .ok_or_else(no_git_dir)?;
        let is_git_file = fs::symlink_metadata(root.join(".git")).is_ok_and(|m| m.is_file());
        if is_git_file && !repo::git_dir_links_back(root, &git_dir) {
            return Err(format!(
                "the .git file of {} points to {}, a git directory that does not name this folder back (it \
                 is not this folder's worktree or submodule git directory), so Yalper will not use it. \
                 Nothing was changed.",
                root.display(),
                git_dir.display()
            ));
        }
        let common_dir = repo::git_common_dir(root)
            .filter(|dir| is_git_dir(dir))
            .ok_or_else(no_git_dir)?;
        Ok(Self {
            root: root.to_owned(),
            git_dir,
            common_dir,
        })
    }

    /// The exclude file git reads for this working tree.
    pub fn exclude_file(&self) -> PathBuf {
        self.common_dir.join("info").join("exclude")
    }
}

/// Whether `dir` is a real directory (not a link) holding a `HEAD` file, as every git directory does.
fn is_git_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.is_dir())
        && fs::symlink_metadata(dir.join("HEAD")).is_ok_and(|metadata| metadata.is_file())
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

/// Whether `handler`, in a group whose matcher matches every call (`group_matches_all`), is a Yalper handler
/// that runs on every event the way `yalper init` registers it: a synchronous command with no `if`
/// condition. Any other Yalper handler (for example one committed with a matcher that never matches) does
/// not count, so it cannot stop init from registering the real one.
pub(crate) fn is_registration(group_matches_all: bool, handler: &Value) -> bool {
    group_matches_all
        && is_yalper_handler(handler)
        && handler.get("type").and_then(Value::as_str) == Some("command")
        && handler.get("if").is_none()
        && handler
            .get("async")
            .is_none_or(|value| *value == Value::Bool(false))
}

/// Whether the matcher group `group` runs its handlers for every call: no matcher, an empty one, or `*`.
pub(crate) fn matches_all(group: &Map<String, Value>) -> bool {
    match group.get("matcher") {
        None => true,
        Some(Value::String(matcher)) => matcher.is_empty() || matcher == "*",
        Some(_) => false,
    }
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
/// A leading byte order mark (Windows PowerShell 5.1 writes one) is kept.
fn registered_settings(path: &Path, exe: &str) -> Result<Settings, String> {
    let existing = read_settings(path)?;
    let (bom, text) = match existing.as_deref().map(|text| text.strip_prefix(BOM)) {
        Some(Some(rest)) => (true, Some(rest)),
        _ => (false, existing.as_deref()),
    };
    match register_hooks(text, exe) {
        Ok(Settings::Updated(text)) if bom => Ok(Settings::Updated(format!("{BOM}{text}"))),
        Ok(settings) => Ok(settings),
        Err(why) => Err(format!(
            "cannot register the hooks in {}: {why}. Nothing was changed.",
            path.display()
        )),
    }
}

/// The text of the settings file at `path`, or `None` if there is none. A link at the file or at its folder,
/// or a file that is not UTF-8 text, is an error: Yalper never edits it.
pub(crate) fn read_settings(path: &Path) -> Result<Option<String>, String> {
    let claude_dir = path.parent().unwrap_or(path);
    if fs::symlink_metadata(claude_dir).is_ok_and(|metadata| !metadata.is_dir()) {
        return Err(format!(
            "{} is not a folder (it may be a link), so Yalper will not edit the settings there. \
             Nothing was changed.",
            claude_dir.display()
        ));
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        Ok(metadata) if !metadata.is_file() => Err(format!(
            "{} is not a regular file (it may be a link), so Yalper will not edit it. Nothing was \
             changed.",
            path.display()
        )),
        Ok(_) => read_text(path).map(Some).ok_or_else(|| {
            format!(
                "cannot read {} as UTF-8 text. Nothing was changed.",
                path.display()
            )
        }),
    }
}

/// The content of the regular file at `path`, if it is UTF-8 and not too large.
fn read_text(path: &Path) -> Option<String> {
    String::from_utf8(safe_fs::read_small_regular_file(path, MAX_SETTINGS_BYTES)?).ok()
}

/// Registers `exe` for every event of [`EVENTS`] in the settings JSON `existing` (`None`: no file yet).
///
/// An event whose matcher groups already hold Yalper's registration (see [`is_registration`]) keeps it, with
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
            let Value::Object(group) = group else {
                return Err(format!(
                    "`hooks.{name}` holds an entry that is not a matcher group"
                ));
            };
            let all = matches_all(group);
            let handlers = match group.get_mut("hooks") {
                Some(Value::Array(handlers)) => handlers,
                None => continue,
                Some(_) => return Err(format!("`hooks.{name}` holds a group without a hook list")),
            };
            for handler in handlers.iter_mut() {
                if !handler.is_object() {
                    return Err(format!("`hooks.{name}` holds a hook that is not an object"));
                }
                if is_registration(all, handler) {
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

/// Replaces the file at `path` with `bytes` through a temporary file next to it, renamed over `path` at the
/// end, so the file is never seen half written. No link is ever followed: whatever is at the temporary path
/// (a leftover, or a link a repository planted) is removed first, never its target, and the temporary file
/// is created new. On Unix it is readable only by its owner, or gets the permissions of `existing` (the
/// replaced file). The temporary file is removed on any error.
pub(crate) fn replace_file(
    path: &Path,
    bytes: &[u8],
    existing: Option<&fs::Metadata>,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".yalper-tmp");
    let temporary = PathBuf::from(temporary);
    match fs::remove_file(&temporary) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        // A Windows junction or directory symlink is removed like a folder.
        Err(_) => fs::remove_dir(&temporary)?,
        Ok(()) => {}
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary)?;
    let written = fill(&mut file, bytes, existing);
    drop(file);
    written
        .and_then(|()| fs::rename(&temporary, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
}

/// Writes `bytes` to the new `file` and, on Unix, gives it the permissions of `existing`.
fn fill(file: &mut fs::File, bytes: &[u8], existing: Option<&fs::Metadata>) -> io::Result<()> {
    file.write_all(bytes)?;
    #[cfg(unix)]
    if let Some(existing) = existing {
        use std::os::unix::fs::PermissionsExt;
        let mode = existing.permissions().mode() & 0o777;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    let _ = existing;
    Ok(())
}

/// Appends each line of [`EXCLUDE_LINES`] that the exclude file at `path` does not already have (in any
/// equivalent form, such as `/.yalper/`), creating the file and its folder if needed. Returns the lines
/// added. A link at the file or its folder is refused.
fn add_excludes(path: &Path) -> io::Result<Vec<&'static str>> {
    check_exclude_path(path)?;
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

/// Refuses an exclude file at `path`, or its `info` folder, that is a link or not what git creates there.
/// Either may be missing.
pub(crate) fn check_exclude_path(path: &Path) -> io::Result<()> {
    let refuse_link =
        |path: &Path, is_expected: fn(&fs::Metadata) -> bool| match fs::symlink_metadata(path) {
            Ok(metadata) if !is_expected(&metadata) => Err(io::Error::other(format!(
                "{} is a link or not what git creates there",
                path.display()
            ))),
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    if let Some(info) = path.parent() {
        refuse_link(info, fs::Metadata::is_dir)?;
    }
    refuse_link(path, fs::Metadata::is_file)
}

/// What is at `.yalper/` before init changes anything.
enum Existing {
    /// Nothing, or an empty folder (git never creates one, so a clone cannot plant it).
    Missing { empty_dir: bool },
    /// `yalper init` created it for this repository, and the event log and snapshot store it has belong to
    /// it.
    Usable(YalperDir),
    /// It was not created by `yalper init` for this repository, or holds contents created for another
    /// `.yalper/`. The reason is given.
    Unusable(String),
}

/// Looks at `path`, the `.yalper/` of the repository at `root`, without changing anything. A folder the hook
/// would refuse for another reason than its origin (another owner, writable by others) or that cannot be read
/// is an error: deleting it would not help.
fn inspect(root: &Path, path: &Path) -> Result<Existing, String> {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Existing::Missing { empty_dir: false });
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        Ok(metadata) => metadata,
    };
    if !metadata.is_dir() {
        return Ok(Existing::Unusable(
            "it is not a folder (it is a file or a link)".to_owned(),
        ));
    }
    let empty = fs::read_dir(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .next()
        .is_none();
    if empty {
        return Ok(Existing::Missing { empty_dir: true });
    }
    let yalper = match repo::open_yalper_dir(root, path) {
        Ok(yalper) => yalper,
        Err(refusal @ Refusal::TokenMismatch) => {
            return Ok(Existing::Unusable(refusal.to_string()));
        }
        Err(refusal) => {
            let hint = match refusal {
                Refusal::WritableByOthers => " Run `chmod 700 .yalper` to make it private.",
                _ => "",
            };
            return Err(format!(
                "Yalper cannot use {}: {refusal}.{hint}",
                path.display()
            ));
        }
    };
    Ok(match check_contents(&yalper)? {
        Some(why) => Existing::Unusable(why),
        None => Existing::Usable(yalper),
    })
}

/// Opens the event log and the snapshot store of `yalper` that exist, and returns why they cannot be used
/// if they were created for another `.yalper/`. Other failures are errors.
fn check_contents(yalper: &YalperDir) -> Result<Option<String>, String> {
    let exists = |name: &str| fs::symlink_metadata(yalper.dir.path().join(name));
    if let Ok(metadata) = exists(DATABASE_FILE) {
        if !metadata.is_file() {
            return Ok(Some(format!("its {DATABASE_FILE} is not a regular file")));
        }
        match Store::open(&yalper.dir, &yalper.token) {
            Ok(_) => {}
            Err(error @ (store::Error::ForeignDatabase | store::Error::UnexpectedSchema)) => {
                return Ok(Some(error.to_string()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    if let Ok(metadata) = exists(SNAPSHOTS_DIR) {
        if !metadata.is_dir() {
            return Ok(Some(format!("its {SNAPSHOTS_DIR} is not a folder")));
        }
        match ShadowStore::open(&yalper.dir, &yalper.token) {
            Ok(_) => {}
            Err(error @ snapshot::Error::UnexpectedLayout(_)) => {
                return Ok(Some(error.to_string()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(None)
}

/// Creates `.yalper/` at `path` (on Unix accessible only by its owner), or uses the empty folder there, and
/// writes a new init token to it and to `git_dir`.
fn create_yalper_dir(path: &Path, git_dir: &Path, empty_dir: bool) -> Result<YalperDir, String> {
    let failed = |error: io::Error| format!("cannot create {}: {error}", path.display());
    if !empty_dir {
        #[cfg_attr(windows, allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(path).map_err(failed)?;
    }
    #[cfg(unix)]
    if empty_dir {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(failed)?;
    }
    let dir = OwnedDir::open(path).map_err(failed)?;
    let token = Token::generate().map_err(failed)?;
    let line = format!("{token}\n");
    let git_token = git_dir.join(GIT_ID_FILE);
    replace_file(&git_token, line.as_bytes(), None)
        .map_err(|error| format!("cannot write {}: {error}", git_token.display()))?;
    dir.open_file(ID_FILE, Access::ReadWrite)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .map_err(failed)?;
    Ok(YalperDir { dir, token })
}

/// Creates the event log and the snapshot store of `yalper` if they are missing. Returns whether the
/// snapshot store had to be created.
fn create_contents(yalper: &YalperDir) -> Result<bool, String> {
    Store::open(&yalper.dir, &yalper.token).map_err(|error| error.to_string())?;
    match fs::symlink_metadata(yalper.dir.path().join(SNAPSHOTS_DIR)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ShadowStore::init(&yalper.dir, &yalper.token).map_err(|error| error.to_string())?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// After the snapshot store was created again, the event log may still name a latest snapshot whose trees
/// and blobs are gone, and every later snapshot would fail on it. It is forgotten, so the baseline starts
/// over.
fn forget_lost_snapshots(yalper: &YalperDir, out: &mut dyn Write) -> Result<(), String> {
    let store = Store::open(&yalper.dir, &yalper.token).map_err(|error| error.to_string())?;
    if !store.has_snapshot().map_err(|error| error.to_string())? {
        return Ok(());
    }
    let lock = WriterLock::acquire(&yalper.dir, LOCK_TIMEOUT).map_err(|error| error.to_string())?;
    store
        .forget_snapshot(&lock)
        .map_err(|error| error.to_string())?;
    say(
        out,
        "  Snapshot store: missing, created again (earlier snapshots are no longer available)",
    );
    Ok(())
}

/// Deletes whatever is at `path`: a folder with everything in it, or a file or link (never its target).
pub(crate) fn remove(path: &Path) -> io::Result<()> {
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
                // The skipped paths are names from the repository.
                say(out, &printable(&format!("  warning: {problems}")));
            }
        }
        Err(error) => say(
            out,
            &printable(&format!(
                "failed ({error}). Recording still works: each step tries to take a snapshot again."
            )),
        ),
    }
    Ok(())
}

/// Lists the commands, other than Yalper's own hook, that Claude Code's project settings files at `root` make
/// it run: command hooks and command settings such as `statusLine` or `apiKeyHelper`. A cloned repository can
/// commit them, and they run as soon as the folder is trusted, like Yalper's hook.
fn list_other_commands(root: &Path, exe: &str, out: &mut dyn Write) {
    let mut lines = Vec::new();
    for name in [SHARED_SETTINGS_FILE, SETTINGS_FILE] {
        let path = root.join(CLAUDE_DIR).join(name);
        let Some(settings) = read_text(&path).and_then(|text| {
            serde_json::from_str::<Value>(text.strip_prefix(BOM).unwrap_or(&text)).ok()
        }) else {
            continue;
        };
        for (setting, command) in other_commands(&settings, exe) {
            // The setting names come from the repository too (event keys), so the whole line is filtered.
            lines.push(printable(&format!(
                "    .claude/{name} {setting}: {command}"
            )));
        }
    }
    if !lines.is_empty() {
        say(
            out,
            "  Other commands in Claude Code's settings (they also run once this folder is trusted):",
        );
        for line in lines {
            say(out, &line);
        }
    }
}

/// The commands `settings` makes Claude Code run, except Yalper's registration with `exe`, as (setting,
/// command line) pairs.
fn other_commands(settings: &Value, exe: &str) -> Vec<(String, String)> {
    let mut commands = Vec::new();
    let hooks = settings.get("hooks").and_then(Value::as_object);
    for (event, groups) in hooks.into_iter().flatten() {
        for group in groups.as_array().into_iter().flatten() {
            let Some(group) = group.as_object() else {
                continue;
            };
            let handlers = group.get("hooks").and_then(Value::as_array);
            for handler in handlers.into_iter().flatten() {
                let is_command = handler.get("type").and_then(Value::as_str) == Some("command");
                let is_ours =
                    is_registration(matches_all(group), handler) && handler["command"] == exe;
                if let Some(command) = handler.get("command").and_then(Value::as_str)
                    && is_command
                    && !is_ours
                {
                    let args = handler.get("args").and_then(Value::as_array);
                    let line = args
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .fold(command.to_owned(), |line, arg| line + " " + arg);
                    commands.push((format!("hooks.{event}"), line));
                }
            }
        }
    }
    for setting in COMMAND_SETTINGS {
        if let Some(command) = settings.get(setting).and_then(Value::as_str) {
            commands.push((setting.to_owned(), command.to_owned()));
        }
    }
    let status_line = settings
        .get("statusLine")
        .and_then(|status| status.get("command"));
    if let Some(command) = status_line.and_then(Value::as_str) {
        commands.push(("statusLine".to_owned(), command.to_owned()));
    }
    for (_, command) in &mut commands {
        *command = shortened(command);
    }
    commands
}

/// `text` with every control character (line breaks, terminal escape sequences) replaced by a space, and every
/// invisible formatting character (see [`is_hidden_format`]) shown as `<U+XXXX>`, so text from a repository or
/// a recording cannot change what the terminal shows or hide what it says.
pub fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            shown.push(' ');
        } else if is_hidden_format(c) {
            shown.push_str(&format!("<U+{:04X}>", u32::from(c)));
        } else {
            shown.push(c);
        }
    }
    shown
}

/// Characters that reorder the text around them, break lines outside ASCII, or show nothing: every format
/// character (Unicode General_Category Cf: bidirectional marks and overrides, zero-width spaces, word
/// joiners, byte order mark, tag characters, invisible operators), plus the line and paragraph separators,
/// variation selectors, and the invisible fillers that can pass for letters in identifiers. The zero-width
/// joiner U+200D is kept: emoji sequences need it and it hides nothing.
fn is_hidden_format(c: char) -> bool {
    matches!(
        c,
        // General_Category Cf (Unicode 16), without U+200D.
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200C}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            // Tag characters (Cf, and the unassigned ones between them).
            | '\u{E0000}'..='\u{E007F}'
            // Line and paragraph separators.
            | '\u{2028}'..='\u{2029}'
            // Invisible fillers and joiners that are not Cf.
            | '\u{034F}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{3164}'
            | '\u{FFA0}'
            // Variation selectors, except the text and emoji presentation selectors U+FE0E and U+FE0F, which
            // follow emoji in ordinary text and hide nothing.
            | '\u{FE00}'..='\u{FE0D}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

/// `command` on one line, cut after [`MAX_LISTED_COMMAND_CHARS`] characters.
fn shortened(command: &str) -> String {
    let shown = printable(command);
    let mut line: String = shown.chars().take(MAX_LISTED_COMMAND_CHARS).collect();
    if shown.chars().count() > MAX_LISTED_COMMAND_CHARS {
        line.push_str("...");
    }
    line
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

/// Reasons not to register `exe` as it is: Claude Code runs it on every step, so anyone who can replace it
/// runs code as the user. Warns when it is in the temporary folder, and on Unix when it or one of its parent
/// folders can be changed by other users (group or world writable, without the sticky bit).
fn exe_warnings(exe: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_owned());
    if fs::canonicalize(env::temp_dir()).is_ok_and(|temp| exe.starts_with(temp)) {
        warnings.push(format!(
            "the yalper binary is in the temporary folder ({}), which other programs can change or clean \
             up. Install yalper somewhere permanent and run `yalper init` again.",
            exe.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let open = exe.ancestors().find(|path| {
            fs::metadata(path).is_ok_and(|metadata| {
                let mode = metadata.permissions().mode();
                mode & 0o022 != 0 && mode & 0o1000 == 0
            })
        });
        if let Some(open) = open {
            warnings.push(format!(
                "{} can be changed by other users, and Claude Code runs the yalper binary inside it on every \
                 step. Install yalper in a folder only you can write to and run `yalper init` again.",
                open.display()
            ));
        }
    }
    warnings
}

/// Prints one line of the report. A closed output does not stop init.
pub(crate) fn say(out: &mut dyn Write, line: &str) {
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
    fn a_yalper_handler_that_does_not_run_on_every_call_is_not_the_registration() {
        let decoys = [
            json!({"matcher": "NeverMatches", "hooks": [{"type": "command", "command": EXE, "args": ["hook"]}]}),
            json!({"hooks": [{"type": "command", "command": EXE, "args": ["hook"], "if": "Bash(x)"}]}),
            json!({"hooks": [{"type": "command", "command": EXE, "args": ["hook"], "async": true}]}),
            json!({"hooks": [{"type": "http", "command": EXE, "args": ["hook"]}]}),
        ];
        for decoy in decoys {
            let existing = json!({"hooks": {"Stop": [decoy.clone()]}}).to_string();
            let settings = updated(Some(&existing), EXE);
            let groups = settings["hooks"]["Stop"].as_array().unwrap();
            assert_eq!(groups.len(), 2, "{decoy}");
            assert_eq!(groups[0], decoy);
            assert_eq!(groups[1], json!({"hooks": [handler(HookEvent::Stop, EXE)]}));
        }
        // The forms `yalper init` accepts as its own.
        for matcher in [json!("*"), json!("")] {
            let existing = json!({"hooks": {"Stop": [{"matcher": matcher, "hooks": [
                {"type": "command", "command": EXE, "args": ["hook"], "async": false}
            ]}]}})
            .to_string();
            let settings = updated(Some(&existing), EXE);
            assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
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
    fn a_byte_order_mark_is_accepted_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        fs::write(&path, format!("{BOM}{{\"model\": \"x\"}}")).unwrap();
        let Settings::Updated(text) = registered_settings(&path, EXE).unwrap() else {
            panic!("expected a change");
        };
        let rest = text.strip_prefix(BOM).unwrap();
        assert_eq!(serde_json::from_str::<Value>(rest).unwrap()["model"], "x");
        fs::write(&path, &text).unwrap();
        assert_eq!(
            registered_settings(&path, EXE).unwrap(),
            Settings::Unchanged
        );
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
    fn printable_shows_hidden_formatting_characters() {
        assert_eq!(printable("a\u{1b}[31m\r\tb"), "a [31m  b");
        // A right-to-left override makes `rm -rf ~ # txt.exe` look like something else.
        assert_eq!(printable("run \u{202E}exe.txt"), "run <U+202E>exe.txt");
        for c in [
            '\u{061C}',
            '\u{200B}',
            '\u{200E}',
            '\u{200F}',
            '\u{2028}',
            '\u{2029}',
            '\u{202A}',
            '\u{2066}',
            '\u{2069}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
            // Other format characters, invisible fillers and variation selectors.
            '\u{00AD}',
            '\u{034F}',
            '\u{0600}',
            '\u{115F}',
            '\u{1160}',
            '\u{17B4}',
            '\u{180E}',
            '\u{200C}',
            '\u{2064}',
            '\u{206F}',
            '\u{3164}',
            '\u{FE00}',
            '\u{FE0D}',
            '\u{FFA0}',
            '\u{FFFB}',
            '\u{1D173}',
            '\u{E0100}',
            '\u{E01EF}',
        ] {
            assert_eq!(
                printable(&c.to_string()),
                format!("<U+{:04X}>", u32::from(c))
            );
        }
        // Kept: the emoji joiner, the emoji and text presentation selectors, accents, other scripts.
        let kept = "\u{1F469}\u{200D}\u{1F4BB} \u{2764}\u{FE0F} \u{2764}\u{FE0E} é 日本 שלום";
        assert_eq!(printable(kept), kept);
    }

    #[test]
    fn other_commands_are_every_command_but_the_registration() {
        let settings = json!({
            "hooks": {
                "Stop": [
                    {"hooks": [handler(HookEvent::Stop, EXE)]},
                    {"matcher": "NeverMatches", "hooks": [{"type": "command", "command": "/tmp/yalper", "args": ["hook"]}]},
                    {"hooks": [{"type": "command", "command": "notify\nme"}, {"type": "prompt", "prompt": "x"}]}
                ]
            },
            "statusLine": {"type": "command", "command": "status.sh"},
            "apiKeyHelper": "get-key.sh",
            "model": "x"
        });
        let commands = other_commands(&settings, EXE);
        let expected = [
            ("hooks.Stop", "/tmp/yalper hook"),
            ("hooks.Stop", "notify me"),
            ("apiKeyHelper", "get-key.sh"),
            ("statusLine", "status.sh"),
        ]
        .map(|(setting, command)| (setting.to_owned(), command.to_owned()));
        assert_eq!(commands, expected);
        assert_eq!(other_commands(&json!({}), EXE), []);
        let long = "x".repeat(MAX_LISTED_COMMAND_CHARS + 5);
        assert_eq!(
            shortened(&long),
            "x".repeat(MAX_LISTED_COMMAND_CHARS) + "..."
        );
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

    #[cfg(unix)]
    #[test]
    fn a_linked_exclude_file_or_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        fs::write(&target, "keep me").unwrap();
        fs::create_dir(dir.path().join("info")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("info").join("exclude")).unwrap();
        assert!(add_excludes(&dir.path().join("info").join("exclude")).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");

        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join("info")).unwrap();
        assert!(add_excludes(&linked.path().join("info").join("exclude")).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_file_is_replaced_and_a_leftover_temporary_file_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let temporary = dir.path().join("file.yalper-tmp");
        fs::write(&temporary, "leftover").unwrap();
        replace_file(&path, b"new", None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert!(!temporary.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            let existing = fs::metadata(&path).unwrap();
            replace_file(&path, b"newer", Some(&existing)).unwrap();
            assert_eq!(mode(&path), 0o640);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_temporary_path_is_removed_and_its_target_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("settings.json");
        fs::write(&target, "keep me").unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        std::os::unix::fs::symlink(&target, dir.path().join("settings.local.json.yalper-tmp"))
            .unwrap();
        replace_file(&path, b"{}\n", None).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
        assert!(!fs::symlink_metadata(&path).unwrap().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_claude_folder_or_settings_file_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join(SETTINGS_FILE);
        fs::write(&target, "{}").unwrap();

        let linked_folder = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked_folder.path().join(CLAUDE_DIR)).unwrap();
        let path = linked_folder.path().join(CLAUDE_DIR).join(SETTINGS_FILE);
        let error = registered_settings(&path, EXE).unwrap_err();
        assert!(error.contains("not a folder"), "{error}");

        let linked_file = tempfile::tempdir().unwrap();
        fs::create_dir(linked_file.path().join(CLAUDE_DIR)).unwrap();
        let path = linked_file.path().join(CLAUDE_DIR).join(SETTINGS_FILE);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let error = registered_settings(&path, EXE).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
    }

    #[test]
    fn a_binary_in_the_temporary_folder_is_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("yalper");
        fs::write(&exe, "").unwrap();
        let warnings = exe_warnings(&exe);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("temporary folder")),
            "{warnings:?}"
        );
        let elsewhere = Path::new("/usr/no-such-folder/yalper");
        assert_eq!(exe_warnings(elsewhere), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_others_can_replace_is_warned_about() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let exe = open.join("yalper");
        fs::write(&exe, "").unwrap();
        let warnings = exe_warnings(&exe);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("changed by other users")),
            "{warnings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_hook_check_warns_about_a_folder_others_can_write() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        let yalper = root.path().join(YALPER_DIR);
        fs::create_dir(&yalper).unwrap();
        let token = "0123456789abcdef0123456789abcdef\n";
        fs::write(root.path().join(".git").join(GIT_ID_FILE), token).unwrap();
        fs::write(yalper.join(ID_FILE), token).unwrap();
        fs::set_permissions(&yalper, fs::Permissions::from_mode(0o700)).unwrap();
        let mut out = Vec::new();
        assert!(!warn_if_the_hook_cannot_use(root.path(), &mut out));
        assert!(out.is_empty());

        // What a file system that ignores Unix permissions (WSL's /mnt/c) shows for every folder.
        fs::set_permissions(&yalper, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(warn_if_the_hook_cannot_use(root.path(), &mut out));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("warning: the hook will not record"), "{text}");
        assert!(text.contains("other users"), "{text}");
    }
}
