//! `yalper bisect`: finds the step of a session that broke the tests by running them against the snapshots
//! of its steps.
//!
//! The tests never run in the project: each run happens in a test copy that holds the code of one step (see
//! [`TestCopy`]), and is started by [`run`] with the environment of [`run_env`]. The copies, and all other
//! working data of bisect, live outside the project in its folder of the user's cache folder (see
//! [`CacheDir`]), so tools that search the folders above a copy never reach the project. The boundary files of
//! [`write_boundaries`] stop those searches before the user's home folder, and [`clean_root`] removes what tools
//! and tests leave next to them.
//!
//! [`DependencySets`] picks the copy of each step: the one linked to the project's installed dependencies
//! when the step's dependency files are the project's, otherwise one per dependency set, installed once.
//!
//! [`bisect`] is the search itself: it picks the entry to test next and its time limit, and reads what the
//! runs say, without touching files or processes.

mod boundary;
mod cache;
mod copy;
mod deps;
mod engine;
mod env;
mod link;
mod node_modules;
mod python;
mod run;
#[cfg(test)]
mod testing;

pub use boundary::write_boundaries;
pub use cache::{
    CACHE_DIR_KEY, CACHE_DIR_VAR, CacheDir, ProjectFolder, ProjectFolders, clean_root,
    find_project_folders, remove_stale,
};
pub use copy::{Switched, TestCopy};
pub use deps::{COPIES_KEY, DependencySets, InstallFailure, Prepared, SetCopy};
pub use engine::{
    Answer, End, Failure, LINKER_RERUNS, Reason, Report, Settings, Skip, Tested, Trial,
    automatic_limit, bisect,
};
pub use env::{EnvAdditions, env_additions, run_env};
pub use link::CANNOT_LINK;
pub use node_modules::{Linked, NodeModules};
pub use python::{LinkedEnvs, PythonEnvs};
pub use run::{
    Exit, Logs, Outcome, Run, SKIP_EXIT_CODE, STOP, STOPPED_EXIT_CODE, handle_ctrl_c, run,
};
