use clap::Parser;

/// Record, replay, and debug AI coding agent sessions.
#[derive(Parser)]
#[command(name = "yalper", version, about)]
struct Cli {}

fn main() {
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
