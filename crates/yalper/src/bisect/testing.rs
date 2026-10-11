//! Helpers shared by the tests of the bisect modules.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gix::ObjectId;
use gix::objs::tree::{Entry, EntryKind};

use super::TestCopy;
use crate::repo::Token;
use crate::safe_fs::OwnedDir;
use crate::snapshot::{Change, FileKind, ShadowStore};

/// A project folder with a `.yalper/` holding a snapshot store.
pub struct Project {
    root: tempfile::TempDir,
    pub yalper: OwnedDir,
    pub store: ShadowStore,
}

impl Project {
    pub fn new() -> Self {
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

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Writes `content` at `path` in the project, creating the folders above it.
    pub fn write(&self, path: &str, content: &str) {
        write(self.path(), path, content);
    }

    /// The tree holding `files` (path, content), all regular files.
    pub fn tree(&self, files: &[(&str, &str)]) -> ObjectId {
        let files: Vec<(&str, FileKind, &str)> = files
            .iter()
            .map(|&(path, content)| (path, FileKind::Regular, content))
            .collect();
        self.typed_tree(&files)
    }

    /// The tree holding `files` (path, kind, content).
    pub fn typed_tree(&self, files: &[(&str, FileKind, &str)]) -> ObjectId {
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
    pub fn crafted_tree(&self, entries: &[(&str, EntryKind, ObjectId)]) -> ObjectId {
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

    /// Test copy number 0.
    pub fn copy(&self) -> TestCopy {
        TestCopy::open(&self.yalper, 0).unwrap()
    }
}

/// Writes `content` at `path` inside `root`, creating the folders above it.
pub fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

/// Every entry under `dir` (links not followed), except `.yalper` at its top: kind, content or link target,
/// and mtime. Two equal states mean that nothing under `dir` was changed.
pub fn state(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if dir == root && path.file_name() == Some(OsStr::new(".yalper")) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).unwrap();
            let what = if metadata.is_symlink() {
                format!("link to {}", fs::read_link(&path).unwrap().display())
            } else if metadata.is_dir() {
                walk(root, &path, out);
                "folder".to_owned()
            } else {
                format!("file {:?}", fs::read(&path).unwrap())
            };
            let mtime = metadata.modified().unwrap();
            out.insert(path, format!("{what}, modified {mtime:?}"));
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The first of `names` that runs with `--version`. Without one the test is skipped, unless
/// `YALPER_REQUIRE_TOOLS=1`, which CI sets.
pub fn tool(names: &[&'static str]) -> Option<&'static str> {
    let found = names.iter().copied().find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    });
    assert!(
        found.is_some() || std::env::var("YALPER_REQUIRE_TOOLS").as_deref() != Ok("1"),
        "{names:?} is required (YALPER_REQUIRE_TOOLS=1) but does not run"
    );
    found
}

/// The Python interpreter of the virtual environment at `venv`.
pub fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}
