//! The user's git repository around a project, which Yalper reads but never writes while recording: where its
//! git directory is, and the init token that ties a `.yalper/` to it.
//!
//! A cloned repository can commit a whole `.yalper/` (database, snapshot store, any marker file), so nothing
//! inside `.yalper/` proves that `yalper init` created it. `yalper init` writes one random token to
//! `.yalper/id` and to `<git dir>/yalper-id`, and Yalper only uses a `.yalper/` whose token matches. Git never
//! copies anything inside the git directory through a clone, so a repository cannot forge the token.

use std::fs;
use std::path::{Path, PathBuf};

use crate::safe_fs::{self, OwnedDir};

/// The token file inside `.yalper/`.
pub const ID_FILE: &str = "id";

/// The token file inside the git directory of the project.
pub const GIT_ID_FILE: &str = "yalper-id";

/// A token is 128 random bits written as this many lowercase hex digits, optionally followed by a newline.
pub const TOKEN_HEX_DIGITS: usize = 32;

/// The `.git` file of a linked worktree or submodule holds one short line.
const MAX_GIT_FILE_BYTES: u64 = 4096;

/// A token file is never larger than this.
const MAX_TOKEN_FILE_BYTES: u64 = 64;

/// The git directory of the repository whose work tree is `root`: `.git` itself, or for a linked worktree or
/// a submodule (a `.git` file) the directory that file names.
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if fs::symlink_metadata(&dot_git).ok()?.is_dir() {
        return Some(dot_git);
    }
    let text = safe_fs::read_small_regular_file(&dot_git, MAX_GIT_FILE_BYTES)?;
    Some(
        root.join(
            String::from_utf8(text)
                .ok()?
                .strip_prefix("gitdir:")?
                .trim(),
        ),
    )
}

/// The directory holding `info/exclude` for the repository whose work tree is `root`: `.git` itself, or for
/// a linked worktree the repository's common directory.
pub fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let git_dir = git_dir(root)?;
    match safe_fs::read_small_regular_file(&git_dir.join("commondir"), MAX_GIT_FILE_BYTES) {
        Some(common) => Some(git_dir.join(String::from_utf8(common).ok()?.trim())),
        None => Some(git_dir),
    }
}

/// Whether `yalper init` created `yalper_dir` for the repository whose work tree is `root`: `.yalper/id` and
/// `<git dir>/yalper-id` are regular files holding the same valid token. Every caller that opens the event
/// log or the snapshot store of a `.yalper/` it found on disk checks this first.
pub fn is_initialized(root: &Path, yalper_dir: &OwnedDir) -> bool {
    let Some(git_dir) = git_dir(root) else {
        return false;
    };
    match (
        read_token(&git_dir.join(GIT_ID_FILE)),
        read_token(&yalper_dir.path().join(ID_FILE)),
    ) {
        (Some(expected), Some(found)) => expected == found,
        _ => false,
    }
}

fn read_token(path: &Path) -> Option<String> {
    let bytes = safe_fs::read_small_regular_file(path, MAX_TOKEN_FILE_BYTES)?;
    let text = String::from_utf8(bytes).ok()?;
    let token = text.strip_suffix('\n').map_or(text.as_str(), |line| {
        line.strip_suffix('\r').unwrap_or(line)
    });
    let valid = token.len() == TOKEN_HEX_DIGITS
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    valid.then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// A project with `.git/` and `.yalper/`, and the given token files (`None`: no file).
    fn project(git_token: Option<&str>, yalper_token: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir(root.path().join(".yalper")).unwrap();
        if let Some(token) = git_token {
            fs::write(root.path().join(".git").join(GIT_ID_FILE), token).unwrap();
        }
        if let Some(token) = yalper_token {
            fs::write(root.path().join(".yalper").join(ID_FILE), token).unwrap();
        }
        root
    }

    fn initialized(root: &Path) -> bool {
        is_initialized(root, &OwnedDir::open(&root.join(".yalper")).unwrap())
    }

    #[test]
    fn matching_tokens_mark_an_initialized_project() {
        assert!(initialized(project(Some(TOKEN), Some(TOKEN)).path()));
        let with_newlines = format!("{TOKEN}\n");
        assert!(initialized(
            project(Some(&with_newlines), Some(&format!("{TOKEN}\r\n"))).path()
        ));
    }

    #[test]
    fn a_missing_or_different_token_is_refused() {
        let other = "fedcba9876543210fedcba9876543210";
        for (git, yalper) in [
            (Some(TOKEN), None),
            (None, Some(TOKEN)),
            (None, None),
            (Some(TOKEN), Some(other)),
        ] {
            assert!(
                !initialized(project(git, yalper).path()),
                "{git:?} {yalper:?}"
            );
        }
    }

    #[test]
    fn malformed_tokens_are_refused_even_when_equal() {
        for token in [
            "",
            "\n",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef",
            " 0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef0",
        ] {
            assert!(
                !initialized(project(Some(token), Some(token)).path()),
                "{token:?}"
            );
        }
    }

    #[test]
    fn a_directory_in_place_of_a_token_file_is_refused() {
        let root = project(Some(TOKEN), None);
        fs::create_dir(root.path().join(".yalper").join(ID_FILE)).unwrap();
        assert!(!initialized(root.path()));
    }

    #[test]
    fn the_token_of_a_linked_worktree_is_in_its_own_git_dir() {
        let main = tempfile::tempdir().unwrap();
        let worktree_git_dir = main.path().join(".git").join("worktrees").join("wt");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        fs::write(worktree_git_dir.join(GIT_ID_FILE), TOKEN).unwrap();

        let worktree = tempfile::tempdir().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        fs::create_dir(worktree.path().join(".yalper")).unwrap();
        fs::write(worktree.path().join(".yalper").join(ID_FILE), TOKEN).unwrap();

        assert_eq!(git_dir(worktree.path()), Some(worktree_git_dir.clone()));
        assert_eq!(
            git_common_dir(worktree.path()),
            Some(worktree_git_dir.join("../.."))
        );
        assert!(initialized(worktree.path()));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_token_file_is_refused() {
        let root = project(Some(TOKEN), None);
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("token");
        fs::write(&target, TOKEN).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(".yalper").join(ID_FILE)).unwrap();
        assert!(!initialized(root.path()));
    }
}
