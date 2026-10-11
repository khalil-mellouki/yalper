//! Reuse of the project's Python virtual environments in a test copy, without letting tests change them.
//!
//! For each folder of the step that holds a Python manifest (`pyproject.toml`, `setup.py`,
//! `requirements*.txt`) where the project has a virtual environment `.venv` or `venv`, the copy gets one link
//! to the project's environment: a symlink on Unix, a junction on Windows. An environment cannot be moved, but
//! it works through a link: Python finds `pyvenv.cfg` next to the path it was started from, so the copy's
//! `.venv/bin/python` runs with the project's installed packages.
//!
//! Python and uv would write into the environment on their own: bytecode next to the packages a test
//! imports, and `uv run` syncing the environment. The variables of [`env_additions`](super::env_additions)
//! stop both (`PYTHONPYCACHEPREFIX`, `UV_NO_SYNC`). Known limit: a test that installs packages itself
//! (`pip install`) writes into the project's environment.
//!
//! Editable installs point at the project's folders (uv installs the project editable by default, and so do
//! `pip install -e` and Poetry), so tests would import the project's current code instead of the step's.
//! [`PythonEnvs::python_path`] gives the same folders inside the copy, to put first in `PYTHONPATH`, which
//! Python searches before site-packages. They are read from the `*.pth` files and the setuptools
//! `__editable__*finder.py` mappings of the environment's site-packages. Known limit: a package that the step
//! does not have yet is still imported from the project.
//!
//! The rules, as for the `node_modules` link farms (see [`NodeModules`](super::NodeModules)):
//!
//! - an environment is reused only if it is a real folder reached through real folders, holds a regular
//!   `pyvenv.cfg`, is ignored (by the project's rules, or by its own `.gitignore`, which `python -m venv` and
//!   uv write), and the step's tree has nothing at its path (compared without regard to case);
//! - every link is an extra path of the copy (see [`TestCopy::set_extra_paths`]), checked before every run:
//!   anything else at its path (a link a test changed, a file, a folder) is removed, never followed, and the
//!   link made anew. A link no longer wanted is removed;
//! - files of the environment are read only through real folders, never through a link, and up to
//!   [`MAX_READ_BYTES`]; the paths they hold are mapped into the copy from their text only, never followed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str::Chars;

use super::TestCopy;
use super::copy::{join, native, parents, remove_entry};
use super::link::{
    CANNOT_LINK, at, has_special_prefix, is_network, is_real_dir_no_follow, is_under, link_to,
    list, normalize, plain,
};
use crate::safe_fs;
use crate::snapshot::{CASE_INSENSITIVE, Excludes};

/// The names of the virtual environment folders that are reused.
const ENV_NAMES: [&str; 2] = [".venv", "venv"];

/// The most bytes read of one `.pth` or finder file. Real ones hold a few lines.
const MAX_READ_BYTES: u64 = 1024 * 1024;

/// The most `.pth` and finder files read in one environment. A large environment has a few dozen.
const MAX_FILES_READ: usize = 1000;

/// The most bytes read in one environment, all `.pth` and finder files together.
const MAX_ENV_BYTES: u64 = 4 * 1024 * 1024;

/// The most editable install folders kept for one environment. A large monorepo has a few dozen.
const MAX_ROOTS: usize = 256;

/// The links of one test copy to the project's Python virtual environments (the copy that reuses the
/// project's installed dependencies).
pub struct PythonEnvs {
    /// The project folder, absolute: the links point into it.
    project: PathBuf,
    /// The project folder with every link resolved: editable paths may be written in either form.
    project_real: PathBuf,
    /// Whether the project is on a network drive (see [`CANNOT_LINK`]).
    on_network: bool,
    excludes: Excludes,
    /// Folders of the project whose `.gitignore` was read into `excludes`.
    rules_read: HashSet<String>,
    /// For each environment path (relative, `/`-separated): `None` if the project has no environment there
    /// that can be reused, otherwise the folders of its editable installs inside the project, relative. Read
    /// once, since the project's environments do not change during a bisect.
    found: HashMap<String, Option<Vec<PathBuf>>>,
    /// The links in the copy, relative and `/`-separated.
    links: BTreeSet<String>,
    /// Whether a link was made in the copy (see [`link_to`]).
    made_link: bool,
}

/// What a [`PythonEnvs::link`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinkedEnvs {
    /// Links to environments in the copy after the call.
    pub envs: usize,
    /// Links created, new or in place of a wrong entry.
    pub links: usize,
    /// Links no longer wanted, removed.
    pub removed: usize,
}

impl PythonEnvs {
    /// The environments of a copy of the project at `project`. Use one value per copy, for one bisect.
    pub fn new(project: &Path) -> io::Result<Self> {
        // On Windows, a mapped network drive resolves to its share.
        let real = fs::canonicalize(project)?;
        let project = plain(&std::path::absolute(project)?);
        Ok(Self {
            excludes: Excludes::new(&project),
            project,
            on_network: is_network(&real),
            project_real: plain(&real),
            rules_read: HashSet::new(),
            found: HashMap::new(),
            links: BTreeSet::new(),
            made_link: false,
        })
    }

    /// Makes the copy's links to environments match the step it holds: called after every
    /// [`TestCopy::switch`], before the test run. Links no longer wanted are removed; the links are registered
    /// as extra paths of the copy, next to the extra paths set by others. Links an earlier bisect left are
    /// removed by the first switch, which removes every link that is not an extra path.
    ///
    /// Fails with [`CANNOT_LINK`] if a link is needed but cannot be made on this system: for a project on a
    /// network drive before changing anything, otherwise when the first link fails.
    pub fn link(&mut self, copy: &mut TestCopy) -> io::Result<LinkedEnvs> {
        let files: Vec<&str> = copy
            .tree_files()
            .ok_or_else(|| io::Error::other("the test copy holds no step yet"))?
            .collect();
        let mut candidates = BTreeSet::new();
        for file in &files {
            let (dir, name) = file.rsplit_once('/').unwrap_or(("", file));
            if is_manifest(name) {
                candidates.extend(ENV_NAMES.iter().map(|env| join(dir, env)));
            }
        }
        let occupied = occupied(
            &files,
            candidates.iter().chain(&self.links).map(String::as_str),
        );
        let mut wanted = BTreeSet::new();
        for path in candidates {
            if !occupied.contains(&key(&path)) && self.find(&path)? {
                wanted.insert(path);
            }
        }
        if self.on_network && !wanted.is_empty() {
            return Err(io::Error::new(
                CANNOT_LINK,
                "the project is on a network drive, and Windows junctions can only point to local \
                 folders",
            ));
        }

        let root = plain(&std::path::absolute(copy.path())?);
        let mut linked = LinkedEnvs::default();
        for stale in self.links.difference(&wanted) {
            // Where the tree now has files, the path is the tree's: the switch already replaced the link.
            if !occupied.contains(&key(stale)) && remove_link(&root, stale)? {
                linked.removed += 1;
            }
        }

        // Registered before the links are made, so a link left half made by an error is still known to the
        // next call.
        let mut extra = copy.extra_paths().clone();
        extra.retain(|path| !self.links.contains(path));
        extra.extend(wanted.iter().cloned());
        copy.set_extra_paths(extra);
        self.links = wanted;

        for path in &self.links {
            // The switch made every folder of the tree a real folder; checked again, so nothing is ever
            // written through a link.
            for parent in parents(path) {
                if !is_real_dir_no_follow(&native(&root, parent)) {
                    return Err(io::Error::other(format!(
                        "cannot link {path} in the test copy: {parent} is not a real folder"
                    )));
                }
            }
            let full = native(&root, path);
            let is_link = fs::symlink_metadata(&full).is_ok_and(|metadata| metadata.is_symlink());
            let made = link_to(
                &full,
                is_link,
                &native(&self.project, path),
                &mut self.made_link,
            )
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot link {path} in the test copy: {error}"),
                )
            })?;
            if made {
                linked.links += 1;
            }
        }
        linked.envs = self.links.len();
        Ok(linked)
    }

    /// The folders of the editable installs of the environments linked in `copy`, inside the copy: to put
    /// first in `PYTHONPATH`, so that tests import the step's code. In the order Python reads them.
    pub fn python_path(&self, copy: &TestCopy) -> io::Result<Vec<PathBuf>> {
        let root = plain(&std::path::absolute(copy.path())?);
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        for link in &self.links {
            for relative in self.found.get(link).into_iter().flatten().flatten() {
                let path = if relative.as_os_str().is_empty() {
                    root.clone()
                } else {
                    root.join(relative)
                };
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
        }
        Ok(paths)
    }

    /// Reads, once, whether the project has a reusable environment at `env` (see [`found`](Self::found)).
    fn find(&mut self, env: &str) -> io::Result<bool> {
        if let Some(found) = self.found.get(env) {
            return Ok(found.is_some());
        }
        let full = at(&self.project, env);
        let reusable = parents(env).all(|parent| is_real_dir_no_follow(&at(&self.project, parent)))
            && is_real_dir_no_follow(&full)
            && fs::symlink_metadata(full.join("pyvenv.cfg"))
                .is_ok_and(|metadata| metadata.is_file())
            && self.is_ignored(env);
        let found = if reusable {
            Some(self.editable_roots(env)?)
        } else {
            None
        };
        self.found.insert(env.to_owned(), found);
        Ok(reusable)
    }

    /// Whether the project's ignore rules ignore the environment folder `env` (relative, `/`-separated), or
    /// its own `.gitignore` ignores what it holds (the `*` that `python -m venv` and uv write there). If the
    /// rules go over the budget of a snapshot, nothing is taken as ignored.
    fn is_ignored(&mut self, env: &str) -> bool {
        // The rules of every folder above it, outermost first, as a walk of the project reads them.
        for dir in std::iter::once("").chain(parents(env)) {
            if self.rules_read.insert(dir.to_owned()) {
                self.excludes.add_dir(&at(&self.project, dir), true);
            }
        }
        let full = at(&self.project, env);
        if self.excludes.exceeded() {
            return false;
        }
        if self.excludes.is_ignored(&full, true) {
            return true;
        }
        if self.rules_read.insert(env.to_owned()) {
            self.excludes.add_dir(&full, true);
        }
        !self.excludes.exceeded() && self.excludes.is_ignored(&full.join("pyvenv.cfg"), false)
    }

    /// The folders of the editable installs of the environment `env`, relative to the project: the paths of
    /// its `*.pth` files and setuptools finders that lead into the project, outside the environment itself.
    /// A hostile environment cannot make this slow: at most [`MAX_FILES_READ`] files and [`MAX_ENV_BYTES`]
    /// bytes are read and [`MAX_ROOTS`] folders kept, and what is over a limit is left out.
    fn editable_roots(&self, env: &str) -> io::Result<Vec<PathBuf>> {
        let env_paths = [native(&self.project, env), native(&self.project_real, env)];
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        let mut files_read = 0;
        let mut bytes_left = MAX_ENV_BYTES;
        for site in site_packages(&native(&self.project, env))? {
            let mut pth = Vec::new();
            let mut finders = Vec::new();
            for (name, file_type, path) in list(&site)? {
                // The type of the entry itself: a link is not read.
                let Some(name) = name.to_str().filter(|_| file_type.is_file()) else {
                    continue;
                };
                // Python skips hidden `.pth` files.
                if name.ends_with(".pth") && !name.starts_with('.') {
                    pth.push((name.to_owned(), path));
                } else if name.starts_with("__editable__") && name.ends_with("finder.py") {
                    finders.push((name.to_owned(), path));
                }
            }
            // In the order Python reads them: `.pth` files first, by name, then the finders they install.
            pth.sort_unstable();
            finders.sort_unstable();
            let files = pth
                .iter()
                .map(|(_, path)| (path, false))
                .chain(finders.iter().map(|(_, path)| (path, true)));
            for (path, is_finder) in files {
                if files_read == MAX_FILES_READ || bytes_left == 0 || roots.len() == MAX_ROOTS {
                    return Ok(roots);
                }
                files_read += 1;
                // A file larger than the bytes left is skipped and later, smaller ones are still read: the
                // total stays within MAX_ENV_BYTES.
                let Some(bytes) =
                    safe_fs::read_small_regular_file(path, MAX_READ_BYTES.min(bytes_left))
                else {
                    continue;
                };
                bytes_left -= bytes.len() as u64;
                let found: Vec<PathBuf> = if is_finder {
                    // A package folder or a module file: the folder that holds it is the root.
                    mapping_values(&String::from_utf8_lossy(&bytes))
                        .into_iter()
                        .filter_map(|value| {
                            normalize(&site.join(value)).parent().map(Path::to_owned)
                        })
                        .collect()
                } else {
                    pth_paths(&bytes)
                        .into_iter()
                        .map(|line| site.join(line))
                        .collect()
                };
                for path in found {
                    let path = normalize(&plain(&path));
                    if has_special_prefix(&path) || env_paths.iter().any(|env| is_under(&path, env))
                    {
                        continue;
                    }
                    if let Some(relative) = self.relative(&path)
                        && is_plain_entry(&relative)
                        && seen.insert(relative.clone())
                    {
                        roots.push(relative);
                        if roots.len() == MAX_ROOTS {
                            return Ok(roots);
                        }
                    }
                }
            }
        }
        Ok(roots)
    }

    /// `path` relative to the project's folder, if it is inside it (in either form, see
    /// [`project_real`](Self::project_real)).
    fn relative(&self, path: &Path) -> Option<PathBuf> {
        [&self.project, &self.project_real]
            .into_iter()
            .find(|root| is_under(path, root))
            .map(|root| path.components().skip(root.components().count()).collect())
    }
}

/// Whether `relative`, an editable folder relative to the project, can be put in `PYTHONPATH` as one entry
/// that stays inside the copy: its text has no path list separator (`;` on Windows, `:` on Unix), which
/// would split it into entries that could lead elsewhere, no `"`, and no control character such as NUL.
fn is_plain_entry(relative: &Path) -> bool {
    let separator = if cfg!(windows) { ';' } else { ':' };
    relative.to_str().is_some_and(|text| {
        !text
            .chars()
            .any(|c| c == separator || c == '"' || c.is_control())
    })
}

/// Whether `name` is the name of a Python manifest: `pyproject.toml`, `setup.py` or `requirements*.txt`,
/// without regard to case where the file system ignores it.
fn is_manifest(name: &str) -> bool {
    let name = if CASE_INSENSITIVE {
        name.to_ascii_lowercase()
    } else {
        name.to_owned()
    };
    name == "pyproject.toml"
        || name == "setup.py"
        || (name.starts_with("requirements") && name.ends_with(".txt"))
}

/// `path` in the form that tells whether two paths of the tree may be the same file: lowercase, since even a
/// case-sensitive file system may have folders that ignore case (ext4 `casefold`).
fn key(path: &str) -> String {
    path.to_lowercase()
}

/// The keys (see [`key`]) of the `paths` where the tree of `files` has a file, or a folder holding one.
fn occupied<'a>(files: &[&str], paths: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let keys: HashSet<String> = paths.map(key).collect();
    let mut occupied = HashSet::new();
    if keys.is_empty() {
        return occupied;
    }
    for file in files {
        let file = key(file);
        for path in parents(&file).chain(std::iter::once(file.as_str())) {
            if keys.contains(path) {
                occupied.insert(path.to_owned());
            }
        }
    }
    occupied
}

/// Removes the link `path` of the copy at `root` (or whatever a test put there), never following it. Returns
/// whether there was something to remove. Nothing is removed through a link: if a folder above it is not a
/// real folder, the link is not there.
fn remove_link(root: &Path, path: &str) -> io::Result<bool> {
    if !parents(path).all(|parent| is_real_dir_no_follow(&native(root, parent))) {
        return Ok(false);
    }
    let full = native(root, path);
    match fs::symlink_metadata(&full) {
        Ok(metadata) => {
            remove_entry(&full, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The site-packages folders of the environment at `env`, reached through real folders only:
/// `lib/<python>/site-packages` (Unix) and `Lib/site-packages` (Windows).
fn site_packages(env: &Path) -> io::Result<Vec<PathBuf>> {
    let mut sites = Vec::new();
    for (name, file_type, lib) in list(env)? {
        let is_lib = name.to_str().is_some_and(|name| {
            if CASE_INSENSITIVE {
                name.eq_ignore_ascii_case("lib")
            } else {
                name == "lib"
            }
        });
        if !is_lib || !file_type.is_dir() {
            continue;
        }
        let direct = lib.join("site-packages");
        if is_real_dir_no_follow(&direct) {
            sites.push(direct);
        }
        for (_, file_type, version) in list(&lib)? {
            let site = version.join("site-packages");
            if file_type.is_dir() && is_real_dir_no_follow(&site) {
                sites.push(site);
            }
        }
    }
    sites.sort_unstable();
    Ok(sites)
}

/// The path lines of a `.pth` file, as Python's `site` module reads them: comments, blank lines and `import`
/// lines (code, which Yalper never runs) are skipped, and trailing whitespace is removed. A line that is not
/// valid UTF-8 is skipped.
fn pth_paths(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    text.lines()
        .filter(|line| {
            !(line.starts_with('#')
                || line.trim().is_empty()
                || line.starts_with("import ")
                || line.starts_with("import\t")
                || line.contains(char::REPLACEMENT_CHARACTER))
        })
        .map(|line| line.trim_end().to_owned())
        .collect()
}

/// The values of the `MAPPING` dictionary of a setuptools editable finder, which setuptools writes with
/// Python's `repr()` on one line: `MAPPING: dict[str, str] = {'pkg': '/p/src/pkg'}`. Empty if there is none;
/// what follows a part that cannot be read is left out.
fn mapping_values(text: &str) -> Vec<String> {
    let Some(start) = text
        .lines()
        .find(|line| line.starts_with("MAPPING"))
        .and_then(|line| Some(&line[line.find('{')? + 1..]))
    else {
        return Vec::new();
    };
    let mut chars = start.chars().peekable();
    let mut values = Vec::new();
    loop {
        skip_spaces(&mut chars);
        if chars.peek() == Some(&'}') || python_str(&mut chars).is_none() {
            return values;
        }
        skip_spaces(&mut chars);
        if chars.next() != Some(':') {
            return values;
        }
        skip_spaces(&mut chars);
        let Some(value) = python_str(&mut chars) else {
            return values;
        };
        values.push(value);
        skip_spaces(&mut chars);
        if chars.next_if_eq(&',').is_none() {
            return values;
        }
    }
}

fn skip_spaces(chars: &mut Peekable<Chars>) {
    while chars.next_if(|c| c.is_whitespace()).is_some() {}
}

/// Reads a Python string literal as `repr()` writes it, quotes included.
fn python_str(chars: &mut Peekable<Chars>) -> Option<String> {
    let quote = chars.next_if(|&c| c == '\'' || c == '"')?;
    let mut value = String::new();
    loop {
        match chars.next()? {
            c if c == quote => return Some(value),
            '\\' => match chars.next()? {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                'x' => value.push(hex_char(chars, 2)?),
                'u' => value.push(hex_char(chars, 4)?),
                'U' => value.push(hex_char(chars, 8)?),
                c @ ('\\' | '\'' | '"') => value.push(c),
                c => {
                    value.push('\\');
                    value.push(c);
                }
            },
            c => value.push(c),
        }
    }
}

/// The character of the next `digits` hexadecimal digits.
fn hex_char(chars: &mut Peekable<Chars>, digits: usize) -> Option<char> {
    let hex: String = chars.by_ref().take(digits).collect();
    if hex.len() != digits {
        return None;
    }
    char::from_u32(u32::from_str_radix(&hex, 16).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use crate::bisect::env_additions;
    use crate::bisect::link::make_link;
    use crate::bisect::testing::{Project, state, tool, venv_python, write};

    /// The site-packages folder of an environment at `env`, as `python -m venv` lays it out on this system.
    fn site(env: &str) -> String {
        if cfg!(windows) {
            format!("{env}/Lib/site-packages")
        } else {
            format!("{env}/lib/python3.12/site-packages")
        }
    }

    /// Writes a fake virtual environment at `env` in the project: `pyvenv.cfg` and one installed package.
    fn fake_env(project: &Project, env: &str) {
        project.write(&format!("{env}/pyvenv.cfg"), "home = /usr/bin\n");
        project.write(&format!("{}/dep/__init__.py", site(env)), "");
    }

    /// A project with a `.venv` that its `.gitignore` ignores.
    fn with_env() -> Project {
        let project = Project::new();
        project.write(".gitignore", ".venv/\nvenv/\n");
        fake_env(&project, ".venv");
        project
    }

    /// The tree of a usual step: the project's `.gitignore`, a manifest and a source file.
    const STEP: [(&str, &str); 3] = [
        (".gitignore", ".venv/\nvenv/\n"),
        ("pyproject.toml", "[project]\nname = \"demo\"\n"),
        ("src/demo/__init__.py", ""),
    ];

    fn envs(project: &Project) -> PythonEnvs {
        PythonEnvs::new(project.path()).unwrap()
    }

    fn assert_links_to(link: &Path, target: &Path) {
        let metadata = fs::symlink_metadata(link).unwrap();
        assert!(metadata.is_symlink(), "{} is not a link", link.display());
        assert_eq!(
            fs::read_link(link).unwrap(),
            std::path::absolute(target).unwrap()
        );
    }

    fn extra(copy: &TestCopy) -> Vec<&str> {
        let mut extra: Vec<&str> = copy.extra_paths().iter().map(String::as_str).collect();
        extra.sort_unstable();
        extra
    }

    #[test]
    fn an_env_next_to_a_manifest_is_linked_next_to_other_extra_paths() {
        let project = with_env();
        let before = state(project.path());
        let mut copy = project.copy();
        copy.set_extra_paths([".env".to_owned()]);
        let tree = project.tree(&STEP);
        copy.switch(&project.store, tree).unwrap();
        let mut envs = envs(&project);
        assert_eq!(
            envs.link(&mut copy).unwrap(),
            LinkedEnvs {
                envs: 1,
                links: 1,
                removed: 0,
            }
        );
        let link = copy.path().join(".venv");
        assert_links_to(&link, &project.path().join(".venv"));
        assert!(link.join("pyvenv.cfg").is_file());
        assert_eq!(extra(&copy), [".env", ".venv"]);

        // Checked again before the next run: nothing to do.
        copy.switch(&project.store, tree).unwrap();
        assert_eq!(
            envs.link(&mut copy).unwrap(),
            LinkedEnvs {
                envs: 1,
                ..LinkedEnvs::default()
            }
        );
        assert_links_to(&link, &project.path().join(".venv"));
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn each_kind_of_manifest_gets_the_envs_next_to_it() {
        let project = Project::new();
        project.write(".gitignore", ".venv/\nvenv/\n");
        for env in [
            ".venv",
            "venv",
            "api/venv",
            "tools/.venv",
            "other/.venv",
            "docs/.venv",
        ] {
            fake_env(&project, env);
        }
        let step = [
            (".gitignore", ".venv/\nvenv/\n"),
            ("pyproject.toml", ""),
            ("api/requirements-dev.txt", ""),
            ("tools/setup.py", ""),
            ("other/README.md", ""),
            ("docs/REQUIREMENTS.TXT", ""),
        ];
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&step)).unwrap();
        let linked = envs(&project).link(&mut copy).unwrap();
        let mut expected = vec![".venv", "api/venv", "tools/.venv", "venv"];
        // The name of a manifest is compared without regard to case where the file system ignores it.
        if CASE_INSENSITIVE {
            expected.push("docs/.venv");
            expected.sort_unstable();
        }
        assert_eq!(linked.envs, expected.len());
        assert_eq!(extra(&copy), expected);
        for env in expected {
            assert_links_to(&native(copy.path(), env), &native(project.path(), env));
        }
        assert!(!copy.path().join("other/.venv").exists());
    }

    #[test]
    fn an_env_ignored_by_its_own_gitignore_is_reused() {
        let project = Project::new();
        fake_env(&project, ".venv");
        // What `python -m venv` (3.13 and later) and uv write.
        project.write(".venv/.gitignore", "*\n");
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&STEP[1..]))
            .unwrap();
        assert_eq!(envs(&project).link(&mut copy).unwrap().envs, 1);
    }

    #[test]
    fn an_env_not_ignored_not_an_env_behind_a_link_or_in_the_tree_is_not_reused() {
        let assert_none = |project: &Project, step: &[(&str, &str)]| {
            let mut copy = project.copy();
            copy.switch(&project.store, project.tree(step)).unwrap();
            assert_eq!(
                envs(project).link(&mut copy).unwrap(),
                LinkedEnvs::default()
            );
            assert!(extra(&copy).is_empty());
            copy
        };

        // Not ignored.
        let project = Project::new();
        fake_env(&project, ".venv");
        assert_none(&project, &STEP[1..]);

        // Not an environment: no `pyvenv.cfg`.
        let project = Project::new();
        project.write(".gitignore", ".venv/\nvenv/\n");
        project.write("venv/lib/x.py", "");
        assert_none(&project, &STEP);

        // The environment is a link, or in a folder that is one.
        let elsewhere = tempfile::tempdir().unwrap();
        write(elsewhere.path(), ".venv/pyvenv.cfg", "");
        write(elsewhere.path(), "pyvenv.cfg", "");
        let project = Project::new();
        project.write(".gitignore", ".venv/\nvenv/\n");
        make_link(elsewhere.path(), &project.path().join(".venv")).unwrap();
        make_link(elsewhere.path(), &project.path().join("sub")).unwrap();
        let mut step = STEP.to_vec();
        step.push(("sub/pyproject.toml", ""));
        let copy = assert_none(&project, &step);
        assert!(fs::symlink_metadata(copy.path().join(".venv")).is_err());

        // The step's tree has files there, whatever the case of the folder's name.
        for folder in [".venv", ".VENV"] {
            let project = with_env();
            let local = format!("{folder}/local.py");
            let mut step = STEP.to_vec();
            step.push((&local, "local"));
            let copy = assert_none(&project, &step);
            assert!(
                fs::symlink_metadata(copy.path().join(&local))
                    .unwrap()
                    .is_file()
            );
        }
    }

    #[test]
    fn what_a_test_put_in_place_of_a_link_is_replaced_never_followed() {
        let project = with_env();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep.txt", "keep me");
        let canary_before = state(canary.path());
        let before = state(project.path());
        let mut copy = project.copy();
        let mut envs = envs(&project);
        let tree = project.tree(&STEP);
        copy.switch(&project.store, tree).unwrap();
        envs.link(&mut copy).unwrap();

        let link = copy.path().join(".venv");
        let replacements: [&dyn Fn(); 3] = [
            &|| make_link(canary.path(), &link).unwrap(),
            &|| fs::write(&link, "a file").unwrap(),
            &|| write(&link, "pyvenv.cfg", "a folder"),
        ];
        for replace in replacements {
            remove_entry(&link, &fs::symlink_metadata(&link).unwrap()).unwrap();
            replace();
            copy.switch(&project.store, tree).unwrap();
            assert_eq!(
                envs.link(&mut copy).unwrap(),
                LinkedEnvs {
                    envs: 1,
                    links: 1,
                    removed: 0,
                }
            );
            assert_links_to(&link, &project.path().join(".venv"));
        }
        assert_eq!(state(canary.path()), canary_before);
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn a_link_goes_when_the_step_has_no_manifest_or_has_files_there() {
        let project = with_env();
        let before = state(project.path());
        let mut copy = project.copy();
        copy.set_extra_paths([".env".to_owned()]);
        let mut envs = envs(&project);
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        envs.link(&mut copy).unwrap();
        let link = copy.path().join(".venv");

        // No manifest: the link is removed and no longer an extra path.
        copy.switch(&project.store, project.tree(&STEP[2..]))
            .unwrap();
        assert_eq!(
            envs.link(&mut copy).unwrap(),
            LinkedEnvs {
                removed: 1,
                ..LinkedEnvs::default()
            }
        );
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(extra(&copy), [".env"]);

        // Back, then a step with files there: the switch replaces the link with the tree's folder.
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        assert_eq!(envs.link(&mut copy).unwrap().links, 1);
        let mut step = STEP.to_vec();
        step.push((".venv/local.py", "local"));
        copy.switch(&project.store, project.tree(&step)).unwrap();
        assert_eq!(envs.link(&mut copy).unwrap(), LinkedEnvs::default());
        assert!(fs::symlink_metadata(&link).unwrap().is_dir());
        assert_eq!(fs::read(link.join("local.py")).unwrap(), b"local");
        assert_eq!(extra(&copy), [".env"]);
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn a_link_left_by_an_earlier_bisect_is_removed_by_the_first_switch() {
        let project = with_env();
        let before = state(project.path());
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        envs(&project).link(&mut copy).unwrap();
        drop(copy);

        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&STEP[2..]))
            .unwrap();
        assert!(fs::symlink_metadata(copy.path().join(".venv")).is_err());
        assert_eq!(
            envs(&project).link(&mut copy).unwrap(),
            LinkedEnvs::default()
        );
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn a_project_on_a_network_drive_fails_with_a_kind_the_caller_can_fall_back_on() {
        let project = with_env();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        let mut envs = envs(&project);
        envs.on_network = true;
        assert_eq!(envs.link(&mut copy).unwrap_err().kind(), CANNOT_LINK);
        assert!(fs::symlink_metadata(copy.path().join(".venv")).is_err());
        assert!(extra(&copy).is_empty());

        // Nothing to link: no error.
        copy.switch(&project.store, project.tree(&STEP[2..]))
            .unwrap();
        assert_eq!(envs.link(&mut copy).unwrap(), LinkedEnvs::default());
    }

    #[test]
    fn deleting_the_copy_keeps_the_projects_env() {
        let project = with_env();
        let before = state(project.path());
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        envs(&project).link(&mut copy).unwrap();
        let path = copy.path().to_owned();
        assert!(path.join(".venv/pyvenv.cfg").exists());
        // The copy holds its folder open on Windows.
        drop(copy);
        fs::remove_dir_all(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(state(project.path()), before);
    }

    /// `text` as Python's `repr()` writes a string.
    fn repr(text: &str) -> String {
        format!("'{}'", text.replace('\\', r"\\").replace('\'', r"\'"))
    }

    #[test]
    fn editable_folders_inside_the_project_are_mapped_into_the_copy() {
        let project = with_env();
        let root = project.path().to_owned();
        let real = plain(&fs::canonicalize(&root).unwrap());
        let site = site(".venv");
        let up = "../".repeat(site.matches('/').count() + 1);
        let outside = tempfile::tempdir().unwrap();
        let p = |path: &str| root.join(path).display().to_string();
        let pth = [
            (
                "a.pth",
                format!(
                    "# a comment\n\nimport sys; sys.path.insert(0, 'x')\n{}\n{}   \n{up}rel\n{}\n{}\n",
                    p("src"),
                    p("src/../lib2"),
                    p(&format!("{site}/inside")),
                    outside.path().join("elsewhere").display(),
                ),
            ),
            ("b.pth", format!("{}\r\n", root.display())),
            (".hidden.pth", format!("{}\n", p("hidden"))),
            (
                "c.pth",
                format!("{}\n{}", p("big"), "#".repeat(MAX_READ_BYTES as usize)),
            ),
            ("e.pth", format!("{}\n", real.join("canon").display())),
        ];
        for (name, content) in &pth {
            project.write(&format!("{site}/{name}"), content);
        }
        let mapping = format!(
            "MAPPING: dict[str, str] = {{{}: {}, {}: {}}}\nNAMESPACES: dict[str, list[str]] = {{}}\n",
            repr("tool"),
            repr(&p("tools/tool")),
            repr("mod"),
            repr(&p("single/mod.py")),
        );
        project.write(
            &format!("{site}/__editable___tool_0_1_0_finder.py"),
            &mapping,
        );
        // A `.pth` file that is a link is not read.
        #[cfg(unix)]
        {
            write(outside.path(), "d.pth", &format!("{}\n", p("linked")));
            std::os::unix::fs::symlink(
                outside.path().join("d.pth"),
                root.join(&site).join("d.pth"),
            )
            .unwrap();
        }

        let mut copy = project.copy();
        let mut envs = envs(&project);
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        envs.link(&mut copy).unwrap();
        let copy_root = plain(&std::path::absolute(copy.path()).unwrap());
        let expected: Vec<PathBuf> = ["src", "lib2", "rel", "", "canon", "tools", "single"]
            .into_iter()
            .map(|path| {
                if path.is_empty() {
                    copy_root.clone()
                } else {
                    copy_root.join(path)
                }
            })
            .collect();
        assert_eq!(envs.python_path(&copy).unwrap(), expected);
        assert_eq!(
            env_additions(&copy, Some(&envs)).unwrap().python_path,
            expected
        );

        // Without the link, no folders.
        copy.switch(&project.store, project.tree(&STEP[2..]))
            .unwrap();
        envs.link(&mut copy).unwrap();
        assert!(envs.python_path(&copy).unwrap().is_empty());
    }

    #[test]
    fn a_huge_pth_file_is_read_up_to_the_limits() {
        let project = with_env();
        let site = site(".venv");
        // A `.pth` file of 1 MiB with tens of thousands of different folders inside the project.
        let envs_of = |project: &Project| PythonEnvs::new(project.path()).unwrap();
        let mut huge = String::new();
        let mut index = 0;
        while huge.len() < MAX_READ_BYTES as usize - 100 {
            huge.push_str(&format!(
                "{}\n",
                project.path().join(format!("r{index}")).display()
            ));
            index += 1;
        }
        assert!(index > 10 * MAX_ROOTS);
        project.write(&format!("{site}/huge.pth"), &huge);
        let started = std::time::Instant::now();
        let mut copy = project.copy();
        let mut envs = envs(&project);
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        envs.link(&mut copy).unwrap();
        let paths = envs.python_path(&copy).unwrap();
        assert_eq!(paths.len(), MAX_ROOTS);
        assert_eq!(paths[0], copy.path().join("r0"));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));

        // Past the number of files or the bytes read, nothing more is read: `zz.pth` is left out.
        let many_files = with_env();
        for file in 0..MAX_FILES_READ {
            many_files.write(&format!("{site}/a{file:05}.pth"), "# nothing\n");
        }
        let many_bytes = with_env();
        let comment = format!("#{}\n", "x".repeat(MAX_READ_BYTES as usize - 2));
        for file in 0..MAX_ENV_BYTES / MAX_READ_BYTES {
            many_bytes.write(&format!("{site}/a{file}.pth"), &comment);
        }
        for project in [&many_files, &many_bytes] {
            project.write(
                &format!("{site}/zz.pth"),
                &format!("{}\n", project.path().join("late").display()),
            );
            assert!(envs_of(project).editable_roots(".venv").unwrap().is_empty());
        }
    }

    #[test]
    fn a_folder_that_would_not_be_one_pythonpath_entry_is_left_out() {
        let project = with_env();
        let site = site(".venv");
        let separator = if cfg!(windows) { ";" } else { ":" };
        let p = |name: &str| project.path().join(name).display().to_string();
        let pth = [
            p(&format!("a{separator}b")),
            p("quote\"d"),
            p("tab\there"),
            p("kept"),
        ]
        .join("\n");
        project.write(&format!("{site}/a.pth"), &pth);
        let mut copy = project.copy();
        let mut envs = envs(&project);
        copy.switch(&project.store, project.tree(&STEP)).unwrap();
        envs.link(&mut copy).unwrap();
        let root = plain(&std::path::absolute(copy.path()).unwrap());
        assert_eq!(envs.python_path(&copy).unwrap(), [root.join("kept")]);
        assert!(!is_plain_entry(Path::new("nul\0byte")));
        assert!(is_plain_entry(Path::new("src")));
    }

    #[test]
    fn pth_paths_are_read_as_python_reads_them() {
        let text =
            "\u{feff}/p/a\r\n#/p/comment\nimport os\nimport\tos\n   \n /p/space \t\nimports/x\n";
        assert_eq!(
            pth_paths(text.as_bytes()),
            ["/p/a", " /p/space", "imports/x"]
        );
        assert_eq!(pth_paths(b"/p/\xff\n/p/b\n"), ["/p/b"]);
    }

    #[test]
    fn mapping_values_are_read_from_a_python_repr() {
        let none = Vec::<String>::new();
        assert_eq!(
            mapping_values(
                "import sys\nMAPPING: dict[str, str] = {'a': '/p/a', \"b'q\": 'C:\\\\p\\\\b', \
                 'c': '\\u00e9t\\xe9\\U0001f600'}\n"
            ),
            ["/p/a", r"C:\p\b", "\u{e9}t\u{e9}\u{1f600}"]
        );
        assert_eq!(mapping_values("MAPPING = {}\n"), none);
        assert_eq!(mapping_values("NAMESPACES = {'a': ['/p/a']}\n"), none);
        // What follows a part that cannot be read is left out.
        assert_eq!(
            mapping_values("MAPPING = {'a': '/p/a', 'b' '/p/b'}"),
            ["/p/a"]
        );
        assert_eq!(mapping_values("MAPPING = {'a': '/p/a"), none);
        assert_eq!(mapping_values("MAPPING = {'a': '\\x4'}"), none);
    }

    /// Runs `command` in the test copy with the environment additions, and returns what it printed.
    fn run_in_copy(mut command: Command, copy: &TestCopy, envs: &PythonEnvs) -> String {
        let additions = env_additions(copy, Some(envs)).unwrap();
        let python_path = std::env::join_paths(&additions.python_path).unwrap();
        let output = command
            .current_dir(copy.path())
            .env("PYTHONPATH", python_path)
            .envs(additions.vars.iter().map(|(name, value)| (name, value)))
            .env_remove("PYTHONDONTWRITEBYTECODE")
            .env_remove("PYTHONHOME")
            .env_remove("VIRTUAL_ENV")
            .env_remove("UV_PROJECT_ENVIRONMENT")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs `program` with `args` in `dir`, checks that it succeeded, and returns what it printed.
    fn run(program: impl AsRef<std::ffi::OsStr>, args: &[&str], dir: &Path) -> String {
        let output = Command::new(program)
            .args(args)
            .current_dir(dir)
            .env_remove("VIRTUAL_ENV")
            .env_remove("UV_NO_SYNC")
            .env_remove("UV_PROJECT_ENVIRONMENT")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// Whether some file under `dir` (links not followed) has a name ending with `suffix`.
    fn has_file_ending_with(dir: &Path, suffix: &str) -> bool {
        dir.exists()
            && state(dir).keys().any(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
            })
    }

    #[test]
    fn python_imports_the_steps_editable_packages_and_writes_nothing_into_the_projects_env() {
        let Some(python) = tool(&["python3", "python"]) else {
            return;
        };
        let setuptools = "[build-system]\nrequires = [\"setuptools>=64\"]\n\
                          build-backend = \"setuptools.build_meta\"\n";
        let pkg = format!("{setuptools}[project]\nname = \"pkg\"\nversion = \"0.1.0\"\n");
        let toolpkg = format!("{setuptools}[project]\nname = \"toolpkg\"\nversion = \"0.1.0\"\n");
        let files = |value: &str| {
            [
                (".gitignore", ".venv/\n*.egg-info/\nbuild/\n".to_owned()),
                ("pyproject.toml", pkg.clone()),
                ("src/pkg/__init__.py", format!("VALUE = '{value}'\n")),
                ("tools/pyproject.toml", toolpkg.clone()),
                ("tools/toolpkg/__init__.py", format!("VALUE = '{value}'\n")),
            ]
        };
        let project = Project::new();
        for (path, content) in files("project") {
            project.write(path, &content);
        }
        // A src layout (a `.pth` file) and a flat layout (a setuptools finder), installed editable.
        run(python, &["-m", "venv", ".venv"], project.path());
        let project_python = venv_python(&project.path().join(".venv"));
        run(
            &project_python,
            &[
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-input",
                "--quiet",
                "-e",
                ".",
                "-e",
                "tools",
            ],
            project.path(),
        );
        // A canary module in the project's environment, never imported yet: Python would write its bytecode
        // next to it.
        let purelib = run(
            &project_python,
            &[
                "-c",
                "import sysconfig; print(sysconfig.get_path('purelib'))",
            ],
            project.path(),
        );
        let purelib = PathBuf::from(purelib.trim());
        fs::write(purelib.join("yalper_canary.py"), "VALUE = 'canary'\n").unwrap();
        let before = state(project.path());

        let step = files("step");
        let step: Vec<(&str, &str)> = step
            .iter()
            .map(|(path, content)| (*path, content.as_str()))
            .collect();
        let mut copy = project.copy();
        let mut envs = envs(&project);
        copy.switch(&project.store, project.tree(&step)).unwrap();
        envs.link(&mut copy).unwrap();
        let mut command = Command::new(venv_python(&copy.path().join(".venv")));
        command.args([
            "-c",
            "import pkg, toolpkg, yalper_canary, sys; \
             sys.stdout.write(' '.join([pkg.VALUE, toolpkg.VALUE, yalper_canary.VALUE]))",
        ]);
        assert_eq!(run_in_copy(command, &copy, &envs), "step step canary");
        // The bytecode went to the copy's prefix folder.
        assert!(has_file_ending_with(copy.pycache_path(), ".pyc"));
        assert_eq!(state(project.path()), before);

        // The canary works: without the prefix folder, Python writes into the project's environment.
        let output = Command::new(&project_python)
            .args(["-c", "import yalper_canary"])
            .env_remove("PYTHONDONTWRITEBYTECODE")
            .env_remove("PYTHONPYCACHEPREFIX")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(has_file_ending_with(&purelib.join("__pycache__"), ".pyc"));
    }

    #[test]
    fn uv_run_imports_the_steps_code_and_never_syncs_the_projects_env() {
        let Some(uv) = tool(&["uv"]) else {
            return;
        };
        let manifest = "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.8\"\n\n\
                        [build-system]\nrequires = [\"uv_build>=0.8,<1\"]\nbuild-backend = \"uv_build\"\n";
        let project = Project::new();
        project.write(".gitignore", ".venv/\n");
        project.write("pyproject.toml", manifest);
        project.write("src/demo/__init__.py", "VALUE = 'project'\n");
        // uv installs the project editable.
        run(uv, &["sync", "--quiet"], project.path());
        let lock = fs::read_to_string(project.path().join("uv.lock")).unwrap();
        let before = state(project.path());

        let step = [
            (".gitignore", ".venv/\n"),
            ("pyproject.toml", manifest),
            ("uv.lock", lock.as_str()),
            ("src/demo/__init__.py", "VALUE = 'step'\n"),
        ];
        let mut copy = project.copy();
        let mut envs = envs(&project);
        copy.switch(&project.store, project.tree(&step)).unwrap();
        envs.link(&mut copy).unwrap();
        let mut command = Command::new(uv);
        command.args([
            "run",
            "--quiet",
            "python",
            "-c",
            "import demo, sys; sys.stdout.write(demo.VALUE)",
        ]);
        assert_eq!(run_in_copy(command, &copy, &envs), "step");
        assert_eq!(state(project.path()), before);
    }
}
