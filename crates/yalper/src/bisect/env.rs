//! The environment variables a test run gets on top of the user's, so that the tools it runs reuse the
//! project's dependencies without writing into the project:
//!
//! - `PYTHONPATH` starts with the copy's own folders of the project's editable installs (see
//!   [`PythonEnvs::python_path`]);
//! - `PYTHONPYCACHEPREFIX` sends Python's bytecode to the copy's folder `.yalper/bisect/pycache/copy-<n>/`
//!   (Python 3.8 and later), so none is written into the project's virtual environment through the copy's
//!   link to it;
//! - `UV_NO_SYNC=1` stops `uv run` from syncing, and so writing into, that environment;
//! - `CARGO_TARGET_DIR` is `.yalper/bisect/cargo-target/`, one folder shared by every copy and every bisect:
//!   the project's `target/` is never used, and cargo's own fingerprints decide what to rebuild. The first
//!   bisect of a Rust project compiles its dependencies there once.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::copy::remove_entry;
use super::link::plain;
use super::{PythonEnvs, TestCopy};

/// The folder in `.yalper/bisect/` where cargo builds the tests of every copy.
const CARGO_TARGET_DIR: &str = "cargo-target";

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
    let bisect = plain(&std::path::absolute(copy.bisect_dir().path())?);
    let cargo_target = bisect.join(CARGO_TARGET_DIR);
    if let Some(parent) = pycache.parent() {
        make_real_dir(parent)?;
    }
    make_real_dir(&pycache)?;
    make_real_dir(&cargo_target)?;
    Ok(EnvAdditions {
        python_path: python.python_path(copy)?,
        vars: vec![
            ("PYTHONPYCACHEPREFIX", pycache.into_os_string()),
            ("UV_NO_SYNC", OsString::from("1")),
            ("CARGO_TARGET_DIR", cargo_target.into_os_string()),
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

        let bisect = plain(&std::path::absolute(project.path().join(".yalper/bisect")).unwrap());
        let pycache = bisect.join("pycache").join("copy-0");
        let cargo_target = bisect.join("cargo-target");
        assert_eq!(
            additions,
            EnvAdditions {
                python_path: Vec::new(),
                vars: vec![
                    ("PYTHONPYCACHEPREFIX", pycache.clone().into_os_string()),
                    ("UV_NO_SYNC", OsString::from("1")),
                    ("CARGO_TARGET_DIR", cargo_target.clone().into_os_string()),
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

        // A test run replaces both folders with links to the canary, and the pycache folder of the copy with a
        // file.
        let bisect = project.path().join(".yalper/bisect");
        for folder in ["pycache", "cargo-target"] {
            let path = bisect.join(folder);
            fs::remove_dir_all(&path).unwrap();
            make_link(canary.path(), &path).unwrap();
        }
        env_additions(&copy, &python).unwrap();
        fs::remove_dir_all(bisect.join("pycache/copy-0")).unwrap();
        fs::write(bisect.join("pycache/copy-0"), "a file").unwrap();
        env_additions(&copy, &python).unwrap();

        for folder in ["pycache", "pycache/copy-0", "cargo-target"] {
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
        assert!(
            project
                .path()
                .join(".yalper/bisect/cargo-target/debug")
                .is_dir()
        );
        assert_eq!(state(project.path()), before);
    }
}
