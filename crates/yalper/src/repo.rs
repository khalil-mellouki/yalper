//! The user's git repository around a project, which Yalper reads but never writes while recording: where its
//! root and git directory are, and the init token that ties a `.yalper/` to it.
//!
//! A cloned repository can commit a whole `.yalper/` (database, snapshot store, any marker file), so nothing
//! inside `.yalper/` proves that `yalper init` created it. `yalper init` writes one random token to
//! `.yalper/id` and to `<git dir>/yalper-id`, and Yalper only uses a `.yalper/` whose token matches. Git never
//! copies anything inside the git directory through a clone, so a repository cannot forge the token.
//!
//! A pulled commit can still overwrite files inside an existing `.yalper/`, so the event log and the snapshot
//! store also carry the token, and are refused when it differs (see `Store::open` and `ShadowStore::open`).

use std::fmt;
use std::fs;
use std::io;
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

/// A git directory's `config` or `gitdir` file larger than this is not read.
const MAX_GIT_CONFIG_BYTES: u64 = 1024 * 1024;

/// A token file is never larger than this.
const MAX_TOKEN_FILE_BYTES: u64 = 64;

/// The init token: 32 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// A new token from the operating system's secure random number generator.
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0u8; TOKEN_HEX_DIGITS / 2];
        getrandom::fill(&mut bytes).map_err(|error| {
            io::Error::other(format!(
                "cannot get random bytes from the operating system: {error}"
            ))
        })?;
        Ok(Self(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }

    /// Reads a token as written in a token file: exactly [`TOKEN_HEX_DIGITS`] lowercase hex digits, optionally
    /// followed by `\n` or `\r\n`.
    pub fn parse(text: &str) -> Option<Self> {
        let token = text
            .strip_suffix('\n')
            .map_or(text, |line| line.strip_suffix('\r').unwrap_or(line));
        let valid = token.len() == TOKEN_HEX_DIGITS
            && token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        valid.then(|| Self(token.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The root of the repository around `start`: the nearest directory, `start` included, that contains a `.git`
/// directory or file. A submodule or a nested repository is its own root.
pub fn find_root(start: &Path) -> Option<&Path> {
    start
        .ancestors()
        .find(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())
}

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

/// Whether `git_dir`, the git directory that the `.git` file of `root` names, names `root` back: a linked
/// worktree's git directory has a `gitdir` file naming that `.git` file, and a submodule's git directory has
/// a `core.worktree` setting naming the folder. Without this check, a `.git` file could make Yalper use (and
/// `yalper uninstall` clean up) the git directory of another repository.
pub fn git_dir_links_back(root: &Path, git_dir: &Path) -> bool {
    let read = |name: &str| {
        safe_fs::read_small_regular_file(&git_dir.join(name), MAX_GIT_CONFIG_BYTES)
            .and_then(|bytes| String::from_utf8(bytes).ok())
    };
    let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if let Some(gitdir) = read("gitdir") {
        return same(&git_dir.join(gitdir.trim()), &root.join(".git"));
    }
    read("config")
        .as_deref()
        .and_then(core_worktree)
        .is_some_and(|worktree| same(&git_dir.join(worktree), root))
}

/// The value of `core.worktree` in the git config text `config`, if it is set there.
fn core_worktree(config: &str) -> Option<&str> {
    let mut in_core = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line.eq_ignore_ascii_case("[core]");
        } else if in_core
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("worktree")
        {
            return Some(value.trim().trim_matches('"'));
        }
    }
    None
}

/// A `.yalper/` directory that `yalper init` created for its repository, open, with its init token.
#[derive(Debug)]
pub struct YalperDir {
    pub dir: OwnedDir,
    pub token: Token,
}

/// Why [`open_yalper_dir`] refuses a `.yalper/` directory.
#[derive(Debug)]
pub enum Refusal {
    /// It is not a real directory owned by the current user (a link, a file, another owner), or cannot be
    /// opened.
    NotOwnedDir(io::Error),
    /// Its group or other users can create, rename or delete entries in it (Unix).
    WritableByOthers,
    /// `.yalper/id` and `<git dir>/yalper-id` do not hold the same valid token.
    TokenMismatch,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOwnedDir(error) => write!(f, "{error}"),
            Self::WritableByOthers => {
                f.write_str("other users can change it (it is group or world writable)")
            }
            Self::TokenMismatch => write!(
                f,
                "it was not created by `yalper init` for this repository (its {ID_FILE} file does not \
                 hold the same init token as {GIT_ID_FILE} in the git directory)"
            ),
        }
    }
}

/// Opens the `.yalper/` directory at `path` for the repository whose work tree is `root`, or says why it
/// cannot be used. It must be a real directory owned by the current user (see [`OwnedDir`]), on Unix not
/// writable by its group or other users, and `.yalper/id` and `<git dir>/yalper-id` must be regular files
/// holding the same valid token. Every caller that opens the event log or the snapshot store of a `.yalper/`
/// it found on disk goes through this first, `yalper init` included.
pub fn open_yalper_dir(root: &Path, path: &Path) -> Result<YalperDir, Refusal> {
    let dir = OwnedDir::open(path).map_err(Refusal::NotOwnedDir)?;
    if dir.is_writable_by_others().map_err(Refusal::NotOwnedDir)? {
        return Err(Refusal::WritableByOthers);
    }
    let token = init_token(root, &dir).ok_or(Refusal::TokenMismatch)?;
    Ok(YalperDir { dir, token })
}

/// The init token of `yalper_dir`, if `.yalper/id` and `<git dir>/yalper-id` of the repository whose work
/// tree is `root` are regular files holding the same valid token.
pub fn init_token(root: &Path, yalper_dir: &OwnedDir) -> Option<Token> {
    let expected = read_token(&git_dir(root)?.join(GIT_ID_FILE))?;
    let found = read_token(&yalper_dir.path().join(ID_FILE))?;
    (expected == found).then_some(found)
}

/// The token in the file at `path`, if it is a small regular file holding a valid token.
pub fn read_token(path: &Path) -> Option<Token> {
    let bytes = safe_fs::read_small_regular_file(path, MAX_TOKEN_FILE_BYTES)?;
    Token::parse(&String::from_utf8(bytes).ok()?)
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
        open_yalper_dir(root, &root.join(".yalper")).is_ok()
    }

    #[test]
    fn matching_tokens_mark_an_initialized_project() {
        assert!(initialized(project(Some(TOKEN), Some(TOKEN)).path()));
        let with_newlines = format!("{TOKEN}\n");
        assert!(initialized(
            project(Some(&with_newlines), Some(&format!("{TOKEN}\r\n"))).path()
        ));
        let root = project(Some(TOKEN), Some(TOKEN));
        let found = open_yalper_dir(root.path(), &root.path().join(".yalper")).unwrap();
        assert_eq!(found.token.as_str(), TOKEN);
    }

    #[test]
    fn generated_tokens_have_the_token_file_format_and_differ() {
        let first = Token::generate().unwrap();
        assert_eq!(Token::parse(first.as_str()), Some(first.clone()));
        assert_eq!(Token::parse(&format!("{first}\n")), Some(first.clone()));
        assert_ne!(Token::generate().unwrap(), first);
    }

    #[test]
    fn the_root_is_the_nearest_directory_with_git() {
        let root = project(None, None);
        let deep = root.path().join("a").join("b");
        fs::create_dir_all(deep.join("nested")).unwrap();
        fs::write(deep.join("nested").join(".git"), "gitdir: x\n").unwrap();
        assert_eq!(find_root(&deep), Some(root.path()));
        assert_eq!(
            find_root(&deep.join("nested")),
            Some(deep.join("nested").as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_yalper_dir_other_users_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o770, 0o707, 0o777] {
            let root = project(Some(TOKEN), Some(TOKEN));
            let yalper = root.path().join(".yalper");
            fs::set_permissions(&yalper, fs::Permissions::from_mode(mode)).unwrap();
            let error = open_yalper_dir(root.path(), &yalper)
                .unwrap_err()
                .to_string();
            assert!(error.contains("other users"), "{mode:o}: {error}");
            fs::set_permissions(&yalper, fs::Permissions::from_mode(0o750)).unwrap();
            assert!(initialized(root.path()), "{mode:o}");
        }
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

    #[test]
    fn core_worktree_is_read_from_the_core_section_only() {
        let config =
            "[core]\n\tbare = false\n\tWorktree = \"../../../sub\"\n[other]\n\tworktree = x\n";
        assert_eq!(core_worktree(config), Some("../../../sub"));
        assert_eq!(core_worktree("[other]\n\tworktree = x\n"), None);
        assert_eq!(core_worktree("[core]\n\tbare = false\n"), None);
    }

    #[test]
    fn a_git_file_must_be_named_back_by_its_git_dir() {
        let other = project(None, None);
        let root = tempfile::tempdir().unwrap();
        let other_git = other.path().join(".git");
        assert!(!git_dir_links_back(root.path(), &other_git));
        fs::write(other_git.join("gitdir"), "/somewhere/else/.git\n").unwrap();
        assert!(!git_dir_links_back(root.path(), &other_git));
        fs::write(
            other_git.join("gitdir"),
            format!("{}\n", root.path().join(".git").display()),
        )
        .unwrap();
        fs::write(root.path().join(".git"), "gitdir: x\n").unwrap();
        assert!(git_dir_links_back(root.path(), &other_git));
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
