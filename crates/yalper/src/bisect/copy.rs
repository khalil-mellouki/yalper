//! Test copies: folders `.yalper/bisect/copy-<n>/` that hold the code of one snapshot step, reused from one
//! step to the next.
//!
//! Switching a copy to another step writes only the files that differ, so a switch costs milliseconds even
//! in a large project. Nothing on disk is trusted between two bisects: the first switch of a [`TestCopy`]
//! hashes every file of the target tree found in the copy and writes only those that are missing or differ.
//! Later switches apply the tree diff between the step the copy holds and the next one.
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
//! - the cached bytecode of every Python file written or removed is deleted (see [`remove_bytecode`]).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;

use gix::ObjectId;

use super::BISECT_DIR;
use crate::safe_fs::{self, OwnedDir};
use crate::snapshot::{
    Error, FileChange, FileKind, MAX_FILE_BYTES, Result, ShadowStore, TreeFile, validate_path,
};

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

/// A test copy: a real folder `.yalper/bisect/copy-<n>/` that [`switch`](Self::switch) fills with the code of
/// a snapshot step.
#[derive(Debug)]
pub struct TestCopy {
    dir: OwnedDir,
    /// The tree the copy holds, `None` until a switch succeeded: the first switch then checks every file.
    tree: Option<ObjectId>,
}

/// What a [`TestCopy::switch`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Switched {
    /// Files and links written.
    pub written: usize,
    /// Files and links removed.
    pub removed: usize,
}

impl TestCopy {
    /// Opens test copy number `number` of the `.yalper/` folder `yalper_dir`, creating `.yalper/bisect/` and
    /// the copy if they are missing, as real folders accessible only by their owner on Unix. A link at either
    /// path is refused, and so is a folder other users can change.
    pub fn open(yalper_dir: &OwnedDir, number: usize) -> io::Result<Self> {
        let bisect = open_private_dir(&yalper_dir.path().join(BISECT_DIR))?;
        let dir = open_private_dir(&bisect.path().join(format!("copy-{number}")))?;
        Ok(Self { dir, tree: None })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Makes the copy hold the tree `to` of `store`, writing only what differs: on the first switch of this
    /// `TestCopy`, the files of `to` that are missing in the copy or differ from it; afterwards, the files
    /// that differ between the tree the copy holds and `to`. Files the tree does not have are not touched
    /// by a first switch.
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
        let first = from.is_none();
        let changes =
            store.file_changes(from.unwrap_or_else(|| store.empty_tree()), to, max_changes)?;
        if !changes.complete {
            return Err(io::Error::other(format!(
                "cannot switch the test copy: more than {max_changes} files and folders differ"
            ))
            .into());
        }
        let changes = checked(&changes.files)?;

        // Checked right before writing: a test run could have replaced the copy's folder.
        self.dir.check_still_at_path()?;
        let root = self.dir.path();
        make_owner_writable(root, &fs::symlink_metadata(root)?)?;

        let mut switched = Switched::default();
        let mut kept = HashMap::new();
        for &(path, new) in &changes {
            if new.is_none()
                && remove(root, path, &mut kept).map_err(|error| in_copy(error, path))?
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
            if prepare(root, path, new, first, &kept, &mut ready)
                .map_err(|error| in_copy(error, path))?
            {
                files.push((path, new));
            }
        }
        write_files(store, root, &files)?;
        switched.written += files.len();

        ready.clear();
        for (path, new) in links {
            if prepare(root, path, new, first, &kept, &mut ready)
                .map_err(|error| in_copy(error, path))?
            {
                let target = read(store, path, new)?;
                write_link(&native(root, path), &target).map_err(|error| in_copy(error, path))?;
                switched.written += 1;
            }
        }

        self.tree = Some(to);
        Ok(switched)
    }
}

/// Creates the folder `path` if it is missing (on Unix with mode 0700) and opens it, refusing a link and, on
/// Unix, a folder its group or other users can write to.
fn open_private_dir(path: &Path) -> io::Result<OwnedDir> {
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
fn native(root: &Path, path: &str) -> PathBuf {
    let mut native = root.to_path_buf();
    native.extend(path.split('/'));
    native
}

/// The folders above `path`, outermost first: `a`, `a/b` for `a/b/c`.
fn parents(path: &str) -> impl Iterator<Item = &str> {
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
fn remove(root: &Path, path: &str, kept: &mut HashMap<String, String>) -> io::Result<bool> {
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
        remove_bytecode(&full)?;
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
        remove_bytecode(&full)?;
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
fn make_owner_writable(dir: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700))?;
    }
    Ok(())
}

/// Windows lets a folder's entries be replaced whatever the folder's attributes.
#[cfg(windows)]
fn make_owner_writable(_dir: &Path, _metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

/// Whether the entry at `path` already holds `file`: the same kind and the same content, hashed like a blob.
fn holds(path: &Path, file: TreeFile) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    if file.kind == FileKind::Symlink {
        return Ok(metadata.is_symlink() && is_blob(&link_target(path)?, file.blob)?);
    }
    // On Windows, a link of the tree is a plain file holding its target.
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES || !plain_as(&metadata, file.kind) {
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

/// Whether the regular file with `metadata` is stored as `kind` would write it: on Unix, the same executable
/// bit and no other hard link (writing to the copy's file must not change another file).
#[cfg(unix)]
fn plain_as(metadata: &fs::Metadata, kind: FileKind) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() == 1 && (metadata.mode() & 0o100 != 0) == (kind == FileKind::Executable)
}

/// Windows has no executable bit. The standard library does not expose the hard link count there (it is
/// unstable), so a file with a second hard link whose content matches is kept by a first switch, and a test
/// writing to it would change the other file too. Only a local process can create a hard link (a clone
/// cannot), and later switches always replace a file instead of writing into it.
#[cfg(windows)]
fn plain_as(_metadata: &fs::Metadata, _kind: FileKind) -> bool {
    true
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
fn write_file(path: &Path, kind: FileKind, bytes: &[u8]) -> io::Result<()> {
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
fn remove_entry(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.is_dir() {
        return fs::remove_dir_all(path);
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
    #[cfg(windows)]
    if metadata.permissions().readonly() {
        let mut permissions = metadata.permissions();
        // Clears the Windows read-only attribute; this code does not run on Unix.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    fs::remove_file(path)
}

/// Removes the cached bytecode of the Python file at `path`: the `__pycache__/<name>.*.pyc` files next to it,
/// then `__pycache__` if it is left empty.
///
/// Python takes its bytecode as current while the source keeps its size and its mtime in whole seconds, so a
/// same-size change written in the same second as the previous version would otherwise run the old code.
fn remove_bytecode(path: &Path) -> io::Result<()> {
    let (Some(dir), Some(name)) = (
        path.parent(),
        path.file_stem().and_then(|name| name.to_str()),
    ) else {
        return Ok(());
    };
    let cache = dir.join("__pycache__");
    if !is_real_dir(&cache)? {
        return Ok(());
    }
    let prefix = format!("{name}.");
    for entry in fs::read_dir(&cache)? {
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
    // Fails if something is left, which is fine.
    let _ = fs::remove_dir(&cache);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::process::Command;

    use gix::objs::tree::{Entry, EntryKind};

    use crate::repo::Token;
    use crate::snapshot::Change;

    use FileKind::{Executable, Regular, Symlink};

    /// A project folder with a `.yalper/` holding a snapshot store.
    struct Project {
        root: tempfile::TempDir,
        yalper: OwnedDir,
        store: ShadowStore,
    }

    impl Project {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join(".yalper")).unwrap();
            let yalper = OwnedDir::open(&root.path().join(".yalper")).unwrap();
            let token = Token::parse("0123456789abcdef0123456789abcdef").unwrap();
            let store = ShadowStore::init(&yalper, &token).unwrap();
            Self {
                root,
                yalper,
                store,
            }
        }

        fn copy(&self) -> TestCopy {
            TestCopy::open(&self.yalper, 0).unwrap()
        }

        /// The tree holding `files` (path, kind, content).
        fn tree(&self, files: &[(&str, FileKind, &str)]) -> ObjectId {
            let changes: Vec<Change> = files
                .iter()
                .map(|&(path, kind, content)| Change::Upsert {
                    path: path.to_owned(),
                    kind,
                    blob: self.store.write_blob(content.as_bytes()).unwrap(),
                })
                .collect();
            self.store
                .edit_tree(self.store.empty_tree(), &changes)
                .unwrap()
        }

        /// A tree with the entries `(name, kind, id)`, written without any path check.
        fn crafted_tree(&self, entries: &[(&str, EntryKind, ObjectId)]) -> ObjectId {
            let mut entries: Vec<Entry> = entries
                .iter()
                .map(|&(name, kind, oid)| Entry {
                    mode: kind.into(),
                    filename: name.into(),
                    oid,
                })
                .collect();
            entries.sort();
            self.store
                .write_unchecked_tree(&gix::objs::Tree { entries })
                .unwrap()
        }
    }

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
                    let kind = if plain_as(&metadata, Executable) && cfg!(unix) {
                        Executable
                    } else {
                        Regular
                    };
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

    /// Borrowed form of generated files, for [`Project::tree`] and [`assert_holds`].
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
    fn copies_are_real_private_folders_in_yalper_bisect() {
        let project = Project::new();
        let copy = TestCopy::open(&project.yalper, 3).unwrap();
        let bisect = project.root.path().join(".yalper").join(BISECT_DIR);
        assert!(fs::symlink_metadata(&bisect).unwrap().is_dir());
        assert_eq!(
            fs::canonicalize(copy.path()).unwrap(),
            fs::canonicalize(bisect.join("copy-3")).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&bisect, &bisect.join("copy-3")] {
                let mode = fs::metadata(dir).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
            }
        }
        // Opening it again reuses it.
        TestCopy::open(&project.yalper, 3).unwrap();
    }

    #[test]
    fn a_link_at_the_bisect_folder_is_refused() {
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        link_dir(
            outside.path(),
            &project.root.path().join(".yalper").join(BISECT_DIR),
        );
        assert!(TestCopy::open(&project.yalper, 0).is_err());
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
        let (tree_a, tree_b) = (project.tree(&a), project.tree(&b));
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
        let tree = project.tree(&[
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
        let (tree_flat, tree_nested) = (project.tree(&flat), project.tree(&nested));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_flat).unwrap();
        let switched = copy.switch(&project.store, tree_nested).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 1,
                removed: 0
            }
        );
        assert_holds(&copy, &nested);
        let switched = copy.switch(&project.store, tree_flat).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 0,
                removed: 1
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
        let (tree_file, tree_folder) = (project.tree(&file), project.tree(&folder));
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
        let tree = project.tree(&[("x", Regular, "a file")]);
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
        let (tree_old, tree_new) = (project.tree(&old), project.tree(&new));
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
        let old = [("Src/Lib/a.txt", Regular, "a")];
        let new = [("src/lib/a.txt", Regular, "a")];
        let (tree_old, tree_new) = (project.tree(&old), project.tree(&new));
        let mut copy = project.copy();
        copy.switch(&project.store, tree_old).unwrap();
        // A build output the tree does not have keeps both folders from being removed.
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
            assert_eq!(names(copy.path()), ["src"]);
            assert_eq!(names(&src), ["lib"]);
            assert_eq!(names(&src.join("lib")), ["a.txt", "out.o"]);
        } else {
            // Two folders on a case-sensitive file system: the old one keeps only the build output.
            assert_eq!(names(copy.path()), ["Src", "src"]);
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
        let (tree_a, tree_b) = (project.tree(&a), project.tree(&b));

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
        copy.switch(&project.store, project.tree(&[])).unwrap();
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
        let tree = project.tree(&files);
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
        copy.switch(&project.store, project.tree(&[("f.txt", Regular, "old")]))
            .unwrap();
        fs::remove_file(copy.path().join("f.txt")).unwrap();
        fs::hard_link(&other, copy.path().join("f.txt")).unwrap();
        // Treated as not matching on a first switch, and replaced, not written into, on a later one.
        let mut again = project.copy();
        let tree = project.tree(&[("f.txt", Regular, "old")]);
        assert_eq!(again.switch(&project.store, tree).unwrap().written, 1);
        fs::remove_file(copy.path().join("f.txt")).unwrap();
        fs::hard_link(&other, copy.path().join("f.txt")).unwrap();
        let new = project.tree(&[("f.txt", Regular, "new")]);
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
        let (tree_base, tree_step) = (project.tree(&view(&base)), project.tree(&view(&step)));

        let mut copy = project.copy();
        let switched = copy.switch(&project.store, tree_base).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 100,
                removed: 0
            }
        );
        let switched = copy.switch(&project.store, tree_step).unwrap();
        assert_eq!(
            switched,
            Switched {
                written: 5,
                removed: 0
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
                removed: 0
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
        let tree = project.tree(&files);
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
        let tree = project.tree(&files);
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
        let (tree_a, tree_b) = (project.tree(&a), project.tree(&b));
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
        let tree = project.tree(&[
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
        copy.switch(&project.store, project.tree(&files)).unwrap();
        assert_holds(&copy, &files);
    }

    #[test]
    fn a_switch_that_fails_after_writing_is_undone_by_the_next_switch() {
        let project = Project::new();
        let a = [("a.txt", Regular, "a"), ("m.txt", Regular, "m")];
        let tree_a = project.tree(&a);
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
                removed: 0
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
        let tree = project.tree(&files);
        let mut copy = project.copy();
        assert_eq!(copy.switch(&project.store, tree).unwrap().written, 300);
        assert_holds(&copy, &files);
    }

    #[test]
    fn bytecode_of_a_written_or_removed_python_file_is_removed() {
        let project = Project::new();
        let a = [
            ("pkg/mod.py", Regular, "VALUE = 1\n"),
            ("pkg/other.py", Regular, ""),
        ];
        let b = [
            ("pkg/mod.py", Regular, "VALUE = 2\n"),
            ("pkg/other.py", Regular, ""),
        ];
        let (tree_a, tree_b) = (project.tree(&a), project.tree(&b));
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
        copy.switch(&project.store, project.tree(&[])).unwrap();
        assert!(!copy.path().join("pkg").exists());
    }

    /// A Python 3 interpreter on `PATH`. Without one the test is skipped, unless `YALPER_REQUIRE_TOOLS=1`.
    fn python() -> Option<&'static str> {
        let found = ["python3", "python"].into_iter().find(|name| {
            Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        });
        assert!(
            found.is_some() || std::env::var("YALPER_REQUIRE_TOOLS").as_deref() != Ok("1"),
            "Python is required (YALPER_REQUIRE_TOOLS=1) but neither python3 nor python runs"
        );
        found
    }

    #[test]
    fn python_runs_a_same_size_change_made_in_the_same_second() {
        let Some(python) = python() else { return };
        let project = Project::new();
        let a = [("mod.py", Regular, "VALUE = 1\n")];
        let b = [("mod.py", Regular, "VALUE = 2\n")];
        let (tree_a, tree_b) = (project.tree(&a), project.tree(&b));
        let mut copy = project.copy();
        let value = |copy: &TestCopy| {
            let output = Command::new(python)
                .args(["-c", "import mod, sys; sys.stdout.write(str(mod.VALUE))"])
                .current_dir(copy.path())
                .env_remove("PYTHONDONTWRITEBYTECODE")
                .env_remove("PYTHONPYCACHEPREFIX")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };

        copy.switch(&project.store, tree_a).unwrap();
        assert_eq!(value(&copy), "1");
        let cache = copy.path().join("__pycache__");
        assert_eq!(
            fs::read_dir(&cache).unwrap().count(),
            1,
            "no bytecode written"
        );
        let source = copy.path().join("mod.py");
        let mtime = fs::metadata(&source).unwrap().modified().unwrap();

        copy.switch(&project.store, tree_b).unwrap();
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
}
