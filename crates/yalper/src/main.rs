use std::env;
use std::ffi::OsStr;
use std::io;

use clap::Parser;

/// Record, replay, and debug AI coding agent sessions.
#[derive(Parser)]
#[command(name = "yalper", version, about)]
struct Cli {}

fn main() {
    // `yalper hook` is run by Claude Code on every agent step, never by hand, so it is not listed in the
    // help. It is handled before argument parsing, which exits with code 2 on unexpected arguments, and
    // Claude Code treats exit code 2 as "block this prompt or tool".
    if env::args_os().nth(1).as_deref() == Some(OsStr::new("hook")) {
        yalper::hook::silence_panics();
        yalper::hook::run(&mut io::stdin().lock());
        return;
    }

    let _cli = Cli::parse();
    println!("Yalper is in early development. Run `yalper --help` for usage.");
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
