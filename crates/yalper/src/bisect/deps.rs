//! Dependency sets: which test copy runs the tests of a step, and the installs that fill the copies.
//!
//! What a step needs installed is given by its dependency files, in any folder of its tree: every lockfile,
//! `requirements*.txt`, the package manager settings `pnpm-workspace.yaml`, `.npmrc` and `.yarnrc.yml`, and only
//! the dependency sections of `package.json` and `pyproject.toml` (see [`PACKAGE_JSON_SECTIONS`] and
//! [`PYPROJECT_SECTIONS`]), so that an agent editing a script or a tool setting does not cost an install. Files
//! inside a `node_modules` folder do not count: packages committed with the code are code. Steps whose
//! dependency files are the same have the same key and form one dependency set.
//!
//! - When every dependency file of the step is the same in the project (read from its working tree: regular
//!   files reached through real folders only) and the project has the folders the step's installs fill
//!   (`node_modules`, `.venv` or `venv`), the step runs in `copy-0`, which links to the project's installed
//!   dependencies (see [`NodeModules`] and [`PythonEnvs`]): no install. Where those links cannot be made
//!   ([`CANNOT_LINK`]), `copy-0` is not used for the rest of the bisect.
//! - Otherwise the step runs in the copy of its set, one of `copy-1` to `copy-3`, filled with the step's code and
//!   installed once: with the command of each lockfile (see [`Tool`]) in its folder, or with the `--setup`
//!   command at the top of the copy. The copy is kept for the next steps and bisects of the set. When a set
//!   needs a copy and all three hold an install, the least recently used one is deleted, with its Cargo target
//!   and Python bytecode folders. Which copy holds which set is recorded in the event log (see [`Book`]).
//! - An install that fails makes every step of its set "cannot test" for the rest of the bisect.
//!
//! Installs get the protections of test runs: the root of the project's cache folder is cleaned and its
//! boundary files written before each one, and they run with the environment of [`run_env`], through [`run`],
//! with no time limit and their output in a log. They run the step's package scripts (`postinstall`, build
//! backends): a bisect runs the code of every step anyway.
//!
//! Known limits:
//!
//! - Installs run in the copy, outside the project, so configuration the project ignores (an ignored `.npmrc`
//!   with a private registry, an ignored `.cargo/config.toml`) does not apply to them; when an install fails
//!   with what looks like a registry or login error, [`InstallFailure`] suggests bringing the file in.
//! - Packages that take their version from git cannot find a repository in a copy (it is outside the project,
//!   and runs and installs never search above it). An install that builds a Python project versioned by
//!   setuptools-scm or hatch-vcs (`uv sync`, `poetry install`) fails with "LookupError: setuptools-scm was
//!   unable to detect version", so the steps of that set cannot be tested, unless the user's environment sets
//!   `SETUPTOOLS_SCM_PRETEND_VERSION`. Steps that run in `copy-0` are not affected: the project's environment is
//!   already built. A Rust build script that reads git (vergen) runs in the test runs of every copy: depending
//!   on its settings it fails every step, so the bisect reports that the good end fails, or it builds with
//!   placeholder values.
//! - A step that lists dependencies in a manifest without a lockfile installs nothing, unless `--setup` is given.
//! - What an install puts in a copy is kept only in the folders it fills next to each lockfile (`node_modules`,
//!   `.venv`) and in ignored files; Yarn Plug'n'Play files (`.pnp.cjs`, `.yarn/`) only when they are ignored. A
//!   test that deletes or changes those folders in a copy with its own install changes them for the next runs
//!   in that copy.

use std::collections::btree_map;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

use gix::ObjectId;
use serde_json::{Value, json};

use super::copy::{CARGO_TARGET_DIR, PYCACHE_DIR, join, native, parents, remove_entry};
use super::env::same_name;
use super::link::{CANNOT_LINK, is_real_dir_no_follow};
use super::run::{Logs, Outcome, SKIP_EXIT_CODE, run};
use super::{
    CacheDir, NodeModules, PythonEnvs, TestCopy, clean_root, env_additions, run_env,
    write_boundaries,
};
use crate::snapshot::{Error, FileKind, MAX_FILE_BYTES, Result, ShadowStore, validate_path};
use crate::store::Store;

/// The `meta` key of the event log that records the copies with their own install: see [`Book`].
pub const COPIES_KEY: &str = "bisect.copies";

/// The most copies with their own install, `copy-1` to `copy-3`, kept in the project's cache folder.
const MAX_OWN_COPIES: usize = 3;

/// The most entries of a tree listed to find its dependency files, as for a switch of a test copy.
const MAX_TREE_ENTRIES: usize = 1_000_000;

/// The most dependency files a step may have: far more than a large monorepo has, while a crafted tree with
/// millions of manifests is refused instead of read.
const MAX_DEPENDENCY_FILES: usize = 10_000;

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
const SETTINGS_FILES: [&str; 3] = ["pnpm-workspace.yaml", ".npmrc", ".yarnrc.yml"];

/// The fields of `package.json` that count. npm accepts both spellings of bundled dependencies.
const PACKAGE_JSON_SECTIONS: [&str; 10] = [
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

    /// The folders, next to the lockfile, where a project keeps what this tool installs. The command installs
    /// into the first one.
    fn folders(self) -> &'static [&'static str] {
        match self {
            Self::Npm | Self::Pnpm | Self::Yarn | Self::YarnBerry | Self::Bun => &["node_modules"],
            Self::Uv | Self::Poetry | Self::Pip => &[".venv", "venv"],
            Self::Cargo => &[],
        }
    }
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

/// Writes `value` as JSON with the keys of every object sorted, so that the order of a manifest's entries does
/// not count. The parsers limit how deeply values nest (128 levels for JSON, 80 for TOML).
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
    for change in &listing.files {
        let Some(file) = change.new else { continue };
        // A path that is not UTF-8 is refused by the switch of the copy.
        let Ok(path) = std::str::from_utf8(&change.path) else {
            continue;
        };
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some(counted) = counted(name) else {
            continue;
        };
        if file.kind == FileKind::Symlink || path.split('/').any(|part| part == "node_modules") {
            continue;
        }
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
    Ok(Deps {
        key: sha1(&parts)?,
        installs: installs(files.iter().map(|(path, _)| path.as_str())),
        files,
    })
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

/// Whether the project has one of `names` as a real folder, reached through real folders, in its folder `dir`.
/// True if `names` is empty.
fn project_has_folder(project: &Path, dir: &str, names: &[&str]) -> bool {
    names.is_empty()
        || names.iter().any(|name| {
            let path = join(dir, name);
            parents(&path).all(|parent| is_real_dir_no_follow(&native(project, parent)))
                && is_real_dir_no_follow(&native(project, &path))
        })
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

/// A test copy of one dependency set.
pub struct SetCopy {
    copy: TestCopy,
    /// `copy-0` only: its links to the project's installed dependencies.
    links: Option<Links>,
}

struct Links {
    node: NodeModules,
    python: PythonEnvs,
}

impl SetCopy {
    pub fn copy(&self) -> &TestCopy {
        &self.copy
    }

    /// The complete environment of a run in this copy, for [`run`]: [`run_env`] of the user's environment `user`
    /// with the additions of [`env_additions`]. In a copy with its own install, the variables of
    /// [`OWN_INSTALL_REMOVED`] are removed and those of [`OWN_INSTALL_SET`] set, so that tools use the copy's
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

/// Why the dependencies of a set could not be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallFailure {
    /// The command that failed.
    pub command: String,
    /// The folder of the copy it ran in (relative, `/`-separated, `""` for the top), as the step's tree names it.
    pub dir: String,
    pub outcome: Outcome,
    /// The log holding the install's output.
    pub log: PathBuf,
    /// The end of the log shows what looks like a registry or login error.
    pub registry_error: bool,
}

/// The message for the user. It holds a folder name from the step's tree and paths in the cache folder: print
/// it through `printable()`.
impl fmt::Display for InstallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = if self.dir.is_empty() {
            "at the top of the test copy".to_owned()
        } else {
            format!("in {}", self.dir)
        };
        let ended = match self.outcome {
            Outcome::Failed(exit) => exit.to_string(),
            Outcome::Skipped => format!("exit code {SKIP_EXIT_CODE}"),
            Outcome::OutputTooLarge => "more output than a log may hold".to_owned(),
            Outcome::TimedOut => "a time limit".to_owned(),
            Outcome::Passed | Outcome::Stopped => "no error".to_owned(),
        };
        write!(
            f,
            "installing the dependencies failed: `{}` {place} ended with {ended}",
            self.command
        )?;
        if let Some(hint) = self.outcome.hint() {
            write!(f, " ({hint})")?;
        }
        write!(f, ". Its output is in {}", self.log.display())?;
        if self.registry_error {
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
    /// The copies used in this bisect, by number.
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
        })
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
        if !self.no_links && self.project_matches(&deps) && self.use_project_copy(store, tree)? {
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

    /// Whether the project has the same dependency files as the step, and the folders its installs fill.
    fn project_matches(&mut self, deps: &Deps) -> bool {
        let project = self.cache.project();
        for (path, digest) in &deps.files {
            let found = self.project_digests.entry(path.clone()).or_insert_with(|| {
                let name = path.rsplit('/').next().unwrap_or(path);
                let bytes = read_project_file(project, path)?;
                self::digest(counted(name)?, &bytes).ok()
            });
            if found.as_deref() != Some(digest.as_str()) {
                return false;
            }
        }
        deps.installs
            .iter()
            .all(|install| project_has_folder(project, &install.dir, install.tool.folders()))
    }

    /// Switches `copy-0` to `tree` and links it to the project's installed dependencies. Returns `false`, and
    /// stops using `copy-0` for the rest of the bisect, when the links cannot be made on this system.
    fn use_project_copy(&mut self, store: &ShadowStore, tree: ObjectId) -> Result<bool> {
        if let btree_map::Entry::Vacant(entry) = self.copies.entry(0) {
            let project = self.cache.project();
            entry.insert(SetCopy {
                copy: TestCopy::open(&self.cache, 0)?,
                links: Some(Links {
                    node: NodeModules::new(project)?,
                    python: PythonEnvs::new(project)?,
                }),
            });
        }
        let SetCopy { copy, links } = self.copies.get_mut(&0).expect("opened above");
        let links = links.as_mut().expect("copy-0 has links");
        copy.switch(store, tree)?;
        match links.node.link(copy).and_then(|_| links.python.link(copy)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == CANNOT_LINK => {
                self.no_links = true;
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
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
        self.book.copies.push(Recorded {
            number,
            key: set.clone(),
            state: State::Started,
            last_use,
        });
        self.book.save(events, self.cache.instance())?;

        self.open_own(number, deps)?.copy.switch(store, tree)?;
        let state = match self.run_installs(number, deps, logs, stop)? {
            Installed::Done => {
                // Undoes what the install changed in the step's files, as before every run.
                self.copies
                    .get_mut(&number)
                    .expect("opened above")
                    .copy
                    .switch(store, tree)?;
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

    /// Opens copy `number`, a copy with its own install for `deps`, and keeps the folders its installs fill
    /// (`node_modules`, `.venv` next to each lockfile) through every switch, ignored or not.
    fn open_own(&mut self, number: usize, deps: &Deps) -> Result<&mut SetCopy> {
        let set_copy = match self.copies.entry(number) {
            btree_map::Entry::Occupied(entry) => entry.into_mut(),
            btree_map::Entry::Vacant(entry) => entry.insert(SetCopy {
                copy: TestCopy::open(&self.cache, number)?,
                links: None,
            }),
        };
        let mut extra = set_copy.copy.extra_paths().clone();
        extra.extend(deps.installs.iter().filter_map(|install| {
            let folder = install.tool.folders().first()?;
            Some(join(&install.dir, folder))
        }));
        set_copy.copy.set_extra_paths(extra);
        Ok(set_copy)
    }

    /// Runs the installs of `deps`, or the `--setup` command, in copy `number`, one after the other, with the
    /// protections of a test run, their output in one new log of `logs`.
    fn run_installs(
        &mut self,
        number: usize,
        deps: &Deps,
        logs: &Logs,
        stop: &AtomicBool,
    ) -> Result<Installed> {
        let commands: Vec<(&str, &str)> = match &self.setup {
            Some(setup) => vec![("", setup.as_str())],
            None => deps
                .installs
                .iter()
                .map(|install| (install.dir.as_str(), install.tool.command()))
                .collect(),
        };
        self.installs += 1;
        let name = format!("install-{}.log", self.installs);
        let mut log = logs.create(&name)?;
        let set_copy = &self.copies[&number];
        for (dir, command) in commands {
            clean_root(&self.cache)?;
            write_boundaries(&self.cache)?;
            let env = set_copy.run_env(self.user.iter().cloned())?;
            let cwd = folder_in_copy(set_copy.copy.path(), dir)?;
            let place = if dir.is_empty() { "." } else { dir };
            writeln!(log, "$ {command}    (in {place})")?;
            match run(command, &cwd, &env, &log, None, stop)?.outcome {
                Outcome::Passed => {}
                Outcome::Stopped => return Ok(Installed::Stopped),
                outcome => {
                    return Ok(Installed::Failed(InstallFailure {
                        command: command.to_owned(),
                        dir: dir.to_owned(),
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

    use crate::bisect::link::make_link;
    use crate::bisect::run::Exit;
    use crate::bisect::testing::{Project, state, token, tool, venv_python, write};

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
                let log = fs::read_to_string(&failure.log).unwrap_or_default();
                panic!("{failure}\n{log}")
            }
            Prepared::Stopped => panic!("stopped"),
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
        assert_eq!(failure.outcome, Outcome::Failed(Exit::Code(1)));
        assert_eq!(failure.command, setup);
        assert_eq!(failure.dir, "");
        assert!(failure.log.is_file());
        assert!(!failure.registry_error);
        let message = failure.to_string();
        assert!(message.contains("ended with exit code 1"), "{message}");
        assert!(
            message.contains(&failure.log.display().to_string()),
            "{message}"
        );
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
        assert!(failure.registry_error);
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
        if tool(&["python3", "python"]).is_none() {
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
}
