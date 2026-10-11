//! `yalper bisect`: finds the step of a session that broke the tests by running them against the snapshots
//! of its steps.
//!
//! The tests never run in the project: each run happens in a test copy inside `.yalper/bisect/` that holds
//! the code of one step (see [`TestCopy`]), and is started by [`run`].

mod copy;
mod env;
mod link;
mod node_modules;
mod python;
mod run;
#[cfg(test)]
mod testing;

pub use copy::{Switched, TestCopy};
pub use env::{EnvAdditions, env_additions};
pub use link::CANNOT_LINK;
pub use node_modules::{Linked, NodeModules};
pub use python::{LinkedEnvs, PythonEnvs};
pub use run::{
    Exit, Logs, Outcome, Run, SKIP_EXIT_CODE, STOP, STOPPED_EXIT_CODE, handle_ctrl_c, run,
};

/// The folder inside `.yalper/` that holds the test copies.
pub const BISECT_DIR: &str = "bisect";
