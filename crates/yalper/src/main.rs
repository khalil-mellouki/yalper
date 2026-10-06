use std::env;
use std::ffi::OsStr;
use std::io;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
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

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
