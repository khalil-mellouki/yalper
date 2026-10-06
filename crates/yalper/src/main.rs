use std::env;
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use jiff::tz::TimeZone;

/// Record, replay, and debug AI coding agent sessions.
#[derive(Parser)]
#[command(name = "yalper", version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set up recording of Claude Code sessions in the current git project.
    Init {
        /// Delete and recreate a .yalper folder that Yalper cannot use (one it did not create for this
        /// repository). A working .yalper folder is always kept.
        #[arg(long)]
        recreate: bool,
    },
    /// Stop recording in the current git project: remove Yalper's Claude Code hooks. Recordings are kept.
    Uninstall {
        /// Also delete the .yalper folder with every recording.
        #[arg(long)]
        purge: bool,
    },
    /// List the recorded sessions and their steps. Shows the most recent session unless told otherwise.
    Log {
        /// Show the session whose id starts with this.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Show every session, the most recent first.
        #[arg(long, conflicts_with = "session")]
        all: bool,
    },
    /// Show one step of the most recent session: its prompt or tool call, and the diff of the files it
    /// changed.
    Show {
        /// The step number, as `yalper log` lists it.
        step: u32,
        /// Show a step of the session whose id starts with this.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Show long texts and diffs whole instead of cut short.
        #[arg(long)]
        full: bool,
    },
}

fn main() -> ExitCode {
    // `yalper hook` is run by Claude Code on every agent step, never by hand, so it is not listed in the
    // help. It is handled before argument parsing, which exits with code 2 on unexpected arguments, and
    // Claude Code treats exit code 2 as "block this prompt or tool".
    if env::args_os().nth(1).as_deref() == Some(OsStr::new("hook")) {
        yalper::hook::silence_panics();
        yalper::hook::run(&mut io::stdin().lock());
        return ExitCode::SUCCESS;
    }

    let result = match Cli::parse().command {
        Command::Init { recreate } => init(recreate),
        Command::Uninstall { purge } => uninstall(purge),
        Command::Log { session, all } => log(session, all),
        Command::Show {
            step,
            session,
            full,
        } => show(step, session, full),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // Messages quote paths and file system errors, which can hold repository text.
            eprintln!("error: {}", yalper::init::printable(&message));
            ExitCode::FAILURE
        }
    }
}

fn init(recreate: bool) -> Result<(), String> {
    let start = env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    // Not canonicalized: on Windows that would give a `\\?\` path, which is not needed to run the binary.
    let exe = env::current_exe()
        .map_err(|error| format!("cannot find the path of the yalper binary: {error}"))?;
    yalper::init::init(&start, &exe, recreate, &mut io::stdout().lock())
}

fn uninstall(purge: bool) -> Result<(), String> {
    let start = env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    yalper::uninstall::uninstall(&start, purge, &mut io::stdout().lock())
}

fn log(session: Option<String>, all: bool) -> Result<(), String> {
    let start = env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    let selection = match (session, all) {
        (Some(prefix), _) => yalper::log::Selection::Session(prefix),
        (None, true) => yalper::log::Selection::All,
        (None, false) => yalper::log::Selection::Latest,
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    yalper::log::log(&start, &selection, &TimeZone::system(), &mut out)?;
    // A closed output (`yalper log | head`) is not an error.
    let _ = out.flush();
    Ok(())
}

fn show(step: u32, session: Option<String>, full: bool) -> Result<(), String> {
    let start = env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    let stdout = io::stdout();
    let color = color_wanted(
        stdout.is_terminal(),
        env::var_os("TERM").as_deref(),
        env::var_os("NO_COLOR").as_deref(),
    );
    let options = yalper::show::Options {
        session,
        full,
        color,
    };
    let mut out = io::BufWriter::new(stdout.lock());
    yalper::show::show(&start, step, &options, &TimeZone::system(), &mut out)?;
    // A closed output (`yalper show 3 | head`) is not an error.
    let _ = out.flush();
    Ok(())
}

/// Whether to color the output: only on a terminal that shows colors (`TERM` is not `dumb`), unless
/// `NO_COLOR` (https://no-color.org) is set to anything. Never on Windows, where an older console would print
/// the escape sequences as text.
fn color_wanted(is_terminal: bool, term: Option<&OsStr>, no_color: Option<&OsStr>) -> bool {
    cfg!(not(windows))
        && is_terminal
        && term != Some(OsStr::new("dumb"))
        && no_color.is_none_or(OsStr::is_empty)
}

#[cfg(test)]
mod tests {
    use super::{Cli, color_wanted};
    use clap::CommandFactory;
    use std::ffi::OsStr;

    #[test]
    fn colors_only_on_a_capable_terminal_without_no_color() {
        let os = |text| Some(OsStr::new(text));
        let unix = cfg!(not(windows));
        assert_eq!(color_wanted(true, os("xterm-256color"), None), unix);
        assert_eq!(color_wanted(true, None, os("")), unix);
        assert!(!color_wanted(false, os("xterm"), None));
        assert!(!color_wanted(true, os("dumb"), None));
        assert!(!color_wanted(true, os("xterm"), os("1")));
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
