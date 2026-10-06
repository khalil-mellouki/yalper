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
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gix::ObjectId;
use ignore::{WalkBuilder, WalkState};

use super::excludes::Excludes;
use super::{Change, FileKind, Result, ShadowStore, validate_path};
use crate::hook::YALPER_DIR;
use crate::repo::Token;
use crate::safe_fs::{self, OwnedDir};
use crate::store::{CachedFile, FileCache, Store, WriterLock};

/// Files larger than this are left out of snapshots.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// A file whose mtime is this close to the start of the scan is read again by the next scan, because a change
/// made later within the same timestamp tick would leave its size and mtime unchanged (the same idea as git's
/// "racy" index entries). File systems with sub-second mtimes (NTFS, ext4, APFS) take them from a clock that
/// advances in ticks of at most about 16 ms (Windows), so a change after the scan started gets an mtime at most
/// one tick before that start, and a file changed and changed again within one tick has an mtime at most two
/// ticks before it. 100 ms is three times that. A wider window costs time on every step: each file changed
/// within it is read again (measured: about 3 ms per step on Windows with 2 s and steps 30 ms apart).
const RACY_WINDOW: Duration = Duration::from_millis(100);

/// The racy window for an mtime that is a whole number of seconds, as file systems that keep whole or even
/// seconds only (FAT, HFS+, some network shares) give.
const COARSE_RACY_WINDOW: Duration = Duration::from_secs(2);

/// Whether a file with mtime `mtime_ns`, seen by a scan that started at `started_ns`, must be read again by the
/// next scan (see [`RACY_WINDOW`]).
fn is_racy(mtime_ns: i64, started_ns: i64) -> bool {
    let window = if mtime_ns % 1_000_000_000 == 0 {
        COARSE_RACY_WINDOW
    } else {
        RACY_WINDOW
    };
    mtime_ns >= started_ns.saturating_sub(window.as_nanos() as i64)
}

/// Threads of the directory walk. Listing directories is mostly waiting on the file system, so a few threads
/// help, and more only cost time to start.
const WALK_THREADS: usize = 4;

/// Changed files the walk can read ahead of the thread that stores them. Each carries its content (at most
/// [`MAX_FILE_BYTES`]), so this bounds memory when many files changed at once.
const QUEUED_CONTENTS: usize = 16;

/// Directories nested deeper than this are not entered. Paths, and checking them against ignore rules, grow with
/// depth, so an absurdly deep tree would make every snapshot slow.
pub const MAX_DEPTH: usize = 256;

/// The result of [`snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The tree of the working tree after this step. Unchanged files keep the tree of the previous snapshot.
    pub tree_id: ObjectId,
    /// The tree this snapshot was built from: the previous snapshot, or the empty tree when the stat cache
    /// could not be used and the snapshot started over. `changed` lists the files that differ between it
    /// and `tree_id`.
    pub base_tree_id: ObjectId,
    /// Files added, modified (content, executable bit or file/symlink kind) or removed since the previous
    /// snapshot, sorted. Its length is the step's number of files changed.
    pub changed: Vec<String>,
    /// Files and directories left out of the snapshot or kept as they were, sorted by path.
    pub skipped: Vec<Skipped>,
    /// How many files were read. Files the stat cache knew to be unchanged are not read.
    pub files_read: usize,
}

impl Snapshot {
    /// One line about the skipped entries worth logging (unreadable or invalid paths: files over the size
    /// limit are expected), naming the first few. `None` if there are none.
    pub fn problems(&self) -> Option<String> {
        const NAMED: usize = 3;
        let problems: Vec<&Skipped> = self
            .skipped
            .iter()
            .filter(|skipped| !matches!(skipped.reason, SkipReason::TooLarge(_)))
            .collect();
        let first = problems.first()?;
        let mut line = format!("snapshot skipped {} path(s): {first}", problems.len());
        for skipped in problems.iter().skip(1).take(NAMED - 1) {
            line.push_str(&format!("; {skipped}"));
        }
        if problems.len() > NAMED {
            line.push_str(&format!("; and {} more", problems.len() - NAMED));
        }
        Some(line)
    }
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
    /// An ignore file (`.gitignore` or `info/exclude`) whose rules are not used, so the files it would ignore
    /// are snapshotted. The file itself is snapshotted like any other.
    IgnoreFileNotUsed(String),
    /// A directory [`MAX_DEPTH`] levels down: what is inside it is not in the snapshot.
    TooDeep,
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
            SkipReason::IgnoreFileNotUsed(reason) => {
                write!(f, "{path:?} not used as an ignore file: {reason}")
            }
            SkipReason::TooDeep => write!(
                f,
                "{path:?} not entered: nested more than {MAX_DEPTH} directories deep"
            ),
        }
    }
}

/// Takes a snapshot of the project that contains `yalper_dir`: every file that `.gitignore`, `.git/info/exclude`
/// and the user's global excludes do not ignore (see [`Excludes`]), except anything named `.git` or `.yalper`.
/// Symlinks are stored as symlinks (their target is never read), and on Unix the executable bit is kept.
///
/// [`take`] followed by [`Pending::save`], under `lock`. A failure because of the ignore rules budget is
/// remembered (see [`Base`]).
///
/// One file that cannot be taken never fails the snapshot: it is listed in [`Snapshot::skipped`] instead.
pub fn snapshot(yalper_dir: &OwnedDir, store: &Store, lock: &WriterLock) -> Result<Snapshot> {
    let base = Base::read(store, lock)?;
    match take(yalper_dir, store.token(), || Some(base)) {
        Ok(pending) => {
            pending.save(store, lock)?;
            Ok(pending.snapshot)
        }
        Err(error) => {
            remember_failure(store, lock, &error)?;
            Err(error)
        }
    }
}

/// What a snapshot starts from, read from the event log under the writer lock: the latest snapshot with its
/// stat cache, and whether the previous snapshots failed because the ignore rules go over the budget.
#[derive(Debug)]
pub struct Base {
    cache: Option<FileCache>,
    over_budget: Option<OverBudget>,
}

impl Base {
    pub fn read(store: &Store, _lock: &WriterLock) -> Result<Self> {
        Ok(Self {
            cache: store.file_cache()?,
            over_budget: store
                .meta(OVER_BUDGET_KEY)?
                .and_then(|text| OverBudget::parse(&text)),
        })
    }
}

/// A snapshot that [`take`] built and stored in the shadow store, not yet saved as the latest one.
#[derive(Debug)]
pub struct Pending {
    pub snapshot: Snapshot,
    rows: Vec<CachedFile>,
    removed: Vec<String>,
    start_over: bool,
    clears_over_budget: bool,
}

impl Pending {
    /// Saves the snapshot as the latest one with its stat cache rows, under `lock`. Inside
    /// [`Store::write_transaction`] it is part of that transaction.
    pub fn save(&self, store: &Store, lock: &WriterLock) -> crate::store::Result<()> {
        let snapshot = &self.snapshot;
        if self.start_over
            || snapshot.tree_id != snapshot.base_tree_id
            || !self.rows.is_empty()
            || !self.removed.is_empty()
        {
            store.save_snapshot(
                lock,
                &snapshot.tree_id.to_string(),
                &self.rows,
                &self.removed,
                self.start_over,
            )?;
        }
        if self.clears_over_budget {
            store.set_meta(OVER_BUDGET_KEY, None)?;
        }
        Ok(())
    }
}

/// Saves what the next snapshots need to know about `error`, a failure of [`take`]: the ignore files of a
/// project over the ignore rules budget, so the next snapshots do not walk it again for nothing.
pub fn remember_failure(
    store: &Store,
    _lock: &WriterLock,
    error: &super::Error,
) -> crate::store::Result<()> {
    if let super::Error::IgnoreRulesOverBudget {
        remember: Some(marker),
        ..
    } = error
    {
        store.set_meta(OVER_BUDGET_KEY, Some(marker))?;
    }
    Ok(())
}

/// The `meta` key under which a failure because of the ignore rules budget is remembered.
const OVER_BUDGET_KEY: &str = "ignore_rules_over_budget";

/// After a failure because of the ignore rules budget, the next snapshots fail at once, without walking the
/// project, as long as every ignore file that failure read is unchanged and for at most this long. The limit
/// covers what the check cannot see: a new `.gitignore` that ignores a directory holding many rules, or a
/// change to the user's global excludes file.
const OVER_BUDGET_RECHECK: Duration = Duration::from_secs(60);

/// A remembered failure because of the ignore rules budget: when it happened, and the size and mtime of
/// every ignore file it read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OverBudget {
    at_ms: i64,
    files: Vec<(String, u64, i64)>,
}

impl OverBudget {
    fn to_text(&self) -> String {
        serde_json::json!({ "at_ms": self.at_ms, "files": self.files }).to_string()
    }

    /// The value as [`to_text`](Self::to_text) writes it, or `None` (as if nothing was remembered).
    fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let files = value.get("files")?.as_array()?;
        Some(Self {
            at_ms: value.get("at_ms")?.as_i64()?,
            files: files
                .iter()
                .map(|file| {
                    let file = file.as_array()?;
                    match file.as_slice() {
                        [path, size, mtime_ns] => Some((
                            path.as_str()?.to_owned(),
                            size.as_u64()?,
                            mtime_ns.as_i64()?,
                        )),
                        _ => None,
                    }
                })
                .collect::<Option<_>>()?,
        })
    }

    /// Whether the failure is recent and no ignore file it read changed, so a new walk would fail the same
    /// way.
    fn still_holds(&self, now_ms: i64) -> bool {
        let age_ms = now_ms.saturating_sub(self.at_ms);
        (0..OVER_BUDGET_RECHECK.as_millis() as i64).contains(&age_ms)
            && self.files.iter().all(|(path, size, mtime_ns)| {
                fs::symlink_metadata(path).is_ok_and(|metadata| {
                    metadata.len() == *size
                        && metadata.modified().map(nanos_since_epoch).ok() == Some(*mtime_ns)
                })
            })
    }
}

/// Walks the project and stores every new or changed file and the new tree in the shadow store (opened with
/// `token`), starting from the base that `base` returns. Nothing is written to the event log: [`Pending::save`]
/// does that, so a snapshot that is abandoned (see `record`) leaves the latest snapshot and the stat cache as
/// they were. Any blobs and trees it already stored stay in the shadow store, which makes a new attempt faster.
///
/// The ignore files outside the project tree are read and the shadow store is opened before `base` is called,
/// so a caller running this on its own thread can read the base (under the writer lock) meanwhile. `base`
/// returns `None` when the caller gave up. The walk runs in its own threads while this thread stores what they
/// found.
pub fn take(
    yalper_dir: &OwnedDir,
    token: &Token,
    base: impl FnOnce() -> Option<Base>,
) -> Result<Pending> {
    let root = yalper_dir
        .path()
        .parent()
        .ok_or_else(|| io::Error::other("the .yalper directory has no parent"))?;
    let excludes = Arc::new(Excludes::new(root));
    let shadow = ShadowStore::open(yalper_dir, token)?;
    let base =
        base().ok_or_else(|| io::Error::other("the snapshot was given up before it started"))?;
    let started_ns = nanos_since_epoch(SystemTime::now());
    let started_ms = started_ns / 1_000_000;
    if let Some(over_budget) = &base.over_budget
        && over_budget.still_holds(started_ms)
    {
        return Err(super::Error::IgnoreRulesOverBudget {
            reason: format!(
                "{}; the project was not walked again: no ignore file changed since the last attempt",
                super::excludes::over_budget_reason()
            ),
            remember: None,
        });
    }

    // Without a usable cache, the snapshot starts over from the empty tree and every file is read.
    let previous = base.cache.and_then(check_cache);
    let start_over = previous.is_none();
    let (base_tree, cache) =
        previous.unwrap_or_else(|| (ObjectId::empty_tree(gix::hash::Kind::Sha1), HashMap::new()));

    let mut pending = Pending {
        snapshot: Snapshot {
            tree_id: base_tree,
            base_tree_id: base_tree,
            changed: Vec::new(),
            skipped: Vec::new(),
            files_read: 0,
        },
        rows: Vec::new(),
        removed: Vec::new(),
        start_over,
        clears_over_budget: base.over_budget.is_some(),
    };
    let snapshot = &mut pending.snapshot;
    let mut changes = Vec::new();
    // Files of this snapshot found by the walk. Under the kept prefixes (unreadable files and directories),
    // what the previous snapshot had stays.
    let mut present = HashSet::new();
    let mut kept_prefixes = Vec::new();

    thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        // The walk takes a permit before it sends a changed file's content, and gets it back once the content
        // is stored here.
        let (permits, taken_permits) = mpsc::sync_channel(QUEUED_CONTENTS);
        let (cache, walk_excludes) = (&cache, Arc::clone(&excludes));
        scope.spawn(move || walk(root, cache, walk_excludes, sender, permits));
        // If this loop stops early, dropping the receivers makes the walk stop too.
        for walked in receiver {
            if excludes.exceeded() {
                // The snapshot fails below: nothing more to store.
                break;
            }
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
            let (oid, len) = match check {
                Check::Unchanged => {
                    present.insert(file.path);
                    continue;
                }
                Check::Same { oid, len } => {
                    snapshot.files_read += 1;
                    (oid, len)
                }
                Check::Changed { bytes } => {
                    // Sent before the content, so it is already there.
                    let _ = taken_permits.recv();
                    snapshot.files_read += 1;
                    let oid = shadow.write_blob(&bytes)?;
                    changes.push(Change::Upsert {
                        path: file.path.clone(),
                        kind: file.kind,
                        blob: oid,
                    });
                    snapshot.changed.push(file.path.clone());
                    (oid, bytes.len() as u64)
                }
            };
            let row = Cached {
                size: file.size,
                mtime_ns: file.mtime_ns,
                kind: file.kind,
                oid,
                // A length that differs from the size seen by the walk means the file changed while it was
                // read.
                racy: is_racy(file.mtime_ns, started_ns) || len != file.size,
            };
            if cached != Some(&row) {
                pending.rows.push(row.to_cached_file(&file.path));
            }
            present.insert(file.path);
        }
        Ok(())
    })?;
    // Fails before anything is saved: the previous snapshot and the stat cache stay as they were.
    if excludes.exceeded() {
        let marker = OverBudget {
            at_ms: started_ms,
            files: excludes
                .seen()
                .into_iter()
                .map(|(path, size, mtime_ns)| (path.to_string_lossy().into_owned(), size, mtime_ns))
                .collect(),
        };
        return Err(super::Error::IgnoreRulesOverBudget {
            reason: super::excludes::over_budget_reason(),
            remember: Some(marker.to_text()),
        });
    }
    for (path, reason) in excludes.refused() {
        snapshot.skipped.push(Skipped {
            path: relative_path(root, &path).unwrap_or_else(|lossy| lossy),
            reason: SkipReason::IgnoreFileNotUsed(reason),
        });
    }

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
            pending.removed.push(path.clone());
        }
    }

    snapshot.tree_id = shadow.edit_tree(base_tree, &changes)?;
    snapshot.changed.sort_unstable();
    snapshot
        .skipped
        .sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(pending)
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

enum Walked {
    File(Found, Check),
    Skipped(Skipped),
}

/// What the walk found out about a file's content.
enum Check {
    /// Size, mtime and kind match a stat cache entry that is not racy: not read.
    Unchanged,
    /// Read and hashed by the walk thread: the same content and kind as the stat cache entry.
    Same { oid: ObjectId, len: u64 },
    /// Read by the walk thread: new, or its content or kind changed. Kept to be stored.
    Changed { bytes: Vec<u8> },
}

/// Sends every file and symlink of the project that is not ignored, with its size, mtime and kind, to
/// `sender`, using parallel threads. Files the stat cache cannot vouch for are read and hashed in those
/// threads, and only changed contents are sent, each after taking one of the bounded `permits`, so memory
/// stays bounded whatever the size of the project.
fn walk(
    root: &Path,
    cache: &HashMap<String, Cached>,
    excludes: Arc<Excludes>,
    sender: mpsc::Sender<Walked>,
    permits: mpsc::SyncSender<()>,
) {
    let filter_excludes = Arc::clone(&excludes);
    WalkBuilder::new(root)
        // Dotfiles are code too. Ignore files are read by `Excludes`, not by the walker (see there), and
        // never above the project, as git does.
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .follow_links(false)
        // Runs before an entry is queued, so an ignored directory is never listed. Its parent was visited
        // first, so the parent's `.gitignore` is known.
        .filter_entry(move |entry| {
            let name = entry.file_name();
            // At any depth: nested repositories and submodules have their own `.git`.
            let never = name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(YALPER_DIR);
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            !never && !filter_excludes.is_ignored(entry.path(), is_dir)
        })
        .threads(WALK_THREADS)
        .build_parallel()
        .run(|| {
            let (sender, permits, excludes) = (sender.clone(), permits.clone(), &excludes);
            Box::new(move |entry| {
                if excludes.exceeded() {
                    // The snapshot fails: no need to go on.
                    return WalkState::Quit;
                }
                let (walked, state) = visit(root, cache, excludes, entry);
                let Some(walked) = walked else {
                    return state;
                };
                let permit = match &walked {
                    Walked::File(_, Check::Changed { .. }) => permits.send(()),
                    _ => Ok(()),
                };
                match permit.and_then(|()| sender.send(walked).map_err(|_| mpsc::SendError(()))) {
                    Ok(()) => state,
                    // The snapshot stopped early: so does the walk.
                    Err(_) => WalkState::Quit,
                }
            })
        });
}

/// Handles one entry of the walk. The walker calls this for a directory before any entry inside it, so its
/// `.gitignore` is read in time.
fn visit(
    root: &Path,
    cache: &HashMap<String, Cached>,
    excludes: &Excludes,
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
        excludes.add_dir(entry.path());
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
    if file_type.is_dir() && entry.depth() >= MAX_DEPTH {
        return skip(path, SkipReason::TooDeep);
    }

    let kind = if file_type.is_dir() {
        None
    } else if file_type.is_file() || file_type.is_symlink() {
        Some(FileKind::Regular)
    } else {
        // FIFOs, sockets and devices: git does not store them either, and they are never opened.
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
        excludes.add_dir(entry.path());
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
    let cached = cache.get(&found.path);
    if cached.is_some_and(|cached| cached.matches(&found)) {
        return (
            Some(Walked::File(found, Check::Unchanged)),
            WalkState::Continue,
        );
    }
    if found.size > MAX_FILE_BYTES {
        return skip(found.path, SkipReason::TooLarge(found.size));
    }
    let check = match read(root, &found) {
        Ok(Content::Bytes(bytes)) => match blob_id(&bytes) {
            Ok(oid) if cached.is_some_and(|c| c.oid == oid && c.kind == found.kind) => {
                Check::Same {
                    oid,
                    len: bytes.len() as u64,
                }
            }
            Ok(_) => Check::Changed { bytes },
            Err(error) => return skip(found.path, SkipReason::Unreadable(error.to_string())),
        },
        Ok(Content::Gone) => return (None, WalkState::Continue),
        Ok(Content::TooLarge(size)) => return skip(found.path, SkipReason::TooLarge(size)),
        Err(error) => return skip(found.path, SkipReason::Unreadable(error.to_string())),
    };
    (Some(Walked::File(found, check)), WalkState::Continue)
}

/// The id `bytes` get as a blob, without writing anything.
fn blob_id(bytes: &[u8]) -> io::Result<ObjectId> {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .map_err(|error| io::Error::other(error.to_string()))
}

/// A walk error that may hide files: an unreadable directory or file. Errors about paths outside the
/// project are left out. Without a path, the whole project is reported, so nothing is taken as removed.
fn walk_error(root: &Path, error: &ignore::Error) -> Option<Walked> {
    let io_error = error.io_error()?;
    let path = match error_path(error) {
        Some(path) if !path.starts_with(root) => return None,
        Some(path) => relative_path(root, path).unwrap_or_default(),
        None => String::new(),
    };
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

pub(super) fn nanos_since_epoch(time: SystemTime) -> i64 {
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
    use crate::repo::Token;
    use crate::snapshot::excludes::{MAX_IGNORE_FILE_BYTES, MAX_IGNORE_WEIGHT, weight};
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
            let token = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
            ShadowStore::init(&yalper, &token).unwrap();
            let store = Store::open(&yalper, &token).unwrap();
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
            snapshot(&self.yalper, &self.store, &self.lock()).unwrap()
        }

        fn snapshot_result(&self) -> Result<Snapshot> {
            snapshot(&self.yalper, &self.store, &self.lock())
        }

        fn lock(&self) -> WriterLock {
            WriterLock::acquire(&self.yalper, crate::store::LOCK_TIMEOUT).unwrap()
        }

        /// Every file of `tree`: kind and content.
        fn files(&self, tree: ObjectId) -> BTreeMap<String, (FileKind, Vec<u8>)> {
            let shadow = ShadowStore::open(&self.yalper, self.store.token()).unwrap();
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
    fn whole_second_mtimes_get_the_wider_racy_window() {
        const MS: i64 = 1_000_000;
        let started = 1_700_000_000_500 * MS;
        // Sub-second mtimes: racy within 100 ms of the start, and after it.
        assert!(is_racy(started - 99 * MS, started));
        assert!(is_racy(started + 5 * MS, started));
        assert!(!is_racy(started - 101 * MS, started));
        // Whole seconds (FAT, HFS+): racy within 2 s.
        let whole = 1_700_000_000_000 * MS;
        assert!(is_racy(whole, started));
        assert!(is_racy(whole - 1_000 * MS, started));
        assert!(!is_racy(whole - 2_000 * MS, started));
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
            .save_snapshot(&project.lock(), &planted.tree_id, &rows, &[], true)
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

    /// Runs `test` in a thread and fails if it does not finish within 20 seconds.
    #[cfg(unix)]
    fn without_hanging(test: impl FnOnce() + Send + 'static) {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            test();
            sender.send(()).unwrap();
        });
        receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("the snapshot did not finish");
    }

    /// The paths of the regular files in the latest snapshot.
    fn paths(project: &Project, snapshot: &Snapshot) -> Vec<String> {
        project.texts(snapshot.tree_id).into_keys().collect()
    }

    #[test]
    fn deeper_gitignore_files_take_precedence() {
        let project = Project::new();
        project.write(".gitignore", "*.log\ngenerated/\n");
        project.write("keep/.gitignore", "!important.log\n*.tmp\n");
        project.write("a.log", "x");
        project.write("a.tmp", "x");
        project.write("keep/important.log", "x");
        project.write("keep/other.log", "x");
        project.write("keep/b.tmp", "x");
        project.write("generated/out.rs", "x");
        project.write("keep/deep/c.tmp", "x");
        let snapshot = project.snapshot();
        assert_eq!(
            paths(&project, &snapshot),
            [
                ".gitignore",
                "a.tmp",
                "keep/.gitignore",
                "keep/important.log"
            ]
        );
    }

    #[test]
    fn an_oversized_gitignore_is_not_used() {
        let project = Project::new();
        let mut huge = "*.txt\n".to_owned();
        while huge.len() as u64 <= MAX_IGNORE_FILE_BYTES {
            huge.push_str("# padding to make the file larger than the limit\n");
        }
        project.write(".gitignore", &huge);
        project.write("kept.txt", "kept");
        let snapshot = project.snapshot();
        assert_eq!(paths(&project, &snapshot), [".gitignore", "kept.txt"]);
        assert_eq!(snapshot.skipped.len(), 1, "{:?}", snapshot.skipped);
        assert_eq!(snapshot.skipped[0].path, ".gitignore");
        let problems = snapshot.problems().unwrap();
        assert!(
            problems.contains("not used as an ignore file") && problems.contains("bytes"),
            "{problems}"
        );
    }

    #[test]
    fn ignore_rules_over_the_budget_fail_the_snapshot_and_keep_the_previous_one() {
        let project = Project::new();
        // Rules worth just over half of the budget each: one file fits, two do not.
        let rules = |extra: &str| -> String {
            let mut rules = extra.to_owned();
            let mut total = weight(extra.trim());
            for i in 0.. {
                if total > MAX_IGNORE_WEIGHT / 2 {
                    break;
                }
                let pattern = format!("x{i}*");
                total += weight(&pattern);
                rules.push_str(&pattern);
                rules.push('\n');
            }
            rules
        };
        project.write(".gitignore", &rules("*.log\n"));
        project.write("a.log", "x");
        project.write("sub/c.tmp", "x");
        let first = project.snapshot();
        assert_eq!(paths(&project, &first), [".gitignore", "sub/c.tmp"]);
        let before = project.store.file_cache().unwrap();

        project.write("sub/.gitignore", &rules("*.tmp\n"));
        project.write("new.txt", "x");
        let lock = project.lock();
        let error = snapshot(&project.yalper, &project.store, &lock).unwrap_err();
        let message = error.to_string();
        assert!(
            matches!(error, super::super::Error::IgnoreRulesOverBudget { .. }),
            "{message}"
        );
        assert!(
            message.starts_with("ignore rules exceed the budget: "),
            "{message}"
        );
        assert!(
            message.contains("pattern weight") && !message.contains("sub"),
            "{message}"
        );
        assert_eq!(project.store.file_cache().unwrap(), before);
        drop(lock);

        // Every step fails the same way until the rules fit again, without walking the project again while no
        // ignore file that the failure read changes.
        let again = project.snapshot_result().unwrap_err().to_string();
        assert!(again.contains("not walked again"), "{again}");
        assert_eq!(project.store.file_cache().unwrap(), before);
        project.write("sub/.gitignore", &rules("*.tmp\n*.bak\n"));
        let walked = project.snapshot_result().unwrap_err().to_string();
        assert!(!walked.contains("not walked again"), "{walked}");
        assert!(project.snapshot_result().is_err());

        fs::remove_file(project.root().join("sub/.gitignore")).unwrap();
        let fitting = project.snapshot();
        assert_eq!(fitting.changed, ["new.txt"]);
        // The failure is forgotten.
        assert_eq!(project.store.meta(OVER_BUDGET_KEY).unwrap(), None);
    }

    #[test]
    fn a_remembered_budget_failure_is_checked_again_after_a_minute() {
        let project = Project::new();
        project.write(".gitignore", "*.log\n");
        let gitignore = project.root().join(".gitignore");
        let metadata = fs::metadata(&gitignore).unwrap();
        let mtime_ns = nanos_since_epoch(metadata.modified().unwrap());
        let at_ms = 1_000_000;
        let marker = OverBudget {
            at_ms,
            files: vec![(
                gitignore.to_string_lossy().into_owned(),
                metadata.len(),
                mtime_ns,
            )],
        };
        assert_eq!(OverBudget::parse(&marker.to_text()), Some(marker.clone()));
        assert_eq!(OverBudget::parse("{\"at_ms\": 1}"), None);

        assert!(marker.still_holds(at_ms));
        assert!(marker.still_holds(at_ms + 59_999));
        assert!(!marker.still_holds(at_ms + 60_000));
        // The clock went back.
        assert!(!marker.still_holds(at_ms - 1));
        project.write(".gitignore", "*.log\n*.tmp\n");
        assert!(!marker.still_holds(at_ms));
        fs::remove_file(&gitignore).unwrap();
        assert!(!marker.still_holds(at_ms));
    }

    #[test]
    fn a_linked_gitignore_or_exclude_file_is_not_read() {
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        let rules = outside.path().join("rules");
        fs::write(&rules, "*\n").unwrap();
        project.write("kept.txt", "kept");
        project.write("sub/also-kept.txt", "kept");
        if !symlink_file(&rules, &project.root().join(".gitignore")) {
            return;
        }
        assert!(symlink_file(&rules, &project.root().join("sub/.gitignore")));
        let exclude = project.root().join(".git/info/exclude");
        fs::remove_file(&exclude).unwrap();
        assert!(symlink_file(&rules, &exclude));

        let snapshot = project.snapshot();
        let mut files: Vec<String> = project.files(snapshot.tree_id).into_keys().collect();
        files.sort();
        // The links themselves are part of the project, as git would commit them.
        assert_eq!(
            files,
            [
                ".gitignore",
                "kept.txt",
                "sub/.gitignore",
                "sub/also-kept.txt"
            ]
        );
    }

    /// Creates a file symlink. Returns false on Windows when the user may not create symlinks.
    fn symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);
        match result {
            Ok(()) => true,
            Err(_) if cfg!(windows) => false,
            Err(error) => panic!("cannot create symlink: {error}"),
        }
    }

    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        let status = Command::new("mkfifo").arg(path).status().unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn a_gitignore_that_is_a_fifo_or_a_link_to_one_does_not_hang() {
        without_hanging(|| {
            let project = Project::new();
            project.write("kept.txt", "kept");
            let fifo = project.root().join("fifo");
            mkfifo(&fifo);
            std::os::unix::fs::symlink(&fifo, project.root().join(".gitignore")).unwrap();
            fs::create_dir(project.root().join("sub")).unwrap();
            mkfifo(&project.root().join("sub/.gitignore"));
            let snapshot = project.snapshot();
            let files: Vec<String> = project.files(snapshot.tree_id).into_keys().collect();
            assert_eq!(files, [".gitignore", "kept.txt"]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_in_the_project_is_left_out_without_hanging() {
        without_hanging(|| {
            let project = Project::new();
            project.write("kept.txt", "kept");
            mkfifo(&project.root().join("pipe"));
            let snapshot = project.snapshot();
            assert_eq!(paths(&project, &snapshot), ["kept.txt"]);
            assert!(snapshot.skipped.is_empty(), "{:?}", snapshot.skipped);
        });
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_stored_as_a_link_and_never_walked() {
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        project.write("kept.txt", "kept");
        let status = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(project.root().join("junction"))
            .arg(outside.path())
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let snapshot = project.snapshot();
        let files = project.files(snapshot.tree_id);
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["junction", "kept.txt"],
            "{:?}",
            snapshot.skipped
        );
        let (kind, target) = &files["junction"];
        assert_eq!(*kind, FileKind::Symlink);
        let target = String::from_utf8(target.clone()).unwrap();
        let name = outside.path().file_name().unwrap().to_str().unwrap();
        assert!(target.ends_with(name), "{target}");
    }

    #[test]
    fn problems_are_summed_up_in_one_line() {
        let skipped = |path: &str, reason| Skipped {
            path: path.to_owned(),
            reason,
        };
        let unreadable = || SkipReason::Unreadable("denied".to_owned());
        let mut snapshot = Snapshot {
            tree_id: ObjectId::empty_tree(gix::hash::Kind::Sha1),
            base_tree_id: ObjectId::empty_tree(gix::hash::Kind::Sha1),
            changed: Vec::new(),
            skipped: vec![skipped("big.bin", SkipReason::TooLarge(MAX_FILE_BYTES + 1))],
            files_read: 0,
        };
        assert_eq!(snapshot.problems(), None);
        for name in ["a", "b", "c", "d", "e"] {
            snapshot.skipped.push(skipped(name, unreadable()));
        }
        let line = snapshot.problems().unwrap();
        assert!(
            line.starts_with("snapshot skipped 5 path(s): \"a\" kept"),
            "{line}"
        );
        assert!(
            line.contains("\"c\" kept") && !line.contains("\"d\""),
            "{line}"
        );
        assert!(line.ends_with("; and 2 more"), "{line}");
        assert!(!line.contains("big.bin"), "{line}");
    }

    /// The files git lists as untracked and not ignored, sorted.
    fn git_sees(project: &Project) -> Vec<String> {
        let output = git(
            project.root(),
            &["ls-files", "-z", "--others", "--exclude-standard"],
        );
        let mut paths: Vec<String> = output
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect();
        paths.sort();
        paths
    }

    /// Checks that the snapshot holds exactly the files git does not ignore.
    fn same_as_git(case: &str, setup: impl Fn(&Project)) {
        let project = Project::new();
        setup(&project);
        let snapshot = project.snapshot();
        let ours: Vec<String> = project.files(snapshot.tree_id).into_keys().collect();
        assert_eq!(ours, git_sees(&project), "case {case}");
    }

    #[test]
    fn ignore_rules_match_git() {
        same_as_git("byte order mark", |p| {
            p.write(".gitignore", "\u{feff}node_modules/\n*.tmp\n");
            p.write("node_modules/x/index.js", "x");
            p.write("a.tmp", "x");
            p.write("a.rs", "x");
        });
        same_as_git("directory only, anchored, middle slash", |p| {
            p.write(".gitignore", "foo/\n/rootonly.txt\na/b\ndoc/frotz/\n");
            p.write("foo", "a file named foo");
            p.write("x/foo/in.txt", "x");
            p.write("rootonly.txt", "x");
            p.write("sub/rootonly.txt", "x");
            p.write("a/b", "x");
            p.write("x/a/b", "x");
            p.write("doc/frotz/f.txt", "x");
            p.write("x/doc/frotz/f.txt", "x");
        });
        same_as_git("double stars", |p| {
            let rules = "**/deep.txt\ndocs/**/*.tmp\nlib/**\n!lib/keep.txt\n**/foo/bar\nabc/**/\n";
            p.write(".gitignore", rules);
            p.write("a/b/c/deep.txt", "x");
            p.write("deep.txt", "x");
            p.write("docs/x.tmp", "x");
            p.write("docs/a/b/x.tmp", "x");
            p.write("lib/keep.txt", "x");
            p.write("lib/other.txt", "x");
            p.write("lib/sub/keep.txt", "x");
            p.write("z/foo/bar", "x");
            p.write("foo/bar", "x");
            p.write("abc/file.txt", "x");
            p.write("abc/d/file.txt", "x");
        });
        same_as_git("no re-include inside an ignored directory", |p| {
            p.write(
                ".gitignore",
                "ign/\n!ign/keep.txt\nstar/*\n!star/keep.txt\n",
            );
            p.write("ign/keep.txt", "x");
            p.write("ign/other.txt", "x");
            p.write("star/keep.txt", "x");
            p.write("star/other.txt", "x");
            p.write("star/d/keep.txt", "x");
            p.write("ign2/.gitignore", "!*\n");
            p.write("ign2/keep.txt", "x");
            p.write("x/.gitignore", "../ign2/\n");
        });
        same_as_git("spaces, escapes, CRLF and comments", |p| {
            let rules =
                "trail.txt   \r\n\\#hash.txt\r\n\\!bang.txt\r\ncrlf.txt\r\n# comment.txt\r\n";
            p.write(".gitignore", rules);
            p.write("trail.txt", "x");
            p.write("#hash.txt", "x");
            p.write("!bang.txt", "x");
            p.write("crlf.txt", "x");
            p.write("comment.txt", "x");
        });
        same_as_git("ranges", |p| {
            // POSIX classes such as `[[:digit:]]` are a known difference (see `excludes`).
            p.write(".gitignore", "[a-c]x.txt\n[!a]y.txt\n");
            p.write("bx.txt", "x");
            p.write("dx.txt", "x");
            p.write("ay.txt", "x");
            p.write("by.txt", "x");
        });
        same_as_git("letter case", |p| {
            p.write(".gitignore", "*.LOG\nBuild/\nSecret.txt\n");
            p.write("x.log", "x");
            p.write("build/out.txt", "x");
            p.write("secret.txt", "x");
            p.write("kept.txt", "x");
        });
        same_as_git("info/exclude and .gitignore precedence", |p| {
            let exclude = p.root().join(".git/info/exclude");
            fs::write(&exclude, ".yalper/\ny.txt\n!x.txt\n*.ex\n").unwrap();
            p.write(".gitignore", "x.txt\n!y.txt\n");
            p.write("sub/.gitignore", "!*.ex\n");
            p.write("x.txt", "x");
            p.write("y.txt", "x");
            p.write("a.ex", "x");
            p.write("sub/b.ex", "x");
        });
        same_as_git("patterns relative to a subdirectory", |p| {
            p.write("sub/.gitignore", "/x\ny/z.txt\n*.o\n");
            p.write("x", "x");
            p.write("sub/x", "x");
            p.write("sub/q/x", "x");
            p.write("sub/y/z.txt", "x");
            p.write("y/z.txt", "x");
            p.write("sub/deeper/m.o", "x");
            p.write("m.o", "x");
        });
        same_as_git("everything but directories and sources", |p| {
            p.write(".gitignore", "*\n!*/\n!*.rs\n");
            p.write("a.rs", "x");
            p.write("a.txt", "x");
            p.write("d/b.rs", "x");
            p.write("d/b.txt", "x");
        });
        same_as_git("directory pattern and a file of that name", |p| {
            p.write(".gitignore", "cache/\n");
            p.write("a/cache/x", "x");
            p.write("b/cache", "x");
        });
    }

    #[test]
    fn a_very_deep_tree_is_cut_off_and_reported() {
        let project = Project::new();
        project.write(".gitignore", "*.log\n");
        let nested = |depth: usize| vec!["d"; depth].join("/");
        project.write(&format!("{}/mid.txt", nested(100)), "x");
        project.write(&format!("{}/deep.txt", nested(MAX_DEPTH + 40)), "x");

        let start = std::time::Instant::now();
        let snapshot = project.snapshot();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(
            paths(&project, &snapshot),
            [".gitignore".to_owned(), format!("{}/mid.txt", nested(100))]
        );
        assert_eq!(
            snapshot.skipped,
            [Skipped {
                path: nested(MAX_DEPTH),
                reason: SkipReason::TooDeep,
            }]
        );
        let problems = snapshot.problems().unwrap();
        assert!(
            problems.starts_with("snapshot skipped 1 path(s): "),
            "{problems}"
        );
        assert!(problems.contains("not entered"), "{problems}");
    }
}
