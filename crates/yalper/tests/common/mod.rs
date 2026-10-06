//! Helpers shared by the integration tests.

#![allow(dead_code)] // Each test crate uses a different part.

use std::fs;
use std::path::Path;

use yalper::hook::YALPER_DIR;
use yalper::repo::{GIT_ID_FILE, ID_FILE, Token, YalperDir};
use yalper::safe_fs::OwnedDir;
use yalper::snapshot::ShadowStore;

/// An init token, as `yalper init` writes it.
pub const TOKEN: &str = "0123456789abcdef0123456789abcdef";

pub fn token() -> Token {
    Token::parse(TOKEN).unwrap()
}

/// Sets up `.yalper/` in the git project at `root` (which must have a `.git` directory) the way `yalper init`
/// does, without its baseline snapshot and settings: the private directory, the snapshot store, and the init
/// token in `.yalper/id` and `.git/yalper-id`.
pub fn init(root: &Path) -> YalperDir {
    let yalper_dir = root.join(YALPER_DIR);
    fs::create_dir(&yalper_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&yalper_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(yalper_dir.join(ID_FILE), format!("{TOKEN}\n")).unwrap();
    fs::write(root.join(".git").join(GIT_ID_FILE), format!("{TOKEN}\n")).unwrap();
    let dir = OwnedDir::open(&yalper_dir).unwrap();
    ShadowStore::init(&dir, &token()).unwrap();
    YalperDir {
        dir,
        token: token(),
    }
}

/// A temporary git project (it only needs a `.git` directory) set up with [`init`].
pub fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    init(dir.path());
    dir
}
