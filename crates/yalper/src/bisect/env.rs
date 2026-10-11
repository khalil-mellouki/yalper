//! The environment variables a test run gets on top of the user's, so that the tools it runs reuse the
//! project's dependencies without writing into the project:
//!
//! - `PYTHONPATH` starts with the copy's own folders of the project's editable installs (see
//!   [`PythonEnvs::python_path`]);
//! - `PYTHONPYCACHEPREFIX` sends Python's bytecode to the copy's folder `.yalper/bisect/pycache/copy-<n>/`
//!   (Python 3.8 and later), so none is written into the project's virtual environment through the copy's
//!   link to it;
//! - `UV_NO_SYNC=1` stops `uv run` from syncing, and so writing into, that environment;
//! - `CARGO_TARGET_DIR` is the copy's folder `.yalper/bisect/cargo-target/copy-<n>/`, reused by every step and
//!   every bisect of that copy: the project's `target/` is never used, and cargo's own fingerprints decide
//!   what to rebuild. The first bisect of a Rust project compiles its dependencies there once per copy (see
//!   [`TestCopy::cargo_target_path`] for why copies do not share one). `CARGO_BUILD_BUILD_DIR` names the same
//!   folder, for cargo's intermediate build files.
//!
//! [`run_env`] builds the complete environment of a run from the user's, so that the tools it runs stay in the
//! copy even though the copy sits inside the project (see also [`write_boundaries`](super::write_boundaries)):
//!
//! - the variables that tell git which repository to use are removed, and `GIT_CEILING_DIRECTORIES` stops git
//!   from searching for one above the copy: a test running `git stash` or `git clean` fails with "not a git
//!   repository" instead of acting on the user's repository;
//! - every variable whose value is a path into the project, or a list of paths holding one (an activated
//!   virtual environment in `PATH` and `VIRTUAL_ENV`, `NODE_PATH`, `UV_PROJECT_ENVIRONMENT`, npm's `INIT_CWD`),
//!   points to the same place in the copy instead.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::copy::remove_entry;
use super::link::{has_special_prefix, is_under, normalize, plain};
use super::{BISECT_DIR, PythonEnvs, TestCopy};
use crate::hook::YALPER_DIR;
use crate::snapshot::CASE_INSENSITIVE;

/// The variables that tell git which repository, index or objects to use, removed from the environment of a
/// test run: the ones git itself clears before it runs a command in another repository (`local_repo_env` in
/// git's `environment.c`), and `GIT_NAMESPACE`. `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG_COUNT` (configuration
/// given with `git -c`) go with them, as git does, since they were meant for the repository Yalper was started
/// from. The user's and the system's configuration files (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`) are kept:
/// no configuration makes git find a repository that `GIT_CEILING_DIRECTORIES` hides.
const GIT_REPOSITORY_VARS: [&str; 16] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

const GIT_CEILING_DIRECTORIES: &str = "GIT_CEILING_DIRECTORIES";

/// The longest `PYTHONPATH` a run may get, name and `=` included: the limit of one environment variable on
/// Windows, in characters (counted here in bytes, which are never fewer). Each virtual environment adds at most
/// 256 folders, so several large ones could pass it.
const MAX_PYTHONPATH: usize = 32_767;

/// What the environment of a test run adds to the user's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvAdditions {
    /// Folders to put first in `PYTHONPATH`, in this order, before what the user's `PYTHONPATH` holds.
    pub python_path: Vec<PathBuf>,
    /// Variables to set, in place of the user's values.
    pub vars: Vec<(&'static str, OsString)>,
}

/// The environment additions for a test run in `copy`, whose Python environments are `python`. Call it
/// before every run, after [`PythonEnvs::link`]: it also makes sure that the folders the variables name are
/// real folders inside `.yalper/bisect/`, since a test run could have put a link in their place.
pub fn env_additions(copy: &TestCopy, python: &PythonEnvs) -> io::Result<EnvAdditions> {
    copy.bisect_dir().check_still_at_path()?;
    let pycache = plain(&std::path::absolute(copy.pycache_path())?);
    let cargo_target = plain(&std::path::absolute(copy.cargo_target_path())?);
    for folder in [&pycache, &cargo_target] {
        // `.yalper/bisect/pycache/` or `.yalper/bisect/cargo-target/` first.
        if let Some(parent) = folder.parent() {
            make_real_dir(parent)?;
        }
        make_real_dir(folder)?;
    }
    Ok(EnvAdditions {
        python_path: python.python_path(copy)?,
        vars: vec![
            ("PYTHONPYCACHEPREFIX", pycache.into_os_string()),
            ("UV_NO_SYNC", OsString::from("1")),
            ("CARGO_TARGET_DIR", cargo_target.clone().into_os_string()),
            // Cargo's intermediate build files go there too: a relative `build-dir` in a `.cargo/config.toml`
            // that cargo reads from the project (it reads those of every folder above) would be resolved
            // against the project.
            ("CARGO_BUILD_BUILD_DIR", cargo_target.into_os_string()),
        ],
    })
}

/// Makes `path` a real folder: kept if it is one, otherwise whatever is there (a link a test put in its place,
/// a file) is removed, never followed, and the folder is created, on Unix with mode 0700.
fn make_real_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(metadata) => remove_entry(path, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg_attr(windows, allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot create {}: {error}", path.display()),
        )
    })
}

/// The complete environment of a test run in `copy` of the project at `project`, for [`run`](super::run):
/// the user's environment `user` (usually `std::env::vars_os()`) with
///
/// - the git variables of [`GIT_REPOSITORY_VARS`] removed, and `GIT_CEILING_DIRECTORIES` set to
///   `.yalper/bisect/` and to the project folder: git never searches above the copy for a repository, and a
///   command run one folder above the copy does not reach the project either;
/// - every other variable whose value is a path into the project, outside `.yalper/`, or a list of paths
///   holding one, pointing to the same place in the copy (see [`Redirect`]); on Windows, the hidden variables
///   holding the current folder of each drive (`=C:`) are left out when they point into the project;
/// - `PYTHONPATH` starting with the folders of `additions`, and the variables of `additions` in place of the
///   user's.
///
/// Fails if `PYTHONPATH` would be longer than [`MAX_PYTHONPATH`]; if the project's path holds the separator of
/// path lists (`;` on Windows, `:` elsewhere), since git would then read a wrong `GIT_CEILING_DIRECTORIES`; and
/// if the project is the user's home folder or holds it, since tools keep their settings and caches there.
pub fn run_env(
    user: impl IntoIterator<Item = (OsString, OsString)>,
    project: &Path,
    copy: &TestCopy,
    additions: EnvAdditions,
) -> io::Result<Vec<(OsString, OsString)>> {
    let project = forms(project)?;
    let separator = if cfg!(windows) { ';' } else { ':' };
    if let Some(path) = project
        .iter()
        .find(|path| path.to_string_lossy().contains(separator))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cannot keep the test runs inside the test copy: the project's path holds {separator:?}, which \
                 cannot be written in a list of paths for git: {}",
                path.display()
            ),
        ));
    }
    let ceilings: Vec<PathBuf> = project
        .iter()
        .flat_map(|root| [root.join(YALPER_DIR).join(BISECT_DIR), root.clone()])
        .collect();
    let ceilings = join(&ceilings)?;
    let redirect = Redirect {
        project,
        copy: plain(&std::path::absolute(copy.path())?),
    };

    let mut env = Vec::new();
    let mut python_path: Vec<PathBuf> = additions.python_path;
    let mut python_path_name = OsString::from("PYTHONPATH");
    for (name, value) in user {
        let replaced = GIT_REPOSITORY_VARS
            .iter()
            .chain([&GIT_CEILING_DIRECTORIES])
            .chain(additions.vars.iter().map(|(name, _)| name))
            .any(|var| same_name(&name, var));
        if replaced {
            continue;
        }
        if ["HOME", "USERPROFILE"]
            .iter()
            .any(|home| same_name(&name, home))
            && !value.is_empty()
            && redirect.relative(Path::new(&value)).is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot bisect in your home folder or a folder that holds it ({}): tools keep their \
                     settings and caches there, so test runs could not be kept out of the project",
                    Path::new(&value).display()
                ),
            ));
        }
        if cfg!(windows) && name.to_string_lossy().starts_with('=') {
            // `=C:` and the like: the current folder of a drive, for paths such as `C:file`.
            if redirect.relative(Path::new(&value)).is_none() {
                env.push((name, value));
            }
        } else if same_name(&name, "PYTHONPATH") {
            // An empty `PYTHONPATH` holds no folder.
            if !value.is_empty() {
                python_path.extend(redirect.entries(&value));
            }
            python_path_name = name;
        } else {
            let value = redirect.list(&value)?;
            env.push((name, value));
        }
    }
    if !python_path.is_empty() {
        let value = join(&python_path)?;
        let length = python_path_name.len() + 1 + value.len();
        if length > MAX_PYTHONPATH {
            return Err(io::Error::other(format!(
                "cannot start the test run: its PYTHONPATH (the folders of the editable installs in the \
                 project's virtual environments, then your own PYTHONPATH) would be {length} characters \
                 long, over the limit of {MAX_PYTHONPATH} that Yalper keeps to on every system"
            )));
        }
        env.push((python_path_name, value));
    }
    env.push((GIT_CEILING_DIRECTORIES.into(), ceilings));
    env.extend(
        additions
            .vars
            .into_iter()
            .map(|(name, value)| (name.into(), value)),
    );
    Ok(env)
}

/// Whether the variable name `name` is `var`, without regard to case on Windows, where names ignore it.
fn same_name(name: &OsStr, var: &str) -> bool {
    if cfg!(windows) {
        name.eq_ignore_ascii_case(var)
    } else {
        name == var
    }
}

/// `path` made absolute and, if it exists, with every link resolved, both without Windows' `\\?\` prefix: paths
/// in the environment may be written in either form (on macOS, `/var` is a link to `/private/var`; on Windows,
/// a short 8.3 name such as `RUNNER~1` resolves to the long one).
fn forms(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut forms = vec![plain(&std::path::absolute(path)?)];
    if let Ok(real) = fs::canonicalize(path) {
        let real = plain(&real);
        if real != forms[0] {
            forms.push(real);
        }
    }
    Ok(forms)
}

/// `paths` joined with the system's separator for path lists (`;` on Windows, `:` elsewhere).
fn join(paths: &[PathBuf]) -> io::Result<OsString> {
    std::env::join_paths(paths).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot build the environment of the test run: {error}"),
        )
    })
}

/// Points paths inside the project to the same place in the test copy.
#[derive(Debug)]
struct Redirect {
    /// The forms of the project folder (see [`forms`]).
    project: Vec<PathBuf>,
    /// The copy folder, absolute.
    copy: PathBuf,
}

impl Redirect {
    /// The path in the copy for `path`, if `path` is the project folder or inside it, outside `.yalper/` (see
    /// [`relative`](Self::relative)).
    fn path(&self, path: &Path) -> Option<PathBuf> {
        let rest = self.relative(path)?;
        if let Some(Component::Normal(first)) = rest.components().next() {
            let in_yalper = if CASE_INSENSITIVE {
                first.eq_ignore_ascii_case(YALPER_DIR)
            } else {
                first == YALPER_DIR
            };
            if in_yalper {
                return None;
            }
        }
        let mut redirected = self.copy.clone();
        redirected.extend(rest.components());
        Some(redirected)
    }

    /// Where `path` is inside the project (empty for the project itself), or `None` if it is not.
    ///
    /// A relative path is relative to the folder of the run, the copy, and is not inside the project; on Windows
    /// a path from the root of the current drive (`\proj\bin`) is on the copy's drive. Paths are compared by
    /// their names after resolving `.` and `..` from the text, without regard to case where the file system
    /// ignores it, so `/proj-other` is not inside `/proj`. A path that does not match is resolved, if it exists,
    /// and compared again, which catches links to the project and Windows' short 8.3 names; network shares and
    /// devices are never resolved, and on Windows nor are other drives than the project's (a network drive
    /// could keep the run waiting).
    fn relative(&self, path: &Path) -> Option<PathBuf> {
        let path = if path.is_absolute() {
            drive_path(&plain(path))
        } else if cfg!(windows) && path.has_root() {
            let mut on_copy_drive: PathBuf = self.copy.components().take(1).collect();
            on_copy_drive.push(path);
            on_copy_drive
        } else {
            return None;
        };
        let path = normalize(&path);
        if let Some(rest) = self.inside(&path) {
            return Some(rest);
        }
        let first = path.components().next();
        let same_drive = self.project.iter().any(|root| {
            let (a, b) = (root.components().next(), first);
            a.zip(b).is_some_and(|(a, b)| {
                a.as_os_str()
                    .eq_ignore_ascii_case(b.as_os_str().to_string_lossy().as_ref())
            })
        });
        if has_special_prefix(&path) || !same_drive {
            return None;
        }
        let real = normalize(&drive_path(&plain(&fs::canonicalize(&path).ok()?)));
        self.inside(&real)
    }

    /// Where the absolute, normalized `path` is inside a form of the project, by its text.
    fn inside(&self, path: &Path) -> Option<PathBuf> {
        let root = self.project.iter().find(|root| is_under(path, root))?;
        Some(path.components().skip(root.components().count()).collect())
    }

    /// The entries of the path list `value`, each redirected if it points into the project.
    fn entries<'a>(&'a self, value: &'a OsStr) -> impl Iterator<Item = PathBuf> + 'a {
        std::env::split_paths(value).map(|entry| self.path(&entry).unwrap_or(entry))
    }

    /// The path list `value`, which may be a single path, with every entry that points into the project
    /// redirected; exactly `value` when none does.
    fn list(&self, value: &OsStr) -> io::Result<OsString> {
        let mut moved = false;
        let entries: Vec<PathBuf> = std::env::split_paths(value)
            .map(|entry| match self.path(&entry) {
                Some(redirected) => {
                    moved = true;
                    redirected
                }
                None => entry,
            })
            .collect();
        if moved {
            join(&entries)
        } else {
            Ok(value.to_owned())
        }
    }
}

/// `path` with a Windows device prefix naming a drive (`\\.\C:\proj`) written as a plain drive path
/// (`C:\proj`); any other path as it is.
fn drive_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = path.components().next()
        && let std::path::Prefix::DeviceNS(name) = prefix.kind()
        && let [letter, b':'] = name.as_encoded_bytes()
        && letter.is_ascii_alphabetic()
    {
        let mut drive = PathBuf::from(format!("{}:\\", char::from(*letter)));
        drive.extend(
            path.components()
                .skip(1)
                .filter(|component| *component != Component::RootDir),
        );
        return drive;
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use crate::bisect::link::make_link;
    use crate::bisect::testing::{Project, state, tool, write};

    #[test]
    fn the_variables_name_folders_of_yalper_bisect() {
        let project = Project::new();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let mut python = PythonEnvs::new(project.path()).unwrap();
        python.link(&mut copy).unwrap();
        let additions = env_additions(&copy, &python).unwrap();

        // The copy's folder: canonical on Unix, where `/var` may be a link to `/private/var`.
        let bisect = plain(&std::path::absolute(copy.path().parent().unwrap()).unwrap());
        assert!(bisect.ends_with(".yalper/bisect"));
        let pycache = bisect.join("pycache").join("copy-0");
        let cargo_target = bisect.join("cargo-target").join("copy-0");
        assert_eq!(
            additions,
            EnvAdditions {
                python_path: Vec::new(),
                vars: vec![
                    ("PYTHONPYCACHEPREFIX", pycache.clone().into_os_string()),
                    ("UV_NO_SYNC", OsString::from("1")),
                    ("CARGO_TARGET_DIR", cargo_target.clone().into_os_string()),
                    (
                        "CARGO_BUILD_BUILD_DIR",
                        cargo_target.clone().into_os_string()
                    ),
                ],
            }
        );
        for dir in [&pycache, &cargo_target] {
            assert!(fs::symlink_metadata(dir).unwrap().is_dir());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(dir).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700);
            }
        }
    }

    /// `path` written with `/`, as an absolute path of this system: on Windows on drive `C:`, with `\`.
    fn p(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", path.replace('/', "\\")))
        } else {
            PathBuf::from(path)
        }
    }

    /// The project `/proj`, also reached as `/real/proj`, and its copy `/proj/.yalper/bisect/copy-0`.
    fn redirect() -> Redirect {
        Redirect {
            project: vec![p("/proj"), p("/real/proj")],
            copy: p("/proj/.yalper/bisect/copy-0"),
        }
    }

    fn in_copy(path: &str) -> PathBuf {
        let mut in_copy = p("/proj/.yalper/bisect/copy-0");
        in_copy.extend(path.split('/').filter(|name| !name.is_empty()));
        in_copy
    }

    #[test]
    fn paths_inside_the_project_point_into_the_copy() {
        let redirect = redirect();
        let path = |text: &str| redirect.path(&p(text));
        assert_eq!(
            path("/proj/node_modules/.bin"),
            Some(in_copy("node_modules/.bin"))
        );
        assert_eq!(path("/proj/.venv/bin/"), Some(in_copy(".venv/bin")));
        assert_eq!(path("/proj"), Some(in_copy("")));
        assert_eq!(path("/real/proj/src"), Some(in_copy("src")));
        // Dots are resolved from the text first.
        assert_eq!(path("/proj/a/../bin"), Some(in_copy("bin")));
        assert_eq!(path("/proj/./bin"), Some(in_copy("bin")));
        assert_eq!(path("/proj/../other/bin"), None);
        // Whole names only.
        assert_eq!(path("/proj-other/bin"), None);
        assert_eq!(path("/pro"), None);
        assert_eq!(path("/"), None);
        assert_eq!(path("/real"), None);
        // Yalper's own folder, the copies included, is left as it is.
        assert_eq!(path("/proj/.yalper/bin"), None);
        assert_eq!(path("/proj/.yalper/bisect/copy-0/bin"), None);
        assert_eq!(path("/proj/.yalperx/bin"), Some(in_copy(".yalperx/bin")));
        // Relative paths already lead into the copy, where the run starts.
        assert_eq!(redirect.path(Path::new("node_modules/.bin")), None);
        assert_eq!(redirect.path(Path::new("")), None);
        // Case, where the file system ignores it.
        let upper = path("/PROJ/Bin");
        assert_eq!(upper, CASE_INSENSITIVE.then(|| in_copy("Bin")));
        let yalper = path("/proj/.YALPER/bin");
        assert_eq!(yalper, (!CASE_INSENSITIVE).then(|| in_copy(".YALPER/bin")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefixes_and_drive_letters_are_compared_as_paths() {
        let redirect = redirect();
        let path = |text: &str| redirect.path(Path::new(text));
        assert_eq!(path(r"\\?\C:\proj\bin"), Some(in_copy("bin")));
        assert_eq!(path(r"c:\proj\bin"), Some(in_copy("bin")));
        assert_eq!(path(r"C:/proj/bin"), Some(in_copy("bin")));
        assert_eq!(path(r"D:\proj\bin"), None);
        // A device path naming a drive.
        assert_eq!(path(r"\\.\C:\proj\bin"), Some(in_copy("bin")));
        assert_eq!(path(r"\\.\c:\proj\bin"), Some(in_copy("bin")));
        assert_eq!(path(r"\\.\pipe\proj"), None);
        // From the root of the current drive: the copy's drive, where the run starts.
        assert_eq!(path(r"\proj\bin"), Some(in_copy("bin")));
        assert_eq!(path(r"\other\bin"), None);
        // Relative to the current folder of drive C:, which is the copy during a run.
        assert_eq!(path(r"C:proj\bin"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_project_on_a_network_share_is_matched_in_both_unc_forms() {
        let redirect = Redirect {
            project: vec![PathBuf::from(r"\\server\share\proj")],
            copy: PathBuf::from(r"\\server\share\proj\.yalper\bisect\copy-0"),
        };
        let in_copy = |rest: &str| Some(redirect.copy.join(rest));
        let path = |text: &str| redirect.path(Path::new(text));
        assert_eq!(path(r"\\server\share\proj\bin"), in_copy("bin"));
        assert_eq!(path(r"\\?\UNC\server\share\proj\bin"), in_copy("bin"));
        assert_eq!(path(r"\\SERVER\Share\proj\bin"), in_copy("bin"));
        assert_eq!(path(r"\proj\bin"), in_copy("bin"));
        // Not resolved: network paths are never looked up.
        assert_eq!(path(r"\\server\share\other\bin"), None);
        assert_eq!(path(r"\\server\other\proj\bin"), None);
        assert_eq!(path(r"C:\proj\bin"), None);
    }

    #[test]
    fn a_path_list_keeps_its_other_entries_and_its_exact_text_when_nothing_moves() {
        let redirect = redirect();
        let list = |entries: &[PathBuf]| std::env::join_paths(entries).unwrap();
        let untouched = list(&[p("/usr/bin"), PathBuf::new(), PathBuf::from("bin")]);
        assert_eq!(redirect.list(&untouched).unwrap(), untouched);

        let mixed = list(&[
            p("/usr/bin"),
            p("/proj/node_modules/.bin"),
            PathBuf::new(),
            p("/proj-other/bin"),
            p("/proj/.venv/bin"),
        ]);
        let expected = list(&[
            p("/usr/bin"),
            in_copy("node_modules/.bin"),
            PathBuf::new(),
            p("/proj-other/bin"),
            in_copy(".venv/bin"),
        ]);
        assert_eq!(redirect.list(&mixed).unwrap(), expected);
        let separator = if cfg!(windows) { ';' } else { ':' };
        assert_eq!(expected.to_string_lossy().matches(separator).count(), 4);
    }

    /// The value of `name` in `env`, which must hold it once.
    fn value<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
        let mut found = env.iter().filter(|(var, _)| same_name(var, name));
        let value = found.next().map(|(_, value)| value.as_os_str());
        assert!(found.next().is_none(), "{name} is set twice");
        value
    }

    #[test]
    fn the_run_environment_is_the_users_kept_inside_the_copy() {
        let project = Project::new();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let mut python = PythonEnvs::new(project.path()).unwrap();
        python.link(&mut copy).unwrap();
        let mut additions = env_additions(&copy, &python).unwrap();
        let root = plain(&std::path::absolute(project.path()).unwrap());
        let copy_root = plain(&std::path::absolute(copy.path()).unwrap());
        additions.python_path = vec![copy_root.join("src")];

        let os = |text: &str| OsString::from(text);
        let user_path =
            std::env::join_paths([root.join(".venv").join("bin"), p("/usr/bin")]).unwrap();
        let user = vec![
            (os("GIT_DIR"), root.join(".git").into_os_string()),
            (os("GIT_WORK_TREE"), root.clone().into_os_string()),
            (
                os("GIT_INDEX_FILE"),
                root.join(".git/index").into_os_string(),
            ),
            (os("GIT_OBJECT_DIRECTORY"), os("objects")),
            (os("GIT_ALTERNATE_OBJECT_DIRECTORIES"), os("alternates")),
            (os("GIT_COMMON_DIR"), os("common")),
            (os("GIT_NAMESPACE"), os("namespace")),
            (os("GIT_CONFIG_PARAMETERS"), os("'core.worktree'='x'")),
            (os("GIT_CONFIG_COUNT"), os("1")),
            (os("GIT_CEILING_DIRECTORIES"), os("elsewhere")),
            (os("GIT_CONFIG_GLOBAL"), os("my-gitconfig")),
            (os(if cfg!(windows) { "Path" } else { "PATH" }), user_path),
            (os("VIRTUAL_ENV"), root.join(".venv").into_os_string()),
            (os("PYTHONPATH"), root.join("lib").into_os_string()),
            (os("CARGO_TARGET_DIR"), root.join("target").into_os_string()),
            (os("YALPER_TEST_OTHER"), os("kept")),
        ];
        let env = run_env(user, project.path(), &copy, additions.clone()).unwrap();

        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            assert_eq!(value(&env, name), None, "{name}");
        }
        let ceilings: Vec<PathBuf> =
            std::env::split_paths(value(&env, "GIT_CEILING_DIRECTORIES").unwrap()).collect();
        assert!(
            ceilings.contains(&root.join(".yalper").join("bisect")),
            "{ceilings:?}"
        );
        assert!(ceilings.contains(&root), "{ceilings:?}");
        assert_eq!(
            value(&env, "GIT_CONFIG_GLOBAL"),
            Some(OsStr::new("my-gitconfig"))
        );
        assert_eq!(value(&env, "YALPER_TEST_OTHER"), Some(OsStr::new("kept")));

        // The name of `PATH` keeps its case; its project entry points into the copy.
        let path_name = if cfg!(windows) { "Path" } else { "PATH" };
        assert!(env.iter().any(|(name, _)| name == path_name));
        let path: Vec<PathBuf> = std::env::split_paths(value(&env, "PATH").unwrap()).collect();
        assert_eq!(path, [copy_root.join(".venv").join("bin"), p("/usr/bin")]);
        assert_eq!(
            value(&env, "VIRTUAL_ENV"),
            Some(copy_root.join(".venv").as_os_str())
        );
        let python_path: Vec<PathBuf> =
            std::env::split_paths(value(&env, "PYTHONPATH").unwrap()).collect();
        assert_eq!(python_path, [copy_root.join("src"), copy_root.join("lib")]);
        for (name, expected) in &additions.vars {
            assert_eq!(value(&env, name), Some(expected.as_os_str()), "{name}");
        }
    }

    /// A project with a copy holding one file, and the environment additions for it.
    fn project_with_copy() -> (Project, TestCopy, EnvAdditions) {
        let project = Project::new();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        let additions = env_additions(&copy, &python).unwrap();
        (project, copy, additions)
    }

    #[test]
    fn every_variable_holding_a_path_into_the_project_points_into_the_copy() {
        let (project, copy, additions) = project_with_copy();
        let root = plain(&std::path::absolute(project.path()).unwrap());
        let copy_root = plain(&std::path::absolute(copy.path()).unwrap());
        let outside = tempfile::tempdir().unwrap();
        let outside = plain(&std::path::absolute(outside.path()).unwrap());
        let list = |paths: &[PathBuf]| std::env::join_paths(paths).unwrap();
        let moved = [
            ("UV_PROJECT_ENVIRONMENT", ".venv"),
            ("UV_PROJECT", ""),
            ("UV_WORKING_DIRECTORY", "sub"),
            ("NPM_CONFIG_WORKSPACE_DIR", ""),
            ("CONDA_PREFIX", "env"),
            ("PIPENV_PIPFILE", "Pipfile"),
            ("INIT_CWD", ""),
            ("npm_config_local_prefix", ""),
            ("npm_package_json", "package.json"),
            ("YALPER_TEST_ANY_NAME", "anything"),
        ];
        let mut user: Vec<(OsString, OsString)> = moved
            .iter()
            .map(|(name, rest)| ((*name).into(), root.join(rest).into()))
            .collect();
        user.push((
            "NODE_PATH".into(),
            list(&[root.join("node_modules"), outside.join("lib")]),
        ));
        user.push(("YALPER_TEST_OUTSIDE".into(), outside.join("x").into()));
        user.push((
            "YALPER_TEST_YALPER".into(),
            root.join(".yalper").join("x").into(),
        ));
        user.push(("YALPER_TEST_WORD".into(), "proj".into()));
        let env = run_env(user, project.path(), &copy, additions).unwrap();

        for (name, rest) in moved {
            let expected = if rest.is_empty() {
                copy_root.clone()
            } else {
                copy_root.join(rest)
            };
            assert_eq!(value(&env, name), Some(expected.as_os_str()), "{name}");
        }
        assert_eq!(
            value(&env, "NODE_PATH"),
            Some(list(&[copy_root.join("node_modules"), outside.join("lib")]).as_os_str())
        );
        assert_eq!(
            value(&env, "YALPER_TEST_OUTSIDE"),
            Some(outside.join("x").as_os_str())
        );
        assert_eq!(
            value(&env, "YALPER_TEST_YALPER"),
            Some(root.join(".yalper").join("x").as_os_str())
        );
        assert_eq!(value(&env, "YALPER_TEST_WORD"), Some(OsStr::new("proj")));
    }

    #[test]
    fn a_path_through_a_link_to_the_project_points_into_the_copy() {
        let (project, copy, additions) = project_with_copy();
        project.write("bin/tool", "");
        let copy_root = plain(&std::path::absolute(copy.path()).unwrap());
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        make_link(project.path(), &link).unwrap();
        let user = vec![
            ("YALPER_TEST_LINKED".into(), link.join("bin").into()),
            ("YALPER_TEST_ROOT".into(), link.clone().into()),
            ("YALPER_TEST_YALPER".into(), link.join(".yalper").into()),
        ];
        let env = run_env(user, project.path(), &copy, additions).unwrap();
        assert_eq!(
            value(&env, "YALPER_TEST_LINKED"),
            Some(copy_root.join("bin").as_os_str())
        );
        assert_eq!(value(&env, "YALPER_TEST_ROOT"), Some(copy_root.as_os_str()));
        assert_eq!(
            value(&env, "YALPER_TEST_YALPER"),
            Some(link.join(".yalper").as_os_str())
        );
    }

    #[test]
    fn a_project_that_is_or_holds_the_home_folder_is_refused() {
        let (project, copy, additions) = project_with_copy();
        let root = plain(&std::path::absolute(project.path()).unwrap());
        for name in ["HOME", "USERPROFILE"] {
            for home in [root.clone(), root.join("me")] {
                let user = vec![(name.into(), home.into())];
                let error = run_env(user, project.path(), &copy, additions.clone()).unwrap_err();
                assert!(error.to_string().contains("home folder"), "{error}");
            }
        }
        // A project inside the home folder, the usual case.
        let user = vec![("HOME".into(), root.parent().unwrap().into())];
        run_env(user, project.path(), &copy, additions).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn the_current_folder_of_a_drive_is_left_out_when_it_is_in_the_project() {
        let (project, copy, additions) = project_with_copy();
        let root = plain(&std::path::absolute(project.path()).unwrap());
        let user = vec![
            ("=C:".into(), root.join("sub").into()),
            ("=D:".into(), r"D:\elsewhere".into()),
        ];
        let env = run_env(user, project.path(), &copy, additions).unwrap();
        assert!(!env.iter().any(|(name, _)| name == "=C:"));
        assert_eq!(value(&env, "=D:"), Some(OsStr::new(r"D:\elsewhere")));
    }

    #[cfg(windows)]
    #[test]
    fn variable_names_are_matched_without_regard_to_case_on_windows() {
        let project = Project::new();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        let additions = env_additions(&copy, &python).unwrap();
        let user = vec![
            (OsString::from("git_dir"), OsString::from("x")),
            (OsString::from("Cargo_Target_Dir"), OsString::from("x")),
            (OsString::from("PythonPath"), OsString::from(r"C:\lib")),
        ];
        let env = run_env(user, project.path(), &copy, additions).unwrap();
        assert_eq!(value(&env, "GIT_DIR"), None);
        assert_ne!(value(&env, "CARGO_TARGET_DIR"), Some(OsStr::new("x")));
        assert!(env.iter().any(|(name, _)| name == "PythonPath"));
    }

    #[test]
    fn pythonpath_is_set_only_when_it_holds_a_folder() {
        let project = Project::new();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        let additions = env_additions(&copy, &python).unwrap();
        let env = run_env(Vec::new(), project.path(), &copy, additions.clone()).unwrap();
        assert_eq!(value(&env, "PYTHONPATH"), None);
        let empty = vec![(OsString::from("PYTHONPATH"), OsString::new())];
        let env = run_env(empty, project.path(), &copy, additions).unwrap();
        assert_eq!(value(&env, "PYTHONPATH"), None);
    }

    #[test]
    fn a_pythonpath_over_the_windows_limit_is_refused() {
        let project = Project::new();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        let mut additions = env_additions(&copy, &python).unwrap();
        let folder = |index: usize| p(&format!("/{index:0>99}"));
        // Each folder takes its length plus a separator, except the last; then comes `PYTHONPATH=`.
        let with_separator = folder(0).as_os_str().len() + 1;
        let fits = (MAX_PYTHONPATH - "PYTHONPATH=".len() + 1) / with_separator;
        additions.python_path = (0..fits).map(folder).collect();
        run_env(Vec::new(), project.path(), &copy, additions.clone()).unwrap();

        additions.python_path.push(folder(fits));
        let error = run_env(Vec::new(), project.path(), &copy, additions).unwrap_err();
        assert!(error.to_string().contains("PYTHONPATH"), "{error}");
    }

    #[test]
    fn a_project_path_that_cannot_be_written_in_a_path_list_is_an_error() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent
            .path()
            .join(if cfg!(windows) { "a;b" } else { "a:b" });
        fs::create_dir_all(root.join(".yalper")).unwrap();
        let yalper = crate::safe_fs::OwnedDir::open(&root.join(".yalper")).unwrap();
        let mut copy = TestCopy::open(&yalper, 0).unwrap();
        let project = Project::new();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let python = PythonEnvs::new(&root).unwrap();
        let additions = env_additions(&copy, &python).unwrap();
        let error = run_env(Vec::new(), &root, &copy, additions).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("git"), "{error}");
    }

    #[test]
    fn a_link_a_test_put_in_place_of_a_folder_is_replaced_never_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "keep.txt", "keep me");
        let before = state(canary.path());
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        env_additions(&copy, &python).unwrap();

        // A test run replaces the folders with links to the canary, then the copy's own folders with files.
        let bisect = project.path().join(".yalper/bisect");
        for folder in ["pycache", "cargo-target"] {
            let path = bisect.join(folder);
            fs::remove_dir_all(&path).unwrap();
            make_link(canary.path(), &path).unwrap();
        }
        env_additions(&copy, &python).unwrap();
        for folder in ["pycache/copy-0", "cargo-target/copy-0"] {
            fs::remove_dir_all(bisect.join(folder)).unwrap();
            fs::write(bisect.join(folder), "a file").unwrap();
        }
        env_additions(&copy, &python).unwrap();

        for folder in [
            "pycache",
            "pycache/copy-0",
            "cargo-target",
            "cargo-target/copy-0",
        ] {
            assert!(fs::symlink_metadata(bisect.join(folder)).unwrap().is_dir());
        }
        assert_eq!(state(canary.path()), before);
    }

    /// `cargo test` in `copy` with the environment additions: what cargo printed.
    ///
    /// On Windows the linker can fail to replace a test program that was run a moment before (LNK1104, the
    /// file is still held by the system or a virus scanner): cargo is then run again, up to 3 times.
    fn cargo_test(cargo: &str, copy: &TestCopy, additions: &EnvAdditions) -> String {
        let mut printed = String::new();
        for attempt in 1..=3 {
            let output = Command::new(cargo)
                .args(["test", "--offline"])
                .env("CARGO_TERM_COLOR", "never")
                .current_dir(copy.path())
                .envs(additions.vars.iter().map(|(name, value)| (name, value)))
                .env_remove("CARGO_BUILD_TARGET_DIR")
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            printed.push_str(&stderr);
            if output.status.success() {
                return printed;
            }
            assert!(
                cfg!(windows) && stderr.contains("LNK1104") && attempt < 3,
                "{output:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        unreachable!("the last attempt either succeeds or fails the test")
    }

    /// The packages cargo compiled, from its output, sorted and once each (an attempt run again prints them
    /// again).
    fn compiled(output: &str) -> Vec<&str> {
        let mut compiled: Vec<&str> = output
            .lines()
            .filter_map(|line| line.trim().strip_prefix("Compiling "))
            .filter_map(|rest| rest.split_whitespace().next())
            .collect();
        compiled.sort_unstable();
        compiled.dedup();
        compiled
    }

    #[test]
    fn cargo_builds_in_the_shared_folder_and_rebuilds_only_the_changed_crate() {
        let Some(cargo) = tool(&["cargo"]) else {
            return;
        };
        let workspace = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
        let manifest = |name: &str| {
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        };
        let (manifest_a, manifest_b) = (manifest("a"), manifest("b"));
        let lib = |value: u32| {
            format!(
                "pub fn value() -> u32 {{ {value} }}\n\n#[test]\nfn works() {{ assert_eq!(value(), {value}); }}\n"
            )
        };
        let (lib_a, lib_b1, lib_b2) = (lib(1), lib(2), lib(3));
        let files = |lib_b: &str| {
            [
                (".gitignore", "target/\n".to_owned()),
                ("Cargo.toml", workspace.to_owned()),
                ("a/Cargo.toml", manifest_a.clone()),
                ("a/src/lib.rs", lib_a.clone()),
                ("b/Cargo.toml", manifest_b.clone()),
                ("b/src/lib.rs", lib_b.to_owned()),
            ]
        };
        let project = Project::new();
        for (path, content) in files(&lib_b1) {
            project.write(path, &content);
        }
        // The project's own build folder, with a canary.
        project.write("target/canary.txt", "keep me");
        project.write("target/debug/.fingerprint/a/keep", "");
        let before = state(project.path());

        let tree = |lib_b: &str| {
            let files = files(lib_b);
            let files: Vec<(&str, &str)> = files
                .iter()
                .map(|(path, content)| (*path, content.as_str()))
                .collect();
            project.tree(&files)
        };
        let mut copy = project.copy();
        let python = PythonEnvs::new(project.path()).unwrap();
        copy.switch(&project.store, tree(&lib_b1)).unwrap();
        let additions = env_additions(&copy, &python).unwrap();
        assert_eq!(compiled(&cargo_test(cargo, &copy, &additions)), ["a", "b"]);

        // One file of `b` changes: only `b` is compiled again.
        let switched = copy.switch(&project.store, tree(&lib_b2)).unwrap();
        assert_eq!(switched.written, 1);
        let additions = env_additions(&copy, &python).unwrap();
        assert_eq!(compiled(&cargo_test(cargo, &copy, &additions)), ["b"]);

        assert!(!copy.path().join("target").exists());
        assert!(copy.cargo_target_path().join("debug").is_dir());
        assert_eq!(state(project.path()), before);
    }

    #[test]
    fn a_copy_never_runs_the_build_of_another_copy() {
        let Some(cargo) = tool(&["cargo"]) else {
            return;
        };
        let manifest = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let main = |value: u32| format!("fn main() {{\n    print!(\"{value}\");\n}}\n");
        let project = Project::new();
        let python = PythonEnvs::new(project.path()).unwrap();
        let tree = |value: u32| {
            let main = main(value);
            project.tree(&[
                (".gitignore", "target/\n"),
                ("Cargo.toml", manifest),
                ("src/main.rs", &main),
            ])
        };
        let cargo_run = |copy: &TestCopy| {
            let additions = env_additions(copy, &python).unwrap();
            let output = Command::new(cargo)
                .args(["run", "--offline", "--quiet"])
                .current_dir(copy.path())
                .envs(additions.vars.iter().map(|(name, value)| (name, value)))
                .env_remove("CARGO_BUILD_TARGET_DIR")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };

        // Copy 1 gets its step first, so its sources are older than the build copy 0 makes next: with one
        // target folder shared by both, cargo would take that build as fresh for copy 1.
        let mut second = TestCopy::open(&project.yalper, 1).unwrap();
        second.switch(&project.store, tree(1)).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for file in ["Cargo.toml", "src/main.rs"] {
            fs::File::options()
                .write(true)
                .open(second.path().join(file))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let mut first = project.copy();
        first.switch(&project.store, tree(2)).unwrap();
        assert_eq!(cargo_run(&first), "2");
        assert_eq!(cargo_run(&second), "1");
        assert_ne!(first.cargo_target_path(), second.cargo_target_path());
    }
}
