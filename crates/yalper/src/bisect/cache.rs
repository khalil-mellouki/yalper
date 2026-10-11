//! Where bisect keeps its working data: one private folder per project in the user's cache folder, outside the
//! project, so that no tool a test starts finds the project by searching the folders above a test copy.
//!
//! The folder, `<cache>/yalper/<slug>-<id>/` (see [`CacheDir`]), holds the test copies `copy-<n>/`, the boundary
//! files of [`write_boundaries`](super::write_boundaries), the Python bytecode and Cargo build folders
//! `pycache/` and `cargo-target/`, the run logs `logs/`, the switch clock `clock`, the bisect lock `lock`, and the
//! marker `project` that binds the folder to its project. Recordings and remembered settings stay in `.yalper/`.
//!
//! `<cache>/yalper/` is, in this order:
//!
//! - `YALPER_CACHE_DIR`, an absolute path, when it is set;
//! - Linux and other Unix systems: `$XDG_CACHE_HOME/yalper/` if that is set to an absolute path (the XDG base
//!   directory specification ignores an empty or relative one), else `~/.cache/yalper/`;
//! - macOS: `~/Library/Caches/yalper/`;
//! - Windows: `%LOCALAPPDATA%\yalper\` if that is an absolute path, else `<home>\AppData\Local\yalper\`.
//!
//! Nothing falls back to another place: a wrong one could be inside the project. [`CacheDir::open`] refuses,
//! with a message naming `YALPER_CACHE_DIR`, when the home folder is unknown, when `YALPER_CACHE_DIR` is relative,
//! when the project's folder would be the project, inside it, or hold it (a project that is the home folder),
//! and on Windows when it would be on a network share or a mapped network drive (junctions to the project cannot
//! be made there, and copies and builds would be slow).
//!
//! The folder is deleted by `yalper uninstall --purge` (see [`find_project_folders`]) and, once its project is
//! gone, by the next bisect of any project (see [`remove_stale`]).

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::boundary;
use super::copy::{CARGO_TARGET_DIR, CLOCK_FILE, PYCACHE_DIR, open_private_dir, remove_entry};
use super::link::{has_special_prefix, is_under, normalize, plain};
use super::run::LOGS_DIR;
use crate::hook::YALPER_DIR;
use crate::repo::{self, Token};
use crate::safe_fs::{self, OwnedDir};
use crate::snapshot::CASE_INSENSITIVE;

/// The variable that names Yalper's cache folder, in place of `<cache>/yalper/`.
pub const CACHE_DIR_VAR: &str = "YALPER_CACHE_DIR";

/// The `meta` key of the event log that records the project's cache folder: see [`CacheDir::meta_value`].
pub const CACHE_DIR_KEY: &str = "bisect.cache_dir";

/// The marker at the top of a project's folder: see [`CacheDir`].
const MARKER_FILE: &str = "project";

/// How long the folder of a project that is gone stays after its last use: the folder of a project on a drive
/// that is not plugged in is kept that long.
const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The lock that lets one bisect of a project run at a time. Like `clock`, it must never be written through an
/// existing file at its name (remove it and create it with `create_new` instead): on Windows a hard link a test
/// put there cannot be told from a plain file, so a write would reach the file it links to.
const LOCK_FILE: &str = "lock";

/// A marker larger than this was not written by Yalper (it holds a digest, an id and a path).
const MAX_MARKER_BYTES: u64 = 64 * 1024;

/// How many times an empty marker is read, 10 ms apart, while another open may be writing it.
const MARKER_READS: usize = 20;

/// An empty marker older than this was left by an interrupted creation; a newer one is being written by
/// another bisect that is starting.
const STALE_EMPTY_MARKER: std::time::Duration = std::time::Duration::from_secs(5);

/// How many times a first open tries to create the marker while other opens create or remove it.
const MARKER_ATTEMPTS: usize = 3;

/// The most characters of the project folder's name in the name of its cache folder: paths stay short for
/// tools that do not handle long Windows paths.
const MAX_SLUG_CHARS: usize = 16;

/// The hex digits of the digest in the name of a project's cache folder: 48 bits.
const ID_HEX_DIGITS: usize = 12;

/// The system whose cache folder is used, a parameter so that every rule can be tested on any system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum System {
    Windows,
    MacOs,
    /// Linux and the other Unix systems.
    Unix,
}

impl System {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unix
        }
    }
}

/// What the place of Yalper's cache folder depends on: the variables, and the home folder.
#[derive(Debug, Default)]
struct Vars {
    yalper_cache_dir: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    local_app_data: Option<OsString>,
    home: Option<PathBuf>,
}

impl Vars {
    fn current() -> Self {
        Self {
            yalper_cache_dir: std::env::var_os(CACHE_DIR_VAR),
            xdg_cache_home: std::env::var_os("XDG_CACHE_HOME"),
            local_app_data: std::env::var_os("LOCALAPPDATA"),
            home: std::env::home_dir(),
        }
    }
}

/// Yalper's cache folder on `system` with `vars` (see the module documentation). An empty variable counts as
/// not set; an empty or relative home folder as unknown.
fn yalper_folder(system: System, vars: &Vars) -> io::Result<PathBuf> {
    if let Some(value) = vars.yalper_cache_dir.as_deref().filter(|v| !v.is_empty()) {
        let path = Path::new(value);
        if !path.is_absolute() {
            return Err(refusal(format!(
                "{CACHE_DIR_VAR} must be an absolute path, not {}",
                path.display()
            )));
        }
        return Ok(path.to_owned());
    }
    let absolute = |value: &Option<OsString>| {
        value
            .as_deref()
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .map(Path::to_owned)
    };
    let home = || vars.home.as_deref().filter(|home| home.is_absolute());
    let base = match system {
        System::Unix => {
            absolute(&vars.xdg_cache_home).or_else(|| home().map(|home| home.join(".cache")))
        }
        System::MacOs => home().map(|home| home.join("Library").join("Caches")),
        System::Windows => absolute(&vars.local_app_data)
            .or_else(|| home().map(|home| home.join("AppData").join("Local"))),
    };
    base.map(|base| base.join("yalper")).ok_or_else(|| {
        refusal(format!(
            "cannot find your cache folder for the bisect data: your home folder is unknown. Set \
             {CACHE_DIR_VAR} to the absolute path of a folder outside the project"
        ))
    })
}

/// A project's folder in Yalper's cache folder, `<slug>-<id>`, held open: a real folder, not a link, owned by
/// the user and, on Unix, with mode 0700 and not writable by its group or others (Windows access lists are not
/// inspected). So is Yalper's cache folder above it.
///
/// `<slug>` is the project folder's name made safe (see [`slug`]); `<id>` the first 12 hex digits of the SHA-1
/// of the project's init token and canonical path. The token gives a re-initialized project a new folder and
/// the path gives a copy of the project (same token) its own; the token itself never appears in a path.
///
/// The marker file `project`, written right after the folder is created, holds the full digest, a random
/// instance id and the project's path (for whoever looks at the folder). A folder is used only if it is empty
/// (its creation was interrupted) or its marker is a regular file with the project's digest: anything else is
/// refused, since it was not made by Yalper for this project.
#[derive(Debug)]
pub struct CacheDir {
    dir: OwnedDir,
    /// The folder's path as computed from the variables, before any link in it is resolved.
    given: PathBuf,
    instance: String,
    project: PathBuf,
    warning: Option<String>,
}

impl CacheDir {
    /// Opens, creating it if needed, the cache folder of the project at `project`, whose init token is `token`,
    /// in the user's cache folder (see the module documentation).
    pub fn open(project: &Path, token: &Token) -> io::Result<Self> {
        let folder = yalper_folder(System::current(), &Vars::current())?;
        Self::open_in(&folder, project, token)
    }

    /// Like [`open`](Self::open), with `folder` as Yalper's cache folder: an absolute path, created if missing.
    pub fn open_in(folder: &Path, project: &Path, token: &Token) -> io::Result<Self> {
        let project_real = plain(&fs::canonicalize(project).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot resolve the project folder {}: {error}",
                    project.display()
                ),
            )
        })?);
        let (name, digest) = folder_name(&project_real, token)?;
        let given = folder.join(&name);
        let resolved = check_place(folder, &name, &project_real)?;
        #[cfg(unix)]
        check_parent(folder, folder)?;
        let warning = if cfg!(windows) {
            let profile = std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .filter(|profile| profile.is_absolute())
                .or_else(std::env::home_dir)
                .map(|profile| fs::canonicalize(&profile).map_or(profile, |real| plain(&real)));
            access_warning(&resolved, profile.as_deref())
        } else {
            None
        };

        #[cfg_attr(windows, allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(folder).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot create {}: {error}", folder.display()),
            )
        })?;
        let yalper = open_private_dir(folder)?;
        // Again on the path held open, the one every later operation uses: a link in the path as written could
        // lead elsewhere since the first check.
        #[cfg(unix)]
        check_parent(yalper.path(), folder)?;
        let dir = open_private_dir(&yalper.path().join(&name))?;
        let instance = bind(&dir, &digest, &project_real)?;
        Ok(Self {
            dir,
            given,
            instance,
            project: project.to_owned(),
            warning,
        })
    }

    /// A warning for the user, to be printed when a bisect starts: on Windows, when the folder is outside the
    /// user's profile folder, where other local users may be able to read it (see [`access_warning`]).
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// The folder's path (canonical on Unix).
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The random id written in the marker when the folder was created: a folder deleted and made again gets
    /// another one, so what is recorded about its contents elsewhere no longer applies.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The value of [`CACHE_DIR_KEY`] for this folder, a JSON object with its `path` and `instance` id: with
    /// it, `yalper uninstall --purge` finds the folder after the variables that place it changed.
    pub fn meta_value(&self) -> String {
        serde_json::json!({
            "path": self.path().to_string_lossy(),
            "instance": self.instance,
        })
        .to_string()
    }

    /// The project folder, as given to [`open`](Self::open).
    pub fn project(&self) -> &Path {
        &self.project
    }

    pub(super) fn dir(&self) -> &OwnedDir {
        &self.dir
    }

    /// The spellings of the folder: as computed from the variables, and as held open.
    pub(super) fn spellings(&self) -> [&Path; 2] {
        [&self.given, self.dir.path()]
    }

    pub(super) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            dir: self.dir.try_clone()?,
            given: self.given.clone(),
            instance: self.instance.clone(),
            project: self.project.clone(),
            warning: self.warning.clone(),
        })
    }
}

/// Removes every entry at the top of the project's cache folder that Yalper does not put there, never following
/// a link: folders that tools made where the boundary files sent them (`node_modules/` of an `npm install`, uv's
/// `.venv/`, `.pytest_cache/`, `Cargo.lock`), and files a test planted to be found from every copy
/// (`conftest.py`, `.cargo/config.toml`, `pnpm-workspace.yaml`). Kept: the folders `copy-<digits>`, `logs`,
/// `pycache` and `cargo-target`, and the regular files `clock`, `lock`, `project` and the boundary files; an
/// entry with one of those names but of another kind (a link in place of a folder) is removed too. Returns how
/// many entries were removed.
///
/// Call it at the start of every bisect, and before every run and every install, before
/// [`write_boundaries`](super::write_boundaries): listing a dozen entries is cheap.
pub fn clean_root(cache: &CacheDir) -> io::Result<usize> {
    cache.dir.check_still_at_path()?;
    let mut removed = 0;
    for entry in fs::read_dir(cache.path())? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let keep = entry.file_name().to_str().is_some_and(|name| {
            if kind.is_dir() {
                [LOGS_DIR, PYCACHE_DIR, CARGO_TARGET_DIR].contains(&name)
                    || name
                        .strip_prefix("copy-")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            } else {
                kind.is_file()
                    && ([CLOCK_FILE, LOCK_FILE, MARKER_FILE].contains(&name)
                        || boundary::FILES.iter().any(|(file, _)| *file == name))
            }
        });
        if !keep {
            let path = entry.path();
            remove_entry(&path, &fs::symlink_metadata(&path)?).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot remove {}: {error}", path.display()),
                )
            })?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// A cache folder of a project, held open, found by [`find_project_folders`].
#[derive(Debug)]
pub struct ProjectFolder(OwnedDir);

impl ProjectFolder {
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Deletes the folder and everything in it, never following a link (see [`delete_folder`]).
    pub fn delete(self) -> io::Result<()> {
        delete_folder(self.0)
    }
}

/// What [`find_project_folders`] found.
#[derive(Debug, Default)]
pub struct ProjectFolders {
    /// The project's folders.
    pub ours: Vec<ProjectFolder>,
    /// Folders where the project's would be that are not the project's (they are never deleted), with the
    /// reason.
    pub others: Vec<(PathBuf, String)>,
    /// Why the place of the project's folder could not be computed from the variables, when no folder was
    /// recorded either.
    pub unknown: Option<String>,
}

/// Looks for the cache folders of the project at `project`, whose init token is `token`: the one recorded in
/// `recorded` (the [`CACHE_DIR_KEY`] value of its event log, if any) and the one the variables give now (see
/// the module documentation), when they differ. Nothing is changed, no link is followed, and a network path is
/// never looked up.
///
/// A folder is the project's when it is named as Yalper names a project's folder (`<slug>-<id>`, the id taken
/// from its marker's digest) and its marker has the project's digest, or, for the recorded folder of a project
/// moved since, the recorded instance id while the project the marker names no longer gives the folder its
/// name (a copy of the project carries the same record, and must not take the original's folder). A folder that
/// holds nothing, or only an empty marker older than 5 seconds (a creation or deletion was interrupted), is the
/// project's at the place the variables give, and at a recorded place named as Yalper names folders.
pub fn find_project_folders(
    project: &Path,
    token: &Token,
    recorded: Option<&str>,
) -> ProjectFolders {
    let folder = yalper_folder(System::current(), &Vars::current());
    find_in(folder, project, token, recorded)
}

/// [`find_project_folders`] with `folder` as Yalper's cache folder.
fn find_in(
    folder: io::Result<PathBuf>,
    project: &Path,
    token: &Token,
    recorded: Option<&str>,
) -> ProjectFolders {
    let mut found = ProjectFolders::default();
    let named = fs::canonicalize(project).and_then(|real| folder_name(&plain(&real), token));
    let digest = named.as_ref().ok().map(|(_, digest)| digest.as_str());

    // Each place, with the instance id recorded for it (`None`: the place the variables give).
    let mut places = Vec::new();
    if let Some((path, instance)) = recorded.and_then(parse_meta_value) {
        places.push((path, Some(instance)));
    }
    match (folder, &named) {
        (Ok(folder), Ok((name, _))) => places.push((folder.join(name), None)),
        (Err(error), _) if places.is_empty() => found.unknown = Some(error.to_string()),
        (_, Err(error)) if places.is_empty() => found.unknown = Some(error.to_string()),
        _ => {}
    }

    let not_ours = "it was not created by Yalper for this project";
    let now = SystemTime::now();
    let mut seen = Vec::new();
    for (path, instance) in places {
        // Before anything else, so that a network path is never looked up.
        if has_special_prefix(&plain(&path)) {
            found.others.push((
                path,
                "it is on a network share, which Yalper does not look at".to_owned(),
            ));
            continue;
        }
        // The same folder spelled twice (through a link above it, or in another case) is looked at once.
        let key = resolve(&path).ok().map(|resolved| {
            let text = resolved.to_string_lossy().into_owned();
            if CASE_INSENSITIVE {
                text.to_lowercase()
            } else {
                text
            }
        });
        if key.is_some() && seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        match look(&path) {
            Look::Missing => {}
            Look::Other(shown, why) => found.others.push((shown, why)),
            Look::Unmarked(dir, unmarked) => {
                let place_is_ours = instance.is_none() || folder_id(name).is_some();
                let interrupted =
                    !unmarked.marker || older(now, unmarked.changed, STALE_EMPTY_MARKER);
                if place_is_ours && interrupted {
                    found.ours.push(ProjectFolder(dir));
                } else {
                    found
                        .others
                        .push((dir.path().to_owned(), not_ours.to_owned()));
                }
            }
            Look::Marked(dir, marker) => {
                let shown = dir.path().to_owned();
                if !named_for(name, &marker.digest) {
                    found.others.push((shown, not_ours.to_owned()));
                } else if Some(marker.digest.as_str()) == digest {
                    found.ours.push(ProjectFolder(dir));
                } else if instance.as_deref() != Some(marker.instance.as_str()) {
                    found.others.push((shown, not_ours.to_owned()));
                } else {
                    let owner = marker.project.as_deref();
                    match owner.and_then(|owner| gives_name(Path::new(owner), name)) {
                        Some(false) => found.ours.push(ProjectFolder(dir)),
                        _ => found.others.push((
                            shown,
                            owner.map_or_else(
                                || "it may belong to another project".to_owned(),
                                |owner| format!("it belongs to the project at {owner}"),
                            ),
                        )),
                    }
                }
            }
        }
    }
    found
}

/// The path and instance id recorded in a [`CACHE_DIR_KEY`] value (see [`CacheDir::meta_value`]). A path that
/// was not valid Unicode was written with replacement characters, so it is not a folder's path.
fn parse_meta_value(value: &str) -> Option<(PathBuf, String)> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    let path = value.get("path")?.as_str()?;
    let instance = Token::parse(value.get("instance")?.as_str()?)?;
    let path = PathBuf::from(path);
    (path.is_absolute() && !path.to_string_lossy().contains('\u{FFFD}'))
        .then(|| (path, instance.as_str().to_owned()))
}

/// The id in a name `<slug>-<id>` as Yalper names a project's cache folder (see [`folder_name`]), if `name`
/// has that form.
fn folder_id(name: &str) -> Option<&str> {
    let (slug, id) = name.rsplit_once('-')?;
    let slug_ok = !slug.is_empty()
        && slug.chars().count() <= MAX_SLUG_CHARS
        && slug
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    let id_ok =
        id.len() == ID_HEX_DIGITS && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (slug_ok && id_ok).then_some(id)
}

/// Whether `name` is the name Yalper gives the folder whose marker holds `digest`: a marker copied into
/// another folder does not make that folder Yalper's.
fn named_for(name: &str, digest: &str) -> bool {
    folder_id(name).is_some_and(|id| digest.get(..ID_HEX_DIGITS) == Some(id))
}

/// Whether `time` is more than `limit` before `now` (a time ahead of `now` is not).
fn older(now: SystemTime, time: SystemTime, limit: Duration) -> bool {
    now.duration_since(time).is_ok_and(|age| age > limit)
}

/// What is at the place of a project's cache folder.
enum Look {
    Missing,
    /// A folder whose marker is valid, held open.
    Marked(OwnedDir, MarkerInfo),
    /// A folder that holds nothing that says whose it is, held open.
    Unmarked(OwnedDir, Unmarked),
    /// Anything else: the path to show (as held open when it was opened, so that it is spelled like the
    /// project's folders) and the reason it is not a project's folder.
    Other(PathBuf, String),
}

/// What a valid marker holds.
struct MarkerInfo {
    digest: String,
    instance: String,
    /// The project's path, when it can be looked up (see [`marker_project`]).
    project: Option<String>,
}

/// A folder that holds nothing at all, or only an empty marker: what a creation interrupted before the marker
/// was written, or a deletion interrupted after the marker was deleted, leaves.
struct Unmarked {
    /// Whether the empty marker is there.
    marker: bool,
    /// When the marker, or else the folder, was last changed.
    changed: SystemTime,
}

/// Looks at `path`, the place of a project's cache folder, without following a link: the folder and the folder
/// above it must be real folders, and the folder is opened (see [`OwnedDir::open`]) before anything in it is
/// read.
fn look(path: &Path) -> Look {
    let other = |why: String| Look::Other(path.to_owned(), why);
    for dir in [path.parent().unwrap_or(path), path] {
        match fs::symlink_metadata(dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Look::Missing,
            Err(error) => return other(format!("cannot read {}: {error}", dir.display())),
            Ok(metadata) if !metadata.is_dir() => {
                return other(format!(
                    "{} is a link or a file, not a folder",
                    dir.display()
                ));
            }
            Ok(_) => {}
        }
    }
    let dir = match OwnedDir::open(path) {
        Ok(dir) => dir,
        Err(error) => return other(error.to_string()),
    };
    let opened = dir.path().to_owned();
    match unmarked(&opened) {
        Ok(Some(unmarked)) => return Look::Unmarked(dir, unmarked),
        Ok(None) => {}
        Err(error) => return Look::Other(opened, format!("cannot read it: {error}")),
    }
    let text = safe_fs::read_small_regular_file(&opened.join(MARKER_FILE), MAX_MARKER_BYTES)
        .and_then(|bytes| String::from_utf8(bytes).ok());
    match text.as_deref().and_then(|text| {
        let (digest, instance) = parse_marker(text.as_bytes())?;
        let project = marker_project(text).map(str::to_owned);
        Some(MarkerInfo {
            digest,
            instance,
            project,
        })
    }) {
        Some(marker) => Look::Marked(dir, marker),
        None => Look::Other(
            opened,
            "it was not created by Yalper for this project".to_owned(),
        ),
    }
}

/// What the folder at `path` holds when nothing in it says whose it is (see [`Unmarked`]).
fn unmarked(path: &Path) -> io::Result<Option<Unmarked>> {
    let mut entries = fs::read_dir(path)?;
    let Some(first) = entries.next().transpose()? else {
        let changed = fs::symlink_metadata(path)?.modified()?;
        return Ok(Some(Unmarked {
            marker: false,
            changed,
        }));
    };
    if first.file_name() != MARKER_FILE || entries.next().is_some() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(first.path())?;
    if !metadata.is_file() || metadata.len() != 0 {
        return Ok(None);
    }
    // No mtime does not keep the folder forever, as in `read_marker`.
    let changed = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(Some(Unmarked {
        marker: true,
        changed,
    }))
}

/// Deletes the project's cache folder `dir` without following any link inside it (a link is removed, never
/// what it points to: the test copies link to the project's dependencies). The folder is checked to still be
/// at its path first (see [`OwnedDir::check_still_at_path`]). Its marker goes last: if a file in use stops the
/// deletion halfway (Windows), the folder still says whose it is, so running the deletion again finishes it.
fn delete_folder(dir: OwnedDir) -> io::Result<()> {
    dir.check_still_at_path()?;
    let path = dir.path().to_owned();
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        if entry.file_name() != MARKER_FILE {
            let entry = entry.path();
            remove_entry(&entry, &fs::symlink_metadata(&entry)?)?;
        }
    }
    let marker = path.join(MARKER_FILE);
    match fs::symlink_metadata(&marker).and_then(|metadata| remove_entry(&marker, &metadata)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    // On Windows the held handle stops the folder itself from being deleted.
    drop(dir);
    fs::remove_dir(&path)
}

/// Deletes the folders next to `current` in Yalper's cache folder that projects no longer use, unused (their
/// `clock`, else their marker, last changed) for more than 7 days: those whose marker names a project that is
/// gone or no longer gives the folder its name (moved, purged, set up again with `yalper init`), and those left
/// holding nothing, or only an empty marker, by an interrupted creation or deletion. Only folders named as
/// Yalper names them are looked at. Folders without a valid marker, links, and `current` are never touched;
/// nothing is ever followed out of Yalper's cache folder, and a project on a network share, or whose `.git` is
/// not a folder, is never looked up. Returns the deleted folders.
///
/// Call it when a bisect starts: it lists one folder and reads a small marker in each entry, and looks at a
/// project only for a folder unused for 7 days. A folder that cannot be checked or deleted is kept.
pub fn remove_stale(current: &CacheDir) -> Vec<PathBuf> {
    let (Some(folder), Some(own)) = (current.path().parent(), current.path().file_name()) else {
        return Vec::new();
    };
    if !fs::symlink_metadata(folder).is_ok_and(|metadata| metadata.is_dir()) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| folder_id(name).is_some()) else {
            continue;
        };
        if name == own || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        let stale = match look(&path) {
            Look::Unmarked(dir, unmarked) => {
                older(now, unmarked.changed, STALE_AFTER).then_some(dir)
            }
            Look::Marked(dir, marker) => is_stale(dir.path(), name, &marker, now).then_some(dir),
            Look::Missing | Look::Other(..) => None,
        };
        if let Some(dir) = stale
            && delete_folder(dir).is_ok()
        {
            removed.push(path);
        }
    }
    removed
}

/// Whether the folder `name` at `path` in Yalper's cache folder, whose marker holds `marker`, is stale at `now`
/// (see [`remove_stale`]).
fn is_stale(path: &Path, name: &str, marker: &MarkerInfo, now: SystemTime) -> bool {
    if !named_for(name, &marker.digest) {
        return false;
    }
    let Some(project) = &marker.project else {
        return false;
    };
    let last_use = [CLOCK_FILE, MARKER_FILE].into_iter().find_map(|file| {
        fs::symlink_metadata(path.join(file))
            .ok()
            .filter(fs::Metadata::is_file)
            .and_then(|metadata| metadata.modified().ok())
    });
    last_use.is_some_and(|last_use| older(now, last_use, STALE_AFTER))
        && gives_name(Path::new(project), name) == Some(false)
}

/// Whether the project at `project` (canonical, as a marker holds it) gives its cache folder the name `name`
/// now. `Some(false)` only when something it needs is not found (the project, its `.git` or `.yalper/` folder,
/// an init token) or its two init tokens differ, or when its token and path give another name. `None` when that
/// cannot be told: a network path, a `.git` that is not a real folder (a `.git` file naming a git directory
/// elsewhere is never followed), a `.yalper` that is not a real folder, or any other error reading them.
fn gives_name(project: &Path, name: &str) -> Option<bool> {
    if !project.is_absolute() || has_special_prefix(project) {
        return None;
    }
    let real = match fs::canonicalize(project) {
        Ok(real) => plain(&real),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    let git_dir = real.join(".git");
    let yalper = real.join(YALPER_DIR);
    for dir in [&git_dir, &yalper] {
        match fs::symlink_metadata(dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(false),
            Ok(metadata) if metadata.is_dir() => {}
            _ => return None,
        }
    }
    // `Some(None)`: no token file; `None`: one that cannot be read.
    let token_at = |path: PathBuf| match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(None),
        Ok(metadata) if metadata.is_file() => repo::read_token(&path).map(Some),
        _ => None,
    };
    let git_token = token_at(git_dir.join(repo::GIT_ID_FILE))?;
    let own_token = token_at(yalper.join(repo::ID_FILE))?;
    let (Some(git_token), Some(own_token)) = (git_token, own_token) else {
        return Some(false);
    };
    if git_token != own_token {
        return Some(false);
    }
    let (now, _) = folder_name(&real, &git_token).ok()?;
    Some(now == name)
}

fn refusal(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Refuses Yalper's cache folder `folder` for the project at the canonical path `project` when the project's
/// folder `name` in it would be the project, inside it, or hold it, or, on Windows, when `folder` is on a
/// network share. Paths are compared once resolved (see [`resolve`]), without regard to case where the file
/// system ignores it. Returns `folder` resolved.
fn check_place(folder: &Path, name: &str, project: &Path) -> io::Result<PathBuf> {
    let set_it =
        format!("Set {CACHE_DIR_VAR} to the absolute path of a local folder outside the project");
    if !folder.is_absolute() {
        return Err(refusal(format!(
            "cannot keep the bisect data in {}: not an absolute path. {set_it}",
            folder.display()
        )));
    }
    let network = |path: &Path| {
        refusal(format!(
            "cannot keep the bisect data in {}: it is on a network share, where test copies cannot link to \
             the project's dependencies and would be slow. {set_it}",
            path.display()
        ))
    };
    // Checked before resolving, so that a network path is never looked up.
    if has_special_prefix(&plain(folder)) {
        return Err(network(folder));
    }
    let resolved = resolve(folder)?;
    // A mapped network drive is a network path once resolved.
    if has_special_prefix(&resolved) {
        return Err(network(folder));
    }
    let own = resolved.join(name);
    if let Some(problem) = overlap(&own, project) {
        return Err(refusal(format!(
            "cannot keep the bisect data in {}: it would be {problem} the project {}, so tools started by the \
             tests could find the project. {set_it}",
            own.display(),
            project.display()
        )));
    }
    Ok(resolved)
}

/// Unix: refuses Yalper's cache folder `folder` (spelled `path`, as written or as held open) when another user
/// could replace it, or a folder above it, between two bisects, as OpenSSH's `safe_path` checks: every folder
/// from the first existing one above `path` (resolved) up to `/` must pass [`unsafe_folder`]. Missing folders
/// are created with mode 0700.
#[cfg(unix)]
fn check_parent(path: &Path, folder: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(existing) = parent
        .ancestors()
        .find_map(|dir| fs::canonicalize(dir).ok())
    else {
        return Ok(());
    };
    let me = rustix::process::geteuid().as_raw();
    let my_group = rustix::process::getegid().as_raw();
    for dir in existing.ancestors() {
        let metadata = fs::metadata(dir)?;
        let why = unsafe_folder(
            metadata.mode(),
            metadata.uid(),
            metadata.gid(),
            me,
            my_group,
        );
        if let Some(why) = why {
            return Err(refusal(format!(
                "cannot keep the bisect data in {}: {} is {why}, so another user could replace the bisect \
                 data. Set {CACHE_DIR_VAR} to the absolute path of a folder that only you can change",
                folder.display(),
                dir.display()
            )));
        }
    }
    Ok(())
}

/// Why a folder with permission bits `mode`, owner `uid` and group `gid` lets another user than `me` (whose
/// own group is `my_group`) replace what it holds, if it does: it is owned by someone other than `me` or root,
/// or, without the sticky bit (which lets only owners rename or delete entries), writable by every user or by
/// its group. Group write is accepted only on a folder of `me` whose group is `my_group`, a user-private group
/// as `~/.cache` gets with umask 002 on Linux; never on macOS, where every user's primary group is `staff`.
#[cfg(unix)]
fn unsafe_folder(mode: u32, uid: u32, gid: u32, me: u32, my_group: u32) -> Option<&'static str> {
    let sticky = mode & 0o1000 != 0;
    let own_group = uid == me && gid == my_group && !cfg!(target_os = "macos");
    if uid != me && uid != 0 {
        Some("owned by another user")
    } else if mode & 0o002 != 0 && !sticky {
        Some("writable by every user")
    } else if mode & 0o020 != 0 && !sticky && !own_group {
        Some("writable by its group")
    } else {
        None
    }
}

/// The warning for Yalper's cache folder `folder` (resolved) on Windows, where Yalper does not check access
/// lists: a folder outside the user's profile folder `profile` (resolved) may be readable by other local users
/// (a folder created under `C:\` lets every signed-in user read it), and run logs can hold secrets that tests
/// print.
fn access_warning(folder: &Path, profile: Option<&Path>) -> Option<String> {
    if profile.is_some_and(|profile| is_under(folder, profile)) {
        return None;
    }
    Some(format!(
        "the bisect data is kept in {}, outside your user profile folder: Yalper does not check Windows access \
         rights, so other users of this computer may be able to read it, run logs included (which can hold \
         secrets that tests print). Set {CACHE_DIR_VAR} to a folder inside your profile folder to keep it \
         private",
        folder.display()
    ))
}

/// How the project's cache folder `own` and the project `project`, both resolved, overlap, if they do.
fn overlap(own: &Path, project: &Path) -> Option<&'static str> {
    if is_under(own, project) {
        Some("the project's folder or inside")
    } else if is_under(project, own) {
        Some("a folder that holds")
    } else {
        None
    }
}

/// `path`, absolute, with every link of its longest existing part resolved and `.` and `..` of the rest
/// resolved from the text, without Windows' `\\?\` prefix.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    for existing in path.ancestors() {
        match fs::canonicalize(existing) {
            Ok(real) => {
                let rest = path.strip_prefix(existing).unwrap_or(Path::new(""));
                return Ok(normalize(&plain(&real).join(rest)));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot resolve {}: {error}", existing.display()),
                ));
            }
        }
    }
    Ok(normalize(path))
}

/// The name of the cache folder of the project at the canonical path `project` with the init token `token`,
/// `<slug>-<id>`, and the full digest the id comes from (see [`CacheDir`]).
fn folder_name(project: &Path, token: &Token) -> io::Result<(String, String)> {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(token.as_str().as_bytes());
    hasher.update(&[0]);
    if CASE_INSENSITIVE {
        hasher.update(project.to_string_lossy().to_lowercase().as_bytes());
    } else {
        hasher.update(project.as_os_str().as_encoded_bytes());
    }
    let digest = hasher
        .try_finalize()
        .map_err(|error| io::Error::other(format!("cannot hash the project's path: {error}")))?
        .to_string();
    let name = project.file_name().map(|name| name.to_string_lossy());
    let slug = slug(name.as_deref().unwrap_or(""));
    Ok((format!("{slug}-{}", &digest[..ID_HEX_DIGITS]), digest))
}

/// The project folder's name `name` as the start of a folder name that is valid and short on every system:
/// lowercase, every character other than `a` to `z`, `0` to `9`, `_` and `-` replaced by `-` (dots too, so no
/// Windows device name such as `con.x` can appear), without leading `-`, at most [`MAX_SLUG_CHARS`] characters,
/// and `project` if nothing is left.
fn slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9' | '_' | '-') => c,
            _ => '-',
        })
        .skip_while(|&c| c == '-')
        .take(MAX_SLUG_CHARS)
        .collect();
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug
    }
}

/// Checks the marker of the project's cache folder `dir`, or writes it if the folder is empty (its creation was
/// interrupted: nothing in it, or only an empty marker older than [`STALE_EMPTY_MARKER`]), and returns the
/// instance id it holds. The marker is created exclusively: when two first opens of the folder race, the one
/// that loses reads the winner's marker.
fn bind(dir: &OwnedDir, digest: &str, project: &Path) -> io::Result<String> {
    let marker = dir.path().join(MARKER_FILE);
    let not_ours = || {
        io::Error::other(format!(
            "{} was not created by Yalper for this project; delete it by hand",
            dir.path().display()
        ))
    };
    let busy = || {
        io::Error::other(format!(
            "another bisect is starting in this project (its folder {} is being created); try again",
            dir.path().display()
        ))
    };
    let only_marker = || -> io::Result<bool> {
        for entry in fs::read_dir(dir.path())? {
            if entry?.file_name() != MARKER_FILE {
                return Ok(false);
            }
        }
        Ok(true)
    };
    for _ in 0..MARKER_ATTEMPTS {
        match read_marker(&marker)? {
            Marker::Valid { digest: found, .. } if found != digest => return Err(not_ours()),
            Marker::Valid { instance, .. } => return Ok(instance),
            Marker::Other => return Err(not_ours()),
            Marker::Empty { .. } if !only_marker()? => return Err(not_ours()),
            Marker::Empty { stale: false } => return Err(busy()),
            Marker::Empty { stale: true } => {
                // Another open may have removed it first: then it is created again below, or read.
                match fs::symlink_metadata(&marker)
                    .and_then(|metadata| remove_entry(&marker, &metadata))
                {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                    _ => {}
                }
            }
            Marker::Missing if !only_marker()? => return Err(not_ours()),
            Marker::Missing => {}
        }
        dir.check_still_at_path()?;
        let mut options = fs::File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = match options.open(&marker) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let instance = Token::generate()?.as_str().to_owned();
        let text = format!(
            "Bisect data of a project, kept by Yalper. This folder can be deleted at any time.\n\
             digest {digest}\ninstance {instance}\nproject {}\n",
            project.display()
        );
        file.write_all(text.as_bytes())?;
        return Ok(instance);
    }
    Err(busy())
}

/// What is at the marker's place.
enum Marker {
    Missing,
    /// An empty regular file: a marker another open has just created and not written yet, or one whose creation
    /// was interrupted (`stale`: older than [`STALE_EMPTY_MARKER`]).
    Empty {
        stale: bool,
    },
    Valid {
        digest: String,
        instance: String,
    },
    /// Anything else: a link, a folder, a file Yalper did not write.
    Other,
}

/// Reads the marker at `path`. An empty one is read again for a moment, since another first open of the folder
/// may be writing it.
fn read_marker(path: &Path) -> io::Result<Marker> {
    let mut modified = None;
    for _ in 0..MARKER_READS {
        match fs::symlink_metadata(path) {
            Ok(metadata) => modified = metadata.modified().ok(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Marker::Missing),
            Err(error) => return Err(error),
        }
        match safe_fs::read_small_regular_file(path, MAX_MARKER_BYTES) {
            None => return Ok(Marker::Other),
            Some(bytes) if bytes.is_empty() => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Some(bytes) => {
                return Ok(
                    parse_marker(&bytes).map_or(Marker::Other, |(digest, instance)| {
                        Marker::Valid { digest, instance }
                    }),
                );
            }
        }
    }
    // No mtime, or one further ahead than the margin, does not keep the folder from being adopted forever.
    let fresh = modified.is_some_and(|modified| {
        let now = std::time::SystemTime::now();
        match now.duration_since(modified) {
            Ok(age) => age < STALE_EMPTY_MARKER,
            // Ahead of this clock: fresh while within the same margin.
            Err(ahead) => ahead.duration() < STALE_EMPTY_MARKER,
        }
    });
    Ok(Marker::Empty { stale: !fresh })
}

/// The digest and the instance id of a marker: the first lines starting with `digest ` and `instance `.
fn parse_marker(bytes: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let digest = marker_value(text, "digest ")?;
    let instance = Token::parse(marker_value(text, "instance ")?)?;
    Some((digest.to_owned(), instance.as_str().to_owned()))
}

/// The rest of the first line of the marker `text` that starts with `key`.
fn marker_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| line.strip_prefix(key))
}

/// The project's path in the marker `text`, its last line (see [`bind`]), when it can be looked up: not a path
/// that was not valid Unicode (written with replacement characters), nor one holding a line break (cut short
/// by the marker's lines).
fn marker_project(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("\nproject ")?;
    let path = rest.strip_suffix('\n')?;
    (!path.contains(['\n', '\r', '\u{FFFD}'])).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bisect::link::make_link;
    use crate::bisect::testing::{Project, state, write};
    use crate::bisect::{Logs, PythonEnvs, env_additions, write_boundaries};

    /// `path` written with `/`, as an absolute path of this system: on Windows on drive `C:`, with `\`.
    fn p(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", path.replace('/', "\\")))
        } else {
            PathBuf::from(path)
        }
    }

    fn os(path: &Path) -> Option<OsString> {
        Some(path.as_os_str().to_owned())
    }

    fn home() -> Vars {
        Vars {
            home: Some(p("/home/me")),
            ..Vars::default()
        }
    }

    #[test]
    fn the_cache_folder_of_each_system() {
        let folder = |system, vars: &Vars| yalper_folder(system, vars).unwrap();
        let joined = |base: &str, rest: &[&str]| {
            let mut path = p(base);
            path.extend(rest);
            path
        };
        assert_eq!(
            folder(System::Unix, &home()),
            joined("/home/me", &[".cache", "yalper"])
        );
        assert_eq!(
            folder(System::MacOs, &home()),
            joined("/home/me", &["Library", "Caches", "yalper"])
        );
        assert_eq!(
            folder(System::Windows, &home()),
            joined("/home/me", &["AppData", "Local", "yalper"])
        );

        // XDG_CACHE_HOME set, empty and relative; it means nothing on macOS.
        let xdg = |value: OsString| Vars {
            xdg_cache_home: Some(value),
            ..home()
        };
        let set = xdg(p("/xdg").into());
        assert_eq!(folder(System::Unix, &set), joined("/xdg", &["yalper"]));
        let mac = folder(System::MacOs, &set);
        assert_eq!(mac, joined("/home/me", &["Library", "Caches", "yalper"]));
        for ignored in ["", "relative/cache"] {
            assert_eq!(
                folder(System::Unix, &xdg(ignored.into())),
                joined("/home/me", &[".cache", "yalper"])
            );
        }

        // LOCALAPPDATA set, missing and relative.
        let local = |value: OsString| Vars {
            local_app_data: Some(value),
            ..home()
        };
        assert_eq!(
            folder(System::Windows, &local(p("/local").into())),
            joined("/local", &["yalper"])
        );
        assert_eq!(
            folder(System::Windows, &local("relative".into())),
            joined("/home/me", &["AppData", "Local", "yalper"])
        );

        // YALPER_CACHE_DIR replaces Yalper's folder on every system; empty, it counts as not set.
        for system in [System::Unix, System::MacOs, System::Windows] {
            let mine = Vars {
                yalper_cache_dir: os(&p("/mine")),
                ..local(p("/local").into())
            };
            assert_eq!(folder(system, &mine), p("/mine"));
            let empty = Vars {
                yalper_cache_dir: Some(OsString::new()),
                ..home()
            };
            assert_ne!(folder(system, &empty), PathBuf::new());
        }
    }

    #[test]
    fn a_relative_yalper_cache_dir_or_no_home_folder_is_refused() {
        for system in [System::Unix, System::MacOs, System::Windows] {
            let relative = Vars {
                yalper_cache_dir: Some("cache".into()),
                ..home()
            };
            let error = yalper_folder(system, &relative).unwrap_err();
            assert!(error.to_string().contains(CACHE_DIR_VAR), "{error}");

            for home in [None, Some(PathBuf::new()), Some(PathBuf::from("me"))] {
                let vars = Vars {
                    home,
                    // Relative, so ignored.
                    xdg_cache_home: Some("x".into()),
                    local_app_data: Some("x".into()),
                    ..Vars::default()
                };
                let error = yalper_folder(system, &vars).unwrap_err();
                assert!(error.to_string().contains("home folder"), "{error}");
                assert!(error.to_string().contains(CACHE_DIR_VAR), "{error}");
            }
        }
    }

    fn token(text: char) -> Token {
        Token::parse(&text.to_string().repeat(32)).unwrap()
    }

    #[test]
    fn slugs_are_short_and_plain() {
        assert_eq!(slug("My-App_2"), "my-app_2");
        assert_eq!(slug("con.x"), "con-x");
        assert_eq!(slug("..hidden"), "hidden");
        assert_eq!(slug("été"), "t-");
        assert_eq!(slug("日本"), "project");
        assert_eq!(slug(""), "project");
        assert_eq!(slug("-"), "project");
        assert_eq!(slug("a-very-long-project-name"), "a-very-long-proj");
    }

    /// Yalper's cache folder in a new temporary folder.
    fn base() -> (tempfile::TempDir, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let folder = base.path().join("yalper");
        (base, folder)
    }

    #[test]
    fn the_folder_name_depends_on_the_token_and_the_path_never_on_their_spelling() {
        let (_base, folder) = base();
        let project = tempfile::tempdir().unwrap();
        let named = project.path().join("Demo.App");
        fs::create_dir(&named).unwrap();
        let open = |path: &Path, token: &Token| CacheDir::open_in(&folder, path, token).unwrap();
        let cache = open(&named, &token('a'));
        let name = cache
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(name.starts_with("demo-app-"), "{name}");
        assert_eq!(name.len(), "demo-app-".len() + ID_HEX_DIGITS);
        assert!(!cache.path().to_string_lossy().contains(&"a".repeat(32)));
        let instance = cache.instance().to_owned();
        drop(cache);

        let mut spellings = vec![fs::canonicalize(&named).unwrap()];
        if CASE_INSENSITIVE {
            spellings.push(PathBuf::from(named.to_string_lossy().to_uppercase()));
        }
        #[cfg(windows)]
        spellings.extend(crate::bisect::testing::short_path(&named));
        for spelling in spellings {
            let again = open(&spelling, &token('a'));
            assert_eq!(
                again.path().file_name().unwrap(),
                name.as_str(),
                "{spelling:?}"
            );
            assert_eq!(again.instance(), instance);
        }

        // Another token, or the same token in another folder (a copied project): another folder.
        let other = open(&named, &token('b'));
        assert_ne!(other.path().file_name().unwrap(), name.as_str());
        let copied = project.path().join("elsewhere").join("Demo.App");
        fs::create_dir_all(&copied).unwrap();
        let copied = open(&copied, &token('a'));
        assert_ne!(copied.path(), other.path());
        assert_ne!(copied.path().file_name().unwrap(), name.as_str());
    }

    #[test]
    fn folders_are_private_and_marked() {
        let (_base, folder) = base();
        let project = Project::new();
        let cache = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
        assert!(fs::symlink_metadata(&folder).unwrap().is_dir());
        assert!(fs::symlink_metadata(cache.path()).unwrap().is_dir());
        #[cfg(unix)]
        for dir in [folder.as_path(), cache.path()] {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
        }
        let marker = fs::read_to_string(cache.path().join(MARKER_FILE)).unwrap();
        assert!(marker.contains(cache.instance()), "{marker}");
        assert!(
            marker.contains(
                &project
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            )
        );
        assert_eq!(parse_marker(marker.as_bytes()).unwrap().1, cache.instance());
    }

    #[test]
    fn an_empty_folder_is_adopted_and_any_other_refused() {
        let (_base, folder) = base();
        let project = Project::new();
        let cache = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
        let path = cache.path().to_owned();
        drop(cache);
        let open = || CacheDir::open_in(&folder, project.path(), &token('a'));

        // Its creation stopped before the marker was created, or before it was written.
        let marker = path.join(MARKER_FILE);
        fs::remove_file(&marker).unwrap();
        let instance = open().unwrap().instance().to_owned();
        // An empty marker just created: another bisect is starting.
        fs::write(&marker, "").unwrap();
        let error = open().unwrap_err();
        assert!(error.to_string().contains("another bisect"), "{error}");
        // Left empty for longer: an interrupted creation.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let adopted = open().unwrap();
        assert_ne!(adopted.instance(), instance);
        assert_eq!(open().unwrap().instance(), adopted.instance());
        drop(adopted);

        // Without the marker, or with a wrong one, or another kind of entry at its name, or an empty one next
        // to something else.
        let text = fs::read_to_string(&marker).unwrap();
        let wrong = text.replace("digest ", "digest 0");
        let cases: [&dyn Fn(); 4] = [
            &|| {
                fs::remove_file(&marker).unwrap();
                fs::write(path.join("other"), "").unwrap();
            },
            &|| fs::write(&marker, &wrong).unwrap(),
            &|| {
                fs::write(&marker, "").unwrap();
                fs::write(path.join("other"), "").unwrap();
            },
            &|| {
                fs::remove_file(&marker).unwrap();
                fs::create_dir(&marker).unwrap();
            },
        ];
        for (index, case) in cases.iter().enumerate() {
            case();
            let error = open().unwrap_err();
            assert!(
                error.to_string().contains("not created by Yalper"),
                "case {index}: {error}"
            );
            // Back to a good state for the next case.
            fs::remove_dir_all(&path).unwrap();
            open().unwrap();
        }
    }

    #[test]
    fn first_opens_that_race_share_one_marker() {
        let (_base, folder) = base();
        let project = tempfile::tempdir().unwrap();
        let instances: Vec<String> = std::thread::scope(|scope| {
            let opens: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        // A winner paused between creating and writing the marker makes the others report
                        // another bisect starting: they try again, as a user would.
                        for _ in 0..50 {
                            match CacheDir::open_in(&folder, project.path(), &token('a')) {
                                Ok(cache) => return cache.instance().to_owned(),
                                Err(error) if error.to_string().contains("another bisect") => {
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                }
                                Err(error) => panic!("{error}"),
                            }
                        }
                        panic!("the marker was never written");
                    })
                })
                .collect();
            opens.into_iter().map(|open| open.join().unwrap()).collect()
        });
        assert!(instances.iter().all(|instance| *instance == instances[0]));
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_others_can_change_above_the_cache_folder_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let project = Project::new();
        let (base, folder) = base();
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o777)).unwrap();
        // Right above Yalper's folder, or above folders still to be created.
        let deeper = base.path().join("a").join("b").join("yalper");
        for folder in [&folder, &deeper] {
            let error = CacheDir::open_in(folder, project.path(), &token('a')).unwrap_err();
            assert!(error.to_string().contains("every user"), "{error}");
            assert!(error.to_string().contains(CACHE_DIR_VAR), "{error}");
        }
        assert!(!base.path().join("a").exists());
        // With the sticky bit, as `/tmp`: others cannot rename or delete what they do not own.
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_writable_by_the_users_own_group_above_the_cache_folder_is_accepted() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let project = Project::new();
        let (base, folder) = base();
        // `~/.cache` with umask 002 and a user-private group (Ubuntu, Fedora).
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o775)).unwrap();
        let own_group =
            fs::metadata(base.path()).unwrap().gid() == rustix::process::getegid().as_raw();
        let opened = CacheDir::open_in(&folder, project.path(), &token('a'));
        if own_group && !cfg!(target_os = "macos") {
            opened.unwrap();
        } else {
            // On macOS every user's primary group is `staff`, and a folder may also take the group of the
            // folder above it (BSD rule).
            let error = opened.unwrap_err().to_string();
            assert!(error.contains("writable by its group"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn folders_that_let_another_user_replace_what_they_hold_are_unsafe() {
        let (me, group, other) = (1000, 1000, 1001);
        let why = |mode, uid, gid| unsafe_folder(mode, uid, gid, me, group);
        assert_eq!(why(0o40700, me, group), None);
        assert_eq!(why(0o40755, 0, 0), None);
        assert_eq!(why(0o41777, 0, 0), None);
        assert_eq!(why(0o41775, me, other), None);
        // The user's own folder writable by the user's own group: Linux user-private groups, not macOS.
        let own = why(0o40775, me, group);
        assert_eq!(own.is_none(), !cfg!(target_os = "macos"), "{own:?}");
        assert!(why(0o40700, other, group).unwrap().contains("another user"));
        assert!(why(0o41777, other, other).unwrap().contains("another user"));
        assert!(why(0o40777, me, group).unwrap().contains("every user"));
        assert!(why(0o40775, me, other).unwrap().contains("group"));
        // `root:users 2775`: owned by root, writable by a group that holds other users.
        assert!(why(0o42775, 0, group).unwrap().contains("group"));
        assert!(why(0o40775, 0, other).unwrap().contains("group"));
    }

    #[test]
    fn a_folder_outside_the_profile_folder_gets_a_warning() {
        assert_eq!(
            access_warning(&p("/Users/me/AppData/Local/yalper"), Some(&p("/Users/me"))),
            None
        );
        for (folder, profile) in [
            (p("/cache/yalper"), Some(p("/Users/me"))),
            (p("/Users/me-other/yalper"), Some(p("/Users/me"))),
            (p("/Users/me/yalper"), None),
        ] {
            let warning = access_warning(&folder, profile.as_deref()).unwrap();
            assert!(warning.contains("other users"), "{warning}");
            assert!(warning.contains(CACHE_DIR_VAR), "{warning}");
        }
    }

    #[test]
    fn a_link_at_either_folder_is_refused_and_never_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep.txt", "keep me");
        let before = state(canary.path());

        let (_base, folder) = base();
        make_link(canary.path(), &folder).unwrap();
        assert!(CacheDir::open_in(&folder, project.path(), &token('a')).is_err());

        let (_base, folder) = base();
        let cache = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
        let path = cache.path().to_owned();
        drop(cache);
        fs::remove_dir_all(&path).unwrap();
        make_link(canary.path(), &path).unwrap();
        assert!(CacheDir::open_in(&folder, project.path(), &token('a')).is_err());
        assert_eq!(state(canary.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_others_can_write_to_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let project = Project::new();
        let (_base, folder) = base();
        let cache = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
        let path = cache.path().to_owned();
        drop(cache);
        for dir in [&path, &folder] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o770)).unwrap();
            let error = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap_err();
            assert!(error.to_string().contains("other users"), "{error}");
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
    }

    #[test]
    fn a_folder_inside_the_project_is_refused_before_anything_is_written() {
        // The project is the home folder: `~/.cache` is inside it.
        let project = Project::new();
        let before = state(project.path());
        let vars = Vars {
            home: Some(project.path().to_owned()),
            ..Vars::default()
        };
        for system in [System::Unix, System::MacOs, System::Windows] {
            let folder = yalper_folder(system, &vars).unwrap();
            let error = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap_err();
            assert!(error.to_string().contains("inside"), "{error}");
            assert!(error.to_string().contains(CACHE_DIR_VAR), "{error}");
        }
        // Spelled with another case where the file system ignores it.
        if CASE_INSENSITIVE {
            let upper = PathBuf::from(project.path().to_string_lossy().to_uppercase());
            let error = CacheDir::open_in(&upper.join("c"), project.path(), &token('a'));
            assert!(error.unwrap_err().to_string().contains("inside"));
        }
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn a_folder_that_is_or_holds_the_project_is_refused() {
        assert!(
            overlap(&p("/proj"), &p("/proj"))
                .unwrap()
                .contains("inside")
        );
        assert!(
            overlap(&p("/proj/x"), &p("/proj"))
                .unwrap()
                .contains("inside")
        );
        assert!(overlap(&p("/c"), &p("/c/proj")).unwrap().contains("holds"));
        assert_eq!(overlap(&p("/c/x"), &p("/c/proj")), None);
        assert_eq!(overlap(&p("/proj-x"), &p("/proj")), None);
        assert_eq!(
            overlap(&p("/PROJ/x"), &p("/proj")).is_some(),
            CASE_INSENSITIVE
        );
    }

    #[test]
    fn a_relative_folder_is_refused() {
        let project = Project::new();
        let error = CacheDir::open_in(Path::new("cache"), project.path(), &token('a'));
        assert!(error.unwrap_err().to_string().contains(CACHE_DIR_VAR));
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_on_a_network_share_is_refused_without_looking_it_up() {
        let project = Project::new();
        for folder in [
            r"\\server\share\yalper",
            r"\\?\UNC\server\share\yalper",
            r"\\.\pipe\yalper",
        ] {
            let error = CacheDir::open_in(Path::new(folder), project.path(), &token('a'));
            let error = error.unwrap_err().to_string();
            assert!(error.contains("network"), "{error}");
            assert!(error.contains(CACHE_DIR_VAR), "{error}");
        }
    }

    #[test]
    fn the_root_keeps_only_what_yalper_puts_there() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep.txt", "keep me");
        let before = state(canary.path());
        let root = project.cache.path().to_owned();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("a.txt", "")]))
            .unwrap();
        write_boundaries(&project.cache).unwrap();
        Logs::open(&project.cache).unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        env_additions(&copy, &python).unwrap();
        write(&root, "lock", "");
        let kept: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        write(&root, "node_modules/.package-lock.json", "{}");
        make_link(canary.path(), &root.join("node_modules").join("canary")).unwrap();
        write(&root, "conftest.py", "");
        write(&root, ".cargo/config.toml", "");
        write(&root, "pnpm-workspace.yaml", "");
        write(&root, "no-cargo-toml-in-this-step.rs", "");
        write(&root, ".venv/pyvenv.cfg", "");
        write(&root, "Cargo.lock", "");
        write(&root, "copy-x/a.txt", "");
        write(&root, "copy-/a.txt", "");
        make_link(canary.path(), &root.join("copy-7")).unwrap();
        // An allowed name of the wrong kind.
        fs::remove_dir_all(root.join(LOGS_DIR)).unwrap();
        make_link(canary.path(), &root.join(LOGS_DIR)).unwrap();

        assert_eq!(clean_root(&project.cache).unwrap(), 11);
        let mut left: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        let mut expected: Vec<_> = kept.into_iter().filter(|name| name != LOGS_DIR).collect();
        expected.sort();
        assert_eq!(left, expected);
        assert!(root.join("copy-0").join("a.txt").is_file());
        assert_eq!(state(canary.path()), before);
        assert_eq!(clean_root(&project.cache).unwrap(), 0);
    }

    /// A project set up as `yalper init` leaves it (only its init token files matter here) with `token`.
    fn set_up(token: &Token) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir(root.path().join(YALPER_DIR)).unwrap();
        set_token(root.path(), token);
        root
    }

    fn set_token(root: &Path, token: &Token) {
        fs::write(root.join(".git").join(repo::GIT_ID_FILE), token.as_str()).unwrap();
        fs::write(root.join(YALPER_DIR).join(repo::ID_FILE), token.as_str()).unwrap();
    }

    /// The cache folder of the project at `root` in Yalper's cache folder `folder`, created and closed (on
    /// Windows a folder held open cannot be deleted).
    fn cache_of(folder: &Path, root: &Path, token: &Token) -> PathBuf {
        CacheDir::open_in(folder, root, token)
            .unwrap()
            .path()
            .to_owned()
    }

    /// Makes the file at `path` look last changed `days` ago.
    fn last_changed(path: &Path, days: u64) {
        let then = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(then)
            .unwrap();
    }

    #[test]
    fn folders_of_projects_that_are_gone_are_deleted_after_7_days_unused() {
        let (_base, folder) = base();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep.txt", "keep me");
        let before = state(canary.path());
        let old = |cache: &Path| last_changed(&cache.join(MARKER_FILE), 8);

        // A deleted project, last used 8 days ago, whose folder links to a canary outside.
        let deleted = set_up(&token('b'));
        let deleted_cache = cache_of(&folder, deleted.path(), &token('b'));
        write(&deleted_cache, CLOCK_FILE, "x");
        last_changed(&deleted_cache.join(CLOCK_FILE), 8);
        old(&deleted_cache);
        make_link(canary.path(), &deleted_cache.join("copy-0")).unwrap();
        drop(deleted);

        // A project set up again with `yalper init` (a new token): its old folder, with no clock, was created
        // 8 days ago.
        let again = set_up(&token('c'));
        let again_cache = cache_of(&folder, again.path(), &token('c'));
        old(&again_cache);
        set_token(again.path(), &token('d'));

        // A deleted project used a day ago.
        let recent = set_up(&token('e'));
        let recent_cache = cache_of(&folder, recent.path(), &token('e'));
        write(&recent_cache, CLOCK_FILE, "x");
        last_changed(&recent_cache.join(CLOCK_FILE), 1);
        old(&recent_cache);
        drop(recent);

        // A project still there, unused for 8 days.
        let kept = set_up(&token('f'));
        let kept_cache = cache_of(&folder, kept.path(), &token('f'));
        old(&kept_cache);

        // A folder without a marker, and one whose marker was copied from another folder.
        let unmarked = folder.join("unmarked-0123456789ab");
        write(&unmarked, "copy-0/a.txt", "");
        write(&unmarked, CLOCK_FILE, "x");
        last_changed(&unmarked.join(CLOCK_FILE), 8);
        let copied = folder.join("copied-0123456789ab");
        fs::create_dir(&copied).unwrap();
        fs::copy(again_cache.join(MARKER_FILE), copied.join(MARKER_FILE)).unwrap();
        old(&copied);

        // A link in Yalper's cache folder to the stale folder of a deleted project elsewhere.
        let (_elsewhere, other_folder) = base();
        let linked = set_up(&token('9'));
        let linked_cache = cache_of(&other_folder, linked.path(), &token('9'));
        old(&linked_cache);
        drop(linked);
        let link = folder.join(linked_cache.file_name().unwrap());
        make_link(&linked_cache, &link).unwrap();
        let linked_before = state(&linked_cache);

        // The current project's folder, as old as the others, after its `.yalper/` was deleted.
        let current_project = set_up(&token('a'));
        let current = CacheDir::open_in(&folder, current_project.path(), &token('a')).unwrap();
        old(current.path());
        fs::remove_dir_all(current_project.path().join(YALPER_DIR)).unwrap();

        let mut removed = remove_stale(&current);
        removed.sort();
        let mut expected = vec![deleted_cache, again_cache];
        expected.sort();
        assert_eq!(removed, expected);
        for gone in &expected {
            assert!(!gone.exists(), "{}", gone.display());
        }
        for left in [&recent_cache, &kept_cache, &unmarked, &copied] {
            assert!(left.join(MARKER_FILE).exists() || left == &unmarked);
            assert!(left.is_dir(), "{}", left.display());
        }
        assert!(current.path().join(MARKER_FILE).is_file());
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(state(&linked_cache), linked_before);
        assert_eq!(state(canary.path()), before);
        assert!(remove_stale(&current).is_empty());
    }

    #[test]
    fn the_recorded_folder_and_the_computed_one_are_found_once_each() {
        let (_base, folder) = base();
        let project = set_up(&token('a'));
        let found = |recorded: Option<&str>| {
            find_in(Ok(folder.clone()), project.path(), &token('a'), recorded)
        };
        assert!(found(None).ours.is_empty());
        assert!(found(None).others.is_empty());

        let cache = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
        let (path, meta) = (cache.path().to_owned(), cache.meta_value());
        drop(cache);
        let paths = |found: ProjectFolders| -> Vec<PathBuf> {
            found.ours.iter().map(|f| f.path().to_owned()).collect()
        };
        assert_eq!(paths(found(None)), [path.as_path()]);
        // Recorded where the variables put it too: the same folder, once.
        assert_eq!(paths(found(Some(&meta))), [path.as_path()]);
        // Recorded in another cache folder, with the variables now giving none.
        let other = find_in(
            Err(io::Error::other("no home")),
            project.path(),
            &token('a'),
            Some(&meta),
        );
        assert_eq!(paths(other), [path.as_path()]);
        let unknown = find_in(
            Err(io::Error::other("no home")),
            project.path(),
            &token('a'),
            None,
        );
        assert_eq!(unknown.unknown.as_deref(), Some("no home"));
        // Values a bisect never writes are ignored.
        let replaced = serde_json::json!({
            "path": p("/cache/yalper/x\u{FFFD}-0123456789ab"),
            "instance": "a".repeat(32),
        })
        .to_string();
        for bad in [
            "",
            "{}",
            r#"{"path": "relative", "instance": "x"}"#,
            "[1]",
            &replaced,
        ] {
            assert!(parse_meta_value(bad).is_none(), "{bad}");
        }
        assert_eq!(parse_meta_value(&meta).unwrap().0, path);
    }

    #[test]
    fn a_moved_project_finds_its_recorded_folder_by_its_instance_id() {
        let (_base, folder) = base();
        let parent = tempfile::tempdir().unwrap();
        let project = set_up(&token('a'));
        let first = parent.path().join("first");
        fs::rename(project.path(), &first).unwrap();
        let cache = CacheDir::open_in(&folder, &first, &token('a')).unwrap();
        let (path, meta) = (cache.path().to_owned(), cache.meta_value());
        drop(cache);
        let moved = parent.path().join("moved");
        fs::rename(&first, &moved).unwrap();

        let found = find_in(Ok(folder.clone()), &moved, &token('a'), Some(&meta));
        assert_eq!(found.ours.len(), 1);
        assert_eq!(found.ours[0].path(), path);
        // Without the record, the folder's marker names another path: it is not this project's.
        assert!(
            find_in(Ok(folder), &moved, &token('a'), None)
                .ours
                .is_empty()
        );
    }

    #[test]
    fn an_empty_folder_is_the_projects_and_a_link_or_foreign_folder_is_not() {
        let (_base, folder) = base();
        let project = set_up(&token('a'));
        let path = cache_of(&folder, project.path(), &token('a'));
        let found = || find_in(Ok(folder.clone()), project.path(), &token('a'), None);

        // What an interrupted deletion leaves once the marker is gone.
        fs::remove_file(path.join(MARKER_FILE)).unwrap();
        let empty = found();
        assert_eq!(empty.ours.len(), 1);
        empty.ours.into_iter().next().unwrap().delete().unwrap();
        assert!(!path.exists());

        // A link at its place is never followed.
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "project", "digest x\n");
        make_link(canary.path(), &path).unwrap();
        let linked = found();
        assert!(linked.ours.is_empty());
        assert!(linked.others[0].1.contains("link"), "{:?}", linked.others);
        remove_entry(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();
        assert!(canary.path().join("project").is_file());

        // A folder with something in it and no marker.
        write(&path, "a.txt", "");
        let foreign = found();
        assert!(foreign.ours.is_empty());
        assert!(foreign.others[0].1.contains("not created by Yalper"));
    }

    #[cfg(windows)]
    #[test]
    fn a_recorded_folder_on_a_network_share_is_never_looked_up() {
        let (_base, folder) = base();
        let project = set_up(&token('a'));
        let cache = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
        let (path, instance) = (cache.path().to_owned(), cache.instance().to_owned());
        drop(cache);
        // The project's own folder spelled as a device path. Looked up, it would be the project's (its marker
        // matches), and the folder the variables give would be skipped as the same folder: so finding it only
        // in `others`, next to the computed folder, shows it was never looked up.
        let device = format!(r"\\.\{}", path.display());
        assert!(fs::metadata(&device).is_ok_and(|metadata| metadata.is_dir()));
        for recorded in [device.as_str(), r"\\server\share\yalper\x-0123456789ab"] {
            let meta = serde_json::json!({"path": recorded, "instance": instance}).to_string();
            let found = find_in(Ok(folder.clone()), project.path(), &token('a'), Some(&meta));
            assert_eq!(found.others.len(), 1, "{:?}", found.others);
            assert_eq!(found.others[0].0, Path::new(recorded));
            assert!(found.others[0].1.contains("network"), "{:?}", found.others);
            assert_eq!(found.ours.len(), 1);
            assert_eq!(found.ours[0].path(), path);
        }
    }

    #[test]
    fn a_copy_of_a_project_never_takes_the_originals_recorded_folder() {
        let (_base, folder) = base();
        let original = set_up(&token('a'));
        let cache = CacheDir::open_in(&folder, original.path(), &token('a')).unwrap();
        let (path, meta) = (cache.path().to_owned(), cache.meta_value());
        drop(cache);
        // Same token, same record, another path.
        let copy = set_up(&token('a'));
        let found = find_in(Ok(folder.clone()), copy.path(), &token('a'), Some(&meta));
        assert!(found.ours.is_empty());
        assert_eq!(found.others.len(), 1);
        assert_eq!(found.others[0].0, path);
        assert!(
            found.others[0].1.contains("belongs to the project at"),
            "{:?}",
            found.others
        );
        // Once the original is gone, the folder is the copy's to delete.
        drop(original);
        let found = find_in(Ok(folder), copy.path(), &token('a'), Some(&meta));
        assert_eq!(found.ours.len(), 1);
    }

    #[test]
    fn a_recorded_folder_must_be_named_as_yalper_names_folders() {
        let (_base, folder) = base();
        let project = set_up(&token('a'));
        let cache = CacheDir::open_in(&folder, project.path(), &token('a')).unwrap();
        let (path, instance) = (cache.path().to_owned(), cache.instance().to_owned());
        drop(cache);
        let elsewhere = tempfile::tempdir().unwrap();
        let found = |recorded: &Path| {
            let meta = serde_json::json!({"path": recorded, "instance": instance}).to_string();
            find_in(
                Err(io::Error::other("no home")),
                project.path(),
                &token('a'),
                Some(&meta),
            )
        };

        // An empty folder, then one holding a copy of the project's marker, under another name.
        let victim = elsewhere.path().join("victim");
        fs::create_dir(&victim).unwrap();
        let empty = found(&victim);
        assert!(empty.ours.is_empty());
        assert!(empty.others[0].1.contains("not created by Yalper"));
        fs::copy(path.join(MARKER_FILE), victim.join(MARKER_FILE)).unwrap();
        write(&victim, "keep.txt", "keep me");
        let marked = found(&victim);
        assert!(marked.ours.is_empty());
        assert!(marked.others[0].1.contains("not created by Yalper"));

        // An empty folder with the name Yalper gives: a deletion of the recorded folder interrupted.
        let named = elsewhere.path().join(path.file_name().unwrap());
        fs::create_dir(&named).unwrap();
        assert_eq!(found(&named).ours.len(), 1);
    }

    #[test]
    fn a_folder_whose_creation_was_interrupted_does_not_stay_forever() {
        let (_base, folder) = base();
        let project = set_up(&token('a'));
        let path = cache_of(&folder, project.path(), &token('a'));
        let found = || find_in(Ok(folder.clone()), project.path(), &token('a'), None);

        // Only an empty marker, just created: another bisect is starting.
        fs::write(path.join(MARKER_FILE), "").unwrap();
        let fresh = found();
        assert!(fresh.ours.is_empty());
        assert!(fresh.others[0].1.contains("not created by Yalper"));
        drop(fresh);
        // Left empty for longer than 5 seconds: purge takes it.
        last_changed(&path.join(MARKER_FILE), 1);
        assert_eq!(found().ours.len(), 1);

        // Stale cleanup takes it once unused for 7 days.
        let current_project = set_up(&token('b'));
        let current = CacheDir::open_in(&folder, current_project.path(), &token('b')).unwrap();
        assert!(remove_stale(&current).is_empty());
        assert!(path.join(MARKER_FILE).is_file());
        last_changed(&path.join(MARKER_FILE), 8);
        assert_eq!(remove_stale(&current), [path.as_path()]);
        assert!(!path.exists());
    }

    #[test]
    fn only_a_project_that_is_found_tells_that_it_gives_another_name() {
        let (_base, folder) = base();
        let project = set_up(&token('a'));
        let path = cache_of(&folder, project.path(), &token('a'));
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let real = plain(&fs::canonicalize(project.path()).unwrap());
        let gives = || gives_name(&real, &name);
        assert_eq!(gives(), Some(true));
        assert_eq!(gives_name(&real, "other-0123456789ab"), Some(false));

        // Tokens that differ, or one missing: set up again, or purged.
        let own = real.join(YALPER_DIR).join(repo::ID_FILE);
        fs::write(&own, token('b').as_str()).unwrap();
        assert_eq!(gives(), Some(false));
        fs::remove_file(&own).unwrap();
        assert_eq!(gives(), Some(false));
        // A token file that cannot be read as one: cannot be told.
        fs::write(&own, "not a token").unwrap();
        assert_eq!(gives(), None);
        set_token(&real, &token('a'));
        assert_eq!(gives(), Some(true));

        // A `.yalper` link, or a `.git` file naming a git directory elsewhere, is never followed.
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(
            elsewhere.path().join(repo::GIT_ID_FILE),
            token('a').as_str(),
        )
        .unwrap();
        fs::write(elsewhere.path().join(repo::ID_FILE), token('a').as_str()).unwrap();
        let yalper = real.join(YALPER_DIR);
        fs::rename(&yalper, real.join("kept")).unwrap();
        make_link(elsewhere.path(), &yalper).unwrap();
        assert_eq!(gives(), None);
        remove_entry(&yalper, &fs::symlink_metadata(&yalper).unwrap()).unwrap();
        fs::rename(real.join("kept"), &yalper).unwrap();
        assert_eq!(gives(), Some(true));
        let git = real.join(".git");
        fs::remove_dir_all(&git).unwrap();
        fs::write(&git, format!("gitdir: {}\n", elsewhere.path().display())).unwrap();
        assert_eq!(gives(), None);
        fs::remove_file(&git).unwrap();
        assert_eq!(gives(), Some(false));

        // Gone, relative, or on a network share.
        drop(project);
        assert_eq!(gives(), Some(false));
        assert_eq!(gives_name(Path::new("relative"), &name), None);
        #[cfg(windows)]
        assert_eq!(gives_name(Path::new(r"\\server\share\p"), &name), None);
    }

    #[test]
    fn a_marker_path_that_cannot_be_looked_up_is_not_read() {
        let marker = |path: &str| format!("text\ndigest d\ninstance i\nproject {path}\n");
        assert_eq!(marker_project(&marker("/a/b")), Some("/a/b"));
        assert_eq!(marker_project(&marker("/a\nb")), None);
        assert_eq!(marker_project(&marker("/a\rb")), None);
        assert_eq!(marker_project(&marker("/a\u{FFFD}b")), None);
        assert_eq!(marker_project("digest d\ninstance i\n"), None);
    }
}
