//! Env files and other ignored files of the project in the test copies: `--with-env` and `--with <path>`.
//!
//! Snapshots never hold the files the project's ignore rules ignore, so a step's code lacks the project's
//! `.env` files and its ignored local configuration (an ignored `.cargo/config.toml` or `.npmrc`, which runs
//! outside the project no longer find). Tests get them only when the user asks:
//!
//! - `--with-env`: every `.env` and `.env.*` file the project's rules ignore, in the project root and in every
//!   folder that holds files of the step;
//! - `--with <path>`: an ignored file or folder of the project, or a file over the snapshot size limit.
//!
//! The choice is remembered in the `meta` table of the event log ([`WITH_ENV_KEY`], [`WITH_KEY`]), which is
//! bound to the init token, so a pulled commit cannot make Yalper copy anything. The files are the project's
//! current ones, not those from the time of each step: Yalper never stores their content. They are read
//! without following links, regular files only.
//!
//! In a test copy (see [`LocalFiles::sync`]):
//!
//! - each file is an extra path of the copy (see [`TestCopy::set_extra_paths`]), registered next to the
//!   others', so switches keep it;
//! - before every run and every install, each one is compared byte for byte with the project's file and
//!   written again only if it differs, so a test that changes one does not change the next run. Whatever is at
//!   its path that is not the same regular file is removed (a link itself, never what it points to) and the
//!   file is created anew, 0600 on Unix: nothing is ever written through what a test left there;
//! - where the step's tree has a file or folder at its path, or a file above it, the tree's entry wins, and so
//!   does an extra path someone else registered (a folder of links to the project's `node_modules`).
//!
//! Every copied file is deleted when the bisect ends: when [`LocalFiles`] is dropped or
//! [`remove`](LocalFiles::remove)d (normal end, error, first Ctrl-C), and by [`remove_all_copied`] when a
//! second Ctrl-C makes Yalper exit at once. Yalper can also end without either (killed, crashed, or its
//! console closed on Windows), so before a file is first written into a copy, its copy and path (never its
//! content) are recorded in the event log ([`COPIED_KEY`]); the next bisect deletes what is listed there
//! before it uses any copy ([`remove_left_over`]). Otherwise a file left in a copy would apply to later
//! bisects, even with `--no-env`: switches keep the files the step's rules ignore.
//!
//! They are never written to the snapshot store or the event log: the copies are in the user's cache folder,
//! outside the project, where recording never looks, and bisect records no events. Known limits: a copy that a
//! test makes of one of them, in an ignored folder of the test copy, stays there; run logs may show values a
//! test prints (see [`Logs`](super::Logs)).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use gix::ObjectId;
use ignore::{WalkBuilder, WalkState};
use serde_json::{Value, json};

use super::copy::{native, parents, remove_entry};
use super::link::{is_real_dir_no_follow, is_under, normalize, plain};
use super::{CacheDir, TestCopy};
use crate::hook::YALPER_DIR;
use crate::safe_fs::open_regular_file;
use crate::snapshot::{
    Excludes, FileKind, MAX_DEPTH, MAX_FILE_BYTES, WALK_THREADS, is_racy, nanos_since_epoch,
    validate_path,
};
use crate::store::Store;

/// The `meta` key of the event log that remembers `--with-env`: `true`, or no row.
pub const WITH_ENV_KEY: &str = "bisect.with_env";

/// The `meta` key of the event log that remembers the `--with` paths: a JSON array of paths relative to the
/// project root, `/`-separated, or no row.
pub const WITH_KEY: &str = "bisect.with";

/// The `meta` key of the event log that lists the files copied into test copies and maybe not deleted yet:
/// `{"instance": <the cache folder's instance id>, "files": [{"copy": "copy-0", "path": ".env"}, ...]}`, or no
/// row. Names only, never content.
pub const COPIED_KEY: &str = "bisect.copied";

/// The largest env file copied. Env files hold a few settings; a larger file named like one is something else.
const MAX_ENV_FILE_BYTES: u64 = 1 << 20;

/// The most bytes the files of all `--with` paths may hold together, as a guard against copying a whole data
/// folder into every test copy by mistake.
const MAX_WITH_BYTES: u64 = 1 << 30;

/// The most files the `--with` paths may hold together.
const MAX_WITH_FILES: usize = 10_000;

/// How many files over the snapshot size limit the warning names.
const LARGE_FILES_NAMED: usize = 5;

/// The size of the pieces in which two files are compared.
const COMPARE_CHUNK: usize = 64 * 1024;

const MIB: u64 = 1 << 20;

/// Every file copied into a test copy and not deleted yet, as the canonical path of the copy and the file's
/// path in it, for [`remove_all_copied`]. Held while a file is listed and written, so that
/// [`remove_all_copied`] waits for a write going on and then deletes its file.
static COPIED: Mutex<BTreeSet<(PathBuf, String)>> = Mutex::new(BTreeSet::new());

/// What the user asked for on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choice {
    /// `--with-env` (`Some(true)`) or `--no-env` (`Some(false)`); `None` keeps the remembered choice.
    pub env: Option<bool>,
    /// `--with`: paths as given, relative to the current folder.
    pub with: Vec<PathBuf>,
    /// `--no-with`: forget the remembered `--with` paths. Paths given with `--with` are still added.
    pub no_with: bool,
}

/// What is copied into the test copies of a bisect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chosen {
    /// Copy the env files.
    pub env: bool,
    /// The `--with` paths copied, relative to the project root and `/`-separated, in the order they were given.
    pub with: Vec<String>,
    /// The files of the `--with` paths (the files inside a folder), sorted.
    pub with_files: Vec<String>,
    /// Warnings to show: remembered `--with` paths that no longer exist.
    pub warnings: Vec<String>,
}

/// Applies `choice`, given in the folder `start`, to the choice remembered in `events` for the project at
/// `project`, and remembers the result. Every `--with` path must be an ignored file or folder inside the
/// project, reached and made of real folders and regular files only, or a file over the snapshot size limit
/// (see [`check_with`]). Returns the message to show when one is refused: nothing is remembered then. A
/// remembered path that no longer exists is skipped with a warning, and stays remembered.
pub fn choose(
    events: &Store,
    project: &Path,
    start: &Path,
    choice: &Choice,
) -> Result<Chosen, String> {
    let failed =
        |error: crate::store::Error| format!("cannot read the remembered settings: {error}");
    let env = match choice.env {
        Some(env) => env,
        None => events.meta(WITH_ENV_KEY).map_err(failed)?.as_deref() == Some("true"),
    };
    let remembered: Vec<String> = if choice.no_with {
        Vec::new()
    } else {
        events
            .meta(WITH_KEY)
            .map_err(failed)?
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default()
    };

    // `kept` is remembered; `with` is copied in this bisect.
    let (mut kept, mut with, mut warnings) = (Vec::new(), Vec::new(), Vec::new());
    for path in remembered {
        if kept.contains(&path) {
            continue;
        }
        let missing = fs::symlink_metadata(native(project, &path))
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
        if missing && !path.is_empty() {
            warnings.push(format!(
                "--with {path}, remembered for this project, does not exist in the project now, so it is not \
                 copied. `yalper bisect --no-with` forgets the remembered paths."
            ));
        } else {
            check_with(project, &path).map_err(|why| {
                format!(
                    "--with {path}, remembered for this project, {why}. `yalper bisect --no-with` forgets \
                     the remembered paths."
                )
            })?;
            with.push(path.clone());
        }
        kept.push(path);
    }
    for given in &choice.with {
        let refused = |why: String| format!("--with {}: {why}.", given.display());
        let path = in_project(project, start, given).map_err(refused)?;
        check_with(project, &path).map_err(refused)?;
        if !with.contains(&path) {
            with.push(path.clone());
        }
        if !kept.contains(&path) {
            kept.push(path);
        }
    }
    let with_files = files_of(project, &with)?;

    let saved =
        |error: crate::store::Error| format!("cannot remember the bisect settings: {error}");
    events
        .set_meta(WITH_ENV_KEY, env.then_some("true"))
        .map_err(saved)?;
    let value = (!kept.is_empty()).then(|| Value::from(kept).to_string());
    events.set_meta(WITH_KEY, value.as_deref()).map_err(saved)?;
    Ok(Chosen {
        env,
        with,
        with_files,
        warnings,
    })
}

/// The path `given`, relative to the folder `start`, as a path relative to the project root `project`,
/// `/`-separated. `..` is resolved from the text, never through a link.
fn in_project(project: &Path, start: &Path, given: &Path) -> Result<String, String> {
    let absolute = std::path::absolute(start.join(given))
        .map_err(|error| format!("cannot resolve it: {error}"))?;
    let absolute = normalize(&absolute);
    for root in [Some(project.to_owned()), fs::canonicalize(project).ok()]
        .into_iter()
        .flatten()
    {
        let root = normalize(&plain(&root));
        if is_under(&absolute, &root) {
            let names = absolute
                .components()
                .skip(root.components().count())
                .map(|name| name.as_os_str().to_str())
                .collect::<Option<Vec<&str>>>()
                .ok_or("its name is not valid UTF-8")?;
            return Ok(names.join("/"));
        }
    }
    Err("it is outside the project".to_owned())
}

/// Checks that `path` (relative to the project root `project`, `/`-separated) can be copied with `--with`: a
/// regular file or a real folder, reached through real folders, not in `.git` or `.yalper`, and ignored by the
/// project's rules, or a file larger than snapshots take. Returns why not.
fn check_with(project: &Path, path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("it is the project itself".to_owned());
    }
    if path
        .split('/')
        .any(|name| name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(YALPER_DIR))
    {
        return Err("Yalper does not copy .git or .yalper".to_owned());
    }
    validate_path(path, FileKind::Regular).map_err(|error| error.to_string())?;
    let missing = || "it does not exist in the project".to_owned();
    for parent in parents(path) {
        match fs::symlink_metadata(native(project, parent)) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(metadata) if metadata.is_symlink() => {
                return Err(format!(
                    "{parent} is a link, and Yalper never follows links in the project"
                ));
            }
            _ => return Err(missing()),
        }
    }
    let metadata = fs::symlink_metadata(native(project, path)).map_err(|_| missing())?;
    if metadata.is_symlink() {
        return Err("it is a link, and Yalper never follows links in the project".to_owned());
    }
    if !metadata.is_file() && !metadata.is_dir() {
        return Err("it is not a regular file or a folder".to_owned());
    }
    if is_ignored(project, path, metadata.is_dir())
        || (metadata.is_file() && metadata.len() > MAX_FILE_BYTES)
    {
        return Ok(());
    }
    Err(format!(
        "the project's ignore rules do not ignore it, so the snapshots already hold it. --with copies \
         ignored files and folders, and files over {} MiB, which snapshots skip",
        MAX_FILE_BYTES / MIB
    ))
}

/// Whether the project's ignore rules ignore `path` (relative, `/`-separated), or a folder above it. If the
/// rules go over the budget of a snapshot, nothing is taken as ignored.
fn is_ignored(project: &Path, path: &str, is_dir: bool) -> bool {
    let excludes = Excludes::new(project);
    excludes.add_dir(project, true);
    for dir in parents(path) {
        let full = native(project, dir);
        if !excludes.exceeded() && excludes.is_ignored(&full, true) {
            return true;
        }
        excludes.add_dir(&full, true);
    }
    !excludes.exceeded() && excludes.is_ignored(&native(project, path), is_dir)
}

/// The files of the `--with` paths `with` of the project at `project`: each file, and every file inside each
/// folder. A link inside a folder is refused, like the paths themselves.
fn files_of(project: &Path, with: &[String]) -> Result<Vec<String>, String> {
    let mut files = BTreeSet::new();
    let mut bytes = 0_u64;
    for path in with {
        let refused = |why: String| format!("--with {path}: {why}.");
        let mut todo = vec![(path.clone(), 0)];
        while let Some((current, depth)) = todo.pop() {
            let full = native(project, &current);
            let metadata = fs::symlink_metadata(&full)
                .map_err(|error| refused(format!("cannot read {current}: {error}")))?;
            if metadata.is_symlink() {
                return Err(refused(format!(
                    "{current} is a link, and Yalper never follows links in the project"
                )));
            }
            if metadata.is_file() {
                bytes += metadata.len();
                if files.insert(current) && files.len() > MAX_WITH_FILES {
                    return Err(refused(format!(
                        "the --with paths hold more than {MAX_WITH_FILES} files"
                    )));
                }
                if bytes > MAX_WITH_BYTES {
                    return Err(refused(format!(
                        "the --with paths hold more than {} MiB",
                        MAX_WITH_BYTES / MIB
                    )));
                }
                continue;
            }
            if !metadata.is_dir() || depth >= MAX_DEPTH {
                continue;
            }
            let entries = fs::read_dir(&full)
                .map_err(|error| refused(format!("cannot read {current}: {error}")))?;
            for entry in entries {
                let entry =
                    entry.map_err(|error| refused(format!("cannot read {current}: {error}")))?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| refused(format!("{current} holds a name that is not UTF-8")))?;
                todo.push((format!("{current}/{name}"), depth + 1));
            }
        }
    }
    Ok(files.into_iter().collect())
}

/// What a walk of the project found.
#[derive(Debug, Default)]
struct Found {
    /// Env files to copy, sorted.
    env: Vec<String>,
    /// Files the snapshots skip because they are too large, with their size, sorted.
    large: Vec<(String, u64)>,
    /// Warnings to show.
    notes: Vec<String>,
}

/// Whether `name` is an env file's: `.env` or `.env.<anything>`.
fn is_env_name(name: &OsStr) -> bool {
    name == ".env" || name.as_encoded_bytes().starts_with(b".env.")
}

/// `size` in whole MiB, rounded up, so a file just over a limit never reads as the limit itself.
fn mib(size: u64) -> u64 {
    size.div_ceil(MIB)
}

/// Walks the project at `project` like a snapshot does (the files its ignore rules do not ignore, without
/// `.git` and `.yalper`, never following links, in several threads), listing the files over the snapshot size
/// limit and, if `env`, the env files the rules ignore in the folders it enters.
fn walk_project(project: &Path, env: bool) -> Found {
    let excludes = Arc::new(Excludes::new(project));
    let filter = Arc::clone(&excludes);
    let found = Mutex::new(Found::default());
    WalkBuilder::new(project)
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .follow_links(false)
        .max_depth(Some(MAX_DEPTH))
        // An ignored folder is never entered. Env files are looked at even when ignored: those are the ones
        // copied.
        .filter_entry(move |entry| {
            let name = entry.file_name();
            if name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(YALPER_DIR) {
                return false;
            }
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            (!is_dir && is_env_name(name)) || !filter.is_ignored(entry.path(), is_dir)
        })
        .threads(WALK_THREADS)
        .build_parallel()
        .run(|| {
            let (excludes, found) = (&excludes, &found);
            Box::new(move |entry| visit(project, excludes, found, env, entry))
        });

    let mut found = found.into_inner().unwrap_or_else(PoisonError::into_inner);
    if excludes.exceeded() {
        let note = "the project's ignore files are too large for Yalper to read, so it does not look for env \
                    files or large files";
        return Found {
            notes: vec![note.to_owned()],
            ..Found::default()
        };
    }
    found.env.sort();
    found.large.sort();
    found.notes.sort();
    found
}

/// Handles one entry of [`walk_project`]. The walker calls this for a folder before any entry inside it is
/// checked, so its `.gitignore` is read in time.
fn visit(
    project: &Path,
    excludes: &Excludes,
    found: &Mutex<Found>,
    env: bool,
    entry: Result<ignore::DirEntry, ignore::Error>,
) -> WalkState {
    if excludes.exceeded() {
        return WalkState::Quit;
    }
    let Ok(entry) = entry else {
        return WalkState::Continue;
    };
    let Some(kind) = entry.file_type() else {
        return WalkState::Continue;
    };
    if kind.is_dir() {
        excludes.add_dir(entry.path(), true);
        return WalkState::Continue;
    }
    let Some(path) = entry.path().strip_prefix(project).ok().and_then(|path| {
        let names = path.components().map(|name| name.as_os_str().to_str());
        names
            .collect::<Option<Vec<&str>>>()
            .map(|names| names.join("/"))
    }) else {
        return WalkState::Continue;
    };
    let found = || found.lock().unwrap_or_else(PoisonError::into_inner);
    if is_env_name(entry.file_name()) && excludes.is_ignored(entry.path(), false) {
        if !env {
            return WalkState::Continue;
        }
        if kind.is_symlink() {
            found().notes.push(format!(
                "{path} is a link, so it is not copied: Yalper never follows links in the project"
            ));
        } else if kind.is_file()
            && let Ok(metadata) = entry.metadata()
        {
            if metadata.len() <= MAX_ENV_FILE_BYTES {
                found().env.push(path);
            } else {
                found().notes.push(format!(
                    "{path} is not copied: it holds {} bytes, more than an env file ({} MiB at most)",
                    metadata.len(),
                    MAX_ENV_FILE_BYTES / MIB
                ));
            }
        }
        return WalkState::Continue;
    }
    if kind.is_file()
        && let Ok(metadata) = entry.metadata()
        && metadata.len() > MAX_FILE_BYTES
    {
        found().large.push((path, metadata.len()));
    }
    WalkState::Continue
}

/// The files of the tree a copy holds, for deciding where Yalper may put a file.
#[derive(Debug, Default)]
struct Tree {
    /// Every folder that holds a file, as the tree spells it.
    dirs: HashSet<String>,
    /// The keys (see [`key`]) of every file.
    file_keys: HashSet<String>,
    /// The keys of every file and folder.
    keys: HashSet<String>,
}

impl Tree {
    fn of(copy: &TestCopy) -> io::Result<Self> {
        let files = copy
            .tree_files()
            .ok_or_else(|| io::Error::other("the test copy holds no step yet"))?;
        let mut tree = Self::default();
        for file in files {
            for dir in parents(file) {
                if !tree.dirs.contains(dir) {
                    tree.dirs.insert(dir.to_owned());
                    tree.keys.insert(key(dir));
                }
            }
            tree.file_keys.insert(key(file));
            tree.keys.insert(key(file));
        }
        Ok(tree)
    }

    /// Whether the folder `dir` (`""` for the top) holds files of the tree.
    fn holds_files_in(&self, dir: &str) -> bool {
        dir.is_empty() || self.dirs.contains(dir)
    }

    /// Whether the tree has a file or a folder at `path`, or a file where a folder above it would be.
    fn occupies(&self, path: &str) -> bool {
        let path = key(path);
        self.keys.contains(&path) || parents(&path).any(|parent| self.file_keys.contains(parent))
    }
}

/// `path` in the form that tells whether two paths may be the same file: lowercase, since even a
/// case-sensitive file system may have folders that ignore case (ext4 `casefold`).
fn key(path: &str) -> String {
    path.to_lowercase()
}

/// The size and mtime of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    len: u64,
    /// `None` if the file system gives no mtime: the file is then compared every time.
    mtime_ns: Option<i64>,
}

impl Stat {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            mtime_ns: metadata.modified().ok().map(nanos_since_epoch),
        }
    }

    /// Whether a file whose stat was `self` when compared at `at_ns` may have changed since without its stat
    /// changing (see [`is_racy`]).
    fn is_trusted_at(self, at_ns: i64) -> bool {
        self.mtime_ns.is_some_and(|mtime| !is_racy(mtime, at_ns))
    }
}

/// A copied file, when it was last found to hold the project's file.
#[derive(Debug, Clone, Copy)]
struct Same {
    copy: Stat,
    project: Stat,
    at_ns: i64,
}

/// What Yalper put in one test copy.
#[derive(Debug, Default)]
struct Held {
    /// The copy's folder name, such as `copy-0`.
    name: String,
    /// The copy's canonical path, which holds no link: files are deleted through it only while the copy is
    /// still there (see [`remove_copied`]).
    canonical: PathBuf,
    /// The files copied, with when each was last found to be the same as the project's.
    files: BTreeMap<String, Option<Same>>,
    /// The folders created to hold them, which are not folders of the tree.
    dirs: BTreeSet<String>,
    /// The tree the copy held at the last sync, with its files.
    tree: Option<(ObjectId, Tree)>,
}

/// Which kinds of files were put in a test copy for a run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Used {
    /// Env files.
    pub env: bool,
    /// Files of `--with` paths.
    pub with: bool,
}

/// The env files and `--with` files of a bisect, and what each test copy holds of them. Dropping it deletes
/// every file it copied.
#[derive(Debug)]
pub struct LocalFiles {
    project: PathBuf,
    /// The event log, which lists the copied files (see [`COPIED_KEY`]).
    events: Store,
    /// The instance id of the project's cache folder.
    instance: String,
    chosen: Chosen,
    /// The env files found, each copied for a step whose tree has files in its folder.
    env: Vec<String>,
    /// What each copy holds, by the copy's path.
    copies: HashMap<PathBuf, Held>,
    /// The files listed in the event log: copy name and path.
    listed: BTreeSet<(String, String)>,
    used: Used,
}

impl LocalFiles {
    /// The files `chosen` for the project whose cache folder is `cache`, found by one walk of the project, with
    /// warnings to show: those of `chosen`, env files that are links or too large, and files the snapshots skip
    /// because they are too large (and that no `--with` path brings in). `events` is the project's event log,
    /// where the copied files are listed; files already listed there stay listed.
    pub fn new(cache: &CacheDir, chosen: &Chosen, events: Store) -> (Self, Vec<String>) {
        let project = cache.project().to_owned();
        let found = walk_project(&project, chosen.env);
        let mut warnings = chosen.warnings.clone();
        warnings.extend(found.notes);
        let covered = |path: &str| {
            chosen.with.iter().any(|with| {
                path == with
                    || path
                        .strip_prefix(with.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        let large: Vec<&(String, u64)> = found
            .large
            .iter()
            .filter(|(path, _)| !covered(path))
            .collect();
        if !large.is_empty() {
            let mut named: Vec<String> = large
                .iter()
                .take(LARGE_FILES_NAMED)
                .map(|(path, size)| format!("{path} ({} MiB)", mib(*size)))
                .collect();
            if large.len() > LARGE_FILES_NAMED {
                named.push(format!("and {} more", large.len() - LARGE_FILES_NAMED));
            }
            let (count, verb) = match large.len() {
                1 => ("1 file of the project is".to_owned(), "it"),
                n => (format!("{n} files of the project are"), "them"),
            };
            warnings.push(format!(
                "{count} over {} MiB, so the snapshots skip {verb} and the test copies lack {verb}: {}. \
                 If the tests need one, add it with --with <path>.",
                MAX_FILE_BYTES / MIB,
                named.join(", ")
            ));
        }
        let listed = read_listed(&events, cache.instance()).unwrap_or_default();
        let files = Self {
            project,
            events,
            instance: cache.instance().to_owned(),
            chosen: chosen.clone(),
            env: found.env,
            copies: HashMap::new(),
            listed,
            used: Used::default(),
        };
        (files, warnings)
    }

    /// The line that tells what is copied, or `None` when nothing is.
    pub fn summary(&self) -> Option<String> {
        let mut what = Vec::new();
        if self.chosen.env {
            what.push(match self.env.len() {
                0 => "env files (none found)".to_owned(),
                1 => "env files (1 found)".to_owned(),
                n => format!("env files ({n} found)"),
            });
        }
        what.extend(self.chosen.with.iter().cloned());
        if what.is_empty() {
            return None;
        }
        let off = match (self.chosen.env, !self.chosen.with.is_empty()) {
            (true, true) => "--no-env and --no-with turn it off",
            (true, false) => "--no-env turns it off",
            _ => "--no-with turns it off",
        };
        Some(format!(
            "Copied into the test copies until the bisect ends: {}. Remembered for this project; {off}.",
            what.join(", ")
        ))
    }

    /// Which kinds of files were put in a test copy so far.
    pub fn used(&self) -> Used {
        self.used
    }

    /// Makes `copy`, which holds a step, hold the env files and `--with` files that step gets: those whose path
    /// the step's tree and the other extra paths of the copy leave free and, for env files, whose folder holds
    /// files of the tree. Each one is listed in the event log before it is first written, and written only if
    /// it differs from the project's file; files copied for an earlier step that this one does not get are
    /// deleted. Call it after every switch, before every run and install. Returns how many files were written.
    pub fn sync(&mut self, copy: &mut TestCopy) -> io::Result<usize> {
        let root = copy.path().to_owned();
        if self.env.is_empty()
            && self.chosen.with_files.is_empty()
            && !self.copies.contains_key(&root)
        {
            return Ok(0);
        }
        let Self {
            project,
            events,
            instance,
            chosen,
            env,
            copies,
            listed,
            used,
        } = self;
        let held = match copies.entry(root.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Held {
                name: root
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                canonical: plain(&fs::canonicalize(&root)?),
                ..Held::default()
            }),
        };
        let id = copy
            .tree()
            .ok_or_else(|| io::Error::other("the test copy holds no step yet"))?;
        let (id, tree) = match held.tree.take() {
            Some((cached, tree)) if cached == id => (id, tree),
            _ => (id, Tree::of(copy)?),
        };
        // The extra paths others put in the copy (a folder of links to the project's packages, a link to its
        // Python environment): what is there is theirs, never replaced.
        let others: Vec<String> = copy
            .extra_paths()
            .iter()
            .filter(|path| !held.files.contains_key(*path))
            .map(|path| key(path))
            .collect();
        let taken = |path: &str| {
            let path = key(path);
            tree.occupies(&path)
                || others.iter().any(|other| {
                    path == *other
                        || path
                            .strip_prefix(other.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
        };
        let wanted: BTreeSet<&String> = env
            .iter()
            .filter(|path| {
                let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
                tree.holds_files_in(dir)
            })
            .chain(&chosen.with_files)
            .filter(|path| !taken(path))
            .collect();

        let before: Vec<String> = held.files.keys().cloned().collect();
        for path in &before {
            if wanted.contains(path) {
                continue;
            }
            // Where the tree or another extra path now has an entry, it was put there by its owner.
            let gone = taken(path)
                || remove_copied(&held.canonical, path)
                    .map_err(|error| in_copy(error, "delete", path))?;
            if gone {
                listed.remove(&(held.name.clone(), path.clone()));
            }
            unregister(&held.canonical, path);
            held.files.remove(path);
        }
        save_listed(events, instance, listed)?;
        // Registered before anything is written, so a switch keeps a file left half made by an error.
        let mut extra = copy.extra_paths().clone();
        extra.retain(|path| !before.contains(path));
        extra.extend(wanted.iter().map(|path| (*path).clone()));
        copy.set_extra_paths(extra);

        let mut written = 0;
        for &path in &wanted {
            // Held until the file is written: a second Ctrl-C waits for it, then deletes it.
            let mut copied = copied();
            copied.insert((held.canonical.clone(), path.clone()));
            if listed.insert((held.name.clone(), path.clone())) {
                save_listed(events, instance, listed)?;
            }
            let same = held.files.entry(path.clone()).or_default();
            if refresh(project, &root, path, same, &mut held.dirs, &tree)
                .map_err(|error| in_copy(error, "copy", path))?
            {
                written += 1;
            }
            drop(copied);
            if env.contains(path) {
                used.env = true;
            }
            if chosen.with_files.contains(path) {
                used.with = true;
            }
        }
        held.tree = Some((id, tree));
        Ok(written)
    }

    /// Deletes every file copied into a test copy, and the folders made for them once empty. Returns a message
    /// for each file that could not be deleted, or that was not looked for because a test moved or replaced a
    /// folder on its way: those stay listed in the event log, for the next bisect. Dropping it does the same,
    /// without the messages.
    pub fn remove(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        for (root, held) in &mut self.copies {
            for path in std::mem::take(&mut held.files).into_keys() {
                match remove_copied(&held.canonical, &path) {
                    Ok(true) => {
                        self.listed.remove(&(held.name.clone(), path.clone()));
                    }
                    Ok(false) => failed.push(not_looked_for(&path, root)),
                    Err(error) => failed.push(format!(
                        "cannot delete {path} from the test copy {}: {error}. Delete it by hand.",
                        root.display()
                    )),
                }
                unregister(&held.canonical, &path);
            }
            // Inner folders first.
            for dir in std::mem::take(&mut held.dirs).iter().rev() {
                remove_empty_dir(&held.canonical, dir);
            }
        }
        if let Err(error) = save_listed(&self.events, &self.instance, &self.listed) {
            failed.push(error.to_string());
        }
        failed
    }
}

impl Drop for LocalFiles {
    fn drop(&mut self) {
        self.remove();
    }
}

/// The message for a copied file at `path` in the copy `root` that was not looked for (see [`remove_copied`]).
fn not_looked_for(path: &str, root: &Path) -> String {
    format!(
        "{path} was not deleted from the test copy {}: a test moved or replaced the copy or a folder on its way \
         with a link, and Yalper does not follow it. Delete the file by hand if it is still there.",
        root.display()
    )
}

/// The files listed in `events` (see [`COPIED_KEY`]) for the cache folder whose instance id is `instance`:
/// none if the list is for another cache folder.
fn read_listed(events: &Store, instance: &str) -> io::Result<BTreeSet<(String, String)>> {
    let value = events.meta(COPIED_KEY).map_err(|error| {
        io::Error::other(format!(
            "cannot read the list of copied files in the event log: {error}"
        ))
    })?;
    let Some(value) = value.and_then(|value| serde_json::from_str::<Value>(&value).ok()) else {
        return Ok(BTreeSet::new());
    };
    if value.get("instance").and_then(Value::as_str) != Some(instance) {
        return Ok(BTreeSet::new());
    }
    let files = value.get("files").and_then(Value::as_array);
    Ok(files
        .into_iter()
        .flatten()
        .filter_map(|file| {
            let copy = file.get("copy")?.as_str()?;
            let path = file.get("path")?.as_str()?;
            Some((copy.to_owned(), path.to_owned()))
        })
        .collect())
}

/// Writes `listed` into `events` (see [`COPIED_KEY`]), or deletes the row when it is empty.
fn save_listed(
    events: &Store,
    instance: &str,
    listed: &BTreeSet<(String, String)>,
) -> io::Result<()> {
    let value = (!listed.is_empty()).then(|| {
        let files: Vec<Value> = listed
            .iter()
            .map(|(copy, path)| json!({"copy": copy, "path": path}))
            .collect();
        json!({"instance": instance, "files": files}).to_string()
    });
    events
        .set_meta(COPIED_KEY, value.as_deref())
        .map_err(|error| {
            io::Error::other(format!(
                "cannot record the copied files in the event log: {error}"
            ))
        })
}

/// Whether `name` is the name of a test copy: `copy-<digits>`.
fn is_copy_name(name: &str) -> bool {
    name.strip_prefix("copy-")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// Deletes the files that `events` lists as copied into the test copies of the project's cache folder `cache`
/// (see [`COPIED_KEY`]): an earlier bisect ended without deleting them (Yalper was killed, crashed, or its
/// console was closed). Call it at the start of a bisect, under the bisect lock, before any copy is used.
/// Files that could not be deleted stay listed. Returns a warning for each of them.
pub fn remove_left_over(cache: &CacheDir, events: &Store) -> Vec<String> {
    let listed = match read_listed(events, cache.instance()) {
        Ok(listed) => listed,
        Err(error) => return vec![error.to_string()],
    };
    let mut kept = BTreeSet::new();
    let mut warnings = Vec::new();
    for (copy, path) in listed {
        if !is_copy_name(&copy) || validate_path(&path, FileKind::Regular).is_err() {
            continue;
        }
        let root = cache.path().join(&copy);
        // A copy that is gone was deleted with its files.
        let Ok(canonical) = fs::canonicalize(&root) else {
            continue;
        };
        match remove_copied(&plain(&canonical), &path) {
            Ok(true) => continue,
            Ok(false) => warnings.push(not_looked_for(&path, &root)),
            Err(error) => warnings.push(format!(
                "cannot delete {path}, left by an earlier bisect, from the test copy {}: {error}. Delete it \
                 by hand.",
                root.display()
            )),
        }
        kept.insert((copy, path));
    }
    if let Err(error) = save_listed(events, cache.instance(), &kept) {
        warnings.push(error.to_string());
    }
    warnings
}

/// Adds the file's path to an error.
fn in_copy(error: io::Error, doing: &str, path: &str) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("cannot {doing} {path} in the test copy: {error}"),
    )
}

/// Makes `path` in the copy at `root` hold the project's file at `path` in `project`, unless it already
/// does: `same` says when it was last found to, and is updated. Folders above it that are missing (or are not
/// real folders) are made, and those that are not folders of `tree` added to `dirs`. Returns whether the file
/// was written.
fn refresh(
    project: &Path,
    root: &Path,
    path: &str,
    same: &mut Option<Same>,
    dirs: &mut BTreeSet<String>,
    tree: &Tree,
) -> io::Result<bool> {
    let (source, project_stat) = project_file(project, path)?;
    for parent in parents(path) {
        let dir = native(root, parent);
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(metadata) => remove_entry(&dir, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::create_dir(&dir)?;
        if !tree.holds_files_in(parent) {
            dirs.insert(parent.to_owned());
        }
    }

    let target = native(root, path);
    let found = match fs::symlink_metadata(&target) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(metadata) = &found
        && metadata.is_file()
    {
        let copy_stat = Stat::of(metadata);
        if let Some(last) = same
            && last.copy == copy_stat
            && last.project == project_stat
            && copy_stat.is_trusted_at(last.at_ns)
            && project_stat.is_trusted_at(last.at_ns)
        {
            return Ok(false);
        }
        let at_ns = now_ns();
        if same_bytes(&source, &target)? {
            *same = Some(Same {
                copy: copy_stat,
                project: project_stat,
                at_ns,
            });
            return Ok(false);
        }
    }
    // Never written through: a link, a hard link or a folder at its path is removed first.
    if let Some(metadata) = &found {
        remove_entry(&target, metadata)?;
    }
    let at_ns = now_ns();
    copy_file(&source, &target)?;
    *same = Some(Same {
        copy: Stat::of(&fs::symlink_metadata(&target)?),
        project: project_stat,
        at_ns,
    });
    Ok(true)
}

/// The project's file at `path` and its stat: a regular file reached through real folders.
fn project_file(project: &Path, path: &str) -> io::Result<(PathBuf, Stat)> {
    if !parents(path).all(|parent| is_real_dir_no_follow(&native(project, parent))) {
        return Err(io::Error::other(
            "a folder above it in the project is no longer a real folder",
        ));
    }
    let full = native(project, path);
    let metadata = fs::symlink_metadata(&full)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "it is no longer a regular file in the project",
        ));
    }
    Ok((full, Stat::of(&metadata)))
}

fn now_ns() -> i64 {
    nanos_since_epoch(SystemTime::now())
}

/// Whether the regular files `a` and `b` hold the same bytes. A file a test made unreadable differs.
fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let mut a = open_regular_file(a)?;
    let mut b = match open_regular_file(b) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => return Err(error),
    };
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0; COMPARE_CHUNK], vec![0; COMPARE_CHUNK]);
    loop {
        let read = fill(&mut a, &mut x)?;
        if read != fill(&mut b, &mut y)? || x[..read] != y[..read] {
            return Ok(false);
        }
        if read == 0 {
            return Ok(true);
        }
    }
}

/// Reads from `file` until `buffer` is full or the file ends. Returns how many bytes were read.
fn fill(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Creates the file `target`, which must not exist, with the bytes of the regular file `source`: 0600 on Unix,
/// set on the open file so the mode does not depend on the umask or on the folder.
fn copy_file(source: &Path, target: &Path) -> io::Result<()> {
    let from = open_regular_file(source)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut to = options.open(target)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        to.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let copied = io::copy(&mut from.take(MAX_WITH_BYTES + 1), &mut to)?;
    if copied > MAX_WITH_BYTES {
        return Err(io::Error::other(format!(
            "it holds more than {} MiB",
            MAX_WITH_BYTES / MIB
        )));
    }
    Ok(())
}

/// Deletes the file Yalper copied at `path` in the copy whose canonical path is `root`, or whatever a test put
/// in its place, never following a link. Returns `true` when the file is gone (deleted, or not there, with the
/// copy or a folder above it), and `false` when it was not looked for: the copy is no longer at its canonical
/// path, or a folder on the way is a link or a file. A test moved or replaced it; the file may be elsewhere.
fn remove_copied(root: &Path, path: &str) -> io::Result<bool> {
    match fs::canonicalize(root) {
        Ok(now) if plain(&now) == root => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        _ => return Ok(false),
    }
    for parent in parents(path) {
        match fs::symlink_metadata(native(root, parent)) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        }
    }
    let full = native(root, path);
    match fs::symlink_metadata(&full) {
        Ok(metadata) => remove_entry(&full, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(true)
}

/// Deletes the folder `dir` of the copy whose canonical path is `root` if it is an empty real folder reached
/// through real folders.
fn remove_empty_dir(root: &Path, dir: &str) {
    let still_there = fs::canonicalize(root).is_ok_and(|now| plain(&now) == root);
    if still_there
        && parents(dir)
            .chain([dir])
            .all(|folder| is_real_dir_no_follow(&native(root, folder)))
    {
        let _ = fs::remove_dir(native(root, dir));
    }
}

fn copied() -> MutexGuard<'static, BTreeSet<(PathBuf, String)>> {
    COPIED.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unregister(root: &Path, path: &str) {
    copied().remove(&(root.to_owned(), path.to_owned()));
}

/// Deletes every file copied into a test copy by any bisect of this process, for a second Ctrl-C, which makes
/// Yalper exit without dropping anything. A file being written is waited for, then deleted. Returns the list,
/// locked and empty: held until Yalper exits, it keeps a bisect still going from copying anything more. The
/// event log still lists the files, which the next bisect finds gone.
pub(super) fn remove_all_copied() -> MutexGuard<'static, BTreeSet<(PathBuf, String)>> {
    let mut copied = copied();
    for (root, path) in std::mem::take(&mut *copied) {
        let _ = remove_copied(&root, &path);
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bisect::link::make_link;
    use crate::bisect::testing::Project;

    fn read(copy: &TestCopy, path: &str) -> String {
        fs::read_to_string(native(copy.path(), path)).unwrap()
    }

    fn exists(copy: &TestCopy, path: &str) -> bool {
        fs::symlink_metadata(native(copy.path(), path)).is_ok()
    }

    fn env_only() -> Chosen {
        Chosen {
            env: true,
            ..Chosen::default()
        }
    }

    fn with(paths: &[&str]) -> Choice {
        Choice {
            with: paths.iter().map(PathBuf::from).collect(),
            ..Choice::default()
        }
    }

    #[cfg(unix)]
    fn mode(copy: &TestCopy, path: &str) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(native(copy.path(), path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn env_files_are_copied_where_the_step_has_files_and_written_again_only_when_they_differ() {
        let project = Project::new();
        let rules = ".env\n.env.*\n!.env.example\nignored/\n";
        project.write(".gitignore", rules);
        project.write(".env", "TOKEN=one\n");
        project.write(".env.local", "LOCAL=1\n");
        // Not ignored: snapshots hold it, from the steps that have it.
        project.write(".env.example", "TOKEN=\n");
        project.write("api/.env", "API=1\n");
        // No file of the step is in `docs`, and `ignored` is never entered.
        project.write("docs/.env", "DOCS=1\n");
        project.write("ignored/.env", "IGNORED=1\n");
        // A Python environment named `.env` is a folder, not an env file.
        project.write("venv/.env/pyvenv.cfg", "home = /usr\n");
        let (mut files, warnings) = LocalFiles::new(&project.cache, &env_only(), project.events());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            files.summary().unwrap(),
            "Copied into the test copies until the bisect ends: env files (4 found). Remembered for this \
             project; --no-env turns it off."
        );

        let mut copy = project.copy();
        let tree = project.tree(&[(".gitignore", rules), ("api/main.py", "")]);
        copy.switch(&project.store, tree).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 3);
        assert_eq!(read(&copy, ".env"), "TOKEN=one\n");
        assert_eq!(read(&copy, ".env.local"), "LOCAL=1\n");
        assert_eq!(read(&copy, "api/.env"), "API=1\n");
        for absent in ["docs", "ignored", ".env.example"] {
            assert!(!exists(&copy, absent), "{absent}");
        }
        #[cfg(unix)]
        assert_eq!(mode(&copy, ".env"), 0o600);
        assert_eq!(files.sync(&mut copy).unwrap(), 0);

        // A test run changes the copy's `.env` and puts a folder in place of `.env.local`. The switch before
        // the next run keeps them, as extra paths; the files are then written again.
        fs::write(native(copy.path(), ".env"), "TOKEN=changed by a test\n").unwrap();
        fs::remove_file(native(copy.path(), ".env.local")).unwrap();
        fs::create_dir(native(copy.path(), ".env.local")).unwrap();
        fs::write(native(copy.path(), ".env.local/x"), "x").unwrap();
        copy.switch(&project.store, tree).unwrap();
        assert_eq!(read(&copy, ".env"), "TOKEN=changed by a test\n");
        assert_eq!(files.sync(&mut copy).unwrap(), 2);
        assert_eq!(read(&copy, ".env"), "TOKEN=one\n");
        assert_eq!(read(&copy, ".env.local"), "LOCAL=1\n");
        assert_eq!(files.sync(&mut copy).unwrap(), 0);

        // The project's file changed, to the same size: the next run gets it.
        project.write(".env", "TOKEN=two\n");
        assert_eq!(files.sync(&mut copy).unwrap(), 1);
        assert_eq!(read(&copy, ".env"), "TOKEN=two\n");
        assert_eq!(
            files.used(),
            Used {
                env: true,
                with: false
            }
        );

        assert!(files.remove().is_empty());
        for gone in [".env", ".env.local", "api/.env"] {
            assert!(!exists(&copy, gone), "{gone}");
        }
        assert!(exists(&copy, "api/main.py"));
    }

    #[test]
    fn the_tree_wins_where_it_has_a_file_at_the_path() {
        let project = Project::new();
        project.write(".gitignore", ".env\n");
        project.write(".env", "LOCAL=1\n");
        let (mut files, _) = LocalFiles::new(&project.cache, &env_only(), project.events());
        let mut copy = project.copy();
        let tracked = project.tree(&[(".env", "TRACKED=1\n"), ("a.txt", "a")]);
        let untracked = project.tree(&[("a.txt", "b")]);
        let folder = project.tree(&[(".env/x", "a folder of the tree")]);

        copy.switch(&project.store, tracked).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 0);
        assert_eq!(read(&copy, ".env"), "TRACKED=1\n");
        copy.switch(&project.store, untracked).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 1);
        assert_eq!(read(&copy, ".env"), "LOCAL=1\n");
        copy.switch(&project.store, folder).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 0);
        assert_eq!(read(&copy, ".env/x"), "a folder of the tree");
        copy.switch(&project.store, tracked).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 0);
        assert_eq!(read(&copy, ".env"), "TRACKED=1\n");

        // The tree's file is not one of the copied files.
        drop(files);
        assert_eq!(read(&copy, ".env"), "TRACKED=1\n");
    }

    #[test]
    fn extra_paths_of_others_are_kept_as_they_are() {
        let project = Project::new();
        project.write(".gitignore", "node_modules/\n");
        project.write("node_modules/pkg/settings.json", "project");
        let events = project.events();
        let root = project.path();
        let chosen = choose(
            &events,
            root,
            root,
            &with(&["node_modules/pkg/settings.json"]),
        )
        .unwrap();
        let (mut files, _) = LocalFiles::new(&project.cache, &chosen, project.events());
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("a.txt", "a")]))
            .unwrap();
        // The folder of links to the project's packages, as `NodeModules` registers it.
        copy.set_extra_paths(["node_modules".to_owned()]);
        fs::create_dir_all(native(copy.path(), "node_modules/pkg")).unwrap();
        fs::write(
            native(copy.path(), "node_modules/pkg/settings.json"),
            "theirs",
        )
        .unwrap();

        assert_eq!(files.sync(&mut copy).unwrap(), 0);
        assert_eq!(read(&copy, "node_modules/pkg/settings.json"), "theirs");
        assert_eq!(
            copy.extra_paths(),
            &HashSet::from(["node_modules".to_owned()])
        );
        drop(files);
        assert_eq!(read(&copy, "node_modules/pkg/settings.json"), "theirs");
    }

    #[test]
    fn with_paths_must_be_ignored_and_inside_the_project() {
        let project = Project::new();
        project.write(".gitignore", "local/\n.cargo/\n*.key\n");
        project.write(".cargo/config.toml", "[alias]\n");
        project.write("local/settings.json", "{}");
        project.write("local/nested/more.txt", "more");
        project.write("secret.key", "key");
        project.write("README.md", "# demo\n");
        let big = "x".repeat(MAX_FILE_BYTES as usize + 1);
        project.write("data.bin", &big);
        let events = project.events();
        let root = project.path();

        let chosen = choose(
            &events,
            root,
            root,
            &with(&[".cargo/config.toml", "local", "secret.key", "data.bin"]),
        )
        .unwrap();
        assert_eq!(
            chosen.with,
            [".cargo/config.toml", "local", "secret.key", "data.bin"]
        );
        assert_eq!(
            chosen.with_files,
            [
                ".cargo/config.toml",
                "data.bin",
                "local/nested/more.txt",
                "local/settings.json",
                "secret.key"
            ]
        );
        // Remembered, and paths given from a subfolder start there.
        let more = choose(
            &events,
            root,
            &root.join("local"),
            &with(&["../secret.key"]),
        )
        .unwrap();
        assert_eq!(more.with, chosen.with);
        assert!(!more.env);

        for (path, why) in [
            ("README.md", "the project's ignore rules do not ignore it"),
            ("../outside.key", "it is outside the project"),
            ("missing.key", "it does not exist in the project"),
            (".git/config", "Yalper does not copy .git or .yalper"),
            (".yalper", "Yalper does not copy .git or .yalper"),
            (".", "it is the project itself"),
        ] {
            let error = choose(&events, root, root, &with(&[path])).unwrap_err();
            assert!(
                error.starts_with(&format!("--with {path}: {why}")),
                "{path}: {error}"
            );
        }
        // Nothing refused was remembered.
        assert_eq!(
            choose(&events, root, root, &Choice::default()).unwrap(),
            chosen
        );

        // A remembered path that is no longer ignored is refused, until it is forgotten.
        project.write(".gitignore", ".cargo/\n*.key\n");
        let error = choose(&events, root, root, &Choice::default()).unwrap_err();
        assert!(
            error.starts_with(
                "--with local, remembered for this project, the project's ignore rules"
            ),
            "{error}"
        );
        assert!(error.contains("--no-with"), "{error}");
        let forgotten = Choice {
            no_with: true,
            ..with(&["secret.key"])
        };
        assert_eq!(
            choose(&events, root, root, &forgotten).unwrap().with,
            ["secret.key"]
        );
        let forgotten = Choice {
            no_with: true,
            ..Choice::default()
        };
        assert!(
            choose(&events, root, root, &forgotten)
                .unwrap()
                .with
                .is_empty()
        );
        assert_eq!(events.meta(WITH_KEY).unwrap(), None);
    }

    #[test]
    fn the_env_choice_is_remembered_until_turned_off() {
        let project = Project::new();
        let events = project.events();
        let root = project.path();
        let env = |env| Choice {
            env,
            ..Choice::default()
        };
        assert!(!choose(&events, root, root, &env(None)).unwrap().env);
        assert!(choose(&events, root, root, &env(Some(true))).unwrap().env);
        assert_eq!(events.meta(WITH_ENV_KEY).unwrap().as_deref(), Some("true"));
        assert!(choose(&events, root, root, &env(None)).unwrap().env);
        assert!(!choose(&events, root, root, &env(Some(false))).unwrap().env);
        assert_eq!(events.meta(WITH_ENV_KEY).unwrap(), None);
        assert!(!choose(&events, root, root, &env(None)).unwrap().env);
    }

    #[test]
    fn links_in_the_project_are_never_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("config.toml"), "outside").unwrap();
        project.write(".gitignore", "linked/\nreal/\n.env\n");
        make_link(canary.path(), &project.path().join("linked")).unwrap();
        project.write("real/x.txt", "x");
        make_link(canary.path(), &project.path().join("real").join("inner")).unwrap();
        let events = project.events();
        let root = project.path();
        for (path, why) in [
            ("linked", "it is a link"),
            ("linked/config.toml", "linked is a link"),
            ("real", "real/inner is a link"),
        ] {
            let error = choose(&events, root, root, &with(&[path])).unwrap_err();
            assert!(
                error.starts_with(&format!("--with {path}: {why}")),
                "{path}: {error}"
            );
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(canary.path().join("config.toml"), root.join(".env"))
                .unwrap();
            let (files, warnings) = LocalFiles::new(&project.cache, &env_only(), project.events());
            assert_eq!(
                warnings,
                [".env is a link, so it is not copied: Yalper never follows links in the project"]
            );
            assert!(files.env.is_empty());
        }
    }

    #[test]
    fn deleting_the_copied_files_never_follows_a_link_a_test_left() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join(".env"), "CANARY=1\n").unwrap();
        project.write(".gitignore", ".env\n");
        project.write("api/.env", "API=1\n");
        let (mut files, _) = LocalFiles::new(&project.cache, &env_only(), project.events());
        let mut copy = project.copy();
        let tree = project.tree(&[("api/main.py", "")]);
        copy.switch(&project.store, tree).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 1);

        // A test replaces the copy's `api` folder with a link to a folder holding an `.env`: the next sync
        // writes into a real folder made in place of the link, never through it.
        let api = native(copy.path(), "api");
        fs::remove_dir_all(&api).unwrap();
        make_link(canary.path(), &api).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 1);
        assert!(fs::symlink_metadata(&api).unwrap().is_dir());
        assert_eq!(read(&copy, "api/.env"), "API=1\n");
        assert_eq!(
            fs::read_to_string(canary.path().join(".env")).unwrap(),
            "CANARY=1\n"
        );

        // Again before the bisect ends: nothing is deleted through the link, and that is reported. The file stays
        // listed for the next bisect.
        fs::remove_dir_all(&api).unwrap();
        make_link(canary.path(), &api).unwrap();
        let warnings = files.remove();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("api/.env was not deleted from the test copy "),
            "{warnings:?}"
        );
        assert_eq!(
            fs::read_to_string(canary.path().join(".env")).unwrap(),
            "CANARY=1\n"
        );
        assert!(project.events().meta(COPIED_KEY).unwrap().is_some());
        drop(files);
        // The next bisect does not follow the link either.
        let warnings = remove_left_over(&project.cache, &project.events());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            fs::read_to_string(canary.path().join(".env")).unwrap(),
            "CANARY=1\n"
        );
    }

    #[test]
    fn files_left_by_a_bisect_that_did_not_end_are_deleted_by_the_next_one() {
        let project = Project::new();
        project.write(".gitignore", ".env\n");
        project.write(".env", "LOCAL=1\n");
        let (mut files, _) = LocalFiles::new(&project.cache, &env_only(), project.events());
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("a.txt", "a")]))
            .unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 1);
        // Listed, by name only, before it was written.
        let listed = project.events().meta(COPIED_KEY).unwrap().unwrap();
        let listed: Value = serde_json::from_str(&listed).unwrap();
        assert_eq!(
            listed,
            json!({
                "instance": project.cache.instance(),
                "files": [{"copy": "copy-0", "path": ".env"}]
            })
        );
        // Yalper ends without deleting anything (killed, or its console closed).
        std::mem::forget(files);
        assert!(exists(&copy, ".env"));

        assert!(remove_left_over(&project.cache, &project.events()).is_empty());
        assert!(!exists(&copy, ".env"));
        assert_eq!(project.events().meta(COPIED_KEY).unwrap(), None);
        assert!(remove_left_over(&project.cache, &project.events()).is_empty());

        // A normal end leaves nothing listed. A new bisect opens its copies anew.
        drop(copy);
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("a.txt", "a")]))
            .unwrap();
        let (mut files, _) = LocalFiles::new(&project.cache, &env_only(), project.events());
        assert_eq!(files.sync(&mut copy).unwrap(), 1);
        assert!(files.remove().is_empty());
        assert!(!exists(&copy, ".env"));
        assert_eq!(project.events().meta(COPIED_KEY).unwrap(), None);
    }

    #[test]
    fn a_remembered_with_path_that_is_gone_is_skipped_with_a_warning() {
        let project = Project::new();
        project.write(".gitignore", "*.key\n");
        project.write("a.key", "a");
        project.write("b.key", "b");
        let events = project.events();
        let root = project.path();
        choose(&events, root, root, &with(&["a.key", "b.key"])).unwrap();
        fs::remove_file(root.join("a.key")).unwrap();
        let chosen = choose(&events, root, root, &Choice::default()).unwrap();
        assert_eq!(chosen.with, ["b.key"]);
        assert_eq!(chosen.with_files, ["b.key"]);
        assert_eq!(
            chosen.warnings,
            [
                "--with a.key, remembered for this project, does not exist in the project now, so it is not \
                 copied. `yalper bisect --no-with` forgets the remembered paths."
            ]
        );
        let (_, warnings) = LocalFiles::new(&project.cache, &chosen, project.events());
        assert_eq!(warnings, chosen.warnings);
        // Still remembered: it is copied again once it is back.
        project.write("a.key", "a");
        let chosen = choose(&events, root, root, &Choice::default()).unwrap();
        assert_eq!(chosen.with, ["a.key", "b.key"]);
        assert!(chosen.warnings.is_empty());
    }

    #[test]
    fn with_files_are_copied_with_their_folders_and_deleted_with_them() {
        let project = Project::new();
        project.write(".gitignore", ".cargo/\nlocal/\n");
        project.write(".cargo/config.toml", "[alias]\nx = \"version\"\n");
        project.write("local/a.txt", "a");
        project.write("local/b/c.txt", "c");
        let events = project.events();
        let root = project.path();
        let chosen = choose(&events, root, root, &with(&[".cargo/config.toml", "local"])).unwrap();
        let (mut files, warnings) = LocalFiles::new(&project.cache, &chosen, project.events());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            files.summary().unwrap(),
            "Copied into the test copies until the bisect ends: .cargo/config.toml, local. Remembered for \
             this project; --no-with turns it off."
        );

        let mut copy = project.copy();
        let tree = project.tree(&[("src/lib.rs", "")]);
        copy.switch(&project.store, tree).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 3);
        assert_eq!(
            read(&copy, ".cargo/config.toml"),
            "[alias]\nx = \"version\"\n"
        );
        assert_eq!(read(&copy, "local/b/c.txt"), "c");
        #[cfg(unix)]
        assert_eq!(mode(&copy, "local/b/c.txt"), 0o600);
        copy.switch(&project.store, tree).unwrap();
        assert_eq!(files.sync(&mut copy).unwrap(), 0);
        assert_eq!(
            files.used(),
            Used {
                env: false,
                with: true
            }
        );

        assert!(files.remove().is_empty());
        for gone in [".cargo", "local"] {
            assert!(!exists(&copy, gone), "{gone}");
        }
        assert!(exists(&copy, "src/lib.rs"));
    }

    #[test]
    fn files_the_snapshots_skip_are_named_unless_brought_in() {
        let project = Project::new();
        let big = "x".repeat(MAX_FILE_BYTES as usize + 1);
        project.write("data/big.bin", &big);
        project.write(".gitignore", "ignored.bin\n");
        project.write("ignored.bin", &big);
        let (_, warnings) = LocalFiles::new(&project.cache, &Chosen::default(), project.events());
        assert_eq!(
            warnings,
            [
                "1 file of the project is over 10 MiB, so the snapshots skip it and the test copies lack it: \
                 data/big.bin (11 MiB). If the tests need one, add it with --with <path>."
            ]
        );
        let events = project.events();
        let root = project.path();
        let refused = choose(&events, root, root, &with(&["data"])).unwrap_err();
        assert!(refused.contains("do not ignore it"), "{refused}");
        let chosen = choose(&events, root, root, &with(&["data/big.bin"])).unwrap();
        let (_, warnings) = LocalFiles::new(&project.cache, &chosen, project.events());
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
