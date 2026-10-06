//! The shadow git store: `.yalper/snapshots.git`, a bare repository that holds the working tree after each
//! step as a git tree.
//!
//! Only blobs and trees are written: no commits, no refs, no index (tree ids are kept in the event log).
//! Files are content-addressed, so a file that did not change is never stored twice, and a new snapshot is
//! built from the previous tree by editing only the paths that changed.
//!
//! The store is a separate repository opened by its explicit path with isolated options: no system, global
//! or user git configuration and no `GIT_*` environment variables are read, and the user's own `.git` is
//! never opened.
//!
//! [`snapshot`] walks the working tree and records it in the store.

mod excludes;
mod scan;

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use gix::bstr::{BStr, ByteSlice};
use gix::error::ErrorExt;
use gix::objs::tree::EntryKind;
use gix::objs::{Find, FindExt};
use gix::validate::path::component;
use gix::{ObjectId, oid};

use crate::repo::Token;
use crate::safe_fs::OwnedDir;

pub use scan::{
    Base, MAX_FILE_BYTES, Pending, SkipReason, Skipped, Snapshot, remember_failure, snapshot, take,
};

/// The shadow repository inside `.yalper/`.
pub const SNAPSHOTS_DIR: &str = "snapshots.git";

/// The store's whole `config`: written by [`ShadowStore::init`] and required byte for byte by
/// [`ShadowStore::open`]. Snapshots are only referenced from the event log, never from refs, so automatic gc
/// is off and unreachable objects never expire: running `git gc` on the store cannot delete a snapshot.
/// It also holds the init token of the `.yalper/` the store was created in (see [`crate::repo`]), so a store
/// that a pulled commit wrote over the local one is refused.
fn store_config(token: &Token) -> String {
    format!(
        "[core]\n\
         \trepositoryformatversion = 0\n\
         \tbare = true\n\
         [gc]\n\
         \tauto = 0\n\
         \tpruneExpire = never\n\
         [yalper]\n\
         \tinitToken = {token}\n"
    )
}

/// `HEAD` holds one line like `ref: refs/heads/main`; anything larger was not written by Yalper.
const MAX_HEAD_BYTES: u64 = 256;

/// The most new trees of one snapshot written in parallel, one thread each (see [`ShadowStore::edit_tree`]).
const MAX_PARALLEL_TREE_WRITES: usize = 8;

/// Leftover temporary object files older than this are deleted by [`ShadowStore::remove_stale_temp_files`].
const STALE_TEMP_FILE_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// The most memory gix may allocate for one object, the same cap as for a hook payload. Files over 10 MiB are
/// never snapshotted, so a larger object can only come from corruption.
const ALLOC_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Path rules for names stored in trees: git's defaults for the platform (`core.protectNTFS` everywhere,
/// `core.protectHFS` on macOS, Windows names on Windows). They are fixed here rather than read from the
/// store's configuration, so a crafted `config` cannot turn them off.
const PATH_RULES: component::Options = component::Options {
    protect_windows: cfg!(windows),
    protect_hfs: cfg!(target_os = "macos"),
    protect_ntfs: true,
};

/// How a file is stored in a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Executable,
    /// The blob holds the link target.
    Symlink,
}

impl FileKind {
    /// The git file mode, as kept in the stat cache.
    pub fn mode(self) -> u32 {
        EntryKind::from(self) as u32
    }

    /// The kind with git file mode `mode`, if it is one of the three file modes.
    pub fn from_mode(mode: i64) -> Option<Self> {
        [Self::Regular, Self::Executable, Self::Symlink]
            .into_iter()
            .find(|kind| i64::from(kind.mode()) == mode)
    }
}

impl From<FileKind> for EntryKind {
    fn from(kind: FileKind) -> Self {
        match kind {
            FileKind::Regular => EntryKind::Blob,
            FileKind::Executable => EntryKind::BlobExecutable,
            FileKind::Symlink => EntryKind::Link,
        }
    }
}

/// One change to apply to a tree. Paths are relative to the project root and use `/` as separator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Adds the file at `path`, or replaces whatever is there (a file, or a directory with all its content).
    /// Missing parent directories are created.
    Upsert {
        path: String,
        kind: FileKind,
        blob: ObjectId,
    },
    /// Removes the file or directory at `path`, if present. Directories left empty are removed too.
    Remove { path: String },
}

/// The files that differ between two trees, each list sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangedPaths {
    pub added: Vec<String>,
    /// Content, mode (executable bit) or kind (file or symlink) changed.
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl ChangedPaths {
    /// The number of files changed.
    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Git(gix::Error),
    /// Reading or saving the stat cache failed.
    Store(crate::store::Error),
    /// A path that cannot be stored in a tree, for example with an empty, `..` or `.git` component.
    InvalidPath {
        path: String,
        reason: String,
    },
    /// A change refers to a blob that is not in the store.
    MissingObject(ObjectId),
    /// The store contains something Yalper does not create (a link, alternates, a `commondir` file), so it is
    /// not used.
    UnexpectedLayout(String),
    /// The ignore files of the project go over the budget for one snapshot, so no snapshot is taken (see
    /// `snapshot::excludes`). `remember` is what to save so the next snapshots fail at once while the ignore
    /// files stay the same (see [`scan::remember_failure`]); `None` when this failure was such a fast one.
    IgnoreRulesOverBudget {
        reason: String,
        remember: Option<String>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Git(error) => write!(f, "snapshot store error: {error}"),
            Self::Store(error) => write!(f, "{error}"),
            Self::InvalidPath { path, reason } => {
                write!(f, "cannot store the path {path:?} in a snapshot: {reason}")
            }
            Self::MissingObject(id) => write!(f, "the snapshot store has no object {id}"),
            Self::IgnoreRulesOverBudget { reason, .. } => {
                write!(f, "ignore rules exceed the budget: {reason}")
            }
            Self::UnexpectedLayout(what) => write!(
                f,
                "the snapshot store contains {what}, which Yalper does not create, so it is not used"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Git(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::InvalidPath { .. }
            | Self::MissingObject(_)
            | Self::UnexpectedLayout(_)
            | Self::IgnoreRulesOverBudget { .. } => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::store::Error> for Error {
    fn from(error: crate::store::Error) -> Self {
        Self::Store(error)
    }
}

impl From<gix::Error> for Error {
    fn from(error: gix::Error) -> Self {
        Self::Git(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// An open shadow store.
pub struct ShadowStore {
    repo: gix::Repository,
    /// Held open: on Windows it keeps the store from being renamed or replaced while in use.
    dir: OwnedDir,
}

impl fmt::Debug for ShadowStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShadowStore")
            .field("path", &self.repo.git_dir())
            .finish()
    }
}

impl ShadowStore {
    /// Creates an empty store in `yalper_dir`, bound to its init token `token`, and opens it. Fails if
    /// anything, even a link, already exists at its path.
    pub fn init(yalper_dir: &OwnedDir, token: &Token) -> Result<Self> {
        let path = yalper_dir.path().join(SNAPSHOTS_DIR);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", path.display()),
                )
                .into());
            }
            Err(error) => return Err(error.into()),
        }
        // `gix::init_bare` would read the user's global configuration (for `init.defaultBranch`).
        gix::ThreadSafeRepository::init_opts(
            &path,
            gix::create::Kind::Bare,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )?;
        // A fixed config instead of the probed one (its file system settings only matter for a work tree), and
        // no ref directories: `open` requires `refs/` to be empty.
        fs::write(path.join("config"), store_config(token))?;
        fs::remove_dir(path.join("refs").join("heads"))?;
        fs::remove_dir(path.join("refs").join("tags"))?;
        Self::open(yalper_dir, token)
    }

    /// Opens the store in `yalper_dir`, whose init token is `token`.
    ///
    /// Before gix reads anything, the layout must be exactly what [`init`](Self::init) and object writes
    /// produce: a real directory (on Unix owned by the current user), Yalper's own `config` with `token` in it,
    /// a small `HEAD`, no refs, packs, alternates or `commondir`, and no link in `objects/`. Links, alternates
    /// and `commondir` could send writes or reads to another repository, such as the user's own `.git`; refs
    /// could replace objects. Every object read is also checked against its id, see [`Verified`]. Remaining
    /// gap, outside the threat model: another process of the same user could change the store after these
    /// checks.
    pub fn open(yalper_dir: &OwnedDir, token: &Token) -> Result<Self> {
        let dir = OwnedDir::open(&yalper_dir.path().join(SNAPSHOTS_DIR))?;
        check_layout(&dir, &store_config(token))?;
        let options = gix::open::Options::isolated()
            .open_path_as_is(true)
            // Full trust skips gix's own ownership check (slow on Windows; `OwnedDir` checks ownership on
            // Unix) and its reduced-trust defaults. Of those, only the allocation limit matters for this store
            // (its config is Yalper's own, compared above), so it is set explicitly.
            .with(gix::sec::Trust::Full)
            .config_overrides([format!("gitoxide.objects.allocLimit={ALLOC_LIMIT_BYTES}")]);
        let mut repo = gix::open_opts(dir.path(), options)?;
        // Every new blob or tree is first looked up and not found. By default each miss rescans the store's
        // pack directory, which this store never has (measured: 22 ms instead of 15 ms per 3-file step on
        // Windows). Loose objects are still found.
        repo.objects.refresh_never();
        Ok(Self { repo, dir })
    }

    /// Stores `bytes` as a blob and returns its id. Content already in the store is not written again.
    pub fn write_blob(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.write_object(gix::objs::Kind::Blob, bytes)
    }

    /// Writes `data` as a loose object of `kind`, unless the store already has it, and returns its id.
    ///
    /// gix's own writer streams into a temporary file in `objects/`, then moves it into place and makes it
    /// read-only, about 2 ms per object on Windows (measured). Here the object is compressed in memory and
    /// written with one call to a temporary file next to its final name, then renamed into place (see
    /// [`persist`]): about 1 ms.
    fn write_object(&self, kind: gix::objs::Kind, data: &[u8]) -> Result<ObjectId> {
        let (id, new) = self.prepare(kind, data)?;
        if let Some(new) = new {
            persist(&self.objects_dir(), &new)?;
        }
        Ok(id)
    }

    /// The id of `data` as an object of `kind` and, unless the store already has it, the object compressed
    /// for [`persist`].
    fn prepare(&self, kind: gix::objs::Kind, data: &[u8]) -> Result<(ObjectId, Option<NewObject>)> {
        use std::io::Write as _;
        let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind, data)
            .map_err(|error| io::Error::other(error.to_string()))?;
        if self.repo.has_object(id) {
            return Ok((id, None));
        }
        let mut zlib = gix::zlib::stream::deflate::Write::new(
            Vec::with_capacity(data.len() / 2 + 64),
            gix::zlib::Compression::BEST_SPEED,
        );
        zlib.write_all(&gix::objs::encode::loose_header(kind, data.len() as u64))?;
        zlib.write_all(data)?;
        zlib.flush()?;
        let compressed = zlib.into_inner();
        Ok((id, Some(NewObject { id, compressed })))
    }

    fn objects_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("objects")
    }

    /// Deletes the temporary object files (`objects/xx/tmp_obj_*`) older than an hour: left by hooks stopped
    /// at their deadline or killed while writing. Returns how many were deleted. Errors are ignored: a file
    /// that cannot be deleted now can be deleted another time.
    pub fn remove_stale_temp_files(&self) -> usize {
        let Ok(fan_outs) = fs::read_dir(self.objects_dir()) else {
            return 0;
        };
        let now = std::time::SystemTime::now();
        let mut removed = 0;
        for fan_out in fan_outs.flatten() {
            let name = fan_out.file_name();
            let is_fan_out = name.len() == 2
                && name
                    .to_str()
                    .is_some_and(|name| name.bytes().all(|b| b.is_ascii_hexdigit()));
            // A link is never followed.
            if !is_fan_out || !fan_out.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(entries) = fs::read_dir(fan_out.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let stale = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("tmp_obj_"))
                    && entry.metadata().is_ok_and(|metadata| {
                        metadata.is_file()
                            && metadata
                                .modified()
                                .ok()
                                .and_then(|modified| now.duration_since(modified).ok())
                                .is_some_and(|age| age >= STALE_TEMP_FILE_AGE)
                    });
                if stale && fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        removed
    }
    /// The id of the tree with no entries, the base of the first snapshot.
    pub fn empty_tree(&self) -> ObjectId {
        ObjectId::empty_tree(self.repo.object_hash())
    }

    /// Builds the tree that results from applying `changes` to the tree `base`, writes it, and returns its
    /// id. Only the trees on the paths of the changes are rewritten, and no change at all returns `base`
    /// without reading anything.
    ///
    /// Removals are applied before additions, so a file replaced by a directory of the same name (or the
    /// reverse) can be given in any order. Every path is checked before anything is written, and every
    /// blob must already be in the store.
    pub fn edit_tree(&self, base: ObjectId, changes: &[Change]) -> Result<ObjectId> {
        if changes.is_empty() {
            return Ok(base);
        }
        for change in changes {
            match change {
                Change::Upsert { path, kind, blob } => {
                    validate_path(path, *kind)?;
                    if !self.repo.has_object(blob) {
                        return Err(Error::MissingObject(*blob));
                    }
                }
                Change::Remove { path } => validate_path(path, FileKind::Regular)?,
            }
        }

        // gix's checked editor validates every entry of each rewritten tree and looks up every object they
        // point to, one file system access per entry: slow for large directories. The plain editor is used
        // instead, and only the changed paths are checked, above.
        let objects = self.verified();
        let mut buffer = Vec::new();
        let root = objects
            .find_tree(&base, &mut buffer)
            .map_err(gix::Error::from)?;
        let mut editor =
            gix::objs::tree::Editor::new(root.into(), &objects, self.repo.object_hash());
        for change in changes {
            if let Change::Remove { path } = change {
                editor.remove(path.split('/')).map_err(gix::Error::from)?;
            }
        }
        for change in changes {
            if let Change::Upsert { path, kind, blob } = change {
                editor
                    .upsert(path.split('/'), (*kind).into(), *blob)
                    .map_err(gix::Error::from)?;
            }
        }
        // Each id is known as soon as its tree is encoded, so the new trees are written together at the end: in
        // parallel when there are only a few, as in a usual step (creating a file on Windows is mostly waiting;
        // measured: 4.6 instead of 5.8 ms for 3 trees), one by one otherwise (a new folder hierarchy).
        let mut bytes = Vec::new();
        let mut new_trees = Vec::new();
        let tree = editor.write(|tree| {
            use gix::objs::WriteTo;
            bytes.clear();
            tree.write_to(&mut bytes)?;
            let (id, new) = self.prepare(gix::objs::Kind::Tree, &bytes)?;
            new_trees.extend(new);
            Ok::<_, Error>(id)
        })?;
        let objects = self.objects_dir();
        if new_trees.len() > MAX_PARALLEL_TREE_WRITES {
            for new in &new_trees {
                persist(&objects, new)?;
            }
            return Ok(tree);
        }
        std::thread::scope(|scope| {
            let mut results = Vec::new();
            let mut writers = Vec::new();
            for new in new_trees.iter().skip(1) {
                match std::thread::Builder::new().spawn_scoped(scope, || persist(&objects, new)) {
                    Ok(writer) => writers.push(writer),
                    // No thread available: written here.
                    Err(_) => results.push(persist(&objects, new)),
                }
            }
            if let Some(first) = new_trees.first() {
                results.push(persist(&objects, first));
            }
            for writer in writers {
                results.push(
                    writer
                        .join()
                        .unwrap_or_else(|_| Err(io::Error::other("writing a tree failed"))),
                );
            }
            results.into_iter().collect::<io::Result<()>>()
        })?;
        Ok(tree)
    }

    /// The files added, modified and deleted between the trees `old` and `new`. Directories are not listed,
    /// only the files in them. Every path is checked like a new one (see [`validate_path`]), so a tree that
    /// Yalper could not have written is refused.
    pub fn changed_paths(&self, old: ObjectId, new: ObjectId) -> Result<ChangedPaths> {
        let objects = self.verified();
        let (mut old_buffer, mut new_buffer) = (Vec::new(), Vec::new());
        let old = objects
            .find_tree_iter(&old, &mut old_buffer)
            .map_err(gix::Error::from)?;
        let new = objects
            .find_tree_iter(&new, &mut new_buffer)
            .map_err(gix::Error::from)?;
        let mut recorder = gix::diff::tree::Recorder::default();
        gix::diff::tree(
            old,
            new,
            gix::diff::tree::State::default(),
            objects,
            &mut recorder,
        )
        .map_err(|error| Error::Git(gix::Error::from_error(error)))?;

        let mut changed = ChangedPaths::default();
        for change in recorder.records {
            use gix::diff::tree::recorder::Change::{Addition, Deletion, Modification};
            let (list, entry_mode, path) = match change {
                Addition {
                    entry_mode, path, ..
                } => (&mut changed.added, entry_mode, path),
                Deletion {
                    entry_mode, path, ..
                } => (&mut changed.deleted, entry_mode, path),
                // A file and a directory of the same name are different entries in git's sort order, so a
                // file replaced by a directory shows as a deletion plus additions, never as a modification.
                Modification {
                    entry_mode, path, ..
                } => (&mut changed.modified, entry_mode, path),
            };
            if entry_mode.is_tree() {
                continue;
            }
            let Ok(path) = path.to_str() else {
                return Err(Error::InvalidPath {
                    path: path.to_str_lossy().into_owned(),
                    reason: "not valid UTF-8".to_owned(),
                });
            };
            let kind = if entry_mode.is_link() {
                FileKind::Symlink
            } else {
                FileKind::Regular
            };
            validate_path(path, kind)?;
            list.push(path.to_owned());
        }
        for list in [
            &mut changed.added,
            &mut changed.modified,
            &mut changed.deleted,
        ] {
            list.sort_unstable();
        }
        Ok(changed)
    }

    /// Whether the store has the object `id`. The trees of an event log entry can be gone when the store was
    /// created again after it was lost.
    pub fn has_object(&self, id: ObjectId) -> bool {
        self.repo.has_object(id)
    }

    /// The files that differ between the trees `old` and `new`, sorted by path, for showing a step.
    /// The tree diff stops after `max_entries` entries (files and directories), so a crafted store whose
    /// trees repeat a large subtree many times cannot make it run for long; [`FileChanges::complete`] then
    /// says the list is cut. Paths are returned as stored: check them with [`validate_path`] before use.
    pub fn file_changes(
        &self,
        old: ObjectId,
        new: ObjectId,
        max_entries: usize,
    ) -> Result<FileChanges> {
        let objects = self.verified();
        let (mut old_buffer, mut new_buffer) = (Vec::new(), Vec::new());
        let old = objects
            .find_tree_iter(&old, &mut old_buffer)
            .map_err(gix::Error::from)?;
        let new = objects
            .find_tree_iter(&new, &mut new_buffer)
            .map_err(gix::Error::from)?;
        let mut recorder = Capped {
            inner: gix::diff::tree::Recorder::default(),
            max_entries,
        };
        let complete = match gix::diff::tree(
            old,
            new,
            gix::diff::tree::State::default(),
            objects,
            &mut recorder,
        ) {
            Ok(()) => true,
            Err(gix::diff::tree::Error::Cancelled) => false,
            Err(error) => return Err(Error::Git(gix::Error::from_error(error))),
        };

        let file = |mode: gix::objs::tree::EntryMode, blob: ObjectId| {
            let kind = match mode.kind() {
                EntryKind::Blob => FileKind::Regular,
                EntryKind::BlobExecutable => FileKind::Executable,
                EntryKind::Link => FileKind::Symlink,
                EntryKind::Tree | EntryKind::Commit => return None,
            };
            Some(TreeFile { kind, blob })
        };
        let mut files = Vec::new();
        for change in recorder.inner.records {
            use gix::diff::tree::recorder::Change::{Addition, Deletion, Modification};
            let (path, old, new) = match change {
                Addition {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => (path, None, Some((entry_mode, oid))),
                Deletion {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => (path, Some((entry_mode, oid)), None),
                Modification {
                    previous_entry_mode,
                    previous_oid,
                    entry_mode,
                    oid,
                    path,
                } => (
                    path,
                    Some((previous_entry_mode, previous_oid)),
                    Some((entry_mode, oid)),
                ),
            };
            let (old, new) = (
                old.and_then(|(mode, id)| file(mode, id)),
                new.and_then(|(mode, id)| file(mode, id)),
            );
            // Directories only matter through the files in them. Yalper never stores submodule entries.
            if old.is_some() || new.is_some() {
                files.push(FileChange {
                    path: path.into(),
                    old,
                    new,
                });
            }
        }
        // The diff goes through the trees level by level.
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok(FileChanges { files, complete })
    }

    /// The content of the blob `id`, read through [`Verified`], or `None` if the store does not have it.
    pub fn read_blob(&self, id: ObjectId) -> Result<Option<Vec<u8>>> {
        let mut buffer = Vec::new();
        let Some(data) = self
            .verified()
            .try_find(&id, &mut buffer)
            .map_err(gix::Error::from)?
        else {
            return Ok(None);
        };
        if data.kind != gix::objs::Kind::Blob {
            let error =
                gix::error::corruption(format!("object {id} is a {}, not a blob", data.kind));
            return Err(Error::Git(gix::Error::from(error.raise_erased())));
        }
        Ok(Some(buffer))
    }

    fn verified(&self) -> Verified<'_> {
        Verified(&self.repo.objects)
    }
}

/// One file of a tree: how it is stored and its blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeFile {
    pub kind: FileKind,
    pub blob: ObjectId,
}

/// A file that differs between two trees: `old` is `None` for an added file, `new` for a deleted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// As stored in the tree, `/`-separated, not checked: it may not be valid UTF-8 or a valid path.
    pub path: Vec<u8>,
    pub old: Option<TreeFile>,
    pub new: Option<TreeFile>,
}

/// See [`ShadowStore::file_changes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChanges {
    pub files: Vec<FileChange>,
    /// `false` when the tree diff stopped at its limit, so more files changed than listed.
    pub complete: bool,
}

/// A tree diff recorder that stops the diff once it holds `max_entries` changes.
struct Capped {
    inner: gix::diff::tree::Recorder,
    max_entries: usize,
}

impl gix::diff::tree::Visit for Capped {
    fn pop_front_tracked_path_and_set_current(&mut self) {
        self.inner.pop_front_tracked_path_and_set_current();
    }

    fn push_back_tracked_path_component(&mut self, component: &BStr) {
        self.inner.push_back_tracked_path_component(component);
    }

    fn push_path_component(&mut self, component: &BStr) {
        self.inner.push_path_component(component);
    }

    fn pop_path_component(&mut self) {
        self.inner.pop_path_component();
    }

    fn visit(&mut self, change: gix::diff::tree::visit::Change) -> gix::diff::tree::visit::Action {
        if self.inner.records.len() >= self.max_entries {
            return std::ops::ControlFlow::Break(());
        }
        self.inner.visit(change)
    }
}

/// Object reads that recompute each object's hash and refuse one that does not match its id, so a corrupted
/// or planted object cannot change what a snapshot contains. A tree cannot contain its own id, so this also
/// rules out cycles. Used for every tree read and every blob read (blobs are not read on the recording path).
#[derive(Clone, Copy)]
struct Verified<'a>(&'a gix::OdbHandle);

impl Find for Verified<'_> {
    fn try_find<'a>(
        &self,
        id: &oid,
        buffer: &'a mut Vec<u8>,
    ) -> gix::ExnResult<Option<gix::objs::Data<'a>>> {
        // git knows the empty tree without storing it.
        if id.is_empty_tree() {
            buffer.clear();
            return Ok(Some(gix::objs::Data {
                kind: gix::objs::Kind::Tree,
                object_hash: id.kind(),
                data: buffer,
            }));
        }
        let Some(data) = self.0.try_find(id, buffer)? else {
            return Ok(None);
        };
        let actual =
            gix::objs::compute_hash(id.kind(), data.kind, data.data).map_err(gix::Exn::erased)?;
        if actual != id {
            return Err(gix::error::corruption(format!(
                "object {id} does not match its id ({actual})"
            ))
            .raise_erased());
        }
        Ok(Some(data))
    }
}

/// Checks that `path` can be stored in a tree as a file of `kind`: relative, `/`-separated, with no NUL byte,
/// no empty, `.`, `..` or `.git` component and no name the platform's file system treats specially (see
/// [`PATH_RULES`]).
pub fn validate_path(path: &str, kind: FileKind) -> Result<()> {
    let invalid = |reason: String| Error::InvalidPath {
        path: path.to_owned(),
        reason,
    };
    // A NUL ends a name in git's tree format, so it would silently cut the path.
    if path.contains('\0') {
        return Err(invalid("contains a NUL byte".to_owned()));
    }
    let mut components = path.split('/').peekable();
    while let Some(name) = components.next() {
        let mode = (components.peek().is_none() && kind == FileKind::Symlink)
            .then_some(component::Mode::Symlink);
        if let Err(error) = gix::validate::path::component(BStr::new(name), mode, PATH_RULES) {
            return Err(invalid(error.to_string()));
        }
    }
    Ok(())
}

/// A new loose object, compressed, see [`ShadowStore::prepare`].
struct NewObject {
    id: ObjectId,
    compressed: Vec<u8>,
}

/// Writes `object` into the object directory `objects`: to a temporary file next to its final name, then
/// renamed into place. The rename keeps a hook stopped at its deadline from leaving a partial object under a
/// final name, which later writes of the same content would trust. Like git, the object's directory is created
/// on demand. A temporary file left by a stopped hook is never reused (its name has random bits, and a taken
/// name is skipped) and is deleted by `yalper init` once it is an hour old (see
/// [`ShadowStore::remove_stale_temp_files`]).
fn persist(objects: &Path, object: &NewObject) -> io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_FILES: AtomicU64 = AtomicU64::new(0);
    const ATTEMPTS: usize = 8;

    let hex = object.id.to_string();
    let fan_out = objects.join(&hex[..2]);
    // Checked right before use: the store's layout was checked when it was opened, but the directory could
    // have been created since, and an object must never be written through a link.
    match fs::symlink_metadata(&fan_out) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(io::Error::other(format!(
                "objects/{} is not a directory",
                &hex[..2]
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(&fan_out) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        },
        Err(error) => return Err(error),
    }
    let mut attempt = 0;
    let (mut file, temp) = loop {
        let temp = fan_out.join(format!(
            "tmp_obj_{:016x}_{}",
            temp_file_prefix(),
            TEMP_FILES.fetch_add(1, Ordering::Relaxed)
        ));
        match create_object_file(&temp) {
            Ok(file) => break (file, temp),
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists && attempt + 1 < ATTEMPTS =>
            {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    };
    let written = file.write_all(&object.compressed);
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    let path = fan_out.join(&hex[2..]);
    if let Err(error) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        // On Windows, an object being read cannot be replaced, and it already holds this content.
        if fs::symlink_metadata(&path).is_err() {
            return Err(error);
        }
    }
    Ok(())
}

/// Random bits for the names of this process's temporary object files, so that a process never runs into a
/// file left by an earlier one (process ids are reused).
fn temp_file_prefix() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static PREFIX: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    // The standard library seeds each `RandomState` from the operating system's random number generator.
    *PREFIX.get_or_init(|| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.finish()
    })
}
/// Creates a new file for a loose object at `path`, read-only for everyone on Unix as git makes its objects.
fn create_object_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o444);
    options.open(path)
}

/// See [`ShadowStore::open`]. A few metadata reads, three short directory listings and one read of `config`.
fn check_layout(dir: &OwnedDir, expected_config: &str) -> Result<()> {
    let root = dir.path();
    let unexpected = |what: &str| Err(Error::UnexpectedLayout(what.to_owned()));
    for name in ["commondir", "packed-refs", "objects/info/alternates"] {
        match fs::symlink_metadata(root.join(name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return unexpected(&format!("`{name}`")),
            Err(error) => return Err(error.into()),
        }
    }

    let config = fs::symlink_metadata(root.join("config"))?;
    if !config.is_file()
        || config.len() != expected_config.len() as u64
        || fs::read(root.join("config"))? != expected_config.as_bytes()
    {
        return unexpected("a `config` Yalper did not write for this .yalper folder");
    }
    let head = fs::symlink_metadata(root.join("HEAD"))?;
    if !head.is_file() || head.len() > MAX_HEAD_BYTES {
        return unexpected("a `HEAD` Yalper did not write");
    }

    // Yalper writes no refs and no packs. A ref (for example under `refs/replace/`) could make git tools
    // show other objects; packs are not checked like loose objects.
    for name in ["refs", "objects/pack"] {
        let path = root.join(name);
        if !fs::symlink_metadata(&path)?.is_dir() || fs::read_dir(&path)?.next().is_some() {
            return unexpected(&format!("a `{name}` that is not an empty directory"));
        }
    }

    let objects = root.join("objects");
    if !fs::symlink_metadata(&objects)?.is_dir() {
        return unexpected("an `objects` that is not a directory");
    }
    // One listing of at most 258 entries; the type comes with the listing on Windows, Linux and macOS.
    for entry in fs::read_dir(&objects)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            return unexpected(&format!(
                "a link at `objects/{}`",
                entry.file_name().to_string_lossy()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use FileKind::{Executable, Regular, Symlink};

    fn token() -> Token {
        Token::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    /// A `.yalper` directory with an initialized store in it.
    fn new_store() -> (tempfile::TempDir, ShadowStore) {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = ShadowStore::init(&owned, &token()).unwrap();
        (dir, store)
    }

    type Files<'a> = [(&'a str, FileKind, &'a str)];

    fn upserts(store: &ShadowStore, files: &Files<'_>) -> Vec<Change> {
        files
            .iter()
            .map(|&(path, kind, content)| Change::Upsert {
                path: path.to_owned(),
                kind,
                blob: store.write_blob(content.as_bytes()).unwrap(),
            })
            .collect()
    }

    fn from_scratch(store: &ShadowStore, files: &Files<'_>) -> ObjectId {
        store
            .edit_tree(store.empty_tree(), &upserts(store, files))
            .unwrap()
    }

    fn remove(path: &str) -> Change {
        Change::Remove {
            path: path.to_owned(),
        }
    }

    /// The id of every loose object in the store at `store_dir`, sorted.
    fn loose_objects(store_dir: &Path) -> Vec<String> {
        let mut objects = Vec::new();
        for fan_out in fs::read_dir(store_dir.join("objects")).unwrap() {
            let fan_out = fan_out.unwrap();
            let prefix = fan_out.file_name().into_string().unwrap();
            if prefix.len() != 2 {
                continue;
            }
            for object in fs::read_dir(fan_out.path()).unwrap() {
                let rest = object.unwrap().file_name().into_string().unwrap();
                objects.push(format!("{prefix}{rest}"));
            }
        }
        objects.sort();
        objects
    }

    #[test]
    fn many_new_directories_are_written_one_by_one() {
        let (dir, store) = new_store();
        // 40 new directories two levels deep: 81 new trees in one edit, more than are written in parallel.
        let files: Vec<(String, FileKind, String)> = (0..40)
            .map(|index| (format!("d{index}/sub/f.txt"), Regular, format!("{index}")))
            .collect();
        let files: Vec<(&str, FileKind, &str)> = files
            .iter()
            .map(|(path, kind, content)| (path.as_str(), *kind, content.as_str()))
            .collect();
        let tree = from_scratch(&store, &files);
        assert_eq!(
            store
                .changed_paths(store.empty_tree(), tree)
                .unwrap()
                .added
                .len(),
            40
        );
        assert_eq!(
            loose_objects(&dir.path().join(SNAPSHOTS_DIR)).len(),
            40 + 81
        );
    }

    #[test]
    fn stale_temporary_object_files_are_deleted() {
        let (dir, store) = new_store();
        let fan_out = dir.path().join(SNAPSHOTS_DIR).join("objects").join("ab");
        fs::create_dir(&fan_out).unwrap();
        let old = fan_out.join("tmp_obj_0123_7");
        let fresh = fan_out.join("tmp_obj_0123_8");
        let object = fan_out.join("cdef0123456789abcdef0123456789abcdef01");
        for path in [&old, &fresh, &object] {
            fs::write(path, "x").unwrap();
        }
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        for path in [&old, &object] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }
        assert_eq!(store.remove_stale_temp_files(), 1);
        assert!(!old.exists() && fresh.exists() && object.exists());
    }

    #[test]
    fn an_object_directory_that_is_a_link_is_never_written_through() {
        let (dir, store) = new_store();
        let outside = tempfile::tempdir().unwrap();
        // The blob of "hello\n" goes to `objects/ce`.
        link_dir(
            outside.path(),
            &dir.path().join(SNAPSHOTS_DIR).join("objects").join("ce"),
        );
        assert!(store.write_blob(b"hello\n").is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn the_same_content_is_stored_once() {
        let (dir, store) = new_store();
        let store_dir = dir.path().join(SNAPSHOTS_DIR);
        let first = store.write_blob(b"fn main() {}\n").unwrap();
        let second = store.write_blob(b"fn main() {}\n").unwrap();
        assert_eq!(first, second);
        assert_eq!(loose_objects(&store_dir), [first.to_string()]);

        // Two files with the same content share one blob, and an unchanged tree is not stored again.
        let files = [("a.txt", Regular, "same"), ("b/c.txt", Regular, "same")];
        let tree = from_scratch(&store, &files);
        let count = loose_objects(&store_dir).len();
        assert_eq!(count, 4, "first blob, shared blob, root tree, tree `b`");
        assert_eq!(from_scratch(&store, &files), tree);
        assert_eq!(store.edit_tree(tree, &[]).unwrap(), tree);
        assert_eq!(loose_objects(&store_dir).len(), count);
    }

    #[test]
    fn ids_are_the_ones_git_computes() {
        let (_dir, store) = new_store();
        // Output of `git hash-object --stdin` for "hello" and a newline.
        assert_eq!(
            store.write_blob(b"hello\n").unwrap().to_string(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert_eq!(
            store.empty_tree().to_string(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn an_incremental_tree_equals_one_built_from_scratch() {
        let (_dir, store) = new_store();
        // Each step: files added or replaced, then paths removed.
        let steps: [(&Files<'_>, &[&str]); 4] = [
            (
                &[
                    ("README.md", Regular, "readme"),
                    ("src/main.rs", Regular, "main"),
                    ("src/util/mod.rs", Regular, "util"),
                    ("src/util/deep/leaf.rs", Regular, "leaf"),
                    ("scripts/build.sh", Executable, "#!/bin/sh"),
                    ("link", Symlink, "README.md"),
                    ("was_file", Regular, "file"),
                    ("was_dir/inner.txt", Regular, "inner"),
                ],
                &[],
            ),
            (
                // Edit, change kinds, turn a file into a directory and a directory into a file.
                &[
                    ("src/main.rs", Regular, "main v2"),
                    ("scripts/build.sh", Regular, "#!/bin/sh"),
                    ("link", Regular, "now a file"),
                    ("was_file/child.txt", Regular, "child"),
                    ("was_dir", Regular, "now a file"),
                    ("new/nested/dir/file.txt", Regular, "new"),
                ],
                &["was_file", "was_dir/inner.txt"],
            ),
            (
                // Remove every file of a nested directory one by one, and a directory at once.
                &[("README.md", Executable, "readme")],
                &["src/util/mod.rs", "src/util/deep/leaf.rs", "new"],
            ),
            (
                &[],
                &["README.md", "src", "scripts", "link", "was_file", "was_dir"],
            ),
        ];

        let mut expected: BTreeMap<String, (FileKind, &str)> = BTreeMap::new();
        let mut tree = store.empty_tree();
        for (files, removed) in steps {
            for path in removed {
                let inside = format!("{path}/");
                expected.retain(|file, _| file != path && !file.starts_with(&inside));
            }
            for &(path, kind, content) in files {
                let inside = format!("{path}/");
                expected.retain(|file, _| !file.starts_with(&inside));
                expected.insert(path.to_owned(), (kind, content));
            }
            // Removals are listed after the additions: the order must not matter.
            let mut changes = upserts(&store, files);
            changes.extend(removed.iter().map(|path| remove(path)));
            tree = store.edit_tree(tree, &changes).unwrap();

            let all: Vec<_> = expected
                .iter()
                .map(|(path, &(kind, content))| (path.as_str(), kind, content))
                .collect();
            assert_eq!(tree, from_scratch(&store, &all), "{expected:?}");
        }
        assert_eq!(tree, store.empty_tree(), "empty directories are removed");
    }

    #[test]
    fn nested_directories_are_created_and_removed() {
        let (_dir, store) = new_store();
        let base = from_scratch(&store, &[("keep.txt", Regular, "keep")]);
        let deep = store
            .edit_tree(
                base,
                &upserts(&store, &[("a/b/c/d/e.txt", Regular, "deep")]),
            )
            .unwrap();
        let added = store.changed_paths(base, deep).unwrap();
        assert_eq!(added.added, ["a/b/c/d/e.txt"]);

        assert_eq!(
            store.edit_tree(deep, &[remove("a/b/c/d/e.txt")]).unwrap(),
            base
        );
        assert_eq!(store.edit_tree(deep, &[remove("a/b")]).unwrap(), base);
        assert_eq!(
            store.edit_tree(deep, &[remove("missing/file")]).unwrap(),
            deep
        );
    }

    #[test]
    fn changed_paths_lists_exactly_the_changed_files() {
        let (_dir, store) = new_store();
        let old = from_scratch(
            &store,
            &[
                ("same.txt", Regular, "same"),
                ("same_dir/a.txt", Regular, "a"),
                ("edited.txt", Regular, "old"),
                ("chmod.sh", Regular, "script"),
                ("to_link", Regular, "target"),
                ("deleted.txt", Regular, "gone"),
                ("gone_dir/x/y.txt", Regular, "y"),
                ("gone_dir/z.txt", Regular, "z"),
                ("file_then_dir", Regular, "file"),
            ],
        );
        let mut changes = upserts(
            &store,
            &[
                ("edited.txt", Regular, "new"),
                ("chmod.sh", Executable, "script"),
                ("to_link", Symlink, "target"),
                ("added.txt", Regular, "added"),
                ("new_dir/sub/b.txt", Regular, "b"),
                ("file_then_dir/inner.txt", Regular, "inner"),
            ],
        );
        changes.extend(["deleted.txt", "gone_dir", "file_then_dir"].map(remove));
        let new = store.edit_tree(old, &changes).unwrap();

        let changed = store.changed_paths(old, new).unwrap();
        let strings = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(
            changed,
            ChangedPaths {
                added: strings(&["added.txt", "file_then_dir/inner.txt", "new_dir/sub/b.txt"]),
                modified: strings(&["chmod.sh", "edited.txt", "to_link"]),
                deleted: strings(&[
                    "deleted.txt",
                    "file_then_dir",
                    "gone_dir/x/y.txt",
                    "gone_dir/z.txt",
                ]),
            }
        );
        assert_eq!(changed.len(), 10);

        let reverse = store.changed_paths(new, old).unwrap();
        assert_eq!(reverse.added, changed.deleted);
        assert_eq!(reverse.deleted, changed.added);
        assert_eq!(reverse.modified, changed.modified);

        assert!(store.changed_paths(new, new).unwrap().is_empty());
        let everything = store.changed_paths(store.empty_tree(), old).unwrap();
        assert_eq!(everything.added.len(), 9);
        assert!(everything.modified.is_empty() && everything.deleted.is_empty());
    }

    #[test]
    fn invalid_paths_are_refused_before_anything_is_written() {
        let (dir, store) = new_store();
        let blob = store.write_blob(b"x").unwrap();
        let base = from_scratch(&store, &[("ok.txt", Regular, "x")]);
        let objects = loose_objects(&dir.path().join(SNAPSHOTS_DIR));
        for path in [
            "",
            "/abs",
            "a//b",
            "trailing/",
            ".",
            "..",
            "a/../b",
            ".git",
            ".git/hooks/pre-commit",
            "sub/.GIT/config",
            "GIT~1/config",
            "nul\0byte",
            "dir/nul\0",
        ] {
            let changes = [
                Change::Upsert {
                    path: "fine.txt".to_owned(),
                    kind: Regular,
                    blob,
                },
                Change::Upsert {
                    path: path.to_owned(),
                    kind: Regular,
                    blob,
                },
            ];
            let error = store.edit_tree(base, &changes).unwrap_err();
            assert!(
                matches!(error, Error::InvalidPath { .. }),
                "{path}: {error}"
            );
            assert!(store.edit_tree(base, &[remove(path)]).is_err(), "{path}");
        }
        // A symlink named `.gitmodules` could redirect submodule configuration on a checkout.
        let link = Change::Upsert {
            path: ".gitmodules".to_owned(),
            kind: Symlink,
            blob,
        };
        let error = store.edit_tree(base, &[link]).unwrap_err();
        assert!(matches!(error, Error::InvalidPath { .. }), "{error}");
        assert_eq!(loose_objects(&dir.path().join(SNAPSHOTS_DIR)), objects);
    }

    #[test]
    fn a_blob_missing_from_the_store_is_refused() {
        let (_dir, store) = new_store();
        let missing = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        let change = Change::Upsert {
            path: "a.txt".to_owned(),
            kind: Regular,
            blob: missing,
        };
        let error = store.edit_tree(store.empty_tree(), &[change]).unwrap_err();
        assert!(
            matches!(error, Error::MissingObject(id) if id == missing),
            "{error}"
        );
    }

    #[test]
    fn a_reopened_store_reads_what_was_written() {
        let (dir, store) = new_store();
        let tree = from_scratch(&store, &[("a/b.txt", Regular, "b")]);
        drop(store);
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = ShadowStore::open(&owned, &token()).unwrap();
        let changed = store.changed_paths(store.empty_tree(), tree).unwrap();
        assert_eq!(changed.added, ["a/b.txt"]);
    }

    #[test]
    fn a_store_created_with_another_token_is_refused() {
        let (dir, store) = new_store();
        drop(store);
        let owned = OwnedDir::open(dir.path()).unwrap();
        let other = Token::parse("fedcba9876543210fedcba9876543210").unwrap();
        let error = ShadowStore::open(&owned, &other).unwrap_err();
        assert!(matches!(error, Error::UnexpectedLayout(_)), "{error}");
        ShadowStore::open(&owned, &token()).unwrap();
    }

    #[test]
    fn init_refuses_an_existing_path_and_open_a_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(ShadowStore::open(&owned, &token()).is_err());
        fs::create_dir(dir.path().join(SNAPSHOTS_DIR)).unwrap();
        assert!(ShadowStore::init(&owned, &token()).is_err());
    }

    #[test]
    fn a_store_pointing_to_another_repository_is_refused() {
        for (name, content) in [
            ("commondir", "../../.git\n"),
            ("objects/info/alternates", "../../.git/objects\n"),
        ] {
            let (dir, store) = new_store();
            drop(store);
            fs::write(dir.path().join(SNAPSHOTS_DIR).join(name), content).unwrap();
            let owned = OwnedDir::open(dir.path()).unwrap();
            let error = ShadowStore::open(&owned, &token()).unwrap_err();
            assert!(
                matches!(error, Error::UnexpectedLayout(_)),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn a_link_in_place_of_the_store_or_an_object_directory_is_refused() {
        let outside = tempfile::tempdir().unwrap();

        let (dir, store) = new_store();
        drop(store);
        let fan_out = dir.path().join(SNAPSHOTS_DIR).join("objects").join("ce");
        link_dir(outside.path(), &fan_out);
        let owned = OwnedDir::open(dir.path()).unwrap();
        let error = ShadowStore::open(&owned, &token()).unwrap_err();
        assert!(matches!(error, Error::UnexpectedLayout(_)), "{error}");

        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        ShadowStore::init(&OwnedDir::open(elsewhere.path()).unwrap(), &token()).unwrap();
        link_dir(
            &elsewhere.path().join(SNAPSHOTS_DIR),
            &dir.path().join(SNAPSHOTS_DIR),
        );
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(ShadowStore::open(&owned, &token()).is_err());
        assert!(ShadowStore::init(&owned, &token()).is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    /// A directory symlink on Unix, a junction on Windows (which needs no special rights).
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
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }
    }

    /// Every file under `dir`, by relative path, with its content.
    fn contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in fs::read_dir(&current).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(dir).unwrap().to_string_lossy();
                    files.insert(relative.into_owned(), fs::read(&path).unwrap());
                }
            }
        }
        files
    }

    /// Runs git in `project`. Newer git versions start maintenance in the background after a commit, which
    /// can create lock files in `.git` while the test reads it (seen on macOS CI), so that is turned off, as
    /// is the file system monitor daemon.
    fn git(project: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "core.autocrlf=false", "-c", "maintenance.auto=false"])
            .args(["-c", "gc.auto=0", "-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(project)
            .stdout(Stdio::null())
            .status()
            .expect("git must be installed to run this test");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn the_user_git_directory_is_untouched() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        git(root, &["init", "--quiet"]);
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        git(root, &["add", "main.rs"]);
        git(root, &["commit", "--quiet", "-m", "first"]);
        fs::create_dir(root.join(".yalper")).unwrap();
        let before = contents(&root.join(".git"));

        let yalper = OwnedDir::open(&root.join(".yalper")).unwrap();
        let store = ShadowStore::init(&yalper, &token()).unwrap();
        // The same content as the user's committed file: it is still written to the shadow store.
        let old = from_scratch(&store, &[("main.rs", Regular, "fn main() {}\n")]);
        let added = upserts(&store, &[("lib.rs", Regular, "pub fn f() {}\n")]);
        let new = store.edit_tree(old, &added).unwrap();
        drop(store);
        let store = ShadowStore::open(&yalper, &token()).unwrap();
        assert_eq!(store.changed_paths(old, new).unwrap().added, ["lib.rs"]);
        let store_dir = root.join(".yalper").join(SNAPSHOTS_DIR);
        assert_eq!(loose_objects(&store_dir).len(), 4, "two blobs, two trees");

        let after = contents(&root.join(".git"));
        let differing: Vec<&String> = before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .collect();
        assert!(differing.is_empty(), "changed in .git: {differing:?}");
    }

    const ISOLATION_HOME: &str = "YALPER_TEST_ISOLATION_HOME";

    /// Uses the store with a home directory and `GIT_*` variables that would change its behavior if they
    /// were read. Run in a child process by `user_and_environment_git_settings_are_ignored`, because
    /// changing the environment of the test process would affect other tests.
    #[test]
    #[ignore = "run in a child process by user_and_environment_git_settings_are_ignored"]
    fn isolation_child() {
        let Some(home) = std::env::var_os(ISOLATION_HOME) else {
            return;
        };
        let home = Path::new(&home);
        let yalper = home.join("project").join(".yalper");

        // Control: gix with its default options does read this home's configuration.
        let control = gix::open(home.join("control.git")).unwrap();
        let name = control.config_snapshot().string("user.name").unwrap();
        assert_eq!(name.to_string(), "From Global");

        let owned = OwnedDir::open(&yalper).unwrap();
        let store = ShadowStore::init(&owned, &token()).unwrap();
        let head = fs::read_to_string(yalper.join(SNAPSHOTS_DIR).join("HEAD")).unwrap();
        assert!(!head.contains("from-global"), "{head}");
        assert!(store.repo.config_snapshot().string("user.name").is_none());
        drop(store);

        let store = ShadowStore::open(&owned, &token()).unwrap();
        assert!(store.repo.config_snapshot().string("user.name").is_none());
        let blob = store.write_blob(b"isolated").unwrap();
        assert!(loose_objects(&yalper.join(SNAPSHOTS_DIR)).contains(&blob.to_string()));
        assert!(
            fs::read_dir(home.join("env-objects"))
                .unwrap()
                .next()
                .is_none()
        );
        fs::write(home.join("child-passed"), "").unwrap();
    }

    #[test]
    fn user_and_environment_git_settings_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let config = "[user]\n\tname = From Global\n[init]\n\tdefaultBranch = from-global\n";
        fs::write(home.path().join(".gitconfig"), config).unwrap();
        fs::create_dir_all(home.path().join("xdg").join("git")).unwrap();
        fs::write(home.path().join("xdg").join("git").join("config"), config).unwrap();
        fs::create_dir_all(home.path().join("project").join(".yalper")).unwrap();
        fs::create_dir(home.path().join("env-objects")).unwrap();
        ShadowStore::init(&OwnedDir::open(home.path()).unwrap(), &token()).unwrap();
        fs::rename(
            home.path().join(SNAPSHOTS_DIR),
            home.path().join("control.git"),
        )
        .unwrap();

        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "snapshot::tests::isolation_child", "--ignored"])
            .env(ISOLATION_HOME, home.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("xdg"))
            .env("GIT_CONFIG_GLOBAL", home.path().join(".gitconfig"))
            .env("GIT_OBJECT_DIRECTORY", home.path().join("env-objects"))
            .env("GIT_DIR", home.path().join("control.git"))
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            home.path().join("child-passed").exists(),
            "the child test did not run"
        );
    }

    #[test]
    fn no_change_returns_the_base_without_reading_it() {
        let (dir, store) = new_store();
        let unknown = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(store.edit_tree(unknown, &[]).unwrap(), unknown);
        assert!(loose_objects(&dir.path().join(SNAPSHOTS_DIR)).is_empty());
    }

    /// Writes `data` as an object of `kind` under `id`, whatever its real hash, the way a corrupted or
    /// planted store could hold it. An object already stored under `id` is replaced.
    fn plant(
        store_dir: &Path,
        store: &ShadowStore,
        kind: gix::objs::Kind,
        data: &[u8],
        id: ObjectId,
    ) {
        use gix::objs::Write;
        let hex = id.to_string();
        let path = store_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        if path.exists() {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).unwrap();
            fs::remove_file(&path).unwrap();
        }
        store
            .repo
            .objects
            .write_buf_with_known_id(kind, data, id)
            .unwrap();
        assert!(path.exists());
    }

    fn tree_bytes(entries: Vec<gix::objs::tree::Entry>) -> Vec<u8> {
        use gix::objs::WriteTo;
        let mut bytes = Vec::new();
        gix::objs::Tree { entries }.write_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn a_planted_tree_is_refused() {
        let (dir, store) = new_store();
        let store_dir = dir.path().join(SNAPSHOTS_DIR);
        let honest = from_scratch(&store, &[("dir/x.txt", Regular, "x")]);
        let evil = from_scratch(&store, &[("evil.txt", Regular, "evil")]);
        let evil_data = store.repo.find_object(evil).unwrap().data.clone();

        // The id of the next snapshot, predicted and taken before Yalper writes it.
        let (_other_dir, other) = new_store();
        let predicted = from_scratch(&other, &[("next.txt", Regular, "next")]);
        plant(
            &store_dir,
            &store,
            gix::objs::Kind::Tree,
            &evil_data,
            predicted,
        );
        let next = upserts(&store, &[("next.txt", Regular, "next")]);
        assert!(store.changed_paths(honest, predicted).is_err());
        assert!(store.edit_tree(predicted, &next).is_err());

        // A subtree of an honest snapshot replaced in place: `dir` holds what a root with `x.txt` holds.
        let subtree = from_scratch(&store, &[("x.txt", Regular, "x")]);
        plant(
            &store_dir,
            &store,
            gix::objs::Kind::Tree,
            &evil_data,
            subtree,
        );
        let error = store.changed_paths(store.empty_tree(), honest).unwrap_err();
        assert!(
            error.to_string().contains("does not match its id"),
            "{error}"
        );
        let more = upserts(&store, &[("dir/y.txt", Regular, "y")]);
        assert!(store.edit_tree(honest, &more).is_err());
    }

    #[test]
    fn a_tree_containing_itself_is_refused_without_hanging() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (dir, store) = new_store();
            let id = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
            let data = tree_bytes(vec![gix::objs::tree::Entry {
                mode: EntryKind::Tree.into(),
                filename: "loop".into(),
                oid: id,
            }]);
            plant(
                &dir.path().join(SNAPSHOTS_DIR),
                &store,
                gix::objs::Kind::Tree,
                &data,
                id,
            );
            let blob = store.write_blob(b"x").unwrap();
            let change = Change::Upsert {
                path: "loop/loop/x".to_owned(),
                kind: Regular,
                blob,
            };
            let refused = store.changed_paths(store.empty_tree(), id).is_err()
                && store.edit_tree(id, &[change]).is_err();
            sender.send(refused).unwrap();
        });
        let refused = receiver
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("reading a tree that contains itself did not finish");
        assert!(refused);
    }

    #[test]
    fn file_changes_lists_each_file_with_its_old_and_new_entry() {
        let (_dir, store) = new_store();
        let old = from_scratch(
            &store,
            &[
                ("same.txt", Regular, "same"),
                ("edited.txt", Regular, "old"),
                ("run.sh", Regular, "script"),
                ("gone/deep/file.txt", Regular, "gone"),
            ],
        );
        let mut changes = upserts(
            &store,
            &[
                ("edited.txt", Regular, "new"),
                ("run.sh", Executable, "script"),
                ("new/link", Symlink, "../same.txt"),
            ],
        );
        changes.push(remove("gone"));
        let new = store.edit_tree(old, &changes).unwrap();
        let blob = |content: &str| store.write_blob(content.as_bytes()).unwrap();
        let file = |kind, content| {
            Some(TreeFile {
                kind,
                blob: blob(content),
            })
        };

        let changed = store.file_changes(old, new, 100).unwrap();
        assert!(changed.complete);
        let expected = [
            ("edited.txt", file(Regular, "old"), file(Regular, "new")),
            ("gone/deep/file.txt", file(Regular, "gone"), None),
            ("new/link", None, file(Symlink, "../same.txt")),
            (
                "run.sh",
                file(Regular, "script"),
                file(Executable, "script"),
            ),
        ]
        .map(|(path, old, new)| FileChange {
            path: path.as_bytes().to_vec(),
            old,
            new,
        });
        assert_eq!(changed.files, expected);
        assert_eq!(store.read_blob(blob("new")).unwrap().unwrap(), b"new");
        let missing = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(store.read_blob(missing).unwrap(), None);
        assert!(!store.has_object(missing));
        assert!(store.has_object(new) && store.has_object(store.empty_tree()));
    }

    #[test]
    fn a_tree_bomb_is_cut_short() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (_dir, store) = new_store();
            // Each level holds the level below twice: 2^40 files, all with valid hashes.
            let mut tree = from_scratch(&store, &[("f", Regular, "x")]);
            for _ in 0..40 {
                let entries = ["a", "b"]
                    .map(|name| gix::objs::tree::Entry {
                        mode: EntryKind::Tree.into(),
                        filename: name.into(),
                        oid: tree,
                    })
                    .to_vec();
                tree = store
                    .repo
                    .write_object(&gix::objs::Tree { entries })
                    .unwrap()
                    .detach();
            }
            let changed = store.file_changes(store.empty_tree(), tree, 1000).unwrap();
            sender
                .send((changed.complete, changed.files.len()))
                .unwrap();
        });
        let (complete, files) = receiver
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("diffing a tree bomb did not finish");
        assert!(!complete);
        assert!(files <= 1000, "{files}");
    }

    #[test]
    fn a_planted_blob_is_refused() {
        let (dir, store) = new_store();
        let honest = store.write_blob(b"honest").unwrap();
        plant(
            &dir.path().join(SNAPSHOTS_DIR),
            &store,
            gix::objs::Kind::Blob,
            b"evil",
            honest,
        );
        let error = store.read_blob(honest).unwrap_err();
        assert!(
            error.to_string().contains("does not match its id"),
            "{error}"
        );
        // A tree where a blob is expected.
        let tree = from_scratch(&store, &[("a", Regular, "a")]);
        assert!(store.read_blob(tree).is_err());
    }

    #[test]
    fn a_correctly_hashed_tree_with_an_invalid_name_is_refused() {
        let (_dir, store) = new_store();
        let blob = store.write_blob(b"x").unwrap();
        for name in ["..", ".git", "a\\..\\b"] {
            if name.contains('\\') && !cfg!(windows) {
                // A backslash is an ordinary character outside Windows.
                continue;
            }
            let tree = gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: EntryKind::Blob.into(),
                    filename: name.into(),
                    oid: blob,
                }],
            };
            // gix writes any tree it is given; `edit_tree` would have refused this name.
            let id = store.repo.write_object(&tree).unwrap().detach();
            let error = store.changed_paths(store.empty_tree(), id).unwrap_err();
            assert!(
                matches!(error, Error::InvalidPath { .. }),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn a_store_with_contents_yalper_does_not_write_is_refused() {
        const BLOB_ID: &str = "ce013625030ba8dba906f756967f9e9ca394464a";
        type Tamper = fn(&Path);
        let cases: [(&str, Tamper); 8] = [
            ("changed config", |store| {
                let config = store.join("config");
                let text = fs::read_to_string(&config).unwrap();
                fs::write(config, text.replace("never", "now")).unwrap();
            }),
            ("longer config", |store| {
                let config = store.join("config");
                let text = fs::read_to_string(&config).unwrap();
                fs::write(config, text + "[core]\n\tfsmonitor = true\n").unwrap();
            }),
            ("config directory", |store| {
                fs::remove_file(store.join("config")).unwrap();
                fs::create_dir(store.join("config")).unwrap();
            }),
            ("large HEAD", |store| {
                fs::write(
                    store.join("HEAD"),
                    "ref: refs/heads/".to_owned() + &"x".repeat(300),
                )
                .unwrap();
            }),
            ("packed-refs", |store| {
                fs::write(
                    store.join("packed-refs"),
                    format!("{BLOB_ID} refs/heads/main\n"),
                )
                .unwrap();
            }),
            ("replace ref", |store| {
                let replace = store.join("refs").join("replace");
                fs::create_dir(&replace).unwrap();
                fs::write(replace.join(BLOB_ID), format!("{BLOB_ID}\n")).unwrap();
            }),
            ("branch", |store| {
                fs::create_dir(store.join("refs").join("heads")).unwrap();
                fs::write(store.join("refs").join("heads").join("main"), BLOB_ID).unwrap();
            }),
            ("pack", |store| {
                let pack = store.join("objects").join("pack");
                fs::write(pack.join("pack-1.pack"), "PACK").unwrap();
            }),
        ];
        for (name, tamper) in cases {
            let (dir, store) = new_store();
            drop(store);
            tamper(&dir.path().join(SNAPSHOTS_DIR));
            let owned = OwnedDir::open(dir.path()).unwrap();
            let error = ShadowStore::open(&owned, &token()).unwrap_err();
            assert!(
                matches!(error, Error::UnexpectedLayout(_)),
                "{name}: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_config_is_refused() {
        let (dir, store) = new_store();
        drop(store);
        let store_dir = dir.path().join(SNAPSHOTS_DIR);
        let outside = tempfile::tempdir().unwrap();
        fs::rename(store_dir.join("config"), outside.path().join("config")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("config"), store_dir.join("config"))
            .unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let error = ShadowStore::open(&owned, &token()).unwrap_err();
        assert!(matches!(error, Error::UnexpectedLayout(_)), "{error}");
    }

    #[test]
    fn the_store_keeps_snapshots_from_gc_and_limits_allocations() {
        let (dir, store) = new_store();
        let config = fs::read_to_string(dir.path().join(SNAPSHOTS_DIR).join("config")).unwrap();
        assert_eq!(config, store_config(&token()));
        let config = store.repo.config_snapshot();
        assert_eq!(config.integer("gc.auto"), Some(0));
        assert_eq!(
            config.string("gc.pruneExpire").unwrap().to_string(),
            "never"
        );
        assert_eq!(
            config.integer("gitoxide.objects.allocLimit"),
            Some(ALLOC_LIMIT_BYTES as i64)
        );
    }

    #[test]
    fn git_fsck_finds_the_store_clean() {
        let (dir, store) = new_store();
        // Names around git's tree order, where a directory sorts as if its name ended with `/`.
        let first = from_scratch(
            &store,
            &[
                ("a-b", Regular, "1"),
                ("a.txt", Regular, "2"),
                ("a/inner", Regular, "3"),
                ("a0", Regular, "4"),
                ("link", Symlink, "a.txt"),
                ("run.sh", Executable, "5"),
                ("becomes_dir", Regular, "6"),
                ("becomes_file/inner", Regular, "7"),
            ],
        );
        let mut changes = upserts(
            &store,
            &[
                ("becomes_dir/inner", Regular, "8"),
                ("becomes_file", Regular, "9"),
                ("a/b-c/deep", Regular, "10"),
            ],
        );
        changes.extend(["becomes_dir", "becomes_file/inner"].map(remove));
        let second = store.edit_tree(first, &changes).unwrap();
        assert_ne!(first, second);

        let output = Command::new("git")
            .arg("--git-dir")
            .arg(dir.path().join(SNAPSHOTS_DIR))
            .args(["fsck", "--strict", "--no-dangling", "--no-progress"])
            .output()
            .expect("git must be installed to run this test");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{text}");
        for line in text.lines() {
            assert!(line.starts_with("notice:"), "{text}");
        }
    }
}
