//! `yalper bisect`: finds the step of a session that broke the tests by running them against the snapshots
//! of its steps.
//!
//! The tests never run in the project: each run happens in a test copy inside `.yalper/bisect/` that holds
//! the code of one step (see [`TestCopy`]).

mod copy;

pub use copy::{Switched, TestCopy};

/// The folder inside `.yalper/` that holds the test copies.
pub const BISECT_DIR: &str = "bisect";
