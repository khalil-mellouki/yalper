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

use std::fmt;
use std::fs;
use std::io;

use gix::ObjectId;
use gix::bstr::BStr;
use gix::objs::tree::EntryKind;
use gix::validate::path::component;

use crate::safe_fs::OwnedDir;

/// The shadow repository inside `.yalper/`.
pub const SNAPSHOTS_DIR: &str = "snapshots.git";

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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Git(error) => write!(f, "snapshot store error: {error}"),
            Self::InvalidPath { path, reason } => {
                write!(f, "cannot store the path {path:?} in a snapshot: {reason}")
            }
            Self::MissingObject(id) => write!(f, "the snapshot store has no object {id}"),
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
            Self::InvalidPath { .. } | Self::MissingObject(_) | Self::UnexpectedLayout(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
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
    _dir: OwnedDir,
}

impl fmt::Debug for ShadowStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShadowStore")
            .field("path", &self.repo.git_dir())
            .finish()
    }
}

impl ShadowStore {
    /// Creates an empty store in `yalper_dir` and opens it. Fails if anything, even a link, already exists
    /// at its path.
    pub fn init(yalper_dir: &OwnedDir) -> Result<Self> {
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
        Self::open(yalper_dir)
    }

    /// Opens the store in `yalper_dir`.
    ///
    /// The store must be a real directory (on Unix owned by the current user), and contain no link in
    /// `objects/`, no alternates and no `commondir` file: any of these could send writes or reads to another
    /// repository, such as the user's own `.git`. Remaining gap, outside the threat model: another process of
    /// the same user could plant such a link after these checks.
    pub fn open(yalper_dir: &OwnedDir) -> Result<Self> {
        let dir = OwnedDir::open(&yalper_dir.path().join(SNAPSHOTS_DIR))?;
        check_layout(&dir)?;
        let options = gix::open::Options::isolated()
            .open_path_as_is(true)
            // Ownership was checked by `OwnedDir` (Unix). Asking gix to check it again costs time, mostly on
            // Windows, and with isolated options trust only decides whether the store's own config is used.
            .with(gix::sec::Trust::Full);
        let mut repo = gix::open_opts(dir.path(), options)?;
        // Every new blob or tree is first looked up and not found. By default each miss rescans the store's
        // pack directory, which this store never has (measured: 22 ms instead of 15 ms per 3-file step on
        // Windows). Loose objects are still found.
        repo.objects.refresh_never();
        Ok(Self { repo, _dir: dir })
    }

    /// Stores `bytes` as a blob and returns its id. Content already in the store is not written again.
    pub fn write_blob(&self, bytes: &[u8]) -> Result<ObjectId> {
        Ok(self.repo.write_blob(bytes)?.detach())
    }

    /// The id of the tree with no entries, the base of the first snapshot.
    pub fn empty_tree(&self) -> ObjectId {
        ObjectId::empty_tree(self.repo.object_hash())
    }

    /// Builds the tree that results from applying `changes` to the tree `base`, writes it, and returns its
    /// id. Only the trees on the paths of the changes are rewritten.
    ///
    /// Removals are applied before additions, so a file replaced by a directory of the same name (or the
    /// reverse) can be given in any order. Every path is checked before anything is written, and every
    /// blob must already be in the store.
    pub fn edit_tree(&self, base: ObjectId, changes: &[Change]) -> Result<ObjectId> {
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
        let mut editor = self.repo.edit_tree(base)?.detach();
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
        let tree = editor.write(|tree| self.repo.write_object(tree).map(|id| id.detach()))?;
        Ok(tree)
    }

    /// The files added, modified and deleted between the trees `old` and `new`. Directories are not listed,
    /// only the files in them.
    pub fn changed_paths(&self, old: ObjectId, new: ObjectId) -> Result<ChangedPaths> {
        let old = self.repo.find_tree(old)?;
        let new = self.repo.find_tree(new)?;
        let mut recorder = gix::diff::tree::Recorder::default();
        gix::diff::tree(
            gix::objs::TreeRefIter::from_bytes(&old.data, self.repo.object_hash()),
            gix::objs::TreeRefIter::from_bytes(&new.data, self.repo.object_hash()),
            gix::diff::tree::State::default(),
            &self.repo.objects,
            &mut recorder,
        )
        .map_err(|error| Error::Git(gix::Error::from_error(error)))?;

        let mut changed = ChangedPaths::default();
        for change in recorder.records {
            use gix::diff::tree::recorder::Change::{Addition, Deletion, Modification};
            let (list, path) = match change {
                Addition {
                    entry_mode, path, ..
                } if !entry_mode.is_tree() => (&mut changed.added, path),
                Deletion {
                    entry_mode, path, ..
                } if !entry_mode.is_tree() => (&mut changed.deleted, path),
                // A file and a directory of the same name are different entries in git's sort order, so a
                // file replaced by a directory shows as a deletion plus additions, never as a modification.
                Modification {
                    entry_mode, path, ..
                } if !entry_mode.is_tree() => (&mut changed.modified, path),
                _ => continue,
            };
            list.push(path.to_string());
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
}

/// Checks that `path` can be stored in a tree as a file of `kind`: relative, `/`-separated, with no empty,
/// `.`, `..` or `.git` component and no name the platform's file system treats specially (see
/// [`PATH_RULES`]).
pub fn validate_path(path: &str, kind: FileKind) -> Result<()> {
    let mut components = path.split('/').peekable();
    while let Some(name) = components.next() {
        let mode = (components.peek().is_none() && kind == FileKind::Symlink)
            .then_some(component::Mode::Symlink);
        if let Err(error) = gix::validate::path::component(BStr::new(name), mode, PATH_RULES) {
            return Err(Error::InvalidPath {
                path: path.to_owned(),
                reason: error.to_string(),
            });
        }
    }
    Ok(())
}

/// See [`ShadowStore::open`].
fn check_layout(dir: &OwnedDir) -> Result<()> {
    let root = dir.path();
    for unexpected in ["commondir", "objects/info/alternates"] {
        match fs::symlink_metadata(root.join(unexpected)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(Error::UnexpectedLayout(format!("`{unexpected}`"))),
            Err(error) => return Err(error.into()),
        }
    }
    let objects = root.join("objects");
    if !fs::symlink_metadata(&objects)?.is_dir() {
        return Err(Error::UnexpectedLayout(
            "an `objects` that is not a directory".to_owned(),
        ));
    }
    // One listing of at most 258 entries; the type comes with the listing on Windows, Linux and macOS.
    for entry in fs::read_dir(&objects)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            return Err(Error::UnexpectedLayout(format!(
                "a link at `objects/{}`",
                entry.file_name().to_string_lossy()
            )));
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

    /// A `.yalper` directory with an initialized store in it.
    fn new_store() -> (tempfile::TempDir, ShadowStore) {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = ShadowStore::init(&owned).unwrap();
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
        let store = ShadowStore::open(&owned).unwrap();
        let changed = store.changed_paths(store.empty_tree(), tree).unwrap();
        assert_eq!(changed.added, ["a/b.txt"]);
    }

    #[test]
    fn init_refuses_an_existing_path_and_open_a_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(ShadowStore::open(&owned).is_err());
        fs::create_dir(dir.path().join(SNAPSHOTS_DIR)).unwrap();
        assert!(ShadowStore::init(&owned).is_err());
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
            let error = ShadowStore::open(&owned).unwrap_err();
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
        let error = ShadowStore::open(&owned).unwrap_err();
        assert!(matches!(error, Error::UnexpectedLayout(_)), "{error}");

        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        ShadowStore::init(&OwnedDir::open(elsewhere.path()).unwrap()).unwrap();
        link_dir(
            &elsewhere.path().join(SNAPSHOTS_DIR),
            &dir.path().join(SNAPSHOTS_DIR),
        );
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(ShadowStore::open(&owned).is_err());
        assert!(ShadowStore::init(&owned).is_err());
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

    fn git(project: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "core.autocrlf=false"])
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
        let store = ShadowStore::init(&yalper).unwrap();
        // The same content as the user's committed file: it is still written to the shadow store.
        let old = from_scratch(&store, &[("main.rs", Regular, "fn main() {}\n")]);
        let added = upserts(&store, &[("lib.rs", Regular, "pub fn f() {}\n")]);
        let new = store.edit_tree(old, &added).unwrap();
        drop(store);
        let store = ShadowStore::open(&yalper).unwrap();
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
        let store = ShadowStore::init(&owned).unwrap();
        let head = fs::read_to_string(yalper.join(SNAPSHOTS_DIR).join("HEAD")).unwrap();
        assert!(!head.contains("from-global"), "{head}");
        assert!(store.repo.config_snapshot().string("user.name").is_none());
        drop(store);

        let store = ShadowStore::open(&owned).unwrap();
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
        ShadowStore::init(&OwnedDir::open(home.path()).unwrap()).unwrap();
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
}
