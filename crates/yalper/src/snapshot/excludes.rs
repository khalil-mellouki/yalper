//! Which files git ignores: the `.gitignore` files inside the project, `info/exclude` of the project's
//! repository, and the user's global excludes file.
//!
//! The `ignore` crate reads ignore files by path, following links, with no size limit. A repository could
//! commit a `.gitignore` that is a link to `/dev/zero` (endless memory), to a FIFO or terminal (a hang), to a
//! file outside the project, or to a UNC path on Windows (a network login). So the walker's own ignore
//! handling is off and Yalper reads `.gitignore` and `info/exclude` itself: only regular files, never through
//! a link. A refused `.gitignore` counts as absent, as git does for a symlinked one.
//!
//! Compiling patterns costs time on every step (measured: 2 to 26 µs per pattern, most for `**` and
//! brackets), so the files of one snapshot share a [`Budget`]. A file that does not fit counts as absent.
//! Every refused file is reported, because its files then end up in the snapshot.
//!
//! The global excludes file (`core.excludesFile`, or `git/ignore` in the user's config directory) belongs to
//! the user, not to a repository. It is read by the `ignore` crate, which follows links (dotfiles are often
//! links) and has no size limit, and it does not count against the budget.
//!
//! Matching uses `globset`, which differs from git in two known ways: POSIX classes such as `[[:digit:]]`
//! never match, and an unclosed `[abc` is taken literally.
//!
//! Remaining gap, outside the threat model: on Windows, another process of the same user could swap a file
//! for a link between the check and the open (on Unix the open itself refuses links).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::safe_fs;

/// The most bytes of `.gitignore` and `info/exclude` files one snapshot reads, all files together.
pub const MAX_IGNORE_BYTES: u64 = 1024 * 1024;

/// The most pattern weight one snapshot compiles, all files together. A pattern weighs 1 plus 1 per `*`, `?`
/// or `[`. Measured on the user's machine: the Visual Studio `.gitignore` of github/gitignore weighs 465 and
/// compiles in 4 ms; the costliest patterns take about 26 µs per unit, so the budget costs at most about
/// 26 ms there.
pub const MAX_IGNORE_WEIGHT: u64 = 1000;

/// Patterns match without regard to case where git does by default (`core.ignoreCase`, set by `git init` on
/// case-insensitive file systems).
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// The `.git` file of a linked worktree or submodule holds one short line.
const MAX_GIT_FILE_BYTES: u64 = 4096;

/// What is left for the ignore files of one snapshot.
struct Budget {
    bytes: u64,
    weight: u64,
}

pub(super) struct Excludes {
    root: PathBuf,
    /// The rules of each directory that has a usable `.gitignore`, by absolute path.
    gitignores: RwLock<HashMap<PathBuf, Gitignore>>,
    info_exclude: Gitignore,
    global: Gitignore,
    budget: Mutex<Budget>,
    /// Ignore files that exist but are not used, with the reason.
    refused: Mutex<Vec<(PathBuf, String)>>,
}

impl Excludes {
    /// The rules for the project at `root`, without any `.gitignore` yet (see [`add_dir`](Self::add_dir)).
    pub fn new(root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        let _ = builder.case_insensitive(CASE_INSENSITIVE);
        let (global, _) = builder.build_global();
        let mut excludes = Self {
            root: root.to_owned(),
            gitignores: RwLock::new(HashMap::new()),
            info_exclude: Gitignore::empty(),
            global,
            budget: Mutex::new(Budget {
                bytes: MAX_IGNORE_BYTES,
                weight: MAX_IGNORE_WEIGHT,
            }),
            refused: Mutex::new(Vec::new()),
        };
        if let Some(dir) = git_common_dir(root) {
            excludes.info_exclude = excludes.rules(root, &dir.join("info").join("exclude"));
        }
        excludes
    }

    /// Reads the `.gitignore` of `dir`. Called for each directory before any entry inside it is checked.
    pub fn add_dir(&self, dir: &Path) {
        let gitignore = self.rules(dir, &dir.join(".gitignore"));
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

    /// The ignore files that exist but were not used, with the reason, sorted by path.
    pub fn refused(&self) -> Vec<(PathBuf, String)> {
        let mut refused = self
            .refused
            .lock()
            .map(|refused| refused.clone())
            .unwrap_or_default();
        refused.sort();
        refused
    }

    /// The rules of the ignore file at `file`, relative to `dir`. Empty if the file is missing, and also if
    /// it is refused, which is then recorded.
    fn rules(&self, dir: &Path, file: &Path) -> Gitignore {
        match self.load(dir, file) {
            Ok(rules) => rules,
            Err(reason) => {
                if let Ok(mut refused) = self.refused.lock() {
                    refused.push((file.to_owned(), reason));
                }
                Gitignore::empty()
            }
        }
    }

    fn load(&self, dir: &Path, file: &Path) -> Result<Gitignore, String> {
        let metadata = match fs::symlink_metadata(file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Gitignore::empty()),
            Err(error) => return Err(error.to_string()),
        };
        if metadata.file_type().is_symlink() {
            return Err("it is a link, which git does not follow either".to_owned());
        }
        if !metadata.is_file() {
            return Err("it is not a regular file".to_owned());
        }
        self.spend(metadata.len(), 0)?;
        let mut bytes = Vec::new();
        safe_fs::open_regular_file(file)
            .and_then(|opened| opened.take(metadata.len()).read_to_end(&mut bytes))
            .map_err(|error| error.to_string())?;

        let text = String::from_utf8_lossy(&bytes);
        // git skips a byte order mark at the start of the file.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let patterns: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .collect();
        let weight = patterns.iter().map(|pattern| weight(pattern)).sum();
        self.spend(0, weight)?;

        let mut builder = GitignoreBuilder::new(dir);
        let _ = builder.case_insensitive(CASE_INSENSITIVE);
        for pattern in patterns {
            // git skips a pattern it cannot parse, and so does Yalper.
            let _ = builder.add_line(None, pattern);
        }
        Ok(builder.build().unwrap_or_else(|_| Gitignore::empty()))
    }

    /// Takes `bytes` and `weight` from the budget, or nothing if either does not fit.
    fn spend(&self, bytes: u64, weight: u64) -> Result<(), String> {
        let mut budget = self
            .budget
            .lock()
            .map_err(|_| "the ignore file budget is unavailable".to_owned())?;
        if bytes > budget.bytes {
            return Err(format!(
                "over the budget of {MAX_IGNORE_BYTES} bytes for all ignore files"
            ));
        }
        if weight > budget.weight {
            return Err(format!(
                "over the budget of {MAX_IGNORE_WEIGHT} pattern weight for all ignore files"
            ));
        }
        budget.bytes -= bytes;
        budget.weight -= weight;
        Ok(())
    }
}

/// What a pattern costs to compile, roughly: 1, plus 1 per `*`, `?` or `[`.
fn weight(pattern: &str) -> u64 {
    1 + pattern.matches(['*', '?', '[']).count() as u64
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
