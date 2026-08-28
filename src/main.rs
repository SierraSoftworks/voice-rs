use clap::Parser;

#[macro_use]
mod macros;

mod audio;
mod commands;
mod config;
mod errors;
mod grammar;
mod hotkey;
mod matcher;
mod output;
mod recognition;
mod telemetry;

pub use commands::Command;
pub use human_errors::Error;

/// Speak a phrase, press the keys: a Linux-native voice macro tool.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[tokio::main]
async fn main() {
    use std::io::IsTerminal;

    let args = Args::parse();

    // `test` and `run` render a full-screen TUI when stdout is an interactive
    // terminal, and a tracing stdout layer would draw straight over it — so
    // the console layer is left out entirely in that configuration. This
    // predicate must stay in step with the TUI-vs-plain selection inside those
    // commands (both key off stdout being a TTY). Telemetry export itself is
    // unaffected; failures still reach the user via eprintln! below, after
    // the terminal has been restored.
    let console = !(matches!(args.command, Command::Test(_) | Command::Run(_))
        && std::io::stdout().is_terminal());
    let session = telemetry::setup(console);

    match commands::dispatch(args).await {
        Ok(code) => {
            session.shutdown();
            std::process::exit(code);
        }
        Err(e) => {
            session.record_error(&e);
            // The failure itself goes straight to stderr rather than through
            // tracing: the user must see why their command failed even when
            // telemetry is disabled or misconfigured.
            eprintln!("{}", human_errors::pretty(&e));
            session.shutdown();
            std::process::exit(1);
        }
    }
}
