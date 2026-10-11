//! Boundary files in `.yalper/bisect/`, the folder that holds the test copies.
//!
//! A test copy sits inside the project, so a tool that searches the folders above it for its project file
//! reaches the project when the step has no such file (a step from before the agent created it): `npm test`
//! would run the project's script in the project, `pytest` would load the project's `conftest.py` and write
//! its cache there, `cargo test` would build the project's current code and could write its `Cargo.lock`, and
//! `uv run` would use, or even create, the project's virtual environment. The files written here end those
//! searches in `.yalper/bisect/`, so the tools fail there or find nothing to run:
//!
//! - `package.json`, with no scripts: `npm test` fails with "Missing script". npm, pnpm, bun and Yarn 1 take
//!   the nearest `package.json` as the package a command runs in, and Node takes the module type of a step's
//!   `.js` files from it instead of from the project's;
//! - `pytest.ini`: pytest's root folder (where it writes `.pytest_cache`) is `.yalper/bisect/`, and it loads no
//!   `conftest.py` above it;
//! - `pyproject.toml` with a `[project]` table: uv (and Poetry, Hatch, PDM) take it as the project, so `uv run`
//!   uses its own environment in `.yalper/bisect/.venv`, never the project's. It also ends uv's search for a
//!   workspace above a step's own project;
//! - `Cargo.toml`, a package whose library file does not exist: `cargo test` fails with "can't find lib
//!   `no_cargo_toml_in_this_step`". Cargo also reads it when it searches for a workspace above a step's own
//!   package, where a package that is not a workspace changes nothing.
//!
//! Not covered (searches no file here can end): pnpm finds `pnpm-workspace.yaml` in any folder above, so in a
//! step without one, `pnpm -r` and `pnpm install` act on the project's workspace (a boundary
//! `pnpm-workspace.yaml` would make pnpm install single-package copies into `.yalper/bisect/`); `npx`,
//! `npm exec` and `bunx` find programs in the `node_modules/.bin` of any folder above, like Node's module
//! resolution; Yarn 2 and later were not tested; cargo, ESLint, Prettier and TypeScript read their
//! configuration files (`.cargo/config.toml`, `eslint.config.js`, `tsconfig.json`) from every folder above, so
//! a step without one uses the project's current one; a step whose root package has no `[workspace]` fails to
//! build with cargo when the project's current `Cargo.toml` is a workspace.
//!
//! The files are rewritten whenever their content differs: what is on disk is never trusted, since a test run
//! can change them (`npm pkg set`, `npm install <package>` in a step without `package.json`) and a pulled
//! commit can put files under `.yalper/`.

use std::fs;
use std::io::{self, Read, Seek, Write};

use super::BISECT_DIR;
use super::copy::{open_private_dir, remove_entry};
use crate::safe_fs::{Access, OwnedDir};

/// The boundary files: name in `.yalper/bisect/` and content.
const FILES: [(&str, &str); 4] = [
    ("package.json", "{\"private\": true}\n"),
    (
        "pytest.ini",
        "# Written by Yalper: pytest searches no folder above the test copies.\n[pytest]\n",
    ),
    (
        "pyproject.toml",
        "# Written by Yalper: uv uses this project, not yours, when a step has no pyproject.toml.\n\
         [project]\nname = \"no-pyproject-toml-in-this-step\"\nversion = \"0.0.0\"\n",
    ),
    (
        "Cargo.toml",
        "# Written by Yalper: cargo builds nothing of yours when a step has no Cargo.toml.\n\
         [package]\nname = \"no-cargo-toml-in-this-step\"\nversion = \"0.0.0\"\nedition = \"2015\"\n\
         publish = false\n\n[lib]\npath = \"no-cargo-toml-in-this-step.rs\"\n",
    ),
];

/// Writes the boundary files into `.yalper/bisect/` of the `.yalper/` folder `yalper_dir`, creating that folder
/// if it is missing: each one whose content differs, or that is not a regular file with a single link, is
/// replaced (a link or folder at its name is removed, never followed), on Unix with mode 0600. Returns how many
/// were written.
///
/// Call it at the start of every bisect, and before every run, since a run can change them: checking four
/// small files is cheap.
pub fn write_boundaries(yalper_dir: &OwnedDir) -> io::Result<usize> {
    let bisect = open_private_dir(&yalper_dir.path().join(BISECT_DIR))?;
    let mut written = 0;
    for (name, content) in FILES {
        if write_if_different(&bisect, name, content.as_bytes()).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot write {}: {error}",
                    bisect.path().join(name).display()
                ),
            )
        })? {
            written += 1;
        }
    }
    Ok(written)
}

/// Makes `name` in `dir` a regular file holding exactly `content`: returns whether it was written.
fn write_if_different(dir: &OwnedDir, name: &str, content: &[u8]) -> io::Result<bool> {
    dir.check_still_at_path()?;
    let path = dir.path().join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => remove_entry(&path, &metadata)?,
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut file = match dir.open_file(name, Access::ReadWrite) {
        Ok(file) => file,
        // A file with a second hard link (Unix), or one a test made read-only or unreadable: this name is
        // removed and the file created anew.
        Err(_) => {
            remove_entry(&path, &fs::symlink_metadata(&path)?)?;
            dir.open_file(name, Access::ReadWrite)?
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let mut current = Vec::new();
    (&file)
        .take(content.len() as u64 + 1)
        .read_to_end(&mut current)?;
    if current == content {
        return Ok(false);
    }
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(content)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::AtomicBool;

    use crate::bisect::link::make_link;
    use crate::bisect::testing::{Project, state, tool, venv_python, write};
    use crate::bisect::{Logs, Outcome, PythonEnvs, TestCopy, env_additions, run, run_env};

    fn bisect_dir(project: &Project) -> PathBuf {
        project.path().join(".yalper").join(BISECT_DIR)
    }

    fn read(project: &Project, name: &str) -> String {
        fs::read_to_string(bisect_dir(project).join(name)).unwrap()
    }

    #[test]
    fn boundary_files_are_written_and_then_rewritten_only_when_they_differ() {
        let project = Project::new();
        assert_eq!(write_boundaries(&project.yalper).unwrap(), FILES.len());
        for (name, content) in FILES {
            assert_eq!(read(&project, name), content);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let path = bisect_dir(&project).join(name);
                let mode = fs::metadata(path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{name}");
            }
        }
        assert_eq!(write_boundaries(&project.yalper).unwrap(), 0);

        // A test run added a script to one, made another read-only, and emptied a third: all three are written
        // anew.
        let dir = bisect_dir(&project);
        fs::write(
            dir.join("package.json"),
            r#"{"private": true, "scripts": {"test": "x"}}"#,
        )
        .unwrap();
        let ini = dir.join("pytest.ini");
        let mut permissions = fs::metadata(&ini).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&ini, permissions).unwrap();
        fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert_eq!(write_boundaries(&project.yalper).unwrap(), 3);
        for (name, content) in FILES {
            assert_eq!(read(&project, name), content);
        }
    }

    #[test]
    fn a_link_or_folder_in_place_of_a_boundary_file_is_replaced_never_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "package.json", "keep me");
        write_boundaries(&project.yalper).unwrap();
        let dir = bisect_dir(&project);
        fs::remove_file(dir.join("package.json")).unwrap();
        make_link(canary.path(), &dir.join("package.json")).unwrap();
        fs::remove_file(dir.join("pytest.ini")).unwrap();
        write(&dir, "pytest.ini/conftest.py", "");
        #[cfg(unix)]
        {
            // A second hard link to a file outside: writing through it would change that file.
            let outside = canary.path().join("Cargo.toml");
            fs::remove_file(dir.join("Cargo.toml")).unwrap();
            fs::write(&outside, "keep me too").unwrap();
            fs::hard_link(&outside, dir.join("Cargo.toml")).unwrap();
        }
        let before = state(canary.path());

        assert!(write_boundaries(&project.yalper).unwrap() >= 2);
        for (name, content) in FILES {
            assert!(fs::symlink_metadata(dir.join(name)).unwrap().is_file());
            assert_eq!(read(&project, name), content);
        }
        assert_eq!(state(canary.path()), before);
    }

    #[test]
    fn switches_and_logs_leave_the_boundary_files_alone() {
        let project = Project::new();
        write_boundaries(&project.yalper).unwrap();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("package.json", "{}")]))
            .unwrap();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let mut second = TestCopy::open(&project.yalper, 1).unwrap();
        second
            .switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        Logs::open(&project.yalper).unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        env_additions(&copy, &python).unwrap();
        assert_eq!(write_boundaries(&project.yalper).unwrap(), 0);
    }

    fn environment() -> Vec<(OsString, OsString)> {
        std::env::vars_os().collect()
    }

    /// The user's environment without the variables `names`.
    fn environment_without(names: &[&str]) -> Vec<(OsString, OsString)> {
        let mut env = environment();
        env.retain(|(name, _)| {
            !names
                .iter()
                .any(|var| name.to_string_lossy().eq_ignore_ascii_case(var))
        });
        env
    }

    /// The user's environment for cargo, without the variables `names`: output without colors, and no
    /// `CARGO_BUILD_TARGET_DIR` a developer may have set.
    fn cargo_environment(names: &[&str]) -> Vec<(OsString, OsString)> {
        let mut removed = vec!["CARGO_TERM_COLOR", "CARGO_BUILD_TARGET_DIR"];
        removed.extend(names);
        let mut env = environment_without(&removed);
        env.push(("CARGO_TERM_COLOR".into(), "never".into()));
        env
    }

    /// A test copy holding `files`.
    fn copy_with(project: &Project, files: &[(&str, &str)]) -> TestCopy {
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(files)).unwrap();
        copy
    }

    /// Runs `command` in `copy` as a bisect does: boundary files written, Python environments linked, and the
    /// environment built by [`run_env`] from `user`. The outcome and the output.
    fn run_as_bisect(
        project: &Project,
        copy: &mut TestCopy,
        command: &str,
        user: Vec<(OsString, OsString)>,
    ) -> (Outcome, String) {
        write_boundaries(&project.yalper).unwrap();
        let mut python = PythonEnvs::new(project.path()).unwrap();
        python.link(copy).unwrap();
        let additions = env_additions(copy, &python).unwrap();
        let env = run_env(user, project.path(), copy, additions).unwrap();
        run_with(copy.path(), command, &env)
    }

    /// Runs `command` in `copy` without what keeps it there: no boundary files, and `user` as it is. Shows that
    /// the command does reach the project otherwise.
    fn run_unprotected(
        project: &Project,
        copy: &TestCopy,
        command: &str,
        user: Vec<(OsString, OsString)>,
    ) -> (Outcome, String) {
        for (name, _) in FILES {
            fs::remove_file(bisect_dir(project).join(name)).unwrap();
        }
        run_with(copy.path(), command, &user)
    }

    fn run_with(dir: &Path, command: &str, env: &[(OsString, OsString)]) -> (Outcome, String) {
        let log = tempfile::NamedTempFile::new().unwrap();
        let stop = AtomicBool::new(false);
        let ran = run(command, dir, env, log.as_file(), None, &stop).unwrap();
        let output = String::from_utf8_lossy(&fs::read(log.path()).unwrap()).into_owned();
        (ran.outcome, output)
    }

    fn assert_failed(outcome: Outcome, output: &str, expected: &str) {
        assert!(
            matches!(outcome, Outcome::Failed(_)),
            "{outcome:?}: {output}"
        );
        assert!(output.contains(expected), "{expected:?} not in: {output}");
    }

    #[test]
    fn git_in_a_copy_finds_no_repository_even_when_told_to_use_the_projects() {
        let Some(git) = tool(&["git"]) else {
            return;
        };
        let project = Project::new();
        let git_in_project = |args: &[&str]| {
            // No automatic maintenance: git would run it in the background after the commit, changing `.git`
            // while its state is read.
            let output = Command::new(git)
                .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
                .args(["-c", "user.name=Yalper Test"])
                .args(["-c", "user.email=test@example.com"])
                .args(args)
                .current_dir(project.path())
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        project.write("a.txt", "committed\n");
        git_in_project(&["init", "-q"]);
        git_in_project(&["add", "a.txt"]);
        git_in_project(&["commit", "-q", "-m", "one"]);
        // An uncommitted change: `git stash` would take it away.
        project.write("a.txt", "changed\n");
        let before = state(project.path());

        let mut copy = copy_with(&project, &[("README.md", "")]);
        // As if Yalper had been started by a git hook, which sets these.
        let root = project.path();
        let mut user = environment_without(&["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"]);
        user.push(("GIT_DIR".into(), root.join(".git").into()));
        user.push(("GIT_WORK_TREE".into(), root.into()));
        user.push((
            "GIT_INDEX_FILE".into(),
            root.join(".git").join("index").into(),
        ));
        for command in ["git status", "git stash", "cd .. && git status"] {
            let (outcome, output) = run_as_bisect(&project, &mut copy, command, user.clone());
            assert_failed(outcome, &output, "not a git repository");
        }
        assert_eq!(state(project.path()), before);

        // Without the protections, git in the copy finds the project's repository.
        let user = environment_without(&["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"]);
        let (outcome, output) = run_unprotected(&project, &copy, "git status", user);
        assert_eq!(outcome, Outcome::Passed, "{output}");
    }

    #[test]
    fn npm_in_a_step_without_package_json_never_runs_the_projects_script() {
        if tool(&["npm.cmd", "npm"]).is_none() {
            return;
        }
        let project = Project::new();
        project.write(
            "package.json",
            r#"{"name": "project", "version": "1.0.0", "scripts": {"test": "node ran.js"}}"#,
        );
        project.write(
            "ran.js",
            "require('fs').writeFileSync(require('path').join(__dirname, 'ran'), '')\n",
        );
        let before = state(project.path());

        let mut copy = copy_with(&project, &[("README.md", ""), ("index.js", "")]);
        let (outcome, output) = run_as_bisect(&project, &mut copy, "npm test", environment());
        assert_failed(outcome, &output, "Missing script");
        assert_eq!(state(project.path()), before);

        // Without the boundary, npm runs the project's script in the project.
        let (outcome, output) = run_unprotected(&project, &copy, "npm test", environment());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(project.path().join("ran").exists());
    }

    #[test]
    fn pytest_in_a_step_without_its_configuration_stays_out_of_the_project() {
        let Some(python) = tool(&["python3", "python"]) else {
            return;
        };
        let manifest = "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n";
        let project = Project::new();
        project.write(".gitignore", ".venv/\n");
        project.write("pyproject.toml", manifest);
        project.write("pytest.ini", "[pytest]\n");
        project.write(
            "conftest.py",
            "import pathlib\npathlib.Path(__file__).with_name('ran').write_text('')\n",
        );
        let in_project = |program: &Path, args: &[&str]| {
            let output = Command::new(program)
                .args(args)
                .current_dir(project.path())
                .env_remove("VIRTUAL_ENV")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        in_project(Path::new(python), &["-m", "venv", ".venv"]);
        let venv = project.path().join(".venv");
        let pip_install = [
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-input",
            "--quiet",
            "pytest",
        ];
        in_project(&venv_python(&venv), &pip_install);
        let before = state(project.path());

        // The step has a Python project and a test, but no pytest configuration yet.
        let mut copy = copy_with(
            &project,
            &[
                (".gitignore", ".venv/\n"),
                ("pyproject.toml", manifest),
                ("test_one.py", "def test_one():\n    pass\n"),
            ],
        );
        // The project's environment is activated: first in `PATH`, and `VIRTUAL_ENV`.
        let mut user = environment_without(&["VIRTUAL_ENV", "PYTHONPATH"]);
        let bin = venv_python(&venv).parent().unwrap().to_owned();
        for (name, value) in &mut user {
            if name.to_string_lossy().eq_ignore_ascii_case("PATH") {
                let mut paths = vec![bin.clone()];
                paths.extend(std::env::split_paths(value));
                *value = std::env::join_paths(paths).unwrap();
            }
        }
        user.push(("VIRTUAL_ENV".into(), venv.clone().into()));
        let (outcome, output) = run_as_bisect(&project, &mut copy, "pytest", user.clone());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(output.contains("1 passed"), "{output}");
        assert_eq!(state(project.path()), before);
        assert!(bisect_dir(&project).join(".pytest_cache").is_dir());

        // Without the boundary, pytest loads the project's `conftest.py` and writes its cache there.
        let (outcome, output) = run_unprotected(&project, &copy, "pytest", user);
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(project.path().join("ran").exists());
        assert!(project.path().join(".pytest_cache").is_dir());
    }

    #[test]
    fn cargo_in_a_step_without_cargo_toml_never_builds_the_project() {
        if tool(&["cargo"]).is_none() {
            return;
        }
        let project = Project::new();
        project.write(
            "Cargo.toml",
            "[package]\nname = \"project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        project.write(
            "src/lib.rs",
            "#[test]\nfn writes() {\n    \
             std::fs::write(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/ran\"), \"\").unwrap();\n}\n",
        );
        let before = state(project.path());

        let mut copy = copy_with(&project, &[("README.md", "")]);
        let command = "cargo test --offline";
        let (outcome, output) = run_as_bisect(&project, &mut copy, command, cargo_environment(&[]));
        assert_failed(outcome, &output, "no_cargo_toml_in_this_step");
        assert_eq!(state(project.path()), before);

        // Without the boundary, cargo builds and tests the project's code, and writes its `Cargo.lock`.
        let user = cargo_environment(&["CARGO_TARGET_DIR"]);
        let (outcome, output) = run_unprotected(&project, &copy, command, user);
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(project.path().join("ran").exists());
        assert!(project.path().join("Cargo.lock").exists());
    }

    #[test]
    fn cargo_builds_single_package_and_workspace_steps_as_without_the_boundary() {
        if tool(&["cargo"]).is_none() {
            return;
        }
        let package = |name: &str| {
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        };
        let lib = "#[test]\nfn works() {}\n";
        let (single, member) = (package("single"), package("member"));
        let workspace = "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n";
        let steps: [&[(&str, &str)]; 2] = [
            &[("Cargo.toml", &single), ("src/lib.rs", lib)],
            &[
                ("Cargo.toml", workspace),
                ("member/Cargo.toml", &member),
                ("member/src/lib.rs", lib),
            ],
        ];
        for files in steps {
            // The project holds the same code as the step, as it usually does.
            let project = Project::new();
            for (path, content) in files {
                project.write(path, content);
            }
            let before = state(project.path());
            let mut copy = copy_with(&project, files);
            let user = cargo_environment(&[]);
            let command = "cargo test --offline";
            let (outcome, output) = run_as_bisect(&project, &mut copy, command, user);
            assert_eq!(outcome, Outcome::Passed, "{output}");
            assert!(output.contains("1 passed"), "{output}");
            assert!(!output.contains("no-cargo-toml"), "{output}");
            assert!(!output.contains("warning"), "{output}");
            assert_eq!(state(project.path()), before);
        }
    }

    #[test]
    fn uv_run_in_a_step_without_pyproject_toml_never_uses_the_projects_environment() {
        let Some(uv) = tool(&["uv"]) else {
            return;
        };
        let project = Project::new();
        project.write(".gitignore", ".venv/\n");
        project.write(
            "pyproject.toml",
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.8\"\n",
        );
        let output = Command::new(uv)
            .args(["sync", "--quiet"])
            .current_dir(project.path())
            .env_remove("VIRTUAL_ENV")
            .env_remove("UV_PROJECT_ENVIRONMENT")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let before = state(project.path());

        let mut copy = copy_with(
            &project,
            &[
                ("README.md", ""),
                ("prefix.py", "import sys\nprint('prefix=' + sys.prefix)\n"),
            ],
        );
        // The project's environment is activated.
        let mut user =
            environment_without(&["VIRTUAL_ENV", "UV_PROJECT_ENVIRONMENT", "UV_NO_SYNC"]);
        user.push(("VIRTUAL_ENV".into(), project.path().join(".venv").into()));
        let command = "uv run --quiet python prefix.py";
        let prefix = |output: &str| {
            let line = output
                .lines()
                .find_map(|line| line.trim().strip_prefix("prefix="));
            let line = line.unwrap_or_else(|| panic!("no prefix in: {output}"));
            fs::canonicalize(line).unwrap()
        };
        let (outcome, output) = run_as_bisect(&project, &mut copy, command, user.clone());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        let yalper = fs::canonicalize(project.path().join(".yalper")).unwrap();
        assert!(prefix(&output).starts_with(&yalper), "{output}");
        assert_eq!(state(project.path()), before);

        // Without the boundary, uv runs the project's environment.
        user.push(("UV_NO_SYNC".into(), "1".into()));
        let (outcome, output) = run_unprotected(&project, &copy, command, user);
        assert_eq!(outcome, Outcome::Passed, "{output}");
        let venv = fs::canonicalize(project.path().join(".venv")).unwrap();
        assert_eq!(prefix(&output), venv, "{output}");
    }
}
