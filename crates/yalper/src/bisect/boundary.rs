//! Boundary files at the top of the project's cache folder, the folder that holds the test copies.
//!
//! The copies live outside the project (see [`CacheDir`]), so no tool that searches the folders above a copy
//! reaches the project. Without these files, such searches would go on to the user's home folder, which often
//! holds a stray `package.json`, `node_modules` or `pytest.ini`. The files written here end them in the project's
//! cache folder, so that in a step without the file a tool looks for (a step from before the agent created it),
//! the tool fails there or finds nothing to run:
//!
//! - `package.json`, with no scripts: `npm test` fails with "Missing script". npm, pnpm, bun and Yarn 1 take
//!   the nearest `package.json` as the package a command runs in, and Node takes the module type of a step's
//!   `.js` files from it;
//! - `pytest.ini`: pytest's root folder (where it writes `.pytest_cache`) is the project's cache folder, and it
//!   loads no `conftest.py` above it;
//! - `pyproject.toml` with a `[project]` table: uv (and Poetry, Hatch, PDM) take it as the project, so `uv run`
//!   uses its own environment in the cache folder's `.venv`. It also ends uv's search for a workspace above a
//!   step's own project;
//! - `Cargo.toml`, a package whose library file does not exist: `cargo test` fails with "can't find lib
//!   `no_cargo_toml_in_this_step`". Cargo also reads it when it searches for a workspace above a step's own
//!   package, where a package that is not a workspace changes nothing.
//!
//! Not ended here, as in any folder under the home folder: pnpm's search for `pnpm-workspace.yaml`, the search
//! of `npx`, `npm exec` and `bunx` for programs in the `node_modules/.bin` of every folder above, Node's module
//! resolution, and the configuration files that cargo, ESLint, Prettier and TypeScript read from every folder
//! above (`~/.cargo/config.toml`, which applies anyway as the one of `CARGO_HOME`, or `~/.prettierrc`). Yarn 2
//! and later were not tested.
//!
//! What tools and tests leave next to these files (a `node_modules/` from `npm install <package>`, uv's `.venv/`,
//! a planted `conftest.py`) is removed by [`clean_root`](super::clean_root). The files are rewritten whenever
//! they differ: what is on disk is never trusted, since a test run can change them (`npm pkg set`) or put a
//! hard link to another file in their place. A file that differs is removed and created anew, never written in
//! place.

use std::fs;
use std::io::{self, Write};

use super::CacheDir;
use super::copy::remove_entry;
use crate::safe_fs::{self, OwnedDir};

/// The boundary files: name in the project's cache folder and content.
pub(super) const FILES: [(&str, &str); 4] = [
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

/// Writes the boundary files into the project's cache folder `cache`: each one that is not a regular file
/// holding exactly its content, with a single link (Unix) and as Yalper made it (mode 0600 on Unix, not
/// read-only on Windows), is removed, never followed, and created anew. Returns how many were written.
///
/// Call it at the start of every bisect, and before every run and every install, after
/// [`clean_root`](super::clean_root), since a run can change them: checking four small files is cheap.
pub fn write_boundaries(cache: &CacheDir) -> io::Result<usize> {
    let dir = cache.dir();
    let mut written = 0;
    for (name, content) in FILES {
        if write_if_different(dir, name, content.as_bytes()).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot write {}: {error}", dir.path().join(name).display()),
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
        Ok(metadata) => {
            if is_as_written(&metadata)
                && safe_fs::read_small_regular_file(&path, content.len() as u64).as_deref()
                    == Some(content)
            {
                return Ok(false);
            }
            remove_entry(&path, &metadata)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = fs::File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    #[cfg(unix)]
    {
        // Whatever the umask.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(content)?;
    Ok(true)
}

/// Whether `metadata` (not followed) is of a file as [`write_if_different`] makes it.
fn is_as_written(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.is_file() && metadata.nlink() == 1 && metadata.mode() & 0o777 == 0o600
    }
    // The standard library gives no link count on Windows: a hard link holding the right content is kept, and
    // nothing is ever written through it.
    #[cfg(windows)]
    {
        metadata.is_file() && !metadata.permissions().readonly()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;

    use crate::bisect::link::make_link;
    use crate::bisect::testing::{Project, state, tool, venv_python, write};
    use crate::bisect::{
        Logs, NodeModules, Outcome, PythonEnvs, TestCopy, clean_root, env_additions, run, run_env,
    };
    use crate::repo::Token;

    fn read(project: &Project, name: &str) -> String {
        fs::read_to_string(project.cache.path().join(name)).unwrap()
    }

    #[test]
    fn boundary_files_are_written_and_then_rewritten_only_when_they_differ() {
        let project = Project::new();
        assert_eq!(write_boundaries(&project.cache).unwrap(), FILES.len());
        for (name, content) in FILES {
            assert_eq!(read(&project, name), content);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let path = project.cache.path().join(name);
                let mode = fs::metadata(path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{name}");
            }
        }
        assert_eq!(write_boundaries(&project.cache).unwrap(), 0);

        // A test run added a script to one, made another read-only, and emptied a third: all three are written
        // anew.
        let dir = project.cache.path();
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
        assert_eq!(write_boundaries(&project.cache).unwrap(), 3);
        for (name, content) in FILES {
            assert_eq!(read(&project, name), content);
        }
    }

    #[test]
    fn a_link_hard_link_or_folder_in_place_of_a_boundary_file_is_replaced_never_followed() {
        let project = Project::new();
        let canary = tempfile::tempdir().unwrap();
        write(canary.path(), "package.json", "keep me");
        write(canary.path(), "Cargo.toml", "keep me too");
        write_boundaries(&project.cache).unwrap();
        let dir = project.cache.path();
        fs::remove_file(dir.join("package.json")).unwrap();
        make_link(canary.path(), &dir.join("package.json")).unwrap();
        fs::remove_file(dir.join("pytest.ini")).unwrap();
        write(dir, "pytest.ini/conftest.py", "");
        // A second hard link to a file outside: writing through it would change that file.
        fs::remove_file(dir.join("Cargo.toml")).unwrap();
        fs::hard_link(canary.path().join("Cargo.toml"), dir.join("Cargo.toml")).unwrap();
        #[cfg(unix)]
        {
            write(canary.path(), "pyproject.toml", "keep me three");
            fs::remove_file(dir.join("pyproject.toml")).unwrap();
            let target = canary.path().join("pyproject.toml");
            std::os::unix::fs::symlink(target, dir.join("pyproject.toml")).unwrap();
        }
        let before = state(canary.path());

        let replaced = if cfg!(unix) { 4 } else { 3 };
        assert_eq!(write_boundaries(&project.cache).unwrap(), replaced);
        for (name, content) in FILES {
            assert!(fs::symlink_metadata(dir.join(name)).unwrap().is_file());
            assert_eq!(read(&project, name), content);
        }
        assert_eq!(state(canary.path()), before);
    }

    #[test]
    fn switches_and_logs_leave_the_boundary_files_alone() {
        let project = Project::new();
        write_boundaries(&project.cache).unwrap();
        let mut copy = project.copy();
        copy.switch(&project.store, project.tree(&[("package.json", "{}")]))
            .unwrap();
        copy.switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        let mut second = TestCopy::open(&project.cache, 1).unwrap();
        second
            .switch(&project.store, project.tree(&[("README.md", "")]))
            .unwrap();
        Logs::open(&project.cache).unwrap();
        let python = PythonEnvs::new(project.path()).unwrap();
        env_additions(&copy, &python).unwrap();
        assert_eq!(clean_root(&project.cache).unwrap(), 0);
        assert_eq!(write_boundaries(&project.cache).unwrap(), 0);
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

    /// Runs `command` in `copy` as a bisect does: the top of the cache folder cleaned, boundary files written,
    /// Python environments linked, and the environment built by [`run_env`] from `user`. The outcome and the
    /// output.
    fn run_as_bisect(
        copy: &mut TestCopy,
        command: &str,
        user: Vec<(OsString, OsString)>,
    ) -> (Outcome, String) {
        let project = copy.cache().project().to_owned();
        clean_root(copy.cache()).unwrap();
        write_boundaries(copy.cache()).unwrap();
        let mut python = PythonEnvs::new(&project).unwrap();
        python.link(copy).unwrap();
        let additions = env_additions(copy, &python).unwrap();
        let env = run_env(user, &project, copy, additions).unwrap();
        run_with(copy.path(), command, &env)
    }

    /// Runs `command` in the folder `sub` of the project (created if missing), where the test copies used to
    /// be, with the environment `user` as it is: shows that the command does reach the project from there, so
    /// that each test proves something.
    fn run_in_project(
        project: &Path,
        command: &str,
        user: &[(OsString, OsString)],
    ) -> (Outcome, String) {
        let sub = project.join("sub");
        fs::create_dir_all(&sub).unwrap();
        run_with(&sub, command, user)
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

    /// Runs `git` with `args` in `dir`, outside any environment that names a repository, and checks it worked.
    fn git_in(git: &str, dir: &Path, args: &[&str]) {
        // No automatic maintenance and no file system monitor: git would run them in the background, changing
        // `.git` while its state is read.
        let output = Command::new(git)
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .args(["-c", "core.fsmonitor=false"])
            .args(["-c", "user.name=Yalper Test"])
            .args(["-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn git_in_a_copy_finds_no_repository_even_when_told_to_use_the_projects() {
        let Some(git) = tool(&["git"]) else {
            return;
        };
        let project = Project::new();
        project.write("a.txt", "committed\n");
        git_in(git, project.path(), &["init", "-q"]);
        git_in(git, project.path(), &["add", "a.txt"]);
        git_in(git, project.path(), &["commit", "-q", "-m", "one"]);
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
            let (outcome, output) = run_as_bisect(&mut copy, command, user.clone());
            assert_failed(outcome, &output, "not a git repository");
        }
        assert_eq!(state(project.path()), before);

        // From inside the project, git finds the project's repository.
        let user = environment_without(&["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"]);
        let (outcome, output) = run_in_project(project.path(), "git status", &user);
        assert_eq!(outcome, Outcome::Passed, "{output}");
    }

    #[test]
    fn git_finds_no_repository_around_the_cache_folder() {
        let Some(git) = tool(&["git"]) else {
            return;
        };
        // A home folder kept in git (dotfiles) that holds the cache folder.
        let home = tempfile::tempdir().unwrap();
        git_in(git, home.path(), &["init", "-q"]);
        let project = Project::with_cache_base(home);
        let mut copy = copy_with(&project, &[("README.md", "")]);
        let user = environment_without(&[
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CEILING_DIRECTORIES",
        ]);
        // In the copy, and at the top of the project's cache folder.
        for command in ["git status", "cd .. && git status"] {
            let (outcome, output) = run_as_bisect(&mut copy, command, user.clone());
            assert_failed(outcome, &output, "not a git repository");
        }

        // Without the environment of a run, git in the copy finds the home folder's repository.
        let (outcome, output) = run_with(copy.path(), "git status", &user);
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
        let (outcome, output) = run_as_bisect(&mut copy, "npm test", environment());
        assert_failed(outcome, &output, "Missing script");
        assert_eq!(state(project.path()), before);

        // From inside the project, npm runs the project's script.
        let (outcome, output) = run_in_project(project.path(), "npm test", &environment());
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
        let (outcome, output) = run_as_bisect(&mut copy, "pytest", user.clone());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(output.contains("1 passed"), "{output}");
        assert_eq!(state(project.path()), before);
        assert!(project.cache.path().join(".pytest_cache").is_dir());

        // From inside the project, pytest loads the project's `conftest.py` and writes its cache there.
        run_in_project(project.path(), "pytest", &user);
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
        let (outcome, output) = run_as_bisect(&mut copy, command, cargo_environment(&[]));
        assert_failed(outcome, &output, "no_cargo_toml_in_this_step");
        assert_eq!(state(project.path()), before);

        // From inside the project, cargo builds and tests the project's code, and writes its `Cargo.lock`.
        let user = cargo_environment(&["CARGO_TARGET_DIR"]);
        let (outcome, output) = run_in_project(project.path(), command, &user);
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
            // A relative build folder in the project's cargo settings, which cargo would resolve against the
            // project if it read them for the copy.
            project.write(".cargo/config.toml", "[build]\nbuild-dir = \"build-out\"\n");
            let before = state(project.path());
            let mut copy = copy_with(&project, files);
            let user = cargo_environment(&[]);
            let command = "cargo test --offline";
            let (outcome, output) = run_as_bisect(&mut copy, command, user);
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

        let prefix_py = "import sys\nprint('prefix=' + sys.prefix)\n";
        let mut copy = copy_with(&project, &[("README.md", ""), ("prefix.py", prefix_py)]);
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
        let (outcome, output) = run_as_bisect(&mut copy, command, user.clone());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        let cache = fs::canonicalize(project.cache.path()).unwrap();
        assert!(prefix(&output).starts_with(&cache), "{output}");
        assert_eq!(state(project.path()), before);

        // From inside the project, uv runs the project's environment.
        user.push(("UV_NO_SYNC".into(), "1".into()));
        project.write("sub/prefix.py", prefix_py);
        let (outcome, output) = run_in_project(project.path(), command, &user);
        assert_eq!(outcome, Outcome::Passed, "{output}");
        let venv = fs::canonicalize(project.path().join(".venv")).unwrap();
        assert_eq!(prefix(&output), venv, "{output}");
    }

    #[test]
    fn pnpm_in_a_step_without_its_workspace_file_never_runs_the_projects_workspace() {
        if tool(&["pnpm.cmd", "pnpm"]).is_none() {
            return;
        }
        let root = r#"{"name": "root", "version": "1.0.0", "private": true}"#;
        let package = r#"{"name": "a", "version": "1.0.0", "scripts": {"test": "node ran.js"}}"#;
        let ran = "require('fs').writeFileSync(require('path').join(__dirname, 'ran'), '')\n";
        let files = [
            ("package.json", root),
            ("packages/a/package.json", package),
            ("packages/a/ran.js", ran),
        ];
        let project = Project::new();
        for (path, content) in files {
            project.write(path, content);
        }
        project.write("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n");
        let before = state(project.path());

        // The step has the packages, but not the workspace file yet.
        let mut copy = copy_with(&project, &files);
        let (_, output) = run_as_bisect(&mut copy, "pnpm -r test", environment());
        assert!(!project.path().join("packages/a/ran").exists(), "{output}");
        assert_eq!(state(project.path()), before);

        // From inside the project, pnpm runs the tests of the project's workspace.
        let (outcome, output) = run_in_project(project.path(), "pnpm -r test", &environment());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(project.path().join("packages/a/ran").exists(), "{output}");
    }

    #[test]
    fn npx_and_npm_exec_in_a_copy_never_find_the_projects_programs() {
        if tool(&["npm.cmd", "npm"]).is_none() {
            return;
        }
        let project = Project::new();
        project.write(
            "package.json",
            r#"{"name": "project", "version": "1.0.0", "dependencies": {"yalper-test-bin": "file:./tool"}}"#,
        );
        project.write(
            "tool/package.json",
            r#"{"name": "yalper-test-bin", "version": "1.0.0", "bin": {"yalper-test-bin": "bin.js"}}"#,
        );
        project.write(
            "tool/bin.js",
            "#!/usr/bin/env node\nrequire('fs').writeFileSync(require('path').join(__dirname, 'ran'), '')\n",
        );
        // From a local folder: no network.
        let install = "npm install --offline --no-audit --no-fund --loglevel=error";
        let (outcome, output) = run_with(project.path(), install, &environment());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(project.path().join("node_modules").join(".bin").is_dir());
        let before = state(project.path());

        // A step without the dependency, so no link to the project's packages.
        let step = r#"{"name": "project", "version": "1.0.0"}"#;
        let mut copy = copy_with(&project, &[("package.json", step), ("index.js", "")]);
        // `--no`: without a terminal, npm would otherwise download a package of that name.
        for command in [
            "npx --no yalper-test-bin",
            "npm exec --no -- yalper-test-bin",
        ] {
            let (outcome, output) = run_as_bisect(&mut copy, command, environment());
            assert!(matches!(outcome, Outcome::Failed(_)), "{command}: {output}");
        }
        assert!(!project.path().join("tool").join("ran").exists());
        assert_eq!(state(project.path()), before);

        // From inside the project, npx runs the project's program.
        let command = "npx --no yalper-test-bin";
        let (outcome, output) = run_in_project(project.path(), command, &environment());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(project.path().join("tool").join("ran").exists());
    }

    #[test]
    fn cargo_in_a_copy_never_uses_the_projects_cargo_configuration() {
        if tool(&["cargo"]).is_none() {
            return;
        }
        let project = Project::new();
        // A runner that writes a marker in the project instead of running the tests.
        let marker = project.path().join("ran");
        let marker_text = marker.to_str().unwrap();
        if cfg!(windows) && marker_text.contains(' ') {
            // `cmd /c` below would need quotes.
            return;
        }
        let runner = if cfg!(windows) {
            format!(
                r#"["cmd", "/d", "/c", {:?}]"#,
                format!("echo.>{marker_text}")
            )
        } else {
            format!(r#"["sh", "-c", "echo > \"$1\"", "sh", {marker_text:?}]"#)
        };
        project.write(
            ".cargo/config.toml",
            &format!("[target.'cfg(all())']\nrunner = {runner}\n"),
        );
        let before = state(project.path());

        let manifest = "[package]\nname = \"step\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib = "#[test]\nfn works() {}\n";
        let with_config = [
            ("Cargo.toml", manifest),
            ("src/lib.rs", lib),
            (".cargo/config.toml", "[term]\nquiet = false\n"),
        ];
        // A package without, and one with, its own configuration.
        for files in [&with_config[..2], &with_config[..]] {
            let mut copy = copy_with(&project, files);
            let command = "cargo test --offline";
            let (outcome, output) = run_as_bisect(&mut copy, command, cargo_environment(&[]));
            assert_eq!(outcome, Outcome::Passed, "{output}");
            assert!(output.contains("1 passed"), "{output}");
            assert!(!marker.exists());
        }
        assert_eq!(state(project.path()), before);

        // A package inside the project gets the project's runner.
        for (path, content) in with_config {
            project.write(&format!("sub/{path}"), content);
        }
        let user = cargo_environment(&["CARGO_TARGET_DIR"]);
        let (outcome, output) = run_in_project(project.path(), "cargo test --offline", &user);
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(marker.exists(), "{output}");
    }

    #[test]
    fn prettier_in_a_copy_never_loads_the_projects_configuration() {
        if tool(&["npm.cmd", "npm"]).is_none() {
            return;
        }
        let package =
            r#"{"name": "project", "version": "1.0.0", "devDependencies": {"prettier": "3.3.3"}}"#;
        let project = Project::new();
        project.write(".gitignore", "node_modules/\n");
        project.write("package.json", package);
        let install = "npm install --no-audit --no-fund --loglevel=error";
        let (outcome, output) = run_with(project.path(), install, &environment());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        // Prettier runs its configuration file when it loads it.
        project.write(
            "prettier.config.js",
            "require('fs').writeFileSync(require('path').join(__dirname, 'ran'), '');\n\
             module.exports = {};\n",
        );
        let before = state(project.path());

        // The step has no Prettier configuration yet; its packages are the project's, through a link farm.
        let formatted = "const a = 1;\n";
        let mut copy = copy_with(
            &project,
            &[
                (".gitignore", "node_modules/\n"),
                ("package.json", package),
                ("a.js", formatted),
            ],
        );
        let mut node_modules = NodeModules::new(project.path()).unwrap();
        node_modules.link(&mut copy).unwrap();
        let command = "npx --no prettier --check a.js";
        let (outcome, output) = run_as_bisect(&mut copy, command, environment());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(!project.path().join("ran").exists());
        assert_eq!(state(project.path()), before);

        // From inside the project, Prettier loads the project's configuration.
        project.write("sub/a.js", formatted);
        let (outcome, output) = run_in_project(project.path(), command, &environment());
        assert_eq!(outcome, Outcome::Passed, "{output}");
        assert!(project.path().join("ran").exists(), "{output}");
    }

    #[test]
    fn a_workspace_of_every_folder_in_the_project_still_builds_after_a_test_run() {
        if tool(&["cargo"]).is_none() {
            return;
        }
        let workspace = "[workspace]\nmembers = [\"*/*\"]\nresolver = \"2\"\n";
        let member = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib = "#[test]\nfn works() {}\n";
        let files = [
            ("Cargo.toml", workspace),
            ("crates/a/Cargo.toml", member),
            ("crates/a/src/lib.rs", lib),
        ];
        // The user's project, without a `.yalper/`: `*/*` would also match its folder `.yalper/snapshots.git`.
        // The snapshots come from the store of another project.
        let user_project = tempfile::tempdir().unwrap();
        for (path, content) in files {
            write(user_project.path(), path, content);
        }
        let target = tempfile::tempdir().unwrap();
        let cargo_build = || {
            let mut env = cargo_environment(&["CARGO_TARGET_DIR"]);
            env.push(("CARGO_TARGET_DIR".into(), target.path().into()));
            run_with(user_project.path(), "cargo build --offline", &env)
        };
        let (outcome, output) = cargo_build();
        assert_eq!(outcome, Outcome::Passed, "{output}");

        let snapshots = Project::new();
        let base = tempfile::tempdir().unwrap();
        let token = Token::parse(&"d".repeat(32)).unwrap();
        let folder = base.path().join("yalper");
        let cache = CacheDir::open_in(&folder, user_project.path(), &token).unwrap();
        let mut copy = TestCopy::open(&cache, 0).unwrap();
        copy.switch(&snapshots.store, snapshots.tree(&files))
            .unwrap();
        let command = "cargo test --offline";
        let (outcome, output) = run_as_bisect(&mut copy, command, cargo_environment(&[]));
        assert_eq!(outcome, Outcome::Passed, "{output}");

        let (outcome, output) = cargo_build();
        assert_eq!(outcome, Outcome::Passed, "{output}");
    }
}
