use std::env;
use std::ffi::OsStr;
use std::io;

use clap::{Parser, Subcommand};

/// Record, replay, and debug AI coding agent sessions.
#[derive(Parser)]
#[command(name = "yalper", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Record one Claude Code hook event read from stdin. Run by Claude Code, not by hand.
    #[command(hide = true)]
    Hook,
}

fn main() {
    // `yalper hook` runs inside the agent on every step. It bypasses argument parsing, which exits with
    // code 2 on unexpected arguments, and Claude Code treats exit code 2 as "block this prompt or tool".
    if env::args_os().nth(1).as_deref() == Some(OsStr::new("hook")) {
        yalper::hook::run(&mut io::stdin().lock());
        return;
    }

    match Cli::parse().command {
        Some(Command::Hook) => yalper::hook::run(&mut io::stdin().lock()),
        None => println!("Yalper is in early development. Run `yalper --help` for usage."),
    }
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
