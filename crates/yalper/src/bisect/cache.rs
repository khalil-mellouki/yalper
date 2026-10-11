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

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::boundary;
use super::copy::{CARGO_TARGET_DIR, CLOCK_FILE, PYCACHE_DIR, open_private_dir, remove_entry};
use super::link::{has_special_prefix, is_under, normalize, plain};
use super::run::LOGS_DIR;
use crate::repo::Token;
use crate::safe_fs::{self, Access, OwnedDir};
use crate::snapshot::CASE_INSENSITIVE;

/// The variable that names Yalper's cache folder, in place of `<cache>/yalper/`.
pub const CACHE_DIR_VAR: &str = "YALPER_CACHE_DIR";

/// The marker at the top of a project's folder: see [`CacheDir`].
const MARKER_FILE: &str = "project";

/// The lock that lets one bisect of a project run at a time.
const LOCK_FILE: &str = "lock";

/// A marker larger than this was not written by Yalper (it holds a digest, an id and a path).
const MAX_MARKER_BYTES: u64 = 64 * 1024;

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
        check_place(folder, &name, &project_real)?;

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
        let dir = open_private_dir(&yalper.path().join(&name))?;
        let instance = bind(&dir, &digest, &project_real)?;
        Ok(Self {
            dir,
            given,
            instance,
            project: project.to_owned(),
        })
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

fn refusal(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Refuses Yalper's cache folder `folder` for the project at the canonical path `project` when the project's
/// folder `name` in it would be the project, inside it, or hold it, or, on Windows, when `folder` is on a
/// network share. Paths are compared once resolved (see [`resolve`]), without regard to case where the file
/// system ignores it.
fn check_place(folder: &Path, name: &str, project: &Path) -> io::Result<()> {
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
    Ok(())
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

/// Checks the marker of the project's cache folder `dir`, or writes it if the folder is empty, and returns the
/// instance id it holds.
fn bind(dir: &OwnedDir, digest: &str, project: &Path) -> io::Result<String> {
    let marker = dir.path().join(MARKER_FILE);
    let not_ours = || {
        io::Error::other(format!(
            "{} was not created by Yalper for this project; delete it by hand",
            dir.path().display()
        ))
    };
    match fs::symlink_metadata(&marker) {
        Ok(_) => {
            let bytes = safe_fs::read_small_regular_file(&marker, MAX_MARKER_BYTES);
            return match bytes.as_deref().and_then(parse_marker) {
                Some((found, instance)) if found == digest => Ok(instance),
                _ => Err(not_ours()),
            };
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if fs::read_dir(dir.path())?.next().is_some() {
        return Err(not_ours());
    }
    let instance = Token::generate()?.as_str().to_owned();
    let text = format!(
        "Bisect data of a project, kept by Yalper. This folder can be deleted at any time.\n\
         digest {digest}\ninstance {instance}\nproject {}\n",
        project.display()
    );
    dir.open_file(MARKER_FILE, Access::ReadWrite)?
        .write_all(text.as_bytes())?;
    Ok(instance)
}

/// The digest and the instance id of a marker: the first lines starting with `digest ` and `instance `.
fn parse_marker(bytes: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::to_owned)
    };
    let digest = value("digest ")?;
    let instance = Token::parse(&value("instance ")?)?;
    Some((digest, instance.as_str().to_owned()))
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

        // Emptied, as if its creation had stopped before the marker.
        fs::remove_file(path.join(MARKER_FILE)).unwrap();
        open().unwrap();

        // Without the marker, or with a wrong one, or another kind of entry at its name.
        let marker = path.join(MARKER_FILE);
        let text = fs::read_to_string(&marker).unwrap();
        let wrong = text.replace("digest ", "digest 0");
        let cases: [&dyn Fn(); 4] = [
            &|| {
                fs::remove_file(&marker).unwrap();
                fs::write(path.join("other"), "").unwrap();
            },
            &|| fs::write(&marker, &wrong).unwrap(),
            &|| fs::write(&marker, "").unwrap(),
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
}
