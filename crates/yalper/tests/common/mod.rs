//! Helpers shared by the integration tests.

#![allow(dead_code)] // Each test crate uses a different part.

use std::fs;
use std::path::Path;

use yalper::hook::YALPER_DIR;
use yalper::repo::{GIT_ID_FILE, ID_FILE};
use yalper::safe_fs::OwnedDir;
use yalper::snapshot::ShadowStore;

/// An init token, as `yalper init` writes it.
pub const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Sets up `.yalper/` in the git project at `root` (which must have a `.git` directory) the way `yalper init`
/// does: the directory, the snapshot store, and the init token in `.yalper/id` and `.git/yalper-id`.
pub fn init(root: &Path) -> OwnedDir {
    let yalper_dir = root.join(YALPER_DIR);
    fs::create_dir(&yalper_dir).unwrap();
    fs::write(yalper_dir.join(ID_FILE), format!("{TOKEN}\n")).unwrap();
    fs::write(root.join(".git").join(GIT_ID_FILE), format!("{TOKEN}\n")).unwrap();
    let owned = OwnedDir::open(&yalper_dir).unwrap();
    ShadowStore::init(&owned).unwrap();
    owned
}

/// A temporary git project (it only needs a `.git` directory) set up with [`init`].
pub fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    init(dir.path());
    dir
}
