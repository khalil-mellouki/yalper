//! Dependency sets: which test copy runs the tests of a step, and the installs that fill the copies.
//!
//! What a step needs installed is given by its dependency files, in any folder of its tree: every lockfile,
//! `requirements*.txt`, the package manager settings of [`SETTINGS_FILES`], and only the dependency sections of
//! `package.json` and `pyproject.toml` (see [`PACKAGE_JSON_SECTIONS`] and [`PYPROJECT_SECTIONS`]), so that an
//! agent editing a script or a tool setting does not cost an install. Files inside a `node_modules` folder do
//! not count: packages committed with the code are code. Steps whose dependency files are the same have the
//! same key and form one dependency set.
//!
//! - When every dependency file of the step is the same in the project (read from its working tree: regular
//!   files reached through real folders only), the step runs in `copy-0`, which links to the project's
//!   installed dependencies (see [`NodeModules`] and [`PythonEnvs`]): no install. If the step has Node or Python
//!   lockfiles, the project must have at least one installed folder next to one of them that those links can
//!   reuse; lockfiles of documentation, examples or fixtures that the project never installed do not count.
//!   Where the links cannot be made ([`CANNOT_LINK`]), `copy-0` is not used for the rest of the bisect. The
//!   links are removed when the bisect ends (see [`DependencySets::remove_project_links`]), and by a second
//!   Ctrl-C, so that a tool that follows links cannot reach the project's packages through the cache folder.
//! - Otherwise the step runs in the copy of its set, one of `copy-1` to `copy-3`, filled with the step's code and
//!   installed once: with the `--setup` command at the top of the copy, or with the command of each lockfile
//!   (see [`Tool`]) in its folder. In each ecosystem (Node, Python, Rust), only the lockfiles next to which the
//!   project has an installed folder of that ecosystem (`node_modules`, `.venv` or `venv`, cargo's `target`) are
//!   installed, or all of them when it has none. `cargo fetch` always runs for a `Cargo.lock` at the top of the
//!   step: it costs nothing when the crates are already downloaded, and offline a crate that cannot be
//!   downloaded then makes the set "cannot test" instead of failing the tests. The copy is kept for the next steps and bisects
//!   of the set. When a set needs a copy and all three hold an install, the least recently used one is deleted,
//!   with its Cargo target and Python bytecode folders. Which copy holds which set is recorded in the event log
//!   (see [`Book`]).
//! - An install that fails makes every step of its set "cannot test" for the rest of the bisect. So does a link
//!   (or a file) in the step's code where an install writes, such as a `.venv` that links to an environment
//!   elsewhere on the machine: the install would write through it, so it is not run. Each command checks the
//!   folders it writes in (a `--setup` command, all of them). On Unix, no install runs at all while the step's
//!   code holds a link that may point outside the copy (a workspace package, a `.yarn` folder or `src` linked
//!   elsewhere): package managers write through such links. Targets are judged by their text only (see
//!   [`leaves_tree`]): absolute, climbing above the top of the copy, or going through another link of the step
//!   (chained links), which counts as outside.
//!
//! Installs get the protections of test runs: the root of the project's cache folder is cleaned and its
//! boundary files written before each one, and they run with the environment of [`run_env`], through [`run`],
//! with no time limit and their output in a log. They run the step's package scripts (`postinstall`, build
//! backends): a bisect runs the code of every step anyway.
//!
//! Known limits:
//!
//! - Installs run in the copy, outside the project, so configuration the project ignores (an ignored `.npmrc`
//!   with a private registry, an ignored `.cargo/config.toml`) does not apply to them unless the user brings it
//!   in with `--with` (see [`LocalFiles`]); when an install fails with what looks like a registry or login
//!   error, [`InstallFailure`] suggests it.
//! - Packages that take their version from git cannot find a repository in a copy (it is outside the project,
//!   and runs and installs never search above it). An install that builds a Python project versioned by
//!   setuptools-scm or hatch-vcs (`uv sync`, `poetry install`) fails with "LookupError: setuptools-scm was
//!   unable to detect version", so the steps of that set cannot be tested, unless the user's environment sets
//!   `SETUPTOOLS_SCM_PRETEND_VERSION`. Steps that run in `copy-0` are not affected: the project's environment is
//!   already built. A Rust build script that reads git (vergen) runs in the test runs of every copy: depending
//!   on its settings it fails every step, so the bisect reports that the good end fails, or it builds with
//!   placeholder values.
//! - A step that lists dependencies in a manifest without a lockfile installs nothing, unless `--setup` is given.
//! - Not counted in the key: lockfiles of other ecosystems (`Gemfile.lock`, `composer.lock`, `go.sum`), patch
//!   folders (`patches/` of patch-package or pnpm) and Yarn's `.yarn/releases`.
//! - What an install puts in a copy is kept only in the folders it fills (`node_modules` next to each lockfile
//!   or `package.json`, `.venv` and `venv` next to each Python lockfile or manifest) and in ignored files; Yarn
//!   Plug'n'Play files (`.pnp.cjs`, `.yarn/`) only when they are ignored. A test that deletes or changes those
//!   folders in a copy with its own install changes them for the next runs in that copy.

use std::collections::btree_map;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, PoisonError};

use gix::ObjectId;
use serde_json::{Value, json};

use super::copy::{CARGO_TARGET_DIR, PYCACHE_DIR, join, native, parents, remove_entry};
use super::env::same_name;
use super::link::{CANNOT_LINK, is_real_dir_no_follow, plain};
use super::local_files::remove_copied;
use super::run::{Logs, Outcome, SKIP_EXIT_CODE, run};
use super::{
    CacheDir, LocalFiles, NodeModules, PythonEnvs, TestCopy, clean_root, env_additions, run_env,
    write_boundaries,
};
use crate::redact::redact_str;
use crate::snapshot::{Error, FileKind, MAX_FILE_BYTES, Result, ShadowStore, validate_path};
use crate::store::Store;

/// The `meta` key of the event log that records the copies with their own install: see `Book`.
pub const COPIES_KEY: &str = "bisect.copies";

/// The most copies with their own install, `copy-1` to `copy-3`, kept in the project's cache folder.
const MAX_OWN_COPIES: usize = 3;

/// The most entries of a tree listed to find its dependency files, as for a switch of a test copy.
const MAX_TREE_ENTRIES: usize = 1_000_000;

/// The most dependency files a step may have: far more than a large monorepo has, while a crafted tree with
/// millions of manifests is refused instead of read.
const MAX_DEPENDENCY_FILES: usize = 10_000;

/// The links into the project that `copy-0` holds (its `node_modules` farms and its links to Python
/// environments), as the canonical path of the copy and the path in it, for [`remove_all_project_links`]. Held
/// while they are made or removed, so that [`remove_all_project_links`] waits for that and then removes them.
static PROJECT_LINKS: Mutex<BTreeSet<(PathBuf, String)>> = Mutex::new(BTreeSet::new());

/// How much of the end of a failed install's log is searched for a registry or login error.
const LOG_TAIL_BYTES: u64 = 64 * 1024;

/// The lockfiles: each counts as a whole.
const LOCKFILES: [&str; 9] = [
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "uv.lock",
    "poetry.lock",
    "Cargo.lock",
];

/// Package manager settings that change what gets installed: each counts as a whole.
const SETTINGS_FILES: [&str; 8] = [
    "pnpm-workspace.yaml",
    ".pnpmfile.cjs",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    "bunfig.toml",
    "uv.toml",
    ".python-version",
];

/// The fields of `package.json` that count. npm accepts both spellings of bundled dependencies; `pnpm` holds
/// pnpm's overrides and patches.
const PACKAGE_JSON_SECTIONS: [&str; 11] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
    "bundleDependencies",
    "bundledDependencies",
    "overrides",
    "resolutions",
    "workspaces",
    "packageManager",
    "pnpm",
];

/// The tables and keys of `pyproject.toml` that count, by their path.
const PYPROJECT_SECTIONS: [&[&str]; 10] = [
    &["build-system"],
    &["project", "dependencies"],
    &["project", "optional-dependencies"],
    &["dependency-groups"],
    &["tool", "poetry", "dependencies"],
    &["tool", "poetry", "dev-dependencies"],
    &["tool", "poetry", "group"],
    &["tool", "poetry", "extras"],
    &["tool", "poetry", "source"],
    &["tool", "uv"],
];

/// The folders Node package managers install into, next to the lockfile or `package.json`.
const NODE_FOLDERS: [&str; 1] = ["node_modules"];

/// The folders Python environments are kept in, next to the lockfile or manifest. Installs make the first.
const PYTHON_FOLDERS: [&str; 2] = [".venv", "venv"];

/// Variables removed from the environment of the installs and runs of a copy with its own install, so that the
/// install fills the copy's own environment, never one the user activated: Poetry installs into an activated
/// virtual or Conda environment, and `uv sync` into `UV_PROJECT_ENVIRONMENT`.
const OWN_INSTALL_REMOVED: [&str; 3] = ["VIRTUAL_ENV", "CONDA_PREFIX", "UV_PROJECT_ENVIRONMENT"];

/// Variables set for them: Poetry makes its environment in the copy's `.venv`, whatever the user's settings say.
const OWN_INSTALL_SET: [(&str, &str); 2] = [
    ("POETRY_VIRTUALENVS_CREATE", "true"),
    ("POETRY_VIRTUALENVS_IN_PROJECT", "true"),
];

/// Texts of a failed install's output that suggest a registry or login problem, compared in lowercase.
const REGISTRY_ERROR_TEXTS: [&str; 7] = [
    "unauthorized",
    "forbidden",
    "authenticat",
    "credential",
    "no token found",
    "registry",
    "could not find a version that satisfies",
];

/// Words of a failed install's output that suggest a registry or login problem: HTTP statuses and npm codes.
const REGISTRY_ERROR_WORDS: [&str; 6] = ["401", "403", "e401", "e403", "e404", "eneedauth"];

/// A package manager whose lockfile a folder of a step holds, with the command that installs exactly what the
/// lockfile says (research section 5 of the bisect milestone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    /// `package-lock.json` or `npm-shrinkwrap.json`.
    Npm,
    /// `pnpm-lock.yaml`.
    Pnpm,
    /// `yarn.lock` without `.yarnrc.yml`: Yarn 1.
    Yarn,
    /// `yarn.lock` with `.yarnrc.yml`: Yarn 2 and later.
    YarnBerry,
    /// `bun.lock` or `bun.lockb`.
    Bun,
    /// `uv.lock`.
    Uv,
    /// `poetry.lock`.
    Poetry,
    /// `requirements.txt` without `uv.lock` or `poetry.lock`.
    Pip,
    /// `Cargo.lock`.
    Cargo,
}

impl Tool {
    /// The command that installs from the lockfile, run in its folder.
    fn command(self) -> &'static str {
        match self {
            Self::Npm => "npm ci",
            Self::Pnpm => "pnpm install --frozen-lockfile",
            Self::Yarn => "yarn install --frozen-lockfile",
            Self::YarnBerry => "yarn install --immutable",
            Self::Bun => "bun install --frozen-lockfile",
            Self::Uv => "uv sync --frozen",
            Self::Poetry => "poetry install --no-interaction",
            Self::Pip if cfg!(windows) => {
                "python -m venv .venv && .venv\\Scripts\\python.exe -m pip install -r requirements.txt"
            }
            Self::Pip => {
                "python3 -m venv .venv && .venv/bin/python -m pip install -r requirements.txt"
            }
            // Building happens in the test runs, in the copy's own Cargo target folder.
            Self::Cargo => "cargo fetch",
        }
    }

    /// The folders, next to the lockfile, that the command installs into.
    fn folders(self) -> &'static [&'static str] {
        match self {
            Self::Npm | Self::Pnpm | Self::Yarn | Self::YarnBerry | Self::Bun => &NODE_FOLDERS,
            Self::Uv | Self::Poetry | Self::Pip => &PYTHON_FOLDERS,
            Self::Cargo => &[],
        }
    }

    fn ecosystem(self) -> Ecosystem {
        match self {
            Self::Npm | Self::Pnpm | Self::Yarn | Self::YarnBerry | Self::Bun => Ecosystem::Node,
            Self::Uv | Self::Poetry | Self::Pip => Ecosystem::Python,
            Self::Cargo => Ecosystem::Rust,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ecosystem {
    Node,
    Python,
    Rust,
}

/// One command of the install of a copy: `command` run in the folder `dir` of the copy, which writes in
/// `folders` (relative, `/`-separated).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Planned {
    dir: String,
    command: String,
    folders: Vec<String>,
}

/// One install a step needs: `tool` run in the folder `dir` of the copy (relative, `/`-separated, `""` for its
/// top).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Install {
    dir: String,
    tool: Tool,
}

/// The installs needed by a step whose dependency files are `files` (paths), parent folders first: in each
/// folder at most one Node package manager, one Python one and cargo, chosen by lockfile in the order of
/// [`Tool`].
fn installs<'a>(files: impl IntoIterator<Item = &'a str>) -> Vec<Install> {
    let mut by_dir: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for path in files {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        by_dir.entry(dir).or_default().insert(name);
    }
    let mut installs = Vec::new();
    for (dir, names) in by_dir {
        let has = |name: &str| names.contains(name);
        let node = if has("package-lock.json") || has("npm-shrinkwrap.json") {
            Some(Tool::Npm)
        } else if has("pnpm-lock.yaml") {
            Some(Tool::Pnpm)
        } else if has("yarn.lock") {
            Some(if has(".yarnrc.yml") {
                Tool::YarnBerry
            } else {
                Tool::Yarn
            })
        } else if has("bun.lock") || has("bun.lockb") {
            Some(Tool::Bun)
        } else {
            None
        };
        let python = if has("uv.lock") {
            Some(Tool::Uv)
        } else if has("poetry.lock") {
            Some(Tool::Poetry)
        } else if has("requirements.txt") {
            Some(Tool::Pip)
        } else {
            None
        };
        let cargo = has("Cargo.lock").then_some(Tool::Cargo);
        installs.extend(
            [node, python, cargo]
                .into_iter()
                .flatten()
                .map(|tool| Install {
                    dir: dir.to_owned(),
                    tool,
                }),
        );
    }
    installs
}

/// The folders an install may fill in a step whose files include `files` (paths): the folders of each install
/// of `installs`, `node_modules` next to each `package.json`, and `.venv` and `venv` next to each Python manifest
/// (`pyproject.toml`, `setup.py`, `requirements*.txt`), where a `--setup` command would install. Sorted.
fn install_folders<'a>(
    files: impl IntoIterator<Item = &'a str>,
    installs: &[Install],
) -> Vec<String> {
    let mut folders = BTreeSet::new();
    for install in installs {
        for folder in install.tool.folders() {
            folders.insert(join(&install.dir, folder));
        }
    }
    for path in files {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let names: &[&str] = if name == "package.json" {
            &NODE_FOLDERS
        } else if name == "pyproject.toml"
            || name == "setup.py"
            || (name.starts_with("requirements") && name.ends_with(".txt"))
        {
            &PYTHON_FOLDERS
        } else {
            &[]
        };
        folders.extend(names.iter().map(|folder| join(dir, folder)));
    }
    folders.into_iter().collect()
}

/// How a dependency file counts in the key of its step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Counted {
    Whole,
    PackageJson,
    Pyproject,
}

/// How the file named `name` counts, or `None` if it is not a dependency file.
fn counted(name: &str) -> Option<Counted> {
    match name {
        "package.json" => Some(Counted::PackageJson),
        "pyproject.toml" => Some(Counted::Pyproject),
        _ if LOCKFILES.contains(&name)
            || SETTINGS_FILES.contains(&name)
            || (name.starts_with("requirements") && name.ends_with(".txt")) =>
        {
            Some(Counted::Whole)
        }
        _ => None,
    }
}

/// The SHA-1 of `parts`, in hex.
fn sha1(parts: &[&[u8]]) -> io::Result<String> {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    for part in parts {
        hasher.update(part);
    }
    hasher
        .try_finalize()
        .map(|digest| digest.to_string())
        .map_err(|error| io::Error::other(format!("cannot hash the dependency files: {error}")))
}

/// The digest of a dependency file holding `bytes`: of its dependency sections when it is a manifest that
/// parses, of the whole file otherwise.
fn digest(counted: Counted, bytes: &[u8]) -> io::Result<String> {
    let sections = match counted {
        Counted::Whole => None,
        Counted::PackageJson => package_json_sections(bytes),
        Counted::Pyproject => pyproject_sections(bytes),
    };
    match sections {
        Some(sections) => sha1(&[b"sections\0", sections.as_bytes()]),
        None => sha1(&[b"file\0", bytes]),
    }
}

/// The dependency sections of a `package.json` holding `bytes`, one per line, or `None` if it is not a JSON
/// object.
fn package_json_sections(bytes: &[u8]) -> Option<String> {
    let Value::Object(fields) = serde_json::from_slice(bytes).ok()? else {
        return None;
    };
    let mut sections = String::new();
    for name in PACKAGE_JSON_SECTIONS {
        if let Some(value) = fields.get(name) {
            sections.push_str(name);
            sections.push('=');
            canonical(value, &mut sections);
            sections.push('\n');
        }
    }
    Some(sections)
}

/// The dependency sections of a `pyproject.toml` holding `bytes`, one per line, or `None` if it is not valid
/// TOML.
fn pyproject_sections(bytes: &[u8]) -> Option<String> {
    let table: toml::Table = std::str::from_utf8(bytes).ok()?.parse().ok()?;
    let mut sections = String::new();
    for path in PYPROJECT_SECTIONS {
        let mut value = table.get(path[0]);
        for key in &path[1..] {
            value = value.and_then(|value| value.get(key));
        }
        if let Some(value) = value {
            sections.push_str(&path.join("."));
            sections.push('=');
            canonical(&serde_json::to_value(value).ok()?, &mut sections);
            sections.push('\n');
        }
    }
    Some(sections)
}

/// Writes `value` as JSON with the keys of every object sorted, so that the order of a table's entries does not
/// count (the order of an array's items does). The parsers limit how deeply values nest (128 levels for JSON,
/// 80 for TOML).
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(fields) => {
            let mut fields: Vec<_> = fields.iter().collect();
            fields.sort_unstable_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (name, value)) in fields.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(name.as_str()).to_string());
                out.push(':');
                canonical(value, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// What the tree of a step says about its dependencies.
#[derive(Debug)]
struct Deps {
    /// A hash over the paths and digests of `files`: the steps of one dependency set have the same key.
    key: String,
    /// The dependency files, path and digest, sorted by path.
    files: Vec<(String, String)>,
    installs: Vec<Install>,
    /// See [`install_folders`].
    folders: Vec<String>,
    /// The first link of the tree whose target is absolute or climbs above the top of the tree (see
    /// [`leaves_tree`]).
    link_out: Option<String>,
}

/// Reads the dependency files of `tree`. `digests` holds the digest of each blob already read, by blob and the
/// way it counts: a lockfile that does not change between steps is read once.
fn read_deps(
    store: &ShadowStore,
    tree: ObjectId,
    digests: &mut HashMap<(ObjectId, Counted), String>,
) -> Result<Deps> {
    let listing = store.file_changes(store.empty_tree(), tree, MAX_TREE_ENTRIES)?;
    if !listing.complete {
        return Err(io::Error::other(format!(
            "cannot read the dependency files of a step: it has more than {MAX_TREE_ENTRIES} files and \
             folders"
        ))
        .into());
    }
    let mut files = Vec::new();
    let mut setup_scripts = Vec::new();
    let mut links = Vec::new();
    for change in &listing.files {
        let Some(file) = change.new else { continue };
        // A path that is not UTF-8 is refused by the switch of the copy.
        let Ok(path) = std::str::from_utf8(&change.path) else {
            continue;
        };
        if file.kind == FileKind::Symlink {
            links.push((path, file.blob));
            continue;
        }
        if path.split('/').any(|part| part == "node_modules") {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if name == "setup.py" {
            setup_scripts.push(path);
        }
        let Some(counted) = counted(name) else {
            continue;
        };
        // The path is also read in the project: never outside it.
        validate_path(path, file.kind)?;
        if files.len() == MAX_DEPENDENCY_FILES {
            return Err(io::Error::other(format!(
                "cannot read the dependency files of a step: it has more than {MAX_DEPENDENCY_FILES}"
            ))
            .into());
        }
        let digest = match digests.get(&(file.blob, counted)) {
            Some(digest) => digest.clone(),
            None => {
                let bytes = store
                    .read_blob(file.blob)?
                    .ok_or(Error::MissingObject(file.blob))?;
                let digest = digest(counted, &bytes)?;
                digests.insert((file.blob, counted), digest.clone());
                digest
            }
        };
        files.push((path.to_owned(), digest));
    }
    let mut parts: Vec<&[u8]> = Vec::new();
    for (path, digest) in &files {
        parts.extend([path.as_bytes(), b"\0", digest.as_bytes(), b"\n"]);
    }
    let link_paths: HashSet<&str> = links.iter().map(|&(path, _)| path).collect();
    let mut link_out = None;
    for (path, blob) in links {
        let target = store.read_blob(blob)?.ok_or(Error::MissingObject(blob))?;
        if leaves_tree(path, &target, &link_paths) {
            link_out = Some(path.to_owned());
            break;
        }
    }
    let paths = || files.iter().map(|(path, _)| path.as_str());
    let installs = installs(paths());
    Ok(Deps {
        key: sha1(&parts)?,
        folders: install_folders(paths().chain(setup_scripts), &installs),
        installs,
        files,
        link_out,
    })
}

/// Whether the link at `path` (relative, `/`-separated) of a tree, whose target is `target`, may point outside
/// the tree, where an install could write through it. The target is resolved by its text only, from the link's
/// folder: it may leave if it is absolute, if its `..` parts climb above the top of the tree, or if a part
/// before its last one names another link of the tree (`links`), whose own target would decide where the rest
/// leads: chained links such as `r -> .` and `x -> r/r/../..` are caught that way.
fn leaves_tree(path: &str, target: &[u8], links: &HashSet<&str>) -> bool {
    if target.first() == Some(&b'/') {
        return true;
    }
    let mut at: Vec<&[u8]> = path.split('/').map(str::as_bytes).collect();
    at.pop();
    let parts: Vec<&[u8]> = target
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == b".." {
            if at.pop().is_none() {
                return true;
            }
        } else {
            at.push(part);
        }
        let last = i + 1 == parts.len();
        if !last
            && std::str::from_utf8(&at.join(&b'/')).is_ok_and(|through| links.contains(through))
        {
            return true;
        }
    }
    false
}

/// The key under which a copy with its own install for the dependency set `key` is recorded: with `--setup`,
/// it also holds the setup command, so that another command installs again.
fn set_key(key: &str, setup: Option<&str>) -> io::Result<String> {
    match setup {
        Some(setup) => sha1(&[b"setup\0", setup.as_bytes(), b"\0", key.as_bytes()]),
        None => Ok(key.to_owned()),
    }
}

/// The content of the project's file at `path` (relative, `/`-separated), if it is a regular file reached
/// through real folders and no larger than a snapshot file may be. A file that cannot be read counts as missing:
/// the step then gets a copy with its own install.
fn read_project_file(project: &Path, path: &str) -> Option<Vec<u8>> {
    if !parents(path).all(|parent| is_real_dir_no_follow(&native(project, parent))) {
        return None;
    }
    let full = native(project, path);
    let metadata = fs::symlink_metadata(&full).ok()?;
    (metadata.is_file() && metadata.len() <= MAX_FILE_BYTES)
        .then(|| fs::read(&full).ok())
        .flatten()
}

/// The copies with their own install, as recorded in the event log's `meta` table under [`COPIES_KEY`]: a JSON
/// object with the cache folder's instance id and, for each copy, its name, the key of its set (see
/// [`set_key`]), the result of its install (`started`, `ok` or `failed`) and its last use (a number that grows
/// with each use).
///
/// It counts only while its instance id is the cache folder's (see [`CacheDir::instance`]): a cache folder that
/// was deleted and made anew, or another one, holds none of these installs. A recorded copy whose folder is
/// missing holds no install either.
#[derive(Debug, Default)]
struct Book {
    copies: Vec<Recorded>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Recorded {
    number: usize,
    key: String,
    state: State,
    last_use: u64,
}

/// How far the install of a recorded copy went. A copy whose install was interrupted stays `Started`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Started,
    Done,
    Failed,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Done => "ok",
            Self::Failed => "failed",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Self::Started, Self::Done, Self::Failed]
            .into_iter()
            .find(|state| state.name() == name)
    }
}

impl Book {
    /// The copies recorded in `events` for the cache folder whose instance id is `instance`: none if the record
    /// is missing, unreadable or for another instance.
    fn load(events: &Store, instance: &str) -> Result<Self> {
        let recorded = events.meta(COPIES_KEY)?;
        Ok(recorded
            .and_then(|value| Self::parse(&value, instance))
            .unwrap_or_default())
    }

    fn parse(value: &str, instance: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(value).ok()?;
        if value.get("instance")?.as_str()? != instance {
            return None;
        }
        let mut copies: Vec<Recorded> = Vec::new();
        for entry in value.get("copies")?.as_array()? {
            let recorded = (|| {
                let name = entry.get("name")?.as_str()?;
                let number = (1..=MAX_OWN_COPIES).find(|n| name == format!("copy-{n}"))?;
                Some(Recorded {
                    number,
                    key: entry.get("key")?.as_str()?.to_owned(),
                    state: State::parse(entry.get("install")?.as_str()?)?,
                    last_use: entry.get("last_use")?.as_u64()?,
                })
            })();
            if let Some(recorded) = recorded
                && !copies.iter().any(|copy| copy.number == recorded.number)
            {
                copies.push(recorded);
            }
        }
        Some(Self { copies })
    }

    fn save(&self, events: &Store, instance: &str) -> Result<()> {
        let copies: Vec<Value> = self
            .copies
            .iter()
            .map(|copy| {
                json!({
                    "name": format!("copy-{}", copy.number),
                    "key": copy.key,
                    "install": copy.state.name(),
                    "last_use": copy.last_use,
                })
            })
            .collect();
        let value = json!({ "instance": instance, "copies": copies }).to_string();
        events.set_meta(COPIES_KEY, Some(&value))?;
        Ok(())
    }

    /// The last use of a copy used now.
    fn next_use(&self) -> u64 {
        self.copies
            .iter()
            .map(|copy| copy.last_use)
            .max()
            .unwrap_or(0)
            + 1
    }
}

/// A test copy of one dependency set. Dropping `copy-0` removes its links to the project's installed
/// dependencies (see [`DependencySets::remove_project_links`]).
pub struct SetCopy {
    copy: TestCopy,
    /// `copy-0` only: its links to the project's installed dependencies.
    links: Option<Links>,
}

struct Links {
    node: NodeModules,
    python: PythonEnvs,
    /// The canonical path of the copy, under which [`PROJECT_LINKS`] lists its links.
    canonical: PathBuf,
}

impl Links {
    /// Makes the links of `copy` match the step it holds (see [`NodeModules::link`] and [`PythonEnvs::link`]),
    /// listing them in [`PROJECT_LINKS`], also when making them fails halfway.
    fn link(&mut self, copy: &mut TestCopy) -> io::Result<()> {
        let mut listed = listed_links();
        let linked = self
            .node
            .link(copy)
            .and_then(|_| self.python.link(copy))
            .map(|_| ());
        self.list(&mut listed);
        linked
    }

    /// Removes the links of `copy`, never following them (see [`NodeModules::unlink`] and
    /// [`PythonEnvs::unlink`]), and from [`PROJECT_LINKS`]. Nothing is removed if a test replaced the copy's
    /// folder. Returns the first error; the links not removed stay listed.
    fn unlink(&mut self, copy: &mut TestCopy) -> io::Result<()> {
        if self.node.farms().next().is_none() && self.python.links().next().is_none() {
            return Ok(());
        }
        let mut listed = listed_links();
        copy.check_still_at_path()?;
        let node = self.node.unlink(copy);
        let python = self.python.unlink(copy);
        self.list(&mut listed);
        node.and(python)
    }

    /// Makes `listed` hold, for this copy, exactly the links it has now.
    fn list(&self, listed: &mut BTreeSet<(PathBuf, String)>) {
        listed.retain(|(root, _)| *root != self.canonical);
        listed.extend(
            self.node
                .farms()
                .chain(self.python.links())
                .map(|path| (self.canonical.clone(), path.to_owned())),
        );
    }
}

fn listed_links() -> MutexGuard<'static, BTreeSet<(PathBuf, String)>> {
    PROJECT_LINKS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes every link into the project that a `copy-0` of this process holds, for a second Ctrl-C, which makes
/// Yalper exit without dropping anything. Links being made or removed are waited for, then removed. Returns the
/// list, locked and empty: held until Yalper exits, it keeps a bisect still going from making any more.
pub(super) fn remove_all_project_links() -> MutexGuard<'static, BTreeSet<(PathBuf, String)>> {
    let mut listed = listed_links();
    remove_listed(&mut listed);
    listed
}

/// Removes the links into the project that a bisect which did not end left in `copy-0` of the project's cache
/// folder `cache` (see [`NodeModules::remove_left_links`]), whether or not this bisect uses `copy-0`. Call it at
/// the start of a bisect, after [`clean_root`], which removes a link in place of a copy, and before any copy is
/// used. Nothing is removed unless `copy-0` and the cache folder are the real folders Yalper opened. Returns a
/// message if the links could not be looked for or removed.
pub(super) fn remove_left_links(cache: &CacheDir) -> Option<String> {
    let path = cache.path().join("copy-0");
    if !is_real_dir_no_follow(&path) {
        return None;
    }
    let removed = TestCopy::open(cache, 0).and_then(|copy| {
        copy.check_still_at_path()?;
        NodeModules::new(cache.project())?.remove_left_links(&copy)
    });
    removed.err().map(|error| {
        format!(
            "cannot remove the links to your project's installed dependencies that an earlier bisect left in the \
             test copy {}: {error}. If you delete it by hand, use a tool that does not follow links.",
            path.display()
        )
    })
}

/// Removes every link of `listed` (see [`PROJECT_LINKS`]) and empties it. A link is removed only through a copy
/// still at its canonical path and real folders, never followed (see [`remove_copied`]).
fn remove_listed(listed: &mut BTreeSet<(PathBuf, String)>) {
    for (root, path) in std::mem::take(listed) {
        let _ = remove_copied(&root, &path);
    }
}

impl SetCopy {
    pub fn copy(&self) -> &TestCopy {
        &self.copy
    }

    /// `copy-0` only: removes its links to the project's installed dependencies (`node_modules` farms, links to
    /// Python environments), never following them, so that none stays in the cache folder between bisects. The
    /// next step that runs in `copy-0` makes them again.
    fn remove_links(&mut self) -> io::Result<()> {
        match self.links.as_mut() {
            Some(links) => links.unlink(&mut self.copy),
            None => Ok(()),
        }
    }

    /// The complete environment of a run in this copy, for [`run`]: [`run_env`] of the user's environment `user`
    /// with the additions of [`env_additions`]. In a copy with its own install, the variables of
    /// `OWN_INSTALL_REMOVED` are removed and those of `OWN_INSTALL_SET` set, so that tools use the copy's
    /// own environments. Call it before every run, after [`clean_root`] and [`write_boundaries`].
    pub fn run_env(
        &self,
        user: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> io::Result<Vec<(OsString, OsString)>> {
        let python = self.links.as_ref().map(|links| &links.python);
        let additions = env_additions(&self.copy, python)?;
        let mut env = run_env(user, self.copy.cache().project(), &self.copy, additions)?;
        if self.links.is_none() {
            env.retain(|(name, _)| {
                !OWN_INSTALL_REMOVED
                    .iter()
                    .chain(OWN_INSTALL_SET.iter().map(|(set, _)| set))
                    .any(|var| same_name(name, var))
            });
            env.extend(
                OWN_INSTALL_SET
                    .iter()
                    .map(|&(name, value)| (OsString::from(name), OsString::from(value))),
            );
        }
        Ok(env)
    }
}

impl Drop for SetCopy {
    fn drop(&mut self) {
        let _ = self.remove_links();
    }
}

/// Why the dependencies of a set could not be installed. The message (its `Display`) holds paths from the
/// step's tree and of the cache folder: print it through `printable()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallFailure {
    /// An install command failed.
    Failed {
        /// The command, its secrets masked.
        command: String,
        /// The folder of the copy it ran in (relative, `/`-separated, `""` for the top), as the step's tree names
        /// it.
        dir: String,
        outcome: Outcome,
        /// The log holding the install's output.
        log: PathBuf,
        /// The end of the log shows what looks like a registry or login error.
        registry_error: bool,
    },
    /// The step's code has a link (or a file) at `path`, where an install writes: nothing was run, since the
    /// install would write through it.
    Blocked { path: String, link: bool },
    /// Unix only: the step's code has a link at `path` whose target is outside the test copy (absolute, or
    /// climbing above it): nothing was run, since an install could write through it.
    LinkOut { path: String },
}

impl fmt::Display for InstallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (command, dir, outcome, log, registry_error) = match self {
            Self::Blocked { path, link } => {
                let what = if *link { "a link" } else { "a file" };
                return write!(
                    f,
                    "the step's code has {what} at {path}, where the install of its dependencies writes; \
                     Yalper does not install through it"
                );
            }
            Self::LinkOut { path } => {
                return write!(
                    f,
                    "the step's code has a link at {path} that points outside the test copy; Yalper does \
                     not install its dependencies while it is there"
                );
            }
            Self::Failed {
                command,
                dir,
                outcome,
                log,
                registry_error,
            } => (command, dir, outcome, log, registry_error),
        };
        let place = if dir.is_empty() {
            "at the top of the test copy".to_owned()
        } else {
            format!("in {dir}")
        };
        let ended = match outcome {
            Outcome::Failed(exit) => exit.to_string(),
            Outcome::Skipped => format!("exit code {SKIP_EXIT_CODE}"),
            Outcome::OutputTooLarge => "more output than a log may hold".to_owned(),
            Outcome::TimedOut => "a time limit".to_owned(),
            Outcome::Passed | Outcome::Stopped => "no error".to_owned(),
        };
        write!(
            f,
            "installing the dependencies failed: `{command}` {place} ended with {ended}"
        )?;
        if let Some(hint) = outcome.hint() {
            write!(f, " ({hint})")?;
        }
        write!(f, ". Its output is in {}", log.display())?;
        if *registry_error {
            write!(
                f,
                ". If the install needs a registry login or settings from a file your project ignores \
                 (such as .npmrc or .cargo/config.toml), add that file with --with <path>"
            )?;
        }
        Ok(())
    }
}

/// What [`DependencySets::prepare`] gives for a step.
pub enum Prepared<'a> {
    /// The copy holds the step's code and dependencies: run the tests there.
    Ready(&'a SetCopy),
    /// The step's dependency set could not be installed in this bisect: the step cannot be tested.
    CannotTest(&'a InstallFailure),
    /// The stop flag was set during an install, whose processes were killed.
    Stopped,
}

/// How the installs of a copy ended.
enum Installed {
    Done,
    Failed(InstallFailure),
    Stopped,
}

/// The test copies of one bisect, one per dependency set (see the module documentation).
pub struct DependencySets {
    cache: CacheDir,
    setup: Option<String>,
    user: Vec<(OsString, OsString)>,
    book: Book,
    /// The copies used in this bisect, by number. `copy-0` is opened, with its links, as soon as the project's
    /// installed folders are looked at.
    copies: BTreeMap<usize, SetCopy>,
    deps: HashMap<ObjectId, Rc<Deps>>,
    digests: HashMap<(ObjectId, Counted), String>,
    /// The digest of each dependency file of the project read so far, by path: `None` if it has none there.
    project_digests: HashMap<String, Option<String>>,
    /// Links to the project's dependencies cannot be made on this system: `copy-0` is not used.
    no_links: bool,
    /// The sets whose install failed in this bisect, by the key of [`set_key`].
    failed: HashMap<String, InstallFailure>,
    /// Installs run in this bisect, which numbers their logs.
    installs: usize,
    /// Copies deleted and filled anew for a set in this bisect (see [`copies_filled`](Self::copies_filled)).
    filled: usize,
    /// The env files and `--with` files put in every copy before its runs and installs.
    local_files: Option<LocalFiles>,
}

impl DependencySets {
    /// The dependency sets of a bisect of the project whose cache folder is `cache` and whose event log is
    /// `events`. `setup` is the `--setup` command, and `user` the user's environment (usually
    /// `std::env::vars_os()`), from which every run's and install's environment is built.
    pub fn new(
        cache: &CacheDir,
        events: &Store,
        setup: Option<String>,
        user: Vec<(OsString, OsString)>,
    ) -> Result<Self> {
        Ok(Self {
            cache: cache.try_clone()?,
            setup,
            user,
            book: Book::load(events, cache.instance())?,
            copies: BTreeMap::new(),
            deps: HashMap::new(),
            digests: HashMap::new(),
            project_digests: HashMap::new(),
            no_links: false,
            failed: HashMap::new(),
            installs: 0,
            filled: 0,
            local_files: None,
        })
    }

    /// Puts `files` (the env files and `--with` files of the bisect) into every copy, before each run and
    /// install. They are deleted from every copy when these sets are dropped, or by
    /// [`remove_local_files`](Self::remove_local_files).
    pub fn with_local_files(mut self, files: LocalFiles) -> Self {
        self.local_files = Some(files);
        self
    }

    /// See [`with_local_files`](Self::with_local_files).
    pub fn local_files(&self) -> Option<&LocalFiles> {
        self.local_files.as_ref()
    }

    /// Deletes the env files and `--with` files from every copy. Returns a message for each one that could not
    /// be deleted.
    pub fn remove_local_files(&mut self) -> Vec<String> {
        self.local_files
            .as_mut()
            .map(LocalFiles::remove)
            .unwrap_or_default()
    }

    /// Removes the links of `copy-0` to the project's installed dependencies, never following them: call it
    /// when the bisect ends. Dropping these sets does the same, without the message. Returns a message if some
    /// could not be removed.
    pub fn remove_project_links(&mut self) -> Option<String> {
        let set_copy = self.copies.get_mut(&0)?;
        set_copy.remove_links().err().map(|error| {
            format!(
                "cannot remove the links to your project's installed dependencies from the test copy {}: \
                 {error}. If you delete it by hand, use a tool that does not follow links.",
                set_copy.copy.path().display()
            )
        })
    }

    /// Forgets the links of `copy-0` without removing them, as when Yalper is killed or crashes.
    #[cfg(test)]
    pub(super) fn leak_project_links(&mut self) {
        if let Some(set_copy) = self.copies.get_mut(&0) {
            std::mem::forget(set_copy.links.take());
        }
    }

    /// Puts the env files and `--with` files into copy `number`, which holds a step.
    fn sync_local_files(&mut self, number: usize) -> io::Result<()> {
        if let (Some(files), Some(set_copy)) =
            (self.local_files.as_mut(), self.copies.get_mut(&number))
        {
            files.sync(&mut set_copy.copy)?;
        }
        Ok(())
    }

    /// How many times a copy was deleted and filled anew for a dependency set (installed) in this bisect. When
    /// it grows during a [`prepare`](Self::prepare), the copy it gives was just filled: nothing is built in it
    /// yet (a Rust project compiles everything in its first test run).
    pub fn copies_filled(&self) -> usize {
        self.filled
    }

    /// Makes a copy hold the step whose tree in `store` is `tree`, with its dependencies: `copy-0` linked to the
    /// project's installed dependencies when the step's are the same, otherwise the copy of the step's set,
    /// installed first if no copy holds the set yet (the output in a new log of `logs`, stopped when `stop` is
    /// set). The copies with their own install are recorded in `events`.
    pub fn prepare(
        &mut self,
        events: &Store,
        store: &ShadowStore,
        tree: ObjectId,
        logs: &Logs,
        stop: &AtomicBool,
    ) -> Result<Prepared<'_>> {
        let deps = self.deps_of(store, tree)?;
        if !self.no_links && self.project_matches(&deps)? && self.use_project_copy(store, tree)? {
            self.sync_local_files(0)?;
            return Ok(Prepared::Ready(&self.copies[&0]));
        }
        let set = set_key(&deps.key, self.setup.as_deref())?;
        if self.failed.contains_key(&set) {
            return Ok(Prepared::CannotTest(&self.failed[&set]));
        }
        let last_use = self.book.next_use();
        let installed = self.book.copies.iter_mut().find(|copy| {
            copy.key == set && copy.state == State::Done && copy_exists(&self.cache, copy.number)
        });
        let Some(recorded) = installed else {
            return self.install(events, store, tree, &deps, logs, stop);
        };
        recorded.last_use = last_use;
        let number = recorded.number;
        self.book.save(events, self.cache.instance())?;
        self.open_own(number, &deps)?.copy.switch(store, tree)?;
        self.sync_local_files(number)?;
        Ok(Prepared::Ready(&self.copies[&number]))
    }

    fn deps_of(&mut self, store: &ShadowStore, tree: ObjectId) -> Result<Rc<Deps>> {
        if let Some(deps) = self.deps.get(&tree) {
            return Ok(Rc::clone(deps));
        }
        let deps = Rc::new(read_deps(store, tree, &mut self.digests)?);
        self.deps.insert(tree, Rc::clone(&deps));
        Ok(deps)
    }

    /// Whether the step whose dependencies are `deps` can run with the project's installed dependencies: the
    /// project has the same dependency files and, if the step has Node or Python lockfiles, an installed folder
    /// next to at least one of them that `copy-0` can link.
    fn project_matches(&mut self, deps: &Deps) -> Result<bool> {
        let project = self.cache.project();
        for (path, digest) in &deps.files {
            let found = self.project_digests.entry(path.clone()).or_insert_with(|| {
                let name = path.rsplit('/').next().unwrap_or(path);
                let bytes = read_project_file(project, path)?;
                self::digest(counted(name)?, &bytes).ok()
            });
            if found.as_deref() != Some(digest.as_str()) {
                return Ok(false);
            }
        }
        let mut linked = deps
            .installs
            .iter()
            .filter(|install| install.tool != Tool::Cargo)
            .peekable();
        if linked.peek().is_none() {
            return Ok(true);
        }
        for install in linked {
            if self.project_has_installed(install)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the project has the folder `install` fills, installed: a `node_modules` or Python environment that
    /// `copy-0` links (the same checks as [`NodeModules::link`] and [`PythonEnvs::link`]), or cargo's `target`.
    fn project_has_installed(&mut self, install: &Install) -> Result<bool> {
        if install.tool.ecosystem() == Ecosystem::Rust {
            let target = join(&install.dir, "target");
            let project = self.cache.project();
            return Ok(parents(&target)
                .chain([target.as_str()])
                .all(|folder| is_real_dir_no_follow(&native(project, folder))));
        }
        let links = self.project_links()?;
        Ok(match install.tool.ecosystem() {
            Ecosystem::Python => links.python.reusable(&install.dir)?,
            _ => links.node.reusable(&install.dir)?,
        })
    }

    /// The links of `copy-0` to the project's installed dependencies, opening `copy-0` if needed.
    fn project_links(&mut self) -> Result<&mut Links> {
        if let btree_map::Entry::Vacant(entry) = self.copies.entry(0) {
            let project = self.cache.project();
            let copy = TestCopy::open(&self.cache, 0)?;
            let canonical = plain(&fs::canonicalize(copy.path())?);
            entry.insert(SetCopy {
                copy,
                links: Some(Links {
                    node: NodeModules::new(project)?,
                    python: PythonEnvs::new(project)?,
                    canonical,
                }),
            });
        }
        let set_copy = self.copies.get_mut(&0).expect("opened above");
        Ok(set_copy.links.as_mut().expect("copy-0 has links"))
    }

    /// Switches `copy-0` to `tree` and links it to the project's installed dependencies. Returns `false`, and
    /// stops using `copy-0` for the rest of the bisect, when the links cannot be made on this system.
    fn use_project_copy(&mut self, store: &ShadowStore, tree: ObjectId) -> Result<bool> {
        self.project_links()?;
        let SetCopy { copy, links } = self.copies.get_mut(&0).expect("opened above");
        let links = links.as_mut().expect("copy-0 has links");
        copy.switch(store, tree)?;
        match links.link(copy) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == CANNOT_LINK => {
                self.no_links = true;
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The commands that install the step whose dependencies are `deps` in a copy of its own: the `--setup`
    /// command at the top (which may write in every folder of [`install_folders`]), or the commands of its
    /// lockfiles, parent folders first. In each ecosystem (Node, Python, Rust), a lockfile is installed only
    /// where the project has installed that ecosystem's folder (`node_modules`, a Python environment, cargo's
    /// `target`), or everywhere if the project has none of them: lockfiles of documentation, examples or
    /// fixtures are not installed when the project never installed them. `cargo fetch` also always runs for a
    /// `Cargo.lock` at the top of the step, so that a crate that cannot be downloaded makes the set "cannot test"
    /// instead of failing its tests; it costs nothing when the crates are already downloaded.
    fn own_installs(&mut self, deps: &Deps) -> Result<Vec<Planned>> {
        if let Some(setup) = &self.setup {
            return Ok(vec![Planned {
                dir: String::new(),
                command: setup.clone(),
                folders: deps.folders.clone(),
            }]);
        }
        let mut chosen = vec![false; deps.installs.len()];
        for ecosystem in [Ecosystem::Node, Ecosystem::Python, Ecosystem::Rust] {
            let mut any_installed = false;
            let mut all = Vec::new();
            for (i, install) in deps.installs.iter().enumerate() {
                if install.tool.ecosystem() == ecosystem {
                    all.push(i);
                    if self.project_has_installed(install)? {
                        chosen[i] = true;
                        any_installed = true;
                    }
                }
            }
            for &i in &all {
                let at_top = deps.installs[i].dir.is_empty();
                if !any_installed || (ecosystem == Ecosystem::Rust && at_top) {
                    chosen[i] = true;
                }
            }
        }
        Ok(deps
            .installs
            .iter()
            .zip(chosen)
            .filter(|(_, chosen)| *chosen)
            .map(|(install, _)| Planned {
                dir: install.dir.clone(),
                command: install.tool.command().to_owned(),
                folders: install
                    .tool
                    .folders()
                    .iter()
                    .map(|folder| join(&install.dir, folder))
                    .collect(),
            })
            .collect())
    }

    /// Installs the dependency set of the step `tree`, whose dependencies are `deps`, into a copy of its own: the
    /// lowest numbered copy that holds no install, else the least recently used one, deleted first.
    fn install(
        &mut self,
        events: &Store,
        store: &ShadowStore,
        tree: ObjectId,
        deps: &Deps,
        logs: &Logs,
        stop: &AtomicBool,
    ) -> Result<Prepared<'_>> {
        let set = set_key(&deps.key, self.setup.as_deref())?;
        let commands = self.own_installs(deps)?;
        let holds_install = |number: usize| {
            self.book.copies.iter().any(|copy| {
                copy.number == number
                    && copy.state == State::Done
                    && copy_exists(&self.cache, number)
            })
        };
        let number = (1..=MAX_OWN_COPIES)
            .find(|&number| !holds_install(number))
            .or_else(|| {
                let oldest = self.book.copies.iter().min_by_key(|copy| copy.last_use);
                oldest.map(|copy| copy.number)
            })
            .unwrap_or(1);
        let last_use = self.book.next_use();
        self.book.copies.retain(|copy| copy.number != number);
        // Closed before it is deleted: on Windows a folder held open cannot be deleted.
        self.copies.remove(&number);
        delete_copy(&self.cache, number)?;
        self.filled += 1;
        self.book.copies.push(Recorded {
            number,
            key: set.clone(),
            state: State::Started,
            last_use,
        });
        self.book.save(events, self.cache.instance())?;

        self.open_own(number, deps)?.copy.switch(store, tree)?;
        self.sync_local_files(number)?;
        let state = match self.run_installs(number, deps, &commands, logs, stop)? {
            Installed::Done => {
                // Undoes what the install changed in the step's files, as before every run.
                self.copies
                    .get_mut(&number)
                    .expect("opened above")
                    .copy
                    .switch(store, tree)?;
                self.sync_local_files(number)?;
                State::Done
            }
            Installed::Failed(failure) => {
                self.failed.insert(set.clone(), failure);
                State::Failed
            }
            Installed::Stopped => return Ok(Prepared::Stopped),
        };
        if let Some(recorded) = self
            .book
            .copies
            .iter_mut()
            .find(|copy| copy.number == number)
        {
            recorded.state = state;
        }
        self.book.save(events, self.cache.instance())?;
        Ok(match state {
            State::Done => Prepared::Ready(&self.copies[&number]),
            _ => Prepared::CannotTest(&self.failed[&set]),
        })
    }

    /// Opens copy `number`, a copy with its own install for `deps`, and keeps the folders an install may fill
    /// (see [`install_folders`]) through every switch, ignored or not.
    fn open_own(&mut self, number: usize, deps: &Deps) -> Result<&mut SetCopy> {
        let set_copy = match self.copies.entry(number) {
            btree_map::Entry::Occupied(entry) => entry.into_mut(),
            btree_map::Entry::Vacant(entry) => entry.insert(SetCopy {
                copy: TestCopy::open(&self.cache, number)?,
                links: None,
            }),
        };
        let mut extra = set_copy.copy.extra_paths().clone();
        extra.extend(deps.folders.iter().cloned());
        set_copy.copy.set_extra_paths(extra);
        Ok(set_copy)
    }

    /// Runs `commands` in copy `number`, which holds a step whose dependencies are `deps`, one after the other,
    /// with the protections of a test run, their output in one new log of `logs`. With no command there is
    /// nothing to install. Otherwise nothing runs, on Unix, while the step has a link that may point outside the
    /// copy. Before each command, the folders it writes in are checked: a link or a file there, or above them,
    /// stops the install.
    fn run_installs(
        &mut self,
        number: usize,
        deps: &Deps,
        commands: &[Planned],
        logs: &Logs,
        stop: &AtomicBool,
    ) -> Result<Installed> {
        if commands.is_empty() {
            return Ok(Installed::Done);
        }
        // Windows writes the links of a tree as plain files, which nothing follows.
        if cfg!(unix)
            && let Some(path) = &deps.link_out
        {
            let path = path.clone();
            return Ok(Installed::Failed(InstallFailure::LinkOut { path }));
        }
        self.installs += 1;
        let name = format!("install-{}.log", self.installs);
        let mut log = logs.create(&name)?;
        let set_copy = &self.copies[&number];
        let root = set_copy.copy.path();
        for Planned {
            dir,
            command,
            folders,
        } in commands
        {
            for folder in folders {
                if let Some((path, link)) = blocked(root, folder)? {
                    return Ok(Installed::Failed(InstallFailure::Blocked { path, link }));
                }
            }
            clean_root(&self.cache)?;
            write_boundaries(&self.cache)?;
            let env = set_copy.run_env(self.user.iter().cloned())?;
            let cwd = folder_in_copy(root, dir)?;
            let shown = redact_str(command).unwrap_or_else(|| command.clone());
            let place = if dir.is_empty() { "." } else { dir };
            writeln!(log, "$ {shown}    (in {place})")?;
            match run(command, &cwd, &env, &log, None, stop)?.outcome {
                Outcome::Passed => {}
                Outcome::Stopped => return Ok(Installed::Stopped),
                outcome => {
                    return Ok(Installed::Failed(InstallFailure::Failed {
                        command: shown,
                        dir: dir.clone(),
                        outcome,
                        log: logs.path().join(&name),
                        registry_error: shows_registry_error(&log),
                    }));
                }
            }
        }
        Ok(Installed::Done)
    }
}

/// Whether copy `number` of the project's cache folder `cache` is a real folder.
fn copy_exists(cache: &CacheDir, number: usize) -> bool {
    is_real_dir_no_follow(&cache.path().join(format!("copy-{number}")))
}

/// The path (relative, `/`-separated) of a link or a file, with whether it is a link, found at the folder
/// `folder` of the copy at `root` or at a folder above it: an install would write through it. `None` when
/// `folder` is a real folder reached through real folders, or missing. Any other error looking it up is returned.
fn blocked(root: &Path, folder: &str) -> io::Result<Option<(String, bool)>> {
    for path in parents(folder).chain([folder]) {
        match fs::symlink_metadata(native(root, path)) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(metadata) => return Ok(Some((path.to_owned(), metadata.is_symlink()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot check {path} in the test copy before the install: {error}"),
                ));
            }
        }
    }
    Ok(None)
}

/// Deletes copy `number`, and its folders in `cargo-target/` and `pycache/`, from the project's cache folder
/// `cache`, never following a link. A link in place of `cargo-target/` or `pycache/` is not entered: it is left
/// for [`clean_root`] to remove.
fn delete_copy(cache: &CacheDir, number: usize) -> io::Result<()> {
    cache.dir().check_still_at_path()?;
    let name = format!("copy-{number}");
    let root = cache.path();
    let mut paths = vec![root.join(&name)];
    for folder in [CARGO_TARGET_DIR, PYCACHE_DIR] {
        if is_real_dir_no_follow(&root.join(folder)) {
            paths.push(root.join(folder).join(&name));
        }
    }
    for path in paths {
        match fs::symlink_metadata(&path) {
            Ok(metadata) => remove_entry(&path, &metadata).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot delete {}: {error}", path.display()),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The folder `dir` (relative, `/`-separated, `""` for the top) of the copy at `root`, checked to be a real
/// folder reached through real folders: an earlier install may have put a link there.
fn folder_in_copy(root: &Path, dir: &str) -> io::Result<PathBuf> {
    if dir.is_empty() {
        return Ok(root.to_owned());
    }
    if !parents(dir)
        .chain([dir])
        .all(|folder| is_real_dir_no_follow(&native(root, folder)))
    {
        return Err(io::Error::other(format!(
            "cannot install in {dir} in the test copy: it is not a real folder"
        )));
    }
    Ok(native(root, dir))
}

/// Whether the end of the install log `log` shows what looks like a registry or login error. False if the log
/// cannot be read.
fn shows_registry_error(mut log: &File) -> bool {
    let mut tail = Vec::new();
    let read = log.seek(SeekFrom::End(0)).and_then(|end| {
        log.seek(SeekFrom::Start(end.saturating_sub(LOG_TAIL_BYTES)))?;
        log.take(LOG_TAIL_BYTES).read_to_end(&mut tail)
    });
    read.is_ok() && is_registry_error(&tail)
}

fn is_registry_error(output: &[u8]) -> bool {
    let text = String::from_utf8_lossy(output).to_lowercase();
    REGISTRY_ERROR_TEXTS
        .iter()
        .any(|error| text.contains(error))
        || text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| REGISTRY_ERROR_WORDS.contains(&word))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::process::Command;

    use crate::bisect::link::make_link;
    use crate::bisect::run::Exit;
    use crate::bisect::testing::{Project, links_under, state, token, tool, venv_python, write};

    fn environment() -> Vec<(OsString, OsString)> {
        std::env::vars_os().collect()
    }

    /// The key of the step whose tree holds `files`.
    fn key_of(project: &Project, files: &[(&str, &str)]) -> String {
        read_deps(&project.store, project.tree(files), &mut HashMap::new())
            .unwrap()
            .key
    }

    /// A command that appends a line to the file `counter`, in `sh` and in `cmd.exe`.
    fn append_to(counter: &Path) -> String {
        format!("echo x>> \"{}\"", counter.display())
    }

    fn lines(counter: &Path) -> usize {
        fs::read_to_string(counter).map_or(0, |text| text.lines().count())
    }

    /// The name of the folder of `set_copy`, such as `copy-1`.
    fn name(set_copy: &SetCopy) -> String {
        let path = set_copy.copy().path();
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    /// One bisect of a project: its dependency sets, run logs and event log.
    struct Bisect {
        sets: DependencySets,
        logs: Logs,
        events: Store,
    }

    impl Bisect {
        fn new(project: &Project, setup: Option<String>) -> Self {
            Self::in_cache(project, &project.cache, setup)
        }

        fn in_cache(project: &Project, cache: &CacheDir, setup: Option<String>) -> Self {
            let events = project.events();
            Self {
                sets: DependencySets::new(cache, &events, setup, environment()).unwrap(),
                logs: Logs::open(cache).unwrap(),
                events,
            }
        }

        fn prepare(&mut self, project: &Project, tree: ObjectId) -> Prepared<'_> {
            let stop = AtomicBool::new(false);
            self.sets
                .prepare(&self.events, &project.store, tree, &self.logs, &stop)
                .unwrap()
        }

        /// The name of the copy that holds `tree`, ready for a run.
        fn ready(&mut self, project: &Project, tree: ObjectId) -> String {
            ready(self.prepare(project, tree)).0
        }

        /// The install failure of the step `tree`.
        fn cannot_test(&mut self, project: &Project, tree: ObjectId) -> InstallFailure {
            match self.prepare(project, tree) {
                Prepared::CannotTest(failure) => failure.clone(),
                Prepared::Ready(set_copy) => panic!("{} is ready", name(set_copy)),
                Prepared::Stopped => panic!("stopped"),
            }
        }
    }

    /// The name of the copy that `prepared` gives, and the copy.
    fn ready(prepared: Prepared<'_>) -> (String, &SetCopy) {
        match prepared {
            Prepared::Ready(set_copy) => (name(set_copy), set_copy),
            Prepared::CannotTest(failure) => {
                let log = match failure {
                    InstallFailure::Failed { log, .. } => fs::read_to_string(log).unwrap(),
                    InstallFailure::Blocked { .. } | InstallFailure::LinkOut { .. } => {
                        String::new()
                    }
                };
                panic!("{failure}\n{log}")
            }
            Prepared::Stopped => panic!("stopped"),
        }
    }

    /// What a failed install command gives: the command, its folder, how it ended, its log and whether it shows
    /// a registry error.
    fn failed(failure: &InstallFailure) -> (&str, &str, Outcome, &Path, bool) {
        match failure {
            InstallFailure::Failed {
                command,
                dir,
                outcome,
                log,
                registry_error,
            } => (command, dir, *outcome, log, *registry_error),
            InstallFailure::Blocked { .. } | InstallFailure::LinkOut { .. } => panic!("{failure}"),
        }
    }

    #[test]
    fn scripts_and_tool_settings_keep_the_key_and_dependencies_change_it() {
        let project = Project::new();
        let package = |dependencies: &str, test: &str| {
            format!(
                r#"{{"name": "app", "scripts": {{"test": "{test}"}}, "dependencies": {{{dependencies}}}}}"#
            )
        };
        let pyproject = |dependencies: &str, addopts: &str| {
            format!(
                "[project]\nname = \"app\"\ndependencies = [{dependencies}]\n\n\
                 [tool.pytest.ini_options]\naddopts = \"{addopts}\"\n"
            )
        };
        let key = |package: &str, pyproject: &str, lock: &str, more: &[(&str, &str)]| {
            let mut files = vec![
                ("package.json", package),
                ("pyproject.toml", pyproject),
                ("package-lock.json", lock),
                ("src/app.js", "1"),
            ];
            files.extend_from_slice(more);
            key_of(&project, &files)
        };
        let (deps, requests) = (r#""a": "1", "b": "2""#, r#""requests""#);
        let (node, quiet) = (package(deps, "node --test"), pyproject(requests, "-q"));
        let base = key(&node, &quiet, "1", &[]);

        // A script, the order of the dependencies, a pytest setting, code, a committed package.
        let reordered = package(r#""b": "2", "a": "1""#, "node --test");
        let same = [
            key(&package(deps, "vitest"), &quiet, "1", &[]),
            key(&reordered, &quiet, "1", &[]),
            key(&node, &pyproject(requests, "-x"), "1", &[]),
            key(
                &node,
                &quiet,
                "1",
                &[("src/other.js", "2"), ("node_modules/x/package.json", "{}")],
            ),
        ];
        for (i, same) in same.iter().enumerate() {
            assert_eq!(same, &base, "case {i}");
        }

        // A new dependency of each manifest, the lockfile, manifests in subfolders.
        let web = package(r#""x": "1""#, "jest");
        let different = [
            key(
                &package(r#""a": "1", "c": "3""#, "node --test"),
                &quiet,
                "1",
                &[],
            ),
            key(&node, &pyproject(r#""requests", "rich""#, "-q"), "1", &[]),
            key(&node, &quiet, "2", &[]),
            key(&node, &quiet, "1", &[("web/package.json", &web)]),
            key(
                &node,
                &quiet,
                "1",
                &[("api/requirements-dev.txt", "pytest")],
            ),
        ];
        for (i, different) in different.iter().enumerate() {
            assert_ne!(different, &base, "case {i}");
        }

        // In a subfolder too, only the dependency sections count.
        let with_web = |dependencies: &str, test: &str| {
            key_of(
                &project,
                &[("web/package.json", &package(dependencies, test))],
            )
        };
        assert_eq!(
            with_web(r#""x": "1""#, "jest"),
            with_web(r#""x": "1""#, "vitest")
        );
        assert_ne!(
            with_web(r#""x": "1""#, "jest"),
            with_web(r#""x": "2""#, "jest")
        );

        // Package manager settings count as whole files.
        for settings in [
            ".python-version",
            "uv.toml",
            "bunfig.toml",
            ".yarnrc",
            ".pnpmfile.cjs",
            "pnpm-workspace.yaml",
            ".npmrc",
            ".yarnrc.yml",
        ] {
            let with = |content: &str| key_of(&project, &[("index.js", ""), (settings, content)]);
            assert_ne!(with("1"), with("2"), "{settings}");
        }
        // pnpm's field of `package.json` counts.
        let pnpm = |overrides: &str| {
            let manifest = format!(r#"{{"pnpm": {{"overrides": {{"a": "{overrides}"}}}}}}"#);
            key_of(&project, &[("package.json", &manifest)])
        };
        assert_ne!(pnpm("1"), pnpm("2"));
        // The order of a table's entries does not count, the order of a list's items does.
        assert_ne!(
            key(&node, &pyproject(r#""a", "b""#, "-q"), "1", &[]),
            key(&node, &pyproject(r#""b", "a""#, "-q"), "1", &[])
        );

        // A manifest that does not parse counts as a whole.
        let broken = |text: &str| {
            key_of(
                &project,
                &[("package.json", text), ("pyproject.toml", text)],
            )
        };
        assert_ne!(broken("{ \"scripts\": "), broken("{ \"scripts\": 1"));
    }

    #[test]
    fn each_lockfile_picks_its_install_command() {
        let commands = |files: &[&str]| -> Vec<(String, &'static str)> {
            installs(files.iter().copied())
                .into_iter()
                .map(|install| (install.dir, install.tool.command()))
                .collect()
        };
        let at_top = |command: &'static str| vec![(String::new(), command)];
        assert_eq!(
            commands(&["package.json", "package-lock.json"]),
            at_top("npm ci")
        );
        assert_eq!(commands(&["npm-shrinkwrap.json"]), at_top("npm ci"));
        assert_eq!(
            commands(&["pnpm-lock.yaml", "pnpm-workspace.yaml"]),
            at_top("pnpm install --frozen-lockfile")
        );
        assert_eq!(
            commands(&["yarn.lock"]),
            at_top("yarn install --frozen-lockfile")
        );
        assert_eq!(
            commands(&["yarn.lock", ".yarnrc.yml"]),
            at_top("yarn install --immutable")
        );
        for bun in ["bun.lock", "bun.lockb"] {
            assert_eq!(commands(&[bun]), at_top("bun install --frozen-lockfile"));
        }
        assert_eq!(
            commands(&["pyproject.toml", "uv.lock"]),
            at_top("uv sync --frozen")
        );
        assert_eq!(
            commands(&["pyproject.toml", "poetry.lock"]),
            at_top("poetry install --no-interaction")
        );
        let pip = Tool::Pip.command();
        assert_eq!(commands(&["requirements.txt"]), at_top(pip));
        let python = if cfg!(windows) { "python" } else { "python3" };
        assert!(
            pip.starts_with(&format!("{python} -m venv .venv && ")),
            "{pip}"
        );
        assert!(
            pip.ends_with(" -m pip install -r requirements.txt"),
            "{pip}"
        );
        assert_eq!(commands(&["Cargo.lock"]), at_top("cargo fetch"));

        // One Node and one Python package manager per folder, in the order of the table.
        assert_eq!(
            commands(&[
                "yarn.lock",
                "package-lock.json",
                "requirements.txt",
                "uv.lock"
            ]),
            vec![
                (String::new(), "npm ci"),
                (String::new(), "uv sync --frozen")
            ]
        );
        // Without a lockfile there is nothing to install from.
        assert!(commands(&["package.json", "pyproject.toml", "requirements-dev.txt"]).is_empty());
        // Each folder that holds a lockfile, parents first.
        assert_eq!(
            commands(&[
                "web/package-lock.json",
                "Cargo.lock",
                "api/uv.lock",
                "web/app/yarn.lock"
            ]),
            vec![
                (String::new(), "cargo fetch"),
                ("api".to_owned(), "uv sync --frozen"),
                ("web".to_owned(), "npm ci"),
                ("web/app".to_owned(), "yarn install --frozen-lockfile"),
            ]
        );

        // From a step's tree: committed packages and links are not dependency files.
        let project = Project::new();
        let tree = project.typed_tree(&[
            ("package-lock.json", FileKind::Regular, "{}"),
            ("node_modules/x/package-lock.json", FileKind::Regular, "{}"),
            ("api/uv.lock", FileKind::Symlink, "../uv.lock"),
            ("tools/Cargo.lock", FileKind::Executable, ""),
        ]);
        let deps = read_deps(&project.store, tree, &mut HashMap::new()).unwrap();
        let installs: Vec<(&str, Tool)> = deps
            .installs
            .iter()
            .map(|install| (install.dir.as_str(), install.tool))
            .collect();
        assert_eq!(installs, [("", Tool::Npm), ("tools", Tool::Cargo)]);
    }

    #[test]
    fn each_set_is_installed_once_and_a_fourth_set_evicts_the_least_recently_used() {
        let project = Project::new();
        let counter_dir = tempfile::tempdir().unwrap();
        let counter = counter_dir.path().join("counter");
        let step = |set: &str, code: &str| {
            let files = [
                ("package.json", "{}"),
                ("package-lock.json", set),
                ("index.js", code),
            ];
            project.tree(&files)
        };
        let (a1, a2) = (step("a", "1"), step("a", "2"));
        let (b, c, d) = (step("b", "1"), step("c", "1"), step("d", "1"));

        let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
        assert_eq!(bisect.ready(&project, a1), "copy-1");
        assert_eq!(bisect.ready(&project, a2), "copy-1");
        assert_eq!(lines(&counter), 1);
        let copy_1 = project.cache.path().join("copy-1");
        assert_eq!(fs::read_to_string(copy_1.join("index.js")).unwrap(), "2");
        // `--setup` replaces the command of the lockfile.
        let log = fs::read_to_string(bisect.logs.path().join("install-1.log")).unwrap();
        assert!(log.contains(&append_to(&counter)), "{log}");
        assert!(!log.contains("npm ci"), "{log}");

        assert_eq!(bisect.ready(&project, b), "copy-2");
        assert_eq!(bisect.ready(&project, c), "copy-3");
        assert_eq!(lines(&counter), 3);
        // `a` was used least recently.
        assert_eq!(bisect.ready(&project, d), "copy-1");
        assert_eq!(lines(&counter), 4);
        assert_eq!(bisect.ready(&project, b), "copy-2");
        assert_eq!(lines(&counter), 4);
        // Now `c` was.
        assert_eq!(bisect.ready(&project, a1), "copy-3");
        assert_eq!(lines(&counter), 5);
        drop(bisect);

        // The next bisect finds the installed copies in the event log.
        let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
        assert_eq!(bisect.ready(&project, b), "copy-2");
        assert_eq!(bisect.ready(&project, d), "copy-1");
        assert_eq!(bisect.ready(&project, a2), "copy-3");
        assert_eq!(lines(&counter), 5);
        let recorded = bisect.events.meta(COPIES_KEY).unwrap().unwrap();
        let book = Book::parse(&recorded, project.cache.instance()).unwrap();
        let mut numbers: Vec<usize> = book.copies.iter().map(|copy| copy.number).collect();
        numbers.sort_unstable();
        assert_eq!(numbers, [1, 2, 3]);
        assert!(book.copies.iter().all(|copy| copy.state == State::Done));
        drop(bisect);

        // Another setup command installs again.
        let twice = format!("{} && {}", append_to(&counter), append_to(&counter));
        let mut bisect = Bisect::new(&project, Some(twice));
        bisect.ready(&project, b);
        assert_eq!(lines(&counter), 7);
    }

    #[test]
    fn a_failed_install_makes_its_set_cannot_test_for_the_rest_of_the_bisect() {
        let project = Project::new();
        let counter_dir = tempfile::tempdir().unwrap();
        let counter = counter_dir.path().join("counter");
        let setup = format!("{} && exit 1", append_to(&counter));
        let step =
            |set: &str, code: &str| project.tree(&[("package-lock.json", set), ("index.js", code)]);

        let mut bisect = Bisect::new(&project, Some(setup.clone()));
        let failure = bisect.cannot_test(&project, step("a", "1"));
        let (command, dir, outcome, log, registry_error) = failed(&failure);
        assert_eq!(
            (command, dir, outcome),
            (setup.as_str(), "", Outcome::Failed(Exit::Code(1)))
        );
        assert!(log.is_file());
        assert!(!registry_error);
        let message = failure.to_string();
        assert!(message.contains("ended with exit code 1"), "{message}");
        assert!(message.contains(&log.display().to_string()), "{message}");
        assert!(!message.contains("--with"), "{message}");

        // Another step of the same set is not installed again.
        assert_eq!(bisect.cannot_test(&project, step("a", "2")), failure);
        assert_eq!(lines(&counter), 1);
        // Another set is.
        bisect.cannot_test(&project, step("b", "1"));
        assert_eq!(lines(&counter), 2);
        drop(bisect);

        // The next bisect tries again.
        let mut bisect = Bisect::new(&project, Some(setup));
        bisect.cannot_test(&project, step("a", "1"));
        assert_eq!(lines(&counter), 3);
    }

    #[test]
    fn a_registry_or_login_error_suggests_bringing_in_the_ignored_configuration() {
        for output in [
            "npm error code E401\nnpm error Unable to authenticate, need: Basic realm=\"GitHub\"",
            "npm error code E404\nnpm error 404 Not Found - GET https://registry.npmjs.org/@me%2fx",
            "error: failed to query replaced source registry `crates-io`",
            "error: no token found for `my-registry`, please run `cargo login`",
            "ERROR: HTTP error 401 while getting https://pypi.example.com/simple/private/",
            "ERROR: Could not find a version that satisfies the requirement private-pkg",
        ] {
            assert!(is_registry_error(output.as_bytes()), "{output}");
        }
        for output in [
            "npm error code ELIFECYCLE\nnpm error command failed\nnpm error command sh -c node x.js",
            "error[E0425]: cannot find value `x` in this scope",
            "SyntaxError: Unexpected token '}' at line 4010",
        ] {
            assert!(!is_registry_error(output.as_bytes()), "{output}");
        }

        let project = Project::new();
        let setup = "echo npm error code E401&& exit 1".to_owned();
        let mut bisect = Bisect::new(&project, Some(setup));
        let failure = bisect.cannot_test(&project, project.tree(&[("package-lock.json", "")]));
        assert!(failed(&failure).4);
        let message = failure.to_string();
        assert!(
            message.contains("add that file with --with <path>"),
            "{message}"
        );
    }

    #[test]
    fn an_evicted_copy_goes_with_its_cargo_and_bytecode_folders_never_following_links() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep", "canary");
        let before = state(canary.path());
        let step = |set: &str| project.tree(&[("package-lock.json", set), ("index.js", "")]);

        let mut bisect = Bisect::new(&project, Some("exit 0".to_owned()));
        for (set, copy) in [("a", "copy-1"), ("b", "copy-2"), ("c", "copy-3")] {
            assert_eq!(bisect.ready(&project, step(set)), copy);
        }
        let root = project.cache.path();
        for folder in [CARGO_TARGET_DIR, PYCACHE_DIR] {
            let path = root.join(folder).join("copy-1");
            write(&path, "debug/build", "");
            make_link(canary.path(), &path.join("link")).unwrap();
        }
        // What the install of `a` left in the copy, kept by every switch.
        write(&root.join("copy-1"), "node_modules/a/index.js", "");
        let kept = state(&root.join("copy-2"));

        assert_eq!(bisect.ready(&project, step("d")), "copy-1");
        for folder in [CARGO_TARGET_DIR, PYCACHE_DIR] {
            let path = root.join(folder).join("copy-1");
            assert!(!path.join("debug").exists(), "{}", path.display());
            assert!(
                fs::symlink_metadata(path.join("link")).is_err(),
                "{}",
                path.display()
            );
        }
        assert!(!root.join("copy-1").join("node_modules").exists());
        assert_eq!(state(&root.join("copy-2")), kept);
        assert_eq!(state(canary.path()), before);
    }

    #[test]
    fn installs_count_only_in_the_cache_folder_they_were_made_in() {
        let project = Project::new();
        let base = tempfile::tempdir().unwrap();
        let folder = base.path().join("yalper");
        let counter = base.path().join("counter");
        let tree = project.tree(&[("package-lock.json", "a")]);
        let bisect = || {
            let cache = CacheDir::open_in(&folder, project.path(), &token()).unwrap();
            let mut bisect = Bisect::in_cache(&project, &cache, Some(append_to(&counter)));
            assert_eq!(bisect.ready(&project, tree), "copy-1");
            (cache.path().to_owned(), cache.instance().to_owned())
        };

        let (path, instance) = bisect();
        bisect();
        assert_eq!(lines(&counter), 1);

        // A recorded copy whose folder is gone holds no install.
        fs::remove_dir_all(path.join("copy-1")).unwrap();
        bisect();
        assert_eq!(lines(&counter), 2);

        // A cache folder deleted and made again has a new instance id: what the event log records about the
        // old one does not count.
        fs::remove_dir_all(&folder).unwrap();
        let (_, new_instance) = bisect();
        assert_ne!(new_instance, instance);
        assert_eq!(lines(&counter), 3);
    }

    #[test]
    fn a_step_with_the_projects_dependencies_runs_in_copy_0_without_an_install() {
        let project = Project::new();
        let counter_dir = tempfile::tempdir().unwrap();
        let counter = counter_dir.path().join("counter");
        let package = |test: &str| {
            format!(r#"{{"scripts": {{"test": "{test}"}}, "dependencies": {{"dep": "1"}}}}"#)
        };
        project.write(".gitignore", "node_modules/\n");
        project.write("package.json", &package("node --test"));
        project.write("package-lock.json", "1");
        project.write("node_modules/dep/index.js", "");
        let step = |test: &str, lock: &str| {
            project.tree(&[
                (".gitignore", "node_modules/\n"),
                ("package.json", &package(test)),
                ("package-lock.json", lock),
                ("index.js", ""),
            ])
        };

        let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
        // Only a script differs from the project.
        assert_eq!(bisect.ready(&project, step("vitest", "1")), "copy-0");
        let copy_0 = project.cache.path().join("copy-0");
        let link = copy_0.join("node_modules").join("dep");
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        // Another lockfile.
        assert_eq!(bisect.ready(&project, step("vitest", "2")), "copy-1");
        assert_eq!(bisect.ready(&project, step("node --test", "1")), "copy-0");
        assert_eq!(lines(&counter), 1);
        drop(bisect);

        // Without the project's installed packages, the step gets an install of its own.
        fs::remove_dir_all(project.path().join("node_modules")).unwrap();
        let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
        assert_eq!(bisect.ready(&project, step("vitest", "1")), "copy-2");
        assert_eq!(lines(&counter), 2);
    }

    /// A project with installed Node packages in `web/` and a Python environment in `api/`, both ignored, and
    /// the tree of a step with the same dependency files, which runs in `copy-0`.
    fn with_installed() -> (Project, ObjectId) {
        let project = Project::new();
        let files = [
            (".gitignore", "node_modules/\n.venv/\n"),
            ("web/package.json", r#"{"dependencies": {"dep": "1"}}"#),
            ("web/package-lock.json", "1"),
            ("api/pyproject.toml", "[project]\nname = \"api\"\n"),
            ("api/uv.lock", "1"),
        ];
        for (path, content) in files {
            project.write(path, content);
        }
        project.write("web/node_modules/dep/index.js", "");
        project.write("api/.venv/pyvenv.cfg", "home = /usr/bin\n");
        let mut step = files.to_vec();
        step.push(("index.js", ""));
        let tree = project.tree(&step);
        (project, tree)
    }

    /// What [`PROJECT_LINKS`] lists for the copy at `copy`.
    fn listed_for(copy: &Path) -> BTreeSet<(PathBuf, String)> {
        let canonical = plain(&fs::canonicalize(copy).unwrap());
        listed_links()
            .iter()
            .filter(|(root, _)| *root == canonical)
            .cloned()
            .collect()
    }

    fn paths(listed: &BTreeSet<(PathBuf, String)>) -> Vec<&str> {
        listed.iter().map(|(_, path)| path.as_str()).collect()
    }

    #[test]
    fn copy_0_loses_its_links_into_the_project_when_the_bisect_ends_and_on_a_second_ctrl_c() {
        let (project, step) = with_installed();
        let before = state(project.path());
        let copy_0 = project.cache.path().join("copy-0");
        let mut bisect = Bisect::new(&project, None);
        assert_eq!(bisect.ready(&project, step), "copy-0");
        assert_eq!(links_under(&copy_0).len(), 2);
        assert_eq!(
            paths(&listed_for(&copy_0)),
            ["api/.venv", "web/node_modules"]
        );

        // The end of the bisect.
        assert_eq!(bisect.sets.remove_project_links(), None);
        assert_eq!(links_under(&copy_0), Vec::<PathBuf>::new());
        assert!(copy_0.join("web/package.json").is_file());
        assert!(listed_for(&copy_0).is_empty());
        assert_eq!(state(project.path()), before);
        assert_eq!(bisect.sets.remove_project_links(), None);

        // Dropped without that call (an error, a panic): the same.
        assert_eq!(bisect.ready(&project, step), "copy-0");
        assert_eq!(links_under(&copy_0).len(), 2);
        drop(bisect);
        assert_eq!(links_under(&copy_0), Vec::<PathBuf>::new());
        assert!(listed_for(&copy_0).is_empty());
        assert_eq!(state(project.path()), before);

        // A second Ctrl-C removes what is listed, through the copy only.
        let mut bisect = Bisect::new(&project, None);
        assert_eq!(bisect.ready(&project, step), "copy-0");
        let mut listed = listed_for(&copy_0);
        assert_eq!(listed.len(), 2);
        remove_listed(&mut listed);
        assert!(listed.is_empty());
        assert_eq!(links_under(&copy_0), Vec::<PathBuf>::new());
        assert_eq!(state(project.path()), before);
        drop(bisect);
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn links_a_bisect_that_did_not_end_left_in_copy_0_are_removed_by_the_next_one() {
        let (project, step) = with_installed();
        let before = state(project.path());
        let copy_0 = project.cache.path().join("copy-0");
        let leave_links = || {
            let mut bisect = Bisect::new(&project, None);
            assert_eq!(bisect.ready(&project, step), "copy-0");
            // Yalper ends without removing them: killed, or crashed.
            bisect.sets.leak_project_links();
            drop(bisect);
            assert_eq!(links_under(&copy_0).len(), 2);
        };

        // The next bisect's first step in `copy-0` has neither `web/` nor `api/`: its switch removes the link to
        // the environment, and its first link removes the farm, which the switch keeps as ignored.
        leave_links();
        let mut bisect = Bisect::new(&project, None);
        let other = project.tree(&[(".gitignore", "node_modules/\n.venv/\n"), ("index.js", "")]);
        assert_eq!(bisect.ready(&project, other), "copy-0");
        assert_eq!(links_under(&copy_0), Vec::<PathBuf>::new());
        assert!(!copy_0.join("web").exists());
        assert!(listed_for(&copy_0).is_empty());
        drop(bisect);
        assert_eq!(state(project.path()), before);

        // A bisect that never uses `copy-0` removes them at its start. A link out of the project is not looked
        // at.
        leave_links();
        let outside = tempfile::tempdir().unwrap();
        let out_link = copy_0.join("tools").join("node_modules").join("out");
        fs::create_dir_all(out_link.parent().unwrap()).unwrap();
        make_link(outside.path(), &out_link).unwrap();
        assert_eq!(remove_left_links(&project.cache), None);
        assert_eq!(links_under(&copy_0), std::slice::from_ref(&out_link));
        assert!(copy_0.join("web/package.json").is_file());
        let mut bisect = Bisect::new(&project, Some("exit 0".to_owned()));
        let other_lockfile = project.tree(&[
            (".gitignore", "node_modules/\n.venv/\n"),
            ("web/package.json", r#"{"dependencies": {"dep": "1"}}"#),
            ("web/package-lock.json", "2"),
            ("index.js", ""),
        ]);
        assert_eq!(bisect.ready(&project, other_lockfile), "copy-1");
        drop(bisect);
        assert_eq!(links_under(&copy_0), [out_link]);
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn a_python_step_runs_in_copy_0_when_the_project_has_its_environment() {
        let project = Project::new();
        let pyproject = "[project]\nname = \"app\"\ndependencies = [\"requests\"]\n";
        project.write(".gitignore", "venv/\n");
        project.write("pyproject.toml", pyproject);
        project.write("uv.lock", "1");
        let step = project.tree(&[
            ("pyproject.toml", pyproject),
            ("uv.lock", "1"),
            ("app.py", ""),
        ]);

        let mut bisect = Bisect::new(&project, Some("exit 0".to_owned()));
        assert_eq!(bisect.ready(&project, step), "copy-1");
        drop(bisect);

        project.write("venv/pyvenv.cfg", "home = /usr/bin\n");
        let mut bisect = Bisect::new(&project, Some("exit 0".to_owned()));
        assert_eq!(bisect.ready(&project, step), "copy-0");
        let link = project.cache.path().join("copy-0").join("venv");
        assert!(fs::symlink_metadata(link).unwrap().is_symlink());
    }

    fn value<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
        env.iter()
            .find(|(var, _)| same_name(var, name))
            .map(|(_, value)| value.as_os_str())
    }

    #[test]
    fn a_copy_with_its_own_install_never_uses_an_environment_the_user_activated() {
        let project = Project::new();
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().as_os_str();
        let activated = ["VIRTUAL_ENV", "CONDA_PREFIX", "UV_PROJECT_ENVIRONMENT"];
        let mut user = environment();
        user.retain(|(name, _)| {
            let poetry = name.to_string_lossy().to_uppercase().starts_with("POETRY_");
            !poetry && !activated.iter().any(|var| same_name(name, var))
        });
        user.extend(activated.map(|var| (OsString::from(var), outside.to_owned())));
        user.push(("POETRY_VIRTUALENVS_CREATE".into(), "false".into()));

        let mut bisect = Bisect::new(&project, Some("exit 0".to_owned()));
        let own = project.tree(&[("poetry.lock", ""), ("pyproject.toml", "")]);
        let (copy, set_copy) = ready(bisect.prepare(&project, own));
        assert_eq!(copy, "copy-1");
        let env = set_copy.run_env(user.clone()).unwrap();
        for var in activated {
            assert_eq!(value(&env, var), None, "{var}");
        }
        let yes = Some(OsStr::new("true"));
        assert_eq!(value(&env, "POETRY_VIRTUALENVS_CREATE"), yes);
        assert_eq!(value(&env, "POETRY_VIRTUALENVS_IN_PROJECT"), yes);

        // `copy-0` runs with the user's setup.
        let (copy, set_copy) = ready(bisect.prepare(&project, project.tree(&[("README.md", "")])));
        assert_eq!(copy, "copy-0");
        let env = set_copy.run_env(user).unwrap();
        for var in activated {
            assert_eq!(value(&env, var), Some(outside), "{var}");
        }
        assert_eq!(
            value(&env, "POETRY_VIRTUALENVS_CREATE"),
            Some(OsStr::new("false"))
        );
        assert_eq!(value(&env, "POETRY_VIRTUALENVS_IN_PROJECT"), None);
    }

    /// Runs `command` in `set_copy` as a bisect does, after cleaning the root of the cache folder: whether it
    /// passed. Any outcome but a pass or a plain failure fails the test, with the run's output.
    fn passes(set_copy: &SetCopy, logs: &Logs, command: &str) -> bool {
        let cache = set_copy.copy().cache();
        clean_root(cache).unwrap();
        write_boundaries(cache).unwrap();
        let env = set_copy.run_env(environment()).unwrap();
        let log = logs.create("run.log").unwrap();
        let stop = AtomicBool::new(false);
        let path = set_copy.copy().path();
        let outcome = run(command, path, &env, &log, None, &stop).unwrap().outcome;
        let output = fs::read_to_string(logs.path().join("run.log")).unwrap();
        match outcome {
            Outcome::Passed => true,
            Outcome::Failed(Exit::Code(1)) => false,
            other => panic!("{other:?}: {output}"),
        }
    }

    #[test]
    fn npm_ci_fills_a_copy_whose_packages_stay_through_the_next_switches() {
        if tool(&["npm.cmd", "npm"]).is_none() || tool(&["node"]).is_none() {
            return;
        }
        let project = Project::new();
        let lock = r#"{"name": "app", "lockfileVersion": 3, "requires": true, "packages": {
            "": {"name": "app", "dependencies": {"dep": "file:dep"}},
            "dep": {"name": "dep", "version": "1.0.0"},
            "node_modules/dep": {"resolved": "dep", "link": true}}}"#;
        // No `.gitignore`: the switches keep `node_modules` because the install fills it.
        let step = |answer: &str| {
            let test = format!("process.exit(require('dep') === {answer} ? 0 : 1);\n");
            project.tree(&[
                (
                    "package.json",
                    r#"{"name": "app", "dependencies": {"dep": "file:dep"}}"#,
                ),
                ("package-lock.json", lock),
                ("dep/package.json", r#"{"name": "dep", "version": "1.0.0"}"#),
                ("dep/index.js", "module.exports = 42;\n"),
                ("test.js", &test),
            ])
        };

        let mut bisect = Bisect::new(&project, None);
        let stop = AtomicBool::new(false);
        for (answer, expected) in [("42", true), ("41", false), ("42", true)] {
            let (events, logs) = (&bisect.events, &bisect.logs);
            let prepared = bisect
                .sets
                .prepare(events, &project.store, step(answer), logs, &stop);
            let (copy, set_copy) = ready(prepared.unwrap());
            assert_eq!(copy, "copy-1");
            assert_eq!(passes(set_copy, logs, "node test.js"), expected);
        }
        assert!(bisect.logs.path().join("install-1.log").is_file());
        assert!(!bisect.logs.path().join("install-2.log").exists());
        assert!(!project.path().join("node_modules").exists());
    }

    #[test]
    fn pip_installs_the_requirements_into_an_environment_of_the_copy() {
        // The interpreter the install command starts.
        let Some(python) = tool(if cfg!(windows) {
            &["python"]
        } else {
            &["python3"]
        }) else {
            return;
        };
        let has_venv = Command::new(python)
            .args(["-c", "import venv, ensurepip"])
            .output()
            .is_ok_and(|output| output.status.success());
        if !has_venv {
            return;
        }
        let project = Project::new();
        let step = |code: &str| project.tree(&[("requirements.txt", ""), ("test.py", code)]);
        let python = venv_python(Path::new(".venv"));
        let command = format!("{} test.py", python.display());

        let mut bisect = Bisect::new(&project, None);
        let stop = AtomicBool::new(false);
        for code in ["import sys\n", "import os\n"] {
            let (events, logs) = (&bisect.events, &bisect.logs);
            let prepared = bisect
                .sets
                .prepare(events, &project.store, step(code), logs, &stop);
            let (copy, set_copy) = ready(prepared.unwrap());
            assert_eq!(copy, "copy-1");
            assert!(
                set_copy
                    .copy()
                    .path()
                    .join(".venv")
                    .join("pyvenv.cfg")
                    .is_file()
            );
            assert!(passes(set_copy, logs, &command));
        }
        assert!(!bisect.logs.path().join("install-2.log").exists());
    }

    #[test]
    fn installs_may_fill_the_folders_next_to_lockfiles_and_manifests() {
        let files = [
            "package.json",
            "web/package-lock.json",
            "api/pyproject.toml",
            "tools/setup.py",
            "docs/requirements-docs.txt",
            "Cargo.lock",
        ];
        let installs = installs(files);
        assert_eq!(
            install_folders(files, &installs),
            [
                "api/.venv",
                "api/venv",
                "docs/.venv",
                "docs/venv",
                "node_modules",
                "tools/.venv",
                "tools/venv",
                "web/node_modules",
            ]
        );
    }

    #[test]
    fn a_link_in_the_step_where_an_install_writes_stops_the_install() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep", "canary");
        let before = state(canary.path());
        let counter = canary.path().join("counter");
        let link = FileKind::Symlink;
        let regular = FileKind::Regular;

        // Links to the canary, and links to a folder of the step itself.
        for target in [canary.path().to_str().unwrap(), "inside"] {
            // The commands of the lockfiles (npm first), and a `--setup` command for manifests without a
            // lockfile (which may write in every install folder).
            let with_lockfiles = project.typed_tree(&[
                ("package-lock.json", regular, "{}"),
                ("requirements.txt", regular, ""),
                ("inside/x", regular, ""),
                (".venv", link, target),
                ("node_modules", link, target),
            ]);
            let with_manifests = project.typed_tree(&[
                ("package.json", regular, "{}"),
                ("pyproject.toml", regular, ""),
                ("inside/x", regular, ""),
                (".venv", link, target),
                ("node_modules", link, target),
            ]);
            for (setup, tree, first) in [
                (None, with_lockfiles, "node_modules"),
                (Some(append_to(&counter)), with_manifests, ".venv"),
            ] {
                let mut bisect = Bisect::new(&project, setup);
                let failure = bisect.cannot_test(&project, tree);
                let expected = if cfg!(unix) && target != "inside" {
                    InstallFailure::LinkOut {
                        path: ".venv".to_owned(),
                    }
                } else {
                    // On Windows the copy holds tree links as plain files: refused as well.
                    InstallFailure::Blocked {
                        path: first.to_owned(),
                        link: cfg!(unix),
                    }
                };
                assert_eq!(failure, expected, "{target}");
            }
        }
        let message = InstallFailure::Blocked {
            path: "api/.venv".to_owned(),
            link: true,
        }
        .to_string();
        assert!(message.contains("a link at api/.venv"), "{message}");
        assert!(
            message.contains("Yalper does not install through it"),
            "{message}"
        );
        assert_eq!(lines(&counter), 0);
        assert_eq!(state(canary.path()), before);
    }

    #[test]
    fn lockfiles_the_project_never_installed_do_not_decide_the_copy() {
        let project = Project::new();
        let package = r#"{"dependencies": {"dep": "1"}}"#;
        let files = [
            (".gitignore", "node_modules/\n"),
            ("package.json", package),
            ("package-lock.json", "1"),
            ("docs/requirements.txt", "sphinx\n"),
            ("examples/app/package.json", package),
            ("examples/app/package-lock.json", "1"),
        ];
        for (path, content) in files {
            project.write(path, content);
        }
        project.write("node_modules/dep/index.js", "");

        let mut bisect = Bisect::new(&project, None);
        // The same dependency files: `copy-0`, although the docs and the example were never installed.
        assert_eq!(bisect.ready(&project, project.tree(&files)), "copy-0");

        // Another lockfile at the top: only it is installed, where the project has installed packages.
        let mut changed = files;
        changed[2] = ("package-lock.json", "2");
        let deps = read_deps(&project.store, project.tree(&changed), &mut HashMap::new()).unwrap();
        // Python has no installed folder in the project: all of its lockfiles are installed.
        let commands = bisect.sets.own_installs(&deps).unwrap();
        assert_eq!(dirs(&commands), ["", "docs"]);
        assert_eq!(commands[0].command, "npm ci");
        assert_eq!(commands[0].folders, ["node_modules"]);
        assert_eq!(commands[1].folders, ["docs/.venv", "docs/venv"]);
        drop(bisect);

        // A project that installed nothing: every lockfile folder.
        fs::remove_dir_all(project.path().join("node_modules")).unwrap();
        let mut bisect = Bisect::new(&project, None);
        let commands = bisect.sets.own_installs(&deps).unwrap();
        assert_eq!(dirs(&commands), ["", "docs", "examples/app"]);
    }

    /// The folders of `commands`.
    fn dirs(commands: &[Planned]) -> Vec<&str> {
        commands
            .iter()
            .map(|planned| planned.dir.as_str())
            .collect()
    }

    #[test]
    fn each_ecosystem_installs_where_the_project_installed_it_or_everywhere() {
        let project = Project::new();
        let files = [
            (".gitignore", "node_modules/\ntarget/\n"),
            ("package.json", "{}"),
            ("package-lock.json", "1"),
            ("examples/app/package-lock.json", "1"),
            ("pyproject.toml", "[project]\nname = \"app\"\n"),
            ("uv.lock", "1"),
            ("Cargo.lock", "1"),
            ("examples/tool/Cargo.lock", "1"),
        ];
        for (path, content) in files {
            project.write(path, content);
        }
        // Node packages and a Rust build, but no Python environment.
        project.write("node_modules/dep/index.js", "");
        fs::create_dir_all(project.path().join("examples/tool/target")).unwrap();

        let mut changed = files;
        changed[5] = ("uv.lock", "2");
        let deps = read_deps(&project.store, project.tree(&changed), &mut HashMap::new()).unwrap();
        let mut bisect = Bisect::new(&project, None);
        let commands = bisect.sets.own_installs(&deps).unwrap();
        let plan: Vec<(&str, &str)> = commands
            .iter()
            .map(|planned| (planned.dir.as_str(), planned.command.as_str()))
            .collect();
        // Node where the project installed it; Python everywhere; Rust where the project built it and, always,
        // for the lockfile at the top.
        assert_eq!(
            plan,
            [
                ("", "npm ci"),
                ("", "uv sync --frozen"),
                ("", "cargo fetch"),
                ("examples/tool", "cargo fetch"),
            ]
        );

        // Without a lockfile at the top, Rust follows the same rule as the others.
        let crates = [("a/Cargo.lock", "1"), ("b/Cargo.lock", "1")];
        let deps = read_deps(&project.store, project.tree(&crates), &mut HashMap::new()).unwrap();
        let built: Vec<String> = dirs(&bisect.sets.own_installs(&deps).unwrap())
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(built, ["a", "b"]);
        fs::create_dir_all(project.path().join("a/target")).unwrap();
        let mut bisect = Bisect::new(&project, None);
        assert_eq!(dirs(&bisect.sets.own_installs(&deps).unwrap()), ["a"]);
    }

    #[test]
    fn a_harmless_link_elsewhere_does_not_block_the_install_of_other_folders() {
        let project = Project::new();
        let regular = FileKind::Regular;
        // A committed `docs/venv` link inside the copy, next to a Python manifest that nothing installs.
        let tree = project.typed_tree(&[
            ("Cargo.lock", regular, ""),
            ("docs/requirements-docs.txt", regular, "sphinx\n"),
            ("docs/venv", FileKind::Symlink, "../src"),
            ("src/lib.rs", regular, ""),
        ]);
        // Whether `cargo fetch` itself succeeds here does not matter: the link must not stop it.
        let mut bisect = Bisect::new(&project, None);
        if let Prepared::CannotTest(failure) = bisect.prepare(&project, tree) {
            assert!(
                !matches!(failure, InstallFailure::Blocked { .. }),
                "{failure}"
            );
        }
        drop(bisect);

        // A `--setup` command may write anywhere an install does: there the link blocks it.
        let mut bisect = Bisect::new(&project, Some("exit 0".to_owned()));
        let failure = bisect.cannot_test(&project, tree);
        let expected = InstallFailure::Blocked {
            path: "docs/venv".to_owned(),
            link: cfg!(unix),
        };
        assert_eq!(failure, expected);
    }

    #[test]
    fn links_that_leave_the_tree_are_found() {
        // The paths of the links of the tree (`r -> .` among them).
        let links: HashSet<&str> = ["r", "lib", "x", "y"].into_iter().collect();
        for (path, target) in [
            ("packages/lib", "/home/me/lib"),
            ("vendor/x", "../../outside"),
            ("a", ".."),
            ("a/b", "../c/../../x"),
            // Chained links: `r` resolves to the top, so `r/r/../..` climbs above it.
            ("x", "r/r/../.."),
            ("y", "./lib/package"),
        ] {
            assert!(
                leaves_tree(path, target.as_bytes(), &links),
                "{path} -> {target}"
            );
        }
        for (path, target) in [
            ("docs/venv", "../src"),
            ("a", "b"),
            ("a/b/c", "../../d/./e"),
            ("a", "./b/"),
            ("r", "."),
            // A link to another link, as a whole, is judged by that link's own target.
            ("z", "lib"),
        ] {
            assert!(
                !leaves_tree(path, target.as_bytes(), &links),
                "{path} -> {target}"
            );
        }
    }

    #[test]
    fn a_step_with_nothing_to_install_is_ready_whatever_its_links() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        // A manifest the project does not have, without a lockfile: a copy of its own, with nothing to install.
        let tree = project.typed_tree(&[
            (
                "package.json",
                FileKind::Regular,
                r#"{"dependencies": {"a": "1"}}"#,
            ),
            ("lib", FileKind::Symlink, canary.path().to_str().unwrap()),
        ]);
        let mut bisect = Bisect::new(&project, None);
        assert_eq!(bisect.ready(&project, tree), "copy-1");
        assert!(!bisect.logs.path().join("install-1.log").exists());
    }

    #[test]
    fn on_unix_no_install_runs_while_a_link_points_outside_the_copy() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        let counter = canary.path().join("counter");
        let regular = FileKind::Regular;
        let absolute = canary.path().to_str().unwrap();
        for (path, target) in [("packages/lib", absolute), ("vendor/x", "../../outside")] {
            let tree = project.typed_tree(&[
                ("package-lock.json", regular, path),
                (path, FileKind::Symlink, target),
            ]);
            let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
            if cfg!(unix) {
                let failure = bisect.cannot_test(&project, tree);
                let expected = InstallFailure::LinkOut {
                    path: path.to_owned(),
                };
                assert_eq!(failure, expected);
                let message = failure.to_string();
                assert!(
                    message.contains("points outside the test copy"),
                    "{message}"
                );
            } else {
                // Windows writes the link as a plain file.
                bisect.ready(&project, tree);
            }
        }
        assert_eq!(lines(&counter), if cfg!(unix) { 0 } else { 2 });

        // A link inside the copy does not stop the install.
        let tree = project.typed_tree(&[
            ("package-lock.json", regular, "inside"),
            ("a", FileKind::Symlink, "b"),
        ]);
        let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
        bisect.ready(&project, tree);
    }

    #[test]
    fn an_interrupted_install_does_not_count() {
        let project = Project::new();
        let counter_dir = tempfile::tempdir().unwrap();
        let counter = counter_dir.path().join("counter");
        let tree = project.tree(&[("package-lock.json", "a")]);

        let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
        assert_eq!(bisect.ready(&project, tree), "copy-1");
        // As if the install had been stopped before it ended.
        let recorded = bisect.events.meta(COPIES_KEY).unwrap().unwrap();
        let started = recorded.replace("\"ok\"", "\"started\"");
        assert_ne!(started, recorded);
        bisect.events.set_meta(COPIES_KEY, Some(&started)).unwrap();
        drop(bisect);

        let mut bisect = Bisect::new(&project, Some(append_to(&counter)));
        assert_eq!(bisect.ready(&project, tree), "copy-1");
        assert_eq!(lines(&counter), 2);
    }

    #[test]
    fn what_an_install_changes_in_the_steps_files_is_undone() {
        let project = Project::new();
        let tree = project.tree(&[("package-lock.json", "a"), ("index.js", "1")]);
        let mut bisect = Bisect::new(&project, Some("echo changed> index.js".to_owned()));
        assert_eq!(bisect.ready(&project, tree), "copy-1");
        let index = project.cache.path().join("copy-1").join("index.js");
        assert_eq!(fs::read_to_string(index).unwrap(), "1");
    }

    #[test]
    fn a_setup_without_lockfiles_keeps_the_folders_it_fills() {
        let project = Project::new();
        let setup = if cfg!(windows) {
            "mkdir node_modules && mkdir .venv && echo x> node_modules\\x && echo y> .venv\\y"
        } else {
            "mkdir -p node_modules .venv && echo x > node_modules/x && echo y > .venv/y"
        };
        // No `.gitignore`: nothing ignores the folders.
        let step = |code: &str| {
            project.tree(&[
                ("package.json", "{}"),
                ("pyproject.toml", ""),
                ("index.js", code),
            ])
        };
        let mut bisect = Bisect::new(&project, Some(setup.to_owned()));
        for code in ["1", "2"] {
            assert_eq!(bisect.ready(&project, step(code)), "copy-1");
            let copy = project.cache.path().join("copy-1");
            assert!(copy.join("node_modules").join("x").is_file());
            assert!(copy.join(".venv").join("y").is_file());
        }
    }
}
