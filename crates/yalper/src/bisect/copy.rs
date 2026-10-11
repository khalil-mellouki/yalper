//! Test copies: folders `copy-<n>/` in the project's cache folder (see [`CacheDir`]) that hold the code of one
//! snapshot step, reused from one step to the next.
//!
//! Switching a copy to another step writes only the files that differ, so a switch costs milliseconds even
//! in a large project. Nothing on disk is trusted between two bisects: the first switch of a [`TestCopy`]
//! hashes every file of the target tree found in the copy and writes only those that are missing or differ.
//! Later switches apply the tree diff between the step the copy holds and the next one.
//!
//! Every switch also undoes what the test run before it did, so each run starts from the exact files of its
//! step:
//!
//! - files of the tree the run changed or deleted are written again. The copy keeps the size and mtime of
//!   each file it wrote (a stat manifest, in memory only): a file whose size, mtime or kind changed, or whose
//!   mtime is so close to the end of the previous switch that the run could have changed it within the same
//!   timestamp tick, is hashed, and written again if its content differs (the same rule as recording's stat
//!   cache);
//! - every other file or folder is removed unless the step's ignore rules ignore it (see [`sweep`]), so
//!   build caches such as `target/` or `__pycache__/` are kept for speed. The first switch does the same
//!   cleanup, since the copy may still hold files of another tree.
//!
//! The copy is written with its own small writer, not a git checkout: snapshots hold raw bytes, and a
//! checkout would apply `.gitattributes` filters such as line-ending conversion. Its rules:
//!
//! - every path is checked with [`validate_path`] before anything is written, so a tree with a path that is
//!   invalid on this system is refused as a whole;
//! - removals come before writes (a case-only rename, a file replaced by a folder);
//! - nothing is ever written through a link: every parent folder is checked with `symlink_metadata`, and a
//!   link or file in the way is removed (the link itself, never what it points to) and replaced by a real
//!   folder; a file is removed and created anew, so a link or hard link left at its path is not followed;
//! - files get mode 0644 or 0755 from the tree on Unix; a link of the tree is a link on Unix and, like git
//!   with `core.symlinks=false`, a plain file holding the link target on Windows (creating links there needs
//!   Developer Mode or administrator rights);
//! - a read-only attribute (or, on Unix, a folder without write permission) left by a test is cleared before
//!   an entry is replaced or deleted;
//! - the cached bytecode of every Python file written or removed is deleted, next to the file and in the
//!   copy's `PYTHONPYCACHEPREFIX` folder (see [`Bytecode`]).

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::SystemTime;

use gix::ObjectId;

use super::CacheDir;
use super::link::{is_real_dir_no_follow, normalize, plain};
use crate::repo;
use crate::safe_fs::{self, OwnedDir};
use crate::snapshot::{
    CASE_INSENSITIVE, Error, Excludes, FileChange, FileKind, MAX_FILE_BYTES, Result, ShadowStore,
    TreeFile, is_racy, nanos_since_epoch, over_budget_reason, validate_path,
};

/// The file in the project's cache folder whose mtime gives the time a switch ended, on the file system's
/// clock (see [`is_racy`]).
pub(super) const CLOCK_FILE: &str = "clock";

/// The folder in the project's cache folder that holds the `PYTHONPYCACHEPREFIX` folder of each copy,
/// `pycache/copy-<n>/`: Python writes the bytecode of the files a test run imports there, never next to them,
/// so nothing is written into the project's virtual environment through a link of the copy.
pub(super) const PYCACHE_DIR: &str = "pycache";

/// The folder in the project's cache folder that holds the `CARGO_TARGET_DIR` of each copy,
/// `cargo-target/copy-<n>/`, reused by every step and every bisect of that copy. One per copy, not one shared by
/// all: cargo tells a workspace crate is fresh from its source mtimes and from hashes of paths relative to the
/// workspace, so a copy whose sources are older than another copy's build would run that build.
pub(super) const CARGO_TARGET_DIR: &str = "cargo-target";

/// The most entries (files and folders) of a tree diff one switch takes. Over it the switch is refused: no
/// project has that many files, but a crafted store could repeat a large subtree many times.
const MAX_CHANGES: usize = 1_000_000;

/// From this many files to write, they are written by [`WRITE_THREADS`] threads. Creating files is mostly
/// waiting on the file system (measured on Windows, 1,000 files: 0.53 s with 4 threads instead of 0.75 s with
/// one); for a usual step of a few files, starting threads would cost more than it saves.
const PARALLEL_WRITES_FROM: usize = 64;

const WRITE_THREADS: usize = 4;

/// Files read from the store ahead of the writer threads. Each holds at most [`MAX_FILE_BYTES`], so this
/// bounds memory.
const QUEUED_WRITES: usize = 16;

/// A test copy: a real folder `copy-<n>/` in the project's cache folder that [`switch`](Self::switch) fills with
/// the code of a snapshot step.
#[derive(Debug)]
pub struct TestCopy {
    /// The project's cache folder, which holds [`CLOCK_FILE`].
    cache: CacheDir,
    dir: OwnedDir,
    /// The `info/exclude` file of the project's repository: its rules apply in the copy as in the project.
    info_exclude: Option<PathBuf>,
    /// The tree the copy holds, `None` until a switch succeeded: the first switch then checks every file.
    tree: Option<ObjectId>,
    /// Every file of `tree`, by path, as the last switch left it on disk.
    placed: HashMap<String, Placed>,
    /// When the last switch ended, in nanoseconds since the Unix epoch on the file system's clock.
    clean_at: i64,
    /// See [`set_extra_paths`](Self::set_extra_paths).
    extra: HashSet<String>,
    /// See [`pycache_path`](Self::pycache_path).
    pycache: PathBuf,
    /// See [`cargo_target_path`](Self::cargo_target_path).
    cargo_target: PathBuf,
    /// The paths of the copy's folder inside `pycache` (see [`prefix_mirrors`]).
    mirrors: Vec<PathBuf>,
}

/// A file of the tree a copy holds, with the size and mtime the copy gave it.
#[derive(Debug, Clone, Copy)]
struct Placed {
    file: TreeFile,
    size: u64,
    /// `None` if the file system gives no mtime: the file is then hashed by every check.
    mtime_ns: Option<i64>,
}

/// What a [`TestCopy::switch`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Switched {
    /// Files and links written, `restored` included.
    pub written: usize,
    /// Files and links of the previous tree removed because the new tree does not have them.
    pub removed: usize,
    /// Files and links of the previous tree written again because the test run changed or deleted them.
    pub restored: usize,
    /// Files, links and folders that were neither part of the tree nor ignored (created by the test run, or
    /// left by another tree before a first switch), removed.
    pub deleted: usize,
}

impl TestCopy {
    /// Opens test copy number `number` in the project's cache folder `cache`, creating it if it is missing, as a
    /// real folder accessible only by its owner on Unix. A link at its path is refused, and so is a folder other
    /// users can change.
    pub fn open(cache: &CacheDir, number: usize) -> io::Result<Self> {
        let cache = cache.try_clone()?;
        cache.dir().check_still_at_path()?;
        let name = format!("copy-{number}");
        let dir = open_private_dir(&cache.path().join(&name))?;
        let pycache = cache.path().join(PYCACHE_DIR).join(&name);
        let cargo_target = cache.path().join(CARGO_TARGET_DIR).join(name);
        let info_exclude = repo::git_common_dir(cache.project())
            .map(|git_dir| git_dir.join("info").join("exclude"));
        Ok(Self {
            cache,
            info_exclude,
            tree: None,
            placed: HashMap::new(),
            clean_at: 0,
            extra: HashSet::new(),
            mirrors: prefix_mirrors(dir.path()),
            dir,
            pycache,
            cargo_target,
        })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The copy's `PYTHONPYCACHEPREFIX` folder, `pycache/copy-<n>/` in the project's cache folder, which may not
    /// exist yet (see [`env_additions`](super::env_additions)).
    pub fn pycache_path(&self) -> &Path {
        &self.pycache
    }

    /// The copy's `CARGO_TARGET_DIR`, `cargo-target/copy-<n>/` in the project's cache folder, which may not exist
    /// yet (see [`env_additions`](super::env_additions)).
    pub fn cargo_target_path(&self) -> &Path {
        &self.cargo_target
    }

    /// The project's cache folder, which holds the copy.
    pub fn cache(&self) -> &CacheDir {
        &self.cache
    }

    /// Sets the paths of the copy, relative and `/`-separated, that Yalper puts there besides the files of
    /// the tree (a folder of links to the project's dependencies, env files): switches keep them as they
    /// are, a folder with everything inside it, unless the tree has a folder at that path.
    pub fn set_extra_paths(&mut self, paths: impl IntoIterator<Item = String>) {
        self.extra = paths.into_iter().collect();
    }

    /// See [`set_extra_paths`](Self::set_extra_paths).
    pub fn extra_paths(&self) -> &HashSet<String> {
        &self.extra
    }

    /// The tree the copy holds, or `None` until a switch succeeded.
    pub fn tree(&self) -> Option<ObjectId> {
        self.tree
    }

    /// The files of the tree the copy holds, relative and `/`-separated, or `None` until a switch succeeded.
    pub fn tree_files(&self) -> Option<impl Iterator<Item = &str>> {
        self.tree.map(|_| self.placed.keys().map(String::as_str))
    }

    /// Makes the copy hold exactly the tree `to` of `store`, plus the files its ignore rules ignore, writing
    /// only what differs: on the first switch of this `TestCopy`, the files of `to` that are missing in the
    /// copy or differ from it; afterwards, the files that differ between the tree the copy holds and `to`,
    /// and the files of the tree the copy holds that a test run changed or deleted. Then every file and
    /// folder that is neither part of `to`, ignored by its rules, nor an extra path is removed (see
    /// `sweep`).
    ///
    /// Known limit of a first switch on a case-insensitive file system (Windows, macOS): a file or folder
    /// whose name differs from the tree's only in case is taken as the tree's and keeps its old spelling.
    /// Later switches rename files, and folders whose old spelling still holds other files, to the tree's
    /// spelling.
    ///
    /// If the switch fails, the copy is in an unknown state and the next switch checks every file again.
    pub fn switch(&mut self, store: &ShadowStore, to: ObjectId) -> Result<Switched> {
        self.switch_capped(store, to, MAX_CHANGES)
    }

    fn switch_capped(
        &mut self,
        store: &ShadowStore,
        to: ObjectId,
        max_changes: usize,
    ) -> Result<Switched> {
        let from = self.tree.take();
        let mut placed = std::mem::take(&mut self.placed);
        let first = from.is_none();
        let changes =
            store.file_changes(from.unwrap_or_else(|| store.empty_tree()), to, max_changes)?;
        if !changes.complete {
            return Err(io::Error::other(format!(
                "cannot switch the test copy: more than {max_changes} files and folders differ"
            ))
            .into());
        }
        let mut changes = checked(&changes.files)?;

        // Checked right before writing: a test run could have replaced the copy's folder, or the cache folder,
        // which holds the bytecode folder cleaned below.
        self.cache.dir().check_still_at_path()?;
        self.dir.check_still_at_path()?;
        let root = self.dir.path();
        make_owner_writable(root, &fs::symlink_metadata(root)?)?;

        // Files the diff rewrites or removes anyway are not checked.
        let restores = if first {
            Vec::new()
        } else {
            let in_diff = changes.iter().map(|&(path, _)| path).collect();
            changed_by_run(root, &mut placed, &in_diff, self.clean_at)?
        };
        let mut switched = Switched {
            restored: restores.len(),
            ..Switched::default()
        };
        if !restores.is_empty() {
            changes.extend(
                restores
                    .iter()
                    .map(|(path, file)| (path.as_str(), Some(*file))),
            );
            changes.sort_unstable_by(|a, b| a.0.cmp(b.0));
        }

        let mut bytecode = Bytecode::new(&self.pycache, &self.mirrors);
        let mut kept = HashMap::new();
        for &(path, new) in &changes {
            if new.is_none()
                && remove(root, path, &mut kept, &mut bytecode)
                    .map_err(|error| in_copy(error, path))?
            {
                switched.removed += 1;
            }
        }

        // Links are written last, one by one, after every parent folder of every file has been checked and the
        // files written: so no link the tree holds can stand in a parent folder of a file being written, even
        // in a crafted tree that has both `a` as a link and `a/b`.
        let mut ready = HashSet::new();
        let (mut files, mut links) = (Vec::new(), Vec::new());
        for &(path, new) in &changes {
            let Some(new) = new else { continue };
            if new.kind == FileKind::Symlink {
                links.push((path, new));
                continue;
            }
            if prepare(root, path, new, first, &kept, &mut ready, &mut bytecode)
                .map_err(|error| in_copy(error, path))?
            {
                files.push((path, new));
            }
        }
        write_files(store, root, &files)?;
        switched.written += files.len();

        ready.clear();
        for (path, new) in links {
            if prepare(root, path, new, first, &kept, &mut ready, &mut bytecode)
                .map_err(|error| in_copy(error, path))?
            {
                let target = read(store, path, new)?;
                write_link(&native(root, path), &target).map_err(|error| in_copy(error, path))?;
                switched.written += 1;
            }
        }

        // On a first switch every file of `to` is listed, written or found as it should be; on a later one,
        // every listed file was written.
        for &(path, new) in &changes {
            match new {
                Some(file) => {
                    let metadata = fs::symlink_metadata(native(root, path))
                        .map_err(|error| in_copy(error, path))?;
                    placed.insert(path.to_owned(), Placed::new(file, &metadata));
                }
                None => {
                    placed.remove(path);
                }
            }
        }
        switched.deleted = sweep(root, &placed, &self.extra, self.info_exclude.as_deref())?;

        // The local clock if the file system's cannot be read.
        self.clean_at =
            switch_clock(self.cache.dir()).unwrap_or_else(|_| nanos_since_epoch(SystemTime::now()));
        self.placed = placed;
        self.tree = Some(to);
        Ok(switched)
    }
}

impl Placed {
    fn new(file: TreeFile, metadata: &fs::Metadata) -> Self {
        Self {
            file,
            size: metadata.len(),
            mtime_ns: metadata.modified().ok().map(nanos_since_epoch),
        }
    }
}

/// The files of `placed` (the tree the copy at `root` holds) that a test run changed or deleted, except those
/// in `skip`, with what they should hold. A file counts as changed when it is missing, when a
/// folder above it is not a real folder, or when it is not the kind of entry the copy writes for it.
///
/// A file whose size or mtime differs from `placed`, or whose mtime is so close to `clean_at` (the end of
/// the previous switch) that a change within the same timestamp tick would have kept it, is hashed: if its
/// content is still the tree's, its new size and mtime are kept in `placed` and it is not written again.
fn changed_by_run(
    root: &Path,
    placed: &mut HashMap<String, Placed>,
    skip: &HashSet<&str>,
    clean_at: i64,
) -> io::Result<Vec<(String, TreeFile)>> {
    let mut real_dirs: HashMap<String, bool> = HashMap::new();
    let mut changed = Vec::new();
    for (path, entry) in placed.iter_mut() {
        if skip.contains(path.as_str()) {
            continue;
        }
        let mut in_real_dirs = true;
        for parent in parents(path) {
            let real = match real_dirs.get(parent) {
                Some(&real) => real,
                None => {
                    let real = is_real_dir(&native(root, parent))?;
                    real_dirs.insert(parent.to_owned(), real);
                    real
                }
            };
            if !real {
                in_real_dirs = false;
                break;
            }
        }
        if !in_real_dirs || !still_placed(&native(root, path), entry, clean_at)? {
            changed.push((path.clone(), entry.file));
        }
    }
    Ok(changed)
}

/// Whether the entry at `path` still holds the file of `placed` (see [`changed_by_run`]), updating the size
/// and mtime of `placed` when the file had to be hashed.
fn still_placed(path: &Path, placed: &mut Placed, clean_at: i64) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let now = Placed::new(placed.file, &metadata);
    if !is_written_as(&metadata, placed.file.kind) {
        return Ok(false);
    }
    if let Some(mtime_ns) = now.mtime_ns
        && now.size == placed.size
        && placed.mtime_ns == Some(mtime_ns)
        && !is_racy(mtime_ns, clean_at)
    {
        return Ok(true);
    }
    if !holds(path, placed.file)? {
        return Ok(false);
    }
    *placed = now;
    Ok(true)
}

/// Removes from the copy at `root` every file and folder that is neither a file of the tree (`placed`), a
/// folder holding one, one of the `extra` paths, nor ignored: what a test run created, or what another tree
/// left before a first switch. Returns how many entries were removed.
///
/// "Ignored" means what recording means, so exactly the files that are never in snapshots: the rules of the
/// tree's `.gitignore` files (read from the copy, which the switch just made hold them), the project's
/// `info/exclude` (`info_exclude`) and the user's global excludes file. A `.gitignore` that is not a file of
/// the tree, such as one a test created, has no effect. Ignored folders are not entered; a folder that is
/// neither part of the tree nor ignored is entered, and removed once empty, so ignored files inside it stay
/// as a checkout would leave them. Extra paths are kept wherever they are, unless the tree now has a folder
/// there, which is then cleaned like the tree's other folders. Links are never followed: a link is removed
/// itself, never what it points to, and one that is neither a file of the tree nor an extra path is removed
/// even where it is ignored.
fn sweep(
    root: &Path,
    placed: &HashMap<String, Placed>,
    extra: &HashSet<String>,
    info_exclude: Option<&Path>,
) -> Result<usize> {
    let dirs: HashSet<&str> = placed.keys().flat_map(|path| parents(path)).collect();
    // On a case-insensitive file system, a first switch keeps a name whose case differs from the tree's
    // (see `TestCopy::switch`): it is found through its lowercase spelling.
    let lowercase: HashMap<String, &str> = placed
        .keys()
        .map(String::as_str)
        .chain(dirs.iter().copied())
        .map(|path| (path.to_lowercase(), path))
        .collect();
    let has_rules = |dir: &str| {
        let file = join(dir, ".gitignore");
        // On Windows a link of the tree is a plain file holding its target, not rules.
        placed
            .get(&file)
            .is_some_and(|placed| placed.file.kind != FileKind::Symlink)
    };
    let excludes = Excludes::with_info_exclude(root, info_exclude);
    let over_budget = || {
        io::Error::other(format!(
            "cannot clean the test copy: {}",
            over_budget_reason()
        ))
    };

    let mut deleted = 0;
    // Folders still to list: path on disk, path relative to the copy (`None` if not UTF-8; for a folder of
    // the tree, its spelling in the tree), and whether it is a folder of the tree. The others are folders
    // that are neither part of the tree nor ignored.
    let mut todo = vec![(root.to_path_buf(), Some(String::new()), true)];
    let mut created = Vec::new();
    excludes.add_dir(root, has_rules(""));
    while let Some((dir, dir_path, in_tree)) = todo.pop() {
        if excludes.exceeded() {
            return Err(over_budget().into());
        }
        // A test can take the write permission away from a folder.
        make_owner_writable(&dir, &fs::symlink_metadata(&dir)?)?;
        let entries = fs::read_dir(&dir)?
            .map(|entry| {
                let entry = entry?;
                // The type of the entry itself: a link is not followed.
                Ok((entry.file_name(), entry.file_type()?))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let names: HashSet<&OsStr> = entries.iter().map(|(name, _)| name.as_os_str()).collect();
        for (name, file_type) in &entries {
            let full = dir.join(name);
            let is_dir = file_type.is_dir();
            let path = dir_path
                .as_deref()
                .zip(name.to_str())
                .map(|(dir_path, name)| join(dir_path, name));
            // An extra path where the tree now has a folder belongs to the tree: it is cleaned like the
            // tree's other folders.
            if let Some(path) = &path
                && extra.contains(path)
                && !(in_tree && is_dir && dirs.contains(path.as_str()))
            {
                continue;
            }
            if in_tree {
                let tree_path = path.clone().and_then(|path| {
                    if placed.contains_key(&path) || dirs.contains(path.as_str()) {
                        return Some(path);
                    }
                    let &spelled = lowercase.get(&path.to_lowercase())?;
                    // Both spellings in one folder: a case-sensitive file system, where this is another
                    // entry.
                    let tree_name = spelled.rsplit('/').next().unwrap_or(spelled);
                    (!names.contains(OsStr::new(tree_name))).then(|| spelled.to_owned())
                });
                match tree_path {
                    Some(path) if is_dir && dirs.contains(path.as_str()) => {
                        excludes.add_dir(&full, has_rules(&path));
                        todo.push((full, Some(path), true));
                        continue;
                    }
                    Some(path) if !is_dir && placed.contains_key(&path) => continue,
                    _ => {}
                }
            }
            // Yalper itself only puts links of the tree and extra paths in the copy. Any other link could lead
            // a later run outside the copy, so it is removed even where it is ignored.
            if !file_type.is_symlink() && excludes.is_ignored(&full, is_dir) {
                continue;
            }
            if is_dir {
                // Entered, so the ignored files inside it are kept, as a checkout keeps them; removed at the
                // end if nothing is left in it.
                excludes.add_dir(&full, false);
                created.push(full.clone());
                todo.push((full, path, false));
                continue;
            }
            match fs::symlink_metadata(&full) {
                Ok(metadata) => {
                    remove_entry(&full, &metadata)
                        .map_err(|error| in_copy_at(error, root, &full))?;
                    deleted += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    // Inner folders first.
    created.sort_unstable();
    for dir in created.iter().rev() {
        if fs::remove_dir(dir).is_ok() {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Adds the path of `full`, an entry of the copy at `root`, to an error.
fn in_copy_at(error: io::Error, root: &Path, full: &Path) -> io::Error {
    let path = full.strip_prefix(root).unwrap_or(full);
    in_copy(error, &path.to_string_lossy().replace('\\', "/"))
}

/// The path `name` inside the folder `dir` of the tree (`""` for the root).
pub(super) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Creates the folder `path` if it is missing (on Unix with mode 0700) and opens it, refusing a link and, on
/// Unix, a folder its group or other users can write to.
pub(super) fn open_private_dir(path: &Path) -> io::Result<OwnedDir> {
    #[cfg_attr(windows, allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot create {}: {error}", path.display()),
            ));
        }
        _ => {}
    }
    let dir = OwnedDir::open(path)?;
    if dir.is_writable_by_others()? {
        return Err(io::Error::other(format!(
            "{} can be changed by other users (it is group or world writable)",
            path.display()
        )));
    }
    Ok(dir)
}

/// The paths of `changes` with the file each should hold (`None`: removed), every path checked for this
/// system before anything is written.
fn checked(changes: &[FileChange]) -> Result<Vec<(&str, Option<TreeFile>)>> {
    changes
        .iter()
        .map(|change| {
            let path = std::str::from_utf8(&change.path).map_err(|_| Error::InvalidPath {
                path: String::from_utf8_lossy(&change.path).into_owned(),
                reason: "not valid UTF-8".to_owned(),
            })?;
            let kind = change
                .new
                .or(change.old)
                .map_or(FileKind::Regular, |file| file.kind);
            validate_path(path, kind)?;
            Ok((path, change.new))
        })
        .collect()
}

/// `path` of the copy at `root`, from its `/`-separated form.
pub(super) fn native(root: &Path, path: &str) -> PathBuf {
    let mut native = root.to_path_buf();
    native.extend(path.split('/'));
    native
}

/// The folders above `path`, outermost first: `a`, `a/b` for `a/b/c`.
pub(super) fn parents(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(end, _)| &path[..end])
}

/// Adds the copy's path to an error.
fn in_copy(error: io::Error, path: &str) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("cannot update {path} in the test copy: {error}"),
    )
}

/// Removes the file or link at `path`, its cached bytecode if it is a Python file, and the folders this leaves
/// empty. Returns whether there was something to remove. If a parent is not a real folder, the file is not in
/// the copy and nothing is removed; a folder at `path` is left (it is not a file of the tree).
///
/// The folders that still hold something (files the tree does not have) are added to `kept`, keyed by their
/// lowercase path, for [`fix_case`].
fn remove(
    root: &Path,
    path: &str,
    kept: &mut HashMap<String, String>,
    bytecode: &mut Bytecode,
) -> io::Result<bool> {
    for parent in parents(path) {
        if !is_real_dir(&native(root, parent))? {
            return Ok(false);
        }
    }
    let full = native(root, path);
    let removed = match fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_dir() => false,
        Ok(metadata) => {
            remove_entry(&full, &metadata)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if path.ends_with(".py") {
        bytecode.remove(root, path)?;
    }
    let mut left = parents(path).collect::<Vec<_>>();
    while let Some(parent) = left.last() {
        // Fails at the first folder that still holds something, so it and the folders above it stay.
        if fs::remove_dir(native(root, parent)).is_err() {
            break;
        }
        left.pop();
    }
    for parent in left {
        kept.insert(parent.to_lowercase(), parent.to_owned());
    }
    Ok(removed)
}

/// Renames the folder `kept` of the copy, kept by [`remove`], to `wanted` when the two paths differ only in
/// case and the file system takes them for the same folder (case-insensitive, as on Windows and macOS): a
/// case-only rename of a folder that still holds files the tree does not have. The folders above `wanted`
/// already have their new names.
fn fix_case(root: &Path, kept: &str, wanted: &str) -> io::Result<()> {
    let name = |path: &'_ str| -> String { path.rsplit('/').next().unwrap_or(path).to_owned() };
    let (old, new) = (name(kept), name(wanted));
    let parent = match wanted.rsplit_once('/') {
        Some((parent, _)) => native(root, parent),
        None => root.to_path_buf(),
    };
    let mut has_old = false;
    for entry in fs::read_dir(&parent)? {
        let entry = entry?.file_name();
        if entry == new.as_str() {
            // Both spellings exist, so they are two folders (a case-sensitive file system).
            return Ok(());
        }
        has_old |= entry == old.as_str();
    }
    if has_old {
        fs::rename(parent.join(old), parent.join(new))?;
    }
    Ok(())
}

/// Gets `path` ready to receive `file`: every parent made a real folder (`ready` holds those already checked
/// in this switch) with the case of the tree (see [`fix_case`]), and, on a first switch, whether the copy
/// already holds `file` there. Returns whether `file` must be written. Removes the cached bytecode of a
/// Python file that will be written.
fn prepare(
    root: &Path,
    path: &str,
    file: TreeFile,
    first: bool,
    kept: &HashMap<String, String>,
    ready: &mut HashSet<String>,
    bytecode: &mut Bytecode,
) -> io::Result<bool> {
    for parent in parents(path) {
        if ready.contains(parent) {
            continue;
        }
        let dir = native(root, parent);
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => {
                if let Some(old) = kept.get(&parent.to_lowercase())
                    && old != parent
                {
                    fix_case(root, old, parent)?;
                }
                make_owner_writable(&dir, &metadata)?;
            }
            Ok(metadata) => {
                remove_entry(&dir, &metadata)?;
                fs::create_dir(&dir)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&dir)?,
            Err(error) => return Err(error),
        }
        ready.insert(parent.to_owned());
    }
    let full = native(root, path);
    if first && holds(&full, file)? {
        return Ok(false);
    }
    if path.ends_with(".py") {
        bytecode.remove(root, path)?;
    }
    Ok(true)
}

/// Whether `path` is a real folder, not a link. On Unix a folder that a test made read-only is made
/// writable again, so its entries can be replaced.
fn is_real_dir(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            make_owner_writable(path, &metadata)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
pub(super) fn make_owner_writable(dir: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700))?;
    }
    Ok(())
}

/// Windows lets a folder's entries be replaced whatever the folder's attributes.
#[cfg(windows)]
pub(super) fn make_owner_writable(_dir: &Path, _metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

/// Whether the entry at `path` already holds `file`: the same kind and the same content, hashed like a blob.
fn holds(path: &Path, file: TreeFile) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !is_written_as(&metadata, file.kind) {
        return Ok(false);
    }
    #[cfg(unix)]
    if file.kind == FileKind::Symlink {
        return is_blob(&link_target(path)?, file.blob);
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Ok(false);
    }
    let opened = match safe_fs::open_regular_file(path) {
        Ok(opened) => opened,
        // A file a test made unreadable is replaced, like any file that differs.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    opened.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    is_blob(&bytes, file.blob)
}

/// Whether `bytes` are the content of the blob `id`.
fn is_blob(bytes: &[u8], id: ObjectId) -> io::Result<bool> {
    let actual = gix::objs::compute_hash(id.kind(), gix::objs::Kind::Blob, bytes)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(actual == id)
}

/// Whether the entry with `metadata` (not followed) is the kind of entry the copy writes for a file of kind
/// `kind`: a link for a link of the tree on Unix, otherwise a regular file (see [`plain_as`]). On Windows, a
/// link of the tree is a plain file holding its target.
pub(super) fn is_written_as(metadata: &fs::Metadata, kind: FileKind) -> bool {
    if cfg!(unix) && kind == FileKind::Symlink {
        return metadata.is_symlink();
    }
    metadata.is_file() && plain_as(metadata, kind)
}

/// Whether the regular file with `metadata` is as the copy writes a file of kind `kind`: on Unix, mode 0644
/// or 0755 exactly, so a permission a test changed is noticed, and no other hard link (writing to the copy's
/// file must not change another file).
#[cfg(unix)]
fn plain_as(metadata: &fs::Metadata, kind: FileKind) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mode = if kind == FileKind::Executable {
        0o755
    } else {
        0o644
    };
    metadata.nlink() == 1 && metadata.mode() & 0o777 == mode
}

/// Windows has no executable bit: a file is as the copy writes it unless it has the read-only attribute. The
/// standard library does not expose the hard link count there (it is unstable), so a file with a second hard
/// link whose content matches is kept by a first switch, and a test writing to it would change the other file
/// too. Only a local process can create a hard link (a clone cannot), and later switches always replace a
/// file instead of writing into it.
#[cfg(windows)]
fn plain_as(metadata: &fs::Metadata, _kind: FileKind) -> bool {
    !metadata.permissions().readonly()
}

#[cfg(unix)]
fn link_target(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStringExt;
    Ok(fs::read_link(path)?.into_os_string().into_vec())
}

/// The content of the blob of `file`, the file at `path`. A blob over [`MAX_FILE_BYTES`] is refused:
/// snapshots never hold one, so only a crafted store has it, and refusing it bounds the memory of the files
/// queued for the writer threads. Reading one blob is itself bounded by the store's allocation limit.
fn read(store: &ShadowStore, path: &str, file: TreeFile) -> Result<Vec<u8>> {
    let bytes = store
        .read_blob(file.blob)?
        .ok_or(Error::MissingObject(file.blob))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other(format!(
            "cannot write {path} in the test copy: its snapshot is larger than {} MiB, which Yalper \
             never stores",
            MAX_FILE_BYTES / (1024 * 1024)
        ))
        .into());
    }
    Ok(bytes)
}

/// Writes `files` into the copy at `root`, whose parent folders [`prepare`] made ready.
fn write_files(store: &ShadowStore, root: &Path, files: &[(&str, TreeFile)]) -> Result<()> {
    if files.len() < PARALLEL_WRITES_FROM {
        return files.iter().try_for_each(|&(path, file)| {
            let bytes = read(store, path, file)?;
            write_file(&native(root, path), file.kind, &bytes)
                .map_err(|error| in_copy(error, path))?;
            Ok(())
        });
    }

    // The store is read on this thread only (it is not shared between threads); the writers create the files.
    // A writer that fails keeps taking files without writing them, and nothing in it panics, so this thread
    // never waits forever on a full queue.
    type Job = (String, PathBuf, FileKind, Vec<u8>);
    let (sender, receiver) = mpsc::sync_channel::<Job>(QUEUED_WRITES);
    let receiver = Mutex::new(receiver);
    thread::scope(|scope| {
        let writer = || -> io::Result<()> {
            let mut result = Ok(());
            loop {
                // The lock is released before the file is written.
                let job = receiver
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv();
                let Ok((path, full, kind, bytes)) = job else {
                    return result;
                };
                if result.is_ok() {
                    result = write_file(&full, kind, &bytes).map_err(|error| in_copy(error, &path));
                }
            }
        };
        let writers = (0..WRITE_THREADS)
            .map(|_| thread::Builder::new().spawn_scoped(scope, writer))
            .collect::<io::Result<Vec<_>>>()?;
        let mut result = Ok(());
        for &(path, file) in files {
            match read(store, path, file) {
                Ok(bytes) => {
                    // Cannot fail: the receiver lives until the end of this function.
                    let _ = sender.send((path.to_owned(), native(root, path), file.kind, bytes));
                }
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        // Ends the writers' loops.
        drop(sender);
        for writer in writers {
            let written = writer
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("writing a file panicked")));
            if result.is_ok() {
                result = written.map_err(Error::from);
            }
        }
        result
    })
}

/// Removes whatever is at `path`, then creates the file anew with `bytes` (on Unix with mode 0644 or 0755).
/// Creating a new file never follows a link and never writes into a file with another hard link.
pub(super) fn write_file(path: &Path, kind: FileKind, bytes: &[u8]) -> io::Result<()> {
    clear(path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(path)?;
    // Set on the open file, so the mode is exact whatever the umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if kind == FileKind::Executable {
            0o755
        } else {
            0o644
        };
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    let _ = kind;
    file.write_all(bytes)
}

/// Replaces whatever is at `path` with a link to `target` on Unix, and with a plain file holding `target` on
/// Windows.
fn write_link(path: &Path, target: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        clear(path)?;
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), path)
    }
    #[cfg(windows)]
    write_file(path, FileKind::Regular, target)
}

/// The time on the clock of the file system that holds the project's cache folder `cache`, in nanoseconds
/// since the Unix epoch: the mtime of [`CLOCK_FILE`] right after it is written. Whatever is at its name is
/// removed first and the file is created exclusively, never opened in place: on Windows a hard link a test put
/// there cannot be told from a plain file, and writing through it would change the file it links to.
fn switch_clock(cache: &OwnedDir) -> io::Result<i64> {
    cache.check_still_at_path()?;
    let path = cache.path().join(CLOCK_FILE);
    clear(&path)?;
    let mut options = fs::File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(
        &mut options,
        safe_fs::windows::FILE_FLAG_OPEN_REPARSE_POINT,
    );
    let mut file = options.open(&path)?;
    file.write_all(b"x")?;
    Ok(nanos_since_epoch(file.metadata()?.modified()?))
}

/// Removes whatever is at `path`, if anything.
fn clear(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => remove_entry(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes the file, link or folder at `path`, whose metadata (not followed) is `metadata`. A link is
/// removed itself, never what it points to, and so are the links inside a removed folder.
pub(super) fn remove_entry(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.is_dir() {
        return fs::remove_dir_all(path).or_else(|_| remove_dir_forcing(path));
    }
    if metadata.is_symlink() {
        // On Windows, a link to a folder and a junction are removed as folders.
        return fs::remove_file(path).or_else(|error| {
            if cfg!(windows) {
                fs::remove_dir(path)
            } else {
                Err(error)
            }
        });
    }
    // On Windows the standard library removes a read-only file without clearing its attribute (since Rust
    // 1.86). The attribute is never cleared here: it belongs to the file, not to this name, so with a second
    // hard link outside the copy (a project file) clearing it would change that file too. Known limit: on file
    // systems without POSIX delete semantics (FAT32, exFAT), a file a test made read-only cannot be removed,
    // and the switch fails.
    fs::remove_file(path)
}

/// Removes the folder `dir` and everything in it one entry at a time, making each folder writable by its
/// owner first and clearing read-only attributes: for a folder `remove_dir_all` could not remove, such as one
/// a test left without write permission. Links are removed, never followed. A work list instead of recursion,
/// so a deeply nested folder cannot exhaust the stack.
fn remove_dir_forcing(dir: &Path) -> io::Result<()> {
    let mut todo = vec![dir.to_path_buf()];
    // In the order they were listed: every folder comes after the folder holding it.
    let mut listed = Vec::new();
    while let Some(dir) = todo.pop() {
        make_owner_writable(&dir, &fs::symlink_metadata(&dir)?)?;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                todo.push(path);
            } else {
                remove_entry(&path, &metadata)?;
            }
        }
        listed.push(dir);
    }
    listed.iter().rev().try_for_each(fs::remove_dir)
}

/// Removes the cached bytecode of the Python files a switch writes or removes.
///
/// Python takes its bytecode as current while the source keeps its size and its mtime in whole seconds, so a
/// same-size change written in the same second as the previous version would otherwise run the old code.
/// Python keeps the bytecode in a `__pycache__` folder next to each file or, when a test run has
/// `PYTHONPYCACHEPREFIX` (see [`env_additions`](super::env_additions)), in the prefix folder, under the path of
/// the source's folder (see [`prefix_mirrors`]). A test run can write in the prefix folder, so only real
/// folders are entered there, never a link.
struct Bytecode {
    /// The copy's folders inside the prefix folder that are real folders reached through real folders, checked
    /// once per switch.
    roots: Vec<PathBuf>,
    /// Whether each folder below them is a real folder, checked once per switch.
    real: HashMap<PathBuf, bool>,
}

impl Bytecode {
    /// For a switch of the copy whose prefix folder is `pycache`, with `mirrors` (see [`prefix_mirrors`]).
    /// The folder above `pycache` (`pycache/` in the project's cache folder) must be a real folder too: a test
    /// run can put a link there.
    fn new(pycache: &Path, mirrors: &[PathBuf]) -> Self {
        let parent_is_real = pycache.parent().is_some_and(is_real_dir_no_follow);
        let roots = mirrors
            .iter()
            .filter_map(|mirror| {
                let mut dir = pycache.to_owned();
                if !parent_is_real || !is_real_dir_no_follow(&dir) {
                    return None;
                }
                for component in mirror.components() {
                    dir.push(component);
                    if !is_real_dir_no_follow(&dir) {
                        return None;
                    }
                }
                Some(dir)
            })
            .collect();
        Self {
            roots,
            real: HashMap::new(),
        }
    }

    /// Removes the bytecode of the Python file `path` of the copy at `root`, whose parent folders are real
    /// folders: the `__pycache__/<name>.*.pyc` files next to it (then `__pycache__` if it is left empty), and
    /// the `<name>.*.pyc` files of its folder in the prefix folder.
    fn remove(&mut self, root: &Path, path: &str) -> io::Result<()> {
        let full = native(root, path);
        let (Some(dir), Some(name)) = (full.parent(), full.file_stem().and_then(OsStr::to_str))
        else {
            return Ok(());
        };
        let cache = dir.join("__pycache__");
        if is_real_dir(&cache)? {
            remove_pyc(&cache, name)?;
            // Fails if something is left, which is fine.
            let _ = fs::remove_dir(&cache);
        }

        let folder = path.rsplit_once('/').map_or("", |(folder, _)| folder);
        for mirror_root in &self.roots {
            let mut dir = mirror_root.clone();
            let mut real = true;
            for component in folder.split('/').filter(|component| !component.is_empty()) {
                dir.push(component);
                real = match self.real.get(&dir) {
                    Some(&real) => real,
                    None => {
                        let real = is_real_dir(&dir)?;
                        self.real.insert(dir.clone(), real);
                        real
                    }
                };
                if !real {
                    break;
                }
            }
            if real {
                remove_pyc(&dir, name)?;
            }
        }
        Ok(())
    }
}

/// Removes the files `<name>.*.pyc` of the folder `dir`. A link is removed itself, never followed.
fn remove_pyc(dir: &Path, name: &str) -> io::Result<()> {
    let prefix = format!("{name}.");
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let is_bytecode = entry
            .file_name()
            .to_str()
            .is_some_and(|file| file.starts_with(&prefix) && file.ends_with(".pyc"));
        if is_bytecode {
            let metadata = entry.metadata()?;
            if !metadata.is_dir() {
                remove_entry(&entry.path(), &metadata)?;
            }
        }
    }
    Ok(())
}

/// The paths, relative to a `PYTHONPYCACHEPREFIX` folder, of the folder where Python puts the bytecode of the
/// files at the top of the copy at `copy`. Python's `importlib` takes the absolute path of a source's folder,
/// drops its drive and root, and appends what is left to the prefix: `C:\p\copy-0\pkg` gives
/// `p\copy-0\pkg`, and a network path `\\server\share\p` gives `server\share\p`. Two spellings of the copy's
/// path are kept: the one Yalper gives tests, and the canonical one, which a test's current folder may resolve
/// to (on Windows, a short `RUNNER~1` name becomes the long one).
fn prefix_mirrors(copy: &Path) -> Vec<PathBuf> {
    let mut mirrors: Vec<PathBuf> = Vec::new();
    let spellings = [std::path::absolute(copy).ok(), fs::canonicalize(copy).ok()];
    for path in spellings.into_iter().flatten() {
        let Some(mirror) = mirror(&normalize(&plain(&path))) else {
            continue;
        };
        let same = |known: &PathBuf| {
            if CASE_INSENSITIVE {
                known.to_string_lossy().to_lowercase() == mirror.to_string_lossy().to_lowercase()
            } else {
                *known == mirror
            }
        };
        if !mirrors.iter().any(same) {
            mirrors.push(mirror);
        }
    }
    mirrors
}

/// `path`, absolute, without its drive or root (see [`prefix_mirrors`]). `None` for a device path, which a
/// copy never has.
fn mirror(path: &Path) -> Option<PathBuf> {
    let mut mirror = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {}
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    mirror.push(server);
                    mirror.push(share);
                }
                _ => return None,
            },
            Component::RootDir => {}
            Component::Normal(name) => mirror.push(name),
            Component::CurDir | Component::ParentDir => return None,
        }
    }
    Some(mirror)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::process::Command;
    use std::time::{Duration, UNIX_EPOCH};

    use gix::objs::tree::EntryKind;

    use crate::bisect::testing::{Project, tool};

    use FileKind::{Executable, Regular, Symlink};

    /// Every file and link under `dir` by `/`-separated path, with its kind and content (a link: its target).
    fn contents(dir: &Path) -> BTreeMap<String, (FileKind, Vec<u8>)> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, (FileKind, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let path = format!("{prefix}{name}");
                let metadata = fs::symlink_metadata(entry.path()).unwrap();
                if metadata.is_dir() {
                    walk(&entry.path(), &format!("{path}/"), out);
                } else if metadata.is_symlink() {
                    let target = fs::read_link(entry.path()).unwrap();
                    let target = target.to_string_lossy().into_owned().into_bytes();
                    out.insert(path, (Symlink, target));
                } else {
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.permissions().mode() & 0o100 != 0
                    };
                    #[cfg(windows)]
                    let executable = false;
                    let kind = if executable { Executable } else { Regular };
                    out.insert(path, (kind, fs::read(entry.path()).unwrap()));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, "", &mut out);
        out
    }

    /// What [`contents`] should find for a copy of `files`: on Windows, every file is a plain file.
    fn expected(files: &[(&str, FileKind, &str)]) -> BTreeMap<String, (FileKind, Vec<u8>)> {
        files
            .iter()
            .map(|&(path, kind, content)| {
                let kind = if cfg!(windows) { Regular } else { kind };
                (path.to_owned(), (kind, content.as_bytes().to_vec()))
            })
            .collect()
    }

    fn assert_holds(copy: &TestCopy, files: &[(&str, FileKind, &str)]) {
        assert_eq!(contents(copy.path()), expected(files));
    }

    /// Borrowed form of generated files, for [`Project::typed_tree`] and [`assert_holds`].
    fn view(files: &[(String, FileKind, String)]) -> Vec<(&str, FileKind, &str)> {
        files
            .iter()
            .map(|(path, kind, content)| (path.as_str(), *kind, content.as_str()))
            .collect()
    }

    /// Creates a link at `link` to the folder `target`: a symlink on Unix, a junction on Windows.
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let status = Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }
    }

    #[test]
    fn copies_are_real_private_folders_in_the_cache_folder() {
        let project = Project::new();
        let copy = TestCopy::open(&project.cache, 3).unwrap();
        let cache = project.cache.path();
        assert_eq!(
            fs::canonicalize(copy.path()).unwrap(),
            fs::canonicalize(cache.join("copy-3")).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(copy.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // Opening it again reuses it.
        TestCopy::open(&project.cache, 3).unwrap();
    }

    #[test]
    fn a_link_at_a_copy_folder_is_refused() {
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        link_dir(outside.path(), &project.cache.path().join("copy-0"));
        assert!(TestCopy::open(&project.cache, 0).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn switching_there_and_back_restores_contents_and_modes() {
        let project = Project::new();
        let a = [
            ("README.md", Regular, "# A\n"),
            ("run.sh", Executable, "#!/bin/sh\necho a\n"),
            ("src/lib.rs", Regular, "pub fn a() {}\r\n"),
            ("src/link", Symlink, "lib.rs"),
            ("tool", Regular, "same content, other mode\n"),
        ];
        let b = [
            ("README.md", Regular, "# B, longer\n"),
            ("run.sh", Regular, "#!/bin/sh\necho a\n"),
            ("src/lib.rs", Regular, "pub fn a() {}\r\n"),
            ("src/link", Symlink, "../README.md"),
            ("src/new.rs", Regular, "\0binary\u{7f}"),
            ("tool", Executable, "same content, other mode\n"),
        ];
        let (tree_a, tree_b) = (project.typed_tree(&a), project.typed_tree(&b));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_a).unwrap();
        assert_holds(&copy, &a);
        assert_exact_modes(&copy, &a);
        copy.switch(&project.store, tree_b).unwrap();
        assert_holds(&copy, &b);
        assert_exact_modes(&copy, &b);
        copy.switch(&project.store, tree_a).unwrap();
        assert_holds(&copy, &a);
        assert_exact_modes(&copy, &a);
    }

    /// On Unix, the files of the copy have mode 0644 or 0755 exactly, whatever the umask.
    fn assert_exact_modes(copy: &TestCopy, files: &[(&str, FileKind, &str)]) {
        #[cfg(unix)]
        for &(path, kind, _) in files {
            use std::os::unix::fs::PermissionsExt;
            let expected = match kind {
                Executable => 0o755,
                Regular => 0o644,
                Symlink => continue,
            };
            let metadata = fs::symlink_metadata(copy.path().join(path)).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, expected, "{path}");
        }
        #[cfg(windows)]
        let _ = (copy, files);
    }

    #[test]
    fn a_symlink_of_the_tree_is_a_link_on_unix_and_a_file_with_its_target_on_windows() {
        let project = Project::new();
        let tree = project.typed_tree(&[
            ("target.txt", Regular, "x"),
            ("link", Symlink, "target.txt"),
        ]);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        let link = copy.path().join("link");
        let metadata = fs::symlink_metadata(&link).unwrap();
        if cfg!(unix) {
            assert!(metadata.is_symlink());
            assert_eq!(fs::read_link(&link).unwrap(), Path::new("target.txt"));
        } else {
            assert!(metadata.is_file());
            assert_eq!(fs::read(&link).unwrap(), b"target.txt");
        }
        // A first switch of a copy that holds the link writes nothing.
        let mut again = project.copy();
        assert_eq!(again.switch(&project.store, tree).unwrap().written, 0);
    }

    #[test]
    fn a_nested_folder_is_created_and_removed() {
        let project = Project::new();
        let flat = [("a.txt", Regular, "a")];
        let nested = [
            ("a.txt", Regular, "a"),
            ("one/two/three/deep.txt", Regular, "deep"),
        ];
        let (tree_flat, tree_nested) = (project.typed_tree(&flat), project.typed_tree(&nested));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_flat).unwrap();
        let switched = copy.switch(&project.store, tree_nested).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 1,
                removed: 0,
                ..Switched::default()
            }
        );
        assert_holds(&copy, &nested);
        let switched = copy.switch(&project.store, tree_flat).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 0,
                removed: 1,
                ..Switched::default()
            }
        );
        assert_holds(&copy, &flat);
        assert!(!copy.path().join("one").exists());
    }

    #[test]
    fn a_file_replaced_by_a_folder_and_back() {
        let project = Project::new();
        let file = [("x", Regular, "a file"), ("y.txt", Regular, "y")];
        let folder = [
            ("x/inner.txt", Regular, "in a folder"),
            ("y.txt", Regular, "y"),
        ];
        let (tree_file, tree_folder) = (project.typed_tree(&file), project.typed_tree(&folder));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_file).unwrap();
        copy.switch(&project.store, tree_folder).unwrap();
        assert_holds(&copy, &folder);
        copy.switch(&project.store, tree_file).unwrap();
        assert_holds(&copy, &file);
    }

    #[test]
    fn a_folder_left_where_the_tree_has_a_file_is_replaced() {
        let project = Project::new();
        let tree = project.typed_tree(&[("x", Regular, "a file")]);
        let mut copy = project.copy();
        fs::create_dir_all(copy.path().join("x").join("build")).unwrap();
        fs::write(copy.path().join("x").join("build").join("out"), "old").unwrap();
        copy.switch(&project.store, tree).unwrap();
        assert_holds(&copy, &[("x", Regular, "a file")]);
    }

    #[test]
    fn a_case_only_rename_gives_the_new_name() {
        let project = Project::new();
        let old = [("Readme.md", Regular, "same"), ("Src/a.txt", Regular, "a")];
        let new = [("README.md", Regular, "same"), ("src/a.txt", Regular, "a")];
        let (tree_old, tree_new) = (project.typed_tree(&old), project.typed_tree(&new));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_old).unwrap();
        copy.switch(&project.store, tree_new).unwrap();
        // On Windows and macOS both names are the same file: only the new spelling may be left.
        assert_holds(&copy, &new);
        copy.switch(&project.store, tree_old).unwrap();
        assert_holds(&copy, &old);
    }

    #[test]
    fn a_case_only_folder_rename_keeps_files_the_tree_does_not_have() {
        let project = Project::new();
        let old = [
            (".gitignore", Regular, "*.o\n"),
            ("Src/Lib/a.txt", Regular, "a"),
        ];
        let new = [
            (".gitignore", Regular, "*.o\n"),
            ("src/lib/a.txt", Regular, "a"),
        ];
        let (tree_old, tree_new) = (project.typed_tree(&old), project.typed_tree(&new));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_old).unwrap();
        // An ignored build output keeps both folders from being removed.
        fs::write(copy.path().join("Src").join("Lib").join("out.o"), "o").unwrap();
        copy.switch(&project.store, tree_new).unwrap();
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        let src = copy.path().join("src");
        if cfg!(any(windows, target_os = "macos")) {
            // One folder, renamed to the tree's spelling, with both files.
            assert_eq!(names(copy.path()), [".gitignore", "src"]);
            assert_eq!(names(&src), ["lib"]);
            assert_eq!(names(&src.join("lib")), ["a.txt", "out.o"]);
        } else {
            // Two folders on a case-sensitive file system: the old one keeps only the build output.
            assert_eq!(names(copy.path()), [".gitignore", "Src", "src"]);
            assert_eq!(names(&copy.path().join("Src").join("Lib")), ["out.o"]);
            assert_eq!(names(&src.join("lib")), ["a.txt"]);
        }
    }

    #[test]
    fn a_link_planted_where_the_tree_has_a_folder_is_replaced_and_never_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("canary.txt"), "keep me").unwrap();
        let before = contents(canary.path());
        let a = [
            ("sub/canary.txt", Regular, "written in the copy"),
            ("sub/deeper/f.txt", Regular, "f"),
        ];
        let b = [
            ("sub/canary.txt", Regular, "changed in the copy"),
            ("sub/deeper/f.txt", Regular, "f2"),
            ("sub/new.txt", Regular, "new"),
        ];
        let (tree_a, tree_b) = (project.typed_tree(&a), project.typed_tree(&b));

        // On the first switch of a copy.
        let mut copy = project.copy();
        link_dir(canary.path(), &copy.path().join("sub"));
        copy.switch(&project.store, tree_a).unwrap();
        assert_holds(&copy, &a);
        assert_eq!(contents(canary.path()), before);

        // On a later switch.
        fs::remove_dir_all(copy.path().join("sub")).unwrap();
        link_dir(canary.path(), &copy.path().join("sub"));
        copy.switch(&project.store, tree_b).unwrap();
        assert_holds(&copy, &b);
        assert_eq!(contents(canary.path()), before);

        // A removal does not follow it either.
        fs::remove_dir_all(copy.path().join("sub")).unwrap();
        link_dir(canary.path(), &copy.path().join("sub"));
        copy.switch(&project.store, project.typed_tree(&[]))
            .unwrap();
        assert_eq!(contents(canary.path()), before);
        assert!(fs::symlink_metadata(canary.path()).unwrap().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_planted_at_a_file_path_is_replaced_and_its_target_kept() {
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target.txt");
        fs::write(&target, "keep me").unwrap();
        let files = [("file.txt", Regular, "keep me")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        std::os::unix::fs::symlink(&target, copy.path().join("file.txt")).unwrap();
        // Same content behind the link: still replaced by a regular file.
        assert_eq!(copy.switch(&project.store, tree).unwrap().written, 1);
        assert_holds(&copy, &files);
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_in_the_copy_is_not_written_through() {
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        let other = outside.path().join("other.txt");
        fs::write(&other, "old").unwrap();
        let mut copy = project.copy();
        copy.switch(
            &project.store,
            project.typed_tree(&[("f.txt", Regular, "old")]),
        )
        .unwrap();
        fs::remove_file(copy.path().join("f.txt")).unwrap();
        fs::hard_link(&other, copy.path().join("f.txt")).unwrap();
        // Treated as not matching on a first switch, and replaced, not written into, on a later one.
        let mut again = project.copy();
        let tree = project.typed_tree(&[("f.txt", Regular, "old")]);
        assert_eq!(again.switch(&project.store, tree).unwrap().written, 1);
        fs::remove_file(copy.path().join("f.txt")).unwrap();
        fs::hard_link(&other, copy.path().join("f.txt")).unwrap();
        let new = project.typed_tree(&[("f.txt", Regular, "new")]);
        again.switch(&project.store, new).unwrap();
        assert_eq!(fs::read_to_string(&other).unwrap(), "old");
        assert_eq!(
            fs::read_to_string(copy.path().join("f.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn a_tree_with_an_invalid_path_is_refused_before_anything_is_written() {
        let project = Project::new();
        let blob = project.store.write_blob(b"x").unwrap();
        let config = project.crafted_tree(&[("config", EntryKind::Blob, blob)]);
        // `.git` is refused everywhere, and so are `..` and `GIT~1`, NTFS's short name for `.git`.
        let mut bad = vec![
            (".git", EntryKind::Tree, config),
            ("..", EntryKind::Tree, config),
            ("GIT~1", EntryKind::Tree, config),
        ];
        // A name only Windows refuses, as a store written on another system could hold.
        if cfg!(windows) {
            bad.push(("nul.txt", EntryKind::Blob, blob));
        }
        for entry in bad {
            // The bad entry sits in a folder that sorts after a valid file, so the switch must check every
            // path before writing the first file.
            let later = project.crafted_tree(&[entry]);
            let tree = project.crafted_tree(&[
                ("a.txt", EntryKind::Blob, blob),
                ("z", EntryKind::Tree, later),
            ]);
            let mut copy = project.copy();
            let error = copy.switch(&project.store, tree).unwrap_err();
            assert!(matches!(error, Error::InvalidPath { .. }), "{error}");
            assert!(contents(copy.path()).is_empty(), "{}", entry.0);
        }
    }

    #[test]
    fn a_step_writes_exactly_the_files_it_changed() {
        let project = Project::new();
        let base: Vec<(String, FileKind, String)> = (0..100)
            .map(|index| {
                (
                    format!("dir{}/file{index}.txt", index % 7),
                    Regular,
                    format!("{index}"),
                )
            })
            .collect();
        let mut step = base.clone();
        for index in [3, 17, 42, 64, 99] {
            step[index].2.push_str(" changed");
        }
        let (tree_base, tree_step) = (
            project.typed_tree(&view(&base)),
            project.typed_tree(&view(&step)),
        );

        let mut copy = project.copy();
        let switched = copy.switch(&project.store, tree_base).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 100,
                removed: 0,
                ..Switched::default()
            }
        );
        let switched = copy.switch(&project.store, tree_step).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 5,
                removed: 0,
                ..Switched::default()
            }
        );
        assert_holds(&copy, &view(&step));

        // A new bisect reusing the copy: its files already match, so nothing is written.
        let mut reused = project.copy();
        let switched = reused.switch(&project.store, tree_step).unwrap();
        assert_eq!(switched, Switched::default());
        // A file changed behind its back is found and rewritten, and only that one.
        fs::write(copy.path().join("dir0").join("file0.txt"), "1").unwrap();
        let mut reused = project.copy();
        let switched = reused.switch(&project.store, tree_step).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 1,
                removed: 0,
                ..Switched::default()
            }
        );
        assert_holds(&reused, &view(&step));
    }

    #[cfg(unix)]
    #[test]
    fn a_wrong_mode_is_fixed_on_a_first_switch() {
        let project = Project::new();
        let files = [
            ("run.sh", Executable, "#!/bin/sh\n"),
            ("plain", Regular, "x"),
        ];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let run = copy.path().join("run.sh");
        fs::set_permissions(&run, fs::Permissions::from_mode(0o644)).unwrap();
        let mut again = project.copy();
        assert_eq!(again.switch(&project.store, tree).unwrap().written, 1);
        assert_holds(&again, &files);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_replaced_on_a_first_switch() {
        use std::os::unix::fs::PermissionsExt;
        let project = Project::new();
        let files = [("secret.txt", Regular, "x"), ("other.txt", Regular, "y")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        let path = copy.path().join("secret.txt");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&path).is_ok() {
            // Running as root, which reads any file: nothing to test.
            return;
        }
        let mut again = project.copy();
        assert_eq!(again.switch(&project.store, tree).unwrap().written, 1);
        assert_holds(&again, &files);
        assert_exact_modes(&again, &files);
    }

    #[test]
    fn read_only_files_and_folders_are_replaced_and_removed() {
        let project = Project::new();
        let a = [("dir/a.txt", Regular, "a"), ("dir/b.txt", Regular, "b")];
        let b = [("dir/a.txt", Regular, "changed")];
        let (tree_a, tree_b) = (project.typed_tree(&a), project.typed_tree(&b));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_a).unwrap();
        for name in ["a.txt", "b.txt"] {
            let path = copy.path().join("dir").join(name);
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = copy.path().join("dir");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        }
        copy.switch(&project.store, tree_b).unwrap();
        assert_holds(&copy, &b);
    }

    #[test]
    fn a_diff_over_the_limit_is_refused_without_writing() {
        let project = Project::new();
        let tree = project.typed_tree(&[
            ("a", Regular, "a"),
            ("b", Regular, "b"),
            ("c/d", Regular, "d"),
        ]);
        let mut copy = project.copy();
        let error = copy.switch_capped(&project.store, tree, 2).unwrap_err();
        assert!(error.to_string().contains("more than 2 files"), "{error}");
        assert!(contents(copy.path()).is_empty());
        // The next switch starts over and checks every file.
        assert_eq!(copy.switch(&project.store, tree).unwrap().written, 3);
    }

    #[test]
    fn a_missing_blob_fails_the_switch_and_the_next_one_checks_everything() {
        let project = Project::new();
        let missing = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let broken = project.crafted_tree(&[("gone.txt", EntryKind::Blob, missing)]);
        let mut copy = project.copy();
        assert!(matches!(
            copy.switch(&project.store, broken),
            Err(Error::MissingObject(id)) if id == missing
        ));
        let files = [("ok.txt", Regular, "ok")];
        copy.switch(&project.store, project.typed_tree(&files))
            .unwrap();
        assert_holds(&copy, &files);
    }

    #[test]
    fn a_switch_that_fails_after_writing_is_undone_by_the_next_switch() {
        let project = Project::new();
        let a = [("a.txt", Regular, "a"), ("m.txt", Regular, "m")];
        let tree_a = project.typed_tree(&a);
        // B changes `a.txt`, written before `z.txt`, whose blob is missing.
        let missing = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let changed = project.store.write_blob(b"changed").unwrap();
        let m = project.store.write_blob(b"m").unwrap();
        let tree_b = project.crafted_tree(&[
            ("a.txt", EntryKind::Blob, changed),
            ("m.txt", EntryKind::Blob, m),
            ("z.txt", EntryKind::Blob, missing),
        ]);
        let mut copy = project.copy();
        copy.switch(&project.store, tree_a).unwrap();
        assert!(matches!(
            copy.switch(&project.store, tree_b),
            Err(Error::MissingObject(id)) if id == missing
        ));
        assert_eq!(fs::read(copy.path().join("a.txt")).unwrap(), b"changed");
        let switched = copy.switch(&project.store, tree_a).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 1,
                removed: 0,
                ..Switched::default()
            }
        );
        assert_holds(&copy, &a);
    }

    #[test]
    fn a_blob_larger_than_snapshots_hold_is_refused() {
        let project = Project::new();
        let large = vec![b'x'; MAX_FILE_BYTES as usize + 1];
        let blob = project.store.write_blob(&large).unwrap();
        let tree = project.crafted_tree(&[("large.bin", EntryKind::Blob, blob)]);
        let mut copy = project.copy();
        let error = copy.switch(&project.store, tree).unwrap_err();
        assert!(error.to_string().contains("larger than 10 MiB"), "{error}");
        assert!(contents(copy.path()).is_empty());
    }

    #[test]
    fn many_files_are_written_in_parallel_and_match() {
        let project = Project::new();
        let files: Vec<(String, FileKind, String)> = (0..300)
            .map(|index| {
                let kind = if index % 10 == 0 { Executable } else { Regular };
                (format!("d{}/f{index}", index % 13), kind, "x".repeat(index))
            })
            .collect();
        let files = view(&files);
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        assert_eq!(copy.switch(&project.store, tree).unwrap().written, 300);
        assert_holds(&copy, &files);
    }

    #[test]
    fn bytecode_of_a_written_or_removed_python_file_is_removed() {
        let project = Project::new();
        let a = [
            (".gitignore", Regular, "__pycache__/\n"),
            ("pkg/mod.py", Regular, "VALUE = 1\n"),
            ("pkg/other.py", Regular, ""),
        ];
        let b = [
            (".gitignore", Regular, "__pycache__/\n"),
            ("pkg/mod.py", Regular, "VALUE = 2\n"),
            ("pkg/other.py", Regular, ""),
        ];
        let (tree_a, tree_b) = (project.typed_tree(&a), project.typed_tree(&b));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_a).unwrap();
        let cache = copy.path().join("pkg").join("__pycache__");
        fs::create_dir(&cache).unwrap();
        for name in [
            "mod.cpython-312.pyc",
            "mod.cpython-312.opt-1.pyc",
            "other.cpython-312.pyc",
        ] {
            fs::write(cache.join(name), "bytecode").unwrap();
        }
        copy.switch(&project.store, tree_b).unwrap();
        let mut left: Vec<_> = fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["other.cpython-312.pyc"]);
        // Removing the files removes the rest, and the folders left empty.
        copy.switch(&project.store, project.typed_tree(&[]))
            .unwrap();
        assert!(!copy.path().join("pkg").exists());
    }

    /// Writes `content` at `path` inside `root`, creating the folders above it.
    fn create(root: &Path, path: &str, content: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    #[test]
    fn what_a_test_run_changed_is_undone_and_ignored_build_folders_are_kept() {
        let project = Project::new();
        let files = [
            (".gitignore", Regular, "target/\n__pycache__/\n/dist/\n"),
            ("README.md", Regular, "readme\n"),
            ("pkg/mod.py", Regular, "VALUE = 1\n"),
            ("src/lib.rs", Regular, "pub fn f() {}\n"),
            ("src/main.rs", Regular, "fn main() {}\n"),
        ];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();

        // The test run changes a file, deletes one, creates a file and a folder, and leaves build outputs.
        let root = copy.path().to_path_buf();
        fs::write(root.join("src").join("lib.rs"), "broken").unwrap();
        fs::remove_file(root.join("README.md")).unwrap();
        create(&root, "src/created.txt", "new");
        create(&root, "tmp/deeper/out.txt", "out");
        let ignored = [
            "target/debug/app",
            "pkg/__pycache__/mod.cpython-312.pyc",
            "dist/bundle.js",
        ];
        for path in ignored {
            create(&root, path, "build output");
        }

        let switched = copy.switch(&project.store, tree).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 2,
                removed: 0,
                restored: 2,
                // `src/created.txt`, `tmp/deeper/out.txt`, `tmp/deeper` and `tmp`.
                deleted: 4,
            }
        );
        let mut expected = expected(&files);
        for path in ignored {
            expected.insert(path.to_owned(), (Regular, b"build output".to_vec()));
        }
        assert_eq!(contents(&root), expected);
    }

    #[test]
    fn a_test_run_is_undone_on_a_switch_to_another_tree() {
        let project = Project::new();
        let a = [
            ("changed_by_run.txt", Regular, "a"),
            ("changed_by_both.txt", Regular, "a"),
            ("removed_in_b.txt", Regular, "a"),
        ];
        let b = [
            ("changed_by_run.txt", Regular, "a"),
            ("changed_by_both.txt", Regular, "b"),
        ];
        let (tree_a, tree_b) = (project.typed_tree(&a), project.typed_tree(&b));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_a).unwrap();
        let root = copy.path().to_path_buf();
        for name in ["changed_by_run.txt", "changed_by_both.txt"] {
            fs::write(root.join(name), "run").unwrap();
        }
        fs::remove_file(root.join("removed_in_b.txt")).unwrap();
        create(&root, "created.txt", "run");

        let switched = copy.switch(&project.store, tree_b).unwrap();
        // The file the diff rewrites anyway is not counted as restored.
        assert_eq!(
            switched,
            Switched {
                written: 2,
                removed: 0,
                restored: 1,
                deleted: 1,
            }
        );
        assert_holds(&copy, &b);
    }

    #[test]
    fn a_same_size_change_that_keeps_the_mtime_is_undone() {
        let project = Project::new();
        let files = [("a.txt", Regular, "one")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        fs::write(copy.path().join("a.txt"), "two").unwrap();
        // The copy wrote this size and this mtime, at the end of the switch: the run's change landed in the
        // same timestamp tick.
        let mtime = set_placed_mtime(&mut copy, "a.txt", 0);
        assert!(is_racy(mtime, copy.clean_at));
        assert_eq!(copy.switch(&project.store, tree).unwrap().restored, 1);
        assert_holds(&copy, &files);
    }

    /// Sets the mtime of the file `path` of the copy to `offset_ns` after the end of the last switch, and
    /// makes the copy's stat manifest hold that mtime and the file's size, as if the copy had written it so.
    /// Returns the mtime.
    fn set_placed_mtime(copy: &mut TestCopy, path: &str, offset_ns: i64) -> i64 {
        let full = copy.path().join(path);
        let time = UNIX_EPOCH + Duration::from_nanos((copy.clean_at + offset_ns) as u64);
        fs::File::options()
            .write(true)
            .open(&full)
            .unwrap()
            .set_modified(time)
            .unwrap();
        let metadata = fs::symlink_metadata(&full).unwrap();
        let placed = copy.placed.get_mut(path).unwrap();
        *placed = Placed::new(placed.file, &metadata);
        placed.mtime_ns.unwrap()
    }

    #[test]
    fn a_permission_a_test_changed_is_undone() {
        let project = Project::new();
        let files = [
            ("plain.txt", Regular, "plain"),
            ("run.sh", Executable, "#!/bin/sh\n"),
            ("same.txt", Regular, "same"),
        ];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        // Old enough that size and mtime alone vouch for the files: only the permission tells.
        for (path, _, _) in files {
            let mtime = set_placed_mtime(&mut copy, path, -10_000_000_000);
            assert!(!is_racy(mtime, copy.clean_at));
        }
        let plain = copy.path().join("plain.txt");
        let mut permissions = fs::metadata(&plain).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&plain, permissions).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let run = copy.path().join("run.sh");
            fs::set_permissions(&run, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let restored = if cfg!(unix) { 2 } else { 1 };
        assert_eq!(
            copy.switch(&project.store, tree).unwrap().restored,
            restored
        );
        assert_holds(&copy, &files);
        assert_exact_modes(&copy, &files);
        assert!(!fs::metadata(&plain).unwrap().permissions().readonly());
    }

    #[test]
    fn a_locked_folder_a_test_put_in_place_of_a_file_is_removed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("keep.txt"), "keep me").unwrap();
        let before = contents(canary.path());
        let files = [("x", Regular, "a file")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();

        // The run replaces the file with a folder holding read-only files and a folder without write
        // permission, which holds a link to a folder outside.
        let x = copy.path().join("x");
        fs::remove_file(&x).unwrap();
        create(&x, "top.txt", "top");
        create(&x, "sub/f.txt", "f");
        link_dir(canary.path(), &x.join("sub").join("out"));
        lock(
            &[x.join("top.txt"), x.join("sub").join("f.txt")],
            &x.join("sub"),
        );

        let switched = copy.switch(&project.store, tree).unwrap();
        assert_eq!(switched.restored, 1);
        assert_holds(&copy, &files);
        assert_eq!(contents(canary.path()), before);
    }

    /// Makes `files` read-only and, on Unix, takes the write permission away from `dir`.
    fn lock(files: &[PathBuf], dir: &Path) {
        for file in files {
            let mut permissions = fs::metadata(file).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(file, permissions).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        }
        #[cfg(windows)]
        let _ = dir;
    }

    #[test]
    fn removing_a_locked_folder_never_follows_a_link_in_it() {
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("keep.txt"), "keep me").unwrap();
        let before = contents(canary.path());
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        create(&locked, "deeper/f.txt", "f");
        link_dir(canary.path(), &locked.join("deeper").join("out"));
        lock(
            &[locked.join("deeper").join("f.txt")],
            &locked.join("deeper"),
        );
        // Called directly, so it runs on every system whether or not `remove_dir_all` would manage.
        remove_dir_forcing(&locked).unwrap();
        assert!(!locked.exists());
        assert_eq!(contents(canary.path()), before);
    }

    #[test]
    fn an_extra_path_in_a_folder_outside_the_tree_is_kept() {
        let project = Project::new();
        let files = [("a.txt", Regular, "a")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.set_extra_paths(["config/local.json".to_owned()]);
        create(copy.path(), "config/local.json", "{}");
        create(copy.path(), "config/other.txt", "x");
        assert_eq!(copy.switch(&project.store, tree).unwrap().deleted, 1);
        let mut expected = expected(&files);
        expected.insert("config/local.json".to_owned(), (Regular, b"{}".to_vec()));
        assert_eq!(contents(copy.path()), expected);
    }

    #[test]
    fn a_link_a_test_created_is_removed_even_where_it_is_ignored() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("keep.txt"), "keep me").unwrap();
        let before = contents(canary.path());
        let files = [
            (".gitignore", Regular, "coverage\n"),
            ("a.txt", Regular, "a"),
        ];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        link_dir(canary.path(), &copy.path().join("coverage"));
        assert_eq!(copy.switch(&project.store, tree).unwrap().deleted, 1);
        assert_holds(&copy, &files);
        assert_eq!(contents(canary.path()), before);
    }

    #[test]
    fn a_link_inside_a_folder_a_test_created_is_removed_not_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("keep.txt"), "keep me").unwrap();
        let before = contents(canary.path());
        let files = [("a.txt", Regular, "a")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        create(copy.path(), "tmp/x.txt", "x");
        link_dir(canary.path(), &copy.path().join("tmp").join("out"));
        // `tmp/x.txt`, `tmp/out` and `tmp`.
        assert_eq!(copy.switch(&project.store, tree).unwrap().deleted, 3);
        assert_holds(&copy, &files);
        assert_eq!(contents(canary.path()), before);
    }

    #[test]
    fn a_file_touched_by_a_test_is_hashed_not_rewritten() {
        let project = Project::new();
        let files = [("a.txt", Regular, "same")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(copy.path().join("a.txt"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            copy.switch(&project.store, tree).unwrap(),
            Switched::default()
        );
        assert_holds(&copy, &files);
    }

    #[test]
    fn a_file_created_through_a_link_is_not_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("a.txt"), "keep me").unwrap();
        let files = [("src/a.txt", Regular, "a"), ("top.txt", Regular, "top")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();

        // The test run links a new folder and a folder of the tree to a folder outside, and creates a file
        // through the first link.
        let root = copy.path().to_path_buf();
        link_dir(canary.path(), &root.join("out"));
        fs::write(root.join("out").join("created.txt"), "through the link").unwrap();
        fs::remove_dir_all(root.join("src")).unwrap();
        link_dir(canary.path(), &root.join("src"));
        let before = contents(canary.path());
        assert_eq!(before.len(), 2);

        let switched = copy.switch(&project.store, tree).unwrap();
        assert_eq!((switched.restored, switched.deleted), (1, 1));
        assert_holds(&copy, &files);
        assert_eq!(contents(canary.path()), before);
    }

    #[test]
    fn ignore_rules_changed_or_added_by_a_test_have_no_effect() {
        let project = Project::new();
        let files = [
            (".gitignore", Regular, "build/\n"),
            ("sub/a.txt", Regular, "a"),
        ];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        let root = copy.path().to_path_buf();
        fs::write(root.join(".gitignore"), "*\n").unwrap();
        create(&root, "sub/.gitignore", "*\n");
        create(&root, "created.txt", "x");
        create(&root, "sub/created.txt", "x");
        create(&root, "build/out.o", "o");

        copy.switch(&project.store, tree).unwrap();
        let mut expected = expected(&files);
        expected.insert("build/out.o".to_owned(), (Regular, b"o".to_vec()));
        assert_eq!(contents(&root), expected);
    }

    #[test]
    fn a_first_switch_removes_files_of_another_tree_and_keeps_ignored_ones() {
        let project = Project::new();
        let a = [
            (".gitignore", Regular, "target/\n"),
            ("a.txt", Regular, "a"),
            ("old/only_in_a.txt", Regular, "a"),
        ];
        let b = [
            (".gitignore", Regular, "target/\n"),
            ("a.txt", Regular, "a"),
        ];
        let (tree_a, tree_b) = (project.typed_tree(&a), project.typed_tree(&b));
        project.copy().switch(&project.store, tree_a).unwrap();
        let mut copy = project.copy();
        create(copy.path(), "target/debug/app", "app");

        // A new bisect reusing the copy: nothing tells it that the copy holds another tree.
        let switched = copy.switch(&project.store, tree_b).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 0,
                removed: 0,
                restored: 0,
                deleted: 2,
            }
        );
        let mut expected = expected(&b);
        expected.insert("target/debug/app".to_owned(), (Regular, b"app".to_vec()));
        assert_eq!(contents(copy.path()), expected);
    }

    #[test]
    fn the_switch_after_a_failed_one_removes_stale_files() {
        let project = Project::new();
        let files = [("a.txt", Regular, "a")];
        let tree = project.typed_tree(&files);
        let missing = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let broken = project.crafted_tree(&[
            (
                "a.txt",
                EntryKind::Blob,
                project.store.write_blob(b"a").unwrap(),
            ),
            (
                "b.txt",
                EntryKind::Blob,
                project.store.write_blob(b"b").unwrap(),
            ),
            ("z.txt", EntryKind::Blob, missing),
        ]);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        assert!(copy.switch(&project.store, broken).is_err());
        // Written before the switch failed.
        assert!(copy.path().join("b.txt").exists());
        create(copy.path(), "stray.txt", "x");
        assert_eq!(copy.switch(&project.store, tree).unwrap().deleted, 2);
        assert_holds(&copy, &files);
    }

    #[test]
    fn extra_paths_are_kept_as_they_are() {
        let project = Project::new();
        let files = [("a.txt", Regular, "a")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.set_extra_paths(["node_modules".to_owned(), ".env".to_owned()]);
        create(copy.path(), "node_modules/dep/index.js", "dep");
        create(copy.path(), ".env", "KEY=value");
        copy.switch(&project.store, tree).unwrap();
        let mut expected = expected(&files);
        expected.insert(
            "node_modules/dep/index.js".to_owned(),
            (Regular, b"dep".to_vec()),
        );
        expected.insert(".env".to_owned(), (Regular, b"KEY=value".to_vec()));
        assert_eq!(contents(copy.path()), expected);

        copy.set_extra_paths([]);
        copy.switch(&project.store, tree).unwrap();
        assert_holds(&copy, &files);
    }

    #[test]
    fn an_extra_folder_where_the_tree_now_has_files_is_cleaned_like_the_trees() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        fs::write(canary.path().join("keep.txt"), "keep me").unwrap();
        let before = contents(canary.path());
        let files = [("node_modules/local.js", Regular, "local")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.set_extra_paths(["node_modules".to_owned()]);
        create(copy.path(), "node_modules/stray.txt", "x");
        link_dir(canary.path(), &copy.path().join("node_modules").join("dep"));
        assert_eq!(copy.switch(&project.store, tree).unwrap().deleted, 2);
        assert_holds(&copy, &files);
        assert_eq!(contents(canary.path()), before);
    }

    #[test]
    fn the_projects_info_exclude_applies_in_the_copy() {
        let project = Project::new();
        create(project.path(), ".git/info/exclude", "*.yalper-test-log\n");
        let files = [("a.txt", Regular, "a")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        create(copy.path(), "run.yalper-test-log", "log");
        create(copy.path(), "created.txt", "x");
        assert_eq!(copy.switch(&project.store, tree).unwrap().deleted, 1);
        let mut expected = expected(&files);
        expected.insert("run.yalper-test-log".to_owned(), (Regular, b"log".to_vec()));
        assert_eq!(contents(copy.path()), expected);
    }

    #[test]
    fn read_only_files_and_folders_a_test_created_are_removed() {
        let project = Project::new();
        let files = [("a.txt", Regular, "a")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        let locked = copy.path().join("tmp").join("locked");
        create(copy.path(), "tmp/locked/f.txt", "x");
        let file = locked.join("f.txt");
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        }
        assert_eq!(copy.switch(&project.store, tree).unwrap().deleted, 3);
        assert_holds(&copy, &files);
    }

    #[test]
    fn a_name_differing_only_in_case_is_kept_only_on_a_case_insensitive_file_system() {
        let project = Project::new();
        let files = [("README.md", Regular, "same")];
        let tree = project.typed_tree(&files);
        let mut copy = project.copy();
        create(copy.path(), "readme.md", "same");
        copy.switch(&project.store, tree).unwrap();
        let names: Vec<String> = fs::read_dir(copy.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        if cfg!(any(windows, target_os = "macos")) {
            // The known limit of a first switch: the tree's file, with its old spelling.
            assert_eq!(names, ["readme.md"]);
        } else {
            assert_eq!(names, ["README.md"]);
        }
        assert_eq!(fs::read(copy.path().join("README.md")).unwrap(), b"same");
    }

    /// Python runs a same-size change of `pkg/mod.py` written in the same second as the version it compiled,
    /// with its bytecode next to the file or, if `prefix`, in the copy's `PYTHONPYCACHEPREFIX` folder.
    fn python_runs_a_same_size_change(prefix: bool) {
        let Some(python) = tool(&["python3", "python"]) else {
            return;
        };
        let project = Project::new();
        let a = [
            ("pkg/__init__.py", Regular, ""),
            ("pkg/mod.py", Regular, "VALUE = 1\n"),
        ];
        let b = [
            ("pkg/__init__.py", Regular, ""),
            ("pkg/mod.py", Regular, "VALUE = 2\n"),
        ];
        let (tree_a, tree_b) = (project.typed_tree(&a), project.typed_tree(&b));
        let mut copy = project.copy();
        let value = |copy: &TestCopy| {
            let mut command = Command::new(python);
            command
                .args([
                    "-c",
                    "import pkg.mod, sys; sys.stdout.write(str(pkg.mod.VALUE))",
                ])
                .current_dir(copy.path())
                .env_remove("PYTHONDONTWRITEBYTECODE")
                .env_remove("PYTHONPYCACHEPREFIX");
            if prefix {
                command.env("PYTHONPYCACHEPREFIX", copy.pycache_path());
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        let bytecode_of_mod = |copy: &TestCopy| {
            let cache = if prefix {
                copy.pycache_path().to_owned()
            } else {
                copy.path().join("pkg").join("__pycache__")
            };
            if !cache.exists() {
                return 0;
            }
            contents(&cache)
                .into_keys()
                .filter(|path| {
                    let name = path.rsplit('/').next().unwrap_or(path);
                    name.starts_with("mod.") && name.ends_with(".pyc")
                })
                .count()
        };

        copy.switch(&project.store, tree_a).unwrap();
        assert_eq!(value(&copy), "1");
        assert_eq!(bytecode_of_mod(&copy), 1, "no bytecode written");
        assert_eq!(
            copy.path().join("pkg").join("__pycache__").exists(),
            !prefix
        );
        let source = copy.path().join("pkg").join("mod.py");
        let mtime = fs::metadata(&source).unwrap().modified().unwrap();

        copy.switch(&project.store, tree_b).unwrap();
        assert_eq!(bytecode_of_mod(&copy), 0);
        // Same size and same mtime as the version Python compiled: only removing its bytecode makes Python
        // read the new source.
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(value(&copy), "2");
    }

    #[test]
    fn python_runs_a_same_size_change_made_in_the_same_second() {
        python_runs_a_same_size_change(false);
    }

    #[test]
    fn python_runs_a_same_size_change_with_a_bytecode_prefix_folder() {
        python_runs_a_same_size_change(true);
    }

    #[test]
    fn bytecode_in_the_prefix_folder_is_removed_never_through_a_link() {
        let project = Project::new();
        let a = [
            ("pkg/mod.py", Regular, "VALUE = 1\n"),
            ("pkg/other.py", Regular, ""),
            ("linked/x.py", Regular, ""),
            ("top.py", Regular, ""),
        ];
        let b = [
            ("pkg/mod.py", Regular, "VALUE = 2\n"),
            ("pkg/other.py", Regular, ""),
            ("linked/x.py", Regular, "changed"),
        ];
        let mut copy = project.copy();
        copy.switch(&project.store, project.typed_tree(&a)).unwrap();

        // Where Python puts the bytecode with `PYTHONPYCACHEPREFIX`: under the copy's absolute path without its
        // drive or root.
        let text = copy.path().to_string_lossy().into_owned();
        let without_root = if cfg!(windows) {
            &text[3..]
        } else {
            &text[1..]
        };
        assert_eq!(copy.mirrors[0], PathBuf::from(without_root));
        let mirror = copy.pycache_path().join(without_root);
        for path in [
            "pkg/mod.cpython-312.pyc",
            "pkg/mod.cpython-312.opt-1.pyc",
            "pkg/other.cpython-312.pyc",
            "top.cpython-312.pyc",
        ] {
            create(&mirror, path, "bytecode");
        }
        // A link a test put in the prefix folder, to a canary.
        let canary = tempfile::tempdir().unwrap();
        create(canary.path(), "x.cpython-312.pyc", "keep me");
        link_dir(canary.path(), &mirror.join("linked"));

        copy.switch(&project.store, project.typed_tree(&b)).unwrap();
        assert_eq!(
            contents(&mirror).into_keys().collect::<Vec<_>>(),
            ["linked", "pkg/other.cpython-312.pyc"]
        );
        assert_eq!(
            fs::read(canary.path().join("x.cpython-312.pyc")).unwrap(),
            b"keep me"
        );
    }

    #[test]
    fn a_link_at_the_prefix_folders_parent_is_not_followed() {
        let project = Project::new();
        let a = [("pkg/mod.py", Regular, "VALUE = 1\n")];
        let b = [("pkg/mod.py", Regular, "VALUE = 2\n")];
        let mut copy = project.copy();
        copy.switch(&project.store, project.typed_tree(&a)).unwrap();

        // A test run replaces the `pycache` folder with a link to a canary holding bytecode at the path the
        // copy's own prefix folder would have.
        let canary = tempfile::tempdir().unwrap();
        let text = copy.path().to_string_lossy().into_owned();
        let without_root = if cfg!(windows) {
            &text[3..]
        } else {
            &text[1..]
        };
        let planted = canary.path().join("copy-0").join(without_root);
        create(&planted, "pkg/mod.cpython-312.pyc", "keep me");
        let parent = copy.pycache_path().parent().unwrap().to_owned();
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        link_dir(canary.path(), &parent);

        copy.switch(&project.store, project.typed_tree(&b)).unwrap();
        assert_eq!(
            fs::read(planted.join("pkg").join("mod.cpython-312.pyc")).unwrap(),
            b"keep me"
        );
    }

    #[test]
    fn a_hard_link_at_the_clock_is_replaced_never_written_through() {
        let project = Project::new();
        let tree = project.typed_tree(&[("a.txt", Regular, "a")]);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        // A test run puts a hard link to a project file at the clock's name.
        let canary = tempfile::tempdir().unwrap();
        let target = canary.path().join("keep.txt");
        fs::write(&target, "keep me").unwrap();
        let clock = project.cache.path().join(CLOCK_FILE);
        fs::remove_file(&clock).unwrap();
        fs::hard_link(&target, &clock).unwrap();

        copy.switch(&project.store, tree).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        assert!(fs::symlink_metadata(&clock).unwrap().is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&clock).unwrap().nlink(), 1);
        }
    }

    /// Unix only: on Windows the copy holds the project's cache folder open, so it cannot be renamed.
    #[cfg(unix)]
    #[test]
    fn a_link_in_place_of_the_cache_folder_is_refused() {
        let project = Project::new();
        let tree = project.typed_tree(&[("pkg/mod.py", Regular, "")]);
        let mut copy = project.copy();
        copy.switch(&project.store, tree).unwrap();
        // The copy's own path still leads to the same folder, through the link.
        let cache = copy.path().parent().unwrap().to_owned();
        let moved = cache.with_file_name("moved");
        fs::rename(&cache, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &cache).unwrap();
        let error = copy.switch(&project.store, tree).unwrap_err();
        assert!(error.to_string().contains("was replaced"), "{error}");
    }

    #[test]
    fn the_prefix_folder_mirrors_the_path_without_its_drive_or_root() {
        let path = |text: &str| PathBuf::from(text);
        assert_eq!(
            mirror(&path("/home/me/p/copy-0")),
            Some(path("home/me/p/copy-0"))
        );
        #[cfg(windows)]
        {
            assert_eq!(mirror(&path(r"C:\p\copy-0")), Some(path(r"p\copy-0")));
            assert_eq!(
                mirror(&path(r"\\server\share\p")),
                Some(path(r"server\share\p"))
            );
            assert_eq!(mirror(&path(r"\\.\pipe\x")), None);
        }
    }
}
