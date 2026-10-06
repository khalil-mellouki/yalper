//! Taking a snapshot: walk the project, find the files that changed since the latest snapshot, store them,
//! and build the new tree from the previous one.
//!
//! The stat cache (`file_cache` in the event log) remembers the size, mtime, kind and blob id of every file of
//! the latest snapshot, so a file is only read when its size, mtime or kind changed. Changes made by any
//! process (an edit tool, a shell command, a code generator) show up the same way.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::{Component, Path};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gix::ObjectId;
use ignore::{WalkBuilder, WalkState};

use super::{Change, FileKind, Result, ShadowStore, validate_path};
use crate::hook::YALPER_DIR;
use crate::safe_fs::{self, OwnedDir};
use crate::store::{CachedFile, FileCache, Store};

/// Files larger than this are left out of snapshots.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// A file whose mtime is this close to the start of the scan is read again by the next scan, because a change
/// made later within the same timestamp tick would leave its size and mtime unchanged (the same idea as git's
/// "racy" index entries). Two seconds covers file systems with coarse timestamps (FAT).
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Threads of the directory walk. Listing directories is mostly waiting on the file system, so a few threads
/// help, and more only cost time to start.
const WALK_THREADS: usize = 4;

/// The result of [`snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The tree of the working tree after this step. Unchanged files keep the tree of the previous snapshot.
    pub tree_id: ObjectId,
    /// Files added, modified (content, executable bit or file/symlink kind) or removed since the previous
    /// snapshot, sorted. Its length is the step's number of files changed.
    pub changed: Vec<String>,
    /// Files and directories left out of the snapshot or kept as they were, sorted by path.
    pub skipped: Vec<Skipped>,
    /// How many times a file was read: files the stat cache knew to be unchanged are not read, and a changed
    /// file that the walk hashed is read a second time to be stored.
    pub files_read: usize,
}

/// A file or directory [`snapshot`] could not take as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Relative to the project root, with `/` as separator (lossy if the name is not UTF-8). Empty for the
    /// project root itself.
    pub path: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Larger than [`MAX_FILE_BYTES`] (the size in bytes): not in the snapshot.
    TooLarge(u64),
    /// A name that cannot be stored in a tree (not UTF-8, or refused by [`validate_path`]): not in the
    /// snapshot, nor anything inside it.
    InvalidPath(String),
    /// Could not be read, for example a file locked by another process on Windows or a directory without
    /// read permission: it keeps what the previous snapshot had (for a directory, everything inside it).
    Unreadable(String),
}

impl fmt::Display for Skipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "."
        } else {
            &self.path
        };
        match &self.reason {
            SkipReason::TooLarge(size) => write!(
                f,
                "{path:?} not snapshotted: {size} bytes, over the {MAX_FILE_BYTES} byte limit"
            ),
            SkipReason::InvalidPath(reason) => write!(f, "{path:?} not snapshotted: {reason}"),
            SkipReason::Unreadable(error) => {
                write!(f, "{path:?} kept from the previous snapshot: {error}")
            }
        }
    }
}

/// Takes a snapshot of the project that contains `yalper_dir`: every file that `.gitignore`, `.git/info/exclude`
/// and the user's global excludes do not ignore, except anything named `.git` or `.yalper`. Symlinks are
/// stored as symlinks (their target is never read), and on Unix the executable bit is kept.
///
/// New file contents are written to the shadow store, which is only opened when something changed. The new
/// tree and the stat cache are saved together in `store`. The caller must hold the [`WriterLock`].
///
/// One file that cannot be taken never fails the snapshot: it is listed in [`Snapshot::skipped`] instead.
///
/// [`WriterLock`]: crate::store::WriterLock
pub fn snapshot(yalper_dir: &OwnedDir, store: &Store) -> Result<Snapshot> {
    let root = yalper_dir
        .path()
        .parent()
        .ok_or_else(|| io::Error::other("the .yalper directory has no parent"))?;
    let started = SystemTime::now();
    let racy_from_ns = nanos_since_epoch(started.checked_sub(RACY_WINDOW).unwrap_or(UNIX_EPOCH));

    // Without a usable cache, the snapshot starts over from the empty tree and every file is read.
    let previous = store.file_cache()?.and_then(check_cache);
    let start_over = previous.is_none();
    let (base, cache) =
        previous.unwrap_or_else(|| (ObjectId::empty_tree(gix::hash::Kind::Sha1), HashMap::new()));

    let mut shadow = None;
    let mut snapshot = Snapshot {
        tree_id: base,
        changed: Vec::new(),
        skipped: Vec::new(),
        files_read: 0,
    };
    let mut changes = Vec::new();
    let mut updated_rows = Vec::new();
    // Files of this snapshot found by the walk. Under the kept prefixes (unreadable files and directories),
    // what the previous snapshot had stays.
    let mut present = HashSet::new();
    let mut kept_prefixes = Vec::new();

    for walked in walk(root, &cache) {
        let (file, check) = match walked {
            Walked::File(file, check) => (file, check),
            Walked::Skipped(skipped) => {
                if let SkipReason::Unreadable(_) = skipped.reason {
                    kept_prefixes.push(skipped.path.clone());
                }
                snapshot.skipped.push(skipped);
                continue;
            }
        };
        let cached = cache.get(&file.path);
        let same_content = |oid, kind| cached.is_some_and(|c| c.oid == oid && c.kind == kind);
        let (oid, len) = match check {
            Check::Unchanged => {
                present.insert(file.path);
                continue;
            }
            Check::Hashed { oid, len } if same_content(oid, file.kind) => {
                snapshot.files_read += 1;
                (oid, len)
            }
            Check::Hashed { .. } | Check::New => {
                snapshot.files_read += usize::from(matches!(check, Check::Hashed { .. }));
                // New or changed: read here, where blobs are written one at a time.
                let bytes = match read(root, &file) {
                    Ok(Content::Bytes(bytes)) => bytes,
                    Ok(Content::Gone) => continue,
                    Ok(Content::TooLarge(size)) => {
                        snapshot.skipped.push(file.skip(SkipReason::TooLarge(size)));
                        continue;
                    }
                    Err(error) => {
                        kept_prefixes.push(file.path.clone());
                        let reason = SkipReason::Unreadable(error.to_string());
                        snapshot.skipped.push(file.skip(reason));
                        continue;
                    }
                };
                snapshot.files_read += 1;
                let oid = open_once(&mut shadow, yalper_dir)?.write_blob(&bytes)?;
                if !same_content(oid, file.kind) {
                    changes.push(Change::Upsert {
                        path: file.path.clone(),
                        kind: file.kind,
                        blob: oid,
                    });
                    snapshot.changed.push(file.path.clone());
                }
                (oid, bytes.len() as u64)
            }
        };
        let row = Cached {
            size: file.size,
            mtime_ns: file.mtime_ns,
            kind: file.kind,
            oid,
            // A length that differs from the size seen by the walk means the file changed while it was read.
            racy: file.mtime_ns >= racy_from_ns || len != file.size,
        };
        if cached != Some(&row) {
            updated_rows.push(row.to_cached_file(&file.path));
        }
        present.insert(file.path);
    }

    let mut removed = Vec::new();
    for path in cache.keys() {
        let kept = || {
            kept_prefixes.iter().any(|prefix| {
                prefix.is_empty()
                    || path == prefix
                    || path
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        if !present.contains(path) && !kept() {
            changes.push(Change::Remove { path: path.clone() });
            snapshot.changed.push(path.clone());
            removed.push(path.clone());
        }
    }

    if !changes.is_empty() {
        snapshot.tree_id = open_once(&mut shadow, yalper_dir)?.edit_tree(base, &changes)?;
    }
    if start_over || snapshot.tree_id != base || !updated_rows.is_empty() || !removed.is_empty() {
        store.save_snapshot(
            &snapshot.tree_id.to_string(),
            &updated_rows,
            &removed,
            start_over,
        )?;
    }
    snapshot.changed.sort_unstable();
    snapshot
        .skipped
        .sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(snapshot)
}

fn open_once<'a>(
    shadow: &'a mut Option<ShadowStore>,
    yalper_dir: &OwnedDir,
) -> Result<&'a ShadowStore> {
    match shadow {
        Some(shadow) => Ok(shadow),
        None => Ok(shadow.insert(ShadowStore::open(yalper_dir)?)),
    }
}

/// A stat cache entry, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cached {
    size: u64,
    mtime_ns: i64,
    kind: FileKind,
    oid: ObjectId,
    racy: bool,
}

impl Cached {
    /// Whether `file` is known to be unchanged without reading it.
    fn matches(&self, file: &Found) -> bool {
        !self.racy
            && self.size == file.size
            && self.mtime_ns == file.mtime_ns
            && self.kind == file.kind
    }

    fn to_cached_file(&self, path: &str) -> CachedFile {
        CachedFile {
            path: path.to_owned(),
            size: i64::try_from(self.size).unwrap_or(i64::MAX),
            mtime_ns: self.mtime_ns,
            mode: i64::from(self.kind.mode()),
            oid: self.oid.to_string(),
            racy: self.racy,
        }
    }
}

/// The stat cache, if every row is one Yalper could have written. The database is a file in the project, so
/// a damaged or planted row makes the whole cache unused (the snapshot then starts over) rather than trusted.
fn check_cache(cache: FileCache) -> Option<(ObjectId, HashMap<String, Cached>)> {
    let tree_id = ObjectId::from_hex(cache.tree_id.as_bytes()).ok()?;
    let mut files = HashMap::with_capacity(cache.files.len());
    for file in cache.files {
        let kind = FileKind::from_mode(file.mode)?;
        validate_path(&file.path, kind).ok()?;
        let cached = Cached {
            size: u64::try_from(file.size).ok()?,
            mtime_ns: file.mtime_ns,
            kind,
            oid: ObjectId::from_hex(file.oid.as_bytes()).ok()?,
            racy: file.racy,
        };
        files.insert(file.path, cached);
    }
    Some((tree_id, files))
}

/// A file or symlink found by the walk.
#[derive(Debug)]
struct Found {
    /// Relative to the project root, with `/` as separator.
    path: String,
    kind: FileKind,
    size: u64,
    mtime_ns: i64,
}

impl Found {
    fn skip(&self, reason: SkipReason) -> Skipped {
        Skipped {
            path: self.path.clone(),
            reason,
        }
    }
}

enum Walked {
    File(Found, Check),
    Skipped(Skipped),
}

/// What the walk found out about a file's content.
enum Check {
    /// Size, mtime and kind match a stat cache entry that is not racy: not read.
    Unchanged,
    /// Not in the stat cache: read and stored after the walk.
    New,
    /// In the stat cache, but maybe changed: read and hashed by the walk thread. Only a file whose content
    /// differs is read again after the walk, to be stored.
    Hashed { oid: ObjectId, len: u64 },
}

/// Lists every file and symlink of the project that is not ignored, with its size, mtime and kind, using
/// parallel threads. Files that may have changed since they were cached are hashed in those threads; their
/// content is not kept, so memory stays small whatever the size of the project.
fn walk(root: &Path, cache: &HashMap<String, Cached>) -> Vec<Walked> {
    let (sender, receiver) = mpsc::channel();
    WalkBuilder::new(root)
        // Dotfiles are code too, and `.ignore` and `.rgignore` are ripgrep's files, not git's.
        .hidden(false)
        .ignore(false)
        .parents(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .follow_links(false)
        // At any depth: nested repositories and submodules have their own `.git`.
        .filter_entry(|entry| {
            let name = entry.file_name();
            !(name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(YALPER_DIR))
        })
        .threads(WALK_THREADS)
        .build_parallel()
        .run(|| {
            let sender = sender.clone();
            Box::new(move |entry| {
                let (walked, state) = visit(root, cache, entry);
                if let Some(walked) = walked {
                    // The receiver lives until the walk is over.
                    let _ = sender.send(walked);
                }
                state
            })
        });
    drop(sender);
    receiver.into_iter().collect()
}

fn visit(
    root: &Path,
    cache: &HashMap<String, Cached>,
    entry: std::result::Result<ignore::DirEntry, ignore::Error>,
) -> (Option<Walked>, WalkState) {
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) => return (walk_error(root, &error), WalkState::Continue),
    };
    let Some(file_type) = entry.file_type() else {
        return (None, WalkState::Continue);
    };
    if entry.depth() == 0 {
        return (None, WalkState::Continue);
    }
    let skip = |path: String, reason| {
        let skipped = Skipped { path, reason };
        (Some(Walked::Skipped(skipped)), WalkState::Skip)
    };
    let path = match relative_path(root, entry.path()) {
        Ok(path) => path,
        Err(lossy) => return skip(lossy, SkipReason::InvalidPath("not valid UTF-8".to_owned())),
    };

    let kind = if file_type.is_dir() {
        None
    } else if file_type.is_file() || file_type.is_symlink() {
        Some(FileKind::Regular)
    } else {
        // FIFOs, sockets and devices: git does not store them either.
        return (None, WalkState::Continue);
    };
    let metadata = match kind.map(|_| entry.metadata()) {
        None => None,
        Some(Ok(metadata)) => Some(metadata),
        Some(Err(error)) => {
            return match error.io_error() {
                // Deleted since its directory was listed: not in the snapshot.
                Some(io_error) if io_error.kind() == io::ErrorKind::NotFound => {
                    (None, WalkState::Continue)
                }
                Some(io_error) => skip(path, SkipReason::Unreadable(io_error.to_string())),
                None => skip(path, SkipReason::Unreadable(error.to_string())),
            };
        }
    };
    let kind = metadata.as_ref().map_or(FileKind::Regular, kind_of);
    if let Err(error) = validate_path(&path, kind) {
        let reason = match error {
            super::Error::InvalidPath { reason, .. } => reason,
            other => other.to_string(),
        };
        return skip(path, SkipReason::InvalidPath(reason));
    }
    let Some(metadata) = metadata else {
        return (None, WalkState::Continue);
    };
    let mtime_ns = match metadata.modified() {
        Ok(mtime) => nanos_since_epoch(mtime),
        Err(error) => return skip(path, SkipReason::Unreadable(error.to_string())),
    };
    let found = Found {
        path,
        kind,
        size: metadata.len(),
        mtime_ns,
    };
    let check = match cache.get(&found.path) {
        Some(cached) if cached.matches(&found) => Check::Unchanged,
        _ if found.size > MAX_FILE_BYTES => {
            return skip(found.path, SkipReason::TooLarge(found.size));
        }
        None => Check::New,
        Some(_) => match read(root, &found) {
            Ok(Content::Bytes(bytes)) => match blob_id(&bytes) {
                Ok(oid) => Check::Hashed {
                    oid,
                    len: bytes.len() as u64,
                },
                Err(error) => return skip(found.path, SkipReason::Unreadable(error.to_string())),
            },
            Ok(Content::Gone) => return (None, WalkState::Continue),
            Ok(Content::TooLarge(size)) => {
                return skip(found.path, SkipReason::TooLarge(size));
            }
            Err(error) => return skip(found.path, SkipReason::Unreadable(error.to_string())),
        },
    };
    (Some(Walked::File(found, check)), WalkState::Continue)
}

/// The id `bytes` get as a blob, without writing anything.
fn blob_id(bytes: &[u8]) -> io::Result<ObjectId> {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .map_err(|error| io::Error::other(error.to_string()))
}

/// A walk error that may hide files: an unreadable directory or file. Errors about ignore file patterns are
/// left out (the walk skips the bad pattern). Without a path, the whole project is reported, so nothing is
/// taken as removed.
fn walk_error(root: &Path, error: &ignore::Error) -> Option<Walked> {
    let io_error = error.io_error()?;
    let path = error_path(error)
        .and_then(|path| relative_path(root, path).ok())
        .unwrap_or_default();
    let reason = SkipReason::Unreadable(io_error.to_string());
    Some(Walked::Skipped(Skipped { path, reason }))
}

fn error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errors) if errors.len() == 1 => error_path(&errors[0]),
        _ => None,
    }
}

/// `path` relative to `root` with `/` as separator, or the lossy text of `path` if a component is not UTF-8.
fn relative_path(root: &Path, path: &Path) -> std::result::Result<String, String> {
    let lossy = || path.to_string_lossy().into_owned();
    let relative = path.strip_prefix(root).map_err(|_| lossy())?;
    let mut text = String::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(lossy());
        };
        let name = name.to_str().ok_or_else(|| {
            relative
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        })?;
        if !text.is_empty() {
            text.push('/');
        }
        text.push_str(name);
    }
    Ok(text)
}

fn kind_of(metadata: &Metadata) -> FileKind {
    if metadata.file_type().is_symlink() {
        FileKind::Symlink
    } else if is_executable(metadata) {
        FileKind::Executable
    } else {
        FileKind::Regular
    }
}

/// git's rule: executable if the owner may execute it.
#[cfg(unix)]
fn is_executable(metadata: &Metadata) -> bool {
    std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o100 != 0
}

/// Windows has no executable bit: every file is stored as a regular file.
#[cfg(windows)]
fn is_executable(_metadata: &Metadata) -> bool {
    false
}

fn nanos_since_epoch(time: SystemTime) -> i64 {
    let nanos = |duration: Duration| i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => nanos(after),
        Err(before) => -nanos(before.duration()),
    }
}

enum Content {
    Bytes(Vec<u8>),
    /// Deleted since the walk: the file is not in this snapshot.
    Gone,
    TooLarge(u64),
}

/// The content of `file`: the bytes of a regular file, or the target of a symlink (never followed).
fn read(root: &Path, file: &Found) -> io::Result<Content> {
    let path = root.join(&file.path);
    let result = if file.kind == FileKind::Symlink {
        fs::read_link(&path).and_then(|target| link_bytes(&target))
    } else {
        safe_fs::open_regular_file(&path).and_then(|opened| {
            let mut bytes = Vec::with_capacity(usize::try_from(file.size).unwrap_or(0));
            opened.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        })
    };
    match result {
        Ok(bytes) if bytes.len() as u64 > MAX_FILE_BYTES => {
            Ok(Content::TooLarge(bytes.len() as u64))
        }
        Ok(bytes) => Ok(Content::Bytes(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Content::Gone),
        Err(error) => Err(error),
    }
}

/// A symlink target as git stores it: the raw bytes on Unix, `/`-separated text on Windows.
#[cfg(unix)]
fn link_bytes(target: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(target.as_os_str().as_bytes().to_vec())
}

#[cfg(windows)]
fn link_bytes(target: &Path) -> io::Result<Vec<u8>> {
    let target = target.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "the symlink target is not valid Unicode",
        )
    })?;
    Ok(target.replace('\\', "/").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::process::{Command, Stdio};

    use gix::objs::tree::EntryKind;

    use FileKind::Regular;

    /// A git project with an initialized `.yalper/`, the way `yalper init` leaves it.
    struct Project {
        dir: tempfile::TempDir,
        yalper: OwnedDir,
        store: Store,
    }

    /// An hour ago: files written with this mtime are not racy.
    fn an_hour_ago() -> SystemTime {
        SystemTime::now() - Duration::from_secs(3600)
    }

    impl Project {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            git(dir.path(), &["init", "--quiet"]);
            fs::write(dir.path().join(".git/info/exclude"), ".yalper/\n").unwrap();
            fs::create_dir(dir.path().join(YALPER_DIR)).unwrap();
            let yalper = OwnedDir::open(&dir.path().join(YALPER_DIR)).unwrap();
            ShadowStore::init(&yalper).unwrap();
            let store = Store::open(&yalper).unwrap();
            Self { dir, yalper, store }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// Writes a file with an mtime an hour ago, creating its directories.
        fn write(&self, path: &str, content: &str) {
            self.write_at(path, content, an_hour_ago());
        }

        fn write_at(&self, path: &str, content: &str, mtime: SystemTime) {
            let path = self.root().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            set_mtime(&path, mtime);
        }

        fn snapshot(&self) -> Snapshot {
            snapshot(&self.yalper, &self.store).unwrap()
        }

        /// Every file of `tree`: kind and content.
        fn files(&self, tree: ObjectId) -> BTreeMap<String, (FileKind, Vec<u8>)> {
            let shadow = ShadowStore::open(&self.yalper).unwrap();
            let mut files = BTreeMap::new();
            tree_files(&shadow, tree, "", &mut files);
            files
        }

        /// The files of `tree` as text. All must be regular files.
        fn texts(&self, tree: ObjectId) -> BTreeMap<String, String> {
            self.files(tree)
                .into_iter()
                .map(|(path, (kind, bytes))| {
                    assert_eq!(kind, Regular, "{path}");
                    (path, String::from_utf8(bytes).unwrap())
                })
                .collect()
        }
    }

    fn set_mtime(path: &Path, mtime: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn tree_files(
        shadow: &ShadowStore,
        tree: ObjectId,
        prefix: &str,
        files: &mut BTreeMap<String, (FileKind, Vec<u8>)>,
    ) {
        if tree.is_empty_tree() {
            return;
        }
        let tree = shadow.repo.find_object(tree).unwrap().into_tree();
        for entry in tree.decode().unwrap().entries {
            let path = format!("{prefix}{}", entry.filename);
            let kind = match entry.mode.kind() {
                EntryKind::Tree => {
                    tree_files(shadow, entry.oid.to_owned(), &format!("{path}/"), files);
                    continue;
                }
                EntryKind::Blob => Regular,
                EntryKind::BlobExecutable => FileKind::Executable,
                EntryKind::Link => FileKind::Symlink,
                EntryKind::Commit => panic!("no submodule entries are written"),
            };
            let data = shadow.repo.find_object(entry.oid).unwrap().data.clone();
            files.insert(path, (kind, data));
        }
    }

    fn texts(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files
            .iter()
            .map(|&(path, content)| (path.to_owned(), content.to_owned()))
            .collect()
    }

    /// Runs git in `project`, without background maintenance or file system monitor, and returns its output.
    fn git(project: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "core.autocrlf=false", "-c", "maintenance.auto=false"])
            .args(["-c", "gc.auto=0", "-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(project)
            .stderr(Stdio::inherit())
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn no_change_gives_the_same_tree_without_reading_a_file() {
        let project = Project::new();
        project.write("src/main.rs", "fn main() {}\n");
        project.write("src/lib/mod.rs", "pub mod a;\n");
        project.write("README.md", "# Demo\n");

        let first = project.snapshot();
        assert_eq!(
            first.changed,
            ["README.md", "src/lib/mod.rs", "src/main.rs"]
        );
        assert_eq!(first.files_read, 3);
        assert!(first.skipped.is_empty(), "{:?}", first.skipped);
        assert_eq!(
            project.texts(first.tree_id),
            texts(&[
                ("README.md", "# Demo\n"),
                ("src/lib/mod.rs", "pub mod a;\n"),
                ("src/main.rs", "fn main() {}\n"),
            ])
        );

        for _ in 0..2 {
            let again = project.snapshot();
            assert_eq!(again.tree_id, first.tree_id);
            assert!(again.changed.is_empty(), "{:?}", again.changed);
            assert_eq!(again.files_read, 0);
        }

        // The stat cache decides: an old file rewritten with the same size and mtime is not read. Only recent
        // changes are protected, by the racy window (next test).
        let main = project.root().join("src/main.rs");
        let mtime = fs::metadata(&main).unwrap().modified().unwrap();
        fs::write(&main, "fn main() {1}").unwrap();
        set_mtime(&main, mtime);
        assert_eq!(project.snapshot().tree_id, first.tree_id);
    }

    #[test]
    fn a_change_within_the_same_mtime_tick_is_detected() {
        let project = Project::new();
        let now = SystemTime::now();
        project.write_at("recent.txt", "aaaa", now);
        let first = project.snapshot();
        let cache = project.store.file_cache().unwrap().unwrap();
        assert!(cache.files[0].racy, "{cache:?}");

        // Same size and the very same mtime, as if rewritten within one timestamp tick.
        project.write_at("recent.txt", "bbbb", now);
        let second = project.snapshot();
        assert_eq!(second.changed, ["recent.txt"]);
        assert_ne!(second.tree_id, first.tree_id);
        assert_eq!(
            project.texts(second.tree_id),
            texts(&[("recent.txt", "bbbb")])
        );

        // A racy file is read on every snapshot, but an unchanged one is not reported as changed.
        let third = project.snapshot();
        assert_eq!(third.tree_id, second.tree_id);
        assert!(third.changed.is_empty());
        assert_eq!(third.files_read, 1);

        // Once its mtime is old enough, it is no longer racy and no longer read.
        set_mtime(&project.root().join("recent.txt"), an_hour_ago());
        assert_eq!(project.snapshot().files_read, 1);
        let cache = project.store.file_cache().unwrap().unwrap();
        assert!(!cache.files[0].racy, "{cache:?}");
        assert_eq!(project.snapshot().files_read, 0);
    }

    #[test]
    fn a_file_changed_by_a_child_process_shows_up() {
        let project = Project::new();
        project.write("config.txt", "before");
        project.write("other.txt", "same");
        let first = project.snapshot();

        // What a shell command run by the agent does: another process rewrites the file.
        let status = if cfg!(windows) {
            Command::new("cmd")
                .args(["/C", "echo after> config.txt"])
                .current_dir(project.root())
                .status()
        } else {
            Command::new("sh")
                .args(["-c", "printf after > config.txt"])
                .current_dir(project.root())
                .status()
        };
        assert!(status.unwrap().success());

        let second = project.snapshot();
        assert_eq!(second.changed, ["config.txt"]);
        let on_disk = fs::read_to_string(project.root().join("config.txt")).unwrap();
        assert!(on_disk.starts_with("after"), "{on_disk:?}");
        assert_eq!(
            project.texts(second.tree_id),
            texts(&[("config.txt", &on_disk), ("other.txt", "same")])
        );
        assert_ne!(second.tree_id, first.tree_id);
    }

    #[test]
    fn deletes_renames_and_new_directories_are_recorded() {
        let project = Project::new();
        project.write("a.txt", "a");
        project.write("b.txt", "b");
        project.write("old/deep/c.txt", "c");
        project.write("old/d.txt", "d");
        project.snapshot();

        fs::remove_file(project.root().join("a.txt")).unwrap();
        fs::rename(project.root().join("b.txt"), project.root().join("b2.txt")).unwrap();
        project.write("new/sub/e.txt", "e");
        let second = project.snapshot();
        assert_eq!(
            second.changed,
            ["a.txt", "b.txt", "b2.txt", "new/sub/e.txt"]
        );
        assert_eq!(
            project.texts(second.tree_id),
            texts(&[
                ("b2.txt", "b"),
                ("new/sub/e.txt", "e"),
                ("old/d.txt", "d"),
                ("old/deep/c.txt", "c"),
            ])
        );

        // A whole directory removed, and a file replaced by a directory of the same name.
        fs::remove_dir_all(project.root().join("old")).unwrap();
        fs::remove_file(project.root().join("b2.txt")).unwrap();
        project.write("b2.txt/inside.txt", "now a directory");
        let third = project.snapshot();
        assert_eq!(
            third.changed,
            ["b2.txt", "b2.txt/inside.txt", "old/d.txt", "old/deep/c.txt"]
        );
        assert_eq!(
            project.texts(third.tree_id),
            texts(&[
                ("b2.txt/inside.txt", "now a directory"),
                ("new/sub/e.txt", "e")
            ])
        );
        assert_eq!(project.snapshot().tree_id, third.tree_id);
    }

    #[test]
    fn ignored_files_git_directories_and_yalper_are_left_out() {
        let project = Project::new();
        project.write(".gitignore", "*.log\nbuild/\n");
        let exclude = project.root().join(".git/info/exclude");
        fs::write(&exclude, ".yalper/\nlocal-notes.txt\n").unwrap();
        // ripgrep's own ignore file is not git's: its patterns do not apply.
        project.write(".ignore", "src/\n");
        project.write("app.log", "log");
        project.write("build/out.bin", "binary");
        project.write("local-notes.txt", "excluded");
        project.write("src/main.rs", "fn main() {}");
        project.write(".prettierrc", "{}");
        // A nested repository and a submodule-style `.git` file: their `.git` is never snapshotted.
        project.write("nested/.git/config", "[core]");
        project.write("nested/lib.rs", "pub fn f() {}");
        project.write("module/.git", "gitdir: ../.git/modules/module\n");
        project.write("module/code.rs", "// code");

        let snapshot = project.snapshot();
        assert!(snapshot.skipped.is_empty(), "{:?}", snapshot.skipped);
        assert_eq!(
            project.texts(snapshot.tree_id).keys().collect::<Vec<_>>(),
            [
                ".gitignore",
                ".ignore",
                ".prettierrc",
                "module/code.rs",
                "nested/lib.rs",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn the_tree_is_the_one_git_writes_for_the_same_files() {
        let project = Project::new();
        project.write(".gitignore", "target/\n");
        project.write("target/debug/app", "ignored");
        project.write("Cargo.toml", "[package]\n");
        project.write("src/a-b.rs", "1");
        project.write("src/a.rs", "2");
        project.write("src/a/inner.rs", "3");
        project.write("src/a0.rs", "4");
        project.write("docs/guide/intro.md", "# Intro\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            project.write("run.sh", "#!/bin/sh\n");
            let permissions = fs::Permissions::from_mode(0o755);
            fs::set_permissions(project.root().join("run.sh"), permissions).unwrap();
            std::os::unix::fs::symlink("Cargo.toml", project.root().join("link")).unwrap();
        }

        let snapshot = project.snapshot();
        git(project.root(), &["add", "--all"]);
        let git_tree = git(project.root(), &["write-tree"]);
        assert_eq!(snapshot.tree_id.to_string(), git_tree.trim());
    }

    #[test]
    fn large_files_are_left_out_and_listed() {
        let project = Project::new();
        project.write("small.txt", "small");
        let large = "x".repeat(MAX_FILE_BYTES as usize + 1);
        project.write("large.bin", &large);
        let first = project.snapshot();
        assert_eq!(
            first.skipped,
            [Skipped {
                path: "large.bin".to_owned(),
                reason: SkipReason::TooLarge(MAX_FILE_BYTES + 1),
            }]
        );
        assert_eq!(
            project.texts(first.tree_id),
            texts(&[("small.txt", "small")])
        );

        // Under the limit it is stored; over it again, it leaves the snapshot.
        project.write("large.bin", "now small");
        let second = project.snapshot();
        assert_eq!(second.changed, ["large.bin"]);
        assert!(second.skipped.is_empty());
        project.write("large.bin", &large);
        let third = project.snapshot();
        assert_eq!(third.changed, ["large.bin"]);
        assert_eq!(third.tree_id, first.tree_id);
    }

    #[test]
    fn an_unreadable_file_keeps_its_previous_content() {
        let project = Project::new();
        project.write("locked.txt", "v1");
        project.write("other.txt", "other");
        let first = project.snapshot();
        project.write("locked.txt", "version 2");
        project.write("other.txt", "other 2");

        let Some(lock) = make_unreadable(&project.root().join("locked.txt")) else {
            return;
        };
        let second = project.snapshot();
        assert_eq!(second.changed, ["other.txt"]);
        assert_eq!(second.skipped.len(), 1, "{:?}", second.skipped);
        assert_eq!(second.skipped[0].path, "locked.txt");
        assert!(matches!(
            second.skipped[0].reason,
            SkipReason::Unreadable(_)
        ));
        assert_eq!(
            project.texts(second.tree_id),
            texts(&[("locked.txt", "v1"), ("other.txt", "other 2")])
        );
        assert_ne!(second.tree_id, first.tree_id);

        drop(lock);
        let third = project.snapshot();
        assert_eq!(third.changed, ["locked.txt"]);
        assert_eq!(
            project.texts(third.tree_id),
            texts(&[("locked.txt", "version 2"), ("other.txt", "other 2")])
        );
    }

    /// Restores read access when dropped.
    struct Unreadable {
        #[cfg(windows)]
        _handle: fs::File,
        #[cfg(unix)]
        path: std::path::PathBuf,
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = fs::Permissions::from_mode(0o644);
                fs::set_permissions(&self.path, permissions).unwrap();
            }
        }
    }

    /// Makes `path` unreadable until the result is dropped: a handle that shares nothing on Windows (like a
    /// file locked by another program), no read permission on Unix. `None` if the file stays readable
    /// (running as root).
    fn make_unreadable(path: &Path) -> Option<Unreadable> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let handle = fs::File::options()
                .read(true)
                .share_mode(0)
                .open(path)
                .unwrap();
            Some(Unreadable { _handle: handle })
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
            let unreadable = Unreadable {
                path: path.to_owned(),
            };
            fs::read(path).is_err().then_some(unreadable)
        }
    }

    #[test]
    fn a_damaged_or_planted_cache_is_not_trusted() {
        let project = Project::new();
        project.write("a.txt", "a");
        project.write("b.txt", "b");
        let first = project.snapshot();

        // Rows claiming other content (a valid but wrong blob id) plus one row that Yalper never writes: the
        // whole cache is dropped and the snapshot starts over from the files.
        let planted = project.store.file_cache().unwrap().unwrap();
        let wrong = ObjectId::empty_blob(gix::hash::Kind::Sha1).to_string();
        let mut rows = planted.files.clone();
        for row in &mut rows {
            row.oid = wrong.clone();
        }
        let mut outside = rows[0].clone();
        outside.path = "../outside.txt".to_owned();
        rows.push(outside);
        project
            .store
            .save_snapshot(&planted.tree_id, &rows, &[], true)
            .unwrap();

        let second = project.snapshot();
        assert_eq!(second.tree_id, first.tree_id);
        assert_eq!(second.files_read, 2);
        assert_eq!(
            project.texts(second.tree_id),
            texts(&[("a.txt", "a"), ("b.txt", "b")])
        );
        let cache = project.store.file_cache().unwrap().unwrap();
        assert_eq!(cache.files.len(), 2);
        assert!(cache.files.iter().all(|row| row.oid != wrong));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_stored_as_links_and_never_followed() {
        use std::os::unix::fs::symlink;
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        project.write("real.txt", "real");
        let root = project.root();
        symlink(outside.path(), root.join("dir-link")).unwrap();
        symlink(outside.path().join("secret.txt"), root.join("file-link")).unwrap();
        symlink("real.txt", root.join("relative-link")).unwrap();

        let snapshot = project.snapshot();
        let outside_bytes = outside.path().as_os_str().as_encoded_bytes().to_vec();
        let mut secret_bytes = outside_bytes.clone();
        secret_bytes.extend_from_slice(b"/secret.txt");
        assert_eq!(
            project.files(snapshot.tree_id),
            BTreeMap::from([
                ("dir-link".to_owned(), (FileKind::Symlink, outside_bytes)),
                ("file-link".to_owned(), (FileKind::Symlink, secret_bytes)),
                ("real.txt".to_owned(), (Regular, b"real".to_vec())),
                (
                    "relative-link".to_owned(),
                    (FileKind::Symlink, b"real.txt".to_vec())
                ),
            ])
        );

        // A file replaced by a link is a change of kind.
        fs::remove_file(root.join("real.txt")).unwrap();
        symlink("relative-link", root.join("real.txt")).unwrap();
        assert_eq!(project.snapshot().changed, ["real.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let project = Project::new();
        project.write("run.sh", "#!/bin/sh\n");
        let script = project.root().join("run.sh");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let first = project.snapshot();
        assert_eq!(
            project.files(first.tree_id)["run.sh"].0,
            FileKind::Executable
        );

        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        let second = project.snapshot();
        assert_eq!(second.changed, ["run.sh"]);
        assert_eq!(project.files(second.tree_id)["run.sh"].0, Regular);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_cannot_be_stored_is_skipped_without_failing_the_snapshot() {
        let project = Project::new();
        project.write("fine.txt", "fine");
        // An NTFS short name of `.git`: refused in trees on every platform, as git does.
        project.write("git~1/hooks/pre-commit", "#!/bin/sh");
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::OsStr::from_bytes(b"not-utf8-\xff.txt");
            fs::write(project.root().join(name), "x").unwrap();
        }

        let snapshot = project.snapshot();
        assert_eq!(
            project.texts(snapshot.tree_id),
            texts(&[("fine.txt", "fine")])
        );
        let skipped: Vec<&str> = snapshot.skipped.iter().map(|s| s.path.as_str()).collect();
        let mut expected = vec!["git~1"];
        if cfg!(target_os = "linux") {
            expected.push("not-utf8-\u{fffd}.txt");
        }
        assert_eq!(skipped, expected);
        let invalid = |skipped: &Skipped| matches!(skipped.reason, SkipReason::InvalidPath(_));
        assert!(snapshot.skipped.iter().all(invalid));
    }

    #[test]
    fn modes_round_trip() {
        for kind in [Regular, FileKind::Executable, FileKind::Symlink] {
            assert_eq!(FileKind::from_mode(i64::from(kind.mode())), Some(kind));
        }
        assert_eq!(Regular.mode(), 0o100644);
        assert_eq!(FileKind::from_mode(0o040000), None);
    }
}
