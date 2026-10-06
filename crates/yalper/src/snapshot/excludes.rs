//! Which files git ignores: the `.gitignore` files inside the project, `info/exclude` of the project's
//! repository, and the user's global excludes file.
//!
//! The `ignore` crate reads ignore files by path, following links, with no size limit. A repository could
//! commit a `.gitignore` that is a link to `/dev/zero` (endless memory), to a FIFO or terminal (a hang), to a
//! file outside the project, or to a UNC path on Windows (a network login). So the walker's own ignore
//! handling is off and Yalper reads `.gitignore` and `info/exclude` itself: only regular files, never through
//! a link, of at most [`MAX_IGNORE_FILE_BYTES`]. A refused file counts as absent, as git does for a symlinked
//! `.gitignore`, and is reported, because the files it would ignore then end up in the snapshot.
//!
//! Compiling patterns costs time on every step, so the files of one snapshot share a budget
//! ([`MAX_IGNORE_BYTES`], [`MAX_IGNORE_WEIGHT`]). Going over it fails the whole snapshot instead of leaving
//! one file out: which file would be left out depends on the order of the parallel walk, so the tree could
//! change from one step to the next with no file changed. Whether the budget is exceeded does not depend on
//! that order: if all the rules a full walk reads fit, every walk reads exactly those.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::{repo, safe_fs};

/// The most bytes one ignore file may have. A larger one is refused (it counts as absent).
pub const MAX_IGNORE_FILE_BYTES: u64 = 256 * 1024;

/// The most bytes of `.gitignore` and `info/exclude` files one snapshot reads, all files together.
pub const MAX_IGNORE_BYTES: u64 = 256 * 1024;

/// The most pattern weight one snapshot compiles, all files together (see [`weight`]). Room for realistic
/// monorepos: the Visual Studio `.gitignore` of github/gitignore, one of the largest templates, weighs 839
/// (1,401 on Windows and macOS, where patterns ignore case); Node, Python and Unity weigh 174 to 269 (340 to
/// 475).
///
/// Measured on the user's Windows machine with the budget filled by one kind of pattern, whole snapshot of a
/// tiny project (5 ms without rules), median: with case-insensitive matching (Windows, macOS) at most about
/// 50 ms, for 113 literals of 250 bytes, 30 of 1,000 bytes, or 24 of 1,250 bytes with a `*`; 31 to 35 ms for
/// 650 to 800 short patterns (literals, `*.ext`, `a1*`, `a1?`, `[Dd]ebug1/`); 21 to 25 ms for alternations
/// (`x1.{js,ts}`, `{ab,cd}` repeated) and `**` patterns; 10 to 15 ms for nested braces and long bracket
/// runs. With case-sensitive matching (Linux) at most about 30 ms (800 patterns like `a1*`).
pub const MAX_IGNORE_WEIGHT: u64 = 4000;

/// Patterns match without regard to case where git does by default (`core.ignoreCase`, set by `git init` on
/// case-insensitive file systems).
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Long patterns cost more to compile: one more unit of weight per this many bytes.
const BYTES_PER_WEIGHT: u64 = 8;

/// What is left for the ignore files of one snapshot.
struct Budget {
    bytes: u64,
    weight: u64,
}

/// Why an existing ignore file is not used.
enum Unused {
    /// A link, not a regular file, unreadable, or larger than [`MAX_IGNORE_FILE_BYTES`]: counts as absent.
    Refused(String),
    /// Over the budget of the snapshot: the snapshot fails.
    OverBudget,
}

/// The `.gitignore` rules that apply inside one directory: its own, if it has any, then those of the
/// nearest directories above it that have some. Directories without rules share their parent's chain, so
/// checking an entry costs one lookup of its parent plus one match per directory that has rules.
struct Chain {
    rules: Gitignore,
    parent: Option<Arc<Chain>>,
}

pub(super) struct Excludes {
    root: PathBuf,
    /// The rule chain of each directory the walk entered, by absolute path. `None`: no `.gitignore` rules
    /// apply inside it.
    chains: RwLock<HashMap<PathBuf, Option<Arc<Chain>>>>,
    info_exclude: Gitignore,
    global: Gitignore,
    budget: Mutex<Budget>,
    /// Ignore files that exist but are not used, with the reason.
    refused: Mutex<Vec<(PathBuf, String)>>,
    exceeded: AtomicBool,
}

impl Excludes {
    /// The rules for the project at `root`, without any `.gitignore` yet (see [`add_dir`](Self::add_dir)).
    pub fn new(root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        let _ = builder.case_insensitive(CASE_INSENSITIVE);
        let (global, _) = builder.build_global();
        let mut excludes = Self {
            root: root.to_owned(),
            chains: RwLock::new(HashMap::new()),
            info_exclude: Gitignore::empty(),
            global,
            budget: Mutex::new(Budget {
                bytes: MAX_IGNORE_BYTES,
                weight: MAX_IGNORE_WEIGHT,
            }),
            refused: Mutex::new(Vec::new()),
            exceeded: AtomicBool::new(false),
        };
        if let Some(dir) = repo::git_common_dir(root) {
            excludes.info_exclude = excludes.rules(root, &dir.join("info").join("exclude"));
        }
        excludes
    }

    /// Reads the `.gitignore` of `dir`. Called for each directory the walk enters, after its parent and
    /// before any entry inside it is checked.
    pub fn add_dir(&self, dir: &Path) {
        let parent = if dir == self.root {
            None
        } else {
            dir.parent().and_then(|parent| self.chain(parent))
        };
        let rules = self.rules(dir, &dir.join(".gitignore"));
        let chain = if rules.is_empty() {
            parent
        } else {
            Some(Arc::new(Chain { rules, parent }))
        };
        if let Ok(mut chains) = self.chains.write() {
            chains.insert(dir.to_owned(), chain);
        }
    }

    fn chain(&self, dir: &Path) -> Option<Arc<Chain>> {
        self.chains.read().ok()?.get(dir).cloned().flatten()
    }

    /// Whether git ignores `path`, an entry inside the project. A deeper `.gitignore` takes precedence over
    /// the ones above it, and all of them over `info/exclude`, then the global excludes file. Entries inside
    /// an ignored directory are never checked, because the walk does not enter it.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let mut chain = path.parent().and_then(|parent| self.chain(parent));
        while let Some(link) = chain {
            if let Some(ignored) = decided(link.rules.matched(path, is_dir)) {
                return ignored;
            }
            chain = link.parent.clone();
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

    /// Whether the ignore files went over the budget: the snapshot fails, so the walk can stop.
    pub fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::Relaxed)
    }

    /// If the budget was exceeded, why. No file is named: which file the total ran out at depends on the
    /// order of the walk.
    pub fn budget_error(&self) -> Option<String> {
        self.exceeded().then(|| {
            format!(
                "the project's .gitignore and info/exclude files go over the budget of {MAX_IGNORE_WEIGHT} \
                 pattern weight or {MAX_IGNORE_BYTES} bytes per snapshot"
            )
        })
    }

    /// The rules of the ignore file at `file`, relative to `dir`. Empty if the file is missing, and also if
    /// it is not used, which is then recorded.
    fn rules(&self, dir: &Path, file: &Path) -> Gitignore {
        match self.load(dir, file) {
            Ok(rules) => return rules,
            Err(Unused::Refused(reason)) => {
                if let Ok(mut refused) = self.refused.lock() {
                    refused.push((file.to_owned(), reason));
                }
            }
            Err(Unused::OverBudget) => self.exceeded.store(true, Ordering::Relaxed),
        }
        Gitignore::empty()
    }

    fn load(&self, dir: &Path, file: &Path) -> Result<Gitignore, Unused> {
        let refused = |reason: String| Unused::Refused(reason);
        let metadata = match fs::symlink_metadata(file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Gitignore::empty()),
            Err(error) => return Err(refused(error.to_string())),
        };
        if metadata.file_type().is_symlink() {
            return Err(refused(
                "it is a link, which git does not follow either".to_owned(),
            ));
        }
        if !metadata.is_file() {
            return Err(refused("it is not a regular file".to_owned()));
        }
        if metadata.len() > MAX_IGNORE_FILE_BYTES {
            return Err(refused(format!(
                "{} bytes, over the {MAX_IGNORE_FILE_BYTES} byte limit",
                metadata.len()
            )));
        }
        self.spend(metadata.len(), 0)?;
        let mut bytes = Vec::new();
        safe_fs::open_regular_file(file)
            .and_then(|opened| opened.take(metadata.len()).read_to_end(&mut bytes))
            .map_err(|error| refused(error.to_string()))?;

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
    fn spend(&self, bytes: u64, weight: u64) -> Result<(), Unused> {
        let mut budget = self.budget.lock().map_err(|_| Unused::OverBudget)?;
        if bytes > budget.bytes || weight > budget.weight {
            return Err(Unused::OverBudget);
        }
        budget.bytes -= bytes;
        budget.weight -= weight;
        Ok(())
    }
}

/// What a pattern costs to compile, in units of about 10 µs on the user's machine (see
/// [`MAX_IGNORE_WEIGHT`]): 1 for a short case-sensitive literal or `*.ext`, which are matched without a
/// regular expression; otherwise 4, plus 1 per `*`, `?`, `[`, and per `{` and `,` of an alternation. Long
/// patterns add 1 per [`BYTES_PER_WEIGHT`] bytes.
pub(super) fn weight(pattern: &str) -> u64 {
    let count = |chars: &[char]| pattern.matches(chars).count() as u64;
    let wildcards = count(&['*', '?', '[']);
    let alternations = if pattern.contains('{') {
        count(&['{', ','])
    } else {
        0
    };
    let length = pattern.len() as u64 / BYTES_PER_WEIGHT;
    let simple_extension = pattern
        .strip_prefix("*.")
        .is_some_and(|extension| !extension.contains(['*', '?', '[', '{', '/']));
    if !CASE_INSENSITIVE && alternations == 0 && (wildcards == 0 || simple_extension) {
        1 + length
    } else {
        4 + wildcards + alternations + length
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
