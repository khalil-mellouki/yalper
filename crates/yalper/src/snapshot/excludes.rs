//! Which files git ignores: the `.gitignore` files inside the project, `info/exclude` of the project's
//! repository, and the user's global excludes file.
//!
//! The `ignore` crate reads ignore files by path, following links, with no size limit. A repository could
//! commit a `.gitignore` that is a link to `/dev/zero` (endless memory), to a FIFO or terminal (a hang), to a
//! file outside the project, or to a UNC path on Windows (a network login). So the walker's own ignore
//! handling is off and Yalper reads these files itself: only regular files, never through a link, and at most
//! [`MAX_IGNORE_FILE_BYTES`]. A refused `.gitignore` counts as absent, as git does for a symlinked one.
//!
//! Remaining gap, outside the threat model: on Windows, another process of the same user could swap a file
//! for a link between the check and the open (on Unix the open itself refuses links).

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::safe_fs;

/// Ignore files larger than this are not used.
pub const MAX_IGNORE_FILE_BYTES: u64 = 1024 * 1024;

/// The `.git` file of a linked worktree or submodule holds one short line.
const MAX_GIT_FILE_BYTES: u64 = 4096;

pub(super) struct Excludes {
    root: PathBuf,
    /// The rules of each directory that has a usable `.gitignore`, by absolute path.
    gitignores: RwLock<HashMap<PathBuf, Gitignore>>,
    info_exclude: Gitignore,
    global: Gitignore,
}

impl Excludes {
    /// The rules for the project at `root`, without any `.gitignore` yet (see [`add_dir`](Self::add_dir)).
    pub fn new(root: &Path) -> Self {
        let info_exclude = git_common_dir(root)
            .map(|dir| matcher(root, &dir.join("info").join("exclude")))
            .unwrap_or_else(Gitignore::empty);
        // The user's own configuration (`core.excludesFile`, or `git/ignore` in the config directory), which
        // a repository cannot change.
        let (global, _) = GitignoreBuilder::new(root).build_global();
        Self {
            root: root.to_owned(),
            gitignores: RwLock::new(HashMap::new()),
            info_exclude,
            global,
        }
    }

    /// Reads the `.gitignore` of `dir`. Called for each directory before any entry inside it is checked.
    pub fn add_dir(&self, dir: &Path) {
        let gitignore = matcher(dir, &dir.join(".gitignore"));
        if !gitignore.is_empty()
            && let Ok(mut gitignores) = self.gitignores.write()
        {
            gitignores.insert(dir.to_owned(), gitignore);
        }
    }

    /// Whether git ignores `path`, an entry inside the project. A deeper `.gitignore` takes precedence over
    /// the ones above it, and all of them over `info/exclude`, then the global excludes file. Entries inside
    /// an ignored directory are never checked, because the walk does not enter it.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        if let Ok(gitignores) = self.gitignores.read() {
            for dir in path.ancestors().skip(1) {
                let found = gitignores.get(dir).map(|rules| rules.matched(path, is_dir));
                if let Some(ignored) = found.and_then(decided) {
                    return ignored;
                }
                if dir == self.root {
                    break;
                }
            }
        }
        decided(self.info_exclude.matched(path, is_dir))
            .or_else(|| decided(self.global.matched(path, is_dir)))
            .unwrap_or(false)
    }
}

/// The rules of the ignore file at `file`, relative to `dir`. Empty if the file is missing or refused.
fn matcher(dir: &Path, file: &Path) -> Gitignore {
    let Some(text) = read_small_regular_file(file, MAX_IGNORE_FILE_BYTES) else {
        return Gitignore::empty();
    };
    let mut builder = GitignoreBuilder::new(dir);
    for line in String::from_utf8_lossy(&text).lines() {
        // git skips a pattern it cannot parse, and so does Yalper.
        let _ = builder.add_line(None, line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The content of `path` if it is a regular file (not a link, FIFO or device) of at most `max_bytes`.
fn read_small_regular_file(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    let mut bytes = Vec::new();
    safe_fs::open_regular_file(path)
        .ok()?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= max_bytes).then_some(bytes)
}

/// The directory holding `info/exclude` for the repository whose work tree is `root`: `.git` itself, or for
/// a linked worktree (a `.git` file) the repository's common directory.
fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if fs::symlink_metadata(&dot_git).ok()?.is_dir() {
        return Some(dot_git);
    }
    let text = read_small_regular_file(&dot_git, MAX_GIT_FILE_BYTES)?;
    let git_dir = root.join(
        String::from_utf8(text)
            .ok()?
            .strip_prefix("gitdir:")?
            .trim(),
    );
    match read_small_regular_file(&git_dir.join("commondir"), MAX_GIT_FILE_BYTES) {
        Some(common) => Some(git_dir.join(String::from_utf8(common).ok()?.trim())),
        None => Some(git_dir),
    }
}

/// Whether a match ignores (`Some(true)`) or re-includes (`Some(false)`) the path, or `None` if no pattern
/// matched.
fn decided<T>(found: Match<T>) -> Option<bool> {
    match found {
        Match::None => None,
        Match::Ignore(_) => Some(true),
        Match::Whitelist(_) => Some(false),
    }
}
