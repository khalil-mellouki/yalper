//! Helpers shared by the integration tests.

#![allow(dead_code)] // Each test crate uses a different part.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

/// The `yalper` binary built for the integration tests.
pub const EXE: &str = env!("CARGO_BIN_EXE_yalper");

/// Runs `git` in `dir` with a fixed identity and checks that it succeeds. Automatic maintenance is off:
/// after a commit, git can start it in the background, and its lock file in `.git/objects` would come and
/// go while tests compare files.
pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A new git repository with one file.
pub fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    dir
}

/// Runs the real `yalper` binary in `dir` with a temporary folder of its own, so the warning about a binary
/// inside the temporary folder never shows up because of where the test build lives.
pub fn yalper(dir: &Path, args: &[&str]) -> Output {
    let temp = tempfile::tempdir().unwrap();
    Command::new(EXE)
        .args(args)
        .current_dir(dir)
        .env("TMPDIR", temp.path())
        .env("TMP", temp.path())
        .env("TEMP", temp.path())
        .output()
        .unwrap()
}

/// Every file under `dir` and its content. SQLite's shared memory file is left out: it is rewritten whenever
/// the database is opened, whatever the database holds.
pub fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if !path.to_string_lossy().ends_with("-shm") {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}
